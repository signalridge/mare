//! Pure time queries shared by every host surface. No scheduler or task state lives here.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// JSON Schema integers include exact integral JSON floats; Lua clocks return numbers.
// Keep safe-integer validation identical on both boundaries rather than truncating.
fn integer<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<i64, D::Error> {
    let value = serde_json::Number::deserialize(deserializer)?;
    if let Some(n) = value.as_i64() {
        if n.unsigned_abs() <= 9_007_199_254_740_991 {
            return Ok(n);
        }
    } else if let Some(n) = value.as_f64()
        && n.is_finite()
        && n.fract() == 0.0
        && n.abs() <= 9_007_199_254_740_991.0
    {
        return Ok(n as i64);
    }
    Err(serde::de::Error::custom("a safe integer is required"))
}
fn weekdays<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<Vec<u8>>, D::Error> {
    let values = Option::<Vec<serde_json::Number>>::deserialize(deserializer)?;
    values
        .map(|values| {
            values
                .into_iter()
                .map(|n| {
                    let n = n.as_f64().ok_or_else(|| serde::de::Error::custom("an integer weekday is required"))?;
                    if n.fract() != 0.0 || !(1.0..=7.0).contains(&n) {
                        return Err(serde::de::Error::custom("weekdays must be integers in 1..=7"));
                    }
                    Ok(n as u8)
                })
                .collect()
        })
        .transpose()
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TimePattern {
    Instant {
        #[serde(deserialize_with = "integer")]
        at_ms: i64,
    },
    Interval {
        #[serde(deserialize_with = "integer")]
        anchor_ms: i64,
        #[serde(deserialize_with = "integer")]
        every_ms: i64,
    },
    Calendar {
        time: String,
        timezone: String,
        #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "weekdays")]
        weekdays: Option<Vec<u8>>,
    },
    Cron {
        expression: String,
        timezone: String,
    },
    Rrule {
        rrule: String,
    },
}
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum TimeQuery {
    Sample {
        #[serde(default, deserialize_with = "optional_integer")]
        instant_ms: Option<i64>,
    },
    Instant {
        #[serde(default)]
        value: Option<String>,
        #[serde(default)]
        date: Option<String>,
        #[serde(default)]
        time: Option<String>,
        #[serde(default)]
        timezone: Option<String>,
    },
    Format {
        #[serde(deserialize_with = "integer")]
        instant_ms: i64,
        #[serde(default = "utc")]
        timezone: String,
        #[serde(default)]
        format: Option<String>,
    },
    Normalize {
        pattern: TimePattern,
        #[serde(deserialize_with = "integer")]
        after_ms: i64,
    },
    Next {
        pattern: TimePattern,
        #[serde(deserialize_with = "integer")]
        after_ms: i64,
    },
    Resolve {
        pattern: TimePattern,
        #[serde(deserialize_with = "integer")]
        scheduled_ms: i64,
        #[serde(deserialize_with = "integer")]
        accepted_ms: i64,
    },
}
fn optional_integer<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    #[derive(Deserialize)]
    struct Strict(#[serde(deserialize_with = "integer")] i64);
    Ok(Option::<Strict>::deserialize(d)?.map(|n| n.0))
}
fn utc() -> String {
    "UTC".into()
}
/// The discriminator makes each successful response unambiguous to schema clients.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum TimeQueryResult {
    Sample { instant_ms: i64, local_date: String, timestamp: String, timezone: Option<String>, offset: String },
    Instant { instant_ms: i64, iso: String },
    Format { text: String, timezone: String },
    Normalize { pattern: TimePattern, next_ms: Option<i64> },
    Next { next_ms: Option<i64> },
    Resolve { occurrence_ms: i64, next_ms: Option<i64> },
}
