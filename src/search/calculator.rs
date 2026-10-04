use crate::config::Config;
use crate::search::{Action, ResultKind, SearchResult};

/// Evaluate inline arithmetic. Presentation follows the calculator
/// settings: fraction digits, thousands grouping (display only — the
/// copied value stays raw), an optional hex/octal/binary subtitle,
/// "expression = result" vs. result-only titles, and "Enter to paste"
/// instead of "Enter to copy".
pub fn evaluate(q: &str, config: &Config) -> Option<SearchResult> {
    if !q.chars().any(|c| c.is_ascii_digit())
        || !q
            .chars()
            .any(|c| matches!(c, '+' | '-' | '*' | '/' | '%' | '(' | '^'))
    {
        return None;
    }
    let v = ev::parse(q).ok()?;
    let (raw, display) = format_number(v, config);
    let title = if config.calc_show_expr {
        format!("{q} = {display}")
    } else {
        display
    };
    let mut subtitle = enter_label(config).to_string();
    if config.calc_bases {
        if let Some(bases) = base_suffix(v) {
            subtitle.push_str(&bases);
        }
    }
    Some(SearchResult {
        kind: ResultKind::Calculator,
        title,
        subtitle: Some(subtitle),
        icon: Some("accessories-calculator-symbolic".into()),
        action: Action::InsertCalculatorResult(raw),
        score: i32::MAX - 1,
    })
}

/// Format a number the way the calculator and the converters show it:
/// fixed fraction digits (from `calc_precision`), or scientific
/// notation when fixed notation would drown the value (|v| >= 1e15) or
/// lose it to rounding (rounds to zero — unless `calc_sci_notation` is
/// off). Returns `(copied, shown)`; only `shown` gets thousands
/// separators, so the value that lands on the clipboard stays pasteable
/// as-is.
pub fn format_number(v: f64, config: &Config) -> (String, String) {
    let prec = config.calc_precision.min(12) as usize;
    let mut raw = if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        let s = format!("{v:.prec$}");
        let t = s.trim_end_matches('0').trim_end_matches('.').to_string();
        // Values that round to zero ("0.000000") trim down to nothing.
        if t.is_empty() || t == "-" {
            "0".into()
        } else {
            t
        }
    };
    if config.calc_sci_notation
        && v != 0.0
        && (v.abs() >= 1e15 || raw.parse::<f64>().is_ok_and(|x| x == 0.0))
    {
        raw = sci_string(v, prec);
    }
    let display = if config.calc_separators {
        group_thousands(&raw)
    } else {
        raw.clone()
    };
    (raw, display)
}

/// What pressing Enter does, as every calculator/converter row spells
/// it out.
pub fn enter_label(config: &Config) -> &'static str {
    if config.calc_paste {
        "Enter to paste"
    } else {
        "Enter to copy"
    }
}

/// Scientific notation with `prec` decimals in the mantissa and the
/// trailing zeros trimmed: 9460730472580800 → "9.46073e15" at precision
/// 6, 1e-7 → "1e-7".
fn sci_string(v: f64, prec: usize) -> String {
    let s = format!("{:.*e}", prec, v);
    match s.split_once('e') {
        Some((mant, exp)) => {
            let m = mant.trim_end_matches('0').trim_end_matches('.');
            let m = if m.is_empty() || m == "-" { "0" } else { m };
            format!("{m}e{exp}")
        }
        None => s,
    }
}

/// "1234567.5" → "1,234,567.5" — display only; the copied value keeps
/// its plain form.
fn group_thousands(s: &str) -> String {
    let (sign, rest) = match s.strip_prefix('-') {
        Some(r) => ("-", r),
        None => ("", s),
    };
    let (int_part, frac) = match rest.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (rest, None),
    };
    let mut grouped = String::new();
    for (idx, ch) in int_part.chars().enumerate() {
        if idx > 0 && (int_part.len() - idx) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    match frac {
        Some(f) => format!("{sign}{grouped}.{f}"),
        None => format!("{sign}{grouped}"),
    }
}

/// " · 0xFF · 0o377 · 0b11111111" for integer results; None for
/// fractions or magnitudes beyond the i64 range.
fn base_suffix(v: f64) -> Option<String> {
    if v.fract() != 0.0 || v.abs() >= 1e15 {
        return None;
    }
    let i = v as i64;
    let sign = if i < 0 { "-" } else { "" };
    let u = i.unsigned_abs();
    Some(format!(" · {sign}0x{u:X} · {sign}0o{u:o} · {sign}0b{u:b}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_thousands_for_display_only() {
        assert_eq!(group_thousands("1234567"), "1,234,567");
        assert_eq!(group_thousands("-1234.5"), "-1,234.5");
        assert_eq!(group_thousands("999"), "999");
        assert_eq!(group_thousands("42"), "42");
    }

    #[test]
    fn separators_and_precision_shape_the_title_not_the_copy() {
        let mut cfg = Config::default();
        cfg.calc_separators = true;
        let r = evaluate("1000*1000", &cfg).unwrap();
        assert_eq!(r.title, "1000*1000 = 1,000,000");
        match &r.action {
            Action::InsertCalculatorResult(t) => assert_eq!(t, "1000000"),
            other => panic!("unexpected action: {other:?}"),
        }

        cfg.calc_precision = 2;
        let r = evaluate("1/3", &cfg).unwrap();
        assert_eq!(r.title, "1/3 = 0.33");
    }

    #[test]
    fn show_expr_off_bases_and_paste_reshape_the_row() {
        let mut cfg = Config::default();
        cfg.calc_show_expr = false;
        cfg.calc_bases = true;
        cfg.calc_paste = true;
        let r = evaluate("200+55", &cfg).unwrap();
        assert_eq!(r.title, "255");
        assert_eq!(
            r.subtitle.as_deref(),
            Some("Enter to paste · 0xFF · 0o377 · 0b11111111")
        );
    }

    #[test]
    fn values_rounding_to_zero_never_produce_an_empty_title() {
        let mut cfg = Config::default();
        cfg.calc_precision = 6;
        // Scientific notation (default on) keeps a value that fixed
        // digits would drown visible…
        let r = evaluate("1/10000000", &cfg).unwrap();
        assert_eq!(r.title, "1/10000000 = 1e-7");
        // …and with it off the old "0" stays non-empty.
        cfg.calc_sci_notation = false;
        let r = evaluate("1/10000000", &cfg).unwrap();
        assert_eq!(r.title, "1/10000000 = 0");
    }

    #[test]
    fn power_binds_tighter_than_multiply_and_is_right_associative() {
        let cfg = Config::default();
        assert_eq!(evaluate("2^10", &cfg).unwrap().title, "2^10 = 1024");
        assert_eq!(evaluate("3*2^4", &cfg).unwrap().title, "3*2^4 = 48");
        assert_eq!(evaluate("2^3^2", &cfg).unwrap().title, "2^3^2 = 512");
        assert_eq!(evaluate("2^-1", &cfg).unwrap().title, "2^-1 = 0.5");
        // Non-finite results are rejected like division by zero.
        assert!(evaluate("1/0", &cfg).is_none());
        assert!(evaluate("0^-1", &cfg).is_none());
    }

    #[test]
    fn scientific_notation_only_at_the_extremes() {
        let mut cfg = Config::default();
        cfg.calc_bases = false;
        // A result beyond the fixed-digit comfort zone.
        let r = evaluate("1000000000*1000000000", &cfg).unwrap();
        assert_eq!(r.title, "1000000000*1000000000 = 1e18");
        // Ordinary sizes keep plain digits.
        let r = evaluate("1000*1000", &cfg).unwrap();
        assert_eq!(r.title, "1000*1000 = 1000000");
        // Opting out restores full fixed digits.
        cfg.calc_sci_notation = false;
        let r = evaluate("1000000000*1000000000", &cfg).unwrap();
        assert_eq!(
            r.title,
            "1000000000*1000000000 = 1000000000000000000"
        );
    }
}
mod ev {
    pub struct E;
    pub fn parse(s: &str) -> Result<f64, E> {
        let mut p = P {
            s: s.as_bytes(),
            i: 0,
        };
        p.w();
        let v = p.expr()?;
        p.w();
        if p.i != p.s.len() || !v.is_finite() {
            Err(E)
        } else {
            Ok(v)
        }
    }
    struct P<'a> {
        s: &'a [u8],
        i: usize,
    }
    impl P<'_> {
        fn pk(&self) -> Option<u8> {
            self.s.get(self.i).copied()
        }
        fn w(&mut self) {
            while matches!(self.pk(), Some(b' ' | b'\t')) {
                self.i += 1;
            }
        }
        fn eat(&mut self, b: u8) -> bool {
            self.w();
            if self.pk() == Some(b) {
                self.i += 1;
                true
            } else {
                false
            }
        }
        fn expr(&mut self) -> Result<f64, E> {
            let mut a = self.term()?;
            loop {
                self.w();
                if self.eat(b'+') {
                    a += self.term()?
                } else if self.eat(b'-') {
                    a -= self.term()?
                } else {
                    return Ok(a);
                }
            }
        }
        fn term(&mut self) -> Result<f64, E> {
            let mut a = self.pow()?;
            loop {
                self.w();
                if self.eat(b'*') {
                    a *= self.pow()?
                } else if self.eat(b'/') {
                    let d = self.pow()?;
                    if d == 0.0 {
                        return Err(E);
                    }
                    a /= d
                } else if self.eat(b'%') {
                    a %= self.pow()?
                } else {
                    return Ok(a);
                }
            }
        }
        /// `^` binds tighter than `* / %` and is right-associative
        /// (2^3^2 = 2^9 = 512); the exponent may be a signed unary
        /// (2^-1 = 0.5).
        fn pow(&mut self) -> Result<f64, E> {
            let base = self.fac()?;
            self.w();
            if self.eat(b'^') {
                let e = self.pow()?;
                Ok(base.powf(e))
            } else {
                Ok(base)
            }
        }
        fn fac(&mut self) -> Result<f64, E> {
            self.w();
            if self.eat(b'-') {
                Ok(-self.fac()?)
            } else if self.eat(b'+') {
                self.fac()
            } else if self.eat(b'(') {
                let v = self.expr()?;
                if !self.eat(b')') {
                    Err(E)
                } else {
                    Ok(v)
                }
            } else {
                self.num()
            }
        }
        fn num(&mut self) -> Result<f64, E> {
            self.w();
            let s = self.i;
            while let Some(c) = self.pk() {
                if c.is_ascii_digit() || c == b'.' {
                    self.i += 1
                } else {
                    break;
                }
            }
            if self.i == s {
                Err(E)
            } else {
                std::str::from_utf8(&self.s[s..self.i])
                    .map_err(|_| E)?
                    .parse()
                    .map_err(|_| E)
            }
        }
    }
}
