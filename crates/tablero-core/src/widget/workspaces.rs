//! The workspace widget and its normalized data model.
//!
//! [`Workspaces`] is the typed, normalized snapshot a producer feeds in through
//! [`Msg::Workspaces`](super::Msg::Workspaces); [`WorkspaceWidget`] renders it,
//! repainting only when the visible workspace set or the active workspace
//! actually changes.

use std::collections::BTreeMap;

use crate::render::{BG, Bounds, FG, RenderContext};

use super::{Command, Msg, Widget, WidgetStyle, draw_centered};

/// The projection of a snapshot that a single widget renders: the ids to show,
/// in order, and which one (if any) is active in that widget's scope.
///
/// A widget scoped to a monitor and one with no scope (the global fallback)
/// both reduce to a `View`; the redraw policy compares `View`s, so a widget
/// repaints only when *its* slice of the world changes — a switch on another
/// monitor leaves a monitor-scoped widget's `View` untouched.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct View {
    ids: Vec<i32>,
    active: Option<i32>,
}

impl View {
    /// The display label: ids joined by spaces, with the active one bracketed
    /// (e.g. `1 [2] 3`).
    fn label(&self) -> String {
        self.ids
            .iter()
            .map(|&id| {
                if Some(id) == self.active {
                    format!("[{id}]")
                } else {
                    id.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// A normalized, monitor-aware snapshot of the Hyprland workspace set.
///
/// Normalization happens once, at the producer boundary, so widgets and the
/// redraw policy compare clean, canonical values: within any scope ids are
/// sorted ascending and deduplicated, and an active id is always part of its
/// scope's set (Hyprland can report an active workspace a hair before it
/// appears in the workspace list).
///
/// Each workspace carries the monitor (connector name) that owns it, and each
/// monitor reports its own active workspace — Hyprland binds workspaces to
/// monitors, so a per-output bar shows only its monitor's workspaces and
/// highlights that monitor's active one. A widget with no monitor scope sees
/// the union of every monitor and the globally focused active workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspaces {
    /// Each workspace id paired with its owning monitor, if known.
    entries: Vec<(i32, Option<String>)>,
    /// The sorted, deduplicated union of every workspace id (the global view).
    ids: Vec<i32>,
    /// Each monitor's active workspace, by connector name.
    active_by_monitor: BTreeMap<String, i32>,
    /// The globally focused active workspace (the global view's highlight).
    active: i32,
}

impl Workspaces {
    /// Build a snapshot from a flat id set and the focused active id, with no
    /// per-monitor information.
    ///
    /// Used where monitor data is unavailable; every widget sees the same set.
    /// The `active` id is folded in, then the set is sorted and deduplicated —
    /// pass the ids in any order, with or without `active`.
    pub fn new(ids: impl IntoIterator<Item = i32>, active: i32) -> Self {
        let mut entries: Vec<(i32, Option<String>)> =
            ids.into_iter().map(|id| (id, None)).collect();
        entries.push((active, None));
        Self::build(entries, BTreeMap::new(), active)
    }

    /// Build a monitor-aware snapshot.
    ///
    /// `workspaces` pairs each workspace id with its owning monitor; `actives`
    /// gives each monitor's active workspace; `focused` is the globally focused
    /// active workspace (used by an unscoped widget). Every monitor's active id
    /// is folded into its own set, and `focused` into the global set.
    pub fn with_monitors<S: Into<String>>(
        workspaces: impl IntoIterator<Item = (i32, S)>,
        actives: impl IntoIterator<Item = (S, i32)>,
        focused: i32,
    ) -> Self {
        let active_by_monitor: BTreeMap<String, i32> = actives
            .into_iter()
            .map(|(monitor, id)| (monitor.into(), id))
            .collect();

        let mut entries: Vec<(i32, Option<String>)> = workspaces
            .into_iter()
            .map(|(id, monitor)| (id, Some(monitor.into())))
            .collect();
        // Fold each monitor's active workspace into its own set, in case the
        // active id leads the listed set.
        for (monitor, &id) in &active_by_monitor {
            entries.push((id, Some(monitor.clone())));
        }
        entries.push((focused, None));

        Self::build(entries, active_by_monitor, focused)
    }

    /// Normalize raw entries into a snapshot: dedup `(id, monitor)` pairs and
    /// precompute the sorted global id union.
    fn build(
        mut entries: Vec<(i32, Option<String>)>,
        active_by_monitor: BTreeMap<String, i32>,
        active: i32,
    ) -> Self {
        entries.sort();
        entries.dedup();

        let mut ids: Vec<i32> = entries.iter().map(|(id, _)| *id).collect();
        ids.sort_unstable();
        ids.dedup();

        Self {
            entries,
            ids,
            active_by_monitor,
            active,
        }
    }

    /// The normalized, sorted union of every workspace id (the global view).
    pub fn ids(&self) -> &[i32] {
        &self.ids
    }

    /// The globally focused active workspace id.
    pub fn active(&self) -> i32 {
        self.active
    }

    /// The sorted, deduplicated ids owned by `monitor`.
    pub fn ids_for(&self, monitor: &str) -> Vec<i32> {
        let mut ids: Vec<i32> = self
            .entries
            .iter()
            .filter(|(_, m)| m.as_deref() == Some(monitor))
            .map(|(id, _)| *id)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// The active workspace on `monitor`, if that monitor reported one.
    pub fn active_for(&self, monitor: &str) -> Option<i32> {
        self.active_by_monitor.get(monitor).copied()
    }

    /// The view a widget with the given monitor scope renders.
    ///
    /// A `Some` scope filters to that monitor's ids and active workspace; `None`
    /// is the global fallback — every id, focused active.
    fn view(&self, monitor: Option<&str>) -> View {
        match monitor {
            Some(m) => View {
                ids: self.ids_for(m),
                active: self.active_for(m),
            },
            None => View {
                ids: self.ids.clone(),
                active: Some(self.active),
            },
        }
    }

    /// The display label of the global view, with the active workspace
    /// bracketed (e.g. `1 [2] 3`).
    pub fn label(&self) -> String {
        self.view(None).label()
    }
}

/// A bar widget showing a monitor's workspace set with its active workspace
/// marked.
///
/// Holds the last snapshot it was given so [`update`](Widget::update) can report
/// a visible change only when its [`View`] actually differs — a repeated
/// identical view, or a change confined to another monitor, keeps the loop idle.
pub struct WorkspaceWidget {
    bounds: Bounds,
    /// The monitor (connector name) this widget is scoped to, or `None` for the
    /// global fallback that shows every monitor's workspaces.
    monitor: Option<String>,
    state: Option<Workspaces>,
    style: WidgetStyle,
}

impl WorkspaceWidget {
    /// Create an unscoped workspace widget occupying `bounds`, empty until its
    /// first [`Msg::Workspaces`](super::Msg::Workspaces). It shows the global
    /// workspace set across every monitor, with the default (flat) style.
    pub fn new(bounds: Bounds) -> Self {
        Self {
            bounds,
            monitor: None,
            state: None,
            style: WidgetStyle::default(),
        }
    }

    /// Create a workspace widget scoped to one `monitor` (connector name): it
    /// shows only that monitor's workspaces and highlights that monitor's
    /// active workspace. Carries the default (flat) style until one is set.
    pub fn for_monitor(bounds: Bounds, monitor: impl Into<String>) -> Self {
        Self {
            bounds,
            monitor: Some(monitor.into()),
            state: None,
            style: WidgetStyle::default(),
        }
    }

    /// Set the resolved visual style, consuming and returning `self` so it
    /// chains off [`new`](WorkspaceWidget::new) or
    /// [`for_monitor`](WorkspaceWidget::for_monitor) at build time.
    pub fn with_style(mut self, style: WidgetStyle) -> Self {
        self.style = style;
        self
    }

    /// The current view this widget renders (empty before the first snapshot).
    fn view(&self) -> View {
        self.state
            .as_ref()
            .map(|state| state.view(self.monitor.as_deref()))
            .unwrap_or_default()
    }

    /// The currently displayed label (empty before the first snapshot).
    pub fn label(&self) -> String {
        self.view().label()
    }

    /// The per-item cells: each `(id, bounds)` pair is one workspace's slot.
    ///
    /// Items are laid out left-to-right from the widget origin in square cells as
    /// wide as the widget is tall (so each scales with the bar height and the
    /// output's pixel density, since layout runs in physical pixels), clipped to
    /// the widget's slot. Both [`draw`](Widget::draw) and
    /// [`on_click`](Widget::on_click) read this, so what is painted and what is
    /// clickable are the same regions by construction — and the height-derived
    /// extent keeps [`on_click`](Widget::on_click) free of any render context.
    fn item_cells(&self) -> Vec<(i32, Bounds)> {
        let side = self.bounds.height;
        if self.bounds.width == 0 || side == 0 {
            return Vec::new();
        }

        let right = self.bounds.x + self.bounds.width;
        let mut cells = Vec::new();
        for (i, id) in self.view().ids.into_iter().enumerate() {
            let x = self.bounds.x + side * i as u32;
            if x >= right {
                // Ran out of room in the widget's slot; stop placing items.
                break;
            }
            let width = side.min(right - x);
            cells.push((id, Bounds::new(x, self.bounds.y, width, self.bounds.height)));
        }
        cells
    }
}

/// Light or dark text, whichever reads more clearly over the pill `fill` —
/// chosen by the fill's perceived luminance (Rec. 601). So an active workspace's
/// id stays legible whatever accent the theme paints behind it, without the
/// style having to carry a separate "text over the active pill" color.
fn contrast_color(fill: (u8, u8, u8, u8)) -> (u8, u8, u8, u8) {
    let (r, g, b, _) = fill;
    let luma = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
    if luma >= 140.0 { BG } else { FG }
}

impl Widget for WorkspaceWidget {
    fn update(&mut self, msg: &Msg) -> bool {
        match msg {
            Msg::Workspaces(next) => {
                let next_view = next.view(self.monitor.as_deref());
                if self.state.as_ref().map(|s| s.view(self.monitor.as_deref())) == Some(next_view) {
                    return false;
                }
                self.state = Some(next.clone());
                true
            }
            _ => false,
        }
    }

    fn measure(&self, _ctx: &mut RenderContext, height: u32) -> u32 {
        // One square cell per workspace, each as wide as the row is tall, matching
        // the cells [`item_cells`](WorkspaceWidget::item_cells) will draw (which
        // size off the laid-out `bounds.height` this same value becomes).
        self.view().ids.len() as u32 * height
    }

    fn draw(&self, ctx: &mut RenderContext) {
        let active = self.view().active;
        let radius = (self.style.radius * ctx.scale_factor()) as f32;
        for (id, cell) in self.item_cells() {
            let is_active = Some(id) == active;
            let (text, color) = match self.style.background {
                // Pills enabled (a background is configured): the active item
                // fills the accent pill, the rest the base background, and the id
                // reads in whichever of light/dark contrasts with that fill.
                Some(base) => {
                    let fill = if is_active { self.style.accent } else { base };
                    ctx.fill_rounded_rect(cell, fill, radius);
                    (id.to_string(), contrast_color(fill))
                }
                // Flat default (no background): the active id is bracketed and
                // accent-colored, the rest plain foreground — no pill, identical
                // to the bare bar before any styling.
                None if is_active => (format!("[{id}]"), self.style.accent),
                None => (id.to_string(), self.style.foreground),
            };
            if let Some(border) = self.style.border {
                ctx.stroke_rounded_rect(
                    cell,
                    border,
                    radius,
                    (self.style.border_width * ctx.scale_factor()) as f32,
                );
            }
            draw_centered(ctx, &text, cell, color);
        }
    }

    fn bounds(&self) -> Bounds {
        self.bounds
    }

    fn set_bounds(&mut self, bounds: Bounds) {
        self.bounds = bounds;
    }

    fn on_click(&self, px: u32, py: u32, button: super::ClickButton) -> Option<Command> {
        // Single-action widget: only the primary button switches workspaces.
        if button != super::ClickButton::Left {
            return None;
        }
        self.item_cells()
            .into_iter()
            .find(|(_, cell)| cell.contains(px, py))
            .map(|(id, _)| Command::SwitchWorkspace(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widget::ClickButton;
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
        // Square cells as wide as the 32px-tall widget, packed from the origin:
        // 1 -> [0,32), 2 -> [32,64), 3 -> [64,96).
        assert_eq!(
            widget.on_click(0, 0, ClickButton::Left),
            Some(Command::SwitchWorkspace(1))
        );
        assert_eq!(
            widget.on_click(50, 16, ClickButton::Left),
            Some(Command::SwitchWorkspace(2))
        );
        assert_eq!(
            widget.on_click(80, 31, ClickButton::Left),
            Some(Command::SwitchWorkspace(3))
        );
    }

    #[test]
    fn click_on_empty_space_past_the_items_is_ignored() {
        let mut widget = WorkspaceWidget::new(Bounds::new(0, 0, 320, 32));
        widget.update(&ws([1, 2, 3], 1));
        // Past the third item's cell (ends at 96) there is only empty slot.
        assert_eq!(widget.on_click(200, 16, ClickButton::Left), None);
    }

    #[test]
    fn click_before_the_first_snapshot_is_ignored() {
        let widget = WorkspaceWidget::new(Bounds::new(0, 0, 320, 32));
        assert_eq!(widget.on_click(0, 0, ClickButton::Left), None);
    }

    #[test]
    fn click_respects_the_widget_offset() {
        let mut widget = WorkspaceWidget::new(Bounds::new(100, 0, 220, 32));
        widget.update(&ws([1, 2], 1));
        // Cells start at the widget origin: 1 -> [100,132), 2 -> [132,164).
        assert_eq!(widget.on_click(90, 0, ClickButton::Left), None);
        assert_eq!(
            widget.on_click(110, 0, ClickButton::Left),
            Some(Command::SwitchWorkspace(1))
        );
        assert_eq!(
            widget.on_click(150, 0, ClickButton::Left),
            Some(Command::SwitchWorkspace(2))
        );
    }

    #[test]
    fn items_are_clipped_to_the_widget_slot() {
        // A slot only wide enough for one-and-a-bit cells drops the overflow.
        let mut widget = WorkspaceWidget::new(Bounds::new(0, 0, 40, 32));
        widget.update(&ws([1, 2, 3], 1));
        assert_eq!(
            widget.on_click(10, 0, ClickButton::Left),
            Some(Command::SwitchWorkspace(1))
        );
        // Second item starts at x=32, within the 40px slot, clipped to 8px.
        assert_eq!(
            widget.on_click(38, 0, ClickButton::Left),
            Some(Command::SwitchWorkspace(2))
        );
        // Third item would start at x=64, past the slot: never placed.
        assert_eq!(
            widget.on_click(39, 0, ClickButton::Left),
            Some(Command::SwitchWorkspace(2))
        );
    }

    #[test]
    fn contrast_color_picks_dark_over_light_and_light_over_dark() {
        // A bright fill takes dark text; a dark fill takes light text, so an id
        // stays legible over whatever accent the theme paints.
        assert_eq!(contrast_color((0xEA, 0xEA, 0xEA, 0xFF)), BG);
        assert_eq!(contrast_color((0x20, 0x20, 0x20, 0xFF)), FG);
    }

    #[test]
    fn a_styled_workspace_fills_the_active_pill_with_the_accent() {
        // A configured background switches the widget to pills: the active cell
        // is filled with the accent (here pure red), not left flat.
        let style = WidgetStyle {
            background: Some((0x30, 0x30, 0x30, 0xFF)),
            accent: (0xFF, 0x00, 0x00, 0xFF),
            ..WidgetStyle::default()
        };
        let mut widget = WorkspaceWidget::new(Bounds::new(0, 0, 64, 32)).with_style(style);
        widget.update(&ws([1, 2], 1)); // 1 is active

        let mut ctx = RenderContext::new(64, 32);
        ctx.fill_background();
        widget.draw(&mut ctx);
        // The active cell is [0,32); a point on its left-middle edge (clear of the
        // centered id glyph) is solidly the red accent fill.
        let px = ctx.pixels();
        let p = (16 * 64 + 4) * 4;
        assert!(
            px[p] > 0xC0 && px[p + 1] < 0x40 && px[p + 2] < 0x40,
            "active pill not accent-filled"
        );
    }

    #[test]
    fn the_flat_default_paints_no_pill_behind_the_active_workspace() {
        // The default style has no background, so the active workspace draws as
        // bracketed text over the bare bar — the cell corner stays background.
        let mut widget = WorkspaceWidget::new(Bounds::new(0, 0, 64, 32));
        widget.update(&ws([1, 2], 1));

        let mut ctx = RenderContext::new(64, 32);
        ctx.fill_background();
        widget.draw(&mut ctx);
        // The active cell's top-left corner holds no pill fill: it is bare bar.
        let px = ctx.pixels();
        assert_eq!(&px[0..4], &[BG.0, BG.1, BG.2, BG.3]);
    }

    // --- Monitor-aware snapshots ----------------------------------------------

    fn multi() -> Workspaces {
        // DP-1 owns 1,2 (active 2); HDMI-A-1 owns 3,4 (active 3); focused is 2.
        Workspaces::with_monitors(
            [(1, "DP-1"), (2, "DP-1"), (3, "HDMI-A-1"), (4, "HDMI-A-1")],
            [("DP-1", 2), ("HDMI-A-1", 3)],
            2,
        )
    }

    #[test]
    fn ids_for_returns_only_that_monitors_workspaces() {
        let w = multi();
        assert_eq!(w.ids_for("DP-1"), vec![1, 2]);
        assert_eq!(w.ids_for("HDMI-A-1"), vec![3, 4]);
        // An unknown monitor has no workspaces.
        assert!(w.ids_for("eDP-1").is_empty());
    }

    #[test]
    fn active_for_returns_that_monitors_active_workspace() {
        let w = multi();
        assert_eq!(w.active_for("DP-1"), Some(2));
        assert_eq!(w.active_for("HDMI-A-1"), Some(3));
        assert_eq!(w.active_for("eDP-1"), None);
    }

    #[test]
    fn the_global_view_still_spans_every_monitor() {
        let w = multi();
        assert_eq!(w.ids(), &[1, 2, 3, 4]);
        assert_eq!(w.active(), 2);
    }

    #[test]
    fn a_monitor_scoped_widget_shows_only_its_monitor() {
        let mut widget = WorkspaceWidget::for_monitor(Bounds::new(0, 0, 320, 32), "HDMI-A-1");
        assert!(widget.update(&Msg::Workspaces(multi())));
        // HDMI-A-1 owns 3,4 with 3 active — DP-1's 1,2 never appear here.
        assert_eq!(widget.label(), "[3] 4");
    }

    #[test]
    fn a_monitor_scoped_widget_clicks_its_own_ids() {
        let widget = {
            let mut w = WorkspaceWidget::for_monitor(Bounds::new(0, 0, 320, 32), "HDMI-A-1");
            w.update(&Msg::Workspaces(multi()));
            w
        };
        // Cells pack from the origin over this monitor's ids: 3 -> [0,32), 4 -> [32,64).
        assert_eq!(
            widget.on_click(0, 0, ClickButton::Left),
            Some(Command::SwitchWorkspace(3))
        );
        assert_eq!(
            widget.on_click(50, 0, ClickButton::Left),
            Some(Command::SwitchWorkspace(4))
        );
    }

    #[test]
    fn another_monitors_change_is_not_a_visible_change_here() {
        let mut widget = WorkspaceWidget::for_monitor(Bounds::new(0, 0, 320, 32), "DP-1");
        assert!(widget.update(&Msg::Workspaces(multi())));
        // HDMI-A-1 switches its active 3 -> 4; DP-1's view (1,2 active 2) is unchanged.
        let other_moved = Workspaces::with_monitors(
            [(1, "DP-1"), (2, "DP-1"), (3, "HDMI-A-1"), (4, "HDMI-A-1")],
            [("DP-1", 2), ("HDMI-A-1", 4)],
            4,
        );
        assert!(!widget.update(&Msg::Workspaces(other_moved)));
        assert_eq!(widget.label(), "1 [2]");
    }

    #[test]
    fn this_monitors_change_is_a_visible_change() {
        let mut widget = WorkspaceWidget::for_monitor(Bounds::new(0, 0, 320, 32), "DP-1");
        assert!(widget.update(&Msg::Workspaces(multi())));
        // DP-1 switches its active 2 -> 1.
        let moved = Workspaces::with_monitors(
            [(1, "DP-1"), (2, "DP-1"), (3, "HDMI-A-1"), (4, "HDMI-A-1")],
            [("DP-1", 1), ("HDMI-A-1", 3)],
            1,
        );
        assert!(widget.update(&Msg::Workspaces(moved)));
        assert_eq!(widget.label(), "[1] 2");
    }

    #[test]
    fn an_unfiltered_widget_shows_the_global_set_from_monitor_data() {
        // A widget with no monitor scope (the fallback) still renders everything.
        let mut widget = WorkspaceWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&Msg::Workspaces(multi())));
        assert_eq!(widget.label(), "1 [2] 3 4");
    }
}
