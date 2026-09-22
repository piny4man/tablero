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
mod config_reload;
pub mod icon;
mod performance;
pub mod redraw;
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

use std::cell::Cell;
use std::error::Error;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::blit::write_argb8888;
use crate::clock::millis_until_next_minute;
use crate::config::{Config, WidgetKind};
use crate::redraw::RedrawScheduler;
use crate::render::{
    Bounds, RenderContext, RenderSettings, RenderStats, SharedFonts, shared_fonts,
};
use crate::scale::Scale;
use crate::widget::{
    ClickButton, Command, Damage, Dashboard, Hover, Msg, ScrollDirection, Tooltip, TrayMenu,
    TrayMenuItem, TrayMenuToggleKind, TrayMenuToggleState,
};
use calloop::channel::Event as ChannelEvent;
use calloop::generic::Generic;
use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, Interest, LoopHandle, Mode, PostAction, RegistrationToken};
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
use crate::performance::PerformanceLogger;
use crate::power_profiles::{PowerProfilesProducer, PowerProfilesSettings};
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

/// Application-side phases captured for one rendered frame.
struct FrameTimings {
    layout: Option<Duration>,
    draw: Option<Duration>,
    blit: Option<Duration>,
    render: Option<Duration>,
    /// Time since this surface last painted. Separates a cold frame after a long
    /// idle (clocked-down CPU, evicted caches) from a warm one.
    idle_gap: Option<Duration>,
    stats: RenderStats,
    /// The part of the buffer this frame changed.
    damage: Damage,
}

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
    /// When diagnostics are enabled, the start of the previous painted frame.
    last_frame: Option<Instant>,
    /// Set once the first configure has been received; drawing before that is
    /// invalid per the layer-shell protocol.
    configured: bool,
    /// Whether a frame has been committed yet, for the startup metric.
    presented: bool,
    /// Opt-in render timing diagnostics; disabled in the normal path.
    performance: PerformanceLogger,
    /// Pending-repaint state: changes request a frame, [`Surface::flush`] paints it.
    redraw: RedrawScheduler,
    /// Retained to request a frame callback with each commit.
    qh: QueueHandle<App>,
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
        performance: PerformanceLogger,
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
            last_frame: None,
            configured: false,
            presented: false,
            performance,
            redraw: RedrawScheduler::new(),
            qh: qh.clone(),
        }
    }

    /// Rebuild layout and visuals from a new config (hot-reload).
    ///
    /// `seed` is replayed into the fresh dashboard so widgets keep the last
    /// producer snapshot instead of going blank until the next event.
    fn apply_config(&mut self, config: Config, seed: &[Msg]) {
        let height = config.height;
        let height_changed = self.height != height;
        if height_changed {
            self.height = height;
            self.layer.set_size(0, height);
            self.layer.set_exclusive_zone(height as i32);
            // Size change requests a new configure; still repaint at the current
            // geometry so theme/widget changes appear immediately.
            self.layer.commit();
            // Only drop SHM slots when geometry changes — dropping an attached
            // buffer while the compositor still holds it can tear down the surface.
            self.buffers = [None, None];
            self.buffer_px = (0, 0);
        }
        self.config = config;
        let full = Bounds::new(0, 0, self.width.max(1), self.height.max(1));
        self.dashboard = self.config.build_dashboard(full, self.monitor.as_deref());
        self.ctx
            .set_settings(self.config.scaled_render_settings(self.scale));
        // Rebuild clears widget state; replay the last producer snapshots and
        // a fresh clock tick so the bar does not go empty until the next event.
        replay_snapshot(&mut self.dashboard, seed);
        self.redraw.request("config-reload");
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

    /// Apply a message to the dashboard; request a repaint only if a widget
    /// reported a visible change. This is the steady-state redraw policy: the
    /// loop stays idle when an update changes nothing on screen, and a burst of
    /// changes shares the one frame [`Surface::flush`] paints.
    fn handle(&mut self, msg: &Msg) -> bool {
        let changed = self.dashboard.update(msg);
        if changed {
            self.redraw.request(message_kind(msg));
        }
        changed
    }

    /// Adopt a new output buffer scale.
    ///
    /// Re-resolves the physical font size from this output's configuration so
    /// text stays crisp at the new density, then requests a repaint so the
    /// buffer is reallocated at the new physical size. A no-op when the scale is
    /// unchanged.
    fn set_scale(&mut self, scale: Scale) -> bool {
        if self.scale == scale {
            return false;
        }
        self.scale = scale;
        self.ctx
            .set_settings(self.config.scaled_render_settings(scale));
        self.redraw.request("scale-change");
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

    /// Resolve what the pointer is over at surface-local logical coordinates:
    /// whether it is clickable and which tooltip it shows, in one hit-test.
    fn hover_at(&self, x: f64, y: f64) -> Hover {
        if x < 0.0 || y < 0.0 {
            return Hover::default();
        }
        let scale = self.scale.get() as f64;
        self.dashboard
            .hover_at((x * scale) as u32, (y * scale) as u32)
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

    /// Adopt the compositor's configure, seeding and requesting the first frame.
    fn configure(&mut self, configure: LayerSurfaceConfigure) {
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
            // immediately, then paint whether or not anything changed.
            self.dashboard.update(&Msg::tick_now());
            self.redraw.request("configure");
        }
    }

    /// Paint the requested frame, if there is one and the compositor is ready for
    /// it. Returns whether a frame was committed.
    ///
    /// The host loop calls this once per dispatch, after every pending message
    /// has been applied, so however many changes arrived they share one repaint.
    /// Nothing is painted before the first configure (drawing earlier is invalid
    /// per the layer-shell protocol) or while the previous commit's frame
    /// callback is outstanding; the request stays pending in both cases.
    fn flush(&mut self, pool: &mut SlotPool) -> bool {
        if !self.configured {
            return false;
        }
        match self.redraw.take_due(Instant::now()) {
            Some(cause) => self.draw(pool, cause),
            None => false,
        }
    }

    /// Render the current dashboard state and commit it through the app's shared
    /// shared-memory pool. Returns whether the frame was committed.
    ///
    /// Uses two alternating SHM slots: when the compositor has released the
    /// next slot, its mmap is reused instead of allocating a new one.
    fn draw(&mut self, pool: &mut SlotPool, cause: &'static str) -> bool {
        let started = self.performance.start();

        // Logical surface dimensions scale up to the physical buffer the
        // compositor maps back down via `set_buffer_scale`. Everything below this
        // point — buffer, render target, layout, font — works in physical pixels.
        let (width, height) = self.scale.to_physical_size(self.width, self.height);
        let stride = width as i32 * 4;

        if self.buffer_px != (width, height) {
            self.buffers = [None, None];
            self.buffer_px = (width, height);
            // Nothing on screen matches a buffer of another size.
            self.dashboard.invalidate();
        }

        let idx = self.next_buffer;
        self.next_buffer = 1 - self.next_buffer;

        let can_reuse = self.buffers[idx]
            .as_ref()
            .is_some_and(|buf| !buf.slot().has_active_buffers());

        let timings = if can_reuse {
            match pool.canvas(self.buffers[idx].as_ref().unwrap()) {
                Some(canvas) => Some(self.paint_frame(canvas, width, height)),
                None => {
                    // Race: became active between the check and canvas(). Allocate.
                    self.paint_new_buffer(pool, idx, width, height, stride)
                }
            }
        } else {
            self.paint_new_buffer(pool, idx, width, height, stride)
        };
        let damage = timings.as_ref().map(|timings| timings.damage);
        let committed = damage.is_some_and(|damage| self.commit_buffer(idx, width, height, damage));
        if !committed {
            // The compositor never saw this frame, so the next one cannot be
            // described as a change relative to it.
            self.dashboard.invalidate();
        }
        let total_elapsed = started.map(|started| started.elapsed());
        let output = self.monitor.as_deref().unwrap_or("unknown");
        self.performance.record_duration(
            "frame-total",
            total_elapsed,
            format_args!(
                "cause={cause} output={output} width={width} height={height} reused={can_reuse} committed={committed} damage={}",
                damage.map_or_else(|| "none".to_owned(), damage_label)
            ),
        );
        if let Some(timings) = timings {
            self.record_frame_timings(timings, cause, width, height);
        }
        if committed && !self.presented {
            self.presented = true;
            self.performance.record_process_elapsed(
                "startup-to-first-commit",
                format_args!(
                    "output={} width={width} height={height}",
                    self.monitor.as_deref().unwrap_or("unknown")
                ),
            );
        }
        committed
    }

    fn paint_frame(&mut self, canvas: &mut [u8], width: u32, height: u32) -> FrameTimings {
        let started = self.performance.start();
        // Zero for a surface's first frame, so its counters are still logged.
        let idle_gap = started.map(|now| {
            self.last_frame
                .map_or(Duration::ZERO, |last| now.duration_since(last))
        });
        self.last_frame = started;
        self.ctx.resize(width, height);

        let phase_started = self.performance.start();
        self.dashboard.layout(&mut self.ctx, width, height);
        let damage = self.dashboard.take_damage();
        let layout = phase_started.map(|started| started.elapsed());

        let phase_started = self.performance.start();
        self.dashboard.draw(&mut self.ctx);
        let draw = phase_started.map(|started| started.elapsed());

        let phase_started = self.performance.start();
        write_argb8888(self.ctx.pixels(), canvas);
        let blit = phase_started.map(|started| started.elapsed());
        let render = started.map(|started| started.elapsed());
        FrameTimings {
            layout,
            draw,
            blit,
            render,
            idle_gap,
            // Taken even when diagnostics are off so the counters never wrap.
            stats: self.ctx.take_stats(),
            damage,
        }
    }

    fn record_frame_timings(
        &self,
        timings: FrameTimings,
        cause: &'static str,
        width: u32,
        height: u32,
    ) {
        let output = self.monitor.as_deref().unwrap_or("unknown");
        for (metric, elapsed) in [
            ("frame-layout", timings.layout),
            ("frame-draw", timings.draw),
            ("frame-blit", timings.blit),
            ("frame-render", timings.render),
        ] {
            self.performance.record_duration(
                metric,
                elapsed,
                format_args!("cause={cause} output={output} width={width} height={height}"),
            );
        }
        let RenderStats {
            text_shapes,
            text_cache_hits,
            glyph_pixels,
        } = timings.stats;
        self.performance.record_duration(
            "frame-idle-gap",
            timings.idle_gap,
            format_args!(
                "cause={cause} output={output} text_shapes={text_shapes} text_cache_hits={text_cache_hits} glyph_pixels={glyph_pixels}"
            ),
        );
    }

    fn paint_new_buffer(
        &mut self,
        pool: &mut SlotPool,
        idx: usize,
        width: u32,
        height: u32,
        stride: i32,
    ) -> Option<FrameTimings> {
        let (buffer, canvas) = match pool.create_buffer(
            width as i32,
            height as i32,
            stride,
            wl_shm::Format::Argb8888,
        ) {
            Ok(parts) => parts,
            Err(e) => {
                error!("failed to create shm buffer: {e}");
                return None;
            }
        };
        let timings = self.paint_frame(canvas, width, height);
        self.buffers[idx] = Some(buffer);
        Some(timings)
    }

    /// Attach slot `idx` and commit it, damaging only what `damage` covers so the
    /// compositor re-composites (and re-blurs) just that part of the strip. The
    /// buffer itself is always painted in full, so either slot is a complete
    /// frame whichever the compositor shows.
    fn commit_buffer(&mut self, idx: usize, width: u32, height: u32, damage: Damage) -> bool {
        let Some(buffer) = self.buffers[idx].as_ref() else {
            return false;
        };
        let surface = self.layer.wl_surface();
        // Tell the compositor the buffer holds `scale`× physical pixels per
        // logical pixel, so it maps the larger buffer back to the logical size.
        surface.set_buffer_scale(self.scale.get() as i32);
        let (x, y, w, h) = damage_rect(damage, width, height);
        surface.damage_buffer(x, y, w, h);
        if let Err(e) = buffer.attach_to(surface) {
            error!("failed to attach buffer: {e}");
            return false;
        }
        // Ask to be told when this frame has been shown; repaints wait for it.
        surface.frame(&self.qh, surface.clone());
        self.layer.commit();
        self.redraw.committed(Instant::now());
        true
    }
}

/// `damage` as a `wl_surface.damage_buffer` rectangle, clipped to the buffer.
fn damage_rect(damage: Damage, width: u32, height: u32) -> (i32, i32, i32, i32) {
    let full = Bounds::new(0, 0, width, height);
    let region = match damage {
        Damage::Full => full,
        Damage::Region(region) => region,
    };
    let x = region.x.min(width);
    let y = region.y.min(height);
    let w = region.width.min(width - x);
    let h = region.height.min(height - y);
    if w == 0 || h == 0 {
        // A change that paints nothing visible still has to present the buffer.
        return (0, 0, width as i32, height as i32);
    }
    (x as i32, y as i32, w as i32, h as i32)
}

fn damage_label(damage: Damage) -> String {
    match damage {
        Damage::Full => "full".to_owned(),
        Damage::Region(region) => format!("{}x{}", region.width, region.height),
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
    performance: PerformanceLogger,
    requested_at: Option<Instant>,
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
    requested_at: Option<Instant>,
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
    performance: PerformanceLogger,
    requested_at: Option<Instant>,
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
        // The context outlives a popup and the panel is translucent, so start
        // from clear pixels rather than blending over the previous paint.
        self.ctx.fill_background();
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
        self.performance.record_since(
            "tray-menu-input-to-commit",
            self.requested_at.take(),
            format_args!("output={} rows={}", self.output_id, self.rows.len()),
        );
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

        paint_tooltip(
            &mut self.ctx,
            &self.text,
            (width, height),
            self.background,
            self.foreground,
        );
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
        self.performance.record_since(
            "tooltip-input-to-commit",
            self.requested_at.take(),
            format_args!("output={} width={width} height={height}", self.output_id),
        );
    }
}

/// Paint a tooltip panel of physical `size` holding `text` into `ctx`.
fn paint_tooltip(
    ctx: &mut RenderContext,
    text: &str,
    (width, height): (u32, u32),
    background: (u8, u8, u8, u8),
    foreground: (u8, u8, u8, u8),
) {
    let scale = ctx.scale_factor();
    ctx.resize(width, height);
    // The context outlives a popup and the panel is translucent, so start from
    // clear pixels rather than blending over the previous paint.
    ctx.fill_background();
    ctx.fill_rounded_rect(
        Bounds::new(0, 0, width, height),
        background,
        POPUP_RADIUS * scale as f32,
    );
    let padding_x = POPUP_PADDING_X * scale;
    let padding_y = POPUP_PADDING_Y * scale;
    let line_height = tooltip_line_height(ctx);
    for (index, line) in text.lines().enumerate() {
        ctx.draw_text(
            line,
            Bounds::new(
                padding_x,
                padding_y + index as u32 * line_height,
                width.saturating_sub(2 * padding_x),
                line_height,
            ),
            foreground,
        );
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
    /// The render context of the last popup hidden, reused by the next one.
    popup_ctx: Option<RenderContext>,
    /// Process-wide font set shared by every surface and popup.
    fonts: SharedFonts,
    /// Watches app config and active/candidate theme dependencies.
    config_watcher: Option<config_reload::ConfigWatcher>,
    /// Latest producer messages, replayed after a dashboard rebuild on reload.
    producer_snapshot: ProducerSnapshot,
    /// Opt-in latency diagnostics shared with surfaces and popups.
    performance: PerformanceLogger,
    /// Workspace click awaiting the matching Hyprland state.
    pending_workspace: Option<PendingWorkspace>,
    /// Workspace click whose state has arrived, awaiting the bar commit.
    committing_workspace: Option<PendingWorkspace>,
    /// Calloop handle used to arm the frame-callback timeout. `None` until the
    /// loop exists.
    loop_handle: Option<LoopHandle<'static, App>>,
    /// Armed timeout for a repaint blocked on a frame callback, if any.
    redraw_wake: Option<RegistrationToken>,
    /// Deadline that timeout will fire at, so a repeat flush does not replace
    /// a timer that already matches.
    redraw_wake_at: Option<Instant>,
    exit: bool,
}

struct PendingWorkspace {
    target: i32,
    monitor: Option<String>,
    started: Option<Instant>,
}

/// Latest producer payloads retained for config hot-reload reseeding.
///
/// Rebuilds replace every widget; without a snapshot the bar goes blank until
/// each source next emits (workspace/title/volume can stay empty for a long
/// time). Transient menu messages are not stored.
#[derive(Default)]
struct ProducerSnapshot {
    messages: Vec<Msg>,
}

fn replay_snapshot(dashboard: &mut Dashboard, seed: &[Msg]) {
    for msg in seed {
        let _ = dashboard.update(msg);
    }
    let _ = dashboard.update(&Msg::tick_now());
}

impl ProducerSnapshot {
    fn note(&mut self, msg: &Msg) {
        if matches!(
            msg,
            Msg::Tick(_) | Msg::TrayMenu(_) | Msg::TrayMenuUnavailable(_)
        ) {
            return;
        }
        let key = snapshot_key(msg);
        if let Some(slot) = self
            .messages
            .iter_mut()
            .find(|existing| snapshot_key(existing) == key)
        {
            *slot = msg.clone();
        } else {
            self.messages.push(msg.clone());
        }
    }

    fn as_slice(&self) -> &[Msg] {
        &self.messages
    }
}

/// Stable key for the latest-message map. Active-window is per-monitor so a
/// dual-head setup keeps both titles across reload.
fn message_kind(msg: &Msg) -> &'static str {
    match msg {
        Msg::Tick(_) => "tick",
        Msg::Workspaces(_) => "workspaces",
        Msg::Battery(_) => "battery",
        Msg::Backlight(_) => "backlight",
        Msg::System(_) => "system",
        Msg::Network(_) => "network",
        Msg::Bluetooth(_) => "bluetooth",
        Msg::Tray(_) => "tray",
        Msg::TrayMenu(_) => "tray-menu",
        Msg::TrayMenuUnavailable(_) => "tray-menu-unavailable",
        Msg::ActiveWindow { .. } => "active-window",
        Msg::Volume(_) => "volume",
        Msg::Notifications(_) => "notifications",
        Msg::PowerProfiles(_) => "power-profiles",
        Msg::Updates(_) => "updates",
        Msg::Hypridle(_) => "hypridle",
    }
}

fn snapshot_key(msg: &Msg) -> String {
    match msg {
        Msg::Tick(_) => "tick".into(),
        Msg::Workspaces(_) => "workspaces".into(),
        Msg::Battery(_) => "battery".into(),
        Msg::Backlight(_) => "backlight".into(),
        Msg::System(_) => "system".into(),
        Msg::Network(_) => "network".into(),
        Msg::Bluetooth(_) => "bluetooth".into(),
        Msg::Tray(_) => "tray".into(),
        Msg::TrayMenu(_) => "tray-menu".into(),
        Msg::TrayMenuUnavailable(_) => "tray-menu-unavail".into(),
        Msg::ActiveWindow { monitor, .. } => format!("aw:{monitor}"),
        Msg::Volume(_) => "volume".into(),
        Msg::Notifications(_) => "notifications".into(),
        Msg::PowerProfiles(_) => "power-profiles".into(),
        Msg::Updates(_) => "updates".into(),
        Msg::Hypridle(_) => "hypridle".into(),
    }
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
        let performance = self.performance.clone();
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
                performance,
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
        let seed: Vec<Msg> = self.producer_snapshot.as_slice().to_vec();
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
                surface.apply_config(resolved, &seed);
            }
        }
        info!("config reloaded");
    }

    /// Poll app/theme dependencies and apply only fully validated candidates.
    ///
    /// Debounces ~400ms so editors that truncate-then-write do not apply an
    /// empty mid-save document (which would parse as defaults and wipe the bar).
    fn poll_config_reload(&mut self) {
        if let Some(candidate) = self
            .config_watcher
            .as_mut()
            .and_then(|watcher| watcher.poll(Instant::now()))
        {
            match candidate {
                Ok(config) if &config != self.outputs.base() => self.reload_config(config),
                Ok(_) => {}
                Err(error) => warn!("config reload ignored: {error}"),
            }
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

    /// Fan a message out to every output's dashboard; the ones that changed
    /// repaint at the next [`App::flush_redraws`]. The clock timer and every
    /// producer reach all bars this way.
    fn handle_all(&mut self, msg: &Msg) {
        self.producer_snapshot.note(msg);
        let mut changed = false;
        for surface in self.outputs.values_mut() {
            changed |= surface.handle(msg);
        }
        if changed && matches!(msg, Msg::PowerProfiles(_)) {
            self.hide_tooltip();
        }
        if changed
            && let Msg::Workspaces(workspaces) = msg
            && self.pending_workspace.as_ref().is_some_and(|pending| {
                pending.monitor.as_deref().map_or_else(
                    || workspaces.active() == pending.target,
                    |monitor| workspaces.active_for(monitor) == Some(pending.target),
                )
            })
        {
            self.committing_workspace = self.pending_workspace.take();
        }
    }

    /// Paint every bar with a repaint pending. Runs once per loop dispatch, after
    /// all of that dispatch's messages and Wayland events have been applied.
    fn flush_redraws(&mut self) {
        let App { pool, outputs, .. } = self;
        let mut committed = false;
        for surface in outputs.values_mut() {
            committed |= surface.flush(pool);
        }
        if committed && let Some(pending) = self.committing_workspace.take() {
            self.performance.record_since(
                "workspace-input-to-commit",
                pending.started,
                format_args!(
                    "target={} output={}",
                    pending.target,
                    pending.monitor.as_deref().unwrap_or("unknown")
                ),
            );
        }
        self.arm_redraw_wake();
    }

    /// Wake the loop when a repaint is stuck behind a frame callback that may
    /// never arrive. [`RedrawScheduler::take_due`] only notices the timeout if
    /// something else has already woken the loop, so a workspace change that
    /// lands in that window would otherwise sit until the next clock or
    /// producer tick.
    fn arm_redraw_wake(&mut self) {
        let Some(handle) = self.loop_handle.clone() else {
            return;
        };
        let deadline = self
            .outputs
            .values()
            .filter_map(|surface| surface.redraw.wake_deadline())
            .min();
        if self.redraw_wake_at == deadline {
            return;
        }
        if let Some(token) = self.redraw_wake.take() {
            handle.remove(token);
        }
        self.redraw_wake_at = deadline;
        let Some(deadline) = deadline else {
            return;
        };
        match handle.insert_source(Timer::from_deadline(deadline), |_deadline, _, app| {
            // This source is finished. Clear it before flushing so the flush
            // can arm the next deadline without removing the timer that is
            // currently firing.
            app.redraw_wake = None;
            app.redraw_wake_at = None;
            app.flush_redraws();
            TimeoutAction::Drop
        }) {
            Ok(token) => self.redraw_wake = Some(token),
            Err(error) => {
                self.redraw_wake_at = None;
                error!("redraw timeout could not be armed: {error}");
            }
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
        if let Some(tooltip) = self.tooltip.take() {
            self.popup_ctx = Some(tooltip.ctx);
        }
    }

    fn hide_tray_menu(&mut self) {
        self.pending_tray_menu = None;
        if let Some(menu) = self.tray_menu.take() {
            self.popup_ctx = Some(menu.ctx);
        }
    }

    /// A render context for a new popup, painting with `settings`.
    ///
    /// The context of the last popup hidden is handed back here rather than
    /// dropped, so a tooltip shown again finds its lines already shaped in the
    /// text cache instead of starting from an empty context on every hover.
    fn popup_context(&mut self, settings: RenderSettings) -> RenderContext {
        match self.popup_ctx.take() {
            Some(mut ctx) => {
                ctx.set_settings(settings);
                ctx
            }
            None => RenderContext::with_fonts(1, 1, settings, self.fonts.clone()),
        }
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

    /// Show `tooltip` under the bar on `output_id`, or hide the current one when
    /// the pointer is over nothing that has one.
    fn update_tooltip(
        &mut self,
        output_id: OutputId,
        tooltip: Option<Tooltip>,
        qh: &QueueHandle<App>,
    ) {
        let Some(tooltip) = tooltip else {
            self.hide_tooltip();
            return;
        };
        // Most motion stays over the widget whose tooltip is already up, so that
        // is settled before anything is cloned or measured.
        if self
            .tooltip
            .as_ref()
            .is_some_and(|shown| shown.output_id == output_id && shown.text == tooltip.text)
        {
            return;
        }
        let requested_at = self.performance.start();
        let Some(bar) = self.outputs.get(output_id) else {
            self.hide_tooltip();
            return;
        };
        let parent = bar.layer.clone();
        let scale = bar.scale;
        let mut settings = bar.config.scaled_render_settings(scale);

        let background = popup_background(settings.background);
        let foreground = settings.foreground;
        settings.background = (0, 0, 0, 0);
        self.hide_tooltip();
        let mut ctx = self.popup_context(settings);
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
            performance: self.performance.clone(),
            requested_at,
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
        self.hide_tooltip();
        let mut ctx = self.popup_context(settings);
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
            performance: self.performance.clone(),
            requested_at: pending.requested_at,
        });
    }
}

/// The stable per-output key: the `wl_output`'s protocol object id.
///
/// Available in both `new_output` and `output_destroyed` (unlike the output's
/// advertised info, which the compositor may already have dropped by teardown),
/// and unique per output for its lifetime — exactly the key the registry needs.
fn output_key(output: &wl_output::WlOutput) -> OutputId {
    output.id().protocol_id()
}

fn command_kind(command: &Command) -> &'static str {
    match command {
        Command::SwitchWorkspace(_) => "switch-workspace",
        Command::ActivateTrayItem { .. } => "activate-tray-item",
        Command::OpenTrayMenu { .. } => "open-tray-menu",
        Command::ActivateTrayMenuItem { .. } => "activate-tray-menu-item",
        Command::RunProgram(_) => "run-program",
        Command::ToggleNotificationPanel => "toggle-notification-panel",
        Command::ToggleNotificationsDnd => "toggle-notifications-dnd",
        Command::AdjustBacklight { .. } => "adjust-backlight",
        Command::SetPowerProfile(_) => "set-power-profile",
        Command::SetHypridle(_) => "set-hypridle",
    }
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
/// app/theme file changes and the bar hot-reloads theme/layout (producers keep their
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
        producers.push(Box::new(
            config
                .widget
                .system
                .interval()
                .map_or_else(SystemProducer::new, SystemProducer::with_interval),
        ));
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
        producers.push(Box::new(
            PowerProfilesProducer::new().with_settings(power_profiles_settings(&config)),
        ));
    }
    if config.uses_widget(WidgetKind::Updates) {
        producers.push(Box::new(
            config
                .widget
                .updates
                .interval()
                .map_or_else(UpdatesProducer::new, UpdatesProducer::with_interval),
        ));
    }
    if config.uses_widget(WidgetKind::Hypridle) {
        producers.push(Box::new(
            config
                .widget
                .hypridle
                .interval()
                .map_or_else(HypridleProducer::new, HypridleProducer::with_interval),
        ));
    }
    run_with_producers(config, producers, config_path)
}

/// Run the config poll timer until the watcher no longer needs one. `polling`
/// keeps a burst of directory events from stacking up timers.
fn start_config_polling(handle: &LoopHandle<'static, App>, polling: &Rc<Cell<bool>>) {
    if polling.replace(true) {
        return;
    }
    let polling = polling.clone();
    let inserted = handle.insert_source(Timer::immediate(), move |_deadline, _, app| {
        app.poll_config_reload();
        match app.config_watcher.as_ref().and_then(|w| w.next_poll()) {
            Some(delay) => TimeoutAction::ToDuration(delay),
            None => {
                polling.set(false);
                TimeoutAction::Drop
            }
        }
    });
    if let Err(error) = inserted {
        error!("config reload timer could not start: {error}");
    }
}

fn power_profiles_settings(config: &Config) -> PowerProfilesSettings {
    PowerProfilesSettings::new(
        config
            .widget
            .power_profiles_daemon
            .hardware_control
            .unwrap_or(true),
        config
            .widget
            .power_profiles_daemon
            .hardware_helper
            .as_ref()
            .map(PathBuf::from),
    )
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
/// the loop. `config_path` enables hot-reload of the TOML config and selected theme.
pub fn run_with_producers(
    config: Config,
    producers: Vec<Box<dyn Producer>>,
    config_path: Option<PathBuf>,
) -> Result<(), Box<dyn Error>> {
    let performance = PerformanceLogger::from_env();
    let power_settings = power_profiles_settings(&config);
    let height = config.height;
    let config_watcher = config_path.map(|path| config_reload::ConfigWatcher::new(path, &config));
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
        popup_ctx: None,
        fonts,
        config_watcher,
        producer_snapshot: ProducerSnapshot::default(),
        performance,
        pending_workspace: None,
        committing_workspace: None,
        loop_handle: None,
        redraw_wake: None,
        redraw_wake_at: None,
        exit: false,
    };

    // Finish initial output discovery before producers can emit snapshots. If a
    // producer starts first, its one-shot initial state is dispatched while
    // `outputs` is empty and is lost until that source changes again.
    event_queue.roundtrip(&mut app)?;

    let mut event_loop: EventLoop<App> = EventLoop::try_new()?;
    let handle = event_loop.handle();
    app.loop_handle = Some(handle.clone());

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

    // Theme/layout edits apply without restarting. Directory events say when to
    // look, so an idle bar has no reload timer at all: the poll timer runs only
    // while an edit settles (and for the first validation of the on-disk state),
    // or permanently if the files cannot be watched.
    if let Some(watcher) = &app.config_watcher {
        let polling = Rc::new(Cell::new(false));
        if let Some(fd) = watcher.event_fd() {
            let (timer_handle, polling) = (handle.clone(), polling.clone());
            let events = Generic::new(fd, Interest::READ, Mode::Level);
            handle.insert_source(events, move |_, fd, _app| {
                config_reload::drain_events(&**fd);
                start_config_polling(&timer_handle, &polling);
                Ok(PostAction::Continue)
            })?;
        }
        start_config_polling(&handle, &polling);
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
            power_profiles::run_commands(power_rx, power_settings),
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

    // A configure handled during the roundtrip above has a frame waiting.
    app.flush_redraws();

    let signal = event_loop.get_signal();
    event_loop.run(None, &mut app, move |app| {
        // Requests made here go out before the loop next sleeps.
        app.flush_redraws();
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
            outputs,
            tooltip,
            tray_menu,
            ..
        } = self;
        let scale = Scale::new(new_factor);
        let changed_output = outputs
            .values_mut()
            .find(|bar| bar.owns(surface))
            .and_then(|bar| bar.set_scale(scale).then_some(bar.output_id));
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
        surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        // Requested only alongside a commit, so this never becomes a redraw
        // cycle: it releases the repaint (if any) that waited for this frame,
        // which the loop's post-dispatch flush then paints.
        if let Some(bar) = self.outputs.values_mut().find(|bar| bar.owns(surface)) {
            bar.redraw.frame_done();
        }
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
        if let Some(bar) = self.outputs.values_mut().find(|bar| bar.is_layer(layer)) {
            bar.configure(configure);
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
                let hover = self
                    .outputs
                    .values()
                    .find(|bar| bar.owns(&event.surface))
                    .map(|bar| (bar.output_id, bar.hover_at(x, y)));
                if let Some((output_id, hover)) = hover {
                    self.update_tooltip(output_id, hover.tooltip, _qh);
                    self.set_pointer_cursor(
                        conn,
                        if hover.clickable {
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
                let input_started = self.performance.start();
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
                            bar.monitor.clone(),
                        ))
                    });
                if let Some((
                    mut command,
                    parent,
                    output,
                    output_id,
                    scale,
                    bar_height,
                    settings,
                    monitor,
                )) = interaction
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
                            requested_at: input_started,
                        });
                    }
                    if let (Command::SwitchWorkspace(target), Some(started)) =
                        (&command, input_started)
                    {
                        self.pending_workspace = Some(PendingWorkspace {
                            target: *target,
                            monitor,
                            started: Some(started),
                        });
                    }
                    let kind = command_kind(&command);
                    for sender in &self.commands {
                        if sender.send(command.clone()).is_err() {
                            warn!("command channel closed; dropping click command");
                        }
                    }
                    self.performance.record_since(
                        "click-to-command-queue",
                        input_started,
                        format_args!("command={kind} output={output_id}"),
                    );
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
    fn theme_reload_replays_latest_producers_into_rebuilt_dashboards() {
        use crate::widget::{ActiveWindow, DeviceKind, Volume, Workspaces};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let theme = dir.path().join("theme.toml");
        std::fs::write(&path, "[appearance]\ntheme_file = 'theme.toml'\n[bar]\nmodules-left = ['workspaces']\nmodules-center = ['title']\nmodules-right = ['volume']").unwrap();
        let text = include_str!("../tests/fixtures/swatches.toml");
        std::fs::write(&theme, text).unwrap();
        let mut snapshot = ProducerSnapshot::default();
        snapshot.note(&Msg::Workspaces(Workspaces::new([1], 1)));
        snapshot.note(&Msg::Workspaces(Workspaces::with_monitors(
            [(3, "DP-1"), (7, "HDMI-A-1")],
            [("DP-1", 3), ("HDMI-A-1", 7)],
            3,
        )));
        for monitor in ["DP-1", "HDMI-A-1"] {
            snapshot.note(&Msg::ActiveWindow {
                monitor: monitor.into(),
                window: Some(ActiveWindow::new("app", "superseded title")),
            });
            snapshot.note(&Msg::ActiveWindow {
                monitor: monitor.into(),
                window: Some(ActiveWindow::new("app", monitor)),
            });
        }
        snapshot.note(&Msg::Volume(Some(Volume::new(
            0.12,
            true,
            DeviceKind::Speakers,
        ))));
        let volume = Msg::Volume(Some(Volume::new(0.73, false, DeviceKind::Headphones)));
        snapshot.note(&volume);
        snapshot.note(&Msg::tick_now());
        snapshot.note(&Msg::TrayMenuUnavailable("stale".into()));
        assert_eq!(snapshot.as_slice().len(), 4);
        for accent in ["#80D4FF", "#010203"] {
            std::fs::write(&theme, text.replace("#80D4FF", accent)).unwrap();
            let config = Config::load_for_reload(&path).unwrap();
            for monitor in ["DP-1", "HDMI-A-1"] {
                let bounds = Bounds::new(0, 0, 800, 32);
                let mut dashboard = config.build_dashboard(bounds, Some(monitor));
                replay_snapshot(&mut dashboard, snapshot.as_slice());
                let title = Msg::ActiveWindow {
                    monitor: monitor.into(),
                    window: Some(ActiveWindow::new("app", monitor)),
                };
                // A fresh dashboard must consume these readings, whereas the
                // replayed one already has the latest title and volume. This
                // guards against accidentally testing an absent widget.
                let mut fresh = config.build_dashboard(bounds, Some(monitor));
                assert!(fresh.update(&title));
                assert!(fresh.update(&volume));
                assert!(
                    !dashboard.update(&title),
                    "latest title restored for {monitor}"
                );
                assert!(
                    !dashboard.update(&volume),
                    "latest volume restored for {monitor}"
                );

                let mut ctx = RenderContext::with_settings(800, 32, config.render_settings());
                dashboard.layout(&mut ctx, 800, 32);
                dashboard.draw(&mut ctx);
                let restored_pixels = ctx.pixels().to_vec();
                assert_eq!(
                    dashboard.on_click(10, 16, ClickButton::Left),
                    Some(Command::SwitchWorkspace(if monitor == "DP-1" {
                        3
                    } else {
                        7
                    }))
                );

                // Each restored value must be visible, not merely retained in
                // the snapshot map. Clearing it changes pixels; restoring it
                // recreates the exact pre-clear frame without another producer.
                for (clear, restore) in [
                    (
                        Msg::ActiveWindow {
                            monitor: monitor.into(),
                            window: None,
                        },
                        title,
                    ),
                    (Msg::Volume(None), volume.clone()),
                ] {
                    assert!(dashboard.update(&clear));
                    dashboard.layout(&mut ctx, 800, 32);
                    dashboard.draw(&mut ctx);
                    assert_ne!(ctx.pixels(), restored_pixels.as_slice());
                    assert!(dashboard.update(&restore));
                    dashboard.layout(&mut ctx, 800, 32);
                    dashboard.draw(&mut ctx);
                    assert_eq!(ctx.pixels(), restored_pixels.as_slice());
                }
            }
            assert_eq!(snapshot.as_slice().len(), 4);
            assert_eq!(
                popup_background(config.render_settings().background),
                (16, 37, 63, 255)
            );
        }
    }

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

    #[test]
    fn full_damage_covers_the_whole_buffer() {
        assert_eq!(damage_rect(Damage::Full, 3840, 76), (0, 0, 3840, 76));
    }

    #[test]
    fn region_damage_is_passed_through_in_buffer_pixels() {
        let region = Damage::Region(Bounds::new(3600, 8, 200, 60));
        assert_eq!(damage_rect(region, 3840, 76), (3600, 8, 200, 60));
    }

    #[test]
    fn region_damage_is_clipped_to_the_buffer() {
        let region = Damage::Region(Bounds::new(3800, 70, 200, 60));
        assert_eq!(damage_rect(region, 3840, 76), (3800, 70, 40, 6));
    }

    #[test]
    fn an_empty_or_offscreen_region_falls_back_to_full_damage() {
        for region in [Bounds::new(10, 10, 0, 20), Bounds::new(5000, 0, 10, 10)] {
            assert_eq!(
                damage_rect(Damage::Region(region), 3840, 76),
                (0, 0, 3840, 76)
            );
        }
    }

    fn tooltip_pixels(ctx: &mut RenderContext, text: &str, size: (u32, u32)) -> Vec<u8> {
        paint_tooltip(
            ctx,
            text,
            size,
            POPUP_FALLBACK_BACKGROUND,
            (0xFF, 0xFF, 0xFF, 0xFF),
        );
        ctx.pixels().to_vec()
    }

    fn popup_settings() -> RenderSettings {
        RenderSettings {
            background: (0, 0, 0, 0),
            ..Config::default().render_settings()
        }
    }

    #[test]
    fn a_reused_popup_context_paints_what_a_fresh_one_would() {
        // One panel size for both, so the second paint lands on the first one's
        // pixels rather than on a reallocated pixmap.
        let mut reused = RenderContext::with_settings(1, 1, popup_settings());
        let size = tooltip_size(&mut reused, "Hypridle inactive");
        tooltip_pixels(&mut reused, "Hypridle inactive", size);

        let mut fresh = RenderContext::with_settings(1, 1, popup_settings());
        assert_eq!(
            tooltip_pixels(&mut reused, "Hypridle active", size),
            tooltip_pixels(&mut fresh, "Hypridle active", size)
        );
    }

    #[test]
    fn a_reused_popup_context_does_not_reshape_a_tooltip_it_has_shown() {
        let mut ctx = RenderContext::with_settings(1, 1, popup_settings());
        let size = tooltip_size(&mut ctx, "Hypridle active");
        tooltip_pixels(&mut ctx, "Hypridle active", size);
        ctx.take_stats();

        ctx.set_settings(popup_settings());
        let size = tooltip_size(&mut ctx, "Hypridle active");
        tooltip_pixels(&mut ctx, "Hypridle active", size);

        assert_eq!(ctx.take_stats().text_shapes, 0);
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
