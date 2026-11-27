//! Session query predicates over already-authorized observations, independent of consumers.
use crate::session_query::query_err;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionFilters {
    pub exclude_session_id: Option<String>,
    #[serde(default)]
    pub exclude_session_ids: Vec<String>,
    pub archived: Option<bool>,
    pub session_ids: Option<Vec<String>>,
    pub created_from_ms: Option<i64>,
    pub created_to_ms: Option<i64>,
    pub parent_session_ids: Option<Vec<String>>,
    pub include_root_sessions: Option<bool>,
    pub availability: Option<Vec<String>>,
}
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EventFilters {
    pub seq_from: Option<usize>,
    pub seq_to: Option<usize>,
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    pub event_types: Option<Vec<String>>,
    pub surfaces: Option<Vec<String>>,
}
fn time_range(from: Option<i64>, to: Option<i64>) -> mlua::Result<(Option<i64>, Option<i64>)> {
    if from.zip(to).is_some_and(|(f, t)| f > t) {
        return Err(query_err("QUERY_INVALID_FILTER: reversed time range"));
    }
    Ok((from, to))
}
fn in_time(value: Option<f64>, range: (Option<i64>, Option<i64>)) -> bool {
    if range == (None, None) {
        return true;
    }
    let Some(v) = value else {
        return false;
    };
    range.0.is_none_or(|f| v >= f as f64) && range.1.is_none_or(|t| v <= t as f64)
}
fn validate_list<T>(list: &Option<Vec<T>>) -> mlua::Result<()> {
    if list.as_ref().is_some_and(|a| a.is_empty() || a.len() > 100) {
        return Err(query_err("QUERY_INVALID_FILTER: list must contain 1..100 values"));
    }
    Ok(())
}
pub(super) fn archived(entries: &[Value]) -> bool {
    entries.iter().rev().filter(|e| e["type"] == "archive").find_map(|e| e["archived"].as_bool()).unwrap_or(false)
}
impl SessionFilters {
    pub fn validate(&self) -> mlua::Result<()> {
        if self.exclude_session_ids.len() > 100 {
            return Err(query_err("QUERY_INVALID_FILTER: too many exclusions"));
        }
        validate_list(&self.session_ids)?;
        validate_list(&self.parent_session_ids)?;
        validate_list(&self.availability)?;
        time_range(self.created_from_ms, self.created_to_ms)?;
        if self.availability.as_ref().is_some_and(|a| a.iter().any(|v| v != "live" && v != "persisted")) {
            return Err(query_err("QUERY_INVALID_FILTER: unknown source availability"));
        }
        Ok(())
    }
    pub fn accepts_with_availability(
        &self,
        entries: &[Value],
        authorized: &[Value],
        is_live: bool,
    ) -> mlua::Result<bool> {
        let h = &entries[0];
        let id = h["session_id"].as_str().unwrap_or_default();
        if self.archived.is_some_and(|wanted| archived(entries) != wanted) {
            return Ok(false);
        }
        if self.exclude_session_ids.iter().any(|excluded| excluded == id)
            || self.exclude_session_id.as_deref() == Some(id)
            || self.session_ids.as_ref().is_some_and(|ids| !ids.iter().any(|s| s == id))
        {
            return Ok(false);
        }
        if !in_time(h["created_at_ms"].as_f64(), time_range(self.created_from_ms, self.created_to_ms)?) {
            return Ok(false);
        }
        let kind = if is_live { "live" } else { "persisted" };
        if self.availability.as_ref().is_some_and(|a| !a.iter().any(|v| v == kind)) {
            return Ok(false);
        }
        if self.parent_session_ids.is_some() || self.include_root_sessions == Some(true) {
            let parent = h["parent_session_id"].as_str();
            let parent_match = parent.is_some_and(|p| {
                self.parent_session_ids.as_ref().is_some_and(|ids| ids.iter().any(|s| s == p))
                    && authorized.iter().any(|e| e["session_id"] == p)
            });
            if !parent_match && !(parent.is_none() && self.include_root_sessions == Some(true)) {
                return Ok(false);
            }
        }
        Ok(true)
    }
}
impl EventFilters {
    pub fn validate(&self) -> mlua::Result<()> {
        if self.seq_from.zip(self.seq_to).is_some_and(|(f, t)| f > t) {
            return Err(query_err("QUERY_INVALID_FILTER: reversed sequence range"));
        }
        time_range(self.from_ms, self.to_ms)?;
        validate_list(&self.event_types)?;
        validate_list(&self.surfaces)?;
        if let Some(surfaces) = &self.surfaces {
            for s in surfaces {
                if !matches!(s.as_str(), "current" | "shadowed" | "log_only") {
                    return Err(query_err("QUERY_INVALID_FILTER: unsupported event surface"));
                }
            }
        }
        Ok(())
    }
    pub fn accepts_with_surface(&self, e: &Value, seq: usize, surface: Option<&str>) -> mlua::Result<bool> {
        self.accepts_with_surface_and_type(e, seq, surface, None)
    }

    pub fn accepts_with_surface_and_type(
        &self,
        e: &Value,
        seq: usize,
        surface: Option<&str>,
        event_type: Option<&str>,
    ) -> mlua::Result<bool> {
        if self.seq_from.is_some_and(|f| seq < f) || self.seq_to.is_some_and(|t| seq > t) {
            return Ok(false);
        }
        if !in_time(e["created_at_ms"].as_f64(), time_range(self.from_ms, self.to_ms)?) {
            return Ok(false);
        }
        if let Some(allowed_surfaces) = &self.surfaces {
            let Some(surface) = surface else { return Ok(false) };
            if !allowed_surfaces.iter().any(|s| s == surface) {
                return Ok(false);
            }
        }
        let actual_type = event_type.or_else(|| e["type"].as_str());
        Ok(self
            .event_types
            .as_ref()
            .is_none_or(|types| actual_type.is_some_and(|t| types.iter().any(|kind| t == kind))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn ranges_are_inclusive_and_invalid_ranges_fail() {
        let filter: EventFilters = serde_json::from_value(json!({
            "seq_from": 3,
            "seq_to": 5,
            "from_ms": 1767225600000i64,
            "to_ms": 1767225600000i64,
            "event_types": ["message"],
        }))
        .unwrap();
        filter.validate().unwrap();
        let event = json!({"type":"message","created_at_ms":1767225600000i64});
        assert!(filter.accepts_with_surface(&event, 3, None).unwrap());
        assert!(filter.accepts_with_surface(&event, 5, None).unwrap());
        assert!(!filter.accepts_with_surface(&event, 2, None).unwrap());
        let bad: EventFilters = serde_json::from_value(json!({"seq_from":5,"seq_to":3})).unwrap();
        assert!(bad.validate().is_err());
        assert!(serde_json::from_value::<SessionFilters>(json!({"created_from_ms":"2026-01-01"})).is_err());
        assert!(serde_json::from_value::<SessionFilters>(json!({"created_at_from":"2026-01-01T00:00:00Z"})).is_err());
        assert!(serde_json::from_value::<SessionFilters>(json!({"exclude_id":"a"})).is_err());
    }
    #[test]
    fn invalid_filters_report_invalid_arguments() {
        let cases = [
            (
                "time range",
                SessionFilters {
                    created_from_ms: Some(1767312000000),
                    created_to_ms: Some(1767225600000),
                    ..Default::default()
                }
                .validate(),
            ),
            (
                "event time range",
                EventFilters { from_ms: Some(2000), to_ms: Some(1000), ..Default::default() }.validate(),
            ),
            ("empty list", SessionFilters { session_ids: Some(vec![]), ..Default::default() }.validate()),
            (
                "exclusions",
                SessionFilters { exclude_session_ids: vec!["id".into(); 101], ..Default::default() }.validate(),
            ),
            (
                "availability",
                SessionFilters { availability: Some(vec!["unknown".into()]), ..Default::default() }.validate(),
            ),
            ("sequence", EventFilters { seq_from: Some(2), seq_to: Some(1), ..Default::default() }.validate()),
            ("surface", EventFilters { surfaces: Some(vec!["unknown".into()]), ..Default::default() }.validate()),
        ];
        for (name, result) in cases {
            let error = result.expect_err(name);
            let coded = error
                .chain()
                .find_map(|cause| cause.downcast_ref::<mare_shared::errors::Coded>())
                .unwrap_or_else(|| panic!("{name}: missing coded error: {error}"));
            assert_eq!(coded.code, "invalid_arguments", "{name}: {error}");
            assert!(coded.message.starts_with("QUERY_INVALID_FILTER:"), "{name}: {error}");
        }
    }
    #[test]
    fn archive_filter_uses_latest_record_not_path_or_title() {
        let header = json!({"session_id":"one"});
        let mut entries = vec![header.clone(), json!({"type":"archive","archived":true})];
        let archived_filter = SessionFilters { archived: Some(true), ..Default::default() };
        assert!(archived_filter.accepts_with_availability(&entries, std::slice::from_ref(&header), false).unwrap());
        entries.push(json!({"type":"archive","archived":false}));
        assert!(!archived_filter.accepts_with_availability(&entries, std::slice::from_ref(&header), false).unwrap());
    }
    #[test]
    fn unknown_parent_never_authorizes_a_filter() {
        let filter: SessionFilters = serde_json::from_value(json!({"parent_session_ids":["hidden"]})).unwrap();
        let entries = vec![json!({"session_id":"child","parent_session_id":"hidden"})];
        assert!(!filter.accepts_with_availability(&entries, std::slice::from_ref(&entries[0]), false).unwrap());
        assert!(
            filter
                .accepts_with_availability(&entries, &[entries[0].clone(), json!({"session_id":"hidden"})], false)
                .unwrap()
        );
    }
}
