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

/// Write the secret to stdout, with no trailing newline, as `resolve` does.
pub fn emit_secret(secret: &str) -> anyhow::Result<()> {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    out.write_all(secret.as_bytes())?;
    out.flush()?;
    Ok(())
}
