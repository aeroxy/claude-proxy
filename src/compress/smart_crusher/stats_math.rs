//! Numeric statistics helpers. `mean` / `sample_stdev` /
//! `sample_variance` are used by `analyzer`; the rest is staged for
//! the public statistics API.
#![allow(dead_code)]

//! Numeric statistics helpers used by `SmartAnalyzer`.
//!
//! Uses **sample** variance/stdev (n-1 denominator), not population (n denominator).
//! Mismatching the denominator shifts every variance-based decision (change
//! points, anomaly thresholds, crushability cases).

/// Arithmetic mean. Returns `None` on empty input or if the result is non-finite (Inf/NaN).
pub fn mean(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let sum: f64 = values.iter().sum();
    let m = sum / values.len() as f64;
    if m.is_finite() {
        Some(m)
    } else {
        None
    }
}

/// Sample variance with `n-1` denominator.
/// Requires at least 2 values; returns `None` for fewer. Also returns `None`
/// on non-finite results.
pub fn sample_variance(values: &[f64]) -> Option<f64> {
    if values.len() < 2 {
        return None;
    }
    let m = mean(values)?;
    let sum_sq_diff: f64 = values.iter().map(|v| (v - m).powi(2)).sum();
    let var = sum_sq_diff / (values.len() - 1) as f64;
    if var.is_finite() {
        Some(var)
    } else {
        None
    }
}

/// Sample standard deviation — sqrt of `sample_variance`. Same n>=2
/// requirement as the variance helper.
pub fn sample_stdev(values: &[f64]) -> Option<f64> {
    sample_variance(values).map(f64::sqrt)
}

/// Median. Returns the middle element for
/// odd-count input, mean of two middles for even-count. Returns `None`
/// on empty input.
///
/// We sort with `total_cmp` to keep behavior deterministic.
pub fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted: Vec<f64> = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    if n % 2 == 0 {
        // Mean of the two middle elements.
        let lo = sorted[n / 2 - 1];
        let hi = sorted[n / 2];
        Some(lo / 2.0 + hi / 2.0)
    } else {
        Some(sorted[n / 2])
    }
}

/// General-purpose float formatting.
///
/// Rules:
/// - 4 significant digits.
/// - Scientific notation when `exponent < -4` OR `exponent >= 4`.
/// - Trailing zeros stripped (and the `.` if all decimals removed).
/// - Scientific exponent padded to at least 2 digits with explicit sign.
///
/// Used for crusher strategy debug strings.
///
/// - **Banker's rounding (round half-to-even)**: we use banker's rounding.
/// - **NaN/Inf**: prints `nan`, `inf`, `-inf`.
pub fn format_g(x: f64) -> String {
    if x.is_nan() {
        return "nan".to_string();
    }
    if x.is_infinite() {
        return if x > 0.0 {
            "inf".to_string()
        } else {
            "-inf".to_string()
        };
    }
    if x == 0.0 {
        return "0".to_string();
    }

    let abs = x.abs();
    let exp = abs.log10().floor() as i32;

    if !(-4..4).contains(&exp) {
        // Scientific. Uses 4 sig figs → 3 digits after decimal in mantissa.
        let s = format!("{:.3e}", x);
        normalize_scientific_exp(&s)
    } else {
        let digits_after = (3 - exp).max(0) as usize;
        let s = format!("{:.*}", digits_after, x);
        if s.contains('.') {
            // Trim trailing zeros and a dangling decimal point.
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s
        }
    }
}

fn normalize_scientific_exp(s: &str) -> String {
    let Some(epos) = s.find('e') else {
        return s.to_string();
    };
    let (mantissa, rest) = s.split_at(epos);
    let exp_part = &rest[1..];
    let exp_num: i32 = exp_part.parse().unwrap_or(0);
    let mantissa_clean = if mantissa.contains('.') {
        mantissa
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    } else {
        mantissa.to_string()
    };
    let sign = if exp_num >= 0 { "+" } else { "-" };
    format!("{}e{}{:02}", mantissa_clean, sign, exp_num.abs())
}
