//! Integration coverage for the Arch package-update command boundary.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use tablero::updates::{check_once, count_output_lines, parse_update_output};

fn executable(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

#[test]
fn output_count_ignores_blank_lines() {
    assert_eq!(count_output_lines(b"linux 1 -> 2\n\nmesa 3 -> 4\n"), 2);
}

#[test]
fn update_output_preserves_package_names_and_versions() {
    let updates = parse_update_output(b"linux 6.15.6.arch1-1 -> 6.15.7.arch1-1\n");

    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].name(), "linux");
    assert_eq!(updates[0].current_version(), "6.15.6.arch1-1");
    assert_eq!(updates[0].new_version(), "6.15.7.arch1-1");
}

#[test]
fn official_and_aur_counts_are_combined() {
    let temp = tempfile::tempdir().unwrap();
    let checkupdates = temp.path().join("checkupdates");
    let paru = temp.path().join("paru");
    executable(&checkupdates, "printf 'linux 1 -> 2\\nmesa 3 -> 4\\n'");
    executable(&paru, "printf 'visual-studio-code-bin 1 -> 2\\n'");

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let updates = runtime
        .block_on(check_once(&checkupdates, &paru, Duration::from_secs(1)))
        .unwrap()
        .unwrap();

    assert_eq!(updates.official().len(), 2);
    assert_eq!(updates.aur().len(), 1);
    assert_eq!(updates.total(), 3);
}

#[test]
fn checkupdates_exit_two_still_allows_aur_updates() {
    let temp = tempfile::tempdir().unwrap();
    let checkupdates = temp.path().join("checkupdates");
    let paru = temp.path().join("paru");
    executable(&checkupdates, "exit 2");
    executable(&paru, "printf 'paru-bin 1 -> 2\\n'");

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let updates = runtime
        .block_on(check_once(&checkupdates, &paru, Duration::from_secs(1)))
        .unwrap()
        .unwrap();

    assert!(updates.official().is_empty());
    assert_eq!(updates.aur().len(), 1);
}

#[test]
fn missing_paru_falls_back_to_official_updates() {
    let temp = tempfile::tempdir().unwrap();
    let checkupdates = temp.path().join("checkupdates");
    executable(&checkupdates, "printf 'linux 1 -> 2\\n'");

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let updates = runtime
        .block_on(check_once(
            &checkupdates,
            &temp.path().join("missing-paru"),
            Duration::from_secs(1),
        ))
        .unwrap()
        .unwrap();

    assert_eq!(updates.official().len(), 1);
    assert!(updates.aur().is_empty());
}

#[test]
fn failed_paru_falls_back_to_official_updates() {
    let temp = tempfile::tempdir().unwrap();
    let checkupdates = temp.path().join("checkupdates");
    let paru = temp.path().join("paru");
    executable(&checkupdates, "printf 'linux 1 -> 2\\n'");
    executable(&paru, "printf 'AUR unavailable' >&2; exit 1");

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let updates = runtime
        .block_on(check_once(&checkupdates, &paru, Duration::from_secs(1)))
        .unwrap()
        .unwrap();

    assert_eq!(updates.official().len(), 1);
    assert!(updates.aur().is_empty());
}

#[test]
fn empty_paru_exit_one_is_a_normal_no_updates_result() {
    let temp = tempfile::tempdir().unwrap();
    let checkupdates = temp.path().join("checkupdates");
    let paru = temp.path().join("paru");
    executable(&checkupdates, "printf 'linux 1 -> 2\\n'");
    executable(&paru, "exit 1");

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let updates = runtime
        .block_on(check_once(&checkupdates, &paru, Duration::from_secs(1)))
        .unwrap()
        .unwrap();

    assert_eq!(updates.official().len(), 1);
    assert!(updates.aur().is_empty());
}

#[test]
fn zero_total_returns_hidden_state() {
    let temp = tempfile::tempdir().unwrap();
    let checkupdates = temp.path().join("checkupdates");
    let paru = temp.path().join("paru");
    executable(&checkupdates, "exit 2");
    executable(&paru, "exit 0");

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let updates = runtime
        .block_on(check_once(&checkupdates, &paru, Duration::from_secs(1)))
        .unwrap();

    assert_eq!(updates, None);
}

#[test]
fn a_failed_official_check_is_not_reported_as_a_partial_total() {
    let temp = tempfile::tempdir().unwrap();
    let checkupdates = temp.path().join("checkupdates");
    let paru = temp.path().join("paru");
    executable(&checkupdates, "printf 'network failure' >&2; exit 1");
    executable(&paru, "printf 'paru-bin 1 -> 2\\n'");

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let error = runtime
        .block_on(check_once(&checkupdates, &paru, Duration::from_secs(1)))
        .unwrap_err();

    assert!(error.to_string().contains("network failure"));
}

#[test]
fn a_hung_official_check_times_out() {
    let temp = tempfile::tempdir().unwrap();
    let checkupdates = temp.path().join("checkupdates");
    executable(&checkupdates, "sleep 1");

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let error = runtime
        .block_on(check_once(
            &checkupdates,
            &temp.path().join("missing-paru"),
            Duration::from_millis(10),
        ))
        .unwrap_err();

    assert!(error.to_string().contains("timed out"));
}

#[test]
fn a_timed_out_check_kills_descendant_processes() {
    let temp = tempfile::tempdir().unwrap();
    let checkupdates = temp.path().join("checkupdates");
    let marker = temp.path().join("descendant-finished");
    executable(
        &checkupdates,
        &format!("(sleep 0.1; touch '{}') & wait", marker.display()),
    );

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let error = runtime
        .block_on(check_once(
            &checkupdates,
            &temp.path().join("missing-paru"),
            Duration::from_millis(10),
        ))
        .unwrap_err();
    std::thread::sleep(Duration::from_millis(200));

    assert!(error.to_string().contains("timed out"));
    assert!(!marker.exists());
}
