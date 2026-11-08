//! Every scope named in the documentation has to exist.
//!
//! This test exists because its absence once cost real time. A page told
//! people to ask for `universe.image:read` and `legacy-universe.badge:read`.
//! Neither exists: the first is not a scope type at all, and the second has
//! only `write` and `manage-and-spend-robux`. Somebody following
//! the page got `400 Response.InvalidScopes` from Roblox, which is a bad place
//! to learn that a documented scope was invented.
//!
//! Two things let it survive, and both are worth knowing:
//!
//! - **The tool is deliberately forgiving.** An unknown scope is a warning and
//!   exit 0, because the catalog ships with the binary and Roblox adds scopes
//!   between releases. That is right, and it means a wrong scope in a page
//!   surfaces only when somebody creates a key with it.
//! - **Nothing read the scope tables**, while the authoritative list of scopes
//!   sat in this very crate the whole time.
//!
//! What this refuses is narrow on purpose: a scope *type* the catalog does not
//! know, or an *operation* the catalog does not list for a type it does know.
//! It is not a claim that the catalog is complete: it is not, and
//! `KNOWN_ABSENT` below is where that gets recorded rather than papered over.

use std::path::{Path, PathBuf};

use rbx_apikey::scope_catalog;

/// Scope types Roblox accepts and the catalog does not carry.
///
/// The catalog is parsed out of Roblox's published `openapi.json`, which does
/// not describe every scope the API takes. A type listed here is one somebody
/// has confirmed against the live API; anything else is a typo until proven
/// otherwise. Each entry needs the evidence, not just the name.
const KNOWN_ABSENT: &[(&str, &str)] = &[
    // Nothing here yet. When a scope earns a line, the second field is where it
    // was confirmed (a successful `apikey create`, or an introspect that read
    // it back) so the next person does not have to re-establish it.
];

/// Backticked `a:b` spans that are not scopes.
///
/// A scope type and an ordinary lowercase word are structurally identical
/// (`asset:read` and `file:line` have the same shape) so the matcher cannot
/// tell them apart and this list does. Keep it short: every entry is a place
/// the check is blind, so a real scope typo spelled like one of these would
/// pass. Prefer rewording the prose over adding a line here.
const NOT_SCOPES: &[&str] = &[
    // README's "reference code as file:line" convention.
    "file:line",
];

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is the repository root: one crate, no workspace.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn markdown_files(root: &Path) -> Vec<PathBuf> {
    fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect(&path, out);
            } else if path.extension().is_some_and(|e| e == "md") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    collect(&root.join("docs"), &mut out);
    for name in ["README.md", "CONTRIBUTING.md", "SECURITY.md"] {
        let p = root.join(name);
        if p.is_file() {
            out.push(p);
        }
    }
    // CHANGELOG.md is left out: it records what was true at a release, which
    // is history rather than instruction.
    out.sort();
    out
}

/// Pull `` `scope-type:op,op` `` out of a line of Markdown.
///
/// Only backticked spans count. A scope named in prose without them is not
/// something a reader would copy, and matching bare text would catch every
/// `http:` and `file:` on the page.
fn scopes_in(line: &str) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    for span in line.split('`').skip(1).step_by(2) {
        let Some((ty, ops)) = span.split_once(':') else {
            continue;
        };
        if ty.is_empty() || ops.is_empty() {
            continue;
        }
        // A scope type is lowercase letters, digits, dots and dashes. This also
        // rejects `https`, `file` and friends, whose "operations" start with a
        // slash.
        let ty_ok = ty
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-');
        let ops_ok = ops
            .chars()
            .all(|c| c.is_ascii_lowercase() || c == ',' || c == '-');
        if !ty_ok || !ops_ok {
            continue;
        }
        out.push((
            ty.to_string(),
            ops.split(',').map(str::to_string).collect::<Vec<_>>(),
        ));
    }
    out
}

#[test]
fn every_documented_scope_exists() {
    let root = repo_root();
    let files = markdown_files(&root);
    assert!(
        !files.is_empty(),
        "found no Markdown to scan; the paths in this test are wrong"
    );

    let mut wrong = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("read markdown");
        for (n, line) in text.lines().enumerate() {
            for (ty, ops) in scopes_in(line) {
                let span = format!("{ty}:{}", ops.join(","));
                if NOT_SCOPES.contains(&span.as_str()) {
                    continue;
                }
                if KNOWN_ABSENT.iter().any(|(known, _)| *known == ty) {
                    continue;
                }
                let found = scope_catalog::lookup(&ty);
                let rel = file.strip_prefix(&root).unwrap_or(file).display();
                if !found.known {
                    wrong.push(format!(
                        "{rel}:{}: `{ty}:{}`: no such scope type. If Roblox does take it, \
                         add it to KNOWN_ABSENT with where that was confirmed.",
                        n + 1,
                        ops.join(",")
                    ));
                    continue;
                }
                let known_ops = found.known_operations.unwrap_or_default();
                for op in ops {
                    if !known_ops.contains(&op) {
                        wrong.push(format!(
                            "{rel}:{}: `{ty}:{op}`: that type has only [{}].",
                            n + 1,
                            known_ops.join(", ")
                        ));
                    }
                }
            }
        }
    }

    assert!(
        wrong.is_empty(),
        "the documentation names {} scope(s) that do not exist. A reader following one \
         gets `400 Response.InvalidScopes` from Roblox, which is the worst place to find out:\n\n{}\n",
        wrong.len(),
        wrong.join("\n")
    );
}
