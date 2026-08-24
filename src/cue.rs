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
    /// What to show in the cue list: plain text, with ASS override tags stripped and its
    /// `\N` breaks turned into real ones.
    pub text: String,
    /// The cue's original `Dialogue:` line, for an ASS track only.
    ///
    /// [`text`](Self::text) is what the *reader* needs and this is what the *renderer*
    /// needs, and for ASS they are not the same string. A cue's line carries its style
    /// name, its layer, its margins, and any `{\pos}`-style overrides — which for a sign
    /// or a piece of typesetting is most of what the cue is. Burning the stripped text
    /// instead would draw it in the default style at the bottom of the frame, so the page
    /// would be showing something the user will never see, on the one page whose entire
    /// job is comparing subtitles against the picture.
    ///
    /// Empty for every other format, where the text is the whole cue.
    ///
    /// **More than one line when several events were folded into this cue** — see
    /// [`collapse`]. A typeset or karaoke line is routinely a dozen `Dialogue:` events
    /// sharing a timing and a set of words, differing only in their override tags; they are
    /// one line to the reader and one row on the page, so they are one cue here, and all of
    /// their lines are staged together.
    pub dialogue: Vec<String>,
    /// How many of the file's entries were folded into this cue — one for a cue that stands
    /// on its own. ASS calls them events and SubRip calls them blocks.
    ///
    /// Counted rather than read off [`dialogue`](Self::dialogue), which holds a line only for
    /// ASS: a SubRip cue folded from two blocks has no lines to count and is still a row
    /// standing for two entries. The page puts it on the row so a fold is visible rather than
    /// silent — the alternative is a list that quietly has fewer rows than the file has
    /// entries, with nothing on screen to say so.
    pub events: usize,
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

/// Every cue that is on the screen at `instant`, `cue` among them, in file order.
///
/// **A cue is not what the viewer sees; the cues on screen together are.** A typeset or
/// karaoke line is routinely one visible line split across a dozen `Dialogue:` events that
/// share a timing and differ only in their override tags — each drawing part of an effect,
/// with the picture being the sum of all of them. Burning one of those on its own draws a
/// tenth of a picture nobody will ever see, on the page whose whole job is judging a
/// subtitle against the picture. The same holds, less dramatically, for a sign over a line
/// of dialogue: leave the sign out and the frame is missing something the viewer has.
///
/// Half-open on purpose (`start <= instant < end`), matching every other timing comparison
/// on this page: a cue that ends exactly as the grab lands is off the screen, not on it.
///
/// `cue` itself is included whether or not it passes that test, and that is not belt and
/// braces — [`crate::preview::seek_for`] holds a seek back from the very end of the media,
/// so a cue running to the last frame is grabbed at an instant just outside itself. Leaving
/// it to the filter would burn every cue on screen *except* the one being previewed.
/// `at` is a **position in `cues`**, not a [`Cue::index`]. The two agree for a parsed track
/// and the field would read better here, but every caller has the position already, and a
/// list whose indices had drifted would silently burn in every cue sharing the anchor's
/// index instead of the anchor.
pub fn on_screen_at(cues: &[Cue], at: usize, instant: Duration) -> Vec<Cue> {
    cues.iter()
        .enumerate()
        .filter(|(position, cue)| *position == at || (cue.start <= instant && instant < cue.end))
        .map(|(_, cue)| cue.clone())
        .collect()
}

/// Every cue that is on the screen at some point during `start..end`, `cue` among them, in
/// file order.
///
/// The span form of [`on_screen_at`], for the scrub playback: a playback runs from before
/// the cue to after it, so the question is not what is on screen at one instant but what
/// appears at any point in the stretch being watched. Standard half-open overlap — a cue
/// ending exactly as the span begins never appears in it.
pub fn on_screen_between(cues: &[Cue], at: usize, start: Duration, end: Duration) -> Vec<Cue> {
    cues.iter()
        .enumerate()
        .filter(|(position, cue)| *position == at || shares_screen(cue, start, end))
        .map(|(_, cue)| cue.clone())
        .collect()
}

/// Whether `cue` is on screen at any point during `start..end`.
///
/// Half-open at both ends, the boundary [`pack_lanes`] and [`group_overlaps`] also draw: a
/// cue ending exactly as the span begins is never on screen with it, and treating it as
/// though it were would make most of an ordinary dialogue track one continuous overlap.
///
/// Its own function because two callers need the rule for spans that are *not* a cue's:
/// [`on_screen_between`] asks it of a playback's stretch, and the timing page asks it of a
/// cue's span before and after a nudge, to find whose pictures the move invalidated.
pub fn shares_screen(cue: &Cue, start: Duration, end: Duration) -> bool {
    cue.start < end && cue.end > start
}

/// The tail every parse ends with: sort by time, fold the runs that are one line, number
/// what is left.
///
/// One function so the two parsers cannot disagree about what a cue list is. The order
/// matters: [`collapse`] folds *consecutive* cues, so the sort has to have brought the
/// candidates together first, and the numbering has to come last or it would number rows
/// that no longer exist.
fn ordered(mut cues: Vec<Cue>) -> Vec<Cue> {
    // Stable, so cues sharing a timing keep the order the file put them in — which is the
    // order libass draws them in, and the order `collapse` folds them in.
    cues.sort_by_key(|cue| (cue.start, cue.end));
    let mut cues = collapse(cues);
    for (index, cue) in cues.iter_mut().enumerate() {
        cue.index = index;
    }
    cues
}

/// Folds each run of cues that are the same line into one cue carrying all of their
/// `Dialogue:` lines.
///
/// **One visible line is often many events.** A typeset or karaoke line is routinely a dozen
/// `Dialogue:` events sharing a timing and a set of words, each carrying a different
/// override tag so that together they animate; the reader sees one line, and the timing page
/// showed a dozen identical rows, each previewing a fraction of the picture. Folding them
/// makes the row match what is on the screen: one entry, one frame, one span to play.
///
/// **The test is deliberately dumb: identical start, identical end, identical text, and
/// adjacent in the file.** Rows that are indistinguishable to the reader become one row, and
/// nothing else is touched. Anything looser starts guessing which events belong to one line
/// — real karaoke gives each syllable its own timing, and merging those would hide the very
/// differences the page exists to show.
///
/// The folded cue's own timing needs no thought precisely because of that test: every member
/// shares it. What is kept from each of them is its `Dialogue:` line, since that is the only
/// thing they differ in, and staging all of them is what draws the whole line.
fn collapse(cues: Vec<Cue>) -> Vec<Cue> {
    let mut collapsed: Vec<Cue> = Vec::with_capacity(cues.len());
    for cue in cues {
        match collapsed.last_mut() {
            Some(last)
                if last.start == cue.start && last.end == cue.end && last.text == cue.text =>
            {
                last.dialogue.extend(cue.dialogue);
                last.events += cue.events;
            }
            _ => collapsed.push(cue),
        }
    }
    collapsed
}

/// Renders a cue list back to SubRip.
///
/// The inverse of [`parse_srt`] for everything a SubRip file actually carries: counters
/// numbered from one, `00:00:01,500 --> 00:00:03,000` timings, the text, a blank line
/// between blocks. Written with `\n` and a trailing newline, which is what every player
/// reads and what `parse_srt` is at home with; a file that arrived with CRLF comes back
/// with LF, since keeping the returns would mean threading the file's line ending through a
/// parse that deliberately ignores it.
///
/// **A folded cue is written once, not once per event it stands for.** `collapse` only folds
/// entries that are identical in every way a reader can see — same timing, same text — so
/// the duplicates it removes carry nothing a player would draw differently. Writing them
/// back would preserve a defect rather than the file.
pub fn write_srt(cues: &[Cue]) -> String {
    let mut out = String::new();
    for (position, cue) in cues.iter().enumerate() {
        out.push_str(&format!(
            "{}\n{} --> {}\n{}\n\n",
            position + 1,
            format_srt_timestamp(cue.start),
            format_srt_timestamp(cue.end),
            cue.text
        ));
    }
    out
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
            dialogue: Vec::new(),
            events: 1,
        });
    }

    ordered(cues)
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

/// An ASS script split into the parts the timing page needs.
///
/// The cues alone are not enough to draw one. An ASS cue names a *style* rather than
/// carrying one, and positions itself against the script's declared `PlayResX`/`PlayResY`
/// rather than against the video — so a `Dialogue:` line lifted out on its own renders in
/// libass's defaults at the wrong scale. What the header holds is the other half of every
/// cue in the file.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AssScript {
    /// Everything before `[Events]`, verbatim: `[Script Info]`, `[V4+ Styles]`, and any
    /// other section the file carries. Kept as text rather than parsed, because the page
    /// only ever hands it back to libass — anything understood here is something that
    /// could be understood *wrongly*.
    pub header: String,
    /// The `[Events]` section's own `Format:` line, verbatim.
    ///
    /// Field order is declared per file rather than fixed by the format. Almost every
    /// file uses the same order and the one that does not would have its cues staged with
    /// their columns shuffled, which libass reads as a different cue entirely.
    pub events_format: String,
    pub cues: Vec<Cue>,
}

/// Parses an ASS or SSA script.
///
/// Lenient in the same way [`parse_srt`] is and for the same reason: what cannot be read
/// is skipped rather than failing the track. A file with no `[Events]` section yields no
/// cues, which the page already reports as an empty track.
pub fn parse_ass(source: &str) -> AssScript {
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let mut header = String::new();
    let mut events_format = String::new();
    let mut columns: Vec<String> = Vec::new();
    let mut cues: Vec<Cue> = Vec::new();
    let mut in_events = false;

    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            // Case-insensitively, because `[Events]` is spelled every way in the wild and
            // a missed section header silently costs the whole track its cues.
            in_events = trimmed.eq_ignore_ascii_case("[events]");
            if !in_events {
                header.push_str(line);
                header.push('\n');
            }
            continue;
        }
        if !in_events {
            header.push_str(line);
            header.push('\n');
            continue;
        }
        if let Some(rest) = strip_ass_key(trimmed, "Format:") {
            events_format = line.to_string();
            columns = rest
                .split(',')
                .map(|name| name.trim().to_string())
                .collect();
            continue;
        }
        let Some(rest) = strip_ass_key(trimmed, "Dialogue:") else {
            continue;
        };
        // A `Dialogue:` before the `Format:` that describes it cannot be read at all, and
        // guessing the standard order would put a cue on screen at a time nothing in the
        // file says.
        let (Some(start_at), Some(end_at)) =
            (column_of(&columns, "Start"), column_of(&columns, "End"))
        else {
            continue;
        };
        // `columns.len()` splits, not more: the last column is Text and is the one field
        // allowed to contain commas.
        let fields: Vec<&str> = rest.splitn(columns.len(), ',').collect();
        if fields.len() < columns.len() {
            continue;
        }
        let (Some(start), Some(end)) = (
            parse_timestamp(fields[start_at]),
            parse_timestamp(fields[end_at]),
        ) else {
            continue;
        };
        let text = column_of(&columns, "Text")
            .and_then(|at| fields.get(at))
            .map(|field| plain_ass_text(field))
            .unwrap_or_default();
        cues.push(Cue {
            index: 0,
            start,
            end: end.max(start),
            text,
            dialogue: vec![line.to_string()],
            events: 1,
        });
    }

    let cues = ordered(cues);
    AssScript {
        header: header.trim_end().to_string(),
        events_format,
        cues,
    }
}

/// Reads and parses an `.ass` file, lossily for the same reason [`read_srt`] is.
pub fn read_ass(path: &Path) -> std::io::Result<AssScript> {
    let bytes = std::fs::read(path)?;
    Ok(parse_ass(&String::from_utf8_lossy(&bytes)))
}

/// The value after an ASS `Key:` prefix, matched without regard to case.
fn strip_ass_key<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.get(..key.len())
        .filter(|prefix| prefix.eq_ignore_ascii_case(key))
        .map(|_| line[key.len()..].trim_start())
}

fn column_of(columns: &[String], name: &str) -> Option<usize> {
    columns
        .iter()
        .position(|column| column.eq_ignore_ascii_case(name))
}

/// An ASS text field as something worth putting in a list.
///
/// Override blocks (`{\pos(320,10)}`, `{\an8}`, karaoke timings) are markup, not words,
/// and a cue list full of them is unreadable. `\N` and `\n` are the format's line breaks
/// and become real ones. The *rendered* cue keeps all of it — see [`Cue::dialogue`].
fn plain_ass_text(field: &str) -> String {
    let mut text = String::with_capacity(field.len());
    let mut depth = 0usize;
    let mut characters = field.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '{' => depth += 1,
            // Saturating, so a stray `}` with no opening brace does not swallow the rest
            // of the line by wrapping the depth around.
            '}' => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            '\\' if matches!(characters.peek(), Some('N' | 'n')) => {
                characters.next();
                text.push('\n');
            }
            // `\h` is a non-breaking space, which reads as an ordinary one in a list.
            '\\' if characters.peek() == Some(&'h') => {
                characters.next();
                text.push(' ');
            }
            _ => text.push(character),
        }
    }
    text.trim().to_string()
}

/// Renders a cue time the way an ASS `Dialogue:` line spells it: `H:MM:SS.cc`.
///
/// Centiseconds, and exactly one hour digit — libass parses more leniently than that, but
/// a staged line that does not look like the ones around it is a difference waiting to
/// matter.
pub fn format_ass_timestamp(at: Duration) -> String {
    let centiseconds = (at.as_millis() / 10) as u64;
    format!(
        "{}:{:02}:{:02}.{:02}",
        centiseconds / 360_000,
        (centiseconds / 6_000) % 60,
        (centiseconds / 100) % 60,
        centiseconds % 100
    )
}

/// Rewrites a `Dialogue:` line's Start and End columns, leaving every other column alone.
///
/// The preview burns one cue onto a frame grabbed from the middle of that cue, so the
/// staged line has to be on screen at whatever moment the grab lands on — the same job
/// `one_cue_srt` does for SubRip. Rewriting rather than rebuilding, because every other
/// column is the cue's styling and this code has no business understanding it.
///
/// Returns `None` when the line cannot be read as a `Dialogue:` with the columns
/// `events_format` declares, which is the same condition that stops it becoming a cue.
pub fn retimed_dialogue(
    events_format: &str,
    dialogue: &str,
    start: Duration,
    end: Duration,
) -> Option<String> {
    let columns: Vec<String> = strip_ass_key(events_format.trim(), "Format:")?
        .split(',')
        .map(|name| name.trim().to_string())
        .collect();
    let start_at = column_of(&columns, "Start")?;
    let end_at = column_of(&columns, "End")?;
    let rest = strip_ass_key(dialogue.trim(), "Dialogue:")?;
    let mut fields: Vec<String> = rest
        .splitn(columns.len(), ',')
        .map(str::to_string)
        .collect();
    if fields.len() < columns.len() {
        return None;
    }
    fields[start_at] = format_ass_timestamp(start);
    fields[end_at] = format_ass_timestamp(end);
    Some(format!("Dialogue: {}", fields.join(",")))
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

/// Renders a moment as a clock reading for the timeline axis: `1:23` or `1:02:03`.
///
/// The third and most compact of the three time formats here, because an axis reading is
/// repeated across the track and pays for every character it spends. Whole seconds are
/// enough: the axis is a reference to read cue positions *against*, and the exact times
/// are in the cue list and the timeline's own title.
///
/// Whether hours appear is the caller's decision rather than derived per value, so one
/// axis cannot mix `59:59` with `1:00:00` — readings of two different widths would make
/// the spacing between them lie about the interval.
pub fn format_clock(at: Duration, with_hours: bool) -> String {
    let seconds = at.as_secs();
    if with_hours {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            (seconds % 3600) / 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

/// Renders a cue time for a block that has no room for [`format_timestamp`]: `5:05.0` or
/// `1:05:05.0`.
///
/// Between the other two formats in what it spends: it keeps the tenth of a second, which
/// is the digit this page is actually read for, and gives up the leading zeroes and the
/// hours field a short file never needs. Two of these plus an arrow fit inside a cue panel
/// split in half, where one [`format_timestamp`] alone does not.
///
/// `with_hours` is the caller's decision for the same reason it is in [`format_clock`]: a
/// list that mixed `59:05.0` with `1:00:05.0` would change width halfway down, and the
/// caller has already had to size a block against the widest reading it will draw.
pub fn format_compact(at: Duration, with_hours: bool) -> String {
    let seconds = at.as_secs();
    let tenths = at.subsec_millis() / 100;
    if with_hours {
        format!(
            "{}:{:02}:{:02}.{}",
            seconds / 3600,
            (seconds % 3600) / 60,
            seconds % 60,
            tenths
        )
    } else {
        format!("{}:{:02}.{}", seconds / 60, seconds % 60, tenths)
    }
}

/// Renders a cue time the way SubRip writes one, to the millisecond.
///
/// The comma is not a stylistic choice: `ffmpeg`'s SubRip demuxer is what reads the file
/// this produces, and a period there makes it a different format.
pub fn format_srt_timestamp(at: Duration) -> String {
    let seconds = at.as_secs();
    format!(
        "{:02}:{:02}:{:02},{:03}",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60,
        at.subsec_millis()
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

/// A run of cues that are on screen together, as a slice of the cue list.
///
/// `len` is never zero: every cue belongs to exactly one group, and a cue overlapping
/// nothing is a group of one. That is the ordinary case on a dialogue track, which is why
/// the cue panel draws a group of one exactly as it drew a lone cue before groups existed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CueGroup {
    /// Position in the cue list of the group's first cue.
    pub first: usize,
    /// How many consecutive cues the group holds.
    pub len: usize,
}

impl CueGroup {
    /// One past the group's last cue, for slicing and for range checks.
    pub fn end(&self) -> usize {
        self.first + self.len
    }

    /// Whether the cue at this position in the list belongs to the group.
    pub fn holds(&self, cue: usize) -> bool {
        cue >= self.first && cue < self.end()
    }
}

/// Splits the cue list into runs of cues that share the screen.
///
/// A cue joins the run it is next to when it starts before the run's *furthest* end so far,
/// not merely before its predecessor's. A sign that covers a whole scene and the four lines
/// of dialogue under it are one group, which is what the reader sees, where pairing cues off
/// against their immediate neighbour would split them into four.
///
/// `<` rather than `<=`, the same boundary [`pack_lanes`] draws for the same reason: a cue
/// starting exactly where the last one ended is never on screen with it, and treating it as
/// though it were would group most of an ordinary dialogue track into one run.
///
/// Assumes the start-ordered list both parsers produce (`ordered`). Given an unsorted list
/// it still returns a partition of every cue into groups of at least one — nothing panics
/// or is dropped — but the runs it finds are not meaningful.
pub fn group_overlaps(cues: &[Cue]) -> Vec<CueGroup> {
    let mut groups: Vec<CueGroup> = Vec::new();
    let mut reach = Duration::ZERO;

    for (position, cue) in cues.iter().enumerate() {
        match groups.last_mut() {
            // The run reaches past this cue's start, so the two are on screen together.
            Some(group) if cue.start < reach => {
                group.len += 1;
                reach = reach.max(cue.end);
            }
            _ => {
                groups.push(CueGroup {
                    first: position,
                    len: 1,
                });
                reach = cue.end;
            }
        }
    }

    groups
}

/// How much of the timeline the track shows at once.
///
/// A scrolling window rather than the whole file: at whole-file scale a two-second cue
/// in a ninety-minute film occupies a fraction of one cell, so every cue would render
/// as a bare `|` and the track would carry no timing information at all. Across a
/// typical track width this is roughly half a second per column.
pub const WINDOW: Duration = Duration::from_secs(60);

/// The window lengths the timeline will pick between, longest first.
///
/// A minute is right for the ordinary dialogue track, where it holds a dozen cues and each
/// one is wide enough to read. It is hopeless for a typeset ASS track, where a minute holds
/// several hundred events: every lane fills end to end with brackets and the track becomes a
/// texture rather than a timing. The window therefore shortens until what is in it can
/// actually be drawn — the reader loses context they could not read anyway, and gets back
/// the one thing this pane is for, which is where the selected cue sits against its
/// neighbours.
pub const WINDOW_STEPS: [Duration; 4] = [
    WINDOW,
    Duration::from_secs(30),
    Duration::from_secs(15),
    Duration::from_secs(8),
];

/// Columns a cue needs before it reads as a span rather than as a mark.
///
/// `|<─>|` is five, and one column of gap keeps two neighbours from running together into a
/// single unreadable rule.
const READABLE_CUE_COLUMNS: u16 = 6;

/// The slice of time the timeline track is currently showing, and how wide it is drawn.
///
/// [`TimelineWindow::fitted`] is the only constructor the application uses, and it
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
    /// The window for a selected cue on *this* track, shortened until its cues are drawable.
    ///
    /// The choice is made against how many cues the candidate window holds rather than
    /// against the format or the cue count of the file: a sparse ASS track keeps the full
    /// minute and a dialogue track that happens to be dense loses it, which is the right way
    /// round. Capacity is every lane's columns divided by what one readable cue costs, since
    /// the cues in view are spread across the lanes rather than queued in one.
    ///
    /// **Never shorter than the selected cue plus half again.** A twelve-second sign in an
    /// eight-second window would be clamped at both edges and drawn as a bar with no ends,
    /// which loses the only timing the reader came for. A track dense enough to want a
    /// shorter window than that keeps the longer one and stays crowded — legibility of the
    /// selection outranks legibility of its company.
    pub fn fitted(on: &Cue, cues: &[Cue], duration: Duration, width: u16, lanes: usize) -> Self {
        let capacity = usize::from(width / READABLE_CUE_COLUMNS) * lanes.max(1);
        let floor = on.end.saturating_sub(on.start) * 3 / 2;
        let mut window = Self::spanning(on, duration, width, WINDOW_STEPS[0]);
        for step in WINDOW_STEPS {
            if step < floor {
                break;
            }
            window = Self::spanning(on, duration, width, step);
            if window.count(cues) <= capacity {
                break;
            }
        }
        window
    }

    /// How many of the track's cues fall inside the window.
    ///
    /// Counted by intersection rather than by start, so a sign that opened before the window
    /// and is still on screen is charged for — it is drawn across the whole track and is
    /// exactly the kind of cue that crowds one.
    fn count(&self, cues: &[Cue]) -> usize {
        cues.iter()
            .filter(|cue| cue.end >= self.start && cue.start <= self.end)
            .count()
    }

    /// The window of a given length, clamped to the media's bounds.
    ///
    /// Clamping at both ends matters: a cue two seconds in must not produce a window
    /// starting at minus twenty-eight seconds, which would waste half the track on time
    /// that does not exist.
    fn spanning(on: &Cue, duration: Duration, width: u16, span: Duration) -> Self {
        // A track can outlast its video — subtitles running past the last frame are
        // common — and the probed duration can be missing entirely. Either way the
        // window still has to contain the cue it was built for.
        let duration = duration.max(on.end).max(Duration::from_millis(1));
        if duration <= span {
            return Self {
                start: Duration::ZERO,
                end: duration,
                width,
            };
        }
        let start = on.midpoint().saturating_sub(span / 2).min(duration - span);
        Self {
            start,
            end: start + span,
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
            dialogue: Vec::new(),
            events: 1,
        }
    }

    fn texts(cues: &[Cue]) -> Vec<&str> {
        cues.iter().map(|cue| cue.text.as_str()).collect()
    }

    /// The shape a karaoke or typeset effect really has in a file: one visible line spread
    /// over several `Dialogue:` events that share a timing and a set of words, each carrying
    /// a different override tag so that together they animate.
    const ANIMATED_ASS: &str = "[Script Info]\n\
         ScriptType: v4.00+\n\
         \n\
         [V4+ Styles]\n\
         Format: Name, Fontname, Fontsize, Alignment\n\
         Style: Default,Arial,16,2\n\
         \n\
         [Events]\n\
         Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
         Dialogue: 0,0:00:29.30,0:00:30.10,Default,,0,0,0,,fu\n\
         Dialogue: 0,0:00:29.50,0:00:29.80,Default,,0,0,0,,{\\t(0,100,\\fscx120)}wo\n\
         Dialogue: 0,0:00:29.50,0:00:29.80,Default,,0,0,0,,{\\t(100,200,\\fscx140)}wo\n\
         Dialogue: 0,0:00:29.50,0:00:29.80,Default,,0,0,0,,{\\t(200,300,\\fscx160)}wo\n";

    /// One visible line is one row, however many events draw it.
    ///
    /// The complaint this answers: a track like `ANIMATED_ASS` filled the cue list with
    /// identical rows — same words, same timing, no way to tell them apart — each previewing
    /// a fraction of the picture. They are one line to a reader, so they are one cue here.
    #[test]
    fn events_that_are_one_line_should_collapse_into_one_cue() {
        // Act
        let script = parse_ass(ANIMATED_ASS);

        // Assert: two rows, not four — and the odd one out is left alone, because its timing
        // differs and a difference in timing is exactly what this page exists to show.
        assert_that!(texts(&script.cues)).is_equal_to(vec!["fu", "wo"]);
        assert_that!(script.cues[1].start).is_equal_to(milliseconds(29_500));
        assert_that!(script.cues[1].end).is_equal_to(milliseconds(29_800));

        // Assert: every line that drew it is kept, in file order. Dropping any of them would
        // preview a fraction of the effect, which is the whole reason they are folded rather
        // than filtered.
        assert_that!(script.cues[1].dialogue.len()).is_equal_to(3);
        for (position, scale) in ["fscx120", "fscx140", "fscx160"].iter().enumerate() {
            assert_that!(script.cues[1].dialogue[position].as_str()).contains(*scale);
        }
        assert_that!(script.cues[0].dialogue.len()).is_equal_to(1);

        // Assert: and the numbering describes the list that is left, so nothing downstream
        // can address a row that no longer exists.
        assert_that!(script.cues[0].index).is_equal_to(0);
        assert_that!(script.cues[1].index).is_equal_to(1);
    }

    /// The test is deliberately dumb, and these are the cases it must *not* fold: anything a
    /// reader could tell apart stays its own row.
    #[test]
    fn cues_that_differ_in_any_way_should_stay_their_own_rows() {
        // Arrange: same words at a different time, same time with different words, and the
        // same line either side of an interloper that shares its timing.
        let differing = ANIMATED_ASS
            .replace(
                "Dialogue: 0,0:00:29.50,0:00:29.80,Default,,0,0,0,,{\\t(100,200,\\fscx140)}wo",
                "Dialogue: 0,0:00:29.50,0:00:29.80,Default,,0,0,0,,ka",
            )
            .replace(
                "Dialogue: 0,0:00:29.50,0:00:29.80,Default,,0,0,0,,{\\t(200,300,\\fscx160)}wo\n",
                "Dialogue: 0,0:00:29.50,0:00:29.80,Default,,0,0,0,,{\\t(200,300,\\fscx160)}wo\n\
                 Dialogue: 0,0:00:31.00,0:00:31.50,Default,,0,0,0,,wo\n",
            );

        // Act
        let script = parse_ass(&differing);

        // Assert: nothing folds at all. The two `wo`s at 29.5 share a timing and their words
        // but the `ka` between them broke the run, and a rule that reached past its
        // neighbours would start deciding which of a file's events belong together — which
        // it has no business doing. The last `wo` says the same thing at a different time,
        // and a difference in timing is the one thing this page must never hide.
        assert_that!(texts(&script.cues)).is_equal_to(vec!["fu", "wo", "ka", "wo", "wo"]);
        assert_that!(script.cues[4].start).is_equal_to(milliseconds(31_000));
        assert_that!(
            script
                .cues
                .iter()
                .map(|cue| cue.dialogue.len())
                .collect::<Vec<_>>()
        )
        .is_equal_to(vec![1, 1, 1, 1, 1]);
    }

    /// The rule is about cues, not about formats: a SubRip file that repeats a line at the
    /// same moment shows it once, for the same reason.
    #[test]
    fn a_subrip_line_repeated_at_the_same_moment_should_collapse_too() {
        // Act
        let cues = parse_srt(
            "1\n00:00:01,000 --> 00:00:02,000\nsame\n\n\
             2\n00:00:01,000 --> 00:00:02,000\nsame\n\n\
             3\n00:00:03,000 --> 00:00:04,000\nsame\n\n",
        );

        // Assert: two rows — the repeat folds, the later one stands.
        assert_that!(texts(&cues)).is_equal_to(vec!["same", "same"]);
        assert_that!(cues[1].start).is_equal_to(milliseconds(3000));
        // Nothing to keep from a SubRip cue but its text, which the fold already has.
        assert_that!(cues[0].dialogue.is_empty()).is_true();
    }

    /// The cue list a typeset or karaoke line really produces: one visible line spread
    /// across several events that share a moment, plus ordinary cues either side of it.
    fn overlapping_track() -> Vec<Cue> {
        let named = |start, end, text: &str| Cue {
            text: text.to_string(),
            ..cue(start, end)
        };
        vec![
            named(0, 1000, "before"),
            named(2000, 3000, "under"),
            named(2400, 2600, "effect one"),
            named(2400, 2600, "effect two"),
            named(4000, 5000, "after"),
        ]
    }

    /// The point of previewing at all: what a viewer sees at one moment is every cue on
    /// screen at it, and a karaoke or typeset line is routinely a dozen events sharing that
    /// moment. Burning one of them would draw a fraction of a picture nobody ever sees.
    #[test]
    fn what_is_on_screen_at_a_moment_should_be_every_cue_covering_it() {
        // Arrange
        let cues = overlapping_track();

        // Act / Assert: the anchor's own frame carries the effect lines over it, in file
        // order, and nothing from either side of them.
        assert_that!(texts(&on_screen_at(&cues, 1, milliseconds(2500)))).is_equal_to(vec![
            "under",
            "effect one",
            "effect two",
        ]);

        // Act / Assert: and asking from one of the effect lines gives the same picture —
        // which is what makes the row you are sitting on a matter of *filing* rather than
        // of what gets drawn.
        assert_that!(texts(&on_screen_at(&cues, 2, milliseconds(2500)))).is_equal_to(vec![
            "under",
            "effect one",
            "effect two",
        ]);

        // Act / Assert: half-open, like every other timing comparison on this page. At the
        // effect lines' end instant they are already gone.
        assert_that!(texts(&on_screen_at(&cues, 1, milliseconds(2600)))).is_equal_to(vec!["under"]);
        assert_that!(texts(&on_screen_at(&cues, 1, milliseconds(2400)))).is_equal_to(vec![
            "under",
            "effect one",
            "effect two",
        ]);
    }

    /// `seek_for` holds a grab back from the very last instant of the media, so a cue
    /// running to the end of the file is sampled at a moment just outside itself. Filtering
    /// on the instant alone would then burn in every cue on screen *except* the one being
    /// previewed — a blank preview for the last line of every track.
    #[test]
    fn the_cue_being_previewed_should_be_on_screen_even_when_the_instant_is_not_inside_it() {
        // Arrange
        let cues = overlapping_track();

        // Act / Assert: nothing covers 3.5 s, and the anchor is still there.
        assert_that!(texts(&on_screen_at(&cues, 0, milliseconds(3500))))
            .is_equal_to(vec!["before"]);

        // Act / Assert: and it keeps its place in file order rather than being appended.
        assert_that!(texts(&on_screen_at(&cues, 4, milliseconds(2500)))).is_equal_to(vec![
            "under",
            "effect one",
            "effect two",
            "after",
        ]);
    }

    /// A playback runs from before the cue to after it, so the question is not what is on
    /// screen at one instant but everything that appears during the stretch being watched.
    #[test]
    fn what_appears_in_a_span_should_be_every_cue_overlapping_it() {
        // Arrange
        let cues = overlapping_track();

        // Act / Assert: a second of padding either side of the middle cue pulls in the
        // effect lines over it, and nothing else.
        assert_that!(texts(&on_screen_between(
            &cues,
            1,
            milliseconds(1000),
            milliseconds(4000)
        )))
        .is_equal_to(vec!["under", "effect one", "effect two"]);

        // Act / Assert: half-open at both ends — a cue ending exactly as the span opens
        // never appears in it, and one starting exactly as it closes has not arrived.
        assert_that!(texts(&on_screen_between(
            &cues,
            1,
            milliseconds(1000),
            milliseconds(4001)
        )))
        .is_equal_to(vec!["under", "effect one", "effect two", "after"]);

        // Act / Assert: and the anchor is kept even for a span that has left it behind,
        // for the same reason `on_screen_at` keeps it.
        assert_that!(texts(&on_screen_between(
            &cues,
            0,
            milliseconds(4000),
            milliseconds(5000)
        )))
        .is_equal_to(vec!["before", "after"]);
    }

    /// The shape `ffmpeg`'s own ASS muxer writes, which is what an extracted track looks
    /// like in practice.
    const ASS: &str = "[Script Info]\n\
                       ScriptType: v4.00+\n\
                       PlayResX: 384\n\
                       PlayResY: 288\n\
                       \n\
                       [V4+ Styles]\n\
                       Format: Name, Fontname, Fontsize, PrimaryColour, Alignment\n\
                       Style: Default,Arial,16,&Hffffff,2\n\
                       Style: Sign,Impact,40,&H00ffff,8\n\
                       \n\
                       [Events]\n\
                       Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
                       Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,first line\n\
                       Dialogue: 0,0:00:03.00,0:00:04.50,Sign,,0,0,0,,{\\pos(320,10)}a sign, with a comma\n";

    #[test]
    fn parse_ass_should_read_cues_and_keep_the_header_that_styles_them() {
        // Act
        let script = parse_ass(ASS);

        // Assert: the header is everything before `[Events]`, kept verbatim — the styles
        // the cues name and the `PlayRes` they position against.
        assert_that!(script.header.as_str()).contains("[Script Info]");
        assert_that!(script.header.as_str()).contains("PlayResX: 384");
        assert_that!(script.header.as_str()).contains("[V4+ Styles]");
        assert_that!(script.header.as_str()).contains("Style: Sign,Impact,40,&H00ffff,8");
        assert_that!(script.header.contains("[Events]")).is_false();
        assert_that!(script.header.contains("Dialogue:")).is_false();
        assert_that!(script.events_format.as_str()).is_equal_to(
            "Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text",
        );

        // Assert: the cues themselves.
        assert_that!(script.cues.len()).is_equal_to(2);
        assert_that!(script.cues[0].start).is_equal_to(Duration::from_secs(1));
        assert_that!(script.cues[0].end).is_equal_to(Duration::from_secs(2));
        assert_that!(script.cues[1].end).is_equal_to(Duration::from_millis(4500));
        assert_that!(script.cues[1].index).is_equal_to(1);

        // Assert: the list text is readable, and the line the renderer needs is verbatim.
        // The second cue is the whole point — its override block is markup rather than
        // words, and its text contains the comma that a naive split would cut it at.
        assert_that!(texts(&script.cues)).is_equal_to(vec!["first line", "a sign, with a comma"]);
        assert_that!(script.cues[1].dialogue.as_slice()).is_equal_to(
            [
                "Dialogue: 0,0:00:03.00,0:00:04.50,Sign,,0,0,0,,{\\pos(320,10)}a sign, with a comma"
                    .to_string(),
            ]
            .as_slice(),
        );
    }

    /// Field order is declared per file rather than fixed by the format. A parser that
    /// assumed the usual order would read this file's cues at the wrong times and stage
    /// them with their columns shuffled, which libass renders as something else entirely.
    #[test]
    fn parse_ass_should_take_its_column_order_from_the_files_own_format_line() {
        // Arrange: Start and End after Style rather than before it.
        let source = "[Events]\n\
                      Format: Layer, Style, Start, End, Text\n\
                      Dialogue: 0,Sign,0:00:05.00,0:00:06.25,shuffled\n";

        // Act
        let script = parse_ass(source);

        // Assert
        assert_that!(script.cues.len()).is_equal_to(1);
        assert_that!(script.cues[0].start).is_equal_to(Duration::from_secs(5));
        assert_that!(script.cues[0].end).is_equal_to(Duration::from_millis(6250));
        assert_that!(script.cues[0].text.as_str()).is_equal_to("shuffled");
    }

    /// Everything malformed is skipped rather than failing the track, the way SubRip
    /// parsing is — a file with one bad line should show the rest.
    #[test]
    fn parse_ass_should_skip_what_it_cannot_read_and_keep_what_it_can() {
        // Arrange
        let source = "[Events]\n\
                      Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,before any format line\n\
                      Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
                      Dialogue: 0,not a time,0:00:02.00,Default,,0,0,0,,unparseable start\n\
                      Dialogue: 0,0:00:07.00\n\
                      Comment: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,a comment is not a cue\n\
                      [Truncated\n\
                      Dialogue: 0,0:00:09.00,0:00:10.00,Default,,0,0,0,,good\n";

        // Act
        let script = parse_ass(source);

        // Assert
        assert_that!(texts(&script.cues)).is_equal_to(vec!["good"]);
    }

    /// A script with no `[Events]` at all is an empty track, not a failure — the same
    /// answer an empty SubRip file gives.
    #[test]
    fn parse_ass_should_report_a_script_with_no_events_as_empty() {
        // Act
        let script = parse_ass("[Script Info]\nPlayResX: 1920\n");

        // Assert
        assert_that!(script.cues.as_slice()).is_empty();
        assert_that!(script.header.as_str()).contains("PlayResX: 1920");
        assert_that!(script.events_format.as_str()).is_empty();
    }

    /// Override blocks are markup rather than words, so a cue list full of them is
    /// unreadable — but the rendered cue keeps every one of them, which is what
    /// `Cue::dialogue` is for.
    #[test]
    fn ass_list_text_should_drop_the_markup_and_keep_the_words() {
        // Act / Assert
        assert_that!(plain_ass_text("{\\an8}{\\fad(200,200)}up top").as_str())
            .is_equal_to("up top");
        assert_that!(plain_ass_text("two\\Nlines").as_str()).is_equal_to("two\nlines");
        assert_that!(plain_ass_text("lower\\ncase break").as_str())
            .is_equal_to("lower\ncase break");
        assert_that!(plain_ass_text("wide\\hspace").as_str()).is_equal_to("wide space");
        // Any other backslash is a literal one — only `\N`, `\n` and `\h` mean something
        // outside an override block, and swallowing the rest would eat real characters.
        assert_that!(plain_ass_text("a\\bc").as_str()).is_equal_to("a\\bc");
        assert_that!(plain_ass_text("  padded  ").as_str()).is_equal_to("padded");
        // A stray closing brace must not swallow the line: saturating the depth is what
        // keeps an unbalanced tag from hiding every word after it.
        assert_that!(plain_ass_text("}still here").as_str()).is_equal_to("still here");
        // And an unclosed one genuinely does run to the end, which is what libass does
        // with it too.
        assert_that!(plain_ass_text("visible{\\b1 and not").as_str()).is_equal_to("visible");
    }

    #[test]
    fn ass_timestamps_should_be_written_the_way_a_dialogue_line_spells_them() {
        // Act / Assert
        assert_that!(format_ass_timestamp(Duration::ZERO).as_str()).is_equal_to("0:00:00.00");
        assert_that!(format_ass_timestamp(Duration::from_millis(4500)).as_str())
            .is_equal_to("0:00:04.50");
        assert_that!(format_ass_timestamp(Duration::from_secs(3725)).as_str())
            .is_equal_to("1:02:05.00");
        // Truncated to centiseconds, which is all the format carries.
        assert_that!(format_ass_timestamp(Duration::from_millis(1239)).as_str())
            .is_equal_to("0:00:01.23");
    }

    /// The preview burns one cue onto a frame from the middle of it, so the staged line
    /// has to be on screen whenever the grab lands — and every other column has to survive
    /// untouched, since those columns are the styling the whole feature exists to show.
    #[test]
    fn retiming_a_dialogue_line_should_move_only_its_two_time_columns() {
        // Arrange
        let script = parse_ass(ASS);
        let sign = script.cues[1].dialogue[0].clone();

        // Act
        let retimed = retimed_dialogue(
            &script.events_format,
            &sign,
            Duration::ZERO,
            Duration::from_secs(600),
        )
        .expect("a line that parsed as a cue should retime");

        // Assert
        assert_that!(retimed.as_str()).is_equal_to(
            "Dialogue: 0,0:00:00.00,0:10:00.00,Sign,,0,0,0,,{\\pos(320,10)}a sign, with a comma",
        );
    }

    /// Retiming reads the same `Format:` line the parse did, so a shuffled file has its
    /// times written back into the columns that file declares rather than the usual ones.
    #[test]
    fn retiming_should_follow_the_files_column_order_rather_than_the_usual_one() {
        // Arrange
        let events_format = "Format: Layer, Style, Start, End, Text";

        // Act
        let retimed = retimed_dialogue(
            events_format,
            "Dialogue: 0,Sign,0:00:05.00,0:00:06.25,shuffled",
            Duration::ZERO,
            Duration::from_secs(600),
        )
        .unwrap();

        // Assert
        assert_that!(retimed.as_str())
            .is_equal_to("Dialogue: 0,Sign,0:00:00.00,0:10:00.00,shuffled");
    }

    /// A line that cannot be retimed must say so rather than be staged half-rewritten:
    /// libass would draw whatever it made of it, under a cache key claiming it was right.
    #[test]
    fn retiming_should_refuse_a_line_or_a_format_it_cannot_read() {
        // Act / Assert: no usable `Format:` line.
        assert_that!(retimed_dialogue(
            "",
            "Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,text",
            Duration::ZERO,
            Duration::from_secs(1),
        ))
        .is_none();

        // Act / Assert: a format line naming no Start column.
        assert_that!(retimed_dialogue(
            "Format: Layer, Style, Text",
            "Dialogue: 0,Default,text",
            Duration::ZERO,
            Duration::from_secs(1),
        ))
        .is_none();

        // Act / Assert: not a `Dialogue:` line at all.
        assert_that!(retimed_dialogue(
            "Format: Layer, Start, End, Text",
            "Comment: 0,0:00:01.00,0:00:02.00,text",
            Duration::ZERO,
            Duration::from_secs(1),
        ))
        .is_none();

        // Act / Assert: a line with fewer columns than the format declares.
        assert_that!(retimed_dialogue(
            "Format: Layer, Start, End, Text",
            "Dialogue: 0,0:00:01.00",
            Duration::ZERO,
            Duration::from_secs(1),
        ))
        .is_none();
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

    /// The file the cue editor writes has to be one the parser reads back unchanged, or a
    /// second edit of the same track would be editing something else. Round-tripping is the
    /// only way to assert that without pinning the exact bytes of a lenient format.
    #[test]
    fn write_srt_should_produce_a_file_that_parses_back_to_the_same_cues() {
        // Arrange: two cues, one of them two lines and past the minute.
        let source = "1\n00:00:01,000 --> 00:00:02,500\nHello\nthere\n\n\
                      2\n00:01:03,250 --> 00:01:04,000\nWorld\n\n";
        let cues = parse_srt(source);

        // Act
        let written = write_srt(&cues);

        // Assert: the counters, the timings to the millisecond, and the line break inside
        // the first cue all survive.
        assert_that!(written.as_str()).contains("2\n00:01:03,250 --> 00:01:04,000\nWorld\n");
        let round_tripped = parse_srt(&written);
        assert_that!(round_tripped.len()).is_equal_to(2);
        assert_that!(texts(&round_tripped))
            .contains_exactly_in_given_order(["Hello\nthere", "World"]);
        assert_that!(round_tripped[1].start).is_equal_to(milliseconds(63_250));
        assert_that!(round_tripped[1].end).is_equal_to(milliseconds(64_000));
    }

    /// The counters a file arrives with are advisory and routinely wrong — restarted,
    /// skipped, duplicated. What is written back is numbered from one by position, so a
    /// rewritten file is a file with the defect gone rather than carried forward.
    #[test]
    fn write_srt_should_number_from_one_however_the_file_was_counted() {
        // Arrange: counters that restart mid-file.
        let cues = parse_srt(
            "7\n00:00:01,000 --> 00:00:02,000\none\n\n\
             1\n00:00:03,000 --> 00:00:04,000\ntwo\n\n",
        );

        // Act
        let written = write_srt(&cues);

        // Assert
        assert_that!(written.starts_with("1\n00:00:01,000")).is_true();
        assert_that!(written.contains("\n2\n00:00:03,000")).is_true();
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

    /// The comma is what makes it SubRip. `ffmpeg`'s demuxer reads the file this writes,
    /// and a period there is a different format entirely.
    #[test]
    fn format_srt_timestamp_should_write_subrip_time_to_the_millisecond() {
        // Act / Assert
        assert_that!(format_srt_timestamp(Duration::ZERO).as_str()).is_equal_to("00:00:00,000");
        assert_that!(format_srt_timestamp(milliseconds(62_300)).as_str())
            .is_equal_to("00:01:02,300");
        assert_that!(format_srt_timestamp(milliseconds(3_723_999)).as_str())
            .is_equal_to("01:02:03,999");
        assert_that!(format_srt_timestamp(Duration::from_secs(600)).as_str())
            .is_equal_to("00:10:00,000");
    }

    /// Whether hours appear is the caller's, not the value's: an axis that decided per
    /// reading would put `59:59` next to `1:00:00`, and the spacing between two readings
    /// is the only thing telling you what interval the axis ticks at. A short reading with
    /// hours on is therefore `0:00:59`, not `0:59`.
    #[test]
    fn format_clock_should_follow_the_callers_choice_of_hours_rather_than_the_value() {
        // Act / Assert: without hours, minutes run past sixty rather than rolling over.
        assert_that!(format_clock(Duration::ZERO, false).as_str()).is_equal_to("0:00");
        assert_that!(format_clock(milliseconds(83_900), false).as_str()).is_equal_to("1:23");
        assert_that!(format_clock(Duration::from_secs(3723), false).as_str()).is_equal_to("62:03");

        // Act / Assert: with hours, every reading carries them.
        assert_that!(format_clock(Duration::from_secs(59), true).as_str()).is_equal_to("0:00:59");
        assert_that!(format_clock(Duration::from_secs(3723), true).as_str()).is_equal_to("1:02:03");
        assert_that!(format_clock(Duration::from_secs(36_000), true).as_str())
            .is_equal_to("10:00:00");
    }

    /// Sub-second remainders are dropped rather than rounded up, so a reading can never
    /// name a moment later than the column it sits on.
    #[test]
    fn format_clock_should_truncate_toward_the_moment_it_marks() {
        // Act / Assert
        assert_that!(format_clock(milliseconds(1999), false).as_str()).is_equal_to("0:01");
        assert_that!(format_clock(milliseconds(59_999), true).as_str()).is_equal_to("0:00:59");
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
    fn group_overlaps_should_give_every_cue_of_a_clean_track_its_own_group() {
        // Arrange: the ordinary dialogue track, where nothing shares the screen.
        let cues = [cue(0, 1000), cue(2000, 3000), cue(4000, 5000)];

        // Act
        let groups = group_overlaps(&cues);

        // Assert
        assert_that!(groups).contains_exactly_in_given_order([
            CueGroup { first: 0, len: 1 },
            CueGroup { first: 1, len: 1 },
            CueGroup { first: 2, len: 1 },
        ]);
    }

    /// The same boundary `pack_lanes` draws: a cue starting exactly where the last one
    /// ended is never on screen with it. Getting this wrong would collapse most of an
    /// ordinary track into one enormous group.
    #[test]
    fn group_overlaps_should_not_group_a_cue_that_starts_exactly_where_the_last_one_ended() {
        // Arrange
        let cues = [cue(0, 1000), cue(1000, 2000)];

        // Act
        let groups = group_overlaps(&cues);

        // Assert
        assert_that!(groups).contains_exactly_in_given_order([
            CueGroup { first: 0, len: 1 },
            CueGroup { first: 1, len: 1 },
        ]);
    }

    #[test]
    fn group_overlaps_should_collect_a_run_of_cues_that_share_the_screen() {
        // Arrange: a lone line, then three that overlap in a chain, then a lone line.
        let cues = [
            cue(0, 1000),
            cue(2000, 5000),
            cue(3000, 6000),
            cue(4000, 7000),
            cue(8000, 9000),
        ];

        // Act
        let groups = group_overlaps(&cues);

        // Assert
        assert_that!(groups).contains_exactly_in_given_order([
            CueGroup { first: 0, len: 1 },
            CueGroup { first: 1, len: 3 },
            CueGroup { first: 4, len: 1 },
        ]);
    }

    /// The run is measured against its furthest end, not its last cue's. A sign covering a
    /// whole scene keeps every line spoken under it in one group, which is what the reader
    /// sees — pairing each cue off against its immediate predecessor would split them.
    #[test]
    fn group_overlaps_should_reach_from_the_runs_furthest_end_rather_than_its_last_cue() {
        // Arrange: the sign runs 0–10s; neither line under it overlaps the other.
        let cues = [cue(0, 10_000), cue(1000, 2000), cue(3000, 4000)];

        // Act
        let groups = group_overlaps(&cues);

        // Assert
        assert_that!(groups).contains_exactly_in_given_order([CueGroup { first: 0, len: 3 }]);
    }

    #[test]
    fn group_overlaps_should_report_nothing_for_an_empty_track() {
        // Act / Assert
        assert_that!(group_overlaps(&[])).is_empty();
    }

    #[test]
    fn cue_group_should_report_its_end_and_which_cues_it_holds() {
        // Arrange
        let group = CueGroup { first: 2, len: 3 };

        // Act / Assert
        assert_that!(group.end()).is_equal_to(5);
        assert_that!(group.holds(1)).is_false();
        assert_that!(group.holds(2)).is_true();
        assert_that!(group.holds(4)).is_true();
        assert_that!(group.holds(5)).is_false();
    }

    #[test]
    fn format_compact_should_drop_the_hours_field_when_the_caller_says_to() {
        // Act / Assert
        assert_that!(format_compact(Duration::ZERO, false).as_str()).is_equal_to("0:00.0");
        assert_that!(format_compact(milliseconds(62_300), false).as_str()).is_equal_to("1:02.3");
        // Past the hour without hours: the minutes carry on counting rather than wrapping,
        // so the reading stays truthful even where the caller sized the block wrongly.
        assert_that!(format_compact(milliseconds(3_723_999), false).as_str())
            .is_equal_to("62:03.9");
    }

    #[test]
    fn format_compact_should_keep_the_hours_field_when_the_caller_asks_for_it() {
        // Act / Assert
        assert_that!(format_compact(Duration::ZERO, true).as_str()).is_equal_to("0:00:00.0");
        assert_that!(format_compact(milliseconds(3_723_999), true).as_str())
            .is_equal_to("1:02:03.9");
    }

    #[test]
    fn the_window_should_show_the_whole_media_when_it_is_shorter_than_the_window() {
        // Act
        let window = TimelineWindow::fitted(
            &cue(1000, 2000),
            &[cue(1000, 2000)],
            Duration::from_secs(30),
            100,
            1,
        );

        // Assert
        assert_that!(window.start).is_equal_to(Duration::ZERO);
        assert_that!(window.end).is_equal_to(Duration::from_secs(30));
    }

    /// A minute of a typeset track holds several hundred events, which no width can draw as
    /// spans — every lane fills end to end and the pane becomes a texture. The window
    /// shortens until what is in it can be drawn, and a sparse track is left alone, so the
    /// choice follows the track's actual density rather than its format.
    #[test]
    fn the_window_should_shorten_for_a_track_too_dense_to_draw() {
        // Arrange: a cue every half second for ten minutes, and the same span with four
        // cues in it.
        let dense: Vec<Cue> = (0..1200)
            .map(|n| cue(n * 500, n * 500 + 400))
            .collect::<Vec<_>>();
        let sparse: Vec<Cue> = (0..4).map(|n| cue(n * 20_000, n * 20_000 + 2000)).collect();
        let media = Duration::from_secs(600);

        // Act: lanes taken from the packing rather than assumed, since the capacity a
        // window is measured against is every lane's columns.
        let lanes = |cues: &[Cue]| pack_lanes(cues, MAX_LANES).lane_count;
        let crowded = TimelineWindow::fitted(&dense[600], &dense, media, 120, lanes(&dense));
        let roomy = TimelineWindow::fitted(&sparse[2], &sparse, media, 120, lanes(&sparse));

        // Assert
        assert_that!(crowded.end - crowded.start).is_equal_to(WINDOW_STEPS[3]);
        assert_that!(roomy.end - roomy.start).is_equal_to(WINDOW);
    }

    /// Shortening past the selected cue would clamp it at both edges and draw it as a bar
    /// with no ends, losing the one timing the reader came for. A sign covering a scene on
    /// a dense track therefore keeps a long window and stays crowded.
    #[test]
    fn the_window_should_never_shorten_past_the_selected_cue() {
        // Arrange: a dense track, with a twenty-second sign over it.
        let mut cues: Vec<Cue> = (0..1200).map(|n| cue(n * 500, n * 500 + 400)).collect();
        cues.push(cue(295_000, 315_000));
        let sign = cues.last().unwrap().clone();

        // Act
        let lanes = pack_lanes(&cues, MAX_LANES).lane_count;
        let window = TimelineWindow::fitted(&sign, &cues, Duration::from_secs(600), 120, lanes);

        // Assert: the shortest step that still holds the sign and half again.
        assert_that!(window.end - window.start).is_equal_to(WINDOW_STEPS[1]);
        assert_that!(window.span(&sign)).is_some();
    }

    #[test]
    fn the_window_should_place_the_selected_cue_in_the_middle() {
        // Act
        let window = TimelineWindow::fitted(
            &cue(600_000, 602_000),
            &[cue(600_000, 602_000)],
            Duration::from_secs(1200),
            100,
            1,
        );

        // Assert
        assert_that!(window.start).is_equal_to(Duration::from_secs(571));
        assert_that!(window.end).is_equal_to(Duration::from_secs(631));
    }

    /// Without clamping, an early cue produces a window starting before zero, wasting
    /// half the track on time that does not exist.
    #[test]
    fn the_window_should_clamp_to_the_start_of_the_media() {
        // Act
        let window = TimelineWindow::fitted(
            &cue(2000, 3000),
            &[cue(2000, 3000)],
            Duration::from_secs(1200),
            100,
            1,
        );

        // Assert
        assert_that!(window.start).is_equal_to(Duration::ZERO);
        assert_that!(window.end).is_equal_to(WINDOW);
    }

    #[test]
    fn the_window_should_clamp_to_the_end_of_the_media() {
        // Act
        let window = TimelineWindow::fitted(
            &cue(1_199_000, 1_200_000),
            &[cue(1_199_000, 1_200_000)],
            Duration::from_secs(1200),
            100,
            1,
        );

        // Assert
        assert_that!(window.start).is_equal_to(Duration::from_secs(1140));
        assert_that!(window.end).is_equal_to(Duration::from_secs(1200));
    }

    /// Subtitles running past the last video frame are common, and a probed duration can
    /// be missing entirely. The window still has to contain the cue it was built for.
    #[test]
    fn the_window_should_still_contain_a_cue_that_outlasts_the_media() {
        // Act
        let window = TimelineWindow::fitted(
            &cue(9000, 10_000),
            &[cue(9000, 10_000)],
            Duration::ZERO,
            100,
            1,
        );

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
