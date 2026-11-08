//! Resolve and cache target → (universe_id, owner) mappings.
//!
//! In schema v4 the lockfile stores a single `[envs.<name>]` table that
//! every key shares. A target is an env, a universe id or a place id (see
//! [`Target`]); this module keeps that table in sync with `rbxplace.toml`,
//! the ids in `rbxapikey.toml` and (optionally) the Roblox games API:
//!
//! - `universe_id` comes from `rbxplace.toml` for an env, from the config for
//!   a universe, and from Roblox, once, for a place;
//! - owner fields are reused from the cached `LockEnv` when present, its
//!   `universe_id` still matches today's value, AND it actually holds an owner
//!   when one is being asked for;
//! - on a miss, we either fetch from Roblox (`fetch_owners = true`, used by
//!   keys whose scopes need creator-target resolution) or fall back to a
//!   placeholder `(User, 0)` that mirrors v3's "no owner needed" behavior.
//!
//! That third condition is the fix for a latent bug rather than an obvious
//! rule. The placeholder exists because the schema cannot say "not asked", and
//! for a long time the cache test was `universe_id` alone, so the first key
//! on an env with no creator-target scope wrote `(User, 0)`, and every later
//! key that *did* need the owner found a matching `universe_id` and reused it.
//! The resulting scope targets `U0`. Treating the placeholder as a miss also
//! repairs lockfiles already carrying one, on the first command that needs a
//! real owner, with no migration.
//!
//! The returned map only covers the targets asked about. Callers merge it into
//! the live `lock.envs` so entries referenced by other keys persist untouched.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use rbx_core::owner::OwnerType;
use rbx_core::places::PlacesFile;

use crate::api::RbxApiKeyClient;
use crate::config::{effective_envs, Config, KeyConfig};
use crate::lock::{self, LockEnv, UniverseOwner};
use crate::scope_builder;

/// The cached entry to reuse, if it is usable for what this caller needs.
///
/// Separate from `sync_targets` because it is the whole of the decision and none
/// of the I/O: the miss it returns leads to a Roblox call, so the conditions
/// are worth testing without one.
fn reusable_cache_entry(
    cached: Option<&LockEnv>,
    universe_id: u64,
    fetch_owners: bool,
) -> Option<&LockEnv> {
    let cached = cached?;
    if cached.universe_id != universe_id {
        // The env has been repointed at another universe; its owner is a
        // different creator's.
        return None;
    }
    if fetch_owners && cached.owner_is_placeholder() {
        // Written by an earlier caller that needed no owner. Reusing it here
        // is how a scope ends up targeting `U0`.
        return None;
    }
    Some(cached)
}

/// One thing a key targets, as `rbxapikey.toml` names it.
///
/// An env goes through `rbxplace.toml`; a universe or a place is named by id,
/// which is what lets a key be declared in a directory with no `rbxplace.toml`
/// at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Env(String),
    Universe(u64),
    Place(u64),
}

impl Target {
    /// The name this target is cached under in the lockfile's `[envs]` table.
    ///
    /// An env keeps its bare name, so lockfiles written before ids existed
    /// read unchanged. Ids carry a prefix because a universe and a place may
    /// share a number; colliding with an env would take one literally named
    /// `universe:<n>`.
    pub fn cache_key(&self) -> String {
        match self {
            Target::Env(name) => name.clone(),
            Target::Universe(id) => format!("universe:{id}"),
            Target::Place(id) => format!("place:{id}"),
        }
    }
}

/// Everything `key_cfg` targets, in declaration order: envs, then universes,
/// then places.
pub fn targets(cfg: &Config, key_cfg: &KeyConfig) -> Vec<Target> {
    let envs = effective_envs(cfg, key_cfg).into_iter().map(Target::Env);
    let universes = key_cfg.universe_ids.iter().copied().map(Target::Universe);
    let places = key_cfg.place_ids.iter().copied().map(Target::Place);
    envs.chain(universes).chain(places).collect()
}

/// Resolve `targets` to `(universe_id, owner)`, reusing the lockfile's cache.
///
/// `places` is `None` when there is no `rbxplace.toml`. That is fine for a key
/// that names ids only, and an error naming the fix for one that names an env.
///
/// The returned map is keyed by [`Target::cache_key`] and covers `targets`
/// only. Callers merge it into the live `lock.envs`.
pub async fn sync_targets(
    client: &RbxApiKeyClient,
    targets: &[Target],
    places: Option<&PlacesFile>,
    cached: &BTreeMap<String, LockEnv>,
    fetch_owners: bool,
) -> Result<BTreeMap<String, LockEnv>> {
    let mut out: BTreeMap<String, LockEnv> = BTreeMap::new();
    for target in targets {
        let key = target.cache_key();
        let universe_id = match target {
            Target::Env(env_name) => universe_of_env(env_name, places)?,
            Target::Universe(id) => *id,
            // A place does not move between universes, so a cached answer
            // stays the answer and costs no request.
            Target::Place(id) => match cached.get(&key) {
                Some(entry) => entry.universe_id,
                None => client
                    .fetch_place_universe(*id)
                    .await
                    .with_context(|| format!("place_ids entry {id}"))?,
            },
        };

        if let Some(reusable) = reusable_cache_entry(cached.get(&key), universe_id, fetch_owners) {
            out.insert(key, reusable.clone());
            continue;
        }

        let lock_env = if fetch_owners {
            let owner = client.fetch_universe_owner(universe_id).await?;
            LockEnv {
                universe_id,
                owner_type: owner.owner_type,
                owner_id: owner.owner_id,
            }
        } else {
            LockEnv {
                universe_id,
                owner_type: OwnerType::User,
                owner_id: 0,
            }
        };
        out.insert(key, lock_env);
    }
    Ok(out)
}

/// What building one key's scopes needs from its targets.
#[derive(Debug)]
pub struct ResolvedKey {
    /// One per target, in [`targets`] order.
    pub universe_ids: Vec<u64>,
    pub owners: Vec<UniverseOwner>,
    /// The lockfile entries behind them, to merge into `lock.envs` once the
    /// key exists on Roblox.
    pub synced: BTreeMap<String, LockEnv>,
}

/// [`sync_targets`] for one key, fetching owners only when its scopes need
/// them.
pub async fn resolve_key(
    client: &RbxApiKeyClient,
    cfg: &Config,
    key_cfg: &KeyConfig,
    places: Option<&PlacesFile>,
    cached: &BTreeMap<String, LockEnv>,
) -> Result<ResolvedKey> {
    let targets = targets(cfg, key_cfg);
    let fetch_owners = scope_builder::needs_owner_resolution(key_cfg);
    let synced = sync_targets(client, &targets, places, cached, fetch_owners).await?;
    let entries: Vec<&LockEnv> = targets
        .iter()
        .filter_map(|t| synced.get(&t.cache_key()))
        .collect();
    Ok(ResolvedKey {
        universe_ids: entries.iter().map(|e| e.universe_id).collect(),
        owners: entries.iter().map(|e| lock::env_to_owner(e)).collect(),
        synced,
    })
}

fn universe_of_env(env_name: &str, places: Option<&PlacesFile>) -> Result<u64> {
    let Some(places) = places else {
        bail!(
            "this key targets env '{env_name}', which is resolved through rbxplace.toml, and \
             there is none here. Point --places at it, or name the universe in {} with \
             universe_ids = [<id>] instead of envs.",
            crate::config::FILE
        );
    };
    let env = places.get(env_name).with_context(|| {
        format!("env '{env_name}' (referenced by key) not found in rbxplace.toml")
    })?;
    Ok(env.universe_id)
}

/// The universe ids a key targets, read from the lockfile cache alone.
///
/// For the commands that make no request. A target not cached yet is skipped:
/// it is a key that has not been created, which `status` reports as such.
pub fn cached_universe_ids(
    cfg: &Config,
    key_cfg: &KeyConfig,
    cached: &BTreeMap<String, LockEnv>,
) -> Vec<u64> {
    targets(cfg, key_cfg)
        .iter()
        .filter_map(|t| cached.get(&t.cache_key()).map(|e| e.universe_id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved(universe_id: u64) -> LockEnv {
        LockEnv {
            universe_id,
            owner_type: OwnerType::Group,
            owner_id: 42,
        }
    }

    fn placeholder(universe_id: u64) -> LockEnv {
        LockEnv {
            universe_id,
            owner_type: OwnerType::User,
            owner_id: 0,
        }
    }

    #[test]
    fn a_resolved_entry_is_reused_whether_or_not_the_caller_needs_the_owner() {
        let cached = resolved(100);
        assert!(reusable_cache_entry(Some(&cached), 100, true).is_some());
        assert!(reusable_cache_entry(Some(&cached), 100, false).is_some());
    }

    /// The caller does not need an owner, so the placeholder is a correct
    /// answer and costs no request.
    #[test]
    fn a_placeholder_is_reused_when_no_owner_is_needed() {
        let cached = placeholder(100);
        assert!(reusable_cache_entry(Some(&cached), 100, false).is_some());
    }

    /// The bug this function exists for. Reusing it builds a creator-targeted
    /// scope over `U0`.
    #[test]
    fn a_placeholder_is_a_miss_when_the_owner_is_needed() {
        let cached = placeholder(100);
        assert!(reusable_cache_entry(Some(&cached), 100, true).is_none());
    }

    #[test]
    fn a_repointed_env_is_a_miss_even_with_a_resolved_owner() {
        let cached = resolved(100);
        assert!(reusable_cache_entry(Some(&cached), 999, true).is_none());
    }

    #[test]
    fn nothing_cached_is_a_miss() {
        assert!(reusable_cache_entry(None, 100, false).is_none());
    }

    fn config(text: &str) -> Config {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(crate::config::FILE);
        std::fs::write(&path, text).unwrap();
        crate::config::load_from(&path).unwrap()
    }

    /// No cookie, and no server behind it: every test below that passes is
    /// one that made no request.
    fn offline_client() -> RbxApiKeyClient {
        RbxApiKeyClient::new(None)
    }

    #[test]
    fn targets_come_in_declaration_order_with_distinct_cache_keys() {
        let cfg = config(
            "[keys.deploy]\nscopes = [\"universe:read\"]\nenvs = [\"dev\"]\n\
             universe_ids = [1234567890]\nplace_ids = [1234567890]\n",
        );
        let targets = targets(&cfg, &cfg.keys["deploy"]);
        assert_eq!(
            targets,
            vec![
                Target::Env("dev".into()),
                Target::Universe(1234567890),
                Target::Place(1234567890),
            ]
        );
        // The same number as a universe and as a place is two entries.
        let keys: Vec<String> = targets.iter().map(Target::cache_key).collect();
        assert_eq!(keys, vec!["dev", "universe:1234567890", "place:1234567890"]);
    }

    /// The point of naming ids: a key naming a universe resolves with no
    /// `rbxplace.toml` and, when its scopes need no owner, no request.
    #[tokio::test]
    async fn a_universe_id_resolves_without_rbxplace_or_a_request() {
        let synced = sync_targets(
            &offline_client(),
            &[Target::Universe(1234567890)],
            None,
            &BTreeMap::new(),
            false,
        )
        .await
        .unwrap();
        let entry = &synced["universe:1234567890"];
        assert_eq!(entry.universe_id, 1234567890);
        assert!(entry.owner_is_placeholder());
    }

    /// A place's universe is asked once and cached. A cached one is reused,
    /// owner included, without a request.
    #[tokio::test]
    async fn a_cached_place_is_not_resolved_again() {
        let mut cached = BTreeMap::new();
        cached.insert("place:109876543210987".to_string(), resolved(1234567890));
        let synced = sync_targets(
            &offline_client(),
            &[Target::Place(109876543210987)],
            None,
            &cached,
            true,
        )
        .await
        .unwrap();
        assert_eq!(synced["place:109876543210987"].universe_id, 1234567890);
        assert_eq!(synced["place:109876543210987"].owner_id, 42);
    }

    #[tokio::test]
    async fn an_env_without_rbxplace_names_the_fix() {
        let error = sync_targets(
            &offline_client(),
            &[Target::Env("prod".into())],
            None,
            &BTreeMap::new(),
            false,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("'prod'"), "{error}");
        assert!(error.contains("universe_ids"), "{error}");
    }

    /// `list` reads the cache alone. A target with no entry yet is a key not
    /// created yet, and is skipped rather than invented.
    #[test]
    fn cached_universe_ids_skip_what_is_not_cached() {
        let cfg = config(
            "[keys.deploy]\nscopes = [\"universe:read\"]\n\
             universe_ids = [1234567890]\nplace_ids = [109876543210987]\n",
        );
        let mut cached = BTreeMap::new();
        cached.insert("universe:1234567890".to_string(), placeholder(1234567890));
        assert_eq!(
            cached_universe_ids(&cfg, &cfg.keys["deploy"], &cached),
            vec![1234567890]
        );
    }
}
