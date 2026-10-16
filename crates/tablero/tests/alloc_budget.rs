//! Allocation budgets for paths the hotpath profile found expensive.
//!
//! Compiled only with `--features hotpath-alloc` (as CI runs it): each test
//! runs the path under a profiling guard and reads the per-function allocated
//! bytes back from the JSON report.
#![cfg(feature = "hotpath-alloc")]

use std::path::Path;

use zbus::zvariant::Value;

// `#[hotpath::main]` installs this for the bar; a test binary declares it.
#[global_allocator]
static ALLOCATOR: hotpath::CountingAllocator = hotpath::CountingAllocator::new();

/// Run `body` under a hotpath guard and return the bytes `function` (a
/// suffix of its qualified name) allocated, excluding nested measured calls.
fn allocated_bytes(function: &str, body: impl FnOnce()) -> u64 {
    let dir = tempfile::tempdir().unwrap();
    let report = dir.path().join("report.json");
    let guard = hotpath::HotpathGuardBuilder::new("alloc_budget")
        .format(hotpath::Format::Json)
        .output_path(&report)
        .functions_limit(0)
        .build();
    body();
    drop(guard);
    read_total(&report, function)
}

fn read_total(report: &Path, function: &str) -> u64 {
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(report).unwrap()).unwrap();
    let rows = json["functions_alloc"]["data"]
        .as_array()
        .expect("alloc section");
    let row = rows
        .iter()
        .find(|row| {
            row["name"]
                .as_str()
                .is_some_and(|name| name.ends_with(function))
        })
        .unwrap_or_else(|| panic!("{function} missing from report:\n{json:#}"));
    parse_size(row["total"].as_str().expect("total is a size"))
}

/// Parse hotpath's human-readable size (`"770 B"`, `"6.0 KB"`, `"32.5 MB"`).
fn parse_size(text: &str) -> u64 {
    let (number, unit) = text.split_once(' ').expect("value and unit");
    let number: f64 = number.parse().expect("numeric size");
    let scale = match unit {
        "B" => 1.0,
        "KB" => 1024.0,
        "MB" => 1024.0 * 1024.0,
        "GB" => 1024.0 * 1024.0 * 1024.0,
        other => panic!("unknown unit {other}"),
    };
    (number * scale) as u64
}

/// A 256x256 pixmap, the size the profiled tray item sent, decodes in a small
/// multiple of its own size. Decoded through dynamic values it cost ~66 MB.
#[test]
fn an_icon_pixmap_reply_decodes_in_proportion_to_its_size() {
    let raw = 256 * 256 * 4;
    let pixmaps = vec![(256_i32, 256_i32, vec![0x7f_u8; raw])];
    let reply = zbus::Message::method_call("/StatusNotifierItem", "Get")
        .unwrap()
        .build(&Value::from(pixmaps.clone()))
        .unwrap();

    let bytes = allocated_bytes("decode_icon_pixmap", || {
        let decoded = tablero::sni::decode_icon_pixmap(&reply).expect("decodes");
        assert_eq!(decoded, pixmaps);
    });
    assert!(
        bytes < 4 * raw as u64,
        "decoding a {raw}-byte pixmap allocated {bytes} bytes"
    );
}
