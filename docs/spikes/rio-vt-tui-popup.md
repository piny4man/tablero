# Spike: TUI programs in a floating panel with rio-vt

**Status:** throwaway spike. It is `crates/tablero/examples/tui-popup.rs`, and the bar is unchanged. `rio-vt` is only a dev-dependency.

## Question

Can a bar widget's click open a TUI program (btm, nmtui, dnsglobe) in an embedded terminal panel, using Rio's terminal engine and no external terminal emulator?

**Answer: yes, for rendering.** All three programs render correctly. Interactive input still has to be checked by hand (see "Not verified").

## How it works

```text
on-click = ["/…/target/release/examples/tui-popup", "btm"]
```

- **Window:** a wlr-layer-shell surface on the overlay layer with no anchors, so Hyprland centers it. It takes exclusive keyboard focus and is 60% × 60% of the output, at the output's integer scale.
- **Engine:** `rio-vt` 0.5.29 (MIT) with its default `pty` feature.
  - `teletypewriter::create_pty_with_spawn` starts the program on a PTY that is sized before spawning. The program gets `TERM=xterm-256color` and `COLORTERM=truecolor`.
  - `performer::Machine` runs rio-vt's own reader thread, which parses into an `Arc<FairMutex<Crosswords>>`.
  - rio-vt's `EventListener` forwards events to a calloop channel: damage, PTY replies (DA/DSR), OSC color queries, size queries, child exit and close.
- **Rendering:** the example does this itself, because rio-vt has no CPU renderer.
  - On each damage event it copies only the damaged lines, plus the old and new cursor rows, while holding the lock.
  - It paints cell backgrounds and cosmic-text/swash glyphs into an ARGB buffer, then calls `damage_buffer` on just those rows.
  - It supports named, 256 and truecolor colors, OSC 4/10/11 overrides, bold-as-bright, dim, inverse, hidden, underline and strikeout, and block, beam and underline cursors.
  - New paints wait for the frame callback.
- **Input:** the example encodes everything, because rio-vt has no key encoder.
  - xterm keys (cursor/app-cursor mode, modified arrows, F1–F12, Ctrl → C0 codes, Alt → ESC prefix).
  - SGR mouse (press, release, drag/motion, wheel) when the program enables it.
  - When mouse reporting is off, the wheel scrolls the scrollback, or sends arrow keys on the alt screen.
- **Lifecycle:** the panel closes when the program exits. Ctrl+Shift+Q unmaps the panel and sends `Msg::Shutdown`, and teletypewriter then hangs up, kills and reaps the child. Each click starts a fresh process.

## User configuration

- **App configs (`~/.config/<app>`):** the program inherits the bar's environment (`HOME`, `XDG_CONFIG_HOME`, `USER`, `PATH`), and teletypewriter sets `HOME` and `USER` again. Each TUI therefore reads its usual config, for example btm reads `~/.config/bottom/bottom.toml`. Only `TERM=xterm-256color` and `COLORTERM=truecolor` are added.
- **Rio's config:** read from `$RIO_CONFIG_HOME`, else `$XDG_CONFIG_HOME/rio`, else `~/.config/rio`, from the file `config.toml`.
  - `theme = "<name>"` loads `themes/<name>.toml`, and its `[colors]` win over the config's own `[colors]`, as in Rio. Colors are parsed with rio-vt's own `Colors` type and expanded with `term::List`, so the defaults, dim and light colors, and the 256-color cube match Rio. rio-vt's `List` never fills the cursor entry, so the example sets it from `colors.cursor`.
  - `[cursor] shape` (block, underline or beam).
  - `[fonts] size`, and `[fonts] family` or `[fonts.regular] family`, used as the monospace face.
  - A missing or malformed file, or a missing theme, falls back to the built-in look.
  - Ignored: opacity, blur, padding, cursor blinking, the kitty keyboard protocol, and key bindings.

## Results

These were measured on Hyprland at 2560×1440 with scale 1, giving a 2304×1440 px panel and a 134×40 grid, using a release build (`opt-level = "z"`; tiny-skia, cosmic-text and swash at opt-level 3). Each program ran for 5 s.

| Program  | First frame | First frame with output | Frames | Paint p50 / p95 / max   | Lines per frame | Output  |
|----------|------------:|------------------------:|-------:|-------------------------|----------------:|--------:|
| btm      | 64 ms       | 72 ms                   | 11     | 4.3 / 19.1 / 19.1 ms    | 25 of 40        | 2.8 KB/s |
| nmtui    | 74 ms       | 87 ms                   | 3      | 8.0 / 8.5 / 8.5 ms      | 40 of 40        | 0.4 KB/s |
| dnsglobe | 61 ms       | 61 ms                   | 60     | 2.5 / 5.8 / 43.7 ms     | 13 of 40        | 17 KB/s  |

- Loading the fonts (`FontSystem::new`) takes about 50 ms, which is most of the time to first frame. The bar already has a shared font set, so an in-bar version would avoid it.
- The first frames include glyph rasterization. Frames whose glyphs are already cached paint in 2–5 ms.
- Rendering is correct for:
  - btm: box drawing, graphs, braille, colored tables;
  - nmtui: newt dialog, shadow, palette backgrounds;
  - dnsglobe: emoji, braille globe, truecolor, flags.
- A shell test showed truecolor, bold red, inverse and underline drawn correctly.

## Not verified

- **Keyboard, mouse and Ctrl+Shift+Q.** This machine has no input injector (no wtype or ydotool), so the encoders are covered only by unit tests (12 tests: keys, SGR mouse, grid sizing, close chord). They have not been tried interactively.
- **Starting it from a real bar `on-click`.** Doing so needs a local config change.
- HiDPI with fractional scale. The example uses the integer `wl_output` scale only, which renders 1.33× outputs at scale 1.
- Kitty keyboard protocol, bracketed paste, clipboard, selection, sixel/kitty images and wide-glyph shaping beyond single codepoints. These are all out of scope.

## Findings and gotchas

- **rio-vt is a good fit for the engine.**
  - It handles the parser, grid, scrollback, damage tracking and PTY thread.
  - It needs no renderer or font dependencies when used with `default-features` (just `pty`).
  - `Crosswords::new(size, CursorShape::Block, listener, WindowId::from(0), 0, scrollback)` works as planned.
- **The consumer must clear `Crosswords::damage_event_in_flight`.** The `Machine` sends only one `TerminalDamaged` until that flag is cleared, so if the consumer never clears it, the panel freezes after the first frame.
- `Crosswords::damage()` returns `TermDamage::Partial` with line indices. `reset_damage()` must be called after copying the damaged lines.
- **rio-vt has no input encoding** (no key or mouse encoder is exported). An embedder has to write one, at roughly 100 lines for xterm-level keys.
- **When the child exits, rio-vt logs `PTY reader failed: Input/output error (os error 5)`** and reports `ChildExited(None)`, so the exit status is lost. On Linux that EIO is just the PTY closing. It is harmless, but it is noisy, and it does mean the exit code is not available.
- To count output bytes, the example wraps `Pty` in a `ProcessReadWrite`/`EventedPty` adapter. All of that is safe code, so it fits the workspace's `unsafe_code = "deny"`.

## If this becomes a feature

1. Run the panel in-process from the bar: a new `Command` variant, the shared `FontResources`, and a `LayerSurface` per popup. This would avoid the font load and a separate process.
2. Move the glyph cache and cell painting into `render.rs`, and make row painting incremental on scroll.
3. Do the interactive verification listed under "Not verified" above, and add fractional scale.
