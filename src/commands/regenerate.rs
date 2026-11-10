//! `rbx-apikey regenerate <key>|--all`: rotate the API key secret.

use std::path::Path;

use anyhow::{bail, Result};
use colored::Colorize;
use dialoguer::Confirm;

use crate::{config, lock, secret_store, time_iso};
use rbx_core::confirm::confirm_always;
use rbx_core::GlobalFlags;

use super::{make_client, require_no_collision};

pub async fn run(
    global: &GlobalFlags,
    name: Option<&str>,
    all: bool,
    skip_confirm: bool,
    clean_files: bool,
    stdout: bool,
) -> Result<()> {
    require_no_collision(all, name)?;
    if !all && name.is_none() {
        bail!("usage: rbx-apikey regenerate <key>|--all [--yes]");
    }
    if stdout {
        crate::out::reserve_stdout();
    }

    let cfg = config::load()?;
    let mut lk = lock::load()?;
    let places = super::load_places(global)?;
    let client = make_client(global);

    crate::drift::check_all(&lk, places.as_ref())?;

    let names: Vec<String> = if all {
        let mut v: Vec<String> = lk.keys.keys().cloned().collect();
        v.sort();
        if v.is_empty() {
            bail!("no keys in {}", lock::FILE);
        }

        say!("About to rotate {} secrets: {}", v.len(), v.join(", "));
        say!(
            "{}",
            "This invalidates ALL current secrets at once.".yellow()
        );
        if !skip_confirm {
            let ok = Confirm::new()
                .with_prompt("Proceed?")
                .default(false)
                .interact()?;
            if !ok {
                say!("{}", "Aborted.".yellow());
                return Ok(());
            }
        }
        v
    } else {
        let single = name.unwrap().to_string();
        // Single-key rotation: confirm once unless --yes. The --all path above
        // has its own explicit "Proceed?" prompt because it lists every key.
        //
        // The session is checked first so the question can name the account.
        // These keys are minted on whichever account the cookie signs in as,
        // and a key name says nothing about which one that is.
        client.require_valid_session().await?;
        let account = client.known_account().await;
        confirm_always(
            &rbx_core::session::as_account(
                account.as_ref(),
                &format!(
                    "Rotate \"{}\"? Old secret will be invalidated immediately.",
                    single
                ),
            ),
            skip_confirm,
        )?;
        vec![single]
    };

    // Before the first rotation. Rotating invalidates the old secret the
    // moment it lands, so a run that gets through two keys and is then refused
    // has left two projects holding secrets nothing wrote down. Cached, so the
    // single-key path above has already paid for this.
    client.require_valid_session().await?;

    // No save after each key: `regenerate_one` writes the lockfile itself, as
    // soon as the new secret is in it and before it asks anything.
    let mut outcomes = Vec::with_capacity(names.len());
    for n in &names {
        outcomes.push(
            regenerate_one(
                &cfg,
                &mut lk,
                &client,
                n,
                clean_files,
                stdout,
                Path::new(lock::FILE),
            )
            .await,
        );
    }

    let invalidated = outcomes.iter().filter(|o| o.invalidated_the_old()).count();
    let failed = outcomes.iter().filter(|o| o.is_failure()).count();

    // Only when something actually rotated, and this is the point of tracking
    // the outcomes at all. The line used to print after the loop
    // unconditionally, while `regenerate_one` swallowed its failures, so a run
    // that rotated nothing still announced that the old secrets were gone.
    //
    // That is the line a reader acts on, because it is the one saying the value
    // sitting in their CI is dead. Printing it after a total failure sends them
    // to replace a secret that still works, and hides that the rotation they
    // asked for never happened.
    if invalidated > 0 {
        let subject = if invalidated == names.len() {
            "Old secrets".to_string()
        } else {
            format!("Old secrets for the {invalidated} key(s) rotated above")
        };
        say!("{} invalid as of {}.", subject, time_iso::iso_now());
    }

    // Non-zero on any failure, for the same reason. `--all` deliberately walks
    // past a key it could not rotate so one bad entry does not strand the rest,
    // and exiting 0 afterwards reports that tolerance as success.
    if failed > 0 {
        bail!(
            "{} of {} key(s) did not come through. Each one is named above; a key not named \
             there kept the secret it had.",
            failed,
            names.len()
        );
    }

    Ok(())
}

/// What one key's rotation did.
///
/// Two questions, and they do not always have the same answer: whether the old
/// secret is dead, and whether the command did what it was asked. A rotation
/// Roblox performed and this tool then failed to store answers yes to the first
/// and no to the second, and reporting either one alone is how a reader ends up
/// with a key nothing holds the secret to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rotation {
    /// Nothing reached Roblox, or Roblox refused. The key still has its old
    /// secret and nothing needs replacing.
    Untouched,
    /// Rotated, and the new secret is stored where the config says.
    Stored,
    /// Rotated, so the old secret is dead, but the new one could not be written
    /// and was printed for the reader to save by hand.
    Unsaved,
}

impl Rotation {
    /// Whether the secret this key had before the run is now worthless.
    fn invalidated_the_old(self) -> bool {
        matches!(self, Self::Stored | Self::Unsaved)
    }

    /// Whether the run should exit non-zero because of this key.
    fn is_failure(self) -> bool {
        matches!(self, Self::Untouched | Self::Unsaved)
    }
}

async fn regenerate_one(
    cfg: &config::Config,
    lk: &mut lock::Lock,
    client: &crate::api::RbxApiKeyClient,
    name: &str,
    clean_files: bool,
    stdout: bool,
    lock_path: &Path,
) -> Rotation {
    let entry = match lock::get(lk, name).cloned() {
        Some(e) => e,
        None => {
            say!(
                "{}",
                format!("skipping \"{}\": not in {}", name, lock::FILE).yellow()
            );
            return Rotation::Untouched;
        }
    };

    say!("Regenerating \"{}\" (id={})...", name, entry.cloud_auth_id);
    let resp = match client.regenerate_secret(&entry.cloud_auth_id).await {
        Ok(r) => r,
        Err(e) => {
            say!(
                "{}",
                format!("\"{}\": regenerate failed: {}", name, e).yellow()
            );
            return Rotation::Untouched;
        }
    };

    let new_secret = resp.apikey_secret;
    let key_cfg = config::get(cfg, name);

    // Where the OLD (now-invalidated) secret was stored, vs. where the NEW one belongs.
    // After a successful rotate, the OLD location holds garbage; we offer to clean it up
    // when `secret_file` was changed in rbxapikey.toml between writes.
    let previous_resolved = secret_store::previous_backend_from_entry(&entry, name);
    let new_resolved = if stdout {
        secret_store::external()
    } else {
        secret_store::backend_for(cfg, key_cfg, name)
    };
    let backend_changed = secret_store::backend_differs(&previous_resolved, &new_resolved);

    if stdout {
        // Printed before anything else can fail: the old secret is already
        // dead, and Roblox will never show this one again.
        if let Err(e) = crate::out::emit_secret(&new_secret) {
            say!(
                "{}",
                format!(
                    "  ✗ Roblox rotated the secret but printing it failed: {e}. \
                     Run `rbx-apikey regenerate {name} --stdout` again."
                )
                .red()
            );
            return Rotation::Unsaved;
        }
    }

    let mut entry = entry;
    if let Err(e) = secret_store::write(&new_resolved, &new_secret, &mut entry) {
        say!(
            "{}",
            format!(
                "  ✗ Roblox rotated the secret but writing it locally failed: {}",
                e
            )
            .red()
        );
        say!(
            "{}",
            format!("  Secret (save this manually): {}", new_secret).yellow()
        );
        return Rotation::Unsaved;
    }

    // Keep the lock entry's secret_file pointer in sync with the new backend so future
    // `previous_backend_from_entry` calls see the right "previous" state.
    entry.secret_file = if new_resolved.backend == secret_store::Backend::File {
        Some(new_resolved.target.clone())
    } else {
        None
    };
    lock::set(lk, name, entry);
    // On disk now, before the question about the old secret file below. With
    // the lockfile backend the new secret exists nowhere else: the old one
    // died when Roblox answered, and Roblox will not show this one again.
    // Saved by the caller after this returned, a run stopped at that question
    // lost it, and a save that failed lost it without printing it.
    if let Err(e) = lock::save_to(lk, lock_path) {
        say!(
            "{}",
            format!(
                "  ✗ Roblox rotated the secret but {} could not be saved: {e:#}",
                lock_path.display()
            )
            .red()
        );
        if new_resolved.backend == secret_store::Backend::Lockfile {
            say!(
                "{}",
                format!("  Secret (save this manually): {}", new_secret).yellow()
            );
        }
        return Rotation::Unsaved;
    }

    if backend_changed {
        say!(
            "{}",
            format!(
                "  ↪ Secret backend changed: {}:{} → {}:{} (new secret written at new location)",
                previous_resolved.backend.as_str(),
                previous_resolved.target,
                new_resolved.backend.as_str(),
                new_resolved.target
            )
            .cyan()
        );

        if previous_resolved.backend == secret_store::Backend::File {
            let should_delete = if clean_files {
                true
            } else {
                Confirm::new()
                    .with_prompt(format!(
                        "Old secret file \"{}\" holds the now-invalid secret. Delete it?",
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
    }

    say!("{}", format!("✓ \"{}\": secret rotated", name).green());
    Rotation::Stored
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::api::RbxApiKeyClient;
    use crate::lock::LockEntry;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const KEY_ID: &str = "aaaaBBBBccccDDDD";

    /// One key whose secret lives in the lockfile, the default backend: the
    /// case where the lockfile is the only place a rotated secret can go.
    fn lock_with_key() -> lock::Lock {
        let mut lk = lock::Lock::default();
        lock::set(
            &mut lk,
            "deploy",
            LockEntry {
                cloud_auth_id: KEY_ID.into(),
                secret: Some("the-old-secret".into()),
                secret_file: None,
                secret_external: false,
                creator_id: 1234567890,
                is_enabled: true,
                created_at: "now".into(),
                expires_at: None,
            },
        );
        lk
    }

    /// A server that rotates the key, and a client pointed at it.
    async fn rotating(cookie: &str) -> (RbxApiKeyClient, MockServer) {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!(
                "/cloud-authentication/v1/apiKey/{KEY_ID}/regenerate"
            )))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "apikeySecret": "the-new-secret" })),
            )
            .mount(&server)
            .await;
        let client = RbxApiKeyClient::new(Some(cookie.to_string()))
            .with_base_url(server.uri())
            .with_users_base_url(server.uri());
        (client, server)
    }

    /// The old secret died when Roblox answered, and Roblox will not show the
    /// new one again. It has to be on disk by the time `regenerate_one`
    /// returns, with no save after it: the caller used to do that save, after
    /// a question a run could be stopped at.
    #[tokio::test]
    async fn a_rotated_secret_is_on_disk_when_regenerate_one_returns() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join(lock::FILE);
        let mut lk = lock_with_key();
        lock::save_to(&lk, &lock_path).unwrap();
        let (client, _server) = rotating("regenerate-stored").await;

        let outcome = regenerate_one(
            &config::Config::default(),
            &mut lk,
            &client,
            "deploy",
            true,
            false,
            &lock_path,
        )
        .await;

        assert_eq!(outcome, Rotation::Stored);
        let on_disk = lock::load_from(&lock_path).unwrap();
        assert_eq!(
            lock::get(&on_disk, "deploy").unwrap().secret.as_deref(),
            Some("the-new-secret")
        );
    }

    /// A lockfile that cannot be written after the rotation is the case with
    /// both answers at once: the old secret is dead, and the run failed.
    #[tokio::test]
    async fn a_rotation_the_lockfile_could_not_take_is_unsaved() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where the lockfile belongs: nothing can be written there.
        let lock_path = dir.path().join(lock::FILE);
        std::fs::create_dir(&lock_path).unwrap();
        let mut lk = lock_with_key();
        let (client, _server) = rotating("regenerate-unsaved").await;

        let outcome = regenerate_one(
            &config::Config::default(),
            &mut lk,
            &client,
            "deploy",
            true,
            false,
            &lock_path,
        )
        .await;

        assert_eq!(outcome, Rotation::Unsaved);
    }

    /// The bug this exists for: `Old secrets invalid as of <now>` printed after
    /// a run that rotated nothing. A refused session rotates nothing, so
    /// nothing about the old secrets changed and the line has to stay unsaid.
    #[test]
    fn a_rotation_that_did_not_happen_invalidates_nothing() {
        assert!(!Rotation::Untouched.invalidated_the_old());
        assert!(Rotation::Untouched.is_failure());
    }

    /// The case that needs both answers at once, and the reason this is not a
    /// bool: Roblox rotated the key, so the old secret is dead and the line
    /// must print, but the new one never reached disk, so the run failed.
    #[test]
    fn a_rotation_whose_secret_was_not_stored_is_both_at_once() {
        assert!(Rotation::Unsaved.invalidated_the_old());
        assert!(Rotation::Unsaved.is_failure());
    }

    #[test]
    fn a_complete_rotation_is_only_the_first() {
        assert!(Rotation::Stored.invalidated_the_old());
        assert!(!Rotation::Stored.is_failure());
    }
}
