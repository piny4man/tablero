use std::fs;
use tablero::config::{Color, Config};

const THEME: &str = include_str!("fixtures/swatches.toml");

#[test]
fn theme_only_changes_reach_output_pixels_and_keep_monitor_overrides() {
    use tablero::render::RenderContext;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let theme = dir.path().join("theme.toml");
    fs::write(&path, "[appearance]\ntheme_file = 'theme.toml'\n[[monitor]]\nname = 'DP-1'\n[monitor.theme]\nbackground = '#AABBCC'\n").unwrap();
    for (value, expected) in [("#10253F", [16, 37, 63, 255]), ("#010203", [1, 2, 3, 255])] {
        fs::write(&theme, THEME.replace("#10253F", value)).unwrap();
        let config = Config::load_for_reload(&path).unwrap();
        for name in ["DP-1", "HDMI-A-1", "DP-2"] {
            let resolved = config.resolve_for_output(Some(name));
            let mut ctx = RenderContext::with_settings(20, 8, resolved.render_settings());
            ctx.fill_background();
            let pixel = if name == "DP-1" {
                [170, 187, 204, 255]
            } else {
                expected
            };
            assert_eq!(&ctx.pixels()[..4], &pixel);
        }
    }
}

fn color(value: &str) -> Color {
    Color::parse_hex(value).unwrap()
}

#[test]
fn legacy_documents_match_direct_deserialization() {
    for text in ["", "height = 30", include_str!("../config.example.toml")] {
        let legacy: Config = toml::from_str(text).unwrap();
        assert_eq!(Config::from_toml_str(text).unwrap(), legacy);
    }
}

#[test]
fn theme_is_merged_before_defaults_and_output_specific_overrides() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("theme.toml"), THEME).unwrap();
    let path = dir.path().join("config.toml");
    let text = r##"
[appearance]
theme_file = "theme.toml"
[theme]
foreground = "#eeeeee"
[bar]
background = "#01020380"
[widget.clock]
foreground = "#040506"
[widget.battery.warn]
foreground = "#070809"
[[monitor]]
name = "DP-1"
[monitor.theme]
accent = "#101112"
[monitor.font]
family = "Explicit Font"
[monitor.widget.clock]
background = "#131415"
"##;
    fs::write(&path, text).unwrap();
    let config = Config::load_from_path(&path).unwrap();
    assert_eq!(config.theme.background, color("#10253F"));
    assert_eq!(config.theme.foreground, color("#eeeeee"));
    assert_eq!(config.theme.accent, color("#80D4FF"));
    assert_eq!(config.font.family.as_deref(), Some("DejaVu Sans Mono"));
    assert_eq!(config.font.size, Config::default().font.size);
    assert_eq!(config.render_settings().background, (1, 2, 3, 128));
    for name in ["DP-1", "HDMI-A-1"] {
        let resolved = config.resolve_for_output(Some(name));
        assert_eq!(resolved.widget.clock.foreground, Some(color("#040506")));
        assert_eq!(
            resolved.widget.battery.warn.foreground,
            Some(color("#070809"))
        );
        assert_eq!(resolved.render_settings().background, (1, 2, 3, 128));
        if name == "DP-1" {
            assert_eq!(resolved.theme.accent, color("#101112"));
            assert_eq!(resolved.font.family.as_deref(), Some("Explicit Font"));
            assert_eq!(resolved.widget.clock.background, Some(color("#131415")));
        } else {
            assert_eq!(resolved.theme.accent, color("#80D4FF"));
            assert_eq!(resolved.widget.clock.background, None);
        }
    }
    // Explicit old defaults must not be mistaken for absence.
    let defaults = Config::default();
    let rgba = |c: Color| {
        let (r, g, b, a) = c.to_rgba();
        format!("#{r:02X}{g:02X}{b:02X}{a:02X}")
    };
    fs::write(&path, format!("[appearance]\ntheme_file = 'theme.toml'\n[theme]\nbackground = '{}'\nforeground = '{}'\naccent = '{}'\n[font]\nfamily = 'Explicit Font'\n", rgba(defaults.theme.background), rgba(defaults.theme.foreground), rgba(defaults.theme.accent))).unwrap();
    let explicit = Config::load_from_path(&path).unwrap();
    assert_eq!(explicit.theme, defaults.theme);
    assert_eq!(explicit.font.family.as_deref(), Some("Explicit Font"));
}

#[test]
fn invalid_selected_theme_and_app_report_source_and_never_default() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let theme = dir.path().join("theme.toml");
    fs::write(&path, "[appearance]\ntheme_file = 'theme.toml'").unwrap();
    for invalid in [
        None,
        Some(""),
        Some("version = 2"),
        Some("nonsense"),
        Some("version = 1\nextra = true"),
    ] {
        if let Some(text) = invalid {
            fs::write(&theme, text).unwrap();
        }
        let error = Config::load_from_path(&path).unwrap_err().to_string();
        assert!(error.contains("config.toml"), "{error}");
        assert!(error.contains("theme.toml"), "{error}");
    }
    fs::write(&theme, THEME.replace("#80D4FF", "#notrgb")).unwrap();
    assert!(
        Config::load_from_path(&path)
            .unwrap_err()
            .to_string()
            .contains("#RRGGBB")
    );
    fs::write(&theme, THEME).unwrap();
    for text in [
        "[appearance]\ntheme_file = ''",
        "[appearance]\ntheme_file = '~user/theme.toml'",
        "[appearance]\nunknown = true",
        "height = 0\n[appearance]\ntheme_file = 'theme.toml'",
        "[appearance]\ntheme_file = 'theme.toml'\n[theme]\nforeground = 'bad'",
        "[appearance]\ntheme_file = 'theme.toml'\n[font]\nunknown = true",
    ] {
        fs::write(&path, text).unwrap();
        assert!(Config::load_from_path(&path).is_err(), "{text}");
    }
    assert!(
        Config::from_toml_str("[appearance]\ntheme_file = 'theme.toml'")
            .unwrap_err()
            .to_string()
            .contains("config file path")
    );
}
