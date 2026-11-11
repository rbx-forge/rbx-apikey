//! `rbx-apikey delete <key>|--all`: remove a key from Roblox and the local lock.

use std::path::Path;

use anyhow::{bail, Result};
use colored::Colorize;
use dialoguer::Confirm;
use reqwest::StatusCode;

use crate::{config, lock, secret_store};
use rbx_core::api::is_api_status;
use rbx_core::session::SessionAccount;
use rbx_core::GlobalFlags;

use super::{make_client, require_no_collision};

pub async fn run(
    global: &GlobalFlags,
    name: Option<&str>,
    all: bool,
    skip_confirm: bool,
    clean_files: bool,
) -> Result<()> {
    require_no_collision(all, name)?;
    if !all && name.is_none() {
        bail!("usage: rbx-apikey delete <key>|--all [--yes] [--clean-files]");
    }

    let cfg = config::load()?;
    let mut lk = lock::load()?;
    let places = super::load_places(global)?;
    let client = make_client(global);

    crate::drift::check_all(&lk, places.as_ref())?;

    // Asked here so the confirmations below can name the account. It is the
    // same call the pre-delete gate makes, cached for the process, so this
    // costs one round trip rather than two, and a dead session now refuses
    // before a person is asked to approve anything.
    client.require_valid_session().await?;

    // Ahead of every prompt, so nobody is asked to approve a delete that cannot
    // work. `delete_one` checks again for `prune`, which does not come this way.
    if let Some(account) = client.known_account().await {
        let refusals: Vec<String> = lk
            .keys
            .iter()
            .filter(|(key, _)| all || name == Some(key.as_str()))
            .filter_map(|(key, entry)| wrong_account(key, entry.creator_id, &account))
            .collect();
        if !refusals.is_empty() {
            bail!(refusals.join("\n"));
        }
    }

    let names: Vec<String> = if all {
        let mut v: Vec<String> = lk.keys.keys().cloned().collect();
        v.sort();
        if v.is_empty() {
            bail!("no keys in {}", lock::FILE);
        }

        println!("About to DELETE {} keys: {}", v.len(), v.join(", "));
        println!("{}", "This is IRREVERSIBLE.".yellow());
        if !skip_confirm {
            if let Some(a) = client.known_account().await {
                println!("On account {}.", a.label());
            }
            if !Confirm::new()
                .with_prompt("First confirm: really delete ALL keys?")
                .default(false)
                .interact()?
            {
                println!("{}", "Aborted.".yellow());
                return Ok(());
            }
            if !Confirm::new()
                .with_prompt("Second confirm: are you really sure?")
                .default(false)
                .interact()?
            {
                println!("{}", "Aborted.".yellow());
                return Ok(());
            }
        }
        v
    } else {
        let name = name.unwrap();
        let entry = lock::get(&lk, name)
            .ok_or_else(|| anyhow::anyhow!("\"{}\" has no entry in {}", name, lock::FILE))?;
        if !skip_confirm {
            match client.known_account().await {
                Some(a) => println!(
                    "About to DELETE \"{}\" (id={}) on account {}.",
                    name,
                    entry.cloud_auth_id,
                    a.label()
                ),
                None => println!("About to DELETE \"{}\" (id={}).", name, entry.cloud_auth_id),
            }
            if !Confirm::new()
                .with_prompt("This is irreversible. Proceed?")
                .default(false)
                .interact()?
            {
                println!("{}", "Aborted.".yellow());
                return Ok(());
            }
        }
        vec![name.to_string()]
    };

    // Before the first delete. `delete_one` swallows its own failures so one
    // bad key does not abort the rest, which is the right call for a per-key
    // fault and the wrong one for a dead session: without this, `--all` on an
    // expired cookie prints a failure per key and deletes none, having asked
    // Roblox once per key to find out.
    //
    // Cached: the confirmations above already asked, which is what let them
    // name the account. Deleting is the verb where that matters most, because
    // the same key names recur across accounts and a delete cannot be undone.
    client.require_valid_session().await?;

    // No save after the loop: each key records its own removal as soon as
    // Roblox has confirmed it. See `delete_one`.
    let mut failures = 0usize;
    for n in &names {
        let done = delete_one(
            &cfg,
            &mut lk,
            &client,
            n,
            skip_confirm,
            clean_files,
            Path::new(lock::FILE),
        )
        .await;
        if !done {
            failures += 1;
        }
    }

    if failures > 0 {
        bail!("{failures} key(s) could not be deleted");
    }
    Ok(())
}

/// Delete one key the project tracks: Roblox, then the lockfile entry, then
/// the stored secret. `prune` calls this for its tracked selections so the
/// cleanup cannot drift between the two commands.
///
/// Returns whether the key is gone and its record with it. On `false` the
/// failure is already printed, the lockfile on disk is as it was, and the
/// caller owes a non-zero exit.
///
/// `create --force` calls it too, for the key it replaces, so its lines go
/// through `say!`: under `create --stdout`, stdout carries the secret alone.
pub(crate) async fn delete_one(
    cfg: &config::Config,
    lk: &mut lock::Lock,
    client: &crate::api::RbxApiKeyClient,
    name: &str,
    skip_confirm: bool,
    clean_files: bool,
    lock_path: &Path,
) -> bool {
    let entry = match lock::get(lk, name).cloned() {
        Some(e) => e,
        None => {
            say!(
                "{}",
                format!("skipping \"{}\": not in {}", name, lock::FILE).yellow()
            );
            return true;
        }
    };

    let key_cfg = config::get(cfg, name);
    let resolved = secret_store::stored_backend(cfg, key_cfg, name, Some(&entry));

    let account = client.known_account().await;
    if let Some(refusal) = account
        .as_ref()
        .and_then(|a| wrong_account(name, entry.creator_id, a))
    {
        say!("{}", format!("✗ {refusal}").red());
        return false;
    }

    // The lockfile entry and the secret are the only record that this key
    // exists, and the secret cannot be asked for again. So they go only once
    // Roblox has confirmed the key is gone: a failed call may have left it live.
    let already_gone = match client.delete_api_key(&entry.cloud_auth_id).await {
        Ok(()) => false,
        // A 404 means gone only from the account that owns the key, which is
        // what `account` being known and past the check above establishes. From
        // anywhere else Roblox gives the same answer about a key that is live.
        Err(e) if account.is_some() && is_api_status(&e, StatusCode::NOT_FOUND) => {
            say!(
                "{}",
                format!("\"{name}\" no longer exists on Roblox, removing the local record: {e}")
                    .yellow()
            );
            true
        }
        Err(e) => {
            say!(
                "{}",
                format!(
                    "✗ \"{name}\": Roblox delete failed, nothing removed, so the key may still \
                     be live: {e}"
                )
                .red()
            );
            return false;
        }
    };

    lock::remove(lk, name);
    // On disk now, and not after the last key of an `--all` or a `prune`. The
    // key is gone from Roblox, and the question about its secret file comes
    // next: saved once at the end, a run stopped there kept listing as live a
    // key that no longer existed.
    if let Err(e) = lock::save_to(lk, lock_path) {
        // Memory follows the disk. Left out of it, the entry would vanish
        // with the next key's save, right after being reported as still there.
        lock::set(lk, name, entry.clone());
        say!(
            "{}",
            format!(
                "✗ \"{name}\" is deleted on Roblox, but {} could not be saved and still lists \
                 it: {e:#}",
                lock_path.display()
            )
            .red()
        );
        return false;
    }

    let should_delete_file =
        if resolved.backend == secret_store::Backend::File && !clean_files && !skip_confirm {
            Confirm::new()
                .with_prompt(format!("Also delete secret file \"{}\"?", resolved.target))
                .default(false)
                .interact()
                .unwrap_or(false)
        } else {
            clean_files
        };

    let cleanup = secret_store::cleanup(&resolved, should_delete_file);
    for msg in &cleanup.auto_cleaned {
        say!("{}", format!("  ✓ {}", msg).green());
    }
    for msg in &cleanup.manual_action_needed {
        say!("{}", format!("  ⚠ {}", msg).yellow());
    }

    if !already_gone {
        say!(
            "{}",
            format!("✓ \"{}\" deleted (id={})", name, entry.cloud_auth_id).green()
        );
    }
    true
}

/// Why this session must not delete this key, if it must not.
///
/// A key belongs to the account that created it, and Roblox answers 404 to any
/// other, the same answer it gives for a key that no longer exists. Sending the
/// request would prove nothing, and the lockfile already knows the answer.
pub(crate) fn wrong_account(
    name: &str,
    creator_id: u64,
    account: &SessionAccount,
) -> Option<String> {
    (account.id != creator_id).then(|| {
        format!(
            "\"{name}\" was created by account {creator_id}, and this session is {}. Roblox \
             would answer 404 and the key would stay. Sign in to that account and run this \
             again.",
            account.label()
        )
    })
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
    const CREATOR: u64 = 1234567890;

    fn lock_with_key() -> lock::Lock {
        let mut lk = lock::Lock::default();
        lock::set(
            &mut lk,
            "deploy",
            LockEntry {
                cloud_auth_id: KEY_ID.into(),
                secret: Some("not-a-real-secret".into()),
                secret_file: None,
                secret_external: false,
                secret_unsaved: false,
                creator_id: CREATOR,
                is_enabled: true,
                created_at: "now".into(),
                expires_at: None,
            },
        );
        lk
    }

    /// One server answering both the session question and the delete. The
    /// cookie is unique per test because the session check caches by cookie.
    ///
    /// The lock handed back is the one read from disk once `delete_one` has
    /// returned, with no save after it: that file is the record, and a run
    /// stopped before a save at the end kept listing a key that was gone.
    async fn run_delete(
        cookie: &str,
        signed_in: u64,
        delete_status: u16,
    ) -> (bool, lock::Lock, MockServer) {
        let (client, server) = signed_in_client(cookie, signed_in, delete_status).await;

        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join(lock::FILE);
        let mut lk = lock_with_key();
        lock::save_to(&lk, &lock_path).unwrap();

        let done = delete_one(
            &config::Config::default(),
            &mut lk,
            &client,
            "deploy",
            true,
            false,
            &lock_path,
        )
        .await;
        (done, lock::load_from(&lock_path).unwrap(), server)
    }

    /// A client past its session check, and the server answering it.
    async fn signed_in_client(
        cookie: &str,
        signed_in: u64,
        delete_status: u16,
    ) -> (RbxApiKeyClient, MockServer) {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/users/authenticated"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "id": signed_in, "name": "someone" })),
            )
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path(format!("/cloud-authentication/v1/apiKey/{KEY_ID}")))
            .respond_with(ResponseTemplate::new(delete_status).set_body_string("{}"))
            .mount(&server)
            .await;
        let client = RbxApiKeyClient::new(Some(cookie.to_string()))
            .with_base_url(server.uri())
            .with_users_base_url(server.uri());
        client.require_valid_session().await.unwrap();
        (client, server)
    }

    /// Roblox deleted the key and the lockfile could not be saved: the record
    /// is still on disk, so it has to be back in memory as well. Left out, the
    /// next key's save wrote a lockfile without it, a moment after this one
    /// was reported as still listed.
    #[tokio::test]
    async fn a_delete_whose_save_fails_keeps_the_record_in_memory_too() {
        let (client, _server) = signed_in_client("delete-save-fails", CREATOR, 200).await;
        let dir = tempfile::tempdir().unwrap();
        // A directory where the lockfile belongs: nothing can be written there.
        let lock_path = dir.path().join(lock::FILE);
        std::fs::create_dir(&lock_path).unwrap();
        let mut lk = lock_with_key();

        let done = delete_one(
            &config::Config::default(),
            &mut lk,
            &client,
            "deploy",
            true,
            false,
            &lock_path,
        )
        .await;

        assert!(!done);
        assert!(lock::get(&lk, "deploy").is_some());
    }

    async fn delete_requests(server: &MockServer) -> usize {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method == wiremock::http::Method::DELETE)
            .count()
    }

    /// The reported failure: the wrong account signed in. Nothing is sent, and
    /// the record of a key that is still live survives.
    #[tokio::test]
    async fn the_wrong_account_sends_nothing_and_keeps_the_record() {
        let (done, lk, server) = run_delete("delete-wrong-account", 9876543210, 404).await;

        assert!(!done);
        assert!(lock::get(&lk, "deploy").is_some());
        assert_eq!(delete_requests(&server).await, 0);
    }

    #[tokio::test]
    async fn a_failed_delete_keeps_the_record() {
        let (done, lk, _server) = run_delete("delete-server-error", CREATOR, 400).await;

        assert!(!done);
        assert!(lock::get(&lk, "deploy").is_some());
    }

    /// `status --remote` sends people here to clean up a key removed in the
    /// Creator Hub, so this path has to keep working for its owner.
    #[tokio::test]
    async fn a_404_for_the_owner_cleans_the_record() {
        let (done, lk, server) = run_delete("delete-already-gone", CREATOR, 404).await;

        assert!(done);
        assert!(lock::get(&lk, "deploy").is_none());
        assert_eq!(delete_requests(&server).await, 1);
    }

    #[tokio::test]
    async fn a_successful_delete_removes_the_record() {
        let (done, lk, _server) = run_delete("delete-success", CREATOR, 200).await;

        assert!(done);
        assert!(lock::get(&lk, "deploy").is_none());
    }

    #[test]
    fn the_refusal_names_both_accounts() {
        let account = SessionAccount {
            id: 9876543210,
            name: "someone".into(),
            display_name: String::new(),
        };

        let refusal = wrong_account("deploy", CREATOR, &account).unwrap();
        assert!(refusal.contains("1234567890"), "{refusal}");
        assert!(refusal.contains("9876543210"), "{refusal}");
        assert!(wrong_account("deploy", 9876543210, &account).is_none());
    }
}
