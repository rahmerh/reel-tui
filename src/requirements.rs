//! The minimum external tool versions `reel` is willing to run against.
//!
//! FFmpeg 8.1 is a hard floor, not a conservative guess. Its `mov` demuxer is the first
//! to read a track title back out of the ISO-BMFF `name` atom (`e59d964a3c`, first
//! released in `n8.1`). On anything older, ffprobe reports MP4/MOV track titles as
//! absent even though they are present in the file, and every consequence of that is
//! silent and destructive:
//!
//! - Remuxing an MP4/MOV **erases every track title**, because the demuxer never hands
//!   the muxer a title to carry over. A plain `-c copy` under FFmpeg 7.1 is enough.
//! - `apply_edits` writes `title=` for a title it read as `None`, so the erasure is
//!   actively muxed rather than merely dropped.
//! - `validate_result` then compares an absent expectation against an absent result and
//!   agrees the output is correct, so the loss passes verification.
//!
//! None of that is fixable from inside `reel`: no FFmpeg flag exposes the atom to a
//! demuxer that does not parse it. Refusing to start is the only honest option.
//!
//! `seconv` and `tesseract` are optional — they gate subtitle conversion and OCR rather
//! than the whole application — so their floors live in `subtitle::ToolCapabilities`
//! and disable individual format choices instead of blocking startup.

use std::fmt;
use std::process::{Command, Stdio};

/// The oldest FFmpeg suite `reel` will run against. See the module docs for why.
pub const MINIMUM_FFMPEG: Version = Version { major: 8, minor: 1 };

/// A `major.minor` version. The patch level never distinguishes a capability `reel`
/// cares about, so it is parsed away rather than stored.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}", self.major, self.minor)
    }
}

/// Why `reel` refused to start.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RequirementError {
    /// The program is not in `PATH`, or would not run.
    Missing { program: &'static str },
    /// The program ran but its banner carried no release version — a git snapshot build
    /// (`ffmpeg version N-119779-g6c291232cf`) is the usual cause.
    Unknown {
        program: &'static str,
        banner: String,
    },
    /// The program ran and is genuinely too old.
    TooOld {
        program: &'static str,
        found: Version,
    },
}

impl fmt::Display for RequirementError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { program } => write!(
                formatter,
                "reel requires FFmpeg {MINIMUM_FFMPEG} or newer, but could not run `{program}`. \
                 Install FFmpeg {MINIMUM_FFMPEG}+ and make sure `{program}` is in your PATH."
            ),
            Self::Unknown { program, banner } => write!(
                formatter,
                "reel requires FFmpeg {MINIMUM_FFMPEG} or newer, but could not determine the \
                 version of `{program}` from: {banner}. Install a released FFmpeg \
                 {MINIMUM_FFMPEG}+ build."
            ),
            Self::TooOld { program, found } => write!(
                formatter,
                "reel requires FFmpeg {MINIMUM_FFMPEG} or newer, but `{program}` is {found}. \
                 Older builds cannot read MP4/MOV track titles, so editing one silently \
                 erases every title in the file. Upgrade FFmpeg to {MINIMUM_FFMPEG} or newer."
            ),
        }
    }
}

impl std::error::Error for RequirementError {}

/// Verifies that both halves of the FFmpeg suite are new enough.
///
/// Both are checked because they are separate binaries and can be resolved from
/// different installs. `ffprobe` is the one whose demuxer actually decides whether a
/// title survives, but a stale `ffmpeg` next to a current `ffprobe` is just as broken
/// and far more confusing to debug.
pub fn check_ffmpeg_suite() -> Result<(), RequirementError> {
    for program in ["ffprobe", "ffmpeg"] {
        check_program(program, || banner(program))?;
    }
    Ok(())
}

fn check_program(
    program: &'static str,
    read_banner: impl FnOnce() -> Option<String>,
) -> Result<(), RequirementError> {
    let banner = read_banner().ok_or(RequirementError::Missing { program })?;
    let found = parse_version(&banner).ok_or_else(|| RequirementError::Unknown {
        program,
        banner: banner.lines().next().unwrap_or_default().trim().to_string(),
    })?;
    if found < MINIMUM_FFMPEG {
        return Err(RequirementError::TooOld { program, found });
    }
    Ok(())
}

fn banner(program: &str) -> Option<String> {
    Command::new(program)
        .arg("-version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Extracts `major.minor` from an FFmpeg banner's first line.
///
/// The third word is the version, and distributions decorate it freely:
/// `n8.1.2` (Arch), `5.0.1-static` (johnvansickle), `6.1.1-3ubuntu5` (Ubuntu),
/// `5.1.6-0+deb12u1` (Debian), `n8.0.1-34-gbfa334de42-20251231` (BtbN). Git snapshots
/// spell it `N-119779-g6c291232cf`, which carries no release at all and so yields
/// `None` rather than a guess.
pub fn parse_version(banner: &str) -> Option<Version> {
    let word = banner.lines().next()?.split_whitespace().nth(2)?;
    let digits = word.strip_prefix('n').unwrap_or(word);
    let mut parts = digits
        .split(|character: char| !character.is_ascii_digit())
        .take_while(|part| !part.is_empty())
        .map(str::parse::<u32>);
    let major = parts.next()?.ok()?;
    // A bare `ffmpeg version 8` is not a shape FFmpeg ships, but treating a missing
    // minor as 0 keeps the comparison total instead of rejecting it as unparseable.
    let minor = parts.next().transpose().ok()?.unwrap_or(0);
    Some(Version { major, minor })
}

#[cfg(test)]
mod tests {
    use kernal::prelude::*;

    use super::{MINIMUM_FFMPEG, RequirementError, Version, check_program, parse_version};

    #[test]
    fn parse_version_should_read_every_shape_distributions_ship() {
        // Arrange: banners collected from the real builds this floor was measured
        // against, so a packaging quirk cannot lock a supported user out.
        let banners = [
            ("ffmpeg version n8.1.2 Copyright (c) 2000-2026", 8, 1),
            ("ffprobe version n8.1.2 Copyright (c) 2007-2026", 8, 1),
            (
                "ffmpeg version 5.0.1-static https://johnvansickle.com/",
                5,
                0,
            ),
            ("ffmpeg version 6.1.1-3ubuntu5 Copyright (c)", 6, 1),
            ("ffmpeg version 5.1.6-0+deb12u1 Copyright (c)", 5, 1),
            (
                "ffmpeg version n8.0.1-34-gbfa334de42-20251231 Copyright",
                8,
                0,
            ),
            ("ffmpeg version 10.20.30 Copyright (c)", 10, 20),
            ("ffmpeg version 8 Copyright (c)", 8, 0),
        ];

        // Act / Assert
        for (banner, major, minor) in banners {
            assert_that!(parse_version(banner)).is_equal_to(Some(Version { major, minor }));
        }

        // Only the first line carries the version; the `built with`/`configuration`
        // lines that follow it are full of digits.
        assert_that!(parse_version(
            "ffmpeg version n7.1.1 Copyright\nbuilt with gcc 16 (GCC)"
        ))
        .is_equal_to(Some(Version { major: 7, minor: 1 }));
    }

    #[test]
    fn parse_version_should_refuse_to_guess_at_an_unversioned_banner() {
        // Arrange / Act / Assert: a git snapshot carries no release, and a truncated or
        // empty banner carries nothing at all. Each has to be unparseable rather than
        // silently reported as some version that then passes or fails the floor.
        assert_that!(parse_version(
            "ffmpeg version N-119779-g6c291232cf Copyright (c)"
        ))
        .is_none();
        assert_that!(parse_version("ffmpeg version")).is_none();
        assert_that!(parse_version("")).is_none();
    }

    #[test]
    fn check_program_should_accept_the_minimum_and_anything_above_it() {
        // Arrange / Act / Assert: the floor itself passes, as do a newer patch, minor,
        // and major.
        for version in ["n8.1", "n8.1.2", "n8.2", "n9.0"] {
            let banner = format!("ffprobe version {version} Copyright (c)");
            assert_that!(check_program("ffprobe", || Some(banner))).is_ok();
        }
    }

    #[test]
    fn check_program_should_reject_missing_unversioned_and_outdated_builds() {
        // Arrange / Act: 8.0 is the nearest release below the floor — the one that
        // reads every other field correctly and only misses the `name` atom.
        let missing = check_program("ffprobe", || None);
        let unknown = check_program("ffmpeg", || {
            Some("ffmpeg version N-119779-g6c291232cf Copyright\nbuilt with gcc\n".to_string())
        });
        let outdated = check_program("ffprobe", || {
            Some("ffprobe version n8.0.1 Copyright (c)".to_string())
        });

        // Assert
        assert_that!(missing).is_equal_to(Err(RequirementError::Missing { program: "ffprobe" }));
        assert_that!(unknown).is_equal_to(Err(RequirementError::Unknown {
            program: "ffmpeg",
            banner: "ffmpeg version N-119779-g6c291232cf Copyright".to_string(),
        }));
        assert_that!(outdated).is_equal_to(Err(RequirementError::TooOld {
            program: "ffprobe",
            found: Version { major: 8, minor: 0 },
        }));
    }

    #[test]
    fn requirement_errors_should_name_the_program_the_floor_and_the_fix() {
        // Arrange / Act
        let missing = RequirementError::Missing { program: "ffprobe" }.to_string();
        let unknown = RequirementError::Unknown {
            program: "ffmpeg",
            banner: "ffmpeg version N-119779".to_string(),
        }
        .to_string();
        let outdated = RequirementError::TooOld {
            program: "ffprobe",
            found: Version { major: 7, minor: 1 },
        }
        .to_string();

        // Assert: every message carries the floor and the offending program, and the
        // too-old one says what silently breaks — without that the refusal reads as
        // gatekeeping rather than as data protection.
        for message in [&missing, &unknown, &outdated] {
            assert_that!(message.as_str()).contains("8.1");
        }
        assert_that!(missing.as_str()).contains("could not run `ffprobe`");
        assert_that!(unknown.as_str()).contains("N-119779");
        assert_that!(outdated.as_str()).contains("`ffprobe` is 7.1");
        assert_that!(outdated.as_str()).contains("silently erases every title");
    }

    #[test]
    fn the_minimum_should_stay_at_the_release_that_reads_mp4_titles() {
        // The floor is load-bearing: `n8.1` is the first release containing FFmpeg
        // commit e59d964a3c, which taught the `mov` demuxer to read the `name` atom.
        // Lowering it reintroduces silent MP4/MOV title erasure, so it is asserted here
        // rather than left to a constant nobody re-reads.
        assert_that!(MINIMUM_FFMPEG).is_equal_to(Version { major: 8, minor: 1 });
        assert_that!(MINIMUM_FFMPEG.to_string().as_str()).is_equal_to("8.1");
    }
}
