//! Wayland layer-shell front-end for tablero.
//!
//! Opens one top-anchored `wlr-layer-shell` surface **per output** under a
//! compositor such as Hyprland — tracking output hotplug to add and remove bars
//! as monitors come and go — renders a live clock through a shared-memory buffer,
//! and drives redraws from a [`calloop`] timer so the loop only wakes for clock
//! ticks, compositor events (output lifecycle, configure, scale), or shutdown —
//! never a busy redraw loop. Each output's bar is configured independently (see
//! [`outputs`]); producer messages and ticks fan out to every surface.
//!
//! Surface geometry is kept in logical pixels (the layer-shell size request and
//! exclusive zone), while the shared-memory buffer is allocated at the output's
//! physical pixel density: on a scaled output the buffer is `scale`× larger and
//! `set_buffer_scale` maps it back, so the bar stays crisp on HiDPI displays.
//! The logical-to-physical conversion lives entirely in [`crate::scale`].

pub mod blit;
pub mod clock;
pub mod config;
pub mod icon;
pub mod render;
pub mod scale;
pub mod widget;

pub mod backlight;
pub mod bluetooth;
pub mod command;
pub mod hypridle;
pub mod hyprland;
pub mod networkmanager;
pub mod notifications;
pub mod outputs;
pub mod power_profiles;
pub mod producer;
pub mod sni;
pub mod sysmon;
pub mod updates;
pub mod upower;
pub mod volume;

use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::blit::write_argb8888;
use crate::clock::millis_until_next_minute;
use crate::config::{Config, WidgetKind};
use crate::render::{Bounds, RenderContext, RenderSettings, SharedFonts, shared_fonts};
use crate::scale::Scale;
use crate::widget::{
    ClickButton, Command, Dashboard, Msg, ScrollDirection, Tooltip, TrayMenu, TrayMenuItem,
    TrayMenuToggleKind, TrayMenuToggleState,
};
use calloop::EventLoop;
use calloop::channel::Event as ChannelEvent;
use calloop::timer::{TimeoutAction, Timer};
use calloop_wayland_source::WaylandSource;
use log::{debug, error, info, warn};
use smithay_client_toolkit::reexports::protocols::xdg::shell::client::xdg_positioner;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_pointer, delegate_registry,
    delegate_seat, delegate_shm, delegate_xdg_popup, delegate_xdg_shell,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        pointer::{
            BTN_LEFT, BTN_RIGHT, CursorIcon, PointerEvent, PointerEventKind, PointerHandler,
            ThemeSpec, ThemedPointer,
        },
    },
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        xdg::{
            XdgPositioner, XdgShell,
            popup::{Popup, PopupConfigure, PopupHandler},
            window::{Window, WindowConfigure, WindowHandler},
        },
    },
    shm::{
        Shm, ShmHandler,
        slot::{Buffer, SlotPool},
    },
};

use crate::backlight::BacklightProducer;
use crate::bluetooth::BluetoothProducer;
use crate::command::{CommandSender, command_channel};
use crate::hypridle::HypridleProducer;
use crate::hyprland::HyprlandProducer;
use crate::networkmanager::NetworkProducer;
use crate::notifications::NotificationsProducer;
use crate::outputs::{OutputId, Outputs};
use crate::power_profiles::PowerProfilesProducer;
use crate::producer::{Producer, ProducerBridge};
use crate::sni::SniHostProducer;
use crate::sysmon::SystemProducer;
use crate::updates::UpdatesProducer;
use crate::upower::UPowerProducer;
use crate::volume::VolumeProducer;
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_output, wl_pointer, wl_region, wl_seat, wl_shm, wl_surface},
};

/// Layer-shell namespace (also the compositor-visible surface name).
const NAMESPACE: &str = "tablero";

/// Assumed width (px) for the initial shared-memory pool, before the compositor
/// reports the real output width via the first configure event.
const INITIAL_WIDTH: u32 = 1920;

/// One output's bar: its layer-shell surface plus the per-output render state.
///
/// The app holds one of these per Wayland output (see [`Outputs`]); each is
/// configured independently from the output's resolved [`Config`] and drawn into
/// the app's shared [`SlotPool`]. Geometry is kept in logical pixels; the buffer
/// is `scale`× larger and `set_buffer_scale` maps it back, exactly as the
/// single-surface bar did.
struct Surface {
    /// The output this surface is pinned to, so a `closed` event can find and
    /// drop just this bar.
    output_id: OutputId,
    output: wl_output::WlOutput,
    layer: LayerSurface,
    /// Surface dimensions in *logical* pixels (as the compositor reports them in
    /// `configure`). The shared-memory buffer is `scale`× larger; see
    /// [`Surface::draw`].
    width: u32,
    height: u32,
    /// The output's integer buffer scale. Drives the physical buffer size and the
    /// physical font size; `1` until the compositor reports otherwise.
    scale: Scale,
    /// This output's resolved configuration, retained so the physical font size
    /// can be re-resolved whenever the output scale changes.
    config: Config,
    /// Connector name (`DP-1`, …) for workspace scoping and config reloads.
    monitor: Option<String>,
    /// Widgets composing the bar, plus the dirty-flag redraw policy over them.
    dashboard: Dashboard,
    /// Reused software-render target (shared fonts + pixmap).
    ctx: RenderContext,
    /// Alternating SHM buffers so a free slot is reused without a new mmap when
    /// the compositor has released the previous frame.
    buffers: [Option<Buffer>; 2],
    /// Physical pixel size of the slots in [`buffers`].
    buffer_px: (u32, u32),
    /// Next double-buffer index to try (0 or 1).
    next_buffer: usize,
    /// Set once the first configure has been received; drawing before that is
    /// invalid per the layer-shell protocol.
    configured: bool,
}

impl Surface {
    /// Build a bar pinned to `output`, configured from `config`.
    ///
    /// `monitor` is the output's connector name (`DP-1`, …), threaded into the
    /// dashboard so this surface's workspace widget shows only this monitor's
    /// workspaces and highlights its active one. `fonts` is the process-wide
    /// shared font set.
    #[allow(clippy::too_many_arguments)] // Wayland + config + shared fonts seed
    fn new(
        compositor: &CompositorState,
        layer_shell: &LayerShell,
        qh: &QueueHandle<App>,
        output: &wl_output::WlOutput,
        output_id: OutputId,
        monitor: Option<&str>,
        config: Config,
        fonts: SharedFonts,
    ) -> Self {
        // The bar reserves exactly its own height so windows tile beneath it.
        let height = config.height;
        let exclusive_zone = height as i32;

        let wl_surface = compositor.create_surface(qh);
        // Pinning the layer surface to `output` is what makes this one bar per
        // monitor instead of the compositor's default-output single surface.
        let layer = layer_shell.create_layer_surface(
            qh,
            wl_surface,
            Layer::Top,
            Some(NAMESPACE.to_string()),
            Some(output),
        );
        // Top bar spanning the full output width.
        layer.set_anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        // Width 0 with left+right anchors lets the compositor stretch us to fit.
        layer.set_size(0, height);
        layer.set_exclusive_zone(exclusive_zone);
        // Initial commit with no buffer; the compositor replies with a configure.
        layer.commit();

        // The configured widget order drives which widgets are built and in what
        // order; `Dashboard::layout` tiles them into columns each frame, so these
        // initial bounds are just placeholders. The theme and font reach the
        // renderer through the context's settings.
        let full = Bounds::new(0, 0, INITIAL_WIDTH, height);
        let dashboard = config.build_dashboard(full, monitor);
        let ctx = RenderContext::with_fonts(INITIAL_WIDTH, height, config.render_settings(), fonts);

        Self {
            output_id,
            output: output.clone(),
            layer,
            width: INITIAL_WIDTH,
            height,
            // The compositor reports the real scale via `scale_factor_changed`,
            // typically before the first configure; until then assume an
            // unscaled output.
            scale: Scale::ONE,
            config,
            monitor: monitor.map(str::to_owned),
            dashboard,
            ctx,
            buffers: [None, None],
            buffer_px: (0, 0),
            next_buffer: 0,
            configured: false,
        }
    }

    /// Rebuild layout and visuals from a new config (hot-reload).
    fn apply_config(&mut self, config: Config, pool: &mut SlotPool) {
        let height = config.height;
        if self.height != height {
            self.height = height;
            self.layer.set_size(0, height);
            self.layer.set_exclusive_zone(height as i32);
            // Size change requests a new configure; still repaint at the current
            // geometry so theme/widget changes appear immediately.
            self.layer.commit();
        }
        self.config = config;
        let full = Bounds::new(0, 0, self.width.max(1), self.height.max(1));
        self.dashboard = self.config.build_dashboard(full, self.monitor.as_deref());
        self.ctx
            .set_settings(self.config.scaled_render_settings(self.scale));
        // Drop cached SHM slots so a height change cannot reuse a wrong size.
        self.buffers = [None, None];
        self.buffer_px = (0, 0);
        if self.configured {
            self.draw(pool);
        }
    }

    /// Whether this surface owns `wl_surface` — for routing pointer and scale
    /// events, which arrive keyed by the raw surface.
    fn owns(&self, wl_surface: &wl_surface::WlSurface) -> bool {
        self.layer.wl_surface() == wl_surface
    }

    /// Whether this surface is driven by `layer` — for routing configure and
    /// close events, which arrive keyed by the layer surface.
    fn is_layer(&self, layer: &LayerSurface) -> bool {
        self.layer.wl_surface() == layer.wl_surface()
    }

    /// Apply a message to the dashboard; redraw only if a widget reported a
    /// visible change. This is the steady-state redraw policy: the loop stays
    /// idle when an update changes nothing on screen.
    fn handle(&mut self, msg: &Msg, pool: &mut SlotPool) -> bool {
        let changed = self.dashboard.update(msg);
        if changed {
            self.draw(pool);
        }
        changed
    }

    /// Adopt a new output buffer scale.
    ///
    /// Re-resolves the physical font size from this output's configuration so
    /// text stays crisp at the new density, then repaints (once configured) so
    /// the buffer is reallocated at the new physical size. A no-op when the scale
    /// is unchanged.
    fn set_scale(&mut self, scale: Scale, pool: &mut SlotPool) -> bool {
        if self.scale == scale {
            return false;
        }
        self.scale = scale;
        self.ctx
            .set_settings(self.config.scaled_render_settings(scale));
        self.draw(pool);
        true
    }

    /// Resolve a `button` press at surface coordinates `(x, y)` to a
    /// [`Command`], if any. Pure: the caller fans the command out to the
    /// executors. Clicks that hit no interactive region — empty space,
    /// display-only widgets, or off-surface negative coordinates — yield `None`.
    fn on_click(&self, x: f64, y: f64, button: ClickButton) -> Option<Command> {
        if x < 0.0 || y < 0.0 {
            return None;
        }
        // Pointer coordinates are surface-local *logical* pixels, but the widgets
        // are laid out in physical pixels, so the click is scaled by the same
        // factor before the half-open hit-test — the one conversion that keeps
        // input and layout in the same space.
        let s = self.scale.get() as f64;
        self.dashboard
            .on_click((x * s) as u32, (y * s) as u32, button)
    }

    fn is_clickable_at(&self, x: f64, y: f64) -> bool {
        if x < 0.0 || y < 0.0 {
            return false;
        }
        let scale = self.scale.get() as f64;
        self.dashboard
            .is_clickable_at((x * scale) as u32, (y * scale) as u32)
    }

    /// Resolve one logical scroll step against the widget under `(x, y)`.
    fn on_scroll(&self, x: f64, y: f64, direction: ScrollDirection) -> Option<Command> {
        if x < 0.0 || y < 0.0 {
            return None;
        }
        let scale = self.scale.get() as f64;
        self.dashboard
            .on_scroll((x * scale) as u32, (y * scale) as u32, direction)
    }

    /// Resolve tooltip content at surface-local logical coordinates.
    fn tooltip_at(&self, x: f64, y: f64) -> Option<Tooltip> {
        if x < 0.0 || y < 0.0 {
            return None;
        }
        let scale = self.scale.get() as f64;
        self.dashboard
            .tooltip_at((x * scale) as u32, (y * scale) as u32)
    }

    /// Adopt the compositor's configure, seeding and drawing the first frame.
    fn configure(&mut self, configure: LayerSurfaceConfigure, pool: &mut SlotPool) {
        // A zero dimension means "you decide"; keep our current value.
        if configure.new_size.0 != 0 {
            self.width = configure.new_size.0;
        }
        if configure.new_size.1 != 0 {
            self.height = configure.new_size.1;
        }

        let first = !self.configured;
        self.configured = true;
        if first {
            // Lifecycle-forced frame: seed the clock so the bar shows the time
            // immediately, then draw regardless of the dirty flag.
            self.dashboard.update(&Msg::tick_now());
            self.draw(pool);
        }
    }

    /// Render the current dashboard state and commit it through the app's shared
    /// shared-memory pool. Called on a visible change or when the Wayland
    /// lifecycle (first configure, resize) requires a fresh frame regardless.
    ///
    /// Uses two alternating SHM slots: when the compositor has released the
    /// next slot, its mmap is reused instead of allocating a new one.
    fn draw(&mut self, pool: &mut SlotPool) {
        if !self.configured {
            return;
        }

        // Logical surface dimensions scale up to the physical buffer the
        // compositor maps back down via `set_buffer_scale`. Everything below this
        // point — buffer, render target, layout, font — works in physical pixels.
        let (width, height) = self.scale.to_physical_size(self.width, self.height);
        let stride = width as i32 * 4;

        if self.buffer_px != (width, height) {
            self.buffers = [None, None];
            self.buffer_px = (width, height);
        }

        let idx = self.next_buffer;
        self.next_buffer = 1 - self.next_buffer;

        let can_reuse = self.buffers[idx]
            .as_ref()
            .is_some_and(|buf| !buf.slot().has_active_buffers());

        if can_reuse {
            let canvas = match pool.canvas(self.buffers[idx].as_ref().unwrap()) {
                Some(canvas) => canvas,
                None => {
                    // Race: became active between the check and canvas(). Fall through.
                    self.paint_new_buffer(pool, idx, width, height, stride);
                    self.commit_buffer(idx, width, height);
                    return;
                }
            };
            self.paint_frame(canvas, width, height);
        } else {
            self.paint_new_buffer(pool, idx, width, height, stride);
        }
        self.commit_buffer(idx, width, height);
    }

    fn paint_frame(&mut self, canvas: &mut [u8], width: u32, height: u32) {
        self.ctx.resize(width, height);
        self.dashboard.layout(&mut self.ctx, width, height);
        self.dashboard.draw(&mut self.ctx);
        write_argb8888(self.ctx.pixels(), canvas);
    }

    fn paint_new_buffer(
        &mut self,
        pool: &mut SlotPool,
        idx: usize,
        width: u32,
        height: u32,
        stride: i32,
    ) {
        let (buffer, canvas) = match pool.create_buffer(
            width as i32,
            height as i32,
            stride,
            wl_shm::Format::Argb8888,
        ) {
            Ok(parts) => parts,
            Err(e) => {
                error!("failed to create shm buffer: {e}");
                return;
            }
        };
        self.paint_frame(canvas, width, height);
        self.buffers[idx] = Some(buffer);
    }

    fn commit_buffer(&mut self, idx: usize, width: u32, height: u32) {
        let Some(buffer) = self.buffers[idx].as_ref() else {
            return;
        };
        let surface = self.layer.wl_surface();
        // Tell the compositor the buffer holds `scale`× physical pixels per
        // logical pixel, so it maps the larger buffer back to the logical size.
        surface.set_buffer_scale(self.scale.get() as i32);
        surface.damage_buffer(0, 0, width as i32, height as i32);
        if let Err(e) = buffer.attach_to(surface) {
            error!("failed to attach buffer: {e}");
            return;
        }
        self.layer.commit();
    }
}

/// One visible hover tooltip, implemented as an xdg popup parented to a bar.
struct TooltipSurface {
    popup: Popup,
    output_id: OutputId,
    text: String,
    width: u32,
    height: u32,
    scale: Scale,
    background: (u8, u8, u8, u8),
    foreground: (u8, u8, u8, u8),
    ctx: RenderContext,
    configured: bool,
}

const MENU_ROW_HEIGHT: u32 = 28;
const MENU_SEPARATOR_HEIGHT: u32 = 8;
const POPUP_RADIUS: f32 = 6.0;
const POPUP_PADDING_X: u32 = 10;
const POPUP_PADDING_Y: u32 = 5;
const POPUP_FALLBACK_BACKGROUND: (u8, u8, u8, u8) = (0x20, 0x22, 0x27, 0xF8);

#[derive(Clone)]
struct MenuRow {
    id: i32,
    depth: u32,
    label: String,
    enabled: bool,
    separator: bool,
    toggle: Option<(TrayMenuToggleKind, TrayMenuToggleState)>,
    has_children: bool,
}

impl MenuRow {
    fn height(&self) -> u32 {
        if self.separator {
            MENU_SEPARATOR_HEIGHT
        } else {
            MENU_ROW_HEIGHT
        }
    }

    fn activatable(&self) -> bool {
        self.enabled && !self.separator && !self.has_children
    }
}

fn flatten_menu(items: &[TrayMenuItem], depth: u32, rows: &mut Vec<MenuRow>) {
    for item in items.iter().filter(|item| item.visible) {
        rows.push(MenuRow {
            id: item.id,
            depth,
            label: item.label.clone(),
            enabled: item.enabled,
            separator: item.separator,
            toggle: item.toggle.map(|toggle| (toggle.kind, toggle.state)),
            has_children: !item.children.iter().all(|child| !child.visible),
        });
        flatten_menu(&item.children, depth + 1, rows);
    }
}

struct PendingTrayMenu {
    key: String,
    parent: LayerSurface,
    output_id: OutputId,
    anchor: (i32, i32),
    scale: Scale,
    settings: RenderSettings,
    serial: u32,
    seat: Option<wl_seat::WlSeat>,
}

/// One interactive tray menu, rendered as an XDG popup parented to its bar.
struct TrayMenuSurface {
    popup: Popup,
    output_id: OutputId,
    key: String,
    revision: u32,
    rows: Vec<MenuRow>,
    width: u32,
    height: u32,
    scale: Scale,
    background: (u8, u8, u8, u8),
    foreground: (u8, u8, u8, u8),
    accent: (u8, u8, u8, u8),
    ctx: RenderContext,
    configured: bool,
}

impl TrayMenuSurface {
    fn owns(&self, popup: &Popup) -> bool {
        self.popup == *popup
    }

    fn owns_surface(&self, surface: &wl_surface::WlSurface) -> bool {
        self.popup.wl_surface() == surface
    }

    fn row_at(&self, y: f64) -> Option<&MenuRow> {
        if y < 0.0 {
            return None;
        }
        let mut top = 0u32;
        for row in &self.rows {
            let bottom = top + row.height();
            if (y as u32) < bottom {
                return Some(row);
            }
            top = bottom;
        }
        None
    }

    fn command_at(&self, y: f64) -> Option<Command> {
        let row = self.row_at(y)?;
        row.activatable().then(|| Command::ActivateTrayMenuItem {
            key: self.key.clone(),
            id: row.id,
        })
    }

    fn update(&mut self, menu: &TrayMenu, pool: &mut SlotPool) -> bool {
        if menu.revision < self.revision {
            return true;
        }
        let mut rows = Vec::new();
        flatten_menu(&menu.items, 0, &mut rows);
        let height: u32 = rows.iter().map(MenuRow::height).sum();
        if height != self.height {
            // A structural resize requires a new popup position/configure. Close
            // this one rather than drawing against stale compositor geometry;
            // reopening immediately fetches the new revision.
            return false;
        }
        self.revision = menu.revision;
        self.rows = rows;
        self.draw(pool);
        true
    }

    fn draw(&mut self, pool: &mut SlotPool) {
        if !self.configured {
            return;
        }
        let scale = self.scale.get();
        let width = self.width * scale;
        let height = self.height * scale;
        let stride = width as i32 * 4;
        let (buffer, canvas) = match pool.create_buffer(
            width as i32,
            height as i32,
            stride,
            wl_shm::Format::Argb8888,
        ) {
            Ok(parts) => parts,
            Err(error) => {
                warn!("failed to create tray menu buffer: {error}");
                return;
            }
        };

        self.ctx.resize(width, height);
        self.ctx.fill_rounded_rect(
            Bounds::new(0, 0, width, height),
            self.background,
            POPUP_RADIUS * scale as f32,
        );
        let mut top = 0u32;
        for row in &self.rows {
            let row_height = row.height() * scale;
            if row.separator {
                self.ctx.fill_rounded_rect(
                    Bounds::new(
                        POPUP_PADDING_X * scale,
                        top + row_height / 2,
                        width - 2 * POPUP_PADDING_X * scale,
                        scale,
                    ),
                    dim_color(self.foreground),
                    0.0,
                );
                top += row_height;
                continue;
            }
            let prefix = match row.toggle {
                Some((TrayMenuToggleKind::Checkmark, TrayMenuToggleState::On)) => "[x] ",
                Some((TrayMenuToggleKind::Checkmark, _)) => "[ ] ",
                Some((TrayMenuToggleKind::Radio, TrayMenuToggleState::On)) => "(o) ",
                Some((TrayMenuToggleKind::Radio, _)) => "( ) ",
                None => "",
            };
            let suffix = if row.has_children { "  >" } else { "" };
            let label = format!("{prefix}{}{suffix}", row.label);
            let indent = (POPUP_PADDING_X + row.depth * 16) * scale;
            self.ctx.draw_text(
                &label,
                Bounds::new(
                    indent,
                    top,
                    width.saturating_sub(indent + POPUP_PADDING_X * scale),
                    row_height,
                ),
                if row.enabled {
                    if row
                        .toggle
                        .is_some_and(|(_, state)| state == TrayMenuToggleState::On)
                    {
                        self.accent
                    } else {
                        self.foreground
                    }
                } else {
                    dim_color(self.foreground)
                },
            );
            top += row_height;
        }
        write_argb8888(self.ctx.pixels(), canvas);
        self.popup
            .wl_surface()
            .set_buffer_scale(self.scale.get() as i32);
        self.popup
            .wl_surface()
            .damage_buffer(0, 0, width as i32, height as i32);
        if let Err(error) = buffer.attach_to(self.popup.wl_surface()) {
            warn!("failed to attach tray menu buffer: {error}");
            return;
        }
        self.popup.wl_surface().commit();
    }
}

fn dim_color((r, g, b, a): (u8, u8, u8, u8)) -> (u8, u8, u8, u8) {
    (r / 2, g / 2, b / 2, a)
}

fn popup_background((r, g, b, a): (u8, u8, u8, u8)) -> (u8, u8, u8, u8) {
    if a < 0xC0 {
        POPUP_FALLBACK_BACKGROUND
    } else {
        (r, g, b, a.max(0xF0))
    }
}

impl TooltipSurface {
    fn owns(&self, popup: &Popup) -> bool {
        self.popup == *popup
    }

    fn owns_surface(&self, surface: &wl_surface::WlSurface) -> bool {
        self.popup.wl_surface() == surface
    }

    fn draw(&mut self, pool: &mut SlotPool) {
        if !self.configured {
            return;
        }
        let scale = self.scale.get();
        let width = self.width * scale;
        let height = self.height * scale;
        let stride = width as i32 * 4;
        let (buffer, canvas) = match pool.create_buffer(
            width as i32,
            height as i32,
            stride,
            wl_shm::Format::Argb8888,
        ) {
            Ok(parts) => parts,
            Err(error) => {
                warn!("failed to create tooltip buffer: {error}");
                return;
            }
        };

        self.ctx.resize(width, height);
        self.ctx.fill_rounded_rect(
            Bounds::new(0, 0, width, height),
            self.background,
            POPUP_RADIUS * scale as f32,
        );
        let padding_x = POPUP_PADDING_X * scale;
        let padding_y = POPUP_PADDING_Y * scale;
        let line_height = tooltip_line_height(&self.ctx);
        for (index, line) in self.text.lines().enumerate() {
            self.ctx.draw_text(
                line,
                Bounds::new(
                    padding_x,
                    padding_y + index as u32 * line_height,
                    width.saturating_sub(2 * padding_x),
                    line_height,
                ),
                self.foreground,
            );
        }
        write_argb8888(self.ctx.pixels(), canvas);
        self.popup
            .wl_surface()
            .set_buffer_scale(self.scale.get() as i32);
        self.popup
            .wl_surface()
            .damage_buffer(0, 0, width as i32, height as i32);
        if let Err(error) = buffer.attach_to(self.popup.wl_surface()) {
            warn!("failed to attach tooltip buffer: {error}");
            return;
        }
        self.popup.wl_surface().commit();
    }
}

fn tooltip_line_height(ctx: &RenderContext) -> u32 {
    (ctx.settings().font_size * 1.15).ceil() as u32
}

fn tooltip_size(ctx: &mut RenderContext, text: &str) -> (u32, u32) {
    let scale = ctx.scale_factor();
    let width = text
        .lines()
        .map(|line| ctx.measure_text(line))
        .max()
        .unwrap_or(0)
        + 2 * POPUP_PADDING_X * scale;
    let lines = text.lines().count().max(1) as u32;
    let height = lines * tooltip_line_height(ctx) + 2 * POPUP_PADDING_Y * scale;
    (width.max(1), height.max(1))
}

/// The shared application state and the calloop data type.
///
/// Owns everything common to every output — the Wayland registry, seat, shm and
/// shared [`SlotPool`], the compositor and layer-shell handles needed to build
/// new surfaces, the seat pointer, and the command executors — plus the
/// per-output bars in [`Outputs`]. Output hotplug drives surface create/teardown
/// through the [`OutputHandler`] callbacks; producer messages and clock ticks
/// fan out to every surface.
struct App {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    shm: Shm,
    /// Shared shared-memory pool every surface draws into; it grows as outputs
    /// and scales demand.
    pool: SlotPool,
    /// Retained to create a fresh surface each time an output appears.
    compositor: CompositorState,
    /// Retained to create a fresh layer surface per output.
    layer_shell: LayerShell,
    /// XDG shell global used to create layer-shell-associated tooltip popups.
    xdg_shell: XdgShell,
    /// The seat's pointer, created once the seat advertises the capability.
    pointer: Option<ThemedPointer>,
    pointer_seat: Option<wl_seat::WlSeat>,
    pointer_cursor: CursorIcon,
    /// Fractional logical vertical scroll steps retained across pointer frames.
    scroll_remainder: f64,
    /// Outbound command channels into the producer runtime, one per command
    /// executor (Hyprland workspace switching, SNI tray activation). A click's
    /// command is fanned out to every executor; each ignores the commands it does
    /// not handle. Empty when no bridge is running, in which case clicks are
    /// dropped.
    commands: Vec<CommandSender>,
    /// The per-output bars, keyed by output id. The whole multi-monitor lifecycle
    /// lives here.
    outputs: Outputs<Surface>,
    /// The currently visible tooltip; at most one pointer hover exists per seat.
    tooltip: Option<TooltipSurface>,
    pending_tray_menu: Option<PendingTrayMenu>,
    tray_menu: Option<TrayMenuSurface>,
    /// Process-wide font set shared by every surface and popup.
    fonts: SharedFonts,
    /// Optional path watched for config hot-reload.
    config_path: Option<PathBuf>,
    /// Last observed mtime of `config_path` (or `None` if missing).
    config_mtime: Option<SystemTime>,
    exit: bool,
}

impl App {
    /// Create — or, if already tracked, keep — the bar for `output`.
    ///
    /// Idempotent via [`Outputs::ensure`]: a repeated `new_output`/`update_output`
    /// for an output we already show never rebuilds or replaces its surface. The
    /// connector name selects the output's resolved config and scopes its
    /// workspace widget to that monitor.
    fn add_output(&mut self, output: wl_output::WlOutput, qh: &QueueHandle<App>) {
        let id = output_key(&output);
        let name = self.output_state.info(&output).and_then(|info| info.name);
        // Pre-borrow the build inputs as locals so `ensure`'s mutable borrow of
        // `self.outputs` and these shared borrows of other fields stay disjoint.
        let compositor = &self.compositor;
        let layer_shell = &self.layer_shell;
        let fonts = self.fonts.clone();
        let built = self.outputs.ensure(id, name.as_deref(), |config| {
            Surface::new(
                compositor,
                layer_shell,
                qh,
                &output,
                id,
                name.as_deref(),
                config,
                fonts,
            )
        });
        if built {
            debug!(
                "output {id} ({}) added; {} bar(s) live",
                name.as_deref().unwrap_or("<unnamed>"),
                self.outputs.len()
            );
        }
    }

    /// Apply a freshly loaded config to every live bar (theme, layout, widgets).
    ///
    /// Producers already running stay as-is; newly required modules need a
    /// process restart. Invalid files are logged and ignored by the watcher.
    fn reload_config(&mut self, config: Config) {
        self.outputs.set_base(config);
        self.hide_tooltip();
        self.hide_tray_menu();
        let base = self.outputs.base().clone();
        let reloads: Vec<(OutputId, Config)> = self
            .outputs
            .values()
            .map(|surface| {
                let name = self
                    .output_state
                    .info(&surface.output)
                    .and_then(|info| info.name);
                (surface.output_id, base.resolve_for_output(name.as_deref()))
            })
            .collect();
        for (id, resolved) in reloads {
            if let Some(surface) = self.outputs.get_mut(id) {
                surface.apply_config(resolved, &mut self.pool);
            }
        }
        info!("config reloaded");
    }

    /// Poll the config file mtime and reload when it changes.
    fn poll_config_reload(&mut self) {
        let Some(path) = self.config_path.clone() else {
            return;
        };
        let mtime = file_mtime(&path);
        if mtime == self.config_mtime {
            return;
        }
        // First observation of a missing→present or present→missing file also
        // reloads so create/delete of the config takes effect.
        self.config_mtime = mtime;
        match Config::load_from_path(&path) {
            Ok(config) => self.reload_config(config),
            Err(error) => warn!("config reload ignored: {error}"),
        }
    }

    /// Tear down the bar for `output`, if any. The dropped [`LayerSurface`]
    /// destroys the layer-shell surface, so an unplugged monitor leaves no stale
    /// state behind.
    fn remove_output(&mut self, output: &wl_output::WlOutput) {
        let id = output_key(output);
        if self
            .tray_menu
            .as_ref()
            .is_some_and(|menu| menu.output_id == id)
        {
            self.hide_tray_menu();
        }
        if self.outputs.remove(id).is_some() {
            debug!("output {id} removed; {} bar(s) live", self.outputs.len());
        }
    }

    /// Fan a message out to every output's dashboard, redrawing the ones that
    /// changed. The clock timer and every producer reach all bars this way.
    fn handle_all(&mut self, msg: &Msg) {
        let App { pool, outputs, .. } = self;
        let mut changed = false;
        for surface in outputs.values_mut() {
            changed |= surface.handle(msg, pool);
        }
        if changed && matches!(msg, Msg::PowerProfiles(_)) {
            self.hide_tooltip();
        }
    }

    fn handle_message(&mut self, msg: &Msg, qh: &QueueHandle<App>) {
        if let Msg::TrayMenu(menu) = msg {
            if let Some(shown) = self
                .tray_menu
                .as_mut()
                .filter(|shown| shown.key == menu.key)
            {
                if !shown.update(menu, &mut self.pool) {
                    self.tray_menu = None;
                }
            } else {
                self.show_tray_menu(menu, qh);
            }
        } else if let Msg::TrayMenuUnavailable(key) = msg {
            if self
                .pending_tray_menu
                .as_ref()
                .is_some_and(|pending| pending.key == *key)
            {
                self.pending_tray_menu = None;
            }
        } else {
            self.handle_all(msg);
        }
    }

    fn hide_tooltip(&mut self) {
        self.tooltip = None;
    }

    fn hide_tray_menu(&mut self) {
        self.pending_tray_menu = None;
        self.tray_menu = None;
    }

    fn set_pointer_cursor(&mut self, conn: &Connection, icon: CursorIcon, force: bool) {
        if !force && self.pointer_cursor == icon {
            return;
        }
        let Some(pointer) = &self.pointer else {
            return;
        };
        match pointer.set_cursor(conn, icon) {
            Ok(()) => self.pointer_cursor = icon,
            Err(error) => warn!("failed to set pointer cursor: {error}"),
        }
    }

    fn update_tooltip(
        &mut self,
        surface: &wl_surface::WlSurface,
        x: f64,
        y: f64,
        qh: &QueueHandle<App>,
    ) {
        let request = self
            .outputs
            .values()
            .find(|bar| bar.owns(surface))
            .and_then(|bar| {
                let tooltip = bar.tooltip_at(x, y)?;
                Some((
                    bar.output_id,
                    bar.layer.clone(),
                    bar.scale,
                    bar.config.scaled_render_settings(bar.scale),
                    tooltip,
                ))
            });
        let Some((output_id, parent, scale, mut settings, tooltip)) = request else {
            self.hide_tooltip();
            return;
        };
        if self
            .tooltip
            .as_ref()
            .is_some_and(|shown| shown.output_id == output_id && shown.text == tooltip.text)
        {
            return;
        }

        let background = popup_background(settings.background);
        let foreground = settings.foreground;
        settings.background = (0, 0, 0, 0);
        let mut ctx = RenderContext::with_fonts(1, 1, settings, self.fonts.clone());
        let (physical_width, physical_height) = tooltip_size(&mut ctx, &tooltip.text);
        let divisor = scale.get();
        let width = physical_width.div_ceil(divisor);
        let height = physical_height.div_ceil(divisor);
        let anchor = Bounds::new(
            tooltip.bounds.x / divisor,
            tooltip.bounds.y / divisor,
            tooltip.bounds.width.div_ceil(divisor),
            tooltip.bounds.height.div_ceil(divisor),
        );

        let positioner = match XdgPositioner::new(&self.xdg_shell) {
            Ok(positioner) => positioner,
            Err(error) => {
                warn!("failed to create tooltip positioner: {error}");
                return;
            }
        };
        positioner.set_size(width as i32, height as i32);
        positioner.set_anchor_rect(
            anchor.x as i32,
            anchor.y as i32,
            anchor.width.max(1) as i32,
            anchor.height.max(1) as i32,
        );
        positioner.set_anchor(xdg_positioner::Anchor::Bottom);
        positioner.set_gravity(xdg_positioner::Gravity::Bottom);
        positioner.set_constraint_adjustment(xdg_positioner::ConstraintAdjustment::SlideX);

        let popup_surface = self.compositor.create_surface(qh);
        let popup = match Popup::from_surface(None, &positioner, qh, popup_surface, &self.xdg_shell)
        {
            Ok(popup) => popup,
            Err(error) => {
                warn!("failed to create tooltip popup: {error}");
                return;
            }
        };
        parent.get_popup(popup.xdg_popup());
        let input_region = self.compositor.wl_compositor().create_region(qh, ());
        popup.wl_surface().set_input_region(Some(&input_region));
        input_region.destroy();
        popup.wl_surface().commit();
        self.tooltip = Some(TooltipSurface {
            popup,
            output_id,
            text: tooltip.text,
            width,
            height,
            scale,
            background,
            foreground,
            ctx,
            configured: false,
        });
    }

    fn show_tray_menu(&mut self, menu: &TrayMenu, qh: &QueueHandle<App>) {
        let Some(pending) = self
            .pending_tray_menu
            .take()
            .filter(|pending| pending.key == menu.key)
        else {
            return;
        };
        let mut rows = Vec::new();
        flatten_menu(&menu.items, 0, &mut rows);
        if rows.is_empty() {
            return;
        }

        let mut settings = pending.settings;
        let background = popup_background(settings.background);
        let foreground = settings.foreground;
        let accent = settings.accent;
        settings.background = (0, 0, 0, 0);
        let mut ctx = RenderContext::with_fonts(1, 1, settings, self.fonts.clone());
        let scale = pending.scale.get();
        let physical_width = rows
            .iter()
            .filter(|row| !row.separator)
            .map(|row| {
                let indicators = if row.toggle.is_some() { 4 } else { 0 };
                let submenu = if row.has_children { 3 } else { 0 };
                let text = format!(
                    "{}{}{}",
                    " ".repeat(indicators),
                    row.label,
                    " ".repeat(submenu)
                );
                ctx.measure_text(&text) + (32 + row.depth * 16) * scale
            })
            .max()
            .unwrap_or(1);
        let width = physical_width.div_ceil(scale).clamp(120, 420);
        let height: u32 = rows.iter().map(MenuRow::height).sum();

        let positioner = match XdgPositioner::new(&self.xdg_shell) {
            Ok(positioner) => positioner,
            Err(error) => {
                warn!("failed to create tray menu positioner: {error}");
                return;
            }
        };
        positioner.set_size(width as i32, height as i32);
        positioner.set_anchor_rect(pending.anchor.0, pending.anchor.1, 1, 1);
        positioner.set_anchor(xdg_positioner::Anchor::BottomLeft);
        positioner.set_gravity(xdg_positioner::Gravity::BottomRight);
        positioner.set_constraint_adjustment(xdg_positioner::ConstraintAdjustment::SlideX);

        let popup_surface = self.compositor.create_surface(qh);
        let popup = match Popup::from_surface(None, &positioner, qh, popup_surface, &self.xdg_shell)
        {
            Ok(popup) => popup,
            Err(error) => {
                warn!("failed to create tray menu popup: {error}");
                return;
            }
        };
        pending.parent.get_popup(popup.xdg_popup());
        if let Some(seat) = &pending.seat {
            popup.xdg_popup().grab(seat, pending.serial);
        }
        popup.wl_surface().commit();
        self.tooltip = None;
        self.tray_menu = Some(TrayMenuSurface {
            popup,
            output_id: pending.output_id,
            key: menu.key.clone(),
            revision: menu.revision,
            rows,
            width,
            height,
            scale: pending.scale,
            background,
            foreground,
            accent,
            ctx,
            configured: false,
        });
    }
}

/// mtime of `path`, or `None` if the file is missing or unstatable.
fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// The stable per-output key: the `wl_output`'s protocol object id.
///
/// Available in both `new_output` and `output_destroyed` (unlike the output's
/// advertised info, which the compositor may already have dropped by teardown),
/// and unique per output for its lifetime — exactly the key the registry needs.
fn output_key(output: &wl_output::WlOutput) -> OutputId {
    output.id().protocol_id()
}

fn set_tray_command_position(command: &mut Command, origin: (i32, i32), local: (f64, f64)) {
    let screen = (
        origin.0.saturating_add(local.0 as i32),
        origin.1.saturating_add(local.1 as i32),
    );
    match command {
        Command::ActivateTrayItem { x, y, .. } | Command::OpenTrayMenu { x, y, .. } => {
            *x = screen.0;
            *y = screen.1;
        }
        _ => {}
    }
}

/// Open the bar and run its event loop until the compositor closes the surface.
///
/// The bar's height, theme, font, spacing, and widget order all come from
/// `config` (see [`crate::config::Config`]). Wires producers for the widgets
/// that appear in the global layout or any per-monitor override
/// ([`Config::uses_widget`]): Hyprland always runs (workspaces / title);
/// UPower, sysmon, NetworkManager, BlueZ, PipeWire volume, the SNI tray host,
/// swaync, power-profiles-daemon, backlight, Arch updates, and Hypridle start
/// only when their module is configured. The volume source uses a dedicated OS
/// thread (PipeWire's main loop is synchronous). The clock is driven by the
/// synchronous tick timer. When `config_path` is set, the file is polled for
/// mtime changes and the bar hot-reloads theme/layout (producers keep their
/// original set until restart). See [`run_with_producers`] for a custom set.
pub fn run(config: Config, config_path: Option<PathBuf>) -> Result<(), Box<dyn Error>> {
    let mut producers: Vec<Box<dyn Producer>> = vec![Box::new(HyprlandProducer::new())];
    // Gate non-Hyprland sources so a minimal bar does not open PipeWire, host a
    // StatusNotifierWatcher, or poll sysfs/backlight unnecessarily.
    if config.uses_widget(WidgetKind::Battery) {
        producers.push(Box::new(UPowerProducer::new()));
    }
    if config.uses_widget(WidgetKind::Backlight) {
        producers.push(Box::new(BacklightProducer::new()));
    }
    if config.uses_widget(WidgetKind::System) {
        producers.push(Box::new(SystemProducer::new()));
    }
    if config.uses_widget(WidgetKind::Network) {
        producers.push(Box::new(NetworkProducer::new()));
    }
    if config.uses_widget(WidgetKind::Bluetooth) {
        producers.push(Box::new(BluetoothProducer::new()));
    }
    if config.uses_widget(WidgetKind::Volume) {
        producers.push(Box::new(VolumeProducer::new()));
    }
    if config.uses_widget(WidgetKind::Tray) {
        producers.push(Box::new(SniHostProducer::new()));
    }
    if config.uses_widget(WidgetKind::Notifications) {
        producers.push(Box::new(NotificationsProducer::new()));
    }
    if config.uses_widget(WidgetKind::PowerProfilesDaemon) {
        producers.push(Box::new(PowerProfilesProducer::new()));
    }
    if config.uses_widget(WidgetKind::Updates) {
        producers.push(Box::new(UpdatesProducer::new()));
    }
    if config.uses_widget(WidgetKind::Hypridle) {
        producers.push(Box::new(HypridleProducer::new()));
    }
    run_with_producers(config, producers, config_path)
}

/// Open the bar and run its event loop, additionally driving `producers` on an
/// off-thread Tokio runtime.
///
/// The render loop stays fully synchronous: it owns the dashboards, rendering,
/// and Wayland commits. Each producer runs on the [`ProducerBridge`] runtime and
/// reaches the loop only by sending [`Msg`]s through a calloop channel, which is
/// fanned out to every output's dashboard via the app's message handler exactly like
/// the clock timer. With an empty `producers` list no runtime is started at all.
///
/// During setup, one Wayland roundtrip discovers the initial outputs and opens
/// their layer surfaces before producers start. Later outputs still arrive
/// through [`OutputHandler::new_output`], and `output_destroyed` tears each down,
/// so plugging or unplugging a monitor adds or removes its bar without restarting
/// the loop. `config_path` enables mtime-based hot-reload of the TOML config.
pub fn run_with_producers(
    config: Config,
    producers: Vec<Box<dyn Producer>>,
    config_path: Option<PathBuf>,
) -> Result<(), Box<dyn Error>> {
    let height = config.height;
    let config_mtime = config_path.as_ref().and_then(|p| file_mtime(p));
    let fonts = shared_fonts();

    let conn = Connection::connect_to_env()?;
    let (globals, mut event_queue) = registry_queue_init::<App>(&conn)?;
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh)?;
    let xdg_shell = XdgShell::bind(&globals, &qh)?;
    let shm = Shm::bind(&globals, &qh)?;

    // Size the shared pool for one full-width bar at the default height; it grows
    // automatically as further outputs and higher scales demand more buffers.
    // Double-buffering uses two slots per surface, so seed a little larger.
    let pool = SlotPool::new((INITIAL_WIDTH * height * 4 * 2) as usize, &shm)?;

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        shm,
        pool,
        compositor,
        layer_shell,
        xdg_shell,
        pointer: None,
        pointer_seat: None,
        pointer_cursor: CursorIcon::Default,
        scroll_remainder: 0.0,
        commands: Vec::new(),
        outputs: Outputs::new(config),
        tooltip: None,
        pending_tray_menu: None,
        tray_menu: None,
        fonts,
        config_path,
        config_mtime,
        exit: false,
    };

    // Finish initial output discovery before producers can emit snapshots. If a
    // producer starts first, its one-shot initial state is dispatched while
    // `outputs` is empty and is lost until that source changes again.
    event_queue.roundtrip(&mut app)?;

    let mut event_loop: EventLoop<App> = EventLoop::try_new()?;
    let handle = event_loop.handle();

    // Wayland events (output advertisements, configure, close, ...) wake the loop.
    WaylandSource::new(conn, event_queue).insert(handle.clone())?;

    // A timer aligned to the wall-clock minute wakes the loop for each tick. The
    // clock renders HH:MM, so once-per-minute keeps it correct while the loop
    // stays idle the rest of the minute.
    let timer = Timer::from_duration(Duration::from_millis(millis_until_next_minute()));
    handle.insert_source(timer, |_deadline, _, app| {
        app.handle_all(&Msg::tick_now());
        TimeoutAction::ToDuration(Duration::from_millis(millis_until_next_minute()))
    })?;

    // Poll the config file mtime about twice a second so theme/layout edits
    // apply without restarting. Cheap when the path is unset (no-op).
    if app.config_path.is_some() {
        let reload = Timer::from_duration(Duration::from_millis(500));
        handle.insert_source(reload, |_deadline, _, app| {
            app.poll_config_reload();
            TimeoutAction::ToDuration(Duration::from_millis(500))
        })?;
    }

    // Bring up the async producer bridge only when there is async work to do.
    // The bridge owns the Tokio runtime and must outlive the loop, so it is held
    // in `_bridge` until the function returns.
    let _bridge = if producers.is_empty() {
        None
    } else {
        let (bridge, channel) = ProducerBridge::new()?;
        let message_qh = qh.clone();
        handle.insert_source(channel, move |event, _, app| {
            // Producer messages cross the channel into the same synchronous
            // app-state update path the clock timer uses, fanned out to every bar.
            if let ChannelEvent::Msg(msg) = event {
                app.handle_message(&msg, &message_qh);
            }
        })?;
        for producer in producers {
            bridge.spawn(producer);
        }
        // The reverse path: clicks become commands the executors run against the
        // compositor, the session bus, or a user-configured program. The loop
        // holds a sender per executor and fans each command out to all of them;
        // an executor ignores commands it does not handle, so workspace
        // switches reach Hyprland, tray activations reach the SNI items, and
        // configured on-click programs spawn directly via
        // `command::run_commands` — without the loop routing them.
        let (hypr_tx, hypr_rx) = command_channel();
        bridge.spawn_task("hyprland-commands", hyprland::run_commands(hypr_rx));
        let (sni_tx, sni_rx) = command_channel();
        let sni_updates = bridge.sender();
        bridge.spawn_task("sni-commands", sni::run_commands(sni_rx, sni_updates));
        let (run_tx, run_rx) = command_channel();
        bridge.spawn_task("run-commands", command::run_commands(run_rx));
        let (notif_tx, notif_rx) = command_channel();
        bridge.spawn_task(
            "notifications-commands",
            notifications::run_commands(notif_rx),
        );
        let (backlight_tx, backlight_rx) = command_channel();
        let backlight_updates = bridge.sender();
        bridge.spawn_task(
            "backlight-commands",
            backlight::run_commands(backlight_rx, backlight_updates),
        );
        let (power_tx, power_rx) = command_channel();
        bridge.spawn_task(
            "power-profiles-commands",
            power_profiles::run_commands(power_rx),
        );
        let (hypridle_tx, hypridle_rx) = command_channel();
        let hypridle_updates = bridge.sender();
        bridge.spawn_task(
            "hypridle-commands",
            hypridle::run_commands(hypridle_rx, hypridle_updates),
        );
        app.commands = vec![
            hypr_tx,
            sni_tx,
            run_tx,
            notif_tx,
            backlight_tx,
            power_tx,
            hypridle_tx,
        ];
        Some(bridge)
    };

    let signal = event_loop.get_signal();
    event_loop.run(None, &mut app, move |app| {
        if app.exit {
            info!("all surfaces closed; shutting down");
            signal.stop();
        }
    })?;

    Ok(())
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        // The compositor reports a surface's preferred integer buffer scale.
        // Route it to the owning bar so each output renders at its own density.
        let App {
            pool,
            outputs,
            tooltip,
            tray_menu,
            ..
        } = self;
        let scale = Scale::new(new_factor);
        let changed_output = outputs
            .values_mut()
            .find(|bar| bar.owns(surface))
            .and_then(|bar| bar.set_scale(scale, pool).then_some(bar.output_id));
        if tooltip.as_ref().is_some_and(|shown| {
            (shown.owns_surface(surface) && shown.scale != scale)
                || changed_output == Some(shown.output_id)
        }) {
            *tooltip = None;
        }
        if tray_menu.as_ref().is_some_and(|shown| {
            (shown.owns_surface(surface) && shown.scale != scale)
                || changed_output == Some(shown.output_id)
        }) {
            *tray_menu = None;
        }
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        // We never request frame callbacks: redraws are driven solely by the
        // tick timer, which keeps the loop free of a busy redraw cycle.
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        // A monitor appeared: open its bar.
        self.add_output(output, qh);
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        // A property change on an output we already show is a no-op (ensure keeps
        // the live surface); a brand-new output we somehow missed gets its bar.
        self.add_output(output, qh);
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        // A monitor was unplugged: drop its bar, leaving the others untouched.
        self.remove_output(&output);
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, layer: &LayerSurface) {
        // The compositor closed one of our layers: drop just that surface. Exit
        // only once the last bar is gone, matching the single-output shutdown.
        if let Some(id) = self
            .outputs
            .values()
            .find(|bar| bar.is_layer(layer))
            .map(|bar| bar.output_id)
        {
            self.outputs.remove(id);
        }
        if self.outputs.is_empty() {
            self.exit = true;
        }
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let App { pool, outputs, .. } = self;
        if let Some(bar) = outputs.values_mut().find(|bar| bar.is_layer(layer)) {
            bar.configure(configure, pool);
        }
    }
}

impl PopupHandler for App {
    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        popup: &Popup,
        _config: PopupConfigure,
    ) {
        if let Some(tooltip) = self.tooltip.as_mut().filter(|tooltip| tooltip.owns(popup)) {
            tooltip.configured = true;
            tooltip.draw(&mut self.pool);
        } else if let Some(menu) = self.tray_menu.as_mut().filter(|menu| menu.owns(popup)) {
            menu.configured = true;
            menu.draw(&mut self.pool);
        }
    }

    fn done(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, popup: &Popup) {
        if self
            .tooltip
            .as_ref()
            .is_some_and(|tooltip| tooltip.owns(popup))
        {
            self.hide_tooltip();
        } else if self.tray_menu.as_ref().is_some_and(|menu| menu.owns(popup)) {
            self.hide_tray_menu();
        }
    }
}

// XdgShell's dispatch helper also covers toplevel objects. Tablero creates only
// popups, so these callbacks are unreachable but satisfy the shared dispatcher.
impl WindowHandler for App {
    fn request_close(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _window: &Window) {}

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _window: &Window,
        _configure: WindowConfigure,
        _serial: u32,
    ) {
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        // Bind the pointer once, when the seat first advertises one. The bar is
        // pointer-only; keyboard and touch capabilities are ignored.
        if capability == Capability::Pointer && self.pointer.is_none() {
            let cursor_surface = self.compositor.create_surface(qh);
            match self.seat_state.get_pointer_with_theme(
                qh,
                &seat,
                self.shm.wl_shm(),
                cursor_surface,
                ThemeSpec::default(),
            ) {
                Ok(pointer) => {
                    self.pointer = Some(pointer);
                    self.pointer_seat = Some(seat);
                }
                Err(e) => error!("failed to create pointer: {e}"),
            }
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            self.pointer = None;
            self.pointer_seat = None;
            self.pointer_cursor = CursorIcon::Default;
        }
    }

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {
    }
}

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            if matches!(
                event.kind,
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. }
            ) {
                let (x, y) = event.position;
                if let Some(menu) = self
                    .tray_menu
                    .as_ref()
                    .filter(|menu| menu.owns_surface(&event.surface))
                {
                    self.set_pointer_cursor(
                        conn,
                        if menu.row_at(y).is_some_and(MenuRow::activatable) {
                            CursorIcon::Pointer
                        } else {
                            CursorIcon::Default
                        },
                        matches!(event.kind, PointerEventKind::Enter { .. }),
                    );
                    continue;
                }
                let owner = self.outputs.values().find(|bar| bar.owns(&event.surface));
                let clickable = owner.is_some_and(|bar| bar.is_clickable_at(x, y));
                if owner.is_some() {
                    self.update_tooltip(&event.surface, x, y, _qh);
                    self.set_pointer_cursor(
                        conn,
                        if clickable {
                            CursorIcon::Pointer
                        } else {
                            CursorIcon::Default
                        },
                        matches!(event.kind, PointerEventKind::Enter { .. }),
                    );
                } else if matches!(event.kind, PointerEventKind::Enter { .. }) {
                    warn!("pointer enter on unknown surface id={}", event.surface.id());
                }
            } else if matches!(event.kind, PointerEventKind::Leave { .. }) {
                if self.outputs.values().any(|bar| bar.owns(&event.surface)) {
                    self.hide_tooltip();
                    self.pointer_cursor = CursorIcon::Default;
                } else if self
                    .tray_menu
                    .as_ref()
                    .is_some_and(|menu| menu.owns_surface(&event.surface))
                {
                    self.pointer_cursor = CursorIcon::Default;
                }
            } else if let PointerEventKind::Press { button, serial, .. } = event.kind {
                // Normalize the kernel input code to the typed button the
                // widgets branch on; other buttons (middle, side, scroll
                // clicks) are ignored here so widgets never see them.
                let click = match button {
                    BTN_LEFT => ClickButton::Left,
                    BTN_RIGHT => ClickButton::Right,
                    _ => continue,
                };
                let (x, y) = event.position;
                if self
                    .tray_menu
                    .as_ref()
                    .is_some_and(|menu| menu.owns_surface(&event.surface))
                {
                    if click == ClickButton::Left {
                        let command = self.tray_menu.as_ref().and_then(|menu| menu.command_at(y));
                        if let Some(command) = command {
                            self.hide_tray_menu();
                            for sender in &self.commands {
                                if sender.send(command.clone()).is_err() {
                                    warn!("command channel closed; dropping menu command");
                                }
                            }
                        }
                    }
                    continue;
                }
                // Resolve the click against the surface it landed on, then fan the
                // resulting command out to the executors. The immutable lookup ends
                // before `self.commands` is borrowed, so the two never conflict.
                let interaction = self
                    .outputs
                    .values()
                    .find(|bar| bar.owns(&event.surface))
                    .and_then(|bar| {
                        Some((
                            bar.on_click(x, y, click)?,
                            bar.layer.clone(),
                            bar.output.clone(),
                            bar.output_id,
                            bar.scale,
                            bar.height,
                            bar.config.scaled_render_settings(bar.scale),
                        ))
                    });
                if let Some((mut command, parent, output, output_id, scale, bar_height, settings)) =
                    interaction
                {
                    let origin = self
                        .output_state
                        .info(&output)
                        .map(|info| info.logical_position.unwrap_or(info.location))
                        .unwrap_or((0, 0));
                    set_tray_command_position(&mut command, origin, (x, y));
                    if let Command::OpenTrayMenu { key, .. } = &command {
                        self.hide_tray_menu();
                        self.hide_tooltip();
                        self.pending_tray_menu = Some(PendingTrayMenu {
                            key: key.clone(),
                            parent,
                            output_id,
                            anchor: (x as i32, bar_height as i32),
                            scale,
                            settings,
                            serial,
                            seat: self.pointer_seat.clone(),
                        });
                    }
                    for sender in &self.commands {
                        if sender.send(command.clone()).is_err() {
                            warn!("command channel closed; dropping click command");
                        }
                    }
                }
            } else if let PointerEventKind::Axis { vertical, .. } = event.kind {
                let directions = scroll_directions(vertical, &mut self.scroll_remainder);
                for direction in directions {
                    let (x, y) = event.position;
                    let command = self
                        .outputs
                        .values()
                        .find(|bar| bar.owns(&event.surface))
                        .and_then(|bar| bar.on_scroll(x, y, direction));
                    if let Some(command) = command {
                        for sender in &self.commands {
                            if sender.send(command.clone()).is_err() {
                                warn!("command channel closed; dropping scroll command");
                            }
                        }
                    }
                }
            }
        }
    }
}

impl Dispatch<wl_region::WlRegion, ()> for App {
    fn event(
        _state: &mut Self,
        _proxy: &wl_region::WlRegion,
        _event: wl_region::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

/// Normalize wheel, high-resolution wheel, and touchpad motion into logical steps.
fn scroll_directions(
    vertical: smithay_client_toolkit::seat::pointer::AxisScroll,
    remainder: &mut f64,
) -> Vec<ScrollDirection> {
    let delta = if vertical.value120 != 0 {
        vertical.value120 as f64 / 120.0
    } else if vertical.discrete != 0 {
        vertical.discrete as f64
    } else {
        // Continuous devices report pixels; ten pixels is one deliberate step.
        vertical.absolute / 10.0
    };
    *remainder += delta;
    let mut directions = Vec::new();
    while *remainder >= 1.0 {
        directions.push(ScrollDirection::Decrease);
        *remainder -= 1.0;
    }
    while *remainder <= -1.0 {
        directions.push(ScrollDirection::Increase);
        *remainder += 1.0;
    }
    directions
}

#[cfg(test)]
mod scroll_tests {
    use super::*;
    use smithay_client_toolkit::seat::pointer::AxisScroll;

    #[test]
    fn wheel_steps_map_up_to_increase_and_down_to_decrease() {
        let mut remainder = 0.0;
        assert_eq!(
            scroll_directions(
                AxisScroll {
                    value120: -120,
                    ..AxisScroll::default()
                },
                &mut remainder,
            ),
            vec![ScrollDirection::Increase]
        );
        assert_eq!(
            scroll_directions(
                AxisScroll {
                    value120: 120,
                    ..AxisScroll::default()
                },
                &mut remainder,
            ),
            vec![ScrollDirection::Decrease]
        );
    }

    #[test]
    fn smooth_motion_accumulates_before_emitting_a_step() {
        let mut remainder = 0.0;
        let half = AxisScroll {
            absolute: -5.0,
            ..AxisScroll::default()
        };
        assert!(scroll_directions(half, &mut remainder).is_empty());
        assert_eq!(
            scroll_directions(half, &mut remainder),
            vec![ScrollDirection::Increase]
        );
    }

    #[test]
    fn tray_coordinates_include_the_output_logical_origin() {
        let mut command = Command::OpenTrayMenu {
            key: ":1.7/Menu".into(),
            x: 0,
            y: 0,
        };
        set_tray_command_position(&mut command, (1920, -40), (24.8, 18.9));
        assert_eq!(
            command,
            Command::OpenTrayMenu {
                key: ":1.7/Menu".into(),
                x: 1944,
                y: -22,
            }
        );
    }

    #[test]
    fn tray_menu_flattens_visible_nested_entries_and_preserves_depth() {
        let leaf = TrayMenuItem {
            id: 2,
            label: "Child".into(),
            enabled: true,
            visible: true,
            separator: false,
            toggle: None,
            children: vec![],
        };
        let parent = TrayMenuItem {
            id: 1,
            label: "Parent".into(),
            enabled: true,
            visible: true,
            separator: false,
            toggle: None,
            children: vec![leaf],
        };
        let hidden = TrayMenuItem {
            id: 3,
            label: "Hidden".into(),
            enabled: true,
            visible: false,
            separator: false,
            toggle: None,
            children: vec![],
        };
        let mut rows = Vec::new();
        flatten_menu(&[parent, hidden], 0, &mut rows);

        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0].id, rows[0].depth), (1, 0));
        assert!(rows[0].has_children);
        assert!(!rows[0].activatable());
        assert_eq!((rows[1].id, rows[1].depth), (2, 1));
        assert!(rows[1].activatable());
    }

    #[test]
    fn transparent_bar_background_uses_an_opaque_popup_surface() {
        assert_eq!(
            popup_background((0x18, 0x18, 0x18, 0x00)),
            POPUP_FALLBACK_BACKGROUND
        );
        assert_eq!(
            popup_background((0x30, 0x32, 0x38, 0xD0)),
            (0x30, 0x32, 0x38, 0xF0)
        );
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(App);
delegate_output!(App);
delegate_shm!(App);
delegate_layer!(App);
delegate_xdg_shell!(App);
delegate_xdg_popup!(App);
delegate_seat!(App);
delegate_pointer!(App);
delegate_registry!(App);
