//! "Is my cookie still good", which is exactly the question `doctor` exists to
//! answer, and the one it used to leave out.
//!
//! Step 1 reported where the cookie came from and stopped there, so a run could
//! print a green credentials section for a session Roblox had stopped
//! accepting. Every command that needs the cookie would then fail somewhere
//! else, with a message about the resource rather than about the session, and
//! a multi-step write would fail halfway through.
//!
//! One `users/authenticated` call answers it. `doctor` already spends
//! cookie-bearing calls listing the account's keys, so this is not a new kind
//! of traffic, and `rbx_core::session` caches the verdict, so the check and any
//! other question about the same cookie in this process cost one round trip.

use rbx_core::api::{build_client, ApiBase};
use rbx_core::session::{self, Session};

/// The host that answers "who is this cookie". Named `<FIELD>_HOST` after the
/// `ApiBase` it feeds.
const USERS_HOST: &str = "https://users.roblox.com";

/// The session check, pointed at a host the caller owns.
///
/// The same `#[cfg(test)] with_base_url` seam as `probe::Probe` and
/// `ip::IpEcho`, for the same reason: the interesting outcomes here are a
/// refusal and a service that does not answer, and neither can be produced by
/// calling the real Roblox on purpose.
#[derive(Debug)]
pub struct SessionCheck {
    base: ApiBase,
}

impl Default for SessionCheck {
    fn default() -> Self {
        Self {
            base: ApiBase::new(USERS_HOST),
        }
    }
}

impl SessionCheck {
    /// Point the check at another host. Tests only, and compiled only for
    /// them: this type is crate-internal, so outside the test build a `pub`
    /// nothing calls is dead code under `-D warnings`.
    #[cfg(test)]
    pub(crate) fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base = ApiBase::new(url);
        self
    }

    /// The host is not returned separately: `rbx_core::session` puts it in the
    /// sentence it hands back for an unanswered check, which is the only line
    /// that has a reason to name it.
    pub(crate) async fn ask(&self, cookie: &str) -> Session {
        session::check_with_host(&build_client(), cookie, self.base.as_str()).await
    }
}
