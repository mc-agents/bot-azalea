use azalea::FormattedText;
use serde_json::Value;

/// A component as JSON, for mcp-server to flatten. azalea has already parsed it, so what goes is
/// what it kept; a hover event is not part of that, and nothing a reader is shown depends on one.
pub fn component(text: &FormattedText) -> Value {
    serde_json::to_value(text).unwrap_or_default()
}

/// A number as the other kind of bot writes it in a sentence: 8.0 is "8".
pub fn plain(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e21 {
        format!("{}", value as i64)
    } else {
        value.to_string()
    }
}

/// One decimal place, rounded the way Java's formatter rounds, which the other kind of bot uses.
///
/// Java rounds the shortest decimal that reads back as the value, half up, so 0.35 is "0.4".
/// Rust's formatter rounds the exact binary value, which is a hair under 0.35, and says "0.3": a
/// distance that differs between the two kinds in the last digit. Rust's `Display` gives the same
/// shortest decimal, so the rounding is done on that.
pub fn one_decimal(value: f64) -> String {
    let shortest = value.abs().to_string();
    let (whole, fraction) = shortest.split_once('.').unwrap_or((&shortest, ""));
    let digit = |index: usize| fraction.as_bytes().get(index).map_or(0, |digit| u64::from(digit - b'0'));

    let whole: u64 = whole.parse().unwrap_or_default();
    let tenths = whole * 10 + digit(0) + u64::from(digit(1) >= 5);
    let sign = if value < 0.0 { "-" } else { "" };

    format!("{sign}{}.{}", tenths / 10, tenths % 10)
}

#[cfg(test)]
mod tests {
    use super::one_decimal;

    #[test]
    fn rounds_the_decimal_a_reader_sees_rather_than_the_binary_under_it() {
        assert_eq!(one_decimal(0.35), "0.4");
        assert_eq!(one_decimal(2.25), "2.3");
        assert_eq!(one_decimal(9.96), "10.0");
        assert_eq!(one_decimal(3.0), "3.0");
    }
}
