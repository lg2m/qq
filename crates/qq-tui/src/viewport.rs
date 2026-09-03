//! What the main area shows and how far it is scrolled.
//!
//! There is one body on screen: a session transcript or a workspace-wide
//! view. Scroll state is one [`Viewport`], reconciled by the renderer each
//! frame and handed back to the app after composition so building a frame
//! never mutates the model.

use qq_protocol::SessionId;

/// What the main area shows. A transcript follows one session; the other
/// kinds are workspace-wide views that read every session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum View {
    /// One session's transcript, or the empty prompt when `None`.
    Transcript(Option<SessionId>),
    /// Every approval, failure, and unread finish across the workspace.
    Attention,
    /// Every file edited by any agent, grouped by path.
    Changes,
}

impl Default for View {
    fn default() -> Self {
        Self::Transcript(None)
    }
}

impl View {
    /// The session this view follows, if it shows one.
    pub(crate) const fn session(self) -> Option<SessionId> {
        match self {
            Self::Transcript(session) => session,
            Self::Attention | Self::Changes => None,
        }
    }
}

/// Scroll state of the body. `offset` counts rows above the live tail; zero
/// follows the tail. `body_rows` and `height` describe the last frame so
/// scroll commands can clamp without re-laying the body.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Viewport {
    context: Option<View>,
    body_rows: usize,
    height: usize,
    offset: usize,
}

impl Viewport {
    /// Reconcile with the body laid out this frame. A change of view returns
    /// to the live tail; otherwise a scrolled viewport keeps its top row
    /// stable as rows are appended, unless the body asked to preserve the
    /// tail anchor because a live message settled in place.
    pub(crate) fn update(
        &mut self,
        context: View,
        body_rows: usize,
        height: usize,
        preserve_tail_anchor: bool,
    ) {
        if self.context != Some(context) {
            *self = Self {
                context: Some(context),
                body_rows,
                height,
                offset: 0,
            };
            return;
        }
        if self.offset > 0 && self.height > 0 && !preserve_tail_anchor {
            let top = self
                .body_rows
                .saturating_sub(self.offset)
                .saturating_sub(self.height);
            self.offset = body_rows.saturating_sub(top.saturating_add(height));
        }
        self.body_rows = body_rows;
        self.height = height;
        self.offset = self.offset.min(body_rows.saturating_sub(height));
    }

    pub(crate) const fn offset(&self) -> usize {
        self.offset
    }

    pub(crate) const fn height(&self) -> usize {
        self.height
    }

    /// Whether `rows` of the body were visible or below the visible window
    /// on the last frame, meaning a change there should keep the tail anchor.
    pub(crate) fn intersects_or_follows(&self, rows: &std::ops::Range<usize>) -> bool {
        self.height > 0 && self.body_rows.saturating_sub(self.offset) > rows.start
    }

    /// Scroll by `rows`: positive moves toward older content, negative toward
    /// the live tail. Returns whether the offset changed.
    pub(crate) fn scroll(&mut self, rows: isize) -> bool {
        let before = self.offset;
        let maximum = self.body_rows.saturating_sub(self.height);
        self.offset = match rows.cmp(&0) {
            std::cmp::Ordering::Greater => before.saturating_add(rows.unsigned_abs()).min(maximum),
            std::cmp::Ordering::Less => before.saturating_sub(rows.unsigned_abs()),
            std::cmp::Ordering::Equal => before,
        };
        self.offset != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(byte: u8) -> SessionId {
        SessionId::from_bytes([byte; 16])
    }

    #[test]
    fn viewport_follows_the_tail_until_scrolled_and_keeps_its_top_row() {
        let mut viewport = Viewport::default();
        let context = View::Transcript(Some(session(1)));
        viewport.update(context, 100, 10, false);
        assert_eq!(viewport.offset(), 0);
        assert!(viewport.scroll(5));
        assert_eq!(viewport.offset(), 5);
        // Ten rows appended: the same top row stays visible.
        viewport.update(context, 110, 10, false);
        assert_eq!(viewport.offset(), 15);
        // A settled live message asks to keep the tail anchor instead.
        viewport.update(context, 120, 10, true);
        assert_eq!(viewport.offset(), 15);
        assert!(viewport.scroll(-100));
        assert_eq!(viewport.offset(), 0);
        assert!(viewport.scroll(isize::MAX));
        assert_eq!(viewport.offset(), 110);
        assert!(!viewport.scroll(0));
        // Switching view returns to the tail.
        viewport.update(View::Transcript(Some(session(2))), 50, 10, false);
        assert_eq!(viewport.offset(), 0);
        viewport.update(View::Attention, 50, 10, false);
        assert_eq!(viewport.offset(), 0);
    }
}
