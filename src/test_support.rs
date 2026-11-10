//! Fixtures more than one module's tests need.

use std::path::{Path, PathBuf};

/// A path under `dir` whose existence cannot be asked about.
///
/// `Path::exists` answers "no" to it, which is the confusion the loaders must
/// not make: `try_exists` returns an error. On Unix the path runs through a
/// regular file (`ENOTDIR`), which holds even for root, unlike a directory
/// with its permissions removed. On Windows the name holds characters no file
/// name may contain.
pub(crate) fn unlookable(dir: &Path) -> PathBuf {
    let path = if cfg!(windows) {
        dir.join("cannot<be>looked|at")
    } else {
        let file = dir.join("a-file");
        std::fs::write(&file, "").expect("a plain file to look through");
        file.join("cannot-be-looked-at")
    };
    assert!(
        path.try_exists().is_err(),
        "the fixture must be a path that cannot be looked at: {}",
        path.display()
    );
    path
}
