use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use tablero_core::config::Config;
use tablero_wayland::run;

fn main() -> ExitCode {
    env_logger::init();

    let config = match load_config() {
        Ok(config) => config,
        Err(e) => {
            // A present-but-invalid config is fatal: surface the error rather
            // than silently falling back to defaults (see `load_config`).
            eprintln!("tablero: {e}");
            return ExitCode::FAILURE;
        }
    };

    match run(config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tablero: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Load the bar configuration from the user's TOML file, falling back to the
/// built-in defaults when the file is absent.
///
/// A missing file is not an error — the bar runs on documented defaults. A file
/// that exists but fails to parse *is* an error and is returned to the caller so
/// a typo is reported loudly instead of silently reverting to defaults.
fn load_config() -> Result<Config, Box<dyn Error>> {
    match config_path() {
        Some(path) => Ok(Config::load_from_path(path)?),
        None => Ok(Config::default()),
    }
}

/// The config-file path resolved from the environment: `$XDG_CONFIG_HOME` if
/// set, otherwise `$HOME/.config`. `None` when neither is set, so the caller
/// runs on built-in defaults.
fn config_path() -> Option<PathBuf> {
    config_path_from(
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

/// Pure resolver behind [`config_path`].
///
/// Prefers `<xdg>/tablero/config.toml`; an unset or empty `XDG_CONFIG_HOME`
/// falls back to `<home>/.config/tablero/config.toml`. Returns `None` only when
/// there is no home directory to resolve against at all.
fn config_path_from(xdg_config_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    if let Some(xdg) = xdg_config_home.filter(|s| !s.is_empty()) {
        return Some(Path::new(xdg).join("tablero").join("config.toml"));
    }
    let home = home.filter(|s| !s.is_empty())?;
    Some(
        Path::new(home)
            .join(".config")
            .join("tablero")
            .join("config.toml"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_config_home_is_used_when_set() {
        assert_eq!(
            config_path_from(Some("/cfg"), Some("/home/u")),
            Some(PathBuf::from("/cfg/tablero/config.toml"))
        );
    }

    #[test]
    fn falls_back_to_home_config_when_xdg_unset() {
        assert_eq!(
            config_path_from(None, Some("/home/u")),
            Some(PathBuf::from("/home/u/.config/tablero/config.toml"))
        );
    }

    #[test]
    fn empty_xdg_is_ignored_in_favor_of_home() {
        // An exported-but-empty XDG_CONFIG_HOME is treated as unset.
        assert_eq!(
            config_path_from(Some(""), Some("/home/u")),
            Some(PathBuf::from("/home/u/.config/tablero/config.toml"))
        );
    }

    #[test]
    fn no_home_and_no_xdg_resolves_to_no_path() {
        // Nothing to resolve against: the caller uses built-in defaults.
        assert_eq!(config_path_from(None, None), None);
    }
}
