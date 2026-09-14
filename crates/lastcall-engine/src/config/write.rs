//! The first-launch tour's one sanctioned config write (Amendment v1.11).
//!
//! lastcall does not manage the user's configuration: the file is theirs, hand-written, and
//! every other part of the program only ever reads it. This module is the single exception.
//! The welcome overlay offers to remember one default, and choosing it sets exactly one key
//! in the file [`config_path`] resolves.
//!
//! Two properties make that safe enough to do at all:
//!
//! * **Format-preserving.** The file is edited as a `toml_edit` document, so comments,
//!   ordering, spacing and every key the tour is not setting survive byte for byte.
//! * **Atomic.** The new text is written to `<file>.tmp` beside the target and renamed over
//!   it, the ledger's own idiom, so a crash mid-write leaves the original file intact.
//!
//! It also answers the question the cards are gated on: *does the document already set this
//! key?* That cannot be asked of [`Config`](super::Config), which cannot tell a value the
//! user wrote from the default it would have had anyway (design review F9), so it is asked
//! of the parsed document instead. A file that does not exist sets nothing; a file that does
//! not parse is treated the same way for the question, and says so when the write is tried.

use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, Table, value};

use super::config_path;
use crate::env::Env;

/// The comment a **created** file opens with, followed by the date. The one trace the tour
/// leaves in a file nobody has written yet, so `lastcall config` is not the only way to find
/// out where a line came from.
pub const CREATED_BY: &str = "# written by lastcall's first-launch tour on ";

/// What the write reports when there is nowhere to put a file: no `$XDG_CONFIG_HOME` and no
/// home directory. The card shows it and keeps the setting for the session.
pub const NO_CONFIG_DIR: &str = "no configuration directory (set XDG_CONFIG_HOME or HOME)";

/// A default the tour can offer to remember. One key each, and the only keys this module
/// will ever write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    /// `hide_empty_repos = true` at the top level (§6.1).
    HideEmptyRepos,
    /// `scope = "all"` under `[herdr]` (§5.9).
    HerdrScopeAll,
}

impl Setting {
    /// The TOML this setting adds, line by line. A failed write shows these verbatim, so
    /// they are what a user would type in themselves, table header and all.
    pub fn lines(self) -> &'static [&'static str] {
        match self {
            Setting::HideEmptyRepos => &["hide_empty_repos = true"],
            Setting::HerdrScopeAll => &["[herdr]", "scope = \"all\""],
        }
    }
}

/// The config file as the tour sees it: where a write would land, whether a file is there
/// already, and the parsed document (or why it could not be parsed).
///
/// Opened once, when the overlay opens, and then asked [`Document::sets`] per card and
/// [`Document::write`] at most once per card.
#[derive(Debug)]
pub struct Document {
    /// Where a write lands. `None` when there is no config directory to create one in.
    path: Option<PathBuf>,
    /// Whether `path` is a file already. `false` means the write creates it, comment and
    /// all; it flips to `true` once it has.
    existed: bool,
    /// The document to edit, or the one-line reason there is none.
    doc: Result<DocumentMut, String>,
}

impl Document {
    /// Read and parse the config file this environment resolves, if there is one.
    ///
    /// Never an error: a missing file is an empty document over the path a write would
    /// create, and an unreadable or unparsable one is a document that sets nothing and
    /// refuses to be written, carrying the reason for the card to show.
    pub fn open(env: &Env) -> Document {
        match config_path(env) {
            Ok((Some(path), _)) => {
                let doc = match std::fs::read_to_string(&path) {
                    Ok(text) => text.parse::<DocumentMut>().map_err(|e| one_line(&e)),
                    Err(e) => Err(e.to_string()),
                };
                Document {
                    path: Some(path),
                    existed: true,
                    doc,
                }
            }
            Ok((None, _)) => Document {
                path: target(env),
                existed: false,
                doc: Ok(DocumentMut::new()),
            },
            // `LASTCALL_CONFIG` naming a file that is not there. The TUI never gets this far
            // — `config::load` fails first and the terminal is never taken — but the answer
            // is still the honest one: nothing to write to, and a reason to show.
            Err(e) => Document {
                path: None,
                existed: false,
                doc: Err(e.to_string()),
            },
        }
    }

    /// An empty document over `path`, for tests and for the create path.
    #[cfg(test)]
    fn empty_at(path: PathBuf) -> Document {
        Document {
            path: Some(path),
            existed: false,
            doc: Ok(DocumentMut::new()),
        }
    }

    /// Where a write lands, for the message a failed one shows.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Whether the file existed when this document was opened.
    pub fn existed(&self) -> bool {
        self.existed
    }

    /// Whether the document **sets** this key, with either value.
    ///
    /// The cards are gated on this: a user who has already said what they want is not asked
    /// again, and a user who has not is. A document that could not be parsed sets nothing —
    /// the cards are offered, and the write path is where the failure surfaces.
    pub fn sets(&self, setting: Setting) -> bool {
        let Ok(doc) = &self.doc else {
            return false;
        };
        match setting {
            Setting::HideEmptyRepos => doc.get("hide_empty_repos").is_some(),
            Setting::HerdrScopeAll => doc
                .get("herdr")
                .and_then(Item::as_table_like)
                .and_then(|t| t.get("scope"))
                .is_some(),
        }
    }

    /// Set this one key in the config file. `today` is the date the created file's comment
    /// carries, `YYYY-MM-DD` from the engine's injected clock — this module never reads a
    /// wall clock of its own.
    ///
    /// `Err` is a one-line reason, for the card's footer. The caller applies the setting to
    /// the session either way: the write is the *remembering*, not the doing.
    pub fn write(&mut self, setting: Setting, today: &str) -> Result<(), String> {
        let Some(path) = self.path.clone() else {
            return Err(NO_CONFIG_DIR.to_owned());
        };
        let contents = if self.existed {
            let doc = self.doc.as_mut().map_err(|e| e.clone())?;
            apply(doc, setting)?;
            doc.to_string()
        } else {
            // The create branch is written as text rather than through `toml_edit`, because
            // a document that is only a comment has nowhere to put the comment: a leading
            // comment belongs to the first key's decor, and there is no first key yet.
            let mut text = format!("{CREATED_BY}{today}\n");
            for line in setting.lines() {
                text.push_str(line);
                text.push('\n');
            }
            text
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        // `write_stamp`'s idiom: temp beside the target, then rename.
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, contents.as_bytes()).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
        if !self.existed {
            // The file exists now, so a second card edits it instead of creating it again.
            self.existed = true;
            self.doc = contents.parse::<DocumentMut>().map_err(|e| one_line(&e));
        }
        Ok(())
    }
}

/// Where a file would be created when none exists: `$XDG_CONFIG_HOME/lastcall/config.toml`,
/// else `~/.config/lastcall/config.toml` — the same two candidates [`config_path`] searches,
/// in the same order, because `Env::xdg_config_home` is that rule.
fn target(env: &Env) -> Option<PathBuf> {
    env.xdg_config_home()
        .map(|dir| dir.join("lastcall").join("config.toml"))
}

/// Set one key in an existing document, in place.
fn apply(doc: &mut DocumentMut, setting: Setting) -> Result<(), String> {
    match setting {
        Setting::HideEmptyRepos => {
            doc["hide_empty_repos"] = value(true);
        }
        Setting::HerdrScopeAll => {
            let item = doc
                .entry("herdr")
                .or_insert_with(|| Item::Table(Table::new()));
            let table = item
                .as_table_like_mut()
                .ok_or_else(|| "herdr is set to something that is not a table".to_owned())?;
            table.insert("scope", value("all"));
        }
    }
    Ok(())
}

/// A `toml_edit` parse error as one line: its message without the source snippet under it.
fn one_line(error: &toml_edit::TomlError) -> String {
    let text = error.to_string();
    text.lines().next().unwrap_or("invalid TOML").to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lastcall_testkit::tmp::TempDir;

    fn tmp() -> TempDir {
        TempDir::new("lc-config-write")
    }

    /// The question the cards are gated on is asked of the **file**, not of `Config`: a user
    /// who wrote `hide_empty_repos = false` by hand has said what they want as loudly as one
    /// who wrote `true`, and `Config` cannot tell either of them from the default.
    #[test]
    fn config_write_sets_is_about_the_document_not_the_value() {
        let dir = tmp();
        let path = dir.path().join("config.toml");
        for (text, hide, scope) in [
            ("", false, false),
            ("hide_empty_repos = false\n", true, false),
            ("hide_empty_repos = true\n", true, false),
            ("[herdr]\nscope = \"workspace\"\n", false, true),
            ("[herdr]\nscope = \"all\"\n", false, true),
            ("[herdr]\nenabled = true\n", false, false),
            ("herdr = { scope = \"all\" }\n", false, true),
        ] {
            std::fs::write(&path, text).expect("write");
            let env = Env::empty(dir.path()).with_var("LASTCALL_CONFIG", path.to_str().unwrap());
            let doc = Document::open(&env);
            assert_eq!(
                doc.sets(Setting::HideEmptyRepos),
                hide,
                "hide_empty_repos in {text:?}"
            );
            assert_eq!(
                doc.sets(Setting::HerdrScopeAll),
                scope,
                "[herdr] scope in {text:?}"
            );
        }
    }

    /// No file at all: nothing is set, and the write creates the file under
    /// `$XDG_CONFIG_HOME` with the provenance comment and the one line.
    #[test]
    fn config_write_creates_the_file_with_one_comment_and_one_key() {
        let dir = tmp();
        let env = Env::empty(dir.path())
            .with_var("XDG_CONFIG_HOME", dir.path().to_str().unwrap())
            .with_var(
                "LASTCALL_STATE_DIR",
                dir.path().join("state").to_str().unwrap(),
            );
        let mut doc = Document::open(&env);
        assert!(!doc.existed(), "no file yet");
        assert!(!doc.sets(Setting::HideEmptyRepos));
        assert!(!doc.sets(Setting::HerdrScopeAll));
        doc.write(Setting::HideEmptyRepos, "2026-09-14")
            .expect("create");
        let path = dir.path().join("lastcall").join("config.toml");
        let text = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(
            text,
            "# written by lastcall's first-launch tour on 2026-09-14\nhide_empty_repos = true\n",
            "the created file is these two lines and nothing else"
        );
        assert!(
            !dir.path().join("lastcall").join("config.toml.tmp").exists(),
            "the temp file is renamed, not left behind"
        );
        // A second card on the same run edits what the first one created, rather than
        // creating it again over the top.
        doc.write(Setting::HerdrScopeAll, "2026-09-14")
            .expect("edit");
        let text = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(
            text,
            "# written by lastcall's first-launch tour on 2026-09-14\nhide_empty_repos = true\n\n[herdr]\nscope = \"all\"\n"
        );
        // …and the file it left behind is a config file lastcall reads.
        let loaded = super::super::load(&env).expect("load");
        assert!(loaded.config.hide_empty_repos);
        assert_eq!(loaded.config.herdr.scope, crate::config::HerdrScope::All);
    }

    /// With no `$XDG_CONFIG_HOME`, the created file goes under the home directory, the
    /// second candidate `config_path` searches.
    #[test]
    fn config_write_creates_under_the_home_directory_when_xdg_is_unset() {
        let dir = tmp();
        let env = Env::empty(dir.path()).with_home(dir.path());
        let mut doc = Document::open(&env);
        doc.write(Setting::HerdrScopeAll, "2026-01-02")
            .expect("create");
        let path = dir
            .path()
            .join(".config")
            .join("lastcall")
            .join("config.toml");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read back"),
            "# written by lastcall's first-launch tour on 2026-01-02\n[herdr]\nscope = \"all\"\n"
        );
    }

    /// Nowhere to put a file: the write refuses in one line and the card says so.
    #[test]
    fn config_write_without_a_config_directory_says_so() {
        let dir = tmp();
        let env = Env::empty(dir.path());
        let mut doc = Document::open(&env);
        assert_eq!(doc.path(), None);
        assert_eq!(
            doc.write(Setting::HideEmptyRepos, "2026-09-14")
                .unwrap_err(),
            NO_CONFIG_DIR
        );
    }

    /// The whole promise of `toml_edit` here: a hand-written file comes back with every
    /// comment, blank line, table and value where it was, one line longer.
    #[test]
    fn config_write_preserves_every_other_byte_of_a_hand_written_file() {
        let dir = tmp();
        let path = dir.path().join("config.toml");
        let before = concat!(
            "# my lastcall config\n",
            "parent_dirs = [\"~/dev/git\", \"~/work\"]   # two trees\n",
            "\n",
            "collapse_size_bytes = 1024\n",
            "\n",
            "[keys]\n",
            "quit = \"x\"\n",
            "\n",
            "[update]\n",
            "check = false\n",
            "\n",
            "# the end\n",
        );
        std::fs::write(&path, before).expect("write");
        let env = Env::empty(dir.path()).with_var("LASTCALL_CONFIG", path.to_str().unwrap());
        let mut doc = Document::open(&env);
        doc.write(Setting::HideEmptyRepos, "2026-09-14")
            .expect("write");
        let after = std::fs::read_to_string(&path).expect("read back");

        let removed: Vec<&str> = before.lines().filter(|l| !after.contains(*l)).collect();
        assert!(removed.is_empty(), "nothing may be lost: {removed:?}");
        let added: Vec<&str> = after
            .lines()
            .filter(|l| !before.lines().any(|b| b == *l))
            .collect();
        assert_eq!(
            added,
            vec!["hide_empty_repos = true"],
            "exactly one line is added"
        );
        assert_eq!(
            after.lines().count(),
            before.lines().count() + 1,
            "and nothing is duplicated: {after}"
        );
        // The added key is a root key, so it lands above the first table header — anywhere
        // below it would belong to `[keys]` and mean something else entirely.
        let key = after.find("hide_empty_repos").expect("the key");
        assert!(key < after.find("[keys]").expect("the table"), "{after}");
    }

    /// An existing `[herdr]` table gains the one key rather than a second header.
    #[test]
    fn config_write_adds_scope_to_an_existing_herdr_table() {
        let dir = tmp();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[herdr]\n# follow the pane\nenabled = true\n").expect("write");
        let env = Env::empty(dir.path()).with_var("LASTCALL_CONFIG", path.to_str().unwrap());
        let mut doc = Document::open(&env);
        doc.write(Setting::HerdrScopeAll, "2026-09-14")
            .expect("write");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read back"),
            "[herdr]\n# follow the pane\nenabled = true\nscope = \"all\"\n"
        );
    }

    /// A file that does not parse: both cards are offered — the user has said nothing this
    /// module can read — and the write reports the reason in one line instead of throwing
    /// the file away.
    #[test]
    fn config_write_reports_a_file_that_does_not_parse_and_leaves_it_alone() {
        let dir = tmp();
        let path = dir.path().join("config.toml");
        let before = "parent_dirs = [\"~/dev\"\nhide_empty_repos = true\n";
        std::fs::write(&path, before).expect("write");
        let env = Env::empty(dir.path()).with_var("LASTCALL_CONFIG", path.to_str().unwrap());
        let mut doc = Document::open(&env);
        assert!(
            !doc.sets(Setting::HideEmptyRepos),
            "unparsable sets nothing"
        );
        let reason = doc
            .write(Setting::HideEmptyRepos, "2026-09-14")
            .expect_err("a file that does not parse cannot be edited");
        assert!(!reason.contains('\n'), "one line for the card: {reason:?}");
        assert!(!reason.is_empty());
        assert_eq!(
            std::fs::read_to_string(&path).expect("read back"),
            before,
            "the user's file is untouched"
        );
    }

    /// `herdr` set to something that is not a table is refused rather than overwritten.
    #[test]
    fn config_write_refuses_when_herdr_is_not_a_table() {
        let dir = tmp();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "herdr = 3\n").expect("write");
        let env = Env::empty(dir.path()).with_var("LASTCALL_CONFIG", path.to_str().unwrap());
        let mut doc = Document::open(&env);
        let reason = doc
            .write(Setting::HerdrScopeAll, "2026-09-14")
            .expect_err("not a table");
        assert!(reason.contains("not a table"), "{reason}");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read back"),
            "herdr = 3\n"
        );
    }

    /// The lines a failed write tells the user to add themselves are the TOML it would have
    /// written, table header included.
    #[test]
    fn config_write_lines_are_the_toml_the_user_would_type() {
        assert_eq!(
            Setting::HideEmptyRepos.lines(),
            &["hide_empty_repos = true"]
        );
        assert_eq!(
            Setting::HerdrScopeAll.lines(),
            &["[herdr]", "scope = \"all\""]
        );
        // Each set of lines is a config file lastcall parses on its own.
        let dir = tmp();
        for setting in [Setting::HideEmptyRepos, Setting::HerdrScopeAll] {
            let mut doc = Document::empty_at(dir.path().join("config.toml"));
            doc.write(setting, "2026-09-14").expect("write");
            let env = Env::empty(dir.path())
                .with_var(
                    "LASTCALL_CONFIG",
                    dir.path().join("config.toml").to_str().unwrap(),
                )
                .with_var(
                    "LASTCALL_STATE_DIR",
                    dir.path().join("state").to_str().unwrap(),
                );
            super::super::load(&env).expect("the file it wrote loads");
        }
    }
}
