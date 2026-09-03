//! Tiling pane layout: a binary tree of splits whose leaves each show one
//! session. Panes have stable identities so per-pane render state survives
//! any tree mutation, and the tree stores no geometry; `layout` computes
//! tiles for a rectangle each frame so a resize never touches the tree.
//!
//! This module knows nothing about sessions beyond their id and nothing
//! about rendering beyond rectangles. `App` decides what a pane shows;
//! `view` decides what it looks like.

use std::collections::HashMap;

use qq_protocol::SessionId;

use crate::settings::Layout;

/// Panes narrower or shorter than this are hidden rather than rendered
/// unreadably; the side of a split that holds focus wins the space.
pub(crate) const MIN_PANE_WIDTH: usize = 24;
pub(crate) const MIN_PANE_HEIGHT: usize = 4;
/// Resize steps move a split by this fraction of its axis per command.
const RESIZE_STEP: f32 = 0.05;
const MIN_RATIO: f32 = 0.15;
const MAX_RATIO: f32 = 0.85;
/// Bound on the tree so one runaway keybinding cannot exhaust the terminal.
pub(crate) const MAX_PANES: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub(crate) struct PaneId(u32);

/// Which way a split divides its rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Axis {
    /// Children sit side by side; the divider is a vertical line.
    Columns,
    /// Children stack top to bottom; the divider is a horizontal line.
    Rows,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    const fn axis(self) -> Axis {
        match self {
            Self::Left | Self::Right => Axis::Columns,
            Self::Up | Self::Down => Axis::Rows,
        }
    }

    /// Whether moving the divider this way gives the first child more room.
    const fn grows_first(self) -> bool {
        matches!(self, Self::Right | Self::Down)
    }
}

/// A rectangle in frame coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Rect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

impl Rect {
    pub(crate) const fn new(x: usize, y: usize, width: usize, height: usize) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub(crate) const fn right(self) -> usize {
        self.x + self.width
    }

    pub(crate) const fn bottom(self) -> usize {
        self.y + self.height
    }

    pub(crate) const fn contains(self, x: usize, y: usize) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }

    const fn fits(self) -> bool {
        self.width >= MIN_PANE_WIDTH && self.height >= MIN_PANE_HEIGHT
    }
}

/// Scroll state of one pane's transcript. `offset` counts rows above the
/// live tail; zero follows the tail. `body_rows` and `height` describe the
/// last frame so scroll commands can clamp without re-laying the body.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Viewport {
    context: Option<(Option<SessionId>, Layout)>,
    body_rows: usize,
    height: usize,
    offset: usize,
}

impl Viewport {
    /// Reconcile with the body laid out this frame. A change of session or
    /// layout returns to the live tail; otherwise a scrolled viewport keeps
    /// its top row stable as rows are appended, unless the body asked to
    /// preserve the tail anchor because a live message settled in place.
    pub(crate) fn update(
        &mut self,
        context: (Option<SessionId>, Layout),
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

    pub(crate) fn scroll_up(&mut self, rows: usize) -> bool {
        let before = self.offset;
        let maximum = self.body_rows.saturating_sub(self.height);
        self.offset = before.saturating_add(rows).min(maximum);
        self.offset != before
    }

    pub(crate) fn scroll_down(&mut self, rows: usize) -> bool {
        let before = self.offset;
        self.offset = before.saturating_sub(rows);
        self.offset != before
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Pane {
    pub session: Option<SessionId>,
    pub viewport: Viewport,
}

#[derive(Debug, Clone, PartialEq)]
enum Node {
    Leaf(PaneId),
    Split {
        axis: Axis,
        /// Share of the axis given to `first`, in (0, 1).
        ratio: f32,
        first: Box<Node>,
        second: Box<Node>,
    },
}

impl Node {
    fn contains(&self, id: PaneId) -> bool {
        match self {
            Self::Leaf(leaf) => *leaf == id,
            Self::Split { first, second, .. } => first.contains(id) || second.contains(id),
        }
    }

    fn leaves(&self, out: &mut Vec<PaneId>) {
        match self {
            Self::Leaf(id) => out.push(*id),
            Self::Split { first, second, .. } => {
                first.leaves(out);
                second.leaves(out);
            }
        }
    }

    /// Replace the leaf `target` with a split of it and `fresh`.
    fn split(&mut self, target: PaneId, axis: Axis, fresh: PaneId) -> bool {
        match self {
            Self::Leaf(id) if *id == target => {
                *self = Self::Split {
                    axis,
                    ratio: 0.5,
                    first: Box::new(Self::Leaf(target)),
                    second: Box::new(Self::Leaf(fresh)),
                };
                true
            }
            Self::Leaf(_) => false,
            Self::Split { first, second, .. } => {
                first.split(target, axis, fresh) || second.split(target, axis, fresh)
            }
        }
    }

    /// Remove the leaf `target`, collapsing its parent split onto the
    /// sibling. Returns the sibling subtree's first leaf when removed, so the
    /// caller can move focus somewhere adjacent.
    fn remove(&mut self, target: PaneId) -> Option<PaneId> {
        let Self::Split { first, second, .. } = self else {
            return None;
        };
        let survivor = if **first == Self::Leaf(target) {
            Some(std::mem::replace(&mut **second, Self::Leaf(target)))
        } else if **second == Self::Leaf(target) {
            Some(std::mem::replace(&mut **first, Self::Leaf(target)))
        } else {
            None
        };
        match survivor {
            Some(survivor) => {
                let mut leaves = Vec::new();
                survivor.leaves(&mut leaves);
                *self = survivor;
                leaves.first().copied()
            }
            None => first.remove(target).or_else(|| second.remove(target)),
        }
    }

    /// Adjust the ratio of the innermost split along `axis` that contains
    /// `target`. Returns whether any ratio changed.
    fn resize(&mut self, target: PaneId, axis: Axis, delta: f32) -> bool {
        let Self::Split {
            axis: own_axis,
            ratio,
            first,
            second,
        } = self
        else {
            return false;
        };
        let in_first = first.contains(target);
        if !in_first && !second.contains(target) {
            return false;
        }
        let child = if in_first { first } else { second };
        if child.resize(target, axis, delta) {
            return true;
        }
        if *own_axis != axis {
            return false;
        }
        let next = (*ratio + delta).clamp(MIN_RATIO, MAX_RATIO);
        let changed = (next - *ratio).abs() > f32::EPSILON;
        *ratio = next;
        changed
    }

    /// Assign a rectangle to every visible leaf. A split that cannot hold two
    /// readable panes gives everything to the side containing `focus`, which
    /// is how small terminals degrade to fewer visible panes.
    fn layout(&self, rect: Rect, focus: PaneId, tiles: &mut Vec<Tile>, dividers: &mut Vec<Rect>) {
        match self {
            Self::Leaf(id) => tiles.push(Tile { pane: *id, rect }),
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let (a, divider, b) = match axis {
                    Axis::Columns => {
                        let inner = rect.width.saturating_sub(1);
                        let first_width = share(inner, *ratio);
                        (
                            Rect::new(rect.x, rect.y, first_width, rect.height),
                            Rect::new(rect.x + first_width, rect.y, 1, rect.height),
                            Rect::new(
                                rect.x + first_width + 1,
                                rect.y,
                                inner - first_width,
                                rect.height,
                            ),
                        )
                    }
                    Axis::Rows => {
                        let inner = rect.height.saturating_sub(1);
                        let first_height = share(inner, *ratio);
                        (
                            Rect::new(rect.x, rect.y, rect.width, first_height),
                            Rect::new(rect.x, rect.y + first_height, rect.width, 1),
                            Rect::new(
                                rect.x,
                                rect.y + first_height + 1,
                                rect.width,
                                inner - first_height,
                            ),
                        )
                    }
                };
                if a.fits() && b.fits() {
                    first.layout(a, focus, tiles, dividers);
                    dividers.push(divider);
                    second.layout(b, focus, tiles, dividers);
                } else if second.contains(focus) {
                    second.layout(rect, focus, tiles, dividers);
                } else {
                    first.layout(rect, focus, tiles, dividers);
                }
            }
        }
    }
}

/// Integer share of `total` for `ratio`, clamped so neither side is empty.
fn share(total: usize, ratio: f32) -> usize {
    if total < 2 {
        return total;
    }
    // Truncation is intended; the remainder goes to the second child.
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation
    )]
    let first = ((total as f32) * ratio) as usize;
    first.clamp(1, total - 1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Tile {
    pub pane: PaneId,
    pub rect: Rect,
}

/// The tree plus every pane's state and the current focus.
#[derive(Debug, Clone)]
pub(crate) struct Panes {
    root: Node,
    panes: HashMap<PaneId, Pane>,
    focused: PaneId,
    next_id: u32,
    zoomed: bool,
    /// Tiles from the last `layout`, kept for mouse hit-testing.
    tiles: Vec<Tile>,
}

impl Default for Panes {
    fn default() -> Self {
        let first = PaneId(0);
        Self {
            root: Node::Leaf(first),
            panes: HashMap::from([(first, Pane::default())]),
            focused: first,
            next_id: 1,
            zoomed: false,
            tiles: Vec::new(),
        }
    }
}

impl Panes {
    pub(crate) const fn focused_id(&self) -> PaneId {
        self.focused
    }

    pub(crate) fn focused(&self) -> &Pane {
        &self.panes[&self.focused]
    }

    pub(crate) fn focused_mut(&mut self) -> &mut Pane {
        self.panes
            .get_mut(&self.focused)
            .expect("the focused pane always exists")
    }

    pub(crate) fn get(&self, id: PaneId) -> Option<&Pane> {
        self.panes.get(&id)
    }

    pub(crate) fn get_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        self.panes.get_mut(&id)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.panes.len()
    }

    #[cfg(test)]
    pub(crate) const fn is_zoomed(&self) -> bool {
        self.zoomed
    }

    /// Every pane in tree (reading) order.
    #[cfg(test)]
    pub(crate) fn ids(&self) -> Vec<PaneId> {
        let mut leaves = Vec::with_capacity(self.panes.len());
        self.root.leaves(&mut leaves);
        leaves
    }

    /// Every distinct session shown by some pane. These are pinned warm.
    pub(crate) fn sessions(&self) -> impl Iterator<Item = SessionId> + '_ {
        self.panes.values().filter_map(|pane| pane.session)
    }

    pub(crate) fn panes_showing(&self, session: SessionId) -> impl Iterator<Item = PaneId> + '_ {
        self.panes
            .iter()
            .filter(move |(_, pane)| pane.session == Some(session))
            .map(|(id, _)| *id)
    }

    /// Split the focused pane. The new pane starts on the same session and
    /// takes focus. Returns the new pane, or `None` at the pane bound.
    pub(crate) fn split(&mut self, axis: Axis) -> Option<PaneId> {
        if self.panes.len() >= MAX_PANES {
            return None;
        }
        let fresh = PaneId(self.next_id);
        self.next_id += 1;
        let inherited = self.focused().session;
        let split = self.root.split(self.focused, axis, fresh);
        debug_assert!(split, "the focused pane is always in the tree");
        self.panes.insert(
            fresh,
            Pane {
                session: inherited,
                viewport: Viewport::default(),
            },
        );
        self.focused = fresh;
        self.zoomed = false;
        Some(fresh)
    }

    /// Close the focused pane and focus its former neighbour. The last pane
    /// cannot close. Returns the closed pane so render caches can drop it.
    pub(crate) fn close(&mut self) -> Option<PaneId> {
        if self.panes.len() == 1 {
            return None;
        }
        let closing = self.focused;
        let neighbour = self.root.remove(closing)?;
        self.panes.remove(&closing);
        self.focused = neighbour;
        self.zoomed = false;
        Some(closing)
    }

    pub(crate) fn toggle_zoom(&mut self) -> bool {
        if self.panes.len() == 1 {
            return false;
        }
        self.zoomed = !self.zoomed;
        true
    }

    pub(crate) fn focus(&mut self, id: PaneId) -> bool {
        if !self.panes.contains_key(&id) || id == self.focused {
            return false;
        }
        self.focused = id;
        true
    }

    /// The nearest pane in `direction` from the focused one, judged on the
    /// last laid-out tiles: the closest edge among panes that overlap the
    /// focused pane on the perpendicular axis.
    pub(crate) fn neighbour(&self, direction: Direction) -> Option<PaneId> {
        let from = self.tiles.iter().find(|tile| tile.pane == self.focused)?;
        let from = from.rect;
        self.tiles
            .iter()
            .filter(|tile| tile.pane != self.focused)
            .filter_map(|tile| {
                let rect = tile.rect;
                let (distance, overlaps) = match direction {
                    Direction::Left => {
                        (from.x.checked_sub(rect.right())?, rows_overlap(from, rect))
                    }
                    Direction::Right => {
                        (rect.x.checked_sub(from.right())?, rows_overlap(from, rect))
                    }
                    Direction::Up => (
                        from.y.checked_sub(rect.bottom())?,
                        columns_overlap(from, rect),
                    ),
                    Direction::Down => (
                        rect.y.checked_sub(from.bottom())?,
                        columns_overlap(from, rect),
                    ),
                };
                overlaps.then_some((distance, tile.pane))
            })
            .min_by_key(|(distance, pane)| (*distance, *pane))
            .map(|(_, pane)| pane)
    }

    #[cfg(test)]
    fn focus_direction(&mut self, direction: Direction) -> bool {
        match self.neighbour(direction) {
            Some(pane) => self.focus(pane),
            None => false,
        }
    }

    /// Move the divider of the innermost split enclosing the focused pane
    /// one step in `direction`, as tmux does: Left and Up shrink the first
    /// child, Right and Down grow it. No-op without a split on that axis.
    pub(crate) fn resize(&mut self, direction: Direction) -> bool {
        let delta = if direction.grows_first() {
            RESIZE_STEP
        } else {
            -RESIZE_STEP
        };
        self.root.resize(self.focused, direction.axis(), delta)
    }

    /// Lay every visible pane out inside `rect`. Zoomed or too small, only
    /// the focused path is shown. Pure: hand the tiles to `remember_tiles`
    /// once the frame is drawn so hit-testing and neighbour navigation see
    /// what is on screen.
    pub(crate) fn layout(&self, rect: Rect) -> (Vec<Tile>, Vec<Rect>) {
        let mut tiles = Vec::with_capacity(self.panes.len());
        let mut dividers = Vec::new();
        if self.zoomed {
            tiles.push(Tile {
                pane: self.focused,
                rect,
            });
        } else {
            self.root
                .layout(rect, self.focused, &mut tiles, &mut dividers);
        }
        (tiles, dividers)
    }

    /// Record the tiles of the frame just drawn.
    pub(crate) fn remember_tiles(&mut self, tiles: Vec<Tile>) {
        self.tiles = tiles;
    }

    /// The pane under a frame coordinate on the last layout.
    pub(crate) fn hit(&self, x: usize, y: usize) -> Option<PaneId> {
        self.tiles
            .iter()
            .find(|tile| tile.rect.contains(x, y))
            .map(|tile| tile.pane)
    }

    /// Reset every pane to show nothing, keeping the tree shape.
    pub(crate) fn clear_sessions(&mut self) {
        for pane in self.panes.values_mut() {
            pane.session = None;
        }
    }
}

const fn rows_overlap(a: Rect, b: Rect) -> bool {
    a.y < b.bottom() && b.y < a.bottom()
}

const fn columns_overlap(a: Rect, b: Rect) -> bool {
    a.x < b.right() && b.x < a.right()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(byte: u8) -> SessionId {
        SessionId::from_bytes([byte; 16])
    }

    fn tiles_of(panes: &mut Panes, rect: Rect) -> Vec<(PaneId, Rect)> {
        let (tiles, _) = panes.layout(rect);
        panes.remember_tiles(tiles.clone());
        tiles
            .into_iter()
            .map(|tile| (tile.pane, tile.rect))
            .collect()
    }

    #[test]
    fn a_new_layout_is_one_pane_filling_the_rect() {
        let mut panes = Panes::default();
        let rect = Rect::new(0, 2, 100, 30);
        assert_eq!(tiles_of(&mut panes, rect), vec![(panes.focused_id(), rect)]);
        assert!(panes.close().is_none(), "the last pane cannot close");
        assert!(!panes.toggle_zoom());
    }

    #[test]
    fn splitting_inherits_the_session_and_focuses_the_new_pane() {
        let mut panes = Panes::default();
        panes.focused_mut().session = Some(session(1));
        let original = panes.focused_id();
        let fresh = panes.split(Axis::Columns).expect("room to split");
        assert_eq!(panes.focused_id(), fresh);
        assert_eq!(panes.focused().session, Some(session(1)));
        assert_eq!(panes.ids(), vec![original, fresh]);
        let tiles = tiles_of(&mut panes, Rect::new(0, 0, 101, 20));
        assert_eq!(tiles[0], (original, Rect::new(0, 0, 50, 20)));
        assert_eq!(tiles[1], (fresh, Rect::new(51, 0, 50, 20)));
        let (_, dividers) = panes.layout(Rect::new(0, 0, 101, 20));
        assert_eq!(dividers, vec![Rect::new(50, 0, 1, 20)]);
    }

    #[test]
    fn nested_splits_tile_like_a_window_manager() {
        let mut panes = Panes::default();
        let a = panes.focused_id();
        let b = panes.split(Axis::Columns).unwrap();
        let c = panes.split(Axis::Rows).unwrap();
        // a | (b / c)
        let tiles = tiles_of(&mut panes, Rect::new(0, 0, 101, 21));
        assert_eq!(tiles[0], (a, Rect::new(0, 0, 50, 21)));
        assert_eq!(tiles[1], (b, Rect::new(51, 0, 50, 10)));
        assert_eq!(tiles[2], (c, Rect::new(51, 11, 50, 10)));
    }

    #[test]
    fn closing_collapses_onto_the_sibling_and_moves_focus_there() {
        let mut panes = Panes::default();
        let a = panes.focused_id();
        let b = panes.split(Axis::Columns).unwrap();
        let c = panes.split(Axis::Rows).unwrap();
        assert_eq!(panes.close(), Some(c));
        assert_eq!(panes.focused_id(), b);
        assert_eq!(panes.ids(), vec![a, b]);
        assert_eq!(panes.close(), Some(b));
        assert_eq!(panes.focused_id(), a);
        assert_eq!(panes.len(), 1);
        assert!(panes.get(b).is_none());
    }

    #[test]
    fn closing_a_pane_in_a_deep_subtree_promotes_the_sibling_subtree() {
        let mut panes = Panes::default();
        let a = panes.focused_id();
        let b = panes.split(Axis::Columns).unwrap();
        let c = panes.split(Axis::Rows).unwrap();
        panes.focus(a);
        assert_eq!(panes.close(), Some(a));
        // (b / c) is now the whole tree and b, the first leaf, is focused.
        assert_eq!(panes.focused_id(), b);
        let tiles = tiles_of(&mut panes, Rect::new(0, 0, 60, 21));
        assert_eq!(tiles[0], (b, Rect::new(0, 0, 60, 10)));
        assert_eq!(tiles[1], (c, Rect::new(0, 11, 60, 10)));
    }

    #[test]
    fn directional_focus_uses_geometry() {
        let mut panes = Panes::default();
        let a = panes.focused_id();
        let b = panes.split(Axis::Columns).unwrap();
        let c = panes.split(Axis::Rows).unwrap();
        tiles_of(&mut panes, Rect::new(0, 0, 101, 21));
        assert_eq!(panes.focused_id(), c);
        assert!(panes.focus_direction(Direction::Up));
        assert_eq!(panes.focused_id(), b);
        assert!(!panes.focus_direction(Direction::Up), "nothing above b");
        assert!(panes.focus_direction(Direction::Left));
        assert_eq!(panes.focused_id(), a);
        assert!(!panes.focus_direction(Direction::Left));
        // From a, both b and c are to the right; b is the tie-break by id
        // because both share a distance and overlap a's rows.
        assert!(panes.focus_direction(Direction::Right));
        assert_eq!(panes.focused_id(), b);
        assert!(panes.focus_direction(Direction::Down));
        assert_eq!(panes.focused_id(), c);
    }

    #[test]
    fn resize_moves_the_innermost_split_on_that_axis_and_clamps() {
        let mut panes = Panes::default();
        let a = panes.focused_id();
        panes.split(Axis::Columns).unwrap();
        // Left moves the divider left, whichever side holds focus.
        assert!(panes.resize(Direction::Left));
        let tiles = tiles_of(&mut panes, Rect::new(0, 0, 101, 20));
        assert_eq!(tiles[0].1.width, 45);
        assert_eq!(tiles[1].1.width, 55);
        // No row split encloses the focused pane, so that axis is a no-op.
        assert!(!panes.resize(Direction::Up));
        for _ in 0..20 {
            panes.resize(Direction::Left);
        }
        // 201 columns keep both sides readable at the minimum ratio.
        let tiles = tiles_of(&mut panes, Rect::new(0, 0, 201, 20));
        assert_eq!(tiles[0].1.width, 30, "ratio clamps at MIN_RATIO");
        assert!(
            !panes.resize(Direction::Left),
            "a clamped resize reports no change"
        );
        panes.focus(a);
        assert!(panes.resize(Direction::Right));
        let tiles = tiles_of(&mut panes, Rect::new(0, 0, 201, 20));
        assert_eq!(tiles[0].1.width, 40);
    }

    #[test]
    fn small_rects_collapse_to_the_focused_side_without_changing_the_tree() {
        let mut panes = Panes::default();
        let a = panes.focused_id();
        let b = panes.split(Axis::Columns).unwrap();
        let narrow = Rect::new(0, 0, 40, 20);
        assert_eq!(tiles_of(&mut panes, narrow), vec![(b, narrow)]);
        panes.focus(a);
        assert_eq!(tiles_of(&mut panes, narrow), vec![(a, narrow)]);
        assert_eq!(panes.len(), 2, "the tree is intact");
        let wide = Rect::new(0, 0, 101, 20);
        assert_eq!(tiles_of(&mut panes, wide).len(), 2);
    }

    #[test]
    fn zoom_shows_only_the_focused_pane_and_splitting_unzooms() {
        let mut panes = Panes::default();
        let a = panes.focused_id();
        let b = panes.split(Axis::Rows).unwrap();
        assert!(panes.toggle_zoom());
        let rect = Rect::new(0, 0, 100, 30);
        assert_eq!(tiles_of(&mut panes, rect), vec![(b, rect)]);
        assert!(panes.is_zoomed());
        panes.focus(a);
        assert_eq!(tiles_of(&mut panes, rect), vec![(a, rect)]);
        let c = panes.split(Axis::Columns).unwrap();
        assert!(!panes.is_zoomed());
        assert_eq!(tiles_of(&mut panes, rect).len(), 3);
        let _ = c;
    }

    #[test]
    fn hit_testing_finds_the_pane_under_a_cell() {
        let mut panes = Panes::default();
        let a = panes.focused_id();
        let b = panes.split(Axis::Columns).unwrap();
        tiles_of(&mut panes, Rect::new(0, 2, 101, 20));
        assert_eq!(panes.hit(10, 5), Some(a));
        assert_eq!(panes.hit(60, 5), Some(b));
        assert_eq!(panes.hit(50, 5), None, "the divider belongs to no pane");
        assert_eq!(panes.hit(10, 1), None, "above the body");
    }

    #[test]
    fn the_pane_count_is_bounded() {
        let mut panes = Panes::default();
        for _ in 1..MAX_PANES {
            assert!(panes.split(Axis::Columns).is_some());
        }
        assert!(panes.split(Axis::Columns).is_none());
        assert_eq!(panes.len(), MAX_PANES);
    }

    #[test]
    fn viewport_follows_the_tail_until_scrolled_and_keeps_its_top_row() {
        let mut viewport = Viewport::default();
        let context = (Some(session(1)), Layout::Threadline);
        viewport.update(context, 100, 10, false);
        assert_eq!(viewport.offset(), 0);
        assert!(viewport.scroll_up(5));
        assert_eq!(viewport.offset(), 5);
        // Ten rows appended: the same top row stays visible.
        viewport.update(context, 110, 10, false);
        assert_eq!(viewport.offset(), 15);
        // A settled live message asks to keep the tail anchor instead.
        viewport.update(context, 120, 10, true);
        assert_eq!(viewport.offset(), 15);
        assert!(viewport.scroll_down(100));
        assert_eq!(viewport.offset(), 0);
        assert!(viewport.scroll_up(usize::MAX));
        assert_eq!(viewport.offset(), 110);
        // Switching session returns to the tail.
        viewport.update((Some(session(2)), Layout::Threadline), 50, 10, false);
        assert_eq!(viewport.offset(), 0);
    }
}
