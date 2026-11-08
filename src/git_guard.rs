//! Refuse to write a key secret into a file git is tracking.
//!
//! The default secret backend is the lockfile, written in the working
//! directory, and five pages of documentation state that
//! `rbxapikey.lock.toml` "is gitignored". Nothing made that true. The tool
//! never wrote a `.gitignore`, never checked for one, and said nothing on the
//! way past: `rbx-apikey create` printed where the secret went only when it
//! went to a *file* backend, and stayed silent on the default.
//!
//! So the documented happy path: `rbx-apikey create`, then `git add -A`:
//! ends with a live Open Cloud key in a public repository, and
//! every page the reader consulted told them it could not.
//!
//! ## Why this refuses rather than fixes
//!
//! Writing into somebody's `.gitignore` is not this tool's to do: it is the
//! rule the project already follows for formatter and editor config, and a
//! file git reads is more the user's than a formatter's is. What is this
//! tool's to do is not create a credential it is about to leave somewhere
//! dangerous.
//!
//! The check therefore runs **before the key exists on Roblox**. Refusing
//! afterwards would be worse than saying nothing: the key would be live, its
//! secret unsaved, and the operator left to clean up a key they cannot
//! authenticate as.
//!
//! ## Why `git check-ignore` rather than parsing `.gitignore`
//!
//! Because the question is "does git ignore this path", and `.gitignore`
//! files nest, negate, and are joined by `.git/info/exclude` and the global
//! core.excludesFile. Reimplementing that is how a guard ends up confidently
//! wrong. When git is not on PATH the question cannot be answered, and an
//! unanswerable check warns rather than blocks: the same rule the session
//! check follows.

use std::path::{Path, PathBuf};
use std::process::Command;

/// What a lockfile's relationship to git turned out to be.
#[derive(Debug, PartialEq, Eq)]
pub enum GitStatus {
    /// Not inside a git work tree: nothing to leak into.
    NotARepo,
    /// Inside a repository, and git ignores the path.
    Ignored,
    /// Inside a repository, and git does **not** ignore the path.
    Tracked,
    /// The question could not be answered, with the reason.
    Unknown(String),
}

/// Whether `path` sits inside a git work tree and, if so, whether git ignores
/// it.
pub fn status_of(path: &Path) -> GitStatus {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    let dir = match dir {
        Some(d) => d.to_path_buf(),
        None => PathBuf::from("."),
    };

    if !in_work_tree(&dir) {
        return GitStatus::NotARepo;
    }

    // `check-ignore` exits 0 when the path is ignored, 1 when it is not, and
    // 128 when something else went wrong, which is the case that must not be
    // read as either answer.
    match Command::new("git")
        .arg("-C")
        .arg(&dir)
        .arg("check-ignore")
        .arg("--quiet")
        .arg(path)
        .status()
    {
        Ok(s) if s.code() == Some(0) => GitStatus::Ignored,
        Ok(s) if s.code() == Some(1) => GitStatus::Tracked,
        Ok(s) => GitStatus::Unknown(format!("git check-ignore exited {}", s)),
        Err(e) => GitStatus::Unknown(format!("git could not be run: {e}")),
    }
}

/// Walk up looking for `.git`, rather than asking git.
///
/// One process fewer in the common case of somebody working outside a
/// repository, and it answers the same question: `.git` is a directory in a
/// normal clone and a file in a worktree or submodule, so its kind is not
/// checked.
fn in_work_tree(dir: &Path) -> bool {
    let mut cur = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    loop {
        if cur.join(".git").exists() {
            return true;
        }
        match cur.parent() {
            Some(p) => cur = p.to_path_buf(),
            None => return false,
        }
    }
}

/// The refusal, worded so the reader can act without opening the docs.
pub fn refusal(path: &Path) -> String {
    format!(
        "{} would hold the key secret in plain text, and git is not ignoring it.\n\n\
         Creating the key now would put a live Open Cloud credential one `git add` away from \
         being published, which is the one failure this tool exists to prevent. Nothing has been \
         created.\n\n\
         Add this line to .gitignore, then run the same command again:\n\n    \
         {}\n\n\
         Or keep the secret out of the lockfile entirely, which is the better answer for a \
         shared repository:\n\n    \
         [settings]\n    default_secret_file = \".secrets/{{name}}.env\"\n\n\
         and gitignore `.secrets/`.",
        path.display(),
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "rbxapikey.lock.toml".to_string()),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// A directory with no repository above it has nothing to leak into, and
    /// must not be refused: plenty of people keep a project outside git.
    #[test]
    fn a_path_outside_any_repository_is_not_a_refusal() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            status_of(&dir.path().join("rbxapikey.lock.toml")),
            GitStatus::NotARepo
        );
    }

    /// The case the guard exists for, and the one the docs claimed could not
    /// happen: a repository whose `.gitignore` does not cover the lockfile.
    #[test]
    fn a_lockfile_a_repository_does_not_ignore_is_tracked() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .arg("init")
            .arg("--quiet")
            .status()
            .map(|s| s.success())
            .unwrap_or(false));

        let lock = dir.path().join("rbxapikey.lock.toml");
        std::fs::write(&lock, "").unwrap();
        assert_eq!(status_of(&lock), GitStatus::Tracked);
    }

    /// And the case the documentation describes, which must pass silently.
    #[test]
    fn a_lockfile_the_repository_ignores_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .arg("init")
            .arg("--quiet")
            .status()
            .map(|s| s.success())
            .unwrap_or(false));
        std::fs::write(dir.path().join(".gitignore"), "rbxapikey.lock.toml\n").unwrap();

        let lock = dir.path().join("rbxapikey.lock.toml");
        std::fs::write(&lock, "").unwrap();
        assert_eq!(status_of(&lock), GitStatus::Ignored);
    }

    /// The refusal has to carry both ways out, since the reader is stopped
    /// mid-task and the docs they would consult are the ones that were wrong.
    #[test]
    fn the_refusal_names_the_line_to_add_and_the_alternative() {
        let text = refusal(Path::new("rbxapikey.lock.toml"));
        assert!(text.contains("rbxapikey.lock.toml"), "{text}");
        assert!(text.contains("default_secret_file"), "{text}");
        assert!(text.contains("Nothing has been created"), "{text}");
    }
}
