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
                let resolved = secret_store::stored_backend(&cfg, key_cfg, name, Some(entry));
                let secret = secret_store::read(&resolved, Some(entry))?;
                // Printed with --stdout: held elsewhere on purpose, not missing.
                let held_elsewhere = resolved.backend == secret_store::Backend::External;

                if !entry.is_enabled {
                    (
                        Status::Disabled,
                        format!("key is disabled in Roblox - `rbx-apikey update {}`", name),
                    )
                } else if secret.is_none() && !held_elsewhere {
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
                    (Status::SecretMissing, detail)
                } else if days.map(|d| d < 0).unwrap_or(false) {
                    let d = days.unwrap();
                    (
                        Status::Expired,
                        format!(
                            "EXPIRED {}d ago - `rbx-apikey regenerate {}`",
                            d.abs(),
                            name
                        ),
                    )
                } else if days.map(|d| d < 7).unwrap_or(false) {
                    let d = days.unwrap();
                    (
                        Status::ExpiringSoon,
                        format!("expires in {}d - rotate soon", d),
                    )
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
                                .map(|d| format!("expires in {}d", d))
                                .unwrap_or_else(|| "no expiry".to_string());
                            (Status::Healthy, detail)
                        }
                    }
                } else {
                    let detail = days
                        .map(|d| format!("expires in {}d", d))
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
