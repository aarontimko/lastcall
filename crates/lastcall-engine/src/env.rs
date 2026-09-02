//! The injected process environment.
//!
//! **This file is the only place in the engine that reads `std::env`.** Every other module
//! takes an [`Env`] by reference, so unit tests construct one explicitly and can never reach
//! the real home directory, the real `~/.config/herdr`, or a live `HERDR_SOCKET_PATH`
//! (docs/spec/00-spec.md §4.4, §5.10; docs/spec/90-phase1-kickoff.md "Traps").
//! The gate grep `rg -n 'std::env::var|home_dir\(' crates/lastcall-engine/src` must match
//! only this file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A snapshot of the environment variables, home directory, and working directory the engine
/// is allowed to see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Env {
    vars: BTreeMap<String, String>,
    home: Option<PathBuf>,
    cwd: PathBuf,
}

impl Env {
    /// Capture the real process environment. Call this once, in the binary, and pass the
    /// result down; never call it from a test.
    pub fn from_process() -> Self {
        let vars = std::env::vars().collect::<BTreeMap<_, _>>();
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty());
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        Self { vars, home, cwd }
    }

    /// An empty environment (no variables, no home) rooted at `cwd`. The starting point for
    /// every test.
    pub fn empty(cwd: impl Into<PathBuf>) -> Self {
        Self {
            vars: BTreeMap::new(),
            home: None,
            cwd: cwd.into(),
        }
    }

    /// Builder: set a variable.
    #[must_use]
    pub fn with_var(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.vars.insert(key.into(), value.into());
        self
    }

    /// Builder: set the home directory.
    #[must_use]
    pub fn with_home(mut self, home: impl Into<PathBuf>) -> Self {
        self.home = Some(home.into());
        self
    }

    /// Builder: set the working directory.
    #[must_use]
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = cwd.into();
        self
    }

    /// Look up a variable. Empty values count as unset, matching how the XDG spec treats them.
    pub fn var(&self, key: &str) -> Option<&str> {
        self.vars
            .get(key)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }

    /// The home directory, if known.
    pub fn home(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    /// The working directory the program was launched from.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// `$XDG_CONFIG_HOME`, else `~/.config`.
    pub fn xdg_config_home(&self) -> Option<PathBuf> {
        self.xdg_dir("XDG_CONFIG_HOME", ".config")
    }

    /// `$XDG_STATE_HOME`, else `~/.local/state`.
    pub fn xdg_state_home(&self) -> Option<PathBuf> {
        self.xdg_dir("XDG_STATE_HOME", ".local/state")
    }

    fn xdg_dir(&self, var: &str, home_relative: &str) -> Option<PathBuf> {
        match self.var(var).map(PathBuf::from) {
            // The XDG spec says a relative XDG_* value is invalid and must be ignored.
            Some(p) if p.is_absolute() => Some(p),
            _ => self.home.as_ref().map(|h| h.join(home_relative)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_empty_has_no_vars_and_no_home() {
        let env = Env::empty("/work");
        assert_eq!(env.var("HOME"), None);
        assert_eq!(env.home(), None);
        assert_eq!(env.cwd(), Path::new("/work"));
        assert_eq!(env.xdg_config_home(), None);
        assert_eq!(env.xdg_state_home(), None);
    }

    #[test]
    fn env_xdg_dirs_prefer_absolute_var_then_home() {
        let env = Env::empty("/work").with_home("/home/u");
        assert_eq!(
            env.xdg_config_home(),
            Some(PathBuf::from("/home/u/.config"))
        );
        assert_eq!(
            env.xdg_state_home(),
            Some(PathBuf::from("/home/u/.local/state"))
        );

        let env = env
            .with_var("XDG_CONFIG_HOME", "/xdg/config")
            .with_var("XDG_STATE_HOME", "relative/ignored");
        assert_eq!(env.xdg_config_home(), Some(PathBuf::from("/xdg/config")));
        assert_eq!(
            env.xdg_state_home(),
            Some(PathBuf::from("/home/u/.local/state"))
        );
    }

    #[test]
    fn env_empty_var_counts_as_unset() {
        let env = Env::empty("/work").with_var("LASTCALL_CONFIG", "");
        assert_eq!(env.var("LASTCALL_CONFIG"), None);
    }
}
