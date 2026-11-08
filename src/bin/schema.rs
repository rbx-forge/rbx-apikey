//! Writes `schemas/rbxapikey.schema.json` from the serde models that read
//! `rbxapikey.toml`, so editors can validate and autocomplete it.
//!
//! A dev tool rather than a subcommand: `schemars` and its derive machinery
//! would otherwise ride along in the shipped binary and never be called. The
//! models carry `JsonSchema` behind the off-by-default `schema` feature, and
//! this binary requires it.
//!
//! Derived rather than hand-written, because a hand-written schema is a second
//! description of the same format, and the two drift the moment somebody adds
//! a field. The doc comments on the structs become the hover text for free.
//!
//! **The schema must not be stricter than the tool.** Unknown keys are warned
//! about and ignored, never rejected, so nothing here closes
//! `additionalProperties`. An editor painting a valid file red teaches people
//! to ignore the squiggles, and then the real errors go unread too.

use anyhow::{Context, Result};
use clap::Parser;
use schemars::gen::SchemaSettings;
use schemars::schema::{RootSchema, Schema as SchemaNode};

use rbx_core::generated::{CheckReport, GeneratedFile};

const CONFIG_FILE: &str = "rbxapikey.toml";
const FILE_NAME: &str = "rbxapikey.schema.json";
const DESCRIPTION: &str = "Declarative Open Cloud API keys: scopes, expiry, allowed CIDRs, and \
                           where each key's secret is written. See https://rbx-forge.github.io/rbx-apikey/configuration/";

#[derive(Parser, Debug)]
#[command(
    name = "rbx-apikey-schema",
    about = "Generate the JSON Schema for rbxapikey.toml",
    long_about = "Writes schemas/rbxapikey.schema.json.\n\n\
                  Run with --check in CI to fail when the committed schema no longer \
                  matches the models. Exits 2 on drift."
)]
struct Cli {
    /// Compare against the committed schema instead of writing it.
    ///
    /// Exits 2 if the file is missing or stale, so a CI job can tell
    /// "regenerate and commit" from "the command blew up".
    #[arg(long)]
    check: bool,

    /// Directory the schema lives in, relative to the working directory.
    #[arg(long, default_value = "schemas")]
    out_dir: std::path::PathBuf,
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("Error: {err:?}");
            if err
                .chain()
                .any(|cause| cause.is::<rbx_core::generated::Drift>())
            {
                std::process::ExitCode::from(rbx_core::generated::DRIFT_EXIT_CODE)
            } else {
                std::process::ExitCode::FAILURE
            }
        }
    }
}

fn run(cli: &Cli) -> Result<()> {
    let body = schema();
    let json = serde_json::to_string_pretty(&body)
        .with_context(|| format!("serialising the schema for {CONFIG_FILE}"))?;
    // Trailing newline, so the file does not show up as "\ No newline at end
    // of file" in every diff that ever touches it.
    let file = GeneratedFile::new(cli.out_dir.join(FILE_NAME), format!("{json}\n"));

    if cli.check {
        let mut report = CheckReport::new();
        report.check(&file)?;
        report.finish(
            "the serde config models",
            "cargo run -p rbx-apikey --features schema --bin rbx-apikey-schema",
        )
    } else {
        file.write()?;
        println!("{}", file.path.display());
        Ok(())
    }
}

/// Draft 7, not the 2020-12 default: taplo and VS Code's Even Better TOML,
/// the two consumers this exists for, are built on draft 7 validators, and a
/// newer draft makes them fall back to a silent partial validation.
fn schema() -> RootSchema {
    let generator = SchemaSettings::draft07().into_generator();
    let mut body = generator.into_root_schema_for::<rbx_apikey::config::RawFile>();

    // The root model's Rust name is an implementation detail and reads as
    // noise in an editor's hover. The file name is what the user is looking at.
    let meta = body.schema.metadata();
    meta.title = Some(CONFIG_FILE.to_string());
    meta.description = Some(DESCRIPTION.to_string());

    strip_rust_doc_links(&mut body);
    body
}

/// Rewrite rustdoc intra-doc links into plain text, everywhere in the schema.
///
/// The doc comments are written for someone reading the source, so they link
/// Rust items with brackets. That renders as a link in rustdoc and as literal
/// brackets in an editor tooltip, pointing at a name the reader of a TOML file
/// cannot look up. Stripping the brackets keeps the sentence.
fn strip_rust_doc_links(root: &mut RootSchema) {
    fn clean(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find("[`") {
            let Some(end) = rest[start..].find("`]") else {
                break;
            };
            out.push_str(&rest[..start]);
            out.push('`');
            out.push_str(&rest[start + 2..start + end]);
            out.push('`');
            rest = &rest[start + end + 2..];
        }
        out.push_str(rest);
        out
    }

    fn walk(node: &mut SchemaNode) {
        let SchemaNode::Object(object) = node else {
            return;
        };
        if let Some(meta) = object.metadata.as_mut() {
            if let Some(description) = meta.description.as_mut() {
                *description = clean(description);
            }
        }
        if let Some(validation) = object.object.as_mut() {
            for schema in validation.properties.values_mut() {
                walk(schema);
            }
            if let Some(additional) = validation.additional_properties.as_mut() {
                walk(additional);
            }
        }
        if let Some(array) = object.array.as_mut() {
            if let Some(schemars::schema::SingleOrVec::Single(items)) = array.items.as_mut() {
                walk(items);
            }
        }
        for schema in object.subschemas.as_mut().into_iter().flat_map(|s| {
            s.any_of
                .iter_mut()
                .chain(s.one_of.iter_mut())
                .chain(s.all_of.iter_mut())
                .flatten()
        }) {
            walk(schema);
        }
    }

    for schema in root.definitions.values_mut() {
        walk(schema);
    }
    let mut root_node = SchemaNode::Object(root.schema.clone());
    walk(&mut root_node);
    if let SchemaNode::Object(object) = root_node {
        root.schema = object;
    }
}
