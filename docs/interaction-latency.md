# Balanced-mode interaction latency

This is the repeatable investigation procedure and the first local baseline for
[NUKE-69](https://linear.app/tiendacables/issue/NUKE-69). It separates work done
inside Tablero from compositor presentation and from the other desktop apps.

## Diagnostic logging

Build and run the release binary with the opt-in logger:

```sh
cargo build -p tablero --release
TABLERO_PERF=1 RUST_LOG=tablero::performance=info \
  ./target/release/tablero 2>tablero-performance.log
```

`TABLERO_PERF` accepts `1`, `true`, `yes`, or `on`. It is disabled by default;
the normal input and render paths do not read the diagnostic clock.

Each line has a stable `metric=... duration_us=...` prefix:

| Metric | Interval |
| --- | --- |
| `startup-to-first-commit` | Wayland/runtime setup after config load to the first buffer commit. |
| `frame-layout` | Dashboard measurement and placement. |
| `frame-draw` | Software painting, including text shaping/rasterization. |
| `frame-blit` | RGBA-to-Wayland ARGB conversion/copy. |
| `frame-render` | Layout + draw + blit. |
| `frame-total` | Render/allocation through Wayland commit. |
| `frame-idle-gap` | Time since this output's previous frame (0 for its first). Also carries the frame's work counters: `text_shapes` (strings shaped from scratch), `text_cache_hits`, `glyph_pixels` (coverage pixels blended). |
| `click-to-command-queue` | Pointer press received through command fan-out. |
| `workspace-input-to-commit` | Workspace press through the matching Hyprland state and Tablero buffer commit. |
| `tooltip-input-to-commit` | Hover event through the tooltip's first buffer commit. |
| `tray-menu-input-to-commit` | Tray press, DBus menu response, popup configure, and first buffer commit. |

A Wayland commit is an application-side lower bound, not proof that a pixel was
presented. Use a 120/240 fps external recording or a compositor presentation
trace for input-to-visible-response and first-presentation measurements.

## A/B procedure

1. Keep the laptop, display mode, scale, config, power source, and compositor
   effects unchanged. Close unrelated CPU-heavy work.
2. Build once in release mode. Use exactly one Tablero process and the same full
   widget config for both profiles.
3. Save the current profile and restore it on exit:

   ```sh
   original=$(powerprofilesctl get)
   trap 'powerprofilesctl set "$original"' EXIT
   ```

4. For each of `performance` and `balanced`, run
   `powerprofilesctl set <profile>`, wait five seconds, then record at least 10
   repetitions of each interaction:
   - Hyprburst: cold open; warm open; enter the fixed query `firefox`; move the
     selection down once; activate it. `hyprburst --measure` supplies only the
     cold-open comparison, so record the other steps externally.
   - Tablero: move onto a tooltip widget from outside the bar; click an inactive
     workspace and return; click the power-profile widget only after the sample
     set if changing it would invalidate the run.
   - Crabture: open the overlay; drag the same fixed area; cancel rather than
     writing a screenshot.
5. Keep raw samples. Report median, p95, range, sample count, and balanced-mode
   change from performance. Discard only a declared warm-up sample, never an
   unexplained outlier.
6. Repeat once with Hyprland blur and animations disabled. If application-side
   commit timings stay flat but visible response improves, investigate the
   compositor. If `frame-layout`/`frame-draw` grow before commit, the application
   is already blocking independently of compositor presentation.

Useful environment capture:

```sh
uname -a
lscpu | grep -E 'Model name|CPU\(s\)|Thread|Core|MHz'
hyprctl monitors -j
hyprctl -j getoption animations:enabled
hyprctl -j getoption decoration:blur:enabled
Hyprland --version
powerprofilesctl get
upower -i "$(upower -e | grep BAT | head -1)"
```

## Baseline: 2026-09-20

Environment:

- AMD Ryzen 7 8845HS (8 cores/16 threads), 60 GiB RAM, Linux 6.18.52-1-lts.
- Internal 2880×1800 panel at 120 Hz and 1.5 compositor scale. Tablero therefore
  rendered a 3840×76 physical buffer for the configured 38 logical-pixel bar.
- Hyprland 0.56.2, animations enabled, blur enabled, hardware cursor enabled.
- Battery fully charged; the same session and AC state were used for both runs.
- Tablero 0.4.1 at `e3a26a3`, release profile, full daily widget config.
- Installed Hyprburst and Crabture binaries were built 2026-09-13 but expose no
  version flag. Their exact source revisions remain unverified.

### Results

| Measurement | Performance | Balanced | Change |
| --- | ---: | ---: | ---: |
| Hyprburst cold open, median of 5 (`--measure`) | 80.939 ms | 84.125 ms | +3.9% |
| Tablero steady `frame-layout`, median of 3 | 0.186 ms | 22.720 ms | +12,115% |
| Tablero steady `frame-draw`, median of 3 | 15.116 ms | 35.727 ms | +136.4% |
| Tablero steady `frame-blit`, median of 3 | 0.246 ms | 0.247 ms | +0.4% |
| Tablero steady `frame-render`, median of 3 | 15.493 ms | 59.060 ms | +281.2% |
| Tablero steady `frame-render`, observed range | 15.358–15.560 ms | 58.356–59.473 ms | — |

The Tablero samples are periodic system-widget redraws after startup; the first
periodic sample was declared warm-up and discarded. The diagnostic process was
run beside the daily bar, so repeat with one process before treating the numbers
as release benchmarks. The large, stable profile delta is nevertheless inside
Tablero's CPU work and occurs before a Wayland commit.

At 120 Hz one refresh interval is 8.33 ms. The measured full-bar render occupies
about 1.9 intervals in performance mode and 7.1 in balanced mode. Layout measures
every widget on every visible update, and draw repaints every widget; those two
phases account for the regression. The near-identical blit times argue against
buffer conversion or compositor blur as the primary cause of this Tablero
regression. A blocked synchronous render loop also delays pointer handling that
arrives during a redraw.

Hyprburst cold opening regressed only 3.9% in this sample and does not explain a
large perceived launcher delay. Search, selection, warm-toggle, and
first-presentation latency were not reported by its current `--measure` output
and remain unresolved. Crabture has no equivalent measurement mode, so its
opening and area-selection comparison also remains unresolved. The next needed
measurement is the external input-to-presentation run in step 4, correlated with
these application-side metrics.

## Frame-cost fixes: 2026-09-20

Same hardware, config, and 3840×76 buffer as the baseline, measured offscreen
with the `frame-cost` example so both revisions paint identical input with no
second bar on screen:

```sh
cargo run --release -p tablero --example frame-cost -- ~/.config/tablero/config.toml 2 500
```

The third argument sleeps 500 ms between frames, so every sample is the cold
burst after idle that a live bar paints, not a clocked-up tight loop. 40 frames
per cell (the cold first frame excluded), `frame-render` median / p95 in ms:

| Profile | `main` (`3f2c0ad`) | This change | p95 speed-up |
| --- | ---: | ---: | ---: |
| performance | 9.110 / 11.456 | 1.214 / 1.527 | 7.5× |
| balanced | 12.654 / 14.947 | 2.052 / 2.708 | 5.5× |
| power-saver | 30.823 / 32.103 | 3.304 / 3.496 | 9.2× |

Back to back (no idle, 200 frames) the change renders in 0.89 ms median in
performance and 1.12 ms in power-saver, against 8.86 ms and 11.27 ms.

What changed, in order of effect:

- Glyph coverage is blended straight into the pixmap. It used to be one
  `tiny_skia::fill_rect` — a paint and a raster pipeline — per coverage pixel,
  transparent ones included: text was ~6.6 ms of the 8.6 ms draw and is now
  ~0.01 ms.
- `tiny-skia`, `cosmic-text`, `swash`, `zeno` and `harfrust` build at
  `opt-level = 3` while the rest of the graph stays at `"z"`. Render 2.21 →
  0.98 ms for +322 KB of binary (4.98 → 5.30 MB).
- Shaped strings are cached per render context (bounded, dropped on a font
  change), so layout and draw share one shaping and an unchanged label is never
  re-shaped: steady `frame-layout` 0.028 → 0.003 ms, and only the strings that
  actually changed show up in `text_shapes`.
- Built-in icons are rasterised once per (icon, size, color) and composited:
  0.13 → ~0.05 ms per frame.

The remaining ~0.6 ms of draw is the background fill and the widgets' pill
fills and strokes. The first frame after start costs 45–60 ms (font faces load
and every label shapes from cold); that is startup, not interaction latency.

The probe does not exercise the compositor or the input path. The live-bar
`TABLERO_PERF` run over at least 30 samples per profile is still required to
close the acceptance criteria below; `frame-idle-gap` now lets those samples be
split into cold and warm frames.

## Follow-up acceptance criteria

Use these for targeted fixes rather than changing animation or compositor
settings without evidence:

- Tablero, same hardware/full config/balanced mode: steady `frame-render` p95 at
  or below 8.33 ms, with `workspace-input-to-commit` and
  `tooltip-input-to-commit` p95 at or below 16.67 ms.
- Tablero balanced-mode interaction p95 no more than 20% slower than performance
  mode over at least 30 samples.
- Hyprburst cold and warm first presentation p95 no more than 10% slower in
  balanced mode; fixed-query and selection response p95 below 50 ms.
- Crabture overlay first presentation and selection feedback p95 below 50 ms and
  no more than 20% slower in balanced mode.
- Re-run with blur/animations both enabled and disabled. Application acceptance
  must pass with effects enabled; a compositor-only delta should be filed and
  measured separately.
