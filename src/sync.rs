//! State for the subtitle timing page: which cues a track holds, which one is selected,
//! and the scratch directory the preview worker stages files in.
//!
//! Kept out of `App` as one owned struct rather than a dozen loose fields, for the same
//! reason `staging::BatchState` is: the page's lifetime is a single `Option`, so every
//! way of leaving it — Esc, selecting another file, quitting — releases the whole thing
//! including its temp directory, without each exit path having to remember to.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ratatui::layout::Size;
use ratatui_image::protocol::Protocol;
use ratatui_image::protocol::halfblocks::Halfblocks;

use crate::audio::{AudioOutput, frame_index_at};
use crate::cue::{Cue, LaneLayout, MAX_LANES, pack_lanes};
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
    /// Nothing to draw an image with. Only reachable without a picker, which the binary
    /// always has — `Picker::halfblocks()` is its fallback and every terminal can draw
    /// those — so this is here because the code branches on it, not because a user meets
    /// it.
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
    /// Which frame [`Self::drawn`] holds, so a step inside one frame period re-draws
    /// nothing.
    shown: Option<usize>,
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
    pub fn new(cue_index: usize, frames: PlaybackFrames, output: Box<dyn AudioOutput>) -> Self {
        Self {
            cue_index,
            frames,
            output,
            shown: None,
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
    pub fn position(&self) -> Option<Duration> {
        let shown = self.shown?;
        if self.frames.fps == 0 {
            return Some(self.frames.span_start);
        }
        Some(
            self.frames.span_start
                + Duration::from_secs_f64(shown as f64 / f64::from(self.frames.fps)),
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
    pub fn advance(&mut self, now: Instant) -> PlaybackStep {
        let Some(position) = self.output.position(now) else {
            // The device has not started yet. Nothing is drawn during this rather than the
            // first frame being held up: starting the picture before the sound is exactly
            // the error the clock exists to prevent.
            return PlaybackStep::Unchanged;
        };
        let index = frame_index_at(position, self.frames.fps);
        if index >= self.frames.count() {
            return PlaybackStep::Finished;
        }
        if self.shown == Some(index) {
            return PlaybackStep::Unchanged;
        }
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
    /// `Halfblocks::new` directly rather than `Picker::new_protocol`: the picker's
    /// halfblocks font size is an arbitrary 1:2 placeholder, so `Resize::Scale` would blow
    /// the frame up to that font's idea of the pane and `Halfblocks` would immediately
    /// resample it back down to the cell grid — two passes, the expensive one wasted. The
    /// frame was decoded at exactly the cell grid's pixels ([`crate::preview::playback_pixels`]),
    /// so this way there is nothing to scale.
    fn encode(&self, index: usize) -> Option<Box<Protocol>> {
        let (width, height) = self.frames.pixels;
        let bytes = self.frames.frame(index)?;
        let image = image::RgbImage::from_raw(width, height, bytes.to_vec())?;
        let halfblocks =
            Halfblocks::new(image::DynamicImage::ImageRgb8(image), self.frames.cells).ok()?;
        Some(Box::new(Protocol::Halfblocks(halfblocks)))
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
    pub layout: LaneLayout,
    pub selected: usize,
    /// First cue row drawn, moved only to keep `selected` on screen.
    pub list_scroll: usize,
    /// Rows the cue list can show, measured by the renderer.
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
            layout: LaneLayout::default(),
            selected: 0,
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
    pub fn apply_prepared(&mut self, cues: Vec<Cue>, style: CueStyle) {
        // Onto the frame source, because that is what every renderer is handed and the
        // style is as much a part of drawing a cue as the cue is. It arrives only now:
        // an ASS script's styles come out of the file in the same pass as its cues.
        self.frames.style = Arc::new(style);
        self.layout = pack_lanes(&cues, MAX_LANES);
        self.status = if cues.is_empty() {
            SyncStatus::Empty
        } else {
            SyncStatus::Ready
        };
        self.cues = cues;
        self.selected = 0;
        self.list_scroll = 0;
        self.request_frame();
    }

    pub fn fail(&mut self, message: String) {
        self.status = SyncStatus::Failed(message);
        // Nothing to render frames for, so nothing to count. A pass already running for
        // this page is stopped by the generation bump that closing it performs.
        self.warm = WarmState::Off;
        self.cues.clear();
        self.layout = LaneLayout::default();
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
        // Every caller of this is a reason a playback on screen is no longer about what is
        // on screen: the cursor moved to another cue, the pane resized under frames encoded
        // for the old one, or a whole new track arrived. One place rather than five, for
        // the same reason `prune_frames` lives here — an exit path that forgot would leave
        // a span playing under a line it is not about, which reads as the timing being
        // wrong on the one page built to judge that.
        self.stop_playback();
        self.prune_frames();
        self.frame_pending_since = Some(Instant::now());
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
        Some(FrameTarget {
            cue_index,
            cue: cue.clone(),
            // `seek_for` rather than the midpoint outright: a cue running to the end of
            // the media has to be held back from the very last instant, and the background
            // pass has to make exactly the same decision — a disagreement would have the
            // two writing different pictures under one cache key.
            seek: seek_for(cue, self.duration),
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
    pub fn begin_playback(
        &mut self,
        cue_index: usize,
        frames: PlaybackFrames,
        output: Box<dyn AudioOutput>,
    ) {
        self.playback_error = None;
        self.playback = PlaybackState::Playing(Playback::new(cue_index, frames, output));
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

    /// Moves the playhead to wherever the sound has got to, reporting whether the page
    /// needs repainting.
    ///
    /// Called once per loop iteration rather than from the renderer, so the picture and the
    /// playhead are decided together and drawn together.
    pub fn advance_playback(&mut self) -> bool {
        let PlaybackState::Playing(playback) = &mut self.playback else {
            return false;
        };
        match playback.advance(Instant::now()) {
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

    /// Moves the cue cursor, reporting whether it actually moved.
    ///
    /// The return value is what stops a held-down `j` at the end of the list from
    /// re-requesting the same preview frame on every repeat.
    pub fn select(&mut self, delta: isize) -> bool {
        if self.cues.is_empty() {
            return false;
        }
        let last = self.cues.len() - 1;
        let next = if delta < 0 {
            self.selected.saturating_sub(delta.unsigned_abs())
        } else {
            self.selected.saturating_add(delta as usize).min(last)
        };
        if next == self.selected {
            return false;
        }
        self.selected = next;
        self.request_frame();
        true
    }

    pub fn select_first(&mut self) -> bool {
        if self.cues.is_empty() || self.selected == 0 {
            return false;
        }
        self.selected = 0;
        self.request_frame();
        true
    }

    pub fn select_last(&mut self) -> bool {
        if self.cues.is_empty() {
            return false;
        }
        let last = self.cues.len() - 1;
        if self.selected == last {
            return false;
        }
        self.selected = last;
        self.request_frame();
        true
    }

    /// Scrolls the cue list just far enough to keep the selection visible.
    ///
    /// Called from the renderer, which is the only place that knows how many rows the
    /// list actually got — the same arrangement `sync_batch_scroll` uses for the batch
    /// dialog.
    pub fn sync_scroll(&mut self, rows: usize) {
        self.list_rows = rows;
        if rows == 0 {
            return;
        }
        if self.selected < self.list_scroll {
            self.list_scroll = self.selected;
        } else if self.selected >= self.list_scroll + rows {
            self.list_scroll = self.selected + 1 - rows;
        }
        self.list_scroll = self.list_scroll.min(self.cues.len().saturating_sub(rows));
    }
}

#[cfg(test)]
mod tests {
    use kernal::prelude::*;

    use super::*;
    use crate::audio::SilentOutput;

    fn cue(start: u64, end: u64, text: &str) -> Cue {
        Cue {
            index: 0,
            start: Duration::from_millis(start),
            end: Duration::from_millis(end),
            text: text.to_string(),
            dialogue: None,
        }
    }

    fn state() -> SubtitleSyncState {
        costed_state(HALFBLOCKS_BYTES_PER_CELL)
    }

    /// What halfblocks costs: two `Color`s and a `char` per cell, and no dependence on the
    /// font size. The protocol the tests here actually encode with, via `protocol`.
    const HALFBLOCKS_BYTES_PER_CELL: u64 = 12;

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
    /// Sized in pixels from the halfblocks font size, because `Resize::Fit` takes the
    /// cell size from the image's own proportions rather than from what it was asked for.
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

    #[test]
    fn sync_scroll_should_follow_the_selection_down_past_the_last_visible_row() {
        // Arrange
        let mut state = ready(10);
        state.sync_scroll(4);

        // Act
        state.select(5);
        state.sync_scroll(4);

        // Assert
        assert_that!(state.list_scroll).is_equal_to(2);
    }

    #[test]
    fn sync_scroll_should_follow_the_selection_back_up_above_the_first_visible_row() {
        // Arrange
        let mut state = ready(10);
        state.select(9);
        state.sync_scroll(4);

        // Act
        state.select(-9);
        state.sync_scroll(4);

        // Assert
        assert_that!(state.list_scroll).is_equal_to(0);
    }

    #[test]
    fn sync_scroll_should_not_leave_blank_rows_below_a_short_list() {
        // Arrange: scrolled to the bottom, then given a taller pane.
        let mut state = ready(6);
        state.select(5);
        state.sync_scroll(2);
        assert_that!(state.list_scroll).is_equal_to(4);

        // Act
        state.sync_scroll(6);

        // Assert
        assert_that!(state.list_scroll).is_equal_to(0);
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
        // The seek is the cue's midpoint, the same one the background pass renders under,
        // or the two would write different pictures for one cache key.
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

    /// Halfblocks is twelve bytes a cell whatever the pane, and a build with no picker
    /// encodes nothing at all — neither can ever reach the budget, so neither should be
    /// made to pay for it with a shortened window.
    #[test]
    fn a_window_that_costs_nothing_should_never_be_shortened() {
        // Arrange / Act / Assert: halfblocks, on a pane far past what kitty could afford.
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

    /// A span of `count` frames, each a solid colour naming its index, so a test can tell
    /// which one is on screen from the picture rather than from a counter beside it.
    fn span(count: usize, cells: Size, fps: u32) -> PlaybackFrames {
        let pixels = crate::preview::playback_pixels(cells);
        let stride = (pixels.0 as usize) * (pixels.1 as usize) * 3;
        let mut bytes = Vec::with_capacity(stride * count);
        for index in 0..count {
            bytes.extend(std::iter::repeat_n(index as u8, stride));
        }
        PlaybackFrames::new(bytes, pixels, cells, fps, Duration::from_secs(10))
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
        state.begin_playback(
            0,
            span(10, Size::new(4, 2), 10),
            Box::new(SharedOutput(std::sync::Arc::clone(&clock))),
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

    /// A playback that reached its last frame is finished with, not paused on it: the next
    /// `p` should replay the span rather than having to stop it first. The device goes back
    /// at the same moment, which is what stops a page left open holding one open forever.
    #[test]
    fn a_playback_should_end_itself_when_the_sound_runs_past_the_span() {
        // Arrange: three frames at ten a second, so the span lasts 300 ms.
        let mut state = ready(3);
        let clock = SteppedOutput::at(Some(Duration::from_millis(250)));
        state.begin_playback(
            0,
            span(3, Size::new(4, 2), 10),
            Box::new(SharedOutput(std::sync::Arc::clone(&clock))),
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

    /// The playhead is read off the frame on screen rather than off the clock, so the
    /// picture and the mark on the timeline in one drawn frame describe the same instant.
    /// Straight off the clock it would sit up to a frame period ahead of the picture.
    #[test]
    fn the_playhead_should_report_where_the_picture_is_rather_than_where_the_sound_is() {
        // Arrange: a span starting ten seconds into the media, at ten frames a second.
        let mut state = ready(3);
        let clock = SteppedOutput::at(Some(Duration::from_millis(2_390)));
        state.begin_playback(
            0,
            span(30, Size::new(4, 2), 10),
            Box::new(SharedOutput(std::sync::Arc::clone(&clock))),
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
        let started = |state: &mut SubtitleSyncState| {
            state.begin_playback(
                0,
                span(30, Size::new(4, 2), 10),
                Box::new(SilentOutput::new()),
            );
        };

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

        // Act / Assert: the pane resizing, which every frame in the span was encoded for.
        started(&mut state);
        state.set_preview_cells(Size::new(40, 20));
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
        state.begin_playback(
            2,
            span(4, Size::new(4, 2), 10),
            Box::new(SilentOutput::new()),
        );
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
        state.begin_playback(
            1,
            span(6, Size::new(4, 2), 10),
            Box::new(SilentOutput::new()),
        );

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
        state.begin_playback(0, span(3, cells, 10), Box::new(SilentOutput::new()));

        // Act
        assert_that!(state.advance_playback()).is_true();

        // Assert
        let protocol = state.playback_frame().expect("a frame should be drawn");
        assert_that!(protocol.size()).is_equal_to(cells);
    }
}
