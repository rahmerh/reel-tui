//! State for the subtitle timing page: which cues a track holds, which one is selected,
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

use crate::audio::{AudioOutput, AudioSource, frame_index_at};
use crate::cue::{Cue, CueGroup, LaneLayout, MAX_LANES, group_overlaps, pack_lanes, shares_screen};
use crate::preview::{
    CueStyle, FRAME_WINDOW_BUDGET, FrameSource, FrameTarget, PlaybackFrames, seek_for,
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

/// How many cues of one overlap group the panel draws side by side.
///
/// **Two is forced by the width, not chosen.** The cue panel is thirty to forty-eight
/// columns (`SYNC_CUE_PANEL_WIDTH`), so two blocks get fourteen to twenty-three each and a
/// third would leave ten to sixteen — which cannot hold a timing at all, and a block with no
/// timing on this page is a block with nothing worth reading on it. Members past the two are
/// reached with `h`/`l`, and the panel marks that they are there.
pub const GROUP_COLUMNS: usize = 2;

/// Rows one cue's block occupies: two borders with a line of text between them.
pub const SYNC_BLOCK_ROWS: usize = 3;

/// Rows a group of overlapping cues occupies: a block, plus the row the later-starting
/// member is dropped by.
pub const SYNC_GROUP_ROWS: usize = SYNC_BLOCK_ROWS + 1;

/// Rows the fork at the head of a group costs: its crossbar, and the arrows down into each
/// member.
pub const SYNC_FORK_ROWS: usize = 2;

/// Rows the `↓` between one row of the panel and the next costs.
pub const SYNC_CONNECTOR_ROWS: usize = 1;

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
pub enum SyncStatus {
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

/// One cue's span, decoded and playing.
///
/// Holds the whole span's raw pixels rather than encoding them all up front: a protocol is
/// several times the size of the pixels it came from, and only one of them is ever on
/// screen. The frame under the playhead is encoded as it is reached and kept until the
/// playhead leaves it, so a step that does not cross a frame boundary costs nothing at all.
pub struct Playback {
    /// Which cue this span was played for. A span that arrives after the cursor has moved
    /// is dropped rather than played under a line it is not about.
    cue_index: usize,
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
        cue_index: usize,
        frames: PlaybackFrames,
        source: Box<dyn AudioSource>,
        looping: bool,
    ) -> Self {
        let cells = frames.cells;
        let output = source.open();
        Self {
            cue_index,
            frames,
            output,
            source,
            looping,
            shown: None,
            cells,
            drawn: None,
        }
    }

    pub fn cue_index(&self) -> usize {
        self.cue_index
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
    /// refuses that terminal at startup, and the timing page says why instead of rendering
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
/// what a reader of a failure message actually wants: which cue, how far in, out of what.
impl std::fmt::Debug for Playback {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Playback")
            .field("cue_index", &self.cue_index)
            .field("shown", &self.shown)
            .field("frames", &self.frames)
            .finish()
    }
}

/// Whether the page is playing a cue's span, getting ready to, or neither.
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
        cue_index: usize,
    },
    Playing(Playback),
}

/// Everything the subtitle timing page draws and navigates.
#[derive(Debug)]
pub struct SubtitleSyncState {
    /// Which page opening this is. Echoed by every worker message so results belonging
    /// to a page the user has already left can be dropped rather than applied.
    pub generation: u64,
    /// Everything a frame grab needs about the media: the file itself — the container for
    /// an embedded track, or the sidecar's companion media — how to tell it has changed,
    /// and the size its frames are rendered at.
    pub frames: FrameSource,
    pub source: SubtitleSource,
    pub duration: Duration,
    pub status: SyncStatus,
    /// Whether frames can be drawn at all, and if not, why. Fixed for the page's life.
    pub support: PreviewSupport,
    /// How far the background frame pass has got, or why there is not one.
    pub warm: WarmState,
    pub cues: Vec<Cue>,
    /// Whether `h`/`l` move the selected cue through time rather than through its group.
    ///
    /// A mode rather than a dialog because it has to coexist with a playback: no dialog may
    /// be raised over one ([`crate::app::App::playback_in_progress`]), and the work this is
    /// for is alternating a nudge with a `p` until the line lands.
    ///
    /// Sticky, and follows the cursor: `j`/`k` still walk the track, so a whole file can be
    /// retimed without turning the mode on again at every cue.
    pub timing_mode: bool,
    pub layout: LaneLayout,
    /// The cue list split into runs that share the screen, parallel to nothing — each cue
    /// is in exactly one group and the ordinary cue is a group of one.
    ///
    /// The cue panel draws a group as its unit rather than a cue, so this is what its
    /// scrolling and its `j`/`k` movement are measured in.
    pub groups: Vec<CueGroup>,
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
    /// Why the last playback of the cue at this index could not run.
    ///
    /// Keyed on the cue for the same reason `frame_error` is, and on the status row for the
    /// same reason too: a playback is asked for by a keypress against one line, and the
    /// reason it failed says nothing about the next one.
    playback_error: Option<(usize, String)>,
    workspace: PreviewWorkspace,
}

impl SubtitleSyncState {
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
            status: SyncStatus::Preparing,
            support,
            warm: WarmState::Off,
            cues: Vec::new(),
            timing_mode: false,
            layout: LaneLayout::default(),
            groups: Vec::new(),
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
            SyncStatus::Empty
        } else {
            SyncStatus::Ready
        };
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
        self.status = SyncStatus::Failed(message);
        // Nothing to render frames for, so nothing to count. A pass already running for
        // this page is stopped by the generation bump that closing it performs.
        self.warm = WarmState::Off;
        self.cues.clear();
        self.layout = LaneLayout::default();
        self.groups.clear();
        self.encoded.clear();
        self.frame_pending_since = None;
        self.refill_nearby = false;
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
        self.request_frame();
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

    /// Whether the selected cue's own frame has been asked for since the last request was
    /// sent — as opposed to only the neighbours around it.
    pub fn frame_requested(&self) -> bool {
        self.frame_pending_since.is_some()
    }

    /// Whether anything at all has been asked for, of either kind.
    pub fn any_frame_requested(&self) -> bool {
        self.frame_pending_since.is_some() || self.refill_nearby
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

    /// Records that a span is being decoded for `cue_index`, clearing any earlier reason.
    pub fn prepare_playback(&mut self, cue_index: usize) {
        self.playback_error = None;
        self.playback = PlaybackState::Preparing { cue_index };
    }

    /// Whether a span is being decoded, and for which cue — so one arriving for a cue the
    /// cursor has since left can be dropped.
    pub fn preparing_playback(&self) -> Option<usize> {
        match self.playback {
            PlaybackState::Preparing { cue_index } => Some(cue_index),
            _ => None,
        }
    }

    /// Starts playing a decoded span.
    ///
    /// Takes somewhere the sound can be sent rather than an output already open, so that a
    /// looping playback can open another when it comes round — see [`Playback`].
    pub fn begin_playback(
        &mut self,
        cue_index: usize,
        frames: PlaybackFrames,
        source: Box<dyn AudioSource>,
        looping: bool,
    ) {
        self.playback_error = None;
        self.playback = PlaybackState::Playing(Playback::new(cue_index, frames, source, looping));
    }

    /// Records why a span could not be played, and stops waiting for it.
    pub fn fail_playback(&mut self, cue_index: usize, message: String) {
        self.playback = PlaybackState::Idle;
        self.playback_error = Some((cue_index, message));
    }

    /// Why the cue under the cursor could not be played, if that is why it is not playing.
    pub fn playback_error(&self) -> Option<&str> {
        self.playback_error
            .as_ref()
            .filter(|(cue_index, _)| *cue_index == self.selected)
            .map(|(_, message)| message.as_str())
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
            self.request_frame();
        }
    }

    pub fn selected_cue(&self) -> Option<&Cue> {
        self.cues.get(self.selected)
    }

    /// Whether the page is waiting on background work, so the loader keeps animating.
    pub fn is_busy(&self) -> bool {
        self.status == SyncStatus::Preparing
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
    pub fn group_window(&self, group: CueGroup) -> (usize, usize) {
        let shown = group.len.min(GROUP_COLUMNS);
        let page = if group.holds(self.selected) {
            self.group_page
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
    pub fn select(&mut self, delta: isize) -> bool {
        if self.groups.is_empty() {
            return false;
        }
        let last = self.groups.len() - 1;
        let current = self.selected_group();
        let next = if delta < 0 {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current.saturating_add(delta as usize).min(last)
        };
        if next == current {
            return false;
        }
        self.remember_place();
        self.enter_group(next);
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
    fn enter_group(&mut self, index: usize) {
        let group = self.groups[index];
        let (cue, page) = self
            .group_memory
            .get(&index)
            .copied()
            .filter(|(cue, _)| group.holds(*cue))
            .unwrap_or((group.first, 0));
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
    pub fn select_first(&mut self) -> bool {
        if self.cues.is_empty() || self.selected == 0 {
            return false;
        }
        self.remember_place();
        self.selected = 0;
        self.group_page = 0;
        self.remember_place();
        self.select_cue();
        true
    }

    /// The bottom of the list, which is the last *group's* first member rather than the
    /// last cue — the same place `j` would land on arriving there, so `G` and a held `j`
    /// agree about where the end is.
    pub fn select_last(&mut self) -> bool {
        let Some(group) = self.groups.last().copied() else {
            return false;
        };
        if self.selected == group.first {
            return false;
        }
        self.remember_place();
        self.selected = group.first;
        self.group_page = 0;
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
            Some(group) if group.len > 1 => SYNC_FORK_ROWS + SYNC_GROUP_ROWS,
            _ => SYNC_BLOCK_ROWS,
        }
    }

    /// How many groups fit in `height` rows starting from this one.
    ///
    /// At least one, so the shortest panel the layout can produce still shows the group
    /// under the cursor rather than nothing at all — a group that does not fit whole is
    /// clipped from the bottom, exactly as an over-tall cue block always was.
    fn groups_fitting(&self, from: usize, height: usize) -> usize {
        let mut used = self.group_height(from);
        let mut count = 1;
        for group in from + 1..self.groups.len() {
            let next = used + SYNC_CONNECTOR_ROWS + self.group_height(group);
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
    fn max_scroll(&self, height: usize) -> usize {
        // Saturating rather than guarded: an empty track never reaches here — `sync_scroll`
        // has already returned — and if it did, the loop below is empty and the answer is
        // the zero it should be.
        let last = self.groups.len().saturating_sub(1);
        let mut used = self.group_height(last);
        let mut scroll = last;
        for group in (0..last).rev() {
            let next = used + SYNC_CONNECTOR_ROWS + self.group_height(group);
            if next > height {
                break;
            }
            used = next;
            scroll = group;
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
    pub fn sync_scroll(&mut self, height: usize) {
        if height == 0 || self.groups.is_empty() {
            self.list_rows = 0;
            self.list_scroll = 0;
            return;
        }
        let selected = self.selected_group();
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

    fn state() -> SubtitleSyncState {
        costed_state(CHEAP_BYTES_PER_CELL)
    }

    /// A per-cell cost small enough that no pane reaches [`FRAME_WINDOW_BUDGET`], so tests
    /// that are not about the budget get the full window. Sixel on a small font is around
    /// this; kitty is two orders of magnitude more, which is what
    /// `a_costly_window_should_be_shortened_to_fit_the_budget` uses instead.
    const CHEAP_BYTES_PER_CELL: u64 = 12;

    fn costed_state(frame_cost: u64) -> SubtitleSyncState {
        SubtitleSyncState::new(
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

    fn ready(count: usize) -> SubtitleSyncState {
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
        assert_that!(state.status.clone()).is_equal_to(SyncStatus::Preparing);
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
        assert_that!(state.status.clone()).is_equal_to(SyncStatus::Ready);
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
        assert_that!(state.status.clone()).is_equal_to(SyncStatus::Empty);
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
            .is_equal_to(SyncStatus::Failed("ffmpeg exploded".to_string()));
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
    fn grouped() -> SubtitleSyncState {
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
        SYNC_BLOCK_ROWS + groups.saturating_sub(1) * (SYNC_CONNECTOR_ROWS + SYNC_BLOCK_ROWS)
    }

    #[test]
    fn sync_scroll_should_follow_the_selection_down_past_the_last_visible_row() {
        // Arrange
        let mut state = ready(10);
        state.sync_scroll(rows_for(4));

        // Act
        state.select(5);
        state.sync_scroll(rows_for(4));

        // Assert
        assert_that!(state.list_scroll).is_equal_to(2);
        assert_that!(state.list_rows).is_equal_to(4);
    }

    #[test]
    fn sync_scroll_should_follow_the_selection_back_up_above_the_first_visible_row() {
        // Arrange
        let mut state = ready(10);
        state.select(9);
        state.sync_scroll(rows_for(4));

        // Act
        state.select(-9);
        state.sync_scroll(rows_for(4));

        // Assert
        assert_that!(state.list_scroll).is_equal_to(0);
    }

    #[test]
    fn sync_scroll_should_not_leave_blank_rows_below_a_short_list() {
        // Arrange: scrolled to the bottom, then given a taller pane.
        let mut state = ready(6);
        state.select(5);
        state.sync_scroll(rows_for(2));
        assert_that!(state.list_scroll).is_equal_to(4);

        // Act
        state.sync_scroll(rows_for(6));

        // Assert
        assert_that!(state.list_scroll).is_equal_to(0);
    }

    /// A group costs twice what a lone cue does — its fork and the row its second member
    /// is dropped by — so a screenful is no longer a division. A panel holding three lone
    /// cues holds only two rows once the middle one is a group.
    #[test]
    fn sync_scroll_should_charge_a_group_for_the_rows_it_actually_takes() {
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
        state.sync_scroll(rows_for(3));
        assert_that!(state.list_rows).is_equal_to(2);

        // Act / Assert: one row short of the group is one row short of showing it.
        state.sync_scroll(SYNC_BLOCK_ROWS + SYNC_CONNECTOR_ROWS + SYNC_FORK_ROWS + SYNC_GROUP_ROWS);
        assert_that!(state.list_rows).is_equal_to(2);
        state.sync_scroll(
            SYNC_BLOCK_ROWS + SYNC_CONNECTOR_ROWS + SYNC_FORK_ROWS + SYNC_GROUP_ROWS - 1,
        );
        assert_that!(state.list_rows).is_equal_to(1);
    }

    /// A panel too short for even one row still shows the group under the cursor rather
    /// than nothing at all — the renderer clips it from the bottom.
    #[test]
    fn sync_scroll_should_always_keep_one_group_on_screen() {
        // Arrange
        let mut state = ready(10);
        state.select(9);

        // Act
        state.sync_scroll(1);

        // Assert
        assert_that!(state.list_rows).is_equal_to(1);
        assert_that!(state.list_scroll).is_equal_to(9);
    }

    #[test]
    fn sync_scroll_should_record_the_measured_row_count_and_tolerate_a_pane_with_none() {
        // Arrange
        let mut state = ready(10);
        state.select(9);

        // Act
        state.sync_scroll(0);

        // Assert
        assert_that!(state.list_rows).is_equal_to(0);
        assert_that!(state.list_scroll).is_equal_to(0);
    }

    /// A page holding no cues is still rendered — `cues` is public and a track can be
    /// emptied under one — so a panel with rows to give and nothing to put in them has to
    /// answer rather than divide by an empty list.
    #[test]
    fn sync_scroll_should_tolerate_a_track_with_no_cues_in_a_pane_with_rows() {
        // Arrange
        let mut state = state();

        // Act
        state.sync_scroll(20);

        // Assert
        assert_that!(state.list_rows).is_equal_to(0);
        assert_that!(state.list_scroll).is_equal_to(0);
    }

    /// The take-if-due the state used to expose, which `App::start_pending_preview` now
    /// spells out itself so that a frame already in the cache can skip the wait.
    fn take_due(state: &mut SubtitleSyncState) -> bool {
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
        let due = |state: &mut SubtitleSyncState| {
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
    fn play(state: &mut SubtitleSyncState, cue_index: usize, frames: PlaybackFrames) {
        play_through(state, cue_index, frames, Box::new(SilentSource));
    }

    fn play_through(
        state: &mut SubtitleSyncState,
        cue_index: usize,
        frames: PlaybackFrames,
        source: Box<dyn AudioSource>,
    ) {
        play_looping(state, cue_index, frames, source, false);
    }

    fn play_looping(
        state: &mut SubtitleSyncState,
        cue_index: usize,
        frames: PlaybackFrames,
        source: Box<dyn AudioSource>,
        looping: bool,
    ) {
        state.set_preview_cells(Size::new(frames.cells.width, frames.cells.height * 4));
        state.begin_playback(cue_index, frames, source, looping);
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

    fn playing(state: &mut SubtitleSyncState) -> &Playback {
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
        let started = |state: &mut SubtitleSyncState| play(state, 0, span(30, Size::new(4, 2), 10));

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
            0,
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
            0,
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
            0,
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
            0,
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
            0,
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
        state.prepare_playback(0);

        // Assert: waiting, and saying so.
        assert_that!(state.playback_active()).is_true();
        assert_that!(state.preparing_playback()).is_equal_to(Some(0));
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
        state.prepare_playback(0);

        // Act
        state.fail_playback(0, "Could not play this cue: no such file".to_string());

        // Assert
        assert_that!(state.playback_active()).is_false();
        assert_that!(state.playback_error())
            .is_equal_to(Some("Could not play this cue: no such file"));

        // Act / Assert: and it does not follow the cursor onto a line it is not about.
        assert_that!(state.select(1)).is_true();
        assert_that!(state.playback_error()).is_none();

        // Act / Assert: asking again clears it, so a retry that works leaves nothing behind.
        state.prepare_playback(1);
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
        state.prepare_playback(2);
        assert_that!(state.preparing_playback()).is_equal_to(Some(2));
        play(&mut state, 2, span(4, Size::new(4, 2), 10));
        assert_that!(state.preparing_playback()).is_none();
        assert_that!(playing(&mut state).cue_index()).is_equal_to(2);
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
        assert_that!(described.as_str()).contains("cue_index: 1");
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
}
