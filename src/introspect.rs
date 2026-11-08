//! Reading the shape `introspect` answers with.
//!
//! Roblox does not echo back what it was sent. A scope goes out as
//! `{scopeType, targetParts, operations}` and comes back from
//! `/cloud-authentication/v1/apiKey/introspect` as
//! `{name, operations, universeIds}`: a different name for the type, a
//! different name for the target, and targets typed as universe ids rather
//! than the `U<id>` / `G<id>` / `*` strings a creator-targeted scope is sent
//! with.
//!
//! ## The target does not always arrive in `universeIds`
//!
//! It is not one field. The same key comes back carrying targets under at
//! least three names, and which one is used follows the scope:
//!
//! | Field | Seen on |
//! |---|---|
//! | `universeIds: [id]` | `universe`, `universe-places`, `universe-datastores.control` |
//! | `universeDatastores: [{universeId}]` | `universe-datastores.objects`, `.versions` |
//! | `groupIds: [id]` | `asset` |
//!
//! Reading only the first is what a bug report once described: a scope stored exactly
//! as asked was announced as missing on its universe *and* as unasked on `*`,
//! the two warnings mirroring each other, because a target this module could
//! not find became the `*` that means "no target at all".
//!
//! So a target field this build does not know is now an error rather than a
//! wildcard. That is the same rule the section below already applied to an
//! unreadable entry, and for the same reason: the failure this module exists
//! to prevent is a reader that quietly turns "not understood" into a
//! confident, wrong answer. `*` stays the reading of a scope that carries no
//! target field at all, which is how a badge or creator scope is sent.
//!
//! ## How this was found
//!
//! Not by a test. Every test in this crate feeds the parser something this
//! crate wrote, so a parser that only understands what we send passes all of
//! them. It was found by creating a key against real Open Cloud and reading
//! the answer, on 2026-08-16.
//!
//! What made it invisible for longer is that both readers failed *quietly*.
//! `diagnostics::introspect_scopes` collected with `filter_map(... .ok())`,
//! so entries that did not deserialise were dropped one by one until nothing
//! was left, and an empty scope list reads as "this key grants nothing"
//! rather than as "this was not understood". `create`'s post-create check
//! counted the raw JSON array instead of the parsed one, so it compared 5
//! against 5 and reported a match it had not verified.

use serde::Deserialize;

use crate::scope_builder::ScopeDef;

/// An id as introspect spells it, which is a string in every capture so far.
///
/// Accepting a number too costs one variant and removes a whole class of
/// false alarm: a numeric id is the same target written differently, not a
/// shape this module fails to understand, and refusing it would skip the
/// verification over punctuation.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Id {
    Str(String),
    Num(u64),
}

impl Id {
    fn as_target(&self) -> String {
        match self {
            Id::Str(s) => s.clone(),
            Id::Num(n) => n.to_string(),
        }
    }
}

/// One entry of `universeDatastores`.
///
/// Only `universeId` is known to live here. A key scoped to a **named** data
/// store is sent with two target parts (`[universe_id, name]`) and no capture
/// shows how that comes back, so any other field is reported rather than
/// dropped: a name silently discarded here would verify a narrow key against
/// a wide request and call it a match.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UniverseDatastoreTarget {
    universe_id: Id,
    #[serde(flatten)]
    rest: serde_json::Map<String, serde_json::Value>,
}

/// One scope as `introspect` returns it.
///
/// Deliberately its own type rather than serde aliases on [`ScopeDef`]: that
/// type is the **request** shape, it is serialised onto the wire, and giving
/// it aliases would let a future edit send `name` where Roblox expects
/// `scopeType`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IntrospectScope {
    name: String,
    #[serde(default)]
    operations: Vec<String>,
    /// Present on universe-targeted scopes. Absent on the ones targeting a
    /// creator or nothing at all, which is why it defaults rather than
    /// failing the whole entry.
    #[serde(default)]
    universe_ids: Vec<Id>,
    /// `universe-datastores.objects` and `.versions` arrive here instead.
    #[serde(default)]
    universe_datastores: Vec<UniverseDatastoreTarget>,
    /// A creator-targeted scope such as `asset`. Sent as `G<id>`, returned
    /// bare, so the prefix is put back rather than compared against a number
    /// that would never match.
    #[serde(default)]
    group_ids: Vec<Id>,
    /// The other half of a creator target, sent as `U<id>`. No capture shows
    /// it yet; handling it costs three lines and the guard below catches the
    /// spelling if it turns out to be another.
    #[serde(default)]
    user_ids: Vec<Id>,
    /// Everything else Roblox sent. Not decoration: it is what lets a target
    /// under a fourth name be reported instead of read as `*`.
    #[serde(flatten)]
    rest: serde_json::Map<String, serde_json::Value>,
}

impl IntrospectScope {
    /// The targets this entry names, in the spelling the request used.
    ///
    /// Empty means the entry carries no target field, which is the `*` a
    /// badge or creator scope is sent with. It does **not** mean the targets
    /// were unreadable: that is [`Self::unknown_target_fields`].
    fn targets(&self) -> Vec<String> {
        let mut out = Vec::new();
        out.extend(self.universe_ids.iter().map(Id::as_target));
        out.extend(
            self.universe_datastores
                .iter()
                .map(|d| d.universe_id.as_target()),
        );
        out.extend(self.group_ids.iter().map(|g| format!("G{}", g.as_target())));
        out.extend(self.user_ids.iter().map(|u| format!("U{}", u.as_target())));
        out
    }

    /// Field names that hold targets this build cannot read.
    ///
    /// A non-empty array under an unrecognised name is the signature of a
    /// fourth target shape. Scalars and empty arrays are left alone: those
    /// are metadata, and refusing to verify a key over an extra boolean would
    /// be the same overreach in the other direction.
    fn unknown_target_fields(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .rest
            .iter()
            .filter(|(_, v)| v.as_array().is_some_and(|a| !a.is_empty()))
            .map(|(k, _)| k.clone())
            .collect();
        for d in &self.universe_datastores {
            names.extend(d.rest.keys().map(|k| format!("universeDatastores[].{k}")));
        }
        names.sort();
        names.dedup();
        names
    }
}

/// Parse the `scopes` array of an introspect response into the request shape,
/// so a caller can compare what came back against what it sent.
///
/// Returns `None` when the document has no `scopes` array at all: the one
/// case that means "this is not an introspect response" rather than "this key
/// has no scopes". An entry that cannot be read is an error rather than a
/// silent drop: a key reported as granting nothing, because its scopes were
/// unreadable, is the failure this module exists to have stopped.
pub fn scopes_from_response(resp: &serde_json::Value) -> Option<Result<Vec<ScopeDef>, String>> {
    let arr = resp.get("scopes")?.as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for entry in arr {
        match serde_json::from_value::<IntrospectScope>(entry.clone()) {
            Ok(s) => {
                let unknown = s.unknown_target_fields();
                if !unknown.is_empty() {
                    return Some(Err(format!(
                        "scope \"{}\" carries targets under {}, which this build does not read",
                        s.name,
                        unknown.join(", ")
                    )));
                }
                let targets = s.targets();
                out.push(ScopeDef {
                    scope_type: s.name,
                    // `*` is what a scope with no target at all is sent as, and
                    // it is what keeps the comparison in `create` honest for the
                    // badge and creator scopes. Reaching it because a target was
                    // not understood is the mirrored-warnings bug, which is why every
                    // unreadable target field is refused above instead.
                    target_parts: if targets.is_empty() {
                        vec!["*".to_string()]
                    } else {
                        targets
                    },
                    operations: s.operations,
                });
            }
            Err(e) => return Some(Err(format!("unreadable scope entry: {e}"))),
        }
    }
    Some(Ok(out))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use serde_json::json;

    /// The shape Roblox actually returned on 2026-08-16, copied from a live
    /// response. This is the fixture the parser exists for, and the one no
    /// test had before, because every other fixture in this crate was written
    /// by this crate.
    fn live_response() -> serde_json::Value {
        json!({
            "authorizedUserId": 1234567890,
            "enabled": true,
            "name": "opsdev_shoptest",
            "scopes": [
                {
                    "name": "developer-product",
                    "operations": ["read", "write"],
                    "universeIds": ["99887766554"]
                },
                {
                    "name": "legacy-universe.badge",
                    "operations": ["write", "manage-and-spend-robux"]
                }
            ]
        })
    }

    #[test]
    fn the_live_shape_parses_into_the_request_shape() {
        let scopes = scopes_from_response(&live_response()).unwrap().unwrap();
        assert_eq!(scopes.len(), 2);

        assert_eq!(scopes[0].scope_type, "developer-product");
        assert_eq!(scopes[0].target_parts, vec!["99887766554"]);
        assert_eq!(scopes[0].operations, vec!["read", "write"]);
    }

    /// A scope with no universe list is not a scope with no target: it is the
    /// `*` a badge or creator scope is sent with, and comparing it as an empty
    /// list would report every one of them as missing.
    #[test]
    fn a_scope_without_universes_targets_everything() {
        let scopes = scopes_from_response(&live_response()).unwrap().unwrap();
        assert_eq!(scopes[1].scope_type, "legacy-universe.badge");
        assert_eq!(scopes[1].target_parts, vec!["*"]);
    }

    /// The request shape must **not** parse as the response shape. If it did,
    /// this parser would accept what we send and the mismatch would be
    /// invisible again.
    #[test]
    fn the_request_shape_is_not_silently_accepted() {
        let sent = json!({
            "scopes": [{
                "scopeType": "universe",
                "targetParts": ["1"],
                "operations": ["read"]
            }]
        });
        assert!(
            scopes_from_response(&sent).unwrap().is_err(),
            "a document in the request shape must be reported, not read as empty"
        );
    }

    /// No `scopes` key at all means this is not an introspect response, which
    /// is a different thing from a key that grants nothing.
    #[test]
    fn a_document_without_scopes_is_not_an_empty_key() {
        assert!(scopes_from_response(&json!({"enabled": true})).is_none());
    }

    /// The response from that bug report, with the counter-example the same key
    /// carried: `control` came back under `universeIds` and was read
    /// correctly, `objects` came back under `universeDatastores` and was not.
    fn response_of_issue_37() -> serde_json::Value {
        json!({
            "scopes": [
                {
                    "name": "universe-datastores.control",
                    "operations": ["list"],
                    "universeIds": ["109876543210987"]
                },
                {
                    "name": "universe-datastores.objects",
                    "operations": ["read"],
                    "universeDatastores": [{ "universeId": "109876543210987" }]
                },
                {
                    "name": "asset",
                    "operations": ["read"],
                    "groupIds": ["1234567890"]
                }
            ]
        })
    }

    /// The bug itself. A target under `universeDatastores` used to read as
    /// `*`, which `create` then reported twice: missing on the universe, and
    /// stored-but-unasked on `*`.
    #[test]
    fn a_datastore_target_is_the_universe_it_names_rather_than_a_wildcard() {
        let scopes = scopes_from_response(&response_of_issue_37())
            .unwrap()
            .unwrap();
        let objects = &scopes[1];
        assert_eq!(objects.scope_type, "universe-datastores.objects");
        assert_eq!(objects.target_parts, vec!["109876543210987"]);
        assert!(
            !objects.target_parts.contains(&"*".to_string()),
            "a target that was found must not read as the absence of one"
        );
    }

    /// Both spellings of the same target must produce the same triple, since
    /// that is what makes the two entries comparable against one request.
    #[test]
    fn the_two_datastore_scopes_of_one_key_agree_on_their_universe() {
        let scopes = scopes_from_response(&response_of_issue_37())
            .unwrap()
            .unwrap();
        assert_eq!(scopes[0].target_parts, scopes[1].target_parts);
    }

    /// A creator target is sent as `G<id>` and returned bare. Comparing the
    /// two spellings directly would report every `asset` scope as drift.
    #[test]
    fn a_group_target_is_restored_to_the_spelling_it_was_sent_with() {
        let scopes = scopes_from_response(&response_of_issue_37())
            .unwrap()
            .unwrap();
        assert_eq!(scopes[2].scope_type, "asset");
        assert_eq!(scopes[2].target_parts, vec!["G1234567890"]);
    }

    #[test]
    fn a_user_target_is_restored_the_same_way() {
        let doc = json!({
            "scopes": [{ "name": "asset", "operations": ["read"], "userIds": ["1234567890"] }]
        });
        let scopes = scopes_from_response(&doc).unwrap().unwrap();
        assert_eq!(scopes[0].target_parts, vec!["U1234567890"]);
    }

    /// An id written as a number is the same target, not a shape this module
    /// fails to understand. Refusing it would skip the verification over
    /// punctuation.
    #[test]
    fn an_id_may_arrive_as_a_number() {
        let doc = json!({
            "scopes": [{ "name": "universe", "operations": ["read"], "universeIds": [123] }]
        });
        let scopes = scopes_from_response(&doc).unwrap().unwrap();
        assert_eq!(scopes[0].target_parts, vec!["123"]);
    }

    /// The guard that stops that bug from recurring under a fourth field name. A
    /// target this build cannot read must be reported, never read as `*`.
    #[test]
    fn a_target_under_an_unknown_field_is_reported_rather_than_wildcarded() {
        let doc = json!({
            "scopes": [{
                "name": "some-future-scope",
                "operations": ["read"],
                "organisationIds": ["1234567890"]
            }]
        });
        let err = scopes_from_response(&doc).unwrap().unwrap_err();
        assert!(err.contains("organisationIds"), "{err}");
        assert!(err.contains("some-future-scope"), "{err}");
    }

    /// A key scoped to a named data store is sent with two target parts and
    /// no capture shows how that returns. Dropping the name would verify a
    /// narrow key against a wide request and call it a match.
    #[test]
    fn a_named_datastore_inside_the_target_is_reported_rather_than_dropped() {
        let doc = json!({
            "scopes": [{
                "name": "universe-datastores.objects",
                "operations": ["read"],
                "universeDatastores": [
                    { "universeId": "109876543210987", "datastoreName": "PlayerInventory" }
                ]
            }]
        });
        let err = scopes_from_response(&doc).unwrap().unwrap_err();
        assert!(err.contains("universeDatastores[].datastoreName"), "{err}");
    }

    /// Metadata is not a target. Refusing to verify a key over an extra
    /// boolean would be the same overreach as reading one as `*`.
    #[test]
    fn a_scalar_field_this_build_does_not_know_is_not_a_target() {
        let doc = json!({
            "scopes": [{
                "name": "universe",
                "operations": ["read"],
                "universeIds": ["109876543210987"],
                "inherited": false,
                "revokedIds": []
            }]
        });
        let scopes = scopes_from_response(&doc).unwrap().unwrap();
        assert_eq!(scopes[0].target_parts, vec!["109876543210987"]);
    }
}
