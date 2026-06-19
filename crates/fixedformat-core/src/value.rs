//! The neutral decoded-value tree produced by [`crate::decode`] and consumed by
//! [`crate::encode`]. The worker maps this onto Arrow arrays.

/// A decoded field value. Numbers stay exact: decimals carry an unscaled i128
/// plus a scale so the worker can build a DuckDB `DECIMAL(p, s)` without float
/// rounding.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// SQL NULL.
    Null,
    /// VARCHAR.
    Text(String),
    /// BIGINT.
    Int(i64),
    /// DECIMAL(p, s): value = `unscaled * 10^-scale`.
    Decimal { unscaled: i128, scale: u8 },
    /// DOUBLE / REAL.
    Float(f64),
    /// BOOLEAN.
    Bool(bool),
    /// LIST (OCCURS).
    List(Vec<Value>),
    /// STRUCT (group item / folded REDEFINES).
    Struct(Vec<(String, Value)>),
}

impl Value {
    /// Format a decimal's exact textual representation (for tests / display).
    pub fn decimal_string(unscaled: i128, scale: u8) -> String {
        if scale == 0 {
            return unscaled.to_string();
        }
        let neg = unscaled < 0;
        let digits = unscaled.unsigned_abs().to_string();
        let scale = scale as usize;
        let padded = if digits.len() <= scale {
            format!("{}{}", "0".repeat(scale - digits.len() + 1), digits)
        } else {
            digits
        };
        let point = padded.len() - scale;
        let (int_part, frac_part) = padded.split_at(point);
        format!("{}{}.{}", if neg { "-" } else { "" }, int_part, frac_part)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_string_formats() {
        assert_eq!(Value::decimal_string(12345, 2), "123.45");
        assert_eq!(Value::decimal_string(-12345, 2), "-123.45");
        assert_eq!(Value::decimal_string(5, 2), "0.05");
        assert_eq!(Value::decimal_string(100, 0), "100");
        assert_eq!(Value::decimal_string(0, 2), "0.00");
    }
}
