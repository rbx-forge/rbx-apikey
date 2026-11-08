//! Step 5: one cheap authenticated read, to confirm the whole chain works.
//!
//! Every other check reads something local or asks Roblox about the key's
//! *configuration*. None of them proves a call actually succeeds: a key can be
//! enabled, unexpired and correctly scoped and still be refused, most often by
//! an IP allowlist that no longer contains the caller. A stale allowed-CIDR
//! entry fails as an opaque 401, so it is the first thing to check when a call
//! that should work does not.
//!
//! `GET /cloud/v2/universes/{id}` is the probe: one scope (`universe:read`),
//! no side effects, no cost, and it is the request a metadata sync opens with,
//! so a failure here is a failure the user was going to hit anyway.

use rbx_core::api::{build_client, roblox_message, ApiBase};
use reqwest::StatusCode;

/// What one authenticated read did.
#[derive(Debug)]
pub enum ProbeOutcome {
    /// The call succeeded. Carries the universe's display name when the
    /// response had one: proof the bytes came back, not just the status.
    Ok { universe_name: Option<String> },
    /// Roblox refused. Carries the status and its own message, because the two
    /// answer different questions: 401 is about the caller, 403 about the key.
    Refused { status: StatusCode, message: String },
    /// The request never got an answer.
    Unreachable(String),
}

/// The read probe, pointed at a host the caller owns.
///
/// The host used to be an `ApiBase::default()` built inside the call, which
/// meant the refusals this whole command exists to explain could only be
/// asserted as string constants: nothing here had ever run over HTTP. The
/// seam is a `#[cfg(test)] with_base_url`, and the production path is
/// unchanged: `run` builds a [`Probe::default`], which is
/// [`ApiBase::default`].
#[derive(Debug, Default)]
pub struct Probe {
    base: ApiBase,
}

impl Probe {
    /// Point the probe at another host. Tests only, and compiled only for
    /// them: `Probe` is reachable inside the crate only, so outside the test
    /// build this would be dead code under `-D warnings`.
    #[cfg(test)]
    pub(crate) fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base = ApiBase::new(url);
        self
    }

    pub async fn read_universe(&self, api_key: &str, universe_id: u64) -> ProbeOutcome {
        let url = self
            .base
            .join(&format!("/cloud/v2/universes/{universe_id}"));
        let response = build_client()
            .get(&url)
            .header("x-api-key", api_key)
            .send()
            .await;

        let response = match response {
            Ok(r) => r,
            Err(e) => return ProbeOutcome::Unreachable(e.to_string()),
        };

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if status.is_success() {
            return ProbeOutcome::Ok {
                universe_name: universe_name_from(&body),
            };
        }
        ProbeOutcome::Refused {
            status,
            message: message_from(&body),
        }
    }
}

/// Roblox's own error text, which is more specific than the status alone.
///
/// [`roblox_message`] rather than a local parse: Open Cloud answers a bad key
/// with `{"errors":[{"message":"Invalid API Key"}]}` on some paths and a flat
/// `{"message":...}` on others, and that function already knows both envelopes.
/// Reaching for `body["message"]` here quietly missed the first shape, which is
/// the one an invalid key actually produces.
///
/// Falls back to the raw body: an unrecognised shape still tells the reader
/// more than nothing, and truncating keeps an HTML error page from filling the
/// terminal.
fn message_from(body: &str) -> String {
    roblox_message(body).unwrap_or_else(|| truncate(body.trim(), 200))
}

fn universe_name_from(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()?
        .get("displayName")?
        .as_str()
        .map(|s| s.to_string())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    format!("{}…", s.chars().take(max).collect::<String>())
}

/// The one-line reading of a refusal.
///
/// The point of the whole command: 401 and 403 look the same from inside a
/// script and mean entirely different things, and the IP allowlist is the
/// cause people never guess.
pub fn explain(status: StatusCode) -> &'static str {
    match status {
        StatusCode::UNAUTHORIZED => {
            "401 means the key was rejected before any permission was considered, and Roblox \
             says \"Invalid API Key\" for two different causes. Either the secret is wrong (\
             rotated, truncated, or a leftover in RBX_API_KEY from another project) or the \
             IP allowlist no longer contains this machine, which fails identically and is \
             the one nobody guesses. Check the allowed IPs listed above against this \
             machine's public address, and reload the key with \
             `export RBX_API_KEY=\"$(rbx-apikey resolve <name>)\"`."
        }
        StatusCode::FORBIDDEN => {
            "403 means the key is valid but not allowed to make this call: a missing scope, \
             or a scope whose target does not cover this universe. Compare the scope coverage \
             above, then widen the key in rbxapikey.toml and `rbx-apikey update <key>`."
        }
        StatusCode::NOT_FOUND => {
            "404 means the universe id does not exist, or the key's owner cannot see it. \
             Check the id in rbxplace.toml resolves to a game this account owns."
        }
        _ => {
            "The key reached Roblox and the call still failed. The status and message above \
             are Roblox's own; nothing local explains it."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flat_roblox_error_body_yields_its_message() {
        assert_eq!(
            message_from(r#"{"code":"UNAUTHENTICATED","message":"Invalid API key"}"#),
            "Invalid API key"
        );
    }

    /// The shape an invalid key actually produces, measured against
    /// `GET /cloud/v2/universes/{id}`. A parser that only knew the flat form
    /// printed the whole JSON blob instead of the sentence in it.
    #[test]
    fn an_errors_array_body_yields_its_message() {
        assert_eq!(
            message_from(r#"{"errors":[{"code":0,"message":"Invalid API Key"}]}"#),
            "Invalid API Key"
        );
    }

    #[test]
    fn a_body_that_is_not_json_falls_back_to_itself() {
        assert_eq!(message_from("  gateway timeout  "), "gateway timeout");
    }

    #[test]
    fn a_long_html_error_page_does_not_fill_the_terminal() {
        let body = "x".repeat(5000);
        assert!(message_from(&body).chars().count() <= 201);
    }

    #[test]
    fn a_successful_body_yields_the_universe_name() {
        assert_eq!(
            universe_name_from(r#"{"path":"universes/1","displayName":"My Game"}"#).as_deref(),
            Some("My Game")
        );
    }

    #[test]
    fn a_body_without_a_display_name_is_still_a_success() {
        assert!(universe_name_from(r#"{"path":"universes/1"}"#).is_none());
    }

    /// The 401/403 split is the reason the probe exists, so both must say
    /// something different and specific.
    #[test]
    fn unauthorized_and_forbidden_are_explained_differently() {
        let unauthorized = explain(StatusCode::UNAUTHORIZED);
        let forbidden = explain(StatusCode::FORBIDDEN);
        assert_ne!(unauthorized, forbidden);
        assert!(unauthorized.contains("IP allowlist"));
        assert!(forbidden.contains("scope"));
        // 401 has two causes and naming only one sends people the wrong way.
        assert!(unauthorized.contains("secret is wrong"));
    }

    /// The probe over real HTTP.
    ///
    /// Everything above reads bodies the test wrote itself, which cannot catch
    /// the probe asking for the wrong path, forgetting the header, or reading a
    /// status off the wrong response. These run the whole call.
    mod over_http {
        use super::*;
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const UNIVERSE: u64 = 5544332211;

        async fn answering(status: u16, body: &str) -> MockServer {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path(format!("/cloud/v2/universes/{UNIVERSE}")))
                .and(header("x-api-key", "test-key"))
                .respond_with(ResponseTemplate::new(status).set_body_string(body))
                .mount(&server)
                .await;
            server
        }

        async fn probe(server: &MockServer) -> ProbeOutcome {
            Probe::default()
                .with_base_url(server.uri())
                .read_universe("test-key", UNIVERSE)
                .await
        }

        /// The success shape: the name comes back off the wire, which is the
        /// proof that bytes arrived rather than only a status.
        #[tokio::test]
        async fn a_200_carries_the_universe_name_off_the_wire() {
            let server = answering(200, r#"{"path":"universes/1","displayName":"My Game"}"#).await;
            match probe(&server).await {
                ProbeOutcome::Ok { universe_name } => {
                    assert_eq!(universe_name.as_deref(), Some("My Game"))
                }
                other => panic!("expected Ok, got {other:?}"),
            }
        }

        /// The body an invalid key actually produces is the `errors` array
        /// form, and it must survive the round trip as its sentence rather
        /// than as the JSON blob around it.
        #[tokio::test]
        async fn a_401_is_refused_with_robloxs_own_message() {
            let server = answering(
                401,
                r#"{"errors":[{"code":0,"message":"Invalid API Key"}]}"#,
            )
            .await;
            match probe(&server).await {
                ProbeOutcome::Refused { status, message } => {
                    assert_eq!(status, StatusCode::UNAUTHORIZED);
                    assert_eq!(message, "Invalid API Key");
                }
                other => panic!("expected Refused, got {other:?}"),
            }
        }

        #[tokio::test]
        async fn a_403_is_refused_with_robloxs_own_message() {
            let server = answering(
                403,
                r#"{"code":"PERMISSION_DENIED","message":"missing scope universe:read"}"#,
            )
            .await;
            match probe(&server).await {
                ProbeOutcome::Refused { status, message } => {
                    assert_eq!(status, StatusCode::FORBIDDEN);
                    assert_eq!(message, "missing scope universe:read");
                }
                other => panic!("expected Refused, got {other:?}"),
            }
        }

        #[tokio::test]
        async fn a_404_is_refused_rather_than_read_as_an_empty_success() {
            let server = answering(404, r#"{"message":"Universe not found"}"#).await;
            match probe(&server).await {
                ProbeOutcome::Refused { status, message } => {
                    assert_eq!(status, StatusCode::NOT_FOUND);
                    assert_eq!(message, "Universe not found");
                }
                other => panic!("expected Refused, got {other:?}"),
            }
        }

        /// A host that answers nothing is not a refusal: saying "Roblox
        /// refused you" when Roblox was never reached sends the reader off
        /// rotating a key that is fine.
        #[tokio::test]
        async fn a_host_that_is_not_there_is_unreachable_not_refused() {
            // Port 1, rather than a mock server started and dropped: the tests
            // in this binary run in parallel and each starts its own listener
            // on an ephemeral port, so a just-freed port is one another test
            // can be handed, which made this assert against a live server.
            let outcome = Probe::default()
                .with_base_url("http://127.0.0.1:1")
                .read_universe("test-key", UNIVERSE)
                .await;
            assert!(
                matches!(outcome, ProbeOutcome::Unreachable(_)),
                "got {outcome:?}"
            );
        }
    }
}
