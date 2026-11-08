//! Turn parsed `KeyConfig` + cached owners into the `ScopeDef[]` payload Roblox expects.
//!
//! Catalog `target_type` drives the shape of `targetParts`:
//!   - "universe"           → one entry per universe_id
//!   - "universe-datastore" → one entry per universe_id; per-datastore entries are appended separately
//!   - "creator"            → one entry per group/user (`G<id>` / `U<id>`) or per universe owner
//!   - "none"               → single entry with ["*"]

use rbx_core::owner::OwnerType;
use serde::{Deserialize, Serialize};

use crate::config::{KeyConfig, ScopeSpec};
use crate::lock::UniverseOwner;
use crate::scope_catalog;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ScopeDef {
    pub scope_type: String,
    pub target_parts: Vec<String>,
    pub operations: Vec<String>,
}

#[derive(Debug)]
pub struct BuildResult {
    pub scopes: Vec<ScopeDef>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Owner {
    kind: OwnerType,
    id: u64,
}

fn owner_target(o: &Owner) -> String {
    match o.kind {
        OwnerType::Group => format!("G{}", o.id),
        OwnerType::User => format!("U{}", o.id),
    }
}

fn creator_targets(key_cfg: &KeyConfig, owners: &[Owner]) -> Vec<String> {
    let mut parts = Vec::new();
    for g in &key_cfg.group_ids {
        parts.push(format!("G{}", g));
    }
    for u in &key_cfg.user_ids {
        parts.push(format!("U{}", u));
    }
    if parts.is_empty() {
        for o in owners {
            parts.push(owner_target(o));
        }
    }
    if parts.is_empty() {
        parts.push("*".to_string());
    }
    parts
}

fn universe_ids_as_strings(universe_ids: &[u64]) -> Vec<String> {
    universe_ids.iter().map(|u| u.to_string()).collect()
}

fn emit_for_scope(
    spec: &ScopeSpec,
    key_cfg: &KeyConfig,
    universe_ids: &[u64],
    owners: &[Owner],
    out: &mut Vec<ScopeDef>,
) {
    let lookup = scope_catalog::lookup(&spec.scope_type);
    let target = lookup.target_type.unwrap_or_else(|| "universe".to_string());

    if target == "none" {
        out.push(ScopeDef {
            scope_type: spec.scope_type.clone(),
            target_parts: vec!["*".to_string()],
            operations: spec.operations.clone(),
        });
        return;
    }

    if target == "creator" {
        for part in creator_targets(key_cfg, owners) {
            out.push(ScopeDef {
                scope_type: spec.scope_type.clone(),
                target_parts: vec![part],
                operations: spec.operations.clone(),
            });
        }
        return;
    }

    // "universe" or "universe-datastore" → one entry per universe_id (or "*" if none).
    let uids = universe_ids_as_strings(universe_ids);
    if uids.is_empty() {
        out.push(ScopeDef {
            scope_type: spec.scope_type.clone(),
            target_parts: vec!["*".to_string()],
            operations: spec.operations.clone(),
        });
        return;
    }
    for uid in uids {
        out.push(ScopeDef {
            scope_type: spec.scope_type.clone(),
            target_parts: vec![uid],
            operations: spec.operations.clone(),
        });
    }
}

fn emit_for_datastores(key_cfg: &KeyConfig, out: &mut Vec<ScopeDef>) {
    for ds in &key_cfg.datastores {
        out.push(ScopeDef {
            scope_type: "universe-datastores.objects".to_string(),
            target_parts: vec![ds.universe_id.to_string(), ds.name.clone()],
            operations: ds.operations.clone(),
        });
    }
}

/// Collapse entries naming the same scope type and the same target into one,
/// taking the union of their operations.
///
/// `universe:read` and `universe:write` on two lines of `rbxapikey.toml` are two
/// entries by the time they get here, and one permission set by any reading.
/// Roblox stores them as one entry holding both operations: measured on
/// 2026-09-01 against a key that declared them separately, eight entries sent
/// and seven stored. So this sends the shape the API answers in rather than one
/// it has to normalise, which is what makes a created key comparable to the
/// config that asked for it.
///
/// This subsumes collapsing exact duplicates, which is the same operation with
/// nothing left to add.
///
/// Order is each group's first appearance, and operations keep their declared
/// order. Roblox reorders both (`write,manage-and-spend-robux` came back
/// `["manage-and-spend-robux","write"]` in the same measurement), so neither is
/// load-bearing; a stable payload is for whoever diffs two runs.
fn merge_same_target(scopes: Vec<ScopeDef>) -> Vec<ScopeDef> {
    let mut out: Vec<ScopeDef> = Vec::with_capacity(scopes.len());

    for scope in scopes {
        let merged = out.iter_mut().find(|kept| {
            kept.scope_type == scope.scope_type && kept.target_parts == scope.target_parts
        });
        match merged {
            Some(kept) => {
                for operation in scope.operations {
                    if !kept.operations.contains(&operation) {
                        kept.operations.push(operation);
                    }
                }
            }
            None => out.push(scope),
        }
    }

    out
}

pub fn build(
    key_cfg: &KeyConfig,
    universe_ids: &[u64],
    universe_owners: &[UniverseOwner],
) -> BuildResult {
    // Dedup owners, preserving order.
    let mut owners: Vec<Owner> = Vec::new();
    let mut seen: std::collections::HashSet<(OwnerType, u64)> = std::collections::HashSet::new();
    for uo in universe_owners {
        if seen.insert((uo.owner_type, uo.owner_id)) {
            owners.push(Owner {
                kind: uo.owner_type,
                id: uo.owner_id,
            });
        }
    }

    let mut scopes = Vec::new();
    let mut warnings = Vec::new();

    for spec in &key_cfg.scopes {
        let lookup = scope_catalog::lookup(&spec.scope_type);
        if !lookup.known {
            // Nothing for the reader to run: the catalog is embedded with
            // `include_str!`, so an installed binary cannot refresh it, and
            // `catalog regenerate` (where this used to point) only does
            // anything from a checkout. What is actionable is the guess itself,
            // and where a wrong one surfaces.
            warnings.push(format!(
                "unknown scope \"{}\" - not in this binary's catalog, sending it as a universe-target scope. If that guess is wrong, key creation is where it fails, not a later call. The catalog ships with the binary, so a newer release is what carries scopes Roblox has added since.",
                spec.scope_type
            ));
        } else {
            let unk = scope_catalog::unknown_operations(&spec.scope_type, &spec.operations);
            if !unk.is_empty() {
                warnings.push(format!(
                    "scope \"{}\" has unknown operations: {} - sending anyway.",
                    spec.scope_type,
                    unk.join(", ")
                ));
            }
        }
        emit_for_scope(spec, key_cfg, universe_ids, &owners, &mut scopes);
    }

    emit_for_datastores(key_cfg, &mut scopes);

    BuildResult {
        scopes: merge_same_target(scopes),
        warnings,
    }
}

/// True iff at least one scope is creator-targeted AND no explicit group_ids/user_ids are set.
/// Owner resolution (an HTTP call per universe) is skipped otherwise.
pub fn needs_owner_resolution(key_cfg: &KeyConfig) -> bool {
    if !key_cfg.group_ids.is_empty() || !key_cfg.user_ids.is_empty() {
        return false;
    }
    for spec in &key_cfg.scopes {
        let l = scope_catalog::lookup(&spec.scope_type);
        if l.known && l.target_type.as_deref() == Some("creator") {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DatastoreSpec, KeyConfig, ScopeSpec};

    fn k(scope: &str, ops: &[&str]) -> KeyConfig {
        KeyConfig {
            readonly: false,
            envs: vec![],
            env_group: None,
            universe_ids: Vec::new(),
            place_ids: Vec::new(),
            group_ids: vec![],
            user_ids: vec![],
            scopes: vec![ScopeSpec {
                scope_type: scope.to_string(),
                operations: ops.iter().map(|s| s.to_string()).collect(),
            }],
            datastores: vec![],
            name: None,
            description: None,
            enabled: None,
            expiration_months: None,
            expiration_days: None,
            expires_at: None,
            allowed_cidrs: None,
            secret_file: None,
        }
    }

    #[test]
    fn universe_scope_emits_per_universe() {
        let k = k("universe", &["read"]);
        let r = build(&k, &[111, 222], &[]);
        assert_eq!(r.scopes.len(), 2);
        assert_eq!(r.scopes[0].target_parts, vec!["111".to_string()]);
        assert_eq!(r.scopes[1].target_parts, vec!["222".to_string()]);
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn none_target_emits_single_wildcard() {
        let k = k("thumbnail", &["read"]);
        let r = build(&k, &[], &[]);
        assert_eq!(r.scopes.len(), 1);
        assert_eq!(r.scopes[0].target_parts, vec!["*".to_string()]);
    }

    #[test]
    fn creator_falls_back_to_wildcard_without_owners() {
        let k = k("asset", &["read"]);
        let r = build(&k, &[], &[]);
        assert_eq!(r.scopes.len(), 1);
        assert_eq!(r.scopes[0].target_parts, vec!["*".to_string()]);
    }

    /// The key that could not be created, as its TOML declared it.
    ///
    /// `group_ids` is pinned and the scope is `group:read,write`, which used to
    /// emit `targetParts: ["G<id>"]`. The service takes no target for group
    /// scopes and answered `500 Internal Server Error: Exception was thrown by
    /// handler`, naming nothing. One entry with `["*"]` is what it accepts.
    #[test]
    fn a_group_scope_sends_no_target_even_with_group_ids_pinned() {
        let mut k = k("group", &["read", "write"]);
        k.group_ids = vec![1234567890];

        let r = build(&k, &[], &[]);

        assert_eq!(r.scopes.len(), 1, "{:?}", r.scopes);
        assert_eq!(r.scopes[0].scope_type, "group");
        assert_eq!(r.scopes[0].target_parts, vec!["*".to_string()]);
        assert_eq!(r.scopes[0].operations, vec!["read", "write"]);
    }

    #[test]
    fn creator_uses_explicit_group_ids_first() {
        // `asset`, not `group`: the service says group scopes take no target,
        // so they no longer reach this branch at all.
        let mut k = k("asset", &["read"]);
        k.group_ids = vec![99];
        let r = build(
            &k,
            &[],
            &[UniverseOwner {
                universe_id: 1,
                owner_type: OwnerType::User,
                owner_id: 42,
            }],
        );
        // Explicit group_ids take precedence over universe owners.
        assert_eq!(r.scopes[0].target_parts, vec!["G99".to_string()]);
    }

    #[test]
    fn creator_uses_owners_when_no_explicit() {
        let k = k("asset", &["read"]);
        let r = build(
            &k,
            &[],
            &[UniverseOwner {
                universe_id: 1,
                owner_type: OwnerType::Group,
                owner_id: 7,
            }],
        );
        assert_eq!(r.scopes[0].target_parts, vec!["G7".to_string()]);
    }

    #[test]
    fn datastore_entries_appended() {
        let mut k = k("universe-datastores.objects", &["read"]);
        k.datastores = vec![DatastoreSpec {
            universe_id: 10,
            name: "UserData".into(),
            operations: vec!["read".into(), "write".into()],
        }];
        let r = build(&k, &[10], &[]);
        assert_eq!(r.scopes.len(), 2);
        // First: universe-wide entry.
        assert_eq!(r.scopes[0].target_parts, vec!["10".to_string()]);
        // Second: scoped to UserData.
        assert_eq!(
            r.scopes[1].target_parts,
            vec!["10".to_string(), "UserData".to_string()]
        );
        assert_eq!(r.scopes[1].operations, vec!["read", "write"]);
    }

    #[test]
    fn unknown_scope_emits_warning() {
        let k = k("custom-future-scope", &["read"]);
        let r = build(&k, &[1], &[]);
        assert!(!r.warnings.is_empty());
        assert!(r.warnings[0].contains("unknown scope"));
    }

    /// The advice used to send readers to a command that does nothing for them:
    /// the catalog is embedded, so an installed binary cannot refresh it.
    #[test]
    fn the_unknown_scope_warning_does_not_send_users_to_a_maintainer_command() {
        let k = k("custom-future-scope", &["read"]);
        let r = build(&k, &[1], &[]);
        assert!(
            !r.warnings[0].contains("catalog regenerate"),
            "{}",
            r.warnings[0]
        );
        // What is actionable instead: the guess, and where a wrong one fails.
        assert!(
            r.warnings[0].contains("universe-target"),
            "{}",
            r.warnings[0]
        );
        assert!(r.warnings[0].contains("key creation"), "{}", r.warnings[0]);
    }

    #[test]
    fn unknown_op_for_known_scope_warns() {
        let k = k("universe", &["read", "fly"]);
        let r = build(&k, &[1], &[]);
        assert!(r.warnings.iter().any(|w| w.contains("fly")));
    }

    #[test]
    fn scope_def_serializes_to_camel_case_for_roblox() {
        let def = ScopeDef {
            scope_type: "universe".into(),
            target_parts: vec!["12345".into()],
            operations: vec!["read".into()],
        };
        let s = serde_json::to_string(&def).unwrap();
        assert!(s.contains("\"scopeType\""), "got {}", s);
        assert!(s.contains("\"targetParts\""), "got {}", s);
        assert!(s.contains("\"operations\""), "got {}", s);
        // Negative: snake_case must not leak through.
        assert!(!s.contains("scope_type"));
        assert!(!s.contains("target_parts"));
    }

    #[test]
    fn config_properties_serializes_to_camel_case() {
        use crate::api::api_keys::ConfigProperties;
        let cp = ConfigProperties {
            name: "k".into(),
            description: "d".into(),
            is_enabled: true,
            expiration_time: Some("2026-01-01T00:00:00.000Z".into()),
            allowed_cidrs: vec!["10.0.0.0/8".into()],
            scopes: vec![],
        };
        let s = serde_json::to_string(&cp).unwrap();
        assert!(s.contains("\"isEnabled\""), "got {}", s);
        assert!(s.contains("\"expirationTime\""), "got {}", s);
        assert!(s.contains("\"allowedCidrs\""), "got {}", s);
        assert!(!s.contains("is_enabled"));
        assert!(!s.contains("expiration_time"));
        assert!(!s.contains("allowed_cidrs"));
    }

    /// The measurement: `universe:read` and `universe:write` written on
    /// two lines are what Roblox stores as one entry, so one entry is what goes
    /// out.
    #[test]
    fn two_operation_sets_on_one_scope_type_become_one_entry() {
        let mut cfg = k("universe", &["read"]);
        cfg.scopes.push(ScopeSpec {
            scope_type: "universe".to_string(),
            operations: vec!["write".to_string()],
        });

        let r = build(&cfg, &[111], &[]);

        assert_eq!(r.scopes.len(), 1);
        assert_eq!(r.scopes[0].target_parts, vec!["111".to_string()]);
        assert_eq!(
            r.scopes[0].operations,
            vec!["read".to_string(), "write".to_string()]
        );
    }

    /// Merging is per target, not per type. Two universes are two permissions
    /// and stay two entries, which is the line between collapsing a payload and
    /// widening a key.
    #[test]
    fn the_same_type_on_different_targets_stays_separate() {
        let mut cfg = k("universe", &["read"]);
        cfg.scopes.push(ScopeSpec {
            scope_type: "universe".to_string(),
            operations: vec!["write".to_string()],
        });

        let r = build(&cfg, &[111, 222], &[]);

        assert_eq!(r.scopes.len(), 2);
        for scope in &r.scopes {
            assert_eq!(
                scope.operations,
                vec!["read".to_string(), "write".to_string()]
            );
        }
        assert_eq!(r.scopes[0].target_parts, vec!["111".to_string()]);
        assert_eq!(r.scopes[1].target_parts, vec!["222".to_string()]);
    }

    /// An operation named twice is not an operation held twice. Roblox would
    /// have dropped the repeat anyway; sending it once is what keeps the count
    /// this tool prints equal to the count Roblox reports.
    #[test]
    fn an_operation_declared_twice_is_sent_once() {
        let mut cfg = k("universe", &["read", "write"]);
        cfg.scopes.push(ScopeSpec {
            scope_type: "universe".to_string(),
            operations: vec!["write".to_string(), "read".to_string()],
        });

        let r = build(&cfg, &[111], &[]);

        assert_eq!(r.scopes.len(), 1);
        assert_eq!(
            r.scopes[0].operations,
            vec!["read".to_string(), "write".to_string()]
        );
    }

    #[test]
    fn needs_owner_resolution_only_for_creator_scopes() {
        let universe_only = k("universe", &["read"]);
        assert!(!needs_owner_resolution(&universe_only));

        let creator_no_explicit = k("asset", &["read"]);
        assert!(needs_owner_resolution(&creator_no_explicit));

        // A key declaring only group scopes needs no owner at all now, which
        // is one fewer permission lookup on every such create.
        let group_only = k("group", &["read"]);
        assert!(!needs_owner_resolution(&group_only));

        let mut creator_with_explicit = k("asset", &["read"]);
        creator_with_explicit.group_ids = vec![1];
        assert!(!needs_owner_resolution(&creator_with_explicit));
    }
}
