//! `rbx-apikey can-manage`: can you create a key for this experience at all?
//!
//! Asked often enough to deserve a command: before writing a `[keys.x]` block
//! and running `create`, you want to know whether Roblox will let you. For a
//! group-owned universe the answer depends on your role in the group, which is
//! not something the declaration file knows.
//!
//! **This uses the cookie, not an API key, and that is the whole point.** The
//! obvious approach, asking with a key, is circular: a key is bound to its
//! universes at creation, so a key for universe A answers `Forbidden` when
//! asked about universe B. Confirmed against the live API.
//!
//! It earns its place because `create` gives no signal at all. Measured on
//! 2026-08-04, against a universe belonging to somebody else:
//!
//! - `can-manage` said no.
//! - `rbx-apikey create` **succeeded**, and post-create introspection confirmed
//!   the scopes. Roblox does not check ownership when a key is made.
//! - Using that key on a scoped endpoint failed with `The authorized user does
//!   not have sufficient permissions`.
//!
//! So a created key proves nothing, and the refusal only appears at the first
//! real call. This command is the one place the answer is available beforehand.
//!
//! One trap worth knowing while reading results: `GET /cloud/v2/universes/{id}`
//! answers 200 for **any** valid key regardless of what it is bound to. A
//! successful read there is not evidence of permission, and was nearly taken
//! for some.

use anyhow::{bail, Context, Result};
use colored::Colorize;
use reqwest::StatusCode;
use serde::Deserialize;

use rbx_core::api::{build_client_with_user_agent, execute_json, is_api_status};
use rbx_core::GlobalFlags;

use crate::api::games::place_to_universe;

/// The `develop` family, not Open Cloud. There is no Open Cloud equivalent.
const DEVELOP_HOST: &str = "https://develop.roblox.com";

/// Some legacy endpoints reject the default reqwest agent.
const USER_AGENT: &str = "Roblox/WinInet";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UniversePermissions {
    #[serde(default)]
    can_manage: bool,
    #[serde(default)]
    can_cloud_edit: bool,
}

pub async fn run(global: &GlobalFlags, places: &[u64]) -> Result<()> {
    let Some(cookie) = global.resolve_cookie() else {
        bail!(
            "no .ROBLOSECURITY cookie. This check authenticates as you, not as a key, because \
             a key cannot answer questions about a universe it is not bound to. Sign in to \
             Studio, or pass --cookie."
        );
    };

    let client = build_client_with_user_agent(USER_AGENT);

    // Either explicit places, or the single universe the global flags name.
    // Never a bare number whose kind the tool decides for you.
    let targets: Vec<u64> = if places.is_empty() {
        vec![global.single_universe().context(
            "nothing to check. Pass --place-id <id>, or name a universe with --universe-id <id> \
             or --env <name>. There is no positional form: a place id and a universe id \
             are both plain integers and the two spaces overlap, so guessing which one \
             you meant can answer about a different game.",
        )?]
    } else {
        let mut resolved = Vec::with_capacity(places.len());
        for place_id in places {
            let universe_id = place_to_universe(&client, *place_id)
                .await?
                .with_context(|| {
                    format!("{place_id} is not a place id. Did you mean --universe-id?")
                })?;
            println!(
                "{}",
                format!("place {place_id} is in universe {universe_id}").dimmed()
            );
            resolved.push(universe_id);
        }
        resolved
    };

    let mut refused = 0;

    for universe_id in targets {
        let url = format!("{DEVELOP_HOST}/v1/universes/{universe_id}/permissions");
        let result: Result<UniversePermissions> = execute_json(|| {
            let request = client
                .get(&url)
                .header("Cookie", format!(".ROBLOSECURITY={cookie}"));
            async move { request.send().await.map_err(Into::into) }
        })
        .await;

        match result {
            Ok(permissions) => {
                let verdict = if permissions.can_manage {
                    "yes".green().bold()
                } else {
                    refused += 1;
                    "no".red().bold()
                };
                println!("universe {universe_id}: can create keys {verdict}");
                if permissions.can_manage && !permissions.can_cloud_edit {
                    // Reported, not interpreted. A universe answering
                    // `canCloudEdit: false` accepted every Open Cloud write
                    // tried against it, so no consequence is claimed here.
                    println!("  {}", "canCloudEdit is false".dimmed());
                }
            }
            // 401 means the cookie is stale, 403 means it is fine and you are
            // simply not allowed. Saying which is the difference between "log
            // in again" and "ask the group owner".
            Err(error) if is_api_status(&error, StatusCode::UNAUTHORIZED) => {
                bail!("the cookie was rejected. Sign in to Studio again, or pass --cookie.")
            }
            Err(error) if is_api_status(&error, StatusCode::FORBIDDEN) => {
                refused += 1;
                println!(
                    "universe {universe_id}: can create keys {}",
                    "no".red().bold()
                );
                println!(
                    "  {}",
                    "Roblox refused the question itself, which means no access at all.".dimmed()
                );
            }
            Err(error) => return Err(error.context(format!("checking universe {universe_id}"))),
        }
    }

    if refused > 0 {
        println!();
        println!(
            "{}",
            "For a group-owned experience this is a group role permission, so it is the group \
             owner who can grant it, not you."
                .dimmed()
        );
    }
    Ok(())
}
