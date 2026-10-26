/// Parse a Kubernetes resource quantity (`100m`, `16314284Ki`, `1.5Gi`, `1e3`)
/// into base units: cores for CPU, bytes for memory, a count for pods.
pub(crate) fn parse(raw: &str) -> anyhow::Result<f64> {
    let (number, scale) = split(raw);
    let value: f64 = number.parse().map_err(|_| anyhow::anyhow!("invalid quantity {raw:?}"))?;
    anyhow::ensure!(value.is_finite(), "invalid quantity {raw:?}");
    Ok(match scale {
        Scale::Divide(d) => value / d,
        Scale::Multiply(m) => value * m,
    })
}

enum Scale {
    Divide(f64),
    Multiply(f64),
}

const BINARY: [(&str, i32); 6] = [("Ki", 1), ("Mi", 2), ("Gi", 3), ("Ti", 4), ("Pi", 5), ("Ei", 6)];

fn split(raw: &str) -> (&str, Scale) {
    for (suffix, power) in BINARY {
        if let Some(number) = raw.strip_suffix(suffix) {
            return (number, Scale::Multiply(1024f64.powi(power)));
        }
    }
    let Some(last) = raw.chars().last() else { return (raw, Scale::Multiply(1.0)) };
    let scale = match last {
        'n' => Scale::Divide(1e9),
        'u' => Scale::Divide(1e6),
        'm' => Scale::Divide(1e3),
        'k' => Scale::Multiply(1e3),
        'M' => Scale::Multiply(1e6),
        'G' => Scale::Multiply(1e9),
        'T' => Scale::Multiply(1e12),
        'P' => Scale::Multiply(1e15),
        'E' => Scale::Multiply(1e18),
        _ => return (raw, Scale::Multiply(1.0)),
    };
    (&raw[..raw.len() - 1], scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(raw: &str) -> f64 {
        parse(raw).unwrap_or_else(|e| panic!("{raw}: {e}"))
    }

    #[test]
    fn decimal_suffixes() {
        assert_eq!(q("123456789n"), 0.123456789);
        assert_eq!(q("250000u"), 0.25);
        assert_eq!(q("100m"), 0.1);
        assert_eq!(q("1950m"), 1.95);
        assert_eq!(q("2k"), 2000.0);
        assert_eq!(q("3M"), 3e6);
        assert_eq!(q("4G"), 4e9);
        assert_eq!(q("5T"), 5e12);
        assert_eq!(q("1E"), 1e18);
    }

    #[test]
    fn binary_suffixes() {
        assert_eq!(q("16314284Ki"), 16314284.0 * 1024.0);
        assert_eq!(q("512Mi"), 512.0 * 1024.0 * 1024.0);
        assert_eq!(q("1.5Gi"), 1.5 * 1024.0 * 1024.0 * 1024.0);
        assert_eq!(q("2Ti"), 2.0 * 1024f64.powi(4));
    }

    #[test]
    fn plain_decimal_and_exponent_forms() {
        assert_eq!(q("4"), 4.0);
        assert_eq!(q("110"), 110.0);
        assert_eq!(q("1.5"), 1.5);
        assert_eq!(q("1e3"), 1000.0);
        assert_eq!(q("1E3"), 1000.0);
        assert_eq!(q("28514975697"), 28514975697.0);
    }

    #[test]
    fn rejects_garbage() {
        for raw in ["", "abc", "Ki", "1Xi", "inf", "NaN", "1.2.3m"] {
            assert!(parse(raw).is_err(), "{raw} parsed");
        }
        assert!(parse("abc").unwrap_err().to_string().contains("abc"));
    }
}
