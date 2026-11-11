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
            say!("  → replace the key: rbx-apikey create <key> --force (deletes it first)");
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

    // `--force` on a key the lockfile tracks deletes that key first. Said
    // before the question, and refused here when the key belongs to another
    // account: Roblox would answer 404 to the delete and the key would stay.
    let replaced = replaced_keys(&plan, &lk);
    let refusals: Vec<String> = replaced
        .iter()
        .filter_map(|(n, entry)| super::delete::wrong_account(n, entry.creator_id, &account))
        .collect();
    if !refusals.is_empty() {
        bail!(
            "{}\nNothing has been created or deleted.",
            refusals.join("\n")
        );
    }
    if !replaced.is_empty() {
        let list: Vec<String> = replaced
            .iter()
            .map(|(n, entry)| format!("\"{n}\" (id={})", entry.cloud_auth_id))
            .collect();
        say!(
            "{}",
            format!(
                "--force replaces {}: each is deleted on Roblox before its new key is created, \
                 and its current secret stops working.",
                list.join(", ")
            )
            .yellow()
        );
    }

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
                create_planned(
                    &cfg,
                    places.as_ref(),
                    &mut lk,
                    &client,
                    n,
                    resolved,
                    creator_id,
                    no_ip,
                    skip_verify,
                    Path::new(lock::FILE),
                )
                .await?
            }
        }
    }

    Ok(())
}

/// The keys of the plan that `--force` replaces: planned, and already in the
/// lockfile. Without `--force` the plan skips those, so this is empty.
fn replaced_keys<'a>(
    plan: &'a super::Plan<secret_store::Resolved>,
    lk: &'a lock::Lock,
) -> Vec<(&'a str, &'a lock::LockEntry)> {
    plan.iter()
        .filter(|(_, step)| step.is_ok())
        .filter_map(|(n, _)| lock::get(lk, n).map(|entry| (n.as_str(), entry)))
        .collect()
}

/// One planned key: the request built and checked, then the key it replaces
/// deleted, if the lockfile tracks one under this name, then the new key
/// created and recorded.
#[allow(clippy::too_many_arguments)]
async fn create_planned(
    cfg: &config::Config,
    places: Option<&PlacesFile>,
    lk: &mut lock::Lock,
    client: &RbxApiKeyClient,
    name: &str,
    resolved: &secret_store::Resolved,
    creator_id: u64,
    no_ip: bool,
    skip_verify: bool,
    lock_path: &Path,
) -> Result<()> {
    let replacing = lock::get(lk, name).is_some();
    let prepared = prepare_key(cfg, places, lk, client, name, no_ip, replacing).await?;
    if replacing {
        replace_tracked(cfg, lk, client, name, resolved, lock_path).await?;
    }
    // Records the key on disk itself, whatever became of its secret.
    let created = create_prepared(
        lk,
        client,
        name,
        resolved,
        creator_id,
        skip_verify,
        prepared,
        lock_path,
    )
    .await?;
    if created == Created::Unsaved {
        bail!(
            "\"{name}\" exists on Roblox and is recorded, but its secret was not stored. The \
             secret printed above works wherever you save it; `rbx-apikey regenerate {name}` \
             stores a new one here."
        );
    }
    Ok(())
}

/// `--force` on a key the lockfile tracks: delete that key on Roblox, and its
/// record with it, before the new one is created.
///
/// `--force` used to create the new key and write it over the record of the
/// old one, which stayed live on Roblox with nothing here naming it: an
/// untracked credential that only `prune` could find. Deleting first means a
/// failure at any step leaves no live key without a record. A delete that
/// fails creates nothing and keeps the record; a create that fails after the
/// delete leaves no key under the name, and `create` makes one. The caller
/// builds the new request before this runs, so what can fail on this side
/// has failed with the old key still working.
///
/// This goes through `delete`'s own path, so it refuses a key of another
/// account and reads a 404 for the owner as a key already gone.
async fn replace_tracked(
    cfg: &config::Config,
    lk: &mut lock::Lock,
    client: &RbxApiKeyClient,
    name: &str,
    resolved: &secret_store::Resolved,
    lock_path: &Path,
) -> Result<()> {
    let stored = lock::get(lk, name)
        .map(|entry| secret_store::stored_backend(cfg, config::get(cfg, name), name, Some(entry)));
    say!("Replacing \"{name}\": deleting the key the lockfile tracks first.");
    // The old secret file goes with the key when the new secret is written to
    // the same file anyway. One elsewhere is named and left for the reader.
    let clean_files = stored
        .as_ref()
        .is_some_and(|old| !secret_store::backend_differs(old, resolved));
    use super::delete::Deletion;
    match super::delete::delete_one(cfg, lk, client, name, true, clean_files, lock_path).await {
        Deletion::Done => Ok(()),
        Deletion::NotDeleted => bail!(
            "\"{name}\": the key --force replaces could not be deleted, so no new key was \
             created and the lockfile still records the old one, which may be live. The reason \
             is above."
        ),
        Deletion::StillRecorded => bail!(
            "\"{name}\": the key --force replaces is deleted on Roblox, but {} could not be \
             saved and still lists it, so no new key was created. Once the file can be \
             written, `rbx-apikey delete {name}` clears the record (Roblox answers 404) and \
             `rbx-apikey create {name}` makes the new key.",
            lock_path.display()
        ),
    }
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
                    "skipping \"{}\": already in {} (--force replaces it, deleting the key it \
                     tracks first)",
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

/// One key's creation request, built and checked in full before anything on
/// Roblox changes.
#[derive(Debug)]
struct Prepared {
    payload: ConfigProperties,
    /// The lockfile `[envs]` entries behind the targets, merged in once the
    /// key exists.
    synced: std::collections::BTreeMap<String, lock::LockEnv>,
}

/// Everything about the new key that can fail on this side: its targets and
/// their owners, its scopes, its CIDRs, its name and description.
///
/// Done before `--force` deletes the key it replaces, so a creation that was
/// going to fail here fails with the old key still working.
///
/// When `replacing`, a name or description carrying `rbx` or `roblox` is
/// refused here too: Roblox refuses those, and by then the working key would
/// be gone. A plain create leaves that judgement to Roblox (see
/// `explain_invalid_name_or_description`), since nothing is lost if it says no.
async fn prepare_key(
    cfg: &config::Config,
    places: Option<&PlacesFile>,
    lk: &lock::Lock,
    client: &RbxApiKeyClient,
    name: &str,
    no_ip: bool,
    replacing: bool,
) -> Result<Prepared> {
    // `plan` skipped the keys that are not declared.
    let key_cfg = config::get(cfg, name)
        .ok_or_else(|| anyhow::anyhow!("\"{name}\" is not in {}", config::FILE))?;

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
    let payload = ConfigProperties {
        name: config::resolve_remote_name(cfg, key_cfg, name),
        description: build_description(name, key_cfg, &universe_ids),
        is_enabled: config::is_enabled(cfg, key_cfg),
        expiration_time: config::get_expiration_time(cfg, key_cfg),
        allowed_cidrs: cidrs,
        scopes: build.scopes,
    };
    if replacing
        && (super::carries_brand(&payload.name) || super::carries_brand(&payload.description))
    {
        bail!(
            "\"{name}\": Roblox refuses a key whose name or description contains `rbx` or \
             `roblox`, and --force would delete the working key before asking. Nothing was \
             deleted. Sent name: \"{}\". Sent description: \"{}\". Set `name` or `description` \
             on the key in {} to drop the word, then run this again.",
            payload.name,
            payload.description,
            config::FILE
        );
    }
    Ok(Prepared { payload, synced })
}

/// Create the prepared key, store its secret, and record it on disk at once.
#[allow(clippy::too_many_arguments)]
async fn create_prepared(
    lk: &mut lock::Lock,
    client: &RbxApiKeyClient,
    name: &str,
    resolved: &secret_store::Resolved,
    creator_id: u64,
    skip_verify: bool,
    prepared: Prepared,
    lock_path: &Path,
) -> Result<Created> {
    let Prepared { payload, synced } = prepared;
    let stdout = resolved.backend == secret_store::Backend::External;
    let expiration_time = payload.expiration_time.clone();
    let is_enabled = payload.is_enabled;

    say!(
        "Creating \"{}\"... ({} scope entries)",
        name,
        payload.scopes.len()
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
        secret_unsaved: false,
        creator_id,
        is_enabled,
        created_at: info.created_time.clone().unwrap_or_else(time_iso::iso_now),
        expires_at: expiration_time.clone(),
    };

    // Stored where the plan said, or printed under `--stdout` as soon as it is
    // known, before anything else can fail: Roblox will never show it again.
    let kept = secret_store::write(resolved, &secret, &mut entry)
        .map_err(|e| format!("storing its secret failed: {e:#}"))
        .and_then(|()| {
            if stdout {
                crate::out::emit_secret(&secret)
                    .map_err(|e| format!("printing its secret failed: {e}"))
            } else {
                Ok(())
            }
        });

    // Merge the freshly-synced envs back so other keys' envs persist.
    for (env_name, lock_env) in synced {
        lk.envs.insert(env_name, lock_env);
    }

    // The key exists on Roblox either way, so it is recorded either way: an
    // entry is the only thing that keeps it from being a live key nothing
    // names. One whose secret went nowhere is marked so, and every reader
    // treats its secret as missing.
    //
    // On disk before anything else is asked of Roblox: the expiry re-PATCH
    // and the introspect check below can hang or be stopped, and neither may
    // leave a live key nothing records.
    let unsaved = kept.is_err();
    if let Err(why) = kept {
        say!(
            "{}",
            format!(
                "✗ Roblox created \"{name}\" (id={}) but {why}. It is recorded in {} as \
                 having no stored secret.",
                info.id,
                lock::FILE
            )
            .red()
        );
        match piped_note(name, stdout) {
            Some(note) => say!("{}", note.yellow()),
            None => crate::out::hand_over_secret(&secret),
        }
        lock::record_unsaved(lk, name, entry, lock_path)
            .map_err(|e| unrecorded(name, &info.id, lock_path, e))?;
    } else {
        lock::set(lk, name, entry);
        if let Err(e) = lock::save_to(lk, lock_path) {
            // The lockfile was this secret's only copy.
            if resolved.backend == secret_store::Backend::Lockfile {
                crate::out::hand_over_secret(&secret);
            }
            return Err(unrecorded(name, &info.id, lock_path, e));
        }
    }

    // Roblox sometimes ignores expirationTime on CREATE; re-apply via PATCH if
    // missing. Best-effort: the key is recorded and its secret kept by now, so
    // a failure here is a warning, and the key works.
    let response_expiry = info
        .cloud_auth_user_configured_properties
        .as_ref()
        .and_then(|p| p.expiration_time.clone());
    if response_expiry.is_none() && expiration_time.is_some() {
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
    if unsaved {
        return Ok(Created::Unsaved);
    }

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
        verify_against_introspect(client, &secret, &payload.scopes).await;
    }

    Ok(Created::Stored)
}

/// What replaces the secret on stderr when it was meant for `--stdout`, or
/// `None` when the secret itself is handed over there.
///
/// Under `--stdout` the secret is never put on stderr: that is the stream a
/// CI job logs, and the secret was meant for a pipe. The way out is a new one,
/// as `regenerate` says when its own printing fails.
fn piped_note(name: &str, piped: bool) -> Option<String> {
    // A failed write can stop partway, so part of the secret may already be
    // in the pipe: the key is only safe once it has a new one.
    piped.then(|| {
        format!(
            "  Part of the secret may have reached stdout before printing failed. Run \
             `rbx-apikey regenerate {name} --stdout` for a new one."
        )
    })
}

/// The error for a key Roblox created that the lockfile could not record:
/// live, with nothing here naming it, so it gives the id.
fn unrecorded(name: &str, id: &str, lock_path: &Path, e: anyhow::Error) -> anyhow::Error {
    anyhow::anyhow!(
        "\"{name}\" is live on Roblox (id={id}), but {} could not be saved, so nothing here \
         records it: {e:#}. Once the file can be written, `rbx-apikey prune --untracked-only` \
         can delete it.",
        lock_path.display()
    )
}

/// What became of a key Roblox created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Created {
    /// Recorded, and its secret is where the plan said.
    Stored,
    /// Recorded as having no stored secret, which was printed on stderr to
    /// save by hand. The run fails once the record is on disk.
    Unsaved,
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

    mod force {
        use super::*;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const CREATOR: u64 = 1234567890;
        const OLD_ID: &str = "aaaaBBBBccccDDDD";
        const NEW_ID: &str = "eeeeFFFFggggHHHH";

        /// A signed-in client and a server that deletes `OLD_ID` with
        /// `delete_status` and creates `NEW_ID`. The cookie is per test: the
        /// session check caches by cookie.
        async fn server(cookie: &str, delete_status: u16) -> (RbxApiKeyClient, MockServer) {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/v1/users/authenticated"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({ "id": CREATOR, "name": "someone" })),
                )
                .mount(&server)
                .await;
            Mock::given(method("DELETE"))
                .and(path(format!("/cloud-authentication/v1/apiKey/{OLD_ID}")))
                .respond_with(ResponseTemplate::new(delete_status).set_body_string("{}"))
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/cloud-authentication/v1/apiKey"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "cloudAuthInfo": { "id": NEW_ID },
                    "apikeySecret": "the-new-secret"
                })))
                .mount(&server)
                .await;
            let client = RbxApiKeyClient::new(Some(cookie.to_string()))
                .with_base_url(server.uri())
                .with_users_base_url(server.uri());
            // As `run` does before the loop: the delete needs the account.
            client.authenticated_account().await.unwrap();
            (client, server)
        }

        /// `create deploy --force` on a tracked `deploy`, against `server`,
        /// with the lockfile on disk in `dir`.
        async fn force_create(client: &RbxApiKeyClient, dir: &Path) -> (Result<()>, lock::Lock) {
            force_create_with(client, dir, true).await
        }

        /// The same, with `no_ip` as given: without it, `deploy` has no CIDRs
        /// and its request cannot be built.
        async fn force_create_with(
            client: &RbxApiKeyClient,
            dir: &Path,
            no_ip: bool,
        ) -> (Result<()>, lock::Lock) {
            let cfg = two_destinations(dir);
            let lock_path = dir.join(lock::FILE);
            let mut lk = lock::Lock::default();
            let mut old = tracked_entry();
            old.cloud_auth_id = OLD_ID.into();
            old.creator_id = CREATOR;
            lock::set(&mut lk, "deploy", old);
            lock::save_to(&lk, &lock_path).unwrap();

            let names = vec!["deploy".to_string()];
            let steps = plan(&cfg, &lk, &names, true, false);
            let resolved = steps[0].1.as_ref().unwrap().clone();
            let outcome = create_planned(
                &cfg, None, &mut lk, client, "deploy", &resolved, CREATOR, no_ip, true, &lock_path,
            )
            .await;
            (outcome, lock::load_from(&lock_path).unwrap())
        }

        /// A creation that was going to fail on this side deleted the working
        /// key first. The request is built and checked before anything on
        /// Roblox changes.
        #[tokio::test]
        async fn a_request_that_cannot_be_built_deletes_nothing() {
            let dir = tempfile::tempdir().unwrap();
            let (client, server) = server("create-force-no-cidrs", 200).await;

            let (outcome, on_disk) = force_create_with(&client, dir.path(), false).await;

            let err = format!("{:#}", outcome.unwrap_err());
            assert!(err.contains("allowed_cidrs"), "{err}");
            assert!(requests(&server, "").await.is_empty(), "nothing was sent");
            assert_eq!(lock::get(&on_disk, "deploy").unwrap().cloud_auth_id, OLD_ID);
        }

        /// The old key went, and the lockfile could not record that: told
        /// apart from a delete that did not happen, since the key is gone.
        #[tokio::test]
        async fn a_delete_the_lockfile_could_not_record_says_the_old_key_is_gone() {
            let dir = tempfile::tempdir().unwrap();
            let (client, server) = server("create-force-unrecorded", 200).await;
            let cfg = two_destinations(dir.path());
            // A directory where the lockfile belongs: nothing can be written.
            let lock_path = dir.path().join(lock::FILE);
            std::fs::create_dir(&lock_path).unwrap();
            let mut lk = lock::Lock::default();
            let mut old = tracked_entry();
            old.cloud_auth_id = OLD_ID.into();
            old.creator_id = CREATOR;
            lock::set(&mut lk, "deploy", old);
            let names = vec!["deploy".to_string()];
            let resolved = plan(&cfg, &lk, &names, true, false)[0]
                .1
                .as_ref()
                .unwrap()
                .clone();

            let err = create_planned(
                &cfg, None, &mut lk, &client, "deploy", &resolved, CREATOR, true, true, &lock_path,
            )
            .await
            .unwrap_err()
            .to_string();

            assert!(err.contains("is deleted on Roblox"), "{err}");
            assert!(requests(&server, "POST").await.is_empty());
        }

        /// A lockfile that cannot be saved once Roblox has created the key: the
        /// key is live and nothing records it, so the error says so with its id.
        #[tokio::test]
        async fn a_record_that_cannot_be_saved_names_the_live_key() {
            let dir = tempfile::tempdir().unwrap();
            let (client, _server) = server("create-record-fails", 200).await;
            let cfg = two_destinations(dir.path());
            let lock_path = dir.path().join(lock::FILE);
            std::fs::create_dir(&lock_path).unwrap();
            let mut lk = lock::Lock::default();
            let names = vec!["deploy".to_string()];
            let resolved = plan(&cfg, &lk, &names, false, false)[0]
                .1
                .as_ref()
                .unwrap()
                .clone();

            let err = create_planned(
                &cfg, None, &mut lk, &client, "deploy", &resolved, CREATOR, true, true, &lock_path,
            )
            .await
            .unwrap_err()
            .to_string();

            assert!(
                err.contains(&format!("is live on Roblox (id={NEW_ID})")),
                "{err}"
            );
        }

        /// The record is on disk before the introspect check: a hang or a
        /// Ctrl-C there left a live key nothing recorded.
        #[tokio::test]
        async fn the_key_is_recorded_before_the_introspect_check() {
            let dir = tempfile::tempdir().unwrap();
            let (client, server) = server("create-record-before-verify", 200).await;
            Mock::given(method("POST"))
                .and(path("/api-keys/v1/introspect"))
                .respond_with(
                    ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(30)),
                )
                .mount(&server)
                .await;
            let cfg = two_destinations(dir.path());
            let lock_path = dir.path().join(lock::FILE);
            let mut lk = lock::Lock::default();
            let names = vec!["deploy".to_string()];
            let resolved = plan(&cfg, &lk, &names, false, false)[0]
                .1
                .as_ref()
                .unwrap()
                .clone();

            // Stopped while the check hangs, as a Ctrl-C would stop it.
            let stopped = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                create_planned(
                    &cfg, None, &mut lk, &client, "deploy", &resolved, CREATOR, true, false,
                    &lock_path,
                ),
            )
            .await;
            assert!(stopped.is_err(), "the check should still be hanging");

            let on_disk = lock::load_from(&lock_path).unwrap();
            assert_eq!(lock::get(&on_disk, "deploy").unwrap().cloud_auth_id, NEW_ID);
        }

        /// Under `--stdout` a secret nothing kept is never handed over on
        /// stderr, which a CI job logs; the reader is sent to rotate instead.
        #[test]
        fn a_piped_secret_is_never_handed_over_on_stderr() {
            let note = piped_note("deploy", true).expect("a note instead of the secret");
            assert!(note.contains("regenerate deploy --stdout"), "{note}");
            // A write can fail partway: "printed nowhere" may not be true.
            assert!(note.contains("may have reached stdout"), "{note}");
            assert!(!note.contains("not printed anywhere"), "{note}");
            assert!(piped_note("deploy", false).is_none());
        }

        async fn requests(server: &MockServer, verb: &str) -> Vec<String> {
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|r| r.method.as_str() != "GET")
                .map(|r| format!("{} {}", r.method, r.url.path()))
                .filter(|r| r.starts_with(verb) || verb.is_empty())
                .collect()
        }

        /// The bug: `--force` created a second key and wrote it over the
        /// record of the first, which stayed live on Roblox and untracked.
        /// The old key is deleted first, and the lockfile ends up naming the
        /// new one.
        #[tokio::test]
        async fn force_deletes_the_tracked_key_before_creating_its_replacement() {
            let dir = tempfile::tempdir().unwrap();
            let (client, server) = server("create-force-replaces", 200).await;

            let (outcome, on_disk) = force_create(&client, dir.path()).await;

            outcome.unwrap();
            assert_eq!(
                requests(&server, "").await,
                vec![
                    format!("DELETE /cloud-authentication/v1/apiKey/{OLD_ID}"),
                    "POST /cloud-authentication/v1/apiKey".to_string(),
                ]
            );
            let entry = lock::get(&on_disk, "deploy").unwrap();
            assert_eq!(entry.cloud_auth_id, NEW_ID);
            assert_eq!(entry.secret.as_deref(), Some("the-new-secret"));
        }

        /// A delete Roblox refuses may have left the old key live, so nothing
        /// is created and its record stays.
        #[tokio::test]
        async fn a_failed_delete_creates_nothing_and_keeps_the_record() {
            let dir = tempfile::tempdir().unwrap();
            let (client, server) = server("create-force-delete-fails", 400).await;

            let (outcome, on_disk) = force_create(&client, dir.path()).await;

            let err = outcome.unwrap_err().to_string();
            assert!(err.contains("no new key was created"), "{err}");
            assert!(requests(&server, "POST").await.is_empty());
            assert_eq!(lock::get(&on_disk, "deploy").unwrap().cloud_auth_id, OLD_ID);
        }

        /// A key deleted in the Creator Hub answers 404 to its owner: gone
        /// already, which is the case `--force` is most often reached for.
        #[tokio::test]
        async fn a_key_already_gone_is_replaced() {
            let dir = tempfile::tempdir().unwrap();
            let (client, _server) = server("create-force-already-gone", 404).await;

            let (outcome, on_disk) = force_create(&client, dir.path()).await;

            outcome.unwrap();
            assert_eq!(lock::get(&on_disk, "deploy").unwrap().cloud_auth_id, NEW_ID);
        }

        /// Roblox created the key and its secret could not be written. The
        /// run used to stop there with nothing recorded: a live key nothing
        /// named. It is recorded, marked as having no stored secret, `status`
        /// says so, and the run still fails.
        #[tokio::test]
        async fn a_secret_that_could_not_be_stored_still_leaves_a_record() {
            let dir = tempfile::tempdir().unwrap();
            let (client, _server) = server("create-store-fails", 200).await;
            let cfg = two_destinations(dir.path());
            // A directory where `ci`'s secret file belongs: nothing can be
            // written there.
            std::fs::create_dir_all(dir.path().join(".secrets").join("ci.env")).unwrap();
            let lock_path = dir.path().join(lock::FILE);
            let mut lk = lock::Lock::default();

            let names = vec!["ci".to_string()];
            let steps = plan(&cfg, &lk, &names, false, false);
            let resolved = steps[0].1.as_ref().unwrap().clone();
            let err = create_planned(
                &cfg, None, &mut lk, &client, "ci", &resolved, CREATOR, true, true, &lock_path,
            )
            .await
            .unwrap_err()
            .to_string();
            assert!(err.contains("its secret was not stored"), "{err}");

            let on_disk = lock::load_from(&lock_path).unwrap();
            let entry = lock::get(&on_disk, "ci").expect("the live key is recorded");
            assert_eq!(entry.cloud_auth_id, NEW_ID);
            assert!(entry.secret_unsaved);
            assert!(entry.secret.is_none());
            // Kept: it names the file a later cleanup has to find.
            assert_eq!(entry.secret_file.as_deref(), Some(resolved.target.as_str()));

            let verdict = crate::commands::status::local_verdict_for_tests(
                &cfg,
                config::get(&cfg, "ci"),
                "ci",
                entry,
            );
            assert_eq!(verdict.0, "SECRET_MISSING");
            assert!(verdict.1.contains("never stored"), "{}", verdict.1);
        }

        /// A config in `dir` from `text`.
        fn config_in(dir: &Path, text: &str) -> config::Config {
            let path = dir.join(config::FILE);
            std::fs::write(&path, text).unwrap();
            config::load_from(&path).unwrap()
        }

        /// The tracked `deploy`, under `OLD_ID`, in a lockfile on disk.
        fn tracked_deploy(lock_path: &Path) -> lock::Lock {
            let mut lk = lock::Lock::default();
            let mut old = tracked_entry();
            old.cloud_auth_id = OLD_ID.into();
            old.creator_id = CREATOR;
            lock::set(&mut lk, "deploy", old);
            lock::save_to(&lk, lock_path).unwrap();
            lk
        }

        /// The expiry re-PATCH came between the creation and the record: a
        /// hang or a Ctrl-C there left a live key nothing recorded.
        #[tokio::test]
        async fn the_key_is_recorded_before_the_expiry_is_reapplied() {
            let dir = tempfile::tempdir().unwrap();
            let (client, server) = server("create-record-before-patch", 200).await;
            Mock::given(method("PATCH"))
                .and(path("/cloud-authentication/v1/apiKey"))
                .respond_with(
                    ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(30)),
                )
                .mount(&server)
                .await;
            let cfg = config_in(
                dir.path(),
                "[keys.deploy]\nscopes = [\"universe:read\"]\nexpiration_days = 30\n",
            );
            let lock_path = dir.path().join(lock::FILE);
            let mut lk = lock::Lock::default();
            let names = vec!["deploy".to_string()];
            let resolved = plan(&cfg, &lk, &names, false, false)[0]
                .1
                .as_ref()
                .unwrap()
                .clone();

            // Stopped while the PATCH hangs, as a Ctrl-C would stop it.
            let stopped = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                create_planned(
                    &cfg, None, &mut lk, &client, "deploy", &resolved, CREATOR, true, true,
                    &lock_path,
                ),
            )
            .await;
            assert!(stopped.is_err(), "the PATCH should still be hanging");
            assert_eq!(requests(&server, "PATCH").await.len(), 1);

            let on_disk = lock::load_from(&lock_path).unwrap();
            let entry = lock::get(&on_disk, "deploy").unwrap();
            assert_eq!(entry.cloud_auth_id, NEW_ID);
            assert_eq!(entry.secret.as_deref(), Some("the-new-secret"));
        }

        /// Roblox refuses a name or description carrying its brand, and
        /// `--force` deleted the working key before asking. Refused here,
        /// with nothing sent. A plain create still leaves it to Roblox.
        #[tokio::test]
        async fn force_refuses_a_branded_description_before_deleting_anything() {
            let dir = tempfile::tempdir().unwrap();
            let (client, server) = server("create-force-branded", 200).await;
            let cfg = config_in(
                dir.path(),
                "[keys.deploy]\nscopes = [\"universe:read\"]\n\
                 description = \"Deploys the ROBLOX place\"\n",
            );
            let lock_path = dir.path().join(lock::FILE);
            let mut lk = tracked_deploy(&lock_path);
            let names = vec!["deploy".to_string()];
            let resolved = plan(&cfg, &lk, &names, true, false)[0]
                .1
                .as_ref()
                .unwrap()
                .clone();

            let err = create_planned(
                &cfg, None, &mut lk, &client, "deploy", &resolved, CREATOR, true, true, &lock_path,
            )
            .await
            .unwrap_err()
            .to_string();
            assert!(err.contains("Nothing was deleted"), "{err}");
            assert!(err.contains("Deploys the ROBLOX place"), "{err}");
            assert!(requests(&server, "").await.is_empty(), "nothing was sent");
            let on_disk = lock::load_from(&lock_path).unwrap();
            assert_eq!(lock::get(&on_disk, "deploy").unwrap().cloud_auth_id, OLD_ID);

            // Not replacing anything: the request goes to Roblox, which judges.
            let mut fresh = lock::Lock::default();
            let _ = create_planned(
                &cfg, None, &mut fresh, &client, "deploy", &resolved, CREATOR, true, true,
                &lock_path,
            )
            .await;
            assert_eq!(requests(&server, "POST").await.len(), 1);
        }

        /// Only the planned keys the lockfile tracks are replaced.
        #[test]
        fn the_replaced_keys_are_the_planned_ones_already_tracked() {
            let dir = tempfile::tempdir().unwrap();
            let cfg = two_destinations(dir.path());
            let mut lk = lock::Lock::default();
            lock::set(&mut lk, "deploy", tracked_entry());
            let names: Vec<String> = ["ci", "deploy"].map(String::from).to_vec();

            let forced = plan(&cfg, &lk, &names, true, false);
            let replaced: Vec<&str> = replaced_keys(&forced, &lk)
                .iter()
                .map(|(n, _)| *n)
                .collect();
            assert_eq!(replaced, vec!["deploy"]);

            let unforced = plan(&cfg, &lk, &names, false, false);
            assert!(replaced_keys(&unforced, &lk).is_empty());
        }
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
            secret_unsaved: false,
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
