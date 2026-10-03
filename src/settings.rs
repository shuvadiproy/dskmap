//! Remembered preferences for the interactive browser, stored as a tiny
//! `key = value` file at `~/.config/dsk/settings.toml`
//! (`%APPDATA%\dsk\settings.toml` on Windows).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub show_sizes: bool,
    pub tree_view: bool,
    pub hide_hidden: bool,
    /// Sort order: `size`, `name` or `newest`.
    pub sort: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            show_sizes: true,
            tree_view: false,
            hide_hidden: false,
            sort: "size".into(),
        }
    }
}

/// Where settings live, or `None` if no home/config directory is known.
pub fn default_path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    base.map(|b| b.join("dsk").join("settings.toml"))
}

impl Settings {
    /// Load settings; a missing file or unknown/invalid lines give defaults.
    pub fn load(path: &Path) -> Self {
        let mut settings = Settings::default();
        let Ok(text) = fs::read_to_string(path) else {
            return settings;
        };
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            let flag = match value {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            };
            match (key.trim(), flag) {
                ("show_sizes", Some(b)) => settings.show_sizes = b,
                ("tree_view", Some(b)) => settings.tree_view = b,
                ("hide_hidden", Some(b)) => settings.hide_hidden = b,
                ("sort", _) if matches!(value, "size" | "name" | "newest") => settings.sort = value.into(),
                _ => {}
            }
        }
        settings
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let text = format!(
            "# dsk settings\nshow_sizes = {}\ntree_view = {}\nhide_hidden = {}\nsort = {}\n",
            self.show_sizes, self.tree_view, self.hide_hidden, self.sort
        );
        fs::write(path, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Settings::load(&dir.path().join("nope.toml")), Settings::default());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dsk/settings.toml");
        let custom = Settings {
            show_sizes: false,
            tree_view: true,
            hide_hidden: true,
            sort: "newest".into(),
        };
        custom.save(&path).unwrap();
        assert_eq!(Settings::load(&path), custom);
        Settings::default().save(&path).unwrap();
        assert_eq!(Settings::load(&path), Settings::default());
    }

    #[test]
    fn ignores_junk_and_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.toml");
        fs::write(
            &path,
            "garbage\nfuture_key = 1\nshow_sizes = maybe\nsort = biggest\n",
        )
        .unwrap();
        assert_eq!(Settings::load(&path), Settings::default());
        // Older files with only some keys still load.
        fs::write(&path, "  show_sizes=false  \n").unwrap();
        let s = Settings::load(&path);
        assert!(!s.show_sizes);
        assert!(!s.tree_view);
    }
}
