//! One number formatter for every count a human reads: the row-cap notice, the TUI's
//! header and nav counts, and the status line all spell `10000` as `10,000` (the Phase 4
//! close-out ruling: thousands separators everywhere on screen). Nothing here reads a
//! file or the environment; the TUI reuses it so the two crates cannot drift.

/// `10000` → `10,000`; `999` → `999`; `0` → `0`.
pub fn with_thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn count_with_thousands_groups_digits_like_the_ruling() {
        for (n, want) in [
            (0, "0"),
            (3, "3"),
            (999, "999"),
            (1000, "1,000"),
            (10_000, "10,000"),
            (49_997, "49,997"),
            (1_234_567, "1,234,567"),
        ] {
            assert_eq!(super::with_thousands(n), want);
        }
    }
}
