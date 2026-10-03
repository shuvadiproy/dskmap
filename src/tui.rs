//! Interactive terminal UI (`--tui`): browse folders by size, search, sort,
//! move to Trash (with confirmation), and reveal entries in the file manager.
//! Works with the keyboard and the mouse.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use anyhow::Result;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{self, ClearType, EnterAlternateScreen};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame, Terminal};

use crate::fsops::{self, OpenFn, RemoveFn, RemoveMode};
use crate::output::{display_name, fit, format_size};
use crate::scanner::{self, ScanOptions};
use crate::settings::{self, Settings};
use crate::tree::{NONE, NodeId, NodeKind, Tree};

const SEPARATOR: &str = " › ";
const HIGHLIGHT: &str = "▶ ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortMode {
    Size,
    Name,
    Newest,
}

impl SortMode {
    fn next(self) -> Self {
        match self {
            SortMode::Size => SortMode::Name,
            SortMode::Name => SortMode::Newest,
            SortMode::Newest => SortMode::Size,
        }
    }

    fn label(self) -> &'static str {
        match self {
            SortMode::Size => "size",
            SortMode::Name => "name",
            SortMode::Newest => "newest",
        }
    }

    fn from_label(label: &str) -> Self {
        match label {
            "name" => SortMode::Name,
            "newest" => SortMode::Newest,
            _ => SortMode::Size,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Browse,
    Search,
    Help,
    Confirm(NodeId, RemoveMode),
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    None,
    Quit,
    Rescan,
    Shell,
}

enum Status {
    None,
    Info(String),
    Error(String),
}

struct App {
    tree: Tree,
    root_path: PathBuf,
    /// Display name of the scanned root (first breadcrumb segment).
    root_label: String,
    scan_opts: ScanOptions,
    remove: RemoveFn,
    open: OpenFn,
    current: NodeId,
    /// Visible rows: the current folder's children, plus expanded subfolders
    /// in tree view.
    entries: Vec<NodeId>,
    /// Tree guide lines for each row (empty in list view).
    prefixes: Vec<String>,
    /// Rows shown only because they contain a search match (drawn dimmed).
    context: Vec<bool>,
    tree_view: bool,
    /// Hide entries whose name starts with a dot.
    hide_hidden: bool,
    show_sizes: bool,
    /// Where view/sort/hidden/size choices are saved (`None` in tests).
    settings_path: Option<PathBuf>,
    /// The size choice to save. Differs from `show_sizes` when `-z` hides
    /// sizes for one run only.
    remembered_sizes: bool,
    expanded: HashSet<NodeId>,
    list: ListState,
    /// Selection index in each parent, restored when navigating back up.
    history: Vec<usize>,
    sort: SortMode,
    filter: String,
    mode: Mode,
    status: Status,
    /// Layout from the last draw, used to map mouse clicks.
    list_area: Rect,
    crumbs: Vec<(u16, u16, NodeId)>,
    /// Help popup scroll offset, and whether it needed scrolling last draw.
    help_scroll: usize,
    help_scrollable: bool,
}

/// `allow_sizes` is false for `-z`: hide sizes this run without changing the
/// remembered setting.
pub fn run(tree: Tree, root_path: &Path, scan_opts: ScanOptions, allow_sizes: bool) -> Result<()> {
    let mut app = App::new(
        tree,
        root_path,
        scan_opts,
        fsops::remove_path,
        fsops::open_with_default_app,
    );
    let settings_path = settings::default_path();
    let saved = settings_path.as_deref().map(Settings::load).unwrap_or_default();
    app.apply_settings(&saved, allow_sizes);
    app.settings_path = settings_path;

    // `ratatui::init` restores the terminal on panic; also turn mouse
    // capture off so the shell isn't left receiving mouse escape codes.
    let mut terminal = ratatui::init();
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(io::stdout(), DisableMouseCapture);
        prev_hook(info);
    }));
    execute!(io::stdout(), EnableMouseCapture)?;
    let result = app.event_loop(&mut terminal);
    let _ = execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

impl App {
    fn new(tree: Tree, root_path: &Path, scan_opts: ScanOptions, remove: RemoveFn, open: OpenFn) -> Self {
        let root_label = display_name(root_path);
        let mut app = App {
            current: tree.root(),
            tree,
            root_path: root_path.to_path_buf(),
            root_label,
            scan_opts,
            remove,
            open,
            entries: Vec::new(),
            prefixes: Vec::new(),
            context: Vec::new(),
            tree_view: false,
            hide_hidden: false,
            show_sizes: true,
            settings_path: None,
            remembered_sizes: true,
            expanded: HashSet::new(),
            list: ListState::default(),
            history: Vec::new(),
            sort: SortMode::Size,
            filter: String::new(),
            mode: Mode::Browse,
            status: Status::None,
            list_area: Rect::default(),
            crumbs: Vec::new(),
            help_scroll: 0,
            help_scrollable: false,
        };
        app.refresh(0);
        app
    }

    /// Start with remembered choices. `allow_sizes` is false for `-z`.
    fn apply_settings(&mut self, saved: &Settings, allow_sizes: bool) {
        self.tree_view = saved.tree_view;
        self.hide_hidden = saved.hide_hidden;
        self.sort = SortMode::from_label(&saved.sort);
        self.remembered_sizes = saved.show_sizes;
        self.show_sizes = saved.show_sizes && allow_sizes;
        self.refresh(0);
    }

    /// Remember the current view, sort, hidden-files and size choices.
    fn save_settings(&mut self) {
        let Some(path) = &self.settings_path else { return };
        let settings = Settings {
            show_sizes: self.remembered_sizes,
            tree_view: self.tree_view,
            hide_hidden: self.hide_hidden,
            sort: self.sort.label().into(),
        };
        if let Err(e) = settings.save(path) {
            self.status = Status::Error(format!("Could not save settings: {e}"));
        }
    }

    fn event_loop(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        loop {
            terminal.draw(|f| self.draw(f))?;
            let action = match event::read()? {
                Event::Key(key) => self.handle_key(key),
                Event::Mouse(mouse) => self.handle_mouse(mouse),
                _ => Action::None,
            };
            match action {
                Action::Quit => return Ok(()),
                Action::Rescan => {
                    terminal.draw(|f| f.render_widget(Paragraph::new(" Rescanning…"), f.area()))?;
                    self.rescan();
                }
                Action::Shell => {
                    self.open_shell(terminal)?;
                    // Files may have changed in the shell.
                    terminal.draw(|f| f.render_widget(Paragraph::new(" Rescanning…"), f.area()))?;
                    self.rescan();
                }
                Action::None => {}
            }
        }
    }

    /// Suspend the UI and run the user's shell in the current folder until
    /// they type `exit`.
    fn open_shell(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let dir = self.tree.path(self.current);
        execute!(io::stdout(), DisableMouseCapture)?;
        ratatui::restore();
        println!("dsk: shell in {}. Type 'exit' to go back to dsk.", dir.display());

        let status = Command::new(user_shell())
            .current_dir(&dir)
            .env("DSK_SHELL", "1")
            .status();

        terminal::enable_raw_mode()?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            terminal::Clear(ClearType::All),
            EnableMouseCapture
        )?;
        // A fresh handle forces a full redraw. (`Terminal::clear` would ask
        // the terminal for the cursor position, which not every terminal answers.)
        *terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        if let Err(e) = status {
            self.status = Status::Error(format!("Could not start shell: {e}"));
        }
        Ok(())
    }

    // ---- state -----------------------------------------------------------

    /// Rebuild the visible rows (filter + sort, plus open folders in tree
    /// view) and select row `select`.
    fn refresh(&mut self, select: usize) {
        let needle = self.filter.to_lowercase();
        let mut rows = Rows::default();
        self.push_rows(self.current, None, &needle, &mut rows);
        self.entries = rows.ids;
        self.prefixes = rows.prefixes;
        self.context = rows.context;
        let sel = if self.entries.is_empty() {
            None
        } else {
            Some(select.min(self.entries.len() - 1))
        };
        self.list.select(sel);
    }

    /// Refresh and keep `id` selected if it is still visible.
    fn refresh_keeping(&mut self, id: NodeId) {
        self.refresh(0);
        if let Some(i) = self.entries.iter().position(|&e| e == id) {
            self.list.select(Some(i));
        }
    }

    /// Add the children of `parent` that match the search (or contain a match
    /// in an open folder), recursing into open folders in tree view. `base` is
    /// the guide-line prefix; `None` for the top level.
    fn push_rows(&self, parent: NodeId, base: Option<&str>, needle: &str, rows: &mut Rows) {
        let mut kids: Vec<NodeId> = self
            .tree
            .children(parent)
            .filter(|&k| !self.is_hidden(k))
            .filter(|&k| self.matches(k, needle) || self.has_open_match(k, needle))
            .collect();
        self.sort_ids(&mut kids);
        let n = kids.len();
        for (i, kid) in kids.into_iter().enumerate() {
            let last = i + 1 == n;
            rows.ids.push(kid);
            rows.prefixes.push(match base {
                None => String::new(),
                Some(b) => format!("{b}{}", if last { "└─ " } else { "├─ " }),
            });
            rows.context.push(!self.matches(kid, needle));
            if self.is_open(kid) {
                let child_base = match base {
                    None => "  ".to_string(),
                    Some(b) => format!("{b}{}", if last { "   " } else { "│  " }),
                };
                self.push_rows(kid, Some(&child_base), needle, rows);
            }
        }
    }

    fn matches(&self, id: NodeId, needle: &str) -> bool {
        needle.is_empty() || self.tree.get(id).name_lossy().to_lowercase().contains(needle)
    }

    fn is_hidden(&self, id: NodeId) -> bool {
        self.hide_hidden && self.tree.get(id).name.as_encoded_bytes().first() == Some(&b'.')
    }

    fn is_open(&self, id: NodeId) -> bool {
        self.tree_view && self.expanded.contains(&id)
    }

    /// Whether an open folder has a matching entry somewhere inside its open
    /// subfolders.
    fn has_open_match(&self, id: NodeId, needle: &str) -> bool {
        self.is_open(id)
            && self
                .tree
                .children(id)
                .any(|c| !self.is_hidden(c) && (self.matches(c, needle) || self.has_open_match(c, needle)))
    }

    /// Select the first row that actually matches the search.
    fn select_first_match(&mut self) {
        if let Some(i) = self.context.iter().position(|&c| !c) {
            self.list.select(Some(i));
        }
    }

    fn sort_ids(&self, ids: &mut [NodeId]) {
        let tree = &self.tree;
        match self.sort {
            SortMode::Size => ids.sort_by(|&a, &b| {
                let (na, nb) = (tree.get(a), tree.get(b));
                nb.size.cmp(&na.size).then_with(|| na.name.cmp(&nb.name))
            }),
            SortMode::Name => ids.sort_by_cached_key(|&id| tree.get(id).name_lossy().to_lowercase()),
            SortMode::Newest => {
                // Modification times aren't kept in the tree; stat just these entries.
                let mtimes: HashMap<NodeId, SystemTime> = ids
                    .iter()
                    .map(|&id| {
                        let t = fs::symlink_metadata(tree.path(id)).and_then(|m| m.modified());
                        (id, t.unwrap_or(SystemTime::UNIX_EPOCH))
                    })
                    .collect();
                ids.sort_by(|a, b| mtimes[b].cmp(&mtimes[a]));
            }
        }
    }

    fn selected(&self) -> Option<NodeId> {
        self.list.selected().and_then(|i| self.entries.get(i).copied())
    }

    fn move_by(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let cur = self.list.selected().unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, self.entries.len() as isize - 1);
        self.list.select(Some(next as usize));
    }

    fn enter(&mut self) {
        if let Some(id) = self.selected()
            && self.tree.get(id).is_dir()
        {
            self.history.push(self.list.selected().unwrap_or(0));
            self.current = id;
            self.filter.clear();
            self.refresh(0);
        }
    }

    /// Tree view →: expand the selected folder, or step into it if already open.
    fn expand(&mut self) {
        let Some(id) = self.selected() else { return };
        if !self.tree.get(id).is_dir() {
            return;
        }
        if self.expanded.insert(id) {
            self.refresh_keeping(id);
        } else if self.tree.children(id).next().is_some() {
            self.move_by(1);
        }
    }

    /// Tree view ←: collapse the selected folder, else jump to its parent row,
    /// else go up a folder.
    fn collapse(&mut self) {
        let Some(id) = self.selected() else {
            return self.go_up();
        };
        if self.expanded.remove(&id) {
            self.refresh_keeping(id);
            return;
        }
        let parent = self.tree.get(id).parent;
        if parent == self.current {
            self.go_up();
        } else if let Some(i) = self.entries.iter().position(|&e| e == parent) {
            self.list.select(Some(i));
        }
    }

    /// Enter / click on the selected row: open it (list) or toggle it (tree).
    /// Enter / click on the selected row: open a file in its default app;
    /// for a folder, go into it (list) or expand/collapse it (tree).
    fn activate(&mut self) {
        let Some(id) = self.selected() else { return };
        if !self.tree.get(id).is_dir() {
            let path = self.tree.path(id);
            self.status = match (self.open)(&path) {
                Ok(()) => Status::Info(format!("Opened {}", self.tree.get(id).name_lossy())),
                Err(e) => Status::Error(format!("Could not open file: {e}")),
            };
        } else if !self.tree_view {
            self.enter();
        } else {
            if !self.expanded.remove(&id) {
                self.expanded.insert(id);
            }
            self.refresh_keeping(id);
        }
    }

    fn go_up(&mut self) {
        let parent = self.tree.get(self.current).parent;
        if parent == NONE {
            return;
        }
        let came_from = self.current;
        let remembered = self.history.pop();
        self.current = parent;
        self.filter.clear();
        self.refresh(0);
        // Re-select the folder we came from.
        let sel = self
            .entries
            .iter()
            .position(|&c| c == came_from)
            .or(remembered)
            .unwrap_or(0);
        self.list
            .select(Some(sel.min(self.entries.len().saturating_sub(1))));
    }

    /// Jump to an ancestor (from a breadcrumb click).
    fn go_to(&mut self, target: NodeId) {
        while self.current != target && self.tree.get(self.current).parent != NONE {
            self.go_up();
        }
    }

    fn remove_selected(&mut self, id: NodeId, mode: RemoveMode) {
        let path = self.tree.path(id);
        let size = format_size(self.tree.get(id).size);
        let sel = self.list.selected().unwrap_or(0);
        match (self.remove)(&path, mode) {
            Ok(()) => {
                self.tree.remove(id);
                self.expanded.remove(&id);
                let what = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.status = Status::Info(match mode {
                    RemoveMode::Trash => format!("Moved {what} ({size}) to the Trash"),
                    RemoveMode::Permanent => format!("Deleted {what} ({size})"),
                });
            }
            Err(e) => {
                self.status = Status::Error(match mode {
                    RemoveMode::Trash => {
                        format!("Could not move to Trash: {e}. Press D to delete permanently.")
                    }
                    RemoveMode::Permanent => format!("Could not delete: {e}. Press r to rescan."),
                });
            }
        }
        self.refresh(sel);
    }

    fn rescan(&mut self) {
        match scanner::scan(
            &self.root_path,
            &ScanOptions {
                progress: false,
                ..self.scan_opts.clone()
            },
        ) {
            Ok(res) => {
                // Try to stay in the same folder with the same folders expanded.
                let rel = self.tree.rel_path(self.current);
                let open: Vec<String> = self.expanded.iter().map(|&id| self.tree.rel_path(id)).collect();
                self.tree = res.tree;
                self.current = find_rel(&self.tree, &rel).unwrap_or(self.tree.root());
                self.expanded = open.iter().filter_map(|r| find_rel(&self.tree, r)).collect();
                self.history.clear();
                self.refresh(0);
                self.status = Status::Info("Rescanned".into());
            }
            Err(e) => self.status = Status::Error(format!("Rescan failed: {e:#}")),
        }
    }

    // ---- input -----------------------------------------------------------

    fn handle_key(&mut self, key: KeyEvent) -> Action {
        // Windows reports key releases too.
        if key.kind != KeyEventKind::Press {
            return Action::None;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }
        match self.mode {
            Mode::Browse => return self.on_browse_key(key),
            Mode::Search => self.on_search_key(key),
            Mode::Help => self.on_help_key(key),
            Mode::Confirm(id, mode) => {
                self.mode = Mode::Browse;
                // Only an explicit 'y' confirms; any other key cancels.
                if key.code == KeyCode::Char('y') {
                    self.remove_selected(id, mode);
                } else {
                    self.status = Status::Info("Cancelled".into());
                }
            }
        }
        Action::None
    }

    fn on_browse_key(&mut self, key: KeyEvent) -> Action {
        self.status = Status::None;
        let page = self.list_area.height.max(1) as isize;
        match key.code {
            KeyCode::Char('q') => return Action::Quit,
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.refresh(0);
            }
            KeyCode::Esc => return Action::Quit,
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::PageDown => self.move_by(page),
            KeyCode::PageUp => self.move_by(-page),
            KeyCode::Home | KeyCode::Char('g') => self.move_by(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.move_by(isize::MAX / 2),
            KeyCode::Enter => self.activate(),
            KeyCode::Right | KeyCode::Char('l') if self.tree_view => self.expand(),
            KeyCode::Right | KeyCode::Char('l') => self.enter(),
            KeyCode::Left | KeyCode::Char('h') if self.tree_view => self.collapse(),
            KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => self.go_up(),
            KeyCode::Char('z') => {
                self.show_sizes = !self.show_sizes;
                self.remembered_sizes = self.show_sizes;
                self.status = Status::Info(
                    if self.show_sizes {
                        "Showing sizes"
                    } else {
                        "Sizes hidden (press z to show)"
                    }
                    .into(),
                );
                self.save_settings();
            }
            KeyCode::Char('.') => {
                self.hide_hidden = !self.hide_hidden;
                match self.selected() {
                    Some(id) => self.refresh_keeping(id),
                    None => self.refresh(0),
                }
                self.status = Status::Info(
                    if self.hide_hidden {
                        "Hidden files are hidden (press . to show)"
                    } else {
                        "Showing hidden files"
                    }
                    .into(),
                );
                self.save_settings();
            }
            KeyCode::Char('t') => {
                self.tree_view = !self.tree_view;
                match self.selected() {
                    Some(id) => self.refresh_keeping(id),
                    None => self.refresh(0),
                }
                self.status = Status::Info(if self.tree_view { "Tree view" } else { "List view" }.into());
                self.save_settings();
            }
            KeyCode::Char('/') => {
                self.mode = Mode::Search;
                self.filter.clear();
                self.refresh(0);
            }
            KeyCode::Char('s') => {
                self.sort = self.sort.next();
                self.refresh(0);
                self.status = Status::Info(format!("Sorted by {}", self.sort.label()));
                self.save_settings();
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                if let Some(id) = self.selected() {
                    self.mode = Mode::Confirm(id, RemoveMode::Trash);
                }
            }
            KeyCode::Char('D') => {
                if let Some(id) = self.selected() {
                    self.mode = Mode::Confirm(id, RemoveMode::Permanent);
                }
            }
            KeyCode::Char('o') => {
                let path = self.tree.path(self.selected().unwrap_or(self.current));
                self.status = match fsops::open_in_file_manager(&path) {
                    Ok(()) => Status::Info(format!("Opened {}", path.display())),
                    Err(e) => Status::Error(format!("Could not open file manager: {e}")),
                };
            }
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char('r') => return Action::Rescan,
            KeyCode::Char('!') => return Action::Shell,
            _ => {}
        }
        Action::None
    }

    /// In the help popup, arrows scroll (when it doesn't fit); any other key
    /// closes it and does nothing else.
    fn on_help_key(&mut self, key: KeyEvent) {
        let delta: isize = match key.code {
            KeyCode::Down | KeyCode::Char('j') => 1,
            KeyCode::Up | KeyCode::Char('k') => -1,
            KeyCode::PageDown => 10,
            KeyCode::PageUp => -10,
            _ => 0,
        };
        if self.help_scrollable && delta != 0 {
            // Clamped to the content length when drawing.
            self.help_scroll = self.help_scroll.saturating_add_signed(delta);
        } else {
            self.mode = Mode::Browse;
            self.help_scroll = 0;
        }
    }

    fn on_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.filter.clear();
                self.mode = Mode::Browse;
            }
            KeyCode::Enter => self.mode = Mode::Browse,
            KeyCode::Backspace => {
                self.filter.pop();
            }
            KeyCode::Down => self.move_by(1),
            KeyCode::Up => self.move_by(-1),
            KeyCode::Char(c) => self.filter.push(c),
            _ => return,
        }
        if !matches!(key.code, KeyCode::Up | KeyCode::Down) {
            self.refresh(0);
            self.select_first_match();
        }
    }

    fn handle_mouse(&mut self, m: MouseEvent) -> Action {
        if matches!(self.mode, Mode::Confirm(..)) {
            return Action::None;
        }
        if self.mode == Mode::Help {
            match m.kind {
                MouseEventKind::ScrollDown if self.help_scrollable => self.help_scroll += 1,
                MouseEventKind::ScrollUp => self.help_scroll = self.help_scroll.saturating_sub(1),
                MouseEventKind::Down(_) => {
                    self.mode = Mode::Browse;
                    self.help_scroll = 0;
                }
                _ => {}
            }
            return Action::None;
        }
        match m.kind {
            MouseEventKind::ScrollDown => self.move_by(1),
            MouseEventKind::ScrollUp => self.move_by(-1),
            MouseEventKind::Down(MouseButton::Right) => self.go_up(),
            MouseEventKind::Down(MouseButton::Left) => {
                self.status = Status::None;
                let a = self.list_area;
                if m.row >= a.y && m.row < a.y + a.height && m.column >= a.x && m.column < a.x + a.width {
                    // Click selects; clicking the selected folder opens it.
                    let idx = self.list.offset() + (m.row - a.y) as usize;
                    if idx < self.entries.len() {
                        if self.list.selected() == Some(idx) {
                            self.activate();
                        } else {
                            self.list.select(Some(idx));
                        }
                    }
                } else if m.row == 0
                    && let Some(&(_, _, id)) = self.crumbs.iter().find(|c| m.column >= c.0 && m.column < c.1)
                {
                    self.go_to(id);
                }
            }
            _ => {}
        }
        Action::None
    }

    // ---- drawing ---------------------------------------------------------

    fn draw(&mut self, f: &mut Frame) {
        let [header, body, footer] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)])
                .areas(f.area());
        self.list_area = body;
        self.draw_header(f, header);
        self.draw_list(f, body);
        self.draw_footer(f, footer);
        match self.mode {
            Mode::Confirm(id, mode) => self.draw_confirm(f, id, mode),
            Mode::Help => self.draw_help(f),
            _ => {}
        }
    }

    /// Breadcrumb (`home › Desktop › photos`), folder size, and sort mode.
    fn draw_header(&mut self, f: &mut Frame, area: Rect) {
        let mut chain = Vec::new();
        let mut cur = self.current;
        while cur != NONE {
            chain.push(cur);
            cur = self.tree.get(cur).parent;
        }
        chain.reverse();
        let names: Vec<String> = chain
            .iter()
            .map(|&id| {
                if id == self.tree.root() {
                    self.root_label.clone()
                } else {
                    self.tree.get(id).name_lossy().into_owned()
                }
            })
            .collect();

        let size = format!("  {}", format_size(self.tree.get(self.current).size));
        let view = if self.tree_view { "tree · " } else { "" };
        let hidden = if self.hide_hidden { "no hidden · " } else { "" };
        let right = format!("{hidden}{view}sort: {} ", self.sort.label());
        let budget = (area.width as usize).saturating_sub(size.chars().count() + right.chars().count() + 2);

        // Drop leading segments until the breadcrumb fits.
        let width = |from: usize| -> usize {
            names[from..].iter().map(|n| n.chars().count()).sum::<usize>()
                + SEPARATOR.chars().count() * (names.len() - from - 1)
                + if from > 0 { 2 } else { 0 }
        };
        let mut first = 0;
        while first + 1 < names.len() && width(first) > budget {
            first += 1;
        }

        let mut spans = vec![Span::raw(" ")];
        let mut x = area.x + 1;
        self.crumbs.clear();
        if first > 0 {
            spans.push(Span::styled("… ", Style::new().fg(Color::DarkGray)));
            x += 2;
        }
        for (i, name) in names.iter().enumerate().skip(first) {
            if i > first {
                spans.push(Span::styled(SEPARATOR, Style::new().fg(Color::DarkGray)));
                x += SEPARATOR.chars().count() as u16;
            }
            let last = i + 1 == names.len();
            let name = fit(name, budget.max(1)).trim_end().to_string();
            let w = name.chars().count() as u16;
            self.crumbs.push((x, x + w, chain[i]));
            x += w;
            let style = if last {
                Style::new().add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::Gray)
            };
            spans.push(Span::styled(name, style));
        }
        spans.push(Span::styled(size, Style::new().fg(Color::DarkGray)));
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        f.render_widget(
            Paragraph::new(Line::styled(right, Style::new().fg(Color::DarkGray))).right_aligned(),
            area,
        );
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect) {
        if self.entries.is_empty() {
            let msg = if self.filter.is_empty() {
                "  (empty folder)"
            } else {
                "  (no matches)"
            };
            f.render_widget(Paragraph::new(msg).style(Style::new().fg(Color::DarkGray)), area);
            return;
        }
        let labels: Vec<(String, String, Style, String)> = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, &id)| {
                let node = self.tree.get(id);
                let prefix = match (self.tree_view, node.is_dir()) {
                    (false, _) => String::new(),
                    (true, true) if self.expanded.contains(&id) => format!("{}▾ ", self.prefixes[i]),
                    (true, true) => format!("{}▸ ", self.prefixes[i]),
                    (true, false) => format!("{}  ", self.prefixes[i]),
                };
                let (name, style) = match node.kind {
                    NodeKind::Dir => (
                        format!("{}/", node.name_lossy()),
                        Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD),
                    ),
                    NodeKind::Symlink => (format!("{} →", node.name_lossy()), Style::new().fg(Color::Cyan)),
                    _ => (node.name_lossy().into_owned(), Style::new()),
                };
                // Folders shown only because they contain a match are dimmed.
                let style = if self.context.get(i) == Some(&true) {
                    Style::new().fg(Color::DarkGray)
                } else {
                    style
                };
                (prefix, name, style, format_size(node.size))
            })
            .collect();

        let size_w = if self.show_sizes {
            labels.iter().map(|l| l.3.len()).max().unwrap_or(4)
        } else {
            0
        };
        let avail = (area.width as usize).saturating_sub(HIGHLIGHT.chars().count() + size_w + 3);
        let name_w = labels
            .iter()
            .map(|l| l.0.chars().count() + l.1.chars().count())
            .max()
            .unwrap_or(0)
            .min(avail)
            .max(1);

        let items: Vec<ListItem> = labels
            .into_iter()
            .map(|(prefix, name, style, size)| {
                let room = name_w.saturating_sub(prefix.chars().count());
                let mut spans = vec![
                    Span::styled(prefix, Style::new().fg(Color::DarkGray)),
                    Span::styled(fit(&name, room), style),
                ];
                if self.show_sizes {
                    spans.push(Span::raw(format!("  {size:>size_w$}")));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();
        let list = List::new(items)
            .highlight_style(
                Style::new()
                    .bg(Color::Rgb(50, 50, 70))
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol(HIGHLIGHT);
        f.render_stateful_widget(list, area, &mut self.list);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let dim = Style::new().fg(Color::DarkGray);
        let line = match (&self.status, self.mode) {
            (_, Mode::Search) => Line::from(vec![
                Span::styled(" Search: ", Style::new().fg(Color::Yellow)),
                Span::raw(format!("{}█", self.filter)),
                Span::styled("   Enter keep · Esc clear", dim),
            ]),
            (Status::Info(msg), _) => Line::styled(format!(" {msg}"), Style::new().fg(Color::Green)),
            (Status::Error(msg), _) => Line::styled(format!(" {msg}"), Style::new().fg(Color::Red)),
            (Status::None, _) if !self.filter.is_empty() => Line::from(vec![
                Span::styled(
                    format!(" Filter: \"{}\"", self.filter),
                    Style::new().fg(Color::Yellow),
                ),
                Span::styled("  (Esc clears)   ↑↓ move  → open  ← back  q quit", dim),
            ]),
            (Status::None, _) if self.tree_view => Line::styled(
                " ↑↓ move  → expand  ← collapse  t list view  / search  d trash  ? help  q quit",
                dim,
            ),
            (Status::None, _) => Line::styled(
                " ↑↓ move  → open  ← back  t tree view  / search  d trash  ? help  q quit",
                dim,
            ),
        };
        f.render_widget(Paragraph::new(line), area);
    }

    /// The key list, in one column if it fits, two columns if the window is
    /// short but wide, otherwise one scrollable column.
    fn draw_help(&mut self, f: &mut Frame) {
        let screen = f.area();
        let key_style = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);
        let dim = Style::new().fg(Color::DarkGray);
        let key_w = HELP.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0);
        let what_w = HELP.iter().map(|(_, w)| w.chars().count()).max().unwrap_or(0);
        let entry_w = 1 + key_w + 2 + what_w;
        let entry = |(k, what): &(&str, &str), pad: usize| {
            vec![
                Span::styled(format!(" {k:<key_w$}  "), key_style),
                Span::raw(format!("{what:<pad$}")),
            ]
        };
        let mouse = " Mouse: click selects, click again opens, right-click goes back";
        let note_w = mouse.chars().count();
        let max_inner_h = screen.height.saturating_sub(2) as usize;
        let max_inner_w = screen.width.saturating_sub(2) as usize;

        let one_col_h = HELP.len() + 3;
        let half = HELP.len().div_ceil(2);
        let two_col_w = 2 * entry_w + 2;
        let (mut lines, inner_w): (Vec<Line>, usize) = if one_col_h <= max_inner_h {
            self.help_scrollable = false;
            let mut lines: Vec<Line> = HELP.iter().map(|e| Line::from(entry(e, 0))).collect();
            lines.push(Line::from(""));
            lines.push(Line::styled(mouse, dim));
            lines.push(Line::styled(" Press any key to close", dim));
            (lines, entry_w.max(note_w))
        } else if half + 3 <= max_inner_h && two_col_w <= max_inner_w {
            self.help_scrollable = false;
            let mut lines: Vec<Line> = (0..half)
                .map(|row| {
                    let mut spans = entry(&HELP[row], what_w);
                    if let Some(right) = HELP.get(row + half) {
                        spans.push(Span::raw("  "));
                        spans.extend(entry(right, 0));
                    }
                    Line::from(spans)
                })
                .collect();
            lines.push(Line::from(""));
            lines.push(Line::styled(mouse, dim));
            lines.push(Line::styled(" Press any key to close", dim));
            (lines, two_col_w.max(note_w))
        } else {
            // Scroll: keep the last row for the hint.
            self.help_scrollable = true;
            let mut content: Vec<Line> = HELP.iter().map(|e| Line::from(entry(e, 0))).collect();
            content.push(Line::from(""));
            content.push(Line::styled(mouse, dim));
            let visible = max_inner_h.saturating_sub(1).max(1);
            self.help_scroll = self.help_scroll.min(content.len().saturating_sub(visible));
            let mut lines: Vec<Line> = content.into_iter().skip(self.help_scroll).take(visible).collect();
            lines.push(Line::styled(
                " ↑↓ scroll · any other key closes",
                Style::new().fg(Color::Cyan),
            ));
            (lines, entry_w.max(note_w))
        };
        if lines.len() > max_inner_h {
            lines.truncate(max_inner_h);
        }

        // +2 for the border, +1 so text never touches the right edge.
        let area = centered_size(screen, inner_w as u16 + 3, lines.len() as u16 + 2);
        f.render_widget(Clear, area);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(Color::Cyan))
            .title(" Keys ");
        f.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn draw_confirm(&self, f: &mut Frame, id: NodeId, mode: RemoveMode) {
        let node = self.tree.get(id);
        let what = if node.is_dir() {
            "this folder and everything in it"
        } else {
            "this file"
        };
        let (title, question, color, action) = match mode {
            RemoveMode::Trash => (
                " Move to Trash ",
                format!("Move {what} to the Trash?"),
                Color::Yellow,
                " move to Trash    ",
            ),
            RemoveMode::Permanent => (
                " Delete permanently ",
                format!("Permanently delete {what}? This cannot be undone."),
                Color::Red,
                " delete forever    ",
            ),
        };
        let text = vec![
            Line::from(question),
            Line::from(""),
            Line::styled(
                self.tree.path(id).display().to_string(),
                Style::new().add_modifier(Modifier::BOLD),
            ),
            Line::from(format!("Size: {}", format_size(node.size))),
            Line::from(""),
            Line::from(vec![
                Span::styled("[y]", Style::new().fg(color).add_modifier(Modifier::BOLD)),
                Span::raw(action),
                Span::styled(
                    "[any other key]",
                    Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" cancel"),
            ]),
        ];
        let area = centered(f.area(), 70, 10);
        f.render_widget(Clear, area);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(color))
            .title(title);
        f.render_widget(Paragraph::new(text).block(block).wrap(Wrap { trim: false }), area);
    }
}

/// The user's login shell (`$SHELL`), or the platform default.
fn user_shell() -> std::ffi::OsString {
    if cfg!(windows) {
        std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into())
    } else {
        std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into())
    }
}

/// Rows produced by [`App::push_rows`].
#[derive(Default)]
struct Rows {
    ids: Vec<NodeId>,
    prefixes: Vec<String>,
    context: Vec<bool>,
}

/// Every key, shown by `?`.
const HELP: &[(&str, &str)] = &[
    ("↑ ↓  j k", "move (mouse: scroll)"),
    ("→  l", "open folder / expand in tree view"),
    ("←  h", "go back / collapse in tree view"),
    ("Enter", "open folder, or open file in its app"),
    ("PgUp PgDn", "page up / down"),
    ("g  G", "jump to top / bottom"),
    ("t", "switch list view / tree view"),
    ("/", "search (Enter keeps, Esc clears)"),
    ("s", "sort: size → name → newest"),
    (".", "hide / show hidden files"),
    ("z", "hide / show sizes"),
    ("d", "move to Trash (asks first)"),
    ("D", "delete permanently (asks first)"),
    ("o", "show in Finder / file manager"),
    ("!", "open a shell here (exit to return)"),
    ("r", "rescan"),
    ("q  Esc", "quit"),
];

/// A `width` × `height` rectangle centered in `area` (clamped to fit).
fn centered_size(area: Rect, width: u16, height: u16) -> Rect {
    let (width, height) = (width.min(area.width), height.min(area.height));
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

fn centered(area: Rect, percent_x: u16, height: u16) -> Rect {
    let width = (area.width * percent_x / 100).max(20).min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

fn find_rel(tree: &Tree, rel: &str) -> Option<NodeId> {
    let mut cur = tree.root();
    for part in rel.split('/').filter(|p| !p.is_empty()) {
        cur = tree.children(cur).find(|&c| tree.get(c).name_lossy() == part)?;
    }
    Some(cur)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::sync::Mutex;

    /// Records the requested mode, then deletes permanently so tests never
    /// touch the real Trash.
    static LAST_MODE: Mutex<Option<RemoveMode>> = Mutex::new(None);
    fn fake_remove(path: &Path, mode: RemoveMode) -> io::Result<()> {
        *LAST_MODE.lock().unwrap() = Some(mode);
        fsops::remove_path(path, RemoveMode::Permanent)
    }

    static LAST_OPENED: Mutex<Option<PathBuf>> = Mutex::new(None);
    fn fake_open(path: &Path) -> io::Result<()> {
        *LAST_OPENED.lock().unwrap() = Some(path.to_path_buf());
        Ok(())
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn click(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn setup() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        fs::create_dir_all(r.join("big/inner")).unwrap();
        fs::write(r.join("big/inner/data.bin"), vec![1u8; 50_000]).unwrap();
        fs::write(r.join("small.txt"), vec![1u8; 1_000]).unwrap();
        fs::write(r.join("apple.txt"), vec![1u8; 10]).unwrap();
        let opts = ScanOptions {
            apparent_size: true,
            ..Default::default()
        };
        let tree = scanner::scan(r, &opts).unwrap().tree;
        let app = App::new(tree, r, opts, fake_remove, fake_open);
        (dir, app)
    }

    fn screen(app: &mut App) -> String {
        screen_sized(app, 90, 12)
    }

    fn screen_sized(app: &mut App, width: u16, height: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let buf = term.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn names(app: &App) -> Vec<String> {
        app.entries
            .iter()
            .map(|&id| app.tree.get(id).name_lossy().into_owned())
            .collect()
    }

    #[test]
    fn shows_only_names_and_sizes() {
        let (_dir, mut app) = setup();
        let s = screen(&mut app);
        assert!(s.contains("big/"), "{s}");
        assert!(s.contains("49K"), "{s}");
        assert!(!s.contains('%'), "no percentages: {s}");
        assert!(!s.contains('█'), "no bars: {s}");
    }

    #[test]
    fn navigates_with_keys_and_breadcrumb() {
        let (_dir, mut app) = setup();
        app.handle_key(key(KeyCode::Enter)); // big/ is first (largest)
        app.handle_key(key(KeyCode::Enter)); // inner/
        let s = screen(&mut app);
        assert!(s.lines().next().unwrap().contains("big › inner"), "{s}");
        app.handle_key(key(KeyCode::Left));
        app.handle_key(key(KeyCode::Left));
        assert_eq!(app.current, app.tree.root());
        assert_eq!(
            names(&app)[app.list.selected().unwrap()],
            "big",
            "re-selects the folder we came from"
        );
        assert_eq!(app.handle_key(key(KeyCode::Char('!'))), Action::Shell);
        assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Action::Quit);
    }

    #[test]
    fn mouse_click_selects_then_opens_and_breadcrumb_goes_back() {
        let (_dir, mut app) = setup();
        screen(&mut app); // compute layout
        let y0 = app.list_area.y;
        let row_of = |i: u16| y0 + i;
        app.handle_mouse(click(5, row_of(1)));
        assert_eq!(app.list.selected(), Some(1));
        app.handle_mouse(click(5, row_of(0)));
        app.handle_mouse(click(5, row_of(0))); // second click on the selected folder opens it
        assert_eq!(app.tree.get(app.current).name_lossy(), "big");

        screen(&mut app);
        let (x0, _, root) = app.crumbs[0];
        app.handle_mouse(click(x0, 0));
        assert_eq!(app.current, root);
    }

    #[test]
    fn tree_view_expands_and_collapses_in_place() {
        let (dir, mut app) = setup();
        app.handle_key(key(KeyCode::Char('t')));
        assert!(app.tree_view);
        assert!(screen(&mut app).contains("▸ big/"));

        app.handle_key(key(KeyCode::Right)); // expand big/
        assert_eq!(names(&app), vec!["big", "inner", "small.txt", "apple.txt"]);
        let s = screen(&mut app);
        assert!(s.contains("▾ big/") && s.contains("└─ ▸ inner/"), "{s}");

        app.handle_key(key(KeyCode::Right)); // already open: step to first child
        assert_eq!(names(&app)[app.list.selected().unwrap()], "inner");
        app.handle_key(key(KeyCode::Enter)); // toggle open inner/
        assert!(names(&app).contains(&"data.bin".to_string()));
        assert!(screen(&mut app).contains("   └─   data.bin"));

        app.handle_key(key(KeyCode::Left)); // collapse inner/
        assert!(!names(&app).contains(&"data.bin".to_string()));
        app.handle_key(key(KeyCode::Left)); // not expanded: jump to parent row
        assert_eq!(names(&app)[app.list.selected().unwrap()], "big");
        app.handle_key(key(KeyCode::Left)); // collapse big/
        assert_eq!(names(&app).len(), 3);
        assert_eq!(app.current, app.tree.root(), "stays in the same folder");

        // Actions still work on nested rows; expansions survive.
        app.handle_key(key(KeyCode::Right));
        app.handle_key(key(KeyCode::Down)); // inner/
        app.handle_key(key(KeyCode::Char('d')));
        app.handle_key(key(KeyCode::Char('y')));
        assert!(!dir.path().join("big/inner").exists());
        // big/ is now empty, so it sorts last by size.
        assert_eq!(names(&app), vec!["small.txt", "apple.txt", "big"]);

        app.handle_key(key(KeyCode::Char('t')));
        assert!(!app.tree_view);
        assert!(!screen(&mut app).contains('▸'));
    }

    #[test]
    fn tree_search_finds_matches_inside_open_folders() {
        let (_dir, mut app) = setup();
        app.handle_key(key(KeyCode::Char('t')));
        app.handle_key(key(KeyCode::Right)); // open big/
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Right)); // open big/inner/

        app.handle_key(key(KeyCode::Char('/')));
        for c in "DATA".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        // The match plus the open folders that lead to it.
        assert_eq!(names(&app), vec!["big", "inner", "data.bin"]);
        assert_eq!(app.context, vec![true, true, false]);
        assert_eq!(
            names(&app)[app.list.selected().unwrap()],
            "data.bin",
            "selects the match"
        );
        let s = screen(&mut app);
        assert!(s.contains("└─   data.bin"), "{s}");

        // Closed folders are not searched.
        app.handle_key(key(KeyCode::Esc)); // clears the search; big/ is selected
        app.handle_key(key(KeyCode::Left)); // close big/
        app.handle_key(key(KeyCode::Char('/')));
        app.handle_key(key(KeyCode::Char('d')));
        app.handle_key(key(KeyCode::Char('a')));
        app.handle_key(key(KeyCode::Char('t')));
        assert!(names(&app).is_empty(), "{:?}", names(&app));
        assert!(screen(&mut app).contains("(no matches)"));
    }

    #[test]
    fn enter_opens_files_in_default_app() {
        let (dir, mut app) = setup();
        app.handle_key(key(KeyCode::Down)); // small.txt
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            LAST_OPENED.lock().unwrap().as_deref(),
            Some(dir.path().join("small.txt").as_path())
        );
        assert!(screen(&mut app).contains("Opened small.txt"));
        assert_eq!(app.current, app.tree.root(), "opening a file doesn't navigate");
    }

    #[test]
    fn dot_toggles_hidden_files() {
        let (dir, _) = setup();
        fs::write(dir.path().join(".secret"), b"x").unwrap();
        fs::create_dir(dir.path().join(".cache")).unwrap();
        fs::write(dir.path().join(".cache/blob"), vec![0u8; 5000]).unwrap();
        let opts = ScanOptions {
            apparent_size: true,
            ..Default::default()
        };
        let tree = scanner::scan(dir.path(), &opts).unwrap().tree;
        let mut app = App::new(tree, dir.path(), opts, fake_remove, fake_open);
        assert!(names(&app).contains(&".cache".to_string()));

        app.handle_key(key(KeyCode::Char('.')));
        assert_eq!(names(&app), vec!["big", "small.txt", "apple.txt"]);
        let s = screen(&mut app);
        assert!(s.contains("no hidden"), "{s}");
        // Folder totals still include hidden content.
        assert_eq!(app.tree.get(app.current).size, 50_000 + 1_000 + 10 + 1 + 5_000);

        app.handle_key(key(KeyCode::Char('.')));
        assert_eq!(names(&app).len(), 5);
    }

    #[test]
    fn question_mark_shows_help_and_any_key_closes_it() {
        let (_dir, mut app) = setup();
        assert!(screen(&mut app).contains("? help"));
        app.handle_key(key(KeyCode::Char('?')));
        let s = screen(&mut app);
        assert!(s.contains(" Keys ") && s.contains("open folder / expand"), "{s}");
        // 'q' only closes the popup; it doesn't quit.
        assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Action::None);
        assert_eq!(app.mode, Mode::Browse);
        assert!(!screen(&mut app).contains(" Keys "));
    }

    #[test]
    fn help_fits_any_window() {
        let (_dir, mut app) = setup();
        app.handle_key(key(KeyCode::Char('?')));

        // Tall window: one column, everything visible.
        let s = screen_sized(&mut app, 80, 30);
        assert!(
            s.contains("move (mouse: scroll)") && s.contains("quit") && s.contains("Press any key"),
            "{s}"
        );
        assert!(!app.help_scrollable);

        // Short but wide: two columns, everything visible.
        let s = screen_sized(&mut app, 140, 14);
        assert!(!app.help_scrollable, "{s}");
        let row = s.lines().find(|l| l.contains("move (mouse: scroll)")).unwrap();
        let partner = HELP[HELP.len().div_ceil(2)].1;
        assert!(row.contains(partner), "two entries share a row: {s}");
        assert!(s.contains("Press any key"), "{s}");

        // Short and narrow: scrollable; arrows scroll instead of closing.
        let s = screen_sized(&mut app, 70, 10);
        assert!(app.help_scrollable);
        assert!(s.contains("↑↓ scroll") && !s.contains("rescan"), "{s}");
        for _ in 0..30 {
            app.handle_key(key(KeyCode::Down));
        }
        assert_eq!(app.mode, Mode::Help);
        let s = screen_sized(&mut app, 70, 10);
        assert!(
            s.contains("rescan") && s.contains("Mouse:"),
            "scrolled to the end: {s}"
        );
        app.handle_key(key(KeyCode::Char('x')));
        assert_eq!(app.mode, Mode::Browse);
        assert_eq!(app.help_scroll, 0);
    }

    #[test]
    fn remembers_view_sort_hidden_and_sizes() {
        let (dir, mut app) = setup();
        let path = dir.path().join("cfg/settings.toml");
        app.settings_path = Some(path.clone());
        for c in ['t', 's', '.', 'z'] {
            app.handle_key(key(KeyCode::Char(c)));
        }
        let saved = Settings::load(&path);
        assert_eq!(
            saved,
            Settings {
                show_sizes: false,
                tree_view: true,
                hide_hidden: true,
                sort: "name".into()
            }
        );

        // A fresh session starts the same way.
        let (_dir2, mut next) = setup();
        next.apply_settings(&saved, true);
        assert!(next.tree_view && next.hide_hidden && !next.show_sizes);
        assert_eq!(next.sort, SortMode::Name);
        assert_eq!(names(&next), vec!["apple.txt", "big", "small.txt"]);
    }

    #[test]
    fn dash_z_does_not_overwrite_the_saved_size_choice() {
        let (dir, mut app) = setup();
        let path = dir.path().join("cfg/settings.toml");
        app.settings_path = Some(path.clone());
        app.apply_settings(&Settings::default(), false); // `dsk -z`
        assert!(!app.show_sizes);
        app.handle_key(key(KeyCode::Char('t'))); // saves other settings
        assert!(Settings::load(&path).show_sizes, "-z is for this run only");
    }

    #[test]
    fn z_remembers_the_choice() {
        let (dir, mut app) = setup();
        let path = dir.path().join("cfg/settings.toml");
        app.settings_path = Some(path.clone());
        app.handle_key(key(KeyCode::Char('z')));
        assert!(!Settings::load(&path).show_sizes);
        app.handle_key(key(KeyCode::Char('z')));
        assert!(Settings::load(&path).show_sizes);
    }

    #[test]
    fn z_toggles_sizes() {
        let (_dir, mut app) = setup();
        assert!(screen(&mut app).contains("49K"));
        app.handle_key(key(KeyCode::Char('z')));
        let s = screen(&mut app);
        let row = s.lines().find(|l| l.contains("big/")).unwrap();
        assert!(!row.contains('K'), "no size on rows: {s}");
        assert!(
            s.lines().next().unwrap().contains("50K"),
            "header keeps the total: {s}"
        );
        assert_eq!(names(&app)[0], "big", "still sorted by size");
        app.handle_key(key(KeyCode::Char('z')));
        assert!(screen(&mut app).contains("49K"));
    }

    #[test]
    fn sort_cycles_size_name_newest() {
        let (_dir, mut app) = setup();
        assert_eq!(names(&app), vec!["big", "small.txt", "apple.txt"]);
        app.handle_key(key(KeyCode::Char('s')));
        assert_eq!(names(&app), vec!["apple.txt", "big", "small.txt"]);
        app.handle_key(key(KeyCode::Char('s')));
        assert_eq!(app.sort, SortMode::Newest);
        assert!(screen(&mut app).contains("sort: newest"));
        app.handle_key(key(KeyCode::Char('s')));
        assert_eq!(app.sort, SortMode::Size);
    }

    #[test]
    fn search_filters_and_esc_clears() {
        let (_dir, mut app) = setup();
        app.handle_key(key(KeyCode::Char('/')));
        for c in "TXT".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(names(&app), vec!["small.txt", "apple.txt"], "case-insensitive");
        assert!(screen(&mut app).contains("Search: TXT"));
        app.handle_key(key(KeyCode::Enter)); // keep filter, back to browsing
        assert_eq!(app.mode, Mode::Browse);
        assert_eq!(names(&app).len(), 2);
        assert_eq!(
            app.handle_key(key(KeyCode::Esc)),
            Action::None,
            "first Esc clears the filter"
        );
        assert_eq!(names(&app).len(), 3);
    }

    #[test]
    fn d_moves_to_trash_only_after_y() {
        let (dir, mut app) = setup();
        let target = dir.path().join("big");

        app.handle_key(key(KeyCode::Char('d')));
        assert!(screen(&mut app).contains("Move to Trash"));
        app.handle_key(key(KeyCode::Char('n')));
        assert!(target.exists(), "cancel must not remove anything");

        app.handle_key(key(KeyCode::Char('d')));
        app.handle_key(key(KeyCode::Enter));
        assert!(target.exists(), "Enter is not confirmation");

        app.handle_key(key(KeyCode::Char('d')));
        app.handle_key(key(KeyCode::Char('y')));
        assert!(!target.exists());
        assert_eq!(*LAST_MODE.lock().unwrap(), Some(RemoveMode::Trash));
        assert_eq!(app.entries.len(), 2);
        assert!(screen(&mut app).contains("to the Trash"));
    }
}
