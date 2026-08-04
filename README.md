# tablero

A fast, owned Hyprland status bar — a Waybar replacement built on
`wlr-layer-shell` with software (CPU) rendering through shared memory.

## Install

tablero requires a Wayland compositor with `wlr-layer-shell` support. It is
developed for Hyprland and builds against PipeWire, libudev, and xkbcommon.

On Debian or Ubuntu, install the native build dependencies first:

```sh
sudo apt-get install libpipewire-0.3-dev libudev-dev libxkbcommon-dev pkg-config
```

On Arch Linux:

```sh
sudo pacman -S --needed pipewire libxkbcommon pkgconf rust
```

Then install the latest release from crates.io with the current stable Rust
toolchain:

```sh
cargo install tablero --locked
```

Run `tablero` from inside a Hyprland session. Set `RUST_LOG=info` to see startup
and lifecycle logging.

## Project layout

tablero is one publishable package with a library and binary target. The library
keeps the implementation split into compositor-independent config, rendering,
and widget modules plus the Wayland surface and producer integrations. The
binary loads the user's config and starts the Wayland event loop.

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
  - **Title** — the active window's title on the bound monitor (default in the
    center zone). Each bar shows its own monitor's focused window; focus changes
    on one output leave other bars unchanged. Long titles are truncated with
    `…` at 100 characters by default; `icon = "none"` keeps the default
    glyphless look.
  - **Clock** — a live local clock (`HH:MM`). `on-click` and `on-click-right`
  launch the configured program on a left- or right-click, useful for opening
  a calendar app or quick date picker.
  - **Battery** — percentage and charge state via UPower (blank when no battery
    is present).
  - **Backlight** — screen brightness read directly from Linux sysfs. **Opt-in**:
    add `"backlight"` to a zone. Scroll up/down over the widget to adjust the
    selected device through systemd-logind, without `brightnessctl` or another
    status bar. Device state follows native udev events with periodic recovery
    polling, and the widget reserves no slot when no screen backlight is present.
  - **System** — CPU and memory load sampled from procfs.
  - **Updates** — available Arch Linux package updates, checked immediately and
    every five minutes. **Opt-in** in the built-in config and enabled first in the
    ARC preset's right zone. Official repository updates come from the safe,
    isolated database used by `checkupdates`; when `paru` is installed, pending
    AUR updates from `paru -Qua` are added. The widget hides when the total is
    zero or the required official check is unavailable.
  - **Network** — connection state via NetworkManager (disconnected, wired,
    wireless, or unknown), with the Wi-Fi SSID shown when available; blank when
    NetworkManager is unavailable. `on-click` and `on-click-right` launch the
    configured program on a left- or right-click, useful for opening a network
    manager such as `nm-connection-editor` or `networkmanager_dmenu`.
  - **Tray** — StatusNotifierItem (SNI) system-tray icons from background apps,
    over DBus. **Opt-in**: not in the default set; add `"tray"` to `widgets` to
    enable it. Left-click activates conventional items and opens menu-only
    AppIndicators; right-click opens the item's native DBusMenu, including nested,
    disabled, separator, checkbox, and radio entries. Items without an exported
    menu receive the SNI `ContextMenu` fallback. Under a compositor with no native
    `StatusNotifierWatcher` (e.g. Hyprland) the bar hosts one itself. Icons come
    from an embedded pixmap or a themed PNG; an item with neither falls back to
    its initial letter.
  - **Bluetooth** — the local adapter state (powered on/off) and connected-
    device count, read from BlueZ over DBus. Shows `on`, `off`, `N connected`,
    or `unavailable` when no adapter is present (desktops without Bluetooth
    hardware always reserve the slot). **Opt-in**: not in the default set;
    add `"bluetooth"` to a zone to enable it. Any widget can carry an
    `on-click` config field — when set, a left-click inside the widget spawns
    that executable directly (no shell), so a typical use is `on-click =
    "/path/to/blueman-manager"` to launch a Bluetooth manager on click.
  - **Volume** — the active output sink's level (`Vol N%`) and mute state
    (`Mute`), read over the native PipeWire wire protocol. The icon is a
    built-in speaker that follows the level — one more wave per step from
    low to high, and the muted speaker when muted or at 0% — so the icon
    hints at the level at a glance.
    Switching the active sink still triggers a repaint. **Opt-in**: not in the default set;
    add `"volume"` to a zone to enable it. The widget reserves no slot
    when no PipeWire server is reachable (or no output sink exists),
    matching how the `network` and `system` widgets treat an absent
    source. The `on-click` field wires a click to a launcher (e.g.
    `on-click = "/usr/bin/pavucontrol"`).
  - **Notifications** — a bell indicator for swaync (SwayNotificationCenter),
    followed natively over DBus (no `swaync-client` subprocess). A small dot
    in the bell's corner while notifications are pending; slashed and dimmed
    under Do-Not-Disturb. **Left-click toggles the swaync panel, right-click
    toggles DND.** **Opt-in**: not in the default set; add `"notifications"`
    to a zone to enable it. Requires swaync running as the notification
    daemon; while it is not on the bus the widget reserves no slot, and it
    reappears automatically when swaync (re)starts.
  - **Power profiles** — the active power-profiles-daemon profile, followed
    natively over the system DBus. Enabled by default after the network module;
    it hides when the daemon is unavailable. Left-click rotates forward through
    the advertised profiles, right-click rotates backward, and hovering shows
    the configured profile and driver tooltip.
  - **Hypridle** — exact same-user process state read natively from procfs, with
    no polling script or `pgrep`/`killall` helper. **Opt-in**: add `"hypridle"`
    to a zone. The lock glyph uses the widget accent while Hypridle is active and
    its foreground while inactive; left-click starts or stops the daemon and the
    tooltip reports the current state.
  - **Power** — a compact power glyph with configurable direct-launch actions.
    **Opt-in**: add `"power"` to a zone. `on-click = "wlogout"` opens the logout
    menu and `on-click-right = "hyprlock"` preserves a secondary lock action.
    A hover tooltip shows the configured `tooltip-format` text (default
    `"Power menu"`); set `tooltip = false` to disable.
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
  NetworkManager over DBus, and peers) on an off-thread Tokio runtime; they
  reach the synchronous render loop only by sending messages through a
  `calloop` channel. **Each producer starts only when some bar layout includes
  its widget** (Hyprland always runs for workspaces/title), so a minimal config
  does not open PipeWire, host a StatusNotifierWatcher, or poll backlight
  sysfs. The PipeWire volume source is the one exception to the Tokio-worker
  pattern: it runs on a dedicated OS thread (PipeWire's `MainLoop` is
  synchronous and file-descriptor-driven, unlike zbus), and reaches the Tokio
  runtime the same way — by sending `Msg::Volume`s through the cross-thread
  `MsgSender`. That thread supervises its own connection: a `core.sync`
  heartbeat proves the server is really there (connecting to a socket-activated
  `pipewire-0` before the daemon exists otherwise *succeeds* and then never
  handshakes), and an unanswered one reconnects on a 1s→30s backoff — so a bar
  started before its audio server, or a daemon restarted under it, costs a
  couple of seconds rather than the session.
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

An opinionated, ready-to-copy ARC Raiders-inspired desktop preset lives at
[`config.example.toml`](https://github.com/piny4man/tablero/blob/main/crates/tablero/config.example.toml).
It demonstrates the full styling surface and is intentionally different from
the built-in defaults documented below:

```sh
mkdir -p ~/.config/tablero
curl --fail --location \
  https://raw.githubusercontent.com/piny4man/tablero/main/crates/tablero/config.example.toml \
  --output ~/.config/tablero/config.toml
```

### Reference

Every value below is the built-in default.

```toml
# Bar height in logical pixels (scaled to the output's pixel density on HiDPI
# displays). The width always spans the output.
height = 32

[bar]
# Logical-pixel inset around the full module row and gap between adjacent modules.
margin = 0
gap = 0

# Modules are placed independently in left, center, and right zones. Valid names:
# "workspaces", "title", "clock", "battery", "system", "network", "tray",
# "bluetooth", "volume", "backlight", "notifications", "updates", "hypridle",
# "power", and "power-profiles-daemon". Repeats are de-duplicated, keeping first
# position.
modules-left = ["workspaces"]
modules-center = ["title"]
modules-right = ["clock", "battery", "system", "network", "power-profiles-daemon"]

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
| `bar.background` | hex color (opt.) | unset                                       | Bar fill; inherits `theme.background` when unset.                |
| `bar.margin`   | integer (px)    | `0`                                           | Inset around the full module row.                                |
| `bar.gap`      | integer (px)    | `0`                                           | Gap between adjacent modules in a zone.                          |
| `bar.modules-left` | list of strings | `["workspaces"]`                          | Modules packed against the left edge.                            |
| `bar.modules-center` | list of strings | `["title"]`                             | Modules centered independently of the edge zones.                |
| `bar.modules-right` | list of strings | `["clock", "battery", "system", "network", "power-profiles-daemon"]` | Modules packed against the right edge. Also accepts the opt-in modules `"tray"`, `"bluetooth"`, `"volume"`, `"backlight"`, `"notifications"`, `"updates"`, `"hypridle"`, and `"power"`. |
| `theme.background` | hex color   | `"#181818"`                                   | Fill behind every widget.                                        |
| `theme.foreground` | hex color   | `"#eaeaea"`                                   | Default text color.                                              |
| `theme.accent`     | hex color   | `"#eaeaea"`                                   | Emphasis color (e.g. the active workspace).                      |
| `font.family`  | string (opt.)   | unset → system font                           | Font family name.                                                |
| `font.size`    | float (px)      | `16.0`                                         | Text size.                                                       |

Per-widget tables support equipment-like fills and outlines without changing
the global theme:

```toml
[widget.clock]
background = "#292b1d"
foreground = "#f0d77b"
border = "#766b36"
border-width = 1
radius = 2
padding = 10
```

`background`, `foreground`, `accent`, and `border` accept the same six- or
eight-digit hex colors as the theme. `border-width`, `radius`, and `padding` are
logical pixels and scale with the output. A border is optional; when `border` is
set and `border-width` is omitted, the width defaults to one logical pixel.

The battery also supports `format` placeholders: `{icon}`, `{percent}`, and
`{state}`. State-specific colors can override the fill, text, and border while
charging without changing the normal or low-battery appearance:

```toml
[widget.battery]
format = "{icon} {percent}%"

[widget.battery.charging]
background = "#2df18520"
foreground = "#5ff5a0"
border = "#2df185"
```

Widget icons are **built-in vector art** drawn directly with `tiny-skia`, so no
icon font, SVG parser, or bundled glyph set is shipped — the icon inherits the
widget's state colors and scales with the output. Each icon-bearing widget takes
an `icon` field on its `[widget.<name>]` table:

- omitted → the widget's built-in icon,
- `icon = "none"` → **opt out**; the widget renders label-only (or nothing, for
  icon-only widgets like `power`), keeping a simple text look,
- `icon = "<text>"` → override with any literal string, e.g. a Nerd Font glyph
  or an emoji, rendered as text in place of the vector icon.

For widgets with a `format`/`format-icons` string (battery, backlight,
power-profiles-daemon, updates), the `{icon}` placeholder marks where the icon
sits; a `format-icons` entry supplies a per-state text glyph that overrides the
built-in art for that state.

Per-widget click handlers live on every `[widget.<name>]` table as
`on-click = "/path/to/executable"`. When set, a left-click inside that widget
spawns the file directly (**no shell**) — typical use is
`[widget.bluetooth] on-click = "/usr/bin/blueman-manager"` or
`[widget.volume] on-click = "/usr/bin/pavucontrol"`. Rules:

- Prefer **absolute paths** (`/usr/bin/…` or `~/scripts/…`). A leading `~` on
  the program is expanded at click time; absolute paths are preflighted (must
  exist and be executable).
- Bare names (`pavucontrol`, `wlogout`) are resolved through the bar process's
  `PATH` (whatever Hyprland or your autostart gave `tablero`). Missing bare
  names fail with a clear `not found in PATH` log line.
- **No shell**: no pipes, `&&`, env vars, or globs. You *may* pass arguments by
  whitespace (`on-click = "gtk-launch org.gnome.Calendar"`) or as a TOML list
  (`on-click = ["gtk-launch", "org.gnome.Calendar"]`). Multi-step logic still
  belongs in a small `#!/bin/sh` script with `chmod +x`.
- Today bluetooth, volume, updates, network, clock, and power honor `on-click`.
  Power (and network/clock) also accept `on-click-right` with the same semantics.
- If a click shows the hand cursor but nothing happens, run with
  `RUST_LOG=tablero::command=info` (or `warn`): spawn failures and non-zero
  child exits are logged with the resolved path. Child stderr is inherited, so
  script errors also appear next to tablero's logs.

```toml
[bar]
modules-right = ["hypridle", "power", "clock"]

[widget.hypridle]
foreground = "#ff4a4d" # inactive
accent = "#7decff"     # active

[widget.power]
on-click = "/usr/bin/wlogout"
on-click-right = "/usr/bin/hyprlock"

[widget.volume]
on-click = "/usr/bin/pavucontrol"

[widget.network]
on-click = "~/.config/hypr/scripts/networkmanager.sh"

[widget.clock]
# Desktop apps: absolute path is more reliable than a bare name when PATH is thin.
on-click = "/usr/bin/gnome-calendar"
```

The Arch package-updates module requires `checkupdates` from the official
`pacman-contrib` package. `paru` is optional; when available, its AUR count is
added to the official repository count. No Waybar helper module is required:

```sh
sudo pacman -S --needed pacman-contrib
```

```toml
[bar]
modules-right = ["updates", "clock"]

[widget.updates]
format = "{icon} {count}"
# Optional direct executable or script; no shell or arguments are implied.
# on-click = "/path/to/update-system"
```

Only `{count}` and `{icon}` are accepted in `format`. Checks run immediately and
then every 300 seconds. A missing or failed `paru` command falls back to official
updates; a missing or failed `checkupdates` command hides the module rather than
presenting a misleading partial total. Hovering the module lists each package's
installed and available versions, grouped by official repositories and AUR.

The backlight module has four module-specific fields:

```toml
[bar]
modules-right = ["backlight", "battery", "network"]

[widget.backlight]
device = "intel_backlight" # optional; auto-selects the largest range when absent
format = "{icon}  {percent}%"
format-icons = ["", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]
scroll-step = 1
```

Only `{icon}` and `{percent}` are accepted in `format`. Icons are selected from
low to high brightness. Scrolling requires an active systemd-logind session;
display remains available if logind cannot perform writes.

The power-profiles-daemon module is enabled by default and uses the daemon's
advertised profile order for click rotation:

```toml
[widget.power-profiles-daemon]
format = "{icon}"
tooltip-format = "Power profile: {profile}\nDriver: {driver}"
tooltip = true

[widget.power-profiles-daemon.format-icons]
default = ""
performance = ""
balanced = ""
power-saver = ""
```

Both `format` and `tooltip-format` accept `{icon}`, `{profile}`, `{driver}`,
`{cpu_driver}`, and `{platform_driver}`. The module talks directly to the
official system D-Bus API and falls back to its legacy compatible name; it never
spawns `powerprofilesctl`.

### Per-monitor overrides

tablero shows one bar per output. By default every output runs on the global
settings above; add a `[[monitor]]` block to override a specific output, matched
by its Hyprland connector name (`hyprctl monitors` lists them — `DP-1`,
`HDMI-A-1`, `eDP-1`, …):

```toml
[[monitor]]
name = "DP-1"          # required: the connector to match
height = 40            # override just this output's height

[monitor.bar]
modules-center = ["clock"] # replace just this output's center zone

[monitor.theme]
accent = "#88c0d0"     # only the accent changes; background/foreground inherit

[monitor.font]
size = 18.0            # larger text on this monitor only

[[monitor]]
name = "eDP-1"         # the laptop panel: shorter bar, everything else global
height = 28
[monitor.widget.title]
background = "#2e3440" # give the title a pill on this output only
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
| `monitor.bar.background` | hex color (opt.) | Overrides the bar fill on this output.                           |
| `monitor.bar.margin` | integer (px)    | Overrides the module-row inset on this output.                        |
| `monitor.bar.gap`    | integer (px)    | Overrides the inter-module gap on this output.                        |
| `monitor.bar.modules-*` | list of strings | Replaces the named module zone on this output.                     |
| `monitor.theme.*`    | hex color       | Per-channel theme override; omitted channels inherit the global theme. |
| `monitor.font.*`     | family / size   | Per-field font override; omitted fields inherit the global font.      |
| `monitor.widget.*`  | per-widget table | Per-widget style override (e.g. `[monitor.widget.title] background`); folded per-field onto the global `[widget.<name>]` table. |

## Manual verification under Hyprland

The render, blit, widget, and config paths are unit- and integration-tested, but
surface placement and input need a live compositor. To verify on Hyprland:

1. From inside a Hyprland session, run `RUST_LOG=info cargo run -p tablero`.
2. Confirm a bar appears **pinned to the top** of the screen, spanning its full
   width, showing a dark background with the workspaces, **title**, clock,
   battery, system, network, and power-profile widgets in their configured zones.
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
    - Left-clicking a conventional item **activates** the app through DBus
      `Activate`; a menu-only AppIndicator opens its menu instead.
    - Right-click opens the exported DBusMenu. Confirm nested entries,
      separators, disabled actions, checkboxes, and radio items render correctly,
      and selecting an enabled leaf performs its action. An item with no exported
      menu should receive its own SNI `ContextMenu` call instead.
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
    - **`ItemIsMenu` is often omitted.** When a valid `Menu` path exists and the
      property is absent, tablero treats the item as menu-only for AppIndicator
      compatibility. An explicit `false` preserves left-click activation.
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
13. Verify the **title widget** (default in `modules-center`). With at least
    two outputs (`hyprctl monitors`), focus a window on output A and confirm
    that only output A's bar updates; output B's bar should keep its
    previous title. Switching focus between windows on output A should
    update only A's title within one frame. A long title (browser tab with
    a full URL, music player track) should cap at `100` characters with a
    trailing `…` and never overflow the center zone. Closing the last
    focused window should leave the center blank.
14. Verify the **bluetooth widget** (opt-in: add `"bluetooth"` to a zone).
    With BlueZ running:
    - The widget appears with the bluetooth glyph and a label of `on`, `off`,
      `N connected`, or `unavailable`. On a desktop with no Bluetooth
      hardware it should always reserve a slot and read `unavailable`; on a
      laptop with the adapter powered off it reads `off`.
    - Toggle the adapter with `bluetoothctl power on` / `bluetoothctl power
      off` and confirm the label changes by the next poll (the producer
      polls BlueZ every two seconds, so allow up to a couple of seconds
      for the flip).
    - Pair and connect a device (e.g. `bluetoothctl connect <mac>`); confirm
      the label flips to `1 connected` (or higher) and back to `on` on
      disconnect.
    - Pair/unpair with the adapter powered off; the count stays at zero and
      the label stays `off`.
    - If a `[widget.bluetooth] on-click = "/path/to/executable"` is set,
      left-click the widget and confirm the configured process is spawned
      (no shell, direct exec). The script must be marked executable; a
      missing or non-executable path is logged as an error and does not
      crash the bar.
 15. Verify the **volume widget** (opt-in: add `"volume"` to a zone). With
     a running PipeWire server (pipewire + wireplumber / pipewire-pulse)
     and at least one output sink configured:
     - The widget appears with a built-in speaker icon whose waves follow the
       level (bare speaker at 1-33%, one wave at 34-66%, two waves above; the
       muted speaker at 0%) and a label of `Vol N%` (where `N` is the
       active sink's level). On a system with PipeWire but no output sink configured,
       the widget reserves no slot (matching the `network` / `system`
       pattern); on a system with no PipeWire running, the widget also
       reserves no slot.
     - Change the volume with `pactl set-sink-volume @DEFAULT_SINK@ 50%`
       (or `wpctl set-volume @DEFAULT_AUDIO_SINK@ 0.5`) and confirm the
       label updates by the next poll (the producer re-emits every two
       seconds when the cached state changes). The widget should track
       per-cent: `12%`, `13%`, …
     - Toggle mute with `pactl set-sink-mute @DEFAULT_SINK@ 1` and confirm
       the label flips to `Mute` with the muted speaker icon; toggling
       mute back restores `Vol N%` with the level-based icon. Setting the
       level to `0%` unmuted shows `Vol 0%` with the same muted icon.
     - With multiple sinks configured (e.g. headphones + monitor speakers),
       play audio through one and confirm the widget tracks *that* sink
       (active sink has its `NodeState` set to `Running`); switching
       playback to a different sink updates the widget to that sink
       within a poll.
     - If a `[widget.volume] on-click = "/path/to/executable"` is set
       (typical: `pavucontrol`), left-click the widget and confirm the
       configured process is spawned (no shell, direct exec). A missing
       or non-executable path is logged as an error and does not crash
       the bar.
     - Restart the audio server under the running bar with `systemctl --user
       restart pipewire.service` and confirm the widget briefly reserves no
       slot and then comes back with the right level, instead of freezing or
       disappearing for the rest of the session. Starting the bar *before*
       PipeWire (the usual compositor-autostart race) is the same code path;
       `RUST_LOG=tablero=debug` shows the reconnect and the
       `PipeWire core handshake complete` that follows it.
 16. Verify the **notifications widget** (opt-in: add `"notifications"` to a
     zone). With swaync running as the session's notification daemon:
     - The widget appears as a bell glyph with no dot. With swaync not
       running, the widget reserves no slot.
     - Send `notify-send test`; swaync shows its popup and a small dot
       appears in the bell's upper-right corner. Clearing the notification
       (from the popup or the panel) removes the dot.
     - Left-click the bell and confirm the swaync control-center panel
       toggles open/closed.
     - Right-click the bell and confirm Do-Not-Disturb toggles: the bell
       swaps to the slashed glyph and dims; right-clicking again restores
       it. The DND state stays in sync when toggled from inside the swaync
       panel instead.
      - Restart resilience: stopping swaync removes the widget from the bar;
        starting it again brings it back with the current state, without
        restarting tablero. Note that swaync is usually DBus-activatable, so
       on most setups `pkill swaync` resurrects it near-instantly (any bus
       call to its name starts it again) — `systemctl --user mask swaync`
       first (and `unmask` after) to observe the widget-hidden state.
       tablero itself never triggers that activation: its presence probe is
        sent with the `NO_AUTO_START` flag, so it observes the daemon rather
        than restarting one the user stopped.
17. Verify the **Hypridle and power widgets** (opt-in: add `"hypridle"` and
    `"power"` to a zone).
    - Start with Hypridle running. The lock glyph should use the configured
      accent and its tooltip should read `Hypridle active`.
    - Left-click the lock. The process should receive `SIGTERM`, the glyph should
      switch to the configured inactive foreground, and the tooltip should read
      `Hypridle inactive`. Left-click again to start `hypridle` directly.
    - Left-click the power glyph and confirm the configured `on-click` executable
      starts. Right-click it and confirm `on-click-right` starts independently.
      A missing executable should log a warning without terminating tablero.

## License

tablero is licensed under the [GNU General Public License v3.0](LICENSE).
