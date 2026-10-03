//! Parallel directory scanner.
//!
//! Uses `jwalk` (rayon-backed) to read directories in parallel. File metadata
//! is fetched inside jwalk's `process_read_dir` callback, which also runs on
//! the worker pool, so `stat` calls are parallel too. Ignored entries are
//! removed there as well, so ignored directories are never descended into.
//!
//! jwalk yields entries in depth-first order, which lets us build the tree
//! with a simple depth stack instead of a path -> node map.

use std::collections::HashSet;
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::tree::{NodeId, NodeKind, Tree};

/// How many individual error messages to keep for the warning summary.
const MAX_ERROR_SAMPLES: usize = 10;

#[derive(Debug, Clone, Default)]
pub struct ScanOptions {
    /// Glob patterns matched against entry names (and relative paths).
    pub ignore: Vec<String>,
    /// Report apparent file length instead of allocated disk usage.
    pub apparent_size: bool,
    /// Show a live entry counter on stderr.
    pub progress: bool,
}

/// Per-entry metadata collected on the worker threads.
#[derive(Debug, Default)]
pub struct Meta {
    size: u64,
    /// `(device, inode)` for files with more than one hard link.
    hardlink: Option<(u64, u64)>,
    error: Option<String>,
}

type Walk = jwalk::WalkDirGeneric<((), Meta)>;

pub struct ScanResult {
    pub tree: Tree,
    pub error_samples: Vec<String>,
}

pub fn scan(root: &Path, opts: &ScanOptions) -> Result<ScanResult> {
    // Fail early with a clear message if the root itself is unusable.
    fs::symlink_metadata(root).with_context(|| format!("cannot access {}", root.display()))?;

    let ignore = Arc::new(build_globset(&opts.ignore)?);
    let apparent = opts.apparent_size;
    let counter = Arc::new(AtomicU64::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let root_buf: Arc<PathBuf> = Arc::new(root.to_path_buf());

    let progress = (opts.progress && std::io::stderr().is_terminal())
        .then(|| spawn_progress(counter.clone(), done.clone()));

    let walk = {
        let counter = counter.clone();
        let root_buf = root_buf.clone();
        Walk::new(root)
            .skip_hidden(false)
            .follow_links(false)
            .sort(false)
            .process_read_dir(move |depth, _dir, _state, children| {
                if depth.is_some() && !ignore.is_empty() {
                    children.retain(|res| match res {
                        Ok(e) => !is_ignored(&ignore, &root_buf, &e.parent_path, &e.file_name),
                        Err(_) => true,
                    });
                }
                for entry in children.iter_mut().flatten() {
                    let path = entry.parent_path.join(&entry.file_name);
                    // The root may be a followed symlink; jwalk already
                    // resolved it, so stat it through the link.
                    let md = if entry.depth == 0 {
                        fs::metadata(&path)
                    } else {
                        fs::symlink_metadata(&path)
                    };
                    entry.client_state = match md {
                        Ok(md) => meta_from(&md, apparent),
                        Err(e) => Meta {
                            error: Some(format!("{}: {e}", path.display())),
                            ..Meta::default()
                        },
                    };
                }
                counter.fetch_add(children.len() as u64, Ordering::Relaxed);
            })
    };

    let mut tree: Option<Tree> = None;
    let mut stack: Vec<NodeId> = Vec::new();
    let mut seen_inodes: HashSet<(u64, u64)> = HashSet::new();
    let mut error_samples = Vec::new();
    let mut errors = 0u64;
    let mut note_error = |msg: String| {
        errors += 1;
        if error_samples.len() < MAX_ERROR_SAMPLES {
            error_samples.push(msg);
        }
    };

    for result in walk {
        let entry = match result {
            Ok(e) => e,
            Err(e) => {
                note_error(e.to_string());
                continue;
            }
        };
        let kind = kind_of(&entry.file_type);
        let meta = &entry.client_state;
        if let Some(msg) = &meta.error {
            note_error(msg.clone());
        }
        let mut size = meta.size;
        if let Some(key) = meta.hardlink {
            // Count each hard-linked inode only once.
            if !seen_inodes.insert(key) {
                size = 0;
            }
        }

        if entry.depth == 0 {
            let root_kind = if entry.read_children.is_some() {
                NodeKind::Dir
            } else {
                kind
            };
            tree = Some(Tree::with_root(root, root_kind, size));
            stack.push(0);
            continue;
        }
        let tree = tree.as_mut().expect("root is yielded first");
        stack.truncate(entry.depth);
        let parent = *stack.last().expect("parent on stack");
        let id = tree.add_child(parent, &entry.file_name, kind, size);
        if kind == NodeKind::Dir {
            stack.push(id);
        }
    }

    done.store(true, Ordering::Relaxed);
    if let Some(handle) = progress {
        let _ = handle.join();
    }

    let mut tree = tree.with_context(|| format!("cannot scan {}", root.display()))?;
    tree.errors = errors;
    tree.finalize();
    Ok(ScanResult { tree, error_samples })
}

fn kind_of(ft: &fs::FileType) -> NodeKind {
    if ft.is_dir() {
        NodeKind::Dir
    } else if ft.is_file() {
        NodeKind::File
    } else if ft.is_symlink() {
        NodeKind::Symlink
    } else {
        NodeKind::Other
    }
}

#[cfg(unix)]
fn meta_from(md: &fs::Metadata, apparent: bool) -> Meta {
    use std::os::unix::fs::MetadataExt;
    let size = if apparent {
        if md.is_dir() { 0 } else { md.len() }
    } else {
        md.blocks() * 512
    };
    let hardlink = (!md.is_dir() && md.nlink() > 1).then(|| (md.dev(), md.ino()));
    Meta {
        size,
        hardlink,
        error: None,
    }
}

#[cfg(not(unix))]
fn meta_from(md: &fs::Metadata, _apparent: bool) -> Meta {
    // Windows: allocation size and file IDs are not exposed by stable std,
    // so we report apparent size and don't de-duplicate hard links.
    let size = if md.is_dir() { 0 } else { md.len() };
    Meta {
        size,
        hardlink: None,
        error: None,
    }
}

pub fn build_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        builder.add(Glob::new(p).with_context(|| format!("invalid ignore pattern: {p}"))?);
    }
    Ok(builder.build()?)
}

fn is_ignored(set: &GlobSet, root: &Path, parent: &Path, name: &std::ffi::OsStr) -> bool {
    if set.is_match(Path::new(name)) {
        return true;
    }
    let full = parent.join(name);
    let rel = full.strip_prefix(root).unwrap_or(&full);
    set.is_match(rel)
}

fn spawn_progress(counter: Arc<AtomicU64>, done: Arc<AtomicBool>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut ticks = 0u32;
        while !done.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(50));
            ticks += 1;
            // Stay quiet for fast scans.
            if ticks >= 4 && ticks.is_multiple_of(2) {
                eprint!("\r\x1b[2KScanning… {} entries", counter.load(Ordering::Relaxed));
            }
        }
        if ticks >= 4 {
            eprint!("\r\x1b[2K");
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;

    fn write(path: &Path, bytes: usize) {
        let mut f = File::create(path).unwrap();
        f.write_all(&vec![b'x'; bytes]).unwrap();
    }

    fn opts() -> ScanOptions {
        ScanOptions {
            apparent_size: true,
            ..Default::default()
        }
    }

    fn find(tree: &Tree, rel: &str) -> Option<NodeId> {
        tree.descendants(tree.root()).find(|&id| tree.rel_path(id) == rel)
    }

    #[test]
    fn scans_sizes_recursively() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("a/b")).unwrap();
        write(&root.join("a/one"), 100);
        write(&root.join("a/b/two"), 200);
        write(&root.join("three"), 50);

        let tree = scan(root, &opts()).unwrap().tree;
        assert_eq!(tree.get(tree.root()).size, 350);
        assert_eq!(tree.get(find(&tree, "a").unwrap()).size, 300);
        assert_eq!(tree.get(find(&tree, "a/b").unwrap()).size, 200);
        let top = tree.sorted_children(tree.root());
        assert_eq!(tree.get(top[0]).name_lossy(), "a");
        assert_eq!(tree.errors, 0);
    }

    #[test]
    fn ignore_patterns_skip_entries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        write(&root.join("node_modules/pkg/big"), 1000);
        write(&root.join("keep.txt"), 10);
        write(&root.join("skip.log"), 500);

        let o = ScanOptions {
            ignore: vec!["node_modules".into(), "*.log".into()],
            ..opts()
        };
        let tree = scan(root, &o).unwrap().tree;
        assert_eq!(tree.get(tree.root()).size, 10);
        assert!(find(&tree, "node_modules").is_none());
        assert!(find(&tree, "skip.log").is_none());
    }

    #[test]
    fn empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        let tree = scan(dir.path(), &opts()).unwrap().tree;
        assert_eq!(tree.len(), 1);
        assert_eq!(tree.get(0).size, 0);
        assert!(tree.get(0).is_dir());
    }

    #[test]
    fn missing_root_is_an_error() {
        assert!(scan(Path::new("/definitely/not/here/dskmap"), &opts()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn hardlinks_counted_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("orig"), 4000);
        fs::hard_link(root.join("orig"), root.join("link")).unwrap();
        let tree = scan(root, &opts()).unwrap().tree;
        assert_eq!(tree.get(tree.root()).size, 4000);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("real")).unwrap();
        write(&root.join("real/data"), 5000);
        std::os::unix::fs::symlink(root.join("real"), root.join("alias")).unwrap();
        let tree = scan(root, &opts()).unwrap().tree;
        let alias = find(&tree, "alias").unwrap();
        assert_eq!(tree.get(alias).kind, NodeKind::Symlink);
        assert_eq!(tree.children(alias).count(), 0);
        // Only the symlink's own (tiny) length is counted, not the target.
        assert!(tree.get(tree.root()).size < 5000 + 1000);
    }

    #[cfg(unix)]
    #[test]
    fn permission_errors_are_skipped() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let locked = root.join("locked");
        fs::create_dir(&locked).unwrap();
        write(&locked.join("secret"), 100);
        write(&root.join("ok"), 10);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let res = scan(root, &opts());
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        let res = res.unwrap();
        // Running as root bypasses permissions; only assert when it applied.
        if res.tree.errors > 0 {
            assert_eq!(res.tree.get(0).size, 10);
            assert!(!res.error_samples.is_empty());
        }
    }
}
