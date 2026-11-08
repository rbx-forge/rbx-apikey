//! `rbxapikey.lock.toml`: tool-managed, never edit by hand. Tracks Roblox cloud_auth_id,
//! secret (or pointer to external secret file), creator, dates, and a normalized
//! per-env table of `(universe_id, owner_type, owner_id)`.
//!
//! Schema is bumped from v3 → v4: the per-key `universe_ids` / `universe_owners`
//! vectors were lifted into a top-level `[envs.<name>]` table referenced indirectly
//! through each key's `envs` list in `rbxapikey.toml`. Old v3 files are rejected
//! (no migration shim): delete the file and re-run `rbx-apikey create`.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use rbx_core::owner::OwnerType;
use serde::{Deserialize, Serialize};

pub const FILE: &str = "rbxapikey.lock.toml";
pub const VERSION: u32 = 4;

/// Cached (env → universe_id, owner) tuple. Each env appears exactly once
/// under `[envs.<name>]`; keys that reference the env in `rbxapikey.toml`
/// pick it up by name at runtime.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LockEnv {
    pub universe_id: u64,
    pub owner_type: OwnerType,
    pub owner_id: u64,
}

impl LockEnv {
    /// Whether the owner fields are the placeholder rather than a resolution.
    ///
    /// `sync_targets` writes `(User, 0)` when no scope on the key being built
    /// needs creator-targeting, because the schema has nowhere to say "not
    /// asked". Roblox issues no user or group id 0, so the value is
    /// unambiguous as a sentinel, but it is only a sentinel if every reader
    /// treats it as one, which is what this method is for.
    pub fn owner_is_placeholder(&self) -> bool {
        self.owner_id == 0
    }
}

/// In-memory shape returned by `lock::env_owner` so call sites that still
/// want a `(universe_id, owner_type, owner_id)` triple don't have to
/// reach into `LockEnv` themselves. Kept around (instead of being deleted
/// with the v3 schema) because `scope_builder::build` still takes a
/// `&[UniverseOwner]` slice: the *caller* now assembles it from
/// `[envs.X]` instead of reading it off `LockEntry`.
#[derive(Debug, Clone)]
pub struct UniverseOwner {
    pub universe_id: u64,
    pub owner_type: OwnerType,
    pub owner_id: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LockEntry {
    pub cloud_auth_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_file: Option<String>,
    /// The secret was printed with `--stdout` and never stored here. Without
    /// this, a key whose secret lives in a vault would read as
    /// `SECRET_MISSING` for the rest of its life.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub secret_external: bool,
    pub creator_id: u64,
    pub is_enabled: bool,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Lock {
    pub version: u32,
    #[serde(default)]
    pub envs: BTreeMap<String, LockEnv>,
    #[serde(default)]
    pub keys: BTreeMap<String, LockEntry>,
}

impl Default for Lock {
    fn default() -> Self {
        Lock {
            version: VERSION,
            envs: BTreeMap::new(),
            keys: BTreeMap::new(),
        }
    }
}

pub fn load() -> Result<Lock> {
    load_from(Path::new(FILE))
}

pub fn load_from(path: &Path) -> Result<Lock> {
    if !path.exists() {
        return Ok(Lock::default());
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let mut lock: Lock =
        toml::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))?;
    if lock.version == 0 {
        lock.version = VERSION;
    }
    if lock.version != VERSION {
        bail!(
            "{} has schema version {} but this tool expects v{}. \
             Lockfile migrations are not supported: delete {} and re-run \
             `rbx-apikey create --all` to regenerate it.",
            path.display(),
            lock.version,
            VERSION,
            path.display()
        );
    }
    Ok(lock)
}

pub fn save(lock: &Lock) -> Result<()> {
    save_to(lock, Path::new(FILE))
}

pub fn save_to(lock: &Lock, path: &Path) -> Result<()> {
    let s = toml::to_string_pretty(lock).context("failed to serialize lock file")?;
    // The default secret backend keeps the key secret in this file.
    rbx_core::lockfile::write_private(path, &s)
}

pub fn get<'a>(lock: &'a Lock, name: &str) -> Option<&'a LockEntry> {
    lock.keys.get(name)
}

pub fn set(lock: &mut Lock, name: &str, entry: LockEntry) {
    lock.keys.insert(name.to_string(), entry);
}

pub fn remove(lock: &mut Lock, name: &str) {
    lock.keys.remove(name);
}

/// Convert a `[envs.<name>]` entry into the legacy `UniverseOwner` triple
/// that `scope_builder::build` consumes. Helper, not stored anywhere.
pub fn env_to_owner(env: &LockEnv) -> UniverseOwner {
    UniverseOwner {
        universe_id: env.universe_id,
        owner_type: env.owner_type,
        owner_id: env.owner_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_minimal_entry() {
        let mut lock = Lock::default();
        lock.envs.insert(
            "dev".to_string(),
            LockEnv {
                universe_id: 1,
                owner_type: OwnerType::Group,
                owner_id: 99,
            },
        );
        lock.envs.insert(
            "prod".to_string(),
            LockEnv {
                universe_id: 2,
                owner_type: OwnerType::Group,
                owner_id: 99,
            },
        );
        lock.keys.insert(
            "mykey".to_string(),
            LockEntry {
                cloud_auth_id: "abc-123".into(),
                secret: Some("S".into()),
                secret_file: None,
                secret_external: false,
                creator_id: 42,
                is_enabled: true,
                created_at: "2026-05-01T10:00:00.000Z".into(),
                expires_at: Some("2027-08-01T10:00:00.000Z".into()),
            },
        );
        let s = toml::to_string_pretty(&lock).unwrap();
        let parsed: Lock = toml::from_str(&s).unwrap();
        assert_eq!(parsed.version, VERSION);
        let entry = parsed.keys.get("mykey").unwrap();
        assert_eq!(entry.cloud_auth_id, "abc-123");
        assert_eq!(entry.secret.as_deref(), Some("S"));
        assert_eq!(parsed.envs.len(), 2);
        assert_eq!(parsed.envs.get("dev").unwrap().universe_id, 1);
        assert_eq!(
            parsed.envs.get("prod").unwrap().owner_type,
            OwnerType::Group
        );
    }

    #[test]
    fn secret_file_backend_omits_secret() {
        let mut lock = Lock::default();
        lock.keys.insert(
            "mykey".to_string(),
            LockEntry {
                cloud_auth_id: "abc".into(),
                secret: None,
                secret_file: Some("./secret.key".into()),
                secret_external: false,
                creator_id: 1,
                is_enabled: true,
                created_at: "now".into(),
                expires_at: None,
            },
        );
        let s = toml::to_string_pretty(&lock).unwrap();
        assert!(
            !s.contains("\nsecret = "),
            "secret should be omitted: {}",
            s
        );
        assert!(s.contains("secret_file"));
    }

    /// The mark is written only when set, so every existing lockfile reads
    /// and writes back unchanged.
    #[test]
    fn the_external_mark_is_written_only_when_set() {
        let mut entry = LockEntry {
            cloud_auth_id: "abc".into(),
            secret: None,
            secret_file: None,
            secret_external: false,
            creator_id: 1,
            is_enabled: true,
            created_at: "now".into(),
            expires_at: None,
        };
        assert!(!toml::to_string(&entry).unwrap().contains("secret_external"));
        entry.secret_external = true;
        let s = toml::to_string(&entry).unwrap();
        assert!(s.contains("secret_external = true"), "{s}");
        let back: LockEntry = toml::from_str(&s).unwrap();
        assert!(back.secret_external);
    }

    #[test]
    fn owner_type_serializes_lowercase() {
        let env = LockEnv {
            universe_id: 1,
            owner_type: OwnerType::User,
            owner_id: 2,
        };
        let s = toml::to_string(&env).unwrap();
        assert!(s.contains("owner_type = \"user\""), "got {}", s);
    }

    #[test]
    fn load_rejects_old_v3_schema() {
        let dir = std::env::temp_dir().join(format!("rbxapikey_v3_reject_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rbxapikey.lock.toml");
        std::fs::write(
            &path,
            "version = 3\n\n[keys.mykey]\ncloud_auth_id = \"abc\"\ncreator_id = 1\nis_enabled = true\ncreated_at = \"now\"\n",
        )
        .unwrap();
        let err = load_from(&path).unwrap_err().to_string();
        assert!(
            err.contains("v4"),
            "expected message about v4, got: {}",
            err
        );
        assert!(
            err.contains("delete") || err.contains("re-run"),
            "expected actionable hint, got: {}",
            err
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_accepts_v4_schema() {
        let dir = std::env::temp_dir().join(format!("rbxapikey_v4_accept_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rbxapikey.lock.toml");
        std::fs::write(
            &path,
            "version = 4\n\n[envs.dev]\nuniverse_id = 10\nowner_type = \"group\"\nowner_id = 99\n\n[keys.mykey]\ncloud_auth_id = \"abc\"\ncreator_id = 1\nis_enabled = true\ncreated_at = \"now\"\n",
        )
        .unwrap();
        let lock = load_from(&path).unwrap();
        assert_eq!(lock.version, 4);
        assert_eq!(lock.envs.get("dev").unwrap().universe_id, 10);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
