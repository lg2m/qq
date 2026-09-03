//! Input modes. Exactly one overlay may be open; it captures keys, paste, and
//! mouse input ahead of the composer and replaces the transcript body while
//! visible. The approval prompt is not an overlay: it is derived from session
//! data (`App::pending_approval`) and modelled as a mode only for dispatch.

use qq_protocol::SessionId;

use crate::picker::Picker;

/// A destructive session-picker action awaiting its inline y/n answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionConfirm {
    Delete(SessionId),
    Prune,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Overlay {
    Models(Picker),
    Sessions {
        picker: Picker,
        /// When set, only this session and its descendants are listed: the
        /// `/agents` view of one root's delegated work.
        scope: Option<SessionId>,
        /// Identity of the highlighted session. Kept as an id rather than an
        /// index because the tree reorders when sessions are created or
        /// deleted underneath the open picker.
        selected: Option<SessionId>,
        confirm: Option<SessionConfirm>,
    },
}

/// What currently owns keyboard input, from highest to lowest priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Models,
    Sessions,
    Approval,
    Compose,
}

impl Overlay {
    /// A session picker with the given query, highlight, and confirmation.
    #[cfg(test)]
    pub(crate) fn sessions(
        query: &str,
        selected: Option<SessionId>,
        confirm: Option<SessionConfirm>,
    ) -> Self {
        let mut picker = Picker::new();
        picker.push_query(query);
        Self::Sessions {
            picker,
            scope: None,
            selected,
            confirm,
        }
    }

    #[cfg(test)]
    pub(crate) fn models() -> Self {
        Self::Models(Picker::new())
    }

    #[cfg(test)]
    pub(crate) fn set_confirm(&mut self, next: Option<SessionConfirm>) {
        if let Self::Sessions { confirm, .. } = self {
            *confirm = next;
        }
    }

    #[must_use]
    pub(crate) fn mode(&self) -> Mode {
        match self {
            Self::Models(_) => Mode::Models,
            Self::Sessions { .. } => Mode::Sessions,
        }
    }

    /// The query and cursor shared by every overlay that filters a list.
    #[must_use]
    pub(crate) fn picker(&self) -> &Picker {
        match self {
            Self::Models(picker) | Self::Sessions { picker, .. } => picker,
        }
    }

    pub(crate) fn picker_mut(&mut self) -> &mut Picker {
        match self {
            Self::Models(picker) | Self::Sessions { picker, .. } => picker,
        }
    }
}
