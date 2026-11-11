//! Where the human output of a command goes.
//!
//! Normally stdout. Under `create --stdout` and `regenerate --stdout`, stdout
//! carries the secret and nothing else, so a pipe into a vault receives exactly
//! the secret: every progress line, warning and summary moves to stderr. The
//! prompts already render on stderr (dialoguer's default).
//!
//! A process-wide switch rather than a parameter threaded through every helper
//! that prints, because the helpers that print are many and the rule is one:
//! once stdout is reserved, nothing but the secret may reach it.

use std::sync::atomic::{AtomicBool, Ordering};

static STDOUT_RESERVED: AtomicBool = AtomicBool::new(false);

/// What a secret prints as under `{:?}`. Every type holding one implements
/// `Debug` by hand with this in its place.
pub const REDACTED: &str = "<redacted>";

/// Reserve stdout for the secret for the rest of the process.
pub fn reserve_stdout() {
    STDOUT_RESERVED.store(true, Ordering::Relaxed);
}

pub fn stdout_reserved() -> bool {
    STDOUT_RESERVED.load(Ordering::Relaxed)
}

/// `println!`, except on stderr once stdout is reserved for a secret.
macro_rules! say {
    ($($arg:tt)*) => {
        if $crate::out::stdout_reserved() {
            eprintln!($($arg)*)
        } else {
            println!($($arg)*)
        }
    };
}

/// Hand a secret nothing could store to the reader, to save by hand: Roblox
/// will not show it again.
///
/// On stderr whatever stdout is doing. stdout is what gets piped into a file
/// or captured by CI as the command's output, and a secret printed there is
/// one more copy nobody asked to keep.
pub fn hand_over_secret(secret: &str) {
    use colored::Colorize;
    eprintln!(
        "{}",
        format!("  Secret (save this manually): {secret}").yellow()
    );
}

/// Write the secret to stdout, with no trailing newline, as `resolve` does.
pub fn emit_secret(secret: &str) -> anyhow::Result<()> {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    out.write_all(secret.as_bytes())?;
    out.flush()?;
    Ok(())
}
