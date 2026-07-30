//! Cursor-aware prompt editing primitives.
//!
//! A bounded `String` is preferable to a rope for the composer's small (64 KiB)
//! documents. It has excellent cache locality and keeps the common append/edit
//! path allocation-free while capacity remains. Storage is hidden here so it
//! can still be replaced later without changing key handling.

#[derive(Debug, Default)]
pub(crate) struct Composer {
    pub(crate) text: String,
    /// UTF-8 byte offset. `None` means the end, which also lets tests and state
    /// restoration replace `text` directly without leaving a stale cursor.
    cursor: Option<usize>,
    /// Character column retained while moving through lines of unequal length.
    preferred_column: Option<usize>,
}

impl Composer {
    pub(crate) fn cursor(&self) -> usize {
        self.cursor.unwrap_or(self.text.len()).min(self.text.len())
    }

    pub(crate) fn clear(&mut self) {
        self.text.clear();
        self.cursor = None;
        self.preferred_column = None;
    }

    pub(crate) fn replace(&mut self, text: String) {
        self.text = text;
        self.cursor = None;
        self.preferred_column = None;
    }

    pub(crate) fn insert(&mut self, character: char) {
        let cursor = self.cursor();
        self.text.insert(cursor, character);
        self.cursor = Some(cursor + character.len_utf8());
        self.preferred_column = None;
    }

    pub(crate) fn backspace(&mut self) -> bool {
        let cursor = self.cursor();
        let Some((previous, _)) = self.text[..cursor].char_indices().next_back() else {
            return false;
        };
        self.text.drain(previous..cursor);
        self.cursor = Some(previous);
        self.preferred_column = None;
        true
    }

    pub(crate) fn delete(&mut self) -> bool {
        let cursor = self.cursor();
        let Some(character) = self.text[cursor..].chars().next() else {
            return false;
        };
        self.text.drain(cursor..cursor + character.len_utf8());
        self.cursor = Some(cursor);
        self.preferred_column = None;
        true
    }

    pub(crate) fn move_left(&mut self) -> bool {
        let cursor = self.cursor();
        let Some((previous, _)) = self.text[..cursor].char_indices().next_back() else {
            return false;
        };
        self.cursor = Some(previous);
        self.preferred_column = None;
        true
    }

    pub(crate) fn move_right(&mut self) -> bool {
        let cursor = self.cursor();
        let Some(character) = self.text[cursor..].chars().next() else {
            return false;
        };
        self.cursor = Some(cursor + character.len_utf8());
        self.preferred_column = None;
        true
    }

    /// Moves between logical newline-delimited lines. Returning false at a
    /// boundary lets the caller use that arrow for prompt history.
    pub(crate) fn move_up(&mut self) -> bool {
        self.move_vertical(false)
    }

    pub(crate) fn move_down(&mut self) -> bool {
        self.move_vertical(true)
    }

    fn move_vertical(&mut self, down: bool) -> bool {
        let cursor = self.cursor();
        let line_start = self.text[..cursor].rfind('\n').map_or(0, |index| index + 1);
        let column = self.text[line_start..cursor].chars().count();
        let preferred = self.preferred_column.unwrap_or(column);

        let (target_start, target_end) = if down {
            let Some(newline) = self.text[cursor..].find('\n').map(|index| cursor + index) else {
                return false;
            };
            let start = newline + 1;
            let end = self.text[start..]
                .find('\n')
                .map_or(self.text.len(), |index| start + index);
            (start, end)
        } else {
            if line_start == 0 {
                return false;
            }
            let end = line_start - 1;
            let start = self.text[..end].rfind('\n').map_or(0, |index| index + 1);
            (start, end)
        };

        let offset = self.text[target_start..target_end]
            .char_indices()
            .nth(preferred)
            .map_or(target_end - target_start, |(offset, _)| offset);
        self.cursor = Some(target_start + offset);
        self.preferred_column = Some(preferred);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::Composer;

    #[test]
    fn edits_at_unicode_character_boundaries() {
        let mut composer = Composer::default();
        composer.replace("aé".to_owned());
        assert!(composer.move_left());
        composer.insert('!');
        assert_eq!(composer.text, "a!é");
        assert!(composer.backspace());
        assert_eq!(composer.text, "aé");
        assert!(composer.delete());
        assert_eq!(composer.text, "a");
    }

    #[test]
    fn moves_vertically_and_retains_the_preferred_column() {
        let mut composer = Composer::default();
        composer.replace("abcd\nx\nwxyz".to_owned());
        assert!(composer.move_up());
        assert_eq!(composer.cursor(), 6);
        assert!(composer.move_up());
        assert_eq!(composer.cursor(), 4);
        assert!(!composer.move_up());
        assert!(composer.move_down());
        assert_eq!(composer.cursor(), 6);
        assert!(composer.move_down());
        assert_eq!(composer.cursor(), 11);
    }
}
