//! The per-output bar registry.
//!
//! Tracks which Wayland outputs currently have a bar, keyed by output id, and
//! owns the lifecycle decisions: build a surface the first time an output is
//! seen, tear it down when the output is removed, and ignore repeats. It is
//! pure and synchronous — the actual layer-shell surface is the generic payload
//! `S`, built lazily by the caller from each output's resolved [`Config`] — so
//! the output hotplug path is unit-testable without a live compositor.
//!
//! Hyprland (and `wlr-layer-shell` compositors generally) advertise one
//! `wl_output` per monitor; the app extends the bar from one surface to one per
//! output by driving this registry from the [`OutputHandler`] callbacks.
//!
//! [`OutputHandler`]: smithay_client_toolkit::output::OutputHandler

use std::collections::HashMap;
use std::collections::hash_map::{Values, ValuesMut};

use crate::config::Config;

/// A Wayland output's global registry name.
///
/// Unique and stable for the output's lifetime, so per-output bar state keys on
/// it: the same physical monitor unplugged and replugged gets a fresh id, which
/// is exactly the lifecycle we want to track.
pub type OutputId = u32;

/// The per-output bars the app is showing, keyed by [`OutputId`].
///
/// The payload `S` is whatever the caller needs to hold per output — in the bar
/// that is the layer-shell surface plus its dashboard and render context. This
/// module never touches `S` beyond storing, handing back, and iterating it.
pub struct Outputs<S> {
    base: Config,
    entries: HashMap<OutputId, S>,
}

impl<S> Outputs<S> {
    /// Create an empty registry resolving per-output config against `base`.
    pub fn new(base: Config) -> Self {
        Self {
            base,
            entries: HashMap::new(),
        }
    }

    /// Replace the base config used for new outputs and for
    /// [`config_for`](Self::config_for) resolution (config hot-reload).
    pub fn set_base(&mut self, base: Config) {
        self.base = base;
    }

    /// The shared base configuration every output resolves against.
    pub fn base(&self) -> &Config {
        &self.base
    }

    /// The effective [`Config`] an output named `name` should run on.
    ///
    /// Delegates to [`Config::resolve_for_output`]: a matching `[[monitor]]`
    /// entry is folded onto the base, an unmatched or unnamed output keeps the
    /// global defaults.
    pub fn config_for(&self, name: Option<&str>) -> Config {
        self.base.resolve_for_output(name)
    }

    /// Ensure output `id` has a surface, building one if it is new.
    ///
    /// Idempotent: when the output already has a surface, `build` is **not**
    /// called and the live surface is kept — a repeated `new_output`/configure
    /// for an output we already track must never replace it. `build` receives
    /// the output's resolved config. Returns `true` when a new surface was
    /// built, `false` when the output was already present.
    pub fn ensure(
        &mut self,
        id: OutputId,
        name: Option<&str>,
        build: impl FnOnce(Config) -> S,
    ) -> bool {
        if self.entries.contains_key(&id) {
            return false;
        }
        let config = self.base.resolve_for_output(name);
        self.entries.insert(id, build(config));
        true
    }

    /// Remove output `id`, returning its surface so the caller can tear the
    /// layer surface down.
    ///
    /// Idempotent: returns `None` if the output was not tracked, so a duplicate
    /// `output_destroyed` is harmless and never leaves stale state behind.
    pub fn remove(&mut self, id: OutputId) -> Option<S> {
        self.entries.remove(&id)
    }

    /// The surface for output `id`, if tracked.
    pub fn get(&self, id: OutputId) -> Option<&S> {
        self.entries.get(&id)
    }

    /// The surface for output `id`, if tracked, mutably.
    pub fn get_mut(&mut self, id: OutputId) -> Option<&mut S> {
        self.entries.get_mut(&id)
    }

    /// Every tracked surface — for finding the one a Wayland event targets.
    pub fn values(&self) -> Values<'_, OutputId, S> {
        self.entries.values()
    }

    /// Every tracked surface, mutably — for fanning a message out to all bars.
    pub fn values_mut(&mut self) -> ValuesMut<'_, OutputId, S> {
        self.entries.values_mut()
    }

    /// Whether output `id` currently has a surface.
    pub fn contains(&self, id: OutputId) -> bool {
        self.entries.contains_key(&id)
    }

    /// The number of outputs with a bar.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no output currently has a bar.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in surface: records the height of the config it was built from,
    /// so tests can assert per-output config resolution without a compositor.
    #[derive(Debug, PartialEq, Eq)]
    struct FakeSurface {
        height: u32,
    }

    fn build_from(config: Config) -> FakeSurface {
        FakeSurface {
            height: config.height,
        }
    }

    /// A base config with one monitor override, to exercise resolution.
    fn base() -> Config {
        Config::from_toml_str(
            r#"
            height = 30
            [[monitor]]
            name = "DP-1"
            height = 48
        "#,
        )
        .expect("valid config")
    }

    #[test]
    fn ensure_builds_a_surface_for_a_new_output() {
        let mut outputs: Outputs<FakeSurface> = Outputs::new(base());
        assert!(outputs.ensure(1, Some("DP-1"), build_from));
        assert_eq!(outputs.len(), 1);
        assert!(outputs.contains(1));
    }

    #[test]
    fn set_base_updates_resolution_for_new_outputs() {
        let mut outputs: Outputs<FakeSurface> = Outputs::new(base());
        assert_eq!(outputs.config_for(Some("HDMI-A-1")).height, 30);
        let mut taller = base();
        taller.height = 40;
        outputs.set_base(taller);
        assert_eq!(outputs.base().height, 40);
        assert_eq!(outputs.config_for(Some("HDMI-A-1")).height, 40);
        // Monitor override still wins for DP-1.
        assert_eq!(outputs.config_for(Some("DP-1")).height, 48);
    }

    #[test]
    fn ensure_resolves_the_named_monitors_config() {
        let mut outputs = Outputs::new(base());
        outputs.ensure(1, Some("DP-1"), build_from);
        // DP-1 overrides height to 48.
        assert_eq!(outputs.get(1), Some(&FakeSurface { height: 48 }));
    }

    #[test]
    fn an_unmatched_output_falls_back_to_global_defaults() {
        let mut outputs = Outputs::new(base());
        outputs.ensure(2, Some("HDMI-A-1"), build_from);
        // No [[monitor]] for HDMI-A-1: the base height stands.
        assert_eq!(outputs.get(2), Some(&FakeSurface { height: 30 }));
    }

    #[test]
    fn an_unnamed_output_falls_back_to_global_defaults() {
        let mut outputs = Outputs::new(base());
        outputs.ensure(3, None, build_from);
        assert_eq!(outputs.get(3), Some(&FakeSurface { height: 30 }));
    }

    #[test]
    fn ensure_is_idempotent_for_an_existing_output() {
        let mut outputs = Outputs::new(base());
        assert!(outputs.ensure(1, Some("DP-1"), build_from));
        // A second sighting must not rebuild or replace the live surface.
        let rebuilt = outputs.ensure(1, Some("DP-1"), |_| {
            panic!("build must not be called for an output already tracked")
        });
        assert!(!rebuilt);
        assert_eq!(outputs.len(), 1);
    }

    #[test]
    fn remove_returns_the_surface_and_clears_the_entry() {
        let mut outputs = Outputs::new(base());
        outputs.ensure(1, Some("DP-1"), build_from);
        let removed = outputs.remove(1);
        assert_eq!(removed, Some(FakeSurface { height: 48 }));
        assert!(!outputs.contains(1));
        assert!(outputs.is_empty());
    }

    #[test]
    fn removing_an_unknown_output_is_a_harmless_no_op() {
        let mut outputs: Outputs<FakeSurface> = Outputs::new(base());
        assert_eq!(outputs.remove(99), None);
        // A duplicate destroy after a real one is equally harmless.
        outputs.ensure(1, Some("DP-1"), build_from);
        assert!(outputs.remove(1).is_some());
        assert_eq!(outputs.remove(1), None);
        assert!(outputs.is_empty());
    }

    #[test]
    fn outputs_are_tracked_independently() {
        let mut outputs = Outputs::new(base());
        outputs.ensure(1, Some("DP-1"), build_from);
        outputs.ensure(2, Some("HDMI-A-1"), build_from);
        assert_eq!(outputs.len(), 2);
        // Removing one leaves the other untouched — no stale or cross-cleared state.
        outputs.remove(1);
        assert!(!outputs.contains(1));
        assert_eq!(outputs.get(2), Some(&FakeSurface { height: 30 }));
        assert_eq!(outputs.len(), 1);
    }

    #[test]
    fn values_mut_visits_every_tracked_surface() {
        let mut outputs = Outputs::new(base());
        outputs.ensure(1, Some("DP-1"), build_from);
        outputs.ensure(2, Some("HDMI-A-1"), build_from);
        for surface in outputs.values_mut() {
            surface.height += 1;
        }
        assert_eq!(outputs.get(1).map(|s| s.height), Some(49));
        assert_eq!(outputs.get(2).map(|s| s.height), Some(31));
    }
}
