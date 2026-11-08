//! `rbx-apikey update <key>|--all`: re-apply key config from rbxapikey.toml.

use anyhow::{bail, Result};
use colored::Colorize;
use dialoguer::Confirm;

use crate::api::api_keys::ConfigProperties;
use crate::api::RbxApiKeyClient;
use crate::{config, lock, owner_resolver, scope_builder, secret_store};
use rbx_core::confirm::confirm_always;
use rbx_core::places::PlacesFile;
use rbx_core::GlobalFlags;

use super::{explain_invalid_name_or_description, make_client, require_no_collision};

pub async fn run(
    global: &GlobalFlags,
    name: Option<&str>,
    all: bool,
    no_ip: bool,
    clean_files: bool,
    skip_confirm: bool,
) -> Result<()> {
    require_no_collision(all, name)?;
    if !all && name.is_none() {
        bail!("usage: rbx-apikey update <key>|--all [flags]");
    }

    let cfg = config::load()?;
    let mut lk = lock::load()?;
    let places = super::load_places(global)?;
    let client = make_client(global);

    let names: Vec<String> = if all {
        let mut v: Vec<String> = lk.keys.keys().cloned().collect();
        v.sort();
        if v.is_empty() {
            bail!("no keys in {}", lock::FILE);
        }
        say!("Updating {} keys: {}", v.len(), v.join(", "));
        v
    } else {
        vec![name.unwrap().to_string()]
    };

    crate::drift::check_all(&lk, places.as_ref())?;

    // Before the first PATCH, not during the third: `--all` walks the list one
    // key at a time, so a session that dies halfway is the difference between
    // "nothing changed" and "keys 1 and 2 carry the new config and key 3 does
    // not". One cached call, see `RbxApiKeyClient::require_valid_session`.
    client.require_valid_session().await?;

    for n in &names {
        update_one(
            &cfg,
            places.as_ref(),
            &mut lk,
            &client,
            n,
            no_ip,
            clean_files,
            skip_confirm,
        )
        .await?;
    }

    lock::save(&lk)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn update_one(
    cfg: &config::Config,
    places: Option<&PlacesFile>,
    lk: &mut lock::Lock,
    client: &RbxApiKeyClient,
    name: &str,
    no_ip: bool,
    clean_files: bool,
    skip_confirm: bool,
) -> Result<()> {
    let entry = match lock::get(lk, name).cloned() {
        Some(e) => e,
        None => {
            say!(
                "{}",
                format!("skipping \"{}\": not in {}", name, lock::FILE).yellow()
            );
            return Ok(());
        }
    };

    let key_cfg = match config::get(cfg, name) {
        Some(k) => k,
        None => {
            say!("{}", super::missing_key_note(cfg, name).yellow());
            return Ok(());
        }
    };

    // Drift check is global (per-env) and runs once at command entry: see
    // `run()`. By the time we get here, the cached `lk.envs` entries for the
    // envs this key uses are guaranteed to match `rbxplace.toml`.
    let owner_resolver::ResolvedKey {
        universe_ids,
        owners: universe_owners,
        synced,
    } = owner_resolver::resolve_key(client, cfg, key_cfg, places, &lk.envs).await?;

    let build = scope_builder::build(key_cfg, &universe_ids, &universe_owners);
    for w in &build.warnings {
        say!("{}", format!("  ⚠ {}", w).yellow());
    }

    let cidrs = build_cidrs(cfg, key_cfg, name, no_ip)?;
    let expiration_time = config::get_expiration_time(cfg, key_cfg);
    let is_enabled = config::is_enabled(cfg, key_cfg);

    // Detect secret_file change since the last apply and migrate the secret to the new backend
    // BEFORE the Roblox call. Failing here aborts the whole update so we don't talk to Roblox
    // with the local state in a broken half-state.
    let new_resolved = secret_store::backend_for(cfg, Some(key_cfg), name);
    let previous_resolved = secret_store::previous_backend_from_entry(&entry, name);
    let mut entry = entry;
    // A secret printed with --stdout is held elsewhere: there is nothing here
    // to migrate, and the entry keeps saying where it is.
    let held_elsewhere = previous_resolved.backend == secret_store::Backend::External;
    let needs_migration =
        !held_elsewhere && secret_store::backend_differs(&previous_resolved, &new_resolved);

    if needs_migration {
        let secret = secret_store::read(&previous_resolved, Some(&entry)).ok_or_else(|| {
            anyhow::anyhow!(
                "\"{}\": cannot migrate secret, not found at previous location ({}: {}). Run `rbx-apikey regenerate {}` after updating to create a fresh secret at the new location.",
                name,
                previous_resolved.backend.as_str(),
                previous_resolved.target,
                name
            )
        })?;
        secret_store::write(&new_resolved, &secret, &mut entry).map_err(|e| {
            anyhow::anyhow!(
                "\"{}\": failed to write secret to new location ({}: {}): {}",
                name,
                new_resolved.backend.as_str(),
                new_resolved.target,
                e
            )
        })?;
        say!(
            "{}",
            format!(
                "  ↪ Secret backend changed: {}:{} → {}:{} (migrated)",
                previous_resolved.backend.as_str(),
                previous_resolved.target,
                new_resolved.backend.as_str(),
                new_resolved.target
            )
            .cyan()
        );
    }

    let secret_file_target =
        if !held_elsewhere && new_resolved.backend == secret_store::Backend::File {
            Some(new_resolved.target.clone())
        } else {
            None
        };

    let payload = ConfigProperties {
        name: config::resolve_remote_name(cfg, key_cfg, name),
        description: build_description(name, key_cfg, &universe_ids),
        is_enabled,
        expiration_time: expiration_time.clone(),
        allowed_cidrs: cidrs.clone(),
        scopes: build.scopes,
    };

    // The session check ran before the plan was built, so the account is
    // already known and this costs nothing. It is worth saying because an
    // update rewrites scopes and IP allowlists on a live credential, and the
    // key name does not say whose account holds it.
    let account = client.known_account().await;
    confirm_always(
        &rbx_core::session::as_account(
            account.as_ref(),
            &format!("Push updated config for \"{}\" to Roblox?", name),
        ),
        skip_confirm,
    )?;

    say!("Updating \"{}\" (id={})...", name, entry.cloud_auth_id);
    let resp = client
        .update_api_key(&entry.cloud_auth_id, &payload)
        .await
        .map_err(|e| explain_invalid_name_or_description(e, &payload.name, &payload.description))?;
    let _ = resp.cloud_auth_info; // presence verified by deserialization

    entry.expires_at = expiration_time.clone();
    entry.is_enabled = is_enabled;
    entry.secret_file = secret_file_target;
    // Merge synced envs back so other keys' envs persist.
    for (env_name, lock_env) in synced {
        lk.envs.insert(env_name, lock_env);
    }
    lock::set(lk, name, entry);

    say!(
        "{}",
        format!(
            "✓ \"{}\": cidrs={}, expires={}, enabled={}",
            name,
            cidrs.join(","),
            expiration_time.as_deref().unwrap_or("(none)"),
            is_enabled
        )
        .green()
    );

    // Cleanup the old secret file (if any) only after Roblox confirmed the update: don't destroy
    // local state if the API call failed. Same UX as `delete`: prompt unless --clean-files.
    if needs_migration && previous_resolved.backend == secret_store::Backend::File {
        let should_delete = if clean_files {
            true
        } else {
            Confirm::new()
                .with_prompt(format!(
                    "Also delete old secret file \"{}\"?",
                    previous_resolved.target
                ))
                .default(false)
                .interact()
                .unwrap_or(false)
        };
        let cleanup = secret_store::cleanup(&previous_resolved, should_delete);
        for msg in &cleanup.auto_cleaned {
            say!("{}", format!("  ✓ {}", msg).green());
        }
        for msg in &cleanup.manual_action_needed {
            say!("{}", format!("  ⚠ {}", msg).yellow());
        }
    }

    Ok(())
}

/// The `description` a key is created and updated with.
///
/// **The fallback must not name Roblox or this tool.** Roblox refuses a key
/// whose name or description carries a brand with
/// `Response.InvalidNameOrDescription`; the working rule is reasoned about on
/// [`super::explain_invalid_name_or_description`]. The fallback used to read
/// `Managed by rbxapikey (...)`, which contains `rbx` welded to a commerce
/// term, so it put the failure on the path the documentation recommends:
/// `description` is optional, so a key declared without one got a string the
/// server would not take, answered by a refusal naming neither field.
///
/// The wording loses nothing by dropping the brand. What the string is for is
/// telling a human which declaration a key on the Creator Hub came from, and
/// the declaration name still does that. `the_fallback_never_carries_the_brand`
/// is what keeps it out, because the failure is invisible in review and only
/// appears against the live API.
pub(crate) fn build_description(
    name: &str,
    key_cfg: &config::KeyConfig,
    universe_ids: &[u64],
) -> String {
    if let Some(d) = &key_cfg.description {
        return d.clone();
    }
    if !universe_ids.is_empty() {
        let ids: Vec<String> = universe_ids.iter().map(|u| u.to_string()).collect();
        return format!(
            "Managed declaratively ({}). Universes: {}.",
            name,
            ids.join(", ")
        );
    }
    format!("Managed declaratively ({}).", name)
}

pub(crate) fn build_cidrs(
    cfg: &config::Config,
    key_cfg: &config::KeyConfig,
    name: &str,
    no_ip: bool,
) -> Result<Vec<String>> {
    if no_ip {
        return Ok(vec!["0.0.0.0/0".to_string()]);
    }
    let cidrs = config::get_allowed_cidrs(cfg, key_cfg);
    if !cidrs.is_empty() {
        return Ok(cidrs);
    }
    bail!(
        "\"{}\" has no allowed_cidrs. Either:\n  - Add allowed_cidrs to the key in rbxapikey.toml\n  - Add default_allowed_cidrs to [settings] in rbxapikey.toml\n  - Use --no-ip to allow all IPs",
        name
    );
}
