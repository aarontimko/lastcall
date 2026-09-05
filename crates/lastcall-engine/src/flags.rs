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
//! **Control bytes render in caret form** (F13, F9). The export is pasted into a live
//! terminal inside bracketed-paste markers, and the *application* — not the line discipline
//! — decides where the paste ends: one `\x1b[201~` inside the payload would close it early
//! and let everything after it arrive as raw keystrokes. So C0 below `0x20` other than `\n`
//! and `\t` becomes `^X` (`^[` for ESC), DEL becomes `^?`, and the C1 range
//! `U+0080..=U+009F` becomes the caret form of its ESC equivalent (`^[[` for CSI) — a
//! terminal in UTF-8 mode reads a raw `\u{9b}` as CSI, so C0 alone was not the whole hazard.
//! Every rendered field goes through it: root, path, timestamp, attribution, note, hunk
//! header and hunk text (decision 10). Non-UTF-8 bytes render lossy, the same rule `Refused`
//! uses for paths.
//!
//! **The fence is as long as it needs to be** (D1). A diff line that is exactly ```` ``` ````
//! would close a three-backtick block and spill the rest of the hunk into prose, so the
//! fence is one backtick longer than the longest run any line inside it starts with.

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

/// Whether `c` is a control character the export must not emit raw.
fn needs_caret(c: char) -> bool {
    c != '\n' && c != '\t' && matches!(c as u32, 0x00..=0x1f | 0x7f | 0x80..=0x9f)
}

/// Control characters → caret form; everything else verbatim.
///
/// Three ranges, one notation (verifier F9):
///
/// - **C0**, `0x00..=0x1f` other than `\n` and `\t` → `^@` … `^_`; ESC is `^[`.
/// - **DEL**, `0x7f` → `^?`. The classic caret form, `0x7f ^ 0x40`.
/// - **C1**, `U+0080..=U+009F` → `^[` and then the character `0x40` below it — that is, the
///   caret form of the two-byte ESC sequence the C1 control is defined to be equivalent to.
///   U+009B (CSI) is `^[[`, U+0085 (NEL) is `^[E`.
///
/// C1 is not decorative: a terminal in UTF-8 mode reads `\u{9b}` (`0xc2 0x9b` on the wire)
/// as CSI, so `\u{9b}201~` ends a bracketed paste exactly as `\u{1b}[201~` does. The C0-only
/// rule let it through. The input is already lossy-decoded UTF-8, so a lossy decode never
/// invents any of these — every one of them was in the flag.
fn caret(s: &str) -> String {
    if !s.chars().any(needs_caret) {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        let u = c as u32;
        if !needs_caret(c) {
            out.push(c);
        } else if u == 0x7f {
            out.push_str("^?");
        } else if u >= 0x80 {
            out.push_str("^[");
            out.push(char::from((u - 0x40) as u8));
        } else {
            out.push('^');
            out.push(char::from(b'@' + u as u8));
        }
    }
    out
}

/// The fence for a code block holding `body`: three backticks, or one more than the longest
/// run of backticks that *starts* a line, whichever is longer.
///
/// A line that is exactly ```` ``` ```` closes a three-backtick block, and then the rest of
/// the diff renders as prose in the agent's client — the flag stops being one message and
/// the lines the human is objecting to are no longer marked as the lines they are objecting
/// to (D1). Widening the fence is CommonMark's own answer: a fence is closed only by a run
/// at least as long as the one that opened it.
fn fence_for(body: &str) -> String {
    let longest = body
        .lines()
        .map(|l| l.chars().take_while(|c| *c == '`').count())
        .max()
        .unwrap_or(0);
    "`".repeat((longest + 1).max(3))
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
        let header = caret(&h.header);
        let mut text = caret(&h.text);
        if !text.ends_with('\n') {
            text.push('\n');
        }
        // The fence is chosen from what is going inside it, so no line of the diff can end
        // the block early (D1).
        let fence = fence_for(&format!("{header}\n{text}"));
        out.push_str("\n\n");
        out.push_str(&fence);
        out.push_str("diff\n");
        out.push_str(&header);
        out.push('\n');
        out.push_str(&text);
        out.push_str(&fence);
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

    /// The escape and fence contract, pinned in the golden so a change to either shows as a
    /// golden diff (D1/F9). The inputs are deliberately hostile: every control range the
    /// renderer knows about, and a hunk line that is exactly a three-backtick fence.
    fn hazard_flag() -> Flag {
        Flag {
            note: "paste guard · ESC \u{1b} · DEL \u{7f} · CSI \u{9b} · BEL \u{7}".into(),
            created_at: "2026-09-05T18:04:00Z".into(),
            hunk: Some(FlagHunk {
                index: 2,
                header: "@@ -40,3 +40,3 @@ fn render()".into(),
                text: "-println!(\"x\");\n```\n+println!(\"y\");\n".into(),
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
            "{}\n\n{}\n\n{}\n",
            export(&ctx(), b"crates/lastcall-engine/src/ops.rs", &hunk_flag()),
            export(&ctx(), b"crates/lastcall/src/tui/render.rs", &file),
            export(&ctx(), b"crates/lastcall/src/tui/keys.rs", &hazard_flag()),
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

    /// F9: the C0-only rule left two escapes open.
    ///
    /// `\u{9b}` is CSI — a terminal in UTF-8 mode ends a bracketed paste on `\u{9b}201~`
    /// just as it does on `\u{1b}[201~` — and DEL is a control byte the export has no
    /// business emitting raw either. Both now leave in the same caret notation as C0, and
    /// the C1 form is the caret spelling of the ESC sequence it is equivalent to.
    #[test]
    fn flags_export_escapes_del_and_the_c1_range() {
        let flag = Flag {
            note: "csi \u{9b}201~rm -rf / and del \u{7f} and nel \u{85}".into(),
            created_at: "t".into(),
            hunk: Some(FlagHunk {
                index: 0,
                header: "@@ -1,1 +1,1 @@".into(),
                text: "-a\n+b\u{9b}201~\u{7f}\n".into(),
            }),
        };
        let out = export(&ctx(), b"f1", &flag);
        for c in ['\u{9b}', '\u{7f}', '\u{85}', '\u{1b}'] {
            assert!(!out.contains(c), "{c:?} survived raw:\n{out}");
        }
        assert!(out.contains("csi ^[[201~rm -rf / and del ^? and nel ^[E"));
        assert!(out.contains("+b^[[201~^?\n"));
        // The whole C1 block, and nothing above it.
        assert_eq!(caret("\u{80}\u{9f}"), "^[@^[_");
        assert_eq!(caret("\u{a0}é"), "\u{a0}é", "U+00A0 is not a control");
    }

    /// D1: a hunk line that is exactly ``` must not close the block.
    #[test]
    fn flags_export_widens_the_fence_for_a_backtick_line() {
        let mut flag = hunk_flag();
        flag.hunk.as_mut().unwrap().text = "-a\n```\n+b\n".into();
        let out = export(&ctx(), b"f1", &flag);
        assert!(out.contains("\n````diff\n"), "opens with four:\n{out}");
        assert!(out.ends_with("+b\n````"), "and closes with four:\n{out}");
        assert!(!out.contains("`````"));
        // One longer than the longest run that starts a line, so an escalation still works.
        flag.hunk.as_mut().unwrap().text = "-a\n`````x\n+b\n".into();
        let out = export(&ctx(), b"f1", &flag);
        assert!(out.contains("\n``````diff\n"), "{out}");
        assert!(out.ends_with("+b\n``````"), "{out}");
        // A backtick run that does not start a line changes nothing.
        flag.hunk.as_mut().unwrap().text = "-a ``` b\n+b\n".into();
        assert!(export(&ctx(), b"f1", &flag).contains("\n```diff\n"));
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
