//! Command-line interface definition.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "dsk",
    version,
    about = "See what's using disk space. Opens an interactive browser; -t prints a tree.",
    after_help = "Examples:\n  dsk            browse the current folder\n  dsk ~/Downloads\n  dsk -t         print the full tree\n  dsk -1         print one level (also -2, -3, …)\n  dsk -t -z      print the tree without sizes"
)]
pub struct Cli {
    /// Folder to analyze
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Print the full tree instead of opening the interactive browser
    #[arg(short = 't', long = "tree")]
    pub tree: bool,

    /// Print the tree N levels deep (written as -1, -2, …)
    #[arg(long = "depth", value_name = "N", hide = true)]
    pub depth: Option<usize>,

    /// Skip entries matching a glob pattern (repeatable), e.g. --ignore '*.log'
    #[arg(short = 'i', long = "ignore", value_name = "PATTERN")]
    pub ignore: Vec<String>,

    /// Use apparent file sizes instead of disk usage
    #[arg(long)]
    pub apparent_size: bool,

    /// Hide file and folder sizes (press z in the browser to toggle)
    #[arg(short = 'z', long = "no-size")]
    pub no_size: bool,

    /// Disable colored output
    #[arg(long)]
    pub no_color: bool,
}

impl Cli {
    /// Parse the process arguments, accepting `-N` as shorthand for a depth.
    pub fn parse_args() -> Self {
        Cli::parse_from(expand_depth_flags(std::env::args_os()))
    }
}

/// Rewrite `-1`, `-2`, … into `--depth=N` (clap has no numeric flags).
/// Arguments after `--` are left alone so paths like `-3` still work.
pub fn expand_depth_flags(args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    let mut after_dashdash = false;
    args.into_iter()
        .map(|arg| {
            if after_dashdash {
                return arg;
            }
            if arg == "--" {
                after_dashdash = true;
                return arg;
            }
            match arg.to_str().and_then(|s| s.strip_prefix('-')) {
                Some(n) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => {
                    OsString::from(format!("--depth={n}"))
                }
                _ => arg,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(expand_depth_flags(args.iter().map(OsString::from))).unwrap()
    }

    #[test]
    fn numeric_flags_set_depth() {
        assert_eq!(parse(&["dsk", "-1"]).depth, Some(1));
        let cli = parse(&["dsk", "-12", "src"]);
        assert_eq!(cli.depth, Some(12));
        assert_eq!(cli.path, PathBuf::from("src"));
        let cli = parse(&["dsk", "-t", "-2"]);
        assert!(cli.tree);
        assert_eq!(cli.depth, Some(2));
    }

    #[test]
    fn defaults_and_dashdash() {
        let cli = parse(&["dsk"]);
        assert!(!cli.tree);
        assert_eq!(cli.depth, None);
        assert_eq!(cli.path, PathBuf::from("."));
        let cli = parse(&["dsk", "--", "-3"]);
        assert_eq!(cli.path, PathBuf::from("-3"));
        assert_eq!(cli.depth, None);
    }
}
