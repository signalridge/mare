//! Pure instant/civil conversion. Explicit gaps reject; recurrence gaps are skipped.
use jiff::{
    Timestamp,
    civil::DateTime,
    tz::{AmbiguousOffset, TimeZone},
};

pub const MIN_MS: i64 = -62_135_596_800_000;
pub const MAX_MS: i64 = 253_402_300_799_999;
pub const DAY_MS: i64 = 86_400_000;
pub fn timestamp(ms: i64) -> Result<Timestamp, String> {
    if !(MIN_MS..=MAX_MS).contains(&ms) {
        return Err("instant must use a four-digit positive year".into());
    }
    Timestamp::from_millisecond(ms).map_err(|e| e.to_string())
}
pub fn zone(name: &str) -> Result<TimeZone, String> {
    if name == "UTC" {
        return Ok(TimeZone::UTC);
    }
    if !name.contains('/') {
        return Err("time zone must be UTC or an IANA Area/Location name".into());
    }
    TimeZone::get(name).map_err(|e| e.to_string())
}
pub fn local(dt: DateTime, tz: &TimeZone) -> Result<Option<i64>, String> {
    let value = tz.to_ambiguous_timestamp(dt);
    if matches!(value.offset(), AmbiguousOffset::Gap { .. }) {
        return Ok(None);
    }
    let ms = value.earlier().map_err(|e| e.to_string())?.as_millisecond();
    Ok((MIN_MS..=MAX_MS).contains(&ms).then_some(ms))
}
pub fn civil(ms: i64, tz: &TimeZone) -> Result<DateTime, String> {
    Ok(timestamp(ms)?.to_zoned(tz.clone()).datetime())
}
pub fn wall(dt: DateTime) -> Result<i64, String> {
    Ok(TimeZone::UTC.to_timestamp(dt).map_err(|e| e.to_string())?.as_millisecond())
}
pub fn parse(value: &str) -> Result<i64, String> {
    let ts: Timestamp = value.parse().map_err(|e: jiff::Error| e.to_string())?;
    if ts.subsec_nanosecond() % 1_000_000 != 0 {
        return Err("instant supports millisecond precision".into());
    }
    let ms = ts.as_millisecond();
    timestamp(ms)?;
    Ok(ms)
}
pub fn iso(ms: i64) -> Result<String, String> {
    Ok(format!("{:.3}", timestamp(ms)?))
}
pub fn date_time(date: &str, time: &str) -> Result<DateTime, String> {
    format!("{date}T{time}").parse().map_err(|e: jiff::Error| e.to_string())
}
pub fn basic(value: &str) -> Result<DateTime, String> {
    if value.len() != 15
        || value.as_bytes()[8] != b'T'
        || !value.bytes().enumerate().all(|(i, c)| i == 8 || c.is_ascii_digit())
    {
        return Err("date-time must use YYYYMMDDTHHMMSS".into());
    }
    let dt = date_time(
        &format!("{}-{}-{}", &value[..4], &value[4..6], &value[6..8]),
        &format!("{}:{}:{}", &value[9..11], &value[11..13], &value[13..15]),
    )?;
    if dt.year() < 100 {
        return Err("DTSTART and UNTIL years must be 0100 through 9999".into());
    }
    Ok(dt)
}
pub fn basic_text(dt: DateTime) -> String {
    dt.strftime("%Y%m%dT%H%M%S").to_string()
}
