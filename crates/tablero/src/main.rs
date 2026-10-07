mod cli;
mod restart;

use std::error::Error;
use std::process::ExitCode;

use tablero::config::{Config, config_file_path};
use tablero::{
    lifecycle::{Identity, Instance},
    run_instance,
};

fn main() -> ExitCode {
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    env_logger::init();
    let mut startup = None;
    let result = (|| -> Result<(), Box<dyn Error>> {
        if let Some(path) = std::env::var_os(restart::STARTUP_ENV) {
            let stream = UnixStream::connect(path)?;
            stream.set_write_timeout(Some(std::time::Duration::from_secs(1)))?;
            startup = Some(stream);
            // A detached session plus null stdio survives the invoking terminal.
            nix::unistd::setsid()?;
        }
        let cli = cli::Cli::parse(std::env::args_os().skip(1))?;
        execute(cli, startup.as_ref())
    })();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if let Some(stream) = &mut startup {
                let message = format!("ERR {}\n", error.to_string().replace(['\n', '\r'], " "));
                let _ = stream.write_all(message.as_bytes());
            }
            eprintln!("tablero: {error}");
            ExitCode::FAILURE
        }
    }
}

fn execute(
    cli: cli::Cli,
    startup: Option<&std::os::unix::net::UnixStream>,
) -> Result<(), Box<dyn Error>> {
    if cli.action == cli::Action::Help {
        print!("{}", cli::HELP);
        return Ok(());
    }
    let identity = Identity::current(&cli.instance)?;
    match cli.action {
        cli::Action::Reload => {
            if cli.config.is_some() {
                return Err(
                    "reload uses the running instance's config; use restart --config to change it"
                        .into(),
                );
            }
            if identity.is_stopped()? {
                return Err(format!(
                    "instance '{}' is not running; use tablero restart --instance {}",
                    cli.instance, cli.instance
                )
                .into());
            }
            let response =
                tablero::lifecycle::request(&identity, tablero::lifecycle::Operation::Reload)?;
            if let Some(error) = response.error {
                return Err(error.into());
            }
            println!("tablero: instance '{}' reloaded", cli.instance);
            Ok(())
        }
        cli::Action::Restart => {
            restart::restart(&identity, &cli.instance, cli.config)?;
            println!(
                "tablero: instance '{}' ready in the background",
                cli.instance
            );
            Ok(())
        }
        cli::Action::Run => {
            let path = cli
                .config
                .or_else(config_file_path)
                .map(std::path::absolute)
                .transpose()?;
            let config = load_config(path.as_deref())?;
            let mut instance = Instance::claim(identity, path.clone())?;
            if let Some(stream) = startup {
                instance = instance.with_ready_notifier(stream.try_clone()?);
            }
            run_instance(config, path, instance)
        }
        cli::Action::Help => unreachable!(),
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
