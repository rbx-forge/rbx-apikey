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
//! ## Every write, both destinations
//!
//! Three commands put a secret on disk: `create` stores a new one,
//! `regenerate` stores the rotated one, and `update` moves one when
//! `secret_file` changed. Each runs [`check`] on the destination before it
//! writes, whether that is the lockfile or a `secret_file`. A secret file is
//! the user's own choice of location, but a choice is not a `.gitignore`
//! entry, and the README's `.secrets/` advice is only advice until something
//! checks it.
//!
//! ## Why this refuses rather than fixes
//!
//! Writing into somebody's `.gitignore` is not this tool's to do: it is the
//! rule the project already follows for formatter and editor config, and a
//! file git reads is more the user's than a formatter's is. What is this
//! tool's to do is not leave a credential somewhere dangerous.
//!
//! The check therefore runs **before Roblox is asked anything**. Refusing
//! afterwards would be worse than saying nothing: after a create the key
//! would be live with its secret unsaved, and after a rotation the old secret
//! would be dead and the new one nowhere.
//!
//! ## Why `git check-ignore` rather than parsing `.gitignore`
//!
//! Because the question is "does git ignore this path", and `.gitignore`
//! files nest, negate, and are joined by `.git/info/exclude` and the global
//! core.excludesFile. Reimplementing that is how a guard ends up confidently
//! wrong. When git is not on PATH, or a directory on the way cannot be looked
//! at, the question cannot be answered, and an unanswerable check warns
//! rather than blocks: the same rule the session check follows.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Result};

use crate::secret_store::{Backend, Resolved};

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

/// The write a refusal stops, so it can say what did not happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Write {
    /// `create`: a new key, and its first secret.
    Create,
    /// `regenerate`: a new secret for an existing key.
    Rotate,
    /// `update`: the existing secret, to the location the config now names.
    Move,
}

impl Write {
    fn doing(self) -> &'static str {
        match self {
            Write::Create => "Creating the key now",
            Write::Rotate => "Rotating the secret now",
            Write::Move => "Moving the secret there",
        }
    }

    fn nothing(self) -> &'static str {
        match self {
            Write::Create => "Nothing has been created.",
            Write::Rotate => "Nothing has been rotated: the key keeps the secret it has.",
            Write::Move => {
                "Nothing has been changed: the secret stays where it is, and the key was not \
                 updated."
            }
        }
    }
}

/// The file a secret resolved to `resolved` would be written into, or `None`
/// when nothing reaches the disk (`--stdout`).
pub fn destination(resolved: &Resolved, lock_path: &Path) -> Option<PathBuf> {
    match resolved.backend {
        Backend::Lockfile => Some(lock_path.to_path_buf()),
        Backend::File => Some(PathBuf::from(&resolved.target)),
        Backend::External => None,
    }
}

/// Stop before a secret is written where git would commit it.
///
/// `lock_path` is where the lockfile backend writes. Refuses when git does not
/// ignore the destination, warns on stderr when git cannot say, and passes
/// outside a repository. Callers run it before anything is sent to Roblox.
pub fn check(resolved: &Resolved, lock_path: &Path, write: Write) -> Result<()> {
    let Some(path) = destination(resolved, lock_path) else {
        return Ok(());
    };
    check_path(&path, resolved.backend == Backend::Lockfile, write)
}

fn check_path(path: &Path, is_lockfile: bool, write: Write) -> Result<()> {
    match status_of(path) {
        GitStatus::NotARepo | GitStatus::Ignored => Ok(()),
        GitStatus::Tracked => bail!(refusal(path, is_lockfile, write)),
        // An unanswerable check is not a verdict: git missing from PATH is not
        // evidence that the file is safe, nor that it is not.
        GitStatus::Unknown(why) => {
            eprintln!(
                "warning: could not check whether {} is gitignored ({why}); the key secret is \
                 stored there in plain text",
                path.display()
            );
            Ok(())
        }
    }
}

/// Whether `path` sits inside a git work tree and, if so, whether git ignores
/// it.
///
/// `path` need not exist yet, nor its directory: a secret file is usually
/// written into a `.secrets/` the first write creates.
pub fn status_of(path: &Path) -> GitStatus {
    // Absolute, because git resolves the path after `-C` has moved it into
    // `dir`: a relative `.secrets/deploy.env` would be looked up as
    // `.secrets/.secrets/deploy.env`.
    let path = match std::path::absolute(path) {
        Ok(p) => p,
        Err(e) => {
            return GitStatus::Unknown(format!("{} has no absolute form: {e}", path.display()))
        }
    };
    let dir = match nearest_existing_dir(&path) {
        Ok(d) => d,
        Err(why) => return GitStatus::Unknown(why),
    };

    match in_work_tree(&dir) {
        Ok(true) => {}
        Ok(false) => return GitStatus::NotARepo,
        Err(why) => return GitStatus::Unknown(why),
    }

    // `check-ignore` exits 0 when the path is ignored, 1 when it is not, and
    // 128 when something else went wrong, which is the case that must not be
    // read as either answer.
    match Command::new("git")
        .arg("-C")
        .arg(&dir)
        .arg("check-ignore")
        .arg("--quiet")
        .arg(&path)
        .status()
    {
        Ok(s) if s.code() == Some(0) => GitStatus::Ignored,
        Ok(s) if s.code() == Some(1) => GitStatus::Tracked,
        Ok(s) => GitStatus::Unknown(format!("git check-ignore exited {}", s)),
        Err(e) => GitStatus::Unknown(format!("git could not be run: {e}")),
    }
}

/// The closest directory above `path` that is there, for git to run in.
///
/// A directory that cannot be looked at is an error and not a reason to keep
/// climbing: past it, the answer would be about some other directory.
fn nearest_existing_dir(path: &Path) -> std::result::Result<PathBuf, String> {
    let mut cur = path.parent();
    while let Some(dir) = cur {
        match dir.try_exists() {
            Ok(true) => return Ok(dir.to_path_buf()),
            Ok(false) => cur = dir.parent(),
            Err(e) => return Err(format!("{} cannot be looked at: {e}", dir.display())),
        }
    }
    Err(format!("no directory above {} exists", path.display()))
}

/// Walk up looking for `.git`, rather than asking git.
///
/// One process fewer in the common case of somebody working outside a
/// repository, and it answers the same question: `.git` is a directory in a
/// normal clone and a file in a worktree or submodule, so its kind is not
/// checked. `try_exists` and not `exists`: a `.git` that cannot be looked at
/// is not an absent one, and reading it as absent would pass the check.
fn in_work_tree(dir: &Path) -> std::result::Result<bool, String> {
    let mut cur = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    loop {
        let git = cur.join(".git");
        match git.try_exists() {
            Ok(true) => return Ok(true),
            Ok(false) => {}
            Err(e) => return Err(format!("{} cannot be looked at: {e}", git.display())),
        }
        match cur.parent() {
            Some(p) => cur = p.to_path_buf(),
            None => return Ok(false),
        }
    }
}

/// The `.gitignore` line to suggest for `path`.
///
/// A secret file's directory rather than the file, when the config names one
/// relative to the project: `.secrets/` covers the next key too.
fn ignore_line(path: &Path, is_lockfile: bool) -> String {
    let file_name = || {
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| crate::lock::FILE.to_string())
    };
    if is_lockfile || path.is_absolute() {
        return file_name();
    }
    match path.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(dir) => format!("{}/", dir.to_string_lossy().replace('\\', "/")),
        None => file_name(),
    }
}

/// The refusal, worded so the reader can act without opening the docs.
pub fn refusal(path: &Path, is_lockfile: bool, write: Write) -> String {
    let mut text = format!(
        "{} would hold the key secret in plain text, and git is not ignoring it.\n\n\
         {} would put a live Open Cloud credential one `git add` away from being published, \
         which is the one failure this tool exists to prevent. {}\n\n\
         Add this line to .gitignore, then run the same command again:\n\n    \
         {}",
        path.display(),
        write.doing(),
        write.nothing(),
        ignore_line(path, is_lockfile),
    );
    if is_lockfile {
        text.push_str(
            "\n\nOr keep the secret out of the lockfile entirely, which is the better answer for \
             a shared repository:\n\n    \
             [settings]\n    default_secret_file = \".secrets/{name}.env\"\n\n\
             and gitignore `.secrets/`.",
        );
    }
    text
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// A fresh repository with nothing ignored, for the guard to refuse in.
    pub(crate) fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        assert!(Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .arg("init")
            .arg("--quiet")
            .status()
            .map(|s| s.success())
            .unwrap_or(false));
        dir
    }

    fn lockfile() -> Resolved {
        Resolved {
            backend: Backend::Lockfile,
            target: "deploy".into(),
        }
    }

    fn secret_file(path: &Path) -> Resolved {
        Resolved {
            backend: Backend::File,
            target: path.to_string_lossy().to_string(),
        }
    }

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
        let dir = repo();
        let lock = dir.path().join("rbxapikey.lock.toml");
        std::fs::write(&lock, "").unwrap();
        assert_eq!(status_of(&lock), GitStatus::Tracked);
    }

    /// And the case the documentation describes, which must pass silently.
    #[test]
    fn a_lockfile_the_repository_ignores_is_ignored() {
        let dir = repo();
        std::fs::write(dir.path().join(".gitignore"), "rbxapikey.lock.toml\n").unwrap();

        let lock = dir.path().join("rbxapikey.lock.toml");
        std::fs::write(&lock, "").unwrap();
        assert_eq!(status_of(&lock), GitStatus::Ignored);
    }

    /// A secret file is normally written into a directory the first write
    /// creates, so neither the file nor its directory is there when the check
    /// runs. The answer has to be about the path all the same.
    #[test]
    fn a_secret_file_not_written_yet_is_checked_by_its_path() {
        let dir = repo();
        let file = dir.path().join(".secrets").join("deploy.env");

        assert_eq!(status_of(&file), GitStatus::Tracked);

        std::fs::write(dir.path().join(".gitignore"), ".secrets/\n").unwrap();
        assert_eq!(status_of(&file), GitStatus::Ignored);
    }

    /// Both destinations through the one entry point the commands use.
    #[test]
    fn check_refuses_either_destination_git_does_not_ignore() {
        let dir = repo();
        let lock = dir.path().join("rbxapikey.lock.toml");
        let file = dir.path().join(".secrets").join("deploy.env");

        let err = check(&lockfile(), &lock, Write::Rotate).unwrap_err();
        assert!(format!("{err}").contains("rbxapikey.lock.toml"), "{err}");

        let err = check(&secret_file(&file), &lock, Write::Rotate).unwrap_err();
        assert!(format!("{err}").contains("deploy.env"), "{err}");
    }

    /// Each destination is judged on its own: an ignored secret file passes
    /// even though the lockfile beside it is not ignored, because the secret
    /// does not go into the lockfile.
    #[test]
    fn check_passes_a_destination_git_ignores() {
        let dir = repo();
        std::fs::write(dir.path().join(".gitignore"), ".secrets/\n").unwrap();
        let lock = dir.path().join("rbxapikey.lock.toml");
        let file = dir.path().join(".secrets").join("deploy.env");

        check(&secret_file(&file), &lock, Write::Move).unwrap();
        assert!(check(&lockfile(), &lock, Write::Move).is_err());
    }

    /// `--stdout` writes nothing, so there is nothing to refuse even in a
    /// repository that ignores nothing.
    #[test]
    fn a_secret_printed_on_stdout_is_never_refused() {
        let dir = repo();
        let lock = dir.path().join("rbxapikey.lock.toml");
        check(&crate::secret_store::external(), &lock, Write::Create).unwrap();
    }

    /// The refusal has to carry both ways out, since the reader is stopped
    /// mid-task and the docs they would consult are the ones that were wrong.
    #[test]
    fn the_refusal_names_the_line_to_add_and_the_alternative() {
        let text = refusal(Path::new("rbxapikey.lock.toml"), true, Write::Create);
        assert!(text.contains("rbxapikey.lock.toml"), "{text}");
        assert!(text.contains("default_secret_file"), "{text}");
        assert!(text.contains("Nothing has been created"), "{text}");
    }

    /// A secret file's refusal suggests its directory, and does not offer a
    /// secret file as the way out of a secret file.
    #[test]
    fn a_secret_file_refusal_names_its_directory() {
        let text = refusal(Path::new(".secrets/deploy.env"), false, Write::Rotate);
        assert!(text.contains("\n    .secrets/"), "{text}");
        assert!(!text.contains("default_secret_file"), "{text}");
        assert!(text.contains("Nothing has been rotated"), "{text}");
    }
}
