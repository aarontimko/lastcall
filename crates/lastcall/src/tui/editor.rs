//! Resolving `$VISUAL` / `$EDITOR` into an argv that opens a file **at a line**
//! (Phase 8 deliverable 2; ruling P2 — no `editor` config key, §6.1 stays frozen).
//!
//! Pure: [`EditorCommand::resolve`] takes a lookup closure rather than reading the
//! environment, so every row of the table below is a unit test and nothing here needs a
//! process. The spawn itself lives in `run.rs` (deliverable 7).
//!
//! **No shell.** The variable's value is split on ASCII whitespace and the pieces are argv,
//! so `EDITOR='code --wait'` works and `EDITOR='foo "bar baz"'` gives the three words
//! `foo`, `"bar`, `baz"` — quotes and `$` are ordinary characters, never expanded. That is
//! deliberate: running the value through a shell would make a review tool spawn arbitrary
//! shell code from an inherited environment variable, and the wrapper-script case is served
//! by pointing the variable straight at the script.
//!
//! **Non-waiting editors return before their save.** `code` without `--wait`,
//! `emacsclient -n`, anything launched through `open`: the child exits immediately, the
//! file is unchanged when lastcall looks, the status says `no change`, and the user's later
//! save arrives as an ordinary pending row (design review F3). The table below adds
//! `--wait` where the editor is known to need it; an editor the table does not know is
//! opened with the file alone and no line flag.

use std::ffi::OsString;
use std::path::Path;

/// The fallback when neither `$VISUAL` nor `$EDITOR` is set. POSIX guarantees it.
pub const FALLBACK: &str = "vi";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorError {
    /// The chosen variable is set to whitespace only. There is no sane argv for that, and
    /// silently falling through to `vi` would hide a typo in the user's shell profile.
    Blank(String),
}

impl std::fmt::Display for EditorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditorError::Blank(name) => write!(
                f,
                "${name} is set to whitespace; unset it or give it an editor command"
            ),
        }
    }
}

impl std::error::Error for EditorError {}

/// How a program takes "open at this line".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineStyle {
    /// `+<line> <file>` — vi and friends.
    PlusLine,
    /// `<file>:<line>` — helix.
    Colon,
    /// `<file>:<line> --wait` — sublime, zed.
    ColonWait,
    /// `--goto <file>:<line> --wait` — VS Code.
    GotoWait,
    /// The table does not know this program: the file alone.
    Unknown,
}

/// The basename table. Everything not listed opens without a line flag.
const TABLE: &[(&str, LineStyle)] = &[
    ("vi", LineStyle::PlusLine),
    ("vim", LineStyle::PlusLine),
    ("nvim", LineStyle::PlusLine),
    ("view", LineStyle::PlusLine),
    ("nano", LineStyle::PlusLine),
    ("micro", LineStyle::PlusLine),
    ("emacs", LineStyle::PlusLine),
    ("emacsclient", LineStyle::PlusLine),
    ("kak", LineStyle::PlusLine),
    ("hx", LineStyle::Colon),
    ("code", LineStyle::GotoWait),
    ("codium", LineStyle::GotoWait),
    ("subl", LineStyle::ColonWait),
    ("zed", LineStyle::ColonWait),
];

/// A resolved editor command: the program, the arguments the user's own variable carried,
/// and which variable it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorCommand {
    /// argv[0] as written in the variable (a bare name or a path).
    pub program: String,
    /// The rest of the variable's words, in order, before our own arguments.
    pub args: Vec<String>,
    /// `VISUAL`, `EDITOR`, or `None` for the built-in `vi` fallback.
    pub source: Option<String>,
}

impl EditorCommand {
    /// `$VISUAL` if set and non-blank, else `$EDITOR`, else `vi`.
    ///
    /// A variable that is *set but blank* is an error rather than a fall-through: `VISUAL=`
    /// in a profile is a mistake worth naming, and guessing past it would open the wrong
    /// editor without saying so.
    pub fn resolve(var: &dyn Fn(&str) -> Option<String>) -> Result<EditorCommand, EditorError> {
        for name in ["VISUAL", "EDITOR"] {
            let Some(value) = var(name) else { continue };
            let mut words = value.split_ascii_whitespace().map(str::to_owned);
            let Some(program) = words.next() else {
                return Err(EditorError::Blank(name.to_owned()));
            };
            return Ok(EditorCommand {
                program,
                args: words.collect(),
                source: Some(name.to_owned()),
            });
        }
        Ok(EditorCommand {
            program: FALLBACK.to_owned(),
            args: Vec::new(),
            source: None,
        })
    }

    /// The program's basename, which is what the table is keyed by: `/usr/local/bin/nvim`
    /// and `nvim` take the same flags, and a probe script symlinked as `vim` must too.
    pub fn basename(&self) -> &str {
        Path::new(&self.program)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&self.program)
    }

    fn style(&self) -> LineStyle {
        let base = self.basename();
        TABLE
            .iter()
            .find(|(name, _)| *name == base)
            .map(|(_, style)| *style)
            .unwrap_or(LineStyle::Unknown)
    }

    /// Whether the table knows how to put this editor on a line. `false` is not a failure —
    /// the file still opens — but the status line says so.
    pub fn knows_line(&self) -> bool {
        self.style() != LineStyle::Unknown
    }

    /// The full argv **after** the program: the variable's own words, then the file and the
    /// line in the shape this editor wants.
    pub fn argv(&self, file: &Path, line: usize) -> Vec<OsString> {
        let mut out: Vec<OsString> = self.args.iter().map(OsString::from).collect();
        let mut with_line = OsString::from(file);
        with_line.push(format!(":{line}"));
        match self.style() {
            LineStyle::PlusLine => {
                out.push(OsString::from(format!("+{line}")));
                out.push(file.into());
            }
            LineStyle::Colon => out.push(with_line),
            LineStyle::ColonWait => {
                out.push(with_line);
                out.push(OsString::from("--wait"));
            }
            LineStyle::GotoWait => {
                out.push(OsString::from("--goto"));
                out.push(with_line);
                out.push(OsString::from("--wait"));
            }
            LineStyle::Unknown => out.push(file.into()),
        }
        out
    }

    /// What the status line says once the editor is open.
    pub fn opened_note(&self) -> String {
        if self.knows_line() {
            format!("opened in {}", self.basename())
        } else {
            format!("opened in {} (no line flag known)", self.basename())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        }
    }

    fn argv_of(value: Option<&str>, line: usize) -> Vec<String> {
        let cmd = match value {
            Some(v) => EditorCommand::resolve(&env(&[("EDITOR", v)])).unwrap(),
            None => EditorCommand::resolve(&env(&[])).unwrap(),
        };
        cmd.argv(Path::new("/w/src/parse.rs"), line)
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// Deliverable 11: the resolution order and every row of the basename table.
    #[test]
    fn editor_command_resolution_order_and_basename_table() {
        // VISUAL wins; EDITOR is the fallback; `vi` is the fallback's fallback.
        let both = EditorCommand::resolve(&env(&[("VISUAL", "nvim"), ("EDITOR", "nano")])).unwrap();
        assert_eq!(both.program, "nvim");
        assert_eq!(both.source.as_deref(), Some("VISUAL"));
        let only_editor = EditorCommand::resolve(&env(&[("EDITOR", "nano")])).unwrap();
        assert_eq!(only_editor.program, "nano");
        assert_eq!(only_editor.source.as_deref(), Some("EDITOR"));
        let none = EditorCommand::resolve(&env(&[])).unwrap();
        assert_eq!(none.program, FALLBACK);
        assert_eq!(none.source, None);
        assert_eq!(argv_of(None, 12), ["+12", "/w/src/parse.rs"]);

        // A blank variable is named, never guessed past. VISUAL blank does *not* fall
        // through to EDITOR: it is a typo in the profile and saying so is the whole point.
        assert_eq!(
            EditorCommand::resolve(&env(&[("VISUAL", "   \t ")])),
            Err(EditorError::Blank("VISUAL".to_owned()))
        );
        assert_eq!(
            EditorCommand::resolve(&env(&[("EDITOR", "")])),
            Err(EditorError::Blank("EDITOR".to_owned()))
        );

        // Every row of the table, keyed by basename and reached through a path too.
        for name in ["vi", "vim", "nvim", "view", "nano", "micro", "emacs", "kak"] {
            assert_eq!(
                argv_of(Some(name), 40),
                ["+40", "/w/src/parse.rs"],
                "{name}"
            );
        }
        assert_eq!(argv_of(Some("emacsclient"), 40), ["+40", "/w/src/parse.rs"]);
        assert_eq!(argv_of(Some("hx"), 40), ["/w/src/parse.rs:40"]);
        assert_eq!(
            argv_of(Some("code"), 40),
            ["--goto", "/w/src/parse.rs:40", "--wait"]
        );
        assert_eq!(
            argv_of(Some("codium"), 40),
            ["--goto", "/w/src/parse.rs:40", "--wait"]
        );
        assert_eq!(argv_of(Some("subl"), 40), ["/w/src/parse.rs:40", "--wait"]);
        assert_eq!(argv_of(Some("zed"), 40), ["/w/src/parse.rs:40", "--wait"]);
        assert_eq!(
            argv_of(Some("/opt/homebrew/bin/nvim"), 7),
            ["+7", "/w/src/parse.rs"],
            "the table is keyed by basename, not by the whole path"
        );

        // An editor the table does not know opens the file and nothing else, and says so.
        assert_eq!(argv_of(Some("acme"), 40), ["/w/src/parse.rs"]);
        let unknown = EditorCommand::resolve(&env(&[("EDITOR", "acme")])).unwrap();
        assert!(!unknown.knows_line());
        assert_eq!(unknown.opened_note(), "opened in acme (no line flag known)");
        let known = EditorCommand::resolve(&env(&[("EDITOR", "hx")])).unwrap();
        assert!(known.knows_line());
        assert_eq!(known.opened_note(), "opened in hx");
    }

    /// The variable's own words come first, verbatim, and are never shell-expanded.
    #[test]
    fn editor_command_splits_on_whitespace_and_never_shells_out() {
        assert_eq!(
            argv_of(Some("code --new-window"), 3),
            ["--new-window", "--goto", "/w/src/parse.rs:3", "--wait"],
            "the user's flags stay in front of ours"
        );
        assert_eq!(
            argv_of(Some("emacsclient -n"), 3),
            ["-n", "+3", "/w/src/parse.rs"],
            "a non-waiting editor is spawned as asked; the return path handles it"
        );
        assert_eq!(
            argv_of(Some("  nvim   -R  "), 9),
            ["-R", "+9", "/w/src/parse.rs"],
            "runs of whitespace are one separator"
        );
        let quoted = EditorCommand::resolve(&env(&[("EDITOR", "wrap \"a b\" $HOME")])).unwrap();
        assert_eq!(quoted.program, "wrap");
        assert_eq!(
            quoted.args,
            ["\"a", "b\"", "$HOME"],
            "no shell, no expansion"
        );
    }
}
