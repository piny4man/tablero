# Volume widget tied to PipeWire (native wire protocol)

Add a new opt-in `volume` widget to tablero. The widget shows the active output
sink's level (a percentage) and mute state, with the glyph swapping to hint at
the device type (headphones, speakers, HDMI monitor, …). Backed by the native
PipeWire wire protocol via the freedesktop `pipewire` Rust crate.

## Snapshot type & widget (`crates/tablero-core/src/widget/volume.rs`)

- [x] Create `crates/tablero-core/src/widget/volume.rs` with `Volume`, `DeviceKind`, `VolumeWidget` and their unit tests.
- [x] Wire `pub mod volume;` + re-exports in `crates/tablero-core/src/widget/mod.rs`.
- [x] Add a `Msg::Volume(Option<Volume>)` variant in `widget/mod.rs`.

## Config wiring (`crates/tablero-core/src/config.rs`)

- [x] Add `WidgetKind::Volume` and `WidgetStyles { volume, … }` + `get` / `apply` / `build` arms.
- [x] Add three config tests:
  - [x] `volume_is_an_opt_in_widget_name_and_builds`
  - [x] `volume_with_no_on_click_is_display_only`
  - [x] `volume_on_click_folds_onto_a_monitor_override`

## Producer (`crates/tablero-wayland/src/volume.rs`)

- [x] Add `pipewire = "0.9"` to `crates/tablero-wayland/Cargo.toml`.
- [x] Create `crates/tablero-wayland/src/volume.rs` with pure functions (`normalize_volume`, `parse_volume_pod`, `device_kind_from_icon_name`, `device_kind_from_form_factor`, `pick_active_sink_id`) + the `VolumeProducer` running on a dedicated OS thread.
- [x] Unit tests for the pure functions (no live PipeWire).

## Host wiring (`crates/tablero-wayland/src/lib.rs`)

- [x] `pub mod volume;` + import + add `Box::new(VolumeProducer::new())` to the producer list in `run`; update the `run` doc comment.

## Integration test (`crates/tablero-wayland/tests/volume.rs`)

- [x] Create the bluetooth-shaped harness driving pre-built `Msg::Volume` snapshots through a real `ProducerBridge`.

## Docs

- [x] Update `config.example.toml` with the documented widget list comment and a commented `[widget.volume]` block.
- [x] Update `README.md` with the `Volume` bullet, reference-table row, and a verification step.

## Validation

- [x] `cargo build --all-targets`
- [x] `cargo clippy --all-targets --all-features -- -D warnings`
- [x] `cargo fmt --all -- --check`
- [x] `cargo test --all`

## Commit

- [x] One `feat(volume): add PipeWire-backed volume widget` commit covering all phases.
