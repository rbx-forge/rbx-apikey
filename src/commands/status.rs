//! `rbx-apikey status [--remote]`: reconcile config + lockfile + (optionally) Roblox.

use std::collections::BTreeSet;

use anyhow::Result;
use colored::Colorize;

use crate::json::StatusDocument;
use crate::{config, lock, secret_store, time_iso};
use rbx_core::output::{emit, OutputFormat};
use rbx_core::GlobalFlags;

use super::make_client;

#[derive(Debug, Clone, Copy)]
enum Status {
    Healthy,
    Pending,
    Expired,
    ExpiringSoon,
    OrphanLock,
    OrphanRemote,
    SecretMissing,
    SecretUnreadable,
    Disabled,
    CheckFailed,
}

impl Status {
    fn as_str(&self) -> &'static str {
        match self {
            Status::Healthy => "HEALTHY",
            Status::Pending => "PENDING",
            Status::Expired => "EXPIRED",
            Status::ExpiringSoon => "EXPIRING_SOON",
            Status::OrphanLock => "ORPHAN_LOCK",
            Status::OrphanRemote => "ORPHAN_REMOTE",
            Status::SecretMissing => "SECRET_MISSING",
            Status::SecretUnreadable => "SECRET_UNREADABLE",
            Status::Disabled => "DISABLED",
            Status::CheckFailed => "CHECK_FAILED",
        }
    }

    fn is_healthy(&self) -> bool {
        matches!(self, Status::Healthy)
    }

    fn symbol(&self) -> (&'static str, fn(&str) -> colored::ColoredString) {
        match self {
            Status::Healthy => ("✓", |s| s.green()),
            Status::Pending => ("?", |s| s.yellow()),
            Status::ExpiringSoon => ("⚠", |s| s.yellow()),
            Status::CheckFailed => ("?", |s| s.yellow()),
            _ => ("✗", |s| s.red()),
        }
    }
}

pub async fn run(global: &GlobalFlags, remote_check: bool, format: OutputFormat) -> Result<()> {
    let cfg = config::load()?;
    let lk = lock::load()?;
    let places = super::load_places(global)?;

    let mut name_set: BTreeSet<String> = BTreeSet::new();
    for n in cfg.keys.keys() {
        name_set.insert(n.clone());
    }
    for n in lk.keys.keys() {
        name_set.insert(n.clone());
    }
    let names: Vec<String> = name_set.into_iter().collect();

    crate::drift::check_all(&lk, places.as_ref())?;

    let mut document = StatusDocument::new(remote_check);

    if names.is_empty() {
        // A project with nothing in either file is a document with an empty
        // `keys` array, the same non-event the human form reports as "nothing
        // to report".
        if format.is_json() {
            return emit(&document);
        }
        println!(
            "{}",
            format!(
                "(nothing to report - both {} and {} are empty)",
                config::FILE,
                lock::FILE
            )
            .yellow()
        );
        return Ok(());
    }

    let client = if remote_check {
        Some(make_client(global))
    } else {
        None
    };
    let mut issues = 0usize;

    for name in &names {
        let key_cfg = config::get(&cfg, name);
        let entry = lock::get(&lk, name);

        // Hoisted out of the arm that used to own it: the document reports the
        // day count for every key that has an expiry, and reading it twice from
        // the same entry is how the two answers start to disagree.
        let days = entry
            .and_then(|e| e.expires_at.as_deref())
            .filter(|iso| !iso.is_empty())
            .and_then(time_iso::days_until);

        let (status, detail) = match (entry, key_cfg) {
            (None, Some(_)) => (
                Status::Pending,
                format!(
                    "in {}, no key created - run `rbx-apikey create {}`",
                    config::FILE,
                    name
                ),
            ),
            (Some(_), None) => (
                Status::OrphanLock,
                format!(
                    "in {} but not in {} - consider `rbx-apikey delete {}`",
                    lock::FILE,
                    config::FILE,
                    name
                ),
            ),
            (None, None) => continue, // shouldn't happen since name_set unions them.
            (Some(entry), Some(_)) => {
                if let Some(verdict) = local_verdict(&cfg, key_cfg, name, entry, days) {
                    verdict
                } else if let Some(c) = &client {
                    match c.get_api_key(&entry.cloud_auth_id).await {
                        // A network/timeout/5xx error means we couldn't verify
                        // the key, not that it expired - don't push the user to
                        // regenerate a perfectly valid key.
                        Err(e) => (
                            Status::CheckFailed,
                            // `{:#}` for the reason given in `create`: `{}`
                            // renders only the outermost frame, so a wrapped
                            // error reports the operation and drops the cause.
                            format!("could not verify on Roblox: {:#}", e),
                        ),
                        Ok(None) => (
                            Status::OrphanRemote,
                            format!(
                                "404 from Roblox (key deleted there) - `rbx-apikey delete {}` to clean lockfile",
                                name
                            ),
                        ),
                        Ok(Some(_)) => {
                            let detail = days
                                .map(|d| format!("expires in {}", time_iso::span(d)))
                                .unwrap_or_else(|| "no expiry".to_string());
                            (Status::Healthy, detail)
                        }
                    }
                } else {
                    let detail = days
                        .map(|d| format!("expires in {}", time_iso::span(d)))
                        .unwrap_or_else(|| "no expiry".to_string());
                    (Status::Healthy, detail)
                }
            }
        };

        if format.is_json() {
            document.push(name, status.as_str(), status.is_healthy(), days);
        } else {
            let (sym, paint) = status.symbol();
            let status_str = paint(&format!("{} {}", sym, status.as_str()));
            println!("  {}  {}  - {}", status_str, name, detail);
        }
        if !status.is_healthy() {
            issues += 1;
        }
    }

    // The summary and the tip are notes about the run rather than the result,
    // so they go through `note`: stdout under the human form, byte for byte
    // where they were, and stderr under `--json`, where stdout belongs to the
    // document.
    format.note("");
    if issues == 0 {
        format.note("All keys healthy.".green());
    } else {
        format.note(format!("{} key(s) need attention.", issues).yellow());
    }
    if !remote_check {
        format.note(
            "Tip: run `rbx-apikey status --remote` to also check the state on Roblox.".cyan(),
        );
    }

    if format.is_json() {
        emit(&document)?;
    }
    Ok(())
}

/// What the files alone say about a key both of them have, or `None` when
/// they find nothing wrong and the answer is Roblox's to give.
///
/// A secret file that is there and cannot be read is its own verdict, with
/// the error, and the run goes on to the other keys: it used to stop `status`
/// at the first such file, and before that to call it missing.
fn local_verdict(
    cfg: &config::Config,
    key_cfg: Option<&config::KeyConfig>,
    name: &str,
    entry: &lock::LockEntry,
    days: Option<i64>,
) -> Option<(Status, String)> {
    let resolved = secret_store::stored_backend(cfg, key_cfg, name, Some(entry));
    // Printed with --stdout: held elsewhere on purpose, not missing.
    let held_elsewhere = resolved.backend == secret_store::Backend::External;
    let secret = secret_store::read(&resolved, Some(entry));

    if !entry.is_enabled {
        return Some((
            Status::Disabled,
            format!("key is disabled in Roblox - `rbx-apikey update {}`", name),
        ));
    }
    match secret {
        Err(e) => {
            return Some((
                Status::SecretUnreadable,
                format!("secret cannot be read: {e:#}"),
            ))
        }
        Ok(None) if entry.secret_unsaved => {
            return Some((
                Status::SecretMissing,
                format!(
                    "never stored: Roblox created the key and its secret could not be written - \
                     `rbx-apikey regenerate {}` stores a new one",
                    name
                ),
            ));
        }
        Ok(None) if !held_elsewhere => {
            let detail = match resolved.backend {
                secret_store::Backend::File => {
                    format!("file {} not found or empty", resolved.target)
                }
                secret_store::Backend::Lockfile => format!(
                    "secret missing in lockfile - `rbx-apikey regenerate {}`",
                    name
                ),
                // Excluded by `held_elsewhere` above; spelled out for the match.
                secret_store::Backend::External => resolved.target.clone(),
            };
            return Some((Status::SecretMissing, detail));
        }
        Ok(_) => {}
    }
    match days {
        Some(d) if d < 0 => Some((
            Status::Expired,
            format!(
                "EXPIRED {} ago - `rbx-apikey regenerate {}`",
                time_iso::span(d),
                name
            ),
        )),
        Some(d) if d < 7 => Some((
            Status::ExpiringSoon,
            format!("expires in {} - rotate soon", time_iso::span(d)),
        )),
        _ => None,
    }
}

/// [`local_verdict`] for another command's tests, with no expiry: the status
/// word and its detail.
#[cfg(test)]
pub(crate) fn local_verdict_for_tests(
    cfg: &config::Config,
    key_cfg: Option<&config::KeyConfig>,
    name: &str,
    entry: &lock::LockEntry,
) -> (&'static str, String) {
    let (status, detail) =
        local_verdict(cfg, key_cfg, name, entry, None).unwrap_or((Status::Healthy, String::new()));
    (status.as_str(), detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One unreadable secret file is a verdict on its key, with the error,
    /// and not a stop for the whole run nor a "missing" secret.
    #[test]
    fn an_unreadable_secret_is_its_own_verdict() {
        let dir = tempfile::tempdir().expect("a temp dir");
        // A directory where the file belongs: there, and unreadable as one.
        let secret_file = dir.path().join("deploy.env");
        std::fs::create_dir(&secret_file).expect("a directory");
        let config_path = dir.path().join(config::FILE);
        std::fs::write(
            &config_path,
            format!(
                "[keys.deploy]\nscopes = [\"universe:read\"]\nsecret_file = '{}'\n",
                secret_file.display()
            ),
        )
        .expect("a config");
        let cfg = config::load_from(&config_path).expect("it loads");
        let entry = lock::LockEntry {
            cloud_auth_id: "aaaaBBBBccccDDDD".into(),
            secret: None,
            secret_file: Some(secret_file.to_string_lossy().to_string()),
            secret_external: false,
            secret_unsaved: false,
            creator_id: 1234567890,
            is_enabled: true,
            created_at: "now".into(),
            expires_at: None,
        };

        let (status, detail) =
            local_verdict(&cfg, config::get(&cfg, "deploy"), "deploy", &entry, None)
                .expect("a verdict");

        assert_eq!(status.as_str(), "SECRET_UNREADABLE");
        assert!(detail.contains("deploy.env"), "{detail}");
    }

    /// An expiry passed an hour ago counted as 0 days, and `status` called the
    /// key EXPIRING_SOON, "expires in 0d".
    #[test]
    fn an_expiry_passed_within_the_day_is_expired() {
        let an_hour_ago = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let entry = lock::LockEntry {
            cloud_auth_id: "aaaaBBBBccccDDDD".into(),
            secret: Some("not-a-real-secret".into()),
            secret_file: None,
            secret_external: false,
            secret_unsaved: false,
            creator_id: 1234567890,
            is_enabled: true,
            created_at: "now".into(),
            expires_at: Some(an_hour_ago.clone()),
        };

        let (status, detail) = local_verdict(
            &config::Config::default(),
            None,
            "deploy",
            &entry,
            time_iso::days_until(&an_hour_ago),
        )
        .expect("a verdict");

        assert_eq!(status.as_str(), "EXPIRED");
        assert!(detail.starts_with("EXPIRED under 1d ago"), "{detail}");
    }
}
