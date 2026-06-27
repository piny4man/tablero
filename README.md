# tablero

A fast, owned Hyprland status bar — a Waybar replacement built on
`wlr-layer-shell` with software (CPU) rendering through shared memory.

## Workspace layout

| Crate              | Responsibility                                                        |
| ------------------ | --------------------------------------------------------------------- |
| `tablero`          | Binary entry point; wires config into the Wayland event loop.         |
| `tablero-core`     | Compositor-independent logic: clock formatting, text render, blit.    |
| `tablero-wayland`  | `wlr-layer-shell` surface, shared-memory buffers, the `calloop` loop. |

## Build & run

```sh
cargo build --all-targets
cargo test --all          # render + blit + clock unit tests (no compositor needed)
cargo run -p tablero      # opens the bar; run from within a Hyprland session
```

Set `RUST_LOG=info` for startup and lifecycle logging:

```sh
RUST_LOG=info cargo run -p tablero
```

## What it does

- Opens a 32px **top-anchored** layer-shell surface spanning the output width,
  with a 32px **exclusive zone** so tiled windows do not overlap it.
- Renders a dark background and a **live local clock** (`HH:MM:SS`) with
  `cosmic-text` + `tiny-skia`, committed through a `wl_shm` ARGB8888 buffer.
- Wakes **only** for clock ticks (a `calloop` timer aligned to the wall-clock
  second), compositor configure events, or shutdown — there is no busy redraw
  loop and no frame-callback feedback cycle.

## Manual verification under Hyprland

The render and blit paths are unit-tested, but surface placement needs a live
compositor. To verify on Hyprland:

1. From inside a Hyprland session, run `RUST_LOG=info cargo run -p tablero`.
2. Confirm a **32px bar appears pinned to the top** of the screen, spanning its
   full width, showing a dark background and a ticking `HH:MM:SS` clock.
3. Confirm the clock **advances once per second** and that the text changes
   exactly on the second boundary (the timer is second-aligned).
4. Confirm tiled windows are **pushed down by 32px** and never render under the
   bar — this proves the exclusive zone is honored.
5. Confirm the process is **idle between ticks**: `top`/`htop` should show ~0%
   CPU for `tablero` while the clock is running (no busy loop).
6. Optionally inspect the surface: `hyprctl layers` should list a `tablero`
   namespace in the `top` layer on the active output.
7. Press the compositor's close path for the layer (or terminate the session)
   and confirm the process exits cleanly via the `closed` handler.
