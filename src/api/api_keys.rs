//! Cloud-authentication + api-keys/introspect endpoints.

use anyhow::{bail, Result};
use rbx_core::api::roblox_error;
use reqwest::{header, Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::scope_builder::ScopeDef;

use super::{unreadable_response, RbxApiKeyClient};
use crate::out::REDACTED;

// The cloud-authentication URLs now come from the client's `ApiBase` so a mock
// server can stand in for them; see `RbxApiKeyClient::cloud_auth_url`.
// `introspect` is a different service and stays a literal until something
// needs it otherwise.
const INTROSPECT: &str = "https://apis.roblox.com/api-keys/v1/introspect";

// ---------------- Request body shape ----------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigProperties {
    pub name: String,
    pub description: String,
    pub is_enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiration_time: Option<String>,
    pub allowed_cidrs: Vec<String>,
    pub scopes: Vec<ScopeDef>,
}

#[derive(Debug, Serialize)]
struct CreateBody<'a> {
    #[serde(rename = "cloudAuthUserConfiguredProperties")]
    props: &'a ConfigProperties,
}

#[derive(Debug, Serialize)]
struct UpdateBody<'a> {
    #[serde(rename = "cloudAuthId")]
    cloud_auth_id: &'a str,
    #[serde(rename = "cloudAuthUserConfiguredProperties")]
    props: &'a ConfigProperties,
}

#[derive(Serialize)]
struct IntrospectBody<'a> {
    #[serde(rename = "apiKey")]
    api_key: &'a str,
}

// The secret-bearing shapes print their secret as `<redacted>` under `{:?}`,
// so a `dbg!` or a `{:?}` added while debugging cannot put it in a log.
impl std::fmt::Debug for IntrospectBody<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IntrospectBody")
            .field("api_key", &REDACTED)
            .finish()
    }
}

/// Body of the list call. Measured against the Creator Hub, which sends exactly
/// these four fields (`getApiKeys` → `v1ApiKeysPost`). `reverse: false` is the
/// hub's `SortOrder.Desc`, i.e. newest first.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ListBody<'a> {
    cursor: &'a str,
    limit: u32,
    reverse: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    group_id: Option<u64>,
}

// ---------------- Response shapes ----------------

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct UserConfiguredProperties {
    pub expiration_time: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudAuthInfo {
    pub id: String,
    #[serde(default)]
    pub created_time: Option<String>,
    #[serde(default)]
    pub cloud_auth_user_configured_properties: Option<UserConfiguredProperties>,
}

#[derive(Deserialize)]
pub struct CreateApiKeyResponse {
    #[serde(rename = "cloudAuthInfo")]
    pub cloud_auth_info: CloudAuthInfo,
    #[serde(rename = "apikeySecret")]
    pub apikey_secret: String,
}

impl std::fmt::Debug for CreateApiKeyResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreateApiKeyResponse")
            .field("cloud_auth_info", &self.cloud_auth_info)
            .field("apikey_secret", &REDACTED)
            .finish()
    }
}

#[derive(Debug, Deserialize)]
pub struct UpdateApiKeyResponse {
    #[serde(rename = "cloudAuthInfo")]
    pub cloud_auth_info: CloudAuthInfo,
}

#[derive(Deserialize)]
pub struct RegenerateResponse {
    #[serde(rename = "apikeySecret")]
    pub apikey_secret: String,
}

impl std::fmt::Debug for RegenerateResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegenerateResponse")
            .field("apikey_secret", &REDACTED)
            .finish()
    }
}

// `AuthenticatedUser` was here, a one-field mirror of what
// `users/authenticated` returns. The answer now comes back as
// `rbx_core::session::SessionAccount` from the shared check, so the same call
// serves the creator id, the whoami line and the preflight rather than three
// requests deserialised into two shapes.

/// The user-configured half of a key as the list route returns it. Wider than
/// [`UserConfiguredProperties`], which only ever needed the expiry.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct RemoteProperties {
    pub name: String,
    pub description: String,
    pub is_enabled: bool,
    pub expiration_time: Option<String>,
    pub allowed_cidrs: Vec<String>,
    pub scopes: Vec<ScopeDef>,
}

/// One entry of the list route.
///
/// `cloud_auth_bad_status` is deliberately `Value` rather than a typed enum:
/// every key measured so far returns `[]`, so the element shape has never been
/// observed and inventing one is how a deserializer starts dropping fields.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct RemoteApiKey {
    pub id: String,
    pub created_time: Option<String>,
    pub updated_time: Option<String>,
    pub last_generated_time: Option<String>,
    pub owner_id: Option<u64>,
    /// `OWNER_TYPE_USER` / `OWNER_TYPE_GROUP`. Kept as the raw string: it is a
    /// different spelling from [`rbx_core::owner::OwnerType`] and mapping it
    /// would claim a correspondence only two of the variants have been seen to
    /// support.
    pub owner_type: Option<String>,
    /// First characters of the secret, as shown in the Creator Hub's "Key"
    /// column. Enough to recognise a key you hold without storing the secret.
    pub apikey_secret_preview: Option<String>,
    pub cloud_auth_bad_status: Vec<Value>,
    pub cloud_auth_user_configured_properties: Option<RemoteProperties>,
}

impl RemoteApiKey {
    pub fn name(&self) -> &str {
        self.cloud_auth_user_configured_properties
            .as_ref()
            .map(|p| p.name.as_str())
            .unwrap_or("<unnamed>")
    }

    pub fn is_enabled(&self) -> bool {
        self.cloud_auth_user_configured_properties
            .as_ref()
            .map(|p| p.is_enabled)
            .unwrap_or(false)
    }

    pub fn expiration_time(&self) -> Option<&str> {
        self.cloud_auth_user_configured_properties
            .as_ref()
            .and_then(|p| p.expiration_time.as_deref())
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ListApiKeysResponse {
    pub cloud_auth_info: Vec<RemoteApiKey>,
    pub next_cursor: Option<String>,
    pub previous_cursor: Option<String>,
}

impl RbxApiKeyClient {
    pub async fn create_api_key(&self, props: &ConfigProperties) -> Result<CreateApiKeyResponse> {
        let cookie = self.cookie_header()?;
        let cloud_auth = self.cloud_auth_url();
        let body = serde_json::to_string(&CreateBody { props })?;
        let response = self
            .send_with_csrf(|| {
                self.client
                    .request(Method::POST, &cloud_auth)
                    .header(header::COOKIE, &cookie)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(body.clone())
            })
            .await?;
        let text = response.text().await?;
        serde_json::from_str(&text).map_err(|e| unreadable_response("create", &e, &text))
    }

    pub async fn update_api_key(
        &self,
        cloud_auth_id: &str,
        props: &ConfigProperties,
    ) -> Result<UpdateApiKeyResponse> {
        let cookie = self.cookie_header()?;
        let cloud_auth = self.cloud_auth_url();
        let body = serde_json::to_string(&UpdateBody {
            cloud_auth_id,
            props,
        })?;
        let response = self
            .send_with_csrf(|| {
                self.client
                    .request(Method::PATCH, &cloud_auth)
                    .header(header::COOKIE, &cookie)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(body.clone())
            })
            .await?;
        let text = response.text().await?;
        serde_json::from_str(&text).map_err(|e| unreadable_response("update", &e, &text))
    }

    pub async fn regenerate_secret(&self, cloud_auth_id: &str) -> Result<RegenerateResponse> {
        let cookie = self.cookie_header()?;
        let url = format!("{}/{}/regenerate", self.cloud_auth_url(), cloud_auth_id);
        let response = self
            .send_with_csrf(|| {
                self.client
                    .request(Method::POST, &url)
                    .header(header::COOKIE, &cookie)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body("")
            })
            .await?;
        let text = response.text().await?;
        serde_json::from_str(&text).map_err(|e| unreadable_response("regenerate", &e, &text))
    }

    pub async fn delete_api_key(&self, cloud_auth_id: &str) -> Result<()> {
        let cookie = self.cookie_header()?;
        let url = format!("{}/{}", self.cloud_auth_url(), cloud_auth_id);
        let _ = self
            .send_with_csrf(|| {
                self.client
                    .request(Method::DELETE, &url)
                    .header(header::COOKIE, &cookie)
            })
            .await?;
        Ok(())
    }

    /// Returns Ok(None) if Roblox returns 404 (key no longer exists).
    pub async fn get_api_key(&self, cloud_auth_id: &str) -> Result<Option<Value>> {
        let cookie = self.cookie_header()?;
        let url = format!("{}/{}", self.cloud_auth_url(), cloud_auth_id);
        let response = self
            .client
            .get(&url)
            .header(header::COOKIE, &cookie)
            .send()
            .await?;
        let status = response.status();
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let body = response.text().await?;
        if !status.is_success() {
            return Err(roblox_error(status, &body).context(format!("GET {url}")));
        }
        let v: Value =
            serde_json::from_str(&body).map_err(|e| unreadable_response("get", &e, &body))?;
        Ok(Some(v))
    }

    /// One page of the caller's keys.
    ///
    /// A POST that reads: the route is `POST /v1/apiKeys`, which is what the
    /// Creator Hub itself calls. Every `GET` spelling 404s, and
    /// `GET /v1/apiKey/list` answers "Malformed CloudAuthId" because `list`
    /// lands in the by-id route.
    ///
    /// `group_id` selects whose keys come back. Omitted, the answer is the
    /// authenticated user's own keys: a group's keys are simply absent rather
    /// than refused, so an empty result is not evidence the account has none.
    pub async fn list_api_keys(
        &self,
        cursor: &str,
        limit: u32,
        group_id: Option<u64>,
    ) -> Result<ListApiKeysResponse> {
        let cookie = self.cookie_header()?;
        let list_url = self.list_url();
        let body = serde_json::to_string(&ListBody {
            cursor,
            limit,
            reverse: false,
            group_id,
        })?;
        let response = self
            .send_with_csrf(|| {
                self.client
                    .request(Method::POST, &list_url)
                    .header(header::COOKIE, &cookie)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(body.clone())
            })
            .await?;
        let text = response.text().await?;
        serde_json::from_str(&text).map_err(|e| unreadable_response("list", &e, &text))
    }

    /// Every key, following the cursor to the end.
    pub async fn list_all_api_keys(&self, group_id: Option<u64>) -> Result<Vec<RemoteApiKey>> {
        const PAGE: u32 = 100;
        // A cursor that stops advancing would otherwise spin forever against a
        // remote we do not control. 200 pages is far past any real account.
        const MAX_PAGES: usize = 200;

        let mut out = Vec::new();
        let mut cursor = String::new();
        for _ in 0..MAX_PAGES {
            let page = self.list_api_keys(&cursor, PAGE, group_id).await?;
            out.extend(page.cloud_auth_info);
            match page.next_cursor {
                Some(next) if !next.is_empty() && next != cursor => cursor = next,
                _ => return Ok(out),
            }
        }
        bail!(
            "list_api_keys paged past {} pages without ending",
            MAX_PAGES
        )
    }

    pub async fn introspect_api_key(&self, secret: &str) -> Result<Value> {
        let body = serde_json::to_string(&IntrospectBody { api_key: secret })?;
        let response = self
            .client
            .post(INTROSPECT)
            .header(header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await?;
        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            return Err(roblox_error(status, &text).context("introspecting the key"));
        }
        serde_json::from_str(&text).map_err(|e| unreadable_response("introspect", &e, &text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(server: &MockServer) -> RbxApiKeyClient {
        RbxApiKeyClient::new(Some("test-cookie".into())).with_base_url(server.uri())
    }

    fn key(id: &str) -> Value {
        serde_json::json!({
            "id": id,
            "cloudAuthUserConfiguredProperties": { "name": id, "isEnabled": true }
        })
    }

    /// Mount one page, matched on the cursor the client sends for it.
    async fn page(server: &MockServer, cursor: &str, body: Value) {
        Mock::given(method("POST"))
            .and(path("/cloud-authentication/v1/apiKeys"))
            .and(body_partial_json(serde_json::json!({ "cursor": cursor })))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    /// A 2xx whose body does not parse used to come back with the body in the
    /// error, `apikeySecret` and all, and the error is printed.
    #[tokio::test]
    async fn an_unreadable_create_or_regenerate_answer_keeps_its_secret_out_of_the_error() {
        let server = MockServer::start().await;
        // `cloudAuthInfo` as a string: serde quotes the string it did not
        // expect, so the secret is in the parse error as well as the body.
        let body = serde_json::json!({
            "cloudAuthInfo": "not-a-real-secret",
            "apikeySecret": "not-a-real-secret"
        });
        Mock::given(method("POST"))
            .and(path("/cloud-authentication/v1/apiKey"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&body))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(
                "/cloud-authentication/v1/apiKey/aaaaBBBBccccDDDD/regenerate",
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("apikeySecret=not-a-real-secret"),
            )
            .mount(&server)
            .await;
        let client = client(&server);
        let props = ConfigProperties {
            name: "deploy".into(),
            description: String::new(),
            is_enabled: true,
            expiration_time: None,
            allowed_cidrs: Vec::new(),
            scopes: Vec::new(),
        };

        let created = format!("{:#}", client.create_api_key(&props).await.unwrap_err());
        assert!(!created.contains("not-a-real-secret"), "{created}");
        assert!(created.contains("create response"), "{created}");
        assert!(created.contains("top-level fields"), "{created}");
        assert!(created.contains("cloudAuthInfo"), "{created}");

        let rotated = format!(
            "{:#}",
            client
                .regenerate_secret("aaaaBBBBccccDDDD")
                .await
                .unwrap_err()
        );
        assert!(!rotated.contains("not-a-real-secret"), "{rotated}");
        assert!(rotated.contains("regenerate response"), "{rotated}");
    }

    /// `{:?}` on an answer carrying a secret shows that it is there, not what
    /// it is.
    #[test]
    fn debug_never_prints_a_secret() {
        let created: CreateApiKeyResponse = serde_json::from_value(serde_json::json!({
            "cloudAuthInfo": { "id": "aaaaBBBBccccDDDD" },
            "apikeySecret": "not-a-real-secret"
        }))
        .unwrap();
        let rotated = RegenerateResponse {
            apikey_secret: "not-a-real-secret".into(),
        };
        let asked = IntrospectBody {
            api_key: "not-a-real-secret",
        };

        for text in [
            format!("{created:?}"),
            format!("{rotated:?}"),
            format!("{asked:?}"),
        ] {
            assert!(!text.contains("not-a-real-secret"), "{text}");
            assert!(text.contains(REDACTED), "{text}");
        }
        assert!(format!("{created:?}").contains("aaaaBBBBccccDDDD"));
    }

    #[tokio::test]
    async fn every_page_is_followed_to_the_end() {
        // The path that had never run: one page covers most accounts, so
        // pagination shipped unexercised.
        let server = MockServer::start().await;
        page(
            &server,
            "",
            serde_json::json!({
                "cloudAuthInfo": [key("a"), key("b")],
                "nextCursor": "page2"
            }),
        )
        .await;
        page(
            &server,
            "page2",
            serde_json::json!({ "cloudAuthInfo": [key("c")] }),
        )
        .await;

        let keys = client(&server).list_all_api_keys(None).await.unwrap();
        let ids: Vec<&str> = keys.iter().map(|k| k.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn an_empty_next_cursor_ends_the_walk() {
        // Roblox sends "" rather than omitting the field when the listing is
        // exhausted. Treating that as a cursor would re-request page one.
        let server = MockServer::start().await;
        page(
            &server,
            "",
            serde_json::json!({ "cloudAuthInfo": [key("a")], "nextCursor": "" }),
        )
        .await;

        let keys = client(&server).list_all_api_keys(None).await.unwrap();
        assert_eq!(keys.len(), 1);
    }

    #[tokio::test]
    async fn a_cursor_that_stops_advancing_terminates() {
        // A remote we do not control handing back the cursor it was given
        // would otherwise spin forever against a live API.
        let server = MockServer::start().await;
        page(
            &server,
            "",
            serde_json::json!({ "cloudAuthInfo": [key("a")], "nextCursor": "stuck" }),
        )
        .await;
        page(
            &server,
            "stuck",
            serde_json::json!({ "cloudAuthInfo": [key("b")], "nextCursor": "stuck" }),
        )
        .await;

        let keys = client(&server).list_all_api_keys(None).await.unwrap();
        assert_eq!(keys.len(), 2, "should stop once the cursor repeats");
    }

    #[tokio::test]
    async fn group_id_is_sent_only_when_asked_for() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/cloud-authentication/v1/apiKeys"))
            .and(body_partial_json(
                serde_json::json!({ "groupId": 445566778, "reverse": false }),
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"cloudAuthInfo": []})),
            )
            .expect(1)
            .mount(&server)
            .await;

        client(&server)
            .list_all_api_keys(Some(445566778))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_listing_without_group_id_omits_the_field_rather_than_sending_null() {
        // `groupId: null` is not the same request as no `groupId`, and only
        // one of them means "my own keys".
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/cloud-authentication/v1/apiKeys"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"cloudAuthInfo": []})),
            )
            .mount(&server)
            .await;

        client(&server).list_all_api_keys(None).await.unwrap();

        let sent = &server.received_requests().await.unwrap()[0];
        let body: Value = serde_json::from_slice(&sent.body).unwrap();
        assert!(
            body.get("groupId").is_none(),
            "groupId must be absent, got {body}"
        );
    }
}
