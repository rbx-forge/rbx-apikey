//! `rbx-apikey introspect <key>`: print what Roblox has stored for a key.
//! Requires the JWT inside the secret to still be valid (~1h after create/regenerate).

use anyhow::{anyhow, bail, Result};

use crate::{config, lock, secret_store};
use rbx_core::GlobalFlags;

use super::make_client;

pub async fn run(global: &GlobalFlags, name: &str) -> Result<()> {
    let cfg = config::load()?;
    let lk = lock::load()?;

    let key_cfg = config::get(&cfg, name);
    let entry = lock::get(&lk, name)
        .ok_or_else(|| anyhow!("\"{}\" has no entry in {}", name, lock::FILE))?;

    let resolved = secret_store::stored_backend(&cfg, key_cfg, name, Some(entry));
    let secret = secret_store::read(&resolved, Some(entry)).ok_or_else(|| {
        anyhow!(
            "\"{}\" secret not available from backend \"{}\" (target: {})",
            name,
            resolved.backend.as_str(),
            resolved.target
        )
    })?;

    let client = make_client(global);
    match client.introspect_api_key(&secret).await {
        Ok(resp) => {
            println!("{}", serde_json::to_string_pretty(&resp)?);
            Ok(())
        }
        // `{:#}` and not `{}`: the client wraps this in
        // `.context("introspecting the key")`, and `{}` renders only that
        // outermost frame. This printed "introspect failed: introspecting the
        // key", so the status and body never reached the terminal and the
        // hint below read as the diagnosis rather than as one guess.
        Err(e) => bail!(
            "introspect failed: {:#}\n\nIf that says the token is invalid or expired: the JWT inside the secret only works for about an hour after create or regenerate. `rbx-apikey regenerate {}` issues a fresh one, then introspect again.",
            e,
            name
        ),
    }
}
