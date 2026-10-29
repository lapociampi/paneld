//! paneld: a generic touch panel.
//!
//! The binary has three modes:
//!   paneld             -> run the daemon (not implemented yet)
//!   paneld check [-c]  -> validate a config offline
//!   paneld ctl ...     -> talk to a running daemon over a Unix socket
//!
//! See `docs/architecture.md` and ADR 0007.

use std::path::PathBuf;
use std::process::ExitCode;

const DEFAULT_CONFIG: &str = "/etc/paneld/paneld.conf";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(&args) {
        Ok(Cmd::Help) => {
            print_help();
            ExitCode::SUCCESS
        }
        Ok(Cmd::Version) => {
            println!("paneld {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(Cmd::Run { config }) => run_daemon(config),
        Ok(Cmd::Check { config }) => run_check(config),
        Ok(Cmd::Ctl { args }) => run_ctl(&args),
        Err(e) => {
            eprintln!("paneld: {e}");
            eprintln!("try `paneld --help`");
            ExitCode::from(2)
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Cmd {
    Help,
    Version,
    Run { config: PathBuf },
    Check { config: PathBuf },
    Ctl { args: Vec<String> },
}

fn parse_args(args: &[String]) -> Result<Cmd, String> {
    let mut config: Option<PathBuf> = None;
    let mut i = 0;

    // Flags before the subcommand.
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => return Ok(Cmd::Help),
            "-V" | "--version" => return Ok(Cmd::Version),
            "-c" | "--config" => {
                i += 1;
                let v = args.get(i).ok_or("`--config` needs a path")?;
                config = Some(PathBuf::from(v));
                i += 1;
            }
            s if s.starts_with("--config=") => {
                config = Some(PathBuf::from(&s["--config=".len()..]));
                i += 1;
            }
            s if s.starts_with('-') => return Err(format!("unknown option `{s}`")),
            _ => break,
        }
    }

    let sub = args.get(i).map(String::as_str);
    i += 1;

    match sub {
        None => Ok(Cmd::Run { config: config.unwrap_or_else(default_config) }),
        Some("check") => {
            // `--config` is also accepted after `check`.
            while i < args.len() {
                match args[i].as_str() {
                    "-h" | "--help" => return Ok(Cmd::Help),
                    "-c" | "--config" => {
                        i += 1;
                        let v = args.get(i).ok_or("`--config` needs a path")?;
                        config = Some(PathBuf::from(v));
                        i += 1;
                    }
                    s if s.starts_with("--config=") => {
                        config = Some(PathBuf::from(&s["--config=".len()..]));
                        i += 1;
                    }
                    s if s.starts_with('-') => return Err(format!("unknown option `{s}`")),
                    s => return Err(format!("unexpected argument `{s}` to `check`")),
                }
            }
            Ok(Cmd::Check { config: config.unwrap_or_else(default_config) })
        }
        Some("ctl") => Ok(Cmd::Ctl { args: args[i..].to_vec() }),
        Some(other) if other.starts_with('-') => Err(format!("unknown option `{other}`")),
        Some(other) => Err(format!("unknown subcommand `{other}`")),
    }
}

fn default_config() -> PathBuf {
    PathBuf::from(DEFAULT_CONFIG)
}

fn print_help() {
    println!(
        "paneld {} - a generic touch panel

Usage:
  paneld [--config PATH]              Run the daemon
  paneld check [--config PATH]        Validate a config, offline
  paneld ctl SUBCOMMAND [ARGS...]     Talk to a running daemon

Flags:
  -c, --config PATH   Config file (default: {DEFAULT_CONFIG})
  -h, --help          Show this help
  -V, --version       Show the version",
        env!("CARGO_PKG_VERSION")
    );
}

fn run_daemon(_config: PathBuf) -> ExitCode {
    eprintln!("paneld: daemon not implemented yet");
    ExitCode::from(70)
}

fn run_check(config: PathBuf) -> ExitCode {
    use paneld_config::{check, load};

    let (source, doc, mut diags) = match load(&config) {
        Ok(parts) => parts,
        Err(e) => {
            eprintln!("paneld check: {e}");
            return ExitCode::from(1);
        }
    };
    diags.extend(check(&doc, &source));

    if diags.is_empty() {
        println!("{} ok", config.display());
        return ExitCode::SUCCESS;
    }

    for d in &diags {
        let loc = source.location(d.span);
        eprintln!("{loc}: {}", d.message);
    }
    eprintln!(
        "paneld check: {} issue{} found",
        diags.len(),
        if diags.len() == 1 { "" } else { "s" }
    );
    ExitCode::from(1)
}

fn run_ctl(_args: &[String]) -> ExitCode {
    eprintln!("paneld ctl: not implemented yet");
    ExitCode::from(70)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cmd, String> {
        parse_args(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn no_args_is_run_with_default_config() {
        assert_eq!(parse(&[]), Ok(Cmd::Run { config: default_config() }));
    }

    #[test]
    fn help_short_and_long() {
        assert_eq!(parse(&["-h"]), Ok(Cmd::Help));
        assert_eq!(parse(&["--help"]), Ok(Cmd::Help));
    }

    #[test]
    fn version_short_and_long() {
        assert_eq!(parse(&["-V"]), Ok(Cmd::Version));
        assert_eq!(parse(&["--version"]), Ok(Cmd::Version));
    }

    #[test]
    fn config_before_subcommand() {
        assert_eq!(
            parse(&["-c", "/tmp/x.conf", "check"]),
            Ok(Cmd::Check { config: PathBuf::from("/tmp/x.conf") })
        );
        assert_eq!(
            parse(&["--config=/tmp/x.conf", "check"]),
            Ok(Cmd::Check { config: PathBuf::from("/tmp/x.conf") })
        );
    }

    #[test]
    fn config_after_check_subcommand() {
        assert_eq!(
            parse(&["check", "--config", "/tmp/x.conf"]),
            Ok(Cmd::Check { config: PathBuf::from("/tmp/x.conf") })
        );
        assert_eq!(
            parse(&["check", "--config=/tmp/x.conf"]),
            Ok(Cmd::Check { config: PathBuf::from("/tmp/x.conf") })
        );
    }

    #[test]
    fn help_after_check_is_help() {
        assert_eq!(parse(&["check", "--help"]), Ok(Cmd::Help));
    }

    #[test]
    fn check_rejects_unexpected_positional_args() {
        assert!(parse(&["check", "garbage"]).is_err());
        assert!(parse(&["check", "foo", "bar"]).is_err());
    }

    #[test]
    fn check_rejects_unknown_flags() {
        let err = parse(&["check", "--nope"]).unwrap_err();
        assert!(err.contains("unknown option"), "got: {err}");
    }

    #[test]
    fn unknown_subcommand_error() {
        let err = parse(&["banana"]).unwrap_err();
        assert!(err.contains("unknown subcommand"), "got: {err}");
    }

    #[test]
    fn unknown_global_flag_error() {
        let err = parse(&["--whatever"]).unwrap_err();
        assert!(err.contains("unknown option"), "got: {err}");
    }

    #[test]
    fn ctl_passes_through_trailing_args() {
        assert_eq!(
            parse(&["ctl", "status"]),
            Ok(Cmd::Ctl { args: vec!["status".into()] })
        );
        assert_eq!(
            parse(&["ctl", "set", "foo=1", "bar=2"]),
            Ok(Cmd::Ctl { args: vec!["set".into(), "foo=1".into(), "bar=2".into()] })
        );
    }

    #[test]
    fn config_without_value_is_error() {
        assert!(parse(&["-c"]).is_err());
        assert!(parse(&["check", "-c"]).is_err());
    }
}
