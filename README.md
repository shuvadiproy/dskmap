# dsk

[![CI](https://github.com/shuvadiproy/dskmap/actions/workflows/ci.yml/badge.svg)](https://github.com/shuvadiproy/dskmap/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**A fast, parallel disk usage analyzer for the terminal.** Type `dsk` to browse what's using space, or
`dsk -t` to print it as a tree.

```
$ dsk -1
dsk_project        1.6G
├── target/        1.6G
├── .git/          512K
├── src/            76K
├── Cargo.lock      56K
├── README.md      8.0K
├── Cargo.toml     4.0K
└── LICENSE        4.0K
```

## Features

- **Tree view** (`dsk -t`, `dsk -1`, `dsk -2`, …): names and sizes, biggest first
- **Fast parallel scan** with [jwalk](https://crates.io/crates/jwalk), ~2.6× faster than `du` (see [benchmarks](#benchmark-vs-du))
- **Interactive browser** (just type `dsk`): a clean name + size list, keyboard and mouse, search, sorting, move to Trash, reveal in your file manager
- **Correct numbers**: hardlinks are counted once, symlinks are not followed, and on Unix it reports on-disk usage like `du`
- **Safe**: deleting moves things to the Trash and always asks first; unreadable folders are skipped with a warning instead of crashing
- **Scales**: 1,000,000 files in ~65 MB of RAM

## Install

With Homebrew (macOS / Linux):

```sh
brew tap shuvadiproy/tap
brew trust shuvadiproy/tap   # Homebrew 7+ asks you to trust third-party taps
brew install dskmap
```

With Cargo:

```sh
cargo install --git https://github.com/shuvadiproy/dskmap
```

Both install the `dsk` command.

From source:

```sh
git clone https://github.com/shuvadiproy/dskmap
cd dskmap
cargo install --path .
```

Requires Rust 1.88+ (edition 2024). Works on Linux, macOS and Windows.

## Usage

```sh
dsk                       # browse the current folder interactively
dsk ~/Downloads           # browse a specific folder
dsk -t                    # print the full tree
dsk -1                    # print one level (also -2, -3, …)
dsk -t --ignore .git --ignore '*.log'
```

| Flag | Description |
|------|-------------|
| `-t, --tree` | Print the full tree instead of opening the browser |
| `-1`, `-2`, … | Print the tree only that many levels deep |
| `-z, --no-size` | Hide file and folder sizes (the total at the top stays). In the browser, `z` toggles |
| `-i, --ignore <PATTERN>` | Skip entries matching a glob, matched against the name and the relative path. Repeatable |
| `--apparent-size` | Report file length instead of disk usage |
| `--no-color` | Disable colors (`NO_COLOR` is respected too) |

When the output goes to a file or another program (`dsk > sizes.txt`), `dsk` prints the full tree instead
of opening the browser.

A tree taller than your terminal opens in a scrollable view (`less`) with a key hint at the bottom. Scroll with the arrow keys or space,
search with `/`, and press `q` to close it. Short trees print normally.

### Interactive mode

```sh
dsk ~
```

```
 shuvadip › Desktop  2.3G                         sort: size
▶ dsk_project/   1.1G
  screenshots/   528M
  fabric/        341M
 ↑↓ move  → open  ← back  t tree view  / search  d trash  ? help  q quit
```

Each row shows just the name and size. The top line shows where you are, the folder's total size, and the
current sort order.

Your view (`t`), sort order (`s`), hidden files (`.`) and sizes (`z`) are remembered for next time in
`~/.config/dsk/settings.toml` (`%APPDATA%\dsk\settings.toml` on Windows).

Press `t` for the tree view, where folders open in place:

```
 dsk_project  1.7G                                    tree · sort: size
  ▾ target/                 1.7G
▶   ├─ ▾ debug/             1.4G
    │  ├─ ▸ deps/           797M
    │  ├─ ▸ incremental/    592M
    │  └─   dsk              12M
    └─ ▸ release/           240M
  ▸ src/                     76K
```

| Key | Mouse | Action |
|-----|-------|--------|
| `↑` `↓` / `j` `k` | scroll | Move |
| `→` / `l` | | Open folder |
| `Enter` | click the selected row | Open a folder, or open a file in its default app |
| `←` `Backspace` / `h` | right-click, or click a breadcrumb | Go up |
| `PgUp` `PgDn` `g` `G` | | Page / jump to top / bottom |
| `t` | | Switch between list view and tree view (folders expand in place: `→` expand, `←` collapse, `Enter`/click toggles) |
| `/` | | Search as you type (in tree view, also inside open folders). `Enter` keeps the filter, `Esc` clears it |
| `s` | | Sort by size → name → newest |
| `z` | | Hide / show sizes |
| `.` | | Hide / show hidden files (names starting with `.`); folder sizes still include them |
| `d` / `Delete` | | Move to Trash. A dialog opens and **only `y` confirms** |
| `D` | | Delete permanently (also asks; use when Trash isn't available) |
| `o` | | Reveal in Finder / Explorer / your file manager |
| `!` | | Open your shell in the current folder. Type `exit` to come back (dsk rescans) |
| `r` | | Rescan |
| `?` | | Show all keys |
| `q` / `Esc` | | Quit |

## Benchmark vs `du`

Apple M1 (8 cores), APFS SSD, warm cache, release build. Median of 3 runs, timed with `/usr/bin/time`.

| Tree | Entries | `dsk -0` | `du -sh` | Speedup | dsk peak RSS |
|------|--------:|-----------:|---------:|--------:|-------------:|
| Synthetic: 1,000 dirs × 1,000 empty files | 1,001,011 | **4.4 s** | 11.5 s | 2.6× | 65 MB |
| Real home directory | 334,214 | **5.4 s** | 13.8 s | 2.6× | 70 MB |

The totals match `du -sk` exactly. Speedups come from reading directories and calling `stat` in parallel. `du` is single-threaded. Reproduce with:

```sh
cargo build --release
hyperfine --warmup 1 './target/release/dsk ~ -0' 'du -sh ~'
```

## How it works

```
src/
├── main.rs      binary → dskmap::run()
├── lib.rs       wires the CLI to the scanner, tree view and TUI
├── cli.rs       clap definitions
├── scanner.rs   parallel walk (jwalk + rayon), hardlink dedupe, error collection
├── tree.rs      compact arena tree
├── output.rs    tree rendering and size formatting
├── fsops.rs     move to Trash, delete, reveal in file manager
├── settings.rs  remembered preferences
└── tui.rs       ratatui interface
```

- **Memory.** Every entry is one ~40-byte node in a flat `Vec`, linked by `u32` indices (parent, first child,
  next sibling). Names are `Box<OsStr>`. There are no per-directory `Vec`s and no stored full paths.
- **Tree building.** jwalk yields entries in depth-first order, so the scanner builds the tree with a depth
  stack instead of a path→node hash map. Directory sizes are rolled up in one reverse pass, which works
  because a child always has a larger index than its parent.
- **Parallel stat.** `stat` calls happen inside jwalk's `process_read_dir` callback on the rayon pool.
  Ignored entries are dropped there too, so ignored folders are never entered.
- **Hardlinks.** These are de-duplicated by `(device, inode)`, and only files with `nlink > 1` are tracked.

### Platform notes

- **Linux/macOS:** sizes are allocated blocks (`st_blocks × 512`), the same as `du`.
- **Windows:** sizes are apparent lengths, and hardlinks are not de-duplicated, because stable Rust `std`
  doesn't expose allocation size or file IDs there.
- **Non-UTF-8 file names:** these are displayed lossily but handled exactly, so deletion uses the real name.

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

CI runs build and test on Linux, macOS and Windows, plus clippy and rustfmt.

## License

[MIT](LICENSE)
