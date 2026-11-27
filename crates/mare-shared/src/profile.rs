//! Startup / plugin timing.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

type LuaSampleMap = std::collections::HashMap<(String, String, i32), u64>;

static START: OnceLock<Instant> = OnceLock::new();
static ENTRIES: Mutex<Vec<Entry>> = Mutex::new(Vec::new());
/// Whether detailed startup profiling (`--startuptime` / `--profile-startup`) is active.
static STARTUP_PROFILE_ENABLED: AtomicBool = AtomicBool::new(false);
/// Instruction interval for Lua-layer startup sampling (0 = disabled).
static LUA_SAMPLE_EVERY: AtomicU32 = AtomicU32::new(0);
static LUA_SAMPLES: Mutex<Option<LuaSampleMap>> = Mutex::new(None);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LuaSample {
    pub bucket: &'static str,
    pub source: String,
    pub name: String,
    pub line: i32,
    pub hits: u64,
}

/// Enable detailed per-plugin startup profiling (`--startuptime` / `--profile-startup`).
pub fn enable_startup_profile() {
    STARTUP_PROFILE_ENABLED.store(true, Ordering::Relaxed);
}

/// Whether detailed per-plugin startup profiling (`--startuptime` / `--profile-startup`) is active.
pub fn startup_profile_enabled() -> bool {
    STARTUP_PROFILE_ENABLED.load(Ordering::Relaxed)
}

/// Enable Lua-layer startup stack sampling at the given instruction interval.
pub fn enable_lua_sampling(every: u32) {
    LUA_SAMPLE_EVERY.store(every.max(1), Ordering::Relaxed);
}

/// Disable Lua-layer startup stack sampling.
pub fn disable_lua_sampling() {
    LUA_SAMPLE_EVERY.store(0, Ordering::Relaxed);
}

/// Current Lua-layer startup sampling instruction interval (0 when disabled).
pub fn lua_sample_every() -> u32 {
    LUA_SAMPLE_EVERY.load(Ordering::Relaxed)
}

/// Classify a Lua chunk `source` into `ck/` (`cordis`), `mk/` (`mare_kernel`), `plugin`, or `other`.
pub fn classify_lua_source(source: &str) -> &'static str {
    if source.contains("kernel/lua/cordis/") || source.contains("lua/cordis/") {
        "ck/"
    } else if source.contains("kernel/lua/mare_kernel/") || source.contains("lua/mare_kernel/") {
        "mk/"
    } else if source.contains("preset/plugins/")
        || source.starts_with("@builtin/")
        || source.contains("/plugins/")
        || source.contains("\\plugins\\")
    {
        "plugin"
    } else {
        "other"
    }
}

/// Record one Lua stack sample taken by the watchdog count hook under `--startuptime`.
pub fn record_lua_sample(source: &str, name: &str, line: i32) {
    let mut guard = LUA_SAMPLES.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(std::collections::HashMap::new);
    let key = (source.to_string(), name.to_string(), line);
    *map.entry(key).or_insert(0) += 1;
}

/// Return aggregated Lua stack samples sorted by hit count descending.
pub fn lua_samples() -> Vec<LuaSample> {
    let guard = LUA_SAMPLES.lock().unwrap_or_else(|e| e.into_inner());
    let Some(map) = guard.as_ref() else {
        return Vec::new();
    };
    let mut out: Vec<LuaSample> = map
        .iter()
        .map(|((source, name, line), &hits)| LuaSample {
            bucket: classify_lua_source(source),
            source: source.clone(),
            name: name.clone(),
            line: *line,
            hits,
        })
        .collect();
    out.sort_by(|a, b| b.hits.cmp(&a.hits).then_with(|| a.source.cmp(&b.source)).then_with(|| a.line.cmp(&b.line)));
    out
}

#[cfg(test)]
pub fn clear_lua_samples() {
    let mut guard = LUA_SAMPLES.lock().unwrap_or_else(|e| e.into_inner());
    *guard = None;
}

#[derive(Clone, Debug)]
pub struct Entry {
    /// time since process start
    pub at: Duration,
    /// how long the step took (zero for plain marks)
    pub took: Duration,
    pub label: String,
}

/// Call first thing in `main`.
pub fn start() {
    let _ = START.set(Instant::now());
}

pub fn since_start() -> Duration {
    START.get_or_init(Instant::now).elapsed()
}

/// Record a point in time ("first-frame", "ui/ready").
pub fn mark(label: &str) {
    record(label, Duration::ZERO);
}

/// Record a step with its duration ("plugin:agent", "init.lua").
pub fn record(label: &str, took: Duration) {
    let e = Entry { at: since_start(), took, label: label.to_string() };
    ENTRIES.lock().unwrap().push(e);
}

pub fn entries() -> Vec<Entry> {
    ENTRIES.lock().unwrap().clone()
}

/// Neovim `--startuptime`-style text (plus Lua sample summary rows when enabled).
pub fn render() -> String {
    let mut out = String::from("   clock    self  step\n");
    for e in entries() {
        let _ = writeln!(out, "{:8.3} {:7.3}  {}", e.at.as_secs_f64() * 1000.0, e.took.as_secs_f64() * 1000.0, e.label);
    }
    let samples = lua_samples();
    if !samples.is_empty() {
        let total: u64 = samples.iter().map(|s| s.hits).sum();
        let total_f = total.max(1) as f64;
        let mut buckets: std::collections::BTreeMap<&'static str, u64> = std::collections::BTreeMap::new();
        for s in &samples {
            *buckets.entry(s.bucket).or_insert(0) += s.hits;
        }
        for bucket in ["ck/", "mk/", "plugin", "other"] {
            let hits = buckets.get(bucket).copied().unwrap_or(0);
            let pct = (hits as f64) * 100.0 / total_f;
            let _ = writeln!(out, "{:8.3} {:7.3}  lua-sample-bucket {bucket} (hits={hits}/{total})", hits as f64, pct);
        }
        for s in samples.into_iter().take(40) {
            let pct = (s.hits as f64) * 100.0 / total_f;
            let _ = writeln!(
                out,
                "{:8.3} {:7.3}  lua-sample [{}] {}:{} {} (hits={})",
                s.hits as f64, pct, s.bucket, s.source, s.line, s.name, s.hits
            );
        }
    }
    out
}

/// Slow frames (over 16ms), newest last; a fixed ring of [`SLOW_FRAMES`].
/// Each entry: ms since start, frame time in ms, what the frame was doing.
pub const SLOW_FRAMES: usize = 128;
static SLOW: Mutex<std::collections::VecDeque<(u64, f64, String)>> = Mutex::new(std::collections::VecDeque::new());

pub fn slow_frame(took: Duration, what: String) {
    let mut q = SLOW.lock().unwrap_or_else(|e| e.into_inner());
    if q.len() == SLOW_FRAMES {
        q.pop_front();
    }
    q.push_back((since_start().as_millis() as u64, took.as_secs_f64() * 1000.0, what));
}

pub fn slow_frames() -> Vec<(u64, f64, String)> {
    SLOW.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect()
}

/// Slow Lua callbacks (over 1s), newest last; a fixed ring of [`SLOW_CALLBACKS`].
pub const SLOW_CALLBACKS: usize = 128;

#[derive(Clone, Debug)]
pub struct SlowCallback {
    pub at_ms: u64,
    pub ms: f64,
    pub plugin: String,
    pub event: String,
}

static SLOW_CBS: Mutex<std::collections::VecDeque<SlowCallback>> = Mutex::new(std::collections::VecDeque::new());

pub fn slow_callback(took: Duration, plugin: impl Into<String>, event: impl Into<String>) {
    let mut q = SLOW_CBS.lock().unwrap_or_else(|e| e.into_inner());
    if q.len() == SLOW_CALLBACKS {
        q.pop_front();
    }
    q.push_back(SlowCallback {
        at_ms: since_start().as_millis() as u64,
        ms: took.as_secs_f64() * 1000.0,
        plugin: plugin.into(),
        event: event.into(),
    });
}

pub fn slow_callbacks() -> Vec<SlowCallback> {
    SLOW_CBS.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect()
}

/// Default max rows returned by [`callback_timings`].
pub const DEFAULT_CALLBACK_TIMING_ROWS: usize = 100;

/// Maximum `(plugin, event)` entries retained in the callback timing table.
pub const MAX_CALLBACK_TIMING_ENTRIES: usize = DEFAULT_CALLBACK_TIMING_ROWS;

/// Aggregated callback timing for a `(plugin, event)` pair exceeding `slow_callback_ms`.
#[derive(Clone, Debug, PartialEq)]
pub struct CallbackTiming {
    pub plugin: String,
    pub event: String,
    pub count: u64,
    pub total_ms: f64,
    pub max_ms: f64,
}

#[derive(Clone, Copy, Debug, Default)]
struct CallbackAgg {
    count: u64,
    total_ms: f64,
    max_ms: f64,
    last_used: u64,
}

#[derive(Debug)]
struct CallbackTimingTable {
    cap: usize,
    seq: u64,
    entries: std::collections::HashMap<(String, String), CallbackAgg>,
}

impl CallbackTimingTable {
    fn new(cap: usize) -> Self {
        Self { cap, seq: 0, entries: std::collections::HashMap::new() }
    }

    fn record(&mut self, took: Duration, plugin: impl Into<String>, event: impl Into<String>) {
        if self.cap == 0 {
            return;
        }
        self.seq += 1;
        let seq = self.seq;
        let ms = took.as_secs_f64() * 1000.0;
        let key = (plugin.into(), event.into());
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.count += 1;
            entry.total_ms += ms;
            if ms > entry.max_ms {
                entry.max_ms = ms;
            }
            entry.last_used = seq;
            return;
        }

        if self.entries.len() >= self.cap
            && let Some(victim_key) = self.entries.iter().min_by_key(|(_, v)| v.last_used).map(|(k, _)| k.clone())
        {
            self.entries.remove(&victim_key);
        }

        self.entries.insert(key, CallbackAgg { count: 1, total_ms: ms, max_ms: ms, last_used: seq });
    }

    fn rows(&self, limit: usize) -> Vec<CallbackTiming> {
        if limit == 0 {
            return Vec::new();
        }
        let mut rows: Vec<CallbackTiming> = self
            .entries
            .iter()
            .map(|((plugin, event), agg)| CallbackTiming {
                plugin: plugin.clone(),
                event: event.clone(),
                count: agg.count,
                total_ms: agg.total_ms,
                max_ms: agg.max_ms,
            })
            .collect();
        rows.sort_by(|a, b| {
            b.total_ms
                .total_cmp(&a.total_ms)
                .then_with(|| b.max_ms.total_cmp(&a.max_ms))
                .then_with(|| a.plugin.cmp(&b.plugin))
                .then_with(|| a.event.cmp(&b.event))
        });
        rows.truncate(limit);
        rows
    }
}

static CALLBACK_TIMINGS: Mutex<Option<CallbackTimingTable>> = Mutex::new(None);

/// Record a Lua callback that exceeded `slow_callback_ms`, aggregated by `(plugin, event)`.
pub fn record_callback_timing(took: Duration, plugin: impl Into<String>, event: impl Into<String>) {
    let mut guard = CALLBACK_TIMINGS.lock().unwrap_or_else(|e| e.into_inner());
    let table = guard.get_or_insert_with(|| CallbackTimingTable::new(MAX_CALLBACK_TIMING_ENTRIES));
    table.record(took, plugin, event);
}

/// Return aggregated callback timings sorted by `total_ms` descending, capped to `limit` rows.
pub fn callback_timings(limit: usize) -> Vec<CallbackTiming> {
    if limit == 0 {
        return Vec::new();
    }
    let guard = CALLBACK_TIMINGS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(table) = guard.as_ref() else {
        return Vec::new();
    };
    table.rows(limit)
}

#[cfg(test)]
mod tests {
    static GLOBAL_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn slow_frame_ring_keeps_the_newest_128() {
        for i in 0..(super::SLOW_FRAMES + 5) {
            super::slow_frame(std::time::Duration::from_millis(20), format!("f{i}"));
        }
        let v = super::slow_frames();
        assert_eq!(v.len(), super::SLOW_FRAMES);
        assert_eq!(v.last().unwrap().2, format!("f{}", super::SLOW_FRAMES + 4));
    }

    #[test]
    fn slow_callback_ring_keeps_the_newest_128() {
        for i in 0..(super::SLOW_CALLBACKS + 5) {
            super::slow_callback(std::time::Duration::from_millis(1200), format!("p{i}"), format!("e{i}"));
        }
        let v = super::slow_callbacks();
        assert_eq!(v.len(), super::SLOW_CALLBACKS);
        let last = v.last().unwrap();
        assert_eq!(last.plugin, format!("p{}", super::SLOW_CALLBACKS + 4));
        assert_eq!(last.event, format!("e{}", super::SLOW_CALLBACKS + 4));
        assert!((last.ms - 1200.0).abs() < 1.0);
    }

    #[test]
    fn callback_timings_aggregates_sorts_and_caps_rows() {
        let _lock = GLOBAL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prefix = "agg_unit_test";
        // (p1, e1): 2 calls -> 10 + 30 = 40ms total, max 30ms
        super::record_callback_timing(std::time::Duration::from_millis(10), format!("{prefix}_p1"), "e1");
        super::record_callback_timing(std::time::Duration::from_millis(30), format!("{prefix}_p1"), "e1");
        // (p1, e2): 3 calls -> 20 + 20 + 20 = 60ms total, max 20ms
        super::record_callback_timing(std::time::Duration::from_millis(20), format!("{prefix}_p1"), "e2");
        super::record_callback_timing(std::time::Duration::from_millis(20), format!("{prefix}_p1"), "e2");
        super::record_callback_timing(std::time::Duration::from_millis(20), format!("{prefix}_p1"), "e2");
        // (p2, e1): 1 call -> 50ms total, max 50ms
        super::record_callback_timing(std::time::Duration::from_millis(50), format!("{prefix}_p2"), "e1");

        let all: Vec<_> = super::callback_timings(100).into_iter().filter(|r| r.plugin.starts_with(prefix)).collect();
        assert_eq!(all.len(), 3);
        // Sorted by total_ms descending: (p1, e2)=60, (p2, e1)=50, (p1, e1)=40
        assert_eq!((all[0].plugin.as_str(), all[0].event.as_str(), all[0].count), ("agg_unit_test_p1", "e2", 3));
        assert!((all[0].total_ms - 60.0).abs() < 1.0);
        assert!((all[0].max_ms - 20.0).abs() < 1.0);

        assert_eq!((all[1].plugin.as_str(), all[1].event.as_str(), all[1].count), ("agg_unit_test_p2", "e1", 1));
        assert!((all[1].total_ms - 50.0).abs() < 1.0);
        assert!((all[1].max_ms - 50.0).abs() < 1.0);

        assert_eq!((all[2].plugin.as_str(), all[2].event.as_str(), all[2].count), ("agg_unit_test_p1", "e1", 2));
        assert!((all[2].total_ms - 40.0).abs() < 1.0);
        assert!((all[2].max_ms - 30.0).abs() < 1.0);

        assert!(super::callback_timings(0).is_empty());
        assert_eq!(super::callback_timings(2).len(), 2);
    }

    #[test]
    fn callback_timing_table_bounds_size_and_evicts_lru() {
        let mut zero_cap = super::CallbackTimingTable::new(0);
        zero_cap.record(std::time::Duration::from_millis(10), "p0", "e0");
        assert!(zero_cap.entries.is_empty());

        let cap = 5;
        let mut asc = super::CallbackTimingTable::new(cap);
        for i in 1..=20u64 {
            asc.record(std::time::Duration::from_millis(i), format!("p{i}"), format!("e{i}"));
            assert!(asc.entries.len() <= cap);
        }
        assert_eq!(asc.entries.len(), cap);
        let top_asc: Vec<u64> = asc.rows(cap + 5).into_iter().map(|r| r.total_ms.round() as u64).collect();
        assert_eq!(top_asc, vec![20, 19, 18, 17, 16]);

        // Descending insertions also retain the most recently used entries, evicting older ones.
        let mut desc = super::CallbackTimingTable::new(cap);
        for i in (1..=20u64).rev() {
            desc.record(std::time::Duration::from_millis(i), format!("p{i}"), format!("e{i}"));
            assert!(desc.entries.len() <= cap);
        }
        assert_eq!(desc.entries.len(), cap);
        let top_desc: Vec<u64> = desc.rows(cap + 5).into_iter().map(|r| r.total_ms.round() as u64).collect();
        assert_eq!(top_desc, vec![5, 4, 3, 2, 1]);

        // Updating an existing key at capacity refreshes its recency and does not evict another entry.
        // Currently in `desc` (oldest -> newest): p5, p4, p3, p2, p1.
        desc.record(std::time::Duration::from_millis(5), "p5", "e5");
        assert_eq!(desc.entries.len(), cap);
        // Now p4 is the least-recently-used; inserting a new key should evict p4, keeping p5.
        desc.record(std::time::Duration::from_millis(2), "p_new", "e_new");
        assert_eq!(desc.entries.len(), cap);
        let after_update: Vec<(String, u64)> =
            desc.rows(cap).into_iter().map(|r| (r.plugin, r.total_ms.round() as u64)).collect();
        assert_eq!(
            after_update,
            vec![
                ("p5".to_string(), 10),
                ("p3".to_string(), 3),
                ("p2".to_string(), 2),
                ("p_new".to_string(), 2),
                ("p1".to_string(), 1),
            ]
        );
    }

    #[test]
    fn callback_timing_table_admits_and_tracks_new_high_frequency_key() {
        let cap = 4;
        let mut table = super::CallbackTimingTable::new(cap);

        // Fill table with old entries that each have a large total_ms.
        for i in 1..=cap {
            table.record(std::time::Duration::from_millis(500), format!("old_p{i}"), "tick");
        }
        assert_eq!(table.entries.len(), cap);

        // A new high-frequency key with small single-call duration (5ms <= 500ms) must enter
        // by evicting the least-recently-used old entry and stay tracked across many calls
        // even as other new keys arrive.
        for n in 0..20u64 {
            table.record(std::time::Duration::from_millis(5), "hot_plugin", "render");
            if n % 5 == 4 {
                table.record(std::time::Duration::from_millis(10), format!("other_{n}"), "idle");
            }
            assert!(table.entries.len() <= cap);
        }

        let rows = table.rows(cap);
        assert_eq!(rows.len(), cap);
        let hot =
            rows.iter().find(|r| r.plugin == "hot_plugin" && r.event == "render").expect("hot key must be tracked");
        assert_eq!(hot.count, 20);
        assert!((hot.total_ms - 100.0).abs() < 1.0);
        assert!((hot.max_ms - 5.0).abs() < 1.0);
    }

    #[test]
    fn record_callback_timing_caps_global_table_at_max_entries() {
        let _lock = GLOBAL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let total_keys = super::MAX_CALLBACK_TIMING_ENTRIES + 25;
        for i in 1..=total_keys {
            super::record_callback_timing(
                std::time::Duration::from_micros(i as u64),
                format!("cap_test_p{i}"),
                format!("e{i}"),
            );
            let guard = super::CALLBACK_TIMINGS.lock().unwrap_or_else(|e| e.into_inner());
            let len = guard.as_ref().map_or(0, |t| t.entries.len());
            assert!(len <= super::MAX_CALLBACK_TIMING_ENTRIES);
        }
        assert!(super::callback_timings(total_keys).len() <= super::MAX_CALLBACK_TIMING_ENTRIES);

        // render() formats header and entries; rows(0) returns empty; ties break by event name.
        let rendered = super::render();
        assert!(rendered.starts_with("   clock    self  step\n"), "{rendered}");
        let mut tied = super::CallbackTimingTable::new(4);
        assert!(tied.rows(0).is_empty());
        tied.record(std::time::Duration::from_millis(10), "same_plugin", "b_event");
        tied.record(std::time::Duration::from_millis(10), "same_plugin", "a_event");
        let events: Vec<String> = tied.rows(4).into_iter().map(|r| r.event).collect();
        assert_eq!(events, vec!["a_event", "b_event"]);
    }

    #[test]
    fn lua_sampling_classifies_buckets_and_renders_valid_startuptime_rows() {
        let _lock = GLOBAL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        super::clear_lua_samples();
        assert_eq!(super::classify_lua_source("@kernel/lua/cordis/fiber.lua"), "ck/");
        assert_eq!(super::classify_lua_source("@kernel/lua/mare_kernel/component.lua"), "mk/");
        assert_eq!(super::classify_lua_source("@builtin/plugins/session.lua"), "plugin");
        assert_eq!(super::classify_lua_source("=init.lua"), "other");

        super::record_lua_sample("@kernel/lua/cordis/trace.lua", "buildTraceable", 245);
        super::record_lua_sample("@kernel/lua/cordis/trace.lua", "buildTraceable", 245);
        super::record_lua_sample("@kernel/lua/mare_kernel/component.lua", "loadingChildren", 19);

        let rendered = super::render();
        let mut lines = rendered.lines();
        assert_eq!(lines.next().unwrap().split_whitespace().collect::<Vec<_>>(), vec!["clock", "self", "step"]);
        let mut saw_ck = false;
        let mut saw_mk = false;
        for line in lines {
            let mut cols = line.split_whitespace();
            let c1: f64 = cols.next().unwrap().parse().expect("first col must be float");
            let c2: f64 = cols.next().unwrap().parse().expect("second col must be float");
            assert!(c1.is_finite() && c2.is_finite());
            if line.contains("lua-sample [ck/] @kernel/lua/cordis/trace.lua:245 buildTraceable (hits=2)") {
                saw_ck = true;
            }
            if line.contains("lua-sample [mk/] @kernel/lua/mare_kernel/component.lua:19 loadingChildren (hits=1)") {
                saw_mk = true;
            }
        }
        assert!(saw_ck && saw_mk, "expected ck/ and mk/ sample lines in render:\n{rendered}");
        super::clear_lua_samples();
    }
}
