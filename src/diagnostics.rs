//! The read-only view of this crate that `rbx-apikey doctor` runs on.
//!
//! A façade rather than a set of `pub mod` promotions. `doctor` needs four
//! facts: what this project declares, which secret is readable, what Roblox
//! holds for a key, and whether a key carries a scope, and each of them is
//! currently spread over `config`, `lock`, `secret_store`, `api` and
//! `remote_view`. Reaching into those five modules for four facts would tie
//! the diagnosis to every internal detail of key management, and make the
//! next refactor inside them a change to `doctor` too. This module states the
//! four facts instead, and stays the only thing `doctor` reads through.
//!
//! Everything here is read-only. Nothing in this module writes a file, creates
//! a key, or changes anything on Roblox: `doctor` diagnoses, it does not
//! repair, and the boundary is easier to keep if the API it is offered cannot
//! do otherwise.

use anyhow::{Context, Result};

use rbx_core::GlobalFlags;

use crate::api::RbxApiKeyClient;
use crate::remote_view;
use crate::scope_builder::ScopeDef;
use crate::{config, lock, secret_store, time_iso};

/// One key as this project declares it, joined to whatever the lockfile and the
/// secret backend know about it.
#[derive(Debug, Clone)]
pub struct DeclaredKey {
    /// Name in `rbxapikey.toml`, which is also the lockfile key.
    pub name: String,
    /// `None` when the key is declared but has never been created.
    pub cloud_auth_id: Option<String>,
    /// Where the secret is stored: `lockfile`, or `file: <path>`.
    pub secret_origin: String,
    /// The secret itself, when it is actually readable from that backend. A
    /// declared key with no readable secret is the normal state after a fresh
    /// clone, not an error.
    pub secret: Option<String>,
}

impl DeclaredKey {
    pub fn is_created(&self) -> bool {
        self.cloud_auth_id.is_some()
    }
}

/// Every key in `rbxapikey.toml`, in name order.
///
/// An absent `rbxapikey.toml` yields an empty list rather than an error: a
/// directory that declares no keys is a directory `doctor` still has useful
/// things to say about.
pub fn declared_keys() -> Result<Vec<DeclaredKey>> {
    if !std::path::Path::new(config::FILE).exists() {
        return Ok(Vec::new());
    }
    let cfg = config::load()?;
    let lk = lock::load()?;

    Ok(cfg
        .keys
        .keys()
        .map(|name| {
            let key_cfg = config::get(&cfg, name);
            let entry = lock::get(&lk, name);
            let resolved = secret_store::stored_backend(&cfg, key_cfg, name, entry);
            let secret_origin = match resolved.backend {
                secret_store::Backend::Lockfile => format!("{} ({})", lock::FILE, resolved.target),
                secret_store::Backend::File => format!("file: {}", resolved.target),
                secret_store::Backend::External => resolved.target.clone(),
            };
            DeclaredKey {
                name: name.clone(),
                cloud_auth_id: entry.map(|e| e.cloud_auth_id.clone()),
                secret: secret_store::read(&resolved, entry),
                secret_origin,
            }
        })
        .collect())
}

/// Whether the config file this project uses for keys is even here.
pub fn config_file_present() -> bool {
    std::path::Path::new(config::FILE).exists()
}

/// What Roblox holds for one key.
#[derive(Debug, Clone)]
pub struct KeyFacts {
    /// The key's display name on Roblox, which is not always its name here:
    /// `settings.name_prefix` and per-key `name` both rewrite it.
    pub remote_name: String,
    /// The lockfile name, when this project tracks the key.
    pub tracked_as: Option<String>,
    pub enabled: bool,
    pub expires_at: Option<String>,
    /// Negative once the key has expired.
    pub days_left: Option<i64>,
    pub allowed_cidrs: Vec<String>,
    pub scopes: Vec<ScopeDef>,
}

impl KeyFacts {
    pub fn is_expired(&self) -> bool {
        self.days_left.map(|d| d < 0).unwrap_or(false)
    }

    /// Whether the key carries `scope_type` with `operation` on any target.
    ///
    /// The target is deliberately not checked. A scope's `targetParts` name
    /// universes, datastores or creators, and deciding whether a given target
    /// covers a given call means resolving the caller's env, which `doctor`
    /// does not always have and would have to guess at. Answering the narrower
    /// question honestly beats answering the wider one approximately: a key
    /// that lacks the scope type outright is the failure people actually hit.
    pub fn grants(&self, scope_type: &str, operation: &str) -> bool {
        self.scopes
            .iter()
            .any(|s| s.scope_type == scope_type && s.operations.iter().any(|op| op == operation))
    }
}

/// How the account listing answered when asked to identify a secret.
#[derive(Debug)]
pub enum KeyMatch {
    /// Exactly one key on the account starts with this secret's prefix.
    One(Box<KeyFacts>),
    /// The account holds keys, but none of them matches this secret. Usually a
    /// key belonging to a different account than the cookie signs in as.
    None,
    /// More than one key shares the prefix, so the secret cannot be pinned to
    /// one of them. Carries their names so the reader can see the collision.
    Ambiguous(Vec<String>),
}

/// Identify an API key secret against the keys the cookie's account holds.
///
/// Roblox's listing returns `apikeySecretPreview`: the first characters of
/// each secret, the same thing the Creator Hub shows in its "Key" column. That
/// is the only link between a secret sitting in `RBX_API_KEY` and a key's
/// stored configuration: `introspect` is authoritative but stops working about
/// an hour after the key is created, so it cannot answer for the key somebody
/// has been using for a month, which is exactly the key `doctor` is asked
/// about.
pub async fn identify_secret(global: &GlobalFlags, secret: &str) -> Result<KeyMatch> {
    let client = make_client(global);
    // Fail early and with the cookie message rather than sending an
    // unauthenticated POST and reporting whatever Roblox says about it.
    client.cookie_header()?;

    let remote = client
        .list_all_api_keys(None)
        .await
        .context("listing the account's API keys to identify the active one")?;
    let lk = lock::load().unwrap_or_default();
    let joined = remote_view::join_with_lock(remote, &lk);

    let matched: Vec<_> = joined
        .iter()
        .filter(|k| {
            k.info
                .apikey_secret_preview
                .as_deref()
                .filter(|p| !p.is_empty())
                .map(|p| secret.starts_with(p))
                .unwrap_or(false)
        })
        .collect();

    match matched.as_slice() {
        [] => Ok(KeyMatch::None),
        [only] => Ok(KeyMatch::One(Box::new(facts_from_remote(only)))),
        many => Ok(KeyMatch::Ambiguous(
            many.iter().map(|k| k.name().to_string()).collect(),
        )),
    }
}

/// What Roblox holds for a key this project tracks, found by its
/// `cloud_auth_id` rather than by a secret.
///
/// The path for a key that is declared here but not loaded into the
/// environment: there is no secret to match on, and the lockfile already knows
/// the id.
pub async fn facts_for_id(global: &GlobalFlags, cloud_auth_id: &str) -> Result<Option<KeyFacts>> {
    let client = make_client(global);
    client.cookie_header()?;

    let remote = client
        .list_all_api_keys(None)
        .await
        .context("listing the account's API keys")?;
    let lk = lock::load().unwrap_or_default();
    Ok(remote_view::join_with_lock(remote, &lk)
        .iter()
        .find(|k| k.info.id == cloud_auth_id)
        .map(facts_from_remote))
}

fn facts_from_remote(key: &remote_view::RemoteKey) -> KeyFacts {
    let props = key.info.cloud_auth_user_configured_properties.clone();
    KeyFacts {
        remote_name: key.name().to_string(),
        tracked_as: match &key.tracked {
            remote_view::Tracked::Yes(name) => Some(name.clone()),
            remote_view::Tracked::No => None,
        },
        enabled: key.info.is_enabled(),
        expires_at: key.info.expiration_time().map(|s| s.to_string()),
        days_left: key.days_left(),
        allowed_cidrs: props
            .as_ref()
            .map(|p| p.allowed_cidrs.clone())
            .unwrap_or_default(),
        scopes: props.map(|p| p.scopes).unwrap_or_default(),
    }
}

/// The scopes `introspect` reports for a secret.
///
/// Authoritative (it asks about the secret itself rather than about a key
/// found by matching a prefix) but only usable while the JWT inside the secret
/// is still valid, roughly an hour after create or regenerate. An `Err` here is
/// the ordinary case for any older key and callers should treat it as
/// "unavailable", not "broken".
pub async fn introspect_scopes(global: &GlobalFlags, secret: &str) -> Result<Vec<ScopeDef>> {
    let client = make_client(global);
    let resp = client.introspect_api_key(secret).await?;
    match crate::introspect::scopes_from_response(&resp) {
        None => Err(anyhow::anyhow!("introspect returned no scopes array")),
        // An entry this build cannot read is an error, not a scope the key
        // does not have: dropping it silently is what made a whole response
        // read as an empty key.
        Some(Err(why)) => Err(anyhow::anyhow!("introspect: {why}")),
        Some(Ok(scopes)) => Ok(scopes),
    }
}

/// Human-facing summary of an expiry, e.g. `in 91d`.
pub fn expiry_text(expires_at: Option<&str>) -> String {
    match expires_at.and_then(time_iso::days_until) {
        None if expires_at.is_none() => "no expiry".to_string(),
        None => "unparseable expiry".to_string(),
        Some(d) if d < 0 => format!("expired {}d ago", d.abs()),
        Some(d) => format!("in {d}d"),
    }
}

/// The same cookie every other `rbx-apikey` call uses.
///
/// This used to spell out its own resolution order, because
/// `commands::make_client` reached a second lookup that ignored
/// `--no-auto-cookie` and `doctor` cannot be built on a path that disagrees
/// with the credential it prints. That second lookup is gone, so the special
/// case is too: honouring the flag is now simply what `resolve_cookie` does.
fn make_client(global: &GlobalFlags) -> RbxApiKeyClient {
    RbxApiKeyClient::new(global.resolve_cookie())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(scopes: &[(&str, &[&str])]) -> KeyFacts {
        KeyFacts {
            remote_name: "k".into(),
            tracked_as: None,
            enabled: true,
            expires_at: None,
            days_left: None,
            allowed_cidrs: vec![],
            scopes: scopes
                .iter()
                .map(|(t, ops)| ScopeDef {
                    scope_type: (*t).to_string(),
                    target_parts: vec!["1".into()],
                    operations: ops.iter().map(|o| (*o).to_string()).collect(),
                })
                .collect(),
        }
    }

    #[test]
    fn grants_matches_type_and_operation() {
        let f = facts(&[("universe", &["read", "write"])]);
        assert!(f.grants("universe", "read"));
        assert!(f.grants("universe", "write"));
    }

    #[test]
    fn grants_rejects_a_known_type_missing_the_operation() {
        let f = facts(&[("universe", &["read"])]);
        assert!(!f.grants("universe", "write"));
    }

    /// `universe` and `universe.place` are different scopes, and a prefix match
    /// would quietly report a key as able to write places it cannot touch.
    #[test]
    fn grants_does_not_match_on_a_prefix() {
        let f = facts(&[("universe", &["read", "write"])]);
        assert!(!f.grants("universe.place", "write"));
    }

    #[test]
    fn grants_finds_a_scope_that_is_not_the_first() {
        let f = facts(&[("universe", &["read"]), ("asset", &["read", "write"])]);
        assert!(f.grants("asset", "write"));
    }

    #[test]
    fn a_key_with_no_expiry_is_not_expired() {
        assert!(!facts(&[]).is_expired());
    }

    #[test]
    fn a_negative_day_count_is_expired() {
        let mut f = facts(&[]);
        f.days_left = Some(-3);
        assert!(f.is_expired());
    }

    #[test]
    fn expiry_text_reads_naturally_in_all_three_states() {
        assert_eq!(expiry_text(None), "no expiry");
        assert_eq!(expiry_text(Some("not-a-date")), "unparseable expiry");
        assert!(expiry_text(Some(&time_iso::iso_in_days(10))).starts_with("in "));
        assert!(expiry_text(Some(&time_iso::iso_in_days(-10))).starts_with("expired "));
    }
}
