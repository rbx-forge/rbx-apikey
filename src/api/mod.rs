//! Roblox cloud-authentication + api-keys API wrapper.
//!
//! Auth: the session cookie this client is handed. Every source: an explicit
//! flag, `RBX_COOKIE`, Studio auto-detection: is resolved once, in
//! `rbx_core::GlobalFlags::resolve_cookie`. Nothing here looks one up.
//! CSRF: Roblox returns 403 + x-csrf-token on first mutating request; retry with the token.

pub mod api_keys;
pub mod games;

use anyhow::{bail, Result};
use reqwest::{Client, RequestBuilder, Response};

use rbx_core::api::{ApiBase, CsrfToken};
use rbx_core::session::{self, Session};

const UA: &str = "rbx-apikey (https://github.com/rbx-forge/rbx-apikey)";

/// Where "who is this cookie" is asked. The key administration endpoints do
/// not answer it, so it is a second host and gets its own const.
const USERS_HOST: &str = "https://users.roblox.com";

pub struct RbxApiKeyClient {
    client: Client,
    cookie: Option<String>,
    csrf_token: CsrfToken,
    /// Where the cloud-authentication endpoints live.
    ///
    /// Injectable so the request-shaping code can be tested against a mock
    /// server. Without it these URLs were `const`s and nothing here could be
    /// exercised over HTTP: pagination in particular had never run, since one
    /// page covers most accounts.
    base: ApiBase,

    /// Where `users.roblox.com` lives. Separate from `base` because it is a
    /// separate service: a test mocking the key endpoints should not have the
    /// session question answered by the same mock unless it says so.
    users_base: ApiBase,
}

impl RbxApiKeyClient {
    pub fn new(cookie: Option<String>) -> Self {
        let client = rbx_core::api::build_client_with_user_agent(UA);
        Self {
            client,
            cookie,
            csrf_token: CsrfToken::new(),
            base: ApiBase::default(),
            users_base: ApiBase::new(USERS_HOST),
        }
    }

    /// Point the client at another host. Tests only, and compiled only for
    /// them: the module is private, so a `pub` that nothing in a normal build
    /// calls is dead code rather than API.
    #[cfg(test)]
    pub fn with_base_url(mut self, base: impl Into<String>) -> Self {
        self.base = ApiBase::new(base);
        self
    }

    /// Point the session question at another host. Tests only, same reason.
    #[cfg(test)]
    pub fn with_users_base_url(mut self, base: impl Into<String>) -> Self {
        self.users_base = ApiBase::new(base);
        self
    }

    /// `https://apis.roblox.com/cloud-authentication/v1/apiKey`, or the mock
    /// server's equivalent.
    pub(crate) fn cloud_auth_url(&self) -> String {
        self.base.join("/cloud-authentication/v1/apiKey")
    }

    /// Plural, and reached with a POST. See [`Self::list_api_keys`], whose
    /// `impl` block lives in `api_keys` even though the type is declared here.
    pub(crate) fn list_url(&self) -> String {
        self.base.join("/cloud-authentication/v1/apiKeys")
    }

    /// The cookie this client was handed, unwrapped.
    ///
    /// Exists for the one test asserting that `make_client` adds nothing to
    /// what `resolve_cookie` decided: the field itself is private so that
    /// nothing outside this module can send the cookie by a route that skips
    /// `cookie_header`. `cfg(test)` for the same reason `with_base_url` is:
    /// outside the test build it would be dead code under `-D warnings`.
    #[cfg(test)]
    pub(crate) fn cookie(&self) -> Option<&str> {
        self.cookie.as_deref()
    }

    /// Build a `.ROBLOSECURITY=...` header value, or fail with a friendly message.
    ///
    /// The `.ROBLOSECURITY=` prefixing itself lives in `rbx_core::session`,
    /// which is where the session check is: the check has to send the header
    /// the later calls will send, byte for byte, or it vouches for something
    /// else.
    pub fn cookie_header(&self) -> Result<String> {
        Ok(session::cookie_header(self.raw_cookie()?))
    }

    /// The cookie as it was resolved, or the message naming the three ways to
    /// supply one.
    ///
    /// The session check is keyed on this value, not on the header built from
    /// it: keying on both forms would make one run ask Roblox twice about the
    /// same cookie.
    fn raw_cookie(&self) -> Result<&str> {
        self.cookie.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "No Studio cookie found. Sign in to Roblox Studio with the account that holds the keys, pass --cookie, or set RBX_COOKIE."
            )
        })
    }

    /// Refuse to go on when the cookie is no longer a session.
    ///
    /// Called by the subcommands that write with it (`update`, `regenerate`,
    /// `delete`) before their first write. `create` and `prune` need the
    /// signed-in account for their own reasons and get the same guarantee out
    /// of `authenticated_account`, which shares this check's one cached call.
    /// The read-only subcommands do not call it: a listing that fails is a
    /// listing that did not print, with nothing left behind.
    ///
    /// A missing cookie is left to `cookie_header`, whose message names the
    /// three ways to supply one.
    pub async fn require_valid_session(&self) -> Result<()> {
        let Some(cookie) = self.cookie.as_deref() else {
            return Ok(());
        };
        session::require_valid_with_host(&self.client, cookie, self.users_base.as_str()).await
    }

    /// The account the session check already identified, for a prompt.
    ///
    /// Never issues a request, unlike [`Self::authenticated_account`]: this is
    /// for naming the account in a confirmation, and a prompt must not be
    /// preceded by a surprise round trip. `None` when no check has run or it
    /// could not answer, in which case the question stays as it was.
    pub async fn known_account(&self) -> Option<session::SessionAccount> {
        let cookie = self.cookie.as_deref()?;
        session::known_account_with_host(cookie, self.users_base.as_str()).await
    }

    /// The account the cookie signs in as.
    ///
    /// Routed through the shared session check rather than its own request, so
    /// `apikey create` spends one round trip on the question rather than one
    /// for the creator id and another to prove the session, and so a refusal
    /// reads as an expired session everywhere instead of as "identifying the
    /// signed-in account failed".
    pub async fn authenticated_account(&self) -> Result<session::SessionAccount> {
        let cookie = self.raw_cookie()?;
        match session::check_with_host(&self.client, cookie, self.users_base.as_str()).await {
            Session::Valid(account) => Ok(account),
            Session::Refused => bail!(session::EXPIRED_SESSION),
            Session::Empty => bail!(session::EMPTY_COOKIE),
            // Unlike a preflight, this one has no answer to fall back on: the
            // caller needs the account, not reassurance about it.
            Session::Unknown(why) => {
                bail!("could not identify the signed-in account: {why}")
            }
        }
    }

    /// Send an authenticated request, retrying once on 403 with the refreshed CSRF token.
    /// Send a state-changing request, handling Roblox's CSRF token dance.
    ///
    /// The dance itself lives in `rbx_core::api::send_with_csrf`, so there is
    /// one answer to whether `204` is a success. What stays here is the
    /// rendering of a refusal, which is this crate's to choose.
    pub async fn send_with_csrf<F>(&self, build: F) -> Result<Response>
    where
        F: Fn() -> RequestBuilder,
    {
        rbx_core::api::send_with_csrf(&self.csrf_token, build)
            .await
            .map_err(Into::into)
    }
}

// No Studio lookup happens in this crate, on purpose. A second lookup site is
// how `--no-auto-cookie` once stopped working: the flag turned the first one
// off and the second auto-detected anyway. One chain cannot diverge from
// itself, so the only one is `rbx_core::env`.

/// The session preflight, from this crate's side.
///
/// What `rbx-core` owns (the statuses, the caching, the wording) is tested
/// there. What this owns is that its client asks the right host with the right
/// cookie, that the creator-id lookup and the preflight are the same question,
/// and that a refusal comes back as one.
#[cfg(test)]
mod session_tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn users_service(status: u16, body: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/users/authenticated"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .mount(&server)
            .await;
        server
    }

    fn client(users: &MockServer, cookie: &str) -> RbxApiKeyClient {
        RbxApiKeyClient::new(Some(cookie.to_string())).with_users_base_url(users.uri())
    }

    #[tokio::test]
    async fn an_expired_session_is_refused_before_anything_is_written() {
        let users = users_service(401, "{}").await;

        let error = client(&users, "apikey-expired")
            .require_valid_session()
            .await
            .expect_err("an expired session must stop the command")
            .to_string();

        assert!(error.contains("expired"), "got {error}");
        assert!(error.contains("Roblox Studio"), "got {error}");
        assert!(!error.contains("401"), "got {error}");
    }

    /// `create` needs the creator id and every write needs a live session.
    /// Those are one question, so they cost one round trip.
    #[tokio::test]
    async fn the_creator_id_and_the_preflight_share_one_call() {
        let users = users_service(200, r#"{"id":42,"name":"tester"}"#).await;
        let client = client(&users, "apikey-shared");

        let account = client
            .authenticated_account()
            .await
            .expect("a live session");
        client
            .require_valid_session()
            .await
            .expect("the same live session");

        assert_eq!(account.id, 42);
        assert_eq!(users.received_requests().await.unwrap().len(), 1);
    }

    /// The cookie leaves in the form Roblox reads, whichever form it was
    /// supplied in: the same normalisation `cookie_header` applies to every
    /// other call this client makes.
    #[tokio::test]
    async fn the_session_call_carries_the_normalised_cookie() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/users/authenticated"))
            .and(header("cookie", ".ROBLOSECURITY=apikey-raw"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"id":1,"name":"a"}"#))
            .mount(&server)
            .await;

        client(&server, "apikey-raw")
            .require_valid_session()
            .await
            .expect("a live session");
    }

    /// No cookie is not an expired session, and this is not the place that
    /// reports it: `cookie_header` names the three ways to supply one, at the
    /// call that actually needs it.
    #[tokio::test]
    async fn no_cookie_at_all_is_left_to_the_call_that_needs_one() {
        let users = users_service(401, "{}").await;
        let client = RbxApiKeyClient::new(None).with_users_base_url(users.uri());

        assert!(client.require_valid_session().await.is_ok());
        assert!(users.received_requests().await.unwrap().is_empty());
        assert!(client.cookie_header().is_err());
    }
}
