#![allow(clippy::unwrap_used)]

//! Tests for the scope-payload builder. We construct `KeyConfig` literals
//! and check the resulting `ScopeDef[]` matches what the Roblox API expects.

use rbx_apikey::config::{KeyConfig, ScopeSpec};
use rbx_apikey::scope_builder::{build, needs_owner_resolution};

fn key_with_scopes(scope_specs: Vec<ScopeSpec>) -> KeyConfig {
    KeyConfig {
        readonly: false,
        envs: vec![],
        env_group: None,
        universe_ids: Vec::new(),
        place_ids: Vec::new(),
        group_ids: vec![],
        user_ids: vec![],
        scopes: scope_specs,
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

fn scope(scope_type: &str, ops: &[&str]) -> ScopeSpec {
    ScopeSpec {
        scope_type: scope_type.into(),
        operations: ops.iter().map(|s| (*s).to_string()).collect(),
    }
}

// ---------------------------------------------------------------------------
// Universe-targeted scopes
// ---------------------------------------------------------------------------

#[test]
fn universe_scope_with_explicit_universe_ids_emits_one_per_id() {
    let k = key_with_scopes(vec![scope(
        "universe-datastores.objects",
        &["read", "write"],
    )]);
    let result = build(&k, &[100, 200], &[]);

    // One ScopeDef per universe_id.
    let universe_scopes: Vec<_> = result
        .scopes
        .iter()
        .filter(|s| s.scope_type == "universe-datastores.objects")
        .collect();
    assert_eq!(universe_scopes.len(), 2);
    let target_parts: Vec<&str> = universe_scopes
        .iter()
        .flat_map(|s| s.target_parts.iter().map(String::as_str))
        .collect();
    assert!(target_parts.contains(&"100"));
    assert!(target_parts.contains(&"200"));
    // Operations preserved.
    assert_eq!(universe_scopes[0].operations, vec!["read", "write"]);
}

#[test]
fn universe_scope_without_universe_ids_uses_wildcard() {
    let k = key_with_scopes(vec![scope("universe-datastores.objects", &["read"])]);
    let result = build(&k, &[], &[]);

    assert_eq!(result.scopes.len(), 1);
    assert_eq!(result.scopes[0].target_parts, vec!["*"]);
}

/// End-to-end over the embedded catalog: these three scopes were classified
/// `creator` by a fall-through, which builds a key over `G<id>`/`U<id>` (every
/// universe the owner has) for an API whose every path is rooted at a single
/// universe. Guards the catalog entry and the builder together, since a
/// regenerate that reverted the entry would not fail any test in `catalog.rs`.
#[test]
fn memory_store_scopes_are_targeted_at_the_named_universes() {
    for (scope_type, ops) in [
        ("memory-store", &["flush"][..]),
        ("memory-store.queue", &["add", "dequeue"][..]),
        ("memory-store.sorted-map", &["read", "write"][..]),
    ] {
        let k = key_with_scopes(vec![scope(scope_type, ops)]);
        let result = build(&k, &[100, 200], &[]);

        let emitted: Vec<_> = result
            .scopes
            .iter()
            .filter(|s| s.scope_type == scope_type)
            .collect();
        assert_eq!(
            emitted.len(),
            2,
            "{scope_type} should emit one per universe"
        );
        assert_eq!(emitted[0].target_parts, vec!["100"], "{scope_type}");
        assert_eq!(emitted[1].target_parts, vec!["200"], "{scope_type}");
        assert!(
            result.warnings.is_empty(),
            "{scope_type} should be a known scope, got: {:?}",
            result.warnings
        );
    }
}

/// Follows from the above: a universe-targeted scope needs no owner lookup, so
/// a memory-store key no longer pays the extra resolution call on create.
#[test]
fn needs_owner_resolution_false_for_memory_store() {
    let k = key_with_scopes(vec![scope("memory-store.sorted-map", &["read", "write"])]);
    assert!(!needs_owner_resolution(&k));
}

/// The spec states `universes` for both, against a heuristic that called them
/// `none` and built them over `*`.
#[test]
fn shop_scopes_are_targeted_at_the_named_universes() {
    for scope_type in ["developer-product", "game-pass"] {
        let k = key_with_scopes(vec![scope(scope_type, &["read"])]);
        let result = build(&k, &[100], &[]);

        let emitted: Vec<_> = result
            .scopes
            .iter()
            .filter(|s| s.scope_type == scope_type)
            .collect();
        assert_eq!(emitted.len(), 1, "{scope_type}");
        assert_eq!(emitted[0].target_parts, vec!["100"], "{scope_type}");
    }
}

// ---------------------------------------------------------------------------
// Creator-targeted scopes (asset:*, etc.)
// ---------------------------------------------------------------------------

#[test]
fn creator_scope_with_explicit_group_ids() {
    let mut k = key_with_scopes(vec![scope("asset", &["read"])]);
    k.group_ids = vec![42];

    let result = build(&k, &[], &[]);
    let asset_scopes: Vec<_> = result
        .scopes
        .iter()
        .filter(|s| s.scope_type == "asset")
        .collect();
    assert_eq!(asset_scopes.len(), 1);
    assert_eq!(asset_scopes[0].target_parts, vec!["G42"]);
}

#[test]
fn creator_scope_with_explicit_user_ids() {
    let mut k = key_with_scopes(vec![scope("asset", &["read"])]);
    k.user_ids = vec![123];

    let result = build(&k, &[], &[]);
    let asset_scopes: Vec<_> = result
        .scopes
        .iter()
        .filter(|s| s.scope_type == "asset")
        .collect();
    assert_eq!(asset_scopes.len(), 1);
    assert_eq!(asset_scopes[0].target_parts, vec!["U123"]);
}

#[test]
fn creator_scope_with_no_ids_and_no_owners_uses_wildcard() {
    let k = key_with_scopes(vec![scope("asset", &["read"])]);
    let result = build(&k, &[], &[]);
    let asset_scopes: Vec<_> = result
        .scopes
        .iter()
        .filter(|s| s.scope_type == "asset")
        .collect();
    assert_eq!(asset_scopes[0].target_parts, vec!["*"]);
}

// ---------------------------------------------------------------------------
// Datastores section
// ---------------------------------------------------------------------------

#[test]
fn datastores_emit_universe_datastores_objects_scopes() {
    let mut k = key_with_scopes(vec![]);
    k.datastores = vec![
        rbx_apikey::config::DatastoreSpec {
            universe_id: 100,
            name: "PlayerData".into(),
            operations: vec!["read".into(), "write".into()],
        },
        rbx_apikey::config::DatastoreSpec {
            universe_id: 100,
            name: "Inventory".into(),
            operations: vec!["read".into()],
        },
    ];
    let result = build(&k, &[], &[]);
    let ds_scopes: Vec<_> = result
        .scopes
        .iter()
        .filter(|s| s.scope_type == "universe-datastores.objects")
        .collect();
    assert_eq!(ds_scopes.len(), 2);
    assert_eq!(ds_scopes[0].target_parts, vec!["100", "PlayerData"]);
    assert_eq!(ds_scopes[1].target_parts, vec!["100", "Inventory"]);
    assert_eq!(ds_scopes[1].operations, vec!["read"]);
}

// ---------------------------------------------------------------------------
// Unknown scopes warn but emit anyway
// ---------------------------------------------------------------------------

#[test]
fn unknown_scope_type_produces_warning() {
    let k = key_with_scopes(vec![scope("this-scope-does-not-exist", &["read"])]);
    let result = build(&k, &[], &[]);
    assert!(
        result.warnings.iter().any(|w| w.contains("unknown scope")),
        "expected an 'unknown scope' warning, got: {:?}",
        result.warnings
    );
    // Still emitted (best-guess universe target).
    assert!(!result.scopes.is_empty());
}

// ---------------------------------------------------------------------------
// needs_owner_resolution
// ---------------------------------------------------------------------------

#[test]
fn needs_owner_resolution_false_when_explicit_group_set() {
    let mut k = key_with_scopes(vec![scope("asset", &["read"])]);
    k.group_ids = vec![1];
    assert!(!needs_owner_resolution(&k));
}

#[test]
fn needs_owner_resolution_false_when_explicit_user_set() {
    let mut k = key_with_scopes(vec![scope("asset", &["read"])]);
    k.user_ids = vec![1];
    assert!(!needs_owner_resolution(&k));
}

#[test]
fn needs_owner_resolution_false_when_no_creator_scope() {
    let k = key_with_scopes(vec![scope("universe-datastores.objects", &["read"])]);
    assert!(!needs_owner_resolution(&k));
}

#[test]
fn needs_owner_resolution_true_when_creator_scope_and_no_explicit_ids() {
    let k = key_with_scopes(vec![scope("asset", &["read"])]);
    assert!(needs_owner_resolution(&k));
}
