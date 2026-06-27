//! Wayland layer-shell front-end for tablero.
//!
//! Opens a top-anchored `wlr-layer-shell` surface under a compositor such as
//! Hyprland, renders a live clock through a shared-memory buffer, and drives
//! redraws from a [`calloop`] timer so the loop only wakes for clock ticks,
//! compositor events (configure), or shutdown — never a busy redraw loop.

use std::error::Error;
use std::time::Duration;

use calloop::EventLoop;
use calloop::timer::{TimeoutAction, Timer};
use calloop_wayland_source::WaylandSource;
use log::{error, info};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
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
use tablero_core::render::{Bounds, RenderContext};
use tablero_core::widget::{ClockWidget, Dashboard, Msg};
use wayland_client::{
    Connection, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_output, wl_shm, wl_surface},
};

/// Configuration for the layer-shell bar surface.
#[derive(Debug, Clone)]
pub struct SurfaceConfig {
    /// Layer-shell namespace (also the compositor-visible surface name).
    pub namespace: String,
    /// Bar height in pixels. The width spans the output (anchored left+right).
    pub height: u32,
    /// Exclusive zone reserved so other windows don't overlap the bar.
    pub exclusive_zone: i32,
}

impl Default for SurfaceConfig {
    fn default() -> Self {
        Self {
            namespace: "tablero".to_string(),
            height: 32,
            exclusive_zone: 32,
        }
    }
}

/// Assumed width (px) for the initial shared-memory pool, before the compositor
/// reports the real output width via the first configure event.
const INITIAL_WIDTH: u32 = 1920;

struct Bar {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,
    width: u32,
    height: u32,
    /// Widgets composing the bar, plus the dirty-flag redraw policy over them.
    dashboard: Dashboard,
    /// Reused software-render target (font machinery + pixmap).
    ctx: RenderContext,
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

    /// Render the current dashboard state and commit it through the
    /// shared-memory buffer. Called on a visible change or when the Wayland
    /// lifecycle (first configure, resize) requires a fresh frame regardless.
    fn draw(&mut self) {
        if !self.configured {
            return;
        }

        let width = self.width;
        let height = self.height;
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
        surface.damage_buffer(0, 0, width as i32, height as i32);
        if let Err(e) = buffer.attach_to(surface) {
            error!("failed to attach buffer: {e}");
            return;
        }
        self.layer.commit();
    }
}

/// Open the bar and run its event loop until the compositor closes the surface.
pub fn run(config: SurfaceConfig) -> Result<(), Box<dyn Error>> {
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
        Some(config.namespace.clone()),
        None,
    );
    // Top bar spanning the full output width.
    layer.set_anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    // Width 0 with left+right anchors lets the compositor stretch us to fit.
    layer.set_size(0, config.height);
    layer.set_exclusive_zone(config.exclusive_zone);
    // Initial commit with no buffer; the compositor replies with a configure.
    layer.commit();

    let pool = SlotPool::new((INITIAL_WIDTH * config.height * 4) as usize, &shm)?;

    let clock = ClockWidget::new(Bounds::new(0, 0, INITIAL_WIDTH, config.height));
    let dashboard = Dashboard::new(vec![Box::new(clock)]);
    let ctx = RenderContext::new(INITIAL_WIDTH, config.height);

    let mut bar = Bar {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        shm,
        pool,
        layer,
        width: INITIAL_WIDTH,
        height: config.height,
        dashboard,
        ctx,
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

    info!(
        "tablero bar started: {}px tall, exclusive_zone={}",
        config.height, config.exclusive_zone
    );

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
        _new_factor: i32,
    ) {
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

impl ProvidesRegistryState for Bar {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

delegate_compositor!(Bar);
delegate_output!(Bar);
delegate_shm!(Bar);
delegate_layer!(Bar);
delegate_registry!(Bar);
