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
                    KeyEntry::created(
                        name,
                        key_cfg.is_some(),
                        entry,
                        &universe_ids_for_key(&cfg, &lk, key_cfg),
                        // The secret is read to see whether it is there, and
                        // the answer that leaves this function is the boolean.
                        secret_store::read(&resolved, Some(entry))?.is_some(),
                        &resolved.backend,
                    )
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
            let secret = secret_store::read(&resolved, Some(entry))?;
            let secret_status: String = if resolved.backend == secret_store::Backend::External {
                "held elsewhere (printed with --stdout)".to_string()
            } else if secret.is_some() {
                let target = if resolved.backend == secret_store::Backend::Lockfile {
                    String::new()
                } else {
                    format!(": {}", resolved.target)
                };
                format!("set ({}{})", resolved.backend.as_str(), target)
            } else {
                let target = if resolved.backend == secret_store::Backend::Lockfile {
                    String::new()
                } else {
                    format!(": {}", resolved.target)
                };
                warnings += 1;
                format!(
                    "{}",
                    format!("MISSING ({}{})", resolved.backend.as_str(), target).yellow()
                )
            };

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
    let days = (ts - chrono::Utc::now().timestamp()) / 86_400;
    let date = if iso.len() >= 10 { &iso[..10] } else { iso };
    if days < 0 {
        (
            format!("{} (EXPIRED {}d ago)", date, days.abs()),
            LineColor::Red,
            true,
        )
    } else if days < 7 {
        (
            format!("{} (expires in {}d - rotate soon!)", date, days),
            LineColor::Yellow,
            true,
        )
    } else if days < 30 {
        (
            format!("{} (expires in {}d)", date, days),
            LineColor::Cyan,
            true,
        )
    } else {
        (format!("{} (in {}d)", date, days), LineColor::Green, false)
    }
}
