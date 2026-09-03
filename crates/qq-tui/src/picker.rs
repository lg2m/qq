//! Shared state for every filterable selection list: the slash-command
//! autocomplete, the model picker, and the session picker.
//!
//! A picker owns only the query text and a cursor into the *filtered* result
//! list. Callers compute the filtered list from their own data each time; the
//! picker clamps and moves the cursor against that list's length so it never
//! points past the end after a refresh.

use crate::app::terminal_safe_character;

/// Maximum bytes accepted into a picker search query.
pub(crate) const MAX_QUERY_BYTES: usize = 256;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Picker {
    pub query: String,
    selected: usize,
}

impl Picker {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Cursor into a filtered list of `len` entries. Clamps to the last entry
    /// so a shrinking list never leaves the cursor dangling.
    #[must_use]
    pub(crate) fn selected(&self, len: usize) -> usize {
        self.selected.min(len.saturating_sub(1))
    }

    pub(crate) fn select(&mut self, index: usize) {
        self.selected = index;
    }

    pub(crate) fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub(crate) fn move_down(&mut self, len: usize) {
        self.selected = (self.selected + 1).min(len.saturating_sub(1));
    }

    /// Append sanitized characters to the query, bounded by
    /// [`MAX_QUERY_BYTES`]. Resets the cursor because the filtered list
    /// changed shape. Returns whether the query changed.
    pub(crate) fn push_query(&mut self, text: &str) -> bool {
        let before = self.query.len();
        for character in text.chars() {
            if self.query.len() + character.len_utf8() > MAX_QUERY_BYTES {
                break;
            }
            if let Some(character) = terminal_safe_character(character) {
                self.query.push(character);
            }
        }
        self.selected = 0;
        self.query.len() != before
    }

    /// Remove the last query character and reset the cursor. Returns whether
    /// anything was removed.
    pub(crate) fn pop_query(&mut self) -> bool {
        let changed = self.query.pop().is_some();
        self.selected = 0;
        changed
    }

    /// Case-insensitive substring match used by every picker filter.
    #[must_use]
    pub(crate) fn matches(&self, candidates: impl IntoIterator<Item = impl AsRef<str>>) -> bool {
        if self.query.is_empty() {
            return true;
        }
        let query = self.query.to_ascii_lowercase();
        candidates
            .into_iter()
            .any(|candidate| candidate.as_ref().to_ascii_lowercase().contains(&query))
    }

    /// Keep the cursor on the same logical item after the underlying list was
    /// rebuilt. `before` is the identity under the old cursor; `after`
    /// enumerates identities in the new filtered order.
    pub(crate) fn preserve<T: PartialEq>(
        &mut self,
        before: Option<T>,
        after: impl IntoIterator<Item = T>,
    ) {
        let mut len = 0_usize;
        let mut found = None;
        for (index, identity) in after.into_iter().enumerate() {
            len += 1;
            if found.is_none() && before.as_ref() == Some(&identity) {
                found = Some(index);
            }
        }
        self.selected = found.unwrap_or(0).min(len.saturating_sub(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_clamps_to_the_filtered_length() {
        let mut picker = Picker::new();
        picker.move_down(5);
        picker.move_down(5);
        picker.move_down(5);
        assert_eq!(picker.selected(5), 3);
        assert_eq!(picker.selected(2), 1);
        assert_eq!(picker.selected(0), 0);
        picker.move_up();
        assert_eq!(picker.selected(5), 2);
    }

    #[test]
    fn query_edits_reset_the_cursor_and_respect_the_byte_bound() {
        let mut picker = Picker::new();
        picker.move_down(10);
        assert!(picker.push_query("gp"));
        assert_eq!(picker.selected(10), 0);
        assert_eq!(picker.query, "gp");
        assert!(!picker.push_query("\u{7}"));
        assert!(picker.pop_query());
        assert_eq!(picker.query, "g");
        let long = "x".repeat(MAX_QUERY_BYTES + 5);
        picker.push_query(&long);
        assert_eq!(picker.query.len(), MAX_QUERY_BYTES);
    }

    #[test]
    fn matches_is_case_insensitive_and_empty_query_matches_everything() {
        let mut picker = Picker::new();
        assert!(picker.matches(["anything"]));
        picker.push_query("GPT");
        assert!(picker.matches(["openai", "gpt-test"]));
        assert!(!picker.matches(["anthropic", "claude"]));
    }

    #[test]
    fn preserve_keeps_the_cursor_on_the_same_identity() {
        let mut picker = Picker::new();
        picker.select(2);
        picker.preserve(Some("c"), ["x", "c", "a"]);
        assert_eq!(picker.selected(3), 1);
        picker.preserve(Some("missing"), ["x", "c"]);
        assert_eq!(picker.selected(2), 0);
        picker.preserve(Some("x"), std::iter::empty::<&str>());
        assert_eq!(picker.selected(0), 0);
    }
}
