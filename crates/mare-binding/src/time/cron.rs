//! Five-field Vixie Cron. Date enumeration is bounded by one Gregorian cycle.
use super::instant;
use jiff::{ToSpan, civil::Date};
use std::collections::BTreeSet;
#[derive(Clone, Debug)]
struct Field {
    values: BTreeSet<i16>,
    star: bool,
}
#[derive(Clone, Debug)]
pub struct Cron {
    fields: Vec<Field>,
    canonical: String,
}
fn number(value: &str, min: i16, max: i16) -> Result<i16, String> {
    if value.is_empty() || !value.bytes().all(|c| c.is_ascii_digit()) {
        return Err("Cron fields accept numeric atoms only".into());
    }
    let parsed = value.parse::<i16>().map_err(|e| e.to_string())?;
    if parsed < min || parsed > max {
        return Err(format!("Cron value must be in {min}..={max}"));
    }
    Ok(parsed)
}
// Preserve both the matched set and the Vixie leading-star flag.
fn encode(values: &[i16]) -> String {
    if values.len() == 1 {
        return values[0].to_string();
    }
    let first = values[0];
    let last = values[values.len() - 1];
    let step = values[1] - first;
    if values.windows(2).all(|v| v[1] - v[0] == step) {
        return if step == 1 { format!("{first}-{last}") } else { format!("{first}-{last}/{step}") };
    }
    let mut parts = Vec::new();
    let mut begin = first;
    let mut previous = first;
    for &value in &values[1..] {
        if value != previous + 1 {
            parts.push(if begin == previous { begin.to_string() } else { format!("{begin}-{previous}") });
            begin = value;
        }
        previous = value;
    }
    parts.push(if begin == previous { begin.to_string() } else { format!("{begin}-{previous}") });
    parts.join(",")
}
fn canonical_field(values: &BTreeSet<i16>, star: bool, min: i16, max: i16) -> String {
    let ordered = values.iter().copied().collect::<Vec<_>>();
    if !star {
        return encode(&ordered);
    }
    let canonical_max = if max == 7 { 6 } else { max };
    if values.len() == (canonical_max - min + 1) as usize {
        return "*".into();
    }
    let mut best_step = 1;
    let mut best = BTreeSet::new();
    for step in 1..=(max - min + 1) {
        let walk =
            (min..=max).step_by(step as usize).map(|n| if max == 7 && n == 7 { 0 } else { n }).collect::<BTreeSet<_>>();
        if walk.len() > best.len() && walk.is_subset(values) {
            best_step = step;
            best = walk;
        }
    }
    let remaining = values.difference(&best).copied().collect::<Vec<_>>();
    if remaining.is_empty() { format!("*/{best_step}") } else { format!("*/{best_step},{}", encode(&remaining)) }
}
impl Cron {
    pub fn parse(expression: &str) -> Result<Self, String> {
        let raw: Vec<_> = expression.split_whitespace().collect();
        if raw.len() != 5 {
            return Err("Cron requires exactly five numeric fields".into());
        }
        let mut fields = Vec::new();
        let mut encoded = Vec::new();
        for (text, (min, max)) in raw.into_iter().zip([(0, 59), (0, 23), (1, 31), (1, 12), (0, 7)]) {
            let mut values = BTreeSet::new();
            for atom in text.split(',') {
                let pieces: Vec<_> = atom.split('/').collect();
                if pieces.len() > 2 {
                    return Err("Invalid Cron step".into());
                }
                let base = pieces[0];
                let step = if pieces.len() == 2 {
                    let value = pieces[1];
                    if value.is_empty() || !value.bytes().all(|c| c.is_ascii_digit()) {
                        return Err("Cron step must be a positive safe integer".into());
                    }
                    let n = value.parse::<u64>().map_err(|e| e.to_string())?;
                    if n == 0 || n > 9_007_199_254_740_991 {
                        return Err("Cron step must be a positive safe integer".into());
                    }
                    n.min(32767) as i16
                } else {
                    1
                };
                let (first, last) = if base == "*" {
                    (min, max)
                } else if let Some((a, b)) = base.split_once('-') {
                    (number(a, min, max)?, number(b, min, max)?)
                } else {
                    if pieces.len() == 2 {
                        return Err("Cron value/step is not supported; use */step or range/step".into());
                    }
                    let n = number(base, min, max)?;
                    (n, n)
                };
                if first > last {
                    return Err("Cron ranges must be ascending".into());
                }
                for n in (first..=last).step_by(step as usize) {
                    values.insert(if max == 7 && n == 7 { 0 } else { n });
                }
            }
            let star = text.starts_with('*');
            encoded.push(canonical_field(&values, star, min, max));
            fields.push(Field { values, star });
        }
        Ok(Self { fields, canonical: encoded.join(" ") })
    }
    pub fn normalized(&self) -> String {
        self.canonical.clone()
    }
    fn matches(&self, date: Date) -> bool {
        if !self.fields[3].values.contains(&i16::from(date.month())) {
            return false;
        }
        let dom = self.fields[2].values.contains(&i16::from(date.day()));
        let dow = self.fields[4].values.contains(&(i16::from(date.weekday().to_monday_one_offset()) % 7));
        if self.fields[2].star || self.fields[4].star { dom && dow } else { dom || dow }
    }
    pub fn nearest(&self, reference: i64, timezone: &str, forward: bool) -> Result<Option<i64>, String> {
        let tz = instant::zone(timezone)?;
        let mut date = instant::civil(reference, &tz)?.date();
        for _ in 0..=146_097 {
            if self.matches(date) {
                let mut candidates = Vec::new();
                for &hour in &self.fields[1].values {
                    for &minute in &self.fields[0].values {
                        if let Some(ms) = instant::local(date.at(hour as i8, minute as i8, 0, 0), &tz)?
                            && ((forward && ms > reference) || (!forward && ms <= reference))
                        {
                            candidates.push(ms);
                        }
                    }
                }
                if let Some(ms) = if forward { candidates.into_iter().min() } else { candidates.into_iter().max() } {
                    return Ok(Some(ms));
                }
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
}
