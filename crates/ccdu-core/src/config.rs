//! User configuration.
//!
//! Everything here has a working default, so the file is optional and an empty one behaves exactly
//! like no file at all. A malformed one is reported rather than ignored: silently falling back to
//! defaults would mean a user who thought they had protected a directory had not.
//!
//! The format is deliberately small. Options that change what ccdu *does* to files live here;
//! options that change one run live on the command line.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::plan::default_protected;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub scan: ScanConfig,
    pub safety: SafetyConfig,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct ScanConfig {
    /// Entry names never descended into. Applied to every scan unless overridden.
    pub exclude: Vec<String>,
    /// Stay on one filesystem by default.
    pub one_file_system: bool,
    /// Scanning threads. `None` picks from the machine.
    pub threads: Option<usize>,
    /// Show the treemap panel from the start.
    pub treemap: bool,
    /// Show apparent size rather than disk usage from the start.
    pub apparent: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct SafetyConfig {
    /// Extra paths that may never be operated on, on top of the built-in list. Matched exactly:
    /// the directory itself is refused, its contents are not.
    pub protect: Vec<PathBuf>,
    /// Drop the built-in list of system directories. Almost nobody should; it exists because
    /// somebody running ccdu inside a container may genuinely own `/usr`.
    pub no_default_protection: bool,
    /// Fraction of a destination filesystem to leave free after a move.
    pub headroom: f64,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        SafetyConfig { protect: Vec::new(), no_default_protection: false, headroom: 0.02 }
    }
}

impl Config {
    /// Every path that may not be operated on, built-in list plus the user's additions.
    pub fn protected(&self) -> Vec<PathBuf> {
        let mut paths =
            if self.safety.no_default_protection { Vec::new() } else { default_protected() };
        paths.extend(self.safety.protect.iter().cloned());
        paths.sort();
        paths.dedup();
        paths
    }

    /// Load the user's configuration, or the defaults when there is no file.
    ///
    /// A file that exists but does not parse is an error: a typo in a protected path should stop
    /// the program, not quietly leave that path unprotected.
    pub fn load() -> io::Result<Config> {
        match Config::path() {
            Some(path) => Config::load_from(&path),
            None => Ok(Config::default()),
        }
    }

    pub fn load_from(path: &Path) -> io::Result<Config> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(e),
        };
        toml::from_str(&text).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display()))
        })
    }

    /// Where the configuration file lives, honouring `CCDU_CONFIG`.
    pub fn path() -> Option<PathBuf> {
        if let Some(path) = std::env::var_os("CCDU_CONFIG") {
            return Some(PathBuf::from(path));
        }
        let home = std::env::var_os("HOME").map(PathBuf::from)?;
        if cfg!(target_os = "macos") {
            return Some(home.join("Library/Application Support/ccdu/config.toml"));
        }
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        Some(base.join("ccdu/config.toml"))
    }

    /// A commented file showing every option at its default, for `ccdu config --write`.
    pub fn example() -> String {
        let mut text = String::from(
            "# ccdu configuration. Every option here has a working default; delete anything you\n\
             # do not want to change. Options that alter what ccdu does to files live here, and\n\
             # options that alter one run live on the command line.\n\n",
        );
        text.push_str("[scan]\n");
        text.push_str("# Entry names never descended into.\n");
        text.push_str("exclude = []\n");
        text.push_str("# Stay on one filesystem.\n");
        text.push_str("one_file_system = false\n");
        text.push_str("# Scanning threads. Omit to pick from the machine.\n");
        text.push_str("# threads = 8\n");
        text.push_str("# Open with the treemap panel showing.\n");
        text.push_str("treemap = false\n");
        text.push_str("# Show apparent size rather than disk usage.\n");
        text.push_str("apparent = false\n\n");
        text.push_str("[safety]\n");
        text.push_str(
            "# Paths that may never be operated on, on top of the built-in system list.\n",
        );
        text.push_str(
            "# Matched exactly: the directory itself is refused, its contents are not.\n",
        );
        text.push_str("protect = []\n");
        text.push_str("# Drop the built-in list. Almost nobody should.\n");
        text.push_str("no_default_protection = false\n");
        text.push_str("# Fraction of a destination filesystem to leave free after a move.\n");
        text.push_str("headroom = 0.02\n");
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(text: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, text).unwrap();
        (dir, path)
    }

    #[test]
    fn no_file_means_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load_from(&dir.path().join("absent.toml")).unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn an_empty_file_behaves_like_no_file() {
        let (_d, path) = write("");
        assert_eq!(Config::load_from(&path).unwrap(), Config::default());
    }

    #[test]
    fn a_partial_file_leaves_the_rest_at_its_default() {
        let (_d, path) = write("[scan]\nthreads = 2\n");
        let config = Config::load_from(&path).unwrap();
        assert_eq!(config.scan.threads, Some(2));
        assert!(!config.scan.one_file_system);
        assert_eq!(config.safety.headroom, 0.02);
    }

    #[test]
    fn a_malformed_file_is_an_error_not_a_silent_default() {
        // The important case: a typo in the safety section must stop the program rather than
        // leave the user believing a path is protected when it is not.
        let (_d, path) = write("[safety]\nprotekt = [\"/important\"]\n");
        let err = Config::load_from(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("protekt"), "{err}");

        let (_d, path) = write("this is not toml at all {{{");
        assert!(Config::load_from(&path).is_err());
    }

    #[test]
    fn protected_paths_add_to_the_built_in_list() {
        let (_d, path) = write("[safety]\nprotect = [\"/srv/precious\", \"/data\"]\n");
        let config = Config::load_from(&path).unwrap();
        let protected = config.protected();

        assert!(protected.contains(&PathBuf::from("/srv/precious")));
        assert!(protected.contains(&PathBuf::from("/data")));
        assert!(protected.contains(&PathBuf::from("/usr")), "the built-ins should still be there");
    }

    #[test]
    fn the_built_in_list_can_be_dropped_deliberately() {
        let (_d, path) = write("[safety]\nno_default_protection = true\nprotect = [\"/only\"]\n");
        let protected = Config::load_from(&path).unwrap().protected();
        assert_eq!(protected, vec![PathBuf::from("/only")]);
    }

    #[test]
    fn duplicates_between_the_lists_appear_once() {
        let (_d, path) = write("[safety]\nprotect = [\"/usr\", \"/usr\"]\n");
        let protected = Config::load_from(&path).unwrap().protected();
        assert_eq!(protected.iter().filter(|p| *p == Path::new("/usr")).count(), 1);
    }

    #[test]
    fn the_example_file_parses_and_means_the_defaults() {
        let (_d, path) = write(&Config::example());
        let config = Config::load_from(&path).unwrap();
        assert_eq!(config, Config::default(), "the example drifted from the defaults it documents");
    }

    #[test]
    fn the_path_follows_the_environment() {
        // Not run in parallel with anything that reads these; the check is the precedence order.
        let previous = std::env::var_os("CCDU_CONFIG");
        unsafe { std::env::set_var("CCDU_CONFIG", "/tmp/somewhere/config.toml") };
        assert_eq!(Config::path(), Some(PathBuf::from("/tmp/somewhere/config.toml")));
        match previous {
            Some(value) => unsafe { std::env::set_var("CCDU_CONFIG", value) },
            None => unsafe { std::env::remove_var("CCDU_CONFIG") },
        }
    }
}
