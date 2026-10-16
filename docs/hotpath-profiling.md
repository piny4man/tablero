# Profiling with hotpath

[hotpath](https://hotpath.rs/) is an opt-in profiler: built with its cargo
features, tablero reports per-function timing, allocations, per-thread CPU and
memory, channel traffic, and mutex contention when it exits. Without the
features every `#[hotpath::measure]`, `channel!`, and `mutex!` compiles to the
plain code and none of hotpath's dependencies are built (the release binary
differs by 80 bytes).

It complements, rather than replaces, `TABLERO_PERF`
([Interaction latency diagnostics](interaction-latency.md)): `TABLERO_PERF`
measures end-to-end latency (input to commit, frame phases), hotpath explains
where the time and memory inside those phases go.

## Running

| Feature | Report |
| --- | --- |
| `hotpath` | function timing, threads, channels, mutexes |
| `hotpath-alloc` | all of the above plus allocated bytes per function (`HOTPATH_ALLOC_METRIC=count` for allocation counts) |

Offscreen, repeatable, no compositor needed:

```sh
cargo run --release -p tablero --example frame-cost --features hotpath-alloc -- \
  ~/.config/tablero/config.toml 2 500
```

The live bar, as a separate development instance next to the daily one. The
report is written when the bar stops (`tablero --instance hotpath` control
commands, or `HOTPATH_SHUTDOWN_MS` for a fixed window):

```sh
cargo build --release -p tablero --features hotpath-alloc
HOTPATH_SHUTDOWN_MS=120000 HOTPATH_OUTPUT_FORMAT=json \
HOTPATH_OUTPUT_PATH=/tmp/tablero-hotpath.json TABLERO_PERF=1 \
RUST_LOG=tablero::performance=info \
  target/release/tablero --instance hotpath
```

Only the bar run is profiled: `#[hotpath::main]` sits on `run` in `main.rs`, so
`reload` and `restart` print no report.

### What is instrumented

- Render path: `Dashboard::{update,layout,draw,hover_at,on_click,on_scroll}`,
  every widget's `measure` and `draw`, the `RenderContext` primitives, text
  shaping and its cache, icon rasterising, `write_argb8888`, and the
  `Surface`/`App` frame, message, tooltip, and tray-menu paths in `lib.rs`.
- Producers: the parse, read, and snapshot functions of every producer module,
  config reload, and the command spawner.
- Channels: the command channel (`commands`) and the backlight udev channel.
  The producer → render-loop channel is a calloop channel, which hotpath cannot
  wrap.
- Mutexes: the volume state and the two SNI host mutexes.

Two hotpath behaviors to keep in mind when reading a report:

- An `async fn`'s time includes everything it awaits. `hyprland::read_batch`
  topping the live report is a socket read waiting for the next event, not
  work; `sni::introspect_path` is summed over concurrent D-Bus calls.
- A literal `label = "…"` on `channel!`/`mutex!` expands to an
  `unsafe(export_name)` uniqueness check that `unsafe_code = "deny"` rejects;
  pass `label = "…".to_owned()` instead.

`tests/hotpath_profile.rs` (run with `--features hotpath`, and in CI) fails if
the render-phase instrumentation is lost.

## Findings — 2026-10-16

Recorded on an AMD Ryzen 7 8845HS (16 threads), one 2880×1800 output at scale
1.5 (a 3840×76 physical bar buffer), the qc71 platform profile in
`performance`. Release builds. Runs:

- `frame-cost` with the user config at scale 2: 200 back-to-back frames and 40
  frames 500 ms apart, under `hotpath` and `hotpath-alloc` (bytes and counts).
- Three 120 s live runs of `--instance hotpath` (`hotpath`, `hotpath-alloc`
  bytes, `hotpath-alloc` counts): 60 s idle, then five workspace switches, five
  volume and five backlight up/down pairs, each restored afterwards.

Not covered: the balanced and power-saver profiles (no profile switch was made
on the machine), and pointer-driven tooltip, tray menu, and click paths (not
scripted; the command channel saw no traffic).

Live baseline from `TABLERO_PERF` in the same runs (median / p95 / max):

| Metric | Value |
| --- | --- |
| `startup-to-first-commit` | 95–105 ms |
| `frame-total` | 1.72 / 2.4–3.4 / 52–60 ms (max is the first frame) |
| `frame-draw` | 1.39–1.42 / 1.74–1.85 ms |
| `frame-blit` | 0.25–0.26 / 0.67–0.99 ms |
| frames per 120 s | 43–64; 31 of 64 caused by the system widget |

Ranked by expected impact:

### 1. A 58 ms blocking sysfs read stalls the producer runtime every 5 s

`power_profiles::platform::read_platform_profile` reads
`/sys/devices/platform/qc71_laptop/performance_mode` with
`std::fs::read_to_string`. The qc71 driver answers it from the embedded
controller: 57–59 ms per read, measured both by hotpath (24 calls, avg 58.7 ms)
and directly. It runs from `compose` on every power-profiles poll, inside an
async producer on the runtime's **only** worker thread, so every 5 s every other
producer — Hyprland events, volume, SNI, and the command executor that turns a
click into a Hyprland dispatch — waits up to 58 ms.

Proposed fix: move the read to `tokio::task::spawn_blocking` (or a dedicated
thread), and re-read it only after tablero sets the profile or on a slower
cadence. Expected impact: removes a 58 ms worst-case stall from
click-to-command and event-to-redraw latency.

### 2. Every frame repaints and converts the whole bar

`Surface::paint_frame` always runs `Dashboard::draw` over the full pixmap and
`write_argb8888` over the full buffer; the computed `Damage` only narrows what
the compositor recomposites. Half of all live frames are the system widget
changing a few digits, yet each costs the full ~1.4 ms draw plus ~0.26 ms
conversion of 3840×76 pixels (`write_argb8888` alone is ~20 % of an offscreen
frame).

Proposed fix: draw only the widgets inside the damage (the dashboard already
knows which), and convert only the damage region. The two SHM slots alternate,
so a slot must be brought up to date with the union of this frame's and the
previous frame's damage. Expected impact: a system-widget frame drops from
~1.7 ms toward the cost of one pill, roughly 0.2–0.3 ms.

### 3. Rounded pills are rebuilt and rasterised from scratch every frame

`fill_rounded_rect` and `stroke_rounded_rect` take ~66 % of offscreen
`Dashboard::draw` time (80.8 of 121.7 ms over 200 frames; avg 34 µs and 11 µs
per call) and are the largest per-frame allocators: 8 and 12 allocations,
5.6 KB and 6.0 KB, per call — about 20 allocations and 100 KB per frame for nine
pills. Each call builds a fresh `Path` and lets tiny-skia rasterise and stroke
it.

Proposed fix: cache the rasterised pill (fill and stroke) keyed by size,
radius, stroke width, and color, the same way builtin icons are cached, and
blit it. Expected impact: draw time roughly halved and per-frame allocations
near zero; it also shrinks finding 2's remaining per-pill cost.

### 4. SNI fetches every icon pixmap, even when the icon name resolves

`sni::read_item_props` always reads `IconPixmap` before resolving the icon. One
item returned 32.5 MB in a single call; the five calls in a run allocated
38.8 MB, 75 % of all attributed allocation. Process RSS peaked at 108 MB.

Proposed fix: resolve `IconName` (and `IconThemePath`) first and fetch the
pixmap only when no themed icon is found; when reading pixmaps, keep only the
size closest to the bar's icon edge. Expected impact: tens of MB less transient
allocation per tray change, and a lower RSS peak.

### 5. Producer-thread allocation churn is mostly unattributed

The producer thread allocated 119.7 MB over 120 s and freed 118.6 MB (the
render loop: 14.6 MB). Instrumented functions account for about 41 MB of it.
The rest is in code that is not a measured function — most likely zbus message
decoding in the D-Bus signal streams and the PipeWire/volume callbacks.

Proposed next step: wrap the signal streams and long-lived producer futures in
`hotpath::stream!`/`hotpath::future!` and repeat the allocation run before
choosing a fix.

### 6. The first frame pays for font loading and shaping

The first live frame's layout takes 50–57 ms (`ClockWidget::measure` is the
first text measured, so it absorbs loading the font database), which is most
of the 95–105 ms startup-to-first-commit. Steady-state layout is ~30 µs.

Proposed fix: warm the shared font system (load faces and shape the static
labels) on a thread while the Wayland connection and first configure are
pending. Expected impact: up to ~50 ms off startup-to-first-commit.

### Smaller items

- `sysmon::read_cpu_times` allocates 8 KB per sample (`read_to_string` of
  `/proc/stat`); a reused buffer removes it.
- `ConfigWatcher::poll` allocates ~155 KB in ~900 allocations per poll that
  rescans.
- At startup SNI introspects every bus name (174 here) with a 500 ms timeout
  each; they run concurrently, so tray adoption waits ~0.5 s for unresponsive
  names.

### Non-issues

- Mutex contention: `volume-state` wait p95 0.14–0.31 µs over ~150–220
  acquisitions; the SNI mutexes were never contended.
- Channels: backlight udev events reach the producer in ~30 µs (p95 < 50 µs),
  queue depth never above 1.
- CPU: every thread averaged ≤ 0.2 %; the render loop peaks at 36 % of a core
  for the startup frame.
