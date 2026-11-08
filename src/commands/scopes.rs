//! `rbx-apikey scopes list|show`: inspect the bundled scope catalog.

use anyhow::Result;
use colored::Colorize;
use std::collections::BTreeMap;

use rbx_core::output::{emit, OutputFormat};

use crate::json::ScopeDocument;
use crate::scope_catalog;

pub fn show(scope_type: &str, format: OutputFormat) -> Result<()> {
    let lookup = scope_catalog::lookup(scope_type);

    // Before the `known` branch, not inside it: an unknown scope is a normal
    // answer here rather than a failure, so it is a document saying `known:
    // false` rather than two sentences on stdout and no document.
    if format.is_json() {
        return emit(&ScopeDocument::new(
            scope_type,
            &lookup,
            scope_catalog::version(),
        ));
    }

    if !lookup.known {
        println!(
            "{}",
            format!(
                "\"{}\" is not in the catalog (catalog version: {}).",
                scope_type,
                scope_catalog::version()
            )
            .yellow()
        );
        println!(
            "You can still use it in apikey.toml - the tool will send it to Roblox with a warning."
        );
        return Ok(());
    }
    println!("{}", scope_type);
    if let Some(t) = &lookup.target_type {
        println!("  target_type: {}", t);
    }
    if let Some(ops) = &lookup.known_operations {
        println!("  operations:  {}", ops.join(", "));
    }
    Ok(())
}

pub fn list() {
    println!(
        "{}",
        format!("Scope catalog (version {})", scope_catalog::version()).cyan()
    );
    println!("Source: {}", scope_catalog::source_url());
    // Named separately because it is what decides the target column below,
    // and the spec it is reconciled against gets fourteen of them wrong.
    if let Some(authority) = scope_catalog::authority_url() {
        println!("Targets and operations: {}", authority);
    }
    println!();

    let mut groups: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
    groups.insert("universe", vec![]);
    groups.insert("universe-datastore", vec![]);
    groups.insert("creator", vec![]);
    groups.insert("none", vec![]);

    for name in scope_catalog::all_scopes() {
        let lk = scope_catalog::lookup(&name);
        if let Some(t) = lk.target_type {
            let bucket = match t.as_str() {
                "universe" => "universe",
                "universe-datastore" => "universe-datastore",
                "creator" => "creator",
                "none" => "none",
                _ => "universe",
            };
            groups.get_mut(bucket).unwrap().push(name);
        }
    }

    let labels: &[(&str, &str)] = &[
        ("universe", "Universe-target  (targetParts: [universeId])"),
        (
            "universe-datastore",
            "Universe-datastore (targetParts: [universeId, datastoreName?])",
        ),
        (
            "creator",
            "Creator-target   (targetParts: [G<groupId> | U<userId> | *])",
        ),
        ("none", "No resource selection (targetParts: [*])"),
    ];

    for (key, label) in labels {
        println!("{}", label.cyan());
        if let Some(names) = groups.get(*key) {
            for name in names {
                let lk = scope_catalog::lookup(name);
                let ops = lk
                    .known_operations
                    .map(|o| o.join(","))
                    .unwrap_or_else(|| "?".to_string());
                println!("  {}:{}", name, ops);
            }
        }
        println!();
    }
}
