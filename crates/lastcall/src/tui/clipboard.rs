//! OSC 52: the one way a terminal program can put bytes on the *user's* clipboard when it
//! is not running on the user's machine (Phase 8 deliverable 9).
//!
//! `ESC ] 52 ; c ; <base64> BEL` asks the terminal emulator — the thing that actually owns
//! a clipboard — to take the payload. It works over ssh, inside tmux (`set-clipboard on`)
//! and inside herdr's own terminal, which is the whole point: lastcall reviews an agent's
//! work wherever the agent is, and `pbcopy` is not there.
//!
//! Support is uneven and the caps are undocumented, so this module is deliberately careful:
//! the payload is capped at [`CAP`] (the caller refuses above it rather than writing a
//! sequence the terminal will silently drop half of), and the encoder is hand-rolled and
//! pinned against RFC 4648's own vectors rather than pulled in as a dependency for eighty
//! lines of table lookup.
//!
//! What lastcall cannot do is *check*: OSC 52 is write-only, with no reply to read, so
//! nothing here can prove the bytes arrived. The scenes prove the sequence is written and
//! decodes to the right text; the terminal-side proof is the sponsor's.

use std::fmt;

use crossterm::Command;

/// Largest payload (raw, pre-encoding) that will be written: 32 KiB, ≈ 43 KiB encoded.
///
/// Comfortably under tmux's 100 KiB `set-clipboard` buffer and the smaller caps other
/// terminals are reported to have. The exact limits are per-terminal and unverified — this
/// is a number chosen to be smaller than every one that has been named, not a measured
/// ceiling, and a selection over it is refused with a message that says so.
pub const CAP: usize = 32 * 1024;

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding (RFC 4648 §4).
pub fn base64(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = |i: usize| chunk.get(i).copied().unwrap_or(0) as u32;
        let n = (b(0) << 16) | (b(1) << 8) | b(2);
        let take = |shift: u32| ALPHABET[((n >> shift) & 0x3f) as usize] as char;
        out.push(take(18));
        out.push(take(12));
        out.push(if chunk.len() > 1 { take(6) } else { '=' });
        out.push(if chunk.len() > 2 { take(0) } else { '=' });
    }
    out
}

/// The OSC 52 write for `payload`, as a crossterm [`Command`] so it goes out through the
/// same `execute!` path as every other escape sequence the loop writes.
///
/// `c` is the clipboard selection (the system clipboard); `BEL` terminates, which is the
/// form the widest set of terminals accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Osc52(pub Vec<u8>);

impl Command for Osc52 {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b]52;c;{}\x07", base64(&self.0))
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4648 §10's own test vectors, verbatim. The encoder is hand-rolled, so this is
    /// the only thing standing between a selection and a clipboard full of garbage.
    #[test]
    fn clipboard_base64_matches_rfc4648_vectors() {
        let vectors = [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ];
        for (raw, encoded) in vectors {
            assert_eq!(base64(raw.as_bytes()), encoded, "base64({raw:?})");
        }

        // Bytes that are not text, and every value of the last two bits, so the padding
        // arithmetic is exercised outside the ASCII the vectors above use.
        assert_eq!(base64(&[0x00]), "AA==");
        assert_eq!(base64(&[0xff]), "/w==");
        assert_eq!(base64(&[0x00, 0x00, 0x00]), "AAAA");
        assert_eq!(base64(&[0xff, 0xff, 0xff]), "////");
        assert_eq!(base64(&[0xfb, 0xff, 0xbf]), "+/+/", "62 and 63 are + and /");
        assert_eq!(
            base64(&(0u8..=255).collect::<Vec<u8>>()).len(),
            344,
            "256 bytes is 344 encoded characters, padding included"
        );
    }

    /// The sequence around the payload: the `c` selection and the `BEL` terminator, with
    /// nothing between them but the base64.
    #[test]
    fn clipboard_osc52_wraps_the_base64_in_the_escape() {
        let mut out = String::new();
        Osc52(b"hello\n".to_vec())
            .write_ansi(&mut out)
            .expect("writes");
        assert_eq!(out, "\x1b]52;c;aGVsbG8K\x07");
        assert_eq!(
            String::from_utf8(
                out.trim_start_matches("\x1b]52;c;")
                    .trim_end_matches('\x07')
                    .as_bytes()
                    .to_vec()
            )
            .expect("ascii"),
            base64(b"hello\n"),
        );
    }
}
