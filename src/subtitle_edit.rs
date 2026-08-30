//! State for the subtitle edit page: which cues a track holds, which one is selected,
//! and the scratch directory the preview worker stages files in.
//!
//! Kept out of `App` as one owned struct rather than a dozen loose fields, for the same
//! reason `staging::BatchState` is: the page's lifetime is a single `Option`, so every
//! way of leaving it — Esc, selecting another file, quitting — releases the whole thing
//! including its temp directory, without each exit path having to remember to.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ratatui::layout::Size;
use ratatui_image::protocol::Protocol;
use ratatui_image::{FilterType, Resize};

use crate::app::SearchState;
use crate::audio::{AudioOutput, AudioSource, frame_index_at};
use crate::cue::{
    Cue, CueGroup, LaneLayout, MAX_LANES, fold_case, group_overlaps, pack_lanes, shares_screen,
};
use crate::preview::{
    CueStyle, FRAME_WINDOW_BUDGET, FrameSource, FrameTarget, PlaybackAnchor, PlaybackFrames,
    ScrubTarget, seek_ceiling, seek_for,
};
use crate::subtitle::SubtitleSource;

/// How long the selection has to stop moving before a frame is asked for.
///
/// Matches the file list's probe debounce (`App::start_pending_probe`). Walking a cue list
/// with `j` held down would otherwise start an `ffmpeg` per repeat, each one an accurate
/// seek that on a network mount reads from the preceding keyframe.
///
/// Only frames that would have to be rendered wait it out. One already in the frame cache
/// costs a file read and an encode — a few milliseconds — so `App::start_pending_preview`
/// asks for it straight away, which is what makes walking an already-rendered track feel
/// like scrolling rather than like loading.
pub const FRAME_DEBOUNCE: Duration = Duration::from_millis(120);

/// How many cues either side of the selection are kept encoded and ready to draw.
///
/// The point of the window is that moving the cursor draws a picture in the *same* frame
/// the keypress is handled in, with no worker round trip at all. Two rather than one so a
/// cursor moving faster than the worker answers still lands inside it; small because a
/// `Protocol` holds the picture encoded for the pane, and a full-screen preview is several
/// megabytes each.
pub const NEARBY_FRAMES: usize = 2;

/// How far one press of `h`/`l` moves a cue in timing mode.
///
/// Fifty milliseconds is a little over one frame of film and a little under two of PAL
/// video, which is about as fine as a reader can judge a subtitle against a mouth movement.
/// Coarser and a line can never be landed exactly; finer and a half-second correction —
/// the common one — becomes twenty presses instead of ten.
pub const TIMING_STEP: Duration = Duration::from_millis(50);

/// How many steps `H`/`L` move in one press.
pub const TIMING_LEAP: i64 = 10;

/// The shortest a cue may be made.
///
/// One [`TIMING_STEP`], which is the finest length this page can express at all: a cue held
/// at the floor can still be lengthened by exactly one press, and no run of presses can
/// collapse a line to nothing. The floor exists because the two resize keys move one end
/// each — without it, holding `}` down walks the end past the start and writes a cue whose
/// `-->` line runs backwards, which is a file no player will read and which nothing later in
/// the save would notice.
pub const MIN_CUE_LENGTH: Duration = TIMING_STEP;

/// Which end of a cue the resize keys move.
///
/// `Ctrl+H`/`Alt+H` move the start and `Ctrl+L`/`Alt+L` the end: the letter picks the edge,
/// keeping the left-and-right sense the bare `h`/`l` carry on this page, and the modifier
/// picks the direction — `Ctrl` pushes that edge outwards and `Alt` pulls it back in.
///
/// A separate axis from [`SubtitleEditState::nudge_selected`] rather than a mode inside it,
/// because moving a cue and resizing it are two different questions about the same line —
/// *when* it is on screen and *how long* — and a reader correcting one is usually about to
/// check the other.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CueEdge {
    Start,
    End,
}

/// How far one press of `h`/`l` moves the timeline cursor.
///
/// **Ten times [`TIMING_STEP`], and deliberately not the same figure.** Nudging a cue is
/// aimed at a mouth movement, where fifty milliseconds is about as fine as the eye can
/// judge; the timeline cursor is aimed at a *shot*, and is there to reach parts of the file
/// the cue list does not point at. At fifty milliseconds a minute of film is twelve hundred
/// presses, which is not roaming, it is grinding.
pub const TIMELINE_STEP: Duration = Duration::from_millis(500);

/// How many steps `H`/`L` move the timeline cursor in one press.
///
/// Five seconds a press, which crosses a scene rather than a line.
pub const TIMELINE_LEAP: i32 = 10;

/// How far one press of `Ctrl+H`/`Ctrl+L` moves the timeline cursor.
///
/// **Exactly [`TIMING_STEP`], and that is the point rather than a coincidence.** The coarse
/// step reaches a shot and this one reaches a frame, which is what the reader needs once the
/// shot is found — and the workflow the whole pane exists for is scrubbing onto the frame a
/// line should land on and then nudging the cue there. Moving the two by different amounts
/// would mean the cursor could stand where no number of nudges can put the cue.
pub const TIMELINE_FINE_STEP: Duration = TIMING_STEP;

/// Which pane of the subtitle edit page holds the cursor.
///
/// The cue panel by default, which is every movement the page had before: `Ctrl+J` hands the
/// cursor to the timeline and `Ctrl+K` takes it back. A pane holding the cursor **owns the
/// keys that mean movement in it**, which is why `h`/`l` mean something different in each and
/// why the timeline answers nothing at all to `j`/`k` — it has no vertical axis for them to
/// move along.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum EditFocus {
    #[default]
    Cues,
    Timeline,
}

/// How much of the track the timing mode moves.
///
/// **One mode at two scales rather than two modes.** `t` turns it on at [`Self::Cue`] and `T`
/// at [`Self::Track`]; a value can hold only one of them, so `h`/`l` never have two meanings
/// to arbitrate between and there is one mode to describe rather than two that have to be
/// kept consistent with each other. Everything else about the mode is the same at both
/// scales: the same keys, the same colour swap, the same `Esc` to leave it.
///
/// The wide scale exists because the commonest defect in a subtitle file is not a wrong line
/// but a whole file a second or two out. At [`Self::Cue`] that costs one press of `h` per
/// cue, which for a feature film is a thousand presses to fix one mistake.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TimingScope {
    /// Not retiming: `h`/`l` move between cues that share a moment.
    #[default]
    Off,
    /// `h`/`l` move the selected cue.
    Cue,
    /// `h`/`l` move every cue in the track by the same amount.
    Track,
}

impl TimingScope {
    /// Whether the timing mode is on at either scale.
    ///
    /// The page asks this far more often than it asks which scale — the colour swap, the
    /// yellow title and `Esc`'s peel are all about the mode rather than about how much of it
    /// moves.
    pub fn is_on(self) -> bool {
        self != Self::Off
    }
}

/// Where one row of the page's cue list came from.
///
/// The page's list and the *file's* stop being the same list the moment a cue is inserted:
/// the new row takes its place in time, and every file cue after it moves down one. A staged
/// rewrite is addressed by the cue's position in the file ([`crate::subtitle::CueEdit`]), so
/// something has to remember which is which — this is it, kept parallel to
/// [`SubtitleEditState::cues`] so a row and its origin cannot come apart.
///
/// Renumbering the file positions on an insertion instead would be the same information
/// stored twice, and would silently rewrite the keys of edits already staged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CueOrigin {
    /// A cue the file holds, at this position in the parsed list.
    File(usize),
    /// A cue the reader added this session, under this insertion id.
    Inserted(usize),
}

/// How long an inserted cue is given.
///
/// A cue has to have a length before it can be judged against the picture, and two seconds is
/// about what an ordinary line of dialogue runs for — long enough to be visible on the
/// timeline at any window the pane picks, short enough not to swallow its neighbours into one
/// overlap group. It is a starting point rather than a guess at the reader's intent: the cue
/// is retimed with `t` like any other from the moment it exists.
pub const INSERT_DURATION: Duration = Duration::from_secs(2);

/// How many cues of one overlap group the panel draws side by side.
///
/// **Two is forced by the width, not chosen.** The cue panel is thirty to forty-eight
/// columns (`CUE_PANEL_WIDTH`), so two blocks get fourteen to twenty-three each and a
/// third would leave ten to sixteen — which cannot hold a timing at all, and a block with no
/// timing on this page is a block with nothing worth reading on it. Members past the two are
/// reached with `h`/`l`, and the panel marks that they are there.
pub const GROUP_COLUMNS: usize = 2;

/// Rows one cue's block occupies: two borders with a line of text between them.
pub const CUE_BLOCK_ROWS: usize = 3;

/// Rows a group of overlapping cues occupies: a block, plus the row the later-starting
/// member is dropped by.
pub const CUE_GROUP_ROWS: usize = CUE_BLOCK_ROWS + 1;

/// Rows the fork at the head of a group costs: its crossbar, and the arrows down into each
/// member.
pub const CUE_FORK_ROWS: usize = 2;

/// Rows the `↓` between one row of the panel and the next costs.
pub const CUE_CONNECTOR_ROWS: usize = 1;

/// How far the background pass has got in rendering the track's frames.
///
/// Its own enum rather than a pair of counters, because "not running" has three different
/// meanings on this page and only one of them is worth a word on screen: a network mount
/// is a decision the user did not make and would otherwise be left wondering about,
/// whereas a build that cannot draw images at all already shows text previews everywhere.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WarmState {
    /// No background pass, and nothing to say about it: the terminal draws no images, the
    /// FFmpeg build cannot burn subtitles in, or the user turned prefetching off.
    Off,
    /// No background pass because the media is on a network mount, where rendering a
    /// frame per cue means a thousand accurate seeks across the network. Said out loud on
    /// the page, since the frames the user does land on still appear.
    OffForNetwork,
    /// `done` counts cues the pass has finished with, however it finished with them.
    Working {
        done: usize,
        total: usize,
    },
    Done,
}

/// Whether this page can ever draw a frame, and if not, why.
///
/// Decided once when the page opens and fixed for its lifetime — neither the FFmpeg build
/// nor the terminal's image support changes while it is open. That permanence is the whole
/// reason it is drawn *in* the preview pane while a per-cue failure is not: a message that
/// never changes reads as an explanation, where one that changed as the cursor moved read
/// as the flicker this page had text removed from it to stop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreviewSupport {
    /// Frames will be drawn.
    Available,
    /// This FFmpeg has no `subtitles` filter, so a cue cannot be burned onto a frame at
    /// all. The one reachable case in a shipped build.
    NoSubtitleBurn,
    /// Nothing to draw an image with. The answer for every terminal that offers no image
    /// protocol: `preview::drawing_picker` refuses a halfblocks fallback at startup, since
    /// two coloured half-cells cannot show a subtitle burned into a frame, and this page
    /// exists to judge exactly that. Said once and kept on screen, because silence here is
    /// indistinguishable from a slow render.
    NoImageProtocol,
}

impl PreviewSupport {
    /// What to tell the user, or `None` when frames are coming.
    pub fn reason(self) -> Option<&'static str> {
        match self {
            Self::Available => None,
            Self::NoSubtitleBurn => Some(
                "Preview is not possible: this FFmpeg was built without libass, so a cue cannot be drawn onto a frame.",
            ),
            Self::NoImageProtocol => {
                Some("Preview is not possible: this terminal cannot display images.")
            }
        }
    }
}

/// How far the page has got in loading a track's cues.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoadStatus {
    /// Cues are being extracted or read. The page is open and drawn during this.
    Preparing,
    Ready,
    /// The track parsed but holds no cues — an empty or wholly unparseable file.
    /// Distinct from `Failed`, because nothing went wrong, there is just nothing to see.
    Empty,
    Failed(String),
}

/// A scratch directory for one open page, removed when the page closes.
///
/// Lives under the system temp directory rather than beside the media: the name is then
/// entirely ours, which is what lets the preview worker hand `ffmpeg` a bare relative
/// filename instead of escaping a user path through the filter-graph syntax. It also
/// keeps the directory monitor from ever seeing these files.
#[derive(Debug)]
pub struct PreviewWorkspace(PathBuf);

impl PreviewWorkspace {
    pub fn new() -> std::io::Result<Self> {
        let path = std::env::temp_dir()
            .join("reel-tui-preview")
            .join(unique_name());
        std::fs::create_dir_all(&path)?;
        Ok(Self(path))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for PreviewWorkspace {
    fn drop(&mut self) {
        // Best-effort: a page closing is not a place to surface a failed unlink, and the
        // directory holds nothing but a copy of subtitles the user already has.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn unique_name() -> String {
    // The counter is not belt-and-braces: the clock alone is not enough. Two calls in the
    // same nanosecond read the same timestamp, and then two pages share a scratch
    // directory — so the first to close deletes the second's staged subtitles out from
    // under it. Rare enough to look like a flake and not like a bug, which is exactly why
    // it is worth ruling out rather than relying on the clock's granularity.
    static WORKSPACES: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        WORKSPACES.fetch_add(1, Ordering::Relaxed)
    )
}

/// A drawn frame and the cue it belongs to.
///
/// A struct with its own `Debug` rather than a tuple in the state, because `Protocol`
/// holds encoded image data and has none of its own — and without this the whole page
/// state would lose its derived `Debug`, which every test failure message prints.
struct Frame {
    cue_index: usize,
    protocol: Box<Protocol>,
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Frame")
            .field("cue_index", &self.cue_index)
            .field("size", &self.protocol.size())
            .finish()
    }
}

/// A drawn frame and the moment of the media it is of.
///
/// [`Frame`]'s sibling for the timeline cursor, which names a moment rather than a cue. Its
/// own `Debug` for the same reason: `Protocol` has none, and without this the page state
/// would lose the derived one that every test failure message prints.
struct ScrubFrame {
    at: Duration,
    protocol: Box<Protocol>,
}

impl std::fmt::Debug for ScrubFrame {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ScrubFrame")
            .field("at", &self.at)
            .field("size", &self.protocol.size())
            .finish()
    }
}

/// One span, decoded and playing.
///
/// Holds the whole span's raw pixels rather than encoding them all up front: a protocol is
/// several times the size of the pixels it came from, and only one of them is ever on
/// screen. The frame under the playhead is encoded as it is reached and kept until the
/// playhead leaves it, so a step that does not cross a frame boundary costs nothing at all.
pub struct Playback {
    /// What this span was played for — a cue, or a moment the timeline cursor stood on. A
    /// span that arrives after the page has moved on is dropped rather than played under
    /// something it is not about.
    anchor: PlaybackAnchor,
    frames: PlaybackFrames,
    /// Where the sound is, which is the only clock here — see [`crate::audio`]. Held so
    /// that dropping the page stops the device, whichever way the page was left.
    output: Box<dyn AudioOutput>,
    /// Where [`Self::output`] came from, kept so a looping playback can open another.
    ///
    /// A device plays its buffer once, so going round again means a new output rather than
    /// a rewound one. It costs a device open and nothing else: the frames and the samples
    /// are already in memory, and no `ffmpeg` runs a second time.
    source: Box<dyn AudioSource>,
    /// Whether running off the end starts the span again instead of ending it.
    looping: bool,
    /// Which frame [`Self::drawn`] holds, so a step inside one frame period re-draws
    /// nothing.
    shown: Option<usize>,
    /// The cell area [`Self::drawn`] was encoded for, which is the pane as it was when the
    /// frame went up rather than as it was when the span was asked for.
    ///
    /// The two differ routinely: saying "Preparing playback…" puts a status row on the page
    /// and takes a row off the preview pane, so the pane the user sees a playback in is
    /// almost never the one the playback was requested against.
    cells: Size,
    drawn: Option<Box<Protocol>>,
}

/// What one step of a playback did, which is what tells the page whether to repaint and
/// when to let go of the device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackStep {
    /// Either the sound has not started yet, or the playhead is still inside the frame
    /// already on screen. Nothing to redraw.
    Unchanged,
    Drew,
    /// The playhead has run off the end of the span.
    Finished,
}

impl Playback {
    pub fn new(
        anchor: PlaybackAnchor,
        frames: PlaybackFrames,
        source: Box<dyn AudioSource>,
        looping: bool,
    ) -> Self {
        let cells = frames.cells;
        let output = source.open();
        Self {
            anchor,
            frames,
            output,
            source,
            looping,
            shown: None,
            cells,
            drawn: None,
        }
    }

    pub fn anchor(&self) -> PlaybackAnchor {
        self.anchor
    }

    /// Where in the media the playhead is, or `None` before the sound has started.
    ///
    /// Derived from the frame on screen rather than from the clock directly, so the picture
    /// and the playhead in one drawn frame describe the same instant — a playhead read
    /// straight off the clock would sit up to a frame period ahead of the picture beside it.
    /// Multiplied by the span's speed, because frames are `1 / fps` apart in *output* time
    /// and this answers in *media* time. At half speed the hundredth frame is fifty frames'
    /// worth into the media, not a hundred — without the factor the playhead would crawl
    /// along the timeline at the speed the picture is playing at and read as the cue being
    /// mistimed, which is the one thing this page must never say wrongly.
    pub fn position(&self) -> Option<Duration> {
        let shown = self.shown?;
        if self.frames.fps == 0 {
            return Some(self.frames.span_start);
        }
        Some(
            self.frames.span_start
                + Duration::from_secs_f64(
                    shown as f64 / f64::from(self.frames.fps) * self.frames.speed.as_f64(),
                ),
        )
    }

    pub fn frame(&self) -> Option<&Protocol> {
        self.drawn.as_deref()
    }

    /// Moves the playhead to wherever the sound has got to, encoding the frame there.
    ///
    /// The clock is sampled once, here, rather than by each thing that wants to know: two
    /// reads a few microseconds apart could straddle a frame boundary and put the picture
    /// and the playhead on either side of it.
    pub fn advance(&mut self, now: Instant, cells: Size) -> PlaybackStep {
        if cells.width == 0 || cells.height == 0 {
            // The renderer has not measured the pane yet. Encoding for no cells trips a
            // `debug_assert!` inside `ratatui-image`, and there is nowhere to draw anyway.
            return PlaybackStep::Unchanged;
        }
        let Some(position) = self.output.position(now) else {
            // The device has not started yet. Nothing is drawn during this rather than the
            // first frame being held up: starting the picture before the sound is exactly
            // the error the clock exists to prevent.
            return PlaybackStep::Unchanged;
        };
        let mut index = frame_index_at(position, self.frames.fps);
        if index >= self.frames.count() {
            if !self.looping {
                return PlaybackStep::Finished;
            }
            // Round again, from a *fresh* output: a device plays its buffer once, so there
            // is nothing to rewind. Clearing `shown` is what makes the fall-through below
            // encode frame zero rather than decide nothing has changed since the last frame
            // of the pass that just ended.
            self.output = self.source.open();
            self.shown = None;
            self.drawn = None;
            index = 0;
        }
        if self.shown == Some(index) && self.cells == cells {
            return PlaybackStep::Unchanged;
        }
        self.cells = cells;
        let Some(drawn) = self.encode(index) else {
            // Unreachable for an index inside the span — the size was fixed before the
            // decode and the slice is exactly that many bytes — so there is nothing to
            // report and nothing to recover. Ending is what stops it being asked again for
            // every frame of the rest of the span.
            return PlaybackStep::Finished;
        };
        self.shown = Some(index);
        self.drawn = Some(drawn);
        PlaybackStep::Drew
    }

    /// One frame's pixels as something the terminal can draw.
    ///
    /// The frame is drawn as pixels, through whatever image protocol the terminal offered
    /// — which is the only way a burned-in subtitle is *readable* rather than merely
    /// present. The span was decoded at [`crate::preview::playback_pixels`] for this very
    /// picker, so when the pane has not moved `Resize::Scale` has nothing left to scale and
    /// the picture reaches the terminal untouched; when it has, this is the single pass
    /// that absorbs it.
    ///
    /// There is no halfblocks case because there is no halfblocks *page*: `drawing_picker`
    /// refuses that terminal at startup, and the subtitle edit page says why instead of rendering
    /// something nobody could read.
    fn encode(&self, index: usize) -> Option<Box<Protocol>> {
        let (width, height) = self.frames.pixels;
        let bytes = self.frames.frame(index)?;
        let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_raw(
            width,
            height,
            bytes.to_vec(),
        )?);
        let protocol = self
            .frames
            .picker
            .new_protocol(image, self.cells, Resize::Scale(Some(FilterType::Triangle)))
            .ok()?;
        Some(Box::new(protocol))
    }
}

/// `Protocol` has no `Debug` and the frame buffer is megabytes of pixels, so this reports
/// what a reader of a failure message actually wants: what it is of, how far in, out of what.
impl std::fmt::Debug for Playback {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Playback")
            .field("anchor", &self.anchor)
            .field("shown", &self.shown)
            .field("frames", &self.frames)
            .finish()
    }
}

/// Whether the page is playing a span, getting ready to, or neither.
///
/// One field rather than a set of flags, so the audio device is released by every way of
/// leaving the page — the same reason the page itself is one `Option` on `App`.
#[derive(Debug, Default)]
pub enum PlaybackState {
    #[default]
    Idle,
    /// The span is being decoded. The still frame stays on screen throughout, so the page
    /// does not blank for the second or two this takes.
    Preparing {
        anchor: PlaybackAnchor,
    },
    Playing(Playback),
}

/// Everything the subtitle edit page draws and navigates.
#[derive(Debug)]
pub struct SubtitleEditState {
    /// Which page opening this is. Echoed by every worker message so results belonging
    /// to a page the user has already left can be dropped rather than applied.
    pub generation: u64,
    /// Everything a frame grab needs about the media: the file itself — the container for
    /// an embedded track, or the sidecar's companion media — how to tell it has changed,
    /// and the size its frames are rendered at.
    pub frames: FrameSource,
    pub source: SubtitleSource,
    pub duration: Duration,
    pub status: LoadStatus,
    /// Whether frames can be drawn at all, and if not, why. Fixed for the page's life.
    pub support: PreviewSupport,
    /// How far the background frame pass has got, or why there is not one.
    pub warm: WarmState,
    pub cues: Vec<Cue>,
    /// Where each entry of [`Self::cues`] came from, parallel to it and always the same
    /// length. See [`CueOrigin`].
    origins: Vec<CueOrigin>,
    /// Whether `h`/`l` move cues through time rather than through a group, and how many.
    ///
    /// A mode rather than a dialog because it has to coexist with a playback: no dialog may
    /// be raised over one ([`crate::app::App::playback_in_progress`]), and the work this is
    /// for is alternating a shift with a `p` until the line lands.
    ///
    /// Sticky, and follows the cursor: `j`/`k` still walk the track, so a whole file can be
    /// retimed without turning the mode on again at every cue.
    pub timing: TimingScope,
    /// How far global retiming has moved the track, in milliseconds, signed.
    ///
    /// Kept rather than derived because it cannot be derived once hand nudges are mixed in:
    /// [`crate::app::App::selected_cue_shift`] answers for one cue, and with some cues moved
    /// individually there is no cue whose shift is the track's. Reset when a new track
    /// arrives and when the track's timings are put back, and deliberately **untouched by a
    /// per-cue nudge** — it answers for the track, not for whatever the cursor is on.
    pub track_shift: i64,
    pub layout: LaneLayout,
    /// The cue list split into runs that share the screen, parallel to nothing — each cue
    /// is in exactly one group and the ordinary cue is a group of one.
    ///
    /// The cue panel draws a group as its unit rather than a cue, so this is what its
    /// scrolling and its `j`/`k` movement are measured in.
    pub groups: Vec<CueGroup>,
    /// Which groups the cue panel is drawing, as positions in [`Self::groups`], ascending.
    ///
    /// The panel's filter is a *view* rather than an edit: `cues` and `groups` keep every
    /// entry the track has, and this says which of them the reader asked to see. That is
    /// what lets the timeline, the frame window, the staged edits and the cache keys go on
    /// addressing the whole track while the panel shows part of it — a filter that removed
    /// cues instead would move every one of those out from under itself.
    ///
    /// `(0..groups.len())` while no query is in force, so every path that reads it is the
    /// same code filtered or not, rather than a `None` arm shadowing the ordinary one.
    visible: Vec<usize>,
    /// The cue panel's filter bar.
    ///
    /// The page's own rather than [`crate::app::App`]'s, unlike the file list's and the
    /// keybindings popup's: the renderer holds this page mutably while it draws the panel,
    /// so a bar owned by `App` could not be measured there — and a filter dropped with the
    /// page is one that cannot greet the reader on their next visit.
    cue_search: SearchState,
    /// The cue the search was opened on, for the `Esc` that abandons it.
    ///
    /// A [`CueOrigin`] rather than a position, so it survives the list shifting under it —
    /// the same reason every other claim on this page about "which cue" is one.
    search_origin: Option<CueOrigin>,
    pub selected: usize,
    /// Which page of the selected group's members the panel is showing, counted in pages of
    /// [`GROUP_COLUMNS`] from the group's first member.
    ///
    /// Kept rather than derived from `selected`, because the two cues on screen do not
    /// partition the group: the last page backs up to keep two blocks drawn, so a middle
    /// member belongs to the page before it *and* the one after, and which of them the
    /// reader is looking at is a fact about how they got there. Deriving it made `h` from
    /// the right-hand block of a backed-up page turn the page instead of moving to the
    /// block beside it, which is the one thing paging exists to avoid.
    ///
    /// Reset to zero wherever the cursor enters a group with no remembered position — a
    /// group first arrived at is entered on its first member, which is on page zero by
    /// construction.
    group_page: usize,
    /// Where the cursor was left in each group: its cue and its page, keyed by the group's
    /// position in `groups`.
    ///
    /// `j` out of a group and `k` back into it returns to the cue that was under the cursor
    /// rather than to the group's first member. Without it, stepping down a row to look at
    /// something and coming back cost the reader their place sideways — on a group of six
    /// that is five presses of `l` to undo one press of `k`, every time.
    ///
    /// Cleared with the cue list, since the keys are positions in a `groups` that a new
    /// track rebuilds from scratch.
    group_memory: HashMap<usize, (usize, usize)>,
    /// Where the cursor should land once this page's cues arrive, when the page is being
    /// opened again rather than for the first time — after a save, which rewrites the file
    /// and so has to close and reload it. See [`Self::restore_selection`].
    restore: Option<usize>,
    /// First group drawn, moved only to keep `selected` on screen.
    ///
    /// Counted in groups rather than cues, because a group is one row of the panel however
    /// many cues it holds.
    pub list_scroll: usize,
    /// Groups the cue list can show, measured by the renderer.
    pub list_rows: usize,
    /// Size of the preview pane in terminal cells, measured by the renderer.
    ///
    /// Recorded here because the frame grab has to scale to a pane whose size only the
    /// renderer knows, and because a change to it is what tells the worker the frame it
    /// already produced is the wrong size now.
    pub preview_cells: Size,
    /// The frames encoded and ready to draw: the selected cue's, and up to
    /// [`NEARBY_FRAMES`] either side of it.
    ///
    /// A window rather than the single frame on screen, because a round trip to the worker
    /// — even one that only reads the cache — is a visible gap on every cursor move. Kept
    /// pruned to the window by [`Self::prune_frames`], so it is bounded by how far the
    /// cursor is from what has been drawn rather than by how long the page has been open.
    encoded: Vec<Frame>,
    /// Why the cue at this index has no frame, for the one it was reported against.
    ///
    /// Keyed on the cue for the same reason `encoded` is: the reason a *different* line
    /// could not be drawn says nothing about the one under the cursor, and showing it
    /// there would blame the wrong cue.
    frame_error: Option<(usize, String)>,
    /// Set when something invalidated the frames on hand — the selection moved, the cues
    /// arrived, the pane resized — and cleared when the request is actually sent.
    frame_pending_since: Option<Instant>,
    /// Set when the frame cache has gained something the window is still missing, and
    /// cleared when the request is sent.
    ///
    /// Separate from `frame_pending_since` because it asks for strictly less. The
    /// background pass reports progress ten times a second, and letting that re-ask for
    /// the *selected* cue would put an `ffmpeg` per report behind a cue that failed to
    /// draw — the one case where the selected frame is missing and asking again cannot
    /// help. Getting the neighbours ready is always safe: they only ever come from the
    /// cache.
    refill_nearby: bool,
    /// Roughly what one encoded frame costs per cell of the preview pane, from the
    /// terminal's image protocol and font size — see
    /// [`PreviewHandles::frame_bytes_per_cell`]. Multiplied by the pane the renderer
    /// measured, this is what sizes the window; zero means nothing is being encoded, so
    /// there is nothing to bound.
    ///
    /// [`PreviewHandles::frame_bytes_per_cell`]: crate::preview::PreviewHandles::frame_bytes_per_cell
    frame_cost: u64,
    /// The scrub playback, if one is running or being decoded.
    playback: PlaybackState,
    /// Which pane holds the cursor. See [`EditFocus`].
    pub focus: EditFocus,
    /// Where the timeline cursor stands, in media time.
    ///
    /// **Re-seeded from the selected cue every time the cursor enters the timeline**
    /// ([`Self::focus_timeline`]) rather than remembered across visits. Entering the pane
    /// then changes nothing on screen — the moment it lands on is the one the still already
    /// shows — where a remembered position would come back pointing at wherever the reader
    /// happened to leave it, which after a few `j`s is nowhere near what they are looking at.
    cursor: Duration,
    /// Where the timeline's window begins while the cursor is in it, once a draw has settled
    /// on one.
    ///
    /// **The scroll position stops belonging to the selected cue the moment the timeline
    /// takes the cursor.** `TimelineWindow::fitted` anchors a window on the selection, and a
    /// window rebuilt from it on every draw and then slid the minimum needed to hold the
    /// cursor puts the cursor hard against whichever edge it left by — *every frame*. Moving
    /// back the other way then drags the whole track under a cursor that never leaves the
    /// edge, which is the opposite of scrolling. Remembering where the window was is what
    /// makes the cursor move *through* it and reach an edge before anything scrolls.
    ///
    /// `None` until the first draw after entering, so the window the reader arrives on is the
    /// one the selected cue had chosen — the same rule [`Self::cursor`]'s seed follows — and
    /// `None` again on the way out, so a later visit is a fresh arrival rather than a return
    /// to a position the cue list has since moved away from.
    window_start: Option<Duration>,
    /// The frame at [`Self::cursor`], encoded and ready to draw.
    ///
    /// Exactly one, and **kept until a newer one replaces it**. Nothing caches these (see
    /// [`crate::preview::ScrubTarget`]), so every settled position costs an `ffmpeg` seek;
    /// blanking the pane in between would be a flicker on every press of a held key, where
    /// the debounce already collapses a hold into one grab. The timeline's title states the
    /// moment the cursor is really on throughout, so the pane is never the only thing
    /// speaking.
    scrub: Option<ScrubFrame>,
    /// Set when the cursor has moved since the last grab was dispatched, and cleared when the
    /// next one is sent.
    ///
    /// Its own clock rather than a share of [`Self::frame_pending_since`], which carries the
    /// selected cue's request and the debounce arithmetic
    /// ([`crate::app::App::start_pending_preview`]) built around a frame that may already be
    /// cached. Nothing the cursor asks for is ever cached, so this one is simply always
    /// debounced.
    scrub_pending_since: Option<Instant>,
    /// Why the frame at this moment could not be drawn.
    ///
    /// Keyed on the moment for the same reason [`Self::frame_error`] is keyed on the cue: the
    /// reason a moment the reader has left could not be drawn says nothing about the one they
    /// are on now.
    scrub_error: Option<(Duration, String)>,
    /// Why the last playback of this cue — or of this moment — could not run.
    ///
    /// Keyed on what was asked for, for the same reason `frame_error` is, and on the status
    /// row for the same reason too: a playback is asked for by a keypress against one line
    /// or one moment, and the reason it failed says nothing about the next one.
    playback_error: Option<(PlaybackAnchor, String)>,
    workspace: PreviewWorkspace,
}

impl SubtitleEditState {
    pub fn new(
        generation: u64,
        frames: FrameSource,
        source: SubtitleSource,
        duration: Duration,
        support: PreviewSupport,
        frame_cost: u64,
        workspace: PreviewWorkspace,
    ) -> Self {
        Self {
            frame_cost,
            generation,
            frames,
            source,
            duration,
            status: LoadStatus::Preparing,
            support,
            warm: WarmState::Off,
            cues: Vec::new(),
            origins: Vec::new(),
            timing: TimingScope::Off,
            track_shift: 0,
            layout: LaneLayout::default(),
            groups: Vec::new(),
            visible: Vec::new(),
            cue_search: SearchState::default(),
            search_origin: None,
            selected: 0,
            group_page: 0,
            group_memory: HashMap::new(),
            restore: None,
            list_scroll: 0,
            list_rows: 0,
            preview_cells: Size::new(0, 0),
            encoded: Vec::new(),
            frame_error: None,
            frame_pending_since: None,
            refill_nearby: false,
            focus: EditFocus::Cues,
            cursor: Duration::ZERO,
            window_start: None,
            scrub: None,
            scrub_pending_since: None,
            scrub_error: None,
            playback: PlaybackState::Idle,
            playback_error: None,
            workspace,
        }
    }

    pub fn workspace(&self) -> &Path {
        self.workspace.path()
    }

    pub fn media(&self) -> &Path {
        &self.frames.media
    }

    /// Takes the background pass's latest count.
    ///
    /// `done == total` is "the pass is over", not "every cue was rendered" — a cue it
    /// could not draw is finished with too. The status line only reports a count while
    /// there is still one to report.
    pub fn apply_warming(&mut self, done: usize, total: usize) {
        self.warm = if done >= total {
            WarmState::Done
        } else {
            WarmState::Working { done, total }
        };
        // The pass has cached frames the window may have been missing when it was last
        // asked for. Without this the neighbours are only ever refilled by the cursor
        // moving, so the first step in a freshly opened page waits on the worker — the
        // round trip this whole window exists to remove.
        //
        // Only when something is actually missing, though: the pass reports ten times a
        // second for as long as it runs, and a full window has nothing to refill, so the
        // steady state of a settled cursor would otherwise be ten requests a second that
        // exist only to be found empty and dropped.
        //
        // Deliberately *not* gated on the pass having reached the window. The workers
        // render contiguous slices in parallel, so `done` counts frames rather than
        // describing a prefix of the track: a cue near the cursor may well be cached
        // while the count is still low.
        self.refill_nearby |= self.has_missing_nearby();
    }

    /// Takes the cues a worker parsed and makes the page ready.
    ///
    /// Lanes are packed once, here, rather than per frame: the packing is over the whole
    /// track, so doing it while rendering would both repeat the work every draw and let
    /// the track's height change as the user scrolls through denser regions.
    ///
    /// Overlap groups are found here for the same reason, and because the panel's cursor
    /// moves in groups: a grouping recomputed per draw could change under a keypress that
    /// had already been resolved against the old one.
    pub fn apply_prepared(&mut self, cues: Vec<Cue>, style: CueStyle) {
        // Onto the frame source, because that is what every renderer is handed and the
        // style is as much a part of drawing a cue as the cue is. It arrives only now:
        // an ASS script's styles come out of the file in the same pass as its cues.
        self.frames.style = Arc::new(style);
        self.layout = pack_lanes(&cues, MAX_LANES);
        self.groups = group_overlaps(&cues);
        self.status = if cues.is_empty() {
            LoadStatus::Empty
        } else {
            LoadStatus::Ready
        };
        // Every cue is the file's until the reader adds one: this is the reading the staged
        // rewrites' positions are keyed against.
        self.origins = (0..cues.len()).map(CueOrigin::File).collect();
        self.cues = cues;
        // Clamped rather than trusted: the cue the page is being restored to came from the
        // *previous* reading of a file that has since been rewritten, and a save can remove
        // cues as easily as it can change their words.
        self.selected = self
            .restore
            .take()
            .unwrap_or(0)
            .min(self.cues.len().saturating_sub(1));
        self.group_page = self
            .groups
            .get(self.group_of(self.selected))
            .map(|group| (self.selected - group.first) / GROUP_COLUMNS)
            .unwrap_or(0);
        self.group_memory.clear();
        self.list_scroll = 0;
        // These cues are the file's own timings, whatever the last list had been shifted by:
        // a reading of the file that a save has just rewritten is where a shift is measured
        // from, so carrying the old figure over would report this track as already moved.
        self.track_shift = 0;
        // And the filter goes with them, for the same reason: a query is about the list it
        // was typed against, and this is a fresh reading of a file a save has just
        // rewritten. Carried over, it could hide the very cue `restore` was put back on.
        // This is also what fills `visible` for a page that has never been searched.
        self.clear_cue_search();
        // A new cue list is a new track under the cursor, so the timeline cursor has nothing
        // left to be standing on: it is seeded from a cue, and every cue it was measured
        // against has just been replaced.
        self.leave_timeline();
        self.select_cue();
    }

    /// Where the cursor should land when this page's cues arrive.
    ///
    /// For a page being opened again after a save rewrote the file under it: the reader was
    /// on a cue, and the reload is the application's business rather than theirs. Recorded
    /// rather than applied, because there is no cue list to select in until the worker has
    /// read the rewritten file back.
    pub fn restore_selection(&mut self, cue: usize) {
        self.restore = Some(cue);
    }

    pub fn fail(&mut self, message: String) {
        self.status = LoadStatus::Failed(message);
        // Nothing to render frames for, so nothing to count. A pass already running for
        // this page is stopped by the generation bump that closing it performs.
        self.warm = WarmState::Off;
        self.cues.clear();
        self.origins.clear();
        self.layout = LaneLayout::default();
        self.groups.clear();
        self.clear_cue_search();
        self.encoded.clear();
        self.frame_pending_since = None;
        self.refill_nearby = false;
        self.leave_timeline();
        // Nothing left to play against, and the device is let go of with it.
        self.stop_playback();
    }

    /// The frame to draw, if the cue currently selected has one ready.
    ///
    /// Keyed on the selection rather than handed out unconditionally, so a frame is never
    /// drawn under a cue it does not belong to — a stale picture under a fresh cue reads
    /// as the burn-in being wrong.
    pub fn frame(&self) -> Option<&Protocol> {
        self.frame_for(self.selected)
    }

    /// The still the preview pane may fall back on, which is not always the selected cue's.
    ///
    /// With the cue panel holding the cursor the two are the same question: the selection is
    /// what the page points at, so its frame is the picture.
    ///
    /// With the *timeline* holding it they are not. The cue's frame stands in only while the
    /// cursor is still on the moment [`Self::focus_timeline`] seeded it with — which is why
    /// `Ctrl+J` costs no grab at all, the picture already on screen being the right one. Once
    /// the cursor has moved, that frame is a picture of another moment, and handing it to a
    /// pane whose title names the moment the cursor is on is the page contradicting itself.
    ///
    /// **This is what `p` used to fall through to.** Announcing a playback puts "Preparing
    /// playback…" on the status row, that row comes out of the preview pane's height, and the
    /// resize drops the cursor's frame ([`Self::set_preview_cells`]) — so for the second or
    /// two a span takes to decode the pane showed the selected cue's still, which on a page
    /// entered and left alone is the first cue of the track. Nothing is drawn there now: an
    /// empty pane is a page a little behind, where the wrong frame is a page answering the
    /// wrong question.
    pub fn still_frame(&self) -> Option<&Protocol> {
        match self.cursor() {
            Some(at) if self.cursor_seed() != Some(at) => None,
            _ => self.frame(),
        }
    }

    fn frame_for(&self, cue_index: usize) -> Option<&Protocol> {
        self.encoded
            .iter()
            .find(|frame| frame.cue_index == cue_index)
            .map(|frame| frame.protocol.as_ref())
    }

    /// Whether the cue at `cue_index` is already encoded and ready to draw.
    pub fn has_frame(&self, cue_index: usize) -> bool {
        self.frame_for(cue_index).is_some()
    }

    /// Takes a rendered frame, for the selection or for a cue near it.
    pub fn apply_frame(&mut self, cue_index: usize, protocol: Box<Protocol>) {
        // Only this cue's reason, since it is only this cue that turned out to be
        // drawable after all.
        self.frame_error.take_if(|(failed, _)| *failed == cue_index);
        let frame = Frame {
            cue_index,
            protocol,
        };
        match self
            .encoded
            .iter_mut()
            .find(|held| held.cue_index == cue_index)
        {
            Some(held) => *held = frame,
            None => self.encoded.push(frame),
        }
        // A frame for a cue the cursor has since left is worth keeping only while it is
        // still inside the window; pruning here is what bounds the list when the answer
        // arrives after the selection has moved on.
        self.prune_frames();
        // And look again at what is left. The frame worker coalesces its queue down to the
        // newest request, so a refill that was still waiting when the next one arrived was
        // dropped on the floor; without this the window would stay short of that one cue
        // until the cursor happened to move again.
        self.refill_nearby = true;
    }

    /// Records why one cue could not be drawn, dropping any frame held for it.
    pub fn fail_frame(&mut self, cue_index: usize, message: String) {
        self.encoded.retain(|frame| frame.cue_index != cue_index);
        self.frame_error = Some((cue_index, message));
    }

    /// Why the cue under the cursor has no frame, if that is why it has none.
    ///
    /// `None` while one is merely still being rendered — an empty pane during a render is
    /// a wait, not a failure, and saying anything about it would put a message on screen
    /// for the ordinary case.
    pub fn frame_error(&self) -> Option<&str> {
        self.frame_error
            .as_ref()
            .filter(|(cue_index, _)| *cue_index == self.selected)
            .map(|(_, message)| message.as_str())
    }

    /// How many cues either side of the selection this pane can afford to keep encoded.
    ///
    /// [`NEARBY_FRAMES`] is the ceiling, not the answer. A `Protocol` holds the bytes the
    /// terminal is going to be sent rather than the picture, and under kitty that is the
    /// frame's pixels as base64 RGBA — so five of them on a pane filling a large display
    /// is tens of megabytes held to save a round trip on two cues the cursor may never
    /// reach. [`FRAME_WINDOW_BUDGET`] is what the whole window may hold; the selected
    /// cue's own frame is not optional, so it is paid for first and the neighbours divide
    /// what is left.
    ///
    /// Floored at one either side rather than zero: a window of nothing puts the worker
    /// round trip back on every cursor move, which is the thing the window exists to
    /// remove, and one frame over the budget is the better of those two outcomes.
    fn window_radius(&self) -> usize {
        let cells = u64::from(self.preview_cells.width) * u64::from(self.preview_cells.height);
        let per_frame = cells.saturating_mul(self.frame_cost);
        if per_frame == 0 {
            return NEARBY_FRAMES;
        }
        let affordable = FRAME_WINDOW_BUDGET / per_frame;
        usize::try_from(affordable.saturating_sub(1) / 2)
            .unwrap_or(NEARBY_FRAMES)
            .clamp(1, NEARBY_FRAMES)
    }

    /// Drops every frame further from the selection than [`Self::window_radius`].
    fn prune_frames(&mut self) {
        let selected = self.selected;
        let radius = self.window_radius();
        self.encoded
            .retain(|frame| frame.cue_index.abs_diff(selected) <= radius);
    }

    /// Marks the frames on hand as no longer the right ones for where the cursor is.
    ///
    /// Asked for even when the selected cue is already encoded: the window has moved, so
    /// there is a new cue at its far edge to get ready before the cursor reaches it.
    pub fn request_frame(&mut self) {
        self.prune_frames();
        self.frame_pending_since = Some(Instant::now());
    }

    /// Rewrites one cue's text and asks for its picture again.
    ///
    /// For the cue editor: the frame on hand was drawn with the old words burned into it, so
    /// it is dropped rather than left to be redrawn whenever the cursor next moves — the
    /// whole point of editing here is seeing the new line against the picture. The disk
    /// cache needs no telling, since a frame is filed under the bytes handed to libass and
    /// the edited cue simply misses (`framecache::cue_key`).
    ///
    /// The timings are untouched, so the groups, the lanes and the scroll all still describe
    /// this list; only the words changed. A nudge cannot say that — see
    /// [`Self::set_cue_timing`].
    pub fn edit_cue_text(&mut self, cue: usize, text: String) {
        let Some(target) = self.cues.get_mut(cue) else {
            return;
        };
        target.text = text;
        self.encoded.retain(|frame| frame.cue_index != cue);
        if self.frame_error.as_ref().is_some_and(|(at, _)| *at == cue) {
            self.frame_error = None;
        }
        // The words are what the filter matches on, so a rewrite can put this row into the
        // drawn list or take it out of it. The groups and the positions are untouched, so
        // unlike an insertion this is only the projection being rebuilt.
        self.refilter();
        self.request_frame();
    }

    /// Where the cue standing at `position` in the page's list came from.
    ///
    /// The one translation between what the reader is pointing at and what a staged change is
    /// addressed by — see [`CueOrigin`].
    pub fn origin(&self, position: usize) -> Option<CueOrigin> {
        self.origins.get(position).copied()
    }

    /// Where the selected cue came from.
    pub fn selected_origin(&self) -> Option<CueOrigin> {
        self.origin(self.selected)
    }

    /// Which row of the page's list a staged change is about — [`Self::origin`] the other way
    /// round.
    ///
    /// For the cue editor, which remembers what it was opened on as an origin rather than as
    /// a row: a row is a fact about the list as it stands, and an origin is a fact about the
    /// cue, which is what a reader who opened the editor was pointing at.
    pub fn position_of(&self, origin: CueOrigin) -> Option<usize> {
        self.origins.iter().position(|found| *found == origin)
    }

    /// Puts a cue the file has no line for into the page's list, and selects it.
    ///
    /// **At its place in time rather than at the end**, because the panel's groups and the
    /// timeline's lanes both read the list as start-ordered: appended, a cue would be grouped
    /// with whatever happened to precede it in the vector rather than with what shares its
    /// moment. Every file cue after it keeps the position its staged rewrites are keyed by —
    /// that is what [`Self::origins`] is for.
    ///
    /// Three things follow, and each is the same rule [`Self::set_cue_timing`] follows for a
    /// cue that moved: the encoded frames are dropped for every cue now sharing the screen
    /// with the new one, since a frame is burned with everything on screen at that moment;
    /// the lanes are repacked; and a playback is stopped, because a span decoded before this
    /// cue existed is a span of a picture the page no longer describes. The overlap *groups*
    /// are recomputed here, unlike after a nudge, because a new row has to appear in the
    /// panel at all — there is no drawing of this list that predates it.
    ///
    /// Reports where the new cue landed.
    pub fn insert_cue(
        &mut self,
        insert: usize,
        start: Duration,
        end: Duration,
        text: String,
    ) -> usize {
        // After every cue that starts no later, which is where the save's own sort will put
        // it: a cue inserted onto the same moment as another reads after it, the way a cue
        // typed into a file at that point would.
        let at = self
            .cues
            .partition_point(|cue| (cue.start, cue.end) <= (start, end));
        self.cues.insert(
            at,
            Cue {
                index: at,
                start,
                end,
                text,
                // Empty because this is a SubRip-only feature: a cue with no `Dialogue:` line
                // is staged as its own text, which is exactly what a SubRip cue is.
                dialogue: Vec::new(),
                events: 1,
            },
        );
        // Kept as the list's own numbering, which every caller that reads `Cue::index`
        // expects to be the position in this list.
        for (position, cue) in self.cues.iter_mut().enumerate() {
            cue.index = position;
        }
        self.origins.insert(at, CueOrigin::Inserted(insert));
        // The window's indices are positions in `cues`, so everything at or after the new row
        // has moved down one. Shifted rather than cleared, so the frames the reader was
        // looking at survive an insertion somewhere else in the track.
        for frame in &mut self.encoded {
            if frame.cue_index >= at {
                frame.cue_index += 1;
            }
        }
        if let Some((cue, _)) = self.frame_error.as_mut()
            && *cue >= at
        {
            *cue += 1;
        }
        let cues = &self.cues;
        self.encoded.retain(|frame| {
            frame.cue_index != at
                && cues
                    .get(frame.cue_index)
                    .is_some_and(|other| !shares_screen(other, start, end))
        });
        self.layout = pack_lanes(&self.cues, MAX_LANES);
        self.groups = group_overlaps(&self.cues);
        // The keys are positions in `groups`, which has just been rebuilt around a row that
        // was not in it — so every remembered place is about a group that may no longer be
        // the same group.
        self.group_memory.clear();
        self.selected = at;
        self.group_page = self
            .groups
            .get(self.group_of(at))
            .map(|group| (at - group.first) / GROUP_COLUMNS)
            .unwrap_or(0);
        // A new row and a rebuilt `groups` between them move every position the filter
        // holds, so the projection is rebuilt against the list as it now is. The cursor is
        // already on the new cue, so `refilter` moves it only if the reader's query does
        // not match what they just typed — which is honest, and visible in the panel.
        self.refilter();
        // The reader made a cue; the page has to be showing it. That means the cue panel,
        // which is the only pane that marks a selection — the timeline draws no bracket while
        // it holds the cursor, so a new cue made from there would land unmarked and invisible.
        self.leave_timeline();
        self.select_cue();
        at
    }

    /// Takes a row out of the page's list, for a cue the reader added and has now un-added.
    ///
    /// The mirror of [`Self::insert_cue`] and it follows the same rules for the same reasons:
    /// every file cue keeps the position its staged rewrites are keyed by (that is what
    /// [`Self::origins`] is for), the encoded frames above the gone row shift down rather than
    /// being cleared, and the frames of every cue that shared the screen with it are dropped,
    /// since a frame is burned with everything on screen at that moment. The lanes are
    /// repacked and the overlap groups rebuilt, because a row leaving the panel is a change to
    /// the list every drawing of it is derived from.
    ///
    /// **Only ever used for an inserted cue.** A cue the file holds is marked to go and stays
    /// on screen until a save carries it out, so that the mark can be taken back off.
    pub fn remove_cue(&mut self, position: usize) {
        let Some(gone) = self.cues.get(position).cloned() else {
            return;
        };
        self.cues.remove(position);
        self.origins.remove(position);
        // Kept as the list's own numbering, which every caller that reads `Cue::index`
        // expects to be the position in this list.
        for (at, cue) in self.cues.iter_mut().enumerate() {
            cue.index = at;
        }
        // The row itself has no frame to keep, and everything below it has moved up one.
        self.encoded.retain(|frame| frame.cue_index != position);
        for frame in &mut self.encoded {
            if frame.cue_index > position {
                frame.cue_index -= 1;
            }
        }
        match self.frame_error.as_mut() {
            Some((cue, _)) if *cue == position => self.frame_error = None,
            Some((cue, _)) if *cue > position => *cue -= 1,
            _ => {}
        }
        let cues = &self.cues;
        self.encoded.retain(|frame| {
            cues.get(frame.cue_index)
                .is_some_and(|other| !shares_screen(other, gone.start, gone.end))
        });
        self.layout = pack_lanes(&self.cues, MAX_LANES);
        self.groups = group_overlaps(&self.cues);
        // The keys are positions in `groups`, which has just been rebuilt without a row that
        // was in it — so every remembered place is about a group that may no longer be the
        // same group.
        self.group_memory.clear();
        // The row that took the gone one's place, which is where the eye already is. Clamped
        // rather than moved, so removing the last row lands on the new last one.
        self.selected = position.min(self.cues.len().saturating_sub(1));
        self.group_page = self
            .groups
            .get(self.group_of(self.selected))
            .map(|group| (self.selected - group.first) / GROUP_COLUMNS)
            .unwrap_or(0);
        // For the reason `insert_cue` does it: a row leaving moves every position after it.
        self.refilter();
        self.leave_timeline();
        self.select_cue();
    }

    /// Shifts the selected cue through time, keeping its duration, and reports the new
    /// timing for staging.
    ///
    /// **Both ends move by the same amount even at the floor.** A cue near the start of the
    /// media cannot go earlier than zero, and clamping only the start would shorten the cue
    /// a little at a time — a nudge that quietly edits something the reader did not ask to
    /// edit. The whole shift is shortened instead, so the cue stops moving with its duration
    /// intact.
    ///
    /// `None` when there is no cue to move, or when the cue is already against the floor and
    /// the press would do nothing: the caller stages nothing, and a held `h` at 0:00 does not
    /// re-render a frame per repeat.
    pub fn nudge_selected(&mut self, steps: i64) -> Option<(usize, Duration, Duration)> {
        let cue = self.cues.get(self.selected)?;
        let shift = TIMING_STEP.saturating_mul(steps.unsigned_abs().try_into().ok()?);
        let (start, end) = if steps.is_negative() {
            let shift = shift.min(cue.start);
            (cue.start - shift, cue.end - shift)
        } else {
            (cue.start + shift, cue.end + shift)
        };
        if (start, end) == (cue.start, cue.end) {
            return None;
        }
        let selected = self.selected;
        self.set_cue_timing(selected, start, end);
        Some((selected, start, end))
    }

    /// Moves one end of the selected cue and leaves the other where it is, reporting the new
    /// timing for staging.
    ///
    /// The other half of [`Self::nudge_selected`], which moves both ends together: that one
    /// answers *when* a line is on screen and this one answers *how long*. A track is as
    /// often a little out in the second as in the first — a line that arrives with the shot
    /// and goes away while the mouth is still moving — and until this there was no key on the
    /// page that could change it.
    ///
    /// **Neither end may cross the other**, and the floor is [`MIN_CUE_LENGTH`] rather than
    /// zero: a cue of no length is not a subtitle, and one whose end precedes its start is a
    /// `-->` line no player reads and nothing in the save path would refuse. The clamp is
    /// written so that a cue *already* at or under the floor — which a malformed file can
    /// hold — is never pushed the wrong way by a press asking to shorten it; it simply
    /// reports `None`.
    ///
    /// `None` when there is no cue, when the press is against a floor, or when it would
    /// otherwise change nothing, so a held key stages nothing and re-renders no frame — the
    /// contract [`Self::nudge_selected`] follows for the same reason.
    ///
    /// **One step per press rather than a signed count**, unlike the nudge: there is no leap
    /// key for an edge — `H`/`L` are the whole cue's and the other modifier is the other
    /// direction — so a step count would be a parameter the application can only ever pass
    /// one of two values for, with arithmetic under it that nothing could reach.
    pub fn move_selected_edge(
        &mut self,
        edge: CueEdge,
        later: bool,
    ) -> Option<(usize, Duration, Duration)> {
        let cue = self.cues.get(self.selected)?;
        let (start, end) = match (edge, later) {
            (CueEdge::Start, true) => {
                // The `max` is what keeps a degenerate cue still: with the ceiling below
                // where the start already is, a bare `min` would answer a press meaning
                // "later" by moving the start *earlier*.
                let ceiling = cue.end.saturating_sub(MIN_CUE_LENGTH).max(cue.start);
                ((cue.start + TIMING_STEP).min(ceiling), cue.end)
            }
            (CueEdge::Start, false) => (cue.start.saturating_sub(TIMING_STEP), cue.end),
            (CueEdge::End, true) => (cue.start, cue.end + TIMING_STEP),
            (CueEdge::End, false) => {
                let floor = (cue.start + MIN_CUE_LENGTH).min(cue.end);
                (cue.start, cue.end.saturating_sub(TIMING_STEP).max(floor))
            }
        };
        self.resize_selected(start, end)
    }

    /// Sets how long the selected cue is on screen, keeping its start, for the length dialog.
    ///
    /// **The start is what is kept.** The reader is answering "how long should this line be
    /// up for", and the start is the half they have already judged against the picture —
    /// moving it to honour a typed length would undo the work the dialog was opened on top
    /// of.
    ///
    /// `None` for a length under [`MIN_CUE_LENGTH`] and for the length the cue already has,
    /// so the caller can tell a refusal from a no-op and stage nothing for either.
    pub fn set_selected_length(&mut self, length: Duration) -> Option<(usize, Duration, Duration)> {
        if length < MIN_CUE_LENGTH {
            return None;
        }
        let cue = self.cues.get(self.selected)?;
        let (start, end) = (cue.start, cue.start + length);
        self.resize_selected(start, end)
    }

    /// Applies a span to the selected cue, or reports `None` when it is the span it has.
    ///
    /// The one place both resizes end, so an edge nudge and a typed length cannot come to
    /// disagree about what a resize costs the page — which is everything
    /// [`Self::set_cue_timing`] does: the stale frames, the repacked lanes, the stopped
    /// playback and the fresh grab.
    fn resize_selected(
        &mut self,
        start: Duration,
        end: Duration,
    ) -> Option<(usize, Duration, Duration)> {
        let cue = self.cues.get(self.selected)?;
        if (start, end) == (cue.start, cue.end) {
            return None;
        }
        let selected = self.selected;
        self.set_cue_timing(selected, start, end);
        Some((selected, start, end))
    }

    /// How long the selected cue is on screen, for the length dialog to open holding.
    pub fn selected_length(&self) -> Option<Duration> {
        let cue = self.cues.get(self.selected)?;
        Some(cue.end.saturating_sub(cue.start))
    }

    /// Shifts **every** cue through time by the same amount, and reports how far in
    /// milliseconds, signed.
    ///
    /// **The floor clamp is the whole track's, not each cue's.** A backward shift is
    /// shortened to what the *earliest* cue allows, so every cue still moves by the same
    /// amount. Clamping each cue against zero on its own would let the ones near the start
    /// bunch up while the rest kept moving — the track silently stretched by a key that says
    /// it moves the track, which is the wide-scale version of the mistake
    /// [`Self::nudge_selected`] avoids by shortening the whole shift rather than one end.
    ///
    /// **Every frame goes stale, so they are all dropped.** Each cue now begins at a
    /// different instant, so every cached still is a picture of somewhere else; there is
    /// nothing left to filter with [`shares_screen`] the way [`Self::set_cue_timing`] does.
    ///
    /// **The overlap groups are left alone, and here that is provable rather than a
    /// judgement.** A uniform shift moves every cue equally, so no cue can come to overlap
    /// another that it did not overlap before. The lanes are repacked all the same: it costs
    /// one pass and means nothing downstream depends on that proof continuing to hold.
    ///
    /// `None` when there is nothing to move or the track is already against the floor, so a
    /// held `h` at 0:00 stages nothing and re-renders nothing.
    pub fn shift_all(&mut self, steps: i64) -> Option<i64> {
        if self.cues.is_empty() {
            return None;
        }
        let shift = TIMING_STEP.saturating_mul(steps.unsigned_abs().try_into().ok()?);
        let shift = if steps.is_negative() {
            shift.min(self.cues.iter().map(|cue| cue.start).min()?)
        } else {
            shift
        };
        if shift.is_zero() {
            return None;
        }
        for cue in &mut self.cues {
            if steps.is_negative() {
                cue.start -= shift;
                cue.end -= shift;
            } else {
                cue.start += shift;
                cue.end += shift;
            }
        }
        let moved = i64::try_from(shift.as_millis()).ok()? * steps.signum();
        self.track_shift += moved;
        self.retimed();
        Some(moved)
    }

    /// Puts every cue back to a timing supplied by the caller, for the wide `r`.
    ///
    /// The caller owns which timings those are, because the file's copy of them lives in the
    /// staged edits rather than on the page — this only moves the cues and stands the page's
    /// derived state down, exactly as a shift does. Positions the caller says nothing about
    /// are left where they are: an inserted cue has no timing in the file to be put back to.
    pub fn restore_timings(&mut self, timings: &[(usize, Duration, Duration)]) {
        for &(position, start, end) in timings {
            if let Some(cue) = self.cues.get_mut(position) {
                cue.start = start;
                cue.end = end;
            }
        }
        self.track_shift = 0;
        self.retimed();
    }

    /// What every whole-track retiming leaves the page to do: drop the stale pictures, repack
    /// the lanes, stop a playback that is now about timings the track no longer has, and ask
    /// for the selected cue's picture again.
    ///
    /// Shared by [`Self::shift_all`] and [`Self::restore_timings`] so the two cannot come to
    /// disagree about what a wide move costs.
    fn retimed(&mut self) {
        self.encoded.clear();
        self.frame_error = None;
        self.layout = pack_lanes(&self.cues, MAX_LANES);
        self.stop_playback();
        self.request_frame();
    }

    /// Retimes one cue and asks for its picture again.
    ///
    /// Three things follow from a timing change that do not follow from a text change, and
    /// each is load-bearing:
    ///
    /// **A playback is stopped.** A span is decoded ahead of time with the cue already
    /// burned into its frames, so one still playing is playing the timing the reader has
    /// just moved away from — "a span playing under a line it is not about", which
    /// [`Self::select_cue`] exists to prevent. Re-decoding instead would be an `ffmpeg` run
    /// per keypress.
    ///
    /// **More frames than this cue's go stale.** A [`FrameTarget`] carries the cues sharing
    /// its moment as well as the cue itself, so moving one line into or out of a
    /// neighbour's moment changes what the neighbour's picture should show — and its cache
    /// key with it. Every frame overlapping the cue's old *or* new span is dropped.
    /// Clearing the whole window would also be correct and costs a decode per neighbour on
    /// every press of a held key.
    ///
    /// **The lanes are repacked.** [`LaneLayout`] is computed once rather than per draw, so
    /// a moved cue would otherwise keep being drawn in a lane that no longer describes it,
    /// on top of whatever is really there. The overlap *groups* are deliberately left alone:
    /// they are the unit the panel's cursor and scrolling are measured in, and recomputing
    /// them under a held key would reflow the list around a keypress already resolved
    /// against the old one — the same hazard [`Self::apply_prepared`] cites for not deriving
    /// them per draw. A collision a nudge creates still shows, in the repacked lanes.
    pub fn set_cue_timing(&mut self, cue: usize, start: Duration, end: Duration) {
        let Some(target) = self.cues.get_mut(cue) else {
            return;
        };
        let (was_start, was_end) = (target.start, target.end);
        target.start = start;
        target.end = end;
        let cues = &self.cues;
        self.encoded.retain(|frame| {
            frame.cue_index != cue
                && cues.get(frame.cue_index).is_some_and(|other| {
                    !shares_screen(other, was_start, was_end) && !shares_screen(other, start, end)
                })
        });
        if self.frame_error.as_ref().is_some_and(|(at, _)| *at == cue) {
            self.frame_error = None;
        }
        self.layout = pack_lanes(&self.cues, MAX_LANES);
        self.stop_playback();
        self.request_frame();
    }

    /// Marks the frames stale *and* drops any playback, for the moves where the page is no
    /// longer about the same cue.
    ///
    /// **Deliberately not every caller of [`Self::request_frame`].** A pane resize also
    /// makes the still frames stale, and it must *not* stop a playback: saying "Preparing
    /// playback…" puts a status row on the page, which takes a row off the preview pane —
    /// so a playback that stopped on a resize would be stopped by the very act of
    /// announcing itself, every time, on any page that had no status row before. The
    /// playback absorbs a resize by encoding into the pane it finds
    /// ([`Playback::advance`]) instead.
    ///
    /// What does belong here is the cursor landing on another cue, or a whole new track
    /// arriving: a span left playing under a line it is not about reads as the timing being
    /// wrong, on the one page built to judge exactly that.
    fn select_cue(&mut self) {
        self.stop_playback();
        self.request_frame();
    }

    /// Hands the cursor to the timeline, seeding it on the moment the still already shows.
    ///
    /// [`crate::preview::seek_for`] rather than the cue's start, so the seed is the exact
    /// instant the preview pane is displaying: entering the pane is then a change of what the
    /// keys mean and nothing else, with no picture moving under the reader as they arrive.
    ///
    /// Refused while the page is still reading its cues, has none, or failed — there is no
    /// timeline drawn on any of those, so a cursor in it would be one the reader cannot see.
    ///
    /// Reports whether the cursor actually moved pane, so the caller can leave a notice alone
    /// when nothing happened.
    pub fn focus_timeline(&mut self) -> bool {
        if self.focus == EditFocus::Timeline || self.status != LoadStatus::Ready {
            return false;
        }
        let Some(seed) = self.cursor_seed() else {
            return false;
        };
        self.focus = EditFocus::Timeline;
        self.cursor = seed;
        // A span playing for the cue the reader has just stopped pointing at is the same
        // defect `select_cue` exists to prevent, arrived at from the other direction: the
        // cue panel no longer marks a selection while the cursor is here, so a playback
        // still running would be about a line nothing on screen names.
        self.stop_playback();
        // Left for the first draw to fill in, so the reader arrives on the window the
        // selected cue had already chosen rather than on one left over from a previous visit.
        self.window_start = None;
        // Nothing is asked for here on purpose: the cursor is standing on the selected cue's
        // own moment, and `scrub_frame` falls through to that cue's frame until the reader
        // moves off it. Grabbing a second copy of a picture already on screen would spend an
        // `ffmpeg` seek to change nothing.
        self.scrub = None;
        self.scrub_pending_since = None;
        self.scrub_error = None;
        true
    }

    /// Takes the cursor back to the cue panel.
    pub fn focus_cues(&mut self) -> bool {
        if self.focus == EditFocus::Cues {
            return false;
        }
        self.leave_timeline();
        true
    }

    /// Puts the page back in its default state, cursor in the cue panel and nothing scrubbed.
    ///
    /// Also what a new cue list and a failure do, since both leave the cursor standing on a
    /// moment measured against cues that no longer exist.
    fn leave_timeline(&mut self) {
        self.focus = EditFocus::Cues;
        // The cursor is about to stop existing, so a span playing around where it stood is
        // about a moment the page no longer points at — see [`Self::focus_timeline`].
        self.stop_playback();
        self.cursor = Duration::ZERO;
        self.window_start = None;
        self.scrub = None;
        self.scrub_pending_since = None;
        self.scrub_error = None;
    }

    /// Where the timeline's window begins, once a draw has settled on one.
    ///
    /// `None` before the first draw of a visit, which is the drawing code's cue to use the
    /// window the selected cue chose. See [`Self::window_start`].
    pub fn window_start(&self) -> Option<Duration> {
        self.window_start
    }

    /// Remembers where the window the reader is looking at begins.
    ///
    /// Called by the drawing code, which is the only place that knows how wide the pane is
    /// and therefore how long a window is — the state cannot work it out, and a scroll
    /// position is meaningless without it.
    pub fn set_window_start(&mut self, start: Duration) {
        self.window_start = Some(start);
    }

    /// Where the timeline cursor stands, or `None` when the cue panel holds the cursor.
    ///
    /// One question rather than two, so no caller can draw a cursor for a pane that does not
    /// have one.
    pub fn cursor(&self) -> Option<Duration> {
        (self.focus == EditFocus::Timeline).then_some(self.cursor)
    }

    /// Moves the timeline cursor by `steps` of `step`, reporting whether it moved.
    ///
    /// The size comes from the caller rather than from a constant here because the pane has
    /// three of them — [`TIMELINE_FINE_STEP`] for `Ctrl+H`/`Ctrl+L`, [`TIMELINE_STEP`] for
    /// `h`/`l`, and [`TIMELINE_LEAP`] of the latter for `H`/`L`. They differ only in how far
    /// one press reaches, so they are one movement taken at three scales rather than three
    /// movements, and every rule below has to hold for all of them.
    ///
    /// **A move stops a playback**, for the reason retiming a cue does
    /// ([`Self::set_cue_timing`]): a span decoded around one cue, still playing while the
    /// reader looks at a different moment, is a picture in the pane that is not about what
    /// the cursor points at — which on this page reads as the timing being wrong.
    ///
    /// A press against either end reports `false` and asks for nothing, so a held `h` at 0:00
    /// does not re-grab the same frame per repeat.
    pub fn move_cursor(&mut self, steps: i32, step: Duration) -> bool {
        if self.focus != EditFocus::Timeline {
            return false;
        }
        let shift = step.saturating_mul(steps.unsigned_abs());
        let moved = if steps.is_negative() {
            self.cursor.saturating_sub(shift)
        } else {
            self.cursor.saturating_add(shift).min(self.cursor_ceiling())
        };
        if moved == self.cursor {
            return false;
        }
        self.cursor = moved;
        self.scrub_pending_since = Some(Instant::now());
        self.stop_playback();
        true
    }

    /// The latest moment the cursor may stand on.
    ///
    /// [`crate::preview::seek_ceiling`] where the media's length is known, so the cursor and
    /// the still grab agree about where the end of the file is. Where it is not — a container
    /// whose duration would not parse — the last cue's end is the furthest point the page has
    /// any evidence for, and letting the cursor run past that would be inventing media.
    /// The latest moment a cue may be placed at, which is the one the cursor stops at.
    ///
    /// The same answer for the same reason: a cue past the end of the media is a line over no
    /// picture, on the page that exists to judge a line against the picture.
    pub fn cue_ceiling(&self) -> Duration {
        self.cursor_ceiling()
    }

    fn cursor_ceiling(&self) -> Duration {
        seek_ceiling(self.duration).unwrap_or_else(|| {
            self.cues
                .last()
                .map(|cue| cue.end)
                .unwrap_or(Duration::ZERO)
        })
    }

    /// The moment the cursor is seeded on when the timeline takes it: the selected cue's own.
    ///
    /// [`crate::preview::seek_for`] rather than the cue's start, because that is the instant
    /// the still in the pane was grabbed at — see [`Self::focus_timeline`]. Written down once
    /// rather than at each of its two callers, since the other is [`Self::still_frame`], which
    /// asks whether the cursor is *still* standing here: two spellings of the same moment that
    /// drifted apart would leave the pane showing a picture of a moment the cursor has left.
    fn cursor_seed(&self) -> Option<Duration> {
        self.selected_cue()
            .map(|cue| seek_for(cue, self.duration).min(self.cursor_ceiling()))
    }

    /// What the frame worker needs in order to draw the moment the cursor stands on.
    pub fn scrub_target(&self) -> Option<ScrubTarget> {
        (self.focus == EditFocus::Timeline).then(|| ScrubTarget {
            at: self.cursor,
            // Unanchored, unlike a cue's: the reader pointed at this instant, so what belongs
            // on it is exactly what a viewer would see there — which is often nothing.
            on_screen: crate::cue::on_screen_now(&self.cues, self.cursor),
        })
    }

    /// Whether the cursor has moved since the last grab was sent.
    ///
    /// False whenever the cue panel holds the cursor, whatever the clock says, so a request
    /// left over from a pane the reader has left cannot keep the dispatch awake.
    pub fn scrub_requested(&self) -> bool {
        self.focus == EditFocus::Timeline && self.scrub_pending_since.is_some()
    }

    /// Whether that request has waited out [`FRAME_DEBOUNCE`].
    ///
    /// Consulted only for a moment that would have to be rendered, exactly as a cue's is: a
    /// moment already in the frame cache costs a read, and the cursor crossing ground it has
    /// covered before is the ordinary case this makes instant.
    pub fn scrub_request_due(&self) -> bool {
        self.scrub_requested()
            && self
                .scrub_pending_since
                .is_some_and(|since| since.elapsed() >= FRAME_DEBOUNCE)
    }

    pub fn clear_scrub_request(&mut self) {
        self.scrub_pending_since = None;
    }

    /// Takes the frame drawn for one moment, ignoring one the cursor has already left.
    pub fn apply_scrub_frame(&mut self, at: Duration, protocol: Box<Protocol>) {
        if at != self.cursor {
            return;
        }
        self.scrub_error = None;
        self.scrub = Some(ScrubFrame { at, protocol });
    }

    /// Records why one moment could not be drawn.
    ///
    /// The previous moment's picture goes with it. Keeping it would leave the pane showing one
    /// moment while the status row explains why a different one is missing, which is a page
    /// contradicting itself — where holding a picture *while the next one renders* is only a
    /// page being a little behind.
    pub fn fail_scrub_frame(&mut self, at: Duration, message: String) {
        if at != self.cursor {
            return;
        }
        self.scrub = None;
        self.scrub_error = Some((at, message));
    }

    /// The picture for the moment the cursor is on, if the timeline holds the cursor and one
    /// has been drawn.
    pub fn scrub_frame(&self) -> Option<&Protocol> {
        (self.focus == EditFocus::Timeline)
            .then_some(self.scrub.as_ref())
            .flatten()
            .map(|frame| frame.protocol.as_ref())
    }

    /// Why the moment the cursor is on has no picture, if that is why it has none.
    pub fn scrub_error(&self) -> Option<&str> {
        (self.focus == EditFocus::Timeline)
            .then_some(self.scrub_error.as_ref())
            .flatten()
            .filter(|(at, _)| *at == self.cursor)
            .map(|(_, message)| message.as_str())
    }

    /// Whether the selected cue's own frame has been asked for since the last request was
    /// sent — as opposed to only the neighbours around it.
    pub fn frame_requested(&self) -> bool {
        self.frame_pending_since.is_some()
    }

    /// Whether anything at all has been asked for, of either kind.
    pub fn any_frame_requested(&self) -> bool {
        self.frame_pending_since.is_some() || self.refill_nearby || self.scrub_requested()
    }

    /// Whether the outstanding request has waited out [`FRAME_DEBOUNCE`].
    ///
    /// Only consulted for a frame that would have to be rendered. One already in the
    /// cache is dispatched without asking, since there is no `ffmpeg` for the debounce to
    /// be protecting against.
    pub fn frame_request_due(&self) -> bool {
        self.frame_pending_since
            .is_some_and(|since| since.elapsed() >= FRAME_DEBOUNCE)
    }

    /// Forgets the outstanding request, once it has been sent or found to be asking for
    /// nothing. Keeps "asked for" and "waiting to ask" from ever both being true, which
    /// is what would let one settled selection start two `ffmpeg` processes.
    pub fn clear_frame_request(&mut self) {
        self.frame_pending_since = None;
        self.refill_nearby = false;
    }

    /// Forgets the refill alone, leaving the selected cue's request outstanding.
    ///
    /// For the dispatch that carries the neighbours while the selected cue is still
    /// waiting out [`FRAME_DEBOUNCE`]: that request has been deferred, not answered, and
    /// clearing it here would lose the frame the cursor is actually sitting on.
    pub fn clear_nearby_request(&mut self) {
        self.refill_nearby = false;
    }

    /// What the frame worker needs in order to draw the cue at `cue_index`.
    pub fn frame_target(&self, cue_index: usize) -> Option<FrameTarget> {
        let cue = self.cues.get(cue_index)?;
        // `seek_for` rather than the cue's start outright: a cue running to the end of the
        // media has to be held back from the very last instant, and the background pass has
        // to make exactly the same decision — a disagreement would have the two writing
        // different pictures under one cache key.
        let seek = seek_for(cue, self.duration);
        Some(FrameTarget {
            cue_index,
            cue: cue.clone(),
            // Everything on screen at that instant, not this cue alone: a typeset or
            // karaoke line is often a dozen cues sharing a moment, and any one of them on
            // its own is a fraction of the picture the viewer gets. See
            // [`crate::cue::on_screen_at`].
            on_screen: crate::cue::on_screen_at(&self.cues, cue_index, seek),
            seek,
        })
    }

    /// The cues either side of the selection that are not encoded yet, nearest first.
    ///
    /// Both directions at each distance, because which way the cursor is about to move is
    /// not knowable and getting the wrong one ready costs a cache read.
    ///
    /// Reaches exactly as far as [`Self::window_radius`] can afford to keep. Asking wider
    /// than that would render frames the next `prune_frames` throws away, and then ask for
    /// them again on the following report.
    pub fn nearby_frame_targets(&self) -> Vec<FrameTarget> {
        let mut targets = Vec::new();
        for distance in 1..=self.window_radius() {
            let candidates = [
                self.selected.checked_sub(distance),
                self.selected.checked_add(distance),
            ];
            for cue_index in candidates.into_iter().flatten() {
                if !self.has_frame(cue_index)
                    && let Some(target) = self.frame_target(cue_index)
                {
                    targets.push(target);
                }
            }
        }
        targets
    }

    /// Whether any cue inside the window is still short of a frame.
    ///
    /// The same question [`Self::nearby_frame_targets`] answers, without building the
    /// list to answer it — this runs on every progress report the background pass sends,
    /// which is ten a second for as long as a track takes to render.
    fn has_missing_nearby(&self) -> bool {
        (1..=self.window_radius()).any(|distance| {
            [
                self.selected.checked_sub(distance),
                self.selected.checked_add(distance),
            ]
            .into_iter()
            .flatten()
            .any(|cue_index| cue_index < self.cues.len() && !self.has_frame(cue_index))
        })
    }

    /// Whether a playback is running or being decoded, which is what `p` toggles.
    pub fn playback_active(&self) -> bool {
        !matches!(self.playback, PlaybackState::Idle)
    }

    /// The frame the playhead is on, if a playback is drawing one.
    ///
    /// `None` while a span is still decoding *and* for the moment between the stream
    /// starting and the device's first callback, which is what leaves the still frame on
    /// screen rather than blanking the pane.
    pub fn playback_frame(&self) -> Option<&Protocol> {
        match &self.playback {
            PlaybackState::Playing(playback) => playback.frame(),
            _ => None,
        }
    }

    /// Where in the media the playhead is, for the timeline to mark.
    pub fn playback_position(&self) -> Option<Duration> {
        match &self.playback {
            PlaybackState::Playing(playback) => playback.position(),
            _ => None,
        }
    }

    /// Records that a span is being decoded for `anchor`, clearing any earlier reason.
    pub fn prepare_playback(&mut self, anchor: PlaybackAnchor) {
        self.playback_error = None;
        self.playback = PlaybackState::Preparing { anchor };
    }

    /// Whether a span is being decoded, and what for — so one arriving for a cue the cursor
    /// has since left, or a moment it has since moved off, can be dropped.
    pub fn preparing_playback(&self) -> Option<PlaybackAnchor> {
        match self.playback {
            PlaybackState::Preparing { anchor } => Some(anchor),
            _ => None,
        }
    }

    /// Starts playing a decoded span.
    ///
    /// Takes somewhere the sound can be sent rather than an output already open, so that a
    /// looping playback can open another when it comes round — see [`Playback`].
    pub fn begin_playback(
        &mut self,
        anchor: PlaybackAnchor,
        frames: PlaybackFrames,
        source: Box<dyn AudioSource>,
        looping: bool,
    ) {
        self.playback_error = None;
        self.playback = PlaybackState::Playing(Playback::new(anchor, frames, source, looping));
    }

    /// Records why a span could not be played, and stops waiting for it.
    pub fn fail_playback(&mut self, anchor: PlaybackAnchor, message: String) {
        self.playback = PlaybackState::Idle;
        self.playback_error = Some((anchor, message));
    }

    /// Why what the page is pointing at could not be played, if that is why nothing is.
    ///
    /// Kept only while the reader is still pointing at what failed, the same way
    /// [`Self::frame_error`] is: a message about a cue is stale the moment the cursor moves
    /// to another one, and a message about a moment is stale the moment the cursor leaves it.
    /// The pane the cursor is in has to match too — a failure about a cue says nothing about
    /// the moment being scrubbed, and vice versa.
    pub fn playback_error(&self) -> Option<&str> {
        self.playback_error
            .as_ref()
            .filter(|(anchor, _)| self.anchor_is_current(*anchor))
            .map(|(_, message)| message.as_str())
    }

    /// Whether `anchor` still names what the page is pointing at.
    ///
    /// The one place the two cursors' staleness rules are written down, read by everything
    /// that has to decide whether a span — or a message about one — is still about what the
    /// reader is looking at.
    pub fn anchor_is_current(&self, anchor: PlaybackAnchor) -> bool {
        match anchor {
            PlaybackAnchor::Cue(index) => self.focus == EditFocus::Cues && index == self.selected,
            PlaybackAnchor::Cursor(at) => self.focus == EditFocus::Timeline && at == self.cursor,
        }
    }

    /// Stops any playback, releasing the audio device with it. Reports whether there was
    /// one, so the caller knows whether to tell the worker to stop decoding.
    pub fn stop_playback(&mut self) -> bool {
        if matches!(self.playback, PlaybackState::Idle) {
            return false;
        }
        self.playback = PlaybackState::Idle;
        true
    }

    /// The cell area a playback should be drawn into, for the pane as it is *now*.
    ///
    /// Derived from the frames' own pixel size rather than remembered from the request:
    /// that carries the picture's proportions, so this keeps them whatever the pane has
    /// become since — and the pane does change, because announcing the playback is itself
    /// what adds the status row that shortens it.
    fn playback_cells(&self) -> Size {
        match &self.playback {
            PlaybackState::Playing(playback) => crate::preview::playback_cells(
                self.preview_cells,
                playback.frames.pixels,
                playback.frames.picker.font_size(),
            ),
            _ => Size::new(0, 0),
        }
    }

    /// Moves the playhead to wherever the sound has got to, reporting whether the page
    /// needs repainting.
    ///
    /// Called once per loop iteration rather than from the renderer, so the picture and the
    /// playhead are decided together and drawn together.
    pub fn advance_playback(&mut self) -> bool {
        let cells = self.playback_cells();
        let PlaybackState::Playing(playback) = &mut self.playback else {
            return false;
        };
        match playback.advance(Instant::now(), cells) {
            PlaybackStep::Unchanged => false,
            PlaybackStep::Drew => true,
            PlaybackStep::Finished => {
                // Back to the still frame, and the device let go of. A playback that ran to
                // its end is finished with, not paused at the last frame — the next `p`
                // should replay the span rather than have to stop it first.
                self.playback = PlaybackState::Idle;
                true
            }
        }
    }

    /// Records the pane size the renderer measured, dropping every frame on hand and
    /// asking again when it changed: each one was encoded for the old size, and `Image`
    /// draws nothing at all rather than clipping when it no longer fits.
    pub fn set_preview_cells(&mut self, cells: Size) {
        if self.preview_cells != cells {
            self.preview_cells = cells;
            self.encoded.clear();
            // The cursor's frame is encoded for the pane too, so a resize makes it as stale
            // as the rest — `Image` draws nothing at all for a protocol wider than the area
            // it is given, so leaving it would blank the pane rather than shrink the picture.
            self.scrub = None;
            if self.focus == EditFocus::Timeline {
                self.scrub_pending_since = Some(Instant::now());
            }
            self.request_frame();
        }
    }

    pub fn selected_cue(&self) -> Option<&Cue> {
        self.cues.get(self.selected)
    }

    /// Whether the page is waiting on background work, so the loader keeps animating.
    pub fn is_busy(&self) -> bool {
        self.status == LoadStatus::Preparing
    }

    /// The cue panel's filter bar, for the renderer that measures it and the keys that
    /// type into it.
    pub fn cue_search(&self) -> &SearchState {
        &self.cue_search
    }

    /// The bar's buffer, for [`crate::app::App::text_input_mut`].
    pub fn cue_search_mut(&mut self) -> &mut SearchState {
        &mut self.cue_search
    }

    /// The query the panel is filtered by, trimmed, or empty when it is not filtered.
    ///
    /// The single answer to "is a filter in force", read by the renderer to decide whether
    /// to draw the bar and highlight the words, and by the paging rules below.
    pub fn cue_query(&self) -> &str {
        self.cue_search.value.trim()
    }

    /// Whether this cue's words match the query. An empty query matches everything, so the
    /// unfiltered list is the same code path as a filtered one.
    ///
    /// Matched against [`Cue::text`] — the words the panel actually draws, which already
    /// carry a staged rewrite — rather than against `Cue::dialogue`, which is the string
    /// libass reads and can hold override tags the reader never sees. Timings are not
    /// searched: a reader hunting for a moment has the timeline for it.
    ///
    /// Case-insensitive substring, the rule every other search in the application follows.
    fn cue_matches(cue: &Cue, query: &str) -> bool {
        query.is_empty() || fold_case(&cue.text).contains(&fold_case(query))
    }

    /// The first cue of this group whose words match, if any.
    fn first_match_in(&self, group: CueGroup, query: &str) -> Option<usize> {
        (group.first..group.end()).find(|cue| {
            self.cues
                .get(*cue)
                .is_some_and(|cue| Self::cue_matches(cue, query))
        })
    }

    /// Rebuilds which groups the panel is drawing, and puts the cursor somewhere it can
    /// still be seen.
    ///
    /// **A group is shown whole when any of its cues matches.** The group is the panel's
    /// unit — one row, one stop for `j`, one fork whose crossbar says how far the run
    /// reaches — so dropping members out of one would draw a fork with nothing to fork
    /// into and make the row lie about what shares the screen. The highlight is what says
    /// which member hit; `h`/`l` still walk all of them.
    ///
    /// **The cursor holds still while its own group still matches.** Typing another letter
    /// must not move a reader off the line they are reading, which is what a reset to the
    /// first match on every keystroke would do. Only a group filtered out from under the
    /// cursor moves it, and then to the first one still on screen.
    ///
    /// **No matches at all leaves the selection exactly where it is.** There is nowhere
    /// honest to put it, the preview pane goes on showing a real cue, and the panel says
    /// there are none rather than pointing at one it is not drawing.
    pub fn refilter(&mut self) {
        let query = self.cue_search.value.trim().to_string();
        self.visible = (0..self.groups.len())
            .filter(|index| {
                query.is_empty()
                    || self
                        .groups
                        .get(*index)
                        .is_some_and(|group| self.first_match_in(*group, &query).is_some())
            })
            .collect();
        // Cues rather than groups: the reader is counting the lines they were looking for,
        // and a group is a drawing decision they never asked about.
        self.cue_search.match_count = if query.is_empty() {
            0
        } else {
            self.cues
                .iter()
                .filter(|cue| Self::cue_matches(cue, &query))
                .count()
        };
        if self.visible.is_empty() || self.visible_position().is_some() {
            return;
        }
        self.remember_place();
        let first = self.visible[0];
        self.enter_group(first);
        self.select_cue();
    }

    /// Where the cursor's group sits in [`Self::visible`], or `None` when the filter has
    /// hidden it — which is the one case [`Self::refilter`] has to move the cursor for.
    ///
    /// A binary search rather than a scan: `visible` is ascending by construction, and this
    /// is read by every movement key and by every draw of the panel.
    fn visible_position(&self) -> Option<usize> {
        if self.groups.is_empty() {
            return None;
        }
        self.visible.binary_search(&self.selected_group()).ok()
    }

    /// The groups the panel is drawing, as positions in [`Self::groups`].
    pub fn visible_groups(&self) -> &[usize] {
        &self.visible
    }

    /// Opens the filter bar, remembering the cue to come back to if it is abandoned.
    ///
    /// The origin is taken only when there is not one already, so the several ways a bar
    /// can be re-opened mid-search — `/` pressed again, the bar re-activated after a
    /// keystroke was refused — all still return to where the search began rather than to
    /// wherever the filter had since moved the cursor.
    ///
    /// Pressing `Enter` clears it (see [`Self::finish_cue_search`]), so a search the reader
    /// *confirmed* and then re-opened comes back to where they confirmed it. That is the
    /// point of confirming: they accepted the row the filter put them on, and an `Esc`
    /// three searches later must not teleport them back to a cue they left long ago.
    pub fn start_cue_search(&mut self) {
        if self.search_origin.is_none() {
            self.search_origin = self.selected_origin();
        }
        self.cue_search.activate();
    }

    /// Leaves the bar with the filter still in force, handing the keys back to the list.
    pub fn finish_cue_search(&mut self) {
        self.cue_search.deactivate();
        self.search_origin = None;
    }

    /// Drops the filter and puts the cursor back where the search was opened.
    ///
    /// The origin is a [`CueOrigin`], so a cue the reader added or removed while the filter
    /// was up is still found — or honestly not found, in which case the cursor stays where
    /// the search left it rather than jumping somewhere arbitrary.
    pub fn cancel_cue_search(&mut self) {
        let origin = self.search_origin.take();
        self.clear_cue_search();
        if let Some(cue) = origin.and_then(|origin| self.position_of(origin)) {
            self.remember_place();
            self.selected = cue;
            self.group_page = self
                .groups
                .get(self.group_of(cue))
                .map(|group| (cue - group.first) / GROUP_COLUMNS)
                .unwrap_or(0);
            self.select_cue();
        }
    }

    /// Empties the query and leaves the bar, without moving the cursor.
    ///
    /// What `Esc` does to a filter left in force with the bar already closed, and what a
    /// new cue list does to one that was never closed at all.
    pub fn clear_cue_search(&mut self) {
        self.cue_search.clear();
        self.search_origin = None;
        self.refilter();
    }

    /// Which group holds the cue at this position in the list.
    ///
    /// Zero for a list with no groups at all, which is the empty track — every caller here
    /// has already established that there are cues, and `groups` is built with them.
    pub fn group_of(&self, cue: usize) -> usize {
        self.groups
            .partition_point(|group| group.end() <= cue)
            .min(self.groups.len().saturating_sub(1))
    }

    /// The group the cursor is in.
    pub fn selected_group(&self) -> usize {
        self.group_of(self.selected)
    }

    /// Which members of a group the panel has room to draw, as a position and a count.
    ///
    /// **The window is a page rather than a sliding pair**: the group is cut into runs of
    /// `GROUP_COLUMNS` from its first member, and the window is whichever run the cursor's
    /// page ([`Self::group_page`]) names. So `l` from a page's left member moves the
    /// highlight to the one beside it with the row standing still, and only the press after
    /// that turns the page — the reader gets to compare the two cues in front of them
    /// before the list changes under the cursor, where a window that slid on every press
    /// moved both cues every time and never let a pair settle.
    ///
    /// The last page backs up to keep two members drawn, since a page holding one would
    /// leave half the row empty next to the cue the cursor is on.
    ///
    /// **A group the cursor has left keeps the page it was left on** ([`Self::group_memory`]),
    /// rather than snapping back to its first members. The row the reader was just working
    /// in redrawing itself the moment they step off it is the same lost place as re-entering
    /// on the wrong cue — they can see it happen, one row up, as they press `j`. A group
    /// never visited has nothing to anchor a window on and shows its first members.
    ///
    /// **While a filter is in force, a group the cursor is not in is paged to its first
    /// match instead.** The row is on the list *because* something in it matched, so a
    /// page that did not hold that cue would be the panel showing two lines neither of
    /// which is the reason the reader can see them — worst on a group of four whose only
    /// match is its last member, which would otherwise never be drawn at all. The match
    /// outranks the remembered page for exactly that reason; the cursor's own group is
    /// still the exception, because there the reader is steering.
    pub fn group_window(&self, group: CueGroup) -> (usize, usize) {
        let shown = group.len.min(GROUP_COLUMNS);
        let query = self.cue_query();
        let page = if group.holds(self.selected) {
            self.group_page
        } else if !query.is_empty() {
            self.first_match_in(group, query)
                .map(|cue| (cue - group.first) / GROUP_COLUMNS)
                .unwrap_or(0)
        } else {
            self.group_memory
                .get(&self.group_of(group.first))
                .map(|(_, page)| *page)
                .unwrap_or(0)
        };
        let start = (group.first + page * GROUP_COLUMNS)
            .min(group.end().saturating_sub(shown))
            .max(group.first);
        (start, shown)
    }

    /// Moves the cue cursor by whole groups, reporting whether it actually moved.
    ///
    /// A group is one stop: `j` from the cue above a pair lands on the pair's first member,
    /// and `j` again leaves for the cue below it rather than visiting the second member.
    /// Moving *within* a group is `h`/`l` ([`Self::select_within_group`]).
    ///
    /// **A group is re-entered where it was left**, cue and page both
    /// ([`Self::group_memory`]). Stepping down a row to look at something and coming back
    /// otherwise cost the reader their place sideways, which on a long group is several
    /// presses of `l` to undo one press of `k`. A group the cursor has never been in is
    /// entered on its first member, so nothing has to be remembered before it exists.
    ///
    /// The return value is what stops a held-down `j` at the end of the list from
    /// re-requesting the same preview frame on every repeat — including the case that only
    /// grouping creates, where the cursor is on a later member of the last group and there
    /// is no group left to move to. Landing on that group's first member would be a jump
    /// backwards in answer to `j`, so it is refused instead.
    ///
    /// **Steps over the groups the panel is drawing, not over every group the track has**
    /// ([`Self::visible`]). A filter that left `j` walking through rows nobody can see
    /// would be a cursor disappearing for several presses at a time; unfiltered the two
    /// lists are the same, so this is the movement it always was.
    pub fn select(&mut self, delta: isize) -> bool {
        let Some(current) = self.visible_position() else {
            return false;
        };
        let last = self.visible.len() - 1;
        let next = if delta < 0 {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current.saturating_add(delta as usize).min(last)
        };
        if next == current {
            return false;
        }
        self.remember_place();
        let group = self.visible[next];
        self.enter_group(group);
        self.select_cue();
        true
    }

    /// Records where the cursor is being left, for the next `k` back into this group.
    fn remember_place(&mut self) {
        self.group_memory
            .insert(self.selected_group(), (self.selected, self.group_page));
    }

    /// Puts the cursor into a group, at the place it was left if it has been visited.
    ///
    /// The remembered cue is checked against the group rather than trusted: `cues` and
    /// `groups` are public, so a caller can leave a shorter list under a memory taken from
    /// a longer one, and a selection outside its group is a cursor the panel cannot draw.
    ///
    /// **While a filter is in force the cursor lands on the group's first match instead**,
    /// with the page set to show it — the same rule [`Self::group_window`] draws an
    /// unselected group by, applied to the one the reader has just arrived in. Searching
    /// for a word and landing on the line beside it would make `/` point at the wrong cue,
    /// and on a group of four whose match is its last member the reader would have to
    /// press `l` three times to reach what they searched for.
    fn enter_group(&mut self, index: usize) {
        let group = self.groups[index];
        let query = self.cue_query();
        let (cue, page) = if !query.is_empty() {
            let cue = self.first_match_in(group, query).unwrap_or(group.first);
            (cue, (cue - group.first) / GROUP_COLUMNS)
        } else {
            self.group_memory
                .get(&index)
                .copied()
                .filter(|(cue, _)| group.holds(*cue))
                .unwrap_or((group.first, 0))
        };
        self.selected = cue;
        self.group_page = page;
    }

    /// Moves the cursor between the cues of the group it is already in, for `h` and `l`.
    ///
    /// Stops at the group's ends rather than spilling into the next one: a group is a
    /// closed unit and `j`/`k` are the way in and out of it. Without that, `l` held down
    /// would walk the whole track sideways and the two axes would stop meaning different
    /// things.
    ///
    /// **The page turns only when the cursor leaves it**, and the page it turns to is the
    /// one the cue falls in counting from the group's first member. A cue that is already
    /// on screen is simply moved to, whichever of the two blocks it is in — including the
    /// left-hand block of a page that backed up, which is the case a page derived from the
    /// selection alone gets wrong.
    pub fn select_within_group(&mut self, delta: isize) -> bool {
        if self.groups.is_empty() {
            return false;
        }
        let group = self.groups[self.selected_group()];
        let next = if delta < 0 {
            self.selected
                .saturating_sub(delta.unsigned_abs())
                .max(group.first)
        } else {
            self.selected
                .saturating_add(delta as usize)
                .min(group.end() - 1)
        };
        if next == self.selected {
            return false;
        }
        let (start, shown) = self.group_window(group);
        if next < start || next >= start + shown {
            self.group_page = (next - group.first) / GROUP_COLUMNS;
        }
        self.selected = next;
        self.select_cue();
        true
    }

    /// The top of the list, which is the first cue rather than wherever the first group was
    /// last left: `gg` is an absolute move, and answering it with a remembered place would
    /// make it land somewhere the reader did not ask for. The memory is overwritten rather
    /// than left stale, so a later `k` back into the group agrees with where `gg` put the
    /// cursor.
    ///
    /// The top of the *drawn* list while a filter is in force — the first row the reader
    /// can see, entered on its match, since `gg` means "the top of this list" and the list
    /// is what the panel is showing.
    pub fn select_first(&mut self) -> bool {
        let Some(group) = self.visible.first().copied() else {
            return false;
        };
        self.jump_to_group(group)
    }

    /// The bottom of the list, which is the last *group's* first member rather than the
    /// last cue — the same place `j` would land on arriving there, so `G` and a held `j`
    /// agree about where the end is. The last drawn row while a filter is in force, for the
    /// reason `gg` takes the first one.
    pub fn select_last(&mut self) -> bool {
        let Some(group) = self.visible.last().copied() else {
            return false;
        };
        self.jump_to_group(group)
    }

    /// The absolute move `gg` and `G` share: into a group at its first member — or its
    /// first match, while a filter is in force — rather than at the place it was last left.
    ///
    /// The memory is overwritten rather than left stale, so a later `k` back into the group
    /// agrees with where the jump put the cursor.
    fn jump_to_group(&mut self, index: usize) -> bool {
        let group = self.groups[index];
        let query = self.cue_query();
        let target = if query.is_empty() {
            group.first
        } else {
            self.first_match_in(group, query).unwrap_or(group.first)
        };
        if self.selected == target {
            return false;
        }
        self.remember_place();
        self.selected = target;
        self.group_page = (target - group.first) / GROUP_COLUMNS;
        self.remember_place();
        self.select_cue();
        true
    }

    /// Rows one group costs the panel, not counting the `↓` above it.
    ///
    /// A lone cue is its block and nothing else. A group carries its own fork, rather than
    /// the fork being the connector into it, so that the fork is drawn at every scroll
    /// position — including when the group is the first thing on screen and has nothing
    /// above it. The markers saying a group reaches past its two visible members live on the
    /// fork's crossbar, and a marker that vanished when the list happened to scroll would be
    /// worse than none.
    ///
    /// The blocks are charged four rows rather than three because the later-starting member
    /// is drawn one row lower — and charged it whether or not that step is actually taken.
    /// Two members starting at the same instant are drawn level and leave the fourth row
    /// blank, which keeps a group's footprint the same as `h` and `l` move through it. A
    /// list that reflowed under a sideways keypress would be harder to read than one blank
    /// row is to spend.
    pub fn group_height(&self, group: usize) -> usize {
        match self.groups.get(group) {
            Some(group) if group.len > 1 => CUE_FORK_ROWS + CUE_GROUP_ROWS,
            _ => CUE_BLOCK_ROWS,
        }
    }

    /// How many groups fit in `height` rows starting from this one.
    ///
    /// At least one, so the shortest panel the layout can produce still shows the group
    /// under the cursor rather than nothing at all — a group that does not fit whole is
    /// clipped from the bottom, exactly as an over-tall cue block always was.
    ///
    /// `from` and the answer are positions in [`Self::visible`], not in `groups`: rows the
    /// filter is not drawing cost the panel nothing, so counting them would scroll the list
    /// past its own end.
    fn groups_fitting(&self, from: usize, height: usize) -> usize {
        let Some(first) = self.visible.get(from).copied() else {
            return 0;
        };
        let mut used = self.group_height(first);
        let mut count = 1;
        for group in self.visible.iter().skip(from + 1).copied() {
            let next = used + CUE_CONNECTOR_ROWS + self.group_height(group);
            if next > height {
                break;
            }
            used = next;
            count += 1;
        }
        count
    }

    /// The furthest the list can scroll before it starts showing blank rows under the last
    /// group. Walked backwards from the end, because that is the direction the constraint
    /// comes from.
    /// A position in [`Self::visible`], counting only the rows the filter is drawing — for
    /// the reason [`Self::groups_fitting`] does.
    fn max_scroll(&self, height: usize) -> usize {
        // Saturating rather than guarded: an empty list never reaches here — `cue_scroll`
        // has already returned — and if it did, the loop below is empty and the answer is
        // the zero it should be.
        let last = self.visible.len().saturating_sub(1);
        let mut used = self
            .visible
            .get(last)
            .map(|group| self.group_height(*group))
            .unwrap_or(0);
        let mut scroll = last;
        for position in (0..last).rev() {
            let next = used + CUE_CONNECTOR_ROWS + self.group_height(self.visible[position]);
            if next > height {
                break;
            }
            used = next;
            scroll = position;
        }
        scroll
    }

    /// Scrolls the cue list just far enough to keep the selection visible.
    ///
    /// Called from the renderer, which is the only place that knows how many rows the list
    /// actually got — the same arrangement `sync_batch_scroll` uses for the batch dialog.
    ///
    /// Takes a height in rows and works out the groups itself, where this used to take a
    /// count the renderer had divided out. It cannot be a division any more: a lone cue and
    /// a pair are different heights, so how many rows a screenful costs depends on which
    /// groups are in it.
    ///
    /// `list_scroll` and `list_rows` are positions and counts in [`Self::visible`], so a
    /// filtered list scrolls through what it is drawing rather than through the whole
    /// track. Unfiltered the two are the same list.
    ///
    /// A filter matching nothing leaves the scroll where an empty track leaves it: there
    /// are no rows, so there is nothing to keep on screen and nothing to draw.
    pub fn cue_scroll(&mut self, height: usize) {
        if height == 0 || self.visible.is_empty() {
            self.list_rows = 0;
            self.list_scroll = 0;
            return;
        }
        // The cursor's group is off the drawn list only while the filter hides it, which
        // `refilter` allows exactly when nothing matches at all — and that has returned
        // above. Zero rather than a guard, so the scroll is still clamped below.
        let selected = self.visible_position().unwrap_or(0);
        if selected < self.list_scroll {
            self.list_scroll = selected;
        }
        // Upwards one group at a time rather than by arithmetic: the answer depends on the
        // heights of the groups being skipped, so there is nothing to solve in closed form.
        while selected >= self.list_scroll + self.groups_fitting(self.list_scroll, height) {
            self.list_scroll += 1;
        }
        self.list_scroll = self.list_scroll.min(self.max_scroll(height));
        self.list_rows = self.groups_fitting(self.list_scroll, height);
    }
}

#[cfg(test)]
mod tests {
    use kernal::prelude::*;

    use super::*;
    use crate::audio::SilentOutput;
    use crate::preview::PlaybackSpeed;

    fn cue(start: u64, end: u64, text: &str) -> Cue {
        Cue {
            index: 0,
            start: Duration::from_millis(start),
            end: Duration::from_millis(end),
            text: text.to_string(),
            dialogue: Vec::new(),
            events: 1,
        }
    }

    fn state() -> SubtitleEditState {
        costed_state(CHEAP_BYTES_PER_CELL)
    }

    /// A per-cell cost small enough that no pane reaches [`FRAME_WINDOW_BUDGET`], so tests
    /// that are not about the budget get the full window. Sixel on a small font is around
    /// this; kitty is two orders of magnitude more, which is what
    /// `a_costly_window_should_be_shortened_to_fit_the_budget` uses instead.
    const CHEAP_BYTES_PER_CELL: u64 = 12;

    fn costed_state(frame_cost: u64) -> SubtitleEditState {
        SubtitleEditState::new(
            1,
            FrameSource {
                media: PathBuf::from("/media/show.mkv"),
                media_length: 4096,
                media_modified: None,
                pixels: (960, 540),
                style: Arc::new(CueStyle::SubRip),
                workspace: PathBuf::from("/tmp/reel-tui-preview/state"),
            },
            SubtitleSource::Embedded(2),
            Duration::from_secs(600),
            PreviewSupport::Available,
            frame_cost,
            PreviewWorkspace::new().unwrap(),
        )
    }

    /// A protocol occupying exactly `width` x `height` cells.
    ///
    /// Encoded as halfblocks purely as a fixture: what these tests assert is the window
    /// arithmetic, which is driven by the `frame_cost` handed to [`costed_state`] rather
    /// than by the encoder, and halfblocks is the cheapest way to obtain a `Protocol` of a
    /// known cell size. Production never sees this one — `preview::drawing_picker` refuses
    /// a halfblocks terminal at startup.
    ///
    /// Sized in pixels from the picker's font size, because `Resize::Fit` takes the cell
    /// size from the image's own proportions rather than from what it was asked for.
    fn protocol(width: u16, height: u16) -> Box<Protocol> {
        let picker = ratatui_image::picker::Picker::halfblocks();
        let font = picker.font_size();
        let image = image::DynamicImage::new_rgb8(
            u32::from(width) * u32::from(font.width),
            u32::from(height) * u32::from(font.height),
        );
        Box::new(
            picker
                .new_protocol(
                    image,
                    Size::new(width, height),
                    ratatui_image::Resize::Fit(None),
                )
                .expect("halfblocks should encode any image"),
        )
    }

    fn ready(count: usize) -> SubtitleEditState {
        let mut state = state();
        let cues = (0..count)
            .map(|index| {
                let start = index as u64 * 2000;
                cue(start, start + 1000, &format!("line {index}"))
            })
            .collect();
        state.apply_prepared(cues, CueStyle::SubRip);
        state
    }

    #[test]
    fn a_new_page_should_start_preparing_with_nothing_to_show() {
        // Act
        let state = state();

        // Assert
        assert_that!(state.status.clone()).is_equal_to(LoadStatus::Preparing);
        assert_that!(state.is_busy()).is_true();
        assert_that!(state.cues.as_slice()).is_empty();
        assert_that!(state.selected_cue()).is_none();
    }

    /// A page opened again after a save put the cursor back where the reader left it — and
    /// on a file that came back shorter, as far down as there is list to land on.
    #[test]
    fn a_restored_page_should_come_back_to_the_cue_it_was_left_on() {
        // Arrange
        let mut state = state();

        // Act
        state.restore_selection(2);
        state.apply_prepared(
            vec![
                cue(0, 1000, "a"),
                cue(2000, 3000, "b"),
                cue(4000, 5000, "c"),
            ],
            CueStyle::SubRip,
        );

        // Assert
        assert_that!(state.selected).is_equal_to(2);

        // And a cue that is no longer there lands on the last one that is.
        state.restore_selection(9);
        state.apply_prepared(
            vec![cue(0, 1000, "a"), cue(2000, 3000, "b")],
            CueStyle::SubRip,
        );
        assert_that!(state.selected).is_equal_to(1);

        // With nothing to restore, the page opens on its first cue as it always has.
        state.apply_prepared(
            vec![cue(0, 1000, "a"), cue(2000, 3000, "b")],
            CueStyle::SubRip,
        );
        assert_that!(state.selected).is_equal_to(0);
    }

    /// Restoring into a group has to bring the drawn page with it, or the cursor lands on a
    /// member the panel is not showing.
    #[test]
    fn a_restored_page_should_show_the_page_of_the_group_it_lands_in() {
        // Arrange: one group of four cues, all sharing the screen.
        let mut state = state();

        // Act: back onto the third member, which is the second page of that group.
        state.restore_selection(2);
        state.apply_prepared(
            vec![
                cue(0, 9000, "a"),
                cue(1000, 9000, "b"),
                cue(2000, 9000, "c"),
                cue(3000, 9000, "d"),
            ],
            CueStyle::SubRip,
        );

        // Assert
        assert_that!(state.selected).is_equal_to(2);
        let group = state.groups[state.selected_group()];
        assert_that!(state.group_window(group)).is_equal_to((2, 2));
    }

    #[test]
    fn apply_prepared_should_pack_lanes_and_select_the_first_cue() {
        // Arrange: the middle cue overlaps the first, so the track needs two lanes.
        let mut state = state();
        state.selected = 5;

        // Act
        state.apply_prepared(
            vec![
                cue(0, 3000, "a"),
                cue(1000, 4000, "b"),
                cue(5000, 6000, "c"),
            ],
            CueStyle::SubRip,
        );

        // Assert
        assert_that!(state.status.clone()).is_equal_to(LoadStatus::Ready);
        assert_that!(state.layout.lane_count).is_equal_to(2);
        assert_that!(state.layout.lanes.as_slice()).contains_exactly_in_given_order([0, 1, 0]);
        assert_that!(state.selected).is_equal_to(0);
        assert_that!(state.is_busy()).is_false();
    }

    /// An empty track is not a failure — the file was read, it simply holds no cues —
    /// and reporting it as one would send the user hunting for a problem that is not
    /// there.
    #[test]
    fn apply_prepared_should_report_an_empty_track_separately_from_a_failure() {
        // Arrange
        let mut state = state();

        // Act
        state.apply_prepared(Vec::new(), CueStyle::SubRip);

        // Assert
        assert_that!(state.status.clone()).is_equal_to(LoadStatus::Empty);
        assert_that!(state.is_busy()).is_false();
    }

    #[test]
    fn fail_should_report_the_message_and_drop_any_cues() {
        // Arrange
        let mut state = ready(3);

        // Act
        state.fail("ffmpeg exploded".to_string());

        // Assert
        assert_that!(state.status.clone())
            .is_equal_to(LoadStatus::Failed("ffmpeg exploded".to_string()));
        assert_that!(state.cues.as_slice()).is_empty();
        assert_that!(state.layout.lane_count).is_equal_to(0);
        assert_that!(state.is_busy()).is_false();
    }

    #[test]
    fn select_should_move_the_cursor_and_report_that_it_moved() {
        // Arrange
        let mut state = ready(3);

        // Act / Assert
        assert_that!(state.select(1)).is_true();
        assert_that!(state.selected).is_equal_to(1);
        assert_that!(state.select(-1)).is_true();
        assert_that!(state.selected).is_equal_to(0);
    }

    /// Holding `j` at the end of the list must not keep re-requesting the same preview
    /// frame, which is one `ffmpeg` process per key repeat.
    #[test]
    fn select_should_report_no_movement_at_either_end_of_the_list() {
        // Arrange
        let mut state = ready(3);

        // Act / Assert
        assert_that!(state.select(-1)).is_false();
        assert_that!(state.selected).is_equal_to(0);
        state.select(10);
        assert_that!(state.selected).is_equal_to(2);
        assert_that!(state.select(1)).is_false();
        assert_that!(state.selected).is_equal_to(2);
    }

    #[test]
    fn select_should_do_nothing_on_a_track_without_cues() {
        // Arrange
        let mut state = state();

        // Act / Assert
        assert_that!(state.select(1)).is_false();
        assert_that!(state.select_first()).is_false();
        assert_that!(state.select_last()).is_false();
    }

    #[test]
    fn select_first_and_last_should_jump_to_the_ends() {
        // Arrange
        let mut state = ready(5);

        // Act / Assert
        assert_that!(state.select_last()).is_true();
        assert_that!(state.selected).is_equal_to(4);
        assert_that!(state.select_last()).is_false();
        assert_that!(state.select_first()).is_true();
        assert_that!(state.selected).is_equal_to(0);
        assert_that!(state.select_first()).is_false();
    }

    /// A track whose middle three cues share the screen: `a`, then the group `b`/`c`/`d`,
    /// then `e`. Cue positions and group indices deliberately differ, so a test that
    /// confused the two would fail rather than pass by coincidence.
    fn grouped() -> SubtitleEditState {
        let mut state = state();
        state.apply_prepared(
            vec![
                cue(0, 1000, "a"),
                cue(2000, 5000, "b"),
                cue(3000, 6000, "c"),
                cue(4000, 7000, "d"),
                cue(8000, 9000, "e"),
            ],
            CueStyle::SubRip,
        );
        state
    }

    #[test]
    fn j_should_step_over_a_whole_group_rather_than_through_it() {
        // Arrange
        let mut state = grouped();

        // Act / Assert: into the group at its first member, then straight out of it.
        assert_that!(state.select(1)).is_true();
        assert_that!(state.selected).is_equal_to(1);
        assert_that!(state.select(1)).is_true();
        assert_that!(state.selected).is_equal_to(4);
        assert_that!(state.select(1)).is_false();
    }

    /// `k` re-enters a group on the same member `j` enters it on, so the two directions
    /// agree about where a group is and nothing has to be remembered between them.
    #[test]
    fn k_should_re_enter_a_group_on_the_member_j_would_have_landed_on() {
        // Arrange
        let mut state = grouped();
        state.select(2);
        assert_that!(state.selected).is_equal_to(4);

        // Act / Assert
        assert_that!(state.select(-1)).is_true();
        assert_that!(state.selected).is_equal_to(1);
        assert_that!(state.select(-1)).is_true();
        assert_that!(state.selected).is_equal_to(0);
    }

    /// A `j` that would land *behind* the cursor is refused rather than obeyed. Only
    /// grouping creates the case: sitting on a later member of the last group, the group
    /// step has nowhere to go, and moving to the group's first member would answer "down"
    /// by going up.
    #[test]
    fn j_should_do_nothing_from_a_later_member_of_the_last_group() {
        // Arrange: a track that ends in its group, with the cursor on the last member.
        let mut state = state();
        state.apply_prepared(
            vec![
                cue(0, 1000, "a"),
                cue(2000, 5000, "b"),
                cue(3000, 6000, "c"),
            ],
            CueStyle::SubRip,
        );
        state.select(1);
        state.select_within_group(1);
        assert_that!(state.selected).is_equal_to(2);

        // Act / Assert
        assert_that!(state.select(1)).is_false();
        assert_that!(state.selected).is_equal_to(2);
    }

    #[test]
    fn h_and_l_should_move_between_the_cues_of_a_group_and_stop_at_its_ends() {
        // Arrange
        let mut state = grouped();
        state.select(1);

        // Act / Assert: along the group, then held against its far end.
        assert_that!(state.select_within_group(1)).is_true();
        assert_that!(state.selected).is_equal_to(2);
        assert_that!(state.select_within_group(1)).is_true();
        assert_that!(state.selected).is_equal_to(3);
        assert_that!(state.select_within_group(1)).is_false();
        assert_that!(state.selected).is_equal_to(3);

        // Act / Assert: and back, held against the near end.
        assert_that!(state.select_within_group(-2)).is_true();
        assert_that!(state.selected).is_equal_to(1);
        assert_that!(state.select_within_group(-1)).is_false();
        assert_that!(state.selected).is_equal_to(1);
    }

    /// The common case, and the one that makes `h`/`l` safe to hold down: a cue that
    /// overlaps nothing has nowhere sideways to go.
    #[test]
    fn h_and_l_should_do_nothing_on_a_cue_that_overlaps_nothing() {
        // Arrange
        let mut state = grouped();

        // Act / Assert
        assert_that!(state.select_within_group(1)).is_false();
        assert_that!(state.select_within_group(-1)).is_false();
        assert_that!(state.selected).is_equal_to(0);
    }

    #[test]
    fn moving_within_a_group_should_do_nothing_on_a_track_with_no_cues() {
        // Arrange
        let mut state = state();

        // Act / Assert
        assert_that!(state.select_within_group(1)).is_false();
        assert_that!(state.select(1)).is_false();
        assert_that!(state.select_last()).is_false();
    }

    /// `G` lands where a held `j` would have stopped, rather than on the very last cue —
    /// otherwise the two disagree about where the end of the list is.
    #[test]
    fn select_last_should_land_on_the_final_groups_first_member() {
        // Arrange: a track ending in a group.
        let mut state = state();
        state.apply_prepared(
            vec![
                cue(0, 1000, "a"),
                cue(2000, 5000, "b"),
                cue(3000, 6000, "c"),
            ],
            CueStyle::SubRip,
        );

        // Act / Assert
        assert_that!(state.select_last()).is_true();
        assert_that!(state.selected).is_equal_to(1);
        assert_that!(state.select_last()).is_false();
    }

    /// The window is a page rather than a sliding pair, so the first `l` crosses the two
    /// cues on screen without moving them and the next turns the page — backing up at the
    /// group's end so two members are still shown, which is the one place the selected
    /// member is the right-hand block.
    #[test]
    fn the_group_window_should_page_and_back_up_at_the_end() {
        // Arrange
        let mut state = grouped();
        let group = state.groups[1];
        state.select(1);

        // Act / Assert
        assert_that!(state.group_window(group)).is_equal_to((1, 2));
        // Across the page: the cursor moves, the pair does not.
        state.select_within_group(1);
        assert_that!(state.group_window(group)).is_equal_to((1, 2));
        assert_that!(state.selected).is_equal_to(2);
        // The page turns, and backs up rather than running off the end: the cursor is on
        // the second of the two.
        state.select_within_group(1);
        assert_that!(state.group_window(group)).is_equal_to((2, 2));
        assert_that!(state.selected).is_equal_to(3);
    }

    /// Stepping down a row to look at something and coming back should not cost the reader
    /// their place sideways. The cue *and* the page are both remembered, or a `k` back into
    /// a long group would land on the right cue with the wrong pair drawn around it.
    #[test]
    fn a_group_should_be_re_entered_where_it_was_left() {
        // Arrange: into the group, then across it.
        let mut state = grouped();
        let group = state.groups[1];
        state.select(1);
        state.select_within_group(2);
        assert_that!(state.selected).is_equal_to(3);
        assert_that!(state.group_window(group)).is_equal_to((2, 2));

        // Act: down out of the group and back up into it.
        assert_that!(state.select(1)).is_true();
        assert_that!(state.selected).is_equal_to(4);
        assert_that!(state.select(-1)).is_true();

        // Assert: the cue the cursor was on, with the pair it was drawn in.
        assert_that!(state.selected).is_equal_to(3);
        assert_that!(state.group_window(group)).is_equal_to((2, 2));
    }

    /// The row the reader was working in must not redraw itself the moment they step off
    /// it: a group the cursor has left keeps the page it was left on, which is visible one
    /// row up as `j` is pressed.
    #[test]
    fn a_group_the_cursor_has_left_should_keep_the_page_it_was_left_on() {
        // Arrange: across the group, then out of it.
        let mut state = grouped();
        let group = state.groups[1];
        state.select(1);
        state.select_within_group(2);
        assert_that!(state.group_window(group)).is_equal_to((2, 2));

        // Act
        state.select(1);

        // Assert: still the pair it was left showing, though the cursor is elsewhere.
        assert_that!(state.selected_group()).is_equal_to(2);
        assert_that!(state.group_window(group)).is_equal_to((2, 2));
    }

    /// A group the cursor has never been in has nothing to remember, so it is entered on
    /// its first member — and `gg`/`G` are absolute moves that say where they land rather
    /// than deferring to a remembered place.
    #[test]
    fn an_unvisited_group_and_the_absolute_moves_should_enter_on_the_first_member() {
        // Arrange
        let mut state = grouped();

        // Act / Assert: never visited, so the group's first member.
        state.select(1);
        assert_that!(state.selected).is_equal_to(1);

        // Act / Assert: `G` lands on the last group's first member, and leaves the memory
        // agreeing with that rather than stale.
        state.select_within_group(2);
        state.select_last();
        let last = state.groups.last().unwrap().first;
        assert_that!(state.selected).is_equal_to(last);
        state.select(-1);
        state.select(1);
        assert_that!(state.selected).is_equal_to(last);

        // Act / Assert: and `gg` the same way at the top.
        state.select_first();
        assert_that!(state.selected).is_equal_to(0);
    }

    /// A group of three ends on a page that backed up to keep two blocks drawn, so its
    /// middle member is the *left* block there and the *right* block one page earlier.
    /// Deriving the page from the selection alone made `h` from the right-hand block turn
    /// the page rather than move to the block beside it: the cue the reader was pointing at
    /// jumped off the row on its way to being selected.
    #[test]
    fn moving_onto_a_cue_already_on_screen_should_not_turn_the_page() {
        // Arrange: the cursor at the far end of a group of three, on the backed-up page.
        let mut state = grouped();
        let group = state.groups[1];
        state.select(1);
        state.select_within_group(2);
        assert_that!(state.selected).is_equal_to(3);
        assert_that!(state.group_window(group)).is_equal_to((2, 2));

        // Act: back onto the cue in the block beside it.
        state.select_within_group(-1);

        // Assert: the pair on screen is untouched — only the highlight moved.
        assert_that!(state.selected).is_equal_to(2);
        assert_that!(state.group_window(group)).is_equal_to((2, 2));

        // Act / Assert: the press after that is the one that turns the page.
        state.select_within_group(-1);
        assert_that!(state.selected).is_equal_to(1);
        assert_that!(state.group_window(group)).is_equal_to((1, 2));
    }

    /// A group the cursor is not in has nothing to anchor a window on, so it shows its
    /// first members — including when the cursor is in a *different* group, which is the
    /// case that would otherwise leak one group's cursor into another's window.
    #[test]
    fn the_group_window_should_show_the_first_members_of_a_group_the_cursor_is_not_in() {
        // Arrange
        let mut state = grouped();
        state.select(1);
        state.select_within_group(2);
        assert_that!(state.selected).is_equal_to(3);

        // Act / Assert
        assert_that!(state.group_window(state.groups[0])).is_equal_to((0, 1));
        assert_that!(state.group_window(state.groups[2])).is_equal_to((4, 1));
    }

    #[test]
    fn group_of_should_answer_for_every_cue_and_tolerate_an_empty_track() {
        // Arrange
        let grouped = grouped();
        let empty = state();

        // Act / Assert
        let found: Vec<usize> = (0..grouped.cues.len())
            .map(|cue| grouped.group_of(cue))
            .collect();
        assert_that!(found).contains_exactly_in_given_order([0, 1, 1, 1, 2]);
        assert_that!(empty.group_of(0)).is_equal_to(0);
    }

    /// Rows a panel needs to show this many groups of a `ready` track, where every cue
    /// overlaps nothing and so is a lone block with a `↓` between one and the next.
    fn rows_for(groups: usize) -> usize {
        CUE_BLOCK_ROWS + groups.saturating_sub(1) * (CUE_CONNECTOR_ROWS + CUE_BLOCK_ROWS)
    }

    #[test]
    fn cue_scroll_should_follow_the_selection_down_past_the_last_visible_row() {
        // Arrange
        let mut state = ready(10);
        state.cue_scroll(rows_for(4));

        // Act
        state.select(5);
        state.cue_scroll(rows_for(4));

        // Assert
        assert_that!(state.list_scroll).is_equal_to(2);
        assert_that!(state.list_rows).is_equal_to(4);
    }

    #[test]
    fn cue_scroll_should_follow_the_selection_back_up_above_the_first_visible_row() {
        // Arrange
        let mut state = ready(10);
        state.select(9);
        state.cue_scroll(rows_for(4));

        // Act
        state.select(-9);
        state.cue_scroll(rows_for(4));

        // Assert
        assert_that!(state.list_scroll).is_equal_to(0);
    }

    #[test]
    fn cue_scroll_should_not_leave_blank_rows_below_a_short_list() {
        // Arrange: scrolled to the bottom, then given a taller pane.
        let mut state = ready(6);
        state.select(5);
        state.cue_scroll(rows_for(2));
        assert_that!(state.list_scroll).is_equal_to(4);

        // Act
        state.cue_scroll(rows_for(6));

        // Assert
        assert_that!(state.list_scroll).is_equal_to(0);
    }

    /// A group costs twice what a lone cue does — its fork and the row its second member
    /// is dropped by — so a screenful is no longer a division. A panel holding three lone
    /// cues holds only two rows once the middle one is a group.
    #[test]
    fn cue_scroll_should_charge_a_group_for_the_rows_it_actually_takes() {
        // Arrange: cue 1 and cue 2 overlap, so the track is three groups, the middle of
        // them a pair.
        let mut state = state();
        state.apply_prepared(
            vec![
                cue(0, 1000, "a"),
                cue(2000, 4000, "b"),
                cue(3000, 5000, "c"),
                cue(6000, 7000, "d"),
            ],
            CueStyle::SubRip,
        );

        // Act / Assert: a lone cue and the group is all that fits in the rows three lone
        // cues would have taken.
        state.cue_scroll(rows_for(3));
        assert_that!(state.list_rows).is_equal_to(2);

        // Act / Assert: one row short of the group is one row short of showing it.
        state.cue_scroll(CUE_BLOCK_ROWS + CUE_CONNECTOR_ROWS + CUE_FORK_ROWS + CUE_GROUP_ROWS);
        assert_that!(state.list_rows).is_equal_to(2);
        state.cue_scroll(CUE_BLOCK_ROWS + CUE_CONNECTOR_ROWS + CUE_FORK_ROWS + CUE_GROUP_ROWS - 1);
        assert_that!(state.list_rows).is_equal_to(1);
    }

    /// A panel too short for even one row still shows the group under the cursor rather
    /// than nothing at all — the renderer clips it from the bottom.
    #[test]
    fn cue_scroll_should_always_keep_one_group_on_screen() {
        // Arrange
        let mut state = ready(10);
        state.select(9);

        // Act
        state.cue_scroll(1);

        // Assert
        assert_that!(state.list_rows).is_equal_to(1);
        assert_that!(state.list_scroll).is_equal_to(9);
    }

    #[test]
    fn cue_scroll_should_record_the_measured_row_count_and_tolerate_a_pane_with_none() {
        // Arrange
        let mut state = ready(10);
        state.select(9);

        // Act
        state.cue_scroll(0);

        // Assert
        assert_that!(state.list_rows).is_equal_to(0);
        assert_that!(state.list_scroll).is_equal_to(0);
    }

    /// A page holding no cues is still rendered — `cues` is public and a track can be
    /// emptied under one — so a panel with rows to give and nothing to put in them has to
    /// answer rather than divide by an empty list.
    #[test]
    fn cue_scroll_should_tolerate_a_track_with_no_cues_in_a_pane_with_rows() {
        // Arrange
        let mut state = state();

        // Act
        state.cue_scroll(20);

        // Assert
        assert_that!(state.list_rows).is_equal_to(0);
        assert_that!(state.list_scroll).is_equal_to(0);
    }

    /// The take-if-due the state used to expose, which `App::start_pending_preview` now
    /// spells out itself so that a frame already in the cache can skip the wait.
    fn take_due(state: &mut SubtitleEditState) -> bool {
        let due = state.frame_request_due();
        if due {
            state.clear_frame_request();
        }
        due
    }

    #[test]
    fn selected_cue_should_return_the_cue_under_the_cursor() {
        // Arrange
        let mut state = ready(3);
        state.select(2);

        // Act / Assert
        assert_that!(state.selected_cue().map(|cue| cue.text.as_str())).is_equal_to(Some("line 2"));
    }

    /// The workspace exists for exactly as long as the page does. Leaking it would leave
    /// a copy of every previewed subtitle in the temp directory for the session's life.
    #[test]
    fn dropping_the_page_should_remove_its_workspace() {
        // Arrange
        let state = state();
        let path = state.workspace().to_path_buf();
        std::fs::write(path.join("cues.srt"), "1\n").unwrap();
        assert_that!(path.exists()).is_true();

        // Act
        drop(state);

        // Assert
        assert_that!(path.exists()).is_false();
    }

    #[test]
    fn two_workspaces_should_not_share_a_directory() {
        // Act
        let first = PreviewWorkspace::new().unwrap();
        let second = PreviewWorkspace::new().unwrap();

        // Assert
        assert_that!(first.path() == second.path()).is_false();
    }

    /// The page's state is printed whole in every test failure and in the harness's
    /// timeout dump, and `Protocol` contributes nothing to that on its own.
    #[test]
    fn a_stored_frame_should_describe_itself_by_cue_and_size() {
        // Arrange
        let mut state = ready(2);
        state.apply_frame(1, protocol(10, 5));

        // Act
        let described = format!("{:?}", state.encoded);

        // Assert
        assert_that!(described.as_str()).contains("cue_index: 1");
        assert_that!(described.as_str()).contains("width: 10");
        assert_that!(described.as_str()).contains("height: 5");
    }

    /// The cursor's frame is printed in the same dumps and names a moment rather than a
    /// cue, which is the whole reason it is a second type instead of a `Frame`.
    #[test]
    fn the_cursors_frame_should_describe_itself_by_moment_and_size() {
        // Arrange
        let mut state = ready(2);
        state.focus_timeline();
        state.move_cursor(1, TIMELINE_STEP);
        state.apply_scrub_frame(state.cursor().unwrap(), protocol(10, 5));

        // Act
        let described = format!("{:?}", state.scrub);

        // Assert
        assert_that!(described.as_str()).contains("ScrubFrame");
        assert_that!(described.as_str()).contains("500ms");
        assert_that!(described.as_str()).contains("width: 10");
        assert_that!(described.as_str()).contains("height: 5");
    }

    /// A frame belongs to the cue it was drawn for. Showing it under a different cue
    /// would read as the burn-in being wrong, which is the one thing this page exists to
    /// let you judge.
    #[test]
    fn a_frame_should_only_be_drawn_for_the_cue_it_was_rendered_for() {
        // Arrange
        let mut state = ready(3);

        // Act
        state.apply_frame(0, protocol(10, 5));

        // Assert
        assert_that!(state.frame().is_some()).is_true();
        state.select(1);
        assert_that!(state.frame().is_some()).is_false();
        state.select(-1);
        assert_that!(state.frame().is_some()).is_true();
    }

    /// The point of holding more than one frame: moving the cursor onto a cue whose frame
    /// is already encoded draws it in the same pass that handled the keypress, with no
    /// round trip to the worker and so no empty pane in between.
    #[test]
    fn a_frame_already_encoded_should_be_drawn_the_moment_the_cursor_reaches_it() {
        // Arrange
        let mut state = ready(6);
        state.apply_frame(0, protocol(10, 5));
        state.apply_frame(1, protocol(10, 5));
        state.apply_frame(2, protocol(10, 5));

        // Act / Assert: every step forward lands on a frame that is already there.
        for _ in 0..2 {
            assert_that!(state.select(1)).is_true();
            assert_that!(state.frame().is_some()).is_true();
        }

        // Act / Assert: and the step past the window does not, which is what the request
        // it leaves pending is for.
        assert_that!(state.select(1)).is_true();
        assert_that!(state.frame().is_some()).is_false();

        // Cleanup
        drop(state);
    }

    /// A `Protocol` holds the picture encoded for the pane, so the window has to be a
    /// window rather than a growing pile — walking a thousand-cue track would otherwise
    /// keep every frame it passed.
    #[test]
    fn frames_further_than_the_window_should_be_dropped_as_the_cursor_moves() {
        // Arrange
        let mut state = ready(10);
        for cue_index in 0..=NEARBY_FRAMES {
            state.apply_frame(cue_index, protocol(10, 5));
        }
        assert_that!(state.encoded.len()).is_equal_to(NEARBY_FRAMES + 1);

        // Act: far enough that nothing held is still near the cursor.
        assert_that!(state.select(9)).is_true();

        // Assert
        assert_that!(state.encoded.is_empty()).is_true();
        // And a frame that arrives for a cue the cursor has already left is not kept
        // either — the answer to a request the selection outran.
        state.apply_frame(0, protocol(10, 5));
        assert_that!(state.encoded.is_empty()).is_true();

        // Cleanup
        drop(state);
    }

    /// One cue re-rendered — after a resize, or after the pane changed — replaces the
    /// frame held for it rather than stacking a second entry under the same index.
    #[test]
    fn a_second_frame_for_one_cue_should_replace_the_first() {
        // Arrange
        let mut state = ready(3);
        state.apply_frame(0, protocol(10, 5));

        // Act
        state.apply_frame(0, protocol(6, 3));

        // Assert
        assert_that!(state.encoded.len()).is_equal_to(1);
        assert_that!(state.frame().map(|protocol| protocol.size().width)).is_equal_to(Some(6));

        // Cleanup
        drop(state);
    }

    /// Every frame on hand was encoded for the pane it was asked for, and `Image` draws
    /// nothing at all rather than clipping when one no longer fits.
    #[test]
    fn resizing_the_pane_should_drop_every_frame_on_hand() {
        // Arrange
        let mut state = ready(3);
        state.set_preview_cells(Size::new(40, 20));
        state.apply_frame(0, protocol(10, 5));
        state.apply_frame(1, protocol(10, 5));

        // Act
        state.set_preview_cells(Size::new(30, 20));

        // Assert
        assert_that!(state.encoded.is_empty()).is_true();
        assert_that!(state.frame_requested()).is_true();

        // Cleanup
        drop(state);
    }

    /// The worker is told what is missing, not what the window is: a cue already encoded
    /// would otherwise be read out of the cache and re-encoded on every cursor move.
    #[test]
    fn nearby_targets_should_name_the_cues_around_the_cursor_that_have_no_frame_yet() {
        // Arrange
        let mut state = ready(9);
        assert_that!(state.select(4)).is_true();

        // Act
        let targets = state.nearby_frame_targets();

        // Assert: both directions, nearest first, and never the selection itself.
        assert_that!(
            targets
                .iter()
                .map(|target| target.cue_index)
                .collect::<Vec<_>>()
        )
        .is_equal_to(vec![3, 5, 2, 6]);
        assert_that!(targets[0].cue.text.as_str()).is_equal_to("line 3");
        // The seek is the moment the cue comes in, the same one the background pass renders
        // under, or the two would write different pictures for one cache key.
        assert_that!(targets[0].seek).is_equal_to(seek_for(&state.cues[3], state.duration));

        // Act / Assert: the ones already encoded drop out.
        state.apply_frame(3, protocol(10, 5));
        state.apply_frame(6, protocol(10, 5));
        assert_that!(
            state
                .nearby_frame_targets()
                .iter()
                .map(|target| target.cue_index)
                .collect::<Vec<_>>()
        )
        .is_equal_to(vec![5, 2]);

        // Cleanup
        drop(state);
    }

    /// The window runs off both ends of a short track, and an index that does not exist
    /// must not become a request for a cue the page does not have.
    #[test]
    fn nearby_targets_should_stop_at_the_ends_of_the_track() {
        // Arrange
        let mut state = ready(2);

        // Act / Assert: nothing before the first cue.
        assert_that!(
            state
                .nearby_frame_targets()
                .iter()
                .map(|target| target.cue_index)
                .collect::<Vec<_>>()
        )
        .is_equal_to(vec![1]);

        // Act / Assert: nothing after the last.
        assert_that!(state.select_last()).is_true();
        assert_that!(
            state
                .nearby_frame_targets()
                .iter()
                .map(|target| target.cue_index)
                .collect::<Vec<_>>()
        )
        .is_equal_to(vec![0]);

        // Cleanup
        drop(state);
    }

    /// A frame is a picture of the whole screen, so what the page asks the worker to burn in
    /// is every cue on screen at the moment being grabbed — not the selected one alone.
    ///
    /// This is what a typeset or karaoke track needs to preview at all: one visible line is
    /// routinely a dozen events sharing a moment, and any one of them drawn by itself is a
    /// fraction of a picture the viewer never sees.
    #[test]
    fn a_frame_target_should_carry_every_cue_on_screen_with_the_one_it_is_for() {
        // Arrange: a line with two effect cues coming in with it, one arriving later in its
        // span, and an unrelated cue after.
        let mut state = state();
        state.apply_prepared(
            vec![
                cue(2000, 3000, "under"),
                cue(2000, 2600, "effect one"),
                cue(2000, 2600, "effect two"),
                cue(2400, 3000, "later"),
                cue(8000, 9000, "elsewhere"),
            ],
            CueStyle::SubRip,
        );

        // Act
        let target = state.frame_target(0).expect("the first cue has a target");

        // Assert: the grab lands where the line comes in, so the picture holds what is up
        // at that instant — and not the cue that only joins it later in the span.
        assert_that!(target.seek).is_equal_to(Duration::from_millis(2000));
        assert_that!(
            target
                .on_screen
                .iter()
                .map(|cue| cue.text.clone())
                .collect::<Vec<_>>()
        )
        .is_equal_to(vec![
            "under".to_string(),
            "effect one".to_string(),
            "effect two".to_string(),
        ]);
        // Still filed against the cue the cursor is on, whatever else is in the picture.
        assert_that!(target.cue_index).is_equal_to(0);
        assert_that!(target.cue.text.as_str()).is_equal_to("under");

        // Act / Assert: and the later cue's own frame, grabbed where *it* comes in, holds
        // the line it arrives over.
        let later = state.frame_target(3).expect("the fourth cue has a target");
        assert_that!(later.seek).is_equal_to(Duration::from_millis(2400));
        assert_that!(
            later
                .on_screen
                .iter()
                .map(|cue| cue.text.clone())
                .collect::<Vec<_>>()
        )
        .is_equal_to(vec![
            "under".to_string(),
            "effect one".to_string(),
            "effect two".to_string(),
            "later".to_string(),
        ]);

        // Act / Assert: the unrelated cue's own frame carries only itself.
        let apart = state.frame_target(4).expect("the last cue has a target");
        assert_that!(apart.on_screen.len()).is_equal_to(1);

        // Cleanup
        drop(state);
    }

    /// A page whose track has not arrived yet is drawn and navigable, so the dispatch
    /// runs against it — and must find nothing to ask for rather than a cue at index 0.
    #[test]
    fn a_page_with_no_cues_should_have_no_frame_to_ask_for() {
        // Arrange
        let state = state();

        // Act / Assert
        assert_that!(state.nearby_frame_targets().is_empty()).is_true();
        assert_that!(state.frame_target(0).is_none()).is_true();

        // Cleanup
        drop(state);
    }

    /// A frame that could not be drawn belongs to one cue. Dropping the whole window with
    /// it would blank the neighbours the cursor is about to reach, for a failure that
    /// says nothing about them.
    #[test]
    fn a_failed_frame_should_drop_only_the_cue_it_failed_for() {
        // Arrange
        let mut state = ready(4);
        state.apply_frame(0, protocol(10, 5));
        state.apply_frame(1, protocol(10, 5));

        // Act
        state.fail_frame(1, "libass is missing".to_string());

        // Assert: cue 0's frame is untouched, and cue 1's is gone…
        assert_that!(state.frame().is_some()).is_true();
        assert_that!(state.has_frame(1)).is_false();
        // …and the reason belongs to cue 1, so it is not offered under cue 0. Showing it
        // there would blame a line that drew perfectly well.
        assert_that!(state.frame_error()).is_none();
        assert_that!(state.select(1)).is_true();
        assert_that!(state.frame_error()).is_equal_to(Some("libass is missing"));

        // Cleanup
        drop(state);
    }

    #[test]
    fn a_frame_that_could_not_be_drawn_should_replace_the_one_on_screen_with_its_reason() {
        // Arrange
        let mut state = ready(3);
        state.apply_frame(0, protocol(10, 5));

        // Act
        state.fail_frame(0, "libass is missing".to_string());

        // Assert
        assert_that!(state.frame().is_some()).is_false();
        assert_that!(state.frame_error()).is_equal_to(Some("libass is missing"));

        // Act / Assert: and a frame that does arrive clears the reason again.
        state.apply_frame(0, protocol(10, 5));
        assert_that!(state.frame_error()).is_none();
    }

    /// A nudge moves both of a cue's ends, so the cue arrives earlier or later without
    /// getting longer or shorter.
    #[test]
    fn a_nudge_should_move_a_cue_through_time_without_changing_its_length() {
        // Arrange: `ready` puts cue 1 at 2.0s → 3.0s.
        let mut state = ready(3);
        state.select(1);

        // Act: two steps later, then one step back.
        let moved = state.nudge_selected(2);
        state.nudge_selected(-1);

        // Assert: reported against the cue it moved, and landed one step on.
        assert_that!(moved).is_equal_to(Some((
            1,
            Duration::from_millis(2100),
            Duration::from_millis(3100),
        )));
        assert_that!(state.cues[1].start).is_equal_to(Duration::from_millis(2050));
        assert_that!(state.cues[1].end).is_equal_to(Duration::from_millis(3050));

        // Assert: and the cues either side of it are untouched.
        assert_that!(state.cues[0].start).is_equal_to(Duration::ZERO);
        assert_that!(state.cues[2].start).is_equal_to(Duration::from_secs(4));
    }

    /// A cue against the start of the media stops moving with its duration intact.
    ///
    /// Clamping only the start would shave a little off the cue on every press — a nudge
    /// silently editing something the reader did not ask it to.
    #[test]
    fn a_nudge_at_the_start_of_the_media_should_clamp_without_shortening_the_cue() {
        // Arrange: cue 0 runs 0.0s → 1.0s, so it is already against the floor.
        let mut state = ready(2);
        state.cues[0].start = Duration::from_millis(30);
        state.cues[0].end = Duration::from_millis(1030);

        // Act: a step back, which is more than the cue has room for.
        let clamped = state.nudge_selected(-1);

        // Assert: it moved as far as it could and kept its full second.
        assert_that!(clamped).is_equal_to(Some((0, Duration::ZERO, Duration::from_secs(1))));
        assert_that!(state.cues[0].end - state.cues[0].start).is_equal_to(Duration::from_secs(1));

        // Act / Assert: pressed again it reports nothing, so the caller stages nothing and
        // a held key does not re-render a frame per repeat.
        assert_that!(state.nudge_selected(-1)).is_none();
        assert_that!(state.cues[0].start).is_equal_to(Duration::ZERO);
    }

    /// Nothing to move, nothing reported.
    #[test]
    fn a_nudge_on_a_track_with_no_cues_should_do_nothing() {
        let mut state = state();
        assert_that!(state.nudge_selected(1)).is_none();
    }

    /// The nudge's other axis: an edge press moves one end and leaves the other exactly
    /// where it was, so what changes is how long the line is on screen.
    #[test]
    fn an_edge_press_should_move_one_end_and_leave_the_other_alone() {
        // Arrange: `ready` puts cue 1 at 2.0s → 3.0s.
        let mut state = ready(3);
        state.select(1);

        // Act / Assert: the end out, then back, then the start out and back — each press
        // reports the pair it produced, and only ever moves the end it names.
        assert_that!(state.move_selected_edge(CueEdge::End, true)).is_equal_to(Some((
            1,
            Duration::from_secs(2),
            Duration::from_millis(3050),
        )));
        assert_that!(state.move_selected_edge(CueEdge::End, false)).is_equal_to(Some((
            1,
            Duration::from_secs(2),
            Duration::from_secs(3),
        )));
        assert_that!(state.move_selected_edge(CueEdge::Start, false)).is_equal_to(Some((
            1,
            Duration::from_millis(1950),
            Duration::from_secs(3),
        )));
        assert_that!(state.move_selected_edge(CueEdge::Start, true)).is_equal_to(Some((
            1,
            Duration::from_secs(2),
            Duration::from_secs(3),
        )));

        // Assert: and the cues either side are untouched throughout.
        assert_that!(state.cues[0].start).is_equal_to(Duration::ZERO);
        assert_that!(state.cues[2].start).is_equal_to(Duration::from_secs(4));
    }

    /// A cue's start stops at the start of the media, and stopping there is not the same as
    /// refusing: the press that reaches zero still moves, it just moves less far.
    #[test]
    fn an_edge_press_should_stop_a_cues_start_at_zero() {
        // Arrange: cue 0 runs 0.030s → 1.030s, so a step back is more room than it has.
        let mut state = ready(2);
        state.cues[0].start = Duration::from_millis(30);
        state.cues[0].end = Duration::from_millis(1030);

        // Act / Assert: it lands on zero and grows by the thirty milliseconds it had —
        // which is right here, where growing is what was asked for, and would be wrong for a
        // nudge, which must never edit a cue's length by accident.
        assert_that!(state.move_selected_edge(CueEdge::Start, false)).is_equal_to(Some((
            0,
            Duration::ZERO,
            Duration::from_millis(1030),
        )));

        // Act / Assert: pressed again it reports nothing at all.
        assert_that!(state.move_selected_edge(CueEdge::Start, false)).is_none();
        assert_that!(state.cues[0].start).is_equal_to(Duration::ZERO);
    }

    /// Neither end may be pushed through the other. The press is refused outright rather
    /// than landing on the floor, so a held key stages nothing and re-renders nothing.
    #[test]
    fn neither_edge_should_be_pushed_past_the_other() {
        // Arrange: a cue exactly at the floor.
        let mut state = ready(2);
        state.cues[0].start = Duration::from_secs(1);
        state.cues[0].end = Duration::from_secs(1) + MIN_CUE_LENGTH;

        // Act / Assert: from either end, nothing moves.
        assert_that!(state.move_selected_edge(CueEdge::End, false)).is_none();
        assert_that!(state.move_selected_edge(CueEdge::Start, true)).is_none();
        assert_that!(state.cues[0].start).is_equal_to(Duration::from_secs(1));
        assert_that!(state.cues[0].end).is_equal_to(Duration::from_secs(1) + MIN_CUE_LENGTH);

        // Arrange / Act / Assert: a malformed file can hold a cue already shorter than the
        // floor — or one running backwards — and a press must not answer that by moving the
        // edge the wrong way.
        state.cues[0].end = Duration::from_millis(1010);
        assert_that!(state.move_selected_edge(CueEdge::Start, true)).is_none();
        assert_that!(state.move_selected_edge(CueEdge::End, false)).is_none();
        state.cues[0].end = Duration::from_millis(900);
        assert_that!(state.move_selected_edge(CueEdge::Start, true)).is_none();
        assert_that!(state.move_selected_edge(CueEdge::End, false)).is_none();
        assert_that!(state.cues[0].start).is_equal_to(Duration::from_secs(1));
    }

    /// The end is deliberately not capped against the media's own length, exactly as a
    /// nudge's is not: one rule about where a cue may go is worth more than two.
    #[test]
    fn an_edge_press_should_not_cap_a_cue_against_the_media() {
        // Arrange: a cue that already ends where the media does.
        let mut state = ready(1);
        state.cues[0].end = state.duration;

        // Act / Assert
        let past = state.duration + Duration::from_millis(50);
        assert_that!(state.move_selected_edge(CueEdge::End, true)).is_equal_to(Some((
            0,
            Duration::ZERO,
            past,
        )));
    }

    /// Nothing to resize, nothing reported — from either key and from the dialog.
    #[test]
    fn resizing_a_track_with_no_cues_should_do_nothing() {
        let mut state = state();
        assert_that!(state.move_selected_edge(CueEdge::End, true)).is_none();
        assert_that!(state.set_selected_length(Duration::from_secs(2))).is_none();
        assert_that!(state.selected_length()).is_none();
    }

    /// A typed length keeps the cue's start and moves its end, which is the half the reader
    /// has already judged against the picture.
    #[test]
    fn a_typed_length_should_keep_the_start_and_move_the_end() {
        // Arrange: `ready` puts cue 1 at 2.0s → 3.0s.
        let mut state = ready(3);
        state.select(1);
        assert_that!(state.selected_length()).is_equal_to(Some(Duration::from_secs(1)));

        // Act / Assert
        assert_that!(state.set_selected_length(Duration::from_millis(2500))).is_equal_to(Some((
            1,
            Duration::from_secs(2),
            Duration::from_millis(4500),
        )));
        assert_that!(state.selected_length()).is_equal_to(Some(Duration::from_millis(2500)));

        // Act / Assert: a length under the floor is refused, and so is the length the cue
        // already has — the caller stages nothing for either.
        assert_that!(state.set_selected_length(Duration::from_millis(10))).is_none();
        assert_that!(state.set_selected_length(Duration::from_millis(2500))).is_none();
        assert_that!(state.cues[1].end).is_equal_to(Duration::from_millis(4500));
    }

    /// A resize stales the same frames a nudge does and by the same rule: a frame is burned
    /// with everything on screen at its moment, so a cue grown into a neighbour's span
    /// changes what the neighbour's picture should show. The overlap groups are deliberately
    /// left alone, for the reason `set_cue_timing` gives.
    #[test]
    fn a_resize_should_drop_the_frames_it_stales_and_leave_the_groups_alone() {
        // Arrange: cues at 0.0s, 2.0s and 4.0s, each a second long, each with a frame.
        let mut state = ready(3);
        for cue in 0..3 {
            state.apply_frame(cue, protocol(10, 5));
        }
        let groups = state.groups.len();

        // Act: grow cue 0 until it covers cue 1's moment.
        for _ in 0..30 {
            state.move_selected_edge(CueEdge::End, true);
        }

        // Assert: the resized cue's own frame and its new neighbour's are both gone, and the
        // cue nothing reached still has its picture.
        assert_that!(state.encoded.iter().any(|frame| frame.cue_index == 0)).is_false();
        assert_that!(state.encoded.iter().any(|frame| frame.cue_index == 1)).is_false();
        assert_that!(state.encoded.iter().any(|frame| frame.cue_index == 2)).is_true();

        // Assert: the lanes were repacked, so the collision the resize created is drawn —
        // while the groups, which the panel's cursor is measured in, held still.
        assert_that!(state.layout.lanes[1]).is_not_equal_to(state.layout.lanes[0]);
        assert_that!(state.groups.len()).is_equal_to(groups);
    }

    /// A global shift moves every cue by the same amount, including the ones the cursor is
    /// nowhere near — which is the whole difference between it and a nudge.
    #[test]
    fn a_global_shift_should_move_every_cue_by_the_same_amount() {
        // Arrange: `ready` puts cues at 0.0s, 2.0s and 4.0s, each a second long.
        let mut state = ready(3);
        let before: Vec<(Duration, Duration)> =
            state.cues.iter().map(|cue| (cue.start, cue.end)).collect();

        // Act: three steps on, then one back.
        let moved = state.shift_all(3);
        state.shift_all(-1);

        // Assert: reported in milliseconds, signed, and accumulated on the track.
        assert_that!(moved).is_equal_to(Some(150));
        assert_that!(state.track_shift).is_equal_to(100);

        // Assert: every cue moved by exactly that, and none of them changed length.
        for (cue, (was_start, was_end)) in state.cues.iter().zip(before) {
            assert_that!(cue.start).is_equal_to(was_start + Duration::from_millis(100));
            assert_that!(cue.end).is_equal_to(was_end + Duration::from_millis(100));
        }
    }

    /// A backward shift is shortened to what the *earliest* cue allows, so the track keeps
    /// its shape.
    ///
    /// Clamping each cue against zero on its own would hold the early ones still while the
    /// rest kept moving — the track silently stretched by a key that says it moves the track.
    #[test]
    fn a_global_shift_at_the_start_of_the_media_should_clamp_the_whole_track_together() {
        // Arrange: cue 0 is 30ms from the floor, the others are seconds away from it.
        let mut state = ready(3);
        state.cues[0].start = Duration::from_millis(30);
        state.cues[0].end = Duration::from_millis(1030);

        // Act: a step back, which is more than the earliest cue has room for.
        let moved = state.shift_all(-1);

        // Assert: every cue moved by the 30ms the earliest one allowed, not by 50ms, and not
        // by different amounts from each other.
        assert_that!(moved).is_equal_to(Some(-30));
        assert_that!(state.cues[0].start).is_equal_to(Duration::ZERO);
        assert_that!(state.cues[1].start).is_equal_to(Duration::from_millis(1970));
        assert_that!(state.cues[2].start).is_equal_to(Duration::from_millis(3970));
        assert_that!(state.cues[0].end - state.cues[0].start).is_equal_to(Duration::from_secs(1));

        // Act / Assert: pressed again it reports nothing, so a held key at the floor stages
        // nothing and re-renders nothing.
        assert_that!(state.shift_all(-1)).is_none();
        assert_that!(state.cues[1].start).is_equal_to(Duration::from_millis(1970));
        assert_that!(state.track_shift).is_equal_to(-30);
    }

    /// Nothing to move, nothing reported.
    #[test]
    fn a_global_shift_on_a_track_with_no_cues_should_do_nothing() {
        let mut state = state();
        assert_that!(state.shift_all(1)).is_none();
        assert_that!(state.track_shift).is_equal_to(0);
    }

    /// Every cached still is a picture of a moment its cue no longer starts at, so the whole
    /// window goes — where a single nudge keeps the frames it cannot have affected.
    ///
    /// The overlap groups are left alone: a uniform shift moves every cue equally, so no cue
    /// can come to share the screen with one it did not share it with before.
    #[test]
    fn a_global_shift_should_drop_every_frame_and_leave_the_groups_alone() {
        // Arrange: three cues, each with a frame ready, and a note of how they group.
        let mut state = ready(3);
        for cue in 0..3 {
            state.apply_frame(cue, protocol(10, 5));
        }
        let groups = state.groups.clone();

        // Act
        state.shift_all(4);

        // Assert: no frame survived, including the two the cursor is nowhere near.
        assert_that!(state.encoded.is_empty()).is_true();
        // Assert: and the rows the panel draws did not reflow under the reader.
        assert_that!(state.groups).is_equal_to(groups);
    }

    /// `r` at track scale puts back only the cues the caller names, and leaves the shift
    /// readout at zero.
    #[test]
    fn restoring_timings_should_move_the_named_cues_and_clear_the_shift() {
        // Arrange: a track shifted on, with a frame drawn against the new timings.
        let mut state = ready(3);
        state.shift_all(2);
        state.apply_frame(0, protocol(10, 5));

        // Act: put two of the three back where the file has them.
        state.restore_timings(&[
            (0, Duration::ZERO, Duration::from_secs(1)),
            (2, Duration::from_secs(4), Duration::from_secs(5)),
        ]);

        // Assert: the named cues moved and the unnamed one stayed where the shift left it.
        assert_that!(state.cues[0].start).is_equal_to(Duration::ZERO);
        assert_that!(state.cues[2].start).is_equal_to(Duration::from_secs(4));
        assert_that!(state.cues[1].start).is_equal_to(Duration::from_millis(2100));

        // Assert: the track reads as unmoved again, and the stale picture is gone.
        assert_that!(state.track_shift).is_equal_to(0);
        assert_that!(state.encoded.is_empty()).is_true();
    }

    /// A position past the end of the list is ignored rather than panicking: the caller reads
    /// the staged edits, which address a file the page's list can have drifted from.
    #[test]
    fn restoring_a_timing_for_a_cue_that_is_not_there_should_do_nothing() {
        // Arrange
        let mut state = ready(2);

        // Act
        state.restore_timings(&[(9, Duration::ZERO, Duration::from_secs(1))]);

        // Assert
        assert_that!(state.cues.len()).is_equal_to(2);
        assert_that!(state.cues[0].start).is_equal_to(Duration::ZERO);
    }

    /// A new cue list is the file's own timings, so the shift it is measured by starts again.
    #[test]
    fn a_new_track_should_reset_the_shift_readout() {
        // Arrange: a page whose track has been shifted.
        let mut state = ready(2);
        state.shift_all(3);
        assert_that!(state.track_shift).is_equal_to(150);

        // Act: the file is re-read, as it is after a save rewrites it.
        state.apply_prepared(vec![cue(0, 1000, "line 0")], CueStyle::SubRip);

        // Assert
        assert_that!(state.track_shift).is_equal_to(0);
    }

    /// The two scales are one value, so turning one on cannot leave the other on.
    #[test]
    fn the_timing_scope_should_report_whether_the_mode_is_on_at_either_scale() {
        assert_that!(TimingScope::Off.is_on()).is_false();
        assert_that!(TimingScope::Cue.is_on()).is_true();
        assert_that!(TimingScope::Track.is_on()).is_true();
        assert_that!(TimingScope::default()).is_equal_to(TimingScope::Off);
    }

    /// Retiming a cue invalidates the pictures of the lines it *shares the screen with*, not
    /// only its own.
    ///
    /// A frame is burned with every cue on screen at that moment, so moving one line into or
    /// out of a neighbour's moment changes what the neighbour's picture should show. Keeping
    /// a stale one would draw the old company under the new timing — which is exactly the
    /// judgement this page exists to support, made against a lie.
    #[test]
    fn retiming_a_cue_should_drop_the_frames_of_everything_it_shared_the_screen_with() {
        // Arrange: cue 1 overlaps cue 0 (which runs 0.0s → 1.0s) and nothing else; cues 2
        // and 3 are at 4.0s and 6.0s, clear of it and of where it is about to go.
        let mut state = ready(4);
        state.cues[1].start = Duration::from_millis(500);
        state.cues[1].end = Duration::from_millis(1500);
        state.layout = pack_lanes(&state.cues, MAX_LANES);
        state.selected = 1;
        for index in 0..4 {
            state.apply_frame(index, protocol(10, 5));
        }
        assert_that!((0..4).all(|index| state.has_frame(index))).is_true();

        // Act: move cue 1 clear of cue 0, and clear of everything else.
        state.set_cue_timing(1, Duration::from_secs(9), Duration::from_secs(10));

        // Assert: the moved cue and the neighbour it left both lost their frames.
        assert_that!(state.has_frame(1)).is_false();
        assert_that!(state.has_frame(0)).is_false();

        // Assert: and the cues it was never on screen with kept theirs.
        assert_that!(state.has_frame(2)).is_true();
        assert_that!(state.has_frame(3)).is_true();
    }

    /// The lanes are repacked so the timeline draws a moved cue where it now is, and the
    /// overlap groups are deliberately not, so the panel does not reflow under a held key.
    #[test]
    fn retiming_a_cue_should_repack_the_lanes_but_leave_the_groups_alone() {
        // Arrange: four cues that overlap nothing, so every one is its own group and lane.
        let mut state = ready(4);
        let groups_before = state.groups.clone();
        assert_that!(state.layout.lane_count).is_equal_to(1);

        // Act: drop cue 2 on top of cue 1, which needs a second lane to draw.
        state.set_cue_timing(2, Duration::from_millis(2500), Duration::from_millis(3500));

        // Assert: the timeline can draw the collision.
        assert_that!(state.layout.lane_count).is_equal_to(2);

        // Assert: but the panel's rows, its cursor and its scrolling still describe the
        // list the last keypress was resolved against.
        assert_that!(state.groups.len()).is_equal_to(groups_before.len());
        assert_that!(state.groups).is_equal_to(groups_before);
    }

    /// A frame that has to be rendered waits out the debounce, or a held-down `j` starts
    /// an ffmpeg per key repeat. (One already in the cache does not — that decision is
    /// `App::start_pending_preview`'s, and is asserted there.)
    #[test]
    fn moving_the_selection_should_ask_for_a_frame_only_once_the_movement_settles() {
        // Arrange
        let mut state = ready(5);
        take_due(&mut state);

        // Act
        state.select(1);

        // Assert: pending, but not yet due.
        assert_that!(take_due(&mut state)).is_false();
        std::thread::sleep(FRAME_DEBOUNCE + Duration::from_millis(20));
        assert_that!(take_due(&mut state)).is_true();
        // And consumed: one settled selection is one request.
        assert_that!(take_due(&mut state)).is_false();
    }

    #[test]
    fn every_way_the_frame_goes_stale_should_ask_for_a_new_one() {
        // Arrange
        let mut state = state();
        let due = |state: &mut SubtitleEditState| {
            std::thread::sleep(FRAME_DEBOUNCE + Duration::from_millis(20));
            take_due(state)
        };

        // Act / Assert: the cues arriving.
        state.apply_prepared(
            vec![cue(0, 1000, "a"), cue(2000, 3000, "b")],
            CueStyle::SubRip,
        );
        assert_that!(due(&mut state)).is_true();

        // Act / Assert: the pane being measured, and then resized.
        state.set_preview_cells(Size::new(40, 20));
        assert_that!(due(&mut state)).is_true();
        state.set_preview_cells(Size::new(40, 20));
        assert_that!(due(&mut state)).is_false();
        state.set_preview_cells(Size::new(30, 20));
        assert_that!(due(&mut state)).is_true();

        // Act / Assert: jumping to either end of the list.
        assert_that!(state.select_last()).is_true();
        assert_that!(due(&mut state)).is_true();
        assert_that!(state.select_first()).is_true();
        assert_that!(due(&mut state)).is_true();

        // Act / Assert: and a refusal to move asks for nothing.
        assert_that!(state.select_first()).is_false();
        assert_that!(due(&mut state)).is_false();
    }

    /// A track that failed to load has no cue to draw, so a request left pending from
    /// opening the page would grab a frame for a cue list that does not exist.
    #[test]
    fn a_failed_track_should_drop_its_frame_and_stop_asking_for_one() {
        // Arrange
        let mut state = ready(3);
        state.apply_frame(0, protocol(10, 5));

        // Act
        state.fail("ffmpeg exploded".to_string());

        // Assert
        assert_that!(state.frame().is_some()).is_false();
        std::thread::sleep(FRAME_DEBOUNCE + Duration::from_millis(20));
        assert_that!(take_due(&mut state)).is_false();
    }

    /// The status line counts cues the pass is finished with, and stops counting when the
    /// pass is over — however it ended, since a cue it could not draw is finished with too.
    #[test]
    fn warming_should_count_up_and_then_stop_counting() {
        // Arrange
        let mut state = ready(4);
        assert_that!(state.warm).is_equal_to(WarmState::Off);

        // Act / Assert
        state.apply_warming(0, 4);
        assert_that!(state.warm).is_equal_to(WarmState::Working { done: 0, total: 4 });
        state.apply_warming(3, 4);
        assert_that!(state.warm).is_equal_to(WarmState::Working { done: 3, total: 4 });
        state.apply_warming(4, 4);
        assert_that!(state.warm).is_equal_to(WarmState::Done);
        // A pass that gave up early reports everything left as finished at once, which
        // must read as done rather than as a count past the end.
        state.apply_warming(9, 4);
        assert_that!(state.warm).is_equal_to(WarmState::Done);
        // An empty track has nothing to count and is over before it starts.
        state.apply_warming(0, 0);
        assert_that!(state.warm).is_equal_to(WarmState::Done);
    }

    /// The background pass reports ten times a second for as long as a track takes, and a
    /// window with nothing missing has nothing to refill — so the steady state of a
    /// settled cursor has to be silence rather than ten dispatches a second that exist
    /// only to be found empty and dropped.
    #[test]
    fn warming_should_only_ask_for_a_refill_when_the_window_is_short_of_one() {
        // Arrange: a three-cue track with the cursor at the start, so cues 1 and 2 are the
        // window and cue 0 is the selection.
        let mut state = ready(3);
        state.clear_frame_request();

        // Act / Assert: a missing neighbour is worth going back to the cache for.
        state.apply_warming(1, 3);
        assert_that!(state.any_frame_requested()).is_true();

        // Arrange: fill the window, and the selection with it.
        state.clear_frame_request();
        for cue_index in 0..3 {
            state.apply_frame(cue_index, protocol(10, 5));
        }
        state.clear_frame_request();

        // Act
        state.apply_warming(2, 3);

        // Assert: nothing to fetch, so nothing is asked for.
        assert_that!(state.any_frame_requested()).is_false();

        // Act / Assert: and a window that loses a frame starts asking again.
        state.fail_frame(2, "ffmpeg exploded".to_string());
        state.apply_warming(3, 3);
        assert_that!(state.any_frame_requested()).is_true();
    }

    /// The window stops at the ends of the track, so a cursor with fewer than
    /// [`NEARBY_FRAMES`] cues beside it must not count the cues that do not exist as
    /// missing — that would put the page right back to asking on every progress report.
    #[test]
    fn a_window_running_off_the_end_of_the_track_should_not_count_as_short() {
        // Arrange: one cue, so there are no neighbours at any distance.
        let mut state = ready(1);
        state.apply_frame(0, protocol(10, 5));
        state.clear_frame_request();

        // Act
        state.apply_warming(1, 1);

        // Assert
        assert_that!(state.any_frame_requested()).is_false();
    }

    /// What kitty costs on a 10x20 font: the frame's pixels as base64 RGBA, so the pane
    /// size is what turns this into megabytes.
    const KITTY_BYTES_PER_CELL: u64 = 200 * 4 * 4 / 3;

    /// A `Protocol` holds the bytes the terminal is going to be sent rather than the
    /// picture, so a full window on a pane filling a large display runs to tens of
    /// megabytes held to save a round trip on cues the cursor may never reach.
    #[test]
    fn the_ready_window_should_shrink_on_a_pane_it_cannot_afford() {
        // Arrange: a pane of 12,000 cells, which under kitty is about 12 MB a frame —
        // three of them fit the budget where five do not.
        let mut state = costed_state(KITTY_BYTES_PER_CELL);
        state.apply_prepared(
            (0..5)
                .map(|index| {
                    let start = index as u64 * 2000;
                    cue(start, start + 1000, &format!("line {index}"))
                })
                .collect(),
            CueStyle::SubRip,
        );
        state.set_preview_cells(Size::new(200, 60));
        state.select(2);

        // Act: offer the full window anyway, as the worker would.
        for cue_index in 0..5 {
            state.apply_frame(cue_index, protocol(10, 5));
        }

        // Assert: the selected cue and one either side, not two.
        assert_that!(state.has_frame(2)).is_true();
        assert_that!(state.has_frame(1)).is_true();
        assert_that!(state.has_frame(3)).is_true();
        assert_that!(state.has_frame(0)).is_false();
        assert_that!(state.has_frame(4)).is_false();

        // Assert: and the page stops asking for what it would only throw away — cue 0 and
        // cue 4 are outside the window, so an empty list is the steady state rather than a
        // render on every progress report.
        assert_that!(state.nearby_frame_targets().is_empty()).is_true();

        // Act / Assert: the same page on a pane it can afford keeps the full window.
        state.set_preview_cells(Size::new(80, 24));
        for cue_index in 0..5 {
            state.apply_frame(cue_index, protocol(10, 5));
        }
        assert_that!(state.has_frame(0)).is_true();
        assert_that!(state.has_frame(4)).is_true();
    }

    /// A window of nothing puts the worker round trip back on every cursor move, which is
    /// the thing the window exists to remove — so however large the pane, one neighbour
    /// either side is kept and the budget is the one that gives.
    #[test]
    fn the_ready_window_should_keep_a_neighbour_however_large_the_pane() {
        // Arrange: a pane where a single frame alone is over budget.
        let mut state = costed_state(KITTY_BYTES_PER_CELL);
        state.apply_prepared(
            (0..3)
                .map(|index| {
                    let start = index as u64 * 2000;
                    cue(start, start + 1000, &format!("line {index}"))
                })
                .collect(),
            CueStyle::SubRip,
        );
        state.set_preview_cells(Size::new(400, 120));
        state.select(1);

        // Act
        for cue_index in 0..3 {
            state.apply_frame(cue_index, protocol(10, 5));
        }

        // Assert
        assert_that!(state.has_frame(0)).is_true();
        assert_that!(state.has_frame(1)).is_true();
        assert_that!(state.has_frame(2)).is_true();
    }

    /// A cheap protocol on a huge pane still cannot reach the budget, and a build with no
    /// picker encodes nothing at all — neither should be made to pay for the budget with a
    /// shortened window.
    #[test]
    fn a_window_that_costs_nothing_should_never_be_shortened() {
        // Arrange / Act / Assert: a cheap protocol, on a pane far past what kitty could
        // afford.
        let mut state = ready(5);
        state.set_preview_cells(Size::new(400, 120));
        state.select(2);
        for cue_index in 0..5 {
            state.apply_frame(cue_index, protocol(10, 5));
        }
        assert_that!(state.has_frame(0)).is_true();
        assert_that!(state.has_frame(4)).is_true();

        // Arrange / Act / Assert: and a page with no image protocol behind it at all.
        let mut state = costed_state(0);
        state.apply_prepared(
            (0..5)
                .map(|index| {
                    let start = index as u64 * 2000;
                    cue(start, start + 1000, &format!("line {index}"))
                })
                .collect(),
            CueStyle::SubRip,
        );
        state.set_preview_cells(Size::new(400, 120));
        state.select(2);
        for cue_index in 0..5 {
            state.apply_frame(cue_index, protocol(10, 5));
        }
        assert_that!(state.has_frame(0)).is_true();
        assert_that!(state.has_frame(4)).is_true();
    }

    /// A track that could not be read has no cues to render, so a count left on screen
    /// from before it failed would be counting towards nothing.
    #[test]
    fn a_failed_track_should_stop_reporting_a_background_pass() {
        // Arrange
        let mut state = ready(3);
        state.apply_warming(1, 3);

        // Act
        state.fail("ffmpeg exploded".to_string());

        // Assert
        assert_that!(state.warm).is_equal_to(WarmState::Off);
    }

    /// The media is named in every cache key and in every grab, so the page has to hand
    /// out the one its frames were rendered against rather than a second copy.
    #[test]
    fn the_page_should_name_the_media_its_frames_come_from() {
        // Act / Assert
        assert_that!(state().media()).is_equal_to(Path::new("/media/show.mkv"));
    }

    /// The picker a test's spans are decoded and drawn for.
    ///
    /// Kitty, because that is what production looks like now: `preview::drawing_picker`
    /// refuses a halfblocks terminal, so a span is always encoded through a real image
    /// protocol. Built from `Picker::halfblocks` and switched, since that is the one
    /// constructor that does not query the terminal — a test runner has none to query.
    fn test_picker() -> ratatui_image::picker::Picker {
        let mut picker = ratatui_image::picker::Picker::halfblocks();
        picker.set_protocol_type(ratatui_image::picker::ProtocolType::Kitty);
        picker
    }

    /// How many pixels a cell is worth for [`test_picker`].
    fn test_font() -> ratatui_image::FontSize {
        test_picker().font_size()
    }

    /// The shape a test's span is decoded and drawn at: the cell area it was asked for,
    /// the pixels [`test_picker`] wants per cell, and a source whose proportions hand those
    /// same cells back — so a resize test moves the pane and nothing else.
    fn test_shape(cells: Size) -> crate::preview::SpanShape {
        crate::preview::SpanShape {
            pixels: crate::preview::playback_pixels(cells, test_font()),
            cells,
            picker: test_picker(),
        }
    }

    /// How many bytes one frame of a `cells`-sized playback occupies.
    fn frame_bytes(cells: Size) -> usize {
        let (width, height) = crate::preview::playback_pixels(cells, test_font());
        (width as usize) * (height as usize) * 3
    }

    /// A span of `count` frames, each a solid colour naming its index, so a test can tell
    /// which one is on screen from the picture rather than from a counter beside it.
    fn span(count: usize, cells: Size, fps: u32) -> PlaybackFrames {
        span_at(count, cells, fps, PlaybackSpeed::NORMAL)
    }

    /// The same, at a chosen speed — which changes nothing about the frames themselves and
    /// everything about what a playhead over them means in media time.
    fn span_at(count: usize, cells: Size, fps: u32, speed: PlaybackSpeed) -> PlaybackFrames {
        let pixels = crate::preview::playback_pixels(cells, test_font());
        let stride = (pixels.0 as usize) * (pixels.1 as usize) * 3;
        let mut bytes = Vec::with_capacity(stride * count);
        for index in 0..count {
            bytes.extend(std::iter::repeat_n(index as u8, stride));
        }
        PlaybackFrames::new(
            bytes,
            test_shape(cells),
            fps,
            speed,
            Duration::from_secs(10),
            Vec::new(),
        )
    }

    /// An output whose position is set by the test rather than by a device, so the
    /// picture's dependence on the clock can be asserted a frame at a time.
    #[derive(Debug)]
    struct SteppedOutput(std::sync::Mutex<Option<Duration>>);

    impl SteppedOutput {
        fn at(position: Option<Duration>) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self(std::sync::Mutex::new(position)))
        }

        fn set(&self, position: Duration) {
            *self.0.lock().unwrap() = Some(position);
        }
    }

    impl AudioOutput for SteppedOutput {
        fn position(&self, _now: Instant) -> Option<Duration> {
            *self.0.lock().unwrap()
        }
    }

    /// Starts a playback with the pane measured, which in the application the renderer does
    /// on the first draw. The height is generous so the fit is decided by the width, which
    /// makes the drawn cell area exactly the one the span was built with.
    fn play(state: &mut SubtitleEditState, cue_index: usize, frames: PlaybackFrames) {
        play_through(state, cue_index, frames, Box::new(SilentSource));
    }

    fn play_through(
        state: &mut SubtitleEditState,
        cue_index: usize,
        frames: PlaybackFrames,
        source: Box<dyn AudioSource>,
    ) {
        play_looping(state, cue_index, frames, source, false);
    }

    fn play_looping(
        state: &mut SubtitleEditState,
        cue_index: usize,
        frames: PlaybackFrames,
        source: Box<dyn AudioSource>,
        looping: bool,
    ) {
        state.set_preview_cells(Size::new(frames.cells.width, frames.cells.height * 4));
        state.begin_playback(PlaybackAnchor::Cue(cue_index), frames, source, looping);
    }

    /// The silent path, asked for by name — the same `SilentOutput` a machine with no sound
    /// card gets, opened fresh each time the way a real source is.
    #[derive(Debug)]
    struct SilentSource;

    impl AudioSource for SilentSource {
        fn open(&self) -> Box<dyn AudioOutput> {
            Box::new(SilentOutput::new())
        }
    }

    fn playing(state: &mut SubtitleEditState) -> &Playback {
        match &state.playback {
            PlaybackState::Playing(playback) => playback,
            other => panic!("expected a playing page, got {other:?}"),
        }
    }

    /// The picture is derived from the sound, not scheduled against a clock of its own —
    /// so a step that does not cross a frame boundary must not re-encode anything, and one
    /// that does must land on the frame that had already started rather than the next.
    #[test]
    fn the_playhead_should_follow_the_sound_a_frame_at_a_time() {
        // Arrange: ten frames at ten a second, so a frame is exactly 100 ms.
        let mut state = ready(3);
        let clock = SteppedOutput::at(None);
        play_through(
            &mut state,
            0,
            span(10, Size::new(4, 2), 10),
            Box::new(SharedSource::of(&clock)),
        );

        // Act / Assert: nothing is drawn before the device has said where it is. Holding
        // the first frame up instead would start the picture ahead of the sound, which is
        // the one error this page must not make.
        assert_that!(state.advance_playback()).is_false();
        assert_that!(state.playback_frame().is_none()).is_true();
        assert_that!(state.playback_position()).is_none();

        // Act / Assert: the sound starts, and the first frame goes up.
        clock.set(Duration::ZERO);
        assert_that!(state.advance_playback()).is_true();
        assert_that!(state.playback_frame().is_some()).is_true();
        assert_that!(playing(&mut state).shown).is_equal_to(Some(0));

        // Act / Assert: a step inside that frame's own period redraws nothing.
        clock.set(Duration::from_millis(99));
        assert_that!(state.advance_playback()).is_false();
        assert_that!(playing(&mut state).shown).is_equal_to(Some(0));

        // Act / Assert: and crossing the boundary moves exactly one frame on.
        clock.set(Duration::from_millis(100));
        assert_that!(state.advance_playback()).is_true();
        assert_that!(playing(&mut state).shown).is_equal_to(Some(1));

        // Act / Assert: the sound jumping — which is what a device catching up looks like —
        // takes the picture straight to where the sound is, rather than walking to it.
        clock.set(Duration::from_millis(750));
        assert_that!(state.advance_playback()).is_true();
        assert_that!(playing(&mut state).shown).is_equal_to(Some(7));
    }

    /// An `AudioOutput` the test keeps a handle on. `Playback` owns its output outright —
    /// that is what makes dropping the page release the device — so driving one from
    /// outside needs a shared cell rather than a borrow.
    #[derive(Debug)]
    struct SharedOutput(std::sync::Arc<SteppedOutput>);

    impl AudioOutput for SharedOutput {
        fn position(&self, now: Instant) -> Option<Duration> {
            self.0.position(now)
        }
    }

    /// A source handing out that same shared clock every time it is asked, and counting how
    /// often it was asked — which is how a loop's repeat is told apart from a playback that
    /// simply never ended.
    #[derive(Debug)]
    struct SharedSource {
        clock: std::sync::Arc<SteppedOutput>,
        opens: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl SharedSource {
        fn of(clock: &std::sync::Arc<SteppedOutput>) -> Self {
            Self {
                clock: std::sync::Arc::clone(clock),
                opens: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }

        fn counter(&self) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
            std::sync::Arc::clone(&self.opens)
        }
    }

    impl AudioSource for SharedSource {
        fn open(&self) -> Box<dyn AudioOutput> {
            self.opens
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Box::new(SharedOutput(std::sync::Arc::clone(&self.clock)))
        }
    }

    /// A playback that reached its last frame is finished with, not paused on it: the next
    /// `p` should replay the span rather than having to stop it first. The device goes back
    /// at the same moment, which is what stops a page left open holding one open forever.
    #[test]
    fn a_playback_should_end_itself_when_the_sound_runs_past_the_span() {
        // Arrange: three frames at ten a second, so the span lasts 300 ms.
        let mut state = ready(3);
        let clock = SteppedOutput::at(Some(Duration::from_millis(250)));
        play_through(
            &mut state,
            0,
            span(3, Size::new(4, 2), 10),
            Box::new(SharedSource::of(&clock)),
        );
        assert_that!(state.advance_playback()).is_true();
        assert_that!(state.playback_active()).is_true();

        // Act: past the last frame.
        clock.set(Duration::from_millis(300));
        let dirty = state.advance_playback();

        // Assert: over, and back to the still frame.
        assert_that!(dirty).is_true();
        assert_that!(state.playback_active()).is_false();
        assert_that!(state.playback_frame().is_none()).is_true();
        // And advancing again reports nothing, rather than ending a second time.
        assert_that!(state.advance_playback()).is_false();
    }

    /// A looping playback going round again is a *new device*, not a rewound one — a stream
    /// plays its buffer once. Asserting on the frame alone would pass for a playback that
    /// silently kept the finished device and drew a picture with no sound under it, so what
    /// is checked is that the source was asked for another output.
    #[test]
    fn a_looping_playback_should_start_again_instead_of_ending() {
        // Arrange: three frames at ten a second, so the span lasts 300 ms, with the picture
        // settled on the last of them.
        let mut state = ready(3);
        let clock = SteppedOutput::at(Some(Duration::from_millis(250)));
        let source = SharedSource::of(&clock);
        let opens = source.counter();
        play_looping(
            &mut state,
            0,
            span(3, Size::new(4, 2), 10),
            Box::new(source),
            true,
        );
        assert_that!(state.advance_playback()).is_true();
        assert_that!(playing(&mut state).shown).is_equal_to(Some(2));
        assert_that!(opens.load(std::sync::atomic::Ordering::Relaxed)).is_equal_to(1);

        // Act: past the last frame, which for a playback that did not loop is the end.
        clock.set(Duration::from_millis(300));
        let dirty = state.advance_playback();

        // Assert: still playing, back on the first frame, and holding a second device. The
        // frame is drawn in the same pass that noticed the end, so the picture never blanks
        // between passes.
        assert_that!(dirty).is_true();
        assert_that!(state.playback_active()).is_true();
        assert_that!(playing(&mut state).shown).is_equal_to(Some(0));
        assert_that!(state.playback_frame().is_some()).is_true();
        assert_that!(opens.load(std::sync::atomic::Ordering::Relaxed)).is_equal_to(2);

        // Act / Assert: the repeat plays on its new device's clock rather than re-opening on
        // every step — a second round that opened a device per frame would still draw the
        // right pictures.
        clock.set(Duration::from_millis(100));
        assert_that!(state.advance_playback()).is_true();
        assert_that!(playing(&mut state).shown).is_equal_to(Some(1));
        assert_that!(opens.load(std::sync::atomic::Ordering::Relaxed)).is_equal_to(2);

        // Act / Assert: and it goes round a third time rather than only once.
        clock.set(Duration::from_millis(300));
        assert_that!(state.advance_playback()).is_true();
        assert_that!(state.playback_active()).is_true();
        assert_that!(opens.load(std::sync::atomic::Ordering::Relaxed)).is_equal_to(3);
    }

    /// A span whose frames cannot be read must still end, looping or not — otherwise the
    /// page opens a device per iteration of the event loop, forever.
    #[test]
    fn a_looping_playback_with_no_frames_should_still_end() {
        // Arrange: a span of no frames at all, asked to loop.
        let mut state = ready(3);
        let cells = Size::new(4, 2);
        play_looping(
            &mut state,
            0,
            span(0, cells, 10),
            Box::new(SilentSource),
            true,
        );

        // Act
        state.advance_playback();

        // Assert
        assert_that!(state.playback_active()).is_false();
    }

    /// The playhead answers in media time while the frames are spaced in output time, so a
    /// speed other than normal is a factor between them. Without it the mark on the timeline
    /// would crawl at the speed the picture is playing at and read as the cue being
    /// mistimed — which is the one thing this page must never say wrongly.
    #[test]
    fn the_playhead_should_answer_in_media_time_at_any_speed() {
        // Arrange: a span starting ten seconds in, at ten frames a second, half speed.
        let mut state = ready(3);
        let clock = SteppedOutput::at(Some(Duration::from_millis(800)));
        play_through(
            &mut state,
            0,
            span_at(30, Size::new(4, 2), 10, PlaybackSpeed::HALF),
            Box::new(SharedSource::of(&clock)),
        );

        // Act: eight tenths of a second of *sound* have gone by, which is frame eight.
        assert_that!(state.advance_playback()).is_true();

        // Assert: frame eight is eight tenths into the output and four into the media.
        assert_that!(playing(&mut state).shown).is_equal_to(Some(8));
        assert_that!(state.playback_position())
            .is_equal_to(Some(Duration::from_secs(10) + Duration::from_millis(400)));
    }

    /// The playhead is read off the frame on screen rather than off the clock, so the
    /// picture and the mark on the timeline in one drawn frame describe the same instant.
    /// Straight off the clock it would sit up to a frame period ahead of the picture.
    #[test]
    fn the_playhead_should_report_where_the_picture_is_rather_than_where_the_sound_is() {
        // Arrange: a span starting ten seconds into the media, at ten frames a second.
        let mut state = ready(3);
        let clock = SteppedOutput::at(Some(Duration::from_millis(2_390)));
        play_through(
            &mut state,
            0,
            span(30, Size::new(4, 2), 10),
            Box::new(SharedSource::of(&clock)),
        );

        // Act: the picture is settled on frame 23, and *then* the sound moves on — which is
        // what happens on every iteration, since the loop advances the playback and draws
        // it in one pass while the device keeps going underneath.
        state.advance_playback();
        clock.set(Duration::from_millis(2_990));

        // Assert: the playhead still reports frame 23's moment, 10 s + 2.3 s. Read off the
        // clock instead it would say 12.99 s and sit most of a second ahead of the picture
        // drawn beside it.
        assert_that!(state.playback_position()).is_equal_to(Some(Duration::from_millis(12_300)));
    }

    /// A playback is *about* the cue it was started on. Leaving it running while the cursor
    /// moves would play one line's span under another line's timing, which reads as the
    /// timing being wrong on the one page built to judge exactly that.
    #[test]
    fn every_way_the_page_moves_under_a_playback_should_stop_it() {
        let started = |state: &mut SubtitleEditState| play(state, 0, span(30, Size::new(4, 2), 10));

        // Act / Assert: the cursor moving.
        let mut state = ready(5);
        started(&mut state);
        assert_that!(state.select(1)).is_true();
        assert_that!(state.playback_active()).is_false();

        // Act / Assert: jumping to either end.
        started(&mut state);
        assert_that!(state.select_last()).is_true();
        assert_that!(state.playback_active()).is_false();
        started(&mut state);
        assert_that!(state.select_first()).is_true();
        assert_that!(state.playback_active()).is_false();

        // Act / Assert: the cue itself being retimed under the span. A span is decoded ahead
        // of time with the cue already burned into its frames, so one still playing after a
        // nudge is playing the timing the reader has just moved away from — the same lie as
        // playing it under a different line.
        started(&mut state);
        assert_that!(state.nudge_selected(1)).is_some();
        assert_that!(state.playback_active()).is_false();

        // Act / Assert: a new track arriving.
        started(&mut state);
        state.apply_prepared(vec![cue(0, 1000, "a")], CueStyle::SubRip);
        assert_that!(state.playback_active()).is_false();

        // Act / Assert: and the track failing to load.
        started(&mut state);
        state.fail("ffmpeg exploded".to_string());
        assert_that!(state.playback_active()).is_false();
    }

    /// **A resize must not stop a playback**, unlike every other way the page moves — and
    /// this is a regression test, not a preference.
    ///
    /// Announcing the playback is itself what resizes the pane: "Preparing playback…" puts
    /// a status row on the page, and that row comes out of the preview pane's height. A
    /// playback that stopped on a resize was therefore stopped by the act of saying it was
    /// starting, every single time, on any page that had no status row already — which is
    /// every page where the background frame pass has finished. The e2e scenario timed out
    /// waiting for a span that had been cancelled before it arrived.
    ///
    /// So the frames absorb the resize instead: they hold pixels, and the encode targets
    /// whatever the pane has become.
    #[test]
    fn a_playback_should_survive_the_pane_resizing_under_it() {
        // Arrange: playing, with a frame on screen. The pane is exactly as tall as the
        // picture needs, so the row it is about to lose is one the picture was using.
        let mut state = ready(3);
        state.set_preview_cells(Size::new(8, 4));
        state.begin_playback(
            PlaybackAnchor::Cue(0),
            span(30, Size::new(8, 4), 10),
            Box::new(SilentSource),
            false,
        );
        assert_that!(state.advance_playback()).is_true();
        assert_that!(state.playback_frame().map(|frame| frame.size()))
            .is_equal_to(Some(Size::new(8, 4)));

        // Act: the pane loses a row, exactly as it does when the status line appears.
        state.set_preview_cells(Size::new(8, 3));

        // Assert: still playing.
        assert_that!(state.playback_active()).is_true();

        // Assert: and the next step re-encodes for the pane it now has, rather than leaving
        // a protocol too large for it — `Image` draws nothing at all rather than clipping.
        assert_that!(state.advance_playback()).is_true();
        assert_that!(state.playback_frame().map(|frame| frame.size()))
            .is_equal_to(Some(Size::new(6, 3)));
    }

    /// Growing the pane re-fits the playback to it without distorting the picture.
    ///
    /// The pane changes under a running playback as a matter of course — announcing one is
    /// itself what adds the status row that shortens it — so the re-fit is not an edge
    /// case. What it must preserve is the picture's shape: the decoded frames are re-encoded
    /// into whatever cell area the pane has become, and the proportions of the drawn result
    /// have to match the proportions of the pixels that went in, whatever the pane's own
    /// shape is.
    #[test]
    fn a_resized_playback_should_fill_the_new_pane_without_distorting_the_picture() {
        // Arrange: a span whose frames are 4:3, in a pane of exactly that shape.
        let cells = Size::new(8, 3);
        let mut state = ready(3);
        state.set_preview_cells(cells);
        state.begin_playback(
            PlaybackAnchor::Cue(0),
            PlaybackFrames::new(
                vec![0; frame_bytes(cells) * 3],
                test_shape(cells),
                10,
                PlaybackSpeed::NORMAL,
                Duration::from_secs(10),
                Vec::new(),
            ),
            Box::new(SilentSource),
            false,
        );
        let font = test_font();
        let (span_width, span_height) = crate::preview::playback_pixels(cells, font);

        // Act: a pane far larger, and a different shape — squarer than the picture.
        state.set_preview_cells(Size::new(40, 40));
        assert_that!(state.advance_playback()).is_true();

        // Assert: it grew into the new pane rather than staying at its old size.
        let drawn = state
            .playback_frame()
            .map(|frame| frame.size())
            .expect("a frame under the playhead");
        assert_that!(drawn.width > cells.width && drawn.height > cells.height).is_true();

        // Assert: and it kept the picture's proportions rather than the pane's. Compared in
        // pixels, since that is where a cell's own two-to-one shape cancels out.
        let width = u32::from(drawn.width) * u32::from(font.width);
        let height = u32::from(drawn.height) * u32::from(font.height);
        assert_that!(width * span_height).is_equal_to(height * span_width);
    }

    /// A playback is drawn as pixels, through the protocol the terminal actually offered,
    /// at that terminal's own resolution.
    ///
    /// This is the difference between a burned-in subtitle being readable and being a
    /// coloured smear, and it has two halves that must agree: the span is *decoded* at the
    /// cell's own pixel size, and it is *encoded* through the picker that size came from.
    /// The picker travels with the span so those cannot drift apart — and an encode that
    /// fell back to cells-and-colours would still be the right size, the right frame and
    /// the right cue, so every assertion short of this one passes while the picture is
    /// unreadable.
    #[test]
    fn a_playback_should_draw_through_the_protocol_its_terminal_offered() {
        // Arrange: a span sized and carried for the terminal's own picker.
        let cells = Size::new(4, 2);
        let pixels = crate::preview::playback_pixels(cells, test_font());
        let stride = (pixels.0 as usize) * (pixels.1 as usize) * 3;
        let mut state = ready(3);
        state.set_preview_cells(cells);
        state.begin_playback(
            PlaybackAnchor::Cue(0),
            PlaybackFrames::new(
                vec![120; stride * 3],
                test_shape(cells),
                10,
                PlaybackSpeed::NORMAL,
                Duration::from_secs(1),
                Vec::new(),
            ),
            Box::new(SilentSource),
            false,
        );

        // Act
        assert_that!(state.advance_playback()).is_true();

        // Assert: the image protocol, not cells and colours.
        let protocol = state.playback_frame().expect("a frame under the playhead");
        assert_that!(matches!(protocol, Protocol::Kitty(_))).is_true();

        // Assert: and the frames were decoded at the cell's own pixels, so there is nothing
        // to resample on the way to that encoder.
        let font = test_font();
        assert_that!(pixels).is_equal_to((
            u32::from(cells.width) * u32::from(font.width),
            u32::from(cells.height) * u32::from(font.height),
        ));
    }

    /// A rate of zero would make the playhead's arithmetic divide by nothing — and in
    /// floating point that is not a panic but an infinity, which `Duration::from_secs_f64`
    /// *does* panic on, in the event loop. Unreachable through the config, which clamps the
    /// rate well above zero, and cheap to rule out rather than reason about.
    #[test]
    fn a_span_with_no_frame_rate_should_park_the_playhead_at_its_start() {
        // Arrange
        let mut state = ready(3);
        let cells = Size::new(4, 2);
        state.set_preview_cells(Size::new(4, 8));
        state.begin_playback(
            PlaybackAnchor::Cue(0),
            PlaybackFrames::new(
                vec![0; frame_bytes(cells) * 2],
                test_shape(cells),
                0,
                PlaybackSpeed::NORMAL,
                Duration::from_secs(7),
                Vec::new(),
            ),
            Box::new(SilentSource),
            false,
        );

        // Act / Assert
        assert_that!(state.advance_playback()).is_true();
        assert_that!(state.playback_position()).is_equal_to(Some(Duration::from_secs(7)));
    }

    /// The renderer has not measured the pane on the first draw after a span arrives, and
    /// encoding for no cells trips a `debug_assert!` inside `ratatui-image` — which would
    /// take the process down rather than merely draw nothing.
    #[test]
    fn a_playback_should_wait_for_the_pane_to_be_measured() {
        // Arrange: a page nothing has drawn yet.
        let mut state = ready(3);
        state.begin_playback(
            PlaybackAnchor::Cue(0),
            span(4, Size::new(4, 2), 10),
            Box::new(SilentSource),
            false,
        );

        // Act / Assert
        assert_that!(state.advance_playback()).is_false();
        assert_that!(state.playback_frame().is_none()).is_true();
        assert_that!(state.playback_active()).is_true();

        // Act / Assert: and it starts drawing the moment the pane is measured.
        state.set_preview_cells(Size::new(4, 8));
        assert_that!(state.advance_playback()).is_true();
        assert_that!(state.playback_frame().is_some()).is_true();
    }

    /// A span takes a second or two to decode, and `p` has to be the way out of that as
    /// well as the way in — otherwise pressing it by mistake means waiting for a playback
    /// you have already decided you do not want.
    #[test]
    fn a_playback_should_be_stoppable_before_it_has_started() {
        // Arrange
        let mut state = ready(3);
        state.prepare_playback(PlaybackAnchor::Cue(0));

        // Assert: waiting, and saying so.
        assert_that!(state.playback_active()).is_true();
        assert_that!(state.preparing_playback()).is_equal_to(Some(PlaybackAnchor::Cue(0)));
        assert_that!(state.playback_frame().is_none()).is_true();

        // Act
        assert_that!(state.stop_playback()).is_true();

        // Assert
        assert_that!(state.playback_active()).is_false();
        assert_that!(state.preparing_playback()).is_none();
        // And stopping what is already stopped reports that there was nothing to stop,
        // which is what makes the keybinding a toggle rather than a switch.
        assert_that!(state.stop_playback()).is_false();
    }

    /// A span that could not be decoded has to say why, or `p` looks like it did nothing —
    /// and it says it about the cue it was pressed on, since the reason says nothing about
    /// the next line.
    #[test]
    fn a_playback_that_failed_should_explain_itself_under_the_cue_it_was_asked_for() {
        // Arrange
        let mut state = ready(3);
        state.prepare_playback(PlaybackAnchor::Cue(0));

        // Act
        state.fail_playback(
            PlaybackAnchor::Cue(0),
            "Could not play this cue: no such file".to_string(),
        );

        // Assert
        assert_that!(state.playback_active()).is_false();
        assert_that!(state.playback_error())
            .is_equal_to(Some("Could not play this cue: no such file"));

        // Act / Assert: and it does not follow the cursor onto a line it is not about.
        assert_that!(state.select(1)).is_true();
        assert_that!(state.playback_error()).is_none();

        // Act / Assert: asking again clears it, so a retry that works leaves nothing behind.
        state.prepare_playback(PlaybackAnchor::Cue(1));
        assert_that!(state.playback_error()).is_none();
    }

    /// The cursor's playback fails against a moment rather than a line, so the reason keeps
    /// the moment's company: it stays only while the reader is still pointing at it, and it
    /// belongs to the timeline rather than to whatever cue happens to be selected underneath.
    #[test]
    fn a_playback_that_failed_should_explain_itself_under_the_moment_it_was_asked_for() {
        // Arrange
        let mut state = ready(3);
        state.focus_timeline();
        let at = state.cursor().expect("the timeline holds the cursor");

        // Act
        state.fail_playback(
            PlaybackAnchor::Cursor(at),
            "Could not play this moment: no video".to_string(),
        );

        // Assert
        assert_that!(state.playback_error())
            .is_equal_to(Some("Could not play this moment: no video"));

        // Act / Assert: it does not follow the cursor onto a moment it is not about.
        assert_that!(state.move_cursor(1, TIMELINE_STEP)).is_true();
        assert_that!(state.playback_error()).is_none();
    }

    /// Each anchor is answered for by the pane that owns it. A reason about a cue says
    /// nothing about the moment being scrubbed and vice versa, so the focus is half of the
    /// test rather than an afterthought — without it, leaving the timeline would leave a
    /// message about a moment on screen under a cue.
    #[test]
    fn a_reason_should_belong_to_the_pane_that_holds_the_cursor() {
        // Arrange
        let mut state = ready(3);
        state.fail_playback(PlaybackAnchor::Cue(0), "about the cue".to_string());

        // Act / Assert: it is about the selected cue while the cue panel holds the cursor.
        assert_that!(state.playback_error()).is_equal_to(Some("about the cue"));

        // Act / Assert: and stands down the moment the timeline takes it, where no cue is
        // being pointed at at all.
        state.focus_timeline();
        assert_that!(state.playback_error()).is_none();

        // Act / Assert: the other way round, a reason about a moment is not shown under a
        // cue once the cursor has gone home.
        let at = state.cursor().expect("the timeline holds the cursor");
        state.fail_playback(PlaybackAnchor::Cursor(at), "about the moment".to_string());
        assert_that!(state.playback_error()).is_equal_to(Some("about the moment"));
        state.focus_cues();
        assert_that!(state.playback_error()).is_none();
    }

    /// A span arriving for a cue the cursor has already left is dropped rather than played,
    /// which is what `preparing_playback` is consulted for — so it has to answer `None` the
    /// moment the page stops waiting.
    #[test]
    fn a_page_not_waiting_for_a_span_should_say_so() {
        // Arrange
        let mut state = ready(3);

        // Act / Assert
        assert_that!(state.preparing_playback()).is_none();
        state.prepare_playback(PlaybackAnchor::Cue(2));
        assert_that!(state.preparing_playback()).is_equal_to(Some(PlaybackAnchor::Cue(2)));
        play(&mut state, 2, span(4, Size::new(4, 2), 10));
        assert_that!(state.preparing_playback()).is_none();
        assert_that!(playing(&mut state).anchor()).is_equal_to(PlaybackAnchor::Cue(2));
    }

    /// The page's state is printed whole in every test failure and in the harness's timeout
    /// dump. `Protocol` has no `Debug` and the span is megabytes of pixels, so a derived one
    /// would either not compile or drown the message.
    #[test]
    fn a_playback_should_describe_itself_by_where_it_is_rather_than_by_its_pixels() {
        // Arrange
        let mut state = ready(3);
        play(&mut state, 1, span(6, Size::new(4, 2), 10));

        // Act
        let described = format!("{:?}", state.playback);

        // Assert
        assert_that!(described.as_str()).contains("anchor: Cue(1)");
        assert_that!(described.as_str()).contains("frames: 6");
        assert_that!(described.contains("bytes")).is_false();
    }

    /// The frame is encoded straight to halfblocks at the cell area the span was decoded
    /// for, so what the terminal draws is what `ffmpeg` produced with nothing resampled in
    /// between — and it has to fill exactly the cells the pane was measured at, since
    /// `Image` draws nothing at all rather than clipping when it does not fit.
    #[test]
    fn a_played_frame_should_encode_to_exactly_the_cells_it_was_decoded_for() {
        // Arrange
        let mut state = ready(3);
        let cells = Size::new(8, 4);
        play(&mut state, 0, span(3, cells, 10));

        // Act
        assert_that!(state.advance_playback()).is_true();

        // Assert
        let protocol = state.playback_frame().expect("a frame should be drawn");
        assert_that!(protocol.size()).is_equal_to(cells);
    }

    /// Entering the timeline must change what the keys mean and nothing else. The cursor
    /// therefore lands on the exact instant the preview pane is already displaying — the
    /// selected cue's seek moment — so no picture moves under the reader as they arrive.
    #[test]
    fn the_cursor_should_enter_the_timeline_on_the_moment_the_preview_already_shows() {
        // Arrange
        let mut state = ready(3);
        state.select(1);
        let seek = seek_for(state.selected_cue().unwrap(), state.duration);

        // Act
        let entered = state.focus_timeline();

        // Assert
        assert_that!(entered).is_true();
        assert_that!(state.cursor()).is_equal_to(Some(seek));
        assert_that!(state.focus).is_equal_to(EditFocus::Timeline);
        // Nothing asked for: the frame on screen is already this moment's.
        assert_that!(state.scrub_requested()).is_false();
        assert_that!(state.scrub_target().map(|target| target.at)).is_equal_to(Some(seek));
    }

    /// A second `Ctrl+J` is not a fresh entry, and re-seeding on it would throw away
    /// wherever the reader had scrubbed to.
    #[test]
    fn entering_the_timeline_twice_should_leave_the_cursor_where_it_was() {
        // Arrange
        let mut state = ready(3);
        state.focus_timeline();
        state.move_cursor(4, TIMELINE_STEP);
        let at = state.cursor();

        // Act
        let entered = state.focus_timeline();

        // Assert
        assert_that!(entered).is_false();
        assert_that!(state.cursor()).is_equal_to(at);
    }

    /// There is no timeline drawn while the page is reading, empty or failed, so a cursor in
    /// it would be one the reader cannot see and cannot get back out of by looking.
    #[test]
    fn the_cursor_should_be_refused_on_a_page_with_no_timeline_to_stand_in() {
        // Arrange / Act / Assert: still reading.
        let mut preparing = state();
        assert_that!(preparing.focus_timeline()).is_false();

        // Arrange / Act / Assert: parsed, but holding nothing.
        let mut empty = state();
        empty.apply_prepared(Vec::new(), CueStyle::SubRip);
        assert_that!(empty.focus_timeline()).is_false();

        // Arrange / Act / Assert: failed outright.
        let mut failed = ready(3);
        failed.fail("ffmpeg said no".to_string());
        assert_that!(failed.focus_timeline()).is_false();
        assert_that!(failed.cursor()).is_none();
    }

    /// `Ctrl+K` reports whether it did anything, so `Esc` can peel the cursor off the
    /// timeline without also swallowing the press that was meant to leave the page.
    #[test]
    fn taking_the_cursor_back_should_report_whether_it_moved() {
        // Arrange
        let mut state = ready(3);

        // Act / Assert: nothing to take back.
        assert_that!(state.focus_cues()).is_false();

        // Act / Assert: and something to take back.
        state.focus_timeline();
        state.move_cursor(2, TIMELINE_STEP);
        assert_that!(state.focus_cues()).is_true();
        assert_that!(state.cursor()).is_none();
        assert_that!(state.scrub_requested()).is_false();
        assert_that!(state.scrub_target()).is_none();
    }

    /// One press is [`TIMELINE_STEP`] and `H`/`L` are ten of them, which is what makes the
    /// pair a fine control and a coarse one rather than two speeds of the same thing.
    #[test]
    fn a_leap_should_move_ten_steps_of_the_cursor() {
        // Arrange
        let mut state = ready(3);
        state.focus_timeline();

        // Act
        state.move_cursor(1, TIMELINE_STEP);
        let one = state.cursor().unwrap();
        state.move_cursor(TIMELINE_LEAP, TIMELINE_STEP);

        // Assert
        assert_that!(one).is_equal_to(TIMELINE_STEP);
        assert_that!(state.cursor())
            .is_equal_to(Some(TIMELINE_STEP + TIMELINE_STEP * TIMELINE_LEAP as u32));
    }

    /// The three scales are one movement with the size handed in, so the fine step obeys
    /// every rule the coarse one does — including the clamp at zero, which is where the
    /// finest step is most likely to be pressed against the end of the file.
    #[test]
    fn the_fine_step_should_move_the_cursor_by_the_cue_nudge_and_clamp_like_the_rest() {
        // Arrange
        let mut state = ready(3);
        state.focus_timeline();
        state.move_cursor(-i32::from(u16::MAX), TIMELINE_STEP);
        state.clear_scrub_request();

        // Act
        assert_that!(state.move_cursor(1, TIMELINE_FINE_STEP)).is_true();
        let one = state.cursor().unwrap();
        assert_that!(state.move_cursor(-1, TIMELINE_FINE_STEP)).is_true();

        // Assert: one nudge on and back to the floor, which then refuses to go further.
        assert_that!(one).is_equal_to(TIMING_STEP);
        assert_that!(state.cursor()).is_equal_to(Some(Duration::ZERO));
        state.clear_scrub_request();
        assert_that!(state.move_cursor(-1, TIMELINE_FINE_STEP)).is_false();
        assert_that!(state.scrub_requested()).is_false();
    }

    /// A press that would move nothing has to report so, or a held `h` at 0:00 spends an
    /// `ffmpeg` seek per key repeat re-drawing the frame already on screen.
    #[test]
    fn the_cursor_should_stop_at_both_ends_of_the_media() {
        // Arrange
        let mut state = ready(3);
        state.focus_timeline();

        // Act / Assert: the floor.
        assert_that!(state.move_cursor(-1, TIMELINE_STEP)).is_false();
        assert_that!(state.cursor()).is_equal_to(Some(Duration::ZERO));
        state.clear_scrub_request();

        // Act / Assert: and the ceiling, which is held back from the very last instant so
        // the grab lands on a frame that exists.
        let ceiling = seek_ceiling(state.duration).unwrap();
        assert_that!(state.move_cursor(i32::from(u16::MAX), TIMELINE_STEP)).is_true();
        assert_that!(state.cursor()).is_equal_to(Some(ceiling));
        state.clear_scrub_request();
        assert_that!(state.move_cursor(1, TIMELINE_STEP)).is_false();
    }

    /// A container whose duration would not parse arrives here as zero. Clamping against
    /// that would pin the cursor to 0:00; the last cue's end is the furthest point the page
    /// has any evidence of media at.
    #[test]
    fn a_media_of_unknown_length_should_stop_the_cursor_at_the_last_cue() {
        // Arrange
        let mut state = costed_state(CHEAP_BYTES_PER_CELL);
        state.duration = Duration::ZERO;
        state.apply_prepared(
            vec![cue(0, 1000, "one"), cue(2000, 3000, "two")],
            CueStyle::SubRip,
        );
        state.focus_timeline();

        // Act
        state.move_cursor(i32::from(u16::MAX), TIMELINE_STEP);

        // Assert
        assert_that!(state.cursor()).is_equal_to(Some(Duration::from_millis(3000)));
    }

    /// The window's scroll belongs to the visit, not to the page: it starts unset so the
    /// selected cue's own window is what the reader arrives on, and it is dropped on the way
    /// out so a later visit is an arrival rather than a return.
    #[test]
    fn the_scroll_position_should_last_a_visit_and_no_longer() {
        // Arrange
        let mut state = ready(3);

        // Act / Assert: nothing remembered before the first draw of a visit, so the window
        // the selected cue chose is what the reader arrives on.
        state.focus_timeline();
        assert_that!(state.window_start()).is_none();

        // Act / Assert: the drawing code hands back where the window settled, and moving the
        // cursor leaves that alone — only a draw changes it.
        state.set_window_start(Duration::from_secs(30));
        state.move_cursor(1, TIMELINE_STEP);
        assert_that!(state.window_start()).is_equal_to(Some(Duration::from_secs(30)));

        // Act / Assert: and it is forgotten on the way out, so the next visit is a fresh
        // arrival rather than a return to a position the cue list has moved away from.
        state.focus_cues();
        assert_that!(state.window_start()).is_none();
        state.focus_timeline();
        assert_that!(state.window_start()).is_none();
    }

    /// A page that read its cues and then had them taken away draws no timeline at all.
    #[test]
    fn the_cursor_should_be_refused_a_timeline_with_no_cues_left_in_it() {
        // Arrange
        let mut state = ready(3);
        state.cues = Vec::new();

        // Act / Assert
        assert_that!(state.focus_timeline()).is_false();
        assert_that!(state.cursor()).is_none();
        assert_that!(state.scrub_requested()).is_false();
    }

    /// The keys belong to the pane holding the cursor, so a move asked for while the cue
    /// panel has it must do nothing at all rather than move an invisible cursor.
    #[test]
    fn the_cursor_should_not_move_while_the_cue_panel_holds_it() {
        // Arrange
        let mut state = ready(3);

        // Act / Assert
        assert_that!(state.move_cursor(1, TIMELINE_STEP)).is_false();
        assert_that!(state.cursor()).is_none();
        assert_that!(state.scrub_requested()).is_false();
        assert_that!(state.scrub_target()).is_none();
    }

    /// Both ways of stopping pointing at a cue stop the span that was playing for it.
    /// Arriving in the timeline drops the selection altogether — no cue is marked in either
    /// pane while the cursor is there — so a span decoded around one would go on playing
    /// under nothing the reader can see, and moving the cursor afterwards is looking
    /// somewhere else again.
    #[test]
    fn leaving_a_cue_for_the_timeline_should_stop_the_span_playing_for_it() {
        // Arrange
        let mut state = ready(3);
        let cells = Size::new(4, 2);
        play(&mut state, 0, span(4, cells, 10));

        // Act / Assert: arriving in the pane stops it, because the cue it was about is no
        // longer being pointed at.
        state.focus_timeline();
        assert_that!(state.playback_active()).is_false();

        // Act / Assert: and so does going back the other way, with the cursor's own span.
        play(&mut state, 0, span(4, cells, 10));
        state.focus_cues();
        assert_that!(state.playback_active()).is_false();
    }

    /// A span playing for a moment is about that moment, so the first press that moves off
    /// it stops the span rather than leaving a picture up that the cursor has left behind.
    #[test]
    fn moving_the_timeline_cursor_should_stop_a_playback() {
        // Arrange
        let mut state = ready(3);
        let cells = Size::new(4, 2);
        state.focus_timeline();
        play(&mut state, 0, span(4, cells, 10));
        assert_that!(state.playback_active()).is_true();

        // Act
        state.move_cursor(1, TIMELINE_STEP);

        // Assert
        assert_that!(state.playback_active()).is_false();
    }

    /// The selected cue's still stands in for the cursor's only while the cursor is standing
    /// on that cue's own moment.
    ///
    /// A regression test. Pressing `p` from the timeline flashed the selected cue's picture
    /// into the pane — on a page entered and left alone, the first cue of the track: saying
    /// "Preparing playback…" puts a status row on the page, that row comes out of the preview
    /// pane's height, and the resize drops the cursor's frame. The pane then fell through to
    /// `frame()`, which knows only about the selection, and the cheap cache read that refills
    /// it beat the accurate seek the cursor's replacement costs.
    #[test]
    fn a_cues_still_should_not_stand_in_for_a_moment_the_cursor_has_left() {
        // Arrange: the selected cue's picture, which is what the pane falls back on.
        let mut state = ready(3);
        state.set_preview_cells(Size::new(40, 20));
        state.apply_frame(0, protocol(8, 4));
        assert_that!(state.still_frame().map(Protocol::size)).is_equal_to(Some(Size::new(8, 4)));

        // Act / Assert: arriving in the timeline changes nothing. The cursor is seeded on the
        // cue's own moment, so that still *is* the right picture for it — which is why
        // `Ctrl+J` asks for no grab at all.
        state.focus_timeline();
        assert_that!(state.still_frame().map(Protocol::size)).is_equal_to(Some(Size::new(8, 4)));

        // Act: one press away from that moment, answered with a frame of its own.
        state.move_cursor(1, TIMELINE_STEP);
        state.apply_scrub_frame(state.cursor().unwrap(), protocol(6, 3));
        assert_that!(state.scrub_frame().map(Protocol::size)).is_equal_to(Some(Size::new(6, 3)));

        // Act: the resize `p`'s status row causes, and the cue's frame coming back first —
        // it is a cached JPEG re-encoded, where the cursor's is an accurate seek.
        state.set_preview_cells(Size::new(40, 19));
        state.apply_frame(0, protocol(8, 4));

        // Assert: the cursor's frame went with the resize, and the cue's does not take its
        // place. The pane draws nothing until this moment has a picture again.
        assert_that!(state.scrub_frame().is_none()).is_true();
        assert_that!(state.frame().is_some()).is_true();
        assert_that!(state.still_frame().is_none()).is_true();

        // Act / Assert: back on the cue's own moment, it is the right picture once more —
        // whether the cursor walks back to it or the cue panel takes the cursor.
        state.move_cursor(-1, TIMELINE_STEP);
        assert_that!(state.still_frame().map(Protocol::size)).is_equal_to(Some(Size::new(8, 4)));
        state.move_cursor(2, TIMELINE_STEP);
        assert_that!(state.still_frame().is_none()).is_true();
        state.focus_cues();
        assert_that!(state.still_frame().map(Protocol::size)).is_equal_to(Some(Size::new(8, 4)));
    }

    /// Nothing caches these frames, so every settled position is an `ffmpeg` seek. Blanking
    /// the pane between them would flicker on every press of a held key, and the timeline's
    /// title says the true moment throughout.
    #[test]
    fn the_cursors_frame_should_stay_up_until_a_newer_one_replaces_it() {
        // Arrange
        let mut state = ready(3);
        state.set_preview_cells(Size::new(40, 20));
        state.focus_timeline();
        state.apply_scrub_frame(Duration::ZERO, protocol(8, 4));

        // Act: the cursor moves on, and the next frame has not arrived.
        state.move_cursor(2, TIMELINE_STEP);

        // Assert
        assert_that!(state.scrub_frame().map(Protocol::size)).is_equal_to(Some(Size::new(8, 4)));

        // Act: the new one lands.
        state.apply_scrub_frame(state.cursor().unwrap(), protocol(6, 3));

        // Assert
        assert_that!(state.scrub_frame().map(Protocol::size)).is_equal_to(Some(Size::new(6, 3)));
    }

    /// A frame for a moment the cursor has already left is dropped rather than drawn: the
    /// pane would otherwise show one moment while the title named another.
    #[test]
    fn a_frame_for_a_moment_the_cursor_has_left_should_be_dropped() {
        // Arrange
        let mut state = ready(3);
        state.focus_timeline();
        state.move_cursor(2, TIMELINE_STEP);

        // Act
        state.apply_scrub_frame(Duration::from_secs(30), protocol(8, 4));

        // Assert
        assert_that!(state.scrub_frame().is_none()).is_true();
    }

    /// The picture goes with the failure. Keeping the previous moment on screen while the
    /// status row explains why a different one is missing is the page contradicting itself.
    #[test]
    fn a_moment_that_could_not_be_drawn_should_clear_the_picture_and_say_why() {
        // Arrange
        let mut state = ready(3);
        state.focus_timeline();
        state.apply_scrub_frame(Duration::ZERO, protocol(8, 4));
        state.move_cursor(1, TIMELINE_STEP);
        let at = state.cursor().unwrap();

        // Act
        state.fail_scrub_frame(at, "Could not draw this frame".to_string());

        // Assert
        assert_that!(state.scrub_frame().is_none()).is_true();
        assert_that!(state.scrub_error()).is_equal_to(Some("Could not draw this frame"));

        // Act / Assert: a failure reported against a moment already left says nothing.
        state.move_cursor(1, TIMELINE_STEP);
        assert_that!(state.scrub_error()).is_none();
        state.fail_scrub_frame(Duration::from_secs(45), "stale".to_string());
        assert_that!(state.scrub_error()).is_none();

        // Act / Assert: and a frame that does arrive clears the reason with it.
        state.fail_scrub_frame(state.cursor().unwrap(), "fresh".to_string());
        assert_that!(state.scrub_error()).is_equal_to(Some("fresh"));
        state.apply_scrub_frame(state.cursor().unwrap(), protocol(8, 4));
        assert_that!(state.scrub_error()).is_none();
    }

    /// The reason and the picture both belong to the timeline pane, so taking the cursor
    /// back to the cues must take them off the page rather than leave them explaining an
    /// absence nobody is looking at.
    #[test]
    fn taking_the_cursor_back_should_take_its_picture_and_its_reason_with_it() {
        // Arrange
        let mut state = ready(3);
        state.focus_timeline();
        state.apply_scrub_frame(Duration::ZERO, protocol(8, 4));

        // Act
        state.focus_cues();

        // Assert
        assert_that!(state.scrub_frame().is_none()).is_true();
        assert_that!(state.scrub_error()).is_none();
    }

    /// The cursor's frame is encoded for the pane, so a resize makes it as stale as every
    /// other frame on hand — and `Image` draws nothing at all for a protocol wider than the
    /// area it is given, so keeping it would blank the pane rather than shrink the picture.
    #[test]
    fn resizing_the_pane_should_drop_the_cursors_frame_and_ask_for_another() {
        // Arrange
        let mut state = ready(3);
        state.set_preview_cells(Size::new(40, 20));
        state.focus_timeline();
        state.apply_scrub_frame(Duration::ZERO, protocol(8, 4));
        state.clear_scrub_request();

        // Act
        state.set_preview_cells(Size::new(30, 15));

        // Assert
        assert_that!(state.scrub_frame().is_none()).is_true();
        assert_that!(state.scrub_requested()).is_true();
    }

    /// A resize while the cue panel holds the cursor must not leave a request outstanding
    /// that nothing will ever answer: `scrub_target` says nothing while the cursor is up
    /// there, so the dispatch would examine an empty request on every loop iteration.
    #[test]
    fn resizing_with_the_cursor_in_the_cue_panel_should_ask_for_no_moment() {
        // Arrange
        let mut state = ready(3);
        state.set_preview_cells(Size::new(40, 20));

        // Act
        state.set_preview_cells(Size::new(30, 15));

        // Assert
        assert_that!(state.scrub_requested()).is_false();
    }

    /// A new cue list is a new track, and the cursor's moment was measured against cues that
    /// have just been replaced.
    #[test]
    fn a_new_cue_list_should_take_the_cursor_home() {
        // Arrange
        let mut state = ready(3);
        state.focus_timeline();
        state.move_cursor(3, TIMELINE_STEP);
        state.apply_scrub_frame(state.cursor().unwrap(), protocol(8, 4));

        // Act
        state.apply_prepared(vec![cue(0, 1000, "fresh")], CueStyle::SubRip);

        // Assert
        assert_that!(state.cursor()).is_none();
        assert_that!(state.focus).is_equal_to(EditFocus::Cues);
        assert_that!(state.scrub_frame().is_none()).is_true();
    }

    /// What gets burned into the cursor's frame is what a viewer would see at that instant —
    /// which is often nothing at all, and never a cue forced in the way a cue's own grab
    /// forces its anchor.
    #[test]
    fn the_cursors_target_should_carry_only_what_is_on_screen_at_that_moment() {
        // Arrange: cues at 0-1s, 2-3s and 4-5s.
        let mut state = ready(3);
        state.focus_timeline();

        // Act / Assert: a moment inside the second cue carries it and nothing else.
        state.move_cursor(5, TIMELINE_STEP);
        let target = state.scrub_target().expect("the timeline holds the cursor");
        assert_that!(target.at).is_equal_to(Duration::from_millis(2500));
        assert_that!(target.on_screen.len()).is_equal_to(1);
        assert_that!(target.on_screen[0].text.as_str()).is_equal_to("line 1");

        // Act / Assert: and a moment in the gap between two carries none.
        state.move_cursor(1, TIMELINE_STEP);
        let target = state.scrub_target().expect("the timeline holds the cursor");
        assert_that!(target.at).is_equal_to(Duration::from_millis(3000));
        assert_that!(target.on_screen.as_slice()).is_empty();
    }

    /// Every one of these costs an accurate seek, so a held key has to collapse to one grab
    /// — and the request has to survive being examined before the debounce expires, or the
    /// frame the reader settled on is never asked for at all.
    #[test]
    fn the_cursors_grab_should_wait_out_the_debounce_and_still_go_out() {
        // Arrange
        let mut state = ready(3);
        state.focus_timeline();

        // Act / Assert: asked for, but not yet due.
        state.move_cursor(1, TIMELINE_STEP);
        assert_that!(state.scrub_requested()).is_true();
        assert_that!(state.scrub_request_due()).is_false();
        assert_that!(state.any_frame_requested()).is_true();

        // Act / Assert: and due once the cursor has settled.
        std::thread::sleep(FRAME_DEBOUNCE + Duration::from_millis(20));
        assert_that!(state.scrub_request_due()).is_true();
        state.clear_scrub_request();
        assert_that!(state.scrub_requested()).is_false();
        assert_that!(state.scrub_request_due()).is_false();
    }

    /// A cue added at a moment lands where that moment falls in the list, and the file's own
    /// cues keep the positions their staged rewrites are addressed by.
    #[test]
    fn an_inserted_cue_should_land_in_time_order_without_moving_the_files_positions() {
        // Arrange: cues at 0-1s, 2-3s and 4-5s.
        let mut state = ready(3);

        // Act: a cue in the gap between the second and the third.
        let at = state.insert_cue(
            7,
            Duration::from_millis(3200),
            Duration::from_millis(3800),
            "new".to_string(),
        );

        // Assert: it is the third row of the list, and it is what the cursor is on.
        assert_that!(at).is_equal_to(2);
        assert_that!(state.selected).is_equal_to(2);
        let texts: Vec<&str> = state.cues.iter().map(|cue| cue.text.as_str()).collect();
        assert_that!(texts).contains_exactly_in_given_order(["line 0", "line 1", "new", "line 2"]);
        // The file's cues still answer for the positions the staged rewrites use, even the
        // one that moved down a row.
        assert_that!(state.origin(1)).is_equal_to(Some(CueOrigin::File(1)));
        assert_that!(state.origin(2)).is_equal_to(Some(CueOrigin::Inserted(7)));
        assert_that!(state.origin(3)).is_equal_to(Some(CueOrigin::File(2)));
        assert_that!(state.position_of(CueOrigin::File(2))).is_equal_to(Some(3));
        // The list's own numbering follows the list, which is what every reader of
        // `Cue::index` expects.
        let indices: Vec<usize> = state.cues.iter().map(|cue| cue.index).collect();
        assert_that!(indices).contains_exactly_in_given_order([0, 1, 2, 3]);
        // And the panel has a row to draw it on.
        assert_that!(state.groups.len()).is_equal_to(4);
    }

    /// The cursor comes home when a cue is made from the timeline: the cue panel is the only
    /// pane that marks a selection, so a new cue left behind in the timeline would be a cue
    /// nothing on screen points at.
    #[test]
    fn a_cue_made_from_the_timeline_should_bring_the_cursor_back_to_the_panel() {
        // Arrange
        let mut state = ready(3);
        state.focus_timeline();
        assert_that!(state.cursor()).is_some();

        // Act
        state.insert_cue(
            0,
            Duration::from_millis(500),
            Duration::from_millis(900),
            "new".to_string(),
        );

        // Assert
        assert_that!(state.focus).is_equal_to(EditFocus::Cues);
        assert_that!(state.cursor()).is_none();
        assert_that!(state.selected).is_equal_to(1);
    }

    /// A frame is burned with everything on screen at that moment, so a new cue makes its
    /// neighbours' pictures wrong — and only its neighbours'.
    #[test]
    fn an_inserted_cue_should_drop_the_frames_it_now_shares_the_screen_with() {
        // Arrange: frames for all three cues on hand.
        let mut state = ready(3);
        state.set_preview_cells(Size::new(40, 20));
        for index in 0..3 {
            state.apply_frame(index, protocol(40, 20));
        }

        // Act: a cue overlapping the second (2000-3000ms) and nothing else.
        state.insert_cue(
            0,
            Duration::from_millis(2500),
            Duration::from_millis(2800),
            "new".to_string(),
        );

        // Assert: the first cue's frame survives at its own position, the third's survives
        // at the position it moved down to, and the one it overlaps is gone.
        assert_that!(state.has_frame(0)).is_true();
        assert_that!(state.has_frame(1)).is_false();
        assert_that!(state.has_frame(2)).is_false();
        assert_that!(state.has_frame(3)).is_true();
    }

    /// Un-adding a cue takes its row out of the list, and every file cue keeps the position
    /// its staged rewrites are addressed by — the same claim the insertion makes, going the
    /// other way.
    #[test]
    fn removing_a_cue_should_take_its_row_out_without_moving_the_files_positions() {
        // Arrange: an inserted cue sitting third in a list of four.
        let mut state = ready(3);
        let at = state.insert_cue(
            7,
            Duration::from_millis(3200),
            Duration::from_millis(3800),
            "new".to_string(),
        );
        assert_that!(at).is_equal_to(2);

        // Act
        state.remove_cue(at);

        // Assert: the list is what it was, and the file's cues answer for their own
        // positions again.
        let texts: Vec<&str> = state.cues.iter().map(|cue| cue.text.as_str()).collect();
        assert_that!(texts).contains_exactly_in_given_order(["line 0", "line 1", "line 2"]);
        assert_that!(state.origin(2)).is_equal_to(Some(CueOrigin::File(2)));
        assert_that!(state.position_of(CueOrigin::File(2))).is_equal_to(Some(2));
        assert_that!(state.position_of(CueOrigin::Inserted(7))).is_none();
        // The list's own numbering follows the list, which is what every reader of
        // `Cue::index` expects.
        let indices: Vec<usize> = state.cues.iter().map(|cue| cue.index).collect();
        assert_that!(indices).contains_exactly_in_given_order([0, 1, 2]);
        // The panel has one row fewer to draw, and the cursor is on the row that took the
        // gone one's place.
        assert_that!(state.groups.len()).is_equal_to(3);
        assert_that!(state.selected).is_equal_to(2);
    }

    /// A frame is burned with everything on screen at that moment, so a cue leaving makes its
    /// neighbours' pictures wrong — and only its neighbours'. The frames below it shift up
    /// with their rows rather than being thrown away.
    #[test]
    fn removing_a_cue_should_drop_the_frames_it_shared_the_screen_with() {
        // Arrange: a cue overlapping the second one only, with every frame on hand.
        let mut state = ready(3);
        state.set_preview_cells(Size::new(40, 20));
        let at = state.insert_cue(
            0,
            Duration::from_millis(2500),
            Duration::from_millis(2800),
            "new".to_string(),
        );
        assert_that!(at).is_equal_to(2);
        for index in 0..4 {
            state.apply_frame(index, protocol(40, 20));
        }

        // Act
        state.remove_cue(at);

        // Assert: the first cue never shared the screen with it and keeps its frame; the
        // second did and loses it; the third's frame follows the row up from 3 to 2.
        assert_that!(state.has_frame(0)).is_true();
        assert_that!(state.has_frame(1)).is_false();
        assert_that!(state.has_frame(2)).is_true();
        assert_that!(state.has_frame(3)).is_false();
    }

    /// The selection cannot be left pointing past the end of a list that just got shorter:
    /// a cursor outside the list is one the panel cannot draw.
    #[test]
    fn removing_the_last_cue_should_bring_the_cursor_back_into_the_list() {
        // Arrange: an inserted cue at the end of the track, selected.
        let mut state = ready(3);
        let at = state.insert_cue(
            0,
            Duration::from_millis(9000),
            Duration::from_millis(9500),
            "new".to_string(),
        );
        assert_that!(at).is_equal_to(3);

        // Act
        state.remove_cue(at);

        // Assert
        assert_that!(state.cues.len()).is_equal_to(3);
        assert_that!(state.selected).is_equal_to(2);
        assert_that!(state.group_of(state.selected)).is_equal_to(2);
    }

    /// The status row's frame error is keyed on a cue's *position*, so a row leaving the list
    /// moves it: the error of the row that went has nothing left to be about, and one below it
    /// would otherwise be reported against whichever cue moved up into its slot.
    #[test]
    fn removing_a_cue_should_move_the_frame_error_with_the_rows() {
        // Arrange: four rows, with the last one's frame having failed.
        let mut state = ready(3);
        let at = state.insert_cue(
            0,
            Duration::from_millis(3200),
            Duration::from_millis(3800),
            "new".to_string(),
        );
        assert_that!(at).is_equal_to(2);
        state.fail_frame(3, "no such frame".to_string());

        // Act: take the inserted row out from above it.
        state.remove_cue(at);

        // Assert: the error followed its cue up a row.
        state.select_first();
        assert_that!(state.frame_error()).is_none();
        state.select(2);
        assert_that!(state.frame_error()).is_equal_to(Some("no such frame"));

        // Arrange / Act: and an error about the row that goes has nothing left to be about.
        let at = state.insert_cue(
            1,
            Duration::from_millis(3200),
            Duration::from_millis(3800),
            "again".to_string(),
        );
        state.fail_frame(at, "gone".to_string());
        state.remove_cue(at);

        // Assert
        assert_that!(state.frame_error()).is_none();
    }

    /// A position no row stands at is not a row to remove, and answering one by removing
    /// something else would be worse than answering nothing.
    #[test]
    fn removing_a_cue_that_is_not_there_should_do_nothing() {
        // Arrange
        let mut state = ready(3);

        // Act
        state.remove_cue(9);

        // Assert
        assert_that!(state.cues.len()).is_equal_to(3);
        assert_that!(state.selected).is_equal_to(0);
    }

    /// Types `query` into the page's filter bar, as the keys would.
    fn search_for(state: &mut SubtitleEditState, query: &str) {
        state.start_cue_search();
        state.cue_search_mut().input.value = query.to_string();
        state.refilter();
    }

    /// A page whose cues overlap into one group of four, preceded and followed by ordinary
    /// lone cues — the shape every rule about groups under a filter is about.
    fn grouped_page() -> SubtitleEditState {
        let mut state = state();
        state.apply_prepared(
            vec![
                cue(0, 1000, "opening line"),
                // Four cues sharing one stretch of screen, so they are one group.
                cue(2000, 9000, "alpha"),
                cue(2500, 9000, "bravo"),
                cue(3000, 9000, "charlie"),
                cue(3500, 9000, "delta"),
                cue(12000, 13000, "closing line"),
            ],
            CueStyle::SubRip,
        );
        state
    }

    #[test]
    fn an_empty_query_should_leave_every_group_on_the_list() {
        // Arrange
        let state = ready(4);

        // Assert: the projection is the whole list, so every path that reads it is the
        // code it always was rather than a filtered special case.
        assert_that!(state.visible_groups().to_vec()).is_equal_to(vec![0, 1, 2, 3]);
        assert_that!(state.cue_query()).is_equal_to("");
        assert_that!(state.cue_search().match_count).is_equal_to(0);
    }

    #[test]
    fn a_query_should_keep_only_the_matching_rows_and_count_the_cues() {
        // Arrange
        let mut state = ready(4);

        // Act
        search_for(&mut state, "LINE 2");

        // Assert: case-insensitive, and the count is of cues rather than of groups —
        // the reader is counting the lines they were looking for.
        assert_that!(state.visible_groups().to_vec()).is_equal_to(vec![2]);
        assert_that!(state.cue_search().match_count).is_equal_to(1);
    }

    #[test]
    fn a_row_whose_neighbours_share_its_screen_should_stay_whole() {
        // Arrange: only one member of the group of four matches.
        let mut state = grouped_page();

        // Act
        search_for(&mut state, "charlie");

        // Assert: the group is drawn, and it is still the group of four it was. Filtering
        // members out of it would draw a fork with nothing to fork into and make the row
        // lie about what shares the screen.
        assert_that!(state.visible_groups().to_vec()).is_equal_to(vec![1]);
        assert_that!(state.groups[1].len).is_equal_to(4);
        assert_that!(state.cue_search().match_count).is_equal_to(1);
    }

    /// The requirement that makes a filtered group worth showing at all: the reader can see
    /// the cue they searched for, not merely the row it happens to live on.
    #[test]
    fn a_group_should_be_entered_and_drawn_at_the_page_holding_its_match() {
        // Arrange: `delta` is the group's *last* member, on page one of two.
        let mut state = grouped_page();

        // Act
        search_for(&mut state, "delta");

        // Assert: the cursor is on the match rather than on the group's first member, and
        // the pair of blocks drawn is the one holding it.
        assert_that!(state.selected).is_equal_to(4);
        let (first, shown) = state.group_window(state.groups[1]);
        assert_that!(first).is_equal_to(3);
        assert_that!(shown).is_equal_to(2);
    }

    #[test]
    fn a_matching_group_the_cursor_is_not_in_should_still_draw_its_match() {
        // Arrange: a query the closing line matches too, so there is a second drawn row
        // for the cursor to move to and leave the group behind.
        let mut state = grouped_page();
        search_for(&mut state, "delta");
        state.edit_cue_text(5, "closing delta".to_string());
        // Group 1 is the run of four; group 2 is the closing line, cue 5.
        assert_that!(state.visible_groups().to_vec()).is_equal_to(vec![1, 2]);

        // Act: down onto the closing line, off the group entirely.
        assert_that!(state.select(1)).is_true();
        assert_that!(state.selected).is_equal_to(5);

        // Assert: the group the cursor left is still drawn at the page holding the only
        // reason it is on the list — a row showing two lines neither of which matched
        // would leave the reader unable to see why it is there.
        let (first, _) = state.group_window(state.groups[1]);
        assert_that!(first).is_equal_to(3);
    }

    #[test]
    fn moving_down_should_step_over_the_rows_the_filter_hides() {
        // Arrange: "line 0" and "line 3" match; the two between them do not.
        let mut state = ready(4);
        search_for(&mut state, "line 0");
        state.cue_search_mut().input.value = "line".to_string();
        state.refilter();
        state.cue_search_mut().input.value = "line 0".to_string();
        state.refilter();

        // Act / Assert: with one row left there is nowhere to go, and a refused move is
        // what stops a held `j` re-requesting the same frame on every repeat.
        assert_that!(state.select(1)).is_false();
        assert_that!(state.selected).is_equal_to(0);
    }

    #[test]
    fn j_and_k_should_walk_only_the_matching_rows() {
        // Arrange: cues 0 and 3 match, 1 and 2 do not.
        let mut state = state();
        state.apply_prepared(
            vec![
                cue(0, 1000, "keep me"),
                cue(2000, 3000, "skip"),
                cue(4000, 5000, "skip"),
                cue(6000, 7000, "keep me too"),
            ],
            CueStyle::SubRip,
        );

        // Act
        search_for(&mut state, "keep");

        // Assert: `j` lands on the next *drawn* row rather than walking through rows
        // nobody can see, which would be a cursor disappearing for several presses.
        assert_that!(state.visible_groups().to_vec()).is_equal_to(vec![0, 3]);
        assert_that!(state.select(1)).is_true();
        assert_that!(state.selected).is_equal_to(3);
        assert_that!(state.select(1)).is_false();
        assert_that!(state.select(-1)).is_true();
        assert_that!(state.selected).is_equal_to(0);
    }

    #[test]
    fn gg_and_capital_g_should_land_on_the_ends_of_the_drawn_list() {
        // Arrange
        let mut state = ready(5);
        search_for(&mut state, "line");
        state.cue_search_mut().input.value = "line 3".to_string();
        state.refilter();

        // Act / Assert: one row left, so both ends are it and neither key moves.
        assert_that!(state.selected).is_equal_to(3);
        assert_that!(state.select_first()).is_false();
        assert_that!(state.select_last()).is_false();

        // Act: widen the filter to three rows.
        state.cue_search_mut().input.value = "line".to_string();
        state.refilter();
        state.selected = 2;

        // Assert: the ends of the whole list, since everything matches "line".
        assert_that!(state.select_last()).is_true();
        assert_that!(state.selected).is_equal_to(4);
        assert_that!(state.select_first()).is_true();
        assert_that!(state.selected).is_equal_to(0);
    }

    #[test]
    fn gg_should_land_on_a_matching_member_of_the_first_drawn_group() {
        // Arrange: the first drawn row is the group of four, whose only match is its last
        // member; a second row below it gives the cursor somewhere to be first.
        let mut state = grouped_page();
        search_for(&mut state, "delta");
        state.edit_cue_text(5, "closing delta".to_string());
        assert_that!(state.select(1)).is_true();
        assert_that!(state.selected).is_equal_to(5);

        // Act
        assert_that!(state.select_first()).is_true();

        // Assert: `gg` is an absolute move, and the place it lands is still the match
        // rather than the group's first member — the same rule arriving by `k` follows.
        assert_that!(state.selected).is_equal_to(4);
    }

    #[test]
    fn the_cursor_should_hold_still_while_its_own_row_still_matches() {
        // Arrange
        let mut state = ready(4);
        state.select(2);
        assert_that!(state.selected).is_equal_to(2);

        // Act: a query the selected cue matches.
        search_for(&mut state, "line 2");

        // Assert: typing another letter must not move a reader off the line they are
        // reading, which is what resetting to the first match on every keystroke would do.
        assert_that!(state.selected).is_equal_to(2);
    }

    #[test]
    fn a_filter_that_hides_the_cursor_should_move_it_to_the_first_row_left() {
        // Arrange
        let mut state = ready(4);
        state.select(3);

        // Act: a query the selected cue does not match.
        search_for(&mut state, "line 1");

        // Assert
        assert_that!(state.visible_groups().to_vec()).is_equal_to(vec![1]);
        assert_that!(state.selected).is_equal_to(1);
    }

    #[test]
    fn a_filter_matching_nothing_should_leave_the_selection_alone_and_refuse_movement() {
        // Arrange
        let mut state = ready(4);
        state.select(2);

        // Act
        search_for(&mut state, "nothing here");

        // Assert: there is nowhere honest to put the cursor, so it stays — and the preview
        // pane goes on showing a real cue rather than blanking.
        assert_that!(state.visible_groups()).is_empty();
        assert_that!(state.selected).is_equal_to(2);
        assert_that!(state.cue_search().match_count).is_equal_to(0);
        assert_that!(state.select(1)).is_false();
        assert_that!(state.select(-1)).is_false();
        assert_that!(state.select_first()).is_false();
        assert_that!(state.select_last()).is_false();
        assert_that!(state.selected).is_equal_to(2);
    }

    #[test]
    fn a_filter_matching_nothing_should_leave_no_rows_to_scroll_through() {
        // Arrange
        let mut state = ready(8);
        search_for(&mut state, "nothing here");

        // Act
        state.cue_scroll(20);

        // Assert: the same answer an empty track gives, since there is nothing to draw.
        assert_that!(state.list_rows).is_equal_to(0);
        assert_that!(state.list_scroll).is_equal_to(0);
    }

    #[test]
    fn scrolling_should_count_only_the_rows_the_filter_is_drawing() {
        // Arrange: twelve cues of which three match, in a panel with room for two rows.
        let mut state = state();
        let cues = (0..12)
            .map(|index| {
                let start = index as u64 * 2000;
                let text = if index % 4 == 0 {
                    format!("wanted {index}")
                } else {
                    format!("other {index}")
                };
                cue(start, start + 1000, &text)
            })
            .collect();
        state.apply_prepared(cues, CueStyle::SubRip);
        search_for(&mut state, "wanted");
        assert_that!(state.visible_groups().to_vec()).is_equal_to(vec![0, 4, 8]);

        // Act: onto the last match, in a panel holding two lone cues (three rows each plus
        // a connector between them).
        state.select(2);
        state.cue_scroll(7);

        // Assert: positions in the *drawn* list, so the panel scrolls by one row rather
        // than by the eight groups the filter is hiding.
        assert_that!(state.selected).is_equal_to(8);
        assert_that!(state.list_scroll).is_equal_to(1);
        assert_that!(state.list_rows).is_equal_to(2);
    }

    #[test]
    fn h_and_l_should_still_reach_a_shown_groups_unmatched_members() {
        // Arrange: the group is on the list because `delta` matched, and the cursor is on
        // it — but the reader asked for the whole row, not the match alone.
        let mut state = grouped_page();
        search_for(&mut state, "delta");
        assert_that!(state.selected).is_equal_to(4);

        // Act / Assert: `h` walks back through the members that did not match, stopping at
        // the group's own end the way it always did.
        assert_that!(state.select_within_group(-1)).is_true();
        assert_that!(state.selected).is_equal_to(3);
        assert_that!(state.select_within_group(-2)).is_true();
        assert_that!(state.selected).is_equal_to(1);
        assert_that!(state.select_within_group(-1)).is_false();
    }

    #[test]
    fn rewriting_a_cue_should_move_it_into_or_out_of_the_filter() {
        // Arrange
        let mut state = ready(4);
        search_for(&mut state, "wanted");
        assert_that!(state.visible_groups()).is_empty();

        // Act: the words are what the filter matches on.
        state.edit_cue_text(2, "wanted line".to_string());

        // Assert
        assert_that!(state.visible_groups().to_vec()).is_equal_to(vec![2]);
        assert_that!(state.selected).is_equal_to(2);

        // Act: and back out of it again.
        state.edit_cue_text(2, "line 2".to_string());

        // Assert
        assert_that!(state.visible_groups()).is_empty();
    }

    #[test]
    fn adding_and_removing_a_cue_should_rebuild_the_filter_against_the_new_list() {
        // Arrange
        let mut state = ready(3);
        search_for(&mut state, "line 2");
        assert_that!(state.visible_groups().to_vec()).is_equal_to(vec![2]);

        // Act: a cue added before the match moves every position after it. Placed in the
        // gap between two existing cues so it forms a row of its own — this is about the
        // projection following a shift, not about grouping.
        let at = state.insert_cue(
            0,
            Duration::from_millis(1200),
            Duration::from_millis(1800),
            "line 2 as well".to_string(),
        );

        // Assert: both rows match now, and the projection describes the list as it is.
        assert_that!(at).is_equal_to(1);
        assert_that!(state.visible_groups().to_vec()).is_equal_to(vec![1, 3]);

        // Act: and taking it out again puts the projection back.
        state.remove_cue(1);

        // Assert
        assert_that!(state.visible_groups().to_vec()).is_equal_to(vec![2]);
    }

    #[test]
    fn abandoning_a_search_should_drop_the_filter_and_come_back_to_the_starting_cue() {
        // Arrange
        let mut state = ready(4);
        state.select(3);

        // Act: search away from it, then abandon.
        search_for(&mut state, "line 1");
        assert_that!(state.selected).is_equal_to(1);
        state.cancel_cue_search();

        // Assert: the whole list is back, the bar is closed, and the cursor is where the
        // reader left it rather than where the filter had put it.
        assert_that!(state.visible_groups().to_vec()).is_equal_to(vec![0, 1, 2, 3]);
        assert_that!(state.cue_search().is_active).is_false();
        assert_that!(state.cue_query()).is_equal_to("");
        assert_that!(state.selected).is_equal_to(3);
    }

    #[test]
    fn abandoning_a_search_should_find_the_starting_cue_even_after_the_list_shifted() {
        // Arrange: the origin is a `CueOrigin`, not a position, so a row added above it
        // while the filter was up does not send the cursor to the wrong line.
        let mut state = ready(4);
        state.select(2);

        // Act
        search_for(&mut state, "line 3");
        state.insert_cue(
            0,
            Duration::from_millis(100),
            Duration::from_millis(500),
            "line 3 inserted".to_string(),
        );
        state.cancel_cue_search();

        // Assert: cue 2 has become cue 3, and that is where the cursor comes back to.
        assert_that!(state.cues[state.selected].text.as_str()).is_equal_to("line 2");
    }

    #[test]
    fn confirming_a_search_should_keep_the_filter_and_close_the_bar() {
        // Arrange
        let mut state = ready(4);
        search_for(&mut state, "line 1");

        // Act
        state.finish_cue_search();

        // Assert: the keys go back to the list, and the list is still the filtered one —
        // which is what makes the bar stay drawn with the query in it.
        assert_that!(state.cue_search().is_active).is_false();
        assert_that!(state.cue_query()).is_equal_to("line 1");
        assert_that!(state.visible_groups().to_vec()).is_equal_to(vec![1]);
    }

    #[test]
    fn re_opening_a_search_should_come_back_to_where_the_last_one_was_confirmed() {
        // Arrange: search away from cue 3 and press Enter, accepting cue 0.
        let mut state = ready(4);
        state.select(3);
        search_for(&mut state, "line 0");
        state.finish_cue_search();
        assert_that!(state.selected).is_equal_to(0);

        // Act: `/` again on the filter still in force, then abandon it.
        state.start_cue_search();
        state.cancel_cue_search();

        // Assert: back to cue 0, not to cue 3. Confirming is the reader accepting where the
        // filter put them, so an `Esc` in a *later* search must not undo it — that would be
        // a key teleporting them to a cue they left several searches ago.
        assert_that!(state.selected).is_equal_to(0);
        assert_that!(state.cue_query()).is_equal_to("");
    }

    #[test]
    fn re_opening_a_search_mid_flight_should_still_return_to_where_it_began() {
        // Arrange: the bar can be re-opened without ever being confirmed, and then the
        // origin is still the cue the reader pressed `/` on.
        let mut state = ready(4);
        state.select(3);
        search_for(&mut state, "line 0");
        assert_that!(state.selected).is_equal_to(0);

        // Act: re-open without confirming, then abandon.
        state.start_cue_search();
        state.cancel_cue_search();

        // Assert
        assert_that!(state.selected).is_equal_to(3);
    }

    #[test]
    fn a_new_cue_list_should_arrive_unfiltered() {
        // Arrange
        let mut state = ready(4);
        search_for(&mut state, "line 1");

        // Act: what a save does — the page is closed and the rewritten file read back.
        state.apply_prepared(
            vec![cue(0, 1000, "fresh one"), cue(2000, 3000, "fresh two")],
            CueStyle::SubRip,
        );

        // Assert: a query is about the list it was typed against, and this is another one.
        assert_that!(state.cue_query()).is_equal_to("");
        assert_that!(state.visible_groups().to_vec()).is_equal_to(vec![0, 1]);
    }

    #[test]
    fn a_failed_page_should_leave_no_filter_behind_it() {
        // Arrange
        let mut state = ready(4);
        search_for(&mut state, "line 1");

        // Act
        state.fail("could not read the track".to_string());

        // Assert: no cues, so no rows, and nothing left claiming to filter them.
        assert_that!(state.cue_query()).is_equal_to("");
        assert_that!(state.visible_groups()).is_empty();
    }
}
