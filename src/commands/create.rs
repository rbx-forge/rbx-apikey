//! `rbx-apikey create <key>|--all`: generate a new Open Cloud API key.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{bail, Result};
use colored::Colorize;
use dialoguer::MultiSelect;

use crate::api::api_keys::{ConfigProperties, CreateApiKeyResponse};
use crate::api::RbxApiKeyClient;
use crate::git_guard;
use crate::scope_builder::ScopeDef;
use crate::{config, lock, owner_resolver, scope_builder, secret_store, time_iso};
use rbx_core::places::PlacesFile;
use rbx_core::GlobalFlags;

use super::update::{build_cidrs, build_description};
use super::{explain_invalid_name_or_description, make_client, require_no_collision};

/// Name the account and ask, before a credential is minted on it.
///
/// `create` used to print `Creator: user_id=<n>` and go straight on, which is
/// the one thing the other write verbs do not do, and the worst place for it.
/// A key is minted on whichever account the cookie signs in as, Studio
/// auto-detection follows whichever account Studio happens to be signed into,
/// and the numeric id printed after the fact is not something a person can act
/// on. The docs claimed all five write verbs stopped to ask; four did.
fn confirm_creator(
    account: &rbx_core::session::SessionAccount,
    what: &str,
    yes: bool,
) -> Result<()> {
    rbx_core::confirm::confirm_always(
        &format!("Create {what} on account {}?", account.label()),
        yes,
    )
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    global: &GlobalFlags,
    name: Option<&str>,
    all: bool,
    no_ip: bool,
    force: bool,
    skip_verify: bool,
    yes: bool,
    stdout: bool,
) -> Result<()> {
    require_no_collision(all, name)?;
    if stdout {
        // One stream, one secret: the picker could choose several.
        if name.is_none() {
            bail!(
                "--stdout prints one secret: name the key, as in \
                 `rbx-apikey create deploy --stdout`"
            );
        }
        crate::out::reserve_stdout();
    }

    let cfg = config::load()?;
    let mut lk = lock::load()?;
    let places = super::load_places(global)?;
    let client = make_client(global);

    let names: Vec<String> = if all {
        let mut names: Vec<String> = cfg.keys.keys().cloned().collect();
        names.sort();
        if names.is_empty() {
            bail!("no keys in {}", config::FILE);
        }
        names
    } else if let Some(n) = name {
        vec![n.to_string()]
    } else {
        // No args → interactive picker over keys not yet in lock.
        let mut candidates: Vec<String> = cfg
            .keys
            .keys()
            .filter(|n| lock::get(&lk, n).is_none())
            .cloned()
            .collect();
        candidates.sort();

        if candidates.is_empty() {
            say!("All keys in rbxapikey.toml already have entries in the lockfile.");
            say!("  → rotate secret:   rbx-apikey regenerate <key>");
            say!("  → update config:   rbx-apikey update <key>");
            say!("  → start fresh:     rbx-apikey create <key> --force");
            return Ok(());
        }

        let selection = MultiSelect::new()
            .with_prompt("Select keys to create")
            .items(&candidates)
            .interact()?;
        if selection.is_empty() {
            say!("Nothing selected.");
            return Ok(());
        }
        selection
            .into_iter()
            .map(|i| candidates[i].clone())
            .collect()
    };

    let plan = prepare(
        &cfg,
        &lk,
        places.as_ref(),
        &names,
        force,
        stdout,
        Path::new(lock::FILE),
    )?;

    if no_ip {
        say!("{}", "--no-ip: keys accept calls from any IP".yellow());
    }

    let account = client.authenticated_account().await?;
    let creator_id = account.id;
    let what = match (all, name) {
        (true, _) => "every key in rbxapikey.toml".to_string(),
        (false, Some(n)) => format!("\"{n}\""),
        (false, None) => format!("{} key(s)", names.len()),
    };
    confirm_creator(&account, &what, yes)?;

    if all {
        say!(
            "Creating keys for {} entries: {}",
            names.len(),
            names.join(", ")
        );
    }
    for (n, step) in &plan {
        match step {
            Err(note) => say!("{}", note.yellow()),
            Ok(resolved) => {
                create_one(
                    &cfg,
                    places.as_ref(),
                    &mut lk,
                    &client,
                    n,
                    resolved,
                    creator_id,
                    no_ip,
                    skip_verify,
                )
                .await?;
                lock::save(&lk)?;
            }
        }
    }

    Ok(())
}

/// Everything `run` settles before the first question and before Roblox is
/// asked anything: the drift check, then what the run will write, decided
/// once and checked against git.
///
/// The drift check is the one `update`, `delete` and `regenerate` run. Without
/// it, `create` resolved a repointed env afresh and wrote the new universe
/// into the shared `[envs.<name>]` entry, so the keys already created for the
/// old universe no longer showed as drifted anywhere.
///
/// The guard comes before anything is created, since refusing after creation
/// leaves a live key whose secret was never stored, and the loop in `run`
/// writes exactly the plan returned.
#[allow(clippy::too_many_arguments)]
fn prepare(
    cfg: &config::Config,
    lk: &lock::Lock,
    places: Option<&PlacesFile>,
    names: &[String],
    force: bool,
    stdout: bool,
    lock_path: &Path,
) -> Result<super::Plan<secret_store::Resolved>> {
    crate::drift::check_all(lk, places)?;
    let plan = plan(cfg, lk, names, force, stdout);
    super::guard_plan(&plan, super::writes, lock_path, git_guard::Write::Create)?;
    Ok(plan)
}

/// Each key of the run and where its secret goes, or why it is skipped: not
/// declared, or already in the lockfile without `--force`. Under `--stdout`
/// the secret goes nowhere on disk.
fn plan(
    cfg: &config::Config,
    lk: &lock::Lock,
    names: &[String],
    force: bool,
    stdout: bool,
) -> super::Plan<secret_store::Resolved> {
    names
        .iter()
        .map(|n| {
            let step = match config::get(cfg, n) {
                None => Err(super::missing_key_note(cfg, n)),
                Some(_) if lock::get(lk, n).is_some() && !force => Err(format!(
                    "skipping \"{}\": already in {} (use --force to overwrite)",
                    n,
                    lock::FILE
                )),
                Some(_) if stdout => Ok(secret_store::external()),
                Some(key_cfg) => Ok(secret_store::backend_for(cfg, Some(key_cfg), n)),
            };
            (n.clone(), step)
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
async fn create_one(
    cfg: &config::Config,
    places: Option<&PlacesFile>,
    lk: &mut lock::Lock,
    client: &RbxApiKeyClient,
    name: &str,
    resolved: &secret_store::Resolved,
    creator_id: u64,
    no_ip: bool,
    skip_verify: bool,
) -> Result<()> {
    // `plan` skipped the keys that are not declared.
    let key_cfg = config::get(cfg, name)
        .ok_or_else(|| anyhow::anyhow!("\"{name}\" is not in {}", config::FILE))?;
    let stdout = resolved.backend == secret_store::Backend::External;

    // Sync this key's targets into the lockfile's shared `[envs.X]` table.
    // `sync_targets` does the `rbxplace.toml` lookup and reuses cached owners
    // when they're still valid.
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

    let payload = ConfigProperties {
        name: config::resolve_remote_name(cfg, key_cfg, name),
        description: build_description(name, key_cfg, &universe_ids),
        is_enabled,
        expiration_time: expiration_time.clone(),
        allowed_cidrs: cidrs.clone(),
        scopes: build.scopes.clone(),
    };

    say!(
        "Creating \"{}\"... ({} scope entries)",
        name,
        build.scopes.len()
    );

    let resp: CreateApiKeyResponse = client
        .create_api_key(&payload)
        .await
        .map_err(|e| explain_invalid_name_or_description(e, &payload.name, &payload.description))
        .map_err(|e| super::explain_opaque_server_error(e, &payload.scopes))?;
    let info = resp.cloud_auth_info;
    let secret = resp.apikey_secret;
    if info.id.is_empty() || secret.is_empty() {
        bail!("create failed for \"{}\" - unexpected response", name);
    }

    // Roblox sometimes ignores expirationTime on CREATE; re-apply via PATCH if missing.
    let response_expiry = info
        .cloud_auth_user_configured_properties
        .as_ref()
        .and_then(|p| p.expiration_time.clone());
    if response_expiry.is_none() && expiration_time.is_some() {
        // Best-effort: the key already exists and we hold its secret. If this
        // re-PATCH fails, do NOT abort - that would discard the secret and
        // orphan the key on Roblox. Warn and continue to persist it.
        if let Err(e) = client.update_api_key(&info.id, &payload).await {
            eprintln!(
                "{}",
                format!(
                    "warning: key created but re-applying expiration failed: {e}\n  \
                     the key works; verify its expiry with `rbx-apikey status --remote`"
                )
                .yellow()
            );
        }
    }

    // Where `plan` said, which is where the git guard looked.
    let secret_file_target = if resolved.backend == secret_store::Backend::File {
        Some(resolved.target.clone())
    } else {
        None
    };

    let mut entry = lock::LockEntry {
        cloud_auth_id: info.id.clone(),
        secret: None,
        secret_file: secret_file_target,
        secret_external: false,
        creator_id,
        is_enabled,
        created_at: info.created_time.clone().unwrap_or_else(time_iso::iso_now),
        expires_at: expiration_time.clone(),
    };

    if let Err(e) = secret_store::write(resolved, &secret, &mut entry) {
        bail!(
            "\"{}\" key created on Roblox but failed to store secret: {}\n  Roblox cloud_auth_id: {}",
            name,
            e,
            info.id
        );
    }

    if stdout {
        // Printed as soon as it is known, before anything else can fail: Roblox
        // will never show this secret again.
        crate::out::emit_secret(&secret).map_err(|e| {
            anyhow::anyhow!(
                "\"{name}\" key created on Roblox but printing its secret failed: {e}\n  \
                 Run `rbx-apikey regenerate {name} --stdout` for a new one. \
                 Roblox cloud_auth_id: {}",
                info.id
            )
        })?;
    }

    // Merge the freshly-synced envs back so other keys' envs persist.
    for (env_name, lock_env) in synced {
        lk.envs.insert(env_name, lock_env);
    }
    lock::set(lk, name, entry);

    let where_str = match resolved.backend {
        secret_store::Backend::File => format!(" (secret → {})", resolved.target),
        secret_store::Backend::External => " (secret printed on stdout)".to_string(),
        secret_store::Backend::Lockfile => String::new(),
    };
    say!(
        "{}",
        format!(
            "✓ Created \"{}\" (id={}, expires={}){}",
            name,
            info.id,
            expiration_time.as_deref().unwrap_or("(none)"),
            where_str
        )
        .green()
    );

    if !skip_verify {
        verify_against_introspect(client, &secret, &build.scopes).await;
    }

    Ok(())
}

/// The permissions a set of scope entries grants, as comparable triples.
///
/// A scope entry is `{scopeType, targetParts[], operations[]}`, and the same
/// permissions can be arranged in more than one entry: split per target,
/// merged per type, ordered either way. Flattening to
/// `(type, target, operation)` is what survives the arrangement, and it is
/// what "did the key get what was asked for" actually means.
fn permissions(scopes: &[ScopeDef]) -> BTreeSet<(String, String, String)> {
    let mut out = BTreeSet::new();
    for s in scopes {
        for target in &s.target_parts {
            for op in &s.operations {
                out.insert((s.scope_type.clone(), target.clone(), op.clone()));
            }
        }
    }
    out
}

/// Read back the key that was just created and say whether it grants what was
/// asked for.
///
/// Compares permissions rather than the **number** of scope entries, which is
/// what it once compared. Roblox is free to store the same permissions in a
/// different arrangement, and every such rearrangement was reported as drift
/// on a key created exactly as requested. Counting is also weaker in the
/// direction that matters: two counts can match while one permission was
/// dropped and another gained.
async fn verify_against_introspect(client: &RbxApiKeyClient, secret: &str, sent: &[ScopeDef]) {
    let resp = match client.introspect_api_key(secret).await {
        Err(e) => {
            say!(
                "{}",
                // `{:#}` and not `{}`: the error arrives wrapped in context,
                // and `{}` renders only the outermost frame. That printed
                // "skipped: introspecting the key", which names the operation
                // and hides the reason, on the one check that says whether
                // Roblox stored the scopes as they were sent.
                format!("  (skipped post-create introspect verification: {:#})", e).yellow()
            );
            return;
        }
        Ok(resp) => resp,
    };

    let stored: Vec<ScopeDef> = match crate::introspect::scopes_from_response(&resp) {
        None => {
            say!("{}", "  (introspect returned an unexpected shape)".yellow());
            return;
        }
        Some(Err(why)) => {
            // Reported rather than swallowed. A parser that drops what it does
            // not understand turns "we could not read the answer" into "the key
            // grants nothing", which is how the shape mismatch found on
            // 2026-08-16 stayed invisible.
            say!(
                "{}",
                format!("  (introspect answered in a shape this build cannot read: {why})")
                    .yellow()
            );
            return;
        }
        Some(Ok(scopes)) => scopes,
    };

    let asked = permissions(sent);
    let got = permissions(&stored);

    let missing: Vec<String> = asked.difference(&got).map(spell_permission).collect();
    let extra: Vec<String> = got.difference(&asked).map(spell_permission).collect();

    if missing.is_empty() && extra.is_empty() {
        say!(
            "{}",
            format!(
                "  ✓ Introspect verified: {} permission(s) match the request",
                asked.len()
            )
            .green()
        );
        return;
    }

    // Missing is the alarm: the key cannot do something it was created to do,
    // and every later failure will be a permission error on the resource
    // rather than anything naming the key.
    if !missing.is_empty() {
        say!(
            "{}",
            format!("  ⚠ Roblox did not store: {}", missing.join(", ")).yellow()
        );
    }
    // Extra is rarer and worth its own line rather than the same one: a key
    // holding more than it was asked for is a scope boundary that is wider
    // than the config says.
    if !extra.is_empty() {
        say!(
            "{}",
            format!("  ⚠ Roblox stored, unasked: {}", extra.join(", ")).yellow()
        );
    }
}

/// `universe:read on 123`, the way the config would have spelled it.
fn spell_permission((scope_type, target, op): &(String, String, String)) -> String {
    format!("{scope_type}:{op} on {target}")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn def(scope_type: &str, targets: &[&str], ops: &[&str]) -> ScopeDef {
        ScopeDef {
            scope_type: scope_type.to_string(),
            target_parts: targets.iter().map(|s| s.to_string()).collect(),
            operations: ops.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// A config in `dir` declaring `deploy` with its secret in the lockfile
    /// and `ci` with its secret in `.secrets/ci.env`, both inside `dir`.
    fn two_destinations(dir: &Path) -> config::Config {
        let path = dir.join(config::FILE);
        let secret_file = dir.join(".secrets").join("ci.env");
        std::fs::write(
            &path,
            format!(
                "[keys.deploy]\nscopes = [\"universe:read\"]\n\n\
                 [keys.ci]\nscopes = [\"universe:read\"]\nsecret_file = '{}'\n",
                secret_file.display()
            ),
        )
        .unwrap();
        config::load_from(&path).unwrap()
    }

    /// The run's checks, as `run` makes them, with no `rbxplace.toml`.
    fn guarded(
        cfg: &config::Config,
        lk: &lock::Lock,
        names: &[&str],
        force: bool,
        stdout: bool,
        lock_path: &Path,
    ) -> Result<()> {
        let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        prepare(cfg, lk, None, &names, force, stdout, lock_path).map(|_| ())
    }

    /// `create` ran no drift check while `update`, `delete` and `regenerate`
    /// did. A lockfile env that `rbxplace.toml` has since repointed stops it
    /// before anything is asked or created.
    #[test]
    fn a_drifted_env_stops_create_before_anything_happens() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = two_destinations(dir.path());
        let places_path = dir.path().join("rbxplace.toml");
        std::fs::write(&places_path, "[dev]\nuniverse_id = 200\n").unwrap();
        let places = PlacesFile::load(&places_path).unwrap();
        let mut lk = lock::Lock::default();
        lk.envs.insert(
            "dev".into(),
            lock::LockEnv {
                universe_id: 100,
                owner_type: rbx_core::owner::OwnerType::User,
                owner_id: 0,
            },
        );
        let names = vec!["ci".to_string()];
        let lock_path = dir.path().join(lock::FILE);

        let err = prepare(&cfg, &lk, Some(&places), &names, false, false, &lock_path)
            .unwrap_err()
            .to_string();
        assert!(err.contains("'dev'"), "{err}");
        assert!(err.contains("200"), "{err}");

        // The same env in step with the file is no reason to stop.
        lk.envs.get_mut("dev").unwrap().universe_id = 200;
        prepare(&cfg, &lk, Some(&places), &names, false, false, &lock_path).unwrap();
    }

    fn tracked_entry() -> lock::LockEntry {
        lock::LockEntry {
            cloud_auth_id: "aaaaBBBBccccDDDD".into(),
            secret: Some("not-a-real-secret".into()),
            secret_file: None,
            secret_external: false,
            creator_id: 1234567890,
            is_enabled: true,
            created_at: "now".into(),
            expires_at: None,
        }
    }

    /// The lockfile: the default destination, and the one the guard covered
    /// from the start.
    #[test]
    fn a_lockfile_git_does_not_ignore_stops_create() {
        let dir = git_guard::tests::repo();
        std::fs::write(dir.path().join(".gitignore"), ".secrets/\n").unwrap();
        let cfg = two_destinations(dir.path());
        let lock_path = dir.path().join(lock::FILE);

        let lk = lock::Lock::default();

        let err = guarded(&cfg, &lk, &["deploy"], false, false, &lock_path).unwrap_err();
        assert!(
            format!("{err}").contains("Nothing has been created"),
            "{err}"
        );
        guarded(&cfg, &lk, &["ci"], false, false, &lock_path).unwrap();
    }

    /// The plan the guard and the loop share: where each secret goes, or why
    /// the key is skipped.
    #[test]
    fn the_plan_says_where_each_secret_goes_or_why_not() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = two_destinations(dir.path());
        let mut lk = lock::Lock::default();
        lock::set(&mut lk, "deploy", tracked_entry());
        let names: Vec<String> = ["deploy", "ci", "nope"].map(String::from).to_vec();

        let steps = plan(&cfg, &lk, &names, false, false);
        assert!(steps[0].1.as_ref().unwrap_err().contains("already in"));
        assert_eq!(
            steps[1].1.as_ref().unwrap().backend,
            secret_store::Backend::File
        );
        assert!(steps[2].1.is_err());

        let forced = plan(&cfg, &lk, &names, true, false);
        assert_eq!(
            forced[0].1.as_ref().unwrap().backend,
            secret_store::Backend::Lockfile
        );
        let printed = plan(&cfg, &lk, &names, true, true);
        assert_eq!(
            printed[1].1.as_ref().unwrap().backend,
            secret_store::Backend::External
        );
    }

    /// `--all` is refused as a whole, naming every key that was refused and
    /// not just the first: one file per attempt would send the reader round
    /// once per file.
    #[test]
    fn create_all_names_every_refused_key() {
        let dir = git_guard::tests::repo();
        let cfg = two_destinations(dir.path());
        let lock_path = dir.path().join(lock::FILE);
        let err = guarded(
            &cfg,
            &lock::Lock::default(),
            &["ci", "deploy"],
            false,
            false,
            &lock_path,
        )
        .unwrap_err();

        let text = format!("{err}");
        assert!(text.contains("(deploy)"), "{text}");
        assert!(text.contains("ci.env (ci)"), "{text}");
    }

    /// A key already in the lockfile is skipped without `--force`, so nothing
    /// is written for it and nothing about it is refused. `--stdout` writes
    /// nothing either.
    #[test]
    fn a_key_that_will_not_be_written_is_not_refused() {
        let dir = git_guard::tests::repo();
        let cfg = two_destinations(dir.path());
        let lock_path = dir.path().join(lock::FILE);
        let mut lk = lock::Lock::default();
        lock::set(&mut lk, "deploy", tracked_entry());

        guarded(&cfg, &lk, &["deploy"], false, false, &lock_path).unwrap();
        assert!(guarded(&cfg, &lk, &["deploy"], true, false, &lock_path).is_err());
        guarded(&cfg, &lk, &["deploy"], true, true, &lock_path).unwrap();
    }

    /// A `secret_file` git does not ignore, which `create` used to pass on the
    /// grounds that the path was the user's choice. The docs told the reader
    /// to gitignore `.secrets/` because `create` would refuse otherwise.
    #[test]
    fn a_secret_file_git_does_not_ignore_stops_create() {
        let dir = git_guard::tests::repo();
        std::fs::write(dir.path().join(".gitignore"), "rbxapikey.lock.toml\n").unwrap();
        let cfg = two_destinations(dir.path());
        let lock_path = dir.path().join(lock::FILE);

        let lk = lock::Lock::default();

        let err = guarded(&cfg, &lk, &["deploy", "ci"], false, false, &lock_path).unwrap_err();
        assert!(format!("{err}").contains("ci.env"), "{err}");
        guarded(&cfg, &lk, &["deploy"], false, false, &lock_path).unwrap();
    }

    /// The skipped-introspect line reported "introspecting the key" and
    /// nothing else, because `{}` on an `anyhow::Error` renders only the
    /// outermost frame and the client wraps the failure in exactly that
    /// context. Asserted on the rendering rather than left to a comment: the
    /// difference is one character, and this is the check that says whether
    /// Roblox stored the scopes as they were sent.
    #[test]
    fn a_wrapped_error_renders_its_cause_and_not_just_its_context() {
        let wrapped = anyhow::Error::from(rbx_core::api::ApiError::new(
            reqwest::StatusCode::UNAUTHORIZED,
            "Invalid API key",
        ))
        .context("introspecting the key");

        let alternate = format!("{wrapped:#}");
        assert!(alternate.contains("introspecting the key"), "{alternate}");
        assert!(alternate.contains("Invalid API key"), "{alternate}");

        // What it used to print, kept as the contrast.
        assert_eq!(format!("{wrapped}"), "introspecting the key");
    }

    /// The whole point of comparing permissions: Roblox may store the same permissions in a
    /// different arrangement (one entry per target, or two types merged)
    /// and that is not drift. Counting entries called it drift on a key
    /// created exactly as asked.
    #[test]
    fn the_same_permissions_arranged_differently_are_equal() {
        let sent = vec![def("universe", &["1", "2"], &["read", "write"])];
        let stored = vec![
            def("universe", &["1"], &["read"]),
            def("universe", &["1"], &["write"]),
            def("universe", &["2"], &["read"]),
            def("universe", &["2"], &["write"]),
        ];

        assert_eq!(sent.len(), 1, "one entry sent");
        assert_eq!(
            stored.len(),
            4,
            "four stored: the old check called this drift"
        );
        assert_eq!(permissions(&sent), permissions(&stored));
    }

    /// And the direction counting was weakest in: the same number of entries,
    /// one permission dropped and another gained. The old check reported a
    /// match.
    #[test]
    fn one_permission_swapped_for_another_is_not_a_match() {
        let sent = vec![def("universe", &["1"], &["read"])];
        let stored = vec![def("universe", &["1"], &["write"])];

        assert_eq!(sent.len(), stored.len(), "counts agree, contents do not");

        let asked = permissions(&sent);
        let got = permissions(&stored);
        let missing: Vec<String> = asked.difference(&got).map(spell_permission).collect();
        let extra: Vec<String> = got.difference(&asked).map(spell_permission).collect();

        assert_eq!(missing, vec!["universe:read on 1"]);
        assert_eq!(extra, vec!["universe:write on 1"]);
    }

    /// The two directions are reported apart because they mean different
    /// things: missing is a key that cannot do its job, extra is a boundary
    /// wider than the config asked for.
    #[test]
    fn a_permission_nobody_asked_for_is_reported_on_its_own() {
        let sent = vec![def("universe", &["1"], &["read"])];
        let stored = vec![def("universe", &["1"], &["read", "write"])];

        let asked = permissions(&sent);
        let got = permissions(&stored);
        assert!(asked.difference(&got).next().is_none(), "nothing missing");
        assert_eq!(
            got.difference(&asked)
                .map(spell_permission)
                .collect::<Vec<_>>(),
            vec!["universe:write on 1"]
        );
    }

    /// The mirrored-warnings bug, end to end: the request of the repro against the response
    /// Roblox actually returns for it. The two warnings used to mirror each
    /// other, which is the signature of a target that was not read rather
    /// than a permission that was not stored.
    #[test]
    fn the_response_of_issue_37_is_not_drift() {
        let sent = vec![
            def(
                "universe-datastores.control",
                &["109876543210987"],
                &["list"],
            ),
            def(
                "universe-datastores.objects",
                &["109876543210987"],
                &["read"],
            ),
            def("asset", &["G1234567890"], &["read"]),
        ];
        let response = serde_json::json!({
            "scopes": [
                {
                    "name": "universe-datastores.control",
                    "operations": ["list"],
                    "universeIds": ["109876543210987"]
                },
                {
                    "name": "universe-datastores.objects",
                    "operations": ["read"],
                    "universeDatastores": [{ "universeId": "109876543210987" }]
                },
                {
                    "name": "asset",
                    "operations": ["read"],
                    "groupIds": ["1234567890"]
                }
            ]
        });

        let stored = crate::introspect::scopes_from_response(&response)
            .expect("an introspect response")
            .expect("readable scopes");

        let asked = permissions(&sent);
        let got = permissions(&stored);
        let missing: Vec<String> = asked.difference(&got).map(spell_permission).collect();
        let extra: Vec<String> = got.difference(&asked).map(spell_permission).collect();

        assert!(missing.is_empty(), "reported missing: {missing:?}");
        assert!(extra.is_empty(), "reported unasked: {extra:?}");
    }
}
