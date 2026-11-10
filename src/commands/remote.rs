//! `rbx-apikey list --remote`: every key on the account, not just this project's.
//!
//! `list` (config + lockfile) answers "what does this project declare?".
//! `status --remote` answers "is what this project declares still there?".
//! Neither can see a key the project never created, which is most of them on a
//! real account. This is the view that can.

use std::path::Path;

use anyhow::Result;
use colored::Colorize;

use crate::api::RbxApiKeyClient;
use crate::json::RemoteListDocument;
use crate::{lock, remote_view};
use rbx_core::output::{emit, OutputFormat};
use rbx_core::GlobalFlags;

use super::make_client;

pub async fn run(global: &GlobalFlags, group_id: Option<u64>, format: OutputFormat) -> Result<()> {
    run_with(
        &make_client(global),
        Path::new(lock::FILE),
        group_id,
        format,
    )
    .await
}

async fn run_with(
    client: &RbxApiKeyClient,
    lock_path: &Path,
    group_id: Option<u64>,
    format: OutputFormat,
) -> Result<()> {
    // `?` and not `unwrap_or_default`: an absent lockfile already loads as an
    // empty one, so the default only ever replaced an error, and a lockfile
    // that could not be read then showed every key of the project as
    // untracked.
    let lk = lock::load_from(lock_path)?;

    // Printed rather than assumed: the listing is scoped to whichever cookie is
    // active, and switching the signed-in Studio account changes that silently. Two accounts can hold keys
    // with identical names, so without this line the output is ambiguous.
    //
    // The same call is what proves the session is still live, and it is
    // cached per process, so the document costs no round trip of its own.
    let whoami = client.authenticated_account().await?;
    let keys = remote_view::fetch(client, &lk, group_id).await?;

    // Before the empty check, for the same reason `list` emits an empty
    // document: no keys on the account is an answer with a `.totals.total` of
    // zero, not silence.
    if format.is_json() {
        return emit(&RemoteListDocument::new(
            group_id,
            whoami.id,
            &keys,
            remote_view::lock_entries_missing_remotely(&keys, &lk),
        ));
    }

    let scope = match group_id {
        Some(g) => format!("group {}", g),
        None => format!("user {}", whoami.id),
    };

    if keys.is_empty() {
        println!("{}", format!("No API keys for {}.", scope).yellow());
        return Ok(());
    }

    let t = remote_view::tally(&keys);
    println!(
        "{}",
        format!("API keys on Roblox for {} ({} total):", scope, t.total).cyan()
    );

    let width = keys
        .iter()
        .map(|k| k.name().len())
        .max()
        .unwrap_or(0)
        .min(40);

    for k in &keys {
        let (glyph, paint): (&str, fn(&str) -> colored::ColoredString) = match k.state() {
            remote_view::KeyState::Active => ("✓", |s| s.green()),
            remote_view::KeyState::Expired => ("✗", |s| s.red()),
            remote_view::KeyState::Disabled => ("·", |s| s.yellow()),
        };
        let tag = match &k.tracked {
            remote_view::Tracked::Yes(name) => format!("tracked → {}", name).green(),
            remote_view::Tracked::No => "untracked".dimmed(),
        };
        println!(
            "  {} {:width$}  {:<12}  created {}  {:<16}  {}",
            paint(glyph),
            k.name(),
            k.secret_preview(),
            k.created_date(),
            k.expiry_text(),
            tag,
            width = width
        );
    }

    println!();
    println!(
        "{} tracked by this project, {} untracked ({} expired, {} disabled).",
        t.tracked, t.untracked, t.expired, t.disabled
    );

    let missing = remote_view::lock_entries_missing_remotely(&keys, &lk);
    if !missing.is_empty() {
        println!(
            "{}",
            format!(
                "⚠  {} lockfile entry(ies) no longer exist on the account: {}. Run `rbx-apikey delete <name>` to clean the lockfile.",
                missing.len(),
                missing.join(", ")
            )
            .yellow()
        );
    }

    if t.untracked > 0 {
        println!(
            "{}",
            "Tip: `rbx-apikey prune` selects among these and deletes what you pick.".cyan()
        );
    }
    if group_id.is_none() {
        println!(
            "{}",
            "Note: this lists the authenticated user's own keys. Group-owned keys need --group-id."
                .dimmed()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::MockServer;

    /// A lockfile that cannot be looked at stops `list --remote` before it
    /// asks Roblox anything. Defaulted to empty, it labelled every key this
    /// project tracks as untracked, which is the listing `prune` invites you
    /// to delete from.
    #[tokio::test]
    async fn a_lockfile_that_cannot_be_looked_at_stops_the_listing() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let lock_path = crate::test_support::unlookable(dir.path());
        let server = MockServer::start().await;
        let client = RbxApiKeyClient::new(Some("remote-unlookable".to_string()))
            .with_base_url(server.uri())
            .with_users_base_url(server.uri());

        let err = run_with(
            &client,
            &lock_path,
            None,
            OutputFormat::from_json_flag(true),
        )
        .await
        .expect_err("the lockfile cannot be looked at");

        assert!(format!("{err:#}").contains("failed to look at"), "{err:#}");
        assert!(server
            .received_requests()
            .await
            .expect("recording")
            .is_empty());
    }
}
