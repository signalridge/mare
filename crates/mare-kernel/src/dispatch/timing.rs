//! Slow-callback accounting.

/// Default busy-time threshold above which a single Lua callback emits a `tracing::warn!`.
pub const SLOW_CALLBACK: std::time::Duration = std::time::Duration::from_secs(1);

/// Default busy-time threshold above which a Lua callback is aggregated into `/profile`'s
/// callback timing table (kept at 2ms so `/profile` stays fine-grained without spamming logs).
pub const AGGREGATE_SLOW_CALLBACK: std::time::Duration = std::time::Duration::from_millis(2);

/// Threshold above which a slow Lua callback is also recorded individually into `/profile` (MK3).
pub const PROFILE_SLOW_CALLBACK: std::time::Duration = std::time::Duration::from_secs(1);

/// Account for a Lua callback's `busy` CPU time on the loop thread (the sum of its polls, see
/// [`timed`]: a listener awaiting a process, a timer or the network isn't slow, it just waits).
///
/// - Callbacks over `aggregate_threshold` (`min(slow_callback_ms, 2ms)` at runtime) are aggregated
///   into `/profile`'s callback timing table.
/// - Callbacks over `warn_threshold` (`slow_callback_ms`, default 1s) emit a `tracing::warn!`.
/// - Callbacks over `profile_threshold` (`slow_callback_record_ms`, default 1s) are also recorded
///   individually into `/profile`.
pub(crate) fn slow_callback_with_all_thresholds(
    event: &str,
    plugin: &str,
    busy: std::time::Duration,
    aggregate_threshold: std::time::Duration,
    warn_threshold: std::time::Duration,
    profile_threshold: std::time::Duration,
) {
    if busy > aggregate_threshold {
        mare_shared::profile::record_callback_timing(busy, plugin, event);
    }
    if busy > profile_threshold {
        tracing::warn!(
            "slow callback: [{plugin}] {event} ran {:.1}ms (exceeded {}ms)",
            busy.as_secs_f64() * 1000.0,
            profile_threshold.as_millis()
        );
        mare_shared::profile::slow_callback(busy, plugin, event);
    } else if busy > warn_threshold {
        tracing::warn!(
            "slow callback: [{plugin}] {event} ran {:.1}ms (budget {}ms)",
            busy.as_secs_f64() * 1000.0,
            warn_threshold.as_millis()
        );
    }
}

#[cfg(test)]
pub(crate) fn slow_callback_with_thresholds(
    event: &str,
    plugin: &str,
    busy: std::time::Duration,
    slow_threshold: std::time::Duration,
    profile_threshold: std::time::Duration,
) {
    slow_callback_with_all_thresholds(event, plugin, busy, slow_threshold, slow_threshold, profile_threshold);
}

fn duration_ms_opt(cfg: &serde_json::Value, key: &str, default: std::time::Duration) -> std::time::Duration {
    match cfg.get(key).and_then(|v| v.as_f64()) {
        Some(ms) if ms.is_finite() && ms >= 0.0 => std::time::Duration::from_secs_f64(ms / 1000.0),
        _ => default,
    }
}

impl crate::Kernel {
    #[cfg(test)]
    pub(crate) fn callback_thresholds(&self) -> (std::time::Duration, std::time::Duration) {
        let cfg = self.0.config.lock().unwrap_or_else(|e| e.into_inner());
        (
            duration_ms_opt(&cfg, "slow_callback_ms", SLOW_CALLBACK),
            duration_ms_opt(&cfg, "slow_callback_record_ms", PROFILE_SLOW_CALLBACK),
        )
    }

    pub(crate) fn slow_callback_table_rows(&self) -> usize {
        let cfg = self.0.config.lock().unwrap_or_else(|e| e.into_inner());
        match cfg.get("slow_callback_table_rows").and_then(|v| v.as_u64()) {
            Some(n) => usize::try_from(n).unwrap_or(usize::MAX),
            None => mare_shared::profile::DEFAULT_CALLBACK_TIMING_ROWS,
        }
    }

    pub(crate) fn record_slow_callback(&self, event: &str, plugin: &str, busy: std::time::Duration) {
        let (slow_threshold, profile_threshold) = {
            let cfg = self.0.config.lock().unwrap_or_else(|e| e.into_inner());
            (
                duration_ms_opt(&cfg, "slow_callback_ms", SLOW_CALLBACK),
                duration_ms_opt(&cfg, "slow_callback_record_ms", PROFILE_SLOW_CALLBACK),
            )
        };
        let aggregate_threshold = slow_threshold.min(AGGREGATE_SLOW_CALLBACK);
        slow_callback_with_all_thresholds(event, plugin, busy, aggregate_threshold, slow_threshold, profile_threshold);
    }
}

/// Run `fut` and measure the time spent inside its polls (CPU time on the loop thread, awaits
/// excluded).
pub async fn timed<F: std::future::Future>(fut: F) -> (F::Output, std::time::Duration) {
    let mut fut = std::pin::pin!(fut);
    let mut busy = std::time::Duration::ZERO;
    let out = std::future::poll_fn(|cx| {
        let t0 = std::time::Instant::now();
        let r = fut.as_mut().poll(cx);
        busy += t0.elapsed();
        r
    })
    .await;
    (out, busy)
}

#[cfg(test)]
mod timing_tests {
    use super::*;

    /// the slow-callback budget counts the time a callback runs, not the time it waits.
    #[tokio::test]
    async fn timed_counts_polls_not_awaits() {
        // A wide separation tolerates a descheduled poll while still rejecting wall-clock timing.
        let (_, waited) = timed(tokio::time::sleep(std::time::Duration::from_secs(1))).await;
        assert!(waited < std::time::Duration::from_millis(750), "an await is not busy time: {waited:?}");
        let (_, spun) = timed(async {
            let t0 = std::time::Instant::now();
            while t0.elapsed() < std::time::Duration::from_millis(5) {}
        })
        .await;
        assert!(spun >= std::time::Duration::from_millis(5), "{spun:?}");
    }

    #[test]
    fn slow_callback_over_1s_is_recorded_in_profile_without_sleep() {
        let under_event = "timing_test/under_threshold";
        let over_event = "timing_test/over_threshold";
        slow_callback_with_thresholds(
            under_event,
            "timing-plugin",
            std::time::Duration::from_millis(999),
            SLOW_CALLBACK,
            PROFILE_SLOW_CALLBACK,
        );
        slow_callback_with_thresholds(
            over_event,
            "timing-plugin",
            std::time::Duration::from_millis(1250),
            SLOW_CALLBACK,
            PROFILE_SLOW_CALLBACK,
        );
        let recorded = mare_shared::profile::slow_callbacks();
        assert!(!recorded.iter().any(|c| c.event == under_event));
        let hit = recorded
            .iter()
            .find(|c| c.event == over_event && c.plugin == "timing-plugin")
            .expect("callback > 1s should be recorded in profile");
        assert!((hit.ms - 1250.0).abs() < 1.0, "unexpected ms: {}", hit.ms);
    }

    #[test]
    fn slow_callback_respects_custom_thresholds_for_aggregation_and_recording() {
        let plugin = "timing-custom-plugin";
        let slow_thresh = std::time::Duration::from_millis(10);
        let record_thresh = std::time::Duration::from_millis(50);

        // 5ms <= 10ms: neither aggregated nor recorded
        slow_callback_with_thresholds(
            "custom/under_slow",
            plugin,
            std::time::Duration::from_millis(5),
            slow_thresh,
            record_thresh,
        );
        // 25ms > 10ms, <= 50ms: aggregated, not individually recorded
        slow_callback_with_thresholds(
            "custom/over_slow",
            plugin,
            std::time::Duration::from_millis(25),
            slow_thresh,
            record_thresh,
        );
        // 75ms > 50ms: both aggregated and individually recorded
        slow_callback_with_thresholds(
            "custom/over_record",
            plugin,
            std::time::Duration::from_millis(75),
            slow_thresh,
            record_thresh,
        );

        let timings: Vec<_> =
            mare_shared::profile::callback_timings(100).into_iter().filter(|t| t.plugin == plugin).collect();
        assert_eq!(timings.len(), 2);
        assert_eq!(timings[0].event, "custom/over_record");
        assert_eq!(timings[1].event, "custom/over_slow");

        let recorded = mare_shared::profile::slow_callbacks();
        assert!(!recorded.iter().any(|c| c.plugin == plugin && c.event == "custom/over_slow"));
        assert!(recorded.iter().any(|c| c.plugin == plugin && c.event == "custom/over_record"));
    }

    #[test]
    fn kernel_reads_callback_thresholds_and_table_rows_from_config() {
        let lua = mlua::Lua::new();
        let k = crate::Kernel::new(lua, serde_json::json!({})).unwrap();
        assert_eq!(k.callback_thresholds(), (SLOW_CALLBACK, PROFILE_SLOW_CALLBACK));
        assert_eq!(k.slow_callback_table_rows(), mare_shared::profile::DEFAULT_CALLBACK_TIMING_ROWS);

        k.set_config(serde_json::json!({
            "slow_callback_ms": 5,
            "slow_callback_record_ms": 250,
            "slow_callback_table_rows": 42
        }));
        assert_eq!(
            k.callback_thresholds(),
            (std::time::Duration::from_millis(5), std::time::Duration::from_millis(250))
        );
        assert_eq!(k.slow_callback_table_rows(), 42);
    }

    #[test]
    fn record_slow_callback_aggregates_at_2ms_even_when_slow_callback_ms_is_explicitly_high() {
        let lua = mlua::Lua::new();
        let k = crate::Kernel::new(
            lua,
            serde_json::json!({
                "slow_callback_ms": 2000,
                "slow_callback_record_ms": 2000
            }),
        )
        .unwrap();
        let plugin = "timing-sep-plugin";
        // 1ms <= 2ms: not aggregated.
        k.record_slow_callback("timing_sep/under_2ms", plugin, std::time::Duration::from_millis(1));
        // 5ms > 2ms, <= 2000ms: aggregated into /profile timing table, not recorded as >2s slow callback.
        k.record_slow_callback("timing_sep/over_2ms", plugin, std::time::Duration::from_millis(5));

        let timings: Vec<_> =
            mare_shared::profile::callback_timings(200).into_iter().filter(|t| t.plugin == plugin).collect();
        assert_eq!(timings.len(), 1, "5ms callback must still be aggregated at 2ms threshold: {timings:?}");
        assert_eq!(timings[0].event, "timing_sep/over_2ms");

        let recorded = mare_shared::profile::slow_callbacks();
        assert!(!recorded.iter().any(|c| c.plugin == plugin), "5ms callback must not be recorded as slow");

        // Exercise both warn and profile-exceeded tracing arms with an active subscriber.
        struct AcceptAll;
        impl tracing::Subscriber for AcceptAll {
            fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn event(&self, _: &tracing::Event<'_>) {}
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }
        tracing::subscriber::with_default(AcceptAll, || {
            slow_callback_with_all_thresholds(
                "timing_warn/profile_exceeded",
                "timing-warn-plugin",
                std::time::Duration::from_millis(25),
                std::time::Duration::from_millis(2),
                std::time::Duration::from_millis(10),
                std::time::Duration::from_millis(20),
            );
            slow_callback_with_all_thresholds(
                "timing_warn/budget_exceeded",
                "timing-warn-plugin",
                std::time::Duration::from_millis(15),
                std::time::Duration::from_millis(2),
                std::time::Duration::from_millis(10),
                std::time::Duration::from_millis(50),
            );
        });
    }
}
