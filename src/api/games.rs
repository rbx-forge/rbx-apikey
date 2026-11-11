//! Public games / develop endpoints used for universe → owner resolution.

use anyhow::{bail, Context, Result};
use rbx_core::api::execute_json;
use rbx_core::owner::OwnerType;
use reqwest::header;
use serde::Deserialize;

use crate::lock::UniverseOwner;

use super::RbxApiKeyClient;

#[derive(Debug, Deserialize)]
struct GamesResponse {
    #[serde(default)]
    data: Vec<GameEntry>,
}

#[derive(Debug, Deserialize)]
struct GameEntry {
    #[serde(default)]
    creator: Option<GameCreator>,
}

#[derive(Debug, Deserialize)]
struct GameCreator {
    #[serde(default, rename = "type")]
    ty: String,
    #[serde(default)]
    id: u64,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct DevelopUniverseInfo {
    creator_type: String,
    creator_target_id: u64,
}

/// Resolves a place id to the universe that contains it. Public, no auth.
const PLACE_UNIVERSE_URL: &str = "https://apis.roblox.com/universes/v1/places";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaceUniverse {
    /// Null when the id is not a place, which is how the endpoint reports it
    /// rather than by failing.
    #[serde(default)]
    universe_id: Option<u64>,
}

/// The universe containing `place_id`, or `None` when the id is not a place.
///
/// Only called where the caller said the id *is* a place. Guessing was tried
/// and removed: the two id spaces overlap, and not rarely. One live game's
/// universe id is simultaneously a valid place id in a different universe, so
/// an id the tool interprets for you can answer about somebody else's game.
pub(crate) async fn place_to_universe(
    client: &reqwest::Client,
    place_id: u64,
) -> Result<Option<u64>> {
    let url = format!("{PLACE_UNIVERSE_URL}/{place_id}/universe");
    let found: PlaceUniverse = execute_json(|| {
        let request = client.get(&url);
        async move { request.send().await.map_err(Into::into) }
    })
    .await
    .with_context(|| format!("resolving place {place_id}"))?;
    Ok(found.universe_id)
}

impl RbxApiKeyClient {
    /// [`place_to_universe`] through this client, refusing an id that is not
    /// a place.
    pub async fn fetch_place_universe(&self, place_id: u64) -> Result<u64> {
        place_to_universe(&self.client, place_id)
            .await?
            .with_context(|| format!("{place_id} is not a place id"))
    }

    pub async fn fetch_universe_owner(&self, universe_id: u64) -> Result<UniverseOwner> {
        // Public games API first (no auth required for public universes).
        let public_url = format!(
            "https://games.roblox.com/v1/games?universeIds={}",
            universe_id
        );
        let resp = self.client.get(&public_url).send().await?;
        if resp.status().is_success() {
            let body = resp.text().await?;
            if let Ok(data) = serde_json::from_str::<GamesResponse>(&body) {
                if let Some(game) = data.data.into_iter().next() {
                    if let Some(creator) = game.creator {
                        if creator.id != 0 {
                            let owner_type = if creator.ty.eq_ignore_ascii_case("Group") {
                                OwnerType::Group
                            } else {
                                OwnerType::User
                            };
                            return Ok(UniverseOwner {
                                universe_id,
                                owner_type,
                                owner_id: creator.id,
                            });
                        }
                    }
                }
            }
        }

        // Private universe → develop API with cookie.
        let cookie = self.cookie_header()?;
        let url = format!("https://develop.roblox.com/v1/universes/{}", universe_id);
        let resp = self
            .client
            .get(&url)
            .header(header::COOKIE, &cookie)
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            bail!(
                "universe {}: not found publicly and develop API returned {}",
                universe_id,
                status
            );
        }
        let info = parse_develop(universe_id, &body)?;
        if info.creator_target_id == 0 {
            bail!(
                "universe {}: develop API response missing creatorTargetId",
                universe_id
            );
        }
        let owner_type = if info.creator_type.eq_ignore_ascii_case("Group") {
            OwnerType::Group
        } else {
            OwnerType::User
        };
        Ok(UniverseOwner {
            universe_id,
            owner_type,
            owner_id: info.creator_target_id,
        })
    }
}

/// The develop answer about a universe, or an error that quotes none of it,
/// like every other answer this crate reads.
fn parse_develop(universe_id: u64, body: &str) -> Result<DevelopUniverseInfo> {
    serde_json::from_str(body)
        .map_err(|e| super::unreadable_response("develop", &e, body))
        .with_context(|| format!("universe {universe_id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unreadable_develop_answer_is_not_quoted() {
        let body = r#"{"creatorType": ["not-a-real-value"]}"#;
        let text = format!("{:#}", parse_develop(1234567890, body).unwrap_err());
        assert!(!text.contains("not-a-real-value"), "{text}");
        assert!(text.contains("universe 1234567890"), "{text}");
        assert!(text.contains("develop response"), "{text}");
    }
}
