use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;

pub const USAGE: &str = "Usage: reel [OPTIONS] [DIRECTORY]";
pub const HELP_TEXT: &str = concat!(
    env!("CARGO_PKG_DESCRIPTION"),
    "\n\n",
    "Usage: reel [OPTIONS] [DIRECTORY]\n\n",
    "Arguments:\n",
    "  [DIRECTORY]  Directory containing media files [default: current directory]\n\n",
    "Options:\n",
    "  -h, --help     Print help\n",
    "  -V, --version  Print version\n",
);
pub const VERSION_TEXT: &str = concat!("reel ", env!("CARGO_PKG_VERSION"), "\n");

#[derive(Debug, Eq, PartialEq)]
pub enum CliAction {
    Run { target_dir: Option<PathBuf> },
    Help,
    Version,
}

#[derive(Debug, Eq, PartialEq)]
pub enum CliError {
    UnknownOption(OsString),
    TooManyDirectories(OsString),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownOption(argument) => {
                write!(formatter, "unknown option '{}'", argument.to_string_lossy())
            }
            Self::TooManyDirectories(argument) => write!(
                formatter,
                "unexpected argument '{}'; only one DIRECTORY may be provided",
                argument.to_string_lossy()
            ),
        }
    }
}

impl std::error::Error for CliError {}

pub fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<CliAction, CliError> {
    let mut target_dir = None;
    let mut parse_options = true;

    for argument in args {
        if parse_options {
            if argument == "--" {
                parse_options = false;
                continue;
            }
            if argument == "--help" || argument == "-h" {
                return Ok(CliAction::Help);
            }
            if argument == "--version" || argument == "-V" {
                return Ok(CliAction::Version);
            }
            if looks_like_option(&argument) {
                return Err(CliError::UnknownOption(argument));
            }
        }

        if target_dir.is_some() {
            return Err(CliError::TooManyDirectories(argument));
        }
        target_dir = Some(PathBuf::from(argument));
    }

    Ok(CliAction::Run { target_dir })
}

fn looks_like_option(argument: &OsStr) -> bool {
    argument != "-" && argument.as_encoded_bytes().starts_with(b"-")
}

pub fn dispatch<I, C, L, O, E>(
    args: I,
    current_dir: C,
    launch: L,
    mut stdout: O,
    mut stderr: E,
) -> ExitCode
where
    I: IntoIterator<Item = OsString>,
    C: FnOnce() -> io::Result<PathBuf>,
    L: FnOnce(PathBuf) -> Result<()>,
    O: Write,
    E: Write,
{
    match parse_args(args) {
        Ok(CliAction::Help) => write_command_output(&mut stdout, &mut stderr, HELP_TEXT),
        Ok(CliAction::Version) => write_command_output(&mut stdout, &mut stderr, VERSION_TEXT),
        Ok(CliAction::Run { target_dir }) => {
            let target_dir = match target_dir {
                Some(target_dir) => target_dir,
                None => match current_dir() {
                    Ok(target_dir) => target_dir,
                    Err(error) => return report_runtime_error(&mut stderr, &error),
                },
            };
            match launch(target_dir) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => report_runtime_error(&mut stderr, &error),
            }
        }
        Err(error) => {
            let _ = writeln!(
                stderr,
                "error: {error}\n\n{USAGE}\n\nFor more information, try '--help'."
            );
            ExitCode::from(2)
        }
    }
}

fn write_command_output(
    stdout: &mut impl Write,
    stderr: &mut impl Write,
    output: &str,
) -> ExitCode {
    match stdout.write_all(output.as_bytes()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(stderr, "Error: could not write command output: {error}");
            ExitCode::FAILURE
        }
    }
}

fn report_runtime_error(stderr: &mut impl Write, error: &dyn fmt::Display) -> ExitCode {
    let _ = writeln!(stderr, "Error: {error}");
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::ffi::OsString;
    use std::io;
    use std::path::PathBuf;
    use std::process::ExitCode;
    use std::rc::Rc;

    use anyhow::anyhow;

    use super::{CliAction, CliError, HELP_TEXT, USAGE, VERSION_TEXT, dispatch, parse_args};

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn no_arguments_runs_in_the_current_directory() {
        assert_eq!(
            parse_args(args(&[])),
            Ok(CliAction::Run { target_dir: None })
        );
    }

    #[test]
    fn one_directory_is_preserved() {
        assert_eq!(
            parse_args(args(&["media"])),
            Ok(CliAction::Run {
                target_dir: Some(PathBuf::from("media")),
            })
        );
        assert_eq!(
            parse_args(args(&["-"])),
            Ok(CliAction::Run {
                target_dir: Some(PathBuf::from("-")),
            })
        );
    }

    #[test]
    fn help_and_version_flags_work_before_or_after_a_directory() {
        for flag in ["--help", "-h"] {
            assert_eq!(parse_args(args(&[flag])), Ok(CliAction::Help));
            assert_eq!(parse_args(args(&["media", flag])), Ok(CliAction::Help));
        }
        for flag in ["--version", "-V"] {
            assert_eq!(parse_args(args(&[flag])), Ok(CliAction::Version));
            assert_eq!(parse_args(args(&["media", flag])), Ok(CliAction::Version));
        }
    }

    #[test]
    fn double_dash_allows_a_flag_shaped_directory() {
        assert_eq!(
            parse_args(args(&["--", "--help"])),
            Ok(CliAction::Run {
                target_dir: Some(PathBuf::from("--help")),
            })
        );
        assert_eq!(
            parse_args(args(&["--"])),
            Ok(CliAction::Run { target_dir: None })
        );
    }

    #[test]
    fn unknown_options_and_extra_directories_are_rejected() {
        assert_eq!(
            parse_args(args(&["--wat"])),
            Err(CliError::UnknownOption(OsString::from("--wat")))
        );
        assert_eq!(
            parse_args(args(&["one", "two"])),
            Err(CliError::TooManyDirectories(OsString::from("two")))
        );
        assert_eq!(
            parse_args(args(&["one", "--", "two"])),
            Err(CliError::TooManyDirectories(OsString::from("two")))
        );
        assert_eq!(
            CliError::UnknownOption(OsString::from("--wat")).to_string(),
            "unknown option '--wat'"
        );
        assert_eq!(
            CliError::TooManyDirectories(OsString::from("two")).to_string(),
            "unexpected argument 'two'; only one DIRECTORY may be provided"
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_directory_is_preserved() {
        use std::os::unix::ffi::OsStringExt;

        let directory = OsString::from_vec(vec![b'm', 0x80]);
        assert_eq!(
            parse_args(vec![directory.clone()]),
            Ok(CliAction::Run {
                target_dir: Some(PathBuf::from(directory)),
            })
        );
    }

    #[test]
    fn help_and_version_output_are_stable() {
        assert_eq!(
            HELP_TEXT,
            concat!(
                "A terminal interface for inspecting, editing, and converting media files\n\n",
                "Usage: reel [OPTIONS] [DIRECTORY]\n\n",
                "Arguments:\n",
                "  [DIRECTORY]  Directory containing media files [default: current directory]\n\n",
                "Options:\n",
                "  -h, --help     Print help\n",
                "  -V, --version  Print version\n",
            )
        );
        assert_eq!(
            VERSION_TEXT,
            format!("reel {}\n", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn dispatch_prints_help_and_version_without_launching() {
        for (flag, expected) in [("--help", HELP_TEXT), ("--version", VERSION_TEXT)] {
            let launched = Rc::new(Cell::new(false));
            let launched_by_closure = Rc::clone(&launched);
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let status = dispatch(
                args(&[flag]),
                || panic!("informational flags must not resolve the current directory"),
                move |_| {
                    launched_by_closure.set(true);
                    Ok(())
                },
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, ExitCode::SUCCESS);
            assert_eq!(stdout, expected.as_bytes());
            assert!(stderr.is_empty());
            assert!(!launched.get());
        }
    }

    #[test]
    fn dispatch_launches_explicit_and_default_directories() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let status = dispatch(
            args(&["media"]),
            || panic!("an explicit directory must not resolve the current directory"),
            |directory| {
                assert_eq!(directory, PathBuf::from("media"));
                Ok(())
            },
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(status, ExitCode::SUCCESS);

        let status = dispatch(
            args(&[]),
            || Ok(PathBuf::from("current")),
            |directory| {
                assert_eq!(directory, PathBuf::from("current"));
                Ok(())
            },
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(status, ExitCode::SUCCESS);
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
    }

    #[test]
    fn dispatch_reports_parse_current_directory_and_launch_errors() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let status = dispatch(
            args(&["--wat"]),
            || panic!("parse errors must not resolve the current directory"),
            |_| panic!("parse errors must not launch the app"),
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(status, ExitCode::from(2));
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            format!(
                "error: unknown option '--wat'\n\n{USAGE}\n\n\
                 For more information, try '--help'.\n"
            )
        );

        let mut stderr = Vec::new();
        let status = dispatch(
            args(&[]),
            || {
                Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "no current directory",
                ))
            },
            |_| panic!("a current-directory error must not launch the app"),
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(status, ExitCode::FAILURE);
        assert_eq!(stderr, b"Error: no current directory\n");

        let mut stderr = Vec::new();
        let status = dispatch(
            args(&["media"]),
            || panic!("an explicit directory must not resolve the current directory"),
            |_| Err(anyhow!("launch failed")),
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(status, ExitCode::FAILURE);
        assert_eq!(stderr, b"Error: launch failed\n");
    }

    struct BrokenWriter;

    impl io::Write for BrokenWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "broken output"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn dispatch_reports_command_output_write_errors() {
        let mut stderr = Vec::new();
        let status = dispatch(
            args(&["--help"]),
            || panic!("help must not resolve the current directory"),
            |_| panic!("help must not launch the app"),
            BrokenWriter,
            &mut stderr,
        );

        assert_eq!(status, ExitCode::FAILURE);
        assert_eq!(
            stderr,
            b"Error: could not write command output: broken output\n"
        );
    }
}
