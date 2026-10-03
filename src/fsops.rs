//! Filesystem actions with side effects: moving to the Trash, permanent
//! deletion, and revealing paths in the platform file manager.

use std::fs;
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveMode {
    /// Move to the system Trash / Recycle Bin (recoverable).
    Trash,
    /// Delete permanently.
    Permanent,
}

/// Signature shared by [`remove_path`] and test doubles.
pub type RemoveFn = fn(&Path, RemoveMode) -> io::Result<()>;

/// Remove a file or directory. Symlinks are removed, never followed.
pub fn remove_path(path: &Path, mode: RemoveMode) -> io::Result<()> {
    match mode {
        RemoveMode::Trash => move_to_trash(path),
        RemoveMode::Permanent => match fs::symlink_metadata(path)? {
            md if md.is_dir() => fs::remove_dir_all(path),
            _ => fs::remove_file(path),
        },
    }
}

fn move_to_trash(path: &Path) -> io::Result<()> {
    // Fail with a clear "not found" instead of a generic trash error.
    fs::symlink_metadata(path)?;
    #[allow(unused_mut)]
    let mut ctx = trash::TrashContext::default();
    #[cfg(target_os = "macos")]
    {
        // NSFileManager is faster than scripting Finder and needs no
        // automation permission prompt.
        use trash::macos::{DeleteMethod, TrashContextExtMacos};
        ctx.set_delete_method(DeleteMethod::NsFileManager);
    }
    ctx.delete(path).map_err(io::Error::other)
}

/// Signature shared by [`open_with_default_app`] and test doubles.
pub type OpenFn = fn(&Path) -> io::Result<()>;

/// Open a file with its default application (e.g. a PDF in Preview).
pub fn open_with_default_app(path: &Path) -> io::Result<()> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    Command::new(program)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
}

/// Open a folder (or reveal a file) in the platform's file manager.
pub fn open_in_file_manager(path: &Path) -> io::Result<()> {
    let is_dir = path.is_dir();
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        if !is_dir {
            c.arg("-R");
        }
        c.arg(path);
        c
    } else if cfg!(windows) {
        let mut c = Command::new("explorer");
        if is_dir {
            c.arg(path);
        } else {
            let mut arg = std::ffi::OsString::from("/select,");
            arg.push(path);
            c.arg(arg);
        }
        c
    } else {
        // xdg-open cannot reveal a file, so open its folder instead.
        let target = if is_dir {
            path
        } else {
            path.parent().unwrap_or(path)
        };
        let mut c = Command::new("xdg-open");
        c.arg(target);
        c
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permanent_removes_files_and_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f");
        let d = dir.path().join("d/sub");
        fs::write(&f, b"x").unwrap();
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("g"), b"y").unwrap();
        remove_path(&f, RemoveMode::Permanent).unwrap();
        remove_path(&dir.path().join("d"), RemoveMode::Permanent).unwrap();
        assert!(!f.exists());
        assert!(!dir.path().join("d").exists());
    }

    #[test]
    fn missing_path_is_an_error_in_both_modes() {
        let p = Path::new("/no/such/dskmap/path");
        assert!(remove_path(p, RemoveMode::Permanent).is_err());
        assert!(remove_path(p, RemoveMode::Trash).is_err());
    }
}

/// Touches the real system Trash, so it only runs on request:
/// `cargo test -- --ignored trash`
#[cfg(test)]
#[test]
#[ignore]
fn trash_moves_file_out_of_place() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("dskmap-trash-test.txt");
    fs::write(&f, b"safe to delete").unwrap();
    remove_path(&f, RemoveMode::Trash).unwrap();
    assert!(!f.exists());
}
