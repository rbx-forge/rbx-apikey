//! Read/write the API key secret across backends: lockfile (default) or external file.
//! Backend selection is per-key (via `secret_file` in `rbxapikey.toml`).

use std::path::Path;

use anyhow::{Context, Result};

use crate::config::{self, Config, KeyConfig};
use crate::lock::LockEntry;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    Lockfile,
    File,
}

impl Backend {
    pub fn as_str(&self) -> &'static str {
        match self {
            Backend::Lockfile => "lockfile",
            Backend::File => "file",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub backend: Backend,
    /// For `Lockfile`: the tool name. For `File`: the file path.
    pub target: String,
}

pub fn backend_for(cfg: &Config, key_cfg: Option<&KeyConfig>, tool_name: &str) -> Resolved {
    if let Some(k) = key_cfg {
        if let Some(path) = config::resolve_secret_file(cfg, k, tool_name) {
            return Resolved {
                backend: Backend::File,
                target: path,
            };
        }
    }
    Resolved {
        backend: Backend::Lockfile,
        target: tool_name.to_string(),
    }
}

/// Returns None if the secret is missing or empty (does NOT error: callers decide).
pub fn read(resolved: &Resolved, lock_entry: Option<&LockEntry>) -> Option<String> {
    match resolved.backend {
        Backend::Lockfile => {
            let entry = lock_entry?;
            let s = entry.secret.as_ref()?;
            if s.is_empty() {
                None
            } else {
                Some(s.clone())
            }
        }
        Backend::File => {
            let path = Path::new(&resolved.target);
            if !path.is_file() {
                return None;
            }
            let raw = std::fs::read_to_string(path).ok()?;
            let trimmed = raw.trim_end().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        }
    }
}

pub fn write(resolved: &Resolved, secret: &str, lock_entry: &mut LockEntry) -> Result<()> {
    match resolved.backend {
        Backend::Lockfile => {
            lock_entry.secret = Some(secret.to_string());
        }
        Backend::File => {
            let path = Path::new(&resolved.target);
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() && !parent.exists() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("failed to create dir {}", parent.display()))?;
                }
            }
            rbx_core::lockfile::write_private(path, secret)?;
            lock_entry.secret = None;
        }
    }
    Ok(())
}

#[derive(Debug, Default)]
pub struct CleanupResult {
    pub auto_cleaned: Vec<String>,
    pub manual_action_needed: Vec<String>,
}

/// Backend that the lockfile entry was last written with. Mirrors `backend_for` but reads from the
/// lock entry instead of the live config: used by `update` to detect when the user changed
/// `secret_file` in `rbxapikey.toml` and we need to migrate the secret.
pub fn previous_backend_from_entry(entry: &LockEntry, tool_name: &str) -> Resolved {
    if let Some(path) = &entry.secret_file {
        if !path.is_empty() {
            return Resolved {
                backend: Backend::File,
                target: path.clone(),
            };
        }
    }
    Resolved {
        backend: Backend::Lockfile,
        target: tool_name.to_string(),
    }
}

/// True iff the two resolved backends point to different storage. For Lockfile→Lockfile this
/// always returns false (target is the tool name, identical on both sides).
pub fn backend_differs(a: &Resolved, b: &Resolved) -> bool {
    a.backend != b.backend || a.target != b.target
}

pub fn cleanup(resolved: &Resolved, delete_file: bool) -> CleanupResult {
    let mut result = CleanupResult::default();
    if resolved.backend == Backend::File {
        let path = Path::new(&resolved.target);
        if delete_file {
            if path.is_file() {
                if std::fs::remove_file(path).is_ok() {
                    result
                        .auto_cleaned
                        .push(format!("deleted secret file: {}", resolved.target));
                } else {
                    result
                        .manual_action_needed
                        .push(format!("could not delete secret file: {}", resolved.target));
                }
            }
        } else {
            result
                .manual_action_needed
                .push(format!("secret file still present: {}", resolved.target));
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ScopeSpec, Settings};
    use std::collections::BTreeMap;

    fn empty_cfg() -> Config {
        Config {
            settings: Settings {
                default_enabled: true,
                ..Default::default()
            },
            keys: BTreeMap::new(),
        }
    }

    fn key_cfg_with_secret_file(p: Option<&str>) -> KeyConfig {
        KeyConfig {
            readonly: false,
            envs: vec![],
            env_group: None,
            universe_ids: Vec::new(),
            place_ids: Vec::new(),
            group_ids: vec![],
            user_ids: vec![],
            scopes: vec![ScopeSpec {
                scope_type: "universe".into(),
                operations: vec!["read".into()],
            }],
            datastores: vec![],
            name: None,
            description: None,
            enabled: None,
            expiration_months: None,
            expiration_days: None,
            expires_at: None,
            allowed_cidrs: None,
            secret_file: p.map(|s| s.to_string()),
        }
    }

    #[test]
    fn backend_defaults_to_lockfile() {
        let r = backend_for(&empty_cfg(), None, "foo");
        assert_eq!(r.backend, Backend::Lockfile);
        assert_eq!(r.target, "foo");
    }

    #[test]
    fn backend_uses_secret_file_when_set() {
        let k = key_cfg_with_secret_file(Some("/tmp/x.key"));
        let r = backend_for(&empty_cfg(), Some(&k), "foo");
        assert_eq!(r.backend, Backend::File);
        assert_eq!(r.target, "/tmp/x.key");
    }

    #[test]
    fn backend_ignores_empty_secret_file() {
        let k = key_cfg_with_secret_file(Some(""));
        let r = backend_for(&empty_cfg(), Some(&k), "foo");
        assert_eq!(r.backend, Backend::Lockfile);
    }

    #[test]
    fn backend_uses_default_secret_file_template_when_key_has_none() {
        let k = key_cfg_with_secret_file(None);
        let mut cfg = empty_cfg();
        cfg.settings.default_secret_file = Some(".secrets/{name}.env".into());
        let r = backend_for(&cfg, Some(&k), "deploy");
        assert_eq!(r.backend, Backend::File);
        assert_eq!(r.target, ".secrets/deploy.env");
    }

    #[test]
    fn explicit_secret_file_wins_over_template() {
        let k = key_cfg_with_secret_file(Some("/explicit/path.env"));
        let mut cfg = empty_cfg();
        cfg.settings.default_secret_file = Some(".secrets/{name}.env".into());
        let r = backend_for(&cfg, Some(&k), "deploy");
        assert_eq!(r.target, "/explicit/path.env");
    }

    fn entry_with(secret: Option<&str>, secret_file: Option<&str>) -> LockEntry {
        LockEntry {
            cloud_auth_id: "id".into(),
            secret: secret.map(|s| s.to_string()),
            secret_file: secret_file.map(|s| s.to_string()),
            creator_id: 1,
            is_enabled: true,
            created_at: "now".into(),
            expires_at: None,
        }
    }

    #[test]
    fn previous_backend_is_lockfile_when_no_secret_file() {
        let e = entry_with(Some("S"), None);
        let r = previous_backend_from_entry(&e, "foo");
        assert_eq!(r.backend, Backend::Lockfile);
        assert_eq!(r.target, "foo");
    }

    #[test]
    fn previous_backend_is_file_when_secret_file_set() {
        let e = entry_with(None, Some("/tmp/x"));
        let r = previous_backend_from_entry(&e, "foo");
        assert_eq!(r.backend, Backend::File);
        assert_eq!(r.target, "/tmp/x");
    }

    #[test]
    fn previous_backend_ignores_empty_secret_file() {
        let e = entry_with(Some("S"), Some(""));
        let r = previous_backend_from_entry(&e, "foo");
        assert_eq!(r.backend, Backend::Lockfile);
    }

    #[test]
    fn backend_differs_detects_added_secret_file() {
        let prev = Resolved {
            backend: Backend::Lockfile,
            target: "foo".into(),
        };
        let new = Resolved {
            backend: Backend::File,
            target: "/tmp/x".into(),
        };
        assert!(backend_differs(&prev, &new));
    }

    #[test]
    fn backend_differs_detects_removed_secret_file() {
        let prev = Resolved {
            backend: Backend::File,
            target: "/tmp/x".into(),
        };
        let new = Resolved {
            backend: Backend::Lockfile,
            target: "foo".into(),
        };
        assert!(backend_differs(&prev, &new));
    }

    #[test]
    fn backend_differs_detects_changed_path() {
        let prev = Resolved {
            backend: Backend::File,
            target: "/tmp/a".into(),
        };
        let new = Resolved {
            backend: Backend::File,
            target: "/tmp/b".into(),
        };
        assert!(backend_differs(&prev, &new));
    }

    #[test]
    fn backend_does_not_differ_for_same_lockfile() {
        let a = Resolved {
            backend: Backend::Lockfile,
            target: "foo".into(),
        };
        let b = Resolved {
            backend: Backend::Lockfile,
            target: "foo".into(),
        };
        assert!(!backend_differs(&a, &b));
    }

    #[test]
    fn backend_does_not_differ_for_same_file_path() {
        let a = Resolved {
            backend: Backend::File,
            target: "/tmp/x".into(),
        };
        let b = Resolved {
            backend: Backend::File,
            target: "/tmp/x".into(),
        };
        assert!(!backend_differs(&a, &b));
    }

    #[test]
    fn write_to_file_then_read_round_trip() {
        let tmp = std::env::temp_dir().join(format!("rbxapikey_test_{}.key", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let resolved = Resolved {
            backend: Backend::File,
            target: tmp.to_string_lossy().to_string(),
        };
        let mut entry = entry_with(Some("ignored"), None);
        write(&resolved, "S3CR3T", &mut entry).unwrap();
        // File backend clears entry.secret so the lockfile doesn't leak.
        assert!(entry.secret.is_none());
        // Read back from disk.
        let got = read(&resolved, Some(&entry)).unwrap();
        assert_eq!(got, "S3CR3T");
        // Cleanup
        let cleanup = cleanup(&resolved, true);
        assert_eq!(cleanup.auto_cleaned.len(), 1);
        assert!(!Path::new(&resolved.target).exists());
    }
}
