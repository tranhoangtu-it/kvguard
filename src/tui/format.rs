//! Compact number formatting shared by the TUI and text summaries.

/// `999`, `12.3K`, `1.42M`.
pub fn fmt_tokens(tokens: u64) -> String {
    match tokens {
        0..=999 => tokens.to_string(),
        1_000..=999_999 => format!("{:.1}K", tokens as f64 / 1_000.0),
        _ => format!("{:.2}M", tokens as f64 / 1_000_000.0),
    }
}

/// `42.1s`, `18m 42s`, `1h 05m`.
pub fn fmt_duration(seconds: f64) -> String {
    let seconds = seconds.max(0.0);
    if seconds < 60.0 {
        format!("{seconds:.1}s")
    } else if seconds < 3_600.0 {
        let total = seconds.round() as u64;
        format!("{}m {:02}s", total / 60, total % 60)
    } else {
        let total = seconds.round() as u64;
        format!("{}h {:02}m", total / 3_600, (total % 3_600) / 60)
    }
}

/// Thousands separators: `41950` → `41,950`.
pub fn fmt_count(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_values() {
        assert_eq!(fmt_tokens(950), "950");
        assert_eq!(fmt_tokens(1_420_000), "1.42M");
        assert_eq!(fmt_duration(1_122.0), "18m 42s");
        assert_eq!(fmt_duration(42.14), "42.1s");
        assert_eq!(fmt_count(41_950), "41,950");
        assert_eq!(fmt_count(418), "418");
    }
}
