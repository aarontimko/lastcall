//! Configuration layer (docs/spec/00-spec.md §6.1, frozen v1.0).
//!
//! Resolution order for the config file: `LASTCALL_CONFIG` (explicit; missing is an error) →
//! `$XDG_CONFIG_HOME/lastcall/config.toml` → `~/.config/lastcall/config.toml` → built-in
//! defaults when no file exists (not an error). State dir: `LASTCALL_STATE_DIR` →
//! `$XDG_STATE_HOME/lastcall` → `~/.local/state/lastcall`.
//!
//! Every environment read goes through the injected [`Env`]; nothing here touches `std::env`.

pub mod write;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::env::Env;

/// Default `search_depth`: the repositories directly inside each parent dir, which is what
/// every release before Amendment v1.11 did and the only thing it did.
pub const DEFAULT_SEARCH_DEPTH: u8 = 1;

/// The deepest `search_depth` the walk will go, and the draft glob walk's own ceiling.
pub const MAX_SEARCH_DEPTH: u8 = 4;

/// Default `collapse_size_bytes`: 512 KiB.
pub const DEFAULT_COLLAPSE_SIZE_BYTES: u64 = 512 * 1024;

/// Default `draft_dir_parents`: one folder above the matched folder, so a scratch folder
/// called the same thing in two projects reads as two different rows.
pub const DEFAULT_DRAFT_DIR_PARENTS: u8 = 1;

/// The most folders above a matched folder a name may carry.
pub const MAX_DRAFT_DIR_PARENTS: u8 = 4;

/// Default `collapsed_globs`: common lockfiles (§6.1).
pub const DEFAULT_COLLAPSED_GLOBS: &[&str] = &[
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "Cargo.lock",
    "poetry.lock",
    "uv.lock",
    "Gemfile.lock",
    "go.sum",
    "composer.lock",
];

/// Default `ignore_globs`: watch-set noise filters (§6.1).
pub const DEFAULT_IGNORE_GLOBS: &[&str] = &[
    ".git/**",
    "node_modules/**",
    "target/**",
    "vendor/**",
    ".venv/**",
];

/// `config.toml`, every v1 key. All keys are optional; unknown keys are a load error.
///
/// `deny_unknown_fields` is **required** on config types (gate test) and **forbidden** on
/// herdr-facing types in `crate::herdr::wire` — the two are opposite on purpose: our own
/// file must not silently accept a typo, while herdr's wire surface must tolerate anything a
/// newer server adds (§5.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// Absolute paths to watch. Empty means "the launch cwd" (§6.1, G0 Q4).
    pub parent_dirs: Vec<PathBuf>,
    /// Globs relative to parent dirs (e.g. `"_drafts/**"`) or absolute paths. An entry
    /// with a `/**` suffix reads the whole tree below the folder it matches; without the
    /// suffix it reads that one folder. The pattern that picks the folder is the entry
    /// without the suffix, so `notes/**` and `notes` match the same folders.
    pub draft_dirs: Vec<String>,
    /// What a draft root's first sight means (§6.2).
    pub draft_initial: DraftInitial,
    /// Generated files rendered as a single accept row.
    pub collapsed_globs: Vec<String>,
    /// Files at or above this size are collapsed. Must be > 0.
    pub collapse_size_bytes: u64,
    /// Watch-set noise filters; scope the watcher only, never pending computation (§6.5).
    pub ignore_globs: Vec<String>,
    /// The TUI's opening answer to `t` (Amendment v1.9, §6.1): `false` — the default the
    /// sponsor ruled — lists **every** repo under the parent dirs, the ones with nothing
    /// pending included; `true` starts with those hidden. Engine-side only as a value the
    /// binary reads; nothing here changes what a scan or `status` reports.
    pub hide_empty_repos: bool,
    /// How many folders below each parent directory discovery reads for a repository
    /// (Amendment v1.11, §6.1). `1` — the default, and every release before this one — is
    /// the repositories directly inside each parent; `2` also reads one plain folder
    /// further (`worktrees/<name>`) and lists a linked worktree kept inside a listed
    /// repository; up to `4`, the walk's ceiling. The walk never enters a repository or a
    /// dependency folder, so what it costs is a `read_dir` per plain folder.
    pub search_depth: u8,
    /// How many folder names above a matched draft folder its displayed name carries
    /// (Amendment v1.13, §6.1). `1` — the default — shows a `z_ignore` folder inside a
    /// project called `alpha` as `alpha/z_ignore`, so two projects with the same scratch
    /// folder name never read as one row; `0` is the folder's own name alone; up to `4`.
    /// A folder nearer the filesystem root than the setting asks for shows what exists.
    pub draft_dir_parents: u8,
    /// The `[ui]` table (Amendment v1.14).
    pub ui: UiConfig,
    /// The `[herdr]` table.
    pub herdr: HerdrConfig,
    /// The `[update]` table (Amendment v1.10 item 2).
    pub update: UpdateConfig,
    /// The `[keys]` table (Amendment v1.3): `<action> = "<key>"` or `["<key>", …]`, each
    /// entry replacing that action's default bindings. Opaque here — only the TUI knows the
    /// action names and key grammar, so `lastcall config` and `lastcall tui` validate it
    /// (§11 "Keybinding config validated in the binary, not the engine").
    pub keys: BTreeMap<String, KeySpecs>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            parent_dirs: Vec::new(),
            draft_dirs: Vec::new(),
            draft_initial: DraftInitial::Seen,
            collapsed_globs: DEFAULT_COLLAPSED_GLOBS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            collapse_size_bytes: DEFAULT_COLLAPSE_SIZE_BYTES,
            ignore_globs: DEFAULT_IGNORE_GLOBS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            hide_empty_repos: false,
            search_depth: DEFAULT_SEARCH_DEPTH,
            draft_dir_parents: DEFAULT_DRAFT_DIR_PARENTS,
            ui: UiConfig::default(),
            herdr: HerdrConfig::default(),
            update: UpdateConfig::default(),
            keys: BTreeMap::new(),
        }
    }
}

/// One `[keys]` entry: a single key spec or a list of them. The shape is checked here (a
/// wrong type fails every command, like any other config field); what the specs mean is
/// the binary's business.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum KeySpecs {
    One(String),
    Many(Vec<String>),
}

impl<'de> Deserialize<'de> for KeySpecs {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            One(String),
            Many(Vec<String>),
        }
        match Raw::deserialize(d) {
            Ok(Raw::One(s)) => Ok(KeySpecs::One(s)),
            Ok(Raw::Many(v)) => Ok(KeySpecs::Many(v)),
            // serde's untagged message ("data did not match any variant…") names nothing
            Err(_) => Err(serde::de::Error::custom(
                "a [keys] entry must be a key spec (a string) or a list of them",
            )),
        }
    }
}

impl KeySpecs {
    /// The specs in order, one or many.
    pub fn specs(&self) -> Vec<&str> {
        match self {
            KeySpecs::One(s) => vec![s.as_str()],
            KeySpecs::Many(v) => v.iter().map(String::as_str).collect(),
        }
    }
}

/// `draft_initial`: `seen | pending`, default `seen`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DraftInitial {
    #[default]
    Seen,
    Pending,
}

/// The `[herdr]` table.
///
/// No `derive(Default)`: `toast` defaults to `true`, which a derive cannot express.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)] // required on config types; see `Config`
pub struct HerdrConfig {
    /// `auto | on | off`, default `auto`.
    pub mode: HerdrMode,
    /// Optional named-session pin (§6.6).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Ask herdr to show a desktop notification when a repo first goes ready. Default `true`.
    pub toast: bool,
    /// Which repos the herdr overlay covers, `workspace | all`. Default `workspace`.
    pub scope: HerdrScope,
}

impl Default for HerdrConfig {
    fn default() -> Self {
        Self {
            mode: HerdrMode::default(),
            session: None,
            toast: true,
            scope: HerdrScope::default(),
        }
    }
}

/// The `[ui]` table (§6.1, Amendment v1.14).
///
/// No `derive(Default)`: `wrap` defaults to `true`, which a derive cannot express.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)] // required on config types; see `Config`
pub struct UiConfig {
    /// Visual word wrap in the diff pane. Default `true`: a review tool that clips the end
    /// of a line asks the reader to accept text they have not read. Engine-side only as a
    /// value the binary reads; nothing here changes what a scan or `status` reports, and
    /// the TUI's own toggle (`c`, Option-z, `alt-z`) is for the session only and writes nothing.
    pub wrap: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self { wrap: true }
    }
}

/// The `[update]` table (§6.1, Amendment v1.10 item 2).
///
/// No `derive(Default)`: `check` defaults to `true`, which a derive cannot express.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)] // required on config types; see `Config`
pub struct UpdateConfig {
    /// Ask the GitHub releases API, once a day in the background after the first frame,
    /// whether a newer release exists. Default `true`. `false` is the only switch: there
    /// is no environment override (ruling P2). It governs the TUI's background check only,
    /// never the explicit `lastcall update` / `lastcall update --check`, which the user
    /// asked for by typing it.
    pub check: bool,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self { check: true }
    }
}

/// `herdr.mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum HerdrMode {
    #[default]
    Auto,
    On,
    Off,
}

/// `herdr.scope`: which repos the overlay covers when a workspace can be identified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum HerdrScope {
    /// Only the repos of the herdr workspace this pane belongs to.
    #[default]
    Workspace,
    /// Every watched repo, whatever workspace it belongs to.
    All,
}

/// Where the effective config came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConfigSource {
    /// Parsed from this file.
    File { path: PathBuf },
    /// No file found; built-in defaults. `searched` lists the paths that were tried.
    Defaults { searched: Vec<PathBuf> },
}

impl ConfigSource {
    /// The file path, if the config came from a file.
    pub fn path(&self) -> Option<&Path> {
        match self {
            ConfigSource::File { path } => Some(path),
            ConfigSource::Defaults { .. } => None,
        }
    }

    /// A short human label for notices and the `config` command.
    pub fn label(&self) -> String {
        match self {
            ConfigSource::File { path } => path.display().to_string(),
            ConfigSource::Defaults { .. } => "(built-in defaults, no config file)".to_string(),
        }
    }
}

/// A loaded, validated config plus where it came from and the state directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Loaded {
    pub config: Config,
    pub source: ConfigSource,
    pub state_dir: PathBuf,
}

/// The per-run resolution of `parent_dirs` against the launch cwd (G0 Q4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Resolved {
    /// The directories watched this run.
    pub parent_dirs: Vec<PathBuf>,
    /// One-line notices for the UI/status bar. Never errors.
    pub notices: Vec<String>,
}

/// Errors from loading or validating config. Every variant that concerns a file names it.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config file {path} (from LASTCALL_CONFIG) does not exist")]
    ExplicitFileMissing { path: PathBuf },
    #[error("cannot read config file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("config file {path}{}: {message}", line.map(|l| format!(", line {l}")).unwrap_or_default())]
    Parse {
        path: PathBuf,
        line: Option<usize>,
        message: String,
    },
    #[error("config file {path}: {message}")]
    Invalid { path: PathBuf, message: String },
    #[error(
        "cannot determine the state directory: set LASTCALL_STATE_DIR, XDG_STATE_HOME, or HOME"
    )]
    NoStateDir,
}

/// Resolve the config file path per §6.1. `Ok(None)` means "no file, use defaults".
/// `searched` collects every candidate that was considered.
pub fn config_path(env: &Env) -> Result<(Option<PathBuf>, Vec<PathBuf>), ConfigError> {
    let mut searched = Vec::new();
    if let Some(explicit) = env.var("LASTCALL_CONFIG") {
        let path = PathBuf::from(explicit);
        searched.push(path.clone());
        if !path.is_file() {
            return Err(ConfigError::ExplicitFileMissing { path });
        }
        return Ok((Some(path), searched));
    }
    // Env::xdg_config_home already implements "$XDG_CONFIG_HOME, else ~/.config".
    if let Some(dir) = env.xdg_config_home() {
        let path = dir.join("lastcall").join("config.toml");
        searched.push(path.clone());
        if path.is_file() {
            return Ok((Some(path), searched));
        }
    }
    Ok((None, searched))
}

/// Resolve the state directory per §6.1.
///
/// A relative `$LASTCALL_STATE_DIR` is joined onto the launch cwd (verifier (a) F3). The
/// store already landed at `<cwd>/<value>` — every open goes through the process's own cwd
/// — but the path was *printed* verbatim by `lastcall config` and, since Amendment v1.9,
/// by `status --json`'s `state_dir`, whose whole job is to tell two runs apart. A bare
/// `relstate` cannot do that. Joined, never canonicalised: no symlink is resolved and no
/// existence is required, so a store that does not exist yet still names itself.
/// The `xdg_state_home` fallback is absolute already (`Env::xdg_dir` ignores a relative
/// `XDG_STATE_HOME` per the XDG spec and falls back to `$HOME`).
pub fn state_dir(env: &Env) -> Result<PathBuf, ConfigError> {
    if let Some(explicit) = env.var("LASTCALL_STATE_DIR") {
        return Ok(env.cwd().join(explicit));
    }
    env.xdg_state_home()
        .map(|dir| dir.join("lastcall"))
        .ok_or(ConfigError::NoStateDir)
}

/// Load, parse, and validate the effective config for this environment.
pub fn load(env: &Env) -> Result<Loaded, ConfigError> {
    let (path, searched) = config_path(env)?;
    let state_dir = state_dir(env)?;
    match path {
        Some(path) => {
            let contents = std::fs::read_to_string(&path).map_err(|source| ConfigError::Read {
                path: path.clone(),
                source,
            })?;
            let config = Config::parse(&contents, &path)?;
            Ok(Loaded {
                config,
                source: ConfigSource::File { path },
                state_dir,
            })
        }
        None => Ok(Loaded {
            config: Config::default(),
            source: ConfigSource::Defaults { searched },
            state_dir,
        }),
    }
}

impl Config {
    /// Parse and validate TOML text. `path` is used only for error messages.
    pub fn parse(contents: &str, path: &Path) -> Result<Self, ConfigError> {
        let config: Config = toml::from_str(contents).map_err(|err| ConfigError::Parse {
            path: path.to_path_buf(),
            line: err.span().map(|span| {
                contents[..span.start.min(contents.len())]
                    .matches('\n')
                    .count()
                    + 1
            }),
            message: err.message().to_string(),
        })?;
        config.validate(path)?;
        Ok(config)
    }

    /// Validation rules (§6.1 plus the kickoff's deliverable 4).
    pub fn validate(&self, path: &Path) -> Result<(), ConfigError> {
        let invalid = |message: String| ConfigError::Invalid {
            path: path.to_path_buf(),
            message,
        };
        for dir in &self.parent_dirs {
            if !dir.is_absolute() {
                return Err(invalid(format!(
                    "parent_dirs entry {:?} must be an absolute path",
                    dir.display()
                )));
            }
        }
        if self.collapse_size_bytes == 0 {
            return Err(invalid("collapse_size_bytes must be > 0".to_string()));
        }
        if self.search_depth < 1 || self.search_depth > MAX_SEARCH_DEPTH {
            return Err(invalid(format!(
                "search_depth must be between 1 and {MAX_SEARCH_DEPTH}"
            )));
        }
        if self.draft_dir_parents > MAX_DRAFT_DIR_PARENTS {
            return Err(invalid(format!(
                "draft_dir_parents must be between 0 and {MAX_DRAFT_DIR_PARENTS}"
            )));
        }
        for entry in &self.draft_dirs {
            if !is_absolute_or_relative_glob(entry) {
                return Err(invalid(format!(
                    "draft_dirs entry {entry:?} must be an absolute path or a relative glob \
                     (non-empty, no `..` components, no `~`)"
                )));
            }
            let pattern = draft_entry_pattern(entry);
            if pattern.is_empty() || pattern == "**" {
                return Err(invalid(format!(
                    "draft_dirs entry {entry:?} would match every folder under a parent dir: \
                     name the folder (`notes`, `notes/**`) or use a pattern with a fixed part \
                     (`*_drafts`, `**/notes`)"
                )));
            }
            if !Path::new(entry).is_absolute()
                && pattern.split('/').count() > MAX_SEARCH_DEPTH as usize
            {
                return Err(invalid(format!(
                    "draft_dirs entry {entry:?} looks more than {MAX_SEARCH_DEPTH} folders below \
                     a parent dir, which is as deep as the search goes"
                )));
            }
        }
        if let Some(session) = &self.herdr.session
            && (session.trim().is_empty() || session.contains('/'))
        {
            return Err(invalid(format!(
                "herdr.session {session:?} must be a non-empty session name without `/`"
            )));
        }
        Ok(())
    }
}

/// The pattern a `draft_dirs` entry picks folders with: the entry without its `/**`
/// suffix. The suffix says what the folder's record covers, never which folder it is, so
/// `notes/**` and `notes` pick the same folders.
pub fn draft_entry_pattern(entry: &str) -> &str {
    entry.strip_suffix("/**").unwrap_or(entry)
}

/// Whether a `draft_dirs` entry covers the whole tree below the folder it picks.
pub fn draft_entry_is_recursive(entry: &str) -> bool {
    entry.ends_with("/**")
}

/// How many folder levels below a parent dir the search reads for this entry: one per
/// component of its pattern, except that a `**` component reads to the walk's ceiling.
/// Keeping the walk this short is why naming a folder costs one `read_dir`, not a sweep of
/// everything below the parent dir.
pub fn draft_entry_walk_depth(entry: &str) -> usize {
    let pattern = draft_entry_pattern(entry);
    if pattern.split('/').any(|component| component == "**") {
        return MAX_SEARCH_DEPTH as usize;
    }
    pattern
        .split('/')
        .count()
        .clamp(1, MAX_SEARCH_DEPTH as usize)
}

/// A `draft_dirs` entry is either absolute or a relative glob: non-empty, not `~`-prefixed,
/// and without `..` components (which would escape the parent dir).
fn is_absolute_or_relative_glob(entry: &str) -> bool {
    if entry.is_empty() {
        return false;
    }
    if Path::new(entry).is_absolute() {
        return true;
    }
    if entry.starts_with('~') {
        return false;
    }
    !entry.split('/').any(|component| component == "..")
}

impl Loaded {
    /// G0 Q4: if `parent_dirs` is empty, watch the launch cwd; if the launch cwd is outside
    /// every configured parent dir, add it for this run with a notice. Never an error.
    pub fn resolve(&self, launch_cwd: &Path) -> Resolved {
        let mut notices = Vec::new();
        let cwd = normalize(launch_cwd);
        if self.config.parent_dirs.is_empty() {
            return Resolved {
                parent_dirs: vec![cwd],
                notices,
            };
        }
        let mut parent_dirs: Vec<PathBuf> = self
            .config
            .parent_dirs
            .iter()
            .map(|p| normalize(p))
            .collect();
        if !parent_dirs.iter().any(|dir| cwd.starts_with(dir)) {
            notices.push(format!(
                "watching {} ad hoc: not under any parent_dirs in {}",
                cwd.display(),
                self.source.label()
            ));
            parent_dirs.push(cwd);
        }
        Resolved {
            parent_dirs,
            notices,
        }
    }
}

/// Canonicalize (symlinks resolved, `/tmp` → `/private/tmp`). When the path does not exist,
/// canonicalize its nearest existing ancestor and re-append the remainder, so a configured
/// parent dir that is not created yet still compares correctly against a real launch cwd;
/// fictional paths with no existing ancestor are kept as given.
fn normalize(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    let mut remainder = Vec::new();
    let mut cursor = path;
    while let Some(parent) = cursor.parent() {
        if let Some(name) = cursor.file_name() {
            remainder.push(name.to_os_string());
        }
        if let Ok(canonical) = std::fs::canonicalize(parent) {
            let mut out = canonical;
            for name in remainder.iter().rev() {
                out.push(name);
            }
            return out;
        }
        cursor = parent;
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lastcall_testkit::tmp::TempDir;

    const EVERY_KEY: &str = r#"
parent_dirs = ["/srv/code", "/srv/other"]
draft_dirs = ["_drafts/**", "/abs/drafts"]
draft_initial = "pending"
collapsed_globs = ["*.lock"]
collapse_size_bytes = 1024
ignore_globs = [".git/**"]
hide_empty_repos = true
search_depth = 3

[ui]
wrap = false

[herdr]
mode = "on"
session = "work"

[update]
check = false

[keys]
quit = "q"
nav_down = ["down", "j", "ctrl-n"]
"#;

    fn env_with_config(dir: &TempDir, contents: &str) -> (Env, PathBuf) {
        let path = dir.write("config.toml", contents);
        let env = Env::empty(dir.path())
            .with_home(dir.mkdir("home"))
            .with_var("LASTCALL_CONFIG", path.to_string_lossy().to_string());
        (env, path)
    }

    #[test]
    fn config_parses_every_v1_key() {
        let dir = TempDir::new("lc-config");
        let (env, path) = env_with_config(&dir, EVERY_KEY);
        let loaded = load(&env).unwrap();
        assert_eq!(loaded.source, ConfigSource::File { path });
        let c = &loaded.config;
        assert_eq!(
            c.parent_dirs,
            vec![PathBuf::from("/srv/code"), PathBuf::from("/srv/other")]
        );
        assert_eq!(c.draft_dirs, vec!["_drafts/**", "/abs/drafts"]);
        assert_eq!(c.draft_initial, DraftInitial::Pending);
        assert_eq!(c.collapsed_globs, vec!["*.lock"]);
        assert_eq!(c.collapse_size_bytes, 1024);
        assert_eq!(c.ignore_globs, vec![".git/**"]);
        assert!(c.hide_empty_repos);
        assert_eq!(c.search_depth, 3);
        assert!(!c.ui.wrap);
        assert_eq!(c.herdr.mode, HerdrMode::On);
        assert_eq!(c.herdr.session.as_deref(), Some("work"));
        assert!(!c.update.check);
        assert_eq!(c.keys.len(), 2);
        assert_eq!(c.keys["quit"], KeySpecs::One("q".into()));
        assert_eq!(c.keys["quit"].specs(), vec!["q"]);
        assert_eq!(
            c.keys["nav_down"],
            KeySpecs::Many(vec!["down".into(), "j".into(), "ctrl-n".into()])
        );
        assert_eq!(c.keys["nav_down"].specs(), vec!["down", "j", "ctrl-n"]);
    }

    #[test]
    fn config_keys_table_is_opaque_and_string_or_list() {
        // Unknown action names are the binary's problem (§11): the engine keeps them.
        let c: Config = toml::from_str("[keys]\nfrobnicate = \"x\"\n").unwrap();
        assert_eq!(c.keys["frobnicate"].specs(), vec!["x"]);
        // Default: empty.
        assert!(Config::default().keys.is_empty());
        let c: Config = toml::from_str("").unwrap();
        assert!(c.keys.is_empty());
        // Neither a string nor a list of strings is a parse error naming the key.
        let dir = TempDir::new("lc-config");
        let (env, _) = env_with_config(&dir, "[keys]\nquit = 7\n");
        let err = load(&env).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "{err}");
        assert!(
            err.to_string()
                .contains("a [keys] entry must be a key spec (a string) or a list of them"),
            "the shape error names the rule, not serde's untagged variants: {err}"
        );
        let (env, _) = env_with_config(&dir, "[keys]\nquit = [\"q\", 3]\n");
        let err = load(&env).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "{err}");
        // A nested table under [keys] is not a spec either.
        let (env, _) = env_with_config(&dir, "[keys.quit]\nkey = \"q\"\n");
        assert!(load(&env).is_err());
        // Unknown top-level keys are still a load error alongside a valid [keys] table.
        let (env, _) = env_with_config(&dir, "[keys]\nquit = \"q\"\n\n[bogus]\nx = 1\n");
        let err = load(&env).unwrap_err();
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn config_round_trips_through_toml_and_json() {
        let c: Config = toml::from_str(EVERY_KEY).unwrap();
        let toml_text = toml::to_string(&c).unwrap();
        let again: Config = toml::from_str(&toml_text).unwrap();
        assert_eq!(c, again);
        let json = serde_json::to_string(&c).unwrap();
        let from_json: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(c, from_json);
    }

    #[test]
    fn config_rejects_unknown_key() {
        let dir = TempDir::new("lc-config");
        let (env, path) = env_with_config(&dir, "parent_dirs = []\nbogus = 1\n");
        let err = load(&env).unwrap_err();
        match err {
            ConfigError::Parse {
                path: p,
                line,
                message,
            } => {
                assert_eq!(p, path);
                assert_eq!(line, Some(2));
                assert!(message.contains("bogus"), "{message}");
            }
            other => panic!("expected Parse, got {other:?}"),
        }
    }

    #[test]
    fn config_rejects_unknown_key_in_herdr_table() {
        let dir = TempDir::new("lc-config");
        let (env, _) = env_with_config(&dir, "[herdr]\nmode = \"auto\"\nsocket = \"/x\"\n");
        let err = load(&env).unwrap_err();
        assert!(
            matches!(err, ConfigError::Parse { line: Some(3), .. }),
            "{err}"
        );
        assert!(err.to_string().contains("socket"), "{err}");
    }

    /// Phase 5 rulings 1 and 5: the two new keys are optional, and their defaults are the
    /// ones the rulings name — a `[herdr]` table that mentions neither still toasts and
    /// still starts scoped to the workspace.
    #[test]
    fn config_herdr_toast_and_scope_default_on_and_parse() {
        let dir = TempDir::new("lc-config");
        let (env, _) = env_with_config(&dir, "[herdr]\nmode = \"on\"\n");
        let herdr = load(&env).unwrap().config.herdr;
        assert!(herdr.toast, "toast defaults on (ruling 5)");
        assert_eq!(herdr.scope, HerdrScope::Workspace, "ruling 1");

        let (env, _) = env_with_config(&dir, "[herdr]\ntoast = false\nscope = \"all\"\n");
        let herdr = load(&env).unwrap().config.herdr;
        assert!(!herdr.toast);
        assert_eq!(herdr.scope, HerdrScope::All);

        let (env, _) = env_with_config(&dir, "[herdr]\nscope = \"workspaces\"\n");
        assert!(
            matches!(load(&env).unwrap_err(), ConfigError::Parse { .. }),
            "a near-miss spelling is an error, not a silent default"
        );
    }

    #[test]
    fn config_rejects_invalid_enum_value() {
        let dir = TempDir::new("lc-config");
        let (env, _) = env_with_config(&dir, "draft_initial = \"maybe\"\n");
        let err = load(&env).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "{err}");
        let (env, _) = env_with_config(&dir, "[herdr]\nmode = \"sometimes\"\n");
        let err = load(&env).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "{err}");
    }

    #[test]
    fn config_defaults_when_no_file() {
        let dir = TempDir::new("lc-config");
        let home = dir.mkdir("home");
        let env = Env::empty(dir.path()).with_home(&home);
        let loaded = load(&env).unwrap();
        assert_eq!(loaded.config, Config::default());
        assert_eq!(
            loaded.source,
            ConfigSource::Defaults {
                searched: vec![home.join(".config/lastcall/config.toml")]
            }
        );
        assert_eq!(loaded.state_dir, home.join(".local/state/lastcall"));
        let d = &loaded.config;
        assert_eq!(d.collapse_size_bytes, 512 * 1024);
        assert_eq!(d.draft_initial, DraftInitial::Seen);
        assert_eq!(d.herdr.mode, HerdrMode::Auto);
        assert_eq!(d.herdr.session, None);
        for lock in [
            "package-lock.json",
            "yarn.lock",
            "pnpm-lock.yaml",
            "Cargo.lock",
            "poetry.lock",
            "uv.lock",
            "Gemfile.lock",
            "go.sum",
            "composer.lock",
        ] {
            assert!(d.collapsed_globs.iter().any(|g| g == lock), "{lock}");
        }
        for noise in [
            ".git/**",
            "node_modules/**",
            "target/**",
            "vendor/**",
            ".venv/**",
        ] {
            assert!(d.ignore_globs.iter().any(|g| g == noise), "{noise}");
        }
    }

    #[test]
    fn config_defaults_when_no_home_and_state_dir_given() {
        let dir = TempDir::new("lc-config");
        let env = Env::empty(dir.path()).with_var("LASTCALL_STATE_DIR", "/var/lc-state");
        let loaded = load(&env).unwrap();
        assert_eq!(loaded.config, Config::default());
        assert_eq!(loaded.state_dir, PathBuf::from("/var/lc-state"));
        assert_eq!(
            loaded.source,
            ConfigSource::Defaults {
                searched: Vec::new()
            }
        );
    }

    #[test]
    fn config_no_home_and_no_state_dir_is_error() {
        let dir = TempDir::new("lc-config");
        let env = Env::empty(dir.path());
        assert!(matches!(load(&env).unwrap_err(), ConfigError::NoStateDir));
    }

    #[test]
    fn config_env_override_config_path() {
        let dir = TempDir::new("lc-config");
        let home = dir.mkdir("home");
        // A file at the XDG location must lose to LASTCALL_CONFIG.
        dir.write(
            "home/.config/lastcall/config.toml",
            "collapse_size_bytes = 1\n",
        );
        let explicit = dir.write("elsewhere.toml", "collapse_size_bytes = 2\n");
        let env = Env::empty(dir.path())
            .with_home(&home)
            .with_var("LASTCALL_CONFIG", explicit.to_string_lossy().to_string());
        let loaded = load(&env).unwrap();
        assert_eq!(loaded.config.collapse_size_bytes, 2);
        assert_eq!(loaded.source.path(), Some(explicit.as_path()));
    }

    #[test]
    fn config_xdg_config_home_precedes_home_dot_config() {
        let dir = TempDir::new("lc-config");
        let home = dir.mkdir("home");
        let xdg = dir.mkdir("xdg");
        dir.write(
            "home/.config/lastcall/config.toml",
            "collapse_size_bytes = 1\n",
        );
        let xdg_file = dir.write("xdg/lastcall/config.toml", "collapse_size_bytes = 3\n");
        let env = Env::empty(dir.path())
            .with_home(&home)
            .with_var("XDG_CONFIG_HOME", xdg.to_string_lossy().to_string());
        let loaded = load(&env).unwrap();
        assert_eq!(loaded.config.collapse_size_bytes, 3);
        assert_eq!(loaded.source.path(), Some(xdg_file.as_path()));

        let env = Env::empty(dir.path()).with_home(&home);
        let loaded = load(&env).unwrap();
        assert_eq!(loaded.config.collapse_size_bytes, 1);
    }

    #[test]
    fn config_env_override_state_dir() {
        let dir = TempDir::new("lc-config");
        let home = dir.mkdir("home");
        let env = Env::empty(dir.path()).with_home(&home);
        assert_eq!(state_dir(&env).unwrap(), home.join(".local/state/lastcall"));
        let env = env.with_var("XDG_STATE_HOME", "/xdg/state");
        assert_eq!(
            state_dir(&env).unwrap(),
            PathBuf::from("/xdg/state/lastcall")
        );
        let env = env.with_var("LASTCALL_STATE_DIR", "/explicit/state");
        assert_eq!(state_dir(&env).unwrap(), PathBuf::from("/explicit/state"));
        assert_eq!(
            load(&env).unwrap().state_dir,
            PathBuf::from("/explicit/state")
        );
    }

    /// Verifier (a) F3: a relative `$LASTCALL_STATE_DIR` names the same store it always
    /// did — `<cwd>/relstate` — but it now *says* so, because `status --json`'s
    /// `state_dir` exists to tell two runs apart. Joined, not canonicalised: the directory
    /// need not exist.
    #[test]
    fn config_relative_state_dir_is_absolutised_against_the_cwd() {
        let dir = TempDir::new("lc-config");
        let home = dir.mkdir("home");
        let env = Env::empty(dir.path())
            .with_home(&home)
            .with_var("LASTCALL_STATE_DIR", "relstate");
        let resolved = state_dir(&env).unwrap();
        assert_eq!(resolved, dir.path().join("relstate"));
        assert!(resolved.is_absolute(), "{resolved:?}");
        assert!(!resolved.exists(), "no directory is created or required");
        assert_eq!(load(&env).unwrap().state_dir, resolved);
    }

    #[test]
    fn config_missing_explicit_file_is_error() {
        let dir = TempDir::new("lc-config");
        let missing = dir.join("nope.toml");
        let env = Env::empty(dir.path())
            .with_home(dir.mkdir("home"))
            .with_var("LASTCALL_CONFIG", missing.to_string_lossy().to_string());
        let err = load(&env).unwrap_err();
        match err {
            ConfigError::ExplicitFileMissing { path } => assert_eq!(path, missing),
            other => panic!("expected ExplicitFileMissing, got {other:?}"),
        }
    }

    #[test]
    fn config_adhoc_cwd_outside_parent_dirs_adds_notice() {
        let dir = TempDir::new("lc-config");
        let parent = dir.mkdir("parent");
        let elsewhere = dir.mkdir("elsewhere");
        let (env, path) = env_with_config(
            &dir,
            &format!("parent_dirs = [{:?}]\n", parent.to_string_lossy()),
        );
        let loaded = load(&env).unwrap();

        let inside = loaded.resolve(&parent.join("repo"));
        assert_eq!(inside.parent_dirs, vec![normalize(&parent)]);
        assert!(inside.notices.is_empty());

        let outside = loaded.resolve(&elsewhere);
        assert_eq!(
            outside.parent_dirs,
            vec![normalize(&parent), normalize(&elsewhere)]
        );
        assert_eq!(outside.notices.len(), 1);
        assert_eq!(
            outside.notices[0],
            format!(
                "watching {} ad hoc: not under any parent_dirs in {}",
                normalize(&elsewhere).display(),
                path.display()
            )
        );
    }

    #[test]
    fn config_empty_parent_dirs_means_launch_cwd_without_notice() {
        let dir = TempDir::new("lc-config");
        let env = Env::empty(dir.path()).with_home(dir.mkdir("home"));
        let loaded = load(&env).unwrap();
        let resolved = loaded.resolve(dir.path());
        assert_eq!(resolved.parent_dirs, vec![normalize(dir.path())]);
        assert!(resolved.notices.is_empty());
    }

    #[test]
    fn config_validation_rejects_relative_parent_dir() {
        let dir = TempDir::new("lc-config");
        let (env, path) = env_with_config(&dir, "parent_dirs = [\"code\"]\n");
        let err = load(&env).unwrap_err();
        match err {
            ConfigError::Invalid { path: p, message } => {
                assert_eq!(p, path);
                assert!(message.contains("parent_dirs"), "{message}");
                assert!(message.contains("absolute"), "{message}");
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn config_validation_rejects_zero_collapse_size() {
        let dir = TempDir::new("lc-config");
        let (env, _) = env_with_config(&dir, "collapse_size_bytes = 0\n");
        let err = load(&env).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }), "{err}");
        assert!(err.to_string().contains("collapse_size_bytes"), "{err}");
    }

    #[test]
    fn config_validation_rejects_bad_draft_dir_entries() {
        let path = Path::new("/x/config.toml");
        for bad in ["", "~/drafts", "../escape/**", "a/../b"] {
            let c = Config {
                draft_dirs: vec![bad.to_string()],
                ..Config::default()
            };
            let err = c.validate(path).unwrap_err();
            assert!(err.to_string().contains("draft_dirs"), "{bad:?}: {err}");
        }
        for good in ["_drafts/**", "/abs/drafts", "notes/*.md"] {
            let c = Config {
                draft_dirs: vec![good.to_string()],
                ..Config::default()
            };
            c.validate(path).unwrap_or_else(|e| panic!("{good:?}: {e}"));
        }
    }

    /// Amendment v1.13 (§6.1): the search reads as many folder levels as the entry names,
    /// so an entry that names more levels than the ceiling, or names nothing at all, is a
    /// load error rather than a sweep of the whole parent dir.
    #[test]
    fn config_validation_rejects_draft_dir_entries_that_would_sweep_or_overshoot() {
        let path = Path::new("/x/config.toml");
        for bad in ["**", "**/**", "/**", "a/b/c/d/e", "a/b/c/d/e/**"] {
            let c = Config {
                draft_dirs: vec![bad.to_string()],
                ..Config::default()
            };
            let err = c.validate(path).unwrap_err();
            assert!(err.to_string().contains("draft_dirs"), "{bad:?}: {err}");
            assert!(err.to_string().contains(bad), "{bad:?}: {err}");
        }
        // Four components is the ceiling, a `**` component is allowed beside a fixed one,
        // and an absolute path is never walked so its own depth is its business.
        for good in [
            "a/b/c/d",
            "a/b/c/d/**",
            "**/notes",
            "*_drafts",
            "/a/b/c/d/e/f/notes",
            "/a/b/c/d/e/f/notes/**",
        ] {
            let c = Config {
                draft_dirs: vec![good.to_string()],
                ..Config::default()
            };
            c.validate(path).unwrap_or_else(|e| panic!("{good:?}: {e}"));
        }
    }

    /// The `/**` suffix says what a folder's record covers, never which folder it is.
    #[test]
    fn config_draft_entry_pattern_drops_the_recursive_suffix() {
        for (entry, pattern, recursive) in [
            ("notes", "notes", false),
            ("notes/**", "notes", true),
            ("a/*/notes", "a/*/notes", false),
            ("a/*/notes/**", "a/*/notes", true),
            ("/abs/notes/**", "/abs/notes", true),
        ] {
            assert_eq!(draft_entry_pattern(entry), pattern, "{entry:?}");
            assert_eq!(draft_entry_is_recursive(entry), recursive, "{entry:?}");
        }
    }

    /// One `read_dir` level per component; only a `**` component reads to the ceiling.
    #[test]
    fn config_draft_entry_walk_depth_is_the_component_count() {
        for (entry, depth) in [
            ("notes", 1),
            ("notes/**", 1),
            ("*_drafts", 1),
            ("a/b", 2),
            ("a/*/notes", 3),
            ("a/b/c/d", 4),
            ("**/notes", MAX_SEARCH_DEPTH as usize),
            ("a/**/notes", MAX_SEARCH_DEPTH as usize),
        ] {
            assert_eq!(draft_entry_walk_depth(entry), depth, "{entry:?}");
        }
    }

    /// Amendment v1.13 (§6.1): `draft_dir_parents` is a top-level integer, default `1`,
    /// bounded by the same ceiling as the search, and the key is named when it is wrong.
    #[test]
    fn config_draft_dir_parents_defaults_to_one_and_is_bounded() {
        assert_eq!(
            Config::default().draft_dir_parents,
            DEFAULT_DRAFT_DIR_PARENTS
        );
        assert_eq!(DEFAULT_DRAFT_DIR_PARENTS, 1);
        let path = Path::new("/x/config.toml");
        for ok in 0..=MAX_DRAFT_DIR_PARENTS {
            let c = Config {
                draft_dir_parents: ok,
                ..Config::default()
            };
            c.validate(path).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        for bad in [MAX_DRAFT_DIR_PARENTS + 1, 9, u8::MAX] {
            let c = Config {
                draft_dir_parents: bad,
                ..Config::default()
            };
            let err = c.validate(path).unwrap_err();
            assert!(
                err.to_string().contains("draft_dir_parents"),
                "{bad}: {err}"
            );
        }
        let c: Config = toml::from_str("draft_dir_parents = 3\n").unwrap();
        assert_eq!(c.draft_dir_parents, 3);
        let round = toml::to_string(&c).unwrap();
        assert_eq!(
            toml::from_str::<Config>(&round).unwrap().draft_dir_parents,
            3
        );
        assert!(toml::from_str::<Config>("draft_dir_parents = \"1\"\n").is_err());
    }

    #[test]
    fn config_validation_rejects_bad_session_name() {
        let path = Path::new("/x/config.toml");
        let c: Config = toml::from_str("[herdr]\nsession = \"a/b\"\n").unwrap();
        assert!(c.validate(path).is_err());
        let c: Config = toml::from_str("[herdr]\nsession = \"  \"\n").unwrap();
        assert!(c.validate(path).is_err());
    }

    /// Amendment v1.9 (§6.1): `hide_empty_repos` is a top-level bool, default `false`
    /// — a 1.0 config file that has never heard of it loads, `lastcall config` prints it,
    /// and it round-trips through TOML. A wrong type is a load error like every other key.
    #[test]
    fn config_hide_empty_repos_defaults_false_and_round_trips() {
        assert!(!Config::default().hide_empty_repos, "the sponsor's default");
        // A v1.0 file with none of this phase’s keys still loads.
        let old: Config = toml::from_str("parent_dirs = []\ncollapse_size_bytes = 4096\n").unwrap();
        assert!(!old.hide_empty_repos);

        let on: Config = toml::from_str("hide_empty_repos = true\n").unwrap();
        assert!(on.hide_empty_repos);
        let text = toml::to_string_pretty(&on).unwrap();
        assert!(text.contains("hide_empty_repos = true"), "{text}");
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), on);
        // `lastcall config` prints every key with its default, this one included.
        let printed = toml::to_string_pretty(&Config::default()).unwrap();
        assert!(printed.contains("hide_empty_repos = false"), "{printed}");

        let dir = TempDir::new("lc-config");
        let (env, _) = env_with_config(&dir, "hide_empty_repos = \"yes\"\n");
        let err = load(&env).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "{err}");
        assert!(err.to_string().contains("expected a boolean"), "{err}");
    }

    /// Deliverable 8 (§6.1, Amendment v1.11): `search_depth` is a top-level integer,
    /// default `1`, valid `1` to `4`. A config file from before this release loads with the
    /// default, `lastcall config` prints the key on its own (it serializes `Config` whole,
    /// so there is no code to add there, only this assertion), and `0` or `5` is the same
    /// shape of validation error `collapse_size_bytes` has. A value outside `u8` fails in
    /// the parser with serde's own wording, before `validate` ever runs.
    #[test]
    fn config_search_depth_defaults_to_one_and_is_range_checked() {
        assert_eq!(Config::default().search_depth, DEFAULT_SEARCH_DEPTH);
        assert_eq!(DEFAULT_SEARCH_DEPTH, 1, "one folder down, as it always was");
        // A v1.0 file that has never heard of the key still loads.
        let old: Config = toml::from_str("parent_dirs = []\ncollapse_size_bytes = 4096\n").unwrap();
        assert_eq!(old.search_depth, 1);

        let path = Path::new("/c/config.toml");
        for depth in 1..=MAX_SEARCH_DEPTH {
            let c: Config = toml::from_str(&format!("search_depth = {depth}\n")).unwrap();
            assert_eq!(c.search_depth, depth);
            c.validate(path).expect("in range");
        }
        for depth in [0, 5, 200] {
            let c: Config = toml::from_str(&format!("search_depth = {depth}\n")).unwrap();
            let err = c.validate(path).expect_err("out of range");
            assert!(
                err.to_string()
                    .contains("search_depth must be between 1 and 4"),
                "{err}"
            );
        }

        // Round-trips, and `lastcall config` prints it with every other key.
        let two: Config = toml::from_str("search_depth = 2\n").unwrap();
        let text = toml::to_string_pretty(&two).unwrap();
        assert!(text.contains("search_depth = 2"), "{text}");
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), two);
        let printed = toml::to_string_pretty(&Config::default()).unwrap();
        assert!(printed.contains("search_depth = 1"), "{printed}");

        // Out of `u8` is the parser's error, with its own wording, and a load error either
        // way; the range message is not promised for it.
        let dir = TempDir::new("lc-config");
        let (env, _) = env_with_config(&dir, "search_depth = 300\n");
        let err = load(&env).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "{err}");
        let (env, _) = env_with_config(&dir, "search_depth = 0\n");
        let err = load(&env).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }), "{err}");
    }

    /// Amendment v1.10 item 2 (§6.1): `[update] check` is a bool, default `true` — a
    /// config file that has never heard of the table loads, `lastcall config` prints it,
    /// it round-trips, and an unknown key under `[update]` is a load error like every
    /// other table.
    #[test]
    fn config_update_check_defaults_true_and_round_trips() {
        assert!(Config::default().update.check, "the daily check is opt-out");
        let old: Config = toml::from_str("parent_dirs = []\n").unwrap();
        assert!(old.update.check);

        let off: Config = toml::from_str("[update]\ncheck = false\n").unwrap();
        assert!(!off.update.check);
        let text = toml::to_string_pretty(&off).unwrap();
        assert!(text.contains("check = false"), "{text}");
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), off);
        let printed = toml::to_string_pretty(&Config::default()).unwrap();
        assert!(printed.contains("[update]"), "{printed}");
        assert!(printed.contains("check = true"), "{printed}");

        let dir = TempDir::new("lc-config");
        let (env, _) = env_with_config(&dir, "[update]\ncheck = \"no\"\n");
        let err = load(&env).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "{err}");

        let (env, _) = env_with_config(&dir, "[update]\nchek = true\n");
        let err = load(&env).unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
    }

    /// Amendment v1.14 (§6.1): `[ui] wrap` is a bool that defaults to **true**, so a
    /// config file written before this release wraps, and `lastcall config` prints the
    /// table. A wrong type and an unknown key in the table are load errors.
    #[test]
    fn config_ui_wrap_defaults_true_and_round_trips() {
        assert!(
            Config::default().ui.wrap,
            "a review tool wraps out of the box"
        );
        // A file from before the table loads, wrapping.
        let old: Config = toml::from_str("parent_dirs = []\n").unwrap();
        assert!(old.ui.wrap);
        // And so does an empty table.
        let empty: Config = toml::from_str("[ui]\n").unwrap();
        assert!(empty.ui.wrap);

        let off: Config = toml::from_str("[ui]\nwrap = false\n").unwrap();
        assert!(!off.ui.wrap);
        let text = toml::to_string_pretty(&off).unwrap();
        assert!(text.contains("[ui]"), "{text}");
        assert!(text.contains("wrap = false"), "{text}");
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), off);
        let printed = toml::to_string_pretty(&Config::default()).unwrap();
        assert!(printed.contains("[ui]"), "{printed}");
        assert!(printed.contains("wrap = true"), "{printed}");

        let dir = TempDir::new("lc-config");
        let (env, _) = env_with_config(&dir, "[ui]\nwrap = \"yes\"\n");
        let err = load(&env).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "{err}");
        assert!(err.to_string().contains("expected a boolean"), "{err}");

        let (env, _) = env_with_config(&dir, "[ui]\nwrapp = true\n");
        let err = load(&env).unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
    }

    #[test]
    fn config_parse_error_reports_line() {
        let dir = TempDir::new("lc-config");
        let (env, _) = env_with_config(&dir, "parent_dirs = []\n\ncollapse_size_bytes = \"x\"\n");
        let err = load(&env).unwrap_err();
        assert!(
            matches!(err, ConfigError::Parse { line: Some(3), .. }),
            "{err}"
        );
        assert!(err.to_string().contains("line 3"), "{err}");
    }
}
