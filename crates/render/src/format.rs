use peephole_core::Fmt;

/// Print a number the way a [`Fmt`] asks; non-finite values print as `--`.
pub fn format_value(v: f64, fmt: Fmt) -> String {
    if !v.is_finite() {
        return "--".into();
    }
    let v = if v == 0.0 { 0.0 } else { v };
    match fmt {
        Fmt::Auto => auto(v),
        Fmt::Int => format!("{:.0}", v.round()),
        Fmt::Pct => format!("{:.0}%", (v * 100.0).round()),
        Fmt::Dec1 => format!("{v:.1}"),
        Fmt::Bytes => bytes(v),
        Fmt::Duration => duration(v),
        Fmt::Temp => format!("{:.0}°", v.round()),
        Fmt::Thousands => thousands(v),
    }
}

/// Age of a value in its largest whole unit: `45s`, `3m`, `2h`, `3d`.
pub fn format_age(secs: i64) -> String {
    match secs {
        s if s >= 86_400 => format!("{}d", s / 86_400),
        s if s >= 3_600 => format!("{}h", s / 3_600),
        s if s >= 60 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// A [`Fmt`] by its config name (`auto`, `int`, `pct`, `dec1`, `bytes`, `duration`, `temp`, `thousands`).
pub fn parse_fmt(name: &str) -> Option<Fmt> {
    Some(match name {
        "auto" => Fmt::Auto,
        "int" => Fmt::Int,
        "pct" => Fmt::Pct,
        "dec1" => Fmt::Dec1,
        "bytes" => Fmt::Bytes,
        "duration" => Fmt::Duration,
        "temp" => Fmt::Temp,
        "thousands" => Fmt::Thousands,
        _ => return None,
    })
}

fn trimmed(v: f64, decimals: usize) -> String {
    let s = format!("{v:.decimals$}");
    if !s.contains('.') {
        return s;
    }
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".into() } else { s.into() }
}

fn auto(v: f64) -> String {
    let a = v.abs();
    if a >= 1e9 {
        format!("{}G", trimmed(v / 1e9, 1))
    } else if a >= 1e6 {
        format!("{}M", trimmed(v / 1e6, 1))
    } else if a >= 1e3 {
        format!("{}k", trimmed(v / 1e3, 1))
    } else if a >= 100.0 {
        format!("{:.0}", v.round())
    } else if a >= 10.0 {
        trimmed(v, 1)
    } else {
        trimmed(v, 2)
    }
}

fn bytes(v: f64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut x = v;
    let mut i = 0;
    while x.abs() >= 1024.0 && i < UNITS.len() - 1 {
        x /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{:.0} B", x.round()) } else { format!("{x:.1} {}", UNITS[i]) }
}

fn duration(v: f64) -> String {
    let s = v.max(0.0).round() as i64;
    let (d, h, m) = (s / 86_400, s % 86_400 / 3_600, s % 3_600 / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{s}s")
    }
}

fn thousands(v: f64) -> String {
    let n = v.round() as i64;
    let digits = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    if n < 0 { format!("-{out}") } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_every_fmt() {
        let cases: &[(f64, Fmt, &str)] = &[
            (0.5, Fmt::Auto, "0.5"),
            (3.14659, Fmt::Auto, "3.15"),
            (2.0, Fmt::Auto, "2"),
            (0.0, Fmt::Auto, "0"),
            (42.46, Fmt::Auto, "42.5"),
            (42.0, Fmt::Auto, "42"),
            (187.4, Fmt::Auto, "187"),
            (1234.0, Fmt::Auto, "1.2k"),
            (1000.0, Fmt::Auto, "1k"),
            (97213.0, Fmt::Auto, "97.2k"),
            (3_400_000.0, Fmt::Auto, "3.4M"),
            (2_500_000_000.0, Fmt::Auto, "2.5G"),
            (-1234.0, Fmt::Auto, "-1.2k"),
            (-0.25, Fmt::Auto, "-0.25"),
            (12.6, Fmt::Int, "13"),
            (-0.0, Fmt::Int, "0"),
            (-0.0, Fmt::Auto, "0"),
            (97213.0, Fmt::Int, "97213"),
            (0.87, Fmt::Pct, "87%"),
            (1.5, Fmt::Pct, "150%"),
            (0.0, Fmt::Pct, "0%"),
            (3.04659, Fmt::Dec1, "3.0"),
            (512.0, Fmt::Bytes, "512 B"),
            (1536.0, Fmt::Bytes, "1.5 KB"),
            (2.3 * 1024.0 * 1024.0 * 1024.0, Fmt::Bytes, "2.3 GB"),
            (5.0 * 1024f64.powi(4), Fmt::Bytes, "5.0 TB"),
            (45.0, Fmt::Duration, "45s"),
            (720.0, Fmt::Duration, "12m"),
            (3.0 * 3600.0 + 120.0, Fmt::Duration, "3h 2m"),
            (3.0 * 86400.0 + 4.0 * 3600.0, Fmt::Duration, "3d 4h"),
            (72.6, Fmt::Temp, "73°"),
            (97213.0, Fmt::Thousands, "97,213"),
            (1_234_567.4, Fmt::Thousands, "1,234,567"),
            (-4321.0, Fmt::Thousands, "-4,321"),
            (999.0, Fmt::Thousands, "999"),
        ];
        for (v, f, want) in cases {
            assert_eq!(format_value(*v, *f), *want, "{v} as {f:?}");
        }
    }

    #[test]
    fn non_finite_values_print_as_missing() {
        assert_eq!(format_value(f64::NAN, Fmt::Auto), "--");
        assert_eq!(format_value(f64::INFINITY, Fmt::Pct), "--");
    }

    #[test]
    fn age_uses_the_largest_unit() {
        assert_eq!(format_age(0), "0s");
        assert_eq!(format_age(45), "45s");
        assert_eq!(format_age(180), "3m");
        assert_eq!(format_age(7300), "2h");
        assert_eq!(format_age(3 * 86400 + 5), "3d");
    }

    #[test]
    fn parses_fmt_names_in_snake_case() {
        assert_eq!(parse_fmt("thousands"), Some(Fmt::Thousands));
        assert_eq!(parse_fmt("dec1"), Some(Fmt::Dec1));
        assert_eq!(parse_fmt("pct"), Some(Fmt::Pct));
        assert_eq!(parse_fmt("whatever"), None);
    }
}
