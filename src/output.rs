//! Terminal rendering helpers: sizes and the tree view.

use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use crate::tree::{NodeId, Tree};

const MAX_NAME_WIDTH: usize = 60;

/// Format a byte count in binary units like `du -h` (`512B`, `4.0K`, `1.2G`, `37G`).
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 7] = ["B", "K", "M", "G", "T", "P", "E"];
    if bytes < 1024 {
        return format!("{bytes}B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value < 10.0 {
        format!("{value:.1}{}", UNITS[unit])
    } else {
        format!("{value:.0}{}", UNITS[unit])
    }
}

/// Folder name to show for a scanned path (`.` becomes the real folder name).
pub fn display_name(path: &Path) -> String {
    std::fs::canonicalize(path)
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| path.display().to_string())
}

/// Minimal ANSI styling that switches off cleanly.
#[derive(Clone, Copy)]
pub struct Style {
    pub color: bool,
}

impl Style {
    pub fn detect(no_color: bool) -> Self {
        let color = !no_color && std::env::var_os("NO_COLOR").is_none() && io::stdout().is_terminal();
        Style { color }
    }

    fn paint(&self, s: &str, code: &str) -> String {
        if self.color && !s.is_empty() {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    pub fn dir(&self, s: &str) -> String {
        self.paint(s, "1;34")
    }
    pub fn dim(&self, s: &str) -> String {
        self.paint(s, "2")
    }
    pub fn bold(&self, s: &str) -> String {
        self.paint(s, "1")
    }
    pub fn yellow(&self, s: &str) -> String {
        self.paint(s, "33")
    }
}

/// Pad or truncate `s` to exactly `width` display columns (approximated by
/// `char` count, which is right for the vast majority of file names).
pub fn fit(s: &str, width: usize) -> String {
    let len = s.chars().count();
    if len <= width {
        format!("{s}{}", " ".repeat(width - len))
    } else if width == 0 {
        String::new()
    } else {
        let mut out: String = s.chars().take(width - 1).collect();
        out.push('…');
        out
    }
}

/// Print `text`, or open it in a scrollable pager (`less`) when it is
/// taller than the terminal. Falls back to plain printing if no pager runs.
pub fn print_or_page(text: &[u8]) -> io::Result<()> {
    let stdout = io::stdout();
    let rows = ratatui::crossterm::terminal::size()
        .map(|(_, h)| h as usize)
        .unwrap_or(usize::MAX);
    if stdout.is_terminal() && needs_pager(text, rows) && page(text).is_ok() {
        return Ok(());
    }
    let mut out = stdout.lock();
    out.write_all(text)?;
    out.flush()
}

/// `less` prompt; `?e(END) :.` adds "(END)" only on the last page.
const PAGER_PROMPT: &str = "-Ps?e(END) :. ↑↓ scroll  / search  n next match  q quit";

fn needs_pager(text: &[u8], rows: usize) -> bool {
    // Leave one row for the shell prompt that follows.
    text.iter().filter(|&&b| b == b'\n').count() >= rows
}

fn page(text: &[u8]) -> io::Result<()> {
    // -R keeps colors; -S doesn't wrap long lines (scroll sideways instead);
    // -P replaces the bare ":" prompt with a key hint.
    let mut child = Command::new("less")
        .arg("-RS")
        .arg(PAGER_PROMPT)
        .stdin(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    match stdin.write_all(text) {
        // Quitting `less` before reading everything is fine.
        Err(e) if e.kind() != io::ErrorKind::BrokenPipe => return Err(e),
        _ => {}
    }
    drop(stdin);
    child.wait()?;
    Ok(())
}

struct Row {
    prefix: String,
    label: String,
    is_dir: bool,
    size: u64,
}

/// Render the tree sorted by size (names and sizes only), down to
/// `max_depth` levels below the root. With `show_sizes` off, only the
/// root line keeps its total.
pub fn render_tree(
    out: &mut impl Write,
    tree: &Tree,
    root_path: &Path,
    max_depth: usize,
    show_sizes: bool,
    style: Style,
) -> io::Result<()> {
    let root = tree.root();
    let mut rows = Vec::new();
    collect_rows(tree, root, max_depth, 1, String::new(), &mut rows);

    let root_name = display_name(root_path);
    let name_width = rows
        .iter()
        .map(|r| r.prefix.chars().count() + r.label.chars().count())
        .chain([root_name.chars().count()])
        .max()
        .unwrap_or(0)
        .min(MAX_NAME_WIDTH);
    let size_width = rows
        .iter()
        .map(|r| r.size)
        .chain([tree.get(root).size])
        .map(|s| format_size(s).len())
        .max()
        .unwrap_or(4);

    writeln!(
        out,
        "{}  {:>size_width$}",
        style.bold(&fit(&root_name, name_width)),
        style.bold(&format_size(tree.get(root).size)),
    )?;
    for row in &rows {
        if !show_sizes {
            let label = if row.is_dir {
                style.dir(&row.label)
            } else {
                row.label.clone()
            };
            writeln!(out, "{}{}", style.dim(&row.prefix), label)?;
            continue;
        }
        let avail = name_width.saturating_sub(row.prefix.chars().count());
        let fitted = fit(&row.label, avail);
        let name = if row.is_dir {
            // Style only the visible text so padding stays uncolored.
            let trimmed = fitted.trim_end();
            format!("{}{}", style.dir(trimmed), &fitted[trimmed.len()..])
        } else {
            fitted
        };
        writeln!(
            out,
            "{}{}  {:>size_width$}",
            style.dim(&row.prefix),
            name,
            format_size(row.size)
        )?;
    }
    Ok(())
}

fn collect_rows(
    tree: &Tree,
    id: NodeId,
    max_depth: usize,
    depth: usize,
    prefix: String,
    rows: &mut Vec<Row>,
) {
    if depth > max_depth {
        return;
    }
    let kids = tree.sorted_children(id);
    for (i, &kid) in kids.iter().enumerate() {
        let last = i + 1 == kids.len();
        let node = tree.get(kid);
        let name = node.name_lossy();
        rows.push(Row {
            prefix: format!("{prefix}{}", if last { "└── " } else { "├── " }),
            label: if node.is_dir() {
                format!("{name}/")
            } else {
                name.into_owned()
            },
            is_dir: node.is_dir(),
            size: node.size,
        });
        if node.is_dir() {
            let child_prefix = format!("{prefix}{}", if last { "    " } else { "│   " });
            collect_rows(tree, kid, max_depth, depth + 1, child_prefix, rows);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::NodeKind;
    use std::ffi::OsStr;

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(format_size(0), "0B");
        assert_eq!(format_size(1023), "1023B");
        assert_eq!(format_size(1024), "1.0K");
        assert_eq!(format_size(1536), "1.5K");
        assert_eq!(format_size(1_288_490_189), "1.2G");
        assert_eq!(format_size(40 * 1024 * 1024 * 1024), "40G");
    }

    fn sample() -> Tree {
        let mut t = Tree::with_root("/r", NodeKind::Dir, 0);
        let a = t.add_child(0, OsStr::new("a"), NodeKind::Dir, 0);
        t.add_child(a, OsStr::new("deep"), NodeKind::File, 2048);
        for (i, size) in [5u64, 4, 3].iter().enumerate() {
            t.add_child(0, OsStr::new(&format!("f{i}")), NodeKind::File, *size);
        }
        t.finalize();
        t
    }

    fn render(t: &Tree, depth: usize) -> String {
        let mut out = Vec::new();
        render_tree(&mut out, t, Path::new("/r"), depth, true, Style { color: false }).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn full_tree_shows_names_and_sizes_only() {
        let text = render(&sample(), usize::MAX);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 6, "{text}");
        assert!(
            lines[0].starts_with("/r ") && lines[0].ends_with("2.0K"),
            "{text}"
        );
        assert!(
            lines[1].starts_with("├── a/") && lines[1].ends_with("2.0K"),
            "{text}"
        );
        assert!(lines[2].starts_with("│   └── deep"), "{text}");
        assert!(
            lines[5].starts_with("└── f2") && lines[5].ends_with("3B"),
            "{text}"
        );
        assert!(!text.contains('%') && !text.contains('█'), "{text}");
    }

    #[test]
    fn sizes_can_be_hidden() {
        let mut out = Vec::new();
        render_tree(
            &mut out,
            &sample(),
            Path::new("/r"),
            usize::MAX,
            false,
            Style { color: false },
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].ends_with("2.0K"), "root keeps its total: {text}");
        assert_eq!(lines[1], "├── a/");
        assert_eq!(lines[2], "│   └── deep");
        assert_eq!(lines[5], "└── f2");
    }

    #[test]
    fn pager_only_for_tall_output() {
        assert!(!needs_pager(b"a\nb\n", 24));
        assert!(needs_pager("x\n".repeat(24).as_bytes(), 24));
        assert!(!needs_pager("x\n".repeat(100).as_bytes(), usize::MAX));
    }

    #[test]
    fn depth_limits_levels() {
        let text = render(&sample(), 1);
        assert!(!text.contains("deep"), "{text}");
        assert_eq!(text.lines().count(), 5);
    }
}
