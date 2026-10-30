//! The split tree: panes, splitting, resizing, golden ratio layout.
//!
//! A binary tree of splits.  [`Axis::Row`] stacks children top over bottom (a
//! horizontal split, like vim's `:split`); [`Axis::Column`] places them side by
//! side (vim's `:vsplit`).

use ratatui::layout::Rect;

use crate::action::{Direction, SplitMode};
use crate::buffer::BufferId;
use crate::config::LayoutConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PaneId(pub u32);

/// Axis along which a split divides its area.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Axis {
    /// Children stacked vertically — `:split`.
    Row,
    /// Children side by side — `:vsplit`.
    Column,
}

impl Axis {
    pub fn from_direction(direction: Direction) -> Self {
        match direction {
            Direction::Left | Direction::Right => Axis::Column,
            Direction::Up | Direction::Down => Axis::Row,
        }
    }

    pub fn other(self) -> Self {
        match self {
            Axis::Row => Axis::Column,
            Axis::Column => Axis::Row,
        }
    }

    /// Typical terminal cell aspect ratio (width ÷ height).  For the common
    /// monospace fonts a cell is about twice as tall as it is wide, so one
    /// column buys roughly half the physical space of one row.
    const CELL_ASPECT: f32 = 0.5;

    /// Pick the axis with more *physical* room.
    ///
    /// Cell counts alone lie: characters are taller than wide, so a 100×60
    /// pane looks wide but is nearly square on screen.  Comparing physical
    /// extents (`width × CELL_ASPECT` against `height`) means side-by-side
    /// (`:vsplit`) needs roughly twice as many columns as rows.
    pub fn auto(area: Rect) -> Self {
        if area.width as f32 * Self::CELL_ASPECT > area.height as f32 {
            Axis::Column
        } else {
            Axis::Row
        }
    }
}

/// Per-pane viewport state.
#[derive(Debug, Clone, Default)]
pub struct View {
    /// First visible line (byte-anchored by the editor on each draw).
    pub scroll_line: usize,
    /// First visible column (horizontal scroll).
    pub scroll_col: usize,
    /// Column that vertical motions try to restore.
    pub wanted_col: Option<usize>,
}

impl View {
    pub fn reset(&mut self) {
        *self = View::default();
    }
}

/// A terminal pane showing a buffer.
///
/// The selection lives here, not on the buffer: two panes may show the same text
/// with different cursors.
#[derive(Debug, Clone)]
pub struct Pane {
    pub id: PaneId,
    pub buffer: BufferId,
    pub view: View,
    pub selection: crate::selection::Selection,
    /// Overrides the buffer name in the border label.
    pub label: Option<String>,
}

impl Pane {
    pub fn new(id: PaneId, buffer: BufferId) -> Self {
        Self {
            id,
            buffer,
            view: View::default(),
            selection: crate::selection::Selection::point(0),
            label: None,
        }
    }

    pub fn cursor(&self) -> usize {
        self.selection.cursor()
    }
}

/// Why a layout operation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutError;

/// A node of the split tree.
#[derive(Debug, Clone)]
pub enum Node {
    Leaf(Pane),
    Split {
        axis: Axis,
        /// Fraction of the parent given to `first`.
        ratio: f64,
        first: Box<Node>,
        second: Box<Node>,
    },
}

impl Node {
    pub fn leaf(pane: Pane) -> Node {
        Node::Leaf(pane)
    }

    /// Every pane id, left-to-right / top-to-bottom.
    pub fn pane_ids(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        self.walk(&mut |pane| out.push(pane.id));
        out
    }

    pub fn contains(&self, id: PaneId) -> bool {
        self.find(id).is_some()
    }

    pub fn find(&self, id: PaneId) -> Option<&Pane> {
        match self {
            Node::Leaf(pane) if pane.id == id => Some(pane),
            Node::Leaf(_) => None,
            Node::Split { first, second, .. } => first.find(id).or_else(|| second.find(id)),
        }
    }

    pub fn find_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        match self {
            Node::Leaf(pane) if pane.id == id => Some(pane),
            Node::Leaf(_) => None,
            Node::Split { first, second, .. } => first.find_mut(id).or_else(|| second.find_mut(id)),
        }
    }

    fn walk(&self, visit: &mut impl FnMut(&Pane)) {
        match self {
            Node::Leaf(pane) => visit(pane),
            Node::Split { first, second, .. } => {
                first.walk(visit);
                second.walk(visit);
            }
        }
    }

    fn walk_mut(&mut self, visit: &mut impl FnMut(&mut Pane)) {
        match self {
            Node::Leaf(pane) => visit(pane),
            Node::Split { first, second, .. } => {
                first.walk_mut(visit);
                second.walk_mut(visit);
            }
        }
    }

    pub fn pane_count(&self) -> usize {
        let mut count = 0;
        self.walk(&mut |_| count += 1);
        count
    }

    /// Would a split of `id` leave both halves usable?
    pub fn can_split(
        &self,
        id: PaneId,
        axis: Axis,
        keep: f64,
        area: Rect,
        config: &LayoutConfig,
    ) -> bool {
        let Some(rect) = self.rect_of(id, area) else {
            return false;
        };
        let (limit, min) = match axis {
            Axis::Row => (rect.height as f64, config.min_height as f64),
            Axis::Column => (rect.width as f64, config.min_width as f64),
        };
        let keep = keep.clamp(0.05, 0.95);
        limit * keep >= min && limit * (1.0 - keep) >= min
    }

    /// Split the leaf `id` in two.  `keep` is the share the existing pane holds on to.
    pub fn split_leaf(&mut self, id: PaneId, axis: Axis, keep: f64, new_pane: Pane) -> bool {
        match self {
            Node::Leaf(pane) if pane.id == id => {
                let existing = pane.clone();
                *self = Node::Split {
                    axis,
                    ratio: keep.clamp(0.05, 0.95),
                    first: Box::new(Node::Leaf(existing)),
                    second: Box::new(Node::Leaf(new_pane)),
                };
                true
            }
            Node::Split { first, second, .. } => {
                if first.split_leaf(id, axis, keep, new_pane.clone()) {
                    true
                } else {
                    second.split_leaf(id, axis, keep, new_pane)
                }
            }
            Node::Leaf(_) => false,
        }
    }

    /// Rectangle of one pane, or `None` when it is not in the tree.
    pub fn rect_of(&self, id: PaneId, area: Rect) -> Option<Rect> {
        let mut found = None;
        self.rects(area, &mut |pane, rect| {
            if pane.id == id {
                found = Some(rect);
            }
        });
        found
    }

    /// Compute the rectangle of every pane.
    pub fn rects(&self, area: Rect, visit: &mut impl FnMut(&Pane, Rect)) {
        match self {
            Node::Leaf(pane) => visit(pane, area),
            Node::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let ratio = clamp_ratio(*ratio, area, *axis);
                match axis {
                    Axis::Row => {
                        let height = (area.height as f64 * ratio).round() as u16;
                        let height = height.min(area.height.saturating_sub(1)).max(1);
                        let top = Rect { height, ..area };
                        let bottom = Rect {
                            y: area.y + height,
                            height: area.height - height,
                            ..area
                        };
                        first.rects(top, visit);
                        second.rects(bottom, visit);
                    }
                    Axis::Column => {
                        let width = (area.width as f64 * ratio).round() as u16;
                        let width = width.min(area.width.saturating_sub(1)).max(1);
                        let left = Rect { width, ..area };
                        let right = Rect {
                            x: area.x + width,
                            width: area.width - width,
                            ..area
                        };
                        first.rects(left, visit);
                        second.rects(right, visit);
                    }
                }
            }
        }
    }

    /// All pane rectangles.
    pub fn rect_map(&self, area: Rect) -> Vec<(PaneId, Rect)> {
        let mut out = Vec::new();
        self.rects(area, &mut |pane, rect| out.push((pane.id, rect)));
        out
    }

    /// Remove a pane, promoting its sibling into place.
    ///
    /// Returns a pane that still exists (worth focusing), or `None` when the
    /// removed pane was the only one left.
    pub fn remove(&mut self, id: PaneId) -> Option<PaneId> {
        match self {
            Node::Leaf(pane) => {
                if pane.id == id {
                    None
                } else {
                    Some(pane.id)
                }
            }
            Node::Split { first, second, .. } => {
                if first.is_leaf(id) {
                    *self = promote(second);
                    return Some(pane_id(self));
                }
                if second.is_leaf(id) {
                    *self = promote(first);
                    return Some(pane_id(self));
                }
                // Recurse into the branch that actually holds `id`.  Asking
                // `first` first and trusting its answer is wrong: a leaf in
                // `first` that is not `id` returns "survivor" without ever
                // looking at `second`, so nested panes were never removed.
                if first.contains(id) {
                    first.remove(id)
                } else {
                    second.remove(id)
                }
            }
        }
    }

    /// Ratio adjustment for the split directly containing `id`, along `axis`.
    ///
    /// `delta` is added to the ratio of the branch holding `id` (positive grows
    /// it).  Returns false when no split on the path uses that axis.
    pub fn resize(
        &mut self,
        id: PaneId,
        axis: Axis,
        delta: f64,
        min_width: u16,
        min_height: u16,
        area: Rect,
    ) -> bool {
        match self {
            Node::Leaf(_) => false,
            Node::Split {
                axis: node_axis,
                ratio,
                first,
                second,
            } => {
                let first_side = first.contains(id);
                let in_path = first_side || second.contains(id);
                if !in_path {
                    return false;
                }
                if *node_axis == axis {
                    let rect = area;
                    let (limit, min) = match axis {
                        Axis::Row => (rect.height as f64, min_height as f64),
                        Axis::Column => (rect.width as f64, min_width as f64),
                    };
                    let signed = if first_side { delta } else { -delta };
                    let candidate = *ratio + signed;
                    let min_ratio = (min / limit).clamp(0.02, 0.98);
                    let clamped = candidate.clamp(min_ratio, 1.0 - min_ratio);
                    *ratio = clamped;
                    return true;
                }
                let child = if first_side { first } else { second };
                let child_rect = if first_side {
                    first_child_rect(area, *node_axis, *ratio)
                } else {
                    second_child_rect(area, *node_axis, *ratio)
                };
                if child.resize(id, axis, delta, min_width, min_height, child_rect) {
                    return true;
                }
                // Fall back to the other axis so the key does nothing surprising.
                false
            }
        }
    }

    /// Number of Row and Column splits between this node and `id`, or
    /// `None` when `id` is not in this subtree.
    fn path_axes(&self, id: PaneId) -> Option<(usize, usize)> {
        match self {
            Node::Leaf(pane) if pane.id == id => Some((0, 0)),
            Node::Leaf(_) => None,
            Node::Split {
                axis,
                first,
                second,
                ..
            } => {
                let (rows, cols) = first.path_axes(id).or_else(|| second.path_axes(id))?;
                Some(match axis {
                    Axis::Row => (rows + 1, cols),
                    Axis::Column => (rows, cols + 1),
                })
            }
        }
    }

    /// Rectangle of `id` if every split were even.  Golden mode inflates
    /// the focused pane to its golden share, so `split-auto` judges room
    /// on this instead of the live rect — otherwise the focused pane
    /// always looks wide enough to split side by side and the axis never
    /// flips.
    pub fn even_rect_of(&self, id: PaneId, area: Rect) -> Option<Rect> {
        match self {
            Node::Leaf(pane) if pane.id == id => Some(area),
            Node::Leaf(_) => None,
            Node::Split {
                axis,
                first,
                second,
                ..
            } => {
                let (first_area, second_area) = match axis {
                    Axis::Row => {
                        let height = (area.height / 2).max(1).min(area.height);
                        (
                            Rect { height, ..area },
                            Rect {
                                y: area.y + height,
                                height: area.height - height,
                                ..area
                            },
                        )
                    }
                    Axis::Column => {
                        let width = (area.width / 2).max(1).min(area.width);
                        (
                            Rect { width, ..area },
                            Rect {
                                x: area.x + width,
                                width: area.width - width,
                                ..area
                            },
                        )
                    }
                };
                first
                    .even_rect_of(id, first_area)
                    .or_else(|| second.even_rect_of(id, second_area))
            }
        }
    }

    /// Set _every_ split to the golden ratio: splits on the path to `id`
    /// give their axis' step to the focused branch, splits away from the
    /// path take `ratio` with the first (top/left) child larger.  Nothing
    /// is left equalised, so a focus change re-sizes the whole tree, not
    /// just the focused sub-split.
    ///
    /// `step_row` / `step_col` are `ratio` spread over the number of Row /
    /// Column splits on the focus path (see [`Layout::apply_golden`]), so
    /// the focused pane ends up with `ratio` of the whole window along
    /// each axis.  Spreading per axis keeps the axes independent: adding
    /// a column split inside one branch never re-sizes a row split, so
    /// panes outside that branch keep their size.
    pub fn apply_golden(&mut self, id: PaneId, step_row: f64, step_col: f64, ratio: f64) {
        match self {
            Node::Leaf(_) => {}
            Node::Split {
                axis,
                ratio: node_ratio,
                first,
                second,
            } => {
                let first_side = first.contains(id);
                let second_side = second.contains(id);
                let on_path = first_side || second_side;
                // On the path the focused branch wins; away from it the
                // first child takes the golden share.
                let first_gets = if on_path { first_side } else { true };
                let step = match axis {
                    Axis::Row => step_row,
                    Axis::Column => step_col,
                };
                let share = if on_path { step } else { ratio };
                *node_ratio = if first_gets { share } else { 1.0 - share };
                // Written as branches so the two children stay separately
                // borrow-checked.
                if first_side {
                    first.apply_golden(id, step_row, step_col, ratio);
                    second.goldenize(ratio);
                } else if second_side {
                    second.apply_golden(id, step_row, step_col, ratio);
                    first.goldenize(ratio);
                } else {
                    first.goldenize(ratio);
                    second.goldenize(ratio);
                }
            }
        }
    }

    /// Set every split to `ratio`, first child taking the larger share.
    fn goldenize(&mut self, ratio: f64) {
        match self {
            Node::Leaf(_) => {}
            Node::Split {
                ratio: node_ratio,
                first,
                second,
                ..
            } => {
                *node_ratio = ratio;
                first.goldenize(ratio);
                second.goldenize(ratio);
            }
        }
    }

    /// Make every split even.
    pub fn equalize(&mut self) {
        match self {
            Node::Leaf(_) => {}
            Node::Split {
                ratio,
                first,
                second,
                ..
            } => {
                *ratio = 0.5;
                first.equalize();
                second.equalize();
            }
        }
    }

    /// Swap what two panes show.  Pane identity and viewport stay put.
    pub fn swap(&mut self, a: PaneId, b: PaneId) -> bool {
        let Some(pane_a) = self.find(a) else {
            return false;
        };
        let Some(pane_b) = self.find(b) else {
            return false;
        };
        let (buffer_a, label_a) = (pane_a.buffer, pane_a.label.clone());
        let (buffer_b, label_b) = (pane_b.buffer, pane_b.label.clone());
        let pane_a = self.find_mut(a).expect("checked above");
        pane_a.buffer = buffer_b;
        pane_a.label = label_b;
        pane_a.view.reset();
        let pane_b = self.find_mut(b).expect("checked above");
        pane_b.buffer = buffer_a;
        pane_b.label = label_a;
        pane_b.view.reset();
        true
    }

    /// The neighbour of `id` in `direction`, chosen geometrically.
    pub fn neighbour(&self, id: PaneId, direction: Direction, area: Rect) -> Option<PaneId> {
        let rects = self.rect_map(area);
        let (_, from) = rects.iter().find(|(pane, _)| *pane == id)?;
        let mut best: Option<(i64, PaneId)> = None;
        for (pane, rect) in &rects {
            if *pane == id {
                continue;
            }
            let Some(distance) = direction_gap(*from, *rect, direction) else {
                continue;
            };
            // Prefer panes that line up with the current one.
            let overlap = overlap_length(*from, *rect, direction);
            let miss = match direction {
                Direction::Left | Direction::Right => {
                    (rect.height.max(from.height) - overlap) as i64 * 4
                }
                Direction::Up | Direction::Down => {
                    (rect.width.max(from.width) - overlap) as i64 * 4
                }
            };
            let score = distance + miss;
            if best.is_none_or(|(best_score, _)| score < best_score) {
                best = Some((score, *pane));
            }
        }
        best.map(|(_, pane)| pane)
    }

    /// Next pane in tree order (wrapping).
    pub fn next_pane(&self, id: PaneId, backward: bool) -> Option<PaneId> {
        let panes = self.pane_ids();
        let position = panes.iter().position(|p| *p == id)?;
        let next = if backward {
            (position + panes.len() - 1) % panes.len()
        } else {
            (position + 1) % panes.len()
        };
        Some(panes[next])
    }
}

impl Node {
    fn is_leaf(&self, id: PaneId) -> bool {
        matches!(self, Node::Leaf(pane) if pane.id == id)
    }
}

/// Move a subtree out of its (about to disappear) parent slot.
fn promote(child: &mut Box<Node>) -> Node {
    std::mem::replace(&mut **child, Node::Leaf(Pane::new(PaneId(0), BufferId(0))))
}

fn pane_id(node: &Node) -> PaneId {
    let mut out = PaneId(0);
    node.walk(&mut |pane| out = pane.id);
    out
}

fn first_child_rect(area: Rect, axis: Axis, ratio: f64) -> Rect {
    match axis {
        Axis::Row => Rect {
            height: ((area.height as f64 * ratio).round() as u16)
                .max(1)
                .min(area.height),
            ..area
        },
        Axis::Column => Rect {
            width: ((area.width as f64 * ratio).round() as u16)
                .max(1)
                .min(area.width),
            ..area
        },
    }
}

fn second_child_rect(area: Rect, axis: Axis, ratio: f64) -> Rect {
    let first = first_child_rect(area, axis, ratio);
    match axis {
        Axis::Row => Rect {
            y: area.y + first.height,
            height: area.height - first.height,
            ..area
        },
        Axis::Column => Rect {
            x: area.x + first.width,
            width: area.width - first.width,
            ..area
        },
    }
}

/// Keep splits from collapsing when the area is tiny.
fn clamp_ratio(ratio: f64, area: Rect, axis: Axis) -> f64 {
    let limit = match axis {
        Axis::Row => area.height,
        Axis::Column => area.width,
    };
    if limit < 3 {
        return 0.5;
    }
    let min = 1.0 / limit as f64;
    ratio.clamp(min, 1.0 - min)
}

/// Gap between two rectangles along a direction; `None` if not in that direction.
fn direction_gap(from: Rect, to: Rect, direction: Direction) -> Option<i64> {
    match direction {
        Direction::Left if (to.x + to.width) as i64 <= from.x as i64 => {
            Some(from.x as i64 - (to.x + to.width) as i64)
        }
        Direction::Right if to.x as i64 >= (from.x + from.width) as i64 => {
            Some(to.x as i64 - (from.x + from.width) as i64)
        }
        Direction::Up if (to.y + to.height) as i64 <= from.y as i64 => {
            Some(from.y as i64 - (to.y + to.height) as i64)
        }
        Direction::Down if to.y as i64 >= (from.y + from.height) as i64 => {
            Some(to.y as i64 - (from.y + from.height) as i64)
        }
        _ => None,
    }
}

fn overlap_length(from: Rect, to: Rect, direction: Direction) -> u16 {
    match direction {
        Direction::Left | Direction::Right => {
            let start = from.y.max(to.y) as i64;
            let end = (from.y + from.height).min(to.y + to.height) as i64;
            (end - start).max(0) as u16
        }
        Direction::Up | Direction::Down => {
            let start = from.x.max(to.x) as i64;
            let end = (from.x + from.width).min(to.x + to.width) as i64;
            (end - start).max(0) as u16
        }
    }
}

/// The layout: a tree plus the focused pane and how new splits behave.
#[derive(Debug, Clone)]
pub struct Layout {
    pub root: Option<Node>,
    pub focus: Option<PaneId>,
    /// Temporarily fill the terminal with the focused pane.
    pub zoomed: bool,
    next_id: u32,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            root: None,
            focus: None,
            zoomed: false,
            next_id: 1,
        }
    }
}

impl Layout {
    pub fn new(root: Node, focus: PaneId) -> Self {
        let highest = root.pane_ids().iter().map(|id| id.0).max().unwrap_or(1);
        Self {
            root: Some(root),
            focus: Some(focus),
            zoomed: false,
            next_id: highest,
        }
    }

    /// A pane id that is not in the tree yet.
    /// Start a single-pane layout showing `buffer`.
    ///
    /// This is what makes the first `vortex some/file` open a pane instead of
    /// leaving the welcome screen up.
    pub fn show_first(&mut self, buffer: BufferId) -> PaneId {
        let pane_id = self.alloc_id();
        self.root = Some(Node::leaf(Pane::new(pane_id, buffer)));
        self.focus = Some(pane_id);
        pane_id
    }

    pub fn alloc_id(&mut self) -> PaneId {
        loop {
            self.next_id = self.next_id.wrapping_add(1);
            let id = PaneId(self.next_id);
            if self.root.as_ref().is_none_or(|root| !root.contains(id)) {
                return id;
            }
        }
    }

    pub fn focused(&self) -> Option<PaneId> {
        self.focus
            .filter(|id| self.root.as_ref().is_some_and(|root| root.contains(*id)))
    }

    pub fn focused_pane(&self) -> Option<&Pane> {
        self.focus.and_then(|id| self.root.as_ref()?.find(id))
    }

    pub fn focused_pane_mut(&mut self) -> Option<&mut Pane> {
        let id = self.focused()?;
        self.root.as_mut()?.find_mut(id)
    }

    pub fn panes(&self) -> Vec<PaneId> {
        self.root
            .as_ref()
            .map(|root| root.pane_ids())
            .unwrap_or_default()
    }

    pub fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.root.as_ref()?.find(id)
    }

    pub fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        self.root.as_mut()?.find_mut(id)
    }

    pub fn is_focused(&self, id: PaneId) -> bool {
        self.focused() == Some(id)
    }

    /// Rectangles for every pane within `area`.
    pub fn rects(&self, area: Rect) -> Vec<(PaneId, Rect)> {
        if self.zoomed && self.focused().is_some_and(|_focus| self.panes().len() > 1) {
            let focus = self.focused().expect("checked above");
            return vec![(focus, area)];
        }
        match &self.root {
            Some(root) => root.rect_map(area),
            None => Vec::new(),
        }
    }

    /// The pane covering terminal coordinates `(column, row)`.
    pub fn pane_at(&self, area: Rect, column: u16, row: u16) -> Option<PaneId> {
        self.rects(area)
            .into_iter()
            .find(|(_, rect)| rect.contains(ratatui::layout::Position { x: column, y: row }))
            .map(|(id, _)| id)
    }

    /// Split the focused pane.  `axis` names the direction the command asked
    /// for (`split-horizontal` / `split-vertical`); `None` (`split-auto`)
    /// picks the axis with the most physical room, falling back to the other
    /// when this pane is too thin that way.  The split mode only governs the
    /// resulting sizes, never the direction.
    pub fn split_focused(
        &mut self,
        axis: Option<Axis>,
        config: &LayoutConfig,
        area: Rect,
        make_pane: impl FnOnce(PaneId) -> Pane,
    ) -> Option<PaneId> {
        let focus = self.focused()?;
        let root_area = if self.zoomed {
            // Un-zoom before splitting so sizes stay predictable.
            self.zoomed = false;
            area
        } else {
            area
        };
        // Allocate the id before borrowing the tree, so `self` stays free.
        let new_id = self.alloc_id();
        let root = self.root.as_mut()?;
        let rect = root.rect_of(focus, root_area)?;
        // No axis named (`space n`): take the one with more physical space,
        // then fall back to the other if this pane is too thin that way.
        // Golden mode inflates the focused pane to its golden share of the
        // window, so the live rect would always read "wide" and the axis
        // would never flip; judge the room on the even-split rect instead.
        let auto_axis = axis.is_none();
        let mut axis = axis.unwrap_or_else(|| match config.split_mode {
            SplitMode::Golden => {
                let even = root.even_rect_of(focus, root_area).unwrap_or(rect);
                Axis::auto(even)
            }
            SplitMode::Equal => Axis::auto(rect),
        });
        // The share the existing pane keeps.  Golden re-sizes the tree right
        // after the split so the new, focused pane gets `golden_ratio` of the
        // parent; the room check must use those shares, not `split_ratio`.
        let keep = match config.split_mode {
            SplitMode::Golden => 1.0 - config.golden_ratio.clamp(0.05, 0.95),
            SplitMode::Equal => 1.0 - config.split_ratio,
        };
        if !root.can_split(focus, axis, keep, root_area, config) {
            let other = axis.other();
            if !auto_axis || !root.can_split(focus, other, keep, root_area, config) {
                return None;
            }
            axis = other;
        }
        let pane = make_pane(new_id);
        if !root.split_leaf(focus, axis, keep, pane) {
            return None;
        }
        self.focus = Some(new_id);
        if config.split_mode == SplitMode::Golden {
            self.apply_golden(config);
        }
        Some(new_id)
    }

    /// Remove a pane, returning the pane to focus next.
    pub fn remove(&mut self, id: PaneId) -> Option<PaneId> {
        let panes = self.panes();
        let index = panes.iter().position(|p| *p == id)?;
        let neighbour = if panes.len() <= 1 {
            None
        } else if index > 0 {
            panes.get(index - 1).copied()
        } else {
            // The first pane has no predecessor; fall to the next one rather
            // than to the removed pane itself.
            panes.get(1).copied()
        };
        let root = self.root.as_mut()?;
        root.remove(id);
        if root.pane_count() == 0 {
            self.root = None;
            self.focus = None;
            return None;
        }
        let next = neighbour.filter(|n| root.contains(*n)).or_else(|| {
            let mut ids = root.pane_ids();
            ids.sort();
            ids.first().copied()
        });
        self.focus = next;
        next
    }

    /// Keep only the focused pane.
    pub fn only(&mut self) -> Option<()> {
        let focus = self.focused()?;
        let pane = self.root.as_ref()?.find(focus)?.clone();
        self.root = Some(Node::Leaf(pane));
        self.zoomed = false;
        Some(())
    }

    pub fn apply_golden(&mut self, config: &LayoutConfig) {
        let Some(focus) = self.focused() else {
            return;
        };
        let ratio = config.golden_ratio.clamp(0.05, 0.95);
        if let Some(root) = self.root.as_mut() {
            // The focused pane should end up with `ratio` of the *window*
            // along each axis.  Spread the share over the splits on the
            // focus path per axis — `ratio^(1/depth)` of each Row split
            // along the path, likewise for Column — so nesting column
            // splits inside one branch never re-sizes a row split (and
            // thus never resizes panes outside that branch).
            let (rows, cols) = root.path_axes(focus).unwrap_or((0, 0));
            let step = |depth: usize| ratio.powf(1.0 / depth.max(1) as f64);
            root.apply_golden(focus, step(rows), step(cols), ratio);
        }
    }

    /// Called whenever focus changes.
    pub fn on_focus_changed(&mut self, config: &LayoutConfig) {
        if config.split_mode == SplitMode::Golden {
            self.apply_golden(config);
        }
    }

    pub fn equalize(&mut self) {
        if let Some(root) = self.root.as_mut() {
            root.equalize();
        }
    }

    pub fn focus(&mut self, id: PaneId) -> bool {
        if self.root.as_ref().is_some_and(|root| root.contains(id)) {
            self.focus = Some(id);
            return true;
        }
        false
    }

    pub fn focus_direction(&mut self, direction: Direction, area: Rect) -> bool {
        let Some(focus) = self.focused() else {
            return false;
        };
        let Some(root) = self.root.as_ref() else {
            return false;
        };
        match root.neighbour(focus, direction, area) {
            Some(neighbour) => {
                self.focus = Some(neighbour);
                true
            }
            None => false,
        }
    }

    pub fn focus_next(&mut self, backward: bool) -> bool {
        let Some(focus) = self.focused() else {
            return false;
        };
        match self
            .root
            .as_ref()
            .and_then(|root| root.next_pane(focus, backward))
        {
            Some(next) => {
                self.focus = Some(next);
                true
            }
            None => false,
        }
    }

    /// Grow or shrink the focused pane along the direction's axis.
    pub fn resize_focused(
        &mut self,
        direction: Direction,
        config: &LayoutConfig,
        area: Rect,
    ) -> bool {
        let Some(focus) = self.focused() else {
            return false;
        };
        let axis = Axis::from_direction(direction);
        let delta = config.resize_step * f64::from(direction.sign());
        match self.root.as_mut() {
            Some(root) => root.resize(
                focus,
                axis,
                delta,
                config.min_width,
                config.min_height,
                area,
            ),
            None => false,
        }
    }

    /// Move the focused pane towards `direction` by swapping with its neighbour.
    pub fn move_focused(&mut self, direction: Direction, area: Rect) -> bool {
        let Some(focus) = self.focused() else {
            return false;
        };
        let Some(root) = self.root.as_mut() else {
            return false;
        };
        match root.neighbour(focus, direction, area) {
            Some(neighbour) => root.swap(focus, neighbour),
            None => false,
        }
    }

    /// Panes showing `buffer`.
    pub fn panes_showing(&self, buffer: crate::buffer::BufferId) -> Vec<PaneId> {
        let mut out = Vec::new();
        if let Some(root) = &self.root {
            root.walk(&mut |pane| {
                if pane.buffer == buffer {
                    out.push(pane.id);
                }
            });
        }
        out
    }

    /// Replace every pane showing `old_buffer`.
    pub fn reassign_buffer(
        &mut self,
        old_buffer: crate::buffer::BufferId,
        new_buffer: crate::buffer::BufferId,
    ) {
        if let Some(root) = &mut self.root {
            root.walk_mut(&mut |pane| {
                if pane.buffer == old_buffer {
                    pane.buffer = new_buffer;
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::BufferId;

    fn pane(id: u32, buffer: u32) -> Pane {
        Pane::new(PaneId(id), BufferId(buffer))
    }

    fn area() -> Rect {
        Rect::new(0, 0, 100, 40)
    }

    #[test]
    fn split_halves_the_pane() {
        let mut root = Node::leaf(pane(1, 1));
        assert!(root.split_leaf(PaneId(1), Axis::Row, 0.5, pane(2, 1)));
        let rects = root.rect_map(area());
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0].1.height, 20);
        assert_eq!(rects[1].1.height, 20);
        assert_eq!(rects[1].1.y, 20);
    }

    #[test]
    fn column_split_divides_width() {
        let mut root = Node::leaf(pane(1, 1));
        assert!(root.split_leaf(PaneId(1), Axis::Column, 0.25, pane(2, 1)));
        let rects = root.rect_map(area());
        assert_eq!(rects[0].1.width, 25);
        assert_eq!(rects[1].1.width, 75);
    }

    #[test]
    fn split_refused_when_too_small() {
        let mut root = Node::leaf(pane(1, 1));
        let small = Rect::new(0, 0, 100, 4);
        let ok = root.can_split(PaneId(1), Axis::Row, 0.5, small, &LayoutConfig::default());
        assert!(!ok, "4 rows cannot host two 3-row panes");
        // The size guard lives in `can_split`; `split_leaf` is the operation
        // itself, so callers that skip the guard still get a (tiny) split.
        let big = Rect::new(0, 0, 100, 40);
        assert!(root.can_split(PaneId(1), Axis::Row, 0.5, big, &LayoutConfig::default()));
        assert!(root.split_leaf(PaneId(1), Axis::Row, 0.5, pane(2, 1)));
        assert_eq!(root.rect_map(big).len(), 2);
    }

    #[test]
    fn remove_falls_back_to_sibling() {
        let mut root = Node::leaf(pane(1, 1));
        assert!(root.split_leaf(PaneId(1), Axis::Row, 0.5, pane(2, 1)));
        assert_eq!(root.remove(PaneId(2)), Some(PaneId(1)));
        assert_eq!(root.pane_count(), 1);
        assert!(matches!(root, Node::Leaf(_)));
        assert_eq!(root.remove(PaneId(1)), None);
    }

    #[test]
    fn remove_reaches_nested_panes() {
        // Splitting the focused pane nests: Split(1, Split(2, 3)).  Closing a
        // pane inside the second branch used to stop at the first leaf and
        // remove nothing.
        let mut root = Node::leaf(pane(1, 1));
        assert!(root.split_leaf(PaneId(1), Axis::Row, 0.5, pane(2, 1)));
        assert!(root.split_leaf(PaneId(2), Axis::Column, 0.5, pane(3, 1)));
        assert_eq!(root.pane_count(), 3);
        assert!(root.remove(PaneId(3)).is_some());
        assert_eq!(root.pane_count(), 2);
        assert!(root.contains(PaneId(1)));
        assert!(root.contains(PaneId(2)));
        assert!(!root.contains(PaneId(3)));
        // And the middle pane of a three-pane tree, from the other side.
        assert!(root.remove(PaneId(2)).is_some());
        assert_eq!(root.pane_count(), 1);
    }

    #[test]
    fn layout_remove_focuses_a_surviving_neighbour() {
        // Closing the *first* pane used to pick the removed pane as the
        // neighbour (saturating at index 0) before falling back arbitrarily.
        let mut root = Node::leaf(pane(1, 1));
        assert!(root.split_leaf(PaneId(1), Axis::Row, 0.5, pane(2, 1)));
        assert!(root.split_leaf(PaneId(2), Axis::Row, 0.5, pane(3, 1)));
        let mut layout = Layout::new(root, PaneId(1));
        assert_eq!(layout.remove(PaneId(1)), Some(PaneId(2)));
        assert_eq!(layout.focused(), Some(PaneId(2)));
        assert_eq!(layout.panes(), vec![PaneId(2), PaneId(3)]);
    }

    #[test]
    fn auto_split_accounts_for_cell_aspect() {
        // Cells are about twice as tall as wide, so raw counts overstate the
        // horizontal room: 100x60 looks wide but is physically taller than
        // wide and must stack; only ~2x columns vs rows goes side by side.
        assert_eq!(Axis::auto(Rect::new(0, 0, 100, 60)), Axis::Row);
        assert_eq!(Axis::auto(Rect::new(0, 0, 100, 40)), Axis::Column);
        assert_eq!(Axis::auto(Rect::new(0, 0, 40, 40)), Axis::Row);
        assert_eq!(Axis::auto(Rect::new(0, 0, 160, 50)), Axis::Column);
    }

    #[test]
    fn golden_layout_favours_focus() {
        let mut root = Node::leaf(pane(1, 1));
        assert!(root.split_leaf(PaneId(1), Axis::Row, 0.5, pane(2, 1)));
        root.apply_golden(PaneId(2), 0.618, 0.618, 0.618);
        let rects = root.rect_map(area());
        assert_eq!(rects[0].1.height, 15, "unfocused pane shrinks");
        assert_eq!(rects[1].1.height, 25, "focused pane takes the golden share");

        root.apply_golden(PaneId(1), 0.618, 0.618, 0.618);
        let rects = root.rect_map(area());
        assert_eq!(rects[0].1.height, 25);
    }

    #[test]
    fn golden_sizes_every_split_not_just_the_focus_path() {
        // 1 | (2 / 3): focusing pane 1 goldenises the root split _and_ the
        // off-path sub-split (first child takes the larger share), so all
        // three panes move, not just the ones on the path.
        let config = LayoutConfig {
            split_mode: SplitMode::Golden,
            ..LayoutConfig::default()
        };
        let mut root = Node::leaf(pane(1, 1));
        assert!(root.split_leaf(PaneId(1), Axis::Column, 0.5, pane(2, 1)));
        assert!(root.split_leaf(PaneId(2), Axis::Row, 0.5, pane(3, 1)));
        let mut layout = Layout::new(root, PaneId(1));
        layout.apply_golden(&config);
        let rect = |layout: &Layout, id: u32| {
            layout
                .rects(area())
                .iter()
                .find(|(p, _)| p.0 == id)
                .unwrap()
                .1
        };
        assert_eq!(
            rect(&layout, 1).width,
            62,
            "focused pane takes the golden share"
        );
        let (h2, h3) = (rect(&layout, 2).height, rect(&layout, 3).height);
        assert_ne!(h2, h3, "off-path sub-split is goldenised, not equalised");
        assert_eq!((h2, h3), (25, 15), "first child takes the golden share");

        // Focus moving into the sub-split re-balances the whole tree too:
        // the focused pane takes the golden share of the window along each
        // axis it is split on, so the outer pane shrinks to the remainder
        // (one column split on the path: 1 - 0.618 of the width).
        layout.focus(PaneId(3));
        layout.apply_golden(&config);
        assert_eq!(rect(&layout, 3).height, 25, "0.618 of the window height");
        assert_eq!(rect(&layout, 1).width, 38, "outer pane shrinks");
    }

    #[test]
    fn golden_honours_requested_axis() {
        // Direction is the command's choice, sizing is the mode's: golden
        // stacks when the command asks for it, then takes the golden share
        // of the stacked axis.
        let wide = Rect::new(0, 0, 160, 50);
        let config = LayoutConfig {
            split_mode: SplitMode::Golden,
            ..LayoutConfig::default()
        };
        let mut layout = Layout::new(Node::leaf(pane(1, 1)), PaneId(1));
        let fresh = layout
            .split_focused(Some(Axis::Row), &config, wide, |id| pane(id.0, 1))
            .expect("golden split");
        let rects = layout.rects(wide);
        assert_eq!(rects.len(), 2);
        let (_, old) = rects.iter().find(|(id, _)| *id != fresh).unwrap();
        let (_, new) = rects.iter().find(|(id, _)| *id == fresh).unwrap();
        assert_eq!((old.width, new.width), (160, 160), "stacked, as asked");
        // The focused (new) pane keeps the golden share of the height.
        assert_eq!(new.height, 31, "0.618 of 50");
        assert_eq!(old.height, 19);
    }

    #[test]
    fn golden_split_auto_picks_the_roomier_axis() {
        // `split-auto` (no axis) in golden mode uses the same physical-room
        // heuristic as everywhere else: 160x50 is wider than tall.
        let wide = Rect::new(0, 0, 160, 50);
        let config = LayoutConfig {
            split_mode: SplitMode::Golden,
            ..LayoutConfig::default()
        };
        let mut layout = Layout::new(Node::leaf(pane(1, 1)), PaneId(1));
        let fresh = layout
            .split_focused(None, &config, wide, |id| pane(id.0, 1))
            .expect("golden split");
        let rects = layout.rects(wide);
        let (_, old) = rects.iter().find(|(id, _)| *id != fresh).unwrap();
        let (_, new) = rects.iter().find(|(id, _)| *id == fresh).unwrap();
        assert_eq!((old.height, new.height), (50, 50), "side by side");
        assert_eq!(new.width, 99, "0.618 of 160");
        assert_eq!(old.width, 61);
    }

    #[test]
    fn golden_room_check_uses_golden_shares() {
        // 30 columns cannot host two usable panes at the golden 61.8/38.2
        // split (the small half would be 11 < min-width 12) even though a
        // 50/50 split would fit, so split-auto falls back to the other axis...
        let config = LayoutConfig {
            split_mode: SplitMode::Golden,
            ..LayoutConfig::default()
        };
        let thin = Rect::new(0, 0, 30, 10);
        let mut layout = Layout::new(Node::leaf(pane(1, 1)), PaneId(1));
        let fresh = layout
            .split_focused(None, &config, thin, |id| pane(id.0, 1))
            .expect("falls back to the row axis");
        let rects = layout.rects(thin);
        let (_, old) = rects.iter().find(|(id, _)| *id != fresh).unwrap();
        let (_, new) = rects.iter().find(|(id, _)| *id == fresh).unwrap();
        assert_eq!(
            (old.width, new.width),
            (30, 30),
            "stacked, not side by side"
        );
        assert!(
            old.height >= 3 && new.height >= 3,
            "both halves stay usable"
        );

        // ...an explicit axis refuses rather than silently switching...
        let mut layout = Layout::new(Node::leaf(pane(1, 1)), PaneId(1));
        assert!(
            layout
                .split_focused(Some(Axis::Column), &config, thin, |id| pane(id.0, 1))
                .is_none(),
            "no room at golden sizes on the chosen axis"
        );
        assert_eq!(layout.panes(), vec![PaneId(1)]);

        // ...while equal, which really does bisect, still takes the columns.
        let config = LayoutConfig {
            split_mode: SplitMode::Equal,
            ..LayoutConfig::default()
        };
        let mut layout = Layout::new(Node::leaf(pane(1, 1)), PaneId(1));
        let fresh = layout
            .split_focused(None, &config, thin, |id| pane(id.0, 1))
            .expect("equal split");
        let rects = layout.rects(thin);
        let (_, old) = rects.iter().find(|(id, _)| *id != fresh).unwrap();
        assert_eq!(old.height, 10, "split-auto picks the column axis here");
    }

    #[test]
    fn golden_refuses_split_without_room() {
        // 30x6: neither axis can host the golden 61.8/38.2 shares
        // (11 < min-width 12, 2 < min-height 3), so the split is refused
        // even though a 50/50 split would fit.
        let tiny = Rect::new(0, 0, 30, 6);
        let config = LayoutConfig {
            split_mode: SplitMode::Golden,
            ..LayoutConfig::default()
        };
        let mut layout = Layout::new(Node::leaf(pane(1, 1)), PaneId(1));
        assert!(
            layout
                .split_focused(None, &config, tiny, |id| pane(id.0, 1))
                .is_none(),
            "no room at golden sizes"
        );
        assert_eq!(layout.panes(), vec![PaneId(1)]);
    }

    #[test]
    fn resize_adjusts_ratio() {
        let mut root = Node::leaf(pane(1, 1));
        assert!(root.split_leaf(PaneId(1), Axis::Column, 0.5, pane(2, 1)));
        assert!(root.resize(PaneId(1), Axis::Column, 0.1, 12, 3, area()));
        let rects = root.rect_map(area());
        assert_eq!(rects[0].1.width, 60);
        assert_eq!(rects[1].1.width, 40);
        // Wrong axis is refused rather than silently doing the wrong thing.
        assert!(!root.resize(PaneId(1), Axis::Row, 0.1, 12, 3, area()));
    }

    #[test]
    fn resize_clamps_to_minimum() {
        let mut root = Node::leaf(pane(1, 1));
        assert!(root.split_leaf(PaneId(1), Axis::Column, 0.5, pane(2, 1)));
        for _ in 0..40 {
            root.resize(PaneId(1), Axis::Column, 0.1, 20, 3, area());
        }
        let rects = root.rect_map(area());
        assert!(rects[1].1.width >= 20);
    }

    #[test]
    fn neighbour_navigation_is_geometric() {
        let mut root = Node::leaf(pane(1, 1));
        assert!(root.split_leaf(PaneId(1), Axis::Column, 0.5, pane(2, 1)));
        assert!(root.split_leaf(PaneId(2), Axis::Row, 0.5, pane(3, 1)));
        assert_eq!(
            root.neighbour(PaneId(1), Direction::Right, area()),
            Some(PaneId(2))
        );
        assert_eq!(
            root.neighbour(PaneId(2), Direction::Left, area()),
            Some(PaneId(1))
        );
        assert_eq!(
            root.neighbour(PaneId(2), Direction::Down, area()),
            Some(PaneId(3))
        );
        assert_eq!(
            root.neighbour(PaneId(3), Direction::Up, area()),
            Some(PaneId(2))
        );
        assert_eq!(root.neighbour(PaneId(1), Direction::Down, area()), None);
    }

    #[test]
    fn swap_moves_buffers_between_panes() {
        let mut root = Node::Split {
            axis: Axis::Row,
            ratio: 0.5,
            first: Box::new(Node::Leaf(pane(1, 10))),
            second: Box::new(Node::Leaf(pane(2, 20))),
        };
        assert!(root.swap(PaneId(1), PaneId(2)));
        assert_eq!(root.find(PaneId(1)).unwrap().buffer, BufferId(20));
        assert_eq!(root.find(PaneId(2)).unwrap().buffer, BufferId(10));
        // Pane identity (and viewport) stays where it was.
        assert_eq!(root.find(PaneId(1)).unwrap().id, PaneId(1));
    }

    #[test]
    fn three_way_split_rects_tile_the_area() {
        let mut root = Node::leaf(pane(1, 1));
        assert!(root.split_leaf(PaneId(1), Axis::Row, 0.5, pane(2, 2)));
        assert!(root.split_leaf(PaneId(1), Axis::Column, 0.5, pane(3, 3)));
        let rects = root.rect_map(area());
        let total: u64 = rects.iter().map(|(_, r)| r.area() as u64).sum();
        assert_eq!(total, 100 * 40);
    }

    #[test]
    fn golden_focus_takes_the_window_ratio_not_just_its_parent() {
        // 1 | 2 | 3 side by side: whichever pane is focused takes the
        // golden share of the *window*, so the leftmost pane shrinks to
        // the remainder when focus moves into the nested split instead of
        // ending up the same width as the focused pane.
        let wide = Rect::new(0, 0, 160, 50);
        let config = LayoutConfig {
            split_mode: SplitMode::Golden,
            ..LayoutConfig::default()
        };
        let mut layout = Layout::new(Node::leaf(pane(1, 1)), PaneId(1));
        let p2 = layout
            .split_focused(Some(Axis::Column), &config, wide, |id| pane(id.0, 1))
            .expect("first split");
        let p3 = layout
            .split_focused(Some(Axis::Column), &config, wide, |id| pane(id.0, 1))
            .expect("second split");
        let width = |layout: &Layout, id: PaneId| {
            layout
                .rects(wide)
                .iter()
                .find(|(p, _)| *p == id)
                .unwrap()
                .1
                .width
        };
        assert_eq!(
            width(&layout, p3),
            99,
            "focused pane takes 0.618 of the window"
        );
        assert_eq!(width(&layout, PaneId(1)), 34, "outer pane shrinks");

        layout.focus(PaneId(1));
        layout.on_focus_changed(&config);
        assert_eq!(
            width(&layout, PaneId(1)),
            99,
            "focus left: left grows to the golden share"
        );

        layout.focus(p2);
        layout.on_focus_changed(&config);
        assert_eq!(
            width(&layout, p2),
            99,
            "focus middle: focused pane takes the golden share"
        );
        assert_eq!(
            width(&layout, PaneId(1)),
            34,
            "focus middle: leftmost shrinks again"
        );
    }

    #[test]
    fn golden_auto_split_flips_axis_despite_inflated_focus() {
        // 200x55 golden: the first auto split goes side by side, and the
        // focused pane keeps the golden share of the window forever.  The
        // next auto split must still be able to stack, so the axis is
        // judged on the even-split rect, not the inflated live rect.
        let wide = Rect::new(0, 0, 200, 55);
        let config = LayoutConfig {
            split_mode: SplitMode::Golden,
            ..LayoutConfig::default()
        };
        let mut layout = Layout::new(Node::leaf(pane(1, 1)), PaneId(1));
        let p2 = layout
            .split_focused(None, &config, wide, |id| pane(id.0, 1))
            .expect("first auto split");
        let p3 = layout
            .split_focused(None, &config, wide, |id| pane(id.0, 1))
            .expect("second auto split");
        let rect = |layout: &Layout, id: PaneId| {
            layout.rects(wide).iter().find(|(p, _)| *p == id).unwrap().1
        };
        assert_eq!(
            rect(&layout, p2).width,
            rect(&layout, p3).width,
            "second auto split stacks instead of adding another column"
        );
        assert_eq!(rect(&layout, p3).height, 34, "focused half of the stack");
        assert_eq!(rect(&layout, p2).height, 21);
    }

    #[test]
    fn golden_nesting_one_axis_leaves_the_other_alone() {
        // A over B, then column-split A repeatedly: B's height must not
        // change as A gains columns.  A split's share depends only on the
        // splits of _its own axis_ along the focus path.
        let wide = Rect::new(0, 0, 200, 55);
        let config = LayoutConfig {
            split_mode: SplitMode::Golden,
            ..LayoutConfig::default()
        };
        let mut layout = Layout::new(Node::leaf(pane(1, 1)), PaneId(1));
        let b = layout
            .split_focused(Some(Axis::Row), &config, wide, |id| pane(id.0, 1))
            .expect("stacked split");
        layout.focus(PaneId(1));
        layout.on_focus_changed(&config);
        let height = |layout: &Layout, id: PaneId| {
            layout
                .rects(wide)
                .iter()
                .find(|(p, _)| *p == id)
                .unwrap()
                .1
                .height
        };
        let before = height(&layout, b);
        layout
            .split_focused(Some(Axis::Column), &config, wide, |id| pane(id.0, 1))
            .expect("first column split in A");
        assert_eq!(height(&layout, b), before, "B keeps its height");
        layout
            .split_focused(Some(Axis::Column), &config, wide, |id| pane(id.0, 1))
            .expect("second column split in A");
        assert_eq!(height(&layout, b), before, "B still keeps its height");
    }
}
