//! Exact decimal `multipleOf`: delegates to jsonschema's arbitrary-precision validator,
//! guarding the integer-divisor path that would otherwise round through `f64`.
use serde_json::Value;

/// Returns `true` if a JSON number's decimal representation is an exact integer.
///
/// Used to guard `jsonschema 0.61.2`'s `MultipleOfBigIntValidator`, which calls
/// `as_f64()` before `try_parse_bigfraction` and would otherwise round
/// `1.00000000000000001` to `1.0`.
fn is_exact_integer_decimal(s: &str) -> bool {
    let s = s.strip_prefix(['-', '+']).unwrap_or(s);
    let (mantissa, exp) = match s.split_once(['e', 'E']) {
        Some((m, e)) => match e.parse::<i128>() {
            Ok(exp) => (m, exp),
            Err(_) => return false,
        },
        None => (s, 0),
    };
    let (int_part, frac_part) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let frac_trimmed = frac_part.trim_end_matches('0');
    if !frac_trimmed.is_empty() {
        return exp >= i128::try_from(frac_trimmed.len()).unwrap_or(i128::MAX);
    }
    let int_trimmed = int_part.trim_start_matches('0');
    if int_trimmed.is_empty() {
        return true;
    }
    let trailing_zeros = int_trimmed.len() - int_trimmed.trim_end_matches('0').len();
    exp >= -i128::try_from(trailing_zeros).unwrap_or(i128::MAX)
}

pub(super) struct ExactMultipleOfKeyword {
    divisor: Value,
    divisor_is_integer: bool,
    inner: jsonschema::Validator,
}

impl ExactMultipleOfKeyword {
    /// Keyword factory for `jsonschema::options().with_keyword("multipleOf", ...)`.
    pub(super) fn build(val: &Value) -> Result<Self, jsonschema::ValidationError<'static>> {
        let Some(n) = val.as_number() else {
            return Err(jsonschema::ValidationError::custom("multipleOf must be a positive number"));
        };
        let inner = jsonschema::validator_for(&serde_json::json!({ "multipleOf": val }))
            .map_err(|e| jsonschema::ValidationError::custom(e.to_string()))?;
        let divisor_is_integer = is_exact_integer_decimal(&n.to_string());
        Ok(Self { divisor: val.clone(), divisor_is_integer, inner })
    }
}

impl<'i> jsonschema::Keyword<'i> for ExactMultipleOfKeyword {
    fn validate(&self, instance: &'i Value) -> Result<(), jsonschema::ValidationError<'i>> {
        if self.is_valid(instance) {
            Ok(())
        } else {
            Err(jsonschema::ValidationError::custom(format!("{instance} is not a multiple of {}", self.divisor)))
        }
    }

    fn is_valid(&self, instance: &'i Value) -> bool {
        let Some(n) = instance.as_number() else {
            return true;
        };
        if self.divisor_is_integer && !is_exact_integer_decimal(&n.to_string()) {
            return false;
        }
        self.inner.is_valid(instance)
    }
}
