//! Spike (throwaway): run a TUI program in a centered floating panel, using
//! Rio's terminal core (`rio-vt`) for the VT state and the PTY.
//!
//! ```text
//! cargo run --release -p tablero --example tui-popup -- btm
//! ```
//!
//! From the bar, point a widget's `on-click` at the built binary:
//! `on-click = ["/…/target/release/examples/tui-popup", "btm"]`.
//!
//! The panel is an overlay layer surface with no anchors (the compositor
//! centers it), sized to 60% × 60% of the output, with exclusive keyboard
//! focus. It closes when the program exits, or on Ctrl+Shift+Q, which kills
//! the program. Timings are logged when it closes.
//!
//! rio-vt provides parsing, the grid and the PTY reader thread; everything on
//! screen (cell painting, glyphs, colors, input encoding) is this file.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use calloop::{EventLoop, LoopHandle};
use calloop_wayland_source::WaylandSource;
use cosmic_text::{
    Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache, SwashContent, Weight,
};
use rio_vt::ansi::CursorShape;
use rio_vt::config::colors::term::TermColors;
use rio_vt::config::colors::{AnsiColor, ColorArray, ColorRgb};
use rio_vt::corcovado;
use rio_vt::crosswords::grid::Scroll;
use rio_vt::crosswords::pos::Line;
use rio_vt::crosswords::square::{ContentTag, Square, Wide};
use rio_vt::crosswords::style::{Style, StyleFlags};
use rio_vt::crosswords::{Crosswords, CrosswordsSize, Mode, TermDamage};
use rio_vt::event::sync::FairMutex;
use rio_vt::event::{EventListener, Msg, RioEvent, WindowId, WindowSize};
use rio_vt::performer::{Machine, State as MachineState};
use rio_vt::teletypewriter::{self, ChildEvent, EventedPty, ProcessReadWrite, Pty, WinsizeBuilder};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::keyboard::{
    KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers,
};
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shm::slot::{Buffer as ShmBuffer, SlotPool};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm, registry_handlers,
};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::wl_keyboard::WlKeyboard;
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_pointer::WlPointer;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::protocol::wl_shm;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, QueueHandle};

/// Logical font size and inner padding, scaled by the output scale.
const FONT_PX: f32 = 14.0;
const PAD: u32 = 6;
/// Lines of scrollback the primary screen keeps.
const SCROLLBACK: usize = 10_000;
/// Entries in rio-vt's `TermColors`: 256 indexed plus the named colors.
const TERM_COLORS: usize = 269;
/// Linux input codes for the three mouse buttons.
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

type Rgb = (u8, u8, u8);

const FG: Rgb = (0xea, 0xea, 0xea);
const BG: Rgb = (0x18, 0x18, 0x1b);
const BORDER: Rgb = (0x44, 0x44, 0x4c);
const ANSI: [Rgb; 16] = [
    (0x1d, 0x1f, 0x21),
    (0xcc, 0x66, 0x66),
    (0xb5, 0xbd, 0x68),
    (0xf0, 0xc6, 0x74),
    (0x81, 0xa2, 0xbe),
    (0xb2, 0x94, 0xbb),
    (0x8a, 0xbe, 0xb7),
    (0xc5, 0xc8, 0xc6),
    (0x66, 0x66, 0x66),
    (0xd5, 0x4e, 0x53),
    (0xb9, 0xca, 0x4a),
    (0xe7, 0xc5, 0x47),
    (0x7a, 0xa6, 0xda),
    (0xc3, 0x97, 0xd8),
    (0x70, 0xc0, 0xb1),
    (0xea, 0xea, 0xea),
];

/// Keyboard modifiers that change what a key sends.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Mods {
    ctrl: bool,
    alt: bool,
    shift: bool,
}

// ---------------------------------------------------------------- pure helpers

/// Columns and rows of `cell_w × cell_h` cells that fit a `w × h` area, never
/// fewer than 20 × 5 so a tiny panel still gives the program a usable grid.
fn grid_size(w: u32, h: u32, cell_w: u32, cell_h: u32) -> (u16, u16) {
    if cell_w == 0 || cell_h == 0 {
        return (20, 5);
    }
    let cols = (w / cell_w).clamp(20, u32::from(u16::MAX));
    let rows = (h / cell_h).clamp(5, u32::from(u16::MAX));
    (cols as u16, rows as u16)
}

/// The panel's logical size: 60% × 60% of the output, or 800 × 500 when the
/// output size is unknown.
fn popup_size((w, h): (i32, i32)) -> (u32, u32) {
    if w <= 0 || h <= 0 {
        return (800, 500);
    }
    (w as u32 * 3 / 5, h as u32 * 3 / 5)
}

/// Ctrl+Shift+Q closes the panel; everything else goes to the program.
fn is_close_chord(keysym: Keysym, mods: Mods) -> bool {
    mods.ctrl && mods.shift && !mods.alt && (keysym == Keysym::q || keysym == Keysym::Q)
}

/// The bytes an xterm-compatible terminal sends for a key press, or `None`
/// for a key that sends nothing (a bare modifier).
fn encode_key(keysym: Keysym, mods: Mods, text: Option<&str>, app_cursor: bool) -> Option<Vec<u8>> {
    let m = 1 + u8::from(mods.shift) + 2 * u8::from(mods.alt) + 4 * u8::from(mods.ctrl);
    let cursor = |c: char| {
        if m > 1 {
            format!("\x1b[1;{m}{c}")
        } else if app_cursor {
            format!("\x1bO{c}")
        } else {
            format!("\x1b[{c}")
        }
    };
    let ss3 = |c: char| {
        if m > 1 {
            format!("\x1b[1;{m}{c}")
        } else {
            format!("\x1bO{c}")
        }
    };
    let tilde = |n: u8| {
        if m > 1 {
            format!("\x1b[{n};{m}~")
        } else {
            format!("\x1b[{n}~")
        }
    };
    let seq = match keysym {
        Keysym::Up | Keysym::KP_Up => cursor('A'),
        Keysym::Down | Keysym::KP_Down => cursor('B'),
        Keysym::Right | Keysym::KP_Right => cursor('C'),
        Keysym::Left | Keysym::KP_Left => cursor('D'),
        Keysym::Home | Keysym::KP_Home => cursor('H'),
        Keysym::End | Keysym::KP_End => cursor('F'),
        Keysym::Insert => tilde(2),
        Keysym::Delete | Keysym::KP_Delete => tilde(3),
        Keysym::Page_Up | Keysym::KP_Page_Up => tilde(5),
        Keysym::Page_Down | Keysym::KP_Page_Down => tilde(6),
        Keysym::F1 => ss3('P'),
        Keysym::F2 => ss3('Q'),
        Keysym::F3 => ss3('R'),
        Keysym::F4 => ss3('S'),
        Keysym::F5 => tilde(15),
        Keysym::F6 => tilde(17),
        Keysym::F7 => tilde(18),
        Keysym::F8 => tilde(19),
        Keysym::F9 => tilde(20),
        Keysym::F10 => tilde(21),
        Keysym::F11 => tilde(23),
        Keysym::F12 => tilde(24),
        Keysym::ISO_Left_Tab => "\x1b[Z".to_owned(),
        Keysym::Tab if mods.shift => "\x1b[Z".to_owned(),
        _ => return encode_text(keysym, mods, text),
    };
    Some(seq.into_bytes())
}

/// Keys that send a character: editing keys, Ctrl combinations as C0 codes,
/// and the layout's text, each with an ESC prefix when Alt is held.
fn encode_text(keysym: Keysym, mods: Mods, text: Option<&str>) -> Option<Vec<u8>> {
    let mut out = match keysym {
        Keysym::Return | Keysym::KP_Enter => vec![b'\r'],
        Keysym::BackSpace => vec![if mods.ctrl { 0x08 } else { 0x7f }],
        Keysym::Tab => vec![b'\t'],
        Keysym::Escape => vec![0x1b],
        _ => {
            let control = mods
                .ctrl
                .then(|| keysym.key_char().and_then(ctrl_byte))
                .flatten();
            match control {
                Some(byte) => vec![byte],
                None => text.filter(|t| !t.is_empty())?.as_bytes().to_vec(),
            }
        }
    };
    if mods.alt {
        out.insert(0, 0x1b);
    }
    Some(out)
}

/// The C0 control code Ctrl+`c` sends, as xterm maps it.
fn ctrl_byte(c: char) -> Option<u8> {
    Some(match c {
        'a'..='z' => c as u8 - b'a' + 1,
        'A'..='Z' => c as u8 - b'A' + 1,
        '@' | ' ' | '2' => 0,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '/' | '7' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    })
}

/// An SGR (mode 1006) mouse report; `col`/`row` are zero-based cells.
fn encode_mouse_sgr(button: u8, col: u16, row: u16, pressed: bool) -> Vec<u8> {
    let end = if pressed { 'M' } else { 'm' };
    format!("\x1b[<{button};{};{}{end}", col + 1, row + 1).into_bytes()
}

// ---------------------------------------------------------------- colors

fn palette(i: u8) -> Rgb {
    match i {
        0..=15 => ANSI[usize::from(i)],
        16..=231 => {
            let n = i - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + 40 * v };
            (level(n / 36), level((n / 6) % 6), level(n % 6))
        }
        _ => {
            let v = 8 + 10 * (i - 232);
            (v, v, v)
        }
    }
}

fn dim((r, g, b): Rgb) -> Rgb {
    let f = |c: u8| (u16::from(c) * 2 / 3) as u8;
    (f(r), f(g), f(b))
}

fn from_arr(a: ColorArray) -> Rgb {
    let f = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
    (f(a[0]), f(a[1]), f(a[2]))
}

/// Color `i` of rio-vt's color table (0–255 indexed, then the named colors),
/// honoring colors the program set with OSC 4/10/11.
fn index_rgb(i: usize, overrides: &TermColors) -> Rgb {
    if i < TERM_COLORS
        && let Some(set) = overrides[i]
    {
        return from_arr(set);
    }
    match i {
        0..=255 => palette(i as u8),
        257 => BG,
        259..=266 => dim(ANSI[i - 259]),
        268 => dim(FG),
        _ => FG,
    }
}

fn resolve(color: AnsiColor, overrides: &TermColors) -> Rgb {
    match color {
        AnsiColor::Spec(c) => (c.r, c.g, c.b),
        AnsiColor::Indexed(i) => index_rgb(usize::from(i), overrides),
        AnsiColor::Named(n) => index_rgb(n as usize, overrides),
    }
}

/// Foreground and background of a cell after bold-as-bright, dim and inverse.
fn cell_colors(style: &Style, overrides: &TermColors) -> (Rgb, Rgb) {
    let mut fg = style.fg;
    if style.flags.contains(StyleFlags::BOLD) {
        fg = match fg {
            AnsiColor::Named(n) if (n as usize) < 8 => AnsiColor::Indexed(n as u8 + 8),
            AnsiColor::Indexed(i) if i < 8 => AnsiColor::Indexed(i + 8),
            other => other,
        };
    }
    let mut fg = resolve(fg, overrides);
    let mut bg = resolve(style.bg, overrides);
    if style.flags.contains(StyleFlags::DIM) {
        fg = dim(fg);
    }
    if style.flags.contains(StyleFlags::INVERSE) {
        std::mem::swap(&mut fg, &mut bg);
    }
    (fg, bg)
}

// ---------------------------------------------------------------- painting

/// The panel's pixels, opaque ARGB8888 as `u32`s.
struct Frame {
    w: u32,
    h: u32,
    px: Vec<u32>,
}

fn pack((r, g, b): Rgb) -> u32 {
    0xff00_0000 | (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
}

fn mix(dst: u32, (r, g, b): Rgb, a: u8) -> u32 {
    let a = u32::from(a);
    let ch = |src: u8, shift: u32| (u32::from(src) * a + ((dst >> shift) & 0xff) * (255 - a)) / 255;
    0xff00_0000 | (ch(r, 16) << 16) | (ch(g, 8) << 8) | ch(b, 0)
}

impl Frame {
    fn new(w: u32, h: u32) -> Self {
        Self {
            w,
            h,
            px: vec![pack(BG); (w * h) as usize],
        }
    }

    fn fill(&mut self, x: u32, y: u32, w: u32, h: u32, color: Rgb) {
        let x1 = (x + w).min(self.w);
        let y1 = (y + h).min(self.h);
        let x0 = x.min(x1);
        let value = pack(color);
        for row in y.min(y1)..y1 {
            let start = (row * self.w) as usize;
            self.px[start + x0 as usize..start + x1 as usize].fill(value);
        }
    }

    fn blend(&mut self, x: i32, y: i32, color: Rgb, a: u8) {
        if x < 0 || y < 0 || x as u32 >= self.w || y as u32 >= self.h || a == 0 {
            return;
        }
        let i = (y as u32 * self.w + x as u32) as usize;
        self.px[i] = if a == 255 {
            pack(color)
        } else {
            mix(self.px[i], color, a)
        };
    }
}

/// One rasterized glyph, offset from its cell's top-left corner.
struct Raster {
    left: i32,
    top: i32,
    w: u32,
    h: u32,
    /// RGBA pixels (emoji) rather than an alpha mask.
    color: bool,
    data: Vec<u8>,
}

/// Monospace font, cell metrics and a per-character glyph cache.
struct Font {
    system: FontSystem,
    swash: SwashCache,
    metrics: Metrics,
    cell_w: u32,
    cell_h: u32,
    glyphs: HashMap<(char, bool), Option<Raster>>,
}

fn mono(bold: bool) -> Attrs<'static> {
    Attrs::new()
        .family(Family::Monospace)
        .weight(if bold { Weight::BOLD } else { Weight::NORMAL })
}

impl Font {
    fn new() -> Self {
        let mut font = Self {
            system: FontSystem::new(),
            swash: SwashCache::new(),
            metrics: Metrics::new(FONT_PX, FONT_PX),
            cell_w: 1,
            cell_h: 1,
            glyphs: HashMap::new(),
        };
        font.set_size(FONT_PX);
        font
    }

    fn shape(&mut self, text: &str, bold: bool) -> Buffer {
        let mut buffer = Buffer::new(&mut self.system, self.metrics);
        buffer.set_size(None, None);
        buffer.set_text(text, &mono(bold), Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut self.system, false);
        buffer
    }

    /// Set the physical font size and measure one cell.
    fn set_size(&mut self, px: f32) {
        self.metrics = Metrics::new(px, (px * 1.25).ceil());
        self.glyphs.clear();
        let buffer = self.shape("M", false);
        let width = buffer
            .layout_runs()
            .next()
            .map_or(px * 0.6, |run| run.line_w);
        self.cell_w = (width.round() as u32).max(1);
        self.cell_h = self.metrics.line_height as u32;
    }

    fn glyph(&mut self, c: char, bold: bool) -> Option<&Raster> {
        if !self.glyphs.contains_key(&(c, bold)) {
            let raster = self.rasterize(c, bold);
            self.glyphs.insert((c, bold), raster);
        }
        self.glyphs.get(&(c, bold)).and_then(Option::as_ref)
    }

    fn rasterize(&mut self, c: char, bold: bool) -> Option<Raster> {
        let buffer = self.shape(c.encode_utf8(&mut [0; 4]), bold);
        let run = buffer.layout_runs().next()?;
        let glyph = run.glyphs.first()?;
        let physical = glyph.physical((0.0, 0.0), 1.0);
        let image = self
            .swash
            .get_image_uncached(&mut self.system, physical.cache_key)?;
        let (color, data) = match image.content {
            SwashContent::Mask => (false, image.data),
            SwashContent::Color => (true, image.data),
            SwashContent::SubpixelMask => (
                false,
                image
                    .data
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|p| ((u16::from(p[0]) + u16::from(p[1]) + u16::from(p[2])) / 3) as u8)
                    .collect(),
            ),
        };
        Some(Raster {
            left: physical.x + image.placement.left,
            top: run.line_y.round() as i32 + physical.y - image.placement.top,
            w: image.placement.width,
            h: image.placement.height,
            color,
            data,
        })
    }
}

fn blit(frame: &mut Frame, glyph: &Raster, x: i32, y: i32, fg: Rgb, clip: (i32, i32)) {
    for gy in 0..glyph.h {
        let py = y + glyph.top + gy as i32;
        if py < clip.0 || py >= clip.1 {
            continue;
        }
        for gx in 0..glyph.w {
            let px = x + glyph.left + gx as i32;
            let i = (gy * glyph.w + gx) as usize;
            if glyph.color {
                let p = &glyph.data[i * 4..i * 4 + 4];
                frame.blend(px, py, (p[0], p[1], p[2]), p[3]);
            } else {
                frame.blend(px, py, fg, glyph.data[i]);
            }
        }
    }
}

/// Panel geometry in physical pixels and cells.
#[derive(Debug, Clone, Copy, Default)]
struct Geom {
    scale: u32,
    pad: u32,
    cell_w: u32,
    cell_h: u32,
    cols: u16,
    rows: u16,
}

/// Paint visible row `y` from its cells; `cursor` is the cursor's column and
/// shape when it sits on this row.
fn paint_row(
    font: &mut Font,
    frame: &mut Frame,
    g: Geom,
    y: usize,
    cells: &[(Square, Style)],
    colors: &TermColors,
    cursor: Option<(usize, CursorShape)>,
) {
    let top = g.pad + y as u32 * g.cell_h;
    let default_bg = index_rgb(257, colors);
    let cursor_color = index_rgb(258, colors);
    frame.fill(g.pad, top, frame.w - 2 * g.pad, g.cell_h, default_bg);
    let clip = (top as i32, (top + g.cell_h) as i32);
    for (x, (square, style)) in cells.iter().enumerate() {
        if square.wide() == Wide::Spacer {
            continue;
        }
        let span = if square.wide() == Wide::Wide { 2 } else { 1 };
        let left = g.pad + x as u32 * g.cell_w;
        let (mut fg, mut bg) = cell_colors(style, colors);
        if cursor == Some((x, CursorShape::Block)) {
            fg = bg;
            bg = cursor_color;
        }
        frame.fill(left, top, span * g.cell_w, g.cell_h, bg);
        if square.content_tag() == ContentTag::Codepoint
            && !style.flags.contains(StyleFlags::HIDDEN)
        {
            let c = square.c();
            let bold = style.flags.contains(StyleFlags::BOLD);
            if c > ' '
                && let Some(glyph) = font.glyph(c, bold)
            {
                blit(frame, glyph, left as i32, top as i32, fg, clip);
            }
        }
        if style.flags.intersects(StyleFlags::ALL_UNDERLINES) {
            frame.fill(left, top + g.cell_h - g.scale, span * g.cell_w, g.scale, fg);
        }
        if style.flags.contains(StyleFlags::STRIKEOUT) {
            frame.fill(left, top + g.cell_h / 2, span * g.cell_w, g.scale, fg);
        }
    }
    if let Some((x, shape)) = cursor {
        let left = g.pad + x as u32 * g.cell_w;
        match shape {
            CursorShape::Beam => frame.fill(left, top, 2 * g.scale, g.cell_h, cursor_color),
            CursorShape::Underline => {
                frame.fill(
                    left,
                    top + g.cell_h - 2 * g.scale,
                    g.cell_w,
                    2 * g.scale,
                    cursor_color,
                );
            }
            CursorShape::Block | CursorShape::Hidden => {}
        }
    }
}

// ---------------------------------------------------------------- terminal

/// What the PTY thread tells the Wayland thread.
enum Ev {
    Damaged,
    Write(String),
    Color(usize, Arc<dyn Fn(ColorRgb) -> String + Sync + Send>),
    Size(Arc<dyn Fn(WindowSize) -> String + Sync + Send>),
    Exited(Option<i32>),
    Close,
}

/// rio-vt's event sink: forwards the events this spike acts on to calloop.
#[derive(Clone)]
struct Proxy(calloop::channel::Sender<Ev>);

impl EventListener for Proxy {
    fn send_event(&self, event: RioEvent, _id: WindowId) {
        let ev = match event {
            RioEvent::TerminalDamaged(_) | RioEvent::Render | RioEvent::RenderRoute(_) => {
                Ev::Damaged
            }
            RioEvent::PtyWrite(_, text) => Ev::Write(text),
            RioEvent::ColorRequest(_, index, format) => Ev::Color(index, format),
            RioEvent::TextAreaSizeRequest(_, format) => Ev::Size(format),
            RioEvent::ChildExited(_, status) => Ev::Exited(status),
            RioEvent::CloseTerminal(_) => Ev::Close,
            _ => return,
        };
        let _ = self.0.send(ev);
    }
}

/// Reads the PTY through a duplicate of its descriptor, counting bytes.
struct CountingReader {
    file: File,
    bytes: Arc<AtomicU64>,
}

impl Read for CountingReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.file.read(buf)?;
        self.bytes.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

/// teletypewriter's PTY with a byte-counting reader, so the spike can report
/// the program's output rate.
struct CountingPty {
    pty: Pty,
    reader: CountingReader,
}

impl CountingPty {
    fn new(mut pty: Pty, bytes: Arc<AtomicU64>) -> io::Result<Self> {
        let file = ProcessReadWrite::reader(&mut pty).try_clone()?;
        Ok(Self {
            pty,
            reader: CountingReader { file, bytes },
        })
    }
}

impl ProcessReadWrite for CountingPty {
    type Reader = CountingReader;
    type Writer = <Pty as ProcessReadWrite>::Writer;

    fn reader(&mut self) -> &mut CountingReader {
        &mut self.reader
    }

    fn read_token(&self) -> corcovado::Token {
        ProcessReadWrite::read_token(&self.pty)
    }

    fn writer(&mut self) -> &mut Self::Writer {
        ProcessReadWrite::writer(&mut self.pty)
    }

    fn write_token(&self) -> corcovado::Token {
        ProcessReadWrite::write_token(&self.pty)
    }

    fn set_winsize(&mut self, size: WinsizeBuilder) -> io::Result<()> {
        ProcessReadWrite::set_winsize(&mut self.pty, size)
    }

    fn register(
        &mut self,
        poll: &corcovado::Poll,
        tokens: &mut dyn Iterator<Item = corcovado::Token>,
        interest: corcovado::Ready,
        opts: corcovado::PollOpt,
    ) -> io::Result<()> {
        ProcessReadWrite::register(&mut self.pty, poll, tokens, interest, opts)
    }

    fn reregister(
        &mut self,
        poll: &corcovado::Poll,
        interest: corcovado::Ready,
        opts: corcovado::PollOpt,
    ) -> io::Result<()> {
        ProcessReadWrite::reregister(&mut self.pty, poll, interest, opts)
    }

    fn deregister(&mut self, poll: &corcovado::Poll) -> io::Result<()> {
        ProcessReadWrite::deregister(&mut self.pty, poll)
    }
}

impl EventedPty for CountingPty {
    fn shutdown(&mut self) -> io::Result<()> {
        EventedPty::shutdown(&mut self.pty)
    }

    fn child_event_token(&self) -> corcovado::Token {
        self.pty.child_event_token()
    }

    fn next_child_event(&mut self) -> Option<ChildEvent> {
        self.pty.next_child_event()
    }
}

type Term = Arc<FairMutex<Crosswords<Proxy>>>;

/// The rows to repaint, copied out under the terminal lock.
struct Snapshot {
    full: bool,
    lines: Vec<(usize, Vec<(Square, Style)>)>,
    colors: TermColors,
    cursor: Option<(usize, usize, CursorShape)>,
}

fn take_snapshot(
    term: &mut Crosswords<Proxy>,
    rows: usize,
    cols: usize,
    force_full: bool,
    prev_cursor: Option<usize>,
) -> Snapshot {
    term.damage_event_in_flight = false;
    let rows = rows.min(term.screen_lines());
    let offset = term.display_offset();
    let full = force_full || offset != 0;
    let mut lines: Vec<usize> = match term.damage() {
        TermDamage::Full => (0..rows).collect(),
        TermDamage::Partial(damaged) => damaged.map(|d| d.line).collect(),
    };
    if full {
        lines = (0..rows).collect();
    }
    let state = term.cursor();
    let cursor_row = state.pos.row.0 + offset as i32;
    let cursor = (state.content != CursorShape::Hidden && (0..rows as i32).contains(&cursor_row))
        .then_some((cursor_row as usize, state.pos.col.0, state.content));
    lines.extend(prev_cursor);
    lines.extend(cursor.map(|c| c.0));
    lines.retain(|&l| l < rows);
    lines.sort_unstable();
    lines.dedup();
    let lines = lines
        .into_iter()
        .map(|l| {
            let row = &term.grid[Line(l as i32 - offset as i32)];
            let cells = row
                .inner
                .iter()
                .take(cols)
                .map(|sq| (*sq, term.grid.style_of(sq)))
                .collect();
            (l, cells)
        })
        .collect();
    let colors = *term.colors();
    term.reset_damage();
    Snapshot {
        full,
        lines,
        colors,
        cursor,
    }
}

// ---------------------------------------------------------------- app

#[derive(Default)]
struct Stats {
    font_load: Duration,
    spawned: Option<Instant>,
    first_frame: Option<Duration>,
    first_content: Option<Duration>,
    frame_times: Vec<Duration>,
    changed_lines: Vec<usize>,
    bytes: Arc<AtomicU64>,
}

struct App {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    qh: QueueHandle<App>,
    loop_handle: LoopHandle<'static, App>,
    events: calloop::channel::Sender<Ev>,
    start: Instant,

    layer: Option<LayerSurface>,
    pool: Option<SlotPool>,
    buffer: Option<ShmBuffer>,
    keyboard: Option<WlKeyboard>,
    pointer: Option<WlPointer>,

    font: Font,
    frame: Frame,
    geom: Geom,
    logical: (u32, u32),
    configured: bool,
    frame_pending: bool,
    dirty: bool,
    force_full: bool,
    prev_cursor_row: Option<usize>,

    mods: Mods,
    held: Option<u8>,
    last_cell: (u16, u16),
    scroll_acc: f64,

    command: Vec<String>,
    term: Option<Term>,
    pty_tx: Option<corcovado::channel::Sender<Msg>>,
    machine: Option<JoinHandle<(Machine<CountingPty, Proxy>, MachineState)>>,
    child_alive: bool,

    stats: Stats,
    exit: bool,
}

impl App {
    fn window_size(&self) -> WindowSize {
        let g = self.geom;
        WindowSize {
            rows: g.rows,
            cols: g.cols,
            width: (u32::from(g.cols) * g.cell_w).min(u32::from(u16::MAX)) as u16,
            height: (u32::from(g.rows) * g.cell_h).min(u32::from(u16::MAX)) as u16,
        }
    }

    fn crosswords_size(&self) -> CrosswordsSize {
        let ws = self.window_size();
        CrosswordsSize {
            columns: usize::from(ws.cols),
            screen_lines: usize::from(ws.rows),
            width: u32::from(ws.width),
            height: u32::from(ws.height),
            square_width: self.geom.cell_w,
            square_height: self.geom.cell_h,
        }
    }

    /// Recompute the frame and grid for the current logical size.
    fn apply_size(&mut self) {
        let scale = self.geom.scale;
        let (w, h) = (self.logical.0 * scale, self.logical.1 * scale);
        let pad = PAD * scale;
        self.frame = Frame::new(w, h);
        let (cols, rows) = grid_size(
            w.saturating_sub(2 * pad),
            h.saturating_sub(2 * pad),
            self.font.cell_w,
            self.font.cell_h,
        );
        self.geom = Geom {
            scale,
            pad,
            cell_w: self.font.cell_w,
            cell_h: self.font.cell_h,
            cols,
            rows,
        };
        if let Some(term) = &self.term {
            term.lock().resize(self.crosswords_size());
        }
        if let Some(tx) = &self.pty_tx {
            let _ = tx.send(Msg::Resize(self.window_size()));
        }
        log::info!("panel {w}x{h}px, grid {cols}x{rows}");
    }

    /// Start the program on a PTY sized to the grid, with rio-vt's reader thread.
    fn spawn(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let (program, args) = self.command.split_first().ok_or("no program given")?;
        let proxy = Proxy(self.events.clone());
        let term = Arc::new(FairMutex::new(Crosswords::new(
            self.crosswords_size(),
            CursorShape::Block,
            proxy.clone(),
            WindowId::from(0),
            0,
            SCROLLBACK,
        )));
        let env = vec![
            ("TERM".to_owned(), "xterm-256color".to_owned()),
            ("COLORTERM".to_owned(), "truecolor".to_owned()),
        ];
        let ws = self.window_size();
        let pty = teletypewriter::create_pty_with_spawn(
            Some(program.as_str()),
            args.to_vec(),
            &None,
            Some(env),
            ws.cols,
            ws.rows,
            ws.width,
            ws.height,
        )?;
        let pty = CountingPty::new(pty, self.stats.bytes.clone())?;
        let machine = Machine::new(term.clone(), pty, proxy, WindowId::from(0), 0)?;
        self.pty_tx = Some(machine.channel());
        self.machine = Some(machine.spawn());
        self.term = Some(term);
        self.child_alive = true;
        self.stats.spawned = Some(Instant::now());
        log::info!("spawned {:?}", self.command);
        Ok(())
    }

    fn send(&self, bytes: Vec<u8>) {
        if let Some(tx) = &self.pty_tx {
            let _ = tx.send(Msg::Input(Cow::Owned(bytes)));
        }
    }

    fn mode(&self) -> Mode {
        self.term
            .as_ref()
            .map_or(Mode::empty(), |term| term.lock().mode())
    }

    fn on_event(&mut self, ev: Ev) {
        match ev {
            Ev::Damaged => self.draw(),
            Ev::Write(text) => self.send(text.into_bytes()),
            Ev::Color(index, format) => {
                let colors = self.term.as_ref().map(|t| *t.lock().colors());
                let (r, g, b) = colors.map_or(FG, |c| index_rgb(index, &c));
                self.send(format(ColorRgb { r, g, b }).into_bytes());
            }
            Ev::Size(format) => self.send(format(self.window_size()).into_bytes()),
            Ev::Exited(status) => {
                log::info!("program exited: {status:?}");
                self.child_alive = false;
                self.exit = true;
            }
            Ev::Close => {
                self.child_alive = false;
                self.exit = true;
            }
        }
    }

    fn handle_key(&mut self, event: KeyEvent) {
        if is_close_chord(event.keysym, self.mods) {
            log::info!("closed with Ctrl+Shift+Q");
            self.exit = true;
            return;
        }
        let Some(term) = self.term.clone() else {
            return;
        };
        let app_cursor = {
            let mut t = term.lock();
            if t.display_offset() != 0 {
                t.scroll_display(Scroll::Bottom);
                self.force_full = true;
            }
            t.mode().contains(Mode::APP_CURSOR)
        };
        if let Some(bytes) = encode_key(event.keysym, self.mods, event.utf8.as_deref(), app_cursor)
        {
            self.send(bytes);
        }
        if self.force_full {
            self.draw();
        }
    }

    fn cell_at(&self, (x, y): (f64, f64)) -> (u16, u16) {
        let g = self.geom;
        let to_cell = |pos: f64, cell: u32, max: u16| {
            let px = pos * f64::from(g.scale) - f64::from(g.pad);
            ((px / f64::from(cell)).max(0.0) as u16).min(max.saturating_sub(1))
        };
        (to_cell(x, g.cell_w, g.cols), to_cell(y, g.cell_h, g.rows))
    }

    fn handle_pointer(&mut self, event: &PointerEvent) {
        let mode = self.mode();
        let reporting = mode.intersects(Mode::MOUSE_MODE) && mode.contains(Mode::SGR_MOUSE);
        let (col, row) = self.cell_at(event.position);
        match event.kind {
            PointerEventKind::Press { button, .. } | PointerEventKind::Release { button, .. } => {
                let pressed = matches!(event.kind, PointerEventKind::Press { .. });
                let code = match button {
                    BTN_LEFT => 0,
                    BTN_MIDDLE => 1,
                    BTN_RIGHT => 2,
                    _ => return,
                };
                self.held = pressed.then_some(code);
                if reporting {
                    self.send(encode_mouse_sgr(code, col, row, pressed));
                }
            }
            PointerEventKind::Motion { .. } => {
                let moved = (col, row) != self.last_cell;
                self.last_cell = (col, row);
                let wants = mode.contains(Mode::MOUSE_MOTION)
                    || (mode.contains(Mode::MOUSE_DRAG) && self.held.is_some());
                if reporting && wants && moved {
                    self.send(encode_mouse_sgr(
                        self.held.unwrap_or(3) + 32,
                        col,
                        row,
                        true,
                    ));
                }
            }
            PointerEventKind::Axis { vertical, .. } => {
                if vertical.discrete != 0 {
                    self.scroll_acc += f64::from(vertical.discrete);
                } else {
                    self.scroll_acc += vertical.absolute / 15.0;
                }
                let steps = self.scroll_acc.trunc() as i32;
                self.scroll_acc -= f64::from(steps);
                if steps == 0 {
                    return;
                }
                if reporting {
                    let code = if steps < 0 { 64 } else { 65 };
                    for _ in 0..steps.unsigned_abs() {
                        self.send(encode_mouse_sgr(code, col, row, true));
                    }
                } else if mode.contains(Mode::ALT_SCREEN) {
                    let key = if steps < 0 { Keysym::Up } else { Keysym::Down };
                    let app_cursor = mode.contains(Mode::APP_CURSOR);
                    for _ in 0..steps.unsigned_abs() {
                        if let Some(bytes) = encode_key(key, Mods::default(), None, app_cursor) {
                            self.send(bytes);
                        }
                    }
                } else if let Some(term) = &self.term {
                    term.lock().scroll_display(Scroll::Delta(-steps * 3));
                    self.force_full = true;
                    self.draw();
                }
            }
            _ => {}
        }
    }

    /// Repaint what changed and commit it, or defer until the next frame
    /// callback when one is pending.
    fn draw(&mut self) {
        if !self.configured || self.exit {
            return;
        }
        if self.frame_pending {
            self.dirty = true;
            return;
        }
        self.dirty = false;
        let started = Instant::now();
        let force_full = std::mem::take(&mut self.force_full);
        let Some(term) = self.term.clone() else {
            return;
        };
        let snap = take_snapshot(
            &mut term.lock(),
            usize::from(self.geom.rows),
            usize::from(self.geom.cols),
            force_full,
            self.prev_cursor_row,
        );
        if !snap.full && snap.lines.is_empty() {
            return;
        }
        let g = self.geom;
        if snap.full {
            let (w, h) = (self.frame.w, self.frame.h);
            self.frame.fill(0, 0, w, h, BORDER);
            self.frame.fill(
                g.scale,
                g.scale,
                w - 2 * g.scale,
                h - 2 * g.scale,
                index_rgb(257, &snap.colors),
            );
        }
        for (y, cells) in &snap.lines {
            let cursor = snap.cursor.filter(|c| c.0 == *y).map(|c| (c.1, c.2));
            paint_row(
                &mut self.font,
                &mut self.frame,
                g,
                *y,
                cells,
                &snap.colors,
                cursor,
            );
        }
        self.prev_cursor_row = snap.cursor.map(|c| c.0);
        let damage = match (snap.full, snap.lines.first(), snap.lines.last()) {
            (false, Some((first, _)), Some((last, _))) => (
                0,
                (g.pad + *first as u32 * g.cell_h) as i32,
                self.frame.w as i32,
                ((*last - *first + 1) as u32 * g.cell_h) as i32,
            ),
            _ => (0, 0, self.frame.w as i32, self.frame.h as i32),
        };
        if self.commit(damage) {
            let elapsed = self.start.elapsed();
            self.stats.first_frame.get_or_insert(elapsed);
            if self.stats.bytes.load(Ordering::Relaxed) > 0 {
                self.stats.first_content.get_or_insert(elapsed);
            }
            self.stats.frame_times.push(started.elapsed());
            self.stats.changed_lines.push(snap.lines.len());
        }
    }

    fn commit(&mut self, (x, y, w, h): (i32, i32, i32, i32)) -> bool {
        let (Some(layer), Some(pool)) = (&self.layer, &mut self.pool) else {
            return false;
        };
        let (fw, fh) = (self.frame.w, self.frame.h);
        let (buffer, canvas) = match pool.create_buffer(
            fw as i32,
            fh as i32,
            (fw * 4) as i32,
            wl_shm::Format::Argb8888,
        ) {
            Ok(parts) => parts,
            Err(e) => {
                log::error!("failed to create shm buffer: {e}");
                return false;
            }
        };
        for (dst, px) in canvas.as_chunks_mut::<4>().0.iter_mut().zip(&self.frame.px) {
            *dst = px.to_le_bytes();
        }
        let surface = layer.wl_surface();
        surface.set_buffer_scale(self.geom.scale as i32);
        surface.damage_buffer(x, y, w, h);
        if let Err(e) = buffer.attach_to(surface) {
            log::error!("failed to attach buffer: {e}");
            return false;
        }
        surface.frame(&self.qh, surface.clone());
        surface.commit();
        self.buffer = Some(buffer);
        self.frame_pending = true;
        true
    }

    /// Stop the program if it still runs, and wait for rio-vt's reader thread.
    fn shutdown(&mut self) {
        if let Some(tx) = self.pty_tx.take()
            && self.child_alive
        {
            let _ = tx.send(Msg::Shutdown);
        }
        if let Some(handle) = self.machine.take() {
            let _ = handle.join();
        }
    }

    fn report(&mut self) {
        let s = &mut self.stats;
        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        log::info!("font load: {:.1}ms", ms(s.font_load));
        if let Some(d) = s.first_frame {
            log::info!("time to first frame: {:.1}ms", ms(d));
        }
        if let Some(d) = s.first_content {
            log::info!("time to first frame with program output: {:.1}ms", ms(d));
        }
        if !s.frame_times.is_empty() {
            s.frame_times.sort_unstable();
            let n = s.frame_times.len();
            let pct = |p: usize| s.frame_times[(n * p / 100).min(n - 1)];
            log::info!(
                "frames: {n}, paint p50={:.2}ms p95={:.2}ms max={:.2}ms",
                ms(pct(50)),
                ms(pct(95)),
                ms(s.frame_times[n - 1]),
            );
            let lines: usize = s.changed_lines.iter().sum();
            log::info!(
                "changed lines per frame: mean {:.1} of {}",
                lines as f64 / n as f64,
                self.geom.rows
            );
        }
        let bytes = s.bytes.load(Ordering::Relaxed);
        let secs = s.spawned.map_or(0.0, |t| t.elapsed().as_secs_f64());
        if secs > 0.0 {
            log::info!(
                "program output: {bytes} bytes in {secs:.1}s ({:.0} B/s)",
                bytes as f64 / secs
            );
        }
    }
}

// ---------------------------------------------------------------- wayland glue

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlSurface, _: u32) {
        self.frame_pending = false;
        if self.dirty {
            self.draw();
        }
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: &WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: &WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let (w, h) = configure.new_size;
        let size = if w == 0 || h == 0 {
            self.logical
        } else {
            (w, h)
        };
        if self.configured && size == self.logical {
            return;
        }
        self.logical = size;
        self.apply_size();
        if !self.configured {
            self.configured = true;
            if let Err(e) = self.spawn() {
                log::error!("failed to start {:?}: {e}", self.command);
                self.exit = true;
                return;
            }
        }
        self.force_full = true;
        self.draw();
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            let keyboard = self.seat_state.get_keyboard_with_repeat(
                qh,
                &seat,
                None,
                self.loop_handle.clone(),
                Box::new(|app: &mut App, _: &WlKeyboard, event: KeyEvent| app.handle_key(event)),
            );
            match keyboard {
                Ok(keyboard) => self.keyboard = Some(keyboard),
                Err(e) => log::error!("no keyboard: {e}"),
            }
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            match self.seat_state.get_pointer(qh, &seat) {
                Ok(pointer) => self.pointer = Some(pointer),
                Err(e) => log::error!("no pointer: {e}"),
            }
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard
            && let Some(keyboard) = self.keyboard.take()
        {
            keyboard.release();
        }
        if capability == Capability::Pointer
            && let Some(pointer) = self.pointer.take()
        {
            pointer.release();
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}
}

impl KeyboardHandler for App {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: &WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: &WlSurface,
        _: u32,
    ) {
    }

    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        self.handle_key(event);
    }

    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        self.handle_key(event);
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: u32,
        modifiers: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
        self.mods = Mods {
            ctrl: modifiers.ctrl,
            alt: modifiers.alt,
            shift: modifiers.shift,
        };
    }
}

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            let ours = self
                .layer
                .as_ref()
                .is_some_and(|layer| layer.wl_surface() == &event.surface);
            if ours {
                self.handle_pointer(event);
            }
        }
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
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
delegate_seat!(App);
delegate_keyboard!(App);
delegate_pointer!(App);
delegate_registry!(App);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let start = Instant::now();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let mut command: Vec<String> = std::env::args().skip(1).collect();
    if command.is_empty() {
        command.push("btm".to_owned());
    }

    let conn = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<App>(&conn)?;
    let qh = queue.handle();
    let compositor = CompositorState::bind(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh)?;
    let shm = Shm::bind(&globals, &qh)?;

    let mut event_loop: EventLoop<'static, App> = EventLoop::try_new()?;
    let (events, receiver) = calloop::channel::channel::<Ev>();
    event_loop
        .handle()
        .insert_source(receiver, |event, _, app| {
            if let calloop::channel::Event::Msg(ev) = event {
                app.on_event(ev);
            }
        })
        .map_err(|e| e.error)?;

    let font_started = Instant::now();
    let font = Font::new();
    let font_load = font_started.elapsed();

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        qh: qh.clone(),
        loop_handle: event_loop.handle(),
        events,
        start,
        layer: None,
        pool: None,
        buffer: None,
        keyboard: None,
        pointer: None,
        font,
        frame: Frame::new(1, 1),
        geom: Geom::default(),
        logical: (0, 0),
        configured: false,
        frame_pending: false,
        dirty: false,
        force_full: false,
        prev_cursor_row: None,
        mods: Mods::default(),
        held: None,
        last_cell: (0, 0),
        scroll_acc: 0.0,
        command,
        term: None,
        pty_tx: None,
        machine: None,
        child_alive: false,
        stats: Stats {
            font_load,
            ..Stats::default()
        },
        exit: false,
    };

    // Learn the outputs' logical size and scale before sizing the panel.
    queue.roundtrip(&mut app)?;
    queue.roundtrip(&mut app)?;
    let (output_size, scale) = app
        .output_state
        .outputs()
        .find_map(|output| app.output_state.info(&output))
        .map_or(((0, 0), 1), |info| {
            (
                info.logical_size.unwrap_or((0, 0)),
                info.scale_factor.max(1) as u32,
            )
        });
    app.geom.scale = scale;
    app.logical = popup_size(output_size);
    app.font.set_size(FONT_PX * scale as f32);

    let surface = compositor.create_surface(&qh);
    let layer =
        layer_shell.create_layer_surface(&qh, surface, Layer::Overlay, Some("tablero-tui"), None);
    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    layer.set_size(app.logical.0, app.logical.1);
    layer.commit();
    app.layer = Some(layer);
    let (w, h) = (app.logical.0 * scale, app.logical.1 * scale);
    app.pool = Some(SlotPool::new((w * h * 4) as usize, &app.shm)?);

    WaylandSource::new(conn.clone(), queue)
        .insert(event_loop.handle())
        .map_err(|e| e.error)?;
    while !app.exit {
        event_loop.dispatch(None, &mut app)?;
    }

    // Unmap first so the panel disappears while the program is reaped.
    app.layer = None;
    app.buffer = None;
    conn.flush()?;
    app.shutdown();
    app.report();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Mods = Mods {
        ctrl: false,
        alt: false,
        shift: false,
    };
    const CTRL: Mods = Mods {
        ctrl: true,
        alt: false,
        shift: false,
    };

    fn key(keysym: Keysym, mods: Mods, text: Option<&str>) -> Vec<u8> {
        encode_key(keysym, mods, text, false).unwrap_or_default()
    }

    #[test]
    fn grid_size_divides_the_panel_into_whole_cells() {
        assert_eq!(grid_size(800, 600, 10, 20), (80, 30));
        assert_eq!(grid_size(805, 619, 10, 20), (80, 30));
    }

    #[test]
    fn grid_size_never_goes_below_twenty_by_five() {
        assert_eq!(grid_size(50, 30, 10, 20), (20, 5));
        assert_eq!(grid_size(800, 600, 0, 0), (20, 5));
    }

    #[test]
    fn popup_size_is_sixty_percent_of_the_output() {
        assert_eq!(popup_size((2560, 1440)), (1536, 864));
        assert_eq!(popup_size((0, 0)), (800, 500));
    }

    #[test]
    fn close_chord_is_ctrl_shift_q_only() {
        let ctrl_shift = Mods {
            ctrl: true,
            alt: false,
            shift: true,
        };
        assert!(is_close_chord(Keysym::Q, ctrl_shift));
        assert!(is_close_chord(Keysym::q, ctrl_shift));
        assert!(!is_close_chord(Keysym::q, CTRL));
        assert!(!is_close_chord(Keysym::q, NONE));
        assert!(!is_close_chord(Keysym::Escape, ctrl_shift));
    }

    #[test]
    fn printable_text_passes_through() {
        assert_eq!(key(Keysym::a, NONE, Some("a")), b"a");
        assert_eq!(key(Keysym::eacute, NONE, Some("é")), "é".as_bytes());
    }

    #[test]
    fn control_letters_become_c0_codes() {
        assert_eq!(key(Keysym::c, CTRL, Some("c")), [0x03]);
        assert_eq!(key(Keysym::c, CTRL, Some("\u{3}")), [0x03]);
    }

    #[test]
    fn alt_prefixes_escape() {
        let alt = Mods { alt: true, ..NONE };
        assert_eq!(key(Keysym::x, alt, Some("x")), b"\x1bx");
    }

    #[test]
    fn editing_keys() {
        let shift = Mods {
            shift: true,
            ..NONE
        };
        assert_eq!(key(Keysym::Return, NONE, Some("\r")), b"\r");
        assert_eq!(key(Keysym::BackSpace, NONE, Some("\u{8}")), [0x7f]);
        assert_eq!(key(Keysym::Tab, NONE, Some("\t")), b"\t");
        assert_eq!(key(Keysym::ISO_Left_Tab, shift, None), b"\x1b[Z");
        assert_eq!(key(Keysym::Escape, NONE, Some("\u{1b}")), [0x1b]);
        assert_eq!(key(Keysym::Delete, NONE, None), b"\x1b[3~");
        assert_eq!(key(Keysym::Page_Down, NONE, None), b"\x1b[6~");
    }

    #[test]
    fn arrows_follow_cursor_key_mode_and_modifiers() {
        assert_eq!(key(Keysym::Up, NONE, None), b"\x1b[A");
        assert_eq!(encode_key(Keysym::Up, NONE, None, true).unwrap(), b"\x1bOA");
        assert_eq!(key(Keysym::Left, CTRL, None), b"\x1b[1;5D");
        assert_eq!(key(Keysym::Home, NONE, None), b"\x1b[H");
    }

    #[test]
    fn function_keys() {
        assert_eq!(key(Keysym::F1, NONE, None), b"\x1bOP");
        assert_eq!(key(Keysym::F5, NONE, None), b"\x1b[15~");
        assert_eq!(key(Keysym::F12, NONE, None), b"\x1b[24~");
    }

    #[test]
    fn bare_modifiers_send_nothing() {
        assert_eq!(encode_key(Keysym::Shift_L, NONE, None, false), None);
    }

    #[test]
    fn sgr_mouse_is_one_based() {
        assert_eq!(encode_mouse_sgr(0, 0, 0, true), b"\x1b[<0;1;1M");
        assert_eq!(encode_mouse_sgr(2, 9, 4, false), b"\x1b[<2;10;5m");
        assert_eq!(encode_mouse_sgr(64, 3, 3, true), b"\x1b[<64;4;4M");
    }
}
