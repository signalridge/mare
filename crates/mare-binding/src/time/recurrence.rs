//! Bounded RFC5545 profile over UTC-shaped wall times; map valid candidates to IANA instants.
//! Aligned FREQ periods, explicit DTSTART defaults, COUNT on actual distinct instants, gap
//! skipping and earlier folds.
use super::instant::{self, DAY_MS, MAX_MS};
use jiff::{
    civil::{Date, DateTime},
    tz::TimeZone,
};
use std::collections::{BTreeMap, BTreeSet};
const MAX_CANDIDATES: usize = 10_080;
const MAX_PERIODS: usize = 146_098;
const DAYS: [&str; 7] = ["MO", "TU", "WE", "TH", "FR", "SA", "SU"];
#[derive(Debug, Clone)]
pub struct Rule {
    pub normalized: String,
    start: DateTime,
    start_ms: i64,
    tz: TimeZone,
    freq: String,
    interval: i64,
    count: Option<usize>,
    until: i64,
    wkst: i64,
    fields: BTreeMap<String, String>,
}
fn positive(text: &str, max: i64) -> Result<i64, String> {
    if text.is_empty() || !text.bytes().all(|c| c.is_ascii_digit()) {
        return Err("RRULE requires positive integer values".into());
    }
    let n = text.parse::<i64>().map_err(|e| e.to_string())?;
    if n < 1 || n > max {
        return Err(format!("RRULE integer must be in 1..={max}"));
    }
    Ok(n)
}
fn list(text: &str, min: i64, max: i64) -> Result<Vec<i64>, String> {
    let mut values = BTreeSet::new();
    for raw in text.split(',') {
        let digits = if min < 0 { raw.trim_start_matches(['+', '-']) } else { raw };
        if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
            return Err("Invalid RRULE numeric list".into());
        }
        let n = raw.parse::<i64>().map_err(|e| e.to_string())?;
        if n < min || n > max || (min < 0 && n == 0) || !values.insert(n) {
            return Err("RRULE list values must be unique and in range".into());
        }
    }
    Ok(values.into_iter().collect())
}
impl Rule {
    pub fn parse(text: &str) -> Result<Self, String> {
        Self::prepare(text, true)
    }
    pub fn read(text: &str) -> Result<Self, String> {
        Self::prepare(text, false)
    }
    fn prepare(text: &str, authoring: bool) -> Result<Self, String> {
        let lines: Vec<_> = text.lines().map(str::trim).filter(|s| !s.is_empty()).collect();
        if lines.len() != 2 || !lines[1].starts_with("RRULE:") {
            return Err("Require one DTSTART line then one RRULE line".into());
        }
        let (tz, start) = if let Some(raw) = lines[0].strip_prefix("DTSTART:").and_then(|v| v.strip_suffix('Z')) {
            (TimeZone::UTC, instant::basic(raw)?)
        } else if let Some((zone, raw)) = lines[0].strip_prefix("DTSTART;TZID=").and_then(|s| s.split_once(':')) {
            (instant::zone(zone)?, instant::basic(raw)?)
        } else {
            return Err("DTSTART requires UTC Z or an explicit IANA TZID".into());
        };
        let start_ms = instant::local(start, &tz)?.ok_or("DTSTART is nonexistent in its time zone")?;
        let allowed = [
            "FREQ",
            "INTERVAL",
            "COUNT",
            "UNTIL",
            "WKST",
            "BYSECOND",
            "BYMINUTE",
            "BYHOUR",
            "BYDAY",
            "BYMONTHDAY",
            "BYYEARDAY",
            "BYWEEKNO",
            "BYMONTH",
            "BYSETPOS",
        ];
        let mut fields = BTreeMap::new();
        for part in lines[1][6..].split(';') {
            let (key, value) = part.split_once('=').ok_or("RRULE fields require key=value")?;
            if !allowed.contains(&key)
                || value.is_empty()
                || fields.insert(key.to_string(), value.to_string()).is_some()
            {
                return Err("RRULE fields must be supported, nonempty and unique".into());
            }
        }
        let freq = fields.get("FREQ").ok_or("FREQ is required")?.clone();
        if !["MINUTELY", "HOURLY", "DAILY", "WEEKLY", "MONTHLY", "YEARLY"].contains(&freq.as_str()) {
            return Err("Supported FREQ: MINUTELY HOURLY DAILY WEEKLY MONTHLY YEARLY".into());
        }
        let interval = fields.get("INTERVAL").map(|s| positive(s, 9_007_199_254_740_991)).transpose()?.unwrap_or(1);
        let count = fields.get("COUNT").map(|s| positive(s, 1000).map(|n| n as usize)).transpose()?;
        let until = if let Some(raw) = fields.get("UNTIL") {
            if count.is_some() {
                return Err("COUNT and UNTIL are mutually exclusive".into());
            }
            instant::wall(instant::basic(raw.strip_suffix('Z').ok_or("UNTIL requires absolute UTC Z")?)?)?
        } else {
            MAX_MS
        };
        let wkst =
            DAYS.iter().position(|d| *d == fields.get("WKST").map_or("MO", String::as_str)).ok_or("Invalid WKST")?
                as i64;
        let mut lists: BTreeMap<String, Vec<i64>> = BTreeMap::new();
        for (key, min, max) in [
            ("BYSECOND", 0, 59),
            ("BYMINUTE", 0, 59),
            ("BYHOUR", 0, 23),
            ("BYMONTHDAY", -31, 31),
            ("BYYEARDAY", -366, 366),
            ("BYWEEKNO", -53, 53),
            ("BYMONTH", 1, 12),
            ("BYSETPOS", -366, 366),
        ] {
            if let Some(raw) = fields.get(key) {
                let values = list(raw, min, max)?;
                fields.insert(key.into(), values.iter().map(ToString::to_string).collect::<Vec<_>>().join(","));
                lists.insert(key.into(), values);
            }
        }
        let mut byday = 0;
        let mut ordinals = 0;
        if let Some(raw) = fields.get("BYDAY") {
            let mut seen = BTreeSet::new();
            for item in raw.split(',') {
                if item.len() < 2 || !item.is_ascii() {
                    return Err("Invalid BYDAY selector".into());
                }
                let (ordinal, day) = item.split_at(item.len() - 2);
                if !DAYS.contains(&day) {
                    return Err("Invalid BYDAY weekday".into());
                }
                let n = if ordinal.is_empty() {
                    0
                } else {
                    let n = list(ordinal, -53, 53)?[0];
                    ordinals += 1;
                    n
                };
                if !seen.insert((n, day)) {
                    return Err("Duplicate BYDAY selector".into());
                }
                byday += 1;
            }
        }
        if lists.get("BYSECOND").is_some_and(|s| s.len() > 1) {
            return Err("Bounded recurrence permits one BYSECOND value".into());
        }
        if (freq == "MINUTELY" || freq == "HOURLY") && (!lists.is_empty() || byday > 0) {
            return Err("MINUTELY/HOURLY do not accept BY fields".into());
        }
        if (lists.contains_key("BYYEARDAY") || lists.contains_key("BYWEEKNO")) && freq != "YEARLY" {
            return Err("BYYEARDAY/BYWEEKNO require YEARLY".into());
        }
        if (lists.contains_key("BYMONTHDAY") || ordinals > 0) && freq != "MONTHLY" && freq != "YEARLY" {
            return Err("BYMONTHDAY and ordinal BYDAY require MONTHLY/YEARLY".into());
        }
        if ordinals > 0 && lists.contains_key("BYWEEKNO") {
            return Err("Ordinal BYDAY cannot combine with BYWEEKNO".into());
        }
        if lists.contains_key("BYSETPOS") && lists.len() == 1 && byday == 0 {
            return Err("BYSETPOS requires another BY field".into());
        }
        let size = |key: &str| lists.get(key).map_or(f64::INFINITY, |v| v.len() as f64);
        let day_bound =
            |per: f64| if byday == 0 { f64::INFINITY } else { f64::from(ordinals) + per * f64::from(byday - ordinals) };
        let days = match freq.as_str() {
            "MINUTELY" | "HOURLY" | "DAILY" => 1.0,
            "WEEKLY" => 7.0_f64.min(day_bound(1.0)),
            "MONTHLY" => 31.0_f64.min(size("BYMONTHDAY")).min(day_bound(5.0)),
            _ => {
                if let Some(months) = lists.get("BYMONTH") {
                    (31.0 * months.len() as f64)
                        .min(size("BYYEARDAY"))
                        .min(months.len() as f64 * size("BYMONTHDAY"))
                        .min(9.0 * size("BYWEEKNO"))
                        .min(months.len() as f64 * day_bound(5.0))
                } else {
                    366.0_f64
                        .min(size("BYYEARDAY"))
                        .min(12.0 * size("BYMONTHDAY"))
                        .min(9.0 * size("BYWEEKNO"))
                        .min(day_bound(53.0))
                }
            }
        };
        let candidates =
            ["BYHOUR", "BYMINUTE", "BYSECOND"].iter().fold(days, |v, k| v * lists.get(*k).map_or(1, Vec::len) as f64);
        let positions = lists.get("BYSETPOS");
        if candidates > MAX_CANDIDATES as f64 {
            return Err("RRULE period exceeds 10080 candidate bound".into());
        }
        if positions.is_some_and(|ps| {
            ps.len() > 31 || (authoring && candidates <= 1.0) || ps.iter().any(|v| v.unsigned_abs() as f64 > candidates)
        }) {
            return Err("BYSETPOS is ineffective or exceeds the bounded period".into());
        }
        if freq != "MINUTELY" && freq != "HOURLY" {
            fields.entry("BYHOUR".into()).or_insert_with(|| start.hour().to_string());
            fields.entry("BYMINUTE".into()).or_insert_with(|| start.minute().to_string());
            fields.entry("BYSECOND".into()).or_insert_with(|| start.second().to_string());
            if byday == 0
                && !lists.contains_key("BYMONTHDAY")
                && !lists.contains_key("BYYEARDAY")
                && !lists.contains_key("BYWEEKNO")
            {
                if freq == "WEEKLY" {
                    fields.insert("BYDAY".into(), DAYS[(start.weekday().to_monday_one_offset() - 1) as usize].into());
                }
                if freq == "MONTHLY" || freq == "YEARLY" {
                    fields.insert("BYMONTHDAY".into(), start.day().to_string());
                }
                if freq == "YEARLY" {
                    fields.entry("BYMONTH".into()).or_insert_with(|| start.month().to_string());
                }
            }
        }
        for key in ["COUNT", "UNTIL", "INTERVAL"] {
            fields.remove(key);
        }
        Ok(Self { normalized: lines.join("\n"), start, start_ms, tz, freq, interval, count, until, wkst, fields })
    }
    fn index(&self, wall: i64) -> Result<i64, String> {
        let dt = instant::civil(wall, &TimeZone::UTC)?;
        Ok(match self.freq.as_str() {
            "MINUTELY" | "HOURLY" => wall,
            "DAILY" => wall.div_euclid(DAY_MS),
            "WEEKLY" => {
                wall.div_euclid(DAY_MS) - (i64::from(dt.weekday().to_monday_one_offset()) - 1 - self.wkst).rem_euclid(7)
            }
            "MONTHLY" => i64::from(dt.year()) * 12 + i64::from(dt.month()) - 1,
            _ => i64::from(dt.year()),
        })
    }
    fn stride(&self) -> i64 {
        self.interval.saturating_mul(match self.freq.as_str() {
            "MINUTELY" => 60_000,
            "HOURLY" => 3_600_000,
            "WEEKLY" => 7,
            _ => 1,
        })
    }
    fn bounds(&self, index: i64) -> Result<Option<(i64, i64)>, String> {
        let (start, end) = match self.freq.as_str() {
            "MINUTELY" => (index, index.saturating_add(60_000)),
            "HOURLY" => (index, index.saturating_add(3_600_000)),
            "DAILY" => (index.saturating_mul(DAY_MS), index.saturating_add(1).saturating_mul(DAY_MS)),
            "WEEKLY" => (index.saturating_mul(DAY_MS), index.saturating_add(7).saturating_mul(DAY_MS)),
            "MONTHLY" => {
                let y = index.div_euclid(12);
                if !(100..=9999).contains(&y) {
                    return Ok(None);
                }
                let start = instant::wall(
                    Date::new(y as i16, (index.rem_euclid(12) + 1) as i8, 1).map_err(|e| e.to_string())?.at(0, 0, 0, 0),
                )?;
                let end = if index == 9999 * 12 + 11 {
                    MAX_MS + 1
                } else {
                    instant::wall(
                        Date::new(((index + 1).div_euclid(12)) as i16, ((index + 1).rem_euclid(12) + 1) as i8, 1)
                            .map_err(|e| e.to_string())?
                            .at(0, 0, 0, 0),
                    )?
                };
                (start, end)
            }
            _ => {
                if !(100..=9999).contains(&index) {
                    return Ok(None);
                }
                (
                    instant::wall(Date::new(index as i16, 1, 1).map_err(|e| e.to_string())?.at(0, 0, 0, 0))?,
                    if index == 9999 {
                        MAX_MS + 1
                    } else {
                        instant::wall(Date::new((index + 1) as i16, 1, 1).map_err(|e| e.to_string())?.at(0, 0, 0, 0))?
                    },
                )
            }
        };
        if start > MAX_MS || end <= instant::wall(self.start)? {
            return Ok(None);
        }
        Ok(Some((start, end.min(MAX_MS + 1))))
    }
    fn candidates(&self, index: i64) -> Result<Vec<i64>, String> {
        let Some((start, end)) = self.bounds(index)? else {
            return Ok(Vec::new());
        };
        let original = instant::wall(self.start)?;
        if self.freq == "MINUTELY" || self.freq == "HOURLY" {
            let dt = instant::civil(start, &TimeZone::UTC)?;
            return Ok(instant::local(dt, &self.tz)?
                .into_iter()
                .filter(|ms| *ms >= self.start_ms && *ms <= self.until)
                .collect());
        }
        let anchor = instant::civil(start.max(original), &TimeZone::UTC)?;
        let until = instant::civil((end - 1000).max(start), &TimeZone::UTC)?;
        let parts = self.fields.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(";");
        let source =
            format!("DTSTART:{}Z\nRRULE:{};UNTIL={}Z", instant::basic_text(anchor), parts, instant::basic_text(until));
        let parsed: rrule::RRuleSet = source.parse().map_err(|e: rrule::RRuleError| e.to_string())?;
        let result = parsed.all((MAX_CANDIDATES + 1) as u16);
        if result.limited || result.dates.len() > MAX_CANDIDATES {
            return Err("RRULE bounded evaluator exhausted its period budget".into());
        }
        let mut out = BTreeSet::new();
        for dt in result.dates {
            let wall = dt.timestamp_millis();
            if wall < original || wall < start || wall >= end {
                continue;
            }
            if let Some(ms) = instant::local(instant::civil(wall, &TimeZone::UTC)?, &self.tz)?
                && ms >= self.start_ms
                && ms <= self.until
            {
                out.insert(ms);
            }
        }
        Ok(out.into_iter().collect())
    }
    pub fn nearest(&self, reference: i64, forward: bool) -> Result<Option<i64>, String> {
        if self.until < self.start_ms || (forward && reference >= self.until) || (!forward && reference < self.start_ms)
        {
            return Ok(None);
        }
        let origin = self.index(instant::wall(self.start)?)?;
        let stride = self.stride();
        if let Some(count) = self.count {
            let mut index = origin;
            let mut seen = BTreeSet::new();
            let mut latest = None;
            for _ in 0..MAX_PERIODS {
                if self.bounds(index)?.is_none() {
                    break;
                }
                for ms in self.candidates(index)? {
                    if !seen.insert(ms) {
                        continue;
                    }
                    if ms > reference {
                        return Ok(if forward { Some(ms) } else { latest });
                    }
                    latest = Some(ms);
                    if seen.len() >= count {
                        return Ok(if forward { None } else { latest });
                    }
                }
                index = match index.checked_add(stride) {
                    Some(v) => v,
                    None => break,
                };
            }
            return Ok(if forward { None } else { latest });
        }
        if !forward {
            return self.latest_at_or_before(reference, origin, stride);
        }
        let wall = instant::wall(instant::civil(reference.min(self.until).max(self.start_ms), &self.tz)?)?;
        let decision = self.index(wall.max(instant::wall(self.start)?))?;
        let mut index = origin + (decision - origin).div_euclid(stride) * stride;
        if index < origin {
            index = origin;
        }
        for _ in 0..MAX_PERIODS {
            if self.bounds(index)?.is_none() {
                break;
            }
            let ms = if forward {
                self.candidates(index)?.into_iter().find(|ms| *ms > reference)
            } else {
                self.candidates(index)?.into_iter().rev().find(|ms| *ms <= reference)
            };
            if ms.is_some() {
                return Ok(ms);
            }
            index = match if forward { index.checked_add(stride) } else { index.checked_sub(stride) } {
                Some(v) => v,
                None => break,
            };
            if index < origin {
                break;
            }
        }
        Ok(None)
    }
    /// The latest occurrence at or before `reference` for a rule without COUNT. Wall time is not
    /// monotone in a DST fold (after the clocks move back, the second 01:15 is later than the first
    /// 01:45), so the scan runs in instant space: it starts at the period of `reference + FOLD_SPAN`
    /// (no later period can hold an instant at or before the reference) and stops once a period's
    /// earliest candidate is more than FOLD_SPAN before the best found (no earlier period can beat it).
    fn latest_at_or_before(&self, reference: i64, origin: i64, stride: i64) -> Result<Option<i64>, String> {
        // Larger than any real UTC offset change (the largest folds are two hours).
        const FOLD_SPAN_MS: i64 = 3 * 3_600_000;
        let ahead = reference.saturating_add(FOLD_SPAN_MS).min(self.until).max(self.start_ms);
        let wall = instant::wall(instant::civil(ahead, &self.tz)?)?;
        let decision = self.index(wall.max(instant::wall(self.start)?))?;
        let mut index = (origin + (decision - origin).div_euclid(stride) * stride).max(origin);
        let mut best: Option<i64> = None;
        for _ in 0..MAX_PERIODS {
            if self.bounds(index)?.is_some() {
                let candidates = self.candidates(index)?;
                for ms in candidates.iter().copied().filter(|ms| *ms <= reference) {
                    best = Some(best.map_or(ms, |b| b.max(ms)));
                }
                if let (Some(b), Some(first)) = (best, candidates.first().copied())
                    && first.saturating_add(FOLD_SPAN_MS) <= b
                {
                    break;
                }
            }
            index = match index.checked_sub(stride) {
                Some(v) if v >= origin => v,
                _ => break,
            };
        }
        Ok(best)
    }
}
