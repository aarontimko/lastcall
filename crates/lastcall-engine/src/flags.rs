//! The paste-ready flag export (kickoff deliverable 4, gate item 3).
//!
//! A flag is only half the feature: what the human wants is to hand the agent the lines
//! they are objecting to, in a form the agent can read without asking a follow-up question.
//! [`export`] renders one [`Flag`] as that message — deterministic, byte-frozen by the
//! golden `crates/lastcall/tests/golden/flag_export.md`.
//!
//! ```text
//! lastcall flag · <root basename> · <root-relative path> · hunk 2 of 3 · 2026-09-05T18:04:00Z
//! note: why is this unwrap safe?
//!
//! ```diff
//! @@ -10,7 +10,8 @@
//!  context
//! -old
//! +new
//! ```
//! ```
//!
//! A file flag omits the `hunk n of m` segment and the diff block. A batch is the exports
//! joined by a blank line ([`export_all`]).
//!
//! **Control bytes render in caret form** (F13). The export is pasted into a live terminal
//! inside bracketed-paste markers, and the *application* — not the line discipline —
//! decides where the paste ends: one `\x1b[201~` inside the payload would close it early
//! and let everything after it arrive as raw keystrokes. So every byte below `0x20` other
//! than `\n` and `\t` becomes `^X` (`^[` for ESC, `\x1b`). Non-UTF-8 bytes render lossy,
//! the same rule `Refused` uses for paths.

use crate::ledger::Flag;

/// Everything an export needs that the flag itself does not carry.
///
/// `of` is the file's hunk count as rendered when the flag was raised — [`crate::ledger::FlagHunk`]
/// stores the hunk, not the shape of the diff it came from, so the count travels here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExportContext {
    /// The root's basename, as it appears in the header line.
    pub root: String,
    /// Total hunks in the file when the flag was raised. `None` prints `hunk 2` alone.
    pub of: Option<usize>,
    /// `last touched by <agent> · session <id>`, printed on its own line when known.
    /// **Phase 7 always passes `None`** — Phase 8 provides the attribution.
    pub attribution: Option<String>,
}

/// Byte `0x00..0x20` other than `\n` and `\t` → caret form; everything else verbatim.
///
/// The input is already lossy-decoded UTF-8, so the only control *characters* left are the
/// C0 set (a lossy decode never invents one) plus whatever the user typed into a note.
fn caret(s: &str) -> String {
    if !s
        .chars()
        .any(|c| (c as u32) < 0x20 && c != '\n' && c != '\t')
    {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if (c as u32) < 0x20 && c != '\n' && c != '\t' {
            out.push('^');
            out.push(char::from(b'@' + c as u8));
        } else {
            out.push(c);
        }
    }
    out
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Render one flag as the message the human pastes to the agent.
///
/// No trailing newline: [`export_all`] joins with a blank line, and the TUI's export file
/// adds its own separator.
pub fn export(ctx: &ExportContext, path: &[u8], flag: &Flag) -> String {
    let mut out = String::new();
    out.push_str("lastcall flag · ");
    out.push_str(&caret(&ctx.root));
    out.push_str(" · ");
    out.push_str(&caret(&lossy(path)));
    if let Some(h) = &flag.hunk {
        match ctx.of {
            Some(of) => out.push_str(&format!(" · hunk {} of {of}", h.index + 1)),
            None => out.push_str(&format!(" · hunk {}", h.index + 1)),
        }
    }
    out.push_str(" · ");
    out.push_str(&caret(&flag.created_at));
    out.push('\n');
    if let Some(a) = &ctx.attribution {
        out.push_str(&caret(a));
        out.push('\n');
    }
    // Every line of the note, unwrapped and unreordered — but still caret-escaped: F13's
    // hazard is byte-level and does not care which section of the message the byte is in.
    out.push_str("note: ");
    out.push_str(&caret(&flag.note));
    if let Some(h) = &flag.hunk {
        out.push_str("\n\n```diff\n");
        out.push_str(&caret(&h.header));
        out.push('\n');
        let text = caret(&h.text);
        out.push_str(&text);
        if !text.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("```");
    }
    out
}

/// Every flag on one path, oldest first, joined by a blank line.
pub fn export_all(ctx: &ExportContext, path: &[u8], flags: &[Flag]) -> String {
    flags
        .iter()
        .map(|f| export(ctx, path, f))
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::FlagHunk;

    /// The golden lives beside the binary's other goldens; an engine unit test writes it so
    /// the clock is injectable (F12: the binary has no clock override).
    const GOLDEN: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../lastcall/tests/golden/flag_export.md"
    );

    fn ctx() -> ExportContext {
        ExportContext {
            root: "lastcall".into(),
            of: Some(3),
            attribution: None,
        }
    }

    fn hunk_flag() -> Flag {
        Flag {
            note: "why is this unwrap safe? the caller can pass an empty slice".into(),
            created_at: "2026-09-05T18:04:00Z".into(),
            hunk: Some(FlagHunk {
                index: 1,
                header: "@@ -10,7 +10,8 @@".into(),
                text: " let n = parse(s);\n-    n.unwrap()\n+    n.expect(\"parsed above\")\n \
                        }\n"
                .into(),
            }),
        }
    }

    /// The frozen shape. `just flag-export-golden` rewrites the file; anything else compares.
    #[test]
    fn flags_export_matches_the_golden() {
        let clock = crate::ledger::FixedClock::at_unix(1_788_631_440);
        let file = Flag::file("this whole file is generated — don't hand-edit", {
            use crate::ledger::Clock;
            clock.now_iso8601()
        });
        let actual = format!(
            "{}\n\n{}\n",
            export(&ctx(), b"crates/lastcall-engine/src/ops.rs", &hunk_flag()),
            export(&ctx(), b"crates/lastcall/src/tui/render.rs", &file),
        );
        if std::env::var_os("LASTCALL_UPDATE_GOLDEN").is_some() {
            std::fs::write(GOLDEN, &actual).expect("write golden");
            eprintln!("golden rewritten: {GOLDEN}");
            return;
        }
        let expected = std::fs::read_to_string(GOLDEN)
            .unwrap_or_else(|e| panic!("read {GOLDEN}: {e} (run `just flag-export-golden`)"));
        assert!(
            actual == expected,
            "the export differs from {GOLDEN} (run `just flag-export-golden` if intended)\n\
             --- expected ---\n{expected}\n--- actual ---\n{actual}"
        );
    }

    /// F13: a paste-end marker anywhere in the payload would close the bracketed paste and
    /// submit the rest as keystrokes. Every C0 byte but `\n` and `\t` leaves in caret form.
    #[test]
    fn flags_export_escapes_control_bytes() {
        let flag = Flag {
            note: "ends the paste\u{1b}[201~rm -rf /".into(),
            created_at: "2026-09-05T18:04:00Z".into(),
            hunk: Some(FlagHunk {
                index: 0,
                header: "@@ -1,1 +1,1 @@".into(),
                text: "-a\n+b\u{1b}[201~\u{7}\u{0}\n\tkept\n".into(),
            }),
        };
        let out = export(&ctx(), b"f1", &flag);
        assert!(!out.contains('\u{1b}'), "no raw ESC survives:\n{out}");
        assert!(!out.contains('\u{7}') && !out.contains('\u{0}'));
        assert!(out.contains("ends the paste^[[201~rm -rf /"));
        assert!(out.contains("+b^[[201~^G^@\n"));
        assert!(
            out.contains("\n\tkept\n"),
            "tab and newline are kept as they are"
        );
    }

    /// A file flag is the header and the note: no `hunk n of m`, no diff block.
    #[test]
    fn flags_export_of_a_file_flag_has_no_hunk_segment_or_diff() {
        let out = export(&ctx(), b"f1", &Flag::file("look", "2026-09-05T18:04:00Z"));
        assert_eq!(
            out,
            "lastcall flag · lastcall · f1 · 2026-09-05T18:04:00Z\nnote: look"
        );
        assert!(!out.contains("```"));
    }

    /// Non-UTF-8 paths render lossy, the rule `Refused` uses; `of` absent prints the index
    /// alone; an attribution takes its own line; a batch joins with a blank line.
    #[test]
    fn flags_export_lossy_path_bare_index_attribution_and_batch() {
        let ctx = ExportContext {
            root: "r".into(),
            of: None,
            attribution: Some("last touched by claude · session abc".into()),
        };
        let mut f = hunk_flag();
        f.note = "n".into();
        let out = export(&ctx, b"bad\xffname", &f);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[0],
            "lastcall flag · r · bad\u{fffd}name · hunk 2 · 2026-09-05T18:04:00Z"
        );
        assert_eq!(lines[1], "last touched by claude · session abc");
        assert_eq!(lines[2], "note: n");
        let plain = ExportContext {
            root: "r".into(),
            of: None,
            attribution: None,
        };
        let batch = export_all(
            &plain,
            b"f1",
            &[Flag::file("one", "t1"), Flag::file("two", "t2")],
        );
        assert_eq!(
            batch,
            "lastcall flag · r · f1 · t1\nnote: one\n\n\
             lastcall flag · r · f1 · t2\nnote: two"
        );
    }

    /// A hunk text without a trailing newline still closes its fence on its own line.
    #[test]
    fn flags_export_closes_the_fence_when_the_hunk_text_is_unterminated() {
        let flag = Flag {
            note: "n".into(),
            created_at: "t".into(),
            hunk: Some(FlagHunk {
                index: 0,
                header: "@@ -1,1 +1,1 @@".into(),
                text: "-a\n+b".into(),
            }),
        };
        assert!(export(&ctx(), b"f1", &flag).ends_with("-a\n+b\n```"));
    }
}
