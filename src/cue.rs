//! Cue-level subtitle data: parsing SubRip into individual cues, packing overlapping
//! cues into timeline lanes, and mapping cue times onto terminal columns.
//!
//! This is the first place in the crate that looks *inside* a subtitle track. Everything
//! else — `subtitle.rs`, `edit.rs` — treats a subtitle as an opaque stream to convert,
//! tag, or move, and delegates its contents entirely to `ffmpeg` and `seconv`.
//!
//! Deliberately free of any dependency on `App`, `ratatui`, or subprocesses. That is
//! what keeps the parser's many malformed-input branches testable as plain functions
//! over string literals rather than through an application fixture.

use std::path::Path;
use std::time::Duration;

/// One subtitle cue: a span of time and the text shown during it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cue {
    /// Position in the parsed list after sorting, 0-based.
    ///
    /// Deliberately *not* the SRT's own counter line. That counter is advisory, and
    /// files in the wild routinely duplicate it, skip it, restart it mid-file, or omit
    /// it entirely — so anything keyed on it (selection, lane lookup, the preview's
    /// "which cue am I showing") would inherit those defects.
    pub index: usize,
    pub start: Duration,
    pub end: Duration,
    pub text: String,
}

impl Cue {
    /// The moment to grab a preview frame for.
    ///
    /// The midpoint rather than the start: a cue's start frequently coincides with a
    /// scene cut, so the frame at `start` is often the last frame of the *previous*
    /// shot — technically correct and visually useless.
    pub fn midpoint(&self) -> Duration {
        self.start + (self.end.saturating_sub(self.start)) / 2
    }
}

/// Parses SubRip text into cues, discarding anything it cannot make sense of.
///
/// Returns a `Vec` rather than a `Result` on purpose. SubRip is a lenient, widely
/// mangled format, and a file with three good cues and one corrupt block should show
/// three cues rather than an error — the caller's only meaningful branch is
/// "did anything parse at all", which `is_empty` already answers.
///
/// Parsing is driven entirely off the `-->` timing line. Index lines and blank-line
/// separators are treated as advisory, which is what lets the common corruptions
/// (missing separators, absent or wrong counters, stray preamble) fall out correctly
/// instead of each needing its own rule.
pub fn parse_srt(source: &str) -> Vec<Cue> {
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    // No carriage-return trim here: `lines` strips the one CRLF leaves, and the returns a
    // twice-converted file carries are absorbed where they matter anyway — `trim_end` on
    // text, `trim` on the blank-line test, and `split_whitespace` inside the timing parse.
    let lines: Vec<&str> = source.lines().collect();

    let mut cues: Vec<Cue> = Vec::new();
    let mut position = 0;
    while position < lines.len() {
        let Some((start, end)) = parse_timing_line(lines[position]) else {
            position += 1;
            continue;
        };
        position += 1;

        let mut text_lines: Vec<&str> = Vec::new();
        while position < lines.len() {
            let line = lines[position];
            if line.trim().is_empty() || parse_timing_line(line).is_some() {
                break;
            }
            text_lines.push(line.trim_end());
            position += 1;
        }

        // A block that lost its blank-line separator leaves the *next* block's index
        // line trailing this one's text. Whether a bare number is an index or genuine
        // cue text can only be decided here, by what follows it: an index line is
        // immediately followed by a timing line. When the separator is present — the
        // normal case — the loop above stopped on the blank line and this never fires.
        if position < lines.len()
            && parse_timing_line(lines[position]).is_some()
            && text_lines.last().is_some_and(|line| is_index_line(line))
        {
            text_lines.pop();
        }

        cues.push(Cue {
            index: 0,
            start,
            // An end before its start is kept and flattened rather than dropped. A
            // zero-width cue is a real timing defect, and surfacing defects is the
            // entire reason this view exists.
            end: end.max(start),
            text: text_lines.join("\n"),
        });
    }

    cues.sort_by_key(|cue| (cue.start, cue.end));
    for (index, cue) in cues.iter_mut().enumerate() {
        cue.index = index;
    }
    cues
}

/// Reads and parses a `.srt` file.
///
/// Decodes lossily: SubRip carries no encoding declaration and Windows-1252 files are
/// common, so a stray non-UTF-8 byte becomes a replacement character in one cue rather
/// than failing the whole track.
pub fn read_srt(path: &Path) -> std::io::Result<Vec<Cue>> {
    let bytes = std::fs::read(path)?;
    Ok(parse_srt(&String::from_utf8_lossy(&bytes)))
}

fn is_index_line(line: &str) -> bool {
    let line = line.trim();
    !line.is_empty() && line.bytes().all(|byte| byte.is_ascii_digit())
}

/// Parses a `00:00:01,000 --> 00:00:02,000` line into its two timestamps.
///
/// Both halves must parse. Requiring both is what makes it safe to also use this as the
/// "does a new cue start here" test while scanning cue *text* — a line of dialogue
/// containing `-->` has no valid timestamps around it and is left alone.
pub(crate) fn parse_timing_line(line: &str) -> Option<(Duration, Duration)> {
    let (left, right) = line.split_once("-->")?;
    let start = parse_timestamp(left)?;
    // The right half may carry trailing SRT position tags ("X1:040 X2:600 Y1:050
    // Y2:100"), which are layout hints for players and irrelevant to timing.
    let end = parse_timestamp(right.split_whitespace().next()?)?;
    Some((start, end))
}

/// Parses one SubRip timestamp.
///
/// Accepts more than the specification does, because generators emit more than the
/// specification allows: `,` or `.` as the decimal separator, a missing hours field,
/// a missing or short fractional field, and out-of-range components (`00:00:75,000`
/// is an unambiguous 75 seconds).
pub(crate) fn parse_timestamp(token: &str) -> Option<Duration> {
    let token = token.trim();
    let mut groups: Vec<&str> = token.split(':').collect();
    if groups.len() < 2 || groups.len() > 3 {
        return None;
    }

    let last = groups.pop()?;
    let (seconds_text, fraction_text) = match last.split_once([',', '.']) {
        Some((seconds, fraction)) => (seconds, Some(fraction)),
        None => (last, None),
    };

    let mut total: u64 = 0;
    for group in groups {
        total = total.checked_mul(60)?.checked_add(parse_digits(group)?)?;
    }
    total = total
        .checked_mul(60)?
        .checked_add(parse_digits(seconds_text)?)?;

    let milliseconds = match fraction_text {
        Some(fraction) => parse_fraction(fraction)?,
        None => 0,
    };
    Some(Duration::from_millis(
        total.checked_mul(1000)?.checked_add(milliseconds)?,
    ))
}

fn parse_digits(text: &str) -> Option<u64> {
    let text = text.trim();
    // Rejecting non-digits by hand rather than leaning on `parse` alone: `str::parse`
    // accepts a leading `+`, and a signed timestamp is corruption, not a value.
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse::<u64>().ok()
}

/// Reads a fractional-second field as milliseconds, padding short and truncating long.
///
/// `,5` is 500 ms, not 5 ms — the field is a decimal fraction, and reading it as an
/// integer count of milliseconds silently shifts such a cue nearly half a second.
fn parse_fraction(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let digits = text.as_bytes();
    let mut milliseconds = 0;
    for position in 0..3 {
        let digit = digits
            .get(position)
            .map(|byte| u64::from(byte - b'0'))
            .unwrap_or(0);
        milliseconds = milliseconds * 10 + digit;
    }
    Some(milliseconds)
}

/// Renders a cue time for display, to a tenth of a second.
///
/// Tenths rather than milliseconds because the cue list shows one of these per row and
/// three trailing digits of precision no one is reading costs width the text needs.
pub fn format_timestamp(at: Duration) -> String {
    let seconds = at.as_secs();
    format!(
        "{:02}:{:02}:{:02}.{}",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60,
        at.subsec_millis() / 100
    )
}

/// How many rows the timeline track will stack overlapping cues onto before it starts
/// crowding them together.
pub const MAX_LANES: usize = 4;

/// Which timeline row each cue was assigned, and how many rows that needs.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LaneLayout {
    /// Lane per cue, parallel to the cue list.
    pub lanes: Vec<usize>,
    /// Rows the track needs — always at least 1, never more than the cap.
    pub lane_count: usize,
    /// Whether each cue had to share a lane with one it actually overlaps, parallel to
    /// the cue list. Rendered distinctly so a crowded region reads as crowded rather
    /// than as clean adjacent cues.
    pub overflowed: Vec<bool>,
}

/// Assigns overlapping cues to separate timeline lanes.
///
/// Greedy first-fit over start-ordered cues. First-fit rather than least-loaded so an
/// ordinary non-overlapping track stays entirely in lane 0 — "one lane" then falls out
/// of the algorithm instead of needing to be a special case, and the track keeps the
/// single row such a file deserves.
///
/// Cues past the cap are crowded onto the last lane rather than dropped. Dropping a cue
/// from the view whose whole purpose is inspecting cue timing would hide exactly the
/// defect the user came to find.
pub fn pack_lanes(cues: &[Cue], max_lanes: usize) -> LaneLayout {
    let max_lanes = max_lanes.max(1);
    let mut lane_ends: Vec<Duration> = Vec::new();
    let mut lanes = Vec::with_capacity(cues.len());
    let mut overflowed = Vec::with_capacity(cues.len());

    for cue in cues {
        // `<=`, not `<`: a cue starting exactly where the previous one ends does not
        // overlap it and belongs on the same lane. Getting this wrong doubles the lane
        // count of a typical dialogue track, and lane count drives the track's height.
        match lane_ends.iter().position(|end| *end <= cue.start) {
            Some(lane) => {
                lane_ends[lane] = cue.end;
                lanes.push(lane);
                overflowed.push(false);
            }
            None if lane_ends.len() < max_lanes => {
                lane_ends.push(cue.end);
                lanes.push(lane_ends.len() - 1);
                overflowed.push(false);
            }
            None => {
                let lane = max_lanes - 1;
                // Keep the crowded lane's end honest, so a cue arriving after this
                // congestion clears can still claim the lane normally.
                lane_ends[lane] = lane_ends[lane].max(cue.end);
                lanes.push(lane);
                overflowed.push(true);
            }
        }
    }

    LaneLayout {
        lane_count: lane_ends.len().max(1),
        lanes,
        overflowed,
    }
}

/// How much of the timeline the track shows at once.
///
/// A scrolling window rather than the whole file: at whole-file scale a two-second cue
/// in a ninety-minute film occupies a fraction of one cell, so every cue would render
/// as a bare `|` and the track would carry no timing information at all. Across a
/// typical track width this is roughly half a second per column.
pub const WINDOW: Duration = Duration::from_secs(60);

/// The slice of time the timeline track is currently showing, and how wide it is drawn.
///
/// [`TimelineWindow::centered`] is the only constructor the application uses, and it
/// always yields `end > start` — so `column` divides by a non-zero span without needing
/// to guard for it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimelineWindow {
    pub start: Duration,
    pub end: Duration,
    pub width: u16,
}

impl TimelineWindow {
    /// Builds the window shown for a selected cue, clamped to the media's bounds.
    ///
    /// Clamping at both ends matters: a cue two seconds in must not produce a window
    /// starting at minus twenty-eight seconds, which would waste half the track on time
    /// that does not exist.
    pub fn centered(on: &Cue, duration: Duration, width: u16) -> Self {
        // A track can outlast its video — subtitles running past the last frame are
        // common — and the probed duration can be missing entirely. Either way the
        // window still has to contain the cue it was built for.
        let duration = duration.max(on.end).max(Duration::from_millis(1));
        if duration <= WINDOW {
            return Self {
                start: Duration::ZERO,
                end: duration,
                width,
            };
        }
        let start = on
            .midpoint()
            .saturating_sub(WINDOW / 2)
            .min(duration - WINDOW);
        Self {
            start,
            end: start + WINDOW,
            width,
        }
    }

    /// Maps a moment onto a column, or `None` if it falls outside the window.
    pub fn column(&self, at: Duration) -> Option<u16> {
        if self.width == 0 || at < self.start || at > self.end {
            return None;
        }
        let last_column = self.width - 1;
        let span = self.end.saturating_sub(self.start);
        let fraction = (at - self.start).as_secs_f64() / span.as_secs_f64();
        Some(((fraction * f64::from(last_column)).round() as u16).min(last_column))
    }

    /// Maps a cue onto the inclusive column range it occupies.
    ///
    /// Cues reaching past either edge are clamped rather than dropped, since at any
    /// scroll position the cues straddling the edges are the common case.
    ///
    /// The range is never empty. That is a property of the clamping rather than an
    /// extra guard: both ends are pinned inside the window before mapping, and `column`
    /// is monotonic, so `last` cannot land before `first` even for a cue far shorter
    /// than one column.
    pub fn span(&self, cue: &Cue) -> Option<(u16, u16)> {
        if self.width == 0 || cue.end < self.start || cue.start > self.end {
            return None;
        }
        let first = self.column(cue.start.max(self.start))?;
        let last = self.column(cue.end.min(self.end))?;
        Some((first, last))
    }
}

#[cfg(test)]
mod tests {
    use kernal::prelude::*;

    use super::*;

    fn milliseconds(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    fn cue(start: u64, end: u64) -> Cue {
        Cue {
            index: 0,
            start: milliseconds(start),
            end: milliseconds(end),
            text: String::new(),
        }
    }

    fn texts(cues: &[Cue]) -> Vec<&str> {
        cues.iter().map(|cue| cue.text.as_str()).collect()
    }

    #[test]
    fn parse_srt_should_read_a_well_formed_file() {
        // Arrange
        let source = "1\n00:00:01,000 --> 00:00:02,500\nHello\n\n\
                      2\n00:00:03,000 --> 00:00:04,000\nWorld\n\n";

        // Act
        let cues = parse_srt(source);

        // Assert
        assert_that!(cues.len()).is_equal_to(2);
        assert_that!(cues[0].start).is_equal_to(milliseconds(1000));
        assert_that!(cues[0].end).is_equal_to(milliseconds(2500));
        assert_that!(texts(&cues)).contains_exactly_in_given_order(["Hello", "World"]);
    }

    #[test]
    fn parse_srt_should_number_cues_by_position_rather_than_by_their_counter_lines() {
        // Arrange: counters that restart, a habit of files concatenated by hand.
        let source = "7\n00:00:01,000 --> 00:00:02,000\nOne\n\n\
                      7\n00:00:03,000 --> 00:00:04,000\nTwo\n\n";

        // Act
        let cues = parse_srt(source);

        // Assert
        assert_that!(cues.iter().map(|cue| cue.index).collect::<Vec<_>>())
            .contains_exactly_in_given_order([0, 1]);
    }

    /// A `-->` inside dialogue is rare but real, and mistaking it for a timing line
    /// truncates the cue and desynchronises every cue after it.
    #[test]
    fn parse_srt_should_not_treat_an_arrow_inside_cue_text_as_a_timing_line() {
        // Arrange
        let source = "1\n00:00:01,000 --> 00:00:02,000\nleft --> right\nsecond line\n\n";

        // Act
        let cues = parse_srt(source);

        // Assert
        assert_that!(cues.len()).is_equal_to(1);
        assert_that!(cues[0].text.as_str()).is_equal_to("left --> right\nsecond line");
    }

    #[test]
    fn parse_srt_should_end_a_cue_at_the_next_timing_line_when_the_blank_line_is_missing() {
        // Arrange
        let source = "1\n00:00:01,000 --> 00:00:02,000\nHello\n\
                      2\n00:00:03,000 --> 00:00:04,000\nWorld\n";

        // Act
        let cues = parse_srt(source);

        // Assert
        assert_that!(cues.len()).is_equal_to(2);
        assert_that!(texts(&cues)).contains_exactly_in_given_order(["Hello", "World"]);
    }

    /// Back-to-back cues with neither separators nor index lines: the trailing-index
    /// cleanup must leave ordinary text alone when it fires on a non-numeric line.
    #[test]
    fn parse_srt_should_keep_the_last_text_line_when_no_index_precedes_the_next_cue() {
        // Arrange
        let source = "00:00:01,000 --> 00:00:02,000\nHello\n\
                      00:00:03,000 --> 00:00:04,000\nWorld\n";

        // Act
        let cues = parse_srt(source);

        // Assert
        assert_that!(texts(&cues)).contains_exactly_in_given_order(["Hello", "World"]);
    }

    #[test]
    fn is_index_line_should_accept_only_a_bare_number() {
        // Act / Assert
        assert_that!(is_index_line("12")).is_true();
        assert_that!(is_index_line("  7  ")).is_true();
        assert_that!(is_index_line("")).is_false();
        assert_that!(is_index_line("   ")).is_false();
        assert_that!(is_index_line("12a")).is_false();
        assert_that!(is_index_line("-1")).is_false();
    }

    /// A digits-only line is only an index when a timing line follows it; otherwise it
    /// is a line of dialogue that happens to be a number.
    #[test]
    fn parse_srt_should_keep_a_numeric_text_line_that_is_not_followed_by_a_timing_line() {
        // Arrange
        let source = "1\n00:00:01,000 --> 00:00:02,000\nRoom\n237\n\n";

        // Act
        let cues = parse_srt(source);

        // Assert
        assert_that!(cues[0].text.as_str()).is_equal_to("Room\n237");
    }

    #[test]
    fn parse_srt_should_emit_the_last_cue_without_a_trailing_blank_line() {
        // Arrange
        let source = "1\n00:00:01,000 --> 00:00:02,000\nLast";

        // Act
        let cues = parse_srt(source);

        // Assert
        assert_that!(cues.len()).is_equal_to(1);
        assert_that!(cues[0].text.as_str()).is_equal_to("Last");
    }

    #[test]
    fn parse_srt_should_read_a_cue_whose_index_line_is_missing_or_not_a_number() {
        // Arrange
        let source = "00:00:01,000 --> 00:00:02,000\nNo index\n\n\
                      banana\n00:00:03,000 --> 00:00:04,000\nJunk index\n\n";

        // Act
        let cues = parse_srt(source);

        // Assert
        assert_that!(texts(&cues)).contains_exactly_in_given_order(["No index", "Junk index"]);
    }

    /// The byte-order mark has to be tested where it actually lands on the parser: on
    /// an index line it is harmless, so a fixture written that way passes even against
    /// a parser that never strips it.
    #[test]
    fn parse_srt_should_strip_a_byte_order_mark_before_a_timing_line() {
        // Arrange
        let source = "\u{feff}00:00:01,000 --> 00:00:02,000\nHello\n\n";

        // Act
        let cues = parse_srt(source);

        // Assert
        assert_that!(cues.len()).is_equal_to(1);
        assert_that!(cues[0].start).is_equal_to(milliseconds(1000));
    }

    /// `str::lines` already strips the single carriage return CRLF leaves, so plain CRLF
    /// proves nothing. A twice-converted file carries a second one that survives the
    /// split, and left in place it becomes part of the cue's text.
    #[test]
    fn parse_srt_should_strip_a_carriage_return_that_survives_line_splitting() {
        // Arrange
        let source = "1\r\r\n00:00:01,000 --> 00:00:02,000\r\r\nHello\r\r\n\r\r\n";

        // Act
        let cues = parse_srt(source);

        // Assert
        assert_that!(cues.len()).is_equal_to(1);
        assert_that!(cues[0].text.as_str()).is_equal_to("Hello");
    }

    #[test]
    fn parse_srt_should_return_cues_in_start_order_when_the_file_is_out_of_order() {
        // Arrange
        let source = "1\n00:00:05,000 --> 00:00:06,000\nLater\n\n\
                      2\n00:00:01,000 --> 00:00:02,000\nEarlier\n\n";

        // Act
        let cues = parse_srt(source);

        // Assert
        assert_that!(texts(&cues)).contains_exactly_in_given_order(["Earlier", "Later"]);
    }

    /// A backwards cue is a genuine authoring defect. Dropping it would hide the very
    /// problem this view exists to show, and leaving `end` before `start` underflows
    /// every span calculation downstream.
    #[test]
    fn parse_srt_should_flatten_a_cue_whose_end_precedes_its_start() {
        // Arrange
        let source = "1\n00:00:05,000 --> 00:00:02,000\nBackwards\n\n";

        // Act
        let cues = parse_srt(source);

        // Assert
        assert_that!(cues.len()).is_equal_to(1);
        assert_that!(cues[0].start).is_equal_to(milliseconds(5000));
        assert_that!(cues[0].end).is_equal_to(milliseconds(5000));
    }

    #[test]
    fn parse_srt_should_keep_a_cue_with_empty_text() {
        // Arrange
        let source =
            "1\n00:00:01,000 --> 00:00:02,000\n\n2\n00:00:03,000 --> 00:00:04,000\nAfter\n";

        // Act
        let cues = parse_srt(source);

        // Assert
        assert_that!(texts(&cues)).contains_exactly_in_given_order(["", "After"]);
    }

    #[test]
    fn parse_srt_should_ignore_trailing_position_tags_on_the_timing_line() {
        // Arrange
        let source = "1\n00:00:01,000 --> 00:00:02,000  X1:040 X2:600 Y1:050 Y2:100\nHello\n\n";

        // Act
        let cues = parse_srt(source);

        // Assert
        assert_that!(cues.len()).is_equal_to(1);
        assert_that!(cues[0].end).is_equal_to(milliseconds(2000));
    }

    #[test]
    fn parse_srt_should_return_no_cues_for_input_without_timing_lines() {
        // Act / Assert
        assert_that!(parse_srt("")).is_empty();
        assert_that!(parse_srt("   \n\n\t\n")).is_empty();
        assert_that!(parse_srt("WEBVTT\n\nnot a subtitle at all")).is_empty();
    }

    #[test]
    fn parse_timestamp_should_accept_either_decimal_separator() {
        // Act / Assert
        assert_that!(parse_timestamp("00:00:01,500")).is_equal_to(Some(milliseconds(1500)));
        assert_that!(parse_timestamp("00:00:01.500")).is_equal_to(Some(milliseconds(1500)));
    }

    /// The fractional field is a decimal, so a single digit is tenths. Reading it as a
    /// literal millisecond count shifts such a cue by nearly half a second.
    #[test]
    fn parse_timestamp_should_pad_and_truncate_the_fractional_field() {
        // Act / Assert
        assert_that!(parse_timestamp("00:00:01,5")).is_equal_to(Some(milliseconds(1500)));
        assert_that!(parse_timestamp("00:00:01,05")).is_equal_to(Some(milliseconds(1050)));
        assert_that!(parse_timestamp("00:00:01,050")).is_equal_to(Some(milliseconds(1050)));
        assert_that!(parse_timestamp("00:00:01,12345")).is_equal_to(Some(milliseconds(1123)));
    }

    #[test]
    fn parse_timestamp_should_accept_a_missing_hours_or_fractional_field() {
        // Act / Assert
        assert_that!(parse_timestamp("01:02")).is_equal_to(Some(milliseconds(62_000)));
        assert_that!(parse_timestamp("00:00:01")).is_equal_to(Some(milliseconds(1000)));
    }

    #[test]
    fn parse_timestamp_should_carry_an_out_of_range_component() {
        // Act / Assert: unambiguous, and some generators emit it.
        assert_that!(parse_timestamp("00:00:75,000")).is_equal_to(Some(milliseconds(75_000)));
    }

    #[test]
    fn parse_timestamp_should_reject_malformed_tokens() {
        // Act / Assert
        assert_that!(parse_timestamp("")).is_none();
        assert_that!(parse_timestamp("12")).is_none();
        assert_that!(parse_timestamp("00:00:00:00,000")).is_none();
        assert_that!(parse_timestamp("00:00:-1,000")).is_none();
        assert_that!(parse_timestamp("00:00:+1,000")).is_none();
        assert_that!(parse_timestamp("00:aa:01,000")).is_none();
        assert_that!(parse_timestamp("00:00:01,abc")).is_none();
        assert_that!(parse_timestamp("00:00:01,")).is_none();
        assert_that!(parse_timestamp("00::01,000")).is_none();
    }

    /// Debug builds panic on integer overflow, so an absurd hours field in a corrupt
    /// file would crash the app rather than being rejected as the garbage it is.
    #[test]
    fn parse_timestamp_should_reject_a_component_too_large_to_hold() {
        // Act / Assert: one case per multiplication on the way to a `Duration`, since
        // each is a separate place a corrupt file could overflow — too large for `u64`
        // at all, then hours into minutes, then minutes into seconds, then seconds into
        // milliseconds.
        assert_that!(parse_timestamp("99999999999999999999:00:01,000")).is_none();
        assert_that!(parse_timestamp("3074457345618258602:00:00,000")).is_none();
        assert_that!(parse_timestamp("307445734561825860:00:00,000")).is_none();
        assert_that!(parse_timestamp("00:18446744073709552,000")).is_none();
    }

    #[test]
    fn parse_timing_line_should_require_both_halves_to_be_timestamps() {
        // Act / Assert
        assert_that!(parse_timing_line("00:00:01,000 --> 00:00:02,000")).is_some();
        assert_that!(parse_timing_line("00:00:01,000 --> nonsense")).is_none();
        assert_that!(parse_timing_line("nonsense --> 00:00:02,000")).is_none();
        assert_that!(parse_timing_line("no arrow here")).is_none();
        assert_that!(parse_timing_line("00:00:01,000 -->")).is_none();
    }

    #[test]
    fn parse_timing_line_should_accept_an_arrow_without_surrounding_spaces() {
        // Act
        let parsed = parse_timing_line("00:00:01,000-->00:00:02,000");

        // Assert
        assert_that!(parsed).is_equal_to(Some((milliseconds(1000), milliseconds(2000))));
    }

    #[test]
    fn read_srt_should_parse_a_file_from_disk_and_survive_invalid_utf8() {
        // Arrange
        let directory = std::env::temp_dir().join(format!("reel-tui-cue-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("sample.srt");
        let mut bytes = b"1\n00:00:01,000 --> 00:00:02,000\nCaf".to_vec();
        bytes.push(0xe9); // Windows-1252 "é", invalid on its own as UTF-8.
        bytes.extend_from_slice(b"\n\n");
        std::fs::write(&path, bytes).unwrap();

        // Act
        let cues = read_srt(&path).unwrap();

        // Assert
        assert_that!(cues.len()).is_equal_to(1);
        assert_that!(cues[0].text.starts_with("Caf")).is_true();

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn read_srt_should_report_a_missing_file() {
        // Act
        let result = read_srt(Path::new("/nonexistent/reel-tui/missing.srt"));

        // Assert
        assert_that!(result.is_err()).is_true();
    }

    #[test]
    fn midpoint_should_fall_halfway_through_a_cue() {
        // Act / Assert
        assert_that!(cue(1000, 3000).midpoint()).is_equal_to(milliseconds(2000));
        assert_that!(cue(1000, 1000).midpoint()).is_equal_to(milliseconds(1000));
    }

    #[test]
    fn format_timestamp_should_render_hours_minutes_seconds_and_tenths() {
        // Act / Assert
        assert_that!(format_timestamp(Duration::ZERO).as_str()).is_equal_to("00:00:00.0");
        assert_that!(format_timestamp(milliseconds(62_300)).as_str()).is_equal_to("00:01:02.3");
        assert_that!(format_timestamp(milliseconds(3_723_999)).as_str()).is_equal_to("01:02:03.9");
    }

    #[test]
    fn pack_lanes_should_use_one_lane_when_no_cues_overlap() {
        // Arrange
        let cues = [cue(0, 1000), cue(2000, 3000), cue(4000, 5000)];

        // Act
        let layout = pack_lanes(&cues, MAX_LANES);

        // Assert
        assert_that!(layout.lane_count).is_equal_to(1);
        assert_that!(layout.lanes).contains_exactly_in_given_order([0, 0, 0]);
        assert_that!(layout.overflowed).contains_exactly_in_given_order([false, false, false]);
    }

    /// Touching cues do not overlap. Treating them as if they did doubles the lane count
    /// of an ordinary dialogue track, and lane count sets the track's height.
    #[test]
    fn pack_lanes_should_share_a_lane_when_one_cue_ends_exactly_where_the_next_starts() {
        // Arrange
        let cues = [cue(0, 1000), cue(1000, 2000)];

        // Act
        let layout = pack_lanes(&cues, MAX_LANES);

        // Assert
        assert_that!(layout.lane_count).is_equal_to(1);
        assert_that!(layout.lanes).contains_exactly_in_given_order([0, 0]);
    }

    #[test]
    fn pack_lanes_should_stack_overlapping_cues_onto_separate_lanes() {
        // Arrange
        let cues = [cue(0, 3000), cue(1000, 4000), cue(2000, 5000)];

        // Act
        let layout = pack_lanes(&cues, MAX_LANES);

        // Assert
        assert_that!(layout.lane_count).is_equal_to(3);
        assert_that!(layout.lanes).contains_exactly_in_given_order([0, 1, 2]);
        assert_that!(layout.overflowed).contains_exactly_in_given_order([false, false, false]);
    }

    /// The third cue is free to take either lane. First-fit must send it to lane 0, so
    /// that a track drifts back down to one row as soon as its overlaps clear instead of
    /// staying as tall as its worst moment.
    #[test]
    fn pack_lanes_should_reuse_the_earliest_free_lane() {
        // Arrange
        let cues = [cue(0, 1000), cue(0, 2000), cue(3000, 4000)];

        // Act
        let layout = pack_lanes(&cues, MAX_LANES);

        // Assert
        assert_that!(layout.lanes).contains_exactly_in_given_order([0, 1, 0]);
        assert_that!(layout.lane_count).is_equal_to(2);
    }

    #[test]
    fn pack_lanes_should_crowd_cues_onto_the_last_lane_rather_than_drop_them_past_the_cap() {
        // Arrange: five mutually overlapping cues against a cap of four.
        let cues = [
            cue(0, 9000),
            cue(100, 9000),
            cue(200, 9000),
            cue(300, 9000),
            cue(400, 9000),
        ];

        // Act
        let layout = pack_lanes(&cues, MAX_LANES);

        // Assert
        assert_that!(layout.lanes.len()).is_equal_to(5);
        assert_that!(layout.lane_count).is_equal_to(4);
        assert_that!(layout.lanes).contains_exactly_in_given_order([0, 1, 2, 3, 3]);
        assert_that!(layout.overflowed)
            .contains_exactly_in_given_order([false, false, false, false, true]);
    }

    #[test]
    fn pack_lanes_should_let_a_lane_recover_after_crowding() {
        // Arrange: two cues crowd lane 0, then a later cue arrives once both have ended.
        let cues = [cue(0, 1000), cue(500, 2000), cue(3000, 4000)];

        // Act
        let layout = pack_lanes(&cues, 1);

        // Assert
        assert_that!(layout.lanes).contains_exactly_in_given_order([0, 0, 0]);
        assert_that!(layout.overflowed).contains_exactly_in_given_order([false, true, false]);
    }

    #[test]
    fn pack_lanes_should_report_one_lane_for_an_empty_track() {
        // Act
        let layout = pack_lanes(&[], MAX_LANES);

        // Assert
        assert_that!(layout.lane_count).is_equal_to(1);
        assert_that!(layout.lanes).is_empty();
    }

    #[test]
    fn centered_should_show_the_whole_media_when_it_is_shorter_than_the_window() {
        // Act
        let window = TimelineWindow::centered(&cue(1000, 2000), Duration::from_secs(30), 100);

        // Assert
        assert_that!(window.start).is_equal_to(Duration::ZERO);
        assert_that!(window.end).is_equal_to(Duration::from_secs(30));
    }

    #[test]
    fn centered_should_place_the_selected_cue_in_the_middle() {
        // Act
        let window =
            TimelineWindow::centered(&cue(600_000, 602_000), Duration::from_secs(1200), 100);

        // Assert
        assert_that!(window.start).is_equal_to(Duration::from_secs(571));
        assert_that!(window.end).is_equal_to(Duration::from_secs(631));
    }

    /// Without clamping, an early cue produces a window starting before zero, wasting
    /// half the track on time that does not exist.
    #[test]
    fn centered_should_clamp_to_the_start_of_the_media() {
        // Act
        let window = TimelineWindow::centered(&cue(2000, 3000), Duration::from_secs(1200), 100);

        // Assert
        assert_that!(window.start).is_equal_to(Duration::ZERO);
        assert_that!(window.end).is_equal_to(WINDOW);
    }

    #[test]
    fn centered_should_clamp_to_the_end_of_the_media() {
        // Act
        let window =
            TimelineWindow::centered(&cue(1_199_000, 1_200_000), Duration::from_secs(1200), 100);

        // Assert
        assert_that!(window.start).is_equal_to(Duration::from_secs(1140));
        assert_that!(window.end).is_equal_to(Duration::from_secs(1200));
    }

    /// Subtitles running past the last video frame are common, and a probed duration can
    /// be missing entirely. The window still has to contain the cue it was built for.
    #[test]
    fn centered_should_still_contain_a_cue_that_outlasts_the_media() {
        // Act
        let window = TimelineWindow::centered(&cue(9000, 10_000), Duration::ZERO, 100);

        // Assert
        assert_that!(window.end >= milliseconds(10_000)).is_true();
        assert_that!(window.span(&cue(9000, 10_000))).is_some();
    }

    #[test]
    fn column_should_map_the_window_bounds_onto_the_first_and_last_columns() {
        // Arrange
        let window = TimelineWindow {
            start: Duration::ZERO,
            end: Duration::from_secs(10),
            width: 11,
        };

        // Act / Assert
        assert_that!(window.column(Duration::ZERO)).is_equal_to(Some(0));
        assert_that!(window.column(Duration::from_secs(5))).is_equal_to(Some(5));
        assert_that!(window.column(Duration::from_secs(10))).is_equal_to(Some(10));
    }

    #[test]
    fn column_should_reject_moments_outside_the_window_or_a_zero_width_track() {
        // Arrange
        let window = TimelineWindow {
            start: Duration::from_secs(10),
            end: Duration::from_secs(20),
            width: 10,
        };

        // Act / Assert
        assert_that!(window.column(Duration::from_secs(5))).is_none();
        assert_that!(window.column(Duration::from_secs(25))).is_none();
        assert_that!(TimelineWindow { width: 0, ..window }.column(Duration::from_secs(15)))
            .is_none();
    }

    #[test]
    fn span_should_cover_the_columns_a_cue_occupies() {
        // Arrange
        let window = TimelineWindow {
            start: Duration::ZERO,
            end: Duration::from_secs(10),
            width: 11,
        };

        // Act
        let span = window.span(&cue(2000, 5000));

        // Assert
        assert_that!(span).is_equal_to(Some((2, 5)));
    }

    /// A cue rounded down to no columns is a cue the user cannot see exists, which for a
    /// timing view is the worst possible failure. Rounding to the nearest column rather
    /// than truncating toward zero is what keeps a brief cue on the column it belongs to.
    #[test]
    fn span_should_place_a_cue_shorter_than_a_column_on_its_nearest_column() {
        // Arrange: 60 s across 60 columns, against a 100 ms cue at the halfway mark.
        let window = TimelineWindow {
            start: Duration::ZERO,
            end: Duration::from_secs(60),
            width: 60,
        };

        // Act
        let span = window.span(&cue(30_000, 30_100));

        // Assert
        assert_that!(span).is_equal_to(Some((30, 30)));
    }

    #[test]
    fn span_should_clamp_a_cue_that_reaches_past_the_window_edges() {
        // Arrange
        let window = TimelineWindow {
            start: Duration::from_secs(10),
            end: Duration::from_secs(20),
            width: 11,
        };

        // Act
        let span = window.span(&cue(5000, 25_000));

        // Assert
        assert_that!(span).is_equal_to(Some((0, 10)));
    }

    #[test]
    fn span_should_reject_a_cue_wholly_outside_the_window() {
        // Arrange
        let window = TimelineWindow {
            start: Duration::from_secs(10),
            end: Duration::from_secs(20),
            width: 11,
        };

        // Act / Assert
        assert_that!(window.span(&cue(0, 5000))).is_none();
        assert_that!(window.span(&cue(25_000, 30_000))).is_none();
        assert_that!(TimelineWindow { width: 0, ..window }.span(&cue(12_000, 15_000))).is_none();
    }
}
