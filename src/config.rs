//! `rbxapikey.toml`: user-editable, safe to commit. Declarative per-key configuration.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{anyhow, bail, Context, Result};
use colored::Colorize;
use serde::Deserialize;

use rbx_core::places::PlacesFile;

use crate::time_iso;

pub const FILE: &str = "rbxapikey.toml";

#[derive(Debug, Clone)]
pub struct ScopeSpec {
    pub scope_type: String,
    pub operations: Vec<String>,
}

/// One entry of a key's `datastores` array: which store, in which universe,
/// and what may be done to it.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DatastoreSpec {
    /// Universe the data store belongs to.
    pub universe_id: u64,
    /// Data store name, as created in Studio or through the API.
    pub name: String,
    /// Operations allowed on it: `read`, `create`, `update`, `list`, `delete`.
    pub operations: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct KeyConfig {
    /// Refuse this key if it asks for a write operation. Adds to
    /// `[settings] readonly`; neither turns the other off.
    pub readonly: bool,
    /// The env names this one key targets. For a key produced by fan-out these
    /// are its group's envs, not the whole declaration's: that separation is
    /// the point of the feature.
    pub envs: Vec<String>,
    /// The name of the env group this key came from, when it came from one.
    ///
    /// `None` for the array form, which declares a single key spanning every
    /// env it lists. `Some` is what makes the key one of several siblings, and
    /// it is carried this far because two of the key's identities are derived
    /// from it: the display name Roblox stores, and the `{env_group}`
    /// substitution in a secret file path.
    pub env_group: Option<String>,
    /// Universes this key targets by id, beside or instead of `envs`. What
    /// lets a key exist in a directory with no `rbxplace.toml`.
    pub universe_ids: Vec<u64>,
    /// Places this key targets, each standing for the universe it belongs to,
    /// resolved when the key is built and cached in the lockfile.
    pub place_ids: Vec<u64>,
    pub group_ids: Vec<u64>,
    pub user_ids: Vec<u64>,
    pub scopes: Vec<ScopeSpec>,
    pub datastores: Vec<DatastoreSpec>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub enabled: Option<bool>,
    pub expiration_months: Option<i64>,
    pub expiration_days: Option<i64>,
    pub expires_at: Option<String>,
    pub allowed_cidrs: Option<Vec<String>>,
    pub secret_file: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Settings {
    pub default_envs: Vec<String>,
    pub default_expiration_months: Option<i64>,
    pub default_allowed_cidrs: Vec<String>,
    pub default_enabled: bool,
    /// Prepended verbatim to every key's display name on Roblox. You control
    /// the separator: use `mygame_` for underscore, `mygame-` for dash, etc.
    /// Lets multiple games share the same `rbxapikey.toml` while keeping the
    /// Creator Hub disambiguated.
    pub name_prefix: Option<String>,
    /// Template path used when a key has no explicit `secret_file`. The
    /// literal `{name}` is replaced with the key's TOML name and `{env_group}`
    /// with its env group. Empty/None means no template, falling back to the
    /// lockfile backend.
    pub default_secret_file: Option<String>,
    /// Refuse any key in this file that asks for a write operation.
    ///
    /// For a directory whose whole point is that it cannot write (`prodread/`
    /// here) where the rule used to live in a comment and was enforced by
    /// nothing. A per-key `readonly` adds to this; neither turns the other off.
    pub readonly: bool,
}

/// `Default` is the empty config, which is what `prune` runs on when there is
/// no `rbxapikey.toml` at all: cleaning up an account is a legitimate thing to
/// do from a directory that declares no keys.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub settings: Settings,
    pub keys: BTreeMap<String, KeyConfig>,
}

// ---------------- Raw deserialization (TOML schema) ----------------
//
// These three types are the on-disk shape of `rbxapikey.toml`, as opposed to
// the resolved [`Config`] above, where defaults have been applied and scope
// strings parsed. They are `pub` because they are also what the JSON Schema is
// derived from: the schema has to describe what a user may write, not what the
// tool ends up holding. Their doc comments become the hover text an editor
// shows, which is why they carry the field documentation rather than the
// resolved types.

/// The `[settings]` table: defaults every key inherits unless it overrides
/// them.
#[derive(Debug, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RawSettings {
    /// Env names from `rbxplace.toml` that a key targets when it names none
    /// itself. Each env contributes its universe id to the key's scopes.
    #[serde(default)]
    default_envs: Option<Vec<String>>,
    /// Lifetime in months for a key that sets no expiry of its own. Roblox
    /// caps a key at 12 months.
    #[serde(default)]
    default_expiration_months: Option<i64>,
    /// IP ranges allowed to use a key that sets no allowlist of its own, in
    /// CIDR form (`203.0.113.7/32`). Must match the public IP of the machine
    /// making the calls; a stale entry fails as an opaque 401.
    #[serde(default)]
    default_allowed_cidrs: Option<Vec<String>>,
    /// Whether keys are created enabled. Defaults to true.
    #[serde(default)]
    default_enabled: Option<bool>,
    /// Prepended verbatim to every key's display name on Roblox, separator
    /// included (`mygame_`). Lets several projects share one Creator Hub
    /// account without colliding names.
    #[serde(default)]
    name_prefix: Option<String>,
    /// Template path for a key's secret file, where `{name}` is replaced with
    /// the key's TOML name and `{env_group}` with its env group, if it has one.
    /// Without it, secrets fall back to the lockfile backend.
    #[serde(default)]
    default_secret_file: Option<String>,
    /// Refuse any key in this file that asks for a write operation.
    #[serde(default)]
    readonly: Option<bool>,
}

/// What a key's `envs` field may hold: one list, or one list per named group.
///
/// TOML tells the two apart on its own (an array is not a table) so fan-out
/// needs no flag of its own to switch on. The array form keeps the meaning it
/// has always had, which matters: a read-only observability key that legitimately
/// spans dev, staging and prod is still one key, and turning every declaration
/// into per-env keys would have broken it.
///
/// Two alternatives were rejected. A `per_env = true` flag
/// cannot express the split people actually run (dev and staging sharing one
/// key while prod stays alone) since it only produces singletons. A positional
/// list of lists gives the groups no stable identity, so the generated key
/// names, secret files and lockfile entries would be keyed by position and
/// would all move the day somebody reorders the list or adds an env.
#[derive(Debug)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema), serde(untagged))]
pub enum RawEnvs {
    /// `envs = ["dev", "prod"]`: one key, targeting every env listed.
    Shared(Vec<String>),
    /// A table of named groups, written as `keys.<name>.envs` with one array
    /// per group, producing one key per group: a `ci` group of dev and staging
    /// and a `prod` group of prod give the keys `deploy_ci` and `deploy_prod`,
    /// neither of which can reach the other's universes.
    ///
    /// Group names are identity, not decoration: they name the generated key,
    /// its Roblox display name and its secret file. Renaming one is therefore
    /// renaming a key, which the lockfile reports as an orphan rather than
    /// quietly retargeting the live key.
    Groups(BTreeMap<String, Vec<String>>),
}

/// Hand-written rather than `#[serde(untagged)]`, for the error message.
///
/// The derived version reports a mistyped `envs` as "data did not match any
/// variant of untagged enum RawEnvs": a Rust type name, in a message about a
/// TOML file, offered to somebody who has never heard of either. A visitor
/// keeps the line and column that TOML parse errors carry and says what the
/// field takes instead.
///
/// `serde(untagged)` stays on the type behind the `schema` feature, where the
/// only thing reading it is the schemars derive: the JSON Schema still has to
/// describe both forms as alternatives, and schemars has no visitor to read.
impl<'de> Deserialize<'de> for RawEnvs {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct EnvsVisitor;

        impl<'de> serde::de::Visitor<'de> for EnvsVisitor {
            type Value = RawEnvs;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an array of env names, or a table of named env groups")
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                seq: A,
            ) -> Result<Self::Value, A::Error> {
                Deserialize::deserialize(serde::de::value::SeqAccessDeserializer::new(seq))
                    .map(RawEnvs::Shared)
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> Result<Self::Value, A::Error> {
                Deserialize::deserialize(serde::de::value::MapAccessDeserializer::new(map))
                    .map(RawEnvs::Groups)
            }
        }

        deserializer.deserialize_any(EnvsVisitor)
    }
}

/// One `[keys.<name>]` table: a single API key's declaration.
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RawKey {
    /// Refuse this key if it asks for a write operation.
    #[serde(default)]
    readonly: Option<bool>,
    /// Env names this key targets, overriding `default_envs`. Written as an
    /// array for one key spanning them all, or as a table of named groups for
    /// one key per group: see the two forms above.
    #[serde(default)]
    envs: Option<RawEnvs>,
    /// Universe ids this key targets directly, with no `rbxplace.toml` needed.
    /// Combines with `envs` and `place_ids`.
    #[serde(default)]
    universe_ids: Option<Vec<u64>>,
    /// Place ids this key targets, each standing for its universe, which is
    /// looked up once and cached in the lockfile. Combines with `envs` and
    /// `universe_ids`.
    #[serde(default)]
    place_ids: Option<Vec<u64>>,
    /// Group ids this key acts on behalf of, for scopes that take a group
    /// target rather than a universe.
    #[serde(default)]
    group_ids: Option<Vec<u64>>,
    /// User ids this key acts on behalf of, for user-targeted scopes.
    #[serde(default)]
    user_ids: Option<Vec<u64>>,
    /// Open Cloud scopes, each written `scope-type:op1,op2`
    /// (`universe.user-restriction:read,write`). Roblox binds these at
    /// creation, so they cannot be widened afterwards.
    #[serde(default)]
    scopes: Option<Vec<String>>,
    /// Data stores this key may reach, when the scope needs naming a store
    /// rather than a whole universe.
    #[serde(default)]
    datastores: Option<Vec<DatastoreSpec>>,
    /// Display name on Roblox, overriding the derived one. Roblox moderation
    /// rejects names containing "rbx" or "roblox" next to an API or commerce
    /// term.
    #[serde(default)]
    name: Option<String>,
    /// Free text shown in the Creator Hub. Worth spending: it is the only
    /// thing that explains a key to whoever finds it in six months.
    #[serde(default)]
    description: Option<String>,
    /// Whether the key is created enabled, overriding `default_enabled`.
    #[serde(default)]
    enabled: Option<bool>,
    /// Lifetime in months, overriding `default_expiration_months`.
    #[serde(default)]
    expiration_months: Option<i64>,
    /// Lifetime in days, for a key that should outlive a task and no longer.
    #[serde(default)]
    expiration_days: Option<i64>,
    /// Explicit expiry as an ISO 8601 timestamp, when neither month nor day
    /// counts express the deadline.
    #[serde(default)]
    expires_at: Option<String>,
    /// IP allowlist for this key, overriding `default_allowed_cidrs`.
    #[serde(default)]
    allowed_cidrs: Option<Vec<String>>,
    /// Where this key's secret is written, overriding `default_secret_file`.
    #[serde(default)]
    secret_file: Option<String>,
}

/// The whole of `rbxapikey.toml`.
#[derive(Debug, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RawFile {
    #[serde(default)]
    settings: Option<RawSettings>,
    /// One entry per key, keyed by the short name commands refer to
    /// (`rbx-apikey create readonly`).
    #[serde(default)]
    keys: BTreeMap<String, RawKey>,
}

// ---------------- Duplicate scopes ----------------

/// One redundancy the loader took out of a key's `scopes` on the way in.
///
/// Reported rather than dropped in silence. A scope written twice grants
/// nothing extra (the key is no wider for it) so collapsing it is safe, but
/// it is almost always a merge artefact or a half-finished edit, and the line
/// it was meant to be is worth a look. This is the same stance the duplicate
/// resource name in `init --from-remote` takes: a duplicate is a question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollapsedScope {
    /// The table it was written in, `keys.deploy`.
    pub table: String,
    /// The entry as first written, `asset:read`.
    pub entry: String,
    /// What was collapsed and how often, as one clause.
    pub removed: String,
}

/// The warning text for `collapsed`, or `None` when there is nothing to say.
///
/// Pure, and split out of `warn_collapsed_scopes` for the reason
/// `rbx_core::places::unknown_keys_warning` gives: the printing path
/// deduplicates through process-global state, so the first test to warn about a
/// path would silently mute every later one. The wording is the part worth
/// testing, so it lives where a test can reach it.
pub fn collapsed_scopes_warning(path: &Path, collapsed: &[CollapsedScope]) -> Option<String> {
    if collapsed.is_empty() {
        return None;
    }

    let mut out = format!(
        "{} {}: {} duplicate scope declaration{}, collapsed before the request to Roblox:\n\n",
        "warning:".yellow().bold(),
        path.display(),
        collapsed.len(),
        if collapsed.len() == 1 { "" } else { "s" },
    );
    for entry in collapsed {
        out.push_str(&format!("  [{}] {}\n", entry.table, entry.entry.yellow()));
        out.push_str(&format!("    {}\n", entry.removed.dimmed()));
    }
    out.push_str(
        "\nA duplicate grants nothing extra: the key is no wider for it, only the payload\n\
         Roblox is asked to store. It is named here rather than dropped in silence because\n\
         a scope written twice is usually a merge artefact, and the line it was meant to be\n\
         is worth a look.",
    );
    Some(out)
}

/// Report collapsed duplicates on stderr, once per path. Never fails the
/// command: the collapsed config is exactly as capable as the written one, so
/// there is nothing here worth refusing to run over.
///
/// Once per path, and process-global to do it, for the same reason
/// `warn_unknown_keys` is: `rbx-apikey doctor` reaches this file through
/// [`crate::diagnostics`] while the command it is diagnosing loads it too, and
/// the reader should meet the warning once rather than twice.
fn warn_collapsed_scopes(path: &Path, collapsed: &[CollapsedScope]) {
    let Some(message) = collapsed_scopes_warning(path, collapsed) else {
        return;
    };

    static WARNED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    let already_warned = {
        let mut warned = WARNED
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        !warned.insert(path.to_path_buf())
    };
    if already_warned {
        return;
    }

    eprintln!("{message}");
}

// ---------------- Parsing ----------------

/// A scope string after parsing, plus what parsing had to collapse.
struct ParsedScope {
    spec: ScopeSpec,
    /// Operations named more than once inside this one entry, each reported
    /// once with the number of times it appeared.
    repeated_operations: Vec<(String, usize)>,
}

fn parse_scope_string(s: &str) -> Result<ParsedScope> {
    let colon = s.find(':').ok_or_else(|| {
        anyhow!(
            "invalid scope \"{}\" - expected format \"scopeType:op1,op2,...\"",
            s
        )
    })?;
    let scope_type = s[..colon].to_string();
    let ops_raw = &s[colon + 1..];

    // First-seen order, not sorted: the file says what it says, and somebody
    // comparing it against `apikey introspect` should not have to account for a
    // reordering this tool did on the way out.
    let mut operations: Vec<String> = Vec::new();
    let mut counts: Vec<(String, usize)> = Vec::new();
    for op in ops_raw
        .split(',')
        .map(|op| op.trim().to_string())
        .filter(|op| !op.is_empty())
    {
        match counts.iter_mut().find(|(seen, _)| *seen == op) {
            Some(slot) => slot.1 += 1,
            None => {
                counts.push((op.clone(), 1));
                operations.push(op);
            }
        }
    }

    if operations.is_empty() {
        bail!("scope \"{}\" has no operations", s);
    }
    if scope_type.is_empty() {
        bail!("scope \"{}\" has empty scopeType", s);
    }
    Ok(ParsedScope {
        spec: ScopeSpec {
            scope_type,
            operations,
        },
        repeated_operations: counts.into_iter().filter(|(_, times)| *times > 1).collect(),
    })
}

/// Parse a key's `scopes`, collapsing duplicates at both levels.
///
/// Two entries are the same entry when they name the same scope type and the
/// same set of operations, whatever order each wrote them in: `universe:read,write`
/// and `universe:write,read` are one declaration written twice. The first
/// spelling survives verbatim.
///
/// Two entries sharing a scope type but listing *different* operations are left
/// alone. Folding them into their union would be a normalisation rather than a
/// deduplication, and the resulting payload is one this tool has never sent and
/// Roblox has never been observed accepting; deduplication stops deliberately
/// short of it.
fn parse_scopes(
    table: &str,
    scope_strings: &[String],
    collapsed: &mut Vec<CollapsedScope>,
) -> Result<Vec<ScopeSpec>> {
    let mut scopes: Vec<ScopeSpec> = Vec::with_capacity(scope_strings.len());
    // (fingerprint, first spelling, times seen), in first-seen order.
    let mut seen: Vec<((String, Vec<String>), String, usize)> = Vec::new();

    for s in scope_strings {
        let parsed = parse_scope_string(s).with_context(|| format!("[{table}].scopes"))?;
        for (op, times) in &parsed.repeated_operations {
            collapsed.push(CollapsedScope {
                table: table.to_string(),
                entry: s.clone(),
                removed: format!("operation \"{op}\" is listed {times} times, sent once"),
            });
        }

        let mut ops = parsed.spec.operations.clone();
        ops.sort();
        let fingerprint = (parsed.spec.scope_type.clone(), ops);
        match seen.iter_mut().find(|(f, _, _)| *f == fingerprint) {
            Some((_, _, times)) => *times += 1,
            None => {
                seen.push((fingerprint, s.clone(), 1));
                scopes.push(parsed.spec);
            }
        }
    }

    for (_, spelling, times) in seen {
        if times > 1 {
            collapsed.push(CollapsedScope {
                table: table.to_string(),
                entry: spelling,
                removed: format!("the same entry is listed {times} times, sent once"),
            });
        }
    }

    Ok(scopes)
}

/// Env group names end up in three identities at once (the generated key's
/// name here, its display name on Roblox, and its secret file path) so the
/// character set is the intersection of what all three take without escaping.
///
/// Refusing rather than sanitising: a silently rewritten group name is a name
/// the developer then lives with for as long as the key exists, chosen by a
/// tool rather than by them.
fn validate_group_name(key_name: &str, group: &str) -> Result<()> {
    if group.is_empty() {
        bail!("[keys.{key_name}.envs] has a group with an empty name");
    }
    if !group
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        bail!(
            "[keys.{key_name}.envs] group \"{group}\": a group name may hold only ASCII letters, \
             digits, \"_\" and \"-\". It becomes part of a key name, a Roblox display name and a \
             file path, so anything else would have to be escaped differently in each."
        );
    }
    Ok(())
}

/// One declaration to one or more keys.
///
/// The array form yields the declaration itself. The table form yields one key
/// per group, named `<key>_<group>`, differing only in which envs they target,
/// which is the whole point: everything else, scopes above all, is written once
/// and cannot drift between environments the way three hand-copied blocks do.
fn parse_key(
    name: &str,
    raw: RawKey,
    collapsed: &mut Vec<CollapsedScope>,
) -> Result<Vec<(String, KeyConfig)>> {
    let scope_strings = raw
        .scopes
        .ok_or_else(|| anyhow!("[keys.{}]: scopes must be an array of strings", name))?;
    let scopes = parse_scopes(&format!("keys.{name}"), &scope_strings, collapsed)?;

    let datastores = raw.datastores.unwrap_or_default();
    for (i, d) in datastores.iter().enumerate() {
        if d.name.is_empty() {
            bail!(
                "[keys.{}].datastores[{}].name must be a non-empty string",
                name,
                i
            );
        }
    }

    let base = KeyConfig {
        readonly: raw.readonly.unwrap_or(false),
        envs: Vec::new(),
        env_group: None,
        universe_ids: raw.universe_ids.unwrap_or_default(),
        place_ids: raw.place_ids.unwrap_or_default(),
        group_ids: raw.group_ids.unwrap_or_default(),
        user_ids: raw.user_ids.unwrap_or_default(),
        scopes,
        datastores,
        name: raw.name,
        description: raw.description,
        enabled: raw.enabled,
        expiration_months: raw.expiration_months,
        expiration_days: raw.expiration_days,
        expires_at: raw.expires_at,
        allowed_cidrs: raw.allowed_cidrs,
        secret_file: raw.secret_file,
    };

    match raw.envs {
        None | Some(RawEnvs::Shared(_)) => {
            let envs = match raw.envs {
                Some(RawEnvs::Shared(envs)) => envs,
                _ => Vec::new(),
            };
            Ok(vec![(name.to_string(), KeyConfig { envs, ..base })])
        }
        Some(RawEnvs::Groups(groups)) => {
            if groups.is_empty() {
                bail!(
                    "[keys.{name}.envs] names no groups. A table of env groups declares one key \
                     per group, so an empty one declares no key at all - write \
                     `envs = [...]` for a single key, or name at least one group."
                );
            }
            if !base.universe_ids.is_empty() || !base.place_ids.is_empty() {
                // Copied into every group's key, they would give the prod key
                // the ci universes and the reverse: the separation the table
                // exists for, undone by one line.
                bail!(
                    "[keys.{name}]: universe_ids and place_ids do not combine with a table of env \
                     groups, because every group's key would target them. Declare a separate \
                     key for those ids."
                );
            }
            let mut out = Vec::with_capacity(groups.len());
            for (group, envs) in groups {
                validate_group_name(name, &group)?;
                if envs.is_empty() {
                    bail!(
                        "[keys.{name}.envs] group \"{group}\" lists no envs. A key targeting no \
                         universe is scoped to all of them, which is the opposite of what a group \
                         is for."
                    );
                }
                out.push((
                    format!("{name}_{group}"),
                    KeyConfig {
                        envs,
                        env_group: Some(group),
                        ..base.clone()
                    },
                ));
            }
            Ok(out)
        }
    }
}

/// The operations a read-only key may ask for.
///
/// Deliberately a short allow-list rather than a deny-list of writes. Roblox
/// adds scope types and operations whenever it likes, and a deny-list would
/// silently let each new one through, which is the failure mode this guard
/// exists to close, so it must not have it too.
const READ_ONLY_OPERATIONS: &[&str] = &["read", "list"];

/// Refuse a key that declares `readonly` and then asks for a write.
///
/// **This exists because the enforcement that was claimed does not exist.**
/// `prodread/rbxapikey.example.toml` said a key declared there "cannot be made
/// to write no matter what code calls it", on the grounds that Roblox binds
/// scopes at creation. Half of that is true: no call widens a key at runtime.
/// The other half was disproved by running `rbx-apikey update` on an already
/// created key and watching Roblox accept `asset:read,write`. The verb that
/// widens a key is one this tool ships, and the file that forbade writes was
/// the same file somebody would edit to allow one.
///
/// So the guard belongs at config load, which is the one place that sees the
/// declaration before anything is sent, and it belongs in code rather than in
/// prose because a comment enforces nothing.
fn check_readonly(name: &str, scopes: &[ScopeSpec]) -> Result<()> {
    for scope in scopes {
        for op in &scope.operations {
            if !READ_ONLY_OPERATIONS.contains(&op.as_str()) {
                bail!(
                    "[keys.{name}] is readonly and asks for `{}:{op}`. A readonly key may only use {}. Drop the operation, or drop `readonly`, but if this file is the one that is not supposed to hold write scopes, dropping `readonly` is the change to think twice about.",
                    scope.scope_type,
                    READ_ONLY_OPERATIONS.join(" and ")
                );
            }
        }
    }
    Ok(())
}

pub fn load() -> Result<Config> {
    load_from(Path::new(FILE))
}

pub fn load_from(path: &Path) -> Result<Config> {
    // `try_exists` and not `exists`: a config that is there and cannot be
    // looked at is not an absent one, and telling the reader to create a file
    // they already have sends them the wrong way.
    let there = path
        .try_exists()
        .with_context(|| format!("failed to look at {}", path.display()))?;
    if !there {
        bail!(
            "{} not found - create it with at least:\n\n  [keys.<name>]\n  envs = [\"<env>\"]\n  scopes = [\"<scopeType>:<op1>,<op2>\"]\n",
            path.display()
        );
    }
    let raw_text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let raw: RawFile =
        toml::from_str(&raw_text).with_context(|| format!("failed to parse {}", path.display()))?;

    let raw_settings = raw.settings.unwrap_or_default();
    let settings = Settings {
        default_envs: raw_settings.default_envs.unwrap_or_default(),
        default_expiration_months: raw_settings.default_expiration_months,
        default_allowed_cidrs: raw_settings.default_allowed_cidrs.unwrap_or_default(),
        default_enabled: raw_settings.default_enabled.unwrap_or(true),
        name_prefix: raw_settings.name_prefix.filter(|p| !p.is_empty()),
        default_secret_file: raw_settings.default_secret_file.filter(|p| !p.is_empty()),
        readonly: raw_settings.readonly.unwrap_or(false),
    };

    let mut keys = BTreeMap::new();
    let mut collapsed = Vec::new();
    // Generated name → the declaration it came from, so a collision can name
    // both sides instead of reporting that something, somewhere, was overwritten.
    let mut origins: BTreeMap<String, String> = BTreeMap::new();
    // Declaration → the keys it fanned out into, for the secret-file check below.
    let mut siblings: Vec<(String, Vec<String>)> = Vec::new();

    for (name, raw_key) in raw.keys {
        let generated = parse_key(&name, raw_key, &mut collapsed)?;
        // After parse_key, so the check sees the scopes as they were actually
        // resolved rather than as they were typed, and after fan-out, so each
        // generated sibling is named by the name it will be created under.
        for (generated_name, cfg) in &generated {
            if settings.readonly || cfg.readonly {
                check_readonly(generated_name, &cfg.scopes)?;
            }
        }
        let fanned_out =
            generated.len() > 1 || generated.iter().any(|(_, k)| k.env_group.is_some());
        if fanned_out {
            siblings.push((
                name.clone(),
                generated.iter().map(|(n, _)| n.clone()).collect(),
            ));
        }
        for (key_name, parsed) in generated {
            let origin = match &parsed.env_group {
                Some(group) => format!("[keys.{name}.envs] group \"{group}\""),
                None => format!("[keys.{name}]"),
            };
            if let Some(previous) = origins.get(&key_name) {
                bail!(
                    "two keys in {} are both called \"{}\": {} and {}. A key name is its identity \
                     in the lockfile, in its secret file and on Roblox, so one of the two has to \
                     change - rename the group, or the declaration it collides with.",
                    path.display(),
                    key_name,
                    previous,
                    origin
                );
            }
            origins.insert(key_name.clone(), origin);
            keys.insert(key_name, parsed);
        }
    }

    let cfg = Config { settings, keys };
    check_sibling_secret_files(path, &cfg, &siblings)?;
    warn_collapsed_scopes(path, &collapsed);

    Ok(cfg)
}

/// Refuse a fan-out whose generated keys would write their secrets to one file.
///
/// Every other identity a fanned-out key has is distinct by construction (the
/// name carries the group) but `secret_file` is written by hand, and a path
/// with no `{name}` or `{env_group}` in it is the same path for every group.
/// The last `create` would then overwrite the secret of the key created before
/// it, leaving a live key on Roblox that nothing local can authenticate as, and
/// nothing downstream would report it: the lockfile entries differ, the file
/// exists, and its contents are a valid secret for *a* key.
///
/// Only siblings of one declaration are compared. Two unrelated keys pointing
/// at one file is the same hazard, but it is a hazard this change did not
/// introduce and refusing it here would reject configurations that load today.
fn check_sibling_secret_files(
    path: &Path,
    cfg: &Config,
    siblings: &[(String, Vec<String>)],
) -> Result<()> {
    for (declaration, names) in siblings {
        let mut seen: BTreeMap<String, String> = BTreeMap::new();
        for name in names {
            let Some(key_cfg) = cfg.keys.get(name) else {
                continue;
            };
            let Some(file) = resolve_secret_file(cfg, key_cfg, name) else {
                continue;
            };
            if let Some(other) = seen.insert(file.clone(), name.clone()) {
                bail!(
                    "{}: [keys.{}] fans out into one key per env group, but \"{}\" and \"{}\" both \
                     store their secret in {}, so creating one would overwrite the other's. Put \
                     {{name}} or {{env_group}} in the path.",
                    path.display(),
                    declaration,
                    other,
                    name,
                    file
                );
            }
        }
    }
    Ok(())
}

// ---------------- Resolution helpers ----------------

pub fn get<'a>(cfg: &'a Config, name: &str) -> Option<&'a KeyConfig> {
    cfg.keys.get(name)
}

/// The keys a fan-out declaration produced, in name order.
///
/// For the command that was handed `deploy` when the file declares
/// `deploy_ci` and `deploy_prod`. Naming the declaration is the obvious thing
/// to try straight after writing one, and without this the answer is "skipping
/// deploy: not in rbxapikey.toml", which is true, unhelpful, and looks like the
/// file was not saved.
///
/// Empty for a name that declares a key of its own, or nothing at all.
pub fn keys_from_declaration(cfg: &Config, declaration: &str) -> Vec<String> {
    if cfg.keys.contains_key(declaration) {
        return Vec::new();
    }
    cfg.keys
        .iter()
        .filter(|(name, key_cfg)| match &key_cfg.env_group {
            Some(group) => *name == &format!("{declaration}_{group}"),
            None => false,
        })
        .map(|(name, _)| name.clone())
        .collect()
}

pub fn is_enabled(cfg: &Config, key_cfg: &KeyConfig) -> bool {
    if let Some(e) = key_cfg.enabled {
        return e;
    }
    cfg.settings.default_enabled
}

pub fn get_allowed_cidrs(cfg: &Config, key_cfg: &KeyConfig) -> Vec<String> {
    if let Some(c) = &key_cfg.allowed_cidrs {
        if !c.is_empty() {
            return c.clone();
        }
    }
    cfg.settings.default_allowed_cidrs.clone()
}

/// Effective env list for a key: its own `envs` when it has any, else
/// `settings.default_envs`, but only for a key that targets no universe at all.
///
/// A key that names `universe_ids` or `place_ids` has said what it targets, and
/// adding the default envs on top would widen it past what was written.
pub fn effective_envs(cfg: &Config, key_cfg: &KeyConfig) -> Vec<String> {
    if !key_cfg.envs.is_empty() {
        key_cfg.envs.clone()
    } else if key_cfg.universe_ids.is_empty() && key_cfg.place_ids.is_empty() {
        cfg.settings.default_envs.clone()
    } else {
        Vec::new()
    }
}

/// Resolve a key's envs to universe_ids via `rbxplace.toml`. Errors out
/// (rather than silently dropping) if any referenced env is missing.
pub fn resolve_universe_ids(
    cfg: &Config,
    key_cfg: &KeyConfig,
    places: &PlacesFile,
) -> Result<Vec<u64>> {
    let envs = effective_envs(cfg, key_cfg);
    let mut ids = Vec::with_capacity(envs.len());
    for env_name in &envs {
        let env = places.get(env_name).with_context(|| {
            format!("env '{env_name}' (referenced by key) not found in rbxplace.toml")
        })?;
        ids.push(env.universe_id);
    }
    Ok(ids)
}

/// Display name sent to Roblox: explicit `key.name`, else the TOML key,
/// optionally prefixed by `settings.name_prefix` (verbatim, no separator
/// injected: the user controls the separator).
///
/// A fanned-out key appends its group, because Roblox stores this name and two
/// keys called `deploy` in one Creator Hub are two keys nobody can tell apart.
/// The suffix is only added when `key.name` is explicit: with no override the
/// TOML key is already `deploy_ci`, and appending again would give `deploy_ci_ci`.
pub fn resolve_remote_name(cfg: &Config, key_cfg: &KeyConfig, key_name: &str) -> String {
    let base = match (&key_cfg.name, &key_cfg.env_group) {
        (Some(explicit), Some(group)) => format!("{explicit}_{group}"),
        (Some(explicit), None) => explicit.clone(),
        (None, _) => key_name.to_string(),
    };
    match cfg.settings.name_prefix.as_deref() {
        Some(p) if !p.is_empty() => format!("{p}{base}"),
        _ => base,
    }
}

/// Secret file path: explicit `key.secret_file` wins, then
/// `settings.default_secret_file`. Returns None when neither is set (the
/// lockfile backend takes over).
///
/// Both forms are templated, and `{name}` is the *generated* key's name, so
/// `deploy_ci` and `deploy_prod` land in different files under the template
/// that was already there. That is why the placeholder was not redefined to
/// mean the declaration: a config carrying `.secrets/{name}.env` today gains
/// fan-out without editing the template, and no arrangement of the two
/// placeholders can make two keys share one secret file by accident.
///
/// `{env_group}` is the group on its own, for layouts that want it as a path
/// segment of its own (`.secrets/{env_group}/{name}.env`). It expands to
/// nothing for a key that does not fan out, which is why it belongs in a file
/// where every key does: `{name}` already separates the rest.
///
/// Templating the explicit `secret_file` too is a change from the verbatim
/// path it used to be. A fanned-out declaration has one `secret_file` and
/// several keys, so without it the only way to give each its own file would be
/// to abandon the declaration and hand-copy it, which is the triplication this
/// whole feature exists to remove.
pub fn resolve_secret_file(cfg: &Config, key_cfg: &KeyConfig, key_name: &str) -> Option<String> {
    let template = key_cfg
        .secret_file
        .as_ref()
        .filter(|p| !p.is_empty())
        .or_else(|| {
            cfg.settings
                .default_secret_file
                .as_ref()
                .filter(|t| !t.is_empty())
        })?;
    Some(
        template
            .replace("{name}", key_name)
            .replace("{env_group}", key_cfg.env_group.as_deref().unwrap_or("")),
    )
}

/// Priority: expires_at > expiration_days > expiration_months > settings.default_expiration_months.
/// Returns None for "never expires".
pub fn get_expiration_time(cfg: &Config, key_cfg: &KeyConfig) -> Option<String> {
    if let Some(e) = &key_cfg.expires_at {
        return Some(e.clone());
    }
    if let Some(d) = key_cfg.expiration_days {
        return Some(time_iso::iso_in_days(d));
    }
    if let Some(m) = key_cfg.expiration_months {
        return Some(time_iso::iso_in_months(m));
    }
    if let Some(m) = cfg.settings.default_expiration_months {
        return Some(time_iso::iso_in_months(m));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a document from a file, because that is the only entry point
    /// `load_from` has and half of what is tested here happens on the way in.
    /// `label` keeps two tests running in parallel off the same path.
    fn load_text(label: &str, text: &str) -> Result<Config> {
        let dir =
            std::env::temp_dir().join(format!("rbxapikey_cfg_{}_{}", std::process::id(), label));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE);
        std::fs::write(&path, text).unwrap();
        let loaded = load_from(&path);
        let _ = std::fs::remove_dir_all(&dir);
        loaded
    }

    fn key_with(scope: &str) -> KeyConfig {
        let spec = parse_scope_string(scope).unwrap().spec;
        KeyConfig {
            readonly: false,
            envs: vec![],
            env_group: None,
            universe_ids: Vec::new(),
            place_ids: Vec::new(),
            group_ids: vec![],
            user_ids: vec![],
            scopes: vec![spec],
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
    fn parse_scope_simple() {
        let s = parse_scope_string("universe:read,write").unwrap().spec;
        assert_eq!(s.scope_type, "universe");
        assert_eq!(s.operations, vec!["read", "write"]);
    }

    #[test]
    fn parse_scope_trims_whitespace() {
        let s = parse_scope_string("asset: read , write ").unwrap().spec;
        assert_eq!(s.operations, vec!["read", "write"]);
    }

    #[test]
    fn parse_scope_no_colon_fails() {
        assert!(parse_scope_string("badformat").is_err());
    }

    #[test]
    fn parse_scope_no_ops_fails() {
        assert!(parse_scope_string("universe:").is_err());
    }

    #[test]
    fn parse_scope_empty_type_fails() {
        assert!(parse_scope_string(":read").is_err());
    }

    #[test]
    fn expiration_priority_expires_at_wins() {
        let mut k = key_with("universe:read");
        k.expires_at = Some("2099-01-01T00:00:00.000Z".into());
        k.expiration_days = Some(7);
        k.expiration_months = Some(3);
        let cfg = Config {
            settings: Settings {
                default_expiration_months: Some(12),
                default_enabled: true,
                ..Default::default()
            },
            keys: BTreeMap::new(),
        };
        assert_eq!(
            get_expiration_time(&cfg, &k),
            Some("2099-01-01T00:00:00.000Z".into())
        );
    }

    #[test]
    fn expiration_priority_days_over_months() {
        let mut k = key_with("universe:read");
        k.expiration_days = Some(7);
        k.expiration_months = Some(3);
        let cfg = Config {
            settings: Settings {
                default_enabled: true,
                ..Default::default()
            },
            keys: BTreeMap::new(),
        };
        // Just check it's some string (date based on now); priority is days, so it's ~7 days out.
        let s = get_expiration_time(&cfg, &k).unwrap();
        assert!(s.ends_with("Z"));
    }

    #[test]
    fn expiration_none_when_nothing_set() {
        let k = key_with("universe:read");
        let cfg = Config {
            settings: Settings {
                default_enabled: true,
                ..Default::default()
            },
            keys: BTreeMap::new(),
        };
        assert!(get_expiration_time(&cfg, &k).is_none());
    }

    #[test]
    fn is_enabled_defaults_to_true() {
        let k = key_with("universe:read");
        let cfg = Config {
            settings: Settings {
                default_enabled: true,
                ..Default::default()
            },
            keys: BTreeMap::new(),
        };
        assert!(is_enabled(&cfg, &k));
    }

    #[test]
    fn is_enabled_explicit_false_overrides_default() {
        let mut k = key_with("universe:read");
        k.enabled = Some(false);
        let cfg = Config {
            settings: Settings {
                default_enabled: true,
                ..Default::default()
            },
            keys: BTreeMap::new(),
        };
        assert!(!is_enabled(&cfg, &k));
    }

    fn cfg_with_settings(s: Settings) -> Config {
        Config {
            settings: s,
            keys: BTreeMap::new(),
        }
    }

    #[test]
    fn resolve_remote_name_falls_back_to_key_name() {
        let k = key_with("universe:read");
        let cfg = cfg_with_settings(Settings {
            default_enabled: true,
            ..Default::default()
        });
        assert_eq!(resolve_remote_name(&cfg, &k, "deploy"), "deploy");
    }

    #[test]
    fn resolve_remote_name_uses_explicit_name() {
        let mut k = key_with("universe:read");
        k.name = Some("Custom Name".into());
        let cfg = cfg_with_settings(Settings {
            default_enabled: true,
            ..Default::default()
        });
        assert_eq!(resolve_remote_name(&cfg, &k, "deploy"), "Custom Name");
    }

    #[test]
    fn resolve_remote_name_prepends_prefix() {
        let k = key_with("universe:read");
        let cfg = cfg_with_settings(Settings {
            default_enabled: true,
            name_prefix: Some("mygame_".into()),
            ..Default::default()
        });
        assert_eq!(resolve_remote_name(&cfg, &k, "deploy"), "mygame_deploy");
    }

    #[test]
    fn resolve_remote_name_prefix_combines_with_explicit_name() {
        let mut k = key_with("universe:read");
        k.name = Some("Deploy Bot".into());
        let cfg = cfg_with_settings(Settings {
            default_enabled: true,
            name_prefix: Some("PROD".into()),
            ..Default::default()
        });
        assert_eq!(resolve_remote_name(&cfg, &k, "deploy"), "PRODDeploy Bot");
    }

    #[test]
    fn resolve_secret_file_returns_none_when_nothing_set() {
        let k = key_with("universe:read");
        let cfg = cfg_with_settings(Settings {
            default_enabled: true,
            ..Default::default()
        });
        assert_eq!(resolve_secret_file(&cfg, &k, "deploy"), None);
    }

    #[test]
    fn resolve_secret_file_substitutes_name_in_template() {
        let k = key_with("universe:read");
        let cfg = cfg_with_settings(Settings {
            default_enabled: true,
            default_secret_file: Some(".secrets/{name}.env".into()),
            ..Default::default()
        });
        assert_eq!(
            resolve_secret_file(&cfg, &k, "deploy"),
            Some(".secrets/deploy.env".into())
        );
    }

    #[test]
    fn resolve_secret_file_explicit_wins_over_template() {
        let mut k = key_with("universe:read");
        k.secret_file = Some("/explicit/path.env".into());
        let cfg = cfg_with_settings(Settings {
            default_enabled: true,
            default_secret_file: Some(".secrets/{name}.env".into()),
            ..Default::default()
        });
        assert_eq!(
            resolve_secret_file(&cfg, &k, "deploy"),
            Some("/explicit/path.env".into())
        );
    }

    #[test]
    fn allowed_cidrs_falls_back_to_defaults() {
        let k = key_with("universe:read");
        let cfg = Config {
            settings: Settings {
                default_allowed_cidrs: vec!["1.1.1.1/32".into()],
                default_enabled: true,
                ..Default::default()
            },
            keys: BTreeMap::new(),
        };
        assert_eq!(get_allowed_cidrs(&cfg, &k), vec!["1.1.1.1/32".to_string()]);
    }

    #[test]
    fn effective_envs_returns_key_envs_when_set() {
        let mut k = key_with("universe:read");
        k.envs = vec!["dev".into(), "prod".into()];
        let cfg = cfg_with_settings(Settings {
            default_enabled: true,
            default_envs: vec!["fallback".into()],
            ..Default::default()
        });
        assert_eq!(
            effective_envs(&cfg, &k),
            vec!["dev".to_string(), "prod".to_string()]
        );
    }

    #[test]
    fn effective_envs_falls_back_to_default_envs() {
        let k = key_with("universe:read");
        let cfg = cfg_with_settings(Settings {
            default_enabled: true,
            default_envs: vec!["fallback".into(), "alt".into()],
            ..Default::default()
        });
        assert_eq!(
            effective_envs(&cfg, &k),
            vec!["fallback".to_string(), "alt".to_string()]
        );
    }

    #[test]
    fn effective_envs_empty_when_neither_set() {
        let k = key_with("universe:read");
        let cfg = cfg_with_settings(Settings {
            default_enabled: true,
            ..Default::default()
        });
        assert!(effective_envs(&cfg, &k).is_empty());
    }

    /// The same entry written three times used to become three
    /// identical `ScopeDef`s in the request body.
    #[test]
    fn a_repeated_scope_entry_is_sent_once() {
        let cfg = load_text(
            "dup_entry",
            "[keys.deploy]\nscopes = [\"asset:read\", \"asset:read\", \"asset:read\"]\n",
        )
        .unwrap();
        let scopes = &cfg.keys["deploy"].scopes;
        assert_eq!(scopes.len(), 1, "{scopes:?}");
        assert_eq!(scopes[0].operations, vec!["read"]);
    }

    /// The other level: `asset:read,read,read` used to reach Roblox
    /// as one entry whose `operations` array repeated `read` three times.
    #[test]
    fn a_repeated_operation_is_sent_once() {
        let cfg = load_text(
            "dup_op",
            "[keys.deploy]\nscopes = [\"asset:read,read,write,read\"]\n",
        )
        .unwrap();
        let scopes = &cfg.keys["deploy"].scopes;
        assert_eq!(scopes.len(), 1);
        assert_eq!(scopes[0].operations, vec!["read", "write"]);
    }

    /// Order is first-seen, not sorted: the file says what it says, and a
    /// reader comparing the config against `apikey introspect` should not have
    /// to account for a reordering this tool did on the way out.
    #[test]
    fn collapsing_preserves_the_order_the_file_wrote() {
        let cfg = load_text(
            "dup_order",
            "[keys.deploy]\nscopes = [\"universe:write,read\", \"asset:read\", \"universe:read,write\"]\n",
        )
        .unwrap();
        let scopes = &cfg.keys["deploy"].scopes;
        assert_eq!(scopes.len(), 2, "{scopes:?}");
        assert_eq!(scopes[0].scope_type, "universe");
        assert_eq!(scopes[0].operations, vec!["write", "read"]);
        assert_eq!(scopes[1].scope_type, "asset");
    }

    /// Two entries of one scope type listing different operations are not the
    /// same entry, and merging them into their union is a normalisation rather
    /// than a deduplication, and deduplication stops short of it deliberately.
    #[test]
    fn one_scope_type_with_two_operation_sets_is_left_alone() {
        let cfg = load_text(
            "dup_distinct",
            "[keys.deploy]\nscopes = [\"asset:read\", \"asset:write\"]\n",
        )
        .unwrap();
        assert_eq!(cfg.keys["deploy"].scopes.len(), 2);
    }

    /// The wording carries the whole point of not dropping silently: it has to
    /// name the table, the entry and what was taken out of it, or the reader
    /// cannot find the line to fix.
    #[test]
    fn the_collapse_warning_names_what_it_collapsed() {
        let collapsed = vec![CollapsedScope {
            table: "keys.deploy".into(),
            entry: "asset:read".into(),
            removed: "the same entry is listed 3 times, sent once".into(),
        }];
        let message = collapsed_scopes_warning(Path::new(FILE), &collapsed).expect("warns");
        assert!(message.contains("rbxapikey.toml"), "{message}");
        assert!(message.contains("keys.deploy"), "{message}");
        assert!(message.contains("asset:read"), "{message}");
        assert!(message.contains("3 times"), "{message}");
        // The reason it is a warning and not an error, in the message itself.
        assert!(message.contains("grants nothing extra"), "{message}");
    }

    #[test]
    fn nothing_collapsed_says_nothing() {
        assert!(collapsed_scopes_warning(Path::new(FILE), &[]).is_none());
    }

    // ------------------------------------------------------------------
    // Targets named by id, without rbxplace.toml
    // ------------------------------------------------------------------

    #[test]
    fn universe_and_place_ids_are_read_from_the_key() {
        let cfg = load_text(
            "ids",
            "[keys.deploy]\nscopes = [\"universe:read\"]\n\
             universe_ids = [1234567890]\nplace_ids = [109876543210987]\n",
        )
        .unwrap();
        let key = &cfg.keys["deploy"];
        assert_eq!(key.universe_ids, vec![1234567890]);
        assert_eq!(key.place_ids, vec![109876543210987]);
        assert!(key.envs.is_empty());
    }

    /// `default_envs` is a fallback for a key that names no target. A key
    /// naming ids does, so the fallback would add an env it never asked for,
    /// and need an `rbxplace.toml` to resolve it.
    #[test]
    fn ids_turn_the_default_envs_off() {
        let cfg = cfg_with_settings(Settings {
            default_enabled: true,
            default_envs: vec!["fallback".into()],
            ..Default::default()
        });
        let mut k = key_with("universe:read");
        k.universe_ids = vec![1234567890];
        assert!(effective_envs(&cfg, &k).is_empty());

        let mut k = key_with("universe:read");
        k.place_ids = vec![109876543210987];
        assert!(effective_envs(&cfg, &k).is_empty());

        // Named envs beside ids are kept: both are targets.
        k.envs = vec!["dev".into()];
        assert_eq!(effective_envs(&cfg, &k), vec!["dev".to_string()]);
    }

    #[test]
    fn ids_beside_a_table_of_env_groups_are_refused() {
        let error = load_text(
            "ids-groups",
            "[keys.deploy]\nscopes = [\"universe:read\"]\nuniverse_ids = [1234567890]\n\n\
             [keys.deploy.envs]\nci = [\"dev\"]\nprod = [\"prod\"]\n",
        )
        .unwrap_err();
        let error = format!("{error:#}");
        assert!(error.contains("table of env groups"), "{error}");
    }

    // ------------------------------------------------------------------
    // Per-env key fan-out
    // ------------------------------------------------------------------

    const FANOUT: &str = "\
[keys.deploy]
scopes = [\"universe-places:write\"]

[keys.deploy.envs]
ci = [\"dev\", \"staging\"]
prod = [\"prod\"]
";

    #[test]
    fn a_table_of_env_groups_becomes_one_key_per_group() {
        let cfg = load_text("fanout", FANOUT).unwrap();
        assert_eq!(
            cfg.keys.keys().collect::<Vec<_>>(),
            vec!["deploy_ci", "deploy_prod"]
        );
        assert_eq!(cfg.keys["deploy_ci"].envs, vec!["dev", "staging"]);
        assert_eq!(cfg.keys["deploy_prod"].envs, vec!["prod"]);
        assert_eq!(cfg.keys["deploy_ci"].env_group.as_deref(), Some("ci"));
    }

    /// The reason the feature exists: the scopes are written once, so they
    /// cannot be added to one environment's key and forgotten on another's.
    #[test]
    fn every_generated_key_carries_the_declarations_scopes() {
        let cfg = load_text("fanout_scopes", FANOUT).unwrap();
        let ci = &cfg.keys["deploy_ci"].scopes;
        let prod = &cfg.keys["deploy_prod"].scopes;
        assert_eq!(ci.len(), 1);
        assert_eq!(ci[0].scope_type, "universe-places");
        assert_eq!(ci[0].operations, vec!["write"]);
        assert_eq!(prod[0].scope_type, ci[0].scope_type);
        assert_eq!(prod[0].operations, ci[0].operations);
    }

    /// The array form is what it always was: one key, spanning every env it
    /// lists. A read-only observability key across three envs is legitimate,
    /// and turning every declaration into per-env keys would have broken it.
    #[test]
    fn the_array_form_still_declares_exactly_one_key() {
        let cfg = load_text(
            "shared",
            "[keys.ops]\nenvs = [\"dev\", \"staging\", \"prod\"]\nscopes = [\"universe:read\"]\n",
        )
        .unwrap();
        assert_eq!(cfg.keys.len(), 1);
        assert_eq!(cfg.keys["ops"].envs.len(), 3);
        assert!(cfg.keys["ops"].env_group.is_none());
    }

    /// A group with no envs would be a key targeting no universe, which
    /// `scope_builder` renders as the wildcard: the widest key in the file,
    /// from the syntax meant to narrow one.
    #[test]
    fn an_empty_group_is_refused() {
        let err = load_text(
            "empty_group",
            "[keys.deploy]\nscopes = [\"universe:read\"]\n\n[keys.deploy.envs]\nci = []\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("\"ci\""), "{err}");
        assert!(err.contains("no envs"), "{err}");
    }

    #[test]
    fn a_table_naming_no_group_is_refused() {
        let err = load_text(
            "no_groups",
            "[keys.deploy]\nscopes = [\"universe:read\"]\n\n[keys.deploy.envs]\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no groups"), "{err}");
    }

    #[test]
    fn a_group_name_that_would_need_escaping_is_refused() {
        let err = load_text(
            "bad_group",
            "[keys.deploy]\nscopes = [\"universe:read\"]\n\n[keys.deploy.envs]\n\"eu/west\" = [\"prod\"]\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("eu/west"), "{err}");

        // TOML allows an empty key, and `deploy_` is nobody's idea of a name.
        let err = load_text(
            "empty_group_name",
            "[keys.deploy]\nscopes = [\"universe:read\"]\n\n[keys.deploy.envs]\n\"\" = [\"prod\"]\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("empty name"), "{err}");
    }

    /// A generated name and a hand-written one landing on the same key is an
    /// identity collision, and the lockfile, the secret file and Roblox would
    /// each silently take whichever won.
    #[test]
    fn a_generated_name_colliding_with_a_declared_one_is_refused() {
        let err = load_text(
            "collision",
            "[keys.deploy]\nscopes = [\"universe:read\"]\n\n[keys.deploy.envs]\nci = [\"dev\"]\n\n\
             [keys.deploy_ci]\nscopes = [\"universe:read\"]\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("deploy_ci"), "{err}");
        assert!(err.contains("group \"ci\""), "{err}");
    }

    /// The default template already separates the generated keys, because
    /// `{name}` is the generated name. This is the property that let the
    /// placeholder keep its meaning.
    #[test]
    fn generated_keys_get_one_secret_file_each_under_the_existing_template() {
        let cfg = load_text("fanout_secret", FANOUT).unwrap();
        let mut cfg = cfg;
        cfg.settings.default_secret_file = Some(".secrets/{name}.env".into());
        assert_eq!(
            resolve_secret_file(&cfg, &cfg.keys["deploy_ci"], "deploy_ci"),
            Some(".secrets/deploy_ci.env".into())
        );
        assert_eq!(
            resolve_secret_file(&cfg, &cfg.keys["deploy_prod"], "deploy_prod"),
            Some(".secrets/deploy_prod.env".into())
        );
    }

    #[test]
    fn env_group_substitutes_into_a_secret_path() {
        let cfg = load_text("fanout_group_path", FANOUT).unwrap();
        let mut cfg = cfg;
        cfg.settings.default_secret_file = Some(".secrets/{env_group}/deploy.env".into());
        assert_eq!(
            resolve_secret_file(&cfg, &cfg.keys["deploy_ci"], "deploy_ci"),
            Some(".secrets/ci/deploy.env".into())
        );
    }

    /// One hand-written path shared by every group would have the last
    /// `create` overwrite the secret of the key created before it, leaving a
    /// live key nothing local can authenticate as.
    #[test]
    fn siblings_sharing_one_secret_file_are_refused_at_load() {
        let err = load_text(
            "sibling_secret",
            "[keys.deploy]\nscopes = [\"universe:read\"]\nsecret_file = \".secrets/deploy.env\"\n\n\
             [keys.deploy.envs]\nci = [\"dev\"]\nprod = [\"prod\"]\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains(".secrets/deploy.env"), "{err}");
        assert!(err.contains("{env_group}"), "{err}");
    }

    #[test]
    fn an_explicit_secret_file_with_a_placeholder_separates_the_siblings() {
        let cfg = load_text(
            "sibling_secret_ok",
            "[keys.deploy]\nscopes = [\"universe:read\"]\nsecret_file = \".secrets/{env_group}.env\"\n\n\
             [keys.deploy.envs]\nci = [\"dev\"]\nprod = [\"prod\"]\n",
        )
        .unwrap();
        assert_eq!(
            resolve_secret_file(&cfg, &cfg.keys["deploy_prod"], "deploy_prod"),
            Some(".secrets/prod.env".into())
        );
    }

    /// Roblox stores the display name, and two keys called `deploy` in one
    /// Creator Hub are two keys nobody can tell apart.
    #[test]
    fn a_generated_key_is_named_after_its_group_on_roblox() {
        let mut cfg = load_text("fanout_name", FANOUT).unwrap();
        cfg.settings.name_prefix = Some("mygame_".into());
        assert_eq!(
            resolve_remote_name(&cfg, &cfg.keys["deploy_ci"], "deploy_ci"),
            "mygame_deploy_ci"
        );
    }

    /// With an explicit `name` the group has to be appended, since the TOML key
    /// is no longer what reaches Roblox. Without one it must not be, or the
    /// group would land twice.
    /// The two forms are alternatives, and serde's own way of saying so names
    /// the Rust enum in a message about a TOML file. What the reader needs is
    /// the line, which the TOML parser gives, and what the field takes, which
    /// the visitor gives.
    #[test]
    fn a_mistyped_envs_field_says_what_the_field_takes() {
        let err = format!(
            "{:?}",
            load_text(
                "bad_envs",
                "[keys.deploy]\nenvs = 5\nscopes = [\"universe:read\"]\n",
            )
            .unwrap_err()
        );
        assert!(err.contains("line 2"), "{err}");
        assert!(
            err.contains("an array of env names, or a table of named env groups"),
            "{err}"
        );
        assert!(!err.contains("untagged"), "{err}");
    }

    #[test]
    fn an_explicit_display_name_gains_the_group_exactly_once() {
        let cfg = load_text("fanout_explicit_name", FANOUT).unwrap();
        let mut key = cfg.keys["deploy_ci"].clone();
        assert_eq!(resolve_remote_name(&cfg, &key, "deploy_ci"), "deploy_ci");
        key.name = Some("Deploy Bot".into());
        assert_eq!(
            resolve_remote_name(&cfg, &key, "deploy_ci"),
            "Deploy Bot_ci"
        );
    }

    /// The whole point: a file that says it holds no write scopes now refuses
    /// one instead of describing the rule in a comment.
    #[test]
    fn a_readonly_file_refuses_a_write_scope() {
        let err = load_text(
            "ro_file",
            "[settings]
readonly = true

[keys.viewer]
envs = [\"prod\"]
scopes = [\"universe:read,write\"]
",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("readonly"), "got: {err}");
        assert!(
            err.contains("universe:write"),
            "names the offending scope: {err}"
        );
    }

    #[test]
    fn a_readonly_file_accepts_reads_and_lists() {
        let cfg = load_text(
            "ro_ok",
            "[settings]
readonly = true

[keys.viewer]
envs = [\"prod\"]
scopes = [\"universe:read\", \"universe-datastores.objects:read,list\"]
",
        )
        .unwrap();
        assert_eq!(cfg.keys.len(), 1);
    }

    /// Per-key, for a file that is not read-only as a whole.
    #[test]
    fn one_key_can_be_readonly_on_its_own() {
        let err = load_text(
            "ro_key",
            "[keys.viewer]
readonly = true
envs = [\"prod\"]
scopes = [\"asset:read\", \"asset:write\"]
",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("[keys.viewer]"), "got: {err}");

        // and the same scopes are fine without the flag
        assert!(load_text(
            "ro_key_off",
            "[keys.viewer]
envs = [\"prod\"]
scopes = [\"asset:read\", \"asset:write\"]
",
        )
        .is_ok());
    }

    /// An allow-list, not a deny-list. Roblox adds operations whenever it
    /// likes, and a deny-list would let each new one through, which is the
    /// failure this guard exists to close, so it must not have it too.
    #[test]
    fn an_operation_nobody_has_heard_of_is_refused_rather_than_allowed() {
        let err = load_text(
            "ro_unknown",
            "[settings]
readonly = true

[keys.viewer]
envs = [\"prod\"]
scopes = [\"universe:teleport-everyone\"]
",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("teleport-everyone"), "got: {err}");
    }

    /// A fan-out names the generated sibling, not the declaration: that is the
    /// name the key would be created under, and the one to look for on Roblox.
    #[test]
    fn a_fanned_out_key_is_named_by_its_generated_name() {
        let err = load_text(
            "ro_fanout",
            "[settings]
readonly = true

[keys.viewer]
scopes = [\"universe:write\"]

[keys.viewer.envs]
ci = [\"dev\"]
",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("viewer_ci"), "got: {err}");
    }

    /// A config that cannot be looked at is reported as such, naming the
    /// file. `exists()` read it as absent, and the message then told the
    /// reader to create a file they already had.
    #[test]
    fn a_config_that_cannot_be_looked_at_is_not_reported_missing() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = crate::test_support::unlookable(dir.path());

        let err = format!("{:#}", load_from(&path).expect_err("cannot be looked at"));

        assert!(err.contains("failed to look at"), "got: {err}");
        assert!(err.contains("cannot-be-looked-at") || err.contains("cannot<be>looked|at"));
        assert!(!err.contains("not found"), "got: {err}");
    }

    /// The absent file keeps its message: that one is the reader's to fix.
    #[test]
    fn an_absent_config_is_still_reported_missing() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let err = format!(
            "{:#}",
            load_from(&dir.path().join(FILE)).expect_err("absent")
        );
        assert!(err.contains("not found"), "got: {err}");
    }
}
