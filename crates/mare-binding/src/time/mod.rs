//! Public pure time arithmetic. No timer, agent, provider, task store or policy side effects.
mod cron;
mod instant;
mod recurrence;
#[cfg(test)]
mod tests;

use instant::{MAX_MS, MIN_MS};
use jiff::{ToSpan, civil::Time};
use mare_protocol::{TimePattern as Pattern, TimeQuery as Query};
use mare_shared::lua::{json_to_lua_lossless, nil_err, strict_lua_to_json};
use mlua::{Lua, Table, Value};
use serde_json::json;

trait PatternEval {
    fn validate(&mut self, authoring: bool) -> Result<(), String>;
    fn nearest(&self, reference: i64, forward: bool) -> Result<Option<i64>, String>;
}
impl PatternEval for Pattern {
    fn validate(&mut self, authoring: bool) -> Result<(), String> {
        match self {
            Self::Instant { at_ms } => {
                instant::timestamp(*at_ms)?;
            }
            Self::Interval { anchor_ms, every_ms } => {
                instant::timestamp(*anchor_ms)?;
                if *every_ms < 1 || *every_ms > 9_007_199_254_740_991 {
                    return Err("interval must be a positive safe integer of milliseconds".into());
                }
            }
            Self::Calendar { time, timezone, weekdays } => {
                instant::zone(timezone)?;
                let t: Time = time.parse().map_err(|e: jiff::Error| e.to_string())?;
                if t.subsec_nanosecond() % 1_000_000 != 0 {
                    return Err("calendar time supports millisecond precision".into());
                }
                *time = format!("{:02}:{:02}:{:02}.{:03}", t.hour(), t.minute(), t.second(), t.millisecond());
                if let Some(days) = weekdays
                    && (days.is_empty()
                        || days.iter().any(|d| !(1..=7).contains(d))
                        || days.windows(2).any(|v| v[0] >= v[1]))
                {
                    return Err(
                        "weekdays must be a nonempty unique ascending ISO Monday=1 through Sunday=7 array".into()
                    );
                }
            }
            Self::Cron { expression, timezone } => {
                instant::zone(timezone)?;
                *expression = cron::Cron::parse(expression)?.normalized();
            }
            Self::Rrule { rrule } => {
                *rrule =
                    if authoring { recurrence::Rule::parse(rrule)? } else { recurrence::Rule::read(rrule)? }.normalized;
            }
        }
        Ok(())
    }
    fn nearest(&self, reference: i64, forward: bool) -> Result<Option<i64>, String> {
        instant::timestamp(reference)?;
        match self {
            Self::Instant { at_ms } => {
                Ok(((forward && *at_ms > reference) || (!forward && *at_ms <= reference)).then_some(*at_ms))
            }
            Self::Interval { anchor_ms, every_ms } => {
                if reference < *anchor_ms {
                    return Ok(forward.then_some(*anchor_ms));
                }
                let n = (reference - *anchor_ms) / every_ms + i64::from(forward);
                let ms = i128::from(*anchor_ms) + i128::from(n) * i128::from(*every_ms);
                Ok((i128::from(MIN_MS)..=i128::from(MAX_MS)).contains(&ms).then_some(ms as i64))
            }
            Self::Calendar { time, timezone, weekdays } => {
                let tz = instant::zone(timezone)?;
                let time: Time = time.parse().map_err(|e: jiff::Error| e.to_string())?;
                let mut date = instant::civil(reference, &tz)?.date();
                for _ in 0..=370 {
                    if weekdays.as_ref().is_none_or(|ds| ds.contains(&(date.weekday().to_monday_one_offset() as u8)))
                        && let Some(ms) = instant::local(date.to_datetime(time), &tz)?
                        && ((forward && ms > reference) || (!forward && ms <= reference))
                    {
                        return Ok(Some(ms));
                    }
                    date = match date.checked_add(if forward { 1.days() } else { (-1).days() }) {
                        Ok(d) => d,
                        Err(_) => break,
                    };
                    if !(1..=9999).contains(&date.year()) {
                        break;
                    }
                }
                Ok(None)
            }
            Self::Cron { expression, timezone } => cron::Cron::parse(expression)?.nearest(reference, timezone, forward),
            Self::Rrule { rrule } => recurrence::Rule::read(rrule)?.nearest(reference, forward),
        }
    }
}
/// Shared implementation for Lua, RPC, child runtimes and headless hosts.
pub fn query(value: serde_json::Value) -> Result<serde_json::Value, String> {
    let request: Query = serde_json::from_value(value).map_err(|e| e.to_string())?;
    match request {
        Query::Sample { instant_ms } => {
            let ms = instant_ms.unwrap_or_else(|| jiff::Timestamp::now().as_millisecond());
            let tz = jiff::tz::TimeZone::try_system().map_err(|e| format!("host time zone unavailable: {e}"))?;
            let zoned = instant::timestamp(ms)?.to_zoned(tz.clone());
            Ok(json!({"action":"sample", "instant_ms":ms,
                "local_date":zoned.strftime("%Y-%m-%d").to_string(),
                "timestamp":zoned.strftime("%Y-%m-%dT%H:%M:%S%:z").to_string(),
                "timezone":tz.iana_name(), "offset":zoned.offset().to_string()}))
        }
        Query::Instant { value, date, time, timezone } => {
            let ms = if let Some(value) = value {
                if date.is_some() || time.is_some() || timezone.is_some() {
                    return Err("instant accepts either an offset timestamp or date/time/timezone".into());
                }
                instant::parse(&value)?
            } else {
                let dt = instant::date_time(&date.ok_or("date is required")?, &time.ok_or("time is required")?)?;
                let tz = instant::zone(&timezone.ok_or("timezone is required")?)?;
                if dt.subsec_nanosecond() % 1_000_000 != 0 {
                    return Err("local instant supports millisecond precision".into());
                }
                let ambiguous = tz.to_ambiguous_timestamp(dt);
                if ambiguous.is_ambiguous() {
                    return Err("explicit local time is ambiguous or nonexistent".into());
                }
                instant::local(dt, &tz)?.ok_or("local time is outside supported years")?
            };
            Ok(json!({"action":"instant","instant_ms":ms,"iso":instant::iso(ms)?}))
        }
        Query::Format { instant_ms, timezone, format } => {
            let ts = instant::timestamp(instant_ms)?.to_zoned(instant::zone(&timezone)?);
            let text = jiff::fmt::strtime::format(format.as_deref().unwrap_or("%Y-%m-%d %H:%M:%S %Z"), &ts)
                .map_err(|e| e.to_string())?;
            Ok(json!({"action":"format","text":text,"timezone":timezone}))
        }
        Query::Normalize { mut pattern, after_ms } => {
            pattern.validate(true)?;
            let next = pattern.nearest(after_ms, true)?;
            Ok(json!({"action":"normalize","pattern":pattern,"next_ms":next}))
        }
        Query::Next { mut pattern, after_ms } => {
            pattern.validate(false)?;
            Ok(json!({"action":"next","next_ms":pattern.nearest(after_ms,true)?}))
        }
        Query::Resolve { mut pattern, scheduled_ms, accepted_ms } => {
            pattern.validate(false)?;
            instant::timestamp(scheduled_ms)?;
            instant::timestamp(accepted_ms)?;
            if accepted_ms < scheduled_ms {
                return Err("accepted instant precedes the committed occurrence".into());
            }
            let occurrence = pattern
                .nearest(accepted_ms, false)?
                .filter(|ms| *ms >= scheduled_ms)
                .ok_or("no due occurrence at or after the committed occurrence")?;
            Ok(json!({"action":"resolve","occurrence_ms":occurrence,"next_ms":pattern.nearest(accepted_ms,true)?}))
        }
    }
}
pub fn install(lua: &Lua, mare: &Table) -> mlua::Result<()> {
    let t = lua.create_table()?;
    t.set(
        "query",
        lua.create_function(|lua, value: Value| {
            let result = strict_lua_to_json(lua, value).map_err(|e| e.to_string()).and_then(query);
            match result {
                Ok(v) => Ok(mlua::MultiValue::from_vec(vec![json_to_lua_lossless(lua, &v)?])),
                Err(e) => nil_err(lua, e),
            }
        })?,
    )?;
    mare.set("time", t)
}
