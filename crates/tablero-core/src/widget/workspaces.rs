//! The workspace widget and its normalized data model.
//!
//! [`Workspaces`] is the typed, normalized snapshot a producer feeds in through
//! [`Msg::Workspaces`](super::Msg::Workspaces); [`WorkspaceWidget`] renders it,
//! repainting only when the visible workspace set or the active workspace
//! actually changes.

use crate::render::{Bounds, FG, RenderContext};

use super::{Command, Msg, Widget};

/// Width, in pixels, of each workspace item's clickable/rendered cell.
///
/// Items are packed left-to-right from the widget's origin, each in a cell this
/// wide. Only these cells are interactive; the rest of the widget's slot is
/// empty space that ignores clicks.
const ITEM_WIDTH: u32 = 36;

/// A normalized snapshot of the Hyprland workspace set.
///
/// Normalization happens once, at the producer boundary, so widgets and the
/// redraw policy compare clean, canonical values: ids are sorted ascending and
/// deduplicated, and the `active` id is always part of the set (Hyprland can
/// report an active workspace a hair before it appears in the workspace list).
/// Equality is therefore a faithful "does this look different on screen?" test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspaces {
    ids: Vec<i32>,
    active: i32,
}

impl Workspaces {
    /// Build a snapshot from a raw id set and the active id, normalizing both.
    ///
    /// The `active` id is folded into the set, then the whole set is sorted and
    /// deduplicated. Pass the ids in any order, with or without `active`.
    pub fn new(ids: impl IntoIterator<Item = i32>, active: i32) -> Self {
        let mut ids: Vec<i32> = ids.into_iter().collect();
        ids.push(active);
        ids.sort_unstable();
        ids.dedup();
        Self { ids, active }
    }

    /// The normalized, sorted workspace ids.
    pub fn ids(&self) -> &[i32] {
        &self.ids
    }

    /// The active workspace id.
    pub fn active(&self) -> i32 {
        self.active
    }

    /// The display label: ids joined by spaces, with the active one bracketed.
    ///
    /// For example `1 [2] 3` — the brackets are how the bar distinguishes the
    /// active workspace from the rest. Keeping this a pure function makes the
    /// rendered text deterministic and unit-testable without painting pixels.
    pub fn label(&self) -> String {
        self.ids
            .iter()
            .map(|&id| {
                if id == self.active {
                    format!("[{id}]")
                } else {
                    id.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// A bar widget showing the workspace set with the active workspace marked.
///
/// Holds the last snapshot it was given so [`update`](Widget::update) can report
/// a visible change only when the normalized snapshot actually differs — a
/// repeated identical snapshot keeps the loop idle.
pub struct WorkspaceWidget {
    bounds: Bounds,
    state: Option<Workspaces>,
}

impl WorkspaceWidget {
    /// Create a workspace widget occupying `bounds`, empty until its first
    /// [`Msg::Workspaces`](super::Msg::Workspaces).
    pub fn new(bounds: Bounds) -> Self {
        Self {
            bounds,
            state: None,
        }
    }

    /// The currently displayed label (empty before the first snapshot).
    pub fn label(&self) -> String {
        self.state
            .as_ref()
            .map(Workspaces::label)
            .unwrap_or_default()
    }

    /// The per-item cells: each `(id, bounds)` pair is one workspace's slot.
    ///
    /// Items are laid out left-to-right from the widget origin in [`ITEM_WIDTH`]
    /// cells, clipped to the widget's slot. Both [`draw`](Widget::draw) and
    /// [`on_click`](Widget::on_click) read this, so what is painted and what is
    /// clickable are the same regions by construction.
    fn item_cells(&self) -> Vec<(i32, Bounds)> {
        let Some(state) = &self.state else {
            return Vec::new();
        };
        if self.bounds.width == 0 || self.bounds.height == 0 {
            return Vec::new();
        }

        let right = self.bounds.x + self.bounds.width;
        let mut cells = Vec::new();
        for (i, &id) in state.ids().iter().enumerate() {
            let x = self.bounds.x + ITEM_WIDTH * i as u32;
            if x >= right {
                // Ran out of room in the widget's slot; stop placing items.
                break;
            }
            let width = ITEM_WIDTH.min(right - x);
            cells.push((id, Bounds::new(x, self.bounds.y, width, self.bounds.height)));
        }
        cells
    }
}

impl Widget for WorkspaceWidget {
    fn update(&mut self, msg: &Msg) -> bool {
        match msg {
            Msg::Workspaces(next) => {
                if self.state.as_ref() == Some(next) {
                    return false;
                }
                self.state = Some(next.clone());
                true
            }
            _ => false,
        }
    }

    fn draw(&self, ctx: &mut RenderContext) {
        let active = self.state.as_ref().map(Workspaces::active);
        for (id, cell) in self.item_cells() {
            // The active workspace is bracketed to set it apart from the rest.
            let text = if Some(id) == active {
                format!("[{id}]")
            } else {
                id.to_string()
            };
            ctx.draw_text(&text, cell, FG);
        }
    }

    fn bounds(&self) -> Bounds {
        self.bounds
    }

    fn set_bounds(&mut self, bounds: Bounds) {
        self.bounds = bounds;
    }

    fn on_click(&self, px: u32, py: u32) -> Option<Command> {
        self.item_cells()
            .into_iter()
            .find(|(_, cell)| cell.contains(px, py))
            .map(|(id, _)| Command::SwitchWorkspace(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, TimeZone};

    fn ws(ids: impl IntoIterator<Item = i32>, active: i32) -> Msg {
        Msg::Workspaces(Workspaces::new(ids, active))
    }

    #[test]
    fn new_sorts_and_deduplicates_ids() {
        let w = Workspaces::new([3, 1, 2, 1, 3], 2);
        assert_eq!(w.ids(), &[1, 2, 3]);
        assert_eq!(w.active(), 2);
    }

    #[test]
    fn new_always_includes_the_active_workspace() {
        // Active reported but not yet present in the listed set.
        let w = Workspaces::new([1, 2], 5);
        assert_eq!(w.ids(), &[1, 2, 5]);
    }

    #[test]
    fn label_brackets_only_the_active_workspace() {
        assert_eq!(Workspaces::new([1, 2, 3], 2).label(), "1 [2] 3");
        assert_eq!(Workspaces::new([1, 2, 3], 1).label(), "[1] 2 3");
    }

    #[test]
    fn first_snapshot_changes_state() {
        let mut widget = WorkspaceWidget::new(Bounds::new(0, 0, 320, 32));
        assert_eq!(widget.label(), "");
        assert!(widget.update(&ws([1, 2, 3], 1)));
        assert_eq!(widget.label(), "[1] 2 3");
    }

    #[test]
    fn identical_snapshot_is_not_a_visible_change() {
        let mut widget = WorkspaceWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&ws([1, 2, 3], 1)));
        // Same set, same active, different input order — normalizes equal.
        assert!(!widget.update(&ws([3, 2, 1], 1)));
        assert_eq!(widget.label(), "[1] 2 3");
    }

    #[test]
    fn switching_active_workspace_is_a_visible_change() {
        let mut widget = WorkspaceWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&ws([1, 2, 3], 1)));
        assert!(widget.update(&ws([1, 2, 3], 2)));
        assert_eq!(widget.label(), "1 [2] 3");
    }

    #[test]
    fn unrelated_message_is_ignored() {
        let mut widget = WorkspaceWidget::new(Bounds::new(0, 0, 320, 32));
        widget.update(&ws([1], 1));
        let tick = Msg::Tick(Local.with_ymd_and_hms(2026, 6, 27, 8, 0, 0).unwrap());
        assert!(!widget.update(&tick));
        assert_eq!(widget.label(), "[1]");
    }

    #[test]
    fn set_bounds_repositions_the_widget() {
        let mut widget = WorkspaceWidget::new(Bounds::new(0, 0, 1, 1));
        widget.set_bounds(Bounds::new(10, 0, 200, 32));
        assert_eq!(widget.bounds(), Bounds::new(10, 0, 200, 32));
    }

    #[test]
    fn click_on_an_item_switches_to_that_workspace() {
        let mut widget = WorkspaceWidget::new(Bounds::new(0, 0, 320, 32));
        widget.update(&ws([1, 2, 3], 1));
        // Items are 36px-wide cells packed from the origin: 1 -> [0,36),
        // 2 -> [36,72), 3 -> [72,108).
        assert_eq!(widget.on_click(0, 0), Some(Command::SwitchWorkspace(1)));
        assert_eq!(widget.on_click(50, 16), Some(Command::SwitchWorkspace(2)));
        assert_eq!(widget.on_click(80, 31), Some(Command::SwitchWorkspace(3)));
    }

    #[test]
    fn click_on_empty_space_past_the_items_is_ignored() {
        let mut widget = WorkspaceWidget::new(Bounds::new(0, 0, 320, 32));
        widget.update(&ws([1, 2, 3], 1));
        // Past the third item's cell (ends at 108) there is only empty slot.
        assert_eq!(widget.on_click(200, 16), None);
    }

    #[test]
    fn click_before_the_first_snapshot_is_ignored() {
        let widget = WorkspaceWidget::new(Bounds::new(0, 0, 320, 32));
        assert_eq!(widget.on_click(0, 0), None);
    }

    #[test]
    fn click_respects_the_widget_offset() {
        let mut widget = WorkspaceWidget::new(Bounds::new(100, 0, 220, 32));
        widget.update(&ws([1, 2], 1));
        // Cells start at the widget origin: 1 -> [100,136), 2 -> [136,172).
        assert_eq!(widget.on_click(90, 0), None);
        assert_eq!(widget.on_click(110, 0), Some(Command::SwitchWorkspace(1)));
        assert_eq!(widget.on_click(150, 0), Some(Command::SwitchWorkspace(2)));
    }

    #[test]
    fn items_are_clipped_to_the_widget_slot() {
        // A slot only wide enough for one-and-a-bit cells drops the overflow.
        let mut widget = WorkspaceWidget::new(Bounds::new(0, 0, 40, 32));
        widget.update(&ws([1, 2, 3], 1));
        assert_eq!(widget.on_click(10, 0), Some(Command::SwitchWorkspace(1)));
        // Second item would start at x=36, within the 40px slot, clipped to 4px.
        assert_eq!(widget.on_click(38, 0), Some(Command::SwitchWorkspace(2)));
        // Third item would start at x=72, past the slot: never placed.
        assert_eq!(widget.on_click(39, 0), Some(Command::SwitchWorkspace(2)));
    }
}
