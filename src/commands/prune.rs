//! `rbx-apikey prune`: pick keys off the account and delete them.
//!
//! The dangerous cousin of `list --remote`, and built defensively because the
//! danger is measured, not hypothetical: on a real account the keys this
//! project manages are a small minority of what the listing returns. The rest
//! belong to other checkouts and other tools, and deleting one breaks a
//! deployment somewhere else with no local sign that anything happened.
//!
//! Hence: nothing is ever preselected, there is no `--all`, and every entry
//! says plainly whether this project owns it.

use std::path::Path;

use anyhow::{bail, Context, Result};
use colored::Colorize;
use dialoguer::{Confirm, MultiSelect};

use crate::{config, lock, remote_view};
use rbx_core::GlobalFlags;

use super::make_client;

pub struct PruneOptions {
    pub group_id: Option<u64>,
    pub untracked_only: bool,
    pub expired_only: bool,
    pub dry_run: bool,
    pub yes: bool,
    pub clean_files: bool,
}

pub async fn run(global: &GlobalFlags, opts: PruneOptions) -> Result<()> {
    let (cfg, mut lk) = load_local(Path::new(config::FILE), Path::new(lock::FILE))?;
    let client = make_client(global);

    let whoami = client.authenticated_account().await?;
    let all = remote_view::fetch(&client, &lk, opts.group_id).await?;

    let candidates: Vec<remote_view::RemoteKey> = all
        .into_iter()
        .filter(|k| !opts.untracked_only || !k.tracked.is_tracked())
        .filter(|k| !opts.expired_only || k.is_expired())
        .collect();

    let scope = match opts.group_id {
        Some(g) => format!("group {}", g),
        None => format!("user {}", whoami.id),
    };

    if candidates.is_empty() {
        println!("{}", format!("Nothing to prune for {}.", scope).green());
        return Ok(());
    }

    println!(
        "{}",
        format!("{} key(s) on Roblox for {}:", candidates.len(), scope).cyan()
    );

    let labels: Vec<String> = candidates
        .iter()
        .map(|k| {
            let tag = match &k.tracked {
                remote_view::Tracked::Yes(name) => format!("tracked → {}", name),
                remote_view::Tracked::No => "UNTRACKED (another project?)".to_string(),
            };
            format!(
                "{}  [{}]  {}  created {}  {}  - {}",
                k.name(),
                k.state(),
                k.secret_preview(),
                k.created_date(),
                k.expiry_text(),
                tag
            )
        })
        .collect();

    if opts.dry_run {
        for l in &labels {
            println!("  {}", l);
        }
        println!();
        println!("{}", "Dry run: nothing was deleted.".green());
        return Ok(());
    }

    // Never preselected. A prune where the safe answer is "just press enter"
    // is a prune that eventually deletes somebody's production key.
    let picked = MultiSelect::new()
        .with_prompt("Select keys to DELETE (space to toggle, enter to confirm)")
        .items(&labels)
        .interact()?;

    if picked.is_empty() {
        println!("{}", "Nothing selected. Aborted.".yellow());
        return Ok(());
    }

    let chosen: Vec<&remote_view::RemoteKey> = picked.iter().map(|i| &candidates[*i]).collect();
    let untracked_count = chosen.iter().filter(|k| !k.tracked.is_tracked()).count();

    println!();
    println!("{}", "About to DELETE, irreversibly:".yellow());
    for k in &chosen {
        println!("  - {} ({})", k.name(), k.info.id);
    }
    if untracked_count > 0 {
        println!(
            "{}",
            format!(
                "{} of these are NOT tracked by this project. If another checkout or tool depends on one, it will start failing with no local sign why.",
                untracked_count
            )
            .yellow()
        );
    }

    if !opts.yes
        && !Confirm::new()
            .with_prompt(format!("Delete these {} key(s)?", chosen.len()))
            .default(false)
            .interact()?
    {
        println!("{}", "Aborted.".yellow());
        return Ok(());
    }

    let mut failures = 0usize;
    for action in plan(&chosen) {
        match action {
            // Reuses the ordinary delete path so the lockfile entry and the
            // stored secret go with the key. A bare remote delete would leave
            // a lockfile pointing at nothing and an orphan secret on disk.
            //
            // It also saves the lockfile itself, key by key. Untracked keys
            // leave nothing behind locally, so a prune run outside any project
            // writes no lockfile at all.
            PruneAction::Tracked { lock_name } => {
                let done = super::delete::delete_one(
                    &cfg,
                    &mut lk,
                    &client,
                    &lock_name,
                    opts.yes,
                    opts.clean_files,
                    Path::new(lock::FILE),
                )
                .await;
                if !done {
                    failures += 1;
                }
            }
            PruneAction::Untracked { id, name } => match client.delete_api_key(&id).await {
                Ok(()) => println!("{}", format!("✓ \"{name}\" deleted (id={id})").green()),
                Err(e) => {
                    failures += 1;
                    println!("{}", format!("✗ \"{name}\": delete failed: {e}").red());
                }
            },
        }
    }

    if failures > 0 {
        bail!("{} key(s) could not be deleted", failures);
    }
    Ok(())
}

/// The project's config and lockfile, for telling its keys from everybody
/// else's.
///
/// A file that is not there is the ordinary case of a prune run outside a
/// project: nothing is tracked. A file that is there and cannot be read is a
/// different thing, and reading it as empty was how every key of the project
/// came to be labelled UNTRACKED, deleted without its record, and the
/// unreadable lockfile then overwritten with an empty one.
fn load_local(config_path: &Path, lock_path: &Path) -> Result<(config::Config, lock::Lock)> {
    // `try_exists`, because `exists` answers "no" to a file it could not look
    // at, and that is the one answer this must not give by accident.
    let there = config_path
        .try_exists()
        .with_context(|| format!("failed to look at {}", config_path.display()))?;
    let cfg = if there {
        config::load_from(config_path)?
    } else {
        config::Config::default()
    };
    // `load_from` already answers an absent lockfile with an empty one.
    let lk = lock::load_from(lock_path)?;
    Ok((cfg, lk))
}

/// What a selected key gets: the lockfile-aware delete, or a bare remote one.
///
/// Split out from the loop so the decision can be tested. Getting it wrong is
/// silent in both directions: routing a tracked key to the bare delete leaves
/// a lockfile pointing at nothing and an orphan secret on disk, and routing an
/// untracked one through `delete_one` makes it look up a lockfile entry that
/// does not exist.
#[derive(Debug, PartialEq, Eq)]
enum PruneAction {
    /// Deleted through `delete`'s own path, by its lockfile name.
    Tracked { lock_name: String },
    /// Deleted on Roblox only; we never held its secret.
    Untracked { id: String, name: String },
}

fn plan(chosen: &[&remote_view::RemoteKey]) -> Vec<PruneAction> {
    chosen
        .iter()
        .map(|k| match &k.tracked {
            remote_view::Tracked::Yes(lock_name) => PruneAction::Tracked {
                lock_name: lock_name.clone(),
            },
            remote_view::Tracked::No => PruneAction::Untracked {
                id: k.info.id.clone(),
                name: k.name().to_string(),
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::api_keys::{RemoteApiKey, RemoteProperties};
    use crate::remote_view::{RemoteKey, Tracked};

    fn remote_key(id: &str, name: &str, tracked: Tracked) -> RemoteKey {
        RemoteKey {
            info: RemoteApiKey {
                id: id.to_string(),
                cloud_auth_user_configured_properties: Some(RemoteProperties {
                    name: name.to_string(),
                    is_enabled: true,
                    ..Default::default()
                }),
                ..Default::default()
            },
            tracked,
        }
    }

    #[test]
    fn a_tracked_key_is_deleted_by_its_lockfile_name_not_its_remote_one() {
        // The lockfile knows it as `viewer`; Roblox calls it `myproject_viewer`.
        // `delete_one` looks it up by the former, so passing the latter would
        // find nothing and skip the cleanup entirely.
        let k = remote_key(
            "aaaaBBBB",
            "myproject_viewer",
            Tracked::Yes("viewer".to_string()),
        );
        assert_eq!(
            plan(&[&k]),
            vec![PruneAction::Tracked {
                lock_name: "viewer".to_string()
            }]
        );
    }

    #[test]
    fn an_untracked_key_is_deleted_by_id_and_never_touches_the_lockfile() {
        let k = remote_key("bbbb-2222", "somebody_elses", Tracked::No);
        assert_eq!(
            plan(&[&k]),
            vec![PruneAction::Untracked {
                id: "bbbb-2222".to_string(),
                name: "somebody_elses".to_string()
            }]
        );
    }

    /// Outside a project there is no file at all, and that is not an error:
    /// nothing is tracked, and every key on the account is somebody else's.
    #[test]
    fn no_file_at_all_means_nothing_is_tracked() {
        let dir = tempfile::tempdir().expect("tempdir");

        let (_cfg, lk) = load_local(&dir.path().join(config::FILE), &dir.path().join(lock::FILE))
            .expect("an absent file is the ordinary case");

        assert!(lk.keys.is_empty());
    }

    /// The failure this guards. Read as empty, an unreadable lockfile made
    /// every key of the project UNTRACKED: deleted without its record, and the
    /// file then overwritten with an empty one, past any repair.
    #[test]
    fn a_lockfile_that_cannot_be_read_stops_the_prune() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock_path = dir.path().join(lock::FILE);
        std::fs::write(&lock_path, "version = 4\n[keys.deploy\n").expect("write");

        let err = load_local(&dir.path().join(config::FILE), &lock_path)
            .expect_err("a lockfile that does not parse is not an empty one");

        assert!(
            format!("{err:#}").contains(lock::FILE),
            "names the file: {err:#}"
        );
        assert_eq!(
            std::fs::read_to_string(&lock_path).expect("read"),
            "version = 4\n[keys.deploy\n",
            "and leaves it as it was"
        );
    }

    #[test]
    fn a_config_that_cannot_be_read_stops_the_prune_too() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join(config::FILE);
        std::fs::write(&config_path, "[keys.deploy\n").expect("write");

        let err = load_local(&config_path, &dir.path().join(lock::FILE))
            .expect_err("a config that does not parse is not an empty one");

        assert!(format!("{err:#}").contains(config::FILE), "{err:#}");
    }

    #[test]
    fn a_mixed_selection_routes_each_key_independently() {
        let tracked = remote_key("id-1", "mine_remote", Tracked::Yes("mine".to_string()));
        let untracked = remote_key("id-2", "theirs", Tracked::No);
        let actions = plan(&[&tracked, &untracked]);
        assert_eq!(actions.len(), 2);
        assert!(matches!(actions[0], PruneAction::Tracked { .. }));
        assert!(matches!(actions[1], PruneAction::Untracked { .. }));
    }
}
