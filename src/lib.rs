//! dsk — a fast, parallel disk usage analyzer.

pub mod cli;
pub mod fsops;
pub mod output;
pub mod scanner;
pub mod settings;
pub mod tree;
pub mod tui;

use std::io::{self, IsTerminal};
use std::process::ExitCode;

use anyhow::Result;
use cli::Cli;
use output::Style;
use scanner::{ScanOptions, ScanResult};

/// Entry point for the `dsk` binary.
pub fn run() -> ExitCode {
    match run_cli(&Cli::parse_args()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // Output piped into `head` etc. closing early is not an error.
            if let Some(ioe) = err.downcast_ref::<io::Error>()
                && ioe.kind() == io::ErrorKind::BrokenPipe
            {
                return ExitCode::SUCCESS;
            }
            eprintln!("dsk: error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run_cli(args: &Cli) -> Result<()> {
    let opts = ScanOptions {
        ignore: args.ignore.clone(),
        apparent_size: args.apparent_size,
        progress: true,
    };
    let res = scanner::scan(&args.path, &opts)?;
    report_scan_errors(&res);

    // Plain `dsk` opens the browser, unless output goes to a file or pipe.
    let print_tree = args.tree || args.depth.is_some() || !io::stdout().is_terminal();
    if !print_tree {
        return tui::run(
            res.tree,
            &args.path,
            ScanOptions {
                progress: false,
                ..opts
            },
            !args.no_size,
        );
    }

    let depth = args.depth.unwrap_or(usize::MAX);
    let mut text = Vec::new();
    output::render_tree(
        &mut text,
        &res.tree,
        &args.path,
        depth,
        !args.no_size,
        Style::detect(args.no_color),
    )?;
    output::print_or_page(&text)?;
    Ok(())
}

fn report_scan_errors(res: &ScanResult) {
    if res.tree.errors == 0 {
        return;
    }
    let style = Style::detect(false);
    for msg in &res.error_samples {
        eprintln!("{} {msg}", style.yellow("warning:"));
    }
    let extra = res.tree.errors.saturating_sub(res.error_samples.len() as u64);
    if extra > 0 {
        eprintln!(
            "{} … and {extra} more unreadable entries",
            style.yellow("warning:")
        );
    }
}
