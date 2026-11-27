use super::query;
use mlua::LuaSerdeExt;
use serde_json::{Value, json};
fn ms(value: &str) -> i64 {
    super::instant::parse(value).unwrap()
}
fn next(pattern: Value, at: &str) -> Option<i64> {
    query(json!({"action":"next","pattern":pattern,"after_ms":ms(at)})).unwrap()["next_ms"].as_i64()
}
#[test]
fn calendar_dst_skips_gaps_and_chooses_earlier_fold() {
    let rule = json!({"kind":"calendar","time":"02:30","timezone":"America/New_York"});
    assert_eq!(next(rule, "2026-03-08T00:00:00Z"), Some(ms("2026-03-09T06:30:00Z")));
    let fold = json!({"kind":"calendar","time":"01:30","timezone":"America/New_York"});
    assert_eq!(next(fold, "2026-11-01T00:00:00Z"), Some(ms("2026-11-01T05:30:00Z")));
    assert!(
        query(json!({"action":"instant","date":"2026-03-08","time":"02:30","timezone":"America/New_York"})).is_err()
    );
}
#[test]
fn intervals_catch_up_from_anchor_without_completion_drift() {
    let pattern = json!({"kind":"interval","anchor_ms":ms("2026-01-01T00:00:00Z"),"every_ms":60000});
    let actual=query(json!({"action":"resolve","pattern":pattern,"scheduled_ms":ms("2026-01-01T00:01:00Z"),"accepted_ms":ms("2026-01-01T00:04:15Z")})).unwrap();
    assert_eq!(actual["occurrence_ms"], ms("2026-01-01T00:04:00Z"));
    assert_eq!(actual["next_ms"], ms("2026-01-01T00:05:00Z"));
}
#[test]
fn cron_dialect_and_vixie_day_semantics() {
    let pattern = json!({"kind":"cron","expression":"0 9 1 * 1","timezone":"UTC"});
    assert_eq!(next(pattern, "2026-01-02T00:00:00Z"), Some(ms("2026-01-05T09:00:00Z")));
    for expression in ["@daily", "0 0 * * * *", "1/2 * * * *", "0 0 * JAN MON", "0 25 * * *"] {
        assert!(query(json!({"action":"normalize","pattern":{"kind":"cron","expression":expression,"timezone":"UTC"},"after_ms":ms("2026-01-01T00:00:00Z")})).is_err(),"{expression}");
    }
    let impossible = json!({"kind":"cron","expression":"0 9 30 2 *","timezone":"UTC"});
    assert_eq!(next(impossible, "2026-01-01T00:00:00Z"), None);
}
#[test]
fn rrule_count_applies_to_actual_occurrences_after_gap_skip() {
    let pattern =
        json!({"kind":"rrule","rrule":"DTSTART;TZID=America/New_York:20260307T023000\nRRULE:FREQ=DAILY;COUNT=3"});
    assert_eq!(next(pattern.clone(), "2026-03-07T08:00:00Z"), Some(ms("2026-03-09T06:30:00Z")));
    assert_eq!(next(pattern.clone(), "2026-03-09T07:00:00Z"), Some(ms("2026-03-10T06:30:00Z")));
    assert_eq!(next(pattern, "2026-03-10T07:00:00Z"), None);
}
#[test]
fn rrule_calendar_selectors_and_large_intervals() {
    let pattern = json!({"kind":"rrule","rrule":"DTSTART:20260101T090000Z\nRRULE:FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1;COUNT=2"});
    assert_eq!(next(pattern.clone(), "2026-01-01T00:00:00Z"), Some(ms("2026-01-30T09:00:00Z")));
    assert_eq!(next(pattern, "2026-01-31T00:00:00Z"), Some(ms("2026-02-27T09:00:00Z")));
    let large = json!({"kind":"rrule","rrule":"DTSTART:20260101T090000Z\nRRULE:FREQ=MINUTELY;INTERVAL=100000;COUNT=2"});
    assert_eq!(next(large, "2026-01-01T09:00:00Z"), Some(ms("2026-03-11T19:40:00Z")));
}
#[test]
fn recurrence_rejects_invalid_combinations_and_density_before_enumeration() {
    for rule in [
        "FREQ=SECONDLY",
        "FREQ=DAILY;COUNT=0",
        "FREQ=DAILY;COUNT=1001",
        "FREQ=DAILY;COUNT=2;UNTIL=20270101T000000Z",
        "FREQ=DAILY;BYMONTHDAY=2",
        "FREQ=MINUTELY;BYHOUR=1",
        "FREQ=MONTHLY;BYDAY=0MO",
        "FREQ=YEARLY;BYHOUR=0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23;BYMINUTE=0,1",
    ] {
        let result = query(
            json!({"action":"normalize","pattern":{"kind":"rrule","rrule":format!("DTSTART:20260101T090000Z\nRRULE:{rule}")},"after_ms":ms("2026-01-01T00:00:00Z")}),
        );
        assert!(result.is_err(), "{rule}: {result:?}");
    }
}

#[test]
fn public_lua_query_preserves_null_and_rejects_non_json_values() {
    let lua = mlua::Lua::new();
    let mare = lua.create_table().unwrap();
    super::install(&lua, &mare).unwrap();
    lua.globals().set("mare", mare).unwrap();
    lua.globals().set("JSON_NULL", lua.null()).unwrap();
    lua.load(
        r#"
        local r,e=mare.time.query({action='next',pattern={kind='instant',at_ms=1000},after_ms=1000})
        assert(r and not e and r.action=='next' and r.next_ms == JSON_NULL)
        local bad,err=mare.time.query({action='instant',value='2026-01-01T00:00:00Z',extra=function() end})
        assert(not bad and type(err)=='string')
        local v=mare.time.query({action='instant',value='2026-01-01T00:00:00Z'})
        assert(v.iso=='2026-01-01T00:00:00.000Z')
    "#,
    )
    .exec()
    .unwrap();
}
#[test]
fn safe_integer_interval_extremes_exhaust_without_overflow_or_unbounded_scan() {
    for frequency in ["MINUTELY", "HOURLY", "DAILY", "WEEKLY", "MONTHLY", "YEARLY"] {
        let pattern = json!({"kind":"rrule","rrule":format!("DTSTART:20260101T090000Z\nRRULE:FREQ={frequency};INTERVAL=9007199254740991;COUNT=2")});
        assert_eq!(next(pattern, "2026-01-01T09:00:00Z"), None, "{frequency}");
    }
    let until = json!({"kind":"rrule","rrule":"DTSTART:20260101T090000Z\nRRULE:FREQ=DAILY;UNTIL=20260102T090000Z"});
    assert_eq!(next(until, "2026-01-03T00:00:00Z"), None);
}
#[test]
fn integral_lua_clock_numbers_and_rpc_numbers_share_validation() {
    let value = query(
        json!({"action":"next","pattern":{"kind":"interval","anchor_ms":0.0,"every_ms":60000.0},"after_ms":120000.0}),
    )
    .unwrap();
    assert_eq!(value["next_ms"], 180000);
    assert!(
        query(json!({"action":"next","pattern":{"kind":"interval","anchor_ms":0,"every_ms":60000},"after_ms":1.25}))
            .is_err()
    );
    let lua = mlua::Lua::new();
    let mare = lua.create_table().unwrap();
    super::install(&lua, &mare).unwrap();
    lua.globals().set("mare", mare).unwrap();
    lua.load("local r,e=mare.time.query({action='next',pattern={kind='interval',anchor_ms=0.0,every_ms=60000.0},after_ms=120000.0}); assert(r,e);assert(r.next_ms==180000)").exec().unwrap();
}

#[test]
fn canonical_cron_keeps_star_matching_and_folds_sunday_alias() {
    let raw = json!({"kind":"cron","expression":"0 9 */2,2 * 7","timezone":"UTC"});
    let normalized = query(json!({"action":"normalize","pattern":raw,"after_ms":ms("2026-01-01T00:00:00Z")})).unwrap();
    assert_eq!(normalized["pattern"]["expression"], "0 9 */2,2 * 0");
    assert_eq!(next(normalized["pattern"].clone(), "2026-01-01T00:00:00Z"), Some(ms("2026-01-11T09:00:00Z")));
}

#[test]
fn authoring_rejects_ineffective_positions_but_stored_rule_reads_remain_valid() {
    let pattern = json!({"kind":"rrule","rrule":"DTSTART:20260101T090000Z\nRRULE:FREQ=DAILY;BYHOUR=9;BYSETPOS=1"});
    assert!(query(json!({"action":"normalize","pattern":pattern,"after_ms":ms("2026-01-01T00:00:00Z")})).is_err());
    assert_eq!(next(pattern, "2026-01-01T00:00:00Z"), Some(ms("2026-01-01T09:00:00Z")));
}

#[test]
fn host_sample_preserves_one_instant_and_identifies_local_source() {
    let result = query(json!({"action":"sample","instant_ms":0})).unwrap();
    assert_eq!(result["instant_ms"], 0);
    let stamp = result["timestamp"].as_str().unwrap();
    assert_eq!(super::instant::parse(stamp).unwrap(), 0);
    assert_eq!(&stamp[..10], result["local_date"].as_str().unwrap());
    assert!(result["offset"].is_string());
    for value in [json!(0.5), json!("0"), json!(9_007_199_254_740_992_i64)] {
        assert!(query(json!({"action":"sample","instant_ms":value})).is_err());
    }
    assert!(query(json!({"action":"sample","unknown":true})).is_err());
}
// Resolves a repeated wall time to its earlier/later instant: after the clocks move back, the
// second 01:15 is later than the first 01:45.
#[test]
fn rrule_resolve_in_a_fold_takes_the_latest_instant_not_the_latest_wall_time() {
    let resolve = |pattern: &Value, scheduled: &str, accepted: &str| {
        query(json!({"action":"resolve","pattern":pattern,"scheduled_ms":ms(scheduled),"accepted_ms":ms(accepted)}))
            .unwrap()
    };
    let daily = json!({"kind":"rrule","rrule":"DTSTART;TZID=America/New_York:20261101T013000\nRRULE:FREQ=DAILY"});
    assert_eq!(next(daily.clone(), "2026-11-01T00:00:00Z"), Some(ms("2026-11-01T05:30:00Z")));
    let r = resolve(&daily, "2026-11-01T05:30:00Z", "2026-11-01T06:15:00Z");
    assert_eq!(r["occurrence_ms"].as_i64(), Some(ms("2026-11-01T05:30:00Z")));
    assert_eq!(r["next_ms"].as_i64(), Some(ms("2026-11-02T06:30:00Z")));
    let quarter = json!({"kind":"rrule",
        "rrule":"DTSTART;TZID=America/New_York:20261101T000000\nRRULE:FREQ=MINUTELY;INTERVAL=15"});
    assert_eq!(next(quarter.clone(), "2026-11-01T06:15:00Z"), Some(ms("2026-11-01T07:00:00Z")));
    for saved in ["2026-11-01T05:00:00Z", "2026-11-01T05:30:00Z", "2026-11-01T05:45:00Z"] {
        let r = resolve(&quarter, saved, "2026-11-01T06:15:00Z");
        assert_eq!(r["occurrence_ms"].as_i64(), Some(ms("2026-11-01T05:45:00Z")), "saved {saved}: {r}");
        assert_eq!(r["next_ms"].as_i64(), Some(ms("2026-11-01T07:00:00Z")), "saved {saved}: {r}");
    }
}

#[test]
fn format_sample_instant_and_pattern_validation_edges() {
    // Sample without instant_ms uses host clock
    let now_sample = query(json!({"action": "sample"})).unwrap();
    assert!(now_sample["instant_ms"].is_i64());

    // Instant: valid local time, plus conflict/missing/sub-ms/fold errors
    let ok_local =
        query(json!({"action": "instant", "date": "2026-01-15", "time": "09:30:00", "timezone": "America/New_York"}))
            .unwrap();
    assert_eq!(ok_local["instant_ms"], ms("2026-01-15T14:30:00Z"));
    for bad_instant in [
        json!({"action": "instant", "value": "2026-01-01T00:00:00Z", "date": "2026-01-01"}),
        json!({"action": "instant", "time": "09:00", "timezone": "UTC"}),
        json!({"action": "instant", "date": "2026-01-01", "timezone": "UTC"}),
        json!({"action": "instant", "date": "2026-01-01", "time": "09:00"}),
        json!({"action": "instant", "date": "2026-01-01", "time": "09:00:00.1234", "timezone": "UTC"}),
        json!({"action": "instant", "date": "2026-11-01", "time": "01:30", "timezone": "America/New_York"}),
    ] {
        assert!(query(bad_instant.clone()).is_err(), "{bad_instant}");
    }

    // Format: default format, custom format, and invalid format/tz
    let fmt_def = query(json!({"action": "format", "instant_ms": 0})).unwrap();
    assert_eq!(fmt_def["timezone"], "UTC");
    assert!(fmt_def["text"].as_str().unwrap().starts_with("1970-01-01 00:00:00"));
    let fmt_custom =
        query(json!({"action": "format", "instant_ms": 0, "timezone": "Asia/Tokyo", "format": "%Y-%m-%d %H:%M"}))
            .unwrap();
    assert_eq!(fmt_custom["text"], "1970-01-01 09:00");
    assert!(query(json!({"action": "format", "instant_ms": 0, "format": "%"})).is_err());
    assert!(query(json!({"action": "format", "instant_ms": 0, "timezone": "Mars/Olympus"})).is_err());

    // Pattern validation errors
    for bad_pat in [
        json!({"kind": "instant", "at_ms": 9_007_199_254_740_991_i64}),
        json!({"kind": "interval", "anchor_ms": 0, "every_ms": 0}),
        json!({"kind": "interval", "anchor_ms": 0, "every_ms": -5}),
        json!({"kind": "calendar", "time": "09:00", "timezone": "No/SuchZone"}),
        json!({"kind": "calendar", "time": "25:00", "timezone": "UTC"}),
        json!({"kind": "calendar", "time": "09:00:00.0001", "timezone": "UTC"}),
        json!({"kind": "calendar", "time": "09:00", "timezone": "UTC", "weekdays": []}),
        json!({"kind": "calendar", "time": "09:00", "timezone": "UTC", "weekdays": [2, 2]}),
        json!({"kind": "calendar", "time": "09:00", "timezone": "UTC", "weekdays": [5, 2]}),
        json!({"kind": "cron", "expression": "* * * * *", "timezone": "No/SuchZone"}),
    ] {
        assert!(query(json!({"action": "normalize", "pattern": bad_pat, "after_ms": 0})).is_err(), "{bad_pat}");
    }

    // Resolve & nearest edge cases: instant, interval before anchor, calendar weekdays, year 9999
    let inst_pat = json!({"kind": "instant", "at_ms": ms("2026-01-02T00:00:00Z")});
    let r_inst = query(json!({
        "action": "resolve",
        "pattern": inst_pat,
        "scheduled_ms": ms("2026-01-02T00:00:00Z"),
        "accepted_ms": ms("2026-01-02T01:00:00Z"),
    }))
    .unwrap();
    assert_eq!(r_inst["occurrence_ms"], ms("2026-01-02T00:00:00Z"));
    assert!(r_inst["next_ms"].is_null());

    // accepted_ms < scheduled_ms
    assert!(
        query(json!({
            "action": "resolve",
            "pattern": inst_pat,
            "scheduled_ms": ms("2026-01-02T00:00:00Z"),
            "accepted_ms": ms("2026-01-01T23:00:00Z"),
        }))
        .is_err()
    );

    // interval before anchor: next returns anchor, resolve fails with no due occurrence
    let iv_pat = json!({"kind": "interval", "anchor_ms": ms("2026-01-05T00:00:00Z"), "every_ms": 60_000});
    assert_eq!(next(iv_pat.clone(), "2026-01-01T00:00:00Z"), Some(ms("2026-01-05T00:00:00Z")));
    assert!(
        query(json!({
            "action": "resolve",
            "pattern": iv_pat,
            "scheduled_ms": ms("2026-01-01T00:00:00Z"),
            "accepted_ms": ms("2026-01-02T00:00:00Z"),
        }))
        .is_err()
    );

    // Calendar with weekdays (2026-01-01 is Thursday=4; Mon=1, Fri=5)
    let cal_pat = json!({"kind": "calendar", "time": "09:00:00", "timezone": "UTC", "weekdays": [1, 5]});
    assert_eq!(next(cal_pat.clone(), "2026-01-01T10:00:00Z"), Some(ms("2026-01-02T09:00:00Z")));
    let r_cal = query(json!({
        "action": "resolve",
        "pattern": cal_pat,
        "scheduled_ms": ms("2026-01-02T09:00:00Z"),
        "accepted_ms": ms("2026-01-04T12:00:00Z"),
    }))
    .unwrap();
    assert_eq!(r_cal["occurrence_ms"], ms("2026-01-02T09:00:00Z"));
    assert_eq!(r_cal["next_ms"], ms("2026-01-05T09:00:00Z"));

    // Year 9999 boundary returns None (9999-12-30 is Thu, 9999-12-31 is Fri; Mon=1 is in year 10000)
    let mon_pat = json!({"kind": "calendar", "time": "09:00:00", "timezone": "UTC", "weekdays": [1]});
    assert_eq!(next(mon_pat, "9999-12-30T10:00:00Z"), None);
}

#[test]
fn cron_canonicalization_error_paths_and_backward_resolve() {
    for bad in [
        "1/2/3 * * * *",
        "*/ * * * *",
        "*/a * * * *",
        "*/0 * * * *",
        "*/9007199254740992 * * * *",
        "10-5 * * * *",
        "60 * * * *",
        "0 0 0 * *",
        "-1 * * * *",
    ] {
        assert!(super::cron::Cron::parse(bad).is_err(), "expected error for {bad}");
    }

    for (raw, expected) in [
        ("0-59 * * * *", "0-59 * * * *"),
        ("0-10/2 * * * *", "0-10/2 * * * *"),
        ("1,3-5,8 * * * *", "1,3-5,8 * * * *"),
        ("*/1 * * * *", "* * * * *"),
        ("*/15 * * * *", "*/15 * * * *"),
        ("*/50000 * * * *", "*/60 * * * *"),
    ] {
        let parsed = super::cron::Cron::parse(raw).unwrap();
        assert_eq!(parsed.normalized(), expected, "raw={raw}");
    }

    // Backward resolve with cron (both same-day and multi-day) + Vixie dom||dow when neither is '*'
    let cron_pat = json!({"kind": "cron", "expression": "0 9 15 * 1", "timezone": "UTC"});
    let r = query(json!({
        "action": "resolve",
        "pattern": cron_pat,
        "scheduled_ms": ms("2026-01-12T09:00:00Z"),
        "accepted_ms": ms("2026-01-14T10:00:00Z"),
    }))
    .unwrap();
    // 2026-01-12 is Monday (dow=1), 2026-01-15 is Thursday (dom=15)
    assert_eq!(r["occurrence_ms"], ms("2026-01-12T09:00:00Z"));
    assert_eq!(r["next_ms"], ms("2026-01-15T09:00:00Z"));

    // Year 9999 boundary returns None (within jiff MAX_MS = 253402207200s)
    assert_eq!(next(cron_pat, "9999-12-30T21:00:00Z"), None);
}
