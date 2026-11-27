//! Decimal USD string <-> integer micro-USD, without any floating point.

use core_contracts::MicroUsd;

pub fn parse_usd(text: &str) -> Result<MicroUsd, String> {
    let t = text.trim().trim_start_matches('$');
    if t.is_empty() || t.starts_with('-') {
        return Err(format!("invalid USD amount `{text}` (must be non-negative)"));
    }
    let (int_part, frac_part) = t.split_once('.').unwrap_or((t, ""));
    if !int_part.chars().all(|c| c.is_ascii_digit()) || !frac_part.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("invalid USD amount `{text}`"));
    }
    if frac_part.len() > 6 {
        return Err(format!("`{text}` has more than 6 decimal places (micro-USD precision)"));
    }
    let int: i64 = if int_part.is_empty() { 0 } else { int_part.parse().map_err(|e| format!("{e}"))? };
    let frac: i64 = format!("{frac_part:0<6}").parse().map_err(|e| format!("{e}"))?;
    int.checked_mul(1_000_000)
        .and_then(|v| v.checked_add(frac))
        .ok_or_else(|| format!("`{text}` overflows"))
}

pub fn format_usd(micro: MicroUsd) -> String {
    let sign = if micro < 0 { "-" } else { "" };
    let m = micro.unsigned_abs();
    format!("{sign}${}.{:06}", m / 1_000_000, m % 1_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_formats_exactly() {
        assert_eq!(parse_usd("0.50").unwrap(), 500_000);
        assert_eq!(parse_usd("$1").unwrap(), 1_000_000);
        assert_eq!(parse_usd(".000001").unwrap(), 1);
        assert!(parse_usd("-1").is_err());
        assert!(parse_usd("0.0000001").is_err());
        assert!(parse_usd("abc").is_err());
        assert_eq!(format_usd(500_123), "$0.500123");
    }
}
