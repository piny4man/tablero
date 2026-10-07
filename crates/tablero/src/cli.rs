use std::{ffi::OsString, path::PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Run,
    Reload,
    Restart,
    Help,
}

#[derive(Debug)]
pub(crate) struct Cli {
    pub action: Action,
    pub instance: String,
    pub config: Option<PathBuf>,
}

pub(crate) const HELP: &str = "Usage: tablero [reload|restart] [--instance NAME] [--config PATH]\n\nWithout a command, start the bar. Instance defaults to 'default'.\n  reload     Re-read the running instance's config and theme\n  restart    Replace or start the instance in the background\n  --instance NAME  Separate named bar (e.g. dev) in this Wayland session\n  --config PATH    Optional config file; restart retains the running path\n  -h, --help       Show this help\n";

impl Cli {
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Self, String> {
        let mut args = args.into_iter();
        let mut cli = Self {
            action: Action::Run,
            instance: "default".into(),
            config: None,
        };
        let mut named = false;
        let mut command = false;
        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some("-h" | "--help") => {
                    cli.action = Action::Help;
                    return Ok(cli);
                }
                Some("reload" | "restart") if !command => {
                    cli.action = if arg == "reload" {
                        Action::Reload
                    } else {
                        Action::Restart
                    };
                    command = true;
                }
                Some("--instance") if !named => {
                    cli.instance = args
                        .next()
                        .and_then(|v| v.into_string().ok())
                        .ok_or("--instance requires a name")?;
                    if cli.instance.is_empty()
                        || cli.instance.len() > 48
                        || !cli
                            .instance
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
                    {
                        return Err(
                            "instance name must be 1–48 ASCII letters, digits, '-' or '_'".into(),
                        );
                    }
                    named = true;
                }
                Some("--config") if cli.config.is_none() => {
                    let path = args
                        .next()
                        .filter(|v| !v.is_empty())
                        .ok_or("--config requires a path")?;
                    cli.config = Some(path.into());
                }
                _ => return Err(format!("unexpected argument {arg:?}; use --help")),
            }
        }
        Ok(cli)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(args: &[&str]) -> Result<Cli, String> {
        Cli::parse(args.iter().map(std::ffi::OsString::from))
    }
    #[test]
    fn defaults_and_lifecycle_commands() {
        let cli = parse(&[]).unwrap();
        assert_eq!(cli.action, Action::Run);
        assert_eq!(cli.instance, "default");
        assert_eq!(cli.config, None);
        assert_eq!(parse(&["reload"]).unwrap().action, Action::Reload);
        assert_eq!(parse(&["restart"]).unwrap().action, Action::Restart);
        assert_eq!(parse(&["--help"]).unwrap().action, Action::Help);
    }
    #[test]
    fn options_can_surround_commands() {
        let cli = parse(&["--instance", "dev", "restart", "--config", "./dev.toml"]).unwrap();
        assert_eq!(cli.action, Action::Restart);
        assert_eq!(cli.instance, "dev");
        assert_eq!(cli.config, Some("./dev.toml".into()));
    }
    #[test]
    fn malformed_arguments_are_rejected() {
        for args in [
            vec!["unknown"],
            vec!["--instance"],
            vec!["--config"],
            vec!["reload", "restart"],
            vec!["--instance", "../dev"],
            vec!["--instance", ""],
            vec!["--instance", "dev", "--instance", "other"],
            vec!["--config", ""],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
    }
    #[test]
    fn config_paths_need_not_be_utf8() {
        use std::os::unix::ffi::OsStringExt;
        let path = std::ffi::OsString::from_vec(vec![b'/', 255]);
        assert_eq!(
            Cli::parse(["--config".into(), path.clone()])
                .unwrap()
                .config,
            Some(path.into())
        );
    }
}
