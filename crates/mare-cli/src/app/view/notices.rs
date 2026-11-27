//! Notice cards: queueing, merging, icons.

use crate::app::*;

/// How long after startup the first-launch hint's card takes in the next notice.
const STARTUP_MERGE: Duration = Duration::from_secs(3);
const NOTICE_LIFE_MIN: Duration = Duration::from_secs(3);
const NOTICE_LIFE_MAX: Duration = Duration::from_secs(8);
const NOTICE_BASE_SUCCESS: Duration = Duration::from_secs(3);
const NOTICE_BASE_ERROR: Duration = Duration::from_secs(7);
const NOTICE_BASE_DEFAULT: Duration = Duration::from_secs(6);
const NOTICE_EXTRA_SEC: Duration = Duration::from_secs(1);
const NOTICE_CHARS_PER_EXTRA_SEC: usize = 120;

struct Notice {
    entry: (String, String, Instant),
    id: Option<String>,
    life: Duration,
    paused_at: Option<Instant>,
}

impl Notice {
    fn set_paused(&mut self, paused: bool, now: Instant) {
        match (paused, self.paused_at) {
            (true, None) => self.paused_at = Some(now),
            (false, Some(since)) => {
                self.entry.2 += now.saturating_duration_since(since);
                self.paused_at = None;
            }
            _ => {}
        }
    }

    fn age(&self, now: Instant) -> Duration {
        self.paused_at.unwrap_or(now).saturating_duration_since(self.entry.2)
    }
}

fn notice_life(text: &str, level: &str) -> Duration {
    let base = match level {
        "success" => NOTICE_BASE_SUCCESS,
        "error" => NOTICE_BASE_ERROR,
        _ => NOTICE_BASE_DEFAULT,
    };
    let extra = u32::try_from(text.chars().count() / NOTICE_CHARS_PER_EXTRA_SEC).unwrap_or(u32::MAX);
    base.saturating_add(NOTICE_EXTRA_SEC.saturating_mul(extra)).clamp(NOTICE_LIFE_MIN, NOTICE_LIFE_MAX)
}

/// Notification cards and the first-run card's merge state (written only here).
pub(in crate::app) struct Notices {
    /// notification cards (newest last), auto-dismissed
    cards: Vec<Notice>,
    /// the first-launch terminal hint (Windows), while its card can still take in the next
    /// startup notice: one first-run card, not a stack that pushes the conversation up
    startup_card: Option<String>,
    /// the first notice of the startup (the first-run card) and when the app started
    first_card: Option<String>,
    started_at: Instant,
}

impl Notices {
    /// Windows: one-time hint about the console host / Windows Terminal keys
    pub(in crate::app) fn new(startup_hint: Option<String>) -> Self {
        let now = mare_tui::render::clock_now();
        Self {
            cards: startup_hint
                .iter()
                .map(|h| Notice {
                    entry: (h.clone(), "warning".to_string(), now),
                    id: None,
                    life: notice_life(h, "warning"),
                    paused_at: None,
                })
                .collect(),
            startup_card: startup_hint.clone(),
            first_card: startup_hint,
            started_at: now,
        }
    }

    /// A notice that disappears after `life`, e.g. the exit hint
    /// that lasts as long as its 2 s window.
    pub(in crate::app) fn push_for_with_id(&mut self, text: &str, level: &str, life: Duration, id: &str) {
        self.push_inner((text.into(), level.into(), app_now()), Some(id.to_string()), Some(life));
    }

    pub(in crate::app) fn remove_by_id(&mut self, id: &str) {
        self.cards.retain(|card| card.id.as_deref() != Some(id));
    }

    pub(in crate::app) fn push(&mut self, n: (String, String, Instant)) {
        self.push_inner(n, None, None);
    }

    pub(in crate::app) fn push_with_id(&mut self, n: (String, String, Instant), id: Option<String>) {
        self.push_inner(n, id, None);
    }

    fn push_inner(&mut self, n: (String, String, Instant), id: Option<String>, life: Option<Duration>) {
        let mut n = n;
        // the first notice of the startup (the permissions intro) joins the first-launch
        // terminal hint's card instead of stacking a second one
        if id.is_none()
            && let Some(hint) = self.startup_card.take()
            && n.1 != "error"
            && let Some(pos) =
                self.cards.iter().position(|card| card.entry.0 == hint && card.entry.2.elapsed() < STARTUP_MERGE)
        {
            let (_, level, _) = self.cards.remove(pos).entry;
            n.0 = format!("{}\n{hint}", n.0);
            if level == "warning" {
                n.1 = level;
            }
            self.first_card = Some(n.0.clone());
        } else if self.started_at.elapsed() < STARTUP_MERGE && self.first_card.is_none() && n.1 != "error" {
            self.first_card = Some(n.0.clone());
        }
        let life = life.unwrap_or_else(|| notice_life(&n.0, &n.1));
        if let Some(pos) = id.as_ref().and_then(|id| self.cards.iter().position(|card| card.id.as_ref() == Some(id))) {
            let paused_at = self.cards[pos].paused_at.map(|_| n.2);
            self.cards[pos] = Notice { entry: n, id, life, paused_at };
            return;
        }
        self.cards.push(Notice { entry: n, id, life, paused_at: None });
        if self.cards.len() > 4 {
            self.cards.remove(0);
        }
    }

    /// Advance each card's active clock before drawing the next frame.
    pub(in crate::app) fn prepare(&mut self, now: Instant, blocked: bool) {
        self.set_visible_count(if blocked { 0 } else { self.cards.len() }, now);
        self.cards.retain(|card| card.age(now) < card.life);
    }

    /// Pause cards that cannot be displayed without resetting their elapsed time.
    pub(in crate::app) fn defer(&mut self, now: Instant) {
        self.set_visible_count(0, now);
    }

    /// The newest `count` cards fit; older cards wait for room.
    pub(in crate::app) fn set_visible_count(&mut self, count: usize, now: Instant) {
        let hidden = self.cards.len().saturating_sub(count);
        for (index, card) in self.cards.iter_mut().enumerate() {
            card.set_paused(index < hidden, now);
        }
    }

    pub(in crate::app) fn is_empty(&self) -> bool {
        self.cards.is_empty()
    }

    /// The cards, oldest first.
    pub(in crate::app) fn iter(&self) -> impl DoubleEndedIterator<Item = &(String, String, Instant)> {
        self.cards.iter().map(|card| &card.entry)
    }

    pub(in crate::app) fn oldest_age(&self, now: Instant) -> Option<Duration> {
        self.cards.iter().map(|card| card.age(now)).max()
    }

    /// `text` is the first-run card's.
    pub(in crate::app) fn is_first_card(&self, text: &str) -> bool {
        self.first_card.as_deref() == Some(text)
    }

    pub(in crate::app) fn clear(&mut self) {
        self.cards.clear();
    }
}

/// Notification icon for the theme's icon set (`mare.o.icons`).
pub(super) fn icon_for(theme: &Theme, kind: &str) -> &'static str {
    match (theme.icons.as_str(), kind) {
        ("nerd", "error") => "\u{f057}",
        ("nerd", "warn") => "\u{f071}",
        ("nerd", _) => "\u{f05a}",
        ("ascii", "error") => "x",
        ("ascii", "warn") => "!",
        ("ascii", _) => "i",
        (_, "error") => "✖",
        (_, "warn") => "⚠",
        _ => "ℹ",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferring_notices_preserves_each_cards_elapsed_visible_time() {
        let now = Instant::now();
        let mut notices = Notices::new(None);
        notices.push(("older".into(), "info".into(), now - Duration::from_secs(5)));
        notices.push(("newer".into(), "info".into(), now - Duration::from_secs(2)));
        notices.defer(now);
        let ages: Vec<Duration> = notices.iter().map(|(_, _, at)| now.duration_since(*at)).collect();
        assert_eq!(ages, [Duration::from_secs(5), Duration::from_secs(2)]);

        let resumed = now + Duration::from_secs(30);
        notices.prepare(resumed, false);
        assert_eq!(notices.cards.iter().map(|card| card.age(resumed)).collect::<Vec<_>>(), ages);
        notices.set_visible_count(1, resumed);
        let later = resumed + Duration::from_secs(1);
        notices.prepare(later, false);
        assert_eq!(
            notices.cards.iter().map(|card| card.age(later)).collect::<Vec<_>>(),
            [Duration::from_secs(5), Duration::from_secs(3)]
        );
        notices.prepare(later + Duration::from_secs(1), false);
        assert_eq!(notices.iter().map(|(text, _, _)| text.as_str()).collect::<Vec<_>>(), ["newer"]);
    }
}
