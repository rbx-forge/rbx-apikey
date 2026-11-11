//! `rbx-apikey list`: quick overview of every key in config + lockfile.
//! For deeper reconciliation, use `status`.

use std::collections::BTreeSet;

use anyhow::Result;
use colored::{ColoredString, Colorize};

use rbx_core::output::{emit, OutputFormat};

use crate::json::{KeyEntry, ListDocument};
use crate::{config, lock, owner_resolver, secret_store, time_iso};

/// Look up universe_ids for a key by walking its targets through the
/// lockfile's `[envs.X]` table. Targets not yet synced are silently skipped:
/// the user will see PENDING / SECRET_MISSING in `status` when that happens.
fn universe_ids_for_key(
    cfg: &config::Config,
    lk: &lock::Lock,
    key_cfg: Option<&config::KeyConfig>,
) -> Vec<u64> {
    let Some(kc) = key_cfg else { return Vec::new() };
    owner_resolver::cached_universe_ids(cfg, kc, &lk.envs)
}

pub fn run(expiry_only: bool, sort: Option<&str>, format: OutputFormat) -> Result<()> {
    let cfg = config::load()?;
    let lk = lock::load()?;

    let mut name_set: BTreeSet<String> = BTreeSet::new();
    for n in cfg.keys.keys() {
        name_set.insert(n.clone());
    }
    for n in lk.keys.keys() {
        name_set.insert(n.clone());
    }
    let mut names: Vec<String> = name_set.into_iter().collect();

    if sort == Some("expiry") {
        names.sort_by(|a, b| {
            let ea = lock::get(&lk, a);
            let eb = lock::get(&lk, b);
            match (ea, eb) {
                (None, None) => a.cmp(b),
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (Some(_), None) => std::cmp::Ordering::Less,
                (Some(ea), Some(eb)) => {
                    let xa = ea.expires_at.clone().unwrap_or_default();
                    let xb = eb.expires_at.clone().unwrap_or_default();
                    if xa == xb {
                        a.cmp(b)
                    } else {
                        xa.cmp(&xb)
                    }
                }
            }
        });
    } else {
        names.sort();
    }

    // Before the empty check: a project with nothing declared is a document
    // with an empty `keys` array, not silence. `.count` answers either way.
    if format.is_json() {
        let mut document = ListDocument::new(match sort {
            Some("expiry") => "expiry",
            _ => "name",
        });
        for name in &names {
            let key_cfg = config::get(&cfg, name);
            document.push(match lock::get(&lk, name) {
                None => KeyEntry::pending(name),
                Some(entry) => {
                    let resolved = secret_store::stored_backend(&cfg, key_cfg, name, Some(entry));
                    let state = secret_state(&resolved, entry);
                    let key = KeyEntry::created(
                        name,
                        key_cfg.is_some(),
                        entry,
                        &universe_ids_for_key(&cfg, &lk, key_cfg),
                        // The secret is read to see whether it is there, and
                        // the answer that leaves this function is the boolean.
                        state.present,
                        &resolved.backend,
                    );
                    if state.readable {
                        key
                    } else {
                        key.with_unreadable_secret()
                    }
                }
            });
        }
        return emit(&document);
    }

    if names.is_empty() {
        println!(
            "{}",
            format!(
                "(nothing in {} - add a [keys.<name>] section)",
                config::FILE
            )
            .yellow()
        );
        return Ok(());
    }

    let mut warnings = 0usize;
    if !expiry_only {
        println!(
            "{}",
            format!("Keys (config: {}, lock: {}):", config::FILE, lock::FILE).cyan()
        );
    }

    for name in &names {
        let key_cfg = config::get(&cfg, name);
        let entry = lock::get(&lk, name);

        let entry = match entry {
            Some(e) => e,
            None => {
                if expiry_only {
                    println!("{}", format!("  {}  (not created)", name).yellow());
                } else {
                    println!(
                        "{}",
                        format!(
                            "  {}  (not created - run `rbx-apikey create {}`)",
                            name, name
                        )
                        .yellow()
                    );
                }
                warnings += 1;
                continue;
            }
        };

        let (expiry_text, expiry_color, has_warn) = expiry_line(entry.expires_at.as_deref());
        if has_warn {
            warnings += 1;
        }
        let expiry_str = colorize(&expiry_text, expiry_color);

        if expiry_only {
            let orphan_tag = if key_cfg.is_none() { " ← orphan" } else { "" };
            println!("  {}  {}{}", name, expiry_str, orphan_tag);
        } else {
            let resolved = secret_store::stored_backend(&cfg, key_cfg, name, Some(entry));
            let state = secret_state(&resolved, entry);
            if state.warn {
                warnings += 1;
            }
            let secret_status = state.text;

            // v4: universe_ids come from the key's targets ↦ lk.envs.
            // Orphan envs in lk.envs (no key references them anymore) are NOT
            // associated with this key, so we skip them.
            let universe_ids: Vec<String> = universe_ids_for_key(&cfg, &lk, key_cfg)
                .into_iter()
                .map(|u| u.to_string())
                .collect();

            if key_cfg.is_none() {
                println!(
                    "{}",
                    format!("  {}  ← not in {} (orphan)", name, config::FILE).yellow()
                );
                warnings += 1;
            } else {
                println!("  {}", name);
            }
            println!("    id:        {}", entry.cloud_auth_id);
            println!("    creator:   user_id={}", entry.creator_id);
            if !universe_ids.is_empty() {
                println!("    universes: {}", universe_ids.join(", "));
            }
            println!("    expires:   {}", expiry_str);
            println!("    secret:    {}", secret_status);
        }
    }

    if warnings > 0 {
        println!();
        println!(
            "{}",
            format!(
                "⚠  {} item(s) need attention. Run `rbx-apikey status` for details.",
                warnings
            )
            .yellow()
        );
    }
    Ok(())
}

/// What `list` says about a created key's secret, in both forms.
#[derive(Debug)]
struct SecretState {
    present: bool,
    /// False when the secret's file is there and could not be read.
    readable: bool,
    /// The human form's `secret:` value.
    text: String,
    /// Whether it counts towards "item(s) need attention".
    warn: bool,
}

/// Read the secret to say whether it is there. A file that cannot be read is
/// said so, with the error, and the listing goes on: one unreadable file used
/// to stop `list` before it printed anything about the other keys.
fn secret_state(resolved: &secret_store::Resolved, entry: &lock::LockEntry) -> SecretState {
    let target = if resolved.backend == secret_store::Backend::Lockfile {
        String::new()
    } else {
        format!(": {}", resolved.target)
    };
    if resolved.backend == secret_store::Backend::External {
        return SecretState {
            present: false,
            readable: true,
            text: "held elsewhere (printed with --stdout)".to_string(),
            warn: false,
        };
    }
    match secret_store::read(resolved, Some(entry)) {
        Ok(Some(_)) => SecretState {
            present: true,
            readable: true,
            text: format!("set ({}{})", resolved.backend.as_str(), target),
            warn: false,
        },
        Ok(None) => SecretState {
            present: false,
            readable: true,
            text: format!(
                "{}",
                format!("MISSING ({}{})", resolved.backend.as_str(), target).yellow()
            ),
            warn: true,
        },
        Err(e) => SecretState {
            present: false,
            readable: false,
            text: format!("{}", format!("secret cannot be read: {e:#}").yellow()),
            warn: true,
        },
    }
}

#[derive(Clone, Copy)]
enum LineColor {
    Green,
    Yellow,
    Cyan,
    Red,
}

fn colorize(text: &str, color: LineColor) -> ColoredString {
    match color {
        LineColor::Green => text.green(),
        LineColor::Yellow => text.yellow(),
        LineColor::Cyan => text.cyan(),
        LineColor::Red => text.red(),
    }
}

fn expiry_line(iso: Option<&str>) -> (String, LineColor, bool) {
    let iso = match iso {
        Some(s) if !s.is_empty() => s,
        _ => return ("<no expiry>".to_string(), LineColor::Green, false),
    };

    let ts = match time_iso::parse_iso_to_unix(iso) {
        Some(t) => t,
        None => return (format!("{} (unparseable)", iso), LineColor::Yellow, true),
    };
    let days = time_iso::days_until_unix(ts);
    let span = time_iso::span(days);
    let date = if iso.len() >= 10 { &iso[..10] } else { iso };
    if days < 0 {
        (
            format!("{} (EXPIRED {} ago)", date, span),
            LineColor::Red,
            true,
        )
    } else if days < 7 {
        (
            format!("{} (expires in {} - rotate soon!)", date, span),
            LineColor::Yellow,
            true,
        )
    } else if days < 30 {
        (
            format!("{} (expires in {})", date, span),
            LineColor::Cyan,
            true,
        )
    } else {
        (format!("{} (in {})", date, span), LineColor::Green, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> lock::LockEntry {
        lock::LockEntry {
            cloud_auth_id: "aaaaBBBBccccDDDD".into(),
            secret: None,
            secret_file: None,
            secret_external: false,
            secret_unsaved: false,
            creator_id: 1234567890,
            is_enabled: true,
            created_at: "now".into(),
            expires_at: None,
        }
    }

    fn file_at(path: &std::path::Path) -> secret_store::Resolved {
        secret_store::Resolved {
            backend: secret_store::Backend::File,
            target: path.to_string_lossy().to_string(),
        }
    }

    /// A secret file that cannot be read is shown as such on its key, with
    /// the error, and does not stop the listing or turn into "missing".
    #[test]
    fn an_unreadable_secret_is_shown_on_its_key() {
        let dir = tempfile::tempdir().expect("a temp dir");
        // A directory where the file belongs: there, and unreadable as one.
        let path = dir.path().join("deploy.env");
        std::fs::create_dir(&path).expect("a directory");

        let state = secret_state(&file_at(&path), &entry());

        assert!(!state.present && !state.readable && state.warn);
        assert!(state.text.contains("cannot be read"), "{}", state.text);
        assert!(state.text.contains("deploy.env"), "{}", state.text);
    }

    /// And an absent one is still missing, and readable in the sense the
    /// document means: there was nothing to fail on.
    #[test]
    fn an_absent_secret_is_missing() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let state = secret_state(&file_at(&dir.path().join("deploy.env")), &entry());
        assert!(!state.present && state.readable && state.warn);
        assert!(state.text.contains("MISSING"), "{}", state.text);
    }

    /// An expiry passed an hour ago read "expires in 0d - rotate soon!", in
    /// yellow, for a key Roblox already refuses.
    #[test]
    fn an_expiry_passed_within_the_day_is_expired() {
        let an_hour_ago = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let (text, color, warn) = expiry_line(Some(&an_hour_ago));
        assert!(text.ends_with("(EXPIRED under 1d ago)"), "{text}");
        assert!(matches!(color, LineColor::Red) && warn);

        let in_an_hour = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        let (text, _, _) = expiry_line(Some(&in_an_hour));
        assert!(
            text.ends_with("(expires in under 1d - rotate soon!)"),
            "{text}"
        );
    }
}
