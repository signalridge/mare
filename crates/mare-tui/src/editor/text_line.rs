//! One buffer line, shared between the buffer and its undo / yank snapshots: cloning it is a
//! reference count, and the first write to a shared line copies just that line (copy on write).
//! A snapshot of an N-line buffer is N pointers, not N strings; reads see a plain `String`.

use std::borrow::Borrow;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_LINE_REV: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
pub(super) struct TextLine {
    text: Arc<String>,
    rev: u64,
}

impl Default for TextLine {
    fn default() -> Self {
        Self { text: Arc::default(), rev: NEXT_LINE_REV.fetch_add(1, Ordering::Relaxed) }
    }
}

impl PartialEq for TextLine {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.text, &other.text) || self.text == other.text
    }
}

impl Eq for TextLine {}

impl TextLine {
    pub(super) fn rev(&self) -> u64 {
        self.rev
    }
}

impl Deref for TextLine {
    type Target = String;
    fn deref(&self) -> &String {
        &self.text
    }
}

/// Every mutation goes through here: a line still held by a snapshot is copied first.
impl DerefMut for TextLine {
    fn deref_mut(&mut self) -> &mut String {
        self.rev = NEXT_LINE_REV.fetch_add(1, Ordering::Relaxed);
        Arc::make_mut(&mut self.text)
    }
}

impl From<String> for TextLine {
    fn from(s: String) -> Self {
        Self { text: Arc::new(s), rev: NEXT_LINE_REV.fetch_add(1, Ordering::Relaxed) }
    }
}

/// For `lines.join("\n")`.
impl Borrow<str> for TextLine {
    fn borrow(&self) -> &str {
        &self.text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_line_equality_covers_shared_and_distinct_allocations() {
        let a = TextLine::from("hello".to_string());
        let shared = a.clone();
        assert_eq!(a, shared);
        let distinct_same = TextLine::from("hello".to_string());
        assert_eq!(a, distinct_same);
        let mut mutated = shared;
        mutated.push_str("!!");
        assert_ne!(a, mutated);
        assert_ne!(a.rev(), mutated.rev());
    }
}
