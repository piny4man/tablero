# tablero

A fast, owned Hyprland status bar — a Waybar replacement built on
`wlr-layer-shell` with software (CPU) rendering through shared memory.

## Workspace layout

| Crate              | Responsibility                                                                  |
| ------------------ | ------------------------------------------------------------------------------- |
| `tablero`          | Binary entry point; loads the config and wires it into the Wayland event loop.  |
| `tablero-core`     | Compositor-independent logic: config, widgets/messages, text render, blit.      |
| `tablero-wayland`  | `wlr-layer-shell` surface, async producers, shared-memory buffers, `calloop`.   |

## Build & run

```sh
cargo build --all-targets
cargo test --all          # config + render + widget unit/integration tests (no compositor needed)
cargo run -p tablero      # opens the bar; run from within a Hyprland session
```

Set `RUST_LOG=info` for startup and lifecycle logging:

```sh
RUST_LOG=info cargo run -p tablero
```

## What it does

- Opens a **top-anchored** layer-shell surface spanning the output width, with an
  **exclusive zone** equal to its height so tiled windows do not overlap it. The
  height (default 32px) is configurable.
- Shows **one bar per monitor**: it tracks Wayland output lifecycle and opens a
  surface on each output as it appears, tearing it down when the output is
  unplugged — hotplug never crashes the bar or leaks stale state. Each output's
  bar can be configured independently (widget set and visual overrides), and its
  **workspace widget is scoped to that monitor** — it shows only that monitor's
  workspaces and highlights *its* active one, matching Hyprland's per-monitor
  workspace model.
- Renders, left to right, a set of **widgets** driven by a typed message
  architecture, each repainting only when its visible state actually changes:
  - **Workspaces** — the Hyprland workspace set, active one bracketed and drawn
    in the accent color. **Click a workspace to switch to it.**
  - **Clock** — a live local clock (`HH:MM`).
  - **Battery** — percentage and charge state via UPower (blank when no battery
    is present).
  - **System** — CPU and memory load sampled from procfs.
  - **Network** — connection state via NetworkManager (disconnected, wired,
    wireless, or unknown), with the Wi-Fi SSID shown when available; blank when
    NetworkManager is unavailable.
  - **Tray** — StatusNotifierItem (SNI) system-tray icons from background apps,
    over DBus. **Opt-in**: not in the default set; add `"tray"` to `widgets` to
    enable it. **Click an icon to activate** its application. Under a compositor
    with no native `StatusNotifierWatcher` (e.g. Hyprland) the bar hosts one
    itself. Icons come from an embedded pixmap or a themed PNG; an item with
    neither falls back to its initial letter.
- Draws through `cosmic-text` + `tiny-skia`, committed via a `wl_shm` ARGB8888
  buffer.
- Handles **HiDPI / output scaling**: surface geometry stays in logical pixels
  (so the bar keeps a consistent apparent size across displays), while the
  shared-memory buffer is allocated at the output's physical pixel density and
  `set_buffer_scale` maps it back. Text and layout are scaled exactly once — no
  double-scaling. Integer-scale outputs are pixel-crisp; fractional-scale
  Hyprland setups render at the next integer scale and the compositor downscales,
  which stays sharp in practice.
- Pulls live data from **async producers** (Hyprland IPC, UPower, procfs,
  NetworkManager over DBus) running on an off-thread Tokio runtime; they reach
  the synchronous render loop only by sending messages through a `calloop`
  channel.
- Wakes **only** for clock ticks (a `calloop` timer aligned to the wall-clock
  second), producer messages, pointer input, compositor configure events, or
  shutdown — there is no busy redraw loop and no frame-callback feedback cycle.

## Configuration

tablero reads an optional TOML file from
`$XDG_CONFIG_HOME/tablero/config.toml` (falling back to
`$HOME/.config/tablero/config.toml`). **The file is optional**: when it is
absent the bar runs on the documented defaults below. The document may be
partial — any field you omit falls back to its default, so you only specify what
you want to change.

Invalid configuration is a **hard error**, never a silent fallback: an unknown
key, an unknown widget name, or a malformed color stops startup with a clear
message naming the file.

A ready-to-copy, fully-commented template lives at
[`crates/tablero/config.example.toml`](crates/tablero/config.example.toml):

```sh
mkdir -p ~/.config/tablero
cp crates/tablero/config.example.toml ~/.config/tablero/config.toml
```

### Reference

Every value below is the built-in default.

```toml
# Bar height in logical pixels (scaled to the output's pixel density on HiDPI
# displays). The width always spans the output.
height = 32

# Horizontal gap between adjacent widget columns, in pixels.
spacing = 0

# Inner padding inset on each widget column, in pixels.
padding = 0

# Widgets to render, left to right. Valid names: "workspaces", "clock",
# "battery", "system", "network", "tray". "tray" is opt-in (not in the default
# set above) — add it to enable the system tray. Repeats are de-duplicated,
# keeping first position.
widgets = ["workspaces", "clock", "battery", "system", "network"]

[theme]
# Colors are "#rrggbb" hex strings (the leading "#" is optional).
background = "#181818" # fill behind every widget
foreground = "#eaeaea" # default text color
accent     = "#eaeaea" # emphasis color (e.g. the active workspace)

[font]
# family is unset by default, which uses the system default font. Uncomment to
# pick a specific family:
# family = "JetBrains Mono"
size = 16.0
```

| Key            | Type            | Default                                       | Notes                                                            |
| -------------- | --------------- | --------------------------------------------- | ---------------------------------------------------------------- |
| `height`       | integer (px)    | `32`                                          | Bar height; also drives the exclusive zone.                      |
| `spacing`      | integer (px)    | `0`                                           | Gap between adjacent widget columns.                             |
| `padding`      | integer (px)    | `0`                                           | Inset applied inside each widget column.                         |
| `widgets`      | list of strings | `["workspaces", "clock", "battery", "system", "network"]` | Render order, left to right; duplicates keep their first slot. Also accepts `"tray"` (opt-in system tray). |
| `theme.background` | hex color   | `"#181818"`                                   | Fill behind every widget.                                        |
| `theme.foreground` | hex color   | `"#eaeaea"`                                   | Default text color.                                              |
| `theme.accent`     | hex color   | `"#eaeaea"`                                   | Emphasis color (e.g. the active workspace).                      |
| `font.family`  | string (opt.)   | unset → system font                           | Font family name.                                                |
| `font.size`    | float (px)      | `16.0`                                         | Text size.                                                       |

### Per-monitor overrides

tablero shows one bar per output. By default every output runs on the global
settings above; add a `[[monitor]]` block to override a specific output, matched
by its Hyprland connector name (`hyprctl monitors` lists them — `DP-1`,
`HDMI-A-1`, `eDP-1`, …):

```toml
[[monitor]]
name = "DP-1"          # required: the connector to match
height = 40            # override just this output's height
widgets = ["workspaces", "clock"]   # and its widget set

[monitor.theme]
accent = "#88c0d0"     # only the accent changes; background/foreground inherit

[monitor.font]
size = 18.0            # larger text on this monitor only

[[monitor]]
name = "eDP-1"         # the laptop panel: shorter bar, everything else global
height = 28
```

Every field **except `name` is optional** and overrides are shallow per field: a
field you omit keeps the output's global value (and within `[monitor.theme]` /
`[monitor.font]`, an omitted channel inherits the global theme/font rather than
resetting to the built-in default). An output whose connector matches no
`[[monitor]]` block — or that advertises no name — runs on the global defaults
unchanged.

| Key                  | Type            | Notes                                                                 |
| -------------------- | --------------- | --------------------------------------------------------------------- |
| `monitor` (`[[monitor]]`) | array of tables | One block per output you want to customize; omit entirely for a uniform bar. |
| `monitor.name`       | string          | **Required.** Hyprland connector name to match (`hyprctl monitors`).  |
| `monitor.height`     | integer (px)    | Overrides `height` on this output.                                    |
| `monitor.spacing`    | integer (px)    | Overrides `spacing` on this output.                                   |
| `monitor.padding`    | integer (px)    | Overrides `padding` on this output.                                   |
| `monitor.widgets`    | list of strings | Overrides the widget set/order on this output.                        |
| `monitor.theme.*`    | hex color       | Per-channel theme override; omitted channels inherit the global theme. |
| `monitor.font.*`     | family / size   | Per-field font override; omitted fields inherit the global font.      |

## Manual verification under Hyprland

The render, blit, widget, and config paths are unit- and integration-tested, but
surface placement and input need a live compositor. To verify on Hyprland:

1. From inside a Hyprland session, run `RUST_LOG=info cargo run -p tablero`.
2. Confirm a bar appears **pinned to the top** of the screen, spanning its full
   width, showing a dark background with the workspaces, clock, battery,
   system, and network widgets laid out left to right.
3. Confirm the clock **advances once per second** and that the text changes
   exactly on the second boundary (the timer is second-aligned).
4. Confirm the **workspace indicator tracks Hyprland** — switching workspaces by
   any means updates which id is bracketed/accented — and that **clicking a
   workspace in the bar switches to it**.
5. Confirm tiled windows are **pushed down by the bar's height** and never render
   under it — this proves the exclusive zone is honored.
6. Confirm the process is **idle between updates**: `top`/`htop` should show ~0%
   CPU for `tablero` when nothing is changing (no busy loop).
7. Optionally inspect the surface: `hyprctl layers` should list a `tablero`
   namespace in the `top` layer on the active output.
8. Drop a `~/.config/tablero/config.toml` (e.g. change `height` or the `widgets`
   order), restart, and confirm the change takes effect; introduce a typo and
   confirm the process **refuses to start with a clear error** rather than
   silently ignoring it.
9. Verify **HiDPI scaling** on a scaled output. Set a scale on the monitor (e.g.
   `hyprctl keyword monitor <name>,preferred,auto,2` for integer 2×, or `,1.5`
   for fractional), then run the bar on it:
   - The bar keeps the **same apparent height and text size** as on an unscaled
     output — geometry is logical, so it does not shrink or balloon.
   - Text and widget edges stay **crisp**, not blurry: the buffer is rendered at
     the output's physical resolution (`RUST_LOG=info` logs `output scale changed
     to Nx` when the compositor reports the scale).
   - **Clicking a workspace** still switches to it — pointer hit-testing tracks
     the scaled layout. On a fractional scale (e.g. 1.5×) the bar renders at 2×
     and the compositor downscales; confirm it still looks sharp and clicks land.
10. Verify **multi-monitor** placement with at least two outputs (`hyprctl
    monitors` lists them). With the bar running:
    - Confirm **each monitor has its own bar**, pinned to the top of *that*
      output and spanning *its* width — not one bar stretched across both, and no
      output left bare.
    - Confirm each bar's **workspace widget shows only its own monitor's
      workspaces** and brackets/accents the workspace active *on that monitor* —
      switching workspaces on one screen updates that screen's bar without
      disturbing the other's (this mirrors Hyprland's per-monitor workspaces).
    - Confirm **clicking a workspace** on a given monitor's bar switches that
      monitor — input is routed to the surface it lands on.
    - Add a `[[monitor]]` block for one connector (e.g. a different `height`,
      `widgets`, or `[monitor.theme] accent`), restart, and confirm **only that
      monitor's bar changes** while the other keeps the global look.
    - **Hotplug**: unplug a monitor (or `hyprctl keyword monitor <name>,disable`)
      and confirm its bar disappears while the remaining bar(s) keep running;
      re-enable it and confirm a fresh bar reappears. `RUST_LOG=info` logs
      `output … added` / `output … removed` and the app must not crash or leave a
      stale surface (`hyprctl layers` should list exactly one `tablero` namespace
      per live output).
11. Verify the **system tray** with at least two real tray-producing apps. Add
    `"tray"` to `widgets` (it is opt-in), restart, then launch two SNI clients —
    e.g. **Discord** or **Element** (Electron, ARGB pixmaps), **Telegram
    Desktop** (themed icon name), **nm-applet** or **blueman-applet** (classic
    Qt/GTK trays), or `KStatusNotifierItem` apps. Confirm:
    - Each app's icon **appears** in the bar shortly after launch and
      **disappears** when the app quits — the host re-enumerates on every
      watcher registration/unregistration.
    - **Clicking an icon activates** the app (raises/toggles its window) — the
      click maps to the item's DBus `Activate` call.
    - An app that ships only an `IconName` (no pixmap) still renders, resolved
      against the icon theme; an app with neither shows its **initial letter**
      rather than vanishing or crashing.
    - Quitting an app **mid-session** does not crash the bar or leave a stale
      icon.

    **Protocol quirks** worth knowing when reading the code or debugging:
    - **No watcher under bare compositors.** Hyprland (and similar) run no
      `org.kde.StatusNotifierWatcher`, so tablero serves one itself and only
      defers to an existing watcher when the well-known name is already owned.
    - **Registration address is ambiguous.** Apps register either a bare bus
      name (icon lives at the default `/StatusNotifierItem`), or a bus name
      immediately followed by an object path (Ayatana/`libdbusmenu` apps use a
      per-item path like `/org/ayatana/NotificationItem/foo`). The host splits
      at the first `/`; a bare path with no bus name is unaddressable and
      skipped.
    - **Pixmap bytes are ARGB32, network byte order**, *not* the RGBA the
      renderer wants — each pixel is reordered and alpha-premultiplied on
      decode. Items often ship several sizes; the largest by area is chosen.
    - **Icon-theme lookup is pragmatic, not full XDG.** `IconThemePath` (when
      set) leads, then common `hicolor` app/status sizes and `/usr/share/pixmaps`
      — `index.theme` inheritance is not walked, which covers typical bar apps
      without the full resolver.
12. Press the compositor's close path for the layer (or terminate the session)
    and confirm the process exits cleanly: each surface is removed via the
    `closed` handler, and the process shuts down once the last bar is gone.
