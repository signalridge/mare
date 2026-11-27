use serde_json::Value;

// Compare JSON numbers as decimal values without rounding large integers through f64.
pub(super) fn decimal_parts(s: &str) -> Result<(bool, String, i128), String> {
    let (negative, s) = match s.strip_prefix('-') {
        Some(s) => (true, s),
        None => (false, s),
    };
    let (mantissa, exponent) = match s.split_once(['e', 'E']) {
        Some((m, e)) => (m, e.parse::<i128>().map_err(|_| format!("decimal exponent out of range: {s}"))?),
        None => (s, 0),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if (whole.is_empty() && fraction.is_empty())
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(format!("invalid decimal number: {s}"));
    }
    let digits = format!("{whole}{fraction}");
    let trimmed = digits.trim_start_matches('0');
    if trimmed.is_empty() {
        return Ok((false, "0".into(), 0));
    }
    let significant = trimmed.trim_end_matches('0');
    let frac_len = i128::try_from(fraction.len()).map_err(|_| format!("decimal fraction too long: {s}"))?;
    let trailing_zeros =
        i128::try_from(trimmed.len() - significant.len()).map_err(|_| format!("decimal too long: {s}"))?;
    let delta = trailing_zeros.checked_sub(frac_len).ok_or_else(|| format!("decimal exponent out of range: {s}"))?;
    let scale = exponent.checked_add(delta).ok_or_else(|| format!("decimal exponent out of range: {s}"))?;
    Ok((negative, significant.to_string(), scale))
}

pub fn try_json_equal(a: &Value, b: &Value) -> Result<bool, String> {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => Ok(decimal_parts(&a.to_string())? == decimal_parts(&b.to_string())?),
        (Value::Array(a), Value::Array(b)) => {
            if a.len() != b.len() {
                return Ok(false);
            }
            for (a, b) in a.iter().zip(b) {
                if !try_json_equal(a, b)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        (Value::Object(a), Value::Object(b)) => {
            if a.len() != b.len() {
                return Ok(false);
            }
            for (k, v) in a {
                let Some(w) = b.get(k) else {
                    return Ok(false);
                };
                if !try_json_equal(v, w)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        _ => Ok(a == b),
    }
}

pub fn json_equal(a: &Value, b: &Value) -> bool {
    try_json_equal(a, b).unwrap_or(false)
}
