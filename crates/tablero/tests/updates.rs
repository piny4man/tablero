//! Integration coverage for the package-update boundary: `anzen updates
//! --json`, and the `checkupdates`/`paru -Qua` fallback when anzen is not
//! installed. Every program is a fake script, so no test touches the real
//! package databases.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use calloop::EventLoop;
use calloop::channel::Event as ChannelEvent;
use tablero::producer::ProducerBridge;
use tablero::updates::{
    UpdatesError, UpdatesProducer, check_fallback, check_once, parse_report, parse_update_lines,
};
use tablero::widget::{Msg, PackageUpdates, UpdatesStatus};

const DEADLINE: Duration = Duration::from_secs(5);

/// A report with every source `ok` and no updates; tests override fields.
fn report(repo: &str, aur: &str, vendor: &str) -> String {
    format!(
        r#"{{"schema":1,"checked_at":1794470400,"total":0,"sources":{{"repo":{repo},"aur":{aur},"vendor":{vendor}}}}}"#
    )
}

const REPO_OK: &str = r#"{"status":"ok","synced_at":1794466800,"stale":false,"updates":[],"error":null,"refreshed":true,"refresh_error":null}"#;
const AUR_OK: &str = r#"{"status":"ok","updates":[],"warnings":[],"error":null}"#;
const VENDOR_OK: &str = r#"{"status":"ok","updates":[],"failed":[],"unchecked":[],"error":null}"#;

fn parse(json: &str) -> PackageUpdates {
    parse_report(json.as_bytes()).unwrap()
}

/// Write an executable `name` that runs `body` under `/bin/sh`.
fn fake_program(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).unwrap();
    path
}

/// Write an executable `anzen` that runs `body` under `/bin/sh`.
fn fake_anzen(dir: &Path, body: &str) -> PathBuf {
    fake_program(dir, "anzen", body)
}

/// A fake `anzen` that prints `json` verbatim.
fn printing_anzen(dir: &Path, json: &str) -> PathBuf {
    let report = dir.join("report.json");
    fs::write(&report, json).unwrap();
    fake_anzen(dir, &format!("cat '{}'", report.display()))
}

fn check(anzen: &Path, deadline: Duration) -> Result<PackageUpdates, UpdatesError> {
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(check_once(anzen, deadline))
}

#[test]
fn a_clean_report_lists_every_source_without_problems() {
    let updates = parse(&report(
        r#"{"status":"ok","synced_at":1794466800,"stale":false,"updates":[{"name":"linux","old":"6.17.1-1","new":"6.17.2-1"}],"error":null,"refreshed":true,"refresh_error":null}"#,
        r#"{"status":"ok","updates":[{"name":"paru-bin","old":"2.0.4-1","new":"2.0.5-1"}],"warnings":[],"error":null}"#,
        r#"{"status":"ok","updates":[{"name":"app-deb","old":"1.0-1","new":"1.1-1"}],"failed":[],"unchecked":["local-tool"],"error":null}"#,
    ));

    assert_eq!(updates.total(), 3);
    assert_eq!(updates.official()[0].name(), "linux");
    assert_eq!(updates.official()[0].current_version(), "6.17.1-1");
    assert_eq!(updates.official()[0].new_version(), "6.17.2-1");
    assert_eq!(updates.aur()[0].name(), "paru-bin");
    assert_eq!(updates.vendor()[0].name(), "app-deb");
    assert!(updates.problems().is_empty());
    assert_eq!(updates.stale_for(), None);
}

#[test]
fn skipped_sources_are_not_problems() {
    let updates = parse(&report(
        REPO_OK,
        r#"{"status":"skipped","updates":[],"warnings":[],"error":null}"#,
        r#"{"status":"skipped","updates":[],"failed":[],"unchecked":[],"error":null}"#,
    ));
    assert!(updates.problems().is_empty());
}

#[test]
fn an_unavailable_aur_is_a_problem_not_zero_updates() {
    let updates = parse(&report(
        REPO_OK,
        r#"{"status":"unavailable","updates":[],"warnings":[],"error":"AUR returned 503"}"#,
        VENDOR_OK,
    ));
    assert_eq!(updates.total(), 0);
    assert_eq!(updates.problems(), ["AUR unavailable: AUR returned 503"]);
}

#[test]
fn a_failed_repository_check_is_a_problem() {
    let updates = parse(&report(
        r#"{"status":"failed","synced_at":null,"stale":null,"updates":[],"error":"pacman failed","refreshed":false,"refresh_error":null}"#,
        AUR_OK,
        VENDOR_OK,
    ));
    assert_eq!(updates.problems(), ["Repositories failed: pacman failed"]);
}

#[test]
fn a_partial_vendor_check_names_each_failed_package_and_keeps_the_rest() {
    let updates = parse(&report(
        REPO_OK,
        AUR_OK,
        r#"{"status":"partial","updates":[{"name":"app-deb","old":"1","new":"2"}],"failed":[{"name":"other-deb","error":"download failed"}],"unchecked":[],"error":null}"#,
    ));
    assert_eq!(updates.vendor().len(), 1);
    assert_eq!(updates.problems(), ["other-deb: download failed"]);
}

#[test]
fn a_failed_refresh_is_a_problem_even_when_the_list_is_ok() {
    let updates = parse(&report(
        r#"{"status":"ok","synced_at":1794466800,"stale":false,"updates":[],"error":null,"refreshed":false,"refresh_error":"no network"}"#,
        AUR_OK,
        VENDOR_OK,
    ));
    assert_eq!(
        updates.problems(),
        ["Refresh failed, showing the last system sync: no network"]
    );
}

#[test]
fn stale_databases_report_their_age_at_check_time() {
    let updates = parse(&report(
        r#"{"status":"ok","synced_at":1794362400,"stale":true,"updates":[],"error":null,"refreshed":false,"refresh_error":null}"#,
        AUR_OK,
        VENDOR_OK,
    ));
    assert_eq!(updates.stale_for(), Some(Duration::from_secs(30 * 3600)));
}

#[test]
fn an_unknown_status_degrades_instead_of_failing() {
    let updates = parse(&report(
        REPO_OK,
        r#"{"status":"throttled","updates":[],"warnings":[],"error":null}"#,
        VENDOR_OK,
    ));
    assert_eq!(updates.problems(), ["AUR in an unknown state"]);
}

#[test]
fn new_fields_are_ignored_within_schema_one() {
    let json = report(REPO_OK, AUR_OK, VENDOR_OK).replacen(
        r#""total":0"#,
        r#""total":0,"future":{"anything":[1,2]}"#,
        1,
    );
    assert!(parse(&json).problems().is_empty());
}

#[test]
fn another_schema_is_rejected_by_number() {
    let json = report(REPO_OK, AUR_OK, VENDOR_OK).replacen(r#""schema":1"#, r#""schema":2"#, 1);
    let error = parse_report(json.as_bytes()).unwrap_err();
    assert!(matches!(error, UpdatesError::Schema(2)), "{error}");
    assert!(error.to_string().contains("schema 2 is not supported"));
}

#[test]
fn human_readable_output_is_rejected_not_scraped() {
    let error = parse_report(b"linux 6.17.1-1 -> 6.17.2-1\n").unwrap_err();
    assert!(matches!(error, UpdatesError::Parse(_)), "{error}");
}

#[test]
fn check_once_runs_anzen_updates_refresh_json() {
    let temp = tempfile::tempdir().unwrap();
    let json = report(REPO_OK, AUR_OK, VENDOR_OK);
    let report_path = temp.path().join("report.json");
    fs::write(&report_path, &json).unwrap();
    let anzen = fake_anzen(
        temp.path(),
        &format!(
            r#"[ "$*" = "updates --refresh --json" ] || {{ echo "unexpected args: $*" >&2; exit 3; }}
cat '{}'"#,
            report_path.display()
        ),
    );

    let updates = check(&anzen, DEADLINE).unwrap();
    assert!(updates.problems().is_empty());
}

#[test]
fn check_once_reads_the_report_from_stdout() {
    let temp = tempfile::tempdir().unwrap();
    let anzen = printing_anzen(
        temp.path(),
        &report(
            REPO_OK,
            r#"{"status":"ok","updates":[{"name":"paru-bin","old":"1","new":"2"}],"warnings":[],"error":null}"#,
            VENDOR_OK,
        ),
    );
    assert_eq!(check(&anzen, DEADLINE).unwrap().total(), 1);
}

#[test]
fn a_failing_anzen_reports_its_status_and_stderr() {
    let temp = tempfile::tempdir().unwrap();
    let anzen = fake_anzen(temp.path(), "echo 'unknown option --refresh' >&2; exit 3");

    let error = check(&anzen, DEADLINE).unwrap_err();
    assert!(matches!(error, UpdatesError::Failed { code: Some(3), .. }));
    assert!(error.to_string().contains("unknown option --refresh"));
}

#[test]
fn a_missing_anzen_is_a_spawn_error() {
    let temp = tempfile::tempdir().unwrap();
    let error = check(&temp.path().join("missing-anzen"), DEADLINE).unwrap_err();
    assert!(matches!(error, UpdatesError::Spawn { .. }), "{error}");
}

#[test]
fn a_timed_out_check_kills_descendant_processes() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("descendant-finished");
    let anzen = fake_anzen(
        temp.path(),
        &format!("(sleep 0.2; touch '{}') & wait", marker.display()),
    );

    let error = check(&anzen, Duration::from_millis(20)).unwrap_err();
    std::thread::sleep(Duration::from_millis(400));

    assert!(error.to_string().contains("timed out"));
    assert!(!marker.exists());
}

/// Run the producer until its first message arrives.
fn first_message(producer: UpdatesProducer) -> Msg {
    let mut event_loop: EventLoop<Option<Msg>> = EventLoop::try_new().unwrap();
    let (bridge, channel) = ProducerBridge::new().unwrap();
    event_loop
        .handle()
        .insert_source(channel, |event, _, seen| {
            if let ChannelEvent::Msg(msg @ Msg::Updates(_)) = event {
                seen.get_or_insert(msg);
            }
        })
        .unwrap();
    bridge.spawn(Box::new(producer));

    let mut seen = None;
    let mut waited = Duration::ZERO;
    while seen.is_none() && waited < DEADLINE {
        event_loop
            .dispatch(Duration::from_millis(20), &mut seen)
            .unwrap();
        waited += Duration::from_millis(20);
    }
    seen.expect("the producer sent no update message")
}

#[test]
fn without_anzen_the_producer_checks_with_checkupdates_and_paru() {
    let temp = tempfile::tempdir().unwrap();
    let checkupdates = fake_program(
        temp.path(),
        "checkupdates",
        "echo 'linux 6.17.1-1 -> 6.17.2-1'",
    );
    let paru = fake_program(temp.path(), "paru", "echo 'paru-bin 2.0.4-1 -> 2.0.5-1'");
    let producer = UpdatesProducer::with_interval(Duration::from_secs(3600))
        .with_program(temp.path().join("missing-anzen"))
        .with_fallback_programs(checkupdates, paru)
        .with_command_timeout(DEADLINE);

    let Msg::Updates(UpdatesStatus::Checked(updates)) = first_message(producer) else {
        panic!("expected a checked report");
    };
    assert_eq!(updates.official()[0].name(), "linux");
    assert_eq!(updates.aur()[0].name(), "paru-bin");
    assert!(updates.problems().is_empty());
}

#[test]
fn without_anzen_or_checkupdates_the_producer_reports_a_failed_check() {
    let temp = tempfile::tempdir().unwrap();
    let producer = UpdatesProducer::with_interval(Duration::from_secs(3600))
        .with_program(temp.path().join("missing-anzen"))
        .with_fallback_programs(
            temp.path().join("missing-checkupdates"),
            temp.path().join("missing-paru"),
        );

    let Msg::Updates(UpdatesStatus::Failed(error)) = first_message(producer) else {
        panic!("expected a failed check");
    };
    assert!(error.contains("missing-checkupdates"), "{error}");
}

#[test]
fn a_failing_anzen_never_falls_back() {
    let temp = tempfile::tempdir().unwrap();
    let anzen = fake_anzen(temp.path(), "echo 'database locked' >&2; exit 1");
    let checkupdates = fake_program(temp.path(), "checkupdates", "exit 2");
    let producer = UpdatesProducer::with_interval(Duration::from_secs(3600))
        .with_program(anzen)
        .with_fallback_programs(checkupdates, temp.path().join("missing-paru"))
        .with_command_timeout(DEADLINE);

    let Msg::Updates(UpdatesStatus::Failed(error)) = first_message(producer) else {
        panic!("expected a failed check, not a silent fallback");
    };
    assert!(error.contains("database locked"), "{error}");
}

#[test]
fn the_producer_sends_a_checked_report() {
    let temp = tempfile::tempdir().unwrap();
    let anzen = printing_anzen(temp.path(), &report(REPO_OK, AUR_OK, VENDOR_OK));
    let producer = UpdatesProducer::with_interval(Duration::from_secs(3600))
        .with_program(anzen)
        .with_command_timeout(DEADLINE);

    let Msg::Updates(UpdatesStatus::Checked(updates)) = first_message(producer) else {
        panic!("expected a checked report");
    };
    assert_eq!(updates, PackageUpdates::default());
}

fn fallback(checkupdates: &Path, paru: &Path) -> Result<PackageUpdates, UpdatesError> {
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(check_fallback(checkupdates, paru, DEADLINE))
}

#[test]
fn update_lines_are_parsed_and_other_lines_skipped() {
    let updates = parse_update_lines(
        b"linux 6.17.1-1 -> 6.17.2-1\n:: Synchronizing package databases...\n\n  mesa 1:25.2-1 -> 1:25.3-1  \n",
    );
    assert_eq!(updates.len(), 2);
    assert_eq!(updates[0].name(), "linux");
    assert_eq!(updates[0].current_version(), "6.17.1-1");
    assert_eq!(updates[0].new_version(), "6.17.2-1");
    assert_eq!(updates[1].name(), "mesa");
    assert_eq!(updates[1].new_version(), "1:25.3-1");
}

#[test]
fn the_fallback_runs_checkupdates_nocolor_and_paru_qua() {
    let temp = tempfile::tempdir().unwrap();
    let checkupdates = fake_program(
        temp.path(),
        "checkupdates",
        r#"[ "$*" = "--nocolor" ] || { echo "unexpected args: $*" >&2; exit 3; }
echo 'linux 6.17.1-1 -> 6.17.2-1'"#,
    );
    let paru = fake_program(
        temp.path(),
        "paru",
        r#"[ "$*" = "-Qua" ] || { echo "unexpected args: $*" >&2; exit 3; }
echo 'paru-bin 2.0.4-1 -> 2.0.5-1'"#,
    );

    let updates = fallback(&checkupdates, &paru).unwrap();
    assert_eq!(updates.official().len(), 1);
    assert_eq!(updates.aur().len(), 1);
    assert!(updates.vendor().is_empty());
    assert!(updates.problems().is_empty());
}

#[test]
fn checkupdates_status_two_means_no_repository_updates() {
    let temp = tempfile::tempdir().unwrap();
    let checkupdates = fake_program(temp.path(), "checkupdates", "exit 2");
    let paru = fake_program(temp.path(), "paru", "exit 1");

    let updates = fallback(&checkupdates, &paru).unwrap();
    assert_eq!(updates, PackageUpdates::default());
}

#[test]
fn a_failing_checkupdates_fails_the_check_instead_of_showing_zero() {
    let temp = tempfile::tempdir().unwrap();
    let checkupdates = fake_program(
        temp.path(),
        "checkupdates",
        "echo 'Cannot fetch updates' >&2; exit 1",
    );
    let paru = fake_program(temp.path(), "paru", "exit 1");

    let error = fallback(&checkupdates, &paru).unwrap_err();
    assert!(
        matches!(error, UpdatesError::Failed { code: Some(1), .. }),
        "{error}"
    );
    assert!(
        error.to_string().contains("Cannot fetch updates"),
        "{error}"
    );
}

#[test]
fn without_paru_the_aur_is_not_checked_and_not_a_problem() {
    let temp = tempfile::tempdir().unwrap();
    let checkupdates = fake_program(temp.path(), "checkupdates", "echo 'linux 1 -> 2'");

    let updates = fallback(&checkupdates, &temp.path().join("missing-paru")).unwrap();
    assert_eq!(updates.official().len(), 1);
    assert!(updates.aur().is_empty());
    assert!(updates.problems().is_empty());
}

#[test]
fn a_failing_paru_keeps_the_repository_list_and_reports_a_problem() {
    let temp = tempfile::tempdir().unwrap();
    let checkupdates = fake_program(temp.path(), "checkupdates", "echo 'linux 1 -> 2'");
    let paru = fake_program(
        temp.path(),
        "paru",
        "echo 'error: AUR request failed' >&2; exit 1",
    );

    let updates = fallback(&checkupdates, &paru).unwrap();
    assert_eq!(updates.official().len(), 1);
    assert!(updates.aur().is_empty());
    assert_eq!(
        updates.problems(),
        ["AUR failed: paru exited with status 1: error: AUR request failed"]
    );
}

#[test]
fn a_timed_out_paru_is_a_problem_not_zero_updates() {
    let temp = tempfile::tempdir().unwrap();
    let checkupdates = fake_program(temp.path(), "checkupdates", "exit 2");
    let paru = fake_program(temp.path(), "paru", "sleep 5");

    let updates = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(check_fallback(
            &checkupdates,
            &paru,
            Duration::from_millis(200),
        ))
        .unwrap();
    assert_eq!(updates.problems().len(), 1);
    assert!(
        updates.problems()[0].starts_with("AUR failed:"),
        "{:?}",
        updates.problems()
    );
    assert!(
        updates.problems()[0].contains("timed out"),
        "{:?}",
        updates.problems()
    );
}
