//! Optional machine-specific TDP/fan backends layered on power-profiles-daemon.
//!
//! Detection is silent: machines without a known sysfs driver (Framework,
//! typical desktops, ThinkPads, …) stay a pure PPD client. Writes never go
//! through sysfs from this process; they are delegated to a privileged helper.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::command::{find_in_path, is_bare_name, preflight_on_click, resolve_program};
use crate::widget::{PowerProfile, PowerProfilesState};

const QC71_REL: &str = "devices/platform/qc71_laptop";
const QC71_HELPER_NAMES: &[&str] = &["tablero-qc71-set-mode", "qc71-set-mode"];

/// User-facing settings for the power-profiles producer and command executor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PowerProfilesSettings {
    hardware_control: bool,
    hardware_helper: Option<PathBuf>,
    sys_root: PathBuf,
}

impl Default for PowerProfilesSettings {
    fn default() -> Self {
        Self {
            hardware_control: true,
            hardware_helper: None,
            sys_root: PathBuf::from("/sys"),
        }
    }
}

impl PowerProfilesSettings {
    /// Build settings from the widget table. `hardware_control` defaults on so
    /// a missing key keeps auto-detection; `hardware_helper` is an optional
    /// override of the PATH search.
    pub fn new(hardware_control: bool, hardware_helper: Option<PathBuf>) -> Self {
        Self {
            hardware_control,
            hardware_helper,
            sys_root: PathBuf::from("/sys"),
        }
    }

    /// Whether platform backends may be probed.
    pub fn hardware_control(&self) -> bool {
        self.hardware_control
    }

    /// Configured helper path, if any.
    pub fn hardware_helper(&self) -> Option<&Path> {
        self.hardware_helper.as_deref()
    }

    /// Sysfs root used for detection (`/sys` in production).
    pub fn sys_root(&self) -> &Path {
        &self.sys_root
    }

    /// Probe the configured sysfs root when hardware control is enabled.
    pub fn detect_platform(&self) -> Option<PlatformKind> {
        if !self.hardware_control {
            return None;
        }
        detect_platform(&self.sys_root)
    }

    #[cfg(test)]
    pub(crate) fn with_sys_root(mut self, sys_root: PathBuf) -> Self {
        self.sys_root = sys_root;
        self
    }
}

/// A known platform TDP/fan driver. New variants can be added without changing
/// the PPD client path used on every other machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformKind {
    /// Slimbook (and compatible) `qc71_laptop` sysfs.
    Qc71,
}

impl PlatformKind {
    /// Stable tooltip/log name for this backend.
    pub fn name(self) -> &'static str {
        match self {
            Self::Qc71 => "qc71",
        }
    }
}

/// Numeric triple written to qc71 sysfs by the privileged helper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Qc71Mode {
    /// `performance_mode` (1 saver / 2 balanced / 3 performance).
    pub performance_mode: u8,
    /// `silent_mode` (1 only in power-saver).
    pub silent_mode: u8,
    /// `turbo_mode` (1 only in performance).
    pub turbo_mode: u8,
}

/// Map a power-profiles-daemon profile name onto qc71 sysfs values.
pub fn qc71_mode_for(profile: &str) -> Option<Qc71Mode> {
    match profile {
        "power-saver" => Some(Qc71Mode {
            performance_mode: 1,
            silent_mode: 1,
            turbo_mode: 0,
        }),
        "balanced" => Some(Qc71Mode {
            performance_mode: 2,
            silent_mode: 0,
            turbo_mode: 0,
        }),
        "performance" => Some(Qc71Mode {
            performance_mode: 3,
            silent_mode: 0,
            turbo_mode: 1,
        }),
        _ => None,
    }
}

/// Map a `performance_mode` sysfs value back to a daemon profile name.
pub fn profile_from_performance_mode(raw: &str) -> Option<&'static str> {
    match raw.trim() {
        "1" => Some("power-saver"),
        "2" => Some("balanced"),
        "3" => Some("performance"),
        _ => None,
    }
}

/// Probe well-known sysfs locations under `sys_root`. First match wins so a
/// future `asus-nb-wmi` / `thinkpad_acpi` backend can slot in here.
pub fn detect_platform(sys_root: &Path) -> Option<PlatformKind> {
    if sys_root.join(QC71_REL).join("performance_mode").is_file() {
        return Some(PlatformKind::Qc71);
    }
    None
}

/// Read the hardware profile for a previously detected backend. Missing or
/// unparseable sysfs falls back to `None` so the caller can keep PPD's value.
pub fn read_platform_profile(sys_root: &Path, kind: PlatformKind) -> Option<&'static str> {
    match kind {
        PlatformKind::Qc71 => {
            let path = sys_root.join(QC71_REL).join("performance_mode");
            let raw = std::fs::read_to_string(path).ok()?;
            profile_from_performance_mode(&raw)
        }
    }
}

/// Merge daemon state with an optional hardware reading.
///
/// Hardware wins for the *active* profile when it can be parsed. The daemon's
/// advertised list is kept for click rotation. Machines with neither source
/// yield `None`, matching the existing hidden-widget behaviour.
pub fn overlay_snapshot(
    ppd: Option<PowerProfilesState>,
    hardware: Option<&str>,
    hardware_profile: Option<&str>,
) -> Option<PowerProfilesState> {
    match (ppd, hardware) {
        (None, None) => None,
        (None, Some(name)) => {
            let active = hardware_profile?;
            Some(
                PowerProfilesState::new(active, synthetic_profiles(name))
                    .with_hardware(Some(name.to_string())),
            )
        }
        (Some(state), None) => Some(state),
        (Some(mut state), Some(name)) => {
            if let Some(active) = hardware_profile {
                if !state
                    .profiles()
                    .iter()
                    .any(|profile| profile.name() == active)
                {
                    state = state.ensuring_profile(PowerProfile::new(active, name, name, name));
                }
                state = state.with_active(active);
            }
            Some(state.with_hardware(Some(name.to_string())))
        }
    }
}

fn synthetic_profiles(driver: &str) -> Vec<PowerProfile> {
    ["power-saver", "balanced", "performance"]
        .into_iter()
        .map(|name| PowerProfile::new(name, driver, driver, driver))
        .collect()
}

/// Locate the privileged helper: a configured path, otherwise the default
/// names on `PATH`. Missing helpers are not an error; the caller skips writes.
pub fn resolve_hardware_helper(configured: Option<&Path>, kind: PlatformKind) -> Option<PathBuf> {
    if let Some(path) = configured {
        let resolved = resolve_program(path);
        if is_bare_name(path) && !resolved.is_absolute() {
            return None;
        }
        if preflight_on_click(&resolved).is_ok() {
            return Some(resolved);
        }
        return None;
    }
    default_helper_names(kind)
        .iter()
        .find_map(|name| find_in_path(Path::new(name)))
}

fn default_helper_names(kind: PlatformKind) -> &'static [&'static str] {
    match kind {
        PlatformKind::Qc71 => QC71_HELPER_NAMES,
    }
}

/// Run the privileged helper with the profile name. Never writes sysfs itself.
pub async fn apply_platform_profile(
    profile: &str,
    kind: PlatformKind,
    helper: &Path,
) -> Result<(), String> {
    if !maps_profile(kind, profile) {
        return Ok(());
    }
    let mut command = tokio::process::Command::new(helper);
    command
        .arg(profile)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = command
        .output()
        .await
        .map_err(|error| format!("helper {} failed to spawn: {error}", helper.display()))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        Err(format!(
            "helper {} failed ({})",
            helper.display(),
            crate::command::format_exit_status(output.status)
        ))
    } else {
        Err(format!(
            "helper {} failed ({}): {stderr}",
            helper.display(),
            crate::command::format_exit_status(output.status)
        ))
    }
}

fn maps_profile(kind: PlatformKind, profile: &str) -> bool {
    match kind {
        PlatformKind::Qc71 => qc71_mode_for(profile).is_some(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn profiles() -> Vec<PowerProfile> {
        synthetic_profiles("amd_pstate")
    }

    fn write_qc71(root: &Path, mode: &str) {
        let dir = root.join(QC71_REL);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("performance_mode"), mode).unwrap();
        fs::write(dir.join("silent_mode"), "0").unwrap();
        fs::write(dir.join("turbo_mode"), "0").unwrap();
    }

    #[test]
    fn qc71_mapping_matches_slimbook_definitions() {
        assert_eq!(
            qc71_mode_for("power-saver"),
            Some(Qc71Mode {
                performance_mode: 1,
                silent_mode: 1,
                turbo_mode: 0,
            })
        );
        assert_eq!(
            qc71_mode_for("balanced"),
            Some(Qc71Mode {
                performance_mode: 2,
                silent_mode: 0,
                turbo_mode: 0,
            })
        );
        assert_eq!(
            qc71_mode_for("performance"),
            Some(Qc71Mode {
                performance_mode: 3,
                silent_mode: 0,
                turbo_mode: 1,
            })
        );
        assert_eq!(qc71_mode_for("quiet"), None);
    }

    #[test]
    fn performance_mode_parses_and_ignores_unknown_values() {
        assert_eq!(profile_from_performance_mode("1\n"), Some("power-saver"));
        assert_eq!(profile_from_performance_mode(" 2 "), Some("balanced"));
        assert_eq!(profile_from_performance_mode("3"), Some("performance"));
        assert_eq!(profile_from_performance_mode("0"), None);
        assert_eq!(profile_from_performance_mode("balanced"), None);
    }

    #[test]
    fn detection_is_silent_when_qc71_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(detect_platform(dir.path()), None);
        assert_eq!(
            PowerProfilesSettings::default()
                .with_sys_root(dir.path().to_path_buf())
                .detect_platform(),
            None
        );
    }

    #[test]
    fn detection_requires_the_performance_mode_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(QC71_REL)).unwrap();
        assert_eq!(detect_platform(dir.path()), None);
        write_qc71(dir.path(), "2");
        assert_eq!(detect_platform(dir.path()), Some(PlatformKind::Qc71));
    }

    #[test]
    fn hardware_control_false_skips_detection_even_when_sysfs_exists() {
        let dir = tempfile::tempdir().unwrap();
        write_qc71(dir.path(), "2");
        let settings =
            PowerProfilesSettings::new(false, None).with_sys_root(dir.path().to_path_buf());
        assert_eq!(settings.detect_platform(), None);
    }

    #[test]
    fn read_prefers_performance_mode_and_falls_back_on_garbage() {
        let dir = tempfile::tempdir().unwrap();
        write_qc71(dir.path(), "3\n");
        assert_eq!(
            read_platform_profile(dir.path(), PlatformKind::Qc71),
            Some("performance")
        );
        fs::write(
            dir.path().join(QC71_REL).join("performance_mode"),
            "not-a-mode",
        )
        .unwrap();
        assert_eq!(read_platform_profile(dir.path(), PlatformKind::Qc71), None);
    }

    #[test]
    fn overlay_without_hardware_keeps_ppd_state() {
        let ppd = PowerProfilesState::new("balanced", profiles());
        let out = overlay_snapshot(Some(ppd.clone()), None, None).unwrap();
        assert_eq!(out, ppd);
        assert_eq!(out.hardware(), None);
    }

    #[test]
    fn overlay_prefers_readable_hardware_profile() {
        let ppd = PowerProfilesState::new("balanced", profiles());
        let out = overlay_snapshot(Some(ppd), Some("qc71"), Some("performance")).unwrap();
        assert_eq!(out.active_name(), "performance");
        assert_eq!(out.hardware(), Some("qc71"));
        assert_eq!(out.profiles().len(), 3);
    }

    #[test]
    fn overlay_unreadable_hardware_falls_back_to_ppd_and_still_labels() {
        let ppd = PowerProfilesState::new("balanced", profiles());
        let out = overlay_snapshot(Some(ppd), Some("qc71"), None).unwrap();
        assert_eq!(out.active_name(), "balanced");
        assert_eq!(out.hardware(), Some("qc71"));
    }

    #[test]
    fn overlay_without_ppd_synthesizes_standard_profiles() {
        let out = overlay_snapshot(None, Some("qc71"), Some("balanced")).unwrap();
        assert_eq!(out.active_name(), "balanced");
        assert_eq!(out.hardware(), Some("qc71"));
        assert_eq!(
            out.profiles()
                .iter()
                .map(PowerProfile::name)
                .collect::<Vec<_>>(),
            ["power-saver", "balanced", "performance"]
        );
    }

    #[test]
    fn overlay_without_either_source_is_absent() {
        assert!(overlay_snapshot(None, None, None).is_none());
        assert!(overlay_snapshot(None, Some("qc71"), None).is_none());
    }

    #[test]
    fn overlay_injects_a_missing_hardware_profile_into_the_ppd_list() {
        let ppd = PowerProfilesState::new(
            "balanced",
            vec![PowerProfile::new("balanced", "ppd", "ppd", "ppd")],
        );
        let out = overlay_snapshot(Some(ppd), Some("qc71"), Some("power-saver")).unwrap();
        assert_eq!(out.active_name(), "power-saver");
        assert!(out.profiles().iter().any(|p| p.name() == "power-saver"));
    }

    #[test]
    fn configured_helper_must_exist_and_be_executable() {
        let dir = tempfile::tempdir().unwrap();
        let helper = dir.path().join("tablero-qc71-set-mode");
        fs::write(&helper, "#!/bin/sh\n").unwrap();
        assert_eq!(
            resolve_hardware_helper(Some(&helper), PlatformKind::Qc71),
            None
        );
        let mut perms = fs::metadata(&helper).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&helper, perms).unwrap();
        assert_eq!(
            resolve_hardware_helper(Some(&helper), PlatformKind::Qc71).as_deref(),
            Some(helper.as_path())
        );
        assert_eq!(
            resolve_hardware_helper(Some(Path::new("/no/such/qc71-helper")), PlatformKind::Qc71),
            None
        );
    }

    #[test]
    fn apply_invokes_the_helper_and_never_writes_sysfs() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        write_qc71(dir.path(), "2");
        let marker = dir.path().join("applied");
        let helper = dir.path().join("helper");
        fs::write(
            &helper,
            format!("#!/bin/sh\nprintf '%s' \"$1\" > '{}'\n", marker.display()),
        )
        .unwrap();
        let mut perms = fs::metadata(&helper).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&helper, perms).unwrap();

        runtime
            .block_on(apply_platform_profile(
                "performance",
                PlatformKind::Qc71,
                &helper,
            ))
            .unwrap();
        assert_eq!(fs::read_to_string(&marker).unwrap(), "performance");
        assert_eq!(
            fs::read_to_string(dir.path().join(QC71_REL).join("performance_mode")).unwrap(),
            "2"
        );

        runtime
            .block_on(apply_platform_profile("quiet", PlatformKind::Qc71, &helper))
            .unwrap();
        assert_eq!(fs::read_to_string(&marker).unwrap(), "performance");
    }

    #[test]
    fn helper_script_is_valid_posix_sh() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../contrib/qc71/tablero-qc71-set-mode");
        if !path.is_file() {
            return;
        }
        let status = std::process::Command::new("sh")
            .arg("-n")
            .arg(&path)
            .status()
            .expect("sh");
        assert!(status.success(), "helper failed sh -n: {}", path.display());
    }
}
