//! `rbx-apikey update <key>|--all`: re-apply key config from rbxapikey.toml.

use std::path::Path;

use anyhow::{bail, Result};
use colored::Colorize;
use dialoguer::Confirm;

use crate::api::api_keys::ConfigProperties;
use crate::api::RbxApiKeyClient;
use crate::{config, git_guard, lock, owner_resolver, scope_builder, secret_store};
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
        v
    } else {
        vec![name.unwrap().to_string()]
    };
    // What the run will do, decided once. Every secret it moves is checked
    // against git before the first question and the first change: a refusal
    // on the third key of `--all` would leave the first two moved and updated
    // for a reason known from the start. The loop below does exactly this.
    let plan = plan(&cfg, &lk, &names);
    super::guard_plan(
        &plan,
        Option::as_ref,
        Path::new(lock::FILE),
        git_guard::Write::Move,
    )?;
    if all {
        say!("Updating {} keys: {}", names.len(), names.join(", "));
    }

    crate::drift::check_all(&lk, places.as_ref())?;

    // Before the first PATCH, not during the third: `--all` walks the list one
    // key at a time, so a session that dies halfway is the difference between
    // "nothing changed" and "keys 1 and 2 carry the new config and key 3 does
    // not". One cached call, see `RbxApiKeyClient::require_valid_session`.
    client.require_valid_session().await?;

    // No save after the loop: each key records itself as soon as Roblox has
    // taken its update. See `update_one`.
    for (n, step) in &plan {
        match step {
            Err(note) => say!("{}", note.yellow()),
            Ok(move_to) => {
                update_one(
                    &cfg,
                    places.as_ref(),
                    &mut lk,
                    &client,
                    n,
                    move_to.as_ref(),
                    no_ip,
                    clean_files,
                    skip_confirm,
                    Path::new(lock::FILE),
                )
                .await?
            }
        }
    }

    Ok(())
}

/// Where a key's secret moves to, when `secret_file` changed since it was
/// last written; `None` when it stays, or is held outside this tool.
fn move_target(
    cfg: &config::Config,
    key_cfg: &config::KeyConfig,
    entry: &lock::LockEntry,
    name: &str,
) -> Option<secret_store::Resolved> {
    let previous = secret_store::previous_backend_from_entry(entry, name);
    if previous.backend == secret_store::Backend::External {
        return None;
    }
    let new = secret_store::backend_for(cfg, Some(key_cfg), name);
    secret_store::backend_differs(&previous, &new).then_some(new)
}

/// Each key of the run and where its secret moves, `None` when it stays, or
/// why the key is skipped: not in the lockfile, or not declared.
fn plan(
    cfg: &config::Config,
    lk: &lock::Lock,
    names: &[String],
) -> super::Plan<Option<secret_store::Resolved>> {
    names
        .iter()
        .map(|n| {
            let step = match (lock::get(lk, n), config::get(cfg, n)) {
                (None, _) => Err(format!("skipping \"{}\": not in {}", n, lock::FILE)),
                (Some(_), None) => Err(super::missing_key_note(cfg, n)),
                (Some(entry), Some(key_cfg)) => Ok(move_target(cfg, key_cfg, entry, n)),
            };
            (n.clone(), step)
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
async fn update_one(
    cfg: &config::Config,
    places: Option<&PlacesFile>,
    lk: &mut lock::Lock,
    client: &RbxApiKeyClient,
    name: &str,
    move_to: Option<&secret_store::Resolved>,
    no_ip: bool,
    clean_files: bool,
    skip_confirm: bool,
    lock_path: &Path,
) -> Result<()> {
    // `plan` skipped the keys missing from either file.
    let entry = lock::get(lk, name)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("\"{name}\" is not in {}", lock::FILE))?;
    let key_cfg = config::get(cfg, name)
        .ok_or_else(|| anyhow::anyhow!("\"{name}\" is not in {}", config::FILE))?;

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
    // `move_to` is where `plan` said the secret moves, which is where the git
    // guard looked; when it does not move, it stays where the config puts it.
    let new_resolved = move_to
        .cloned()
        .unwrap_or_else(|| secret_store::backend_for(cfg, Some(key_cfg), name));
    let previous_resolved = secret_store::previous_backend_from_entry(&entry, name);
    // A secret printed with --stdout is held elsewhere: there is nothing here
    // to migrate, and the entry keeps saying where it is.
    let held_elsewhere = previous_resolved.backend == secret_store::Backend::External;
    let needs_migration = move_to.is_some();
    let mut entry = entry;

    if needs_migration {
        let secret = secret_store::read(&previous_resolved, Some(&entry))?.ok_or_else(|| {
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
    // On disk now, and not after the last key of an `--all`. Roblox already
    // carries this key's new config, and a migrated secret is about to lose
    // its old file below. Saved once at the end, a refusal or a "no" on a
    // later key threw this record away with the run: a lockfile still pointing
    // at a secret file that had just been deleted.
    lock::save_to(lk, lock_path)?;

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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::lock::LockEntry;
    use serde_json::json;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Two keys, both to be disabled by the update, so a key the run has
    /// reached reads `is_enabled = false` in the lockfile.
    const CONFIG: &str = r#"
[settings]
default_allowed_cidrs = ["203.0.113.0/24"]
default_enabled = false

[keys.first]
scopes = ["universe:read"]

[keys.second]
scopes = ["universe:read"]
"#;

    const CREATOR: u64 = 1234567890;

    fn entry(id: &str) -> LockEntry {
        LockEntry {
            cloud_auth_id: id.into(),
            secret: Some("not-a-real-secret".into()),
            secret_file: None,
            secret_external: false,
            secret_unsaved: false,
            creator_id: CREATOR,
            is_enabled: true,
            created_at: "now".into(),
            expires_at: None,
        }
    }

    fn config_in(dir: &Path, key: &str) -> config::Config {
        let path = dir.join(config::FILE);
        std::fs::write(
            &path,
            format!("[settings]\ndefault_allowed_cidrs = [\"203.0.113.0/24\"]\n\n{key}"),
        )
        .unwrap();
        config::load_from(&path).unwrap()
    }

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| s.to_string()).collect()
    }

    /// The run's guard, as `run` calls it: on the plan the loop then follows.
    fn guarded(
        cfg: &config::Config,
        lk: &lock::Lock,
        names: Vec<String>,
        lock_path: &Path,
    ) -> Result<()> {
        super::super::guard_plan(
            &plan(cfg, lk, &names),
            Option::as_ref,
            lock_path,
            git_guard::Write::Move,
        )
    }

    /// The plan says which secrets move and where, and skips a key missing
    /// from either file.
    #[test]
    fn the_plan_says_which_secrets_move() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg, lk) = three_moves(dir.path());

        let steps = plan(&cfg, &lk, &names(&["first", "second", "third", "nope"]));
        assert_eq!(
            steps[0].1.as_ref().unwrap().as_ref().unwrap().backend,
            secret_store::Backend::File
        );
        assert_eq!(
            steps[1].1.as_ref().unwrap().as_ref().unwrap().backend,
            secret_store::Backend::Lockfile
        );
        assert!(steps[2].1.as_ref().unwrap().is_none());
        assert!(steps[3].1.is_err());
    }

    /// `first` gains a `secret_file`, so its secret moves out of the
    /// lockfile; `second` loses one, so its secret moves into the lockfile;
    /// `third` stays where it is.
    fn three_moves(dir: &Path) -> (config::Config, lock::Lock) {
        let new_file = dir.join(".secrets").join("first.env");
        let old_file = dir.join(".secrets").join("second.env");
        let cfg = config_in(
            dir,
            &format!(
                "[keys.first]\nscopes = [\"universe:read\"]\nsecret_file = '{}'\n\n\
                 [keys.second]\nscopes = [\"universe:read\"]\n\n\
                 [keys.third]\nscopes = [\"universe:read\"]\n",
                new_file.display()
            ),
        );
        let mut lk = lock::Lock::default();
        lock::set(&mut lk, "first", entry("id-first"));
        let mut second = entry("id-second");
        second.secret = None;
        second.secret_file = Some(old_file.to_string_lossy().to_string());
        lock::set(&mut lk, "second", second);
        lock::set(&mut lk, "third", entry("id-third"));
        (cfg, lk)
    }

    /// A move to a secret file git does not ignore is refused, and so is one
    /// into a lockfile git does not ignore, each before the first prompt.
    #[test]
    fn a_move_git_does_not_ignore_is_refused() {
        let dir = git_guard::tests::repo();
        let lock_path = dir.path().join(lock::FILE);
        let (cfg, lk) = three_moves(dir.path());

        std::fs::write(dir.path().join(".gitignore"), "rbxapikey.lock.toml\n").unwrap();
        let err = guarded(&cfg, &lk, names(&["first"]), &lock_path).unwrap_err();
        assert!(format!("{err}").contains("first.env"), "{err}");
        guarded(&cfg, &lk, names(&["second"]), &lock_path).unwrap();

        std::fs::write(dir.path().join(".gitignore"), ".secrets/\n").unwrap();
        let err = guarded(&cfg, &lk, names(&["second"]), &lock_path).unwrap_err();
        assert!(format!("{err}").contains(lock::FILE), "{err}");
        assert!(
            format!("{err}").contains("Nothing has been changed"),
            "{err}"
        );
        guarded(&cfg, &lk, names(&["first"]), &lock_path).unwrap();
    }

    /// `--all` is refused as a whole, naming every key whose move was
    /// refused, and a key whose secret stays put is not one of them: nothing
    /// is written for it.
    #[test]
    fn update_all_names_every_refused_move_and_only_those() {
        let dir = git_guard::tests::repo();
        let lock_path = dir.path().join(lock::FILE);
        let (cfg, lk) = three_moves(dir.path());

        let err = guarded(&cfg, &lk, names(&["first", "second", "third"]), &lock_path).unwrap_err();

        let text = format!("{err}");
        assert!(text.contains("first.env (first)"), "{text}");
        assert!(text.contains("(second)"), "{text}");
        assert!(!text.contains("third"), "{text}");
    }

    /// `--all` walks the keys one at a time. Roblox takes the first update and
    /// refuses the second. The record of the first has to be on disk by then:
    /// saved once after the loop, it went with the run, and the lockfile kept
    /// describing a key Roblox no longer held that way.
    #[tokio::test]
    async fn a_key_already_updated_is_on_disk_when_a_later_one_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join(config::FILE);
        std::fs::write(&config_path, CONFIG).unwrap();
        let cfg = config::load_from(&config_path).unwrap();

        let lock_path = dir.path().join(lock::FILE);
        let mut lk = lock::Lock::default();
        lock::set(&mut lk, "first", entry("id-first"));
        lock::set(&mut lk, "second", entry("id-second"));
        lock::save_to(&lk, &lock_path).unwrap();

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/users/authenticated"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "id": CREATOR, "name": "someone" })),
            )
            .mount(&server)
            .await;
        // Mounted first, so it answers the second key's update: wiremock takes
        // the earliest matching mock.
        Mock::given(method("PATCH"))
            .and(path("/cloud-authentication/v1/apiKey"))
            .and(body_partial_json(json!({ "cloudAuthId": "id-second" })))
            .respond_with(ResponseTemplate::new(400).set_body_string("refused"))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/cloud-authentication/v1/apiKey"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "cloudAuthInfo": { "id": "id-first" } })),
            )
            .mount(&server)
            .await;
        let client = RbxApiKeyClient::new(Some("update-all-refused".to_string()))
            .with_base_url(server.uri())
            .with_users_base_url(server.uri());
        client.require_valid_session().await.unwrap();

        update_one(
            &cfg, None, &mut lk, &client, "first", None, false, false, true, &lock_path,
        )
        .await
        .expect("the first key is updated");
        update_one(
            &cfg, None, &mut lk, &client, "second", None, false, false, true, &lock_path,
        )
        .await
        .expect_err("Roblox refuses the second");

        let on_disk = lock::load_from(&lock_path).unwrap();
        assert!(
            !lock::get(&on_disk, "first").unwrap().is_enabled,
            "the first key's update is recorded"
        );
        assert!(
            lock::get(&on_disk, "second").unwrap().is_enabled,
            "the refused key's record is as it was"
        );
    }
}
