//! `rbx-apikey catalog regenerate [url]`: refresh the embedded scope catalog
//! from Roblox's live `openapi.json`, reconcile it against the key service's
//! own scope list, and write `src/data/catalog.json`.
//!
//! Two documents, because they answer different questions. The spec says which
//! scopes exist and what they are for; the key service says what a creation
//! request may send for them, and it is the one that accepts or rejects. They
//! disagreed about fourteen scopes, so the service wins on every `target_type`
//! and on the operation lists. See `SCOPES_URL`.
//!
//! The catalog is embedded at compile time (include_str!), so a rebuild is required for
//! the change to take effect: the command prints the next steps after writing.
//!
//! That makes this a maintainer command: it only means anything from a checkout, and it
//! refuses to run anywhere else rather than writing a file that can never be applied.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{bail, Result};
use chrono::Utc;
use colored::Colorize;
use serde_json::Value;

use crate::scope_catalog::{Catalog, ScopeInfo};

const DEFAULT_OPENAPI_URL: &str =
    "https://raw.githubusercontent.com/Roblox/creator-docs/main/content/en-us/reference/cloud/openapi.json";
const OUTPUT_FILE: &str = "src/data/catalog.json";

/// The key service's own scope list, and the reason this command fetches two
/// documents instead of one.
///
/// `openapi.json` describes *endpoints*. A scope's target there is inferred
/// from the routes that mention it, and for fourteen scopes that inference is
/// wrong: the group routes are annotated as creator resources, so the catalog
/// recorded `group` as creator-targeted. This endpoint is what actually
/// validates a key creation, and it says `group` takes no target at all.
///
/// Sending a target part for such a scope does not come back as a rejection.
/// The service throws, and `apikey create` reports
/// `500 Internal Server Error: Exception was thrown by handler`, which says
/// nothing about which scope caused it.
///
/// Public, no credentials: it is the list the Creator Hub renders.
const SCOPES_URL: &str = "https://apis.roblox.com/cloud-authentication/v1/scopes";

pub async fn regenerate(url: Option<&str>) -> Result<()> {
    let url = url.unwrap_or(DEFAULT_OPENAPI_URL);

    // Before the download, not after: the answer does not depend on the spec,
    // and a refusal should not cost a network round-trip.
    ensure_writable_from_here(Path::new(OUTPUT_FILE))?;

    println!("{}", "Regenerating scope catalog".cyan());
    println!();

    let body = fetch_openapi(url).await?;
    println!("{}", "✓ Downloaded".green());

    let mut scopes = parse_scopes(&body)?;
    if scopes.is_empty() {
        println!();
        bail!("No scopes found in JSON");
    }
    println!("{}", format!("✓ Parsed {} scopes", scopes.len()).green());

    // Authority, not a cross-check: where the two documents disagree the
    // service wins, because the service is what rejects or accepts a creation.
    // Fatal when unreachable rather than falling back to the spec alone: the
    // catalog that would be written is the one that makes `create` fail with
    // an opaque 500.
    let service = fetch_service_scopes(SCOPES_URL)
        .await
        .map_err(|e| anyhow::anyhow!("{e}\n\nCould not reach {SCOPES_URL}, which is the only document that says what a scope's target really is. Refusing to write a catalog from the endpoint spec alone."))?;
    println!(
        "{}",
        format!("✓ Fetched {} scopes from the key service", service.len()).green()
    );

    let corrections = reconcile(&mut scopes, &service);
    if corrections.is_empty() {
        println!("{}", "✓ The two documents agree".green());
    } else {
        println!(
            "{}",
            format!(
                "✓ Corrected {} entries from the key service:",
                corrections.len()
            )
            .green()
        );
        for line in &corrections {
            println!("    {line}");
        }
    }

    // Descriptions only ever came from `components.securitySchemes`, which the
    // current spec no longer publishes. Without this, every regenerate would
    // quietly strip the descriptions the catalog already has.
    let carried = carry_over_descriptions(Path::new(OUTPUT_FILE), &mut scopes);
    if carried > 0 {
        println!(
            "{}",
            format!("✓ Kept {carried} descriptions from the previous catalog").green()
        );
    }

    let cat = Catalog {
        version: Utc::now().format("%Y-%m-%d").to_string(),
        source_url: provenance_for(url),
        authority_url: Some(SCOPES_URL.to_string()),
        scopes,
    };
    let json = serde_json::to_string_pretty(&cat)?;

    std::fs::write(OUTPUT_FILE, json)?;
    println!("{}", format!("✓ Wrote {}", OUTPUT_FILE).green());
    println!();
    println!("Next steps:");
    println!("  1. Review the generated file");
    println!("  2. Run `cargo build --release` to recompile the binary");
    Ok(())
}

/// Refuse unless the output directory is already there.
///
/// `OUTPUT_FILE` is relative to the working directory, so it only names
/// anything from a checkout of this repository. Creating the parents instead
/// (what this used to do) turned running the command from anywhere else into a
/// junk `./src/data/catalog.json` and a success message, which is the one
/// outcome nobody wants: the catalog is embedded with `include_str!`, so a
/// written file changes nothing until something rebuilds the binary.
fn ensure_writable_from_here(path: &Path) -> Result<()> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    match dir {
        Some(dir) if !dir.is_dir() => bail!(
            "no {} directory here, so this is not a checkout of rbx-forge/rbx-apikey.\n\
             \n\
             `catalog regenerate` rewrites the catalog in the source tree, and the catalog is \
             compiled into the binary, so a rebuild is what applies it. Either:\n  \
             - Run this from the root of a checkout of https://github.com/rbx-forge/rbx-apikey, \
             then `cargo build --release`\n  \
             - Or, to pick up scopes Roblox has added, upgrade to a release that ships them: \
             an installed binary cannot refresh its own catalog",
            dir.display()
        ),
        _ => Ok(()),
    }
}

/// What to record as the catalog's `source_url`.
///
/// A URL is its own provenance. A local path is not: regenerating from a
/// downloaded copy of the spec (the deterministic way to do it, and what the
/// scheduled workflow does) would otherwise stamp an absolute path from one
/// machine into a committed file, which tells the next reader nothing about
/// where the scopes came from.
///
/// So a copy can carry its provenance in a `source.json` beside it, naming the
/// upstream repository, commit and document. When one is there, record the
/// permalink it describes; the bytes parsed are a snapshot of exactly that
/// revision, so the permalink is the truthful answer, not an approximation of
/// one.
fn provenance_for(url: &str) -> String {
    let Some(path_str) = url.strip_prefix("file://") else {
        return url.to_string();
    };
    match permalink_beside(Path::new(path_str)) {
        Some(permalink) => permalink,
        None => {
            // Not fatal: the catalog is still correct, only its provenance line
            // is weak. Say so rather than committing the path silently.
            println!(
                "{}",
                format!(
                    "! no source.json beside {path_str}; recording the local path as the source"
                )
                .yellow()
            );
            url.to_string()
        }
    }
}

/// Build the upstream permalink from the `source.json` sitting next to a
/// vendored spec. `None` if it is missing, unreadable, or lacks a field.
fn permalink_beside(spec_path: &Path) -> Option<String> {
    let provenance = std::fs::read_to_string(spec_path.parent()?.join("source.json")).ok()?;
    let v: Value = serde_json::from_str(&provenance).ok()?;
    let repository = v.get("repository")?.as_str()?;
    let commit = v.get("commit")?.as_str()?;
    let document = v.get("document")?.as_str()?;
    Some(format!(
        "https://github.com/{repository}/blob/{commit}/{document}"
    ))
}

async fn fetch_openapi(url: &str) -> Result<String> {
    println!("Fetching {}...", url);

    if let Some(path_str) = url.strip_prefix("file://") {
        return std::fs::read_to_string(path_str)
            .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path_str, e));
    }

    let client = reqwest::Client::new();
    let resp = client.get(url).send().await?;
    let status = resp.status();
    if !status.is_success() {
        bail!("HTTP {}", status);
    }
    Ok(resp.text().await?)
}

fn parse_scopes(json_body: &str) -> Result<BTreeMap<String, ScopeInfo>> {
    let v: Value = serde_json::from_str(json_body)
        .map_err(|e| anyhow::anyhow!("Failed to parse JSON: {}", e))?;

    let mut scopes: BTreeMap<String, ScopeInfo> = BTreeMap::new();
    // Specifiers seen per scope type, collected across every path that
    // references it and applied once at the end. A scope type appears on many
    // operations and Roblox does not fill the field in consistently
    // (`universe` carries `universes` on some paths and nothing on others) so
    // deciding on first sight would make the catalog depend on map iteration
    // order. Empty strings are dropped at insert: the field is present but
    // says nothing, which is not the same as naming a target.
    let mut specifiers: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    // 1. Extract from paths[*].<method>["x-roblox-scopes"]
    if let Some(paths) = v.get("paths").and_then(|p| p.as_object()) {
        for (_, path_item) in paths {
            if let Some(path_obj) = path_item.as_object() {
                for (_, op) in path_obj {
                    if let Some(roblox_scopes) =
                        op.get("x-roblox-scopes").and_then(|s| s.as_array())
                    {
                        for scope_ref in roblox_scopes {
                            let Some(raw) = scope_name(scope_ref) else {
                                continue;
                            };
                            let (scope_type, operations) = split_scope(raw);
                            if let Some(spec) = target_specifier(scope_ref) {
                                specifiers
                                    .entry(scope_type.to_string())
                                    .or_default()
                                    .insert(spec.to_string());
                            }
                            let entry =
                                scopes
                                    .entry(scope_type.to_string())
                                    .or_insert_with(|| ScopeInfo {
                                        operations: Vec::new(),
                                        target_type: String::new(),
                                        description: None,
                                    });
                            // The same scope type appears on many operations
                            // (`universe:read` on a few dozen paths), each
                            // contributing one operation. Union, don't replace.
                            for op in operations {
                                if !entry.operations.iter().any(|known| known == op) {
                                    entry.operations.push(op.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // 2. components.securitySchemes.<name>.scopes
    //
    // Dead against the current spec: the three schemes Roblox publishes now
    // (roblox-api-key, roblox-legacy-cookie, roblox-oauth2) carry no `scopes`
    // key at all. Kept because it costs nothing and is the only source that
    // ever supplied descriptions, so an older or restored spec still works.
    if let Some(schemes) = v
        .pointer("/components/securitySchemes")
        .and_then(|s| s.as_object())
    {
        for (_, scheme) in schemes {
            if let Some(scope_map) = scheme.get("scopes").and_then(|s| s.as_object()) {
                for (scope_name, scope_desc) in scope_map {
                    scopes
                        .entry(scope_name.clone())
                        .or_insert_with(|| ScopeInfo {
                            operations: Vec::new(),
                            target_type: String::new(),
                            description: scope_desc.as_str().map(|s| s.to_string()),
                        });
                }
            }
        }
    }

    // Stable output: the file is committed, so unordered operations would
    // produce a diff on every regenerate even when nothing changed. Done last
    // so anything added by source 2 is sorted too.
    for (name, info) in scopes.iter_mut() {
        info.operations.sort();
        info.target_type = resolve_target_type(name, specifiers.get(name));
    }

    Ok(scopes)
}

/// Copy descriptions from the catalog already on disk onto the freshly parsed
/// scopes. Returns how many were carried over. A missing or unreadable file is
/// not an error: the first ever run has nothing to carry.
/// One scope type as the key service describes it.
#[derive(Debug)]
struct ServiceScope {
    operations: BTreeSet<String>,
    target_type: String,
}

/// Read the key service's scope list.
///
/// Its rows are one per `(scopeType, operation)` pair, so they get folded into
/// one entry per scope type. `targetType` arrives as `""` for a scope that
/// takes no target, which this maps to `"none"`: the spelling the catalog
/// already uses and that `emit_for_scope` turns into `targetParts: ["*"]`.
async fn fetch_service_scopes(url: &str) -> Result<BTreeMap<String, ServiceScope>> {
    let body = fetch_openapi(url).await?;
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| anyhow::anyhow!("Failed to parse the scope list: {e}"))?;

    let rows = v
        .get("scopes")
        .and_then(|s| s.as_array())
        .ok_or_else(|| anyhow::anyhow!("No `scopes` array in the response"))?;

    let mut out: BTreeMap<String, ServiceScope> = BTreeMap::new();
    for row in rows {
        let Some(scope_type) = row.get("scopeType").and_then(|s| s.as_str()) else {
            continue;
        };
        let target = match row.get("targetType").and_then(|t| t.as_str()) {
            Some("") | None => "none",
            Some(other) => other,
        };
        let entry = out
            .entry(scope_type.to_string())
            .or_insert_with(|| ServiceScope {
                operations: BTreeSet::new(),
                target_type: target.to_string(),
            });
        if entry.target_type != target {
            bail!(
                "The key service reports two different targets for `{scope_type}`: \
                 `{}` and `{target}`. Refusing to guess which one a request should use.",
                entry.target_type
            );
        }
        if let Some(op) = row.get("operation").and_then(|o| o.as_str()) {
            entry.operations.insert(op.to_string());
        }
    }

    if out.is_empty() {
        bail!("The key service returned no scopes");
    }
    Ok(out)
}

/// Apply the service's targets and operations over what the spec inferred, and
/// add any scope the spec never mentioned.
///
/// Returns one line per correction, for the maintainer to read before
/// committing. A silent overwrite here would hide the next divergence as
/// effectively as the bug this fixes.
fn reconcile(
    scopes: &mut BTreeMap<String, ScopeInfo>,
    service: &BTreeMap<String, ServiceScope>,
) -> Vec<String> {
    let mut corrections = Vec::new();

    for (name, svc) in service {
        let svc_ops: Vec<String> = svc.operations.iter().cloned().collect();

        let Some(info) = scopes.get_mut(name) else {
            corrections.push(format!(
                "{name}: absent from the spec, added as {} [{}]",
                svc.target_type,
                svc_ops.join(",")
            ));
            scopes.insert(
                name.clone(),
                ScopeInfo {
                    operations: svc_ops,
                    target_type: svc.target_type.clone(),
                    description: None,
                },
            );
            continue;
        };

        if info.target_type != svc.target_type {
            corrections.push(format!(
                "{name}: target {} -> {}",
                if info.target_type.is_empty() {
                    "(unset)"
                } else {
                    &info.target_type
                },
                svc.target_type
            ));
            info.target_type = svc.target_type.clone();
        }

        let mut spec_ops = info.operations.clone();
        spec_ops.sort();
        if spec_ops != svc_ops {
            corrections.push(format!(
                "{name}: operations [{}] -> [{}]",
                spec_ops.join(","),
                svc_ops.join(",")
            ));
            info.operations = svc_ops;
        }
    }

    // A scope the spec knows and the service does not is left alone rather
    // than dropped: it cannot be requested anyway, and removing it would turn
    // a clear "unknown scope" warning into a silent disappearance.
    corrections
}

fn carry_over_descriptions(path: &Path, scopes: &mut BTreeMap<String, ScopeInfo>) -> usize {
    let Ok(existing) = std::fs::read_to_string(path) else {
        return 0;
    };
    let Ok(previous) = serde_json::from_str::<Catalog>(&existing) else {
        return 0;
    };

    let mut carried = 0;
    for (name, info) in scopes.iter_mut() {
        if info.description.is_some() {
            continue;
        }
        if let Some(description) = previous
            .scopes
            .get(name)
            .and_then(|p| p.description.clone())
        {
            info.description = Some(description);
            carried += 1;
        }
    }
    carried
}

/// Read one `x-roblox-scopes` entry.
///
/// Roblox changed this from a bare string to
/// `{"name": "universe:read", "targetResourceSpecifier": "universes"}`. The
/// old `as_str()` call silently returned `None` for every entry after that,
/// which is why `regenerate` reported "No scopes found" and the catalog stayed
/// frozen at its last working run. Both shapes are accepted so a restored or
/// vendored older spec still parses.
fn scope_name(scope_ref: &Value) -> Option<&str> {
    scope_ref
        .as_str()
        .or_else(|| scope_ref.get("name").and_then(|n| n.as_str()))
}

/// Read one entry's `targetResourceSpecifier`, if it names anything.
///
/// The bare-string form has no specifier at all. The object form often carries
/// `""`, which is treated as absent: the key exists but names no resource, so
/// it carries no more information than omitting it would.
fn target_specifier(scope_ref: &Value) -> Option<&str> {
    scope_ref
        .get("targetResourceSpecifier")
        .and_then(|t| t.as_str())
        .filter(|t| !t.is_empty())
}

/// Split `universe-datastores.objects:create,delete` into its type and its
/// operations. A name with no `:` is a type with no operations.
fn split_scope(raw: &str) -> (&str, Vec<&str>) {
    match raw.split_once(':') {
        Some((scope_type, operations)) => (
            scope_type,
            operations
                .split(',')
                .map(str::trim)
                .filter(|op| !op.is_empty())
                .collect(),
        ),
        None => (raw, Vec::new()),
    }
}

/// Decide a scope's target type, preferring what the spec states over what the
/// name suggests.
///
/// Roblox publishes `targetResourceSpecifier` next to each scope name, but
/// fills it in for four scope families out of forty-odd and writes `""` for
/// several more. So it is an override where it speaks, not a source: the name
/// heuristic still decides everything else.
///
/// Where it does speak it has been wrong-footing us: `developer-product` and
/// `game-pass` both say `universes` while the heuristic called them `none`,
/// which builds a key targeting `*` (every universe the owner has) instead of
/// the ones the config names.
fn resolve_target_type(scope_type: &str, spec_specifiers: Option<&BTreeSet<String>>) -> String {
    let from_spec = spec_specifiers
        .into_iter()
        .flatten()
        .find_map(|s| map_specifier(s));
    from_spec.unwrap_or_else(|| infer_target_type(scope_type))
}

/// Translate a spec `targetResourceSpecifier` into our target vocabulary.
///
/// `universes` is the only value Roblox currently emits. Anything else is
/// returned as `None` so an unrecognised value falls back to the heuristic
/// rather than being written into the catalog untranslated: a target type
/// `scope_builder` does not know produces a malformed key request, and that
/// fails at key creation rather than at compile time.
fn map_specifier(specifier: &str) -> Option<String> {
    match specifier {
        "universes" => Some("universe".into()),
        _ => None,
    }
}

fn infer_target_type(scope_type: &str) -> String {
    if scope_type.starts_with("universe-datastore") {
        "universe-datastore".into()
    } else if scope_type.starts_with("universe") {
        "universe".into()
    } else if scope_type.starts_with("memory-store") {
        // The spec says nothing about these, and the fall-through below would
        // call them `creator`: a key spanning every universe the owner has.
        // The Creator Hub's own key editor offers "Restrict by Experience" for
        // Memory Stores, which it would not if the grant were creator-wide, and
        // every memory-store path is rooted at
        // `/cloud/v2/universes/{universe_id}/memory-store/...`. Both point at a
        // universe target, so that is what we build.
        "universe".into()
    } else if scope_type.starts_with("developer-product") || scope_type.starts_with("game-pass") {
        // Called `none` until the spec was found to say `universes` for both.
        // `resolve_target_type` already prefers the spec, so this branch only
        // runs if Roblox stops publishing the field; it agrees with the spec so
        // that the answer does not silently revert if they do.
        "universe".into()
    } else if scope_type.starts_with("legacy") {
        "none".into()
    } else {
        "creator".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rows as the service publishes them, including its empty-string target,
    /// mapped the way `fetch_service_scopes` maps it. Tests spell `""` rather
    /// than `"none"` so they keep describing the wire shape.
    fn service(rows: &[(&str, &str, &str)]) -> BTreeMap<String, ServiceScope> {
        let mut out: BTreeMap<String, ServiceScope> = BTreeMap::new();
        for (scope, op, target) in rows {
            let mapped = if target.is_empty() { "none" } else { target };
            let e = out
                .entry((*scope).to_string())
                .or_insert_with(|| ServiceScope {
                    operations: BTreeSet::new(),
                    target_type: mapped.to_string(),
                });
            e.operations.insert((*op).to_string());
        }
        out
    }

    fn spec(rows: &[(&str, &[&str], &str)]) -> BTreeMap<String, ScopeInfo> {
        rows.iter()
            .map(|(name, ops, target)| {
                (
                    (*name).to_string(),
                    ScopeInfo {
                        operations: ops.iter().map(|o| (*o).to_string()).collect(),
                        target_type: (*target).to_string(),
                        description: None,
                    },
                )
            })
            .collect()
    }

    /// The bug this reconciliation exists for. The endpoint spec annotates the
    /// group routes as creator resources, so `group` came out creator-targeted
    /// and `apikey create` sent `targetParts: ["G<id>"]` for a scope that takes
    /// none. The service answered 500 rather than rejecting it.
    #[test]
    fn a_scope_the_service_says_takes_no_target_is_corrected() {
        let mut scopes = spec(&[("group", &["read", "write"], "creator")]);
        let corrections = reconcile(
            &mut scopes,
            &service(&[("group", "read", ""), ("group", "write", "")]),
        );

        assert_eq!(scopes["group"].target_type, "none");
        assert_eq!(corrections, vec!["group: target creator -> none"]);
    }

    /// The one that must not move. `asset` is genuinely creator-targeted, and
    /// a blanket "group is creator, so creator is wrong" change would break
    /// every asset key on the account.
    #[test]
    fn a_scope_the_two_documents_agree_on_is_left_alone() {
        let mut scopes = spec(&[("asset", &["read", "write"], "creator")]);
        let corrections = reconcile(
            &mut scopes,
            &service(&[("asset", "read", "creator"), ("asset", "write", "creator")]),
        );

        assert_eq!(scopes["asset"].target_type, "creator");
        assert!(corrections.is_empty(), "{corrections:?}");
    }

    /// The spec under-reports operations too: it lists `group-forum` as
    /// read-only because no published route writes to it yet.
    #[test]
    fn operations_the_service_lists_are_added() {
        let mut scopes = spec(&[("group-forum", &["read"], "none")]);
        let corrections = reconcile(
            &mut scopes,
            &service(&[("group-forum", "read", ""), ("group-forum", "write", "")]),
        );

        assert_eq!(scopes["group-forum"].operations, vec!["read", "write"]);
        assert_eq!(
            corrections,
            vec!["group-forum: operations [read] -> [read,write]"]
        );
    }

    #[test]
    fn a_scope_missing_from_the_spec_is_added_from_the_service() {
        let mut scopes = spec(&[]);
        let corrections = reconcile(&mut scopes, &service(&[("legacy-asset", "manage", "")]));

        assert_eq!(scopes["legacy-asset"].target_type, "none");
        assert_eq!(scopes["legacy-asset"].operations, vec!["manage"]);
        assert_eq!(
            corrections,
            vec!["legacy-asset: absent from the spec, added as none [manage]"]
        );
    }

    /// Kept rather than dropped: it cannot be requested anyway, and removing
    /// it turns a clear "unknown scope" warning into a silent disappearance.
    #[test]
    fn a_scope_the_service_does_not_know_survives() {
        let mut scopes = spec(&[("retired-scope", &["read"], "universe")]);
        reconcile(&mut scopes, &service(&[("asset", "read", "creator")]));

        assert!(scopes.contains_key("retired-scope"));
    }

    /// `fetch_service_scopes` over HTTP, because the mapping that matters
    /// lives in it and not in `reconcile`.
    ///
    /// The `service()` helper above rebuilds `""` -> `"none"` by hand, so it
    /// cannot catch that mapping changing. Exercised against a mock server
    /// the same way the client's own request shaping is.
    mod from_the_service {
        use super::super::fetch_service_scopes;
        use serde_json::json;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        async fn serving(body: serde_json::Value) -> MockServer {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/scopes"))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(&server)
                .await;
            server
        }

        /// One row per `(scopeType, operation)` pair, which is how the service
        /// publishes them, folded into one entry per scope type.
        #[tokio::test]
        async fn rows_fold_per_scope_and_an_empty_target_becomes_none() {
            let server = serving(json!({"scopes": [
                {"scopeType": "group", "operation": "read", "targetType": ""},
                {"scopeType": "group", "operation": "write", "targetType": ""},
                {"scopeType": "asset", "operation": "read", "targetType": "creator"}
            ]}))
            .await;

            let out = fetch_service_scopes(&format!("{}/scopes", server.uri()))
                .await
                .unwrap();

            assert_eq!(out["group"].target_type, "none");
            assert_eq!(
                out["group"].operations.iter().cloned().collect::<Vec<_>>(),
                vec!["read", "write"]
            );
            assert_eq!(out["asset"].target_type, "creator");
        }

        /// A missing `targetType` reads as no target rather than as an empty
        /// string that later compares unequal to everything.
        #[tokio::test]
        async fn an_absent_target_also_becomes_none() {
            let server = serving(json!({"scopes": [
                {"scopeType": "legacy-asset", "operation": "manage"}
            ]}))
            .await;

            let out = fetch_service_scopes(&format!("{}/scopes", server.uri()))
                .await
                .unwrap();

            assert_eq!(out["legacy-asset"].target_type, "none");
        }

        /// Two targets for one scope type is not something to average out: a
        /// request can only send one, and picking would be a guess.
        #[tokio::test]
        async fn disagreeing_targets_for_one_scope_are_refused() {
            let server = serving(json!({"scopes": [
                {"scopeType": "group", "operation": "read", "targetType": ""},
                {"scopeType": "group", "operation": "write", "targetType": "creator"}
            ]}))
            .await;

            let err = fetch_service_scopes(&format!("{}/scopes", server.uri()))
                .await
                .unwrap_err()
                .to_string();

            assert!(err.contains("two different targets"), "{err}");
            assert!(err.contains("group"), "{err}");
        }

        /// An empty list means the document changed shape. Writing a catalog
        /// from it would strip every target in one go.
        #[tokio::test]
        async fn an_empty_list_is_refused() {
            let server = serving(json!({"scopes": []})).await;

            assert!(fetch_service_scopes(&format!("{}/scopes", server.uri()))
                .await
                .is_err());
        }
    }

    /// The shape Roblox publishes today. A regression here means `regenerate`
    /// silently produces an empty catalog again.
    const OBJECT_FORM: &str = r#"{
      "paths": {
        "/a": { "get": { "x-roblox-scopes": [
          { "name": "universe:read", "targetResourceSpecifier": "universes" }
        ] } },
        "/b": { "post": { "x-roblox-scopes": [
          { "name": "universe:write" },
          { "name": "universe.user-restriction:read" }
        ] } }
      }
    }"#;

    /// The shape Roblox used to publish, kept working on purpose.
    const STRING_FORM: &str = r#"{
      "paths": { "/a": { "get": { "x-roblox-scopes": ["universe:read"] } } }
    }"#;

    #[test]
    fn object_form_entries_are_parsed() {
        let scopes = parse_scopes(OBJECT_FORM).unwrap();
        assert_eq!(scopes["universe"].operations, vec!["read", "write"]);
        assert_eq!(scopes["universe.user-restriction"].operations, vec!["read"]);
    }

    #[test]
    fn string_form_entries_still_parse() {
        let scopes = parse_scopes(STRING_FORM).unwrap();
        assert_eq!(scopes["universe"].operations, vec!["read"]);
    }

    #[test]
    fn operations_from_different_paths_are_unioned_not_replaced() {
        let scopes = parse_scopes(OBJECT_FORM).unwrap();
        // `universe` is referenced by two paths, one read and one write.
        assert_eq!(scopes["universe"].operations.len(), 2);
    }

    #[test]
    fn operations_are_sorted_so_the_committed_file_does_not_churn() {
        let json = r#"{"paths":{"/a":{"get":{"x-roblox-scopes":[
            {"name":"universe-datastores.objects:update,create,list"}
        ]}}}}"#;
        let scopes = parse_scopes(json).unwrap();
        assert_eq!(
            scopes["universe-datastores.objects"].operations,
            vec!["create", "list", "update"]
        );
    }

    #[test]
    fn a_scope_without_operations_still_yields_its_type() {
        let json = r#"{"paths":{"/a":{"get":{"x-roblox-scopes":[{"name":"asset"}]}}}}"#;
        let scopes = parse_scopes(json).unwrap();
        assert!(scopes["asset"].operations.is_empty());
    }

    #[test]
    fn an_entry_that_is_neither_string_nor_named_object_is_skipped() {
        let json = r#"{"paths":{"/a":{"get":{"x-roblox-scopes":[
            {"noName":"x"}, {"name":"universe:read"}
        ]}}}}"#;
        let scopes = parse_scopes(json).unwrap();
        assert_eq!(scopes.len(), 1);
    }

    #[test]
    fn the_spec_specifier_overrides_the_name_heuristic() {
        // The heuristic alone calls `game-pass` a `none` target, which builds a
        // key over `*`. The spec says `universes`.
        let json = r#"{"paths":{"/a":{"get":{"x-roblox-scopes":[
            {"name":"game-pass:read","targetResourceSpecifier":"universes"}
        ]}}}}"#;
        let scopes = parse_scopes(json).unwrap();
        assert_eq!(scopes["game-pass"].target_type, "universe");
    }

    #[test]
    fn an_empty_specifier_is_not_a_target_and_leaves_the_heuristic_alone() {
        let json = r#"{"paths":{"/a":{"get":{"x-roblox-scopes":[
            {"name":"ad.campaign:read","targetResourceSpecifier":""}
        ]}}}}"#;
        let scopes = parse_scopes(json).unwrap();
        assert_eq!(scopes["ad.campaign"].target_type, "creator");
    }

    #[test]
    fn an_unrecognised_specifier_falls_back_rather_than_being_written_through() {
        let json = r#"{"paths":{"/a":{"get":{"x-roblox-scopes":[
            {"name":"asset:read","targetResourceSpecifier":"somethingNew"}
        ]}}}}"#;
        let scopes = parse_scopes(json).unwrap();
        assert_eq!(scopes["asset"].target_type, "creator");
    }

    /// Roblox fills the field in on some paths and not others for the same
    /// scope. Whichever path is visited first must not decide the answer.
    #[test]
    fn a_specifier_on_any_path_settles_the_scope_regardless_of_path_order() {
        let json = r#"{"paths":{
            "/a":{"get":{"x-roblox-scopes":[{"name":"game-pass:read"}]}},
            "/b":{"get":{"x-roblox-scopes":[
                {"name":"game-pass:write","targetResourceSpecifier":"universes"}
            ]}}
        }}"#;
        let scopes = parse_scopes(json).unwrap();
        assert_eq!(scopes["game-pass"].target_type, "universe");
    }

    /// The spec is silent on all three memory-store scopes, so the fall-through
    /// used to make them `creator`: one key over every universe the owner has.
    #[test]
    fn memory_store_scopes_target_a_universe_not_the_creator() {
        let json = r#"{"paths":{"/a":{"get":{"x-roblox-scopes":[
            {"name":"memory-store.sorted-map:read"},
            {"name":"memory-store.queue:add"},
            {"name":"memory-store:flush"}
        ]}}}}"#;
        let scopes = parse_scopes(json).unwrap();
        assert_eq!(scopes["memory-store.sorted-map"].target_type, "universe");
        assert_eq!(scopes["memory-store.queue"].target_type, "universe");
        assert_eq!(scopes["memory-store"].target_type, "universe");
    }

    #[test]
    fn datastore_scopes_keep_their_two_part_target() {
        let json = r#"{"paths":{"/a":{"get":{"x-roblox-scopes":[
            {"name":"universe-datastores.objects:read"}
        ]}}}}"#;
        let scopes = parse_scopes(json).unwrap();
        assert_eq!(
            scopes["universe-datastores.objects"].target_type,
            "universe-datastore"
        );
    }

    #[test]
    fn split_scope_handles_multiple_operations() {
        assert_eq!(
            split_scope("a.b:read,write"),
            ("a.b", vec!["read", "write"])
        );
    }

    #[test]
    fn split_scope_handles_no_operations() {
        assert_eq!(split_scope("a.b"), ("a.b", Vec::<&str>::new()));
    }

    #[test]
    fn descriptions_are_carried_over_from_the_previous_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.json");
        std::fs::write(
            &path,
            r#"{"version":"old","source_url":"u","scopes":{
                "universe":{"operations":["read"],"target_type":"universe","description":"kept"}
            }}"#,
        )
        .unwrap();

        let mut scopes = parse_scopes(OBJECT_FORM).unwrap();
        let carried = carry_over_descriptions(&path, &mut scopes);

        assert_eq!(carried, 1);
        assert_eq!(scopes["universe"].description.as_deref(), Some("kept"));
        assert!(scopes["universe.user-restriction"].description.is_none());
    }

    #[test]
    fn an_http_source_is_recorded_as_given() {
        let url = "https://example.invalid/openapi.json";
        assert_eq!(provenance_for(url), url);
    }

    /// Regenerating from the vendored spec must not stamp a developer's
    /// absolute path into the committed catalog.
    #[test]
    fn a_vendored_spec_is_recorded_as_its_upstream_permalink() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("source.json"),
            r#"{"repository":"Roblox/creator-docs","commit":"abc123",
                "document":"content/en-us/reference/cloud/openapi.json"}"#,
        )
        .unwrap();
        let spec = dir.path().join("openapi.json");
        std::fs::write(&spec, "{}").unwrap();

        assert_eq!(
            provenance_for(&format!("file://{}", spec.display())),
            "https://github.com/Roblox/creator-docs/blob/abc123/content/en-us/reference/cloud/openapi.json"
        );
    }

    #[test]
    fn a_local_spec_without_provenance_falls_back_to_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let spec = dir.path().join("openapi.json");
        std::fs::write(&spec, "{}").unwrap();

        let url = format!("file://{}", spec.display());
        assert_eq!(provenance_for(&url), url);
    }

    #[test]
    fn a_malformed_source_json_falls_back_rather_than_producing_a_broken_link() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("source.json"),
            r#"{"repository":"only-this"}"#,
        )
        .unwrap();
        let spec = dir.path().join("openapi.json");
        std::fs::write(&spec, "{}").unwrap();

        let url = format!("file://{}", spec.display());
        assert_eq!(provenance_for(&url), url);
    }

    /// The junk-file case: an empty directory is where an installed binary
    /// normally runs, and it must refuse rather than create `src/data/`.
    #[test]
    fn regenerating_outside_a_checkout_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("src/data/catalog.json");

        let err = ensure_writable_from_here(&path).unwrap_err().to_string();

        assert!(err.contains("rbx-forge/rbx-apikey"), "{err}");
        assert!(
            !dir.path().join("src").exists(),
            "created the parents anyway"
        );
    }

    #[test]
    fn regenerating_from_a_checkout_is_allowed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/data")).unwrap();

        ensure_writable_from_here(&dir.path().join("src/data/catalog.json")).unwrap();
    }

    /// A bare filename has no parent directory to require, and the working
    /// directory it lands in exists by definition.
    #[test]
    fn a_path_without_a_parent_directory_is_allowed() {
        ensure_writable_from_here(Path::new("catalog.json")).unwrap();
    }

    #[test]
    fn a_missing_previous_catalog_is_not_an_error() {
        let mut scopes = parse_scopes(OBJECT_FORM).unwrap();
        assert_eq!(
            carry_over_descriptions(Path::new("does/not/exist.json"), &mut scopes),
            0
        );
    }
}
