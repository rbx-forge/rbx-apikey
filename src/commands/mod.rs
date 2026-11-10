pub mod catalog;
pub mod create;
pub mod delete;
pub mod introspect;
pub mod list;
pub mod permissions;
pub mod prune;
pub mod regenerate;
pub mod remote;
pub mod resolve;
pub mod scopes;
pub mod status;
pub mod update;

use anyhow::{bail, Context, Result};

use crate::api::RbxApiKeyClient;
use crate::config;
use rbx_core::places::PlacesFile;
use rbx_core::GlobalFlags;

/// What a command says when the name it was handed is not a key in
/// `rbxapikey.toml`.
///
/// One wording for both `create` and `update`, because the interesting case is
/// the same in each and it is not the typo: a declaration that fans out into
/// one key per env group is not itself a key, so `rbx-apikey create deploy`
/// matches nothing while `[keys.deploy]` sits there in the file. Saying only
/// "not in rbxapikey.toml" is true and reads like the edit was never saved.
pub fn missing_key_note(cfg: &config::Config, name: &str) -> String {
    let generated = config::keys_from_declaration(cfg, name);
    if generated.is_empty() {
        return format!("skipping \"{}\": not in {}", name, config::FILE);
    }
    format!(
        "skipping \"{}\": it declares one key per env group rather than a key of its own - \
         name one of {}",
        name,
        generated.join(", ")
    )
}

/// Add the missing half of Roblox's `InvalidNameOrDescription`.
///
/// The refusal names neither of the two fields it covers, and gives no rule for
/// either, so the first suspicion lands on the scopes or the cookie and the
/// real cause costs a session to find.
///
/// The working rule, measured against the live API: `rbx` or `roblox` glued
/// to an API or commerce term is rejected. It was first seen on key *names*;
/// the description half was confirmed on 2026-08-22, when a description reading ``Validate `rbx
/// secret` against real Open Cloud.`` was refused and the same declaration with
/// the brand taken out was accepted with nothing else changed. A key named
/// `secrettest` passed in the same session, which is what the rule predicts:
/// the commerce term alone is fine, the brand welded to it is not.
///
/// **Nothing is refused locally, and the rule is why.** "Glued to an API or
/// commerce term" is a judgement, not a substring. A preflight check can only
/// approximate it, and every approximation rejects text Roblox accepts, which
/// is a worse failure than the one it replaces and an invisible one: nobody
/// debugs a check that fires wrongly on a string the server would have taken.
/// So this explains the error once Roblox has returned it, and quotes both
/// candidate values, because which of the two carries a brand is the one thing
/// a reader sees at a glance and the server will not say.
///
/// Leaves any other error untouched.
/// Name the scope entries a failed creation sent, when Roblox answers 5xx.
///
/// The key service does not reject a scope it cannot target. It throws, and
/// the reply is `500 Internal Server Error: Exception was thrown by handler`,
/// which names neither the scope nor the field. Finding out that `group` was
/// the one scope whose target the embedded catalog had wrong cost a day of
/// bisecting a four-line declaration.
///
/// The request is the one thing this side knows for certain, so it prints it.
/// Client errors are left alone: a 400 says what it objected to.
pub fn explain_opaque_server_error(
    error: anyhow::Error,
    scopes: &[crate::scope_builder::ScopeDef],
) -> anyhow::Error {
    match rbx_core::api::api_status(&error) {
        Some(status) if status.is_server_error() => {}
        _ => return error,
    }

    let sent = scopes
        .iter()
        .map(|s| {
            format!(
                "  {}:{} targetParts={:?}",
                s.scope_type,
                s.operations.join(","),
                s.target_parts
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    error.context(format!(
        "Roblox failed on this request rather than refusing it, so it names no \
         field. These are the scope entries that were sent:\n{sent}\n\nA scope \
         carrying a target part that it does not accept fails exactly this way. \
         `rbx-apikey scopes list` shows which target each scope takes, and \
         `catalog regenerate` refreshes that from the key service."
    ))
}

pub fn explain_invalid_name_or_description(
    error: anyhow::Error,
    name: &str,
    description: &str,
) -> anyhow::Error {
    // The chain rather than the outermost error, for the reason
    // `rbx_core::api::explain_missing_scope` documents: `to_string()` renders
    // only the top frame, so a caller that added context first would hide the
    // body this match needs.
    let says_so = error
        .chain()
        .any(|cause| cause.to_string().contains("InvalidNameOrDescription"));
    if !says_so {
        return error;
    }
    // A 5xx echoing the token means Roblox broke, not that the name is wrong,
    // and sending somebody off rewording a valid declaration gets them nowhere.
    if let Some(status) = rbx_core::api::api_status(&error) {
        if !status.is_client_error() {
            return error;
        }
    }
    error.context(format!(
        "Roblox refused the name or the description, and does not say which. \
         Sent name: \"{name}\". Sent description: \"{description}\". A description \
         naming Roblox or this tool has been refused before, so try rewording \
         either field to drop brand names and retry. Set `description` on the key \
         in {} to control the text outright.",
        config::FILE
    ))
}

/// Build the HTTP client on whatever cookie the global flags resolve to.
///
/// No `.or_else` fallback. There used to be one, onto a second lookup that did
/// not honour `--no-auto-cookie`, which is how the flag came to be ignored by
/// every subcommand here. `resolve_cookie` is the whole chain, so this has
/// nothing left to add.
pub fn make_client(global: &GlobalFlags) -> RbxApiKeyClient {
    RbxApiKeyClient::new(global.resolve_cookie())
}

/// `rbxplace.toml`, or `None` when there is none.
///
/// Absent is not an error here: a key that names its universes by id needs no
/// env file, and the one that names an env says so when it is resolved. A file
/// that exists and does not parse is still an error, and so is one that cannot
/// be looked at: read as absent, it switched the drift check off without a
/// word, since there was no env file to compare the lockfile against.
pub fn load_places(global: &GlobalFlags) -> Result<Option<PlacesFile>> {
    let there = global
        .places
        .try_exists()
        .with_context(|| format!("failed to look at {}", global.places.display()))?;
    if !there {
        return Ok(None);
    }
    PlacesFile::load(&global.places).map(Some)
}

/// A run decided before it starts, one entry per key: what the run will do
/// for it, or the note it prints instead of doing anything.
///
/// Built once, and read by both the git guard and the loop that does the
/// work, so the two cannot disagree about which secrets are written where.
pub(crate) type Plan<T> = Vec<(String, std::result::Result<T, String>)>;

/// Every secret a planned run will write, checked against git in one go,
/// before the first prompt. `destination` says where an entry writes one, if
/// it does.
pub(crate) fn guard_plan<T>(
    plan: &[(String, std::result::Result<T, String>)],
    destination: fn(&T) -> Option<&crate::secret_store::Resolved>,
    lock_path: &std::path::Path,
    write: crate::git_guard::Write,
) -> Result<()> {
    let writes: Vec<(String, crate::secret_store::Resolved)> = plan
        .iter()
        .filter_map(|(name, step)| {
            let resolved = destination(step.as_ref().ok()?)?;
            Some((name.clone(), resolved.clone()))
        })
        .collect();
    crate::git_guard::check_all(&writes, lock_path, write)
}

/// For [`guard_plan`]: a plan whose every entry writes a secret.
pub(crate) fn writes(r: &crate::secret_store::Resolved) -> Option<&crate::secret_store::Resolved> {
    Some(r)
}

pub fn require_no_collision(all: bool, name: Option<&str>) -> Result<()> {
    if all && name.is_some() {
        bail!("--all and <key> are mutually exclusive");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(no_auto_cookie: bool) -> GlobalFlags {
        GlobalFlags {
            api_key: None,
            cookie: None,
            no_auto_cookie,
            auto_cookie: false,
            env: None,
            place: None,
            places: "rbxplace.toml".into(),
            universe_id: None,
            place_id: Vec::new(),
        }
    }

    /// An `rbxplace.toml` that cannot be looked at stops the command, naming
    /// it. Read as absent, the drift check had nothing to compare against
    /// and skipped without a word.
    #[test]
    fn an_env_file_that_cannot_be_looked_at_is_not_an_absent_one() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut global = flags(true);
        global.places = crate::test_support::unlookable(dir.path());

        let err = match load_places(&global) {
            Ok(_) => panic!("an env file that cannot be looked at is not absent"),
            Err(e) => format!("{e:#}"),
        };
        assert!(err.contains("failed to look at"), "got: {err}");

        // And an absent one is still no env file at all.
        global.places = dir.path().join("rbxplace.toml");
        assert!(load_places(&global).expect("absent is fine").is_none());
    }

    /// `--no-auto-cookie`, from this side. `resolve_cookie` owns the whole
    /// decision, so the only way this crate can get it wrong is by adding a
    /// source of its own, which is how the flag once stopped working.
    ///
    /// Asserting the two agree, rather than asserting a particular cookie,
    /// keeps this true on a machine with Studio signed in and in CI without.
    /// What `resolve_cookie` decides is `rbx-core`'s business and is tested
    /// there against a seam; what this owns is adding nothing to it.
    #[test]
    fn make_client_takes_the_resolved_cookie_and_adds_nothing() {
        for no_auto_cookie in [false, true] {
            let global = flags(no_auto_cookie);
            assert_eq!(
                make_client(&global).cookie(),
                global.resolve_cookie().as_deref(),
                "make_client must not resolve a cookie of its own (--no-auto-cookie: \
                 {no_auto_cookie})"
            );
        }
    }

    /// The name of a fan-out declaration matches no key, and the useful half
    /// of the answer is which keys it did produce.
    #[test]
    fn naming_a_declaration_instead_of_a_key_lists_the_keys_it_made() {
        let cfg = config::load_from(std::path::Path::new(&write_config(
            "fanout",
            "[keys.deploy]\nscopes = [\"universe:read\"]\n\n\
             [keys.deploy.envs]\nci = [\"dev\"]\nprod = [\"prod\"]\n",
        )))
        .unwrap();

        let note = missing_key_note(&cfg, "deploy");
        assert!(note.contains("deploy_ci"), "{note}");
        assert!(note.contains("deploy_prod"), "{note}");

        // A name that is nobody's declaration keeps the plain answer.
        let note = missing_key_note(&cfg, "nosuchkey");
        assert!(note.contains("not in rbxapikey.toml"), "{note}");
        // And a real key never reaches this message at all.
        assert!(config::get(&cfg, "deploy_ci").is_some());
    }

    /// A key declared without a `description` is handed the fallback, and the
    /// fallback is what Roblox refuses when it carries a brand. The failure
    /// only appears against the live API and a reviewer has no reason to look
    /// for it, so the string itself is the thing asserted.
    #[test]
    fn the_fallback_never_carries_the_brand() {
        let cfg = config::load_from(std::path::Path::new(&write_config(
            "nodesc",
            "[keys.deploy]\nscopes = [\"universe:read\"]\n",
        )))
        .unwrap();
        let key_cfg = config::get(&cfg, "deploy").expect("the key is declared");

        // Both arms of the fallback: with universes listed and without.
        for text in [
            update::build_description("deploy", key_cfg, &[]),
            update::build_description("deploy", key_cfg, &[123, 456]),
        ] {
            let lowered = text.to_lowercase();
            assert!(!lowered.contains("rbx"), "got: {text}");
            assert!(!lowered.contains("roblox"), "got: {text}");
            // And it still says which declaration the key came from, which is
            // the only thing the string is for.
            assert!(text.contains("deploy"), "got: {text}");
        }
    }

    /// `label` keeps two tests from writing different files to one path: the
    /// suite is threaded, and a shared name makes them race.
    fn write_config(label: &str, text: &str) -> String {
        let dir =
            std::env::temp_dir().join(format!("rbxapikey_note_{}_{label}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(config::FILE);
        std::fs::write(&path, text).unwrap();
        path.to_string_lossy().to_string()
    }

    #[test]
    fn an_explicit_cookie_reaches_the_client() {
        let mut global = flags(false);
        global.cookie = Some("explicit".into());
        assert_eq!(make_client(&global).cookie(), Some("explicit"));
    }

    /// The hint's whole value is that it appears exactly when it applies.
    mod explain_invalid_name_or_description {
        use super::super::explain_invalid_name_or_description as explain;
        use reqwest::StatusCode;

        /// Substring of the advice, enough to tell it apart from the original.
        const HINT: &str = "does not say which";

        /// Built the way production builds it. A refusal from
        /// `create_api_key` leaves `send_with_csrf` as a `CsrfError::Refused`
        /// and reaches the chain through `roblox_error`, which keeps **only**
        /// `userFacingMessage` or `message` out of a legacy envelope and
        /// discards the rest. Constructing an `ApiError` directly would skip
        /// exactly the step that can drop the token the hint matches on, which
        /// is to say it would test everything except the assumption.
        fn refusal(status: StatusCode, body: &str) -> anyhow::Error {
            rbx_core::api::roblox_error(status, body)
        }

        mod opaque_server_error {
            use super::refusal;
            use crate::commands::explain_opaque_server_error;
            use crate::scope_builder::ScopeDef;
            use reqwest::StatusCode;

            fn group_entry() -> Vec<ScopeDef> {
                vec![ScopeDef {
                    scope_type: "group".to_string(),
                    target_parts: vec!["G1234567890".to_string()],
                    operations: vec!["read".to_string(), "write".to_string()],
                }]
            }

            /// The body observed against `/cloud-authentication/v1/apiKey`
            /// when a group scope carried a target part.
            const OBSERVED: &str = "Exception was thrown by handler.";

            #[test]
            fn a_5xx_prints_the_scope_entries_that_were_sent() {
                let explained = explain_opaque_server_error(
                    refusal(StatusCode::INTERNAL_SERVER_ERROR, OBSERVED),
                    &group_entry(),
                );
                let text = format!("{explained:#}");

                assert!(text.contains("group:read,write"), "got: {text}");
                assert!(text.contains("G1234567890"), "got: {text}");
                // The original sentence has to survive the added context, or
                // the reader loses what Roblox actually said.
                assert!(text.contains(OBSERVED), "got: {text}");
            }

            /// A 400 says what it objected to, so adding the payload to it is
            /// noise rather than help.
            #[test]
            fn a_client_error_is_left_alone() {
                let explained = explain_opaque_server_error(
                    refusal(StatusCode::BAD_REQUEST, "InvalidScopes"),
                    &group_entry(),
                );
                let text = format!("{explained:#}");

                assert!(!text.contains("targetParts"), "got: {text}");
            }
        }

        /// The body observed on 2026-08-22 against
        /// `/cloud-authentication/v1/apiKey`, which rendered as
        /// `API error 400 Bad Request: Response.InvalidNameOrDescription`.
        const OBSERVED: &str = "Response.InvalidNameOrDescription";

        #[test]
        fn the_refusal_gets_both_values_it_could_have_been() {
            let explained = explain(
                refusal(StatusCode::BAD_REQUEST, OBSERVED),
                "deploy_ci",
                "Managed declaratively (deploy_ci).",
            );
            let text = explained.to_string();
            assert!(text.contains(HINT), "got: {text}");
            assert!(text.contains("deploy_ci"), "got: {text}");
            assert!(text.contains("Managed declaratively"), "got: {text}");
            assert!(text.contains("rbxapikey.toml"), "got: {text}");
        }

        /// The legacy envelope shape, which is what `roblox_error` exists for.
        /// `message` is the only place the token can be, so the hint has to
        /// survive the unwrapping.
        #[test]
        fn the_token_survives_a_legacy_envelope_that_carries_it_as_the_message() {
            let explained = explain(
                refusal(
                    StatusCode::BAD_REQUEST,
                    r#"{"errors":[{"code":5,"message":"Response.InvalidNameOrDescription"}]}"#,
                ),
                "deploy_ci",
                "whatever",
            );
            assert!(explained.to_string().contains(HINT), "got: {explained}");
        }

        /// **A known limit, pinned rather than hidden.** `roblox_error` prefers
        /// `userFacingMessage` and discards the rest, so an envelope that keeps
        /// the token only in `message` *behind* a generic user-facing string
        /// loses it before this function ever sees it, and the hint does not
        /// fire. Matching on the token is still right: the alternative is
        /// keying on 400-from-this-endpoint, which would fire on every other
        /// way a key can be refused. Roblox has not been observed answering
        /// this shape; if it starts, this test is where the change lands.
        #[test]
        fn a_generic_user_facing_message_hides_the_token_and_the_hint_stays_away() {
            let explained = explain(
                refusal(
                    StatusCode::BAD_REQUEST,
                    r#"{"errors":[{"message":"Response.InvalidNameOrDescription",
                       "userFacingMessage":"Something went wrong."}]}"#,
                ),
                "deploy_ci",
                "whatever",
            );
            assert!(!explained.to_string().contains(HINT), "got: {explained}");
        }

        #[test]
        fn a_server_error_quoting_the_same_token_does_not() {
            // Roblox's 5xx echo upstream text. Telling somebody to reword a
            // valid declaration because Roblox is down gets them nowhere.
            let explained = explain(
                refusal(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "upstream said Response.InvalidNameOrDescription",
                ),
                "deploy_ci",
                "whatever",
            );
            assert!(!explained.to_string().contains(HINT), "got: {explained}");
        }

        #[test]
        fn an_unrelated_refusal_is_left_alone() {
            let explained = explain(
                refusal(StatusCode::BAD_REQUEST, "Response.InvalidScopes"),
                "deploy_ci",
                "whatever",
            );
            assert!(!explained.to_string().contains(HINT), "got: {explained}");
        }

        #[test]
        fn the_body_is_found_under_a_context_layer() {
            // `to_string()` renders only the top frame, so a caller that adds
            // context before asking would otherwise lose the hint entirely.
            let explained = explain(
                refusal(StatusCode::BAD_REQUEST, OBSERVED).context("creating \"deploy_ci\""),
                "deploy_ci",
                "whatever",
            );
            assert!(explained.to_string().contains(HINT), "got: {explained}");
        }
    }
}
