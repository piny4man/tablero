//! Wayland layer-shell front-end for tablero.
//!
//! Opens a top-anchored `wlr-layer-shell` surface under a compositor such as
//! Hyprland, renders a live clock through a shared-memory buffer, and drives
//! redraws from a [`calloop`] timer so the loop only wakes for clock ticks,
//! compositor events (configure, scale), or shutdown — never a busy redraw loop.
//!
//! Surface geometry is kept in logical pixels (the layer-shell size request and
//! exclusive zone), while the shared-memory buffer is allocated at the output's
//! physical pixel density: on a scaled output the buffer is `scale`× larger and
//! `set_buffer_scale` maps it back, so the bar stays crisp on HiDPI displays.
//! The logical→physical conversion lives entirely in [`tablero_core::scale`].

pub mod command;
pub mod hyprland;
pub mod networkmanager;
pub mod producer;
pub mod sni;
pub mod sysmon;
pub mod upower;

use std::error::Error;
use std::time::Duration;

use calloop::EventLoop;
use calloop::channel::Event as ChannelEvent;
use calloop::timer::{TimeoutAction, Timer};
use calloop_wayland_source::WaylandSource;
use log::{error, info, warn};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_pointer, delegate_registry,
    delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        pointer::{BTN_LEFT, PointerEvent, PointerEventKind, PointerHandler},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use tablero_core::blit::write_argb8888;
use tablero_core::clock::millis_until_next_second;
use tablero_core::config::Config;
use tablero_core::render::{Bounds, RenderContext};
use tablero_core::scale::Scale;
use tablero_core::widget::{Dashboard, Msg};

use crate::command::{CommandSender, command_channel};
use crate::hyprland::HyprlandProducer;
use crate::networkmanager::NetworkProducer;
use crate::producer::{Producer, ProducerBridge};
use crate::sni::SniHostProducer;
use crate::sysmon::SystemProducer;
use crate::upower::UPowerProducer;
use wayland_client::{
    Connection, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
};

/// Layer-shell namespace (also the compositor-visible surface name).
const NAMESPACE: &str = "tablero";

/// Assumed width (px) for the initial shared-memory pool, before the compositor
/// reports the real output width via the first configure event.
const INITIAL_WIDTH: u32 = 1920;

struct Bar {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,
    /// Surface dimensions in *logical* pixels (as the compositor reports them in
    /// `configure`). The shared-memory buffer is `scale`× larger; see [`Bar::draw`].
    width: u32,
    height: u32,
    /// The output's integer buffer scale. Drives the physical buffer size and the
    /// physical font size; `1` until the compositor reports otherwise.
    scale: Scale,
    /// The resolved configuration, retained so the physical font size can be
    /// re-resolved whenever the output scale changes.
    config: Config,
    /// Widgets composing the bar, plus the dirty-flag redraw policy over them.
    dashboard: Dashboard,
    /// Reused software-render target (font machinery + pixmap).
    ctx: RenderContext,
    /// The seat's pointer, created once the seat advertises the capability.
    pointer: Option<wl_pointer::WlPointer>,
    /// Outbound command channels into the producer runtime, one per command
    /// executor (Hyprland workspace switching, SNI tray activation). A click's
    /// command is fanned out to every executor; each ignores the commands it does
    /// not handle. Empty when no bridge is running, in which case clicks are
    /// dropped.
    commands: Vec<CommandSender>,
    /// Set once the first configure has been received; drawing before that is
    /// invalid per the layer-shell protocol.
    configured: bool,
    exit: bool,
}

impl Bar {
    /// Apply a message to the dashboard; redraw only if a widget reported a
    /// visible change. This is the steady-state redraw policy: the loop stays
    /// idle when an update changes nothing on screen.
    fn handle(&mut self, msg: &Msg) {
        if self.dashboard.update(msg) {
            self.draw();
        }
    }

    /// Adopt a new output buffer scale.
    ///
    /// Re-resolves the physical font size from the configuration so text stays
    /// crisp at the new density, then repaints (once configured) so the buffer is
    /// reallocated at the new physical size. A no-op when the scale is unchanged.
    fn set_scale(&mut self, scale: Scale) {
        if self.scale == scale {
            return;
        }
        self.scale = scale;
        self.ctx
            .set_settings(self.config.scaled_render_settings(scale));
        info!("output scale changed to {}x", scale.get());
        self.draw();
    }

    /// Route a left-button press at surface coordinates `(x, y)` to the
    /// dashboard, dispatching any resulting [`Command`](tablero_core::widget::Command)
    /// into the producer runtime. Clicks that hit no interactive region — empty
    /// space, display-only widgets, or off-surface negative coordinates — are
    /// ignored. With no command channels (no producer bridge) clicks are dropped.
    fn on_click(&mut self, x: f64, y: f64) {
        if x < 0.0 || y < 0.0 {
            return;
        }
        // Pointer coordinates are surface-local *logical* pixels, but the widgets
        // are laid out in physical pixels, so the click is scaled by the same
        // factor before the half-open hit-test — the one conversion that keeps
        // input and layout in the same space.
        let s = self.scale.get() as f64;
        let Some(command) = self.dashboard.on_click((x * s) as u32, (y * s) as u32) else {
            return;
        };
        // Fan the command out to every executor; each runs the commands it
        // handles and ignores the rest, so a single click reaches whichever
        // backend owns it without the loop needing to know which that is.
        for sender in &self.commands {
            if sender.send(command.clone()).is_err() {
                warn!("command channel closed; dropping click command");
            }
        }
    }

    /// Render the current dashboard state and commit it through the
    /// shared-memory buffer. Called on a visible change or when the Wayland
    /// lifecycle (first configure, resize) requires a fresh frame regardless.
    fn draw(&mut self) {
        if !self.configured {
            return;
        }

        // Logical surface dimensions scale up to the physical buffer the
        // compositor maps back down via `set_buffer_scale`. Everything below this
        // point — buffer, render target, layout, font — works in physical pixels.
        let (width, height) = self.scale.to_physical_size(self.width, self.height);
        let stride = width as i32 * 4;

        let (buffer, canvas) = match self.pool.create_buffer(
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

        self.ctx.resize(width, height);
        self.dashboard.layout(width, height);
        self.dashboard.draw(&mut self.ctx);
        write_argb8888(self.ctx.pixels(), canvas);

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

/// Open the bar and run its event loop until the compositor closes the surface.
///
/// The bar's height, theme, font, spacing, and widget order all come from
/// `config` (see [`tablero_core::config::Config`]). Wires the default producer
/// set — the Hyprland workspace source, the UPower battery source, the procfs
/// system-stats source, the NetworkManager connectivity source, and the
/// StatusNotifierItem tray host — so the bar shows live workspaces, battery,
/// CPU/memory load, network state, and tray icons alongside the clock. The clock
/// itself is still driven by the synchronous tick timer; see
/// [`run_with_producers`] to supply a custom producer set.
pub fn run(config: Config) -> Result<(), Box<dyn Error>> {
    run_with_producers(
        config,
        vec![
            Box::new(HyprlandProducer::new()),
            Box::new(UPowerProducer::new()),
            Box::new(SystemProducer::new()),
            Box::new(NetworkProducer::new()),
            Box::new(SniHostProducer::new()),
        ],
    )
}

/// Open the bar and run its event loop, additionally driving `producers` on an
/// off-thread Tokio runtime.
///
/// The render loop stays fully synchronous: it owns the dashboard, rendering,
/// and Wayland commits. Each producer runs on the [`ProducerBridge`] runtime and
/// reaches the loop only by sending [`Msg`]s through a calloop channel, which is
/// dispatched into [`Bar::handle`] exactly like the clock timer. With an empty
/// `producers` list no runtime is started at all.
pub fn run_with_producers(
    config: Config,
    producers: Vec<Box<dyn Producer>>,
) -> Result<(), Box<dyn Error>> {
    // The bar reserves exactly its own height so windows tile beneath it.
    let height = config.height;
    let exclusive_zone = height as i32;

    let conn = Connection::connect_to_env()?;
    let (globals, event_queue) = registry_queue_init::<Bar>(&conn)?;
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh)?;
    let shm = Shm::bind(&globals, &qh)?;

    let surface = compositor.create_surface(&qh);
    let layer = layer_shell.create_layer_surface(
        &qh,
        surface,
        Layer::Top,
        Some(NAMESPACE.to_string()),
        None,
    );
    // Top bar spanning the full output width.
    layer.set_anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    // Width 0 with left+right anchors lets the compositor stretch us to fit.
    layer.set_size(0, height);
    layer.set_exclusive_zone(exclusive_zone);
    // Initial commit with no buffer; the compositor replies with a configure.
    layer.commit();

    let pool = SlotPool::new((INITIAL_WIDTH * height * 4) as usize, &shm)?;

    // The configured widget order drives which widgets are built and in what
    // order; `Dashboard::layout` tiles them into columns each frame, so these
    // initial bounds are just placeholders. The theme and font reach the
    // renderer through the context's settings.
    let full = Bounds::new(0, 0, INITIAL_WIDTH, height);
    let dashboard = config.build_dashboard(full, None);
    let ctx = RenderContext::with_settings(INITIAL_WIDTH, height, config.render_settings());

    let mut bar = Bar {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        shm,
        pool,
        layer,
        width: INITIAL_WIDTH,
        height,
        // The compositor reports the real scale via `scale_factor_changed`,
        // typically before the first configure; until then assume an unscaled
        // output.
        scale: Scale::ONE,
        config,
        dashboard,
        ctx,
        pointer: None,
        commands: Vec::new(),
        configured: false,
        exit: false,
    };

    let mut event_loop: EventLoop<Bar> = EventLoop::try_new()?;
    let handle = event_loop.handle();

    // Wayland events (configure, close, ...) wake the loop.
    WaylandSource::new(conn, event_queue).insert(handle.clone())?;

    // A timer aligned to the wall-clock second wakes the loop for each tick.
    let timer = Timer::from_duration(Duration::from_millis(millis_until_next_second()));
    handle.insert_source(timer, |_deadline, _, bar| {
        bar.handle(&Msg::tick_now());
        TimeoutAction::ToDuration(Duration::from_millis(millis_until_next_second()))
    })?;

    // Bring up the async producer bridge only when there is async work to do.
    // The bridge owns the Tokio runtime and must outlive the loop, so it is held
    // in `_bridge` until the function returns.
    let _bridge = if producers.is_empty() {
        None
    } else {
        let (bridge, channel) = ProducerBridge::new()?;
        handle.insert_source(channel, |event, _, bar| {
            // Producer messages cross the channel into the same synchronous
            // app-state update path the clock timer uses.
            if let ChannelEvent::Msg(msg) = event {
                bar.handle(&msg);
            }
        })?;
        let count = producers.len();
        for producer in producers {
            bridge.spawn(producer);
        }
        // The reverse path: clicks become commands the executors run against the
        // compositor and the session bus. The loop holds a sender per executor and
        // fans each command out to all of them; an executor ignores commands it
        // does not handle, so workspace switches reach Hyprland and tray
        // activations reach the SNI items without the loop routing them.
        let (hypr_tx, hypr_rx) = command_channel();
        bridge.spawn_task("hyprland-commands", hyprland::run_commands(hypr_rx));
        let (sni_tx, sni_rx) = command_channel();
        bridge.spawn_task("sni-commands", sni::run_commands(sni_rx));
        bar.commands = vec![hypr_tx, sni_tx];
        info!("producer bridge started with {count} producer(s)");
        Some(bridge)
    };

    info!("tablero bar started: {height}px tall, exclusive_zone={exclusive_zone}");

    let signal = event_loop.get_signal();
    event_loop.run(None, &mut bar, move |bar| {
        if bar.exit {
            info!("compositor closed the surface; shutting down");
            signal.stop();
        }
    })?;

    Ok(())
}

impl CompositorHandler for Bar {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        // The compositor reports the surface's preferred integer buffer scale.
        // Adopt it so the bar renders at the output's pixel density.
        self.set_scale(Scale::new(new_factor));
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

impl OutputHandler for Bar {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl LayerShellHandler for Bar {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
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
            self.draw();
        }
    }
}

impl ShmHandler for Bar {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl SeatHandler for Bar {
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
            match self.seat_state.get_pointer(qh, &seat) {
                Ok(pointer) => self.pointer = Some(pointer),
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
        }
    }

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {
    }
}

impl PointerHandler for Bar {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        // Compare against an owned handle so the click dispatch can borrow
        // `self` mutably inside the loop.
        let surface = self.layer.wl_surface().clone();
        for event in events {
            if event.surface != surface {
                continue;
            }
            if let PointerEventKind::Press { button, .. } = event.kind
                && button == BTN_LEFT
            {
                let (x, y) = event.position;
                self.on_click(x, y);
            }
        }
    }
}

impl ProvidesRegistryState for Bar {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(Bar);
delegate_output!(Bar);
delegate_shm!(Bar);
delegate_layer!(Bar);
delegate_seat!(Bar);
delegate_pointer!(Bar);
delegate_registry!(Bar);
