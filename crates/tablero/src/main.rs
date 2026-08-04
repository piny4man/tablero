use std::error::Error;
use std::process::ExitCode;

use tablero::config::{Config, config_file_path};
use tablero::run;

fn main() -> ExitCode {
    env_logger::init();

    let path = config_file_path();
    let config = match load_config(path.as_deref()) {
        Ok(config) => config,
        Err(e) => {
            // A present-but-invalid config is fatal: surface the error rather
            // than silently falling back to defaults (see `load_config`).
            eprintln!("tablero: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Pass the resolved path so the bar can hot-reload when the file changes.
    match run(config, path) {
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
fn load_config(path: Option<&std::path::Path>) -> Result<Config, Box<dyn Error>> {
    match path {
        Some(path) => Ok(Config::load_from_path(path)?),
        None => Ok(Config::default()),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tablero::config::config_file_path_from;

    #[test]
    fn xdg_config_home_is_used_when_set() {
        assert_eq!(
            config_file_path_from(Some("/cfg"), Some("/home/u")),
            Some(PathBuf::from("/cfg/tablero/config.toml"))
        );
    }

    #[test]
    fn falls_back_to_home_config_when_xdg_unset() {
        assert_eq!(
            config_file_path_from(None, Some("/home/u")),
            Some(PathBuf::from("/home/u/.config/tablero/config.toml"))
        );
    }

    #[test]
    fn empty_xdg_is_ignored_in_favor_of_home() {
        // An exported-but-empty XDG_CONFIG_HOME is treated as unset.
        assert_eq!(
            config_file_path_from(Some(""), Some("/home/u")),
            Some(PathBuf::from("/home/u/.config/tablero/config.toml"))
        );
    }

    #[test]
    fn no_home_and_no_xdg_resolves_to_no_path() {
        // Nothing to resolve against: the caller uses built-in defaults.
        assert_eq!(config_file_path_from(None, None), None);
    }
}
