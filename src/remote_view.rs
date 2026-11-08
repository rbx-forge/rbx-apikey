//! The account's keys as Roblox holds them, joined to this project's lockfile.
//!
//! Shared by `list --remote` and `prune`, which differ only in what they do
//! with the result.
//!
//! Two facts drive the whole design, both measured rather than assumed:
//!
//! 1. The listing covers the *whole account*, not this project. Of the keys a
//!    real account returns, the ones this repo manages are a small minority;
//!    the rest belong to other checkouts, other tools, or were made by hand in
//!    the Creator Hub. So every key carries a [`Tracked`] verdict and nothing
//!    is ever selected for you.
//! 2. Names are not identity. The lockfile calls a key `viewer` while Roblox
//!    calls it `prodread_viewer`, and two different accounts each hold a
//!    `ske_asphalt`. The join is on `cloud_auth_id` and only on that.

use std::collections::HashMap;

use anyhow::Result;

use crate::api::api_keys::RemoteApiKey;
use crate::{lock, time_iso};

/// Whether this project's lockfile claims a given remote key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tracked {
    /// In `rbxapikey.lock.toml` under this name.
    Yes(String),
    /// Exists on the account, but this project knows nothing about it.
    No,
}

impl Tracked {
    pub fn is_tracked(&self) -> bool {
        matches!(self, Tracked::Yes(_))
    }
}

#[derive(Debug, Clone)]
pub struct RemoteKey {
    pub info: RemoteApiKey,
    pub tracked: Tracked,
}

impl RemoteKey {
    pub fn name(&self) -> &str {
        self.info.name()
    }

    /// Days until expiry; `None` when the key never expires or the timestamp
    /// could not be parsed.
    pub fn days_left(&self) -> Option<i64> {
        self.info.expiration_time().and_then(time_iso::days_until)
    }

    pub fn is_expired(&self) -> bool {
        self.days_left().map(|d| d < 0).unwrap_or(false)
    }

    /// Short, human-facing summary of the expiry, e.g. `in 91d`.
    pub fn expiry_text(&self) -> String {
        match self.days_left() {
            None => "no expiry".to_string(),
            Some(d) if d < 0 => format!("EXPIRED {}d ago", d.abs()),
            Some(d) => format!("in {}d", d),
        }
    }

    /// `created` date without the time, which is all any of this needs.
    pub fn created_date(&self) -> &str {
        self.info
            .created_time
            .as_deref()
            .map(|s| if s.len() >= 10 { &s[..10] } else { s })
            .unwrap_or("?")
    }

    /// The Creator Hub's "Key" column: enough of the secret to recognise it.
    pub fn secret_preview(&self) -> String {
        match self.info.apikey_secret_preview.as_deref() {
            Some(p) if !p.is_empty() => format!("{}…", p),
            _ => "-".to_string(),
        }
    }

    /// One-word state, mirroring what the Creator Hub shows.
    pub fn state(&self) -> KeyState {
        if !self.info.is_enabled() {
            KeyState::Disabled
        } else if self.is_expired() {
            KeyState::Expired
        } else {
            KeyState::Active
        }
    }
}

/// An enum rather than a `&str` so a caller's `match` has to stay exhaustive:
/// matching on stringly state lets a typo fall silently into the catch-all arm
/// and paint a dead key green.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyState {
    Active,
    Expired,
    Disabled,
}

impl KeyState {
    pub fn label(self) -> &'static str {
        match self {
            KeyState::Active => "active",
            KeyState::Expired => "expired",
            KeyState::Disabled => "disabled",
        }
    }
}

impl std::fmt::Display for KeyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Join a remote listing to the lockfile. Order is preserved: Roblox returns
/// newest first, which is the order worth keeping.
pub fn join_with_lock(remote: Vec<RemoteApiKey>, lk: &lock::Lock) -> Vec<RemoteKey> {
    let by_id: HashMap<&str, &str> = lk
        .keys
        .iter()
        .map(|(name, entry)| (entry.cloud_auth_id.as_str(), name.as_str()))
        .collect();

    remote
        .into_iter()
        .map(|info| {
            let tracked = match by_id.get(info.id.as_str()) {
                Some(name) => Tracked::Yes((*name).to_string()),
                None => Tracked::No,
            };
            RemoteKey { info, tracked }
        })
        .collect()
}

/// Counts for the summary line.
pub struct Tally {
    pub total: usize,
    pub tracked: usize,
    pub untracked: usize,
    pub expired: usize,
    pub disabled: usize,
}

pub fn tally(keys: &[RemoteKey]) -> Tally {
    Tally {
        total: keys.len(),
        tracked: keys.iter().filter(|k| k.tracked.is_tracked()).count(),
        untracked: keys.iter().filter(|k| !k.tracked.is_tracked()).count(),
        expired: keys.iter().filter(|k| k.is_expired()).count(),
        disabled: keys.iter().filter(|k| !k.info.is_enabled()).count(),
    }
}

/// Lockfile entries with no counterpart on the account. `status --remote`
/// finds these one GET at a time; a single listing finds them all at once.
pub fn lock_entries_missing_remotely(remote: &[RemoteKey], lk: &lock::Lock) -> Vec<String> {
    let present: Vec<&str> = remote.iter().map(|k| k.info.id.as_str()).collect();
    lk.keys
        .iter()
        .filter(|(_, e)| !present.contains(&e.cloud_auth_id.as_str()))
        .map(|(name, _)| name.clone())
        .collect()
}

/// Fetch every key for the authenticated cookie, joined to the lockfile.
pub async fn fetch(
    client: &crate::api::RbxApiKeyClient,
    lk: &lock::Lock,
    group_id: Option<u64>,
) -> Result<Vec<RemoteKey>> {
    let remote = client.list_all_api_keys(group_id).await?;
    Ok(join_with_lock(remote, lk))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::api_keys::{RemoteApiKey, RemoteProperties};

    fn lock_with(entries: &[(&str, &str)]) -> lock::Lock {
        let mut lk = lock::Lock::default();
        for (name, id) in entries {
            lk.keys.insert(
                (*name).to_string(),
                lock::LockEntry {
                    cloud_auth_id: (*id).to_string(),
                    secret: None,
                    secret_file: None,
                    creator_id: 1,
                    is_enabled: true,
                    created_at: "2026-01-01T00:00:00.000Z".to_string(),
                    expires_at: None,
                },
            );
        }
        lk
    }

    fn remote(id: &str, name: &str) -> RemoteApiKey {
        RemoteApiKey {
            id: id.to_string(),
            cloud_auth_user_configured_properties: Some(RemoteProperties {
                name: name.to_string(),
                is_enabled: true,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn tracked_is_decided_by_id_not_by_name() {
        // The real shape of the problem: the lockfile calls it `viewer`, the
        // account calls it `prodread_viewer`. Matching on names would report
        // this key as untracked and offer it for deletion.
        let lk = lock_with(&[("viewer", "f58b4055")]);
        let joined = join_with_lock(vec![remote("f58b4055", "prodread_viewer")], &lk);
        assert_eq!(joined[0].tracked, Tracked::Yes("viewer".to_string()));
    }

    #[test]
    fn a_matching_name_with_a_different_id_stays_untracked() {
        // Two accounts each hold an `ske_asphalt`. Trusting the name would
        // delete the wrong account's key.
        let lk = lock_with(&[("ske_asphalt", "aaaa-1111")]);
        let joined = join_with_lock(vec![remote("bbbb-2222", "ske_asphalt")], &lk);
        assert_eq!(joined[0].tracked, Tracked::No);
    }

    #[test]
    fn tally_counts_each_category() {
        let lk = lock_with(&[("mine", "id-1")]);
        let mut disabled = remote("id-2", "somebody-elses");
        disabled
            .cloud_auth_user_configured_properties
            .as_mut()
            .unwrap()
            .is_enabled = false;
        let mut expired = remote("id-3", "old");
        expired
            .cloud_auth_user_configured_properties
            .as_mut()
            .unwrap()
            .expiration_time = Some("2020-01-01T00:00:00.000Z".to_string());

        let joined = join_with_lock(vec![remote("id-1", "mine"), disabled, expired], &lk);
        let t = tally(&joined);
        assert_eq!(t.total, 3);
        assert_eq!(t.tracked, 1);
        assert_eq!(t.untracked, 2);
        assert_eq!(t.disabled, 1);
        assert_eq!(t.expired, 1);
    }

    #[test]
    fn lock_entries_absent_from_the_account_are_reported() {
        let lk = lock_with(&[("here", "id-1"), ("gone", "id-404")]);
        let joined = join_with_lock(vec![remote("id-1", "here")], &lk);
        assert_eq!(lock_entries_missing_remotely(&joined, &lk), vec!["gone"]);
    }

    #[test]
    fn expiry_text_distinguishes_past_from_future_and_never() {
        let lk = lock::Lock::default();
        let mut soon = remote("id-1", "soon");
        soon.cloud_auth_user_configured_properties
            .as_mut()
            .unwrap()
            .expiration_time = Some(time_iso::iso_in_days(10));
        let joined = join_with_lock(vec![soon, remote("id-2", "forever")], &lk);

        assert!(joined[0].expiry_text().starts_with("in "));
        assert!(!joined[0].is_expired());
        assert_eq!(joined[1].expiry_text(), "no expiry");
    }
}
