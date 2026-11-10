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
//! `secret_file` changed. Each runs [`check_all`] on every key's destination,
//! the lockfile or a `secret_file`, before its first prompt, and refuses the
//! whole run, naming every refused key, if any of them fails. A secret file is
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

/// What a secret file's relationship to git turned out to be.
#[derive(Debug, PartialEq, Eq)]
pub enum GitStatus {
    /// Not inside a git work tree: nothing to leak into.
    NotARepo,
    /// Inside a repository, and git ignores the path.
    Ignored,
    /// Inside a repository, and git does **not** ignore the path.
    NotIgnored,
    /// Inside a repository, and git already tracks the file. An ignore rule
    /// does not apply to a tracked file, so `check-ignore` keeps saying "not
    /// ignored" after the line is added, and the advice has to say so.
    InIndex,
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
            Write::Create => "Creating these keys now",
            Write::Rotate => "Rotating these secrets now",
            Write::Move => "Moving these secrets there",
        }
    }

    fn nothing(self) -> &'static str {
        match self {
            Write::Create => "Nothing has been created.",
            Write::Rotate => "Nothing has been rotated: every key keeps the secret it has.",
            Write::Move => {
                "Nothing has been changed: every secret stays where it is, and no key was \
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

/// One file git would commit, and the keys whose secrets were headed there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub path: PathBuf,
    pub is_lockfile: bool,
    /// Git already tracks the file, so an ignore line alone does nothing.
    pub in_index: bool,
    /// The `.gitignore` line that covers the file, anchored at the root of
    /// its repository.
    pub ignore_line: String,
    /// The file relative to the root of its repository, for `git rm`.
    pub from_root: Option<String>,
    pub keys: Vec<String>,
}

/// Stop a whole run before any secret is written where git would commit it.
///
/// `writes` is every key the run is about to write a secret for, with where
/// it would go; `lock_path` is where the lockfile backend writes. Every
/// destination is checked, each file once, and every refusal is collected
/// before deciding: under `--all`, stopping at the third key would leave two
/// changed for a reason known from the start, and naming one file at a time
/// would send the reader round once per file. Warns on stderr when git cannot
/// say, and passes outside a repository. Callers run it before the first
/// prompt and before anything is sent to Roblox.
pub fn check_all(writes: &[(String, Resolved)], lock_path: &Path, write: Write) -> Result<()> {
    let mut refused: Vec<Refused> = Vec::new();
    let mut passed: Vec<PathBuf> = Vec::new();
    for (key, resolved) in writes {
        let Some(path) = destination(resolved, lock_path) else {
            continue;
        };
        if let Some(r) = refused.iter_mut().find(|r| r.path == path) {
            r.keys.push(key.clone());
            continue;
        }
        if passed.contains(&path) {
            continue;
        }
        let examined = examine(&path);
        match examined.status {
            GitStatus::NotARepo | GitStatus::Ignored => passed.push(path),
            GitStatus::NotIgnored | GitStatus::InIndex => {
                let from_root = examined.resolved.as_ref().and_then(from_repo_root);
                refused.push(Refused {
                    ignore_line: match &from_root {
                        Some(rel) => format!("/{rel}"),
                        None => file_name_of(&path),
                    },
                    in_index: examined.status == GitStatus::InIndex,
                    from_root,
                    path,
                    is_lockfile: resolved.backend == Backend::Lockfile,
                    keys: vec![key.clone()],
                })
            }
            // An unanswerable check is not a verdict: git missing from PATH
            // is not evidence that the file is safe, nor that it is not.
            GitStatus::Unknown(why) => {
                eprintln!(
                    "warning: could not check whether {} is gitignored ({why}); the key \
                     secret is stored there in plain text",
                    path.display()
                );
                passed.push(path);
            }
        }
    }
    if refused.is_empty() {
        return Ok(());
    }
    bail!(refusal(&refused, write))
}

/// Whether `path` sits inside a git work tree and, if so, whether git ignores
/// it.
///
/// `path` need not exist yet, nor its directory: a secret file is usually
/// written into a `.secrets/` the first write creates. The commands go
/// through [`check_all`]; this is the question on its own, for the tests.
#[cfg(test)]
fn status_of(path: &Path) -> GitStatus {
    examine(path).status
}

/// A destination as git will see it: the file, and the directory git runs in.
#[derive(Debug, Clone)]
struct Resolution {
    target: PathBuf,
    dir: PathBuf,
}

#[derive(Debug)]
struct Examined {
    status: GitStatus,
    resolved: Option<Resolution>,
}

fn examine(path: &Path) -> Examined {
    let resolved = match resolve(path) {
        Ok(r) => r,
        Err(why) => {
            return Examined {
                status: GitStatus::Unknown(why),
                resolved: None,
            }
        }
    };
    let status = git_status(&resolved);
    Examined {
        status,
        resolved: Some(resolved),
    }
}

fn git_status(r: &Resolution) -> GitStatus {
    match in_work_tree(&r.dir) {
        Ok(true) => {}
        Ok(false) => return GitStatus::NotARepo,
        Err(why) => return GitStatus::Unknown(why),
    }

    // `check-ignore` exits 0 when the path is ignored, 1 when it is not, and
    // 128 when something else went wrong, which is the case that must not be
    // read as either answer.
    match git(&r.dir)
        .arg("check-ignore")
        .arg("--quiet")
        .arg(&r.target)
        .status()
    {
        Ok(s) if s.code() == Some(0) => GitStatus::Ignored,
        Ok(s) if s.code() == Some(1) => {
            // "Not ignored" is also what git says about a file it tracks,
            // whatever `.gitignore` holds, and that file needs different
            // advice: an ignore line does not untrack it.
            match git(&r.dir)
                .arg("ls-files")
                .arg("--error-unmatch")
                .arg("--")
                .arg(&r.target)
                .status()
            {
                Ok(s) if s.code() == Some(0) => GitStatus::InIndex,
                Ok(s) if s.code() == Some(1) => GitStatus::NotIgnored,
                Ok(s) => GitStatus::Unknown(format!("git ls-files exited {}", s)),
                Err(e) => GitStatus::Unknown(format!("git could not be run: {e}")),
            }
        }
        Ok(s) => GitStatus::Unknown(format!("git check-ignore exited {}", s)),
        Err(e) => GitStatus::Unknown(format!("git could not be run: {e}")),
    }
}

/// `git -C <dir>`, quiet on stderr: the exit status is the answer, and the
/// cases that are not one become a warning of their own.
fn git(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C")
        .arg(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    c
}

/// `path` the way git will see it: absolute, the part that exists resolved
/// through symlinks and junctions, the part not created yet joined on.
///
/// Absolute, because git resolves the path after `-C` has moved it into the
/// directory: a relative `.secrets/deploy.env` would be looked up as
/// `.secrets/.secrets/deploy.env`. Resolved, because git works out the
/// repository from the real directory, and a path reaching it through a
/// symlink (`/tmp` on macOS is one) is "outside the repository" to git: exit
/// 128, which reads as a check that could not run, and the secret would be
/// written with a warning instead of refused.
fn resolve(path: &Path) -> std::result::Result<Resolution, String> {
    let abs = std::path::absolute(path)
        .map_err(|e| format!("{} has no absolute form: {e}", path.display()))?;
    let existing = nearest_existing_dir(&abs)?;
    let rest = abs.strip_prefix(&existing).unwrap_or(Path::new(""));
    let dir = plain(
        existing
            .canonicalize()
            .map_err(|e| format!("{} cannot be resolved: {e}", existing.display()))?,
    );
    // The part not created yet, without `.` and `..`: nothing on disk can
    // redirect it, so it is folded by hand.
    let mut target = dir.clone();
    for part in rest.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                target.pop();
            }
            other => target.push(other),
        }
    }
    Ok(Resolution { target, dir })
}

/// A canonical Windows path without its `\\?\` prefix, which git does not
/// take. A no-op elsewhere.
fn plain(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(unc) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{unc}"));
    }
    match s.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => path,
    }
}

/// The file relative to the root of its repository, with `/` separators, or
/// `None` when git cannot say where the root is.
fn from_repo_root(r: &Resolution) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(&r.dir)
        .arg("rev-parse")
        .arg("--show-toplevel")
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let top = String::from_utf8(out.stdout).ok()?;
    let top = plain(PathBuf::from(top.trim()).canonicalize().ok()?);
    let rel = r.target.strip_prefix(&top).ok()?;
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| crate::lock::FILE.to_string())
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

/// The refusal for a whole run, worded so the reader can act without opening
/// the docs: every file, the keys headed for it, and the lines to add.
///
/// Each line is the file's path from the root of its repository, anchored
/// with a leading `/`, for the `.gitignore` at that root. The path the config
/// spells is no use there: `./.secrets/` matches nothing, `../x/` is not a
/// pattern, and a directory relative to where the command ran anchors
/// somewhere else.
pub fn refusal(refused: &[Refused], write: Write) -> String {
    let files: Vec<String> = refused
        .iter()
        .map(|r| format!("    {} ({})", r.path.display(), r.keys.join(", ")))
        .collect();
    let lines: Vec<&str> = refused.iter().map(|r| r.ignore_line.as_str()).collect();
    let mut text = format!(
        "These files would hold key secrets in plain text, and git is not ignoring them:\n\n\
         {}\n\n\
         {} would put live Open Cloud credentials one `git add` away from being published, \
         which is the one failure this tool exists to prevent. {}\n\n\
         Add these lines to the .gitignore at the root of the repository, then run the same \
         command again:\n\n    \
         {}",
        files.join("\n"),
        write.doing(),
        write.nothing(),
        lines.join("\n    "),
    );
    let in_index: Vec<String> = refused
        .iter()
        .filter(|r| r.in_index)
        .map(|r| match &r.from_root {
            Some(rel) => format!("    git rm --cached -- {rel}"),
            None => format!("    git rm --cached -- {}", r.path.display()),
        })
        .collect();
    if !in_index.is_empty() {
        text.push_str(&format!(
            "\n\nGit already tracks some of them, and an ignore line does not untrack a file. \
             From the root of the repository, also run:\n\n{}\n\n\
             A secret that was ever pushed in one of them is published: rotate it.",
            in_index.join("\n")
        ));
    }
    if refused.iter().any(|r| r.is_lockfile) {
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
        assert_eq!(status_of(&lock), GitStatus::NotIgnored);
    }

    /// A link to `target` at `link`: a symlink on Unix, a junction on
    /// Windows, where neither needs elevated rights.
    fn link(target: &Path, link: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
        #[cfg(windows)]
        assert!(Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap()
            .success());
    }

    /// Reached through a link, the destination is handed to git as the real
    /// path, joined to the part not created yet. Given the linked path, git
    /// on Unix answers "outside repository" (exit 128), which read as a check
    /// that could not run, and the secret was written with a warning.
    #[test]
    fn a_path_through_a_link_is_resolved_for_git() {
        let base = tempfile::tempdir().unwrap();
        let real = base.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let linked = base.path().join("linked");
        link(&real, &linked);

        let r = resolve(&linked.join(".secrets").join("deploy.env")).unwrap();

        let real = plain(real.canonicalize().unwrap());
        assert_eq!(r.dir, real);
        assert_eq!(r.target, real.join(".secrets").join("deploy.env"));
    }

    /// And end to end: a repository reached through a link is refused like
    /// any other, not let through as "could not check".
    #[test]
    fn a_repository_reached_through_a_link_is_still_checked() {
        let base = tempfile::tempdir().unwrap();
        let real = base.path().join("real");
        std::fs::create_dir(&real).unwrap();
        assert!(Command::new("git")
            .arg("-C")
            .arg(&real)
            .args(["init", "--quiet"])
            .status()
            .unwrap()
            .success());
        let linked = base.path().join("linked");
        link(&real, &linked);

        assert_eq!(
            status_of(&linked.join("rbxapikey.lock.toml")),
            GitStatus::NotIgnored
        );
    }

    /// A file git already tracks stays "not ignored" whatever `.gitignore`
    /// says, so it is told apart, and the refusal says to untrack it.
    #[test]
    fn a_file_git_already_tracks_is_told_apart_and_untracked() {
        let dir = repo();
        let lock = dir.path().join("rbxapikey.lock.toml");
        std::fs::write(&lock, "").unwrap();
        assert!(Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["add", "rbxapikey.lock.toml"])
            .status()
            .unwrap()
            .success());
        std::fs::write(dir.path().join(".gitignore"), "/rbxapikey.lock.toml\n").unwrap();

        assert_eq!(status_of(&lock), GitStatus::InIndex);
        let text = format!(
            "{}",
            check_all(&one("deploy", lockfile()), &lock, Write::Rotate).unwrap_err()
        );
        assert!(
            text.contains("git rm --cached -- rbxapikey.lock.toml"),
            "{text}"
        );
    }

    /// The suggested line is the path from the repository root, anchored,
    /// whatever directory the command ran in and however the config spelled
    /// the path. Proved by adding it and asking git again.
    #[test]
    fn the_ignore_line_is_anchored_at_the_repository_root() {
        let dir = repo();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let lock = sub.join("rbxapikey.lock.toml");
        // As a config in `sub` would spell `../.secrets/ci.env`.
        let ci = sub.join("..").join(".secrets").join("ci.env");
        let writes = vec![
            ("deploy".to_string(), lockfile()),
            ("ci".to_string(), secret_file(&ci)),
        ];

        let text = format!("{}", check_all(&writes, &lock, Write::Create).unwrap_err());
        assert!(text.contains("\n    /sub/rbxapikey.lock.toml"), "{text}");
        assert!(text.contains("\n    /.secrets/ci.env"), "{text}");

        std::fs::write(
            dir.path().join(".gitignore"),
            "/sub/rbxapikey.lock.toml\n/.secrets/ci.env\n",
        )
        .unwrap();
        check_all(&writes, &lock, Write::Create).unwrap();
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

        assert_eq!(status_of(&file), GitStatus::NotIgnored);

        std::fs::write(dir.path().join(".gitignore"), ".secrets/\n").unwrap();
        assert_eq!(status_of(&file), GitStatus::Ignored);
    }

    fn one(key: &str, resolved: Resolved) -> Vec<(String, Resolved)> {
        vec![(key.to_string(), resolved)]
    }

    /// Both destinations through the one entry point the commands use.
    #[test]
    fn check_refuses_either_destination_git_does_not_ignore() {
        let dir = repo();
        let lock = dir.path().join("rbxapikey.lock.toml");
        let file = dir.path().join(".secrets").join("deploy.env");

        let err = check_all(&one("deploy", lockfile()), &lock, Write::Rotate).unwrap_err();
        assert!(format!("{err}").contains("rbxapikey.lock.toml"), "{err}");

        let err = check_all(&one("deploy", secret_file(&file)), &lock, Write::Rotate).unwrap_err();
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

        check_all(&one("deploy", secret_file(&file)), &lock, Write::Move).unwrap();
        assert!(check_all(&one("deploy", lockfile()), &lock, Write::Move).is_err());
    }

    /// A run refuses once, listing every file and every key headed for it,
    /// rather than stopping at the first and leaving the reader to find the
    /// next one on the next attempt.
    #[test]
    fn a_run_is_refused_with_every_refused_key() {
        let dir = repo();
        let lock = dir.path().join("rbxapikey.lock.toml");
        let ci = dir.path().join(".secrets").join("ci.env");
        let writes = vec![
            ("deploy".to_string(), lockfile()),
            ("ci".to_string(), secret_file(&ci)),
            ("viewer".to_string(), lockfile()),
            ("vault".to_string(), crate::secret_store::external()),
        ];

        let text = format!("{}", check_all(&writes, &lock, Write::Create).unwrap_err());

        assert!(text.contains("(deploy, viewer)"), "{text}");
        assert!(text.contains("ci.env (ci)"), "{text}");
        assert!(!text.contains("vault"), "{text}");
        assert_eq!(text.matches("rbxapikey.lock.toml (").count(), 1, "{text}");
    }

    /// `--stdout` writes nothing, so there is nothing to refuse even in a
    /// repository that ignores nothing.
    #[test]
    fn a_secret_printed_on_stdout_is_never_refused() {
        let dir = repo();
        let lock = dir.path().join("rbxapikey.lock.toml");
        check_all(
            &one("deploy", crate::secret_store::external()),
            &lock,
            Write::Create,
        )
        .unwrap();
    }

    fn refused(path: &str, is_lockfile: bool) -> Refused {
        Refused {
            path: PathBuf::from(path),
            is_lockfile,
            in_index: false,
            ignore_line: format!("/{path}"),
            from_root: Some(path.to_string()),
            keys: vec!["deploy".into()],
        }
    }

    /// The refusal has to carry both ways out, since the reader is stopped
    /// mid-task and the docs they would consult are the ones that were wrong.
    #[test]
    fn the_refusal_names_the_line_to_add_and_the_alternative() {
        let text = refusal(&[refused("rbxapikey.lock.toml", true)], Write::Create);
        assert!(text.contains("rbxapikey.lock.toml"), "{text}");
        assert!(text.contains("default_secret_file"), "{text}");
        assert!(text.contains("Nothing has been created"), "{text}");
    }

    /// A secret file's refusal names its line, and does not offer a secret
    /// file as the way out of a secret file.
    #[test]
    fn a_secret_file_refusal_does_not_offer_a_secret_file() {
        let text = refusal(&[refused(".secrets/deploy.env", false)], Write::Rotate);
        assert!(text.contains("\n    /.secrets/deploy.env"), "{text}");
        assert!(!text.contains("git rm"), "{text}");
        assert!(!text.contains("default_secret_file"), "{text}");
        assert!(text.contains("Nothing has been rotated"), "{text}");
    }
}
