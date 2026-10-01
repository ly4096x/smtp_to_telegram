//! Human-readable byte sizes, compatible with the `docker/go-units` helpers
//! the Go version used (`10m` = 10 000 000 bytes, `3B`, `2.048kB`).

/// Parses sizes such as `50m`, `10MB`, `1.5k`, `512`, `4MiB`.
///
/// Suffixes without `i` are decimal (k = 1000), suffixes with `i` are binary
/// (Ki = 1024). The suffix is case-insensitive and a trailing `b` is optional.
pub fn parse_size(input: &str) -> Result<u64, String> {
    let s = input.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (number, suffix) = s.split_at(split);
    if number.is_empty() {
        return Err(format!("invalid size {input:?}: expected a number"));
    }
    let value: f64 = number
        .parse()
        .map_err(|_| format!("invalid size {input:?}: bad number {number:?}"))?;
    let multiplier: f64 = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" => 1e3,
        "m" | "mb" => 1e6,
        "g" | "gb" => 1e9,
        "ki" | "kib" => 1024.0,
        "mi" | "mib" => 1024.0 * 1024.0,
        "gi" | "gib" => 1024.0 * 1024.0 * 1024.0,
        other => return Err(format!("invalid size {input:?}: unknown unit {other:?}")),
    };
    let bytes = value * multiplier;
    if !bytes.is_finite() || bytes < 0.0 || bytes > u64::MAX as f64 {
        return Err(format!("invalid size {input:?}: out of range"));
    }
    Ok(bytes as u64)
}

/// Formats a byte count the way go-units `HumanSize` does: decimal units and
/// at most four significant digits (`3B`, `2.048kB`, `12.35MB`).
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 7] = ["B", "kB", "MB", "GB", "TB", "PB", "EB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    format!("{}{}", four_significant_digits(value), UNITS[unit])
}

fn four_significant_digits(value: f64) -> String {
    if value == 0.0 {
        return "0".to_string();
    }
    let integer_digits = if value >= 100.0 {
        3
    } else if value >= 10.0 {
        2
    } else {
        1
    };
    let formatted = format!("{:.*}", 4 - integer_digits, value);
    if formatted.contains('.') {
        formatted
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    } else {
        formatted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_go_units_style_sizes() {
        assert_eq!(parse_size("10m").unwrap(), 10_000_000);
        assert_eq!(parse_size("50M").unwrap(), 50_000_000);
        assert_eq!(parse_size("5k").unwrap(), 5_000);
        assert_eq!(parse_size("1.5kB").unwrap(), 1_500);
        assert_eq!(parse_size("0").unwrap(), 0);
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert_eq!(parse_size("4MiB").unwrap(), 4 * 1024 * 1024);
        assert_eq!(parse_size(" 2 Ki ").unwrap(), 2048);
        assert!(parse_size("").is_err());
        assert!(parse_size("m").is_err());
        assert!(parse_size("10x").is_err());
        assert!(parse_size("-1").is_err());
    }

    #[test]
    fn formats_like_go_units() {
        assert_eq!(human_size(0), "0B");
        assert_eq!(human_size(3), "3B");
        assert_eq!(human_size(999), "999B");
        assert_eq!(human_size(2048), "2.048kB");
        assert_eq!(human_size(12_345), "12.35kB");
        assert_eq!(human_size(10_000_000), "10MB");
        assert_eq!(human_size(1_500_000), "1.5MB");
    }
}
