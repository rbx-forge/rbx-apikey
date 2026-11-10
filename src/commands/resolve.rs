//! Print the raw secret to stdout (no trailing newline). Used by scripts.

use anyhow::{bail, Result};
use std::io::Write;

use crate::{config, lock, secret_store};

pub fn run(name: &str) -> Result<()> {
    let cfg = config::load()?;
    let lk = lock::load()?;

    let key_cfg = config::get(&cfg, name);
    let entry = lock::get(&lk, name).ok_or_else(|| {
        anyhow::anyhow!(
            "\"{}\" has no entry in {}. Run `rbx-apikey create {}` first.",
            name,
            lock::FILE,
            name
        )
    })?;

    let resolved = secret_store::stored_backend(&cfg, key_cfg, name, Some(entry));
    let secret = secret_store::read(&resolved, Some(entry))?;
    let secret = match secret {
        Some(s) => s,
        None => match resolved.backend {
            secret_store::Backend::External => bail!(
                "\"{}\": its secret was printed with --stdout and is not stored here. Read it \
                 from where you put it, or `rbx-apikey regenerate {}` for a new one.",
                name,
                name
            ),
            secret_store::Backend::File => bail!(
                "\"{}\": secret file {} not found or empty.",
                name,
                resolved.target
            ),
            secret_store::Backend::Lockfile => bail!(
                "\"{}\" has no secret in {}. Run `rbx-apikey regenerate {}`.",
                name,
                lock::FILE,
                name
            ),
        },
    };

    // No trailing newline so it composes into shell command substitution.
    let mut out = std::io::stdout().lock();
    out.write_all(secret.as_bytes())?;
    out.flush()?;
    Ok(())
}
