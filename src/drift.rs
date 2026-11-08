//! Drift validation: bail when the lockfile's `[envs.<name>]` snapshot
//! disagrees with what the env currently resolves to via `rbxplace.toml`.
//!
//! Used by every command that touches Roblox state (create, update, delete,
//! regenerate, status). Centralized here so the message and the comparison
//! logic stay identical across commands.
//!
//! Schema v4 made drift per-env rather than per-key: with envs lifted out
//! of `LockEntry`, the only thing that can drift is the cached
//! `(universe_id, owner)` tuple for an env shared by many keys.

use anyhow::{bail, Result};
use rbx_core::places::PlacesFile;

use crate::lock;

/// Check one env. Returns Err(...) when the cached `universe_id` no longer
/// matches what `rbxplace.toml` says, otherwise Ok(()).
pub fn check_one_env(
    env_name: &str,
    cached: &lock::LockEnv,
    resolved_universe_id: u64,
) -> Result<()> {
    if cached.universe_id != resolved_universe_id {
        bail!(
            "Lockfile env '{}' tracks universe_id {} but rbxplace.toml now resolves to {}. \
             Delete the [envs.{}] section in {} if intentional (or fix the env in rbxplace.toml).",
            env_name,
            cached.universe_id,
            resolved_universe_id,
            env_name,
            lock::FILE
        );
    }
    Ok(())
}

/// Walk every env present in the lockfile. Envs missing from `rbxplace.toml`
/// are silently skipped: orphan envs (left over after a key was deleted)
/// are not drift; reporting them is the responsibility of `status`/`list`.
///
/// Entries cached for a universe or a place id have nothing in `rbxplace.toml`
/// to drift from, and are skipped by the same lookup. So is everything when
/// there is no `rbxplace.toml` at all.
pub fn check_all(lock: &lock::Lock, places: Option<&PlacesFile>) -> Result<()> {
    let Some(places) = places else {
        return Ok(());
    };
    for (env_name, cached) in &lock.envs {
        let env = match places.environments.get(env_name) {
            Some(e) => e,
            None => continue,
        };
        check_one_env(env_name, cached, env.universe_id)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::LockEnv;
    use rbx_core::owner::OwnerType;

    fn env(universe_id: u64) -> LockEnv {
        LockEnv {
            universe_id,
            owner_type: OwnerType::Group,
            owner_id: 1,
        }
    }

    #[test]
    fn check_one_env_ok_when_ids_match() {
        let e = env(100);
        assert!(check_one_env("dev", &e, 100).is_ok());
    }

    #[test]
    fn check_one_env_errors_on_mismatch() {
        let e = env(100);
        let err = check_one_env("dev", &e, 200).unwrap_err().to_string();
        assert!(err.contains("'dev'"), "got {}", err);
        assert!(err.contains("100"), "got {}", err);
        assert!(err.contains("200"), "got {}", err);
    }
}
