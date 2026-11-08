//! What `rbx-apikey list --json`, `status --json` and `scopes show --json`
//! write to stdout.
//!
//! The envelope: a `schema_version` first, then
//! named objects all the way down, optional fields omitted rather than emitted
//! as `null`, ids as strings. Field names are documented in `docs/apikey.md`
//! and are the compatibility surface.
//!
//! ## No secret, and no piece of one
//!
//! This crate holds live Open Cloud credentials. A document it writes goes into
//! a pipe, a CI log, an artifact upload, a monitoring agent: places a secret
//! must never reach, and where nobody will notice it did until the key has to
//! be rotated. So the rule here is absolute and structural rather than
//! careful: **no field in this module ever carries a secret or any part of
//! one.**
//!
//! Two things follow from it that are worth stating, because both are fields
//! the human form does print:
//!
//! - **`list --remote` has no `secret_preview`.** The Creator Hub's "Key"
//!   column, and the human listing's fourth column, is the first characters of
//!   a live secret. It is there so a person can recognise a key on their own
//!   screen. A prefix is still credential material, and a document is not a
//!   screen.
//! - **`list` has no path to the secret file.** The human form prints
//!   `set (file: .secrets/deploy.env)` because the person reading it is
//!   standing in that directory. `secret_backend` says which of the two
//!   backends holds it and `secret_present` says whether it is there, which is
//!   what a script needs; where on disk a credential lives is not something to
//!   publish alongside it.
//!
//! ## What else is deliberately absent
//!
//! A document says no more than the human form already says out loud, and these
//! commands talk about credentials, so the line is drawn tightly.
//!
//! `status` carries no free-text detail. The human form's trailing sentence is
//! advice for a person ("run `rbx-apikey regenerate <name>`") and one of its
//! branches names the secret file. `status` is derivable from the same
//! verdicts, so the document carries the verdict and drops the sentence.
//!
//! `list --remote` carries no `id`. The account listing prints a name, a
//! preview, dates and a tracked tag, and never the `cloud_auth_id`; most of the
//! keys it returns belong to other checkouts and other tools. The local
//! listing does print `id` for the keys this project created, and the document
//! carries it there.

use serde::Serialize;

use rbx_core::output::SCHEMA_VERSION;

use crate::lock::LockEntry;
use crate::remote_view::{self, RemoteKey};
use crate::scope_catalog::Lookup;
use crate::secret_store::Backend;
use crate::time_iso;

/// Days until `iso`, when there is a timestamp and it parses.
///
/// The human form prints `<no expiry>` for the first case and
/// `<iso> (unparseable)` for the second; the document omits the key for both,
/// because "no expiry" and "a date we could not read" are not a number.
fn days_until(iso: Option<&str>) -> Option<i64> {
    time_iso::days_until(expiry(iso)?)
}

/// The expiry timestamp, treating an empty string as no timestamp: the same
/// reading `expiry_line` applies.
fn expiry(iso: Option<&str>) -> Option<&str> {
    iso.filter(|value| !value.is_empty())
}

/// One `rbx-apikey list` invocation, without `--remote`.
///
/// What this project declares, joined to what it created: `rbxapikey.toml` and
/// the lockfile, and nothing from Roblox. `list --remote` answers a different
/// question and writes a different document; the flag is what picks between
/// them, never the data.
#[derive(Debug, Serialize)]
pub struct ListDocument {
    pub schema_version: u32,
    /// `name` or `expiry`: the order `keys` is in, which is what `--sort`
    /// decides. Carried because the order of an array is meaningful and a
    /// stored document should say which one it is.
    pub sort: String,
    /// Entries in `keys`.
    pub count: usize,
    /// One object per name found in either file, in the sorted order.
    pub keys: Vec<KeyEntry>,
}

/// One key, from this project's point of view.
///
/// `declared` and `created` are the two tags the human listing prints, kept as
/// separate booleans rather than folded into one word: a key in
/// `rbxapikey.toml` with no lockfile entry is pending, a lockfile entry with no
/// declaration is an orphan, and they are fixed in opposite directions.
#[derive(Debug, Serialize)]
pub struct KeyEntry {
    /// The `[keys.<name>]` key, which is what every subcommand takes.
    pub name: String,
    /// True when `rbxapikey.toml` has this key. False is the orphan the human
    /// listing marks `← not in rbxapikey.toml`.
    pub declared: bool,
    /// True when the lockfile has it, which is to say the key exists on Roblox.
    /// False is the `(not created)` the human listing prints.
    pub created: bool,
    /// Roblox's `cloud_auth_id`, the id the human listing prints as `id:`.
    /// **Absent** when the key has not been created. Not a credential: it names
    /// the key, and using it still needs the account's cookie.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The account that created the key. **Absent** when it has not been.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creator_id: Option<String>,
    /// The universes this key's envs resolve to, through the lockfile's
    /// `[envs.<name>]` table. Empty when none of them are synced yet, which is
    /// the case the human listing prints no `universes:` line for.
    pub universe_ids: Vec<String>,
    /// As Roblox stored it, full precision. The human listing shortens it to
    /// the date. **Absent** for a key that never expires, and for one that was
    /// never created.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// Negative once it has passed. **Absent** when there is no expiry, and
    /// when the timestamp could not be parsed: the case the human listing
    /// marks `(unparseable)`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub days_until_expiry: Option<i64>,
    /// Whether the secret is where it is meant to be. **Absent** for a key that
    /// was never created. Never the secret, and never a piece of it: see the
    /// module docs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret_present: Option<bool>,
    /// `lockfile` or `file`, whichever holds the secret for this key.
    /// **Absent** for a key that was never created. The path is not part of
    /// the document even when the backend is `file`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret_backend: Option<String>,
}

impl ListDocument {
    pub fn new(sort: &str) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            sort: sort.to_string(),
            count: 0,
            keys: Vec::new(),
        }
    }

    pub fn push(&mut self, entry: KeyEntry) {
        self.keys.push(entry);
        self.count = self.keys.len();
    }
}

impl KeyEntry {
    /// A key `rbxapikey.toml` declares and nothing has created yet.
    pub fn pending(name: &str) -> Self {
        Self {
            name: name.to_string(),
            declared: true,
            created: false,
            id: None,
            creator_id: None,
            universe_ids: Vec::new(),
            expires_at: None,
            days_until_expiry: None,
            secret_present: None,
            secret_backend: None,
        }
    }

    /// A key the lockfile tracks. `declared` is false for an orphan: a lockfile
    /// entry whose `[keys.<name>]` block has since been deleted.
    ///
    /// `pub(crate)`, unlike the type: `LockEntry` and `Backend` live in
    /// crate-private modules, and a public constructor taking them would leak
    /// them.
    pub(crate) fn created(
        name: &str,
        declared: bool,
        entry: &LockEntry,
        universe_ids: &[u64],
        secret_present: bool,
        backend: &Backend,
    ) -> Self {
        Self {
            name: name.to_string(),
            declared,
            created: true,
            id: Some(entry.cloud_auth_id.clone()),
            creator_id: Some(entry.creator_id.to_string()),
            universe_ids: universe_ids.iter().map(u64::to_string).collect(),
            expires_at: expiry(entry.expires_at.as_deref()).map(str::to_string),
            days_until_expiry: days_until(entry.expires_at.as_deref()),
            secret_present: Some(secret_present),
            secret_backend: Some(backend.as_str().to_string()),
        }
    }
}

/// One `rbx-apikey list --remote` invocation.
///
/// What the *account* holds, which is mostly not this project's doing: most of
/// what comes back was made by other checkouts, other tools, or by hand in the
/// Creator Hub. So every entry carries its verdict, and the document names the
/// account it is a listing of: switching the signed-in Studio account changes that silently and the same key names recur
/// across accounts.
#[derive(Debug, Serialize)]
pub struct RemoteListDocument {
    pub schema_version: u32,
    /// Whose keys these are.
    pub owner: Owner,
    pub totals: Totals,
    /// One object per key on the account, newest first: the order Roblox
    /// returns them and the order the human listing prints.
    pub keys: Vec<RemoteKeyEntry>,
    /// Lockfile names with no counterpart on the account: keys this project
    /// still tracks and Roblox no longer has. The same list the human form
    /// prints as a warning, which under `--json` is on stderr.
    pub missing_on_account: Vec<String>,
}

/// The account a listing was taken from.
#[derive(Debug, Serialize)]
pub struct Owner {
    /// `user` or `group`. `--group-id` is what makes it the second.
    pub kind: String,
    pub id: String,
}

/// The counts the human form prints as its summary line.
#[derive(Debug, Serialize)]
pub struct Totals {
    pub total: usize,
    /// Keys this project's lockfile claims, joined on `cloud_auth_id` and never
    /// on the name.
    pub tracked: usize,
    pub untracked: usize,
    pub expired: usize,
    pub disabled: usize,
}

/// One key as the account has it.
///
/// No secret preview and no id. See the module docs: the first is credential
/// material and the second is not something the human listing prints.
#[derive(Debug, Serialize)]
pub struct RemoteKeyEntry {
    /// The name on Roblox, which is not necessarily the lockfile's name for it:
    /// `name_prefix` makes `viewer` into `prodread_viewer` on purpose.
    pub name: String,
    /// `active`, `expired` or `disabled`, the one-word state the human listing
    /// paints as a glyph.
    pub state: String,
    /// Whether this project's lockfile claims this key.
    pub tracked: bool,
    /// The lockfile's name for it. **Absent** when `tracked` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tracked_as: Option<String>,
    /// As Roblox sent it. The human listing shortens it to the date.
    /// **Absent** when Roblox sent none, which the listing prints as `?`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_time: Option<String>,
    /// **Absent** for a key that never expires.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiration_time: Option<String>,
    /// Negative once it has passed. **Absent** when there is no expiry, and
    /// when the timestamp could not be parsed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub days_until_expiry: Option<i64>,
}

impl RemoteListDocument {
    /// `pub(crate)` for the same reason as `KeyEntry::created`: `RemoteKey`
    /// lives in a crate-private module.
    pub(crate) fn new(
        group_id: Option<u64>,
        user_id: u64,
        keys: &[RemoteKey],
        missing_on_account: Vec<String>,
    ) -> Self {
        let tally = remote_view::tally(keys);
        Self {
            schema_version: SCHEMA_VERSION,
            owner: match group_id {
                Some(group) => Owner {
                    kind: "group".to_string(),
                    id: group.to_string(),
                },
                None => Owner {
                    kind: "user".to_string(),
                    id: user_id.to_string(),
                },
            },
            totals: Totals {
                total: tally.total,
                tracked: tally.tracked,
                untracked: tally.untracked,
                expired: tally.expired,
                disabled: tally.disabled,
            },
            keys: keys.iter().map(RemoteKeyEntry::new).collect(),
            missing_on_account,
        }
    }
}

impl RemoteKeyEntry {
    pub(crate) fn new(key: &RemoteKey) -> Self {
        Self {
            name: key.name().to_string(),
            state: key.state().label().to_string(),
            tracked: key.tracked.is_tracked(),
            tracked_as: match &key.tracked {
                remote_view::Tracked::Yes(name) => Some(name.clone()),
                remote_view::Tracked::No => None,
            },
            created_time: expiry(key.info.created_time.as_deref()).map(str::to_string),
            expiration_time: expiry(key.info.expiration_time()).map(str::to_string),
            days_until_expiry: key.days_left(),
        }
    }
}

/// One `rbx-apikey status` invocation.
///
/// The reconciliation between `rbxapikey.toml`, the lockfile and (under
/// `--remote`) Roblox. One verdict per key, and the counts the human form
/// prints as its summary.
#[derive(Debug, Serialize)]
pub struct StatusDocument {
    pub schema_version: u32,
    /// Whether Roblox was asked about each key. Without it `ORPHAN_REMOTE`
    /// cannot be reported at all, so a consumer that sees no orphan needs to
    /// know which of the two runs it is reading.
    pub remote: bool,
    /// Entries in `keys`.
    pub count: usize,
    /// How many of them are not `HEALTHY`. The same number the human form
    /// prints as "N key(s) need attention".
    pub issues: usize,
    /// One object per name found in either file, in alphabetical order.
    pub keys: Vec<StatusEntry>,
}

/// One key's verdict.
#[derive(Debug, Serialize)]
pub struct StatusEntry {
    pub name: String,
    /// `HEALTHY`, `PENDING`, `EXPIRED`, `EXPIRING_SOON`, `ORPHAN_LOCK`,
    /// `ORPHAN_REMOTE`, `SECRET_MISSING`, `DISABLED` or `CHECK_FAILED`: the
    /// same word the human form prints beside the glyph.
    pub status: String,
    /// True only for `HEALTHY`, so a consumer can gate on one field without
    /// having to enumerate eight spellings of "no".
    pub healthy: bool,
    /// Negative once it has passed. **Absent** when there is no expiry, when
    /// the timestamp could not be parsed, and for a key with no lockfile entry
    /// to have an expiry in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub days_until_expiry: Option<i64>,
}

impl StatusDocument {
    pub fn new(remote: bool) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            remote,
            count: 0,
            issues: 0,
            keys: Vec::new(),
        }
    }

    /// Record one verdict, as the walk reaches it.
    pub fn push(&mut self, name: &str, status: &str, healthy: bool, days: Option<i64>) {
        self.keys.push(StatusEntry {
            name: name.to_string(),
            status: status.to_string(),
            healthy,
            days_until_expiry: days,
        });
        self.count = self.keys.len();
        self.issues = self.keys.iter().filter(|key| !key.healthy).count();
    }
}

/// One `rbx-apikey scopes show` invocation.
///
/// The catalog is advisory: an unknown scope is a warning, never an error, and
/// `rbxapikey.toml` forwards any string Roblox will take. So the document
/// answers `known` rather than failing, and carries the catalog version so a
/// consumer can tell "Roblox does not have this scope" from "this catalog is
/// older than that scope".
#[derive(Debug, Serialize)]
pub struct ScopeDocument {
    pub schema_version: u32,
    /// The scope type as it was asked for.
    pub scope_type: String,
    /// False when the bundled catalog does not list it, which is not a verdict
    /// on whether Roblox accepts it.
    pub known: bool,
    /// The bundled catalog's version, the one the human form prints in its
    /// header and in its "not in the catalog" line.
    pub catalog_version: String,
    /// `universe`, `universe-datastore`, `creator` or `none`. **Absent** when
    /// the scope is unknown, because then there is no answer rather than an
    /// empty one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_type: Option<String>,
    /// What `rbxapikey.toml` may put after the colon. **Absent** for an unknown
    /// scope, for the same reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operations: Option<Vec<String>>,
}

impl ScopeDocument {
    pub fn new(scope_type: &str, lookup: &Lookup, catalog_version: &str) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            scope_type: scope_type.to_string(),
            known: lookup.known,
            catalog_version: catalog_version.to_string(),
            target_type: lookup.target_type.clone(),
            operations: lookup.known_operations.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::api_keys::{RemoteApiKey, RemoteProperties};
    use crate::lock;
    use crate::remote_view::Tracked;

    fn parsed(document: &impl Serialize) -> serde_json::Value {
        let mut buf = Vec::new();
        rbx_core::output::write_json(&mut buf, document).expect("write");
        serde_json::from_slice(&buf).expect("the document must be valid JSON")
    }

    fn entry(expires_at: Option<&str>) -> LockEntry {
        LockEntry {
            cloud_auth_id: "f58b4055-cafe-4e2f-9c2a-000000000001".into(),
            // A real secret in the fixture, so the leak assertions below have
            // something to find if a field ever starts carrying one.
            secret: Some("RBX-S3CR3T-must-never-be-serialised".into()),
            secret_file: Some("/home/dev/.secrets/deploy.env".into()),
            creator_id: 1_234_567_890,
            is_enabled: true,
            created_at: "2026-01-01T00:00:00.000Z".into(),
            expires_at: expires_at.map(str::to_string),
        }
    }

    fn remote_key(name: &str, tracked: Tracked) -> RemoteKey {
        RemoteKey {
            info: RemoteApiKey {
                id: "f58b4055-cafe-4e2f-9c2a-000000000001".into(),
                created_time: Some("2026-08-03T09:15:00.123Z".into()),
                apikey_secret_preview: Some("RBX-S3CR3T".into()),
                cloud_auth_user_configured_properties: Some(RemoteProperties {
                    name: name.to_string(),
                    is_enabled: true,
                    ..Default::default()
                }),
                ..Default::default()
            },
            tracked,
        }
    }

    #[test]
    fn a_listing_carries_the_documented_fields() {
        let mut doc = ListDocument::new("name");
        doc.push(KeyEntry::created(
            "deploy",
            true,
            &entry(Some("2026-11-03T00:00:00.000Z")),
            &[5_544_332_211, 66_778_899_001],
            true,
            &Backend::Lockfile,
        ));
        let doc = parsed(&doc);

        assert_eq!(doc["schema_version"], SCHEMA_VERSION);
        assert_eq!(doc["sort"], "name");
        assert_eq!(doc["count"], 1);
        assert_eq!(doc["keys"][0]["name"], "deploy");
        assert_eq!(doc["keys"][0]["declared"], true);
        assert_eq!(doc["keys"][0]["created"], true);
        assert_eq!(doc["keys"][0]["id"], "f58b4055-cafe-4e2f-9c2a-000000000001");
        assert_eq!(doc["keys"][0]["creator_id"], "1234567890");
        assert_eq!(doc["keys"][0]["universe_ids"][0], "5544332211");
        assert_eq!(doc["keys"][0]["universe_ids"][1], "66778899001");
        // Full precision, where the human listing shortens to the date.
        assert_eq!(doc["keys"][0]["expires_at"], "2026-11-03T00:00:00.000Z");
        assert_eq!(doc["keys"][0]["secret_present"], true);
        assert_eq!(doc["keys"][0]["secret_backend"], "lockfile");
    }

    /// The rule the module exists for. The fixture holds a secret and a path to
    /// one; neither may appear anywhere in the document, in whole or in part.
    #[test]
    fn no_secret_and_no_path_to_one_reaches_the_listing() {
        let mut doc = ListDocument::new("name");
        doc.push(KeyEntry::created(
            "deploy",
            true,
            &entry(None),
            &[1],
            true,
            &Backend::File,
        ));
        let rendered = parsed(&doc).to_string();

        for absent in [
            "RBX-S3CR3T",
            "RBX-",
            "S3CR3T",
            "secret\":",
            ".secrets",
            "deploy.env",
            "/home/dev",
        ] {
            assert!(!rendered.contains(absent), "{absent} leaked: {rendered}");
        }
        // The backend is named; where it keeps the file is not.
        let doc = parsed(&doc);
        assert_eq!(doc["keys"][0]["secret_backend"], "file");
        assert_eq!(doc["keys"][0]["secret_present"], true);
    }

    /// A declared key nothing has created yet: the `(not created)` row. Every
    /// field that would have come from the lockfile is absent rather than
    /// zeroed, because there is no entry to read it from.
    #[test]
    fn a_pending_key_omits_everything_the_lockfile_would_have_said() {
        let mut doc = ListDocument::new("expiry");
        doc.push(KeyEntry::pending("newkey"));
        let doc = parsed(&doc);

        assert_eq!(doc["keys"][0]["declared"], true);
        assert_eq!(doc["keys"][0]["created"], false);
        for absent in [
            "id",
            "creator_id",
            "expires_at",
            "days_until_expiry",
            "secret_present",
            "secret_backend",
        ] {
            assert!(doc["keys"][0].get(absent).is_none(), "{absent}: {doc}");
        }
        assert_eq!(
            doc["keys"][0]["universe_ids"].as_array().map(Vec::len),
            Some(0)
        );
    }

    /// The orphan, and its opposite. Two booleans rather than one word, because
    /// the two states are fixed in opposite directions.
    #[test]
    fn an_orphan_is_created_but_not_declared() {
        let mut doc = ListDocument::new("name");
        doc.push(KeyEntry::created(
            "leftover",
            false,
            &entry(None),
            &[],
            false,
            &Backend::Lockfile,
        ));
        let doc = parsed(&doc);

        assert_eq!(doc["keys"][0]["declared"], false);
        assert_eq!(doc["keys"][0]["created"], true);
        assert_eq!(doc["keys"][0]["secret_present"], false);
    }

    /// No expiry and an unreadable expiry are both "not a number of days", and
    /// neither is invented into one.
    #[test]
    fn an_absent_or_unparseable_expiry_omits_the_day_count() {
        for expires in [None, Some(""), Some("whenever")] {
            let key = KeyEntry::created("k", true, &entry(expires), &[], true, &Backend::Lockfile);
            let doc = parsed(&key);
            assert!(doc.get("days_until_expiry").is_none(), "{expires:?}: {doc}");
        }
        // The unparseable one still says what Roblox stored, which is how a
        // human finds out why the day count is missing.
        let key = KeyEntry::created(
            "k",
            true,
            &entry(Some("whenever")),
            &[],
            true,
            &Backend::File,
        );
        assert_eq!(parsed(&key)["expires_at"], "whenever");
    }

    #[test]
    fn an_empty_project_is_still_a_document() {
        let doc = parsed(&ListDocument::new("name"));

        assert_eq!(doc["count"], 0);
        assert_eq!(doc["keys"].as_array().map(Vec::len), Some(0));
    }

    #[test]
    fn a_remote_listing_carries_the_account_and_the_tally() {
        let keys = [
            remote_key("prodread_viewer", Tracked::Yes("viewer".into())),
            remote_key("otherproject_shop", Tracked::No),
        ];
        let doc = parsed(&RemoteListDocument::new(
            None,
            1_234_567_890,
            &keys,
            vec!["gone".to_string()],
        ));

        assert_eq!(doc["schema_version"], SCHEMA_VERSION);
        assert_eq!(doc["owner"]["kind"], "user");
        assert_eq!(doc["owner"]["id"], "1234567890");
        assert_eq!(doc["totals"]["total"], 2);
        assert_eq!(doc["totals"]["tracked"], 1);
        assert_eq!(doc["totals"]["untracked"], 1);
        assert_eq!(doc["keys"][0]["name"], "prodread_viewer");
        assert_eq!(doc["keys"][0]["state"], "active");
        assert_eq!(doc["keys"][0]["tracked"], true);
        // The lockfile's name for it, which is not the name on Roblox.
        assert_eq!(doc["keys"][0]["tracked_as"], "viewer");
        assert_eq!(doc["keys"][0]["created_time"], "2026-08-03T09:15:00.123Z");
        assert_eq!(doc["keys"][1]["tracked"], false);
        assert!(doc["keys"][1].get("tracked_as").is_none(), "{doc}");
        assert_eq!(doc["missing_on_account"][0], "gone");
    }

    /// The Creator Hub's "Key" column is the first characters of a live secret.
    /// It is on every entry the account listing returns, and on none of the
    /// entries this document writes.
    #[test]
    fn the_secret_preview_never_reaches_the_remote_document() {
        let keys = [remote_key("prodread_viewer", Tracked::No)];
        let rendered = parsed(&RemoteListDocument::new(None, 1, &keys, Vec::new())).to_string();

        for absent in ["RBX-S3CR3T", "S3CR3T", "preview", "secret"] {
            assert!(!rendered.contains(absent), "{absent} leaked: {rendered}");
        }
    }

    /// `--group-id` changes whose keys these are, and the document says so
    /// rather than leaving a stored listing ambiguous.
    #[test]
    fn a_group_listing_names_the_group() {
        let doc = parsed(&RemoteListDocument::new(
            Some(445_566_778),
            1_234_567_890,
            &[],
            Vec::new(),
        ));

        assert_eq!(doc["owner"]["kind"], "group");
        assert_eq!(doc["owner"]["id"], "445566778");
        assert_eq!(doc["totals"]["total"], 0);
    }

    #[test]
    fn a_status_document_carries_one_verdict_per_key_and_the_counts() {
        let mut doc = StatusDocument::new(false);
        doc.push("deploy", "HEALTHY", true, Some(91));
        doc.push("viewer", "EXPIRED", false, Some(-3));
        doc.push("newkey", "PENDING", false, None);
        let doc = parsed(&doc);

        assert_eq!(doc["schema_version"], SCHEMA_VERSION);
        assert_eq!(doc["remote"], false);
        assert_eq!(doc["count"], 3);
        assert_eq!(doc["issues"], 2);
        assert_eq!(doc["keys"][0]["status"], "HEALTHY");
        assert_eq!(doc["keys"][0]["healthy"], true);
        assert_eq!(doc["keys"][0]["days_until_expiry"], 91);
        assert_eq!(doc["keys"][1]["days_until_expiry"], -3);
        assert!(doc["keys"][2].get("days_until_expiry").is_none(), "{doc}");
    }

    /// `ORPHAN_REMOTE` only exists under `--remote`, so a consumer that does
    /// not see one needs to know which run it is reading.
    #[test]
    fn a_status_document_says_whether_roblox_was_asked() {
        assert_eq!(parsed(&StatusDocument::new(true))["remote"], true);
        assert_eq!(parsed(&StatusDocument::new(false))["remote"], false);
    }

    /// The advice sentence the human form prints is not in the document: one of
    /// its branches names the secret file, and the verdict says the same thing
    /// without it.
    #[test]
    fn a_status_document_carries_no_free_text_detail() {
        let mut doc = StatusDocument::new(false);
        doc.push("deploy", "SECRET_MISSING", false, None);
        let rendered = parsed(&doc).to_string();

        for absent in ["detail", "regenerate", ".secrets", "not found or empty"] {
            assert!(!rendered.contains(absent), "{absent} leaked: {rendered}");
        }
    }

    #[test]
    fn a_known_scope_carries_its_target_and_operations() {
        let doc = parsed(&ScopeDocument::new(
            "universe",
            &crate::scope_catalog::lookup("universe"),
            "2026-08-01",
        ));

        assert_eq!(doc["schema_version"], SCHEMA_VERSION);
        assert_eq!(doc["scope_type"], "universe");
        assert_eq!(doc["known"], true);
        assert_eq!(doc["catalog_version"], "2026-08-01");
        assert_eq!(doc["target_type"], "universe");
        assert!(
            doc["operations"]
                .as_array()
                .is_some_and(|ops| ops.contains(&serde_json::Value::from("read"))),
            "{doc}"
        );
    }

    /// An unknown scope is a document saying so, not an error: the catalog is
    /// advisory and `rbxapikey.toml` forwards any string. The two answer fields
    /// are absent rather than empty, because there is no answer rather than an
    /// empty one.
    #[test]
    fn an_unknown_scope_says_so_rather_than_failing() {
        let doc = parsed(&ScopeDocument::new(
            "not-a-real-scope",
            &crate::scope_catalog::lookup("not-a-real-scope"),
            "2026-08-01",
        ));

        assert_eq!(doc["known"], false);
        assert_eq!(doc["scope_type"], "not-a-real-scope");
        assert!(doc.get("target_type").is_none(), "{doc}");
        assert!(doc.get("operations").is_none(), "{doc}");
    }

    /// Nothing here reads a lockfile secret into a document, and this is the
    /// test that says so from the other end: the lock entry a listing is built
    /// from carries one, and the rendered bytes do not.
    #[test]
    fn a_lock_entry_with_a_secret_renders_without_it() {
        let mut lk = lock::Lock::default();
        lk.keys.insert("deploy".to_string(), entry(None));
        let held = lk.keys["deploy"].secret.clone().expect("fixture");

        let mut doc = ListDocument::new("name");
        doc.push(KeyEntry::created(
            "deploy",
            true,
            &lk.keys["deploy"],
            &[],
            true,
            &Backend::Lockfile,
        ));

        assert!(
            !parsed(&doc).to_string().contains(&held),
            "the secret leaked"
        );
    }

    /// `--json` owns stdout, so nothing on any of these paths may stop and ask
    /// a question. The reading subcommands have nothing to ask; every
    /// subcommand that does (`create`, `prune`, `update`, `regenerate`,
    /// `delete`) does not carry the flag at all, which `json_flag_tests` in
    /// `lib.rs` pins.
    #[test]
    fn the_json_format_refuses_to_prompt() {
        assert!(!rbx_core::output::OutputFormat::Json.may_prompt());
    }
}
