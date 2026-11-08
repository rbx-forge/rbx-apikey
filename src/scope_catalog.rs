//! Bundled scope catalog. Loaded once from `src/data/catalog.json` (embedded at compile time).
//! Lookups are advisory: unknown scopes emit warnings, not errors.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScopeInfo {
    pub operations: Vec<String>,
    pub target_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Catalog {
    pub version: String,
    pub source_url: String,
    /// The key service's scope list, which decides every `target_type` here.
    ///
    /// Separate from `source_url` because the two documents answer different
    /// questions: the spec says which scopes exist and what they are for, the
    /// service says what a creation request may send for them. They disagreed
    /// about fourteen scopes, and only this one is authoritative.
    ///
    /// Optional so a catalog written before the reconciliation still loads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority_url: Option<String>,
    pub scopes: BTreeMap<String, ScopeInfo>,
}

const EMBEDDED_CATALOG: &str = include_str!("data/catalog.json");

fn catalog() -> &'static Catalog {
    static C: OnceLock<Catalog> = OnceLock::new();
    C.get_or_init(|| {
        serde_json::from_str(EMBEDDED_CATALOG).expect("embedded src/data/catalog.json is invalid")
    })
}

pub fn version() -> &'static str {
    &catalog().version
}

pub fn source_url() -> &'static str {
    &catalog().source_url
}

/// The key service's scope list, if this catalog was reconciled against it.
pub fn authority_url() -> Option<&'static str> {
    catalog().authority_url.as_deref()
}

#[derive(Debug, Clone)]
pub struct Lookup {
    pub known: bool,
    pub target_type: Option<String>,
    pub known_operations: Option<Vec<String>>,
}

pub fn lookup(scope_type: &str) -> Lookup {
    match catalog().scopes.get(scope_type) {
        Some(info) => Lookup {
            known: true,
            target_type: Some(info.target_type.clone()),
            known_operations: Some(info.operations.clone()),
        },
        None => Lookup {
            known: false,
            target_type: None,
            known_operations: None,
        },
    }
}

/// Operations the user asked for that the catalog doesn't list for this scope.
/// Returns an empty Vec for unknown scopes (callers should check `lookup().known`).
pub fn unknown_operations(scope_type: &str, requested: &[String]) -> Vec<String> {
    let info = match catalog().scopes.get(scope_type) {
        Some(i) => i,
        None => return Vec::new(),
    };
    let known: std::collections::HashSet<&str> =
        info.operations.iter().map(|s| s.as_str()).collect();
    requested
        .iter()
        .filter(|op| !known.contains(op.as_str()))
        .cloned()
        .collect()
}

pub fn all_scopes() -> Vec<String> {
    let mut v: Vec<String> = catalog().scopes.keys().cloned().collect();
    v.sort();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_catalog_loads() {
        assert!(!version().is_empty());
        assert!(!all_scopes().is_empty());
    }

    #[test]
    fn known_scope_lookup() {
        let l = lookup("universe");
        assert!(l.known);
        assert_eq!(l.target_type.as_deref(), Some("universe"));
        assert!(l.known_operations.unwrap().contains(&"read".to_string()));
    }

    #[test]
    fn unknown_scope_lookup() {
        let l = lookup("definitely-not-a-real-scope");
        assert!(!l.known);
        assert!(l.target_type.is_none());
    }

    #[test]
    fn unknown_ops_for_known_scope() {
        let unk = unknown_operations("universe", &["read".into(), "destroy".into()]);
        assert_eq!(unk, vec!["destroy".to_string()]);
    }

    #[test]
    fn unknown_ops_for_unknown_scope_is_empty() {
        let unk = unknown_operations("not-real", &["x".into()]);
        assert!(unk.is_empty());
    }

    /// Guards the shipped catalog, not the reconciliation logic.
    ///
    /// `group` targeted `creator` here once, and `apikey create` sent a target
    /// part for a scope that takes none, which the service answered with a 500
    /// naming nothing. A `regenerate` run against the endpoint spec alone
    /// would put it back, so the assertion lives on the embedded file.
    #[test]
    fn group_scopes_take_no_target() {
        for scope in ["group", "group-forum"] {
            assert_eq!(
                lookup(scope).target_type.as_deref(),
                Some("none"),
                "{scope} must take no target part"
            );
        }
    }

    /// The counterweight. `asset` really is creator-targeted, so a change that
    /// made every scope targetless would pass the test above and break every
    /// asset key on the account.
    #[test]
    fn asset_is_still_creator_targeted() {
        assert_eq!(lookup("asset").target_type.as_deref(), Some("creator"));
    }
}
