//! Background preparation of one subtitle track for the timing page.
//!
//! The page opens immediately and shows a loader; this is what fills it in. Two source
//! shapes exist and only two: a `.srt` sidecar, which is read straight off disk, and an
//! embedded `subrip` stream, which `ffmpeg` copies out into the page's workspace first.
//!
//! It runs on its own thread because the second case is slow in a way that is invisible
//! locally: `-c:s copy` still demuxes the container to EOF, which is a second or three on
//! a large local Matroska and tens of seconds over NFS. Doing it inside `handle_key`
//! would freeze the terminal for that whole time with no way out.
//!
//! Deliberately *not* routed through `edit.rs`'s subtitle extraction. That path drags in
//! `SubtitleChange`, `ProgressReporter`, `EditError`, `seconv` and the OCR fallbacks, and
//! — worse — reports its failures into `~/.cache/reel-tui/edit_errors.log`, which
//! `AGENTS.md` designates as the first place to look when hunting an *edit* regression.
//! A read-only preview has no business writing there, so this owns its ~30 lines of
//! command construction instead.
//!
//! Three threads, because they are asked for different things at different rates: cues
//! once per page opening, the cue under the cursor and its immediate neighbours once per
//! cursor movement, and every *other* cue's frame in the background from the moment the
//! cues land. Frames from all three paths go through [`crate::framecache`], so the second
//! visit to a cue — and every visit after the page is re-opened — costs a decode rather
//! than an `ffmpeg` seek.
//!
//! The neighbours are why walking the cue list draws a picture per keypress rather than
//! per round trip: by the time the cursor reaches a cue, the page is already holding it
//! encoded (see `SubtitleSyncState::nearby_frame_targets`).

use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use ratatui::layout::Size;
use ratatui_image::FontSize;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use ratatui_image::{FilterType, Resize};

use crate::cue::{Cue, format_srt_timestamp, read_ass, read_srt, retimed_dialogue};
use crate::framecache::{self, FrameKeyParts};
use crate::subtitle::SubtitleFormat;

/// Filename an embedded track is staged under inside the page's workspace.
pub const CUES_FILE: &str = "cues.srt";

/// Which renderer owns a staging file, and so which name it writes its cue under.
///
/// **Every renderer that can run at once needs its own name**, and that is the whole
/// point of this existing. The background pass runs several threads in one workspace, and
/// two sharing a staged file would each overwrite the other's cue between the write and
/// the burn — so a frame would come back with the wrong line on it, stored under the
/// *right* cache key, where nothing downstream could ever tell. The interactive grab runs
/// alongside all of them and needs a name of its own for the same reason.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CueSlot {
    /// The grab the user is waiting on.
    Interactive,
    /// One worker of the background pass.
    Warm(usize),
    /// The scrub playback's span. Its own name for the same reason the others have one —
    /// it runs while the background pass is still walking the track — and because what it
    /// stages is a *differently timed* copy of the same cue, so sharing a name with the
    /// interactive grab would have each drawing the other's timing.
    Playback,
}

impl CueSlot {
    /// The staged file's name, which the format decides the extension of.
    ///
    /// Deliberately short and relative. `ffmpeg` runs with the workspace as its working
    /// directory and is handed this bare name, so the value inside the `subtitles=` filter
    /// is a literal that needs no escaping at all — the alternative is quoting a user's
    /// path through three layers (filtergraph `[],;`, filter-arg `:`, option-parser `\`
    /// and `'`), which is a class of bug this feature simply does not have to have.
    fn file(self, style: &CueStyle) -> String {
        let extension = style.extension();
        match self {
            Self::Interactive => format!("cue.{extension}"),
            Self::Warm(worker) => format!("warm-{worker}.{extension}"),
            Self::Playback => format!("playback.{extension}"),
        }
    }
}

/// How long the staged cue is made to cover, so the grab cannot land outside it.
const BURN_WINDOW: Duration = Duration::from_secs(600);

/// The most threads that will ever walk a track's cues at once.
///
/// Each frame is an accurate seek plus a libass burn plus a JPEG encode, which is mostly
/// CPU. Three rather than "all of them" because the returns flatten — a measured 4.1x on a
/// 60-cue track — and because every worker competes with the interactive grab the user is
/// actually waiting on.
pub const MAX_WARM_WORKERS: usize = 3;

/// How many threads to walk a track's cues with on *this* machine.
///
/// Half the cores, because a fixed three made the foreground worse on a small machine
/// rather than better: measured against an uncached interactive grab with a pass running
/// underneath, three workers cost +18% on 32 cores, +93% on four and **+157% on two** —
/// two and a half times the latency of the one frame the user is sitting there waiting
/// for. Half leaves the other half for that grab, and the ffmpeg decoding it, which is
/// itself threaded.
///
/// Never zero, so a machine that cannot report its parallelism still renders.
fn warm_workers() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZero::get)
        .unwrap_or(1)
        .div_euclid(2)
        .clamp(1, MAX_WARM_WORKERS)
}

/// "Has the page this was for gone?", as every stage of a grab asks it.
///
/// `Sync`, because the background pass hands one of these to all [`WARM_WORKERS`] threads
/// rather than giving each its own — there is one page, and it closes for all of them at
/// the same instant.
type Abandoned<'a> = &'a (dyn Fn() -> bool + Sync);

/// How often a running `ffmpeg` is checked on, matching `edit.rs`'s runner.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// The largest a frame is rendered before it is cached.
///
/// Fixed rather than the pane's own pixel size, which is what it used to be. A cached frame
/// has to serve whatever size the pane happens to be later — otherwise resizing the
/// terminal invalidates every frame on disk — so this has to be a bound on every pane the
/// frame might be drawn into rather than on any one of them.
///
/// 1080p is that bound in practice: the preview pane is a fraction of the terminal, and a
/// terminal filling a 4K display is around 3840 pixels wide, so a pane wider than this
/// takes a display most people do not have and a font most people do not use. Above it the
/// picture is detail no terminal draws, paid for in cache and in the resize every draw
/// performs.
///
/// It was 960×540, which was visibly soft on a large pane. What made the larger size
/// affordable was storing frames as JPEG rather than PNG — see
/// [`crate::framecache::FRAME_EXTENSION`]. At 1080p a frame is around ninety kilobytes,
/// against two megabytes for a *smaller* PNG.
const MAX_FRAME_PIXELS: (u32, u32) = (1920, 1080);

/// How much memory the whole ready window is allowed to hold at once.
///
/// [`MAX_FRAME_PIXELS`] bounds what is *rendered*; this bounds what is kept *encoded*, and
/// the two are unrelated numbers. A `Protocol` does not hold the picture — it holds the
/// bytes the terminal is going to be sent, and for kitty that is the frame's pixels as
/// base64 RGBA, so a full window on a large pane can run to tens of megabytes for five
/// pictures the user is looking at one of. Forty-eight megabytes buys the full window on
/// any ordinary pane and gives up the outer neighbours on the panes where it would not.
pub const FRAME_WINDOW_BUDGET: u64 = 48 * 1024 * 1024;

/// How often the background pass reports where it has got to.
///
/// A track whose frames are already cached is walked in microseconds per cue, and an event
/// per cue would push thousands of messages through the channel for a status line that
/// only counts. The first and the last are always sent, so the line still appears and
/// still finishes.
const WARM_PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// Consecutive failures the background pass tolerates before it gives up on the track.
///
/// One cue that cannot be drawn is worth stepping over; three in a row means the media or
/// the FFmpeg build cannot produce frames at all, and the alternative is spawning a doomed
/// subprocess for every remaining cue in the file. The user still sees the real reason:
/// selecting such a cue reports it under the preview, from the interactive path.
const WARM_FAILURE_TOLERANCE: usize = 3;

/// One track to read, described the way the worker needs it rather than the way `App`
/// stores it — the worker knows nothing about `SubtitleSource` or which file is selected.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrepareRequest {
    /// The page opening this belongs to. Echoed back so a result for a page the user has
    /// already left can be dropped instead of applied to whatever is open now.
    pub generation: u64,
    /// The container to extract from, or the sidecar to read.
    pub input: PathBuf,
    /// The **absolute** ffprobe stream index for an embedded track, matching `edit.rs`'s
    /// convention — never the `0:s:N` per-type form. `None` means `input` is a sidecar.
    pub stream_index: Option<u64>,
    /// What `input` holds, which decides how it is turned into cues. Settled by
    /// `App::open_subtitle_sync`, which has already refused the formats with no road here
    /// at all — see [`ToolCapabilities::preview_blocked`].
    ///
    /// [`ToolCapabilities::preview_blocked`]: crate::subtitle::ToolCapabilities::preview_blocked
    pub format: SubtitleFormat,
    pub workspace: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrepareOutcome {
    Ready {
        cues: Vec<Cue>,
        /// How this track's cues have to be staged to be drawn, which comes out of the
        /// file with them: an ASS script's styles and `PlayRes` are read in the same pass
        /// as its `Dialogue:` lines and are useless apart from them.
        style: CueStyle,
    },
    Failed(String),
}

/// Everything about a frame grab that is the same for every cue in the track.
///
/// Shared by the interactive request and the background pass so the two cannot disagree
/// about what they are rendering — a disagreement would show up as the two writing
/// different pictures under the same cache key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrameSource {
    /// The video frames are grabbed from.
    pub media: PathBuf,
    /// The media's length and mtime, in the cache key so that re-encoding a file in place
    /// does not serve the old file's frames under the new one's path.
    pub media_length: u64,
    pub media_modified: Option<SystemTime>,
    /// The size every frame for this media is rendered at, from [`target_pixels`].
    pub pixels: (u32, u32),
    /// How this track's cues are written back out for libass, which for ASS is most of
    /// what a cue is. Set when the cues arrive, since it comes out of the file with them.
    pub style: Arc<CueStyle>,
    pub workspace: PathBuf,
}

/// How one cue is staged as a subtitle file for libass to burn in.
///
/// A cue on its own is not enough to draw for every format. An ASS cue names a style
/// rather than carrying one and positions itself against the script's declared
/// `PlayResX`/`PlayResY`, so the track's header has to be staged with it or libass draws
/// the line in its own defaults at its own scale — a picture the user will never see, on
/// the one page whose whole job is comparing subtitles against the picture.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum CueStyle {
    /// Everything the cue needs is its text.
    #[default]
    SubRip,
    /// The track's own `[Script Info]` and `[V4+ Styles]`, verbatim, and the `[Events]`
    /// `Format:` line its cues' columns are declared by.
    Ass {
        header: String,
        events_format: String,
    },
}

impl CueStyle {
    /// The extension the staged file must carry.
    ///
    /// Not decoration: libass picks its reader from the name, so a staged ASS script
    /// called `.srt` is read as SubRip and every override tag in it becomes literal text
    /// on the frame.
    fn extension(&self) -> &'static str {
        match self {
            Self::SubRip => "srt",
            Self::Ass { .. } => "ass",
        }
    }

    /// The whole subtitle file to hand libass, holding every cue on screen at the moment
    /// being grabbed — see [`crate::cue::on_screen_at`] for why that is more than one.
    ///
    /// Every one of them is retimed to cover the entire grab, for the reason [`srt_of`]
    /// explains: `-ss` before `-i` resets output timestamps to about zero, so a cue left at
    /// its original time is a coin toss on exactly the boundary frames this page exists to
    /// inspect. They are all on screen at the instant sampled, so a window that holds all of
    /// them for all of it draws exactly what the viewer would see there.
    ///
    /// The cost is that an *animated* line is frozen at whatever phase a window this long
    /// puts it in: a `\t` transform or a `\fad` runs against its own line's start and end,
    /// and those have been rewritten. That was already true of the one cue this used to
    /// stage, and it is the price of a still being a still — the scrub playback is where an
    /// effect is judged, and [`Self::stage_span`] leaves its timings alone.
    fn stage(&self, cues: &[Cue]) -> String {
        self.staged(
            cues.iter()
                .map(|cue| (cue, Duration::ZERO, BURN_WINDOW))
                .collect(),
        )
    }

    /// The same file for a stretch of media rather than an instant, with every cue left at
    /// its own place inside the span.
    ///
    /// What [`Self::stage`] wants is cues that are *always* on screen, because a still is
    /// grabbed at one instant and the burn must not be a coin toss on the rounding. The
    /// scrub playback wants the exact opposite: the span runs from before the cue to after
    /// it, and watching the line appear and disappear against the picture is the whole point
    /// of playing it.
    ///
    /// So each cue keeps its own timing, measured from the start of the span — which is
    /// where `-ss` before `-i` puts the output's timestamps. Relative offsets survive that
    /// subtraction, which is what makes an effect built from a dozen staggered lines play as
    /// the effect rather than as a dozen lines arriving at once. A cue that began before the
    /// span is clamped to its start, since an ASS timestamp cannot be negative; it is on
    /// screen from the first frame, which is what the viewer would see too.
    pub fn stage_span(&self, cues: &[Cue], span_start: Duration) -> String {
        self.staged(
            cues.iter()
                .map(|cue| {
                    (
                        cue,
                        cue.start.saturating_sub(span_start),
                        cue.end.saturating_sub(span_start),
                    )
                })
                .collect(),
        )
    }

    /// The file itself, once every cue has been given the times it should carry in it.
    ///
    /// One function for both, because the two differ only in those times and the header,
    /// numbering and `Format:` line must not drift apart between them.
    fn staged(&self, timed: Vec<(&Cue, Duration, Duration)>) -> String {
        match self {
            Self::SubRip => srt_of(&timed),
            Self::Ass {
                header,
                events_format,
            } => {
                let events: Vec<String> = timed
                    .iter()
                    .flat_map(|(cue, start, end)| {
                        // Every line the cue carries, because a folded cue carries the
                        // several events that draw one line together — see
                        // [`crate::cue::collapse`]. Retimed as a group, so they keep the one
                        // timing they shared before the fold.
                        //
                        // A line that will not re-read is dropped rather than staged as the
                        // cue's stripped text: `parse_ass` cannot produce one, since it
                        // emits a cue only for a line it could read and `retimed_dialogue`
                        // re-reads it by the same rules. Staging SubRip text under an `.ass`
                        // name instead would fail the grab on a good track to guard a case
                        // that does not occur.
                        cue.dialogue
                            .iter()
                            .filter_map(|line| retimed_dialogue(events_format, line, *start, *end))
                            .collect::<Vec<_>>()
                    })
                    .collect();
                format!(
                    "{header}\n\n[Events]\n{events_format}\n{}\n",
                    events.join("\n")
                )
            }
        }
    }
}

impl FrameSource {
    /// The directory this media's frames are cached under. Public because the e2e suite
    /// has to look inside the frame cache to assert what a page did or did not render.
    ///
    /// Depends on nothing per-cue, so it is the same for every frame of a track — which is
    /// what lets the cache evict a track as one thing. See [`crate::framecache`].
    pub fn media_key(&self) -> String {
        framecache::media_key(FrameKeyParts {
            media: &self.media,
            media_length: self.media_length,
            media_modified: self.media_modified,
            pixels: self.pixels,
        })
    }

    /// The subtitle file these cues are burned from, for this track's format and styling.
    pub fn stage(&self, cues: &[Cue]) -> String {
        self.style.stage(cues)
    }

    /// The filename the staged cue must be written under, distinct per renderer.
    pub fn staged_name(&self, slot: CueSlot) -> String {
        slot.file(&self.style)
    }

    /// Both halves of one frame's cache path, for the callers that need them together.
    ///
    /// Stages the cues to key them, because the staged file is what the key covers — see
    /// [`framecache::cue_key`]. That builds a short string per lookup even when the answer
    /// is a cache hit, which is a few kilobytes of transient allocation against an
    /// `ffmpeg` seek, and buys a key that cannot describe a picture it did not produce.
    ///
    /// `cue` is the one the frame is filed under and `on_screen` is everything burned into
    /// it. Both matter: the timing comes from the first, and the picture from all of them —
    /// so two cues sharing a moment and a staged file still get their own frames, and one
    /// cue gets a different frame when the company it keeps changes.
    pub fn key(&self, cue: &Cue, on_screen: &[Cue]) -> (String, String) {
        (
            self.media_key(),
            framecache::cue_key(cue, &self.stage(on_screen)),
        )
    }

    /// The subtitle file for cues that have to come and go inside a span — the scrub
    /// playback's staging. See [`CueStyle::stage_span`].
    pub fn stage_span(&self, cues: &[Cue], span_start: Duration) -> String {
        self.style.stage_span(cues, span_start)
    }
}

/// One cue to draw a video frame for.
///
/// Carries the cue itself rather than a pointer into the page's cue list: the worker
/// writes it back out as a one-cue subtitle file and hashes it into the cache key, and
/// cannot reach `App` to look either up.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrameTarget {
    /// Which cue in the page's list this is, so a frame that arrives after the selection
    /// has moved on can be filed against the cue it actually belongs to.
    pub cue_index: usize,
    pub cue: Cue,
    /// Every cue on screen at [`seek`](Self::seek), this one among them, in file order.
    ///
    /// What gets burned in. The frame is still *filed* under `cue` — this is the picture
    /// that cue sits in, not a list of cues sharing one frame. See
    /// [`crate::cue::on_screen_at`].
    pub on_screen: Vec<Cue>,
    /// Where to grab the frame, already clamped inside the media by the caller.
    pub seek: Duration,
}

/// What to have ready to draw: the cue under the cursor, and the ones around it.
///
/// Two lists rather than one because they are allowed to cost different amounts. The
/// selected cue is what the user is waiting to see, so it is rendered with `ffmpeg` when
/// the cache does not have it; the ones around it are only being got ready in advance, so
/// they are taken from the cache or skipped. Rendering those too would put a second
/// `ffmpeg` per cursor move behind the one the user is actually waiting for — the
/// background pass already renders the whole track, and this only has to encode.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrameRequest {
    pub generation: u64,
    pub source: FrameSource,
    /// The cue under the cursor, unless its frame is already encoded and on hand.
    pub wanted: Option<FrameTarget>,
    /// Cues either side of the cursor, encoded only from the cache.
    pub nearby: Vec<FrameTarget>,
    /// The preview pane, in terminal cells, as the renderer measured it. Only the encode
    /// to a terminal protocol uses this — the picture itself is rendered at
    /// `source.pixels` so that one cached frame serves every pane size.
    pub cells: Size,
}

/// A whole track to render frames for, ahead of the user reaching them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WarmRequest {
    pub generation: u64,
    pub source: FrameSource,
    pub cues: Vec<Cue>,
    /// The media's duration, for holding a seek back from the very end. Zero means it
    /// could not be parsed, and no clamping is applied — see [`seek_for`].
    pub duration: Duration,
    /// How many media files' frames the cache may hold, pruned to before the pass starts.
    pub cache_tracks: usize,
}

/// One cue's span to play: a stretch of video, decoded and rendered ahead of time.
///
/// Nothing here is cached. The frame cache keys on a *cue* and evicts whole tracks, and a
/// playback is a run of frames keyed on time — storing them would fill the cache with
/// hundreds of pictures per cue for a thing the user watches once. See
/// [`crate::framecache`], which this path deliberately never touches.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaybackRequest {
    /// The playback this is, counted separately from the page's own generation: stopping a
    /// playback must not abandon the background pass that is still filling the track in.
    pub generation: u64,
    pub source: FrameSource,
    /// Which cue in the page's list this span is around, so a span that arrives after the
    /// cursor has moved on can be discarded rather than played under the wrong line.
    pub cue_index: usize,
    pub cue: Cue,
    /// Every cue that appears at some point in the span, this one among them, in file order.
    ///
    /// What gets burned in, so the playback shows the stretch of media as a viewer would
    /// see it rather than with everything but one line removed. See
    /// [`crate::cue::on_screen_between`].
    pub on_screen: Vec<Cue>,
    /// Where the span starts in the media, which is also where its timestamps are reset to
    /// by `-ss` — so the cues are staged relative to it.
    pub span_start: Duration,
    pub span_end: Duration,
    /// Frames a second, already reduced to what [`affordable_fps`] allows.
    ///
    /// Frames of *output*, not of media: under a speed other than normal the span is
    /// stretched or squeezed first, so this many frames a second come out for however long
    /// the stretched span lasts.
    pub fps: u32,
    /// How fast the span plays. Applied by the decode itself, to both the picture and the
    /// sound, so nothing downstream has to keep two timelines in step.
    pub speed: PlaybackSpeed,
    /// The exact size each frame is rendered at. Exact, not a bound: the output is raw
    /// `rgb24` sliced by `width * height * 3`, so a picture one row shorter than expected
    /// does not crop the playback, it shears every frame after the first.
    pub pixels: (u32, u32),
    /// The cell area those pixels fill, which is what they are encoded into for drawing.
    pub cells: Size,
    /// The rate and channel count to emit sound at, or `None` when this media has no audio
    /// track to emit.
    ///
    /// The device's own format, so the callback copies rather than resamples. `None` is not
    /// a failure — a video with no sound plays as a silent slideshow at the right rate,
    /// which is the same path a machine with no output device takes.
    pub audio: Option<crate::audio::OutputFormat>,
}

/// How one span's frames are shaped.
///
/// One value rather than three parameters because the three have to agree and are unusually
/// easy to mismatch: `pixels` is derived from `cells` and `picker`'s font, and a mismatch
/// between any two of them is invisible in every assertion short of the drawn picture.
/// Travelling together is what makes them one decision instead of three.
#[derive(Clone, Debug)]
pub struct SpanShape {
    /// The exact pixel size each frame holds. Exact, not a bound: the span is sliced by
    /// `width * height * 3`, so a picture one row short shears every frame after the first.
    ///
    /// Also the picture's true proportions, because [`playback_cells`] and
    /// [`playback_pixels`] apply the cell's aspect ratio in opposite directions and cancel
    /// — which is why re-fitting to a resized pane can ask this and needs no separate
    /// record of the media's own size.
    pub pixels: (u32, u32),
    /// The cell area those pixels fill, which is what they are encoded into for drawing.
    pub cells: Size,
    /// The encoder [`Self::pixels`] was chosen for, so the frames are drawn through the
    /// same picker they were sized for. Carried with the span rather than looked up when a
    /// frame is drawn, which is what makes it impossible for the two to disagree — and what
    /// removes the question of what to do with a span that arrived without one, since a
    /// terminal with no protocol never starts a playback in the first place.
    pub picker: Picker,
}

/// A span's worth of raw video, and everything needed to read a frame out of it.
///
/// One buffer rather than a `Vec` of frames: `ffmpeg` writes them back to back on stdout
/// already, so slicing is arithmetic and the whole span is one allocation.
#[derive(Clone)]
pub struct PlaybackFrames {
    /// Every frame's `rgb24` pixels, back to back. `Arc` because the page hands it to the
    /// encoder on every step and must not copy tens of megabytes to do so.
    bytes: Arc<Vec<u8>>,
    pub pixels: (u32, u32),
    pub cells: Size,
    pub fps: u32,
    pub picker: Picker,
    /// How fast the span was decoded to play. Frames are `1 / fps` apart in *output* time,
    /// so this is the factor that turns a playhead into a position in the media — see
    /// `sync::Playback::position`.
    pub speed: PlaybackSpeed,
    /// Where in the media frame zero sits, for the playhead the timeline draws.
    pub span_start: Duration,
    /// The span's sound as interleaved floats, at whatever the device asked for. Empty for
    /// media with no audio track, which plays as a silent slideshow rather than not at all.
    samples: Vec<f32>,
}

impl PlaybackFrames {
    pub fn new(
        bytes: Vec<u8>,
        shape: SpanShape,
        fps: u32,
        speed: PlaybackSpeed,
        span_start: Duration,
        samples: Vec<f32>,
    ) -> Self {
        let SpanShape {
            pixels,
            cells,
            picker,
        } = shape;
        Self {
            bytes: Arc::new(bytes),
            pixels,
            cells,
            fps,
            picker,
            speed,
            span_start,
            samples,
        }
    }

    /// Takes the span's sound, leaving the frames behind.
    ///
    /// Taken rather than borrowed because it is handed to the audio device, which owns what
    /// it plays for the life of the stream — and because a span's samples are megabytes
    /// that nothing else ever looks at again.
    ///
    /// Shared rather than owned outright, because a looping playback opens a fresh device
    /// each time round and every repeat would otherwise copy the whole buffer.
    pub fn take_samples(&mut self) -> Arc<Vec<f32>> {
        Arc::new(std::mem::take(&mut self.samples))
    }

    /// How many bytes one frame occupies. Zero for a degenerate size, which is what makes
    /// [`Self::count`] answer zero rather than divide by it.
    pub fn stride(&self) -> usize {
        let (width, height) = self.pixels;
        (width as usize) * (height as usize) * 3
    }

    pub fn count(&self) -> usize {
        match self.stride() {
            0 => 0,
            stride => self.bytes.len() / stride,
        }
    }

    /// One frame's pixels, or `None` past the end of the span.
    pub fn frame(&self, index: usize) -> Option<&[u8]> {
        let stride = self.stride();
        if stride == 0 {
            // Otherwise every index answers with an empty slice, which reads downstream as
            // a frame that exists and encodes to a picture of nothing.
            return None;
        }
        let start = index.checked_mul(stride)?;
        self.bytes.get(start..start.checked_add(stride)?)
    }

    /// How long the whole span lasts, as the frames themselves measure it.
    ///
    /// From the frame count rather than from the request, because `ffmpeg` decides how many
    /// frames a span actually yields — a seek landing between keyframes, or a source
    /// shorter than the span asked for, both come back with fewer.
    pub fn duration(&self) -> Duration {
        if self.fps == 0 {
            return Duration::ZERO;
        }
        Duration::from_secs_f64(self.count() as f64 / f64::from(self.fps))
    }
}

/// Prints what a reader wants — how much video, at what size — rather than megabytes of
/// pixels, which is what a derived `Debug` would put in every test failure message.
impl std::fmt::Debug for PlaybackFrames {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PlaybackFrames")
            .field("frames", &self.count())
            .field("pixels", &self.pixels)
            .field("cells", &self.cells)
            .field("fps", &self.fps)
            .field("span_start", &self.span_start)
            .field("samples", &self.samples.len())
            .finish()
    }
}

/// A span ready to play, or why there is none.
#[derive(Debug)]
pub enum PlaybackOutcome {
    Ready(PlaybackFrames),
    Failed(String),
}

/// A frame ready to draw, or why there is none.
///
/// `Failed` is not an error dialog: the page leaves the preview pane empty and records the
/// reason on the state, for the cue it was reported against.
pub enum FrameOutcome {
    Ready(Box<Protocol>),
    Failed(String),
}

/// `Protocol` holds encoded image data and has no `Debug`, so this reports what a reader
/// of a log actually wants — which protocol, and how big — rather than nothing at all.
impl std::fmt::Debug for FrameOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ready(protocol) => formatter
                .debug_tuple("Ready")
                .field(&protocol.size())
                .finish(),
            Self::Failed(message) => formatter.debug_tuple("Failed").field(message).finish(),
        }
    }
}

/// Anything any of the workers has to say about the open page.
///
/// One channel for all three, so the event loop keeps a single drain and cannot end up
/// pumping one worker's results and not another's.
#[derive(Debug)]
pub enum PreviewEvent {
    /// The track's cues, or why they could not be read.
    Prepared {
        generation: u64,
        outcome: PrepareOutcome,
    },
    /// A frame for one cue, or why there is none.
    Frame {
        generation: u64,
        cue_index: usize,
        outcome: FrameOutcome,
    },
    /// How far the background pass has got.
    ///
    /// `done` counts cues the pass is finished with, which includes ones it cached, ones
    /// it found already cached, and ones it could not draw — a cue is "done" when it is no
    /// longer outstanding, not when it succeeded. `done == total` therefore means the pass
    /// is over however it ended, which is exactly what the status line needs to know.
    Warming {
        generation: u64,
        done: usize,
        total: usize,
    },
    /// A cue's span, decoded and ready to play, or why there is none.
    Playback {
        generation: u64,
        cue_index: usize,
        outcome: PlaybackOutcome,
    },
}

/// The UI thread's half of the preview workers.
///
/// Wraps the request channels together with the generation cell rather than handing `App`
/// each separately, because they must move together: sending a request *is* what makes its
/// generation the live one, and every other write to the cell means "abandon whatever is
/// running".
#[derive(Debug)]
pub struct PreviewHandles {
    prepare_tx: Sender<PrepareRequest>,
    /// `None` when the terminal offered no image protocol, which is also what tests get.
    /// The page then stays on its text preview, the same fallback a build without libass
    /// takes — and nothing is rendered in the background either, since there would be
    /// nothing to draw the result with.
    frame_tx: Option<Sender<FrameRequest>>,
    warm_tx: Option<Sender<WarmRequest>>,
    playback_tx: Option<Sender<PlaybackRequest>>,
    /// The page generation every worker should still be working for, shared with them.
    ///
    /// An `AtomicU64` rather than a cancellation flag because it answers both questions
    /// a worker has — "is this request still wanted" and "should I stop now" — with one
    /// comparison, and because the value is already unique per page opening.
    live_generation: Arc<AtomicU64>,
    /// The playback the worker should still be decoding for, counted apart from
    /// `live_generation`.
    ///
    /// Its own cell rather than a share of the page's, because the two are stopped by
    /// different things: pressing `p` again abandons the span, and the background pass that
    /// is still filling the track in has nothing to do with that decision.
    live_playback: Arc<AtomicU64>,
    /// Roughly what one encoded frame costs per cell, from the picker's protocol and font
    /// size. See [`frame_bytes_per_cell`].
    frame_cost: u64,
    /// A copy of the picker the frame worker took, kept because a *playback* is encoded on
    /// the UI thread rather than by a worker: the frames arrive as raw pixels and are drawn
    /// straight from the buffer as the audio clock reaches them, so there is no round trip
    /// to a thread that could hold the picker instead. `None` for the same reason
    /// [`Self::frame_tx`] is — no protocol, nothing to encode for.
    picker: Option<Picker>,
}

impl PreviewHandles {
    /// Asks for a track's cues, making this the generation the workers work for.
    pub fn request(&self, request: PrepareRequest) {
        self.live_generation
            .store(request.generation, Ordering::Relaxed);
        // A dead worker leaves the page on its loader, which is the same thing that
        // happens if the extraction never finishes; there is nothing better to do here.
        let _ = self.prepare_tx.send(request);
    }

    /// Asks for the frame at one cue. Silently does nothing without a frame worker.
    pub fn request_frame(&self, request: FrameRequest) {
        if let Some(frame_tx) = self.frame_tx.as_ref() {
            let _ = frame_tx.send(request);
        }
    }

    /// Asks for every cue in a track to be rendered in the background.
    pub fn request_warm(&self, request: WarmRequest) {
        if let Some(warm_tx) = self.warm_tx.as_ref() {
            let _ = warm_tx.send(request);
        }
    }

    /// Asks for one cue's span to be decoded, making this the playback the worker decodes
    /// for. Silently does nothing without a frame worker, for the same reason a frame
    /// request does.
    pub fn request_playback(&self, request: PlaybackRequest) {
        self.live_playback
            .store(request.generation, Ordering::Relaxed);
        if let Some(playback_tx) = self.playback_tx.as_ref() {
            let _ = playback_tx.send(request);
        }
    }

    /// Tells the worker the span it is decoding is no longer wanted, so a running `ffmpeg`
    /// is killed rather than left decoding seconds of video for a playback that has been
    /// stopped.
    pub fn abandon_playback(&self, generation: u64) {
        self.live_playback.store(generation, Ordering::Relaxed);
    }

    /// Whether frames are being produced at all, so the page can skip asking for one it
    /// would only have to fall back from.
    pub fn draws_frames(&self) -> bool {
        self.frame_tx.is_some()
    }

    /// Roughly how many bytes one encoded frame costs per cell of the pane it was drawn
    /// for.
    ///
    /// Read off the picker when the workers were spawned, since the picker itself moves
    /// into the frame worker and cannot be consulted afterwards. Zero without one: nothing
    /// is encoded, so nothing is held.
    pub fn frame_bytes_per_cell(&self) -> u64 {
        self.frame_cost
    }

    /// The picker a playback should encode its frames with, if the terminal has one.
    pub fn picker(&self) -> Option<&Picker> {
        self.picker.as_ref()
    }

    /// Tells the workers that the page they were working for is gone, so a running
    /// `ffmpeg` is killed rather than left demuxing a file nobody is looking at.
    pub fn abandon(&self, generation: u64) {
        self.live_generation.store(generation, Ordering::Relaxed);
    }
}

/// Roughly what one encoded frame costs per cell of the pane it was drawn for.
///
/// Read out of `ratatui-image`'s protocol implementations rather than guessed, because the
/// spread between them is fourfold and a single figure would be wrong at one end or the
/// other:
///
/// - **Kitty** keeps the transmit string, which is the frame's pixels as RGBA base64:
///   four bytes a pixel and a third again for the encoding. That is where the megabytes
///   are, and the font size is what turns cells into pixels.
/// - **iTerm2** keeps base64 PNG, so the same bound with compression on top of it; taken
///   uncompressed because how well a video frame packs is not knowable here and a budget
///   that under-counts is not a budget.
/// - **Sixel** keeps its own encoding, around a byte a pixel and no alpha.
///
/// Halfblocks is not listed because it never arrives: [`drawing_picker`] refuses that
/// terminal before the workers are spawned. It falls through the same arm as kitty rather
/// than carrying its own twelve-bytes-a-cell branch, which keeps the match total without a
/// case nothing can take.
fn frame_bytes_per_cell(protocol: ProtocolType, font: FontSize) -> u64 {
    let pixels = u64::from(font.width) * u64::from(font.height);
    match protocol {
        ProtocolType::Sixel => pixels,
        _ => pixels * 4 * 4 / 3,
    }
}

/// Starts the page's background workers: one that reads a track's cues, and — when the
/// terminal can draw images at all — one that renders the frame at the selected cue along
/// with the cues around it, and one that renders the rest of the track behind them.
///
/// Three threads rather than one because they are asked for different things at different
/// rates. A single thread would make a held-down `j` queue behind an extraction that is
/// demuxing a container, or behind a thousand-cue background pass.
pub fn spawn_preview_workers(picker: Option<Picker>) -> (PreviewHandles, Receiver<PreviewEvent>) {
    let (prepare_tx, prepare_rx) = mpsc::channel::<PrepareRequest>();
    let (event_tx, event_rx) = mpsc::channel();
    let live_generation = Arc::new(AtomicU64::new(0));
    let live_playback = Arc::new(AtomicU64::new(0));
    // Taken before the picker moves into the frame worker below, which is the last chance
    // to ask it anything. Both are `Copy` and neither changes for the run.
    let frame_cost = picker
        .as_ref()
        .map(|picker| frame_bytes_per_cell(picker.protocol_type(), picker.font_size()))
        .unwrap_or_default();
    // Cloned for the same reason: the playback encodes on the UI thread and needs one too.
    let ui_picker = picker.clone();
    let prepare_generation = Arc::clone(&live_generation);
    let prepare_events = event_tx.clone();

    std::thread::spawn(move || {
        // FIFO rather than coalescing like `spawn_probe_worker`: requests arrive one per
        // page opening, not one per cursor movement, so there is no burst to collapse —
        // and a superseded request is already cheap, since `prepare` drops it before
        // spawning anything.
        while let Ok(request) = prepare_rx.recv() {
            let Some(outcome) = prepare(&request, &prepare_generation) else {
                continue;
            };
            if prepare_events
                .send(PreviewEvent::Prepared {
                    generation: request.generation,
                    outcome,
                })
                .is_err()
            {
                break;
            }
        }
    });

    // The image workers exist only when there is something that could draw their output.
    let (frame_tx, warm_tx, playback_tx) = match picker {
        Some(picker) => {
            // Cloned before the warm thread below moves the original: the playback worker
            // reports on the same channel as the other three, so the event loop keeps a
            // single drain and cannot end up pumping one worker's results and not another's.
            let playback_events = event_tx.clone();
            let (playback_tx, playback_rx) = mpsc::channel::<PlaybackRequest>();
            let playback_generation = Arc::clone(&live_playback);
            // The span carries the encoder it was sized for, and this is where that becomes
            // impossible to get wrong: a playback worker exists only inside this arm, so
            // there is no such thing as a decoded span without a picker to draw it with.
            let playback_picker = picker.clone();
            std::thread::spawn(move || {
                while let Ok(request) = playback_rx.recv() {
                    // Coalescing, like the others: a span takes a second or two to decode
                    // and only the newest press of `p` is one anybody is waiting on.
                    let request = newest(request, &playback_rx);
                    if !playback(
                        &request,
                        &playback_picker,
                        &playback_generation,
                        &playback_events,
                    ) {
                        break;
                    }
                }
            });

            let (frame_tx, frame_rx) = mpsc::channel::<FrameRequest>();
            let frame_generation = Arc::clone(&live_generation);
            let frame_events = event_tx.clone();
            std::thread::spawn(move || {
                while let Ok(request) = frame_rx.recv() {
                    let request = newest_frame(request, &frame_rx);
                    if !frame(&request, &picker, &frame_generation, &frame_events) {
                        break;
                    }
                }
            });

            let (warm_tx, warm_rx) = mpsc::channel::<WarmRequest>();
            let warm_generation = Arc::clone(&live_generation);
            std::thread::spawn(move || {
                while let Ok(request) = warm_rx.recv() {
                    // Coalescing for the same reason frames do: a request is one whole
                    // track, and only the page that is actually open is worth walking.
                    let request = newest(request, &warm_rx);
                    if !warm(&request, &warm_generation, &event_tx) {
                        break;
                    }
                }
            });
            (Some(frame_tx), Some(warm_tx), Some(playback_tx))
        }
        None => (None, None, None),
    };

    (
        PreviewHandles {
            prepare_tx,
            frame_tx,
            warm_tx,
            playback_tx,
            live_generation,
            live_playback,
            frame_cost,
            picker: ui_picker,
        },
        event_rx,
    )
}

/// Discards everything queued behind `request` in favour of the last of it.
///
/// Frames coalesce where cue extractions do not: walking the cue list produces a request
/// per cue, and every one but the last is for a frame the user has already scrolled past.
/// Rendering them in turn would leave the preview several selections behind the cursor,
/// each one an `ffmpeg` seek nobody is waiting for any more.
fn newest<T>(request: T, receiver: &Receiver<T>) -> T {
    let mut request = request;
    while let Ok(newer) = receiver.try_recv() {
        request = newer;
    }
    request
}

/// [`newest`] for frame requests, which must not drop a grab on the floor.
///
/// A plain coalesce loses the selected cue's frame outright, and nothing asks for it again:
/// the page clears its request the moment it hands one over, so a `wanted` discarded here is
/// a preview pane that stays empty until the cursor happens to move. Two quick presses of
/// `l` across an overlap group reproduce it — the answers to the first request set
/// `refill_nearby`, whose dispatch carries no `wanted` of its own and lands behind the
/// second press's grab.
///
/// So the newest request's `wanted` falls back to the newest discarded one's. That is only
/// ever reached when the newest carries none, which is precisely the case where the page
/// believes the grab it made earlier is still on its way — so the cue adopted is the one
/// under the cursor, not a stale one. A move to another cue always produces a `wanted` of
/// its own ([`crate::sync::SubtitleSyncState::request_frame`]), which wins here.
///
/// Generations must match, or a request for a page the user has left could resurrect a grab
/// the page it belongs to would only discard.
fn newest_frame(request: FrameRequest, receiver: &Receiver<FrameRequest>) -> FrameRequest {
    let mut request = request;
    while let Ok(mut newer) = receiver.try_recv() {
        if newer.wanted.is_none() && newer.generation == request.generation {
            newer.wanted = request.wanted.take();
        }
        request = newer;
    }
    request
}

/// The size to render a frame at, given the source's own resolution.
///
/// Shrunk to fit inside [`MAX_FRAME_PIXELS`], never enlarged: upscaling a 320×240 clip to
/// 960×720 spends cache and decode time inventing pixels the terminal then throws away
/// again. An unknown resolution — a container ffprobe could not measure — takes the cap,
/// since `force_original_aspect_ratio=decrease` makes it a bound rather than a shape.
///
/// Both sides are rounded down to an even number: an odd dimension is legal for PNG but
/// makes the chroma-subsampled decode `ffmpeg` does on the way there a rounding case for
/// no benefit.
pub fn target_pixels(source: Option<(u32, u32)>) -> (u32, u32) {
    let (cap_width, cap_height) = MAX_FRAME_PIXELS;
    let Some((width, height)) = source.filter(|(width, height)| *width > 0 && *height > 0) else {
        return MAX_FRAME_PIXELS;
    };
    if width <= cap_width && height <= cap_height {
        return (even(width), even(height));
    }
    // Whichever axis overflows by more decides the scale, so the result fits both.
    let scaled_height = height * cap_width / width;
    if scaled_height <= cap_height {
        (even(cap_width), even(scaled_height))
    } else {
        (even(width * cap_height / height), even(cap_height))
    }
}

fn even(value: u32) -> u32 {
    (value & !1).max(2)
}

/// How much raw video one scrub playback may hold.
///
/// The span is decoded in full before a single frame is shown — that is what buys a
/// slideshow that never stutters — so this is real resident memory, and it scales with
/// three things at once: how long the cue is, how large the terminal is, and the frame
/// rate. The first two are the user's, so the third is what gives. See [`affordable_fps`].
///
/// Raised from 96 MB when playbacks started being decoded at the terminal's own pixel
/// resolution rather than at a halfblock grid ([`playback_pixels`]), which multiplied a
/// frame's cost by roughly sixty-five. The old figure would have answered that with five
/// frames a second on any full-screen terminal. That is the trade being made deliberately:
/// a readable subtitle at a lower rate answers the question this page exists for, and a
/// smooth unreadable one does not — but a third of a gigabyte, held for the second or two a
/// span plays and released with it, is a fair price for the rate not collapsing too.
pub const PLAYBACK_BUDGET: u64 = 384 * 1024 * 1024;

/// The slowest a playback is allowed to get before the budget stops being honoured.
///
/// Below about this, consecutive frames stop reading as motion and the thing being judged —
/// whether a line lands with the speech — becomes harder to see rather than easier, which
/// is the opposite of the point. A span that cannot fit even at this rate overshoots the
/// budget instead, because a playback that is over budget is recoverable and one that
/// cannot be watched is not.
pub const MIN_PLAYBACK_FPS: u32 = 5;

/// How fast a scrub playback runs, as a percentage of real time.
///
/// Percent in a `u16` rather than a fraction in an `f64` for two reasons. `PreviewSettings`
/// — where the session's chosen speed lives — derives `Eq`, which a float field forbids;
/// and the speeds on offer are a fixed list rather than a range, because the question the
/// page answers is whether a line lands with the speech, and half speed answers it where
/// 0.47 would only be harder to say out loud.
///
/// Applied entirely inside `ffmpeg` — `setpts` on the picture, `atempo` on the sound — so
/// the two come out of one decode already stretched together and the sound remains the
/// clock. See [`playback_command`]; `crate::audio` never learns that speeds exist.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PlaybackSpeed(u16);

impl PlaybackSpeed {
    /// Real time, and the speed a playback runs at unless the user says otherwise. At
    /// exactly this speed no filter is emitted at all, so the default command is the one
    /// this feature was built on top of, byte for byte.
    pub const NORMAL: Self = Self(100);

    /// The ends of the range, named because the rest of this module reasons about them:
    /// [`atempo_chain`] chains filters because of the first and needs no upward half because
    /// of the second.
    ///
    /// A quarter is the floor because below it the picture stops reading as motion at any
    /// frame rate; double is the ceiling because speech past it is no longer something a
    /// line can be judged against.
    pub const SLOWEST: Self = Self(25);
    pub const FASTEST: Self = Self(200);

    /// Half speed — the archetypal use of this feature, and the one to reach for when a line
    /// looks like it might be a syllable out.
    pub const HALF: Self = Self(50);

    /// Every speed the preview-settings popup offers, **fastest first**.
    ///
    /// Descending because the popup lists them descending, and one order for both is what
    /// stops the list and the cursor disagreeing about which end is which.
    pub const STEPS: [Self; 7] = [
        Self::FASTEST,
        Self(150),
        Self(125),
        Self::NORMAL,
        Self(75),
        Self::HALF,
        Self::SLOWEST,
    ];

    /// The multiplier `ffmpeg` is given, and what the page multiplies output time by to get
    /// back to media time.
    pub fn as_f64(self) -> f64 {
        f64::from(self.0) / 100.0
    }

    pub fn is_normal(self) -> bool {
        self == Self::NORMAL
    }
}

impl Default for PlaybackSpeed {
    fn default() -> Self {
        Self::NORMAL
    }
}

/// `1x`, `0.5x`, `1.25x` — trailing zeros trimmed, because a settings row reads as a value
/// rather than as a measurement.
impl std::fmt::Display for PlaybackSpeed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let whole = self.0 / 100;
        match self.0 % 100 {
            0 => write!(formatter, "{whole}x"),
            tenths if tenths % 10 == 0 => write!(formatter, "{whole}.{}x", tenths / 10),
            hundredths => write!(formatter, "{whole}.{hundredths:02}x"),
        }
    }
}

/// The frame rate worth asking a source for, never above `fps`.
///
/// **The `fps` filter duplicates frames to reach a rate the source does not hold.** Asking a
/// 24 fps film for 60 does not make it smoother; it decodes 60 frames a second of which 36
/// are copies, and the span costs two and a half times the memory for exactly the same
/// picture. Since a playback holds its whole span decoded, that waste comes straight out of
/// [`PLAYBACK_BUDGET`] and pushes [`affordable_fps`] into lowering a rate that was never
/// buying anything.
///
/// **Scaled by the speed, because that is how many source frames a second of *output*
/// covers.** At half speed one output second spans half a second of media, so a 24 fps
/// source has twelve distinct frames to give it; at double speed, forty-eight. Dropping the
/// factor would cap a slowed playback at the source's own rate and leave it asking for twice
/// the frames it can show — which is the case where the budget is already tightest.
///
/// Rounded up, so a 23.976 fps source is asked for 24 rather than 23. `None` means ffprobe
/// would not say what the source holds, and then nothing is capped: guessing a rate here
/// would make a playback choppier for a file we simply failed to read.
pub fn source_capped_fps(fps: u32, source: Option<f64>, speed: PlaybackSpeed) -> u32 {
    fps.min(source_frame_ceiling(source, speed).unwrap_or(fps))
}

/// The most frames a second this source can put into a second of output, or `None` when
/// ffprobe would not say what it holds.
///
/// Split from [`source_capped_fps`] because the popup needs the ceiling itself: a dropdown
/// that offers a 23.976 fps track sixty frames a second is offering a rate that cannot
/// happen, and there is no way for a reader to tell that from the row.
pub fn source_frame_ceiling(source: Option<f64>, speed: PlaybackSpeed) -> Option<u32> {
    let source = source.filter(|rate| rate.is_finite() && *rate > 0.0)?;
    let distinct = (source * speed.as_f64()).ceil();
    // Through `u32::try_from` rather than `as`, which saturates silently: a nonsense rate out
    // of a malformed file should cap nothing, not wrap to one frame a second.
    let distinct = u32::try_from(distinct as u64).ok()?;
    // Floored at one, so a source of stills still yields a frame rather than nothing.
    Some(distinct.max(1))
}

/// The frame rate this span can afford at this size, never above `fps`.
///
/// The same shape as `SubtitleSyncState::window_radius`: a ceiling the user asked for,
/// lowered to fit a budget, floored at the point where lowering it further defeats the
/// feature.
///
/// `speed` is not a detail the budget can ignore. `setpts` stretches a span to
/// `span / speed`, so a half-speed playback holds twice the frames of the same stretch of
/// media and costs twice the memory. Charging the media span rather than the stretched one
/// would let a slow playback quietly overshoot [`PLAYBACK_BUDGET`] by the speed factor.
pub fn affordable_fps(fps: u32, span: Duration, speed: PlaybackSpeed, pixels: (u32, u32)) -> u32 {
    let (width, height) = pixels;
    let stride = u64::from(width) * u64::from(height) * 3;
    // Rounded up, so a span of a few milliseconds is still charged for the frame it holds
    // rather than dividing by nothing.
    let seconds = span
        .div_f64(speed.as_f64())
        .as_millis()
        .div_ceil(1000)
        .max(1) as u64;
    let per_second = stride.saturating_mul(seconds);
    if per_second == 0 {
        return fps;
    }
    let affordable = PLAYBACK_BUDGET / per_second;
    // Floored first, then capped: this only ever *lowers* a rate. Raising a user who asked
    // for five to the floor would be answering a question they did not ask, and the floor
    // exists to stop the budget making a playback unwatchable, not to enforce a minimum.
    u32::try_from(affordable)
        .unwrap_or(fps)
        .max(MIN_PLAYBACK_FPS)
        .min(fps)
}

/// The stretch of media a playback covers: `pad` either side of the cue.
///
/// Clamped to the media's own length at the end, for the reason [`seek_for`] clamps a seek
/// — and only when the length is actually known, since a duration that would not parse
/// arrives here as zero and clamping against that would play nothing at all.
pub fn playback_span(cue: &Cue, pad: Duration, duration: Duration) -> (Duration, Duration) {
    let start = cue.start.saturating_sub(pad);
    let end = cue.end.saturating_add(pad);
    let end = if duration.is_zero() {
        end
    } else {
        end.min(duration)
    };
    (start, end.max(start))
}

/// The cell area a playback fills inside `pane`, keeping the source's proportions.
///
/// Worked out here rather than left to `Resize::Scale`, because a playback is decoded at
/// exactly the size it is drawn at: `ffmpeg` is told literal pixels, so this is what
/// decides them. Getting it wrong does not merely mislay a row — it decodes a picture of
/// the wrong shape and then asks the encoder to letterbox it, spending memory and frame
/// rate on bars.
///
/// The source's aspect ratio, corrected by the cell's: a cell is roughly twice as tall as
/// it is wide, so a 16:9 picture is nothing like 16:9 *in cells*. Taking that correction
/// from the font rather than from a constant two is what makes [`playback_pixels`]'s answer
/// come back to the source's own proportions exactly — the two factors cancel, and a cell
/// that is 8x17 rather than a tidy 10x20 no longer stretches the picture by a seventeenth.
pub fn playback_cells(pane: Size, source: (u32, u32), font: FontSize) -> Size {
    let (source_width, source_height) = source;
    let (width, height) = (u32::from(pane.width), u32::from(pane.height));
    let (cell_width, cell_height) = (u32::from(font.width).max(1), u32::from(font.height).max(1));
    if width == 0 || height == 0 || source_width == 0 || source_height == 0 {
        return Size::new(0, 0);
    }
    // Widest first: a preview pane is usually wider than it is tall in pixels, so the
    // height is what runs out.
    let fitted = (width * source_height * cell_width / (source_width * cell_height)).max(1);
    if fitted <= height {
        Size::new(pane.width, fitted as u16)
    } else {
        let fitted =
            (height * cell_height * source_width / (source_height * cell_width)).clamp(1, width);
        Size::new(fitted as u16, pane.height)
    }
}

/// The pixel size a cell area's frames are rendered at, for [`playback_cells`]'s answer.
///
/// Exactly the terminal's own cell size per cell, so the playback is decoded at the
/// resolution it will be drawn at: nothing is resampled on the way, and a burned-in
/// subtitle is as sharp as the source allows. That is what makes the line *readable* rather
/// than merely present, and it is why the figure comes off the picker rather than out of a
/// constant.
///
/// **This is where a scrub playback's whole cost comes from.** A cell is upwards of 130
/// pixels, and a playback holds its entire span decoded — see [`PLAYBACK_BUDGET`] and
/// [`affordable_fps`], which is what gives when the span will not fit.
pub fn playback_pixels(cells: Size, font: FontSize) -> (u32, u32) {
    (
        u32::from(cells.width) * u32::from(font.width).max(1),
        u32::from(cells.height) * u32::from(font.height).max(1),
    )
}

/// The picker previews should be drawn with, or `None` when this terminal cannot draw one.
///
/// Halfblocks is not an image protocol, it is two coloured half-cells — and the timing
/// page's whole job is judging a subtitle against the picture it is burned into, which
/// needs the picture to be legible. `Picker::from_query_stdio` reports halfblocks for every
/// terminal that answered nothing, so without this the page would open, render, cache and
/// play for terminals that can only ever show a coloured smear. Refusing here means the
/// page says [`crate::sync::PreviewSupport::NoImageProtocol`] once and stays quiet, which
/// is the honest answer.
///
/// Taken at startup rather than checked per frame: the protocol cannot change under a
/// running process, and one `None` here switches off the workers, the cache pass and the
/// playback together.
pub fn drawing_picker(picker: Picker) -> Option<Picker> {
    (picker.protocol_type() != ProtocolType::Halfblocks).then_some(picker)
}

/// Where in the media to grab the frame for `cue`.
///
/// The moment the cue comes in. That instant is what the reader is judging on this page —
/// a line that appears a beat late appears late *here*, and nowhere else in the cue's
/// span — so the still has to be the frame the line arrives on. The midpoint stood here
/// first and answered a different question: it showed a frame the line is comfortably on
/// top of, which says nothing about whether it came in with the shot.
///
/// Held back from the very end: seeking to the last instant of a file lands past the final
/// frame, and `-frames:v 1` then writes nothing at all. Only when the duration is actually
/// known, though — a file whose duration would not parse arrives here as zero, and
/// clamping against that would preview the first frame of the media for every cue.
///
/// Grabbing exactly at the start is only safe because [`CueStyle::stage`] stretches every
/// cue in the group across [`BURN_WINDOW`]: at the cue's own timing, a seek landing a
/// rounding error before its first frame would draw nothing at all — precisely the frame
/// this is aiming at.
pub fn seek_for(cue: &Cue, duration: Duration) -> Duration {
    if duration.is_zero() {
        cue.start
    } else {
        cue.start
            .min(duration.saturating_sub(Duration::from_millis(200)))
    }
}

/// Handles with no worker thread behind them, so `App`'s dispatch can be asserted from
/// the request itself instead of through a real extraction.
#[cfg(test)]
pub(crate) struct TestHandles {
    pub handles: PreviewHandles,
    pub prepare_rx: Receiver<PrepareRequest>,
    pub frame_rx: Receiver<FrameRequest>,
    pub warm_rx: Receiver<WarmRequest>,
    pub playback_rx: Receiver<PlaybackRequest>,
    pub live_generation: Arc<AtomicU64>,
    pub live_playback: Arc<AtomicU64>,
}

#[cfg(test)]
pub(crate) fn test_handles() -> TestHandles {
    let (prepare_tx, prepare_rx) = mpsc::channel();
    let (frame_tx, frame_rx) = mpsc::channel();
    let (warm_tx, warm_rx) = mpsc::channel();
    let (playback_tx, playback_rx) = mpsc::channel();
    let live_generation = Arc::new(AtomicU64::new(0));
    let live_playback = Arc::new(AtomicU64::new(0));
    TestHandles {
        handles: PreviewHandles {
            prepare_tx,
            frame_tx: Some(frame_tx),
            warm_tx: Some(warm_tx),
            playback_tx: Some(playback_tx),
            live_generation: Arc::clone(&live_generation),
            live_playback: Arc::clone(&live_playback),
            // What `Picker::halfblocks()` would report, which is what the tests that
            // actually encode a protocol use.
            frame_cost: frame_bytes_per_cell(ProtocolType::Halfblocks, FontSize::new(10, 20)),
            picker: Some(Picker::halfblocks()),
        },
        prepare_rx,
        frame_rx,
        warm_rx,
        playback_rx,
        live_generation,
        live_playback,
    }
}

/// Reads one track's cues, or `None` if the page it was for closed on the way.
///
/// Every format ends at the same place — a file this crate's own parsers read — and
/// differs only in how it gets there:
///
/// - **SubRip and ASS** are copied out of the container bit for bit, or read where they
///   already lie. For ASS that is not a preference: a transcode to SubRip discards the
///   styles, the `PlayRes`, and every positioning override, which is most of what an ASS
///   track is and all of what makes previewing one worth doing.
/// - **WebVTT and MOV Text** are transcoded to SubRip: on the way out of the container for
///   an embedded track, in a second pass for a sidecar. Neither carries styling that
///   survives anywhere, so there is nothing to lose by it.
///
/// Anything else was refused before the page opened, by
/// [`ToolCapabilities::preview_blocked`].
///
/// [`ToolCapabilities::preview_blocked`]: crate::subtitle::ToolCapabilities::preview_blocked
fn prepare(request: &PrepareRequest, live_generation: &AtomicU64) -> Option<PrepareOutcome> {
    let abandoned = || live_generation.load(Ordering::Relaxed) != request.generation;
    if abandoned() {
        return None;
    }
    // The source as a file to read from: pulled out of the container, or the sidecar
    // exactly where it lies. A sidecar is never copied — the common case is one that needs
    // no conversion at all, and one fewer file written is one fewer thing to fail.
    let source = match request.stream_index {
        Some(index) => {
            // What comes *out*, which is not always what went in. `ffmpeg` picks its muxer
            // from the extension, so this name and `extract_command`'s `-c:s` have to
            // agree or the muxer refuses the codec — a disagreement neither half shows on
            // its own.
            let staged = request
                .workspace
                .join(cues_file(extracted_format(request.format)));
            let mut command = extract_command(&request.input, index, request.format, &staged);
            match run_cancellable(&mut command, &abandoned) {
                RunOutcome::Abandoned => return None,
                RunOutcome::Failed(message) => return Some(PrepareOutcome::Failed(message)),
                RunOutcome::Finished(output) if !output.status.success() => {
                    return Some(PrepareOutcome::Failed(command_failure(
                        "Could not read this subtitle track",
                        &output.stderr,
                    )));
                }
                RunOutcome::Finished(_) => staged,
            }
        }
        None => request.input.clone(),
    };
    if abandoned() {
        return None;
    }
    converted(&source, request, &abandoned)
}

/// What a track's format becomes once it has been pulled out of its container.
///
/// One answer shared by the staged filename and the extraction's `-c:s`, so the two cannot
/// disagree about what is being written.
fn extracted_format(format: SubtitleFormat) -> SubtitleFormat {
    match format {
        // Copied unchanged, because this crate reads them itself.
        SubtitleFormat::SubRip | SubtitleFormat::Ass => format,
        _ => SubtitleFormat::SubRip,
    }
}

/// The name a track is staged under inside the page's workspace.
fn cues_file(format: SubtitleFormat) -> String {
    format!("cues.{}", format.extension())
}

/// Turns the extracted or sidecar file into cues, converting it first when it is not
/// already something this crate can read.
fn converted(
    source: &Path,
    request: &PrepareRequest,
    abandoned: Abandoned<'_>,
) -> Option<PrepareOutcome> {
    match request.format {
        SubtitleFormat::SubRip => Some(read_cues(source)),
        SubtitleFormat::Ass => Some(read_ass_cues(source)),
        // An embedded track was transcoded on the way out already; a sidecar has had
        // nothing done to it and still needs the pass.
        SubtitleFormat::WebVtt | SubtitleFormat::MovText => {
            if request.stream_index.is_some() {
                return Some(read_cues(source));
            }
            let staged = request.workspace.join(CUES_FILE);
            let mut command = transcode_command(source, &staged);
            extracted(run_cancellable(&mut command, abandoned), &staged)
        }
        // Refused before the page opened, so this is unreachable from the application and
        // exists to keep the match total.
        other => Some(PrepareOutcome::Failed(format!(
            "{} subtitle previewing is not implemented yet.",
            other.overview_label()
        ))),
    }
}

/// What an extraction run means for the page.
///
/// Separated from `prepare` so each ending — gave up, could not start, `ffmpeg` refused,
/// wrote a file — can be asserted without having to arrange the real subprocess into
/// that state first, which for the first two is not something a test can do reliably.
fn extracted(run: RunOutcome, staged: &Path) -> Option<PrepareOutcome> {
    match run {
        RunOutcome::Abandoned => None,
        RunOutcome::Failed(message) => Some(PrepareOutcome::Failed(message)),
        RunOutcome::Finished(output) if !output.status.success() => Some(PrepareOutcome::Failed(
            command_failure("Could not read this subtitle track", &output.stderr),
        )),
        RunOutcome::Finished(_) => Some(read_cues(staged)),
    }
}

/// Answers one request: the cue under the cursor first, then the ones around it.
///
/// Returns whether the worker should keep serving requests — `false` only when nobody is
/// listening for events any more, which means the application has gone.
///
/// The selected cue is always sent first, even though the nearby ones are cheaper. They
/// are being got ready for a cursor that has not arrived yet; sending one of them first
/// would put a cache read and an encode in front of the picture the user is looking at an
/// empty pane waiting for.
fn frame(
    request: &FrameRequest,
    picker: &Picker,
    live_generation: &AtomicU64,
    events: &Sender<PreviewEvent>,
) -> bool {
    let abandoned = || live_generation.load(Ordering::Relaxed) != request.generation;
    frame_window(request, picker, &abandoned, events)
}

/// The window itself, with "has the page gone" handed in.
///
/// Split from `frame` for the same reason `warm_track` is split from `warm`: the two
/// places this gives up part-way — during the grab the user is waiting on, and between the
/// neighbours behind it — are a race against a real `ffmpeg` otherwise, and they are what
/// stops a page nobody is looking at any more from finishing its window.
fn frame_window(
    request: &FrameRequest,
    picker: &Picker,
    abandoned: Abandoned<'_>,
    events: &Sender<PreviewEvent>,
) -> bool {
    if abandoned() {
        return true;
    }
    if let Some(target) = request.wanted.as_ref() {
        let outcome = drawn(
            png(&request.source, target, CueSlot::Interactive, abandoned),
            picker,
            request.cells,
        );
        // `None` is the page having closed mid-grab: nothing to report, and nothing left
        // to get ready either, since the window it was around is gone.
        let Some(outcome) = outcome else {
            return true;
        };
        if !publish(events, request.generation, target.cue_index, outcome) {
            return false;
        }
    }
    nearby(request, picker, abandoned, events)
}

/// Encodes the cues around the cursor, from the frame cache only.
///
/// A cue the cache does not have is skipped rather than rendered: the background pass is
/// already walking the whole track, so it will be there shortly, and spawning `ffmpeg`
/// here would compete with the grab the user is actually waiting for.
///
/// Failures are skipped too, rather than reported. Nobody is looking at these cues, and a
/// message from one of them would appear under the cue that *is* selected, blaming the
/// wrong line.
fn nearby(
    request: &FrameRequest,
    picker: &Picker,
    abandoned: Abandoned<'_>,
    events: &Sender<PreviewEvent>,
) -> bool {
    for target in &request.nearby {
        if abandoned() {
            return true;
        }
        let (media, cue) = request.source.key(&target.cue, &target.on_screen);
        let Some(bytes) = framecache::read(&media, &cue) else {
            continue;
        };
        let FrameOutcome::Ready(protocol) = encode(&bytes, picker, request.cells) else {
            continue;
        };
        if !publish(
            events,
            request.generation,
            target.cue_index,
            FrameOutcome::Ready(protocol),
        ) {
            return false;
        }
    }
    true
}

/// Sends one frame back to the page, reporting whether anyone was still listening.
fn publish(
    events: &Sender<PreviewEvent>,
    generation: u64,
    cue_index: usize,
    outcome: FrameOutcome,
) -> bool {
    events
        .send(PreviewEvent::Frame {
            generation,
            cue_index,
            outcome,
        })
        .is_ok()
}

/// What one cue's PNG — cached or freshly grabbed — means for the page.
///
/// Separated from `frame` for the same reason `extracted` is separated from `prepare`:
/// the page closing mid-grab is an ending a test cannot reliably steer a real `ffmpeg`
/// into, and it is the one that must not be mistaken for a frame that failed to draw.
fn drawn(png: PngOutcome, picker: &Picker, cells: Size) -> Option<FrameOutcome> {
    match png {
        PngOutcome::Abandoned => None,
        PngOutcome::Failed(message) => Some(FrameOutcome::Failed(message)),
        PngOutcome::Ready(bytes) => Some(encode(&bytes, picker, cells)),
    }
}

/// The PNG for one cue: from the cache when it is there, from `ffmpeg` when it is not.
enum PngOutcome {
    Ready(Vec<u8>),
    /// The page went away while the grab was running.
    Abandoned,
    Failed(String),
}

/// Produces one cue's frame as PNG bytes, caching what it renders.
///
/// The cache lookup comes first and is the whole point of the module: a hit costs a file
/// read where a miss costs an accurate seek into the container plus a libass burn.
///
/// `slot` decides the file the cue is written to inside the workspace, so the interactive
/// path and every worker of the background pass can run at the same time without
/// overwriting each other's subtitle — see [`CueSlot`].
fn png(
    source: &FrameSource,
    target: &FrameTarget,
    slot: CueSlot,
    abandoned: Abandoned<'_>,
) -> PngOutcome {
    let key = source.key(&target.cue, &target.on_screen);
    if let Some(bytes) = framecache::read(&key.0, &key.1) {
        return PngOutcome::Ready(bytes);
    }
    render(source, target, slot, &key.0, &key.1, abandoned)
}

/// What the background pass made of one cue.
///
/// Its own outcome rather than `PngOutcome` because the pass has no use for the bytes,
/// and because "was already there" is the case it is mostly made of.
#[derive(Debug, Eq, PartialEq)]
enum CacheOutcome {
    Cached,
    Rendered,
    Abandoned,
    Failed,
}

/// Makes sure one cue's frame is in the cache, rendering it only if it is not.
///
/// The lookup is a stat where [`png`]'s is a read, and that is the whole reason this
/// exists separately: a track whose frames are all cached — every page opened a second
/// time — would otherwise pull the entire cache off disk to throw the bytes away, which
/// at a thousand cues is hundreds of megabytes of pointless I/O.
fn cache_frame(
    source: &FrameSource,
    target: &FrameTarget,
    slot: CueSlot,
    abandoned: Abandoned<'_>,
) -> CacheOutcome {
    let key = source.key(&target.cue, &target.on_screen);
    if framecache::is_cached(&key.0, &key.1) {
        return CacheOutcome::Cached;
    }
    match render(source, target, slot, &key.0, &key.1, abandoned) {
        PngOutcome::Ready(_) => CacheOutcome::Rendered,
        PngOutcome::Abandoned => CacheOutcome::Abandoned,
        PngOutcome::Failed(_) => CacheOutcome::Failed,
    }
}

/// Stages the cues on screen and grabs their frame, caching whatever comes back.
///
/// The staged file is the track's own format and styling, not always a bare `.srt` — see
/// [`CueStyle::stage`]. Its *name* comes from the same place, because libass picks its
/// reader from the extension.
///
/// Takes the whole [`FrameTarget`] rather than its parts, so the interactive grab and the
/// background pass cannot hand this different things for the same cue: what is burned in
/// and where the frame is seeked from are one value, decided once, and the cache key is
/// taken from that same value by the callers above.
fn render(
    source: &FrameSource,
    target: &FrameTarget,
    slot: CueSlot,
    media_key: &str,
    cue_key: &str,
    abandoned: Abandoned<'_>,
) -> PngOutcome {
    let staged = source.staged_name(slot);
    let path = source.workspace.join(&staged);
    if let Err(error) = std::fs::write(&path, source.stage(&target.on_screen)) {
        return PngOutcome::Failed(format!("Could not stage the cue to burn in: {error}"));
    }
    let mut command = frame_command(
        &source.media,
        target.seek,
        source.pixels,
        &source.workspace,
        &staged,
    );
    grabbed(run_cancellable(&mut command, abandoned), media_key, cue_key)
}

/// What a finished grab means, and the point the result is cached at.
///
/// Separated from `png` for the same reason `extracted` is separated from `prepare`:
/// giving up part-way and failing to start are endings a test cannot reliably steer a
/// real `ffmpeg` into, and they are the two that must not be mistaken for a blank frame.
fn grabbed(run: RunOutcome, media_key: &str, cue_key: &str) -> PngOutcome {
    let output = match run {
        RunOutcome::Abandoned => return PngOutcome::Abandoned,
        RunOutcome::Failed(message) => return PngOutcome::Failed(message),
        RunOutcome::Finished(output) if !output.status.success() => {
            return PngOutcome::Failed(command_failure(
                "Could not draw this frame",
                &output.stderr,
            ));
        }
        RunOutcome::Finished(output) => output,
    };
    // A successful run that wrote nothing is what seeking past the end of the media looks
    // like. Not cached — there is nothing to cache, and storing an empty file would turn
    // one bad grab into a permanently blank cue — but still passed on, so the decoder
    // reports it as the unreadable frame it is.
    if !output.stdout.is_empty() {
        framecache::store(media_key, cue_key, &output.stdout);
    }
    PngOutcome::Ready(output.stdout)
}

/// Turns PNG bytes into something the page can draw.
///
/// The decode and the protocol encode happen on the worker thread rather than on the UI
/// thread — a kitty protocol is base64 over the whole image, which is milliseconds the
/// event loop would otherwise spend not answering keys.
fn encode(bytes: &[u8], picker: &Picker, cells: Size) -> FrameOutcome {
    let image = match image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg) {
        Ok(image) => image,
        Err(error) => return FrameOutcome::Failed(format!("Unreadable frame: {error}")),
    };
    // `Scale`, not `Fit`. Both fit the picture to the pane proportionally, but `Fit`
    // clamps its target to `min(pane, image)` and so never enlarges — which was fine when
    // the grab was made at the pane's own pixel size, and is wrong now that it is made at
    // a fixed one. A cached frame smaller in pixels than the pane would sit in a corner of
    // it at its stored size. `Scale` is what fills the pane from a frame of any size, and
    // it is also why a resize no longer re-runs `ffmpeg`: the same cached bytes re-encode
    // to whatever the pane has become.
    //
    // The filter is stated rather than defaulted, and this is the step the whole rendering
    // resolution rests on. `ratatui-image` defaults to `FilterType::Nearest`, which does
    // not average: going from a 1080p frame to a pane is a large reduction — a halfblocks
    // pane is around 120×70 *pixels* — so nearest simply picks one source pixel per
    // destination and throws the rest away. On a burned-in subtitle, whose strokes are a
    // pixel or two wide at that scale, that is the difference between text and a dashed
    // line, and it makes rendering at a higher resolution actively pointless.
    //
    // `Triangle` over the sharper cubics: `image`'s resampler scales the filter's support
    // by the reduction ratio, so this is a genuine area average rather than a 2×2 tap, and
    // it does not ring. `CatmullRom` measured the same speed but overshoots at high
    // contrast, which on white-on-black text is a halo around every glyph. It also handles
    // the other direction — a pane wider than the stored frame — where nearest is what
    // makes an enlarged picture look blocky.
    //
    // Not asserted anywhere, and that is not an oversight. The only protocol `TestBackend`
    // can read back is halfblocks, which reduces the resized image a second time to reach
    // the cell grid and averages as it does so; measured against a stripe pattern, nearest
    // and triangle come out of that path indistinguishable. The protocols where this
    // decision is the *last* step before the terminal draws — kitty, sixel, iTerm2 — write
    // escape sequences the test buffer never stores. A test here could only be tuned to
    // pass, so there is none.
    match picker.new_protocol(image, cells, Resize::Scale(Some(FilterType::Triangle))) {
        Ok(protocol) => FrameOutcome::Ready(Box::new(protocol)),
        Err(error) => FrameOutcome::Failed(format!("Could not draw this frame: {error}")),
    }
}

/// Decodes one cue's span into raw frames and hands them back.
///
/// Returns whether the worker should keep serving requests — `false` only when nobody is
/// listening for events any more, which means the application has gone.
fn playback(
    request: &PlaybackRequest,
    picker: &Picker,
    live_playback: &AtomicU64,
    events: &Sender<PreviewEvent>,
) -> bool {
    let abandoned = || live_playback.load(Ordering::Relaxed) != request.generation;
    let Some(outcome) = decoded(request, picker, &abandoned) else {
        // The playback was stopped mid-decode. Nothing to report: the page is already back
        // to its still frame, and a span arriving now would start playing under it.
        return true;
    };
    events
        .send(PreviewEvent::Playback {
            generation: request.generation,
            cue_index: request.cue_index,
            outcome,
        })
        .is_ok()
}

/// The span itself, or `None` if the playback was abandoned on the way.
///
/// One `ffmpeg` for the whole span rather than one per frame, which is the entire reason a
/// playback is affordable at all: an accurate seek costs the same whether one frame comes
/// out of it or four hundred, and measured here it is about eight times faster than seeking
/// per frame.
fn decoded(
    request: &PlaybackRequest,
    picker: &Picker,
    abandoned: Abandoned<'_>,
) -> Option<PlaybackOutcome> {
    if abandoned() {
        return None;
    }
    let span = request.span_end.saturating_sub(request.span_start);
    // Every cue in the span, at its own times measured from the start of it — which is where
    // `-ss` before `-i` resets the output's timestamps to. Staging them at their original
    // times instead would put the lines minutes past the end of a span a few seconds long,
    // and the playback would show a silent picture with no subtitle in it at all.
    let staged = request.source.staged_name(CueSlot::Playback);
    let path = request.source.workspace.join(&staged);
    let text = request
        .source
        .stage_span(&request.on_screen, request.span_start);
    if let Err(error) = std::fs::write(&path, text) {
        return Some(PlaybackOutcome::Failed(format!(
            "Could not stage the cue to burn in: {error}"
        )));
    }
    // **Cleared before the run, not after it.** Every playback of this page writes its sound
    // to the same name in the same workspace, and a run that asks for no sound — a muted
    // playback, or media with no audio track — writes nothing at all. Left in place, the
    // previous span's file is still sitting there when the samples are read back, and the
    // page plays the *last* cue's sound under this one's picture. A failure to remove it is
    // not worth refusing the playback over; the read below is gated on having asked for
    // sound, which is the same guarantee arrived at from the other side.
    let audio_path = request.source.workspace.join(AUDIO_FILE);
    let _ = std::fs::remove_file(&audio_path);
    let mut command = playback_command(request, span, &staged);
    let pixels = match played(run_cancellable(&mut command, abandoned)) {
        SpanOutcome::Abandoned => return None,
        SpanOutcome::Failed(message) => return Some(PlaybackOutcome::Failed(message)),
        SpanOutcome::Ready(pixels) => pixels,
    };
    let frames = PlaybackFrames::new(
        pixels,
        SpanShape {
            pixels: request.pixels,
            cells: request.cells,
            picker: picker.clone(),
        },
        request.fps,
        request.speed,
        request.span_start,
        // Read back off disk rather than off a second pipe: `ffmpeg` writes both outputs
        // concurrently, and two pipes into one process is a deadlock waiting for whichever
        // reader falls behind. The file is a few hundred kilobytes in the workspace that
        // the page deletes when it closes.
        //
        // Only when this run asked for sound. A muted playback writes no such file, so
        // reading unconditionally hands it whatever the last unmuted playback of this page
        // left behind — the previous cue's sound, under this cue's picture, from a page the
        // user has just told to be silent.
        match request.audio {
            Some(_) => read_samples(&audio_path),
            None => Vec::new(),
        },
    );
    // A run that succeeded without writing a whole frame is what seeking past the end of
    // the media looks like, and it is the one case the slicing arithmetic cannot represent
    // — nothing to show, and no reason on screen for it.
    if frames.count() == 0 {
        return Some(PlaybackOutcome::Failed(
            "There is no video at this cue to play.".to_string(),
        ));
    }
    Some(PlaybackOutcome::Ready(frames))
}

/// A span's raw pixels, or why there are none.
enum SpanOutcome {
    Ready(Vec<u8>),
    /// The playback was stopped while the decode was running.
    Abandoned,
    Failed(String),
}

/// What a finished playback run means.
///
/// Separated from [`decoded`] for the same reason [`grabbed`] is separated from [`png`]:
/// giving up part-way and failing to start are endings a test cannot reliably steer a real
/// `ffmpeg` into, and the first must not be mistaken for a span that failed to decode.
fn played(run: RunOutcome) -> SpanOutcome {
    match run {
        RunOutcome::Abandoned => SpanOutcome::Abandoned,
        RunOutcome::Failed(message) => SpanOutcome::Failed(message),
        RunOutcome::Finished(output) if !output.status.success() => {
            SpanOutcome::Failed(command_failure("Could not play this cue", &output.stderr))
        }
        RunOutcome::Finished(output) => SpanOutcome::Ready(output.stdout),
    }
}

/// Decodes a span of video with the cue burned in, straight to raw pixels.
///
/// `rawvideo` rather than the still path's mjpeg: hundreds of frames go by in a couple of
/// seconds, and an encode plus a decode per frame is work whose only product is a smaller
/// buffer. Raw at the *display* size is smaller than a JPEG at the cache's size anyway.
///
/// **`scale` is a literal `W:H` with no `force_original_aspect_ratio`.** The output is
/// sliced by `width * height * 3`, so a frame that came back one row shorter than asked for
/// would not crop the picture — it would shear every frame after the first. The proportions
/// are kept by [`playback_cells`] deciding the size instead.
///
/// Filters in this order for the same reason the still grab orders its two: libass lays the
/// text out against the source's own resolution before anything shrinks it, so the burn is
/// legible at a size where a scaled-down overlay would not be. `fps` before `scale`, so the
/// frames thrown away are thrown away before they are resized rather than after.
fn playback_command(request: &PlaybackRequest, span: Duration, staged: &str) -> Command {
    let mut command = Command::new("ffmpeg");
    command
        .current_dir(&request.source.workspace)
        .args(["-v", "error", "-nostdin", "-y", "-ss"])
        .arg(format!("{:.3}", request.span_start.as_secs_f64()))
        .arg("-t")
        .arg(format!("{:.3}", span.as_secs_f64()))
        .arg("-i")
        .arg(&request.source.media)
        .args(["-map", "0:v:0", "-an", "-vf"])
        .arg(video_filters(request, staged))
        .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "-"]);
    // A second output, from the same seek and the same decode — which is what makes the
    // sound and the picture describe the same stretch of media by construction rather than
    // by two commands agreeing about a timestamp.
    //
    // Emitted at the *device's* rate and channel count, so the audio callback copies rather
    // than resamples: the resampling lands in a process that is already scaling video and
    // burning subtitles, instead of on the one thread that must never be late.
    //
    // Written to a file rather than a second pipe. `ffmpeg` writes both outputs as it goes,
    // so two pipes into one process deadlock the moment either reader falls behind — and
    // the video pipe is being drained on a thread that knows nothing about the other.
    if let Some(audio) = request.audio {
        command
            .args(["-map", "0:a:0", "-vn", "-f", "f32le", "-ar"])
            .arg(audio.sample_rate.to_string())
            .arg("-ac")
            .arg(audio.channels.to_string());
        // Pitch-preserving, which is the whole reason speech at half speed is still speech
        // and a line can still be judged against it. A resample would drop it an octave.
        if !request.speed.is_normal() {
            command.args(["-af", &atempo_chain(request.speed)]);
        }
        command.arg(AUDIO_FILE);
    }
    command
}

/// The video filter chain: burn the cue in, retime, decimate, resize — in that order.
///
/// `setpts` sits between the burn and the `fps` filter deliberately. Ahead of `fps`, so the
/// rate filter samples the stretched timeline and yields `fps` frames for every second of
/// *output* rather than `fps × speed` of them. Behind `subtitles`, because libass lays the
/// text out against the source's own timestamps and a retimed stream would move the cue out
/// from under the frames it belongs on.
///
/// At normal speed no `setpts` is emitted at all, so the command is byte-identical to the
/// one this feature was added to — which is what keeps the default path free of a filter it
/// would only ever be a no-op in.
fn video_filters(request: &PlaybackRequest, staged: &str) -> String {
    let (width, height) = request.pixels;
    let mut filters = format!("subtitles={staged}");
    if !request.speed.is_normal() {
        filters.push_str(&format!(",setpts=PTS/{}", request.speed.as_f64()));
    }
    filters.push_str(&format!(",fps={},scale={width}:{height}", request.fps));
    filters
}

/// The narrowest and widest ratio one `atempo` is reliable over.
///
/// FFmpeg has widened the accepted range over the years, but chaining is what every version
/// agrees on — and the floor is what a quarter speed needs regardless.
const ATEMPO_MIN: f64 = 0.5;
const ATEMPO_MAX: f64 = 2.0;

/// One or more `atempo` filters whose product is `speed`.
///
/// A single filter cannot halve a span twice, so a quarter speed comes out as
/// `atempo=0.5,atempo=0.5`. Written as a loop rather than as a special case for the one
/// speed that needs it, so that lowering [`PlaybackSpeed::STEPS`]'s floor later cannot
/// silently produce a filter `ffmpeg` refuses.
///
/// **Only the floor is chained.** [`PlaybackSpeed::FASTEST`] is exactly [`ATEMPO_MAX`], which
/// one filter takes, so there is no upwards case to write — and an unreachable one would be
/// a branch no test could ever exercise. Raising that ceiling means adding the mirror of the
/// loop below, not merely adding a step.
fn atempo_chain(speed: PlaybackSpeed) -> String {
    debug_assert!(
        speed.as_f64() <= ATEMPO_MAX,
        "raising the speed ceiling needs the upwards half of this chain"
    );
    let mut remaining = speed.as_f64();
    let mut filters = Vec::new();
    while remaining < ATEMPO_MIN {
        filters.push(format!("atempo={ATEMPO_MIN}"));
        remaining /= ATEMPO_MIN;
    }
    filters.push(format!("atempo={remaining}"));
    filters.join(",")
}

/// The span's sound, staged in the page's workspace beside its cue.
///
/// A bare relative name for the same reason [`CueSlot`] uses one: `ffmpeg` runs with the
/// workspace as its working directory, so nothing here needs escaping.
const AUDIO_FILE: &str = "playback.f32";

/// Reads a span's sound back as interleaved floats.
///
/// Empty for media with no audio track, and empty for a file that could not be read —
/// which are the same thing as far as the page is concerned, and both play as a silent
/// slideshow at the right rate rather than as a failure.
fn read_samples(path: &Path) -> Vec<f32> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

/// Renders every cue in a track into the cache, reporting progress as it goes.
///
/// Returns whether the worker should keep serving requests: `false` only when nobody is
/// listening for events any more, which means the application has gone.
///
/// In list order rather than outwards from the selection. The interactive worker is a
/// separate thread that always renders whatever the cursor is on right now, so this one
/// never has to race the user — its job is only to make sure that by the time they get
/// somewhere, the frame is already there.
fn warm(request: &WarmRequest, live_generation: &AtomicU64, events: &Sender<PreviewEvent>) -> bool {
    let abandoned = || live_generation.load(Ordering::Relaxed) != request.generation;
    warm_track(request, warm_workers(), &abandoned, events)
}

/// The pass itself, with "has the page gone" handed in.
///
/// Split from `warm` so a test can decide *when* the page goes: the two places this gives
/// up — between cues, and part-way through rendering one — are a race against a real
/// `ffmpeg` otherwise, and they are what stops a thousand-cue track running on for a page
/// nobody is looking at.
///
/// `workers` is handed in rather than read from [`warm_workers`], so the tests below pin
/// it: the slicing is what several of them assert on, and a count that varied with the
/// machine would make them pass here and fail on a two-core CI runner.
///
/// The track is walked by `workers` threads over contiguous slices of it. Scoped
/// threads rather than a pool: they are joined before this returns, so the worker's
/// `newest()` coalescing and its "stop once nobody is listening" contract are untouched,
/// and a pass that runs for minutes does not care what a thread cost to spawn.
///
/// Contiguous slices rather than interleaved, because each `ffmpeg` is an accurate seek
/// and neighbouring cues are near each other in the container — three workers reading three
/// regions is three sequential walks, where interleaving would make all three seek across
/// the whole file.
fn warm_track(
    request: &WarmRequest,
    workers: usize,
    abandoned: Abandoned<'_>,
    events: &Sender<PreviewEvent>,
) -> bool {
    if abandoned() {
        return true;
    }
    // Marked used before anything else, and pruned immediately after. The order is the
    // point: the marker is what ranks this track as the most recent, so a prune that runs
    // a moment later cannot pick it — and the track is named to `prune` as the open one
    // besides, which is what makes evicting the frames this pass was opened to show
    // impossible rather than merely unlikely.
    let media_key = request.source.media_key();
    framecache::touch(&media_key);
    // Before rendering, not after: the pass is about to add a frame per cue, and pruning
    // first is what keeps the cache inside its limit rather than over it until next time.
    framecache::prune(request.cache_tracks, Some(&media_key));

    let total = request.cues.len();
    let progress = Mutex::new(WarmProgress::new(request.generation, total));
    if !progress
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .publish(events, 0)
    {
        return false;
    }

    // Shared rather than per-worker, because the status line counts the track and not the
    // slice. `done` is therefore no longer an index into the cue list — three workers
    // finish cues out of order — which is why nothing may infer *which* cues are done
    // from it.
    let done = AtomicUsize::new(0);
    // `div_ceil`, so the last slice is the short one and no worker is handed an empty
    // slice while cues are left over.
    let per_worker = total.div_ceil(workers).max(1);
    std::thread::scope(|scope| {
        for (worker, cues) in request.cues.chunks(per_worker).enumerate() {
            let progress = &progress;
            let done = &done;
            let slice = WarmSlice {
                cues,
                // Where this slice starts in the track, which `chunks` does not report.
                offset: worker * per_worker,
                worker,
            };
            scope.spawn(move || {
                warm_slice(request, slice, abandoned, events, progress, done);
            });
        }
    });

    if abandoned() {
        // No final event: the page this counted for is gone, and a `Warming` for it would
        // be dropped by `App` anyway.
        return true;
    }
    // The last count is published here rather than by whichever worker happens to finish
    // last: only this point knows every slice is done, and the status line has to stop
    // counting however the pass ended — including a slice that gave up part-way.
    //
    // Its result is also what tells the worker loop that nobody is listening any more,
    // which is why the slices themselves do not track that: a closed event channel means
    // the application is exiting, so stopping a slice a few frames earlier buys nothing
    // that process teardown is not about to do anyway — and a flag for it would be a
    // path across three threads that no test can reach.
    progress
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .publish(events, total)
}

/// One worker's share of a track: which cues it renders, and where they sit in the whole.
///
/// `offset` is what makes the slice locatable. A worker renders a contiguous run of cues,
/// but what each cue is burned *with* comes from the entire cue list — the lines sharing its
/// moment are wherever the file put them, which may be on the other side of a slice
/// boundary. Without the offset the worker could only say "the third cue I was given",
/// which names a different cue in every worker.
#[derive(Clone, Copy)]
struct WarmSlice<'a> {
    cues: &'a [Cue],
    offset: usize,
    worker: usize,
}

/// One worker's contiguous slice of the track.
///
/// The slot is [`CueSlot::Warm`] for this worker and nothing else. Two workers sharing one
/// staged file would each overwrite the other's cue between the write and the burn, and
/// the frame that came out would carry the wrong line — stored under the *right* key, so
/// nothing downstream could ever notice. It is the single most important thing here.
fn warm_slice(
    request: &WarmRequest,
    slice: WarmSlice<'_>,
    abandoned: Abandoned<'_>,
    events: &Sender<PreviewEvent>,
    progress: &Mutex<WarmProgress>,
    done: &AtomicUsize,
) {
    let slot = CueSlot::Warm(slice.worker);
    let mut failures = 0;
    for (position, cue) in slice.cues.iter().enumerate() {
        let position = slice.offset + position;
        if abandoned() {
            return;
        }
        // Built here rather than handed in, from the track's *whole* cue list rather than
        // this worker's slice: the cues sharing a moment with this one are wherever the file
        // put them, and a slice boundary falling between them would have this worker render
        // a different picture from the one the interactive grab produces — under the same
        // cache key, so whichever ran first would win and nothing downstream could notice.
        let seek = seek_for(cue, request.duration);
        let target = FrameTarget {
            cue_index: position,
            cue: cue.clone(),
            on_screen: crate::cue::on_screen_at(&request.cues, position, seek),
            seek,
        };
        // Checked per cue as the pass reaches it, rather than filtered up front, because
        // the interactive worker may have rendered this very cue since the pass started.
        match cache_frame(&request.source, &target, slot, abandoned) {
            CacheOutcome::Cached | CacheOutcome::Rendered => failures = 0,
            CacheOutcome::Abandoned => return,
            CacheOutcome::Failed => {
                failures += 1;
                if failures >= WARM_FAILURE_TOLERANCE {
                    // Per slice rather than shared across the pass. A flag telling the
                    // other workers to stop early was tried and removed: each already
                    // bounds its own waste at this same handful of subprocesses, so the
                    // flag bought a fraction of one worker's tolerance in exchange for a
                    // mechanism no test could pin down.
                    return;
                }
            }
        }
        let counted = done.fetch_add(1, Ordering::Relaxed) + 1;
        // `try_lock`, not `lock`: the count is already recorded, and a worker blocking to
        // publish a number another worker is publishing a larger version of would be
        // waiting to say something already out of date.
        if let Ok(mut progress) = progress.try_lock() {
            progress.publish(events, counted);
        }
    }
}

/// The background pass's rate-limited progress reporting.
struct WarmProgress {
    generation: u64,
    total: usize,
    last_published: Option<Instant>,
}

impl WarmProgress {
    fn new(generation: u64, total: usize) -> Self {
        Self {
            generation,
            total,
            last_published: None,
        }
    }

    /// Publishes `done`, unless it is neither the first nor the last and the previous one
    /// was too recent. Returns whether anyone is still listening.
    fn publish(&mut self, events: &Sender<PreviewEvent>, done: usize) -> bool {
        let due = self
            .last_published
            .is_none_or(|at| at.elapsed() >= WARM_PROGRESS_INTERVAL);
        if !due && done < self.total {
            return true;
        }
        self.last_published = Some(Instant::now());
        events
            .send(PreviewEvent::Warming {
                generation: self.generation,
                done,
                total: self.total,
            })
            .is_ok()
    }
}

/// A SubRip file holding the cues to burn, each at the times it was given.
///
/// `-ss` before `-i` resets output timestamps to about zero, which is why a still's cues
/// arrive here starting at zero and running far past the single frame that emerges: left at
/// their original times they would be burned in or not depending on the frame rounding and
/// the container's `start_time`, making the grab a coin toss on exactly the boundary frames
/// a timing page exists to inspect.
///
/// Numbered from one in the order handed in. SubRip's counter is advisory — `parse_srt`
/// ignores it for that reason — but a file whose blocks all claim to be block 1 is the kind
/// of thing a reader trips over, and libass has no reason to be handed one.
fn srt_of(timed: &[(&Cue, Duration, Duration)]) -> String {
    timed
        .iter()
        .enumerate()
        .map(|(position, (cue, start, end))| {
            format!(
                "{}\n{} --> {}\n{}\n\n",
                position + 1,
                format_srt_timestamp(*start),
                format_srt_timestamp(*end),
                cue.text.trim_end()
            )
        })
        .collect()
}

/// Grabs one frame with the staged cue burned into it, scaled to the cache's fixed size.
///
/// Runs with the workspace as its working directory so `subtitles=cue.srt` is a constant:
/// see [`CUE_FILE`]. The filters are ordered `subtitles` then `scale` so libass lays the
/// text out against the source resolution — its `PlayRes` — before anything shrinks it.
fn frame_command(
    media: &Path,
    seek: Duration,
    pixels: (u32, u32),
    workspace: &Path,
    staged: &str,
) -> Command {
    let (width, height) = pixels;
    let mut command = Command::new("ffmpeg");
    command
        .current_dir(workspace)
        .args(["-v", "error", "-nostdin", "-y", "-ss"])
        .arg(format!("{:.3}", seek.as_secs_f64()))
        .arg("-i")
        .arg(media)
        .args(["-map", "0:v:0", "-frames:v", "1", "-vf"])
        .arg(format!(
            "subtitles={staged},scale={width}:{height}:force_original_aspect_ratio=decrease"
        ))
        // `-q:v 2` is mjpeg's near-lossless end. The frame is judged by eye against a
        // burned-in subtitle, so visible compression artefacts would be read as the
        // rendering being wrong; anything looser is not worth the kilobytes it saves.
        //
        // `-pix_fmt yuvj420p` is not optional: the mjpeg encoder refuses limited-range YUV
        // outright ("Non full-range YUV is non-standard"), which is what most video
        // actually is, and the filter graph only negotiates its way to a full-range format
        // on its own some of the time. Stating it makes every source encode the same way.
        .args([
            "-f",
            "image2pipe",
            "-vcodec",
            "mjpeg",
            "-pix_fmt",
            "yuvj420p",
            "-q:v",
            "2",
            "-",
        ]);
    command
}

/// Pulls one subtitle stream out of a container.
///
/// **`-c:s copy` wherever the format is one this crate parses itself**, because the page
/// shows the track as it is actually stored and anything that re-encoded it would be
/// previewing a file the user does not have. For ASS that is not a preference but a
/// requirement: a transcode to SubRip discards the styles, the `PlayRes`, and every
/// positioning override, which is most of what an ASS track *is*.
///
/// The rest transcode to SubRip on the way out, since nothing here can read them and they
/// carry no styling that would survive the trip anyway.
fn extract_command(media: &Path, index: u64, format: SubtitleFormat, output: &Path) -> Command {
    let codec = if extracted_format(format) == format {
        "copy"
    } else {
        "subrip"
    };
    let mut command = Command::new("ffmpeg");
    command
        .args(["-v", "error", "-nostdin", "-y", "-i"])
        .arg(media)
        .args(["-map", &format!("0:{index}"), "-c:s", codec])
        .arg(output);
    command
}

/// Rewrites a sidecar into SubRip, for the formats FFmpeg can read but this crate cannot.
///
/// The embedded path gets this for free — `extract_command` transcodes as it demuxes — so
/// this exists only for a file that was never inside a container.
fn transcode_command(source: &Path, output: &Path) -> Command {
    let mut command = Command::new("ffmpeg");
    command
        .args(["-v", "error", "-nostdin", "-y", "-i"])
        .arg(source)
        .args(["-c:s", "subrip"])
        .arg(output);
    command
}

fn read_cues(path: &Path) -> PrepareOutcome {
    match read_srt(path) {
        Ok(cues) => PrepareOutcome::Ready {
            cues,
            style: CueStyle::SubRip,
        },
        Err(error) => PrepareOutcome::Failed(format!("Could not read {}: {error}", label(path))),
    }
}

/// Reads an ASS script, keeping the header its cues are drawn by alongside them.
fn read_ass_cues(path: &Path) -> PrepareOutcome {
    match read_ass(path) {
        Ok(script) => PrepareOutcome::Ready {
            cues: script.cues,
            style: CueStyle::Ass {
                header: script.header,
                events_format: script.events_format,
            },
        },
        Err(error) => PrepareOutcome::Failed(format!("Could not read {}: {error}", label(path))),
    }
}

/// A path as it should appear in a message on the page, which has room for a filename
/// and not for a path.
fn label(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .to_string()
}

/// Folds a subprocess's stderr into one line for the page.
///
/// A build that fails without a word would otherwise show only the heading, which reads
/// as a bug in reel rather than as a file `ffmpeg` could not handle.
fn command_failure(heading: &str, stderr: &[u8]) -> String {
    let detail = String::from_utf8_lossy(stderr)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if detail.is_empty() {
        format!("{heading}.")
    } else {
        format!("{heading}: {detail}")
    }
}

enum RunOutcome {
    Finished(Output),
    /// The page went away while the command was running, and the command was killed.
    Abandoned,
    Failed(String),
}

/// Runs `command` to completion, killing it if `abandoned` starts returning true.
///
/// A pared-down `edit::run_cancellable_output`: same piped-reader-thread shape and same
/// 25 ms poll, without the progress reporting, since the page has nothing to report
/// progress *to* — it shows an indeterminate loader either way.
///
/// Both streams are piped rather than inherited. `ffmpeg` writing a single line to the
/// inherited stderr would land on the alternate screen, corrupting the TUI.
fn run_cancellable(command: &mut Command, abandoned: Abandoned<'_>) -> RunOutcome {
    let program = command.get_program().to_string_lossy().to_string();
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return RunOutcome::Failed(if error.kind() == std::io::ErrorKind::NotFound {
                format!("{program} was not found in PATH.")
            } else {
                format!("Could not start {program}: {error}")
            });
        }
    };
    // Read both pipes on their own threads: a subtitle stream is comfortably larger than
    // a pipe buffer, and a child blocked writing into a pipe nobody is draining would
    // hang here forever. Both are `Some` because both were just configured as pipes.
    let stdout_reader = drain(
        child
            .stdout
            .take()
            .expect("stdout was configured as a pipe"),
    );
    let stderr_reader = drain(
        child
            .stderr
            .take()
            .expect("stderr was configured as a pipe"),
    );

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if !abandoned() => {
                std::thread::sleep(POLL_INTERVAL);
                continue;
            }
            // Giving up, and the wait failing outright, are handled identically. There is
            // no separate error message for the latter because there is no way to produce
            // one: `try_wait` fails only when something else has already reaped the child,
            // and this process installs no `SIGCHLD` handler that could. Inventing a
            // distinct message would be an untestable branch either way, and the useful
            // reaction is the same — stop, and clean the child up.
            _ => {}
        }
        let _ = child.kill();
        let _ = child.wait();
        let _ = stdout_reader.join();
        let _ = stderr_reader.join();
        return RunOutcome::Abandoned;
    };
    RunOutcome::Finished(Output {
        status,
        stdout: stdout_reader.join().unwrap_or_default(),
        stderr: stderr_reader.join().unwrap_or_default(),
    })
}

fn drain(stream: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = BufReader::new(stream).read_to_end(&mut bytes);
        bytes
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    use kernal::prelude::*;

    use super::*;

    const SRT: &str = "1\n00:00:01,000 --> 00:00:02,000\nfirst\n\n\
                       2\n00:00:03,000 --> 00:00:04,500\nsecond\n\n";

    /// What the interactive grab stages a SubRip cue under, which is what these fixtures
    /// are. Resolved through [`CueSlot`] rather than spelled out, so a test asserting on
    /// the staged file cannot drift from the name the renderer actually writes.
    fn cue_file() -> String {
        CueSlot::Interactive.file(&CueStyle::SubRip)
    }

    fn warm_cue_file(worker: usize) -> String {
        CueSlot::Warm(worker).file(&CueStyle::SubRip)
    }

    const ASS: &str = "[Script Info]\n\
                       ScriptType: v4.00+\n\
                       PlayResX: 384\n\
                       PlayResY: 288\n\
                       \n\
                       [V4+ Styles]\n\
                       Format: Name, Fontname, Fontsize, Alignment\n\
                       Style: Default,Arial,16,2\n\
                       Style: Sign,Impact,40,8\n\
                       \n\
                       [Events]\n\
                       Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
                       Dialogue: 0,0:00:01.00,0:00:02.00,Sign,,0,0,0,,{\\pos(320,10)}a sign\n";

    fn ass_style() -> CueStyle {
        let script = crate::cue::parse_ass(ASS);
        CueStyle::Ass {
            header: script.header,
            events_format: script.events_format,
        }
    }

    /// The whole reason ASS is not simply transcoded to SubRip. A `Dialogue:` line lifted
    /// out on its own renders in libass's defaults at libass's scale — so the staged file
    /// has to carry the styles it names and the `PlayRes` it positions against, or the
    /// page draws a picture the user will never see.
    #[test]
    fn staging_an_ass_cue_should_carry_the_styles_and_resolution_that_draw_it() {
        // Arrange
        let script = crate::cue::parse_ass(ASS);
        let style = ass_style();

        // Act
        let staged = style.stage(&script.cues[..1]);

        // Assert: a whole script, not a line.
        assert_that!(staged.as_str()).contains("[Script Info]");
        assert_that!(staged.as_str()).contains("PlayResX: 384");
        assert_that!(staged.as_str()).contains("[V4+ Styles]");
        assert_that!(staged.as_str()).contains("Style: Sign,Impact,40,8");
        assert_that!(staged.as_str()).contains("[Events]");
        assert_that!(staged.as_str()).contains(
            "Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text",
        );

        // Assert: the cue's own line, retimed to cover the grab and otherwise untouched —
        // its style name and its position override are what the preview exists to show.
        assert_that!(staged.as_str())
            .contains("Dialogue: 0,0:00:00.00,0:10:00.00,Sign,,0,0,0,,{\\pos(320,10)}a sign");
        assert_that!(staged.contains("0:00:01.00")).is_false();

        // Assert: and exactly one cue, so nothing else in the track can draw over it.
        assert_that!(staged.matches("Dialogue:").count()).is_equal_to(1);
    }

    /// libass picks its reader from the name, so a staged ASS script called `.srt` is read
    /// as SubRip and every override tag in it lands on the frame as literal text.
    #[test]
    fn the_staged_file_should_be_named_for_the_format_it_holds() {
        // Act / Assert
        assert_that!(CueSlot::Interactive.file(&CueStyle::SubRip).as_str()).is_equal_to("cue.srt");
        assert_that!(CueSlot::Interactive.file(&ass_style()).as_str()).is_equal_to("cue.ass");
        assert_that!(CueSlot::Warm(2).file(&CueStyle::SubRip).as_str()).is_equal_to("warm-2.srt");
        assert_that!(CueSlot::Warm(2).file(&ass_style()).as_str()).is_equal_to("warm-2.ass");
    }

    /// Two ASS cues reading the same words in different styles draw differently, so they
    /// must not share a cache key — and the SubRip path must be unaffected by the style
    /// having entered the key at all.
    #[test]
    fn the_cache_key_should_follow_the_style_a_cue_is_drawn_in() {
        // Arrange: one script's cue, and the same words under the other style.
        let directory = scratch("ass-key");
        let source = source(&directory.join("clip.mkv"), &directory);
        let script = crate::cue::parse_ass(ASS);
        let styled = FrameSource {
            style: Arc::new(ass_style()),
            ..source.clone()
        };
        let restyled = FrameSource {
            style: Arc::new(CueStyle::Ass {
                header: script.header.replace("PlayResX: 384", "PlayResX: 1920"),
                events_format: script.events_format.clone(),
            }),
            ..source.clone()
        };

        // Act / Assert: the same media, so the same directory — but not the same frame.
        let sign = &script.cues[0];
        assert_that!(key_alone(&styled, sign).0.as_str())
            .is_equal_to(key_alone(&restyled, sign).0.as_str());
        assert_that!(key_alone(&styled, sign).1.as_str())
            .is_not_equal_to(key_alone(&restyled, sign).1.as_str());

        // Act / Assert: and a SubRip source of the same words keys apart from both, since
        // it stages an entirely different file.
        let plain = cue(1000, 2000, "a sign");
        assert_that!(key_alone(&source, &plain).1.as_str())
            .is_not_equal_to(key_alone(&styled, sign).1.as_str());

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// A frame's cost is the pixels it holds, which is why the window's budget consults the
    /// picker rather than assuming a figure: the same pane costs four times as much under
    /// kitty as under sixel, and both scale with the font where nothing about the *cell*
    /// count does.
    #[test]
    fn a_frame_should_cost_what_its_protocol_actually_holds() {
        // Arrange: a 10x20 font, so a cell is two hundred pixels.
        let font = FontSize::new(10, 20);

        // Act / Assert: kitty holds the pixels as RGBA base64, four bytes each and a third
        // again for the encoding. iTerm2 holds base64 PNG, taken uncompressed because how
        // well a video frame packs is not knowable here.
        assert_that!(frame_bytes_per_cell(ProtocolType::Kitty, font)).is_equal_to(200 * 4 * 4 / 3);
        assert_that!(frame_bytes_per_cell(ProtocolType::Iterm2, font)).is_equal_to(200 * 4 * 4 / 3);

        // Act / Assert: sixel carries no alpha and no base64.
        assert_that!(frame_bytes_per_cell(ProtocolType::Sixel, font)).is_equal_to(200);

        // Act / Assert: and it tracks the font, since that is what says how many pixels a
        // cell actually is.
        assert_that!(frame_bytes_per_cell(
            ProtocolType::Sixel,
            FontSize::new(30, 60)
        ))
        .is_equal_to(1800);
    }

    /// The picker moves into the frame worker, so anything the rest of the program needs
    /// from it has to be read off before the workers are spawned — and a build with no
    /// picker encodes nothing, so it holds nothing.
    #[test]
    fn spawning_the_workers_should_carry_the_pickers_frame_cost_out_with_them() {
        // Act / Assert
        let picker = test_picker();
        let font = picker.font_size();
        let (handles, _events) = spawn_preview_workers(Some(picker));
        assert_that!(handles.frame_bytes_per_cell())
            .is_equal_to(u64::from(font.width) * u64::from(font.height) * 4 * 4 / 3);

        // Act / Assert: the shape a terminal with no image protocol arrives in, since
        // `drawing_picker` turns that answer into `None` before it gets here.
        let (handles, _events) = spawn_preview_workers(None);
        assert_that!(handles.frame_bytes_per_cell()).is_equal_to(0);
        assert_that!(handles.draws_frames()).is_false();
        assert_that!(handles.picker().is_none()).is_true();
    }

    fn scratch(label: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-preview-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    fn require_ffmpeg(test: &str) {
        let available = Command::new("ffmpeg")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        assert!(
            available,
            "{test} requires ffmpeg; install the missing test prerequisite"
        );
    }

    /// A Matroska carrying one `subrip` track at absolute stream index 1.
    fn media_with_subtitle(directory: &Path) -> PathBuf {
        let text = directory.join("source.srt");
        std::fs::write(&text, SRT).unwrap();
        let media = directory.join("clip.mkv");
        let output = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-nostdin",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=32x24:r=5:d=5",
                "-i",
            ])
            .arg(&text)
            .args([
                "-map", "0:v:0", "-map", "1:s:0", "-c:v", "ffv1", "-c:s", "copy",
            ])
            .arg(&media)
            .output()
            .expect("ffmpeg should be runnable");
        assert!(
            output.status.success(),
            "failed to build the fixture: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        media
    }

    /// A blue clip with nothing in it but frames, for burning a cue onto.
    fn video(directory: &Path) -> PathBuf {
        let media = directory.join("clip.mkv");
        let output = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-nostdin",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:s=320x240:r=10:d=6",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-preset",
                "ultrafast",
            ])
            .arg(&media)
            .output()
            .expect("ffmpeg should be runnable");
        assert!(
            output.status.success(),
            "failed to build the video fixture: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        media
    }

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

    /// A target for a cue that shares its moment with nothing else, which is what all but a
    /// handful of the tests here are about. The ones that do care about the company a cue
    /// keeps build [`FrameTarget::on_screen`] themselves.
    fn alone(cue_index: usize, cue: Cue, seek: Duration) -> FrameTarget {
        FrameTarget {
            cue_index,
            on_screen: vec![cue.clone()],
            cue,
            seek,
        }
    }

    /// The cache path for a cue with nothing else on screen, which is how the helpers below
    /// seed, read and forget frames.
    fn key_alone(source: &FrameSource, cue: &Cue) -> (String, String) {
        source.key(cue, std::slice::from_ref(cue))
    }

    /// A source whose key is unique to the caller, so tests sharing the one cache
    /// directory cannot collide on a key or read each other's frames.
    fn source(media: &Path, workspace: &Path) -> FrameSource {
        FrameSource {
            media: media.to_path_buf(),
            media_length: 4096,
            media_modified: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
            pixels: (64, 48),
            style: Arc::new(CueStyle::SubRip),
            workspace: workspace.to_path_buf(),
        }
    }

    fn frame_request(media: &Path, workspace: &Path, seek: Duration) -> FrameRequest {
        FrameRequest {
            generation: 1,
            source: source(media, workspace),
            wanted: Some(alone(0, cue(0, 1000, "BURNED IN"), seek)),
            nearby: Vec::new(),
            cells: Size::new(20, 10),
        }
    }

    /// The cue `frame_request` asks for, for the tests that have to look it up in the
    /// cache or forget it again.
    fn wanted(request: &FrameRequest) -> &Cue {
        &request
            .wanted
            .as_ref()
            .expect("frame_request always asks for a cue")
            .cue
    }

    /// Runs the frame worker over a channel of its own and collects what it published.
    ///
    /// One request now answers for several cues, so the worker reports through the event
    /// channel rather than by return value; this is the shape the tests below assert on.
    /// Panics unless the worker reported that it can keep serving, which is only false
    /// when the results channel has gone — and here it has not.
    fn frames_of(
        request: &FrameRequest,
        picker: &Picker,
        live_generation: &AtomicU64,
    ) -> Vec<(usize, FrameOutcome)> {
        let (events, published) = mpsc::channel();
        assert_that!(frame(request, picker, live_generation, &events)).is_true();
        drop(events);
        published
            .into_iter()
            .map(|event| match event {
                PreviewEvent::Frame {
                    cue_index, outcome, ..
                } => (cue_index, outcome),
                other => panic!("the frame worker should only publish frames, got {other:?}"),
            })
            .collect()
    }

    /// The single frame `frames_of` published, for the tests that ask for one cue.
    fn only_frame(frames: Vec<(usize, FrameOutcome)>) -> Option<FrameOutcome> {
        assert_that!(frames.len() <= 1).is_true();
        frames.into_iter().next().map(|(_, outcome)| outcome)
    }

    /// Frames land in the test binary's shared cache directory, so anything that reads or
    /// writes one has to hold the guard `framecache` uses to keep its own directory-wide
    /// tests from deleting it mid-assertion.
    fn cache_guard() -> std::sync::RwLockReadGuard<'static, ()> {
        crate::framecache::testing::one_key()
    }

    fn forget(source: &FrameSource, cue: &Cue) {
        let (media, cue_key) = key_alone(source, cue);
        if let Some(path) = framecache::path(&media, &cue_key) {
            let _ = std::fs::remove_file(path);
        }
    }

    /// `framecache::store` for a source and cue, which is how every test seeds the cache.
    fn seed(source: &FrameSource, cue: &Cue, bytes: &[u8]) -> bool {
        let (media, cue_key) = key_alone(source, cue);
        framecache::store(&media, &cue_key, bytes)
    }

    /// `framecache::is_cached` for a source and cue.
    fn cached(source: &FrameSource, cue: &Cue) -> bool {
        let (media, cue_key) = key_alone(source, cue);
        framecache::is_cached(&media, &cue_key)
    }

    fn live(generation: u64) -> AtomicU64 {
        AtomicU64::new(generation)
    }

    #[test]
    fn preparing_a_sidecar_should_read_its_cues_without_running_ffmpeg() {
        // Arrange
        let directory = scratch("sidecar");
        let sidecar = directory.join("clip.eng.srt");
        std::fs::write(&sidecar, SRT).unwrap();
        let request = PrepareRequest {
            generation: 7,
            input: sidecar,
            stream_index: None,
            format: SubtitleFormat::SubRip,
            workspace: directory.clone(),
        };

        // Act
        let outcome = prepare(&request, &live(7));

        // Assert
        let PrepareOutcome::Ready { cues, .. } = outcome.expect("the page is still open") else {
            panic!("a readable sidecar should be ready");
        };
        assert_that!(cues.len()).is_equal_to(2);
        assert_that!(cues[1].text.as_str()).is_equal_to("second");
        assert_that!(directory.join(CUES_FILE).exists()).is_false();
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// A WebVTT sidecar is nothing this crate's parser can read, so it takes a second
    /// pass through `ffmpeg` into SubRip before the cue list ever sees it — and the result
    /// has to land in the workspace rather than beside the user's file.
    #[test]
    fn preparing_a_webvtt_sidecar_should_transcode_it_into_the_workspace_first() {
        // Arrange
        require_ffmpeg("preparing_a_webvtt_sidecar_should_transcode_it_into_the_workspace_first");
        let directory = scratch("vtt-sidecar");
        let sidecar = directory.join("clip.eng.vtt");
        std::fs::write(
            &sidecar,
            "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nfirst\n\n\
             00:00:03.000 --> 00:00:04.500\nsecond\n",
        )
        .unwrap();
        let workspace = directory.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let request = PrepareRequest {
            generation: 5,
            input: sidecar.clone(),
            stream_index: None,
            format: SubtitleFormat::WebVtt,
            workspace: workspace.clone(),
        };

        // Act
        let outcome = prepare(&request, &live(5));

        // Assert
        let PrepareOutcome::Ready { cues, .. } = outcome.expect("the page is still open") else {
            panic!("a readable WebVTT sidecar should be ready");
        };
        assert_that!(cues.len()).is_equal_to(2);
        assert_that!(cues[0].text.as_str()).is_equal_to("first");
        assert_that!(cues[0].start).is_equal_to(Duration::from_secs(1));
        assert_that!(cues[1].text.as_str()).is_equal_to("second");
        assert_that!(workspace.join(CUES_FILE).exists()).is_true();
        // The user's own file is left exactly as it was. Nothing on this page edits.
        assert_that!(std::fs::read_to_string(&sidecar).unwrap().as_str()).contains("WEBVTT");
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// An embedded WebVTT track is transcoded on the way *out* of the container, so it
    /// needs no second pass at all — one `ffmpeg` where the sidecar takes two.
    #[test]
    fn preparing_an_embedded_webvtt_track_should_transcode_it_as_it_demuxes() {
        // Arrange
        require_ffmpeg("preparing_an_embedded_webvtt_track_should_transcode_it_as_it_demuxes");
        let directory = scratch("vtt-embedded");
        let text = directory.join("source.srt");
        std::fs::write(&text, SRT).unwrap();
        let media = directory.join("clip.mkv");
        let built = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-nostdin",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=32x24:r=5:d=5",
                "-i",
            ])
            .arg(&text)
            .args([
                "-map", "0:v:0", "-map", "1:s:0", "-c:v", "ffv1", "-c:s", "webvtt",
            ])
            .arg(&media)
            .output()
            .expect("ffmpeg should be runnable");
        assert!(
            built.status.success(),
            "failed to build the fixture: {}",
            String::from_utf8_lossy(&built.stderr)
        );
        let workspace = directory.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let request = PrepareRequest {
            generation: 2,
            input: media,
            stream_index: Some(1),
            format: SubtitleFormat::WebVtt,
            workspace: workspace.clone(),
        };

        // Act
        let outcome = prepare(&request, &live(2));

        // Assert
        let PrepareOutcome::Ready { cues, .. } = outcome.expect("the page is still open") else {
            panic!("an embedded WebVTT track should be ready");
        };
        assert_that!(cues.len()).is_equal_to(2);
        assert_that!(cues[0].text.as_str()).is_equal_to("first");
        assert_that!(cues[1].start).is_equal_to(Duration::from_secs(3));
        // Staged as SubRip, whatever the source was. Written to `cues.vtt` instead, the
        // WebVTT muxer would refuse the `subrip` the extraction asks it to write.
        let staged = std::fs::read_to_string(workspace.join(CUES_FILE)).unwrap();
        assert_that!(staged.as_str()).contains("00:00:01,000 --> 00:00:02,000");
        assert_that!(staged.contains("WEBVTT")).is_false();
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// An embedded ASS track is copied out bit for bit, so the styles and `PlayRes` its
    /// cues are drawn by arrive with them. A transcode to SubRip would leave the cues
    /// readable and the track unrenderable as itself.
    #[test]
    fn preparing_an_embedded_ass_track_should_keep_the_header_that_styles_it() {
        // Arrange
        require_ffmpeg("preparing_an_embedded_ass_track_should_keep_the_header_that_styles_it");
        let directory = scratch("ass-embedded");
        let script = directory.join("source.ass");
        std::fs::write(&script, ASS).unwrap();
        let media = directory.join("clip.mkv");
        let built = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-nostdin",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=32x24:r=5:d=5",
                "-i",
            ])
            .arg(&script)
            .args([
                "-map", "0:v:0", "-map", "1:s:0", "-c:v", "ffv1", "-c:s", "copy",
            ])
            .arg(&media)
            .output()
            .expect("ffmpeg should be runnable");
        assert!(
            built.status.success(),
            "failed to build the fixture: {}",
            String::from_utf8_lossy(&built.stderr)
        );
        let workspace = directory.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let request = PrepareRequest {
            generation: 4,
            input: media,
            stream_index: Some(1),
            format: SubtitleFormat::Ass,
            workspace: workspace.clone(),
        };

        // Act
        let outcome = prepare(&request, &live(4));

        // Assert: the cue, with its markup stripped for the list.
        let PrepareOutcome::Ready { cues, style } = outcome.expect("the page is still open") else {
            panic!("an embedded ASS track should be ready");
        };
        assert_that!(cues.len()).is_equal_to(1);
        assert_that!(cues[0].text.as_str()).is_equal_to("a sign");

        // Assert: and the line the renderer needs, alongside the header that draws it.
        assert_that!(cues[0].dialogue[0].as_str()).contains("{\\pos(320,10)}");
        assert_that!(cues[0].dialogue[0].as_str()).contains("Sign");
        let CueStyle::Ass {
            header,
            events_format,
        } = &style
        else {
            panic!("an ASS track should carry an ASS style, got {style:?}");
        };
        assert_that!(header.as_str()).contains("PlayResX: 384");
        assert_that!(header.as_str()).contains("Style: Sign,");
        assert_that!(events_format.as_str()).contains("Start, End");

        // Assert: staged as ASS, not SubRip — the name is what libass reads it by.
        assert_that!(workspace.join("cues.ass").exists()).is_true();
        assert_that!(workspace.join(CUES_FILE).exists()).is_false();
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// The extraction copies what this crate parses itself and transcodes what it does
    /// not. Getting this wrong is silent in one direction: a `copy` of a WebVTT track
    /// writes a file the SubRip parser reads as zero cues, which the page shows as an
    /// empty track rather than as a failure.
    #[test]
    fn an_extraction_should_copy_only_the_formats_this_crate_reads_itself() {
        // Act / Assert
        let codec = |format| {
            let command = extract_command(
                Path::new("/media/show.mkv"),
                4,
                format,
                Path::new("/tmp/o.srt"),
            );
            let arguments: Vec<String> = command
                .get_args()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect();
            let at = arguments
                .iter()
                .position(|argument| argument == "-c:s")
                .expect("an extraction always names a subtitle codec");
            arguments[at + 1].clone()
        };
        assert_that!(codec(SubtitleFormat::SubRip).as_str()).is_equal_to("copy");
        assert_that!(codec(SubtitleFormat::WebVtt).as_str()).is_equal_to("subrip");
        assert_that!(codec(SubtitleFormat::MovText).as_str()).is_equal_to("subrip");
    }

    /// The page distinguishes "nothing to show" from "something went wrong", so an
    /// unreadable file has to fail rather than come back as an empty track.
    #[test]
    fn preparing_a_sidecar_that_cannot_be_read_should_fail_and_name_the_file() {
        // Arrange
        let directory = scratch("missing-sidecar");
        let request = PrepareRequest {
            generation: 1,
            input: directory.join("gone.eng.srt"),
            stream_index: None,
            format: SubtitleFormat::SubRip,
            workspace: directory.clone(),
        };

        // Act
        let outcome = prepare(&request, &live(1));

        // Assert
        let Some(PrepareOutcome::Failed(message)) = outcome else {
            panic!("a missing sidecar should fail, got {outcome:?}");
        };
        assert_that!(message.as_str()).contains("gone.eng.srt");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn preparing_an_embedded_track_should_extract_it_into_the_workspace_and_parse_it() {
        // Arrange
        require_ffmpeg(
            "preparing_an_embedded_track_should_extract_it_into_the_workspace_and_parse_it",
        );
        let directory = scratch("embedded");
        let media = media_with_subtitle(&directory);
        let workspace = directory.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let request = PrepareRequest {
            generation: 3,
            input: media,
            stream_index: Some(1),
            format: SubtitleFormat::SubRip,
            workspace: workspace.clone(),
        };

        // Act
        let outcome = prepare(&request, &live(3));

        // Assert
        let PrepareOutcome::Ready { cues, .. } = outcome.expect("the page is still open") else {
            panic!("an embedded subrip track should be ready");
        };
        assert_that!(cues.len()).is_equal_to(2);
        assert_that!(cues[0].text.as_str()).is_equal_to("first");
        assert_that!(cues[0].start).is_equal_to(Duration::from_secs(1));
        assert_that!(workspace.join(CUES_FILE).exists()).is_true();
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// The absolute ffprobe index, not the `0:s:N` per-type form — a file whose subtitle
    /// is the third stream would otherwise extract whichever track happens to be third
    /// among the subtitles, or nothing at all.
    #[test]
    fn an_embedded_extraction_should_map_the_absolute_stream_index() {
        // Act
        let command = extract_command(
            Path::new("/media/show.mkv"),
            4,
            SubtitleFormat::SubRip,
            Path::new("/tmp/w/cues.srt"),
        );

        // Assert
        let arguments: Vec<String> = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        assert_that!(arguments).is_equal_to(
            [
                "-v",
                "error",
                "-nostdin",
                "-y",
                "-i",
                "/media/show.mkv",
                "-map",
                "0:4",
                "-c:s",
                "copy",
                "/tmp/w/cues.srt",
            ]
            .map(str::to_string)
            .to_vec(),
        );
    }

    #[test]
    fn an_ffmpeg_failure_should_report_what_it_said() {
        // Arrange
        require_ffmpeg("an_ffmpeg_failure_should_report_what_it_said");
        let directory = scratch("bad-index");
        let media = media_with_subtitle(&directory);
        let request = PrepareRequest {
            generation: 1,
            // No such stream: the fixture has two.
            input: media,
            stream_index: Some(9),
            format: SubtitleFormat::SubRip,
            workspace: directory.clone(),
        };

        // Act
        let outcome = prepare(&request, &live(1));

        // Assert
        let Some(PrepareOutcome::Failed(message)) = outcome else {
            panic!("an impossible mapping should fail, got {outcome:?}");
        };
        assert_that!(message.as_str()).contains("Could not read this subtitle track");
        assert_that!(message.len() > "Could not read this subtitle track.".len()).is_true();
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// Leaving the page before the extraction starts must not spawn `ffmpeg` at all.
    #[test]
    fn a_request_for_a_page_already_closed_should_not_be_prepared() {
        // Arrange
        let directory = scratch("superseded");
        let sidecar = directory.join("clip.eng.srt");
        std::fs::write(&sidecar, SRT).unwrap();
        let request = PrepareRequest {
            generation: 2,
            input: sidecar,
            stream_index: None,
            format: SubtitleFormat::SubRip,
            workspace: directory.clone(),
        };

        // Act: the live page has moved on to generation 3.
        let outcome = prepare(&request, &live(3));

        // Assert
        assert_that!(outcome).is_none();
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// Closing the page during a slow extraction has to kill it. Without this, leaving
    /// the page on a network mount leaves an `ffmpeg` demuxing the whole file for a page
    /// that no longer exists.
    #[test]
    fn abandoning_a_running_command_should_kill_it_rather_than_wait_for_it() {
        // Arrange: a command that would otherwise outlive the test by half a minute.
        let mut command = Command::new("sleep");
        command.arg("30");
        let started = Instant::now();

        // Act
        let outcome = run_cancellable(&mut command, &|| true);

        // Assert
        assert_that!(matches!(outcome, RunOutcome::Abandoned)).is_true();
        assert_that!(started.elapsed() < Duration::from_secs(10)).is_true();
    }

    /// The poll loop only reaches its second iteration for a command that is still
    /// running, which is the path a real extraction spends all its time in.
    #[test]
    fn a_command_that_takes_a_moment_should_be_waited_out() {
        // Arrange
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 0.1; printf out; printf err >&2"]);

        // Act
        let outcome = run_cancellable(&mut command, &|| false);

        // Assert
        let RunOutcome::Finished(output) = outcome else {
            panic!("the command should have finished");
        };
        assert_that!(output.status.success()).is_true();
        assert_that!(String::from_utf8_lossy(&output.stdout).as_ref()).is_equal_to("out");
        assert_that!(String::from_utf8_lossy(&output.stderr).as_ref()).is_equal_to("err");
    }

    #[test]
    fn a_program_that_is_not_installed_should_be_reported_as_missing() {
        // Arrange
        let mut command = Command::new("reel-tui-definitely-not-a-real-program");

        // Act
        let outcome = run_cancellable(&mut command, &|| false);

        // Assert
        let RunOutcome::Failed(message) = outcome else {
            panic!("an absent program should fail");
        };
        assert_that!(message.as_str()).contains("was not found in PATH");
    }

    /// A directory is spawnable-looking and unexecutable, which is the failure shape
    /// that is not `NotFound`.
    #[test]
    fn a_program_that_cannot_be_started_should_report_the_underlying_error() {
        // Arrange
        let directory = scratch("unspawnable");
        let mut command = Command::new(&directory);

        // Act
        let outcome = run_cancellable(&mut command, &|| false);

        // Assert
        let RunOutcome::Failed(message) = outcome else {
            panic!("an unexecutable program should fail");
        };
        assert_that!(message.as_str()).contains("Could not start");
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// The endings a real extraction cannot be steered into on demand: the page closing
    /// mid-run, and `ffmpeg` failing to start at all.
    #[test]
    fn an_extraction_that_never_produced_a_file_should_not_be_read_as_cues() {
        // Arrange
        let directory = scratch("extraction-endings");
        let staged = directory.join(CUES_FILE);
        let succeeded = Command::new("sh").args(["-c", "exit 0"]).output().unwrap();

        // Act
        let abandoned = extracted(RunOutcome::Abandoned, &staged);
        let failed = extracted(RunOutcome::Failed("ffmpeg is missing".to_string()), &staged);
        // A run that "succeeded" without leaving a file behind, which is the shape a
        // silently-empty mapping takes.
        let empty = extracted(RunOutcome::Finished(succeeded), &staged);

        // Assert
        assert_that!(abandoned).is_none();
        assert_that!(failed).is_equal_to(Some(PrepareOutcome::Failed(
            "ffmpeg is missing".to_string(),
        )));
        let Some(PrepareOutcome::Failed(message)) = empty else {
            panic!("a missing extraction should fail rather than read as empty");
        };
        assert_that!(message.as_str()).contains(CUES_FILE);
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// The worker has to stop when its results have nowhere to go. Without this it would
    /// keep spawning an `ffmpeg` per queued request for an application that has exited.
    #[test]
    fn the_worker_should_shut_down_once_its_results_can_no_longer_be_received() {
        // Arrange
        let directory = scratch("worker-shutdown");
        let sidecar = directory.join("clip.eng.srt");
        std::fs::write(&sidecar, SRT).unwrap();
        let (handles, events) = spawn_preview_workers(None);
        // No picker means no frame worker and no background pass either — there would be
        // nothing to draw their output with — and asking anyway is a no-op rather than a
        // panic: the page simply leaves its preview pane empty.
        assert_that!(handles.draws_frames()).is_false();
        handles.request_frame(frame_request(&sidecar, &directory, Duration::ZERO));
        handles.request_warm(WarmRequest {
            generation: 1,
            source: source(&sidecar, &directory),
            cues: vec![cue(0, 1000, "unused")],
            duration: Duration::from_secs(6),
            cache_tracks: usize::MAX,
        });
        let request = |generation| PrepareRequest {
            generation,
            input: sidecar.clone(),
            stream_index: None,
            format: SubtitleFormat::SubRip,
            workspace: directory.clone(),
        };

        // Act
        drop(events);
        handles.request(request(1));

        // Assert: the worker's own end of the request channel closes when it stops.
        let deadline = Instant::now() + Duration::from_secs(10);
        while handles.prepare_tx.send(request(1)).is_ok() {
            assert_that!(Instant::now() < deadline).is_true();
            std::thread::sleep(Duration::from_millis(10));
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// The whole reason the burn is done against a rewritten one-cue file: with `-ss`
    /// before `-i` the output timestamps restart near zero, so a cue covering the first
    /// ten minutes lands on whatever frame emerges no matter how the seek rounds or what
    /// the container's `start_time` is.
    #[test]
    fn the_cue_being_previewed_should_be_retimed_to_cover_the_whole_grab() {
        // Arrange: a cue nowhere near the start of the media, so a staging that kept the
        // original times would be visibly different.
        let cue = cue(725_000, 727_000, "Hello\nthere\n");

        // Act
        let staged = CueStyle::SubRip.stage(std::slice::from_ref(&cue));

        // Assert
        assert_that!(staged.as_str())
            .is_equal_to("1\n00:00:00,000 --> 00:10:00,000\nHello\nthere\n\n");
    }

    /// Everything on screen at the grabbed instant goes into the file, not the selected cue
    /// alone — the frame is meant to be what a viewer would see there.
    ///
    /// Numbered in order, because a SubRip file whose blocks all claim to be block 1 is the
    /// kind of thing a reader trips over even where a parser does not.
    #[test]
    fn a_grab_should_stage_every_cue_on_screen_rather_than_the_selected_one() {
        // Arrange: a line with two effect cues over it, as a karaoke or typeset track has.
        let cues = vec![cue(2000, 3000, "under"), cue(2400, 2600, "over")];

        // Act
        let staged = CueStyle::SubRip.stage(&cues);

        // Assert: both lines, both covering the whole grab, so the picture cannot turn on
        // how the seek rounded.
        assert_that!(staged.as_str()).is_equal_to(
            "1\n00:00:00,000 --> 00:10:00,000\nunder\n\n\
             2\n00:00:00,000 --> 00:10:00,000\nover\n\n",
        );
    }

    /// The same for ASS, where it matters most: a typeset line is routinely a dozen events
    /// sharing a moment, each drawing part of one effect, and any one of them alone is a
    /// fraction of a picture the viewer never sees.
    #[test]
    fn an_ass_grab_should_stage_every_cue_on_screen_with_its_own_styling() {
        // Arrange: the fixture's sign, plus a second event over it in the other style.
        let script = crate::cue::parse_ass(ASS);
        let mut second = script.cues[0].clone();
        second.dialogue = vec![
            "Dialogue: 0,0:00:01.20,0:00:01.80,Default,,0,0,0,,{\\fad(100,100)}and a line"
                .to_string(),
        ];
        let cues = vec![script.cues[0].clone(), second];

        // Act
        let staged = ass_style().stage(&cues);

        // Assert: one script, one `[Events]` section, both lines in it, each keeping its own
        // style name and overrides and each retimed to cover the grab.
        assert_that!(staged.matches("[Events]").count()).is_equal_to(1);
        assert_that!(staged.as_str())
            .contains("Dialogue: 0,0:00:00.00,0:10:00.00,Sign,,0,0,0,,{\\pos(320,10)}a sign");
        assert_that!(staged.as_str()).contains(
            "Dialogue: 0,0:00:00.00,0:10:00.00,Default,,0,0,0,,{\\fad(100,100)}and a line",
        );
    }

    /// The scrub playback wants the exact opposite of the grab: the span runs from before
    /// the cue to after it, and watching the line arrive and leave against the picture is
    /// the whole point of playing it. A cue staged to cover everything would sit on screen
    /// for the entire span and say nothing about its timing.
    #[test]
    fn a_cue_being_played_should_be_retimed_to_where_it_falls_inside_the_span() {
        // Arrange: the cue runs 12.0–14.5 s, and the span starts two seconds before it.
        let cue = cue(12_000, 14_500, "Hello");

        // Act
        let staged =
            CueStyle::SubRip.stage_span(std::slice::from_ref(&cue), Duration::from_secs(10));

        // Assert
        assert_that!(staged.as_str()).is_equal_to("1\n00:00:02,000 --> 00:00:04,500\nHello\n\n");
    }

    /// A folded cue is several events that draw one line, so staging it stages all of them.
    ///
    /// Dropping any would preview a fraction of the effect — which is the whole reason
    /// `cue::collapse` folds them rather than discarding the duplicates it finds.
    #[test]
    fn a_folded_cue_should_stage_every_line_that_draws_it() {
        // Arrange: the shape a karaoke effect really has — one cue, three events.
        let mut folded = crate::cue::parse_ass(ASS).cues[0].clone();
        folded.dialogue = vec![
            "Dialogue: 0,0:00:01.00,0:00:02.00,Sign,,0,0,0,,{\\fscx120}wo".to_string(),
            "Dialogue: 0,0:00:01.00,0:00:02.00,Sign,,0,0,0,,{\\fscx140}wo".to_string(),
            "Dialogue: 0,0:00:01.00,0:00:02.00,Sign,,0,0,0,,{\\fscx160}wo".to_string(),
        ];

        // Act
        let staged = ass_style().stage(std::slice::from_ref(&folded));

        // Assert: all three, all retimed alike, in the order the file had them.
        let events: Vec<&str> = staged
            .lines()
            .filter(|line| line.starts_with("Dialogue:"))
            .collect();
        assert_that!(events.len()).is_equal_to(3);
        for (event, scale) in events.iter().zip(["fscx120", "fscx140", "fscx160"]) {
            assert_that!(*event).contains(scale);
            assert_that!(*event).contains("0:00:00.00,0:10:00.00");
        }
    }

    /// **Relative offsets have to survive the rebase**, or an effect built from staggered
    /// lines plays as several lines arriving at once instead of as the effect. Each cue is
    /// moved by the same amount — the span's start — rather than each being placed
    /// somewhere, which is what keeps the gaps between them intact.
    #[test]
    fn a_span_should_keep_the_gaps_between_the_cues_it_stages() {
        // Arrange: three cues a quarter of a second apart, in a span starting at 10 s.
        let cues = vec![
            cue(11_000, 12_000, "first"),
            cue(11_250, 12_000, "second"),
            cue(11_500, 12_000, "third"),
        ];

        // Act
        let staged = CueStyle::SubRip.stage_span(&cues, Duration::from_secs(10));

        // Assert: one second, then 1.25, then 1.5 — the same quarter-second steps.
        assert_that!(staged.as_str()).is_equal_to(
            "1\n00:00:01,000 --> 00:00:02,000\nfirst\n\n\
             2\n00:00:01,250 --> 00:00:02,000\nsecond\n\n\
             3\n00:00:01,500 --> 00:00:02,000\nthird\n\n",
        );
    }

    /// A cue that began before the span is clamped to its start rather than dropped or
    /// given a negative time, which an ASS timestamp cannot hold. It is on screen from the
    /// first frame, which is what the viewer would see too.
    #[test]
    fn a_cue_already_on_screen_when_a_span_opens_should_start_with_it() {
        // Arrange: a cue running 8–11 s, in a span starting at 10 s.
        let cues = vec![cue(8_000, 11_000, "already here")];

        // Act
        let staged = CueStyle::SubRip.stage_span(&cues, Duration::from_secs(10));

        // Assert
        assert_that!(staged.as_str())
            .is_equal_to("1\n00:00:00,000 --> 00:00:01,000\nalready here\n\n");
    }

    /// An ASS cue is retimed the same way, through the file's own `Format:` line — and
    /// keeps its style name and its positioning override, which is what makes previewing
    /// an ASS track worth doing at all.
    #[test]
    fn an_ass_cue_being_played_should_keep_its_styling_at_its_place_in_the_span() {
        // Arrange
        let script = crate::cue::parse_ass(ASS);

        // Act: the cue runs 1.0–2.0 s and the span starts half a second before it, so it
        // should land at 0.5–1.5 s inside the span.
        let staged = ass_style().stage_span(&script.cues[..1], Duration::from_millis(500));

        // Assert
        assert_that!(staged.as_str())
            .contains("Dialogue: 0,0:00:00.50,0:00:01.50,Sign,,0,0,0,,{\\pos(320,10)}a sign");
        assert_that!(staged.as_str()).contains("PlayResX: 384");
        assert_that!(staged.as_str()).contains("Style: Sign,Impact,40,8");
    }

    /// The `subtitles=` filter value needs three layers of quoting if it is ever a real
    /// path. Running in the workspace and naming the staged file makes it a constant, so
    /// this asserts the two halves of that arrangement together — neither is any use
    /// without the other.
    #[test]
    fn the_burned_in_cue_should_be_named_relative_to_the_workspace_it_runs_in() {
        // Act
        let command = frame_command(
            Path::new("/media/it's a show; [2024].mkv"),
            Duration::from_millis(2500),
            (640, 480),
            Path::new("/tmp/reel-tui-preview/7-1"),
            &cue_file(),
        );

        // Assert
        assert_that!(command.get_current_dir().map(Path::to_path_buf))
            .is_equal_to(Some(PathBuf::from("/tmp/reel-tui-preview/7-1")));
        let arguments: Vec<String> = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        assert_that!(arguments).is_equal_to(
            [
                "-v",
                "error",
                "-nostdin",
                "-y",
                "-ss",
                "2.500",
                "-i",
                "/media/it's a show; [2024].mkv",
                "-map",
                "0:v:0",
                "-frames:v",
                "1",
                "-vf",
                "subtitles=cue.srt,scale=640:480:force_original_aspect_ratio=decrease",
                "-f",
                "image2pipe",
                "-vcodec",
                "mjpeg",
                "-pix_fmt",
                "yuvj420p",
                "-q:v",
                "2",
                "-",
            ]
            .map(str::to_string)
            .to_vec(),
        );
    }

    #[test]
    fn drawing_a_frame_should_burn_the_cue_in_and_encode_it_for_the_pane() {
        // Arrange
        require_ffmpeg("drawing_a_frame_should_burn_the_cue_in_and_encode_it_for_the_pane");
        let directory = scratch("frame");
        let media = video(&directory);
        let request = frame_request(&media, &directory, Duration::from_millis(2500));

        // Act
        let outcome = only_frame(frames_of(&request, &Picker::halfblocks(), &live(1)));

        // Assert
        let Some(FrameOutcome::Ready(protocol)) = outcome else {
            panic!("a video with a burnable cue should produce a frame, got {outcome:?}");
        };
        assert_that!(protocol.size().width <= 20 && protocol.size().height <= 10).is_true();
        assert_that!(protocol.size().width > 0 && protocol.size().height > 0).is_true();
        // The cue is staged where the filter expects to find it: beside the command's
        // working directory, under the bare name it was given.
        let staged = std::fs::read_to_string(directory.join(cue_file())).unwrap();
        assert_that!(staged.as_str()).contains("BURNED IN");
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// Seeking past the end leaves `ffmpeg` exiting successfully having written nothing,
    /// which is why `App` holds the seek back from the very end. Reaching it anyway must
    /// report a failure rather than hand a zero-byte buffer to the decoder.
    #[test]
    fn a_seek_past_the_end_should_fail_rather_than_produce_an_empty_frame() {
        // Arrange
        require_ffmpeg("a_seek_past_the_end_should_fail_rather_than_produce_an_empty_frame");
        let directory = scratch("frame-past-end");
        let media = video(&directory);
        let request = frame_request(&media, &directory, Duration::from_secs(60));

        // Act
        let outcome = only_frame(frames_of(&request, &Picker::halfblocks(), &live(1)));

        // Assert
        let Some(FrameOutcome::Failed(message)) = outcome else {
            panic!("a frame past the end of the media should fail, got {outcome:?}");
        };
        assert_that!(message.as_str()).contains("Unreadable frame");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_frame_for_a_file_ffmpeg_cannot_open_should_report_what_it_said() {
        // Arrange
        require_ffmpeg("a_frame_for_a_file_ffmpeg_cannot_open_should_report_what_it_said");
        let directory = scratch("frame-missing-media");
        let request = frame_request(
            &directory.join("gone.mkv"),
            &directory,
            Duration::from_secs(1),
        );

        // Act
        let outcome = only_frame(frames_of(&request, &Picker::halfblocks(), &live(1)));

        // Assert
        let Some(FrameOutcome::Failed(message)) = outcome else {
            panic!("an absent media file should fail, got {outcome:?}");
        };
        assert_that!(message.as_str()).contains("Could not draw this frame");
        assert_that!(message.len() > "Could not draw this frame.".len()).is_true();
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// The workspace is removed the instant the page closes, so a frame request in flight
    /// can find it gone. That has to report, not panic.
    #[test]
    fn a_frame_with_nowhere_to_stage_the_cue_should_fail() {
        // Arrange
        let directory = scratch("frame-no-workspace");
        let workspace = directory.join("already-gone");
        let request = frame_request(&directory.join("clip.mkv"), &workspace, Duration::ZERO);

        // Act
        let outcome = only_frame(frames_of(&request, &Picker::halfblocks(), &live(1)));

        // Assert
        let Some(FrameOutcome::Failed(message)) = outcome else {
            panic!("a missing workspace should fail, got {outcome:?}");
        };
        assert_that!(message.as_str()).contains("Could not stage the cue");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_frame_for_a_page_already_closed_should_not_be_drawn() {
        // Arrange
        let directory = scratch("frame-superseded");
        let request = frame_request(&directory.join("clip.mkv"), &directory, Duration::ZERO);

        // Act: the live page has moved on past this request's generation.
        let outcome = only_frame(frames_of(&request, &Picker::halfblocks(), &live(9)));

        // Assert: nothing was even staged, let alone spawned.
        assert_that!(outcome).is_none();
        assert_that!(directory.join(cue_file()).exists()).is_false();
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// The endings a real grab cannot be steered into on demand, plus the one that looks
    /// like success and is not: `ffmpeg` exiting zero having written nothing.
    #[test]
    fn a_grab_that_produced_no_image_should_not_be_reported_as_a_frame() {
        // Arrange
        let _guard = cache_guard();
        let media = format!("preview-endings-{}", std::process::id());
        let key = "cue";
        let succeeded = Command::new("sh").args(["-c", "exit 0"]).output().unwrap();
        let refused = Command::new("sh")
            .args(["-c", "echo 'Invalid stream specifier' >&2; exit 1"])
            .output()
            .unwrap();

        // Act
        let abandoned = grabbed(RunOutcome::Abandoned, &media, key);
        let unstartable = grabbed(
            RunOutcome::Failed("ffmpeg was not found in PATH.".to_string()),
            &media,
            key,
        );
        let empty = grabbed(RunOutcome::Finished(succeeded), &media, key);
        let rejected = grabbed(RunOutcome::Finished(refused), &media, key);

        // Assert
        assert_that!(matches!(abandoned, PngOutcome::Abandoned)).is_true();
        let PngOutcome::Failed(unstartable) = unstartable else {
            panic!("a command that never started should fail");
        };
        assert_that!(unstartable.as_str()).contains("not found in PATH");
        let PngOutcome::Ready(empty) = empty else {
            panic!("an empty grab is passed on to the decoder, which is what rejects it");
        };
        let FrameOutcome::Failed(message) =
            encode(&empty, &Picker::halfblocks(), Size::new(20, 10))
        else {
            panic!("an empty grab should fail rather than draw nothing");
        };
        assert_that!(message.as_str()).contains("Unreadable frame");
        let PngOutcome::Failed(rejected) = rejected else {
            panic!("a refused grab should fail");
        };
        assert_that!(rejected.as_str()).contains("Invalid stream specifier");
        // And nothing empty or failed was ever written to the cache: a blank file there
        // would make one bad grab a permanently blank cue.
        assert_that!(framecache::is_cached(&media, key)).is_false();
    }

    /// The worker loop itself: the coalescing receive, the picker it owns, and the event
    /// it publishes. Everything else here calls `frame` directly, which is one layer
    /// below the thread that has to keep answering after the first request.
    #[test]
    fn the_frame_worker_should_answer_with_a_drawn_frame() {
        // Arrange
        require_ffmpeg("the_frame_worker_should_answer_with_a_drawn_frame");
        let directory = scratch("frame-worker");
        let media = video(&directory);
        let (handles, events) = spawn_preview_workers(Some(Picker::halfblocks()));
        assert_that!(handles.draws_frames()).is_true();

        // Act: one request, because which of several the loop settles on is a race the
        // worker wins as often as the test does — the coalescing itself is asserted
        // directly in `a_queue_of_frame_requests_should_collapse_to_the_newest`.
        handles.abandon(1);
        handles.request_frame(FrameRequest {
            wanted: Some(alone(
                3,
                cue(0, 1000, "BURNED IN"),
                Duration::from_millis(2500),
            )),
            ..frame_request(&media, &directory, Duration::from_millis(2500))
        });

        // Assert
        let event = events
            .recv_timeout(Duration::from_secs(30))
            .expect("the worker should draw a frame");
        let PreviewEvent::Frame {
            generation,
            cue_index,
            outcome,
        } = event
        else {
            panic!("a frame request should answer with a frame, not with cues");
        };
        assert_that!(generation).is_equal_to(1);
        assert_that!(cue_index).is_equal_to(3);
        assert_that!(matches!(outcome, FrameOutcome::Ready(_))).is_true();
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// A frame for a page that has closed is dropped by the worker rather than published,
    /// and the worker stops entirely once its results have nowhere to go — otherwise it
    /// would keep seeking a container for an application that has exited.
    #[test]
    fn the_frame_worker_should_skip_a_closed_page_and_stop_when_nobody_is_listening() {
        // Arrange
        let directory = scratch("frame-worker-stale");
        let (handles, events) = spawn_preview_workers(Some(Picker::halfblocks()));
        let request = |generation| FrameRequest {
            generation,
            ..frame_request(&directory.join("clip.mkv"), &directory, Duration::ZERO)
        };

        // Act: the live page is generation 5, so a request for 2 is already stale — and
        // stale is decided before anything is staged or spawned, so no media is needed.
        handles.abandon(5);
        handles.request_frame(request(2));
        // Given time to be picked up on its own: sent back to back, the coalescing
        // receive would discard the stale request without the worker ever looking at it,
        // which is a different path from the one under test. Nothing here is timing
        // dependent — the assertion below holds either way — only which line proves it.
        std::thread::sleep(Duration::from_millis(200));
        handles.request_frame(request(5));

        // Assert: only the live page's answer arrives, and it is a failure because the
        // media does not exist — what matters is which generation got answered at all.
        let event = events
            .recv_timeout(Duration::from_secs(30))
            .expect("the live request should be answered");
        let PreviewEvent::Frame { generation, .. } = event else {
            panic!("a frame request should answer with a frame");
        };
        assert_that!(generation).is_equal_to(5);

        // Act / Assert: with the results channel gone, the worker stops rather than
        // draining the queue it has been handed.
        drop(events);
        handles.request_frame(request(5));
        let deadline = Instant::now() + Duration::from_secs(10);
        while handles
            .frame_tx
            .as_ref()
            .expect("this picker has a frame worker")
            .send(request(5))
            .is_ok()
        {
            assert_that!(Instant::now() < deadline).is_true();
            std::thread::sleep(Duration::from_millis(10));
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// Both of these appear in test failures and in `App`'s own debug output, where
    /// "some image" is exactly what a reader needs and `Protocol` itself offers nothing.
    #[test]
    fn a_frame_outcome_should_describe_itself_by_size_or_by_reason() {
        // Arrange
        let ready = FrameOutcome::Ready(Box::new(
            Picker::halfblocks()
                .new_protocol(
                    image::DynamicImage::new_rgb8(40, 40),
                    Size::new(4, 2),
                    Resize::Fit(None),
                )
                .unwrap(),
        ));

        // Act / Assert
        assert_that!(format!("{ready:?}").as_str()).contains("Ready");
        assert_that!(format!("{ready:?}").as_str()).contains("width");
        assert_that!(format!("{:?}", FrameOutcome::Failed("no libass".to_string())).as_str())
            .is_equal_to("Failed(\"no libass\")");
    }

    /// Walking the cue list queues a request per cue. Serving them in turn would leave
    /// the preview several selections behind the cursor, each one an ffmpeg seek for a
    /// cue nobody is looking at any more.
    #[test]
    fn a_queue_of_frame_requests_should_collapse_to_the_newest() {
        // Arrange
        let (sender, receiver) = mpsc::channel();
        for cue_index in 1..=4 {
            sender.send(cue_index).unwrap();
        }

        // Act
        let request = newest(0, &receiver);

        // Assert
        assert_that!(request).is_equal_to(4);
        assert_that!(newest(5, &receiver)).is_equal_to(5);
    }

    /// Coalescing must not throw a grab away. The page clears its request the moment it
    /// hands one over, so a `wanted` dropped here is a preview pane that stays empty until
    /// the cursor happens to move again — which two quick presses of `l` across an overlap
    /// group reproduce, the first request's answers queueing a neighbours-only refill
    /// behind the second press's grab.
    #[test]
    fn coalescing_frame_requests_should_carry_a_discarded_grab_forward() {
        // Arrange: a grab, then a refill that carries none.
        let (sender, receiver) = mpsc::channel();
        let request = |wanted: Option<usize>| FrameRequest {
            generation: 7,
            source: source(Path::new("/media.mkv"), Path::new("/workspace")),
            wanted: wanted.map(|index| {
                alone(
                    index,
                    cue(index as u64 * 1000, index as u64 * 1000 + 500, "line"),
                    Duration::from_millis(index as u64 * 1000),
                )
            }),
            nearby: Vec::new(),
            cells: Size::new(40, 20),
        };
        sender.send(request(None)).unwrap();

        // Act
        let coalesced = newest_frame(request(Some(3)), &receiver);

        // Assert: the refill won, but it is carrying the grab the page is still waiting on.
        assert_that!(coalesced.wanted.map(|target| target.cue_index)).is_equal_to(Some(3));
    }

    /// A newer grab is the cursor having moved, and wins outright — adopting the older one
    /// too would render a cue nobody is looking at.
    #[test]
    fn coalescing_frame_requests_should_prefer_the_newest_grab() {
        // Arrange
        let (sender, receiver) = mpsc::channel();
        let request = |generation: u64, wanted: Option<usize>| FrameRequest {
            generation,
            source: source(Path::new("/media.mkv"), Path::new("/workspace")),
            wanted: wanted.map(|index| {
                alone(
                    index,
                    cue(index as u64 * 1000, index as u64 * 1000 + 500, "line"),
                    Duration::from_millis(index as u64 * 1000),
                )
            }),
            nearby: Vec::new(),
            cells: Size::new(40, 20),
        };
        sender.send(request(7, Some(5))).unwrap();

        // Act / Assert
        let coalesced = newest_frame(request(7, Some(3)), &receiver);
        assert_that!(coalesced.wanted.map(|target| target.cue_index)).is_equal_to(Some(5));

        // Act / Assert: and a grab is never carried across generations, which would
        // resurrect one for a page the user has already left.
        sender.send(request(8, None)).unwrap();
        let coalesced = newest_frame(request(7, Some(3)), &receiver);
        assert_that!(coalesced.wanted.is_none()).is_true();
    }

    /// The protocol is built on the worker thread, off the event loop — a kitty protocol
    /// is base64 over the whole image. That is only sound while these stay `Send`; if a
    /// future release changes that, the fix is to hand the decoded image to the UI thread
    /// and build the protocol there.
    #[test]
    fn the_image_types_the_worker_hands_over_should_be_sendable() {
        // Act / Assert: a compile-time assertion, deliberately not a runtime one.
        fn assert_send<T: Send>() {}
        assert_send::<Protocol>();
        assert_send::<Picker>();
        assert_send::<image::DynamicImage>();
    }

    #[test]
    fn a_silent_failure_should_still_say_something() {
        // Act
        let quiet = command_failure("Could not read this subtitle track", b"   \n\n");
        let loud = command_failure("Could not read this subtitle track", b"\nStream map fail\n");

        // Assert
        assert_that!(quiet.as_str()).is_equal_to("Could not read this subtitle track.");
        assert_that!(loud.as_str())
            .is_equal_to("Could not read this subtitle track: Stream map fail");
    }

    #[test]
    fn a_path_without_a_filename_should_still_label_itself() {
        // Act / Assert
        assert_that!(label(Path::new("/media/clip.eng.srt")).as_str()).is_equal_to("clip.eng.srt");
        assert_that!(label(Path::new("..")).as_str()).is_equal_to("..");
    }

    /// The whole channel round trip, since the worker thread's own loop — sending,
    /// skipping an abandoned request, and shutting down when the receiver goes — is not
    /// reachable through `prepare` alone.
    #[test]
    fn the_worker_should_answer_a_request_and_skip_one_whose_page_has_closed() {
        // Arrange
        let directory = scratch("worker");
        let sidecar = directory.join("clip.eng.srt");
        std::fs::write(&sidecar, SRT).unwrap();
        let (handles, events) = spawn_preview_workers(None);
        let request = |generation| PrepareRequest {
            generation,
            input: sidecar.clone(),
            stream_index: None,
            format: SubtitleFormat::SubRip,
            workspace: directory.clone(),
        };

        // Act: the first request is queued behind the raw sender rather than through
        // `request`, so it is never the live generation and its skip is not a race — the
        // ordinary path would set it live and then have to lose a footrace to abandon it.
        handles.abandon(9);
        handles.prepare_tx.send(request(1)).unwrap();
        handles.request(request(3));

        // Assert: the answer for the closed page never arrives, only the live one's.
        let event = events
            .recv_timeout(Duration::from_secs(10))
            .expect("the worker should answer the live request");
        let PreviewEvent::Prepared {
            generation,
            outcome,
        } = event
        else {
            panic!("a prepare request should answer with cues, not a frame");
        };
        assert_that!(generation).is_equal_to(3);
        assert_that!(matches!(outcome, PrepareOutcome::Ready { ref cues, .. } if cues.len() == 2))
            .is_true();
        assert_that!(events.try_recv().is_err()).is_true();

        // Act: dropping the handles closes the request channel and ends the thread.
        drop(handles);
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// A frame the decoder will accept, in the format the cache actually stores, for
    /// seeding it with something a real grab could not have produced from the media the
    /// request names.
    fn frame_bytes(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::DynamicImage::new_rgb8(width, height)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Jpeg,
            )
            .expect("an in-memory JPEG should encode");
        bytes
    }

    /// The cues around the cursor are encoded and published alongside it, so that moving
    /// onto one draws it in the same pass that handled the keypress. Proven with a media
    /// file that does not exist: every frame here comes from the cache or not at all.
    #[test]
    fn the_cues_around_the_cursor_should_be_encoded_from_the_cache_and_published_too() {
        // Arrange
        let _guard = cache_guard();
        let directory = scratch("frame-nearby");
        let source = source(&directory.join("never-existed.mkv"), &directory);
        let cues = [cue(0, 1000, "here"), cue(2000, 3000, "next")];
        for one in &cues {
            seed(&source, one, &frame_bytes(64, 48));
        }
        let request = FrameRequest {
            generation: 1,
            source: source.clone(),
            wanted: Some(alone(7, cues[0].clone(), Duration::ZERO)),
            nearby: vec![alone(8, cues[1].clone(), Duration::from_millis(2500))],
            cells: Size::new(20, 10),
        };

        // Act
        let frames = frames_of(&request, &Picker::halfblocks(), &live(1));

        // Assert: the selected cue first — a nearby one ahead of it would put a cache read
        // in front of the picture the user is sitting in front of an empty pane for.
        assert_that!(
            frames
                .iter()
                .map(|(cue_index, _)| *cue_index)
                .collect::<Vec<_>>()
        )
        .is_equal_to(vec![7, 8]);
        assert_that!(
            frames
                .iter()
                .all(|(_, outcome)| matches!(outcome, FrameOutcome::Ready(_)))
        )
        .is_true();
        assert_that!(directory.join(cue_file()).exists()).is_false();

        // Cleanup
        for one in &cues {
            forget(&source, one);
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// A nearby cue the cache does not hold is skipped rather than rendered: the
    /// background pass is already walking the whole track, and an `ffmpeg` here would
    /// compete with the grab the user is actually waiting for.
    #[test]
    fn a_nearby_cue_with_no_cached_frame_should_be_skipped_rather_than_rendered() {
        // Arrange
        require_ffmpeg("a_nearby_cue_with_no_cached_frame_should_be_skipped_rather_than_rendered");
        let _guard = cache_guard();
        let directory = scratch("frame-nearby-uncached");
        let media = video(&directory);
        let uncached = cue(4000, 5000, "not rendered yet");
        let request = FrameRequest {
            nearby: vec![alone(1, uncached.clone(), Duration::from_millis(4500))],
            ..frame_request(&media, &directory, Duration::from_millis(2500))
        };
        forget(&request.source, wanted(&request));
        forget(&request.source, &uncached);

        // Act
        let frames = frames_of(&request, &Picker::halfblocks(), &live(1));

        // Assert: the selected cue was rendered, and the nearby one was left for the
        // background pass rather than grabbed here.
        assert_that!(
            frames
                .iter()
                .map(|(cue_index, _)| *cue_index)
                .collect::<Vec<_>>()
        )
        .is_equal_to(vec![0]);
        assert_that!(cached(&request.source, &uncached)).is_false();

        // Cleanup
        forget(&request.source, wanted(&request));
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// Nobody is looking at a nearby cue, so a frame of its that will not decode is
    /// dropped rather than reported — the message would appear under the cue that *is*
    /// selected, blaming a line that drew perfectly well.
    #[test]
    fn a_nearby_frame_that_will_not_decode_should_be_dropped_rather_than_reported() {
        // Arrange
        let _guard = cache_guard();
        let directory = scratch("frame-nearby-corrupt");
        let source = source(&directory.join("never-existed.mkv"), &directory);
        let selected = cue(0, 1000, "here");
        let corrupt = cue(2000, 3000, "next");
        seed(&source, &selected, &frame_bytes(64, 48));
        seed(&source, &corrupt, b"not a PNG at all");
        let request = FrameRequest {
            generation: 1,
            source: source.clone(),
            wanted: Some(alone(0, selected.clone(), Duration::ZERO)),
            nearby: vec![alone(1, corrupt.clone(), Duration::from_millis(2500))],
            cells: Size::new(20, 10),
        };

        // Act
        let frames = frames_of(&request, &Picker::halfblocks(), &live(1));

        // Assert
        assert_that!(
            frames
                .iter()
                .map(|(cue_index, _)| *cue_index)
                .collect::<Vec<_>>()
        )
        .is_equal_to(vec![0]);

        // Cleanup
        forget(&source, &selected);
        forget(&source, &corrupt);
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// The window a prefetch is around belongs to a page that can close part-way through
    /// it, and the cues left are for a cursor that is no longer anywhere near them.
    #[test]
    fn a_page_that_closes_should_stop_the_prefetch_where_it_stands() {
        // Arrange
        let _guard = cache_guard();
        let directory = scratch("frame-nearby-abandoned");
        let source = source(&directory.join("never-existed.mkv"), &directory);
        let cues = [cue(0, 1000, "here"), cue(2000, 3000, "next")];
        for one in &cues {
            seed(&source, one, &frame_bytes(64, 48));
        }
        let request = FrameRequest {
            generation: 1,
            source: source.clone(),
            wanted: None,
            nearby: cues
                .iter()
                .enumerate()
                .map(|(cue_index, one)| alone(cue_index, one.clone(), Duration::ZERO))
                .collect(),
            cells: Size::new(20, 10),
        };

        // Act: the live page has moved on past this request's generation.
        let frames = frames_of(&request, &Picker::halfblocks(), &live(9));

        // Assert: not one of them, cached and cheap though they were.
        assert_that!(frames.is_empty()).is_true();

        // Cleanup
        for one in &cues {
            forget(&source, one);
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// Collects the frames a `frame_window` run published, for the tests that hand their
    /// own "has the page gone" in.
    fn published(events: Receiver<PreviewEvent>) -> Vec<usize> {
        events
            .into_iter()
            .map(|event| match event {
                PreviewEvent::Frame { cue_index, .. } => cue_index,
                other => panic!("the frame worker should only publish frames, got {other:?}"),
            })
            .collect()
    }

    /// The page is open when the worker checks and gone once the grab is running, which is
    /// exactly the order a real closure arrives in. Nothing is published for it, and the
    /// window it was the middle of is dropped with it — the cues around a cursor that no
    /// longer exists are not worth encoding.
    #[test]
    fn a_page_that_closes_mid_grab_should_publish_nothing_and_drop_its_window() {
        // Arrange
        let _guard = cache_guard();
        require_ffmpeg("a_page_that_closes_mid_grab_should_publish_nothing_and_drop_its_window");
        let directory = scratch("frame-mid-grab");
        let media = video(&directory);
        let ahead = cue(4000, 5000, "the cue behind it");
        seed(&source(&media, &directory), &ahead, &frame_bytes(64, 48));
        let request = FrameRequest {
            nearby: vec![alone(1, ahead.clone(), Duration::from_millis(4500))],
            ..frame_request(&media, &directory, Duration::from_millis(2500))
        };
        forget(&request.source, wanted(&request));
        let (events_tx, events) = mpsc::channel();
        let checks = AtomicUsize::new(0);
        let closes_during_the_grab = || {
            // The worker's own check is the first; everything after it is `ffmpeg` being
            // polled, and by then the page has gone.
            checks.fetch_add(1, Ordering::Relaxed) >= 1
        };

        // Act
        let alive = frame_window(
            &request,
            &Picker::halfblocks(),
            &closes_during_the_grab,
            &events_tx,
        );
        drop(events_tx);

        // Assert: still serving, but nothing published — not the grab it gave up on, and
        // not the cached neighbour it would otherwise have gone on to encode.
        assert_that!(alive).is_true();
        assert_that!(published(events).is_empty()).is_true();

        // Cleanup
        forget(&request.source, &ahead);
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// A page can close between one neighbour and the next, and the rest of the window is
    /// then being got ready for a cursor that is not there any more.
    #[test]
    fn a_page_that_closes_mid_prefetch_should_stop_at_the_cue_it_reached() {
        // Arrange
        let _guard = cache_guard();
        let directory = scratch("frame-mid-prefetch");
        let source = source(&directory.join("never-existed.mkv"), &directory);
        let cues = [cue(0, 1000, "first"), cue(2000, 3000, "second")];
        for one in &cues {
            seed(&source, one, &frame_bytes(64, 48));
        }
        let request = FrameRequest {
            generation: 1,
            source: source.clone(),
            wanted: None,
            nearby: cues
                .iter()
                .enumerate()
                .map(|(cue_index, one)| alone(cue_index, one.clone(), Duration::ZERO))
                .collect(),
            cells: Size::new(20, 10),
        };
        let (events_tx, events) = mpsc::channel();
        let checks = AtomicUsize::new(0);
        // The worker's check, then the first neighbour's; the page goes before the second.
        let closes_after_the_first = || checks.fetch_add(1, Ordering::Relaxed) >= 2;

        // Act
        let alive = frame_window(
            &request,
            &Picker::halfblocks(),
            &closes_after_the_first,
            &events_tx,
        );
        drop(events_tx);

        // Assert
        assert_that!(alive).is_true();
        assert_that!(published(events).as_slice()).contains_exactly_in_given_order([0]);

        // Cleanup
        for one in &cues {
            forget(&source, one);
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// The worker stops entirely once its results have nowhere to go, which means the
    /// application has exited — including part-way through a window, where the grab
    /// succeeded and only the neighbours are left.
    #[test]
    fn a_prefetch_with_nobody_listening_should_stop_the_worker() {
        // Arrange
        let _guard = cache_guard();
        let directory = scratch("frame-prefetch-deaf");
        let source = source(&directory.join("never-existed.mkv"), &directory);
        let ahead = cue(2000, 3000, "second");
        seed(&source, &ahead, &frame_bytes(64, 48));
        let request = FrameRequest {
            generation: 1,
            source: source.clone(),
            wanted: None,
            nearby: vec![alone(1, ahead.clone(), Duration::ZERO)],
            cells: Size::new(20, 10),
        };
        let (events_tx, events) = mpsc::channel();
        drop(events);

        // Act
        let alive = frame(&request, &Picker::halfblocks(), &live(1), &events_tx);

        // Assert
        assert_that!(alive).is_false();

        // Cleanup
        forget(&source, &ahead);
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// The steady state once the cursor stops moving: the selected cue is already encoded
    /// and only the far edge of the window is missing, so the request carries no `wanted`
    /// at all and nothing is grabbed for it.
    #[test]
    fn a_request_for_nearby_cues_alone_should_grab_nothing() {
        // Arrange
        let _guard = cache_guard();
        let directory = scratch("frame-nearby-only");
        let source = source(&directory.join("never-existed.mkv"), &directory);
        let ahead = cue(2000, 3000, "next");
        seed(&source, &ahead, &frame_bytes(64, 48));
        let request = FrameRequest {
            generation: 1,
            source: source.clone(),
            wanted: None,
            nearby: vec![alone(4, ahead.clone(), Duration::from_millis(2500))],
            cells: Size::new(20, 10),
        };

        // Act
        let frames = frames_of(&request, &Picker::halfblocks(), &live(1));

        // Assert
        assert_that!(frames.len()).is_equal_to(1);
        assert_that!(frames[0].0).is_equal_to(4);
        assert_that!(matches!(frames[0].1, FrameOutcome::Ready(_))).is_true();
        assert_that!(directory.join(cue_file()).exists()).is_false();

        // Cleanup
        forget(&source, &ahead);
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// The whole point of the cache: the second time a cue is asked for, no subprocess
    /// runs at all. Proven by naming a media file that does not exist — an `ffmpeg` grab
    /// could not possibly succeed, so a frame coming back means it never ran.
    #[test]
    fn a_cached_frame_should_be_drawn_without_running_ffmpeg() {
        // Arrange
        let _guard = cache_guard();
        let directory = scratch("frame-cached");
        let request = FrameRequest {
            source: source(&directory.join("never-existed.mkv"), &directory),
            ..frame_request(
                &directory.join("never-existed.mkv"),
                &directory,
                Duration::ZERO,
            )
        };
        seed(&request.source, wanted(&request), &frame_bytes(64, 48));

        // Act
        let outcome = only_frame(frames_of(&request, &Picker::halfblocks(), &live(1)));

        // Assert
        let Some(FrameOutcome::Ready(protocol)) = outcome else {
            panic!("a cached frame should be drawn, got {outcome:?}");
        };
        assert_that!(protocol.size().width > 0).is_true();
        // Nothing was staged, which is the step every real grab starts with.
        assert_that!(directory.join(cue_file()).exists()).is_false();

        // Cleanup
        forget(&request.source, wanted(&request));
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// What a stored frame *is*, which is the whole reason a track's frames now survive in
    /// the cache instead of evicting one another as they are written.
    ///
    /// A frame of real video stored as PNG is about two megabytes — PNG is lossless, and
    /// there is nothing lossless to preserve in something that came out of h264, so the
    /// encoder spends the space on grain. At the default 512 MB that is roughly 240 frames,
    /// a fraction of a feature-length subtitle track: opening such a track pruned the cache,
    /// rendered the track, and evicted the start of it on the way past, so *every* opening
    /// rendered it again from the beginning. As JPEG the same frame is under a hundred
    /// kilobytes at twice the resolution, and a whole film's cues fit several times over.
    ///
    /// The size itself cannot be asserted from a `lavfi` fixture — synthetic video
    /// compresses losslessly in a way real footage does not, so a budget measured here
    /// would mean nothing. What is asserted is the decision the size follows from.
    #[test]
    fn a_stored_frame_should_be_a_jpeg_under_a_jpeg_name() {
        // Arrange
        let _guard = cache_guard();
        require_ffmpeg("a_stored_frame_should_be_a_jpeg_under_a_jpeg_name");
        let directory = scratch("frame-format");
        let media = video(&directory);
        let request = frame_request(&media, &directory, Duration::from_millis(2500));
        let key = key_alone(&request.source, wanted(&request));
        forget(&request.source, wanted(&request));

        // Act
        let frames = frames_of(&request, &Picker::halfblocks(), &live(1));

        // Assert: drawn, and kept under a name that says what it holds.
        assert_that!(frames.len()).is_equal_to(1);
        let path = framecache::path(&key.0, &key.1).expect("the test binary has a cache directory");
        assert_that!(
            path.extension()
                .and_then(|extension| extension.to_str())
                .unwrap_or_default()
        )
        .is_equal_to(framecache::FRAME_EXTENSION);
        // The JPEG start-of-image marker. Bytes rather than a decode, because what is at
        // issue is which encoder `ffmpeg` was asked for — a PNG would decode perfectly well
        // and cost twenty times the disk.
        let stored = std::fs::read(&path).expect("the frame should have been cached");
        assert_that!(stored.get(..3)).is_equal_to(Some([0xff, 0xd8, 0xff].as_slice()));

        // Cleanup
        forget(&request.source, wanted(&request));
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// A frame that had to be rendered is kept, and the keeping is what makes the second
    /// visit free — including after the media itself has gone.
    #[test]
    fn a_rendered_frame_should_be_cached_for_the_next_visit() {
        // Arrange
        let _guard = cache_guard();
        require_ffmpeg("a_rendered_frame_should_be_cached_for_the_next_visit");
        let directory = scratch("frame-caching");
        let media = video(&directory);
        let request = frame_request(&media, &directory, Duration::from_millis(2500));
        forget(&request.source, wanted(&request));

        // Act
        let outcome = only_frame(frames_of(&request, &Picker::halfblocks(), &live(1)));

        // Assert
        assert_that!(matches!(outcome, Some(FrameOutcome::Ready(_)))).is_true();
        assert_that!(cached(&request.source, wanted(&request))).is_true();

        // Act / Assert: and with the media deleted and the staged cue removed, the same
        // request still draws — from the cache, since there is nothing left to grab.
        std::fs::remove_file(&media).unwrap();
        std::fs::remove_file(directory.join(cue_file())).unwrap();
        let again = only_frame(frames_of(&request, &Picker::halfblocks(), &live(1)));
        assert_that!(matches!(again, Some(FrameOutcome::Ready(_)))).is_true();
        assert_that!(directory.join(cue_file()).exists()).is_false();

        // Cleanup
        forget(&request.source, wanted(&request));
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// A cue's frame is a picture of the whole screen, so the key has to cover the company
    /// the cue keeps as well as the cue itself.
    ///
    /// Both halves matter and they fail in opposite directions. Keying on the staged file
    /// alone would give two cues sharing a moment one frame between them, and the second one
    /// selected would be filed under the first's name. Keying on the cue alone — which is
    /// what this did before anything but the selected cue was burned in — would serve a
    /// picture drawn without the lines over it, and no test of counts, files or command
    /// lines would notice.
    #[test]
    fn a_cues_frame_should_be_keyed_on_the_company_it_keeps() {
        // Arrange
        let _guard = cache_guard();
        let directory = scratch("frame-company");
        let source = source(&directory.join("clip.mkv"), &directory);
        let under = cue(2000, 3000, "under");
        let over = cue(2400, 2600, "over");
        let together = vec![under.clone(), over.clone()];

        // Act / Assert: the same cue keys differently once something else shares its frame.
        assert_that!(source.key(&under, std::slice::from_ref(&under)).1.as_str())
            .is_not_equal_to(source.key(&under, &together).1.as_str());

        // Act / Assert: and the two cues sharing that one picture still get a frame each,
        // since the key covers which of them the frame is *for* as well as what is in it.
        assert_that!(source.key(&under, &together).1.as_str())
            .is_not_equal_to(source.key(&over, &together).1.as_str());

        // Act / Assert: all of it in the one media directory, whatever is on screen.
        assert_that!(source.key(&under, &together).0.as_str())
            .is_equal_to(source.key(&over, std::slice::from_ref(&over)).0.as_str());

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// A cache key covers one cue's text and timing, so a track whose cues differ cannot
    /// share a frame between them — which is what would show the wrong line burned in.
    ///
    /// And they differ in the *cue* half only: every cue of one media shares its directory,
    /// which is what lets the cache evict that media's frames as one thing.
    #[test]
    fn two_cues_should_not_share_a_frame() {
        // Arrange
        let _guard = cache_guard();
        let directory = scratch("frame-per-cue");
        let source = source(&directory.join("clip.mkv"), &directory);
        let first = key_alone(&source, &cue(0, 1000, "first"));

        // Act / Assert
        assert_that!(first.1.as_str())
            .is_not_equal_to(key_alone(&source, &cue(0, 1000, "second")).1.as_str());
        assert_that!(first.1.as_str())
            .is_not_equal_to(key_alone(&source, &cue(500, 1000, "first")).1.as_str());
        // The same directory for all three, whatever the cue says.
        assert_that!(first.0.as_str())
            .is_equal_to(key_alone(&source, &cue(500, 1000, "first")).0.as_str());

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// Frames are cached, so they cannot be rendered at the pane's size — the pane
    /// changes. They are rendered at the source's own size instead, capped so that a
    /// whole film's cues stay a cache a laptop can hold, and never enlarged.
    #[test]
    fn a_frame_should_be_rendered_at_the_sources_size_within_the_cap() {
        // Act / Assert: a source inside the cap is left alone — including 1080p itself,
        // which is the commonest source there is and is now stored at its own size.
        assert_that!(target_pixels(Some((640, 360)))).is_equal_to((640, 360));
        assert_that!(target_pixels(Some((1920, 1080)))).is_equal_to((1920, 1080));
        // A 4K source shrinks on its widest axis, keeping its shape.
        assert_that!(target_pixels(Some((3840, 2160)))).is_equal_to((1920, 1080));
        // A very wide source is bounded by width, a very tall one by height.
        assert_that!(target_pixels(Some((3840, 1600)))).is_equal_to((1920, 800));
        assert_that!(target_pixels(Some((1200, 1600)))).is_equal_to((810, 1080));
        // Odd sizes round down to even, since the decode on the way here is subsampled.
        assert_that!(target_pixels(Some((641, 361)))).is_equal_to((640, 360));
        // A resolution ffprobe could not measure takes the cap, which
        // `force_original_aspect_ratio=decrease` then treats as a bound rather than a
        // shape — and a zero-sized one is the same case, not a division by zero.
        assert_that!(target_pixels(None)).is_equal_to((1920, 1080));
        assert_that!(target_pixels(Some((0, 1080)))).is_equal_to((1920, 1080));
        assert_that!(target_pixels(Some((1920, 0)))).is_equal_to((1920, 1080));
        // A source smaller than the two-pixel floor still has a size the scaler accepts.
        assert_that!(target_pixels(Some((1, 1)))).is_equal_to((2, 2));
    }

    /// The interactive path and the background pass have to seek to the same instant for
    /// the same cue, or the two would write different pictures under one cache key.
    #[test]
    fn a_seek_should_be_the_moment_the_cue_comes_in_held_back_from_the_end_of_the_media() {
        // Act / Assert: the moment the line arrives, which is what the reader is judging,
        // rather than the midpoint — a frame the line is comfortably on top of says
        // nothing about whether it came in with the shot.
        assert_that!(seek_for(&cue(1000, 3000, "a"), Duration::from_secs(60)))
            .is_equal_to(Duration::from_secs(1));
        // A cue coming in past the end lands before the final frame, not on it: `-frames:v
        // 1` writes nothing at all past the end.
        assert_that!(seek_for(&cue(11_000, 12_000, "a"), Duration::from_secs(10)))
            .is_equal_to(Duration::from_millis(9800));
        // A duration that would not parse arrives as zero, and clamping against that
        // would preview the first frame of the media for every cue in the track.
        assert_that!(seek_for(&cue(11_000, 12_000, "a"), Duration::ZERO))
            .is_equal_to(Duration::from_secs(11));
    }

    fn warm_request(media: &Path, workspace: &Path, cues: Vec<Cue>) -> WarmRequest {
        WarmRequest {
            generation: 1,
            source: source(media, workspace),
            cues,
            duration: Duration::from_secs(6),
            // Deliberately unbounded: `warm` prunes the cache it shares with every other
            // test in this binary, and a real limit here would delete their frames.
            cache_tracks: usize::MAX,
        }
    }

    fn warmed(events: &Receiver<PreviewEvent>) -> Vec<(usize, usize)> {
        let mut progress = Vec::new();
        while let Ok(PreviewEvent::Warming { done, total, .. }) = events.try_recv() {
            progress.push((done, total));
        }
        progress
    }

    /// The pass marks its track used and names it to the prune as the open one, so the
    /// frames it was opened to show cannot be the ones it evicts.
    ///
    /// Both halves matter and are asserted together: the marker is what ranks this track
    /// as the most recently used, and naming it excludes it outright even at a limit that
    /// leaves room for nothing else.
    ///
    /// Takes the exclusive guard and empties the shared directory, because a real limit is
    /// the point here and every other test's frames would otherwise be counted towards it.
    #[test]
    fn the_background_pass_should_mark_its_track_used_and_prune_around_it() {
        // Arrange
        let _guard = crate::framecache::testing::whole_directory();
        let cache = framecache::directory().expect("the test binary has a cache directory");
        let _ = std::fs::remove_dir_all(&cache);
        let directory = scratch("warm-prune");
        // A media file that does not exist, so a cue that is *not* already cached could
        // not be rendered — every frame this pass keeps is one it found.
        let cues = vec![
            cue(1000, 2000, "open first"),
            cue(3000, 4000, "open second"),
        ];
        let request = WarmRequest {
            // Room for one track only, so the prune has to choose between the open track
            // and the other one.
            cache_tracks: 1,
            ..warm_request(
                &directory.join("never-existed.mkv"),
                &directory,
                cues.clone(),
            )
        };
        for cue in &cues {
            assert_that!(seed(&request.source, cue, &[0u8; 100])).is_true();
        }
        // The open track was used long ago; the other one moments ago. Plain
        // least-recently-used would take the open one, which is the bug.
        assert_that!(framecache::touch(&request.source.media_key())).is_true();
        std::fs::File::open(cache.join(request.source.media_key()).join(".used"))
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(3600 * 100))
            .expect("the test filesystem should allow setting an mtime");
        assert_that!(framecache::store("some-other-track", "frame", &[0u8; 100])).is_true();
        assert_that!(framecache::touch("some-other-track")).is_true();

        // Act
        let (events, published) = mpsc::channel();
        let alive = warm_track(&request, 3, &|| false, &events);
        drop(events);
        drop(published);

        // Assert: the other track went whole, and every frame of the open one survived
        // despite its being the least recently used thing in the cache.
        assert_that!(alive).is_true();
        assert_that!(cache.join("some-other-track").exists()).is_false();
        for cue in &cues {
            assert_that!(cached(&request.source, cue)).is_true();
        }
        // And the pass left the marker fresh, so the *next* prune ranks it first too.
        let used = std::fs::metadata(cache.join(request.source.media_key()).join(".used"))
            .and_then(|marker| marker.modified())
            .expect("the pass should have marked its track used");
        assert_that!(used.elapsed().unwrap() < Duration::from_secs(60)).is_true();

        // Cleanup
        let _ = std::fs::remove_dir_all(&cache);
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// The background pass: every cue in the track rendered and cached ahead of the user
    /// reaching it, counting up as it goes.
    #[test]
    fn the_background_pass_should_render_and_cache_every_cue() {
        // Arrange
        let _guard = cache_guard();
        require_ffmpeg("the_background_pass_should_render_and_cache_every_cue");
        let directory = scratch("warm-all");
        let media = video(&directory);
        let cues = vec![cue(1000, 2000, "first"), cue(3000, 4000, "second")];
        let request = warm_request(&media, &directory, cues.clone());
        for cue in &cues {
            forget(&request.source, cue);
        }
        let (events_tx, events) = mpsc::channel();

        // Act
        let alive = warm(&request, &live(1), &events_tx);

        // Assert
        assert_that!(alive).is_true();
        for cue in &cues {
            assert_that!(cached(&request.source, cue)).is_true();
        }
        let progress = warmed(&events);
        assert_that!(progress.first().copied()).is_equal_to(Some((0, 2)));
        assert_that!(progress.last().copied()).is_equal_to(Some((2, 2)));

        // Cleanup
        for cue in &cues {
            forget(&request.source, cue);
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// A track that is already cached — the second time a page is opened — costs nothing
    /// but the walk. Proven by naming a media file that does not exist.
    #[test]
    fn the_background_pass_should_skip_cues_that_are_already_cached() {
        // Arrange
        let _guard = cache_guard();
        let directory = scratch("warm-cached");
        let cues = vec![cue(1000, 2000, "first"), cue(3000, 4000, "second")];
        let request = warm_request(
            &directory.join("never-existed.mkv"),
            &directory,
            cues.clone(),
        );
        for cue in &cues {
            seed(&request.source, cue, &frame_bytes(8, 8));
        }
        let (events_tx, events) = mpsc::channel();

        // Act
        warm(&request, &live(1), &events_tx);

        // Assert: it got all the way to the end without staging a cue, which is the first
        // thing any real grab does.
        assert_that!(warmed(&events).last().copied()).is_equal_to(Some((2, 2)));
        assert_that!(directory.join(warm_cue_file(0)).exists()).is_false();

        // Cleanup
        for cue in &cues {
            forget(&request.source, cue);
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// Leaving the page stops the pass. Without this, walking into a directory and back
    /// out again leaves a thousand-cue render running for a page nobody is looking at.
    #[test]
    fn the_background_pass_should_stop_the_moment_its_page_closes() {
        // Arrange
        let _guard = cache_guard();
        let directory = scratch("warm-abandoned");
        let request = warm_request(
            &directory.join("never-existed.mkv"),
            &directory,
            vec![cue(1000, 2000, "first")],
        );
        let (events_tx, events) = mpsc::channel();

        // Act: the live page has moved on past this request's generation.
        warm(&request, &live(9), &events_tx);

        // Assert: not a single event, and nothing staged or spawned.
        assert_that!(warmed(&events).as_slice()).is_empty();
        assert_that!(directory.join(warm_cue_file(0)).exists()).is_false();

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// Half the machine, never none of it, never more than the cap.
    ///
    /// A fixed three made the foreground *worse* on a small machine: measured against an
    /// uncached interactive grab with a pass running underneath, it cost +18% on 32 cores
    /// but +157% on two — the one frame the user is waiting for taking two and a half
    /// times as long. Leaving half the cores for that grab is what this exists to do, so
    /// the floor and the cap both matter and both are asserted.
    #[test]
    fn the_worker_count_should_leave_the_machine_room_for_the_foreground() {
        // Act
        let workers = warm_workers();

        // Assert: never zero, or a track would never render at all…
        assert_that!(workers >= 1).is_true();
        // …and never more than the cap, whatever the machine reports.
        assert_that!(workers <= MAX_WARM_WORKERS).is_true();
        // And it really is derived from the machine rather than fixed: half its cores,
        // which on anything with six or more is the cap and on a dual core is one.
        let cores = std::thread::available_parallelism()
            .map(std::num::NonZero::get)
            .unwrap_or(1);
        assert_that!(workers).is_equal_to((cores / 2).clamp(1, MAX_WARM_WORKERS));
    }

    /// Every worker stages its cue somewhere no other worker can reach.
    ///
    /// The one thing in this module that fails *silently* if it is wrong: two workers
    /// sharing a staged `.srt` would each overwrite the other's cue between the write and
    /// the burn, and the frame that came back would carry the wrong line — stored under
    /// the right cache key, so no assertion about keys, counts or files could ever notice.
    /// See `the_background_pass_should_burn_each_workers_own_cue` for the end-to-end half.
    #[test]
    fn every_warm_worker_should_stage_its_cue_under_its_own_name() {
        // Act
        let names: std::collections::BTreeSet<String> =
            (0..MAX_WARM_WORKERS).map(warm_cue_file).collect();

        // Assert: one per worker, and none of them the interactive path's — which runs at
        // the same time as all of them.
        assert_that!(names.len()).is_equal_to(MAX_WARM_WORKERS);
        assert_that!(names.contains(&cue_file())).is_false();
        // Bare relative names, so `ffmpeg` takes them as literals inside its working
        // directory and nothing has to be escaped through the filtergraph syntax.
        for name in &names {
            assert_that!(name.contains('/')).is_false();
            assert_that!(name.ends_with(".srt")).is_true();
        }
    }

    /// The frame each worker cached is the one its own cue would have produced alone.
    ///
    /// The end-to-end half of the staging rule, and the only assertion that can catch it
    /// breaking. Two workers sharing a staged `.srt` overwrite each other between the
    /// write and the burn, so a frame comes back carrying a *different cue's* line —
    /// stored under the right key, in the right directory, counted correctly. Every other
    /// test in this file would pass.
    ///
    /// So each cue is rendered again afterwards on its own, through the interactive path
    /// and its own staging file, and the bytes are compared. `mjpeg` is deterministic for
    /// one input at one seek, so the two runs agree exactly unless the picture differs —
    /// and against a solid-colour fixture the burned-in line is the only thing that can
    /// make it differ.
    #[test]
    fn the_background_pass_should_burn_each_workers_own_cue() {
        // Arrange: three cues per worker, each with text of its own.
        let _guard = cache_guard();
        require_ffmpeg("the_background_pass_should_burn_each_workers_own_cue");
        let directory = scratch("warm-staging");
        let media = video(&directory);
        let cues: Vec<Cue> = (0..9u64)
            .map(|index| {
                cue(
                    index * 400,
                    index * 400 + 300,
                    &format!("worker line {index}"),
                )
            })
            .collect();
        let request = warm_request(&media, &directory, cues.clone());
        for cue in &cues {
            forget(&request.source, cue);
        }

        // Act: the parallel pass.
        let (events_tx, events) = mpsc::channel();
        assert_that!(warm_track(&request, 3, &|| false, &events_tx)).is_true();
        drop(events_tx);
        drop(events);
        let in_parallel: Vec<Vec<u8>> = cues
            .iter()
            .map(|cue| {
                let (media_key, cue_key) = key_alone(&request.source, cue);
                framecache::read(&media_key, &cue_key)
                    .unwrap_or_else(|| panic!("the pass should have cached {:?}", cue.text))
            })
            .collect();

        // Act: the same cues again, one at a time, with nothing to race.
        for cue in &cues {
            forget(&request.source, cue);
        }
        let serially: Vec<Vec<u8>> = cues
            .iter()
            .enumerate()
            .map(|(position, cue)| {
                let seek = seek_for(cue, request.duration);
                let target = FrameTarget {
                    cue_index: position,
                    cue: cue.clone(),
                    on_screen: crate::cue::on_screen_at(&request.cues, position, seek),
                    seek,
                };
                match png(&request.source, &target, CueSlot::Interactive, &|| false) {
                    PngOutcome::Ready(bytes) => bytes,
                    PngOutcome::Abandoned => panic!("nothing abandoned {:?}", cue.text),
                    PngOutcome::Failed(why) => {
                        panic!("rendering {:?} alone should work: {why}", cue.text)
                    }
                }
            })
            .collect();

        // Assert
        for ((cue, parallel), serial) in cues.iter().zip(&in_parallel).zip(&serially) {
            assert_that!(parallel.as_slice() == serial.as_slice()).is_true();
            assert_that!(parallel.is_empty()).is_false();
            // Named in the failure message, since the bytes themselves say nothing.
            assert!(
                parallel == serial,
                "the pass cached a different picture for {:?} than rendering it alone \
                 produces, which is what a shared staging file looks like",
                cue.text
            );
        }

        // Cleanup
        for cue in &cues {
            forget(&request.source, cue);
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// A media file that cannot produce frames at all must not cost one doomed subprocess
    /// per cue. Each worker steps over a failure or two and then gives up on its slice,
    /// and the pass still finishes its count so the status line does not freeze part-way.
    ///
    /// Twelve cues, so every worker's slice is longer than the tolerance — with fewer, a
    /// slice runs out before the third consecutive failure and the tolerance is never
    /// reached at all, which is a property of the split rather than of the media.
    #[test]
    fn the_background_pass_should_give_up_on_a_track_that_keeps_failing() {
        // Arrange: each cue named so the staged file says which one that worker last tried.
        let _guard = cache_guard();
        require_ffmpeg("the_background_pass_should_give_up_on_a_track_that_keeps_failing");
        let directory = scratch("warm-failing");
        let cues: Vec<Cue> = (0..12)
            .map(|index| cue(index * 1000, index * 1000 + 500, &format!("cue-{index:02}")))
            .collect();
        let request = warm_request(&directory.join("never-existed.mkv"), &directory, cues);
        let (events_tx, events) = mpsc::channel();

        // Act
        let alive = warm_track(&request, 3, &|| false, &events_tx);

        // Assert: the count finishes, so the status line clears...
        assert_that!(alive).is_true();
        assert_that!(warmed(&events).last().copied()).is_equal_to(Some((12, 12)));
        // ...and every worker stopped at the third cue of its own slice rather than
        // walking all four. Which is also the proof that the slices are the contiguous
        // quarters they are meant to be, and that each worker staged its cue somewhere no
        // other worker could overwrite.
        for (worker, expected) in [(0, "cue-02"), (1, "cue-06"), (2, "cue-10")] {
            let staged = std::fs::read_to_string(directory.join(warm_cue_file(worker))).unwrap();
            assert_that!(staged.as_str()).contains(expected);
        }

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// A fully cached track is walked in microseconds per cue. An event each would push
    /// thousands of messages at the event loop for a line that only counts — but the
    /// first and the last always go, or the line would never appear or never finish.
    #[test]
    fn progress_should_be_reported_at_the_ends_and_rate_limited_in_between() {
        // Arrange
        let (events_tx, events) = mpsc::channel();
        let mut progress = WarmProgress::new(7, 3);

        // Act
        progress.publish(&events_tx, 0);
        progress.publish(&events_tx, 1);
        progress.publish(&events_tx, 2);
        progress.publish(&events_tx, 3);

        // Assert: the first and the last, not the two in between.
        assert_that!(warmed(&events).as_slice()).contains_exactly_in_given_order([(0, 3), (3, 3)]);

        // Act / Assert: and once the interval has passed, reporting resumes.
        let mut progress = WarmProgress::new(7, 3);
        progress.publish(&events_tx, 0);
        std::thread::sleep(WARM_PROGRESS_INTERVAL + Duration::from_millis(20));
        progress.publish(&events_tx, 1);
        assert_that!(warmed(&events).as_slice()).contains_exactly_in_given_order([(0, 3), (1, 3)]);
    }

    /// Nobody left to report to means the application has gone, and the worker with it.
    #[test]
    fn progress_should_report_when_nobody_is_listening_any_more() {
        // Arrange
        let (events_tx, events) = mpsc::channel();
        let mut progress = WarmProgress::new(1, 1);

        // Act / Assert
        assert_that!(progress.publish(&events_tx, 0)).is_true();
        drop(events);
        assert_that!(progress.publish(&events_tx, 1)).is_false();
    }

    /// The worker loop itself: one request per page opening, coalesced down to the page
    /// that is actually open, and a shutdown when its results have nowhere to go.
    #[test]
    fn the_background_worker_should_answer_the_live_page_and_stop_when_nobody_is_listening() {
        // Arrange
        let _guard = cache_guard();
        let directory = scratch("warm-worker");
        let (handles, events) = spawn_preview_workers(Some(Picker::halfblocks()));
        let request = |generation| WarmRequest {
            generation,
            ..warm_request(
                &directory.join("never-existed.mkv"),
                &directory,
                vec![cue(1000, 2000, "only")],
            )
        };

        // Act: an older page's request is queued behind the live one, and the coalescing
        // receive is what drops it.
        handles.abandon(5);
        handles.request_warm(request(2));
        handles.request_warm(request(5));

        // Assert: only the live page is counted for.
        let mut generations = Vec::new();
        while let Ok(event) = events.recv_timeout(Duration::from_secs(30)) {
            let PreviewEvent::Warming {
                generation,
                done,
                total,
            } = event
            else {
                panic!("a warm request should answer with progress");
            };
            generations.push(generation);
            if done == total {
                break;
            }
        }
        assert_that!(generations.iter().all(|generation| *generation == 5)).is_true();

        // Act / Assert: with the results channel gone, the worker stops rather than
        // walking the track it has been handed.
        drop(events);
        handles.request_warm(request(5));
        let deadline = Instant::now() + Duration::from_secs(10);
        while handles
            .warm_tx
            .as_ref()
            .expect("this picker has a background worker")
            .send(request(5))
            .is_ok()
        {
            assert_that!(Instant::now() < deadline).is_true();
            std::thread::sleep(Duration::from_millis(10));
        }

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// The pass's cache lookup is a stat, not a read: a track opened a second time is
    /// entirely cached, and reading every frame back off disk to throw the bytes away
    /// would be hundreds of megabytes of I/O for nothing. Proven by naming a media file
    /// that does not exist — a render could not succeed, so `Cached` means none was tried.
    #[test]
    fn a_cue_already_in_the_cache_should_not_be_rendered_again() {
        // Arrange
        let _guard = cache_guard();
        let directory = scratch("warm-lookup");
        let source = source(&directory.join("never-existed.mkv"), &directory);
        let first = cue(1000, 2000, "cached");
        let second = cue(3000, 4000, "missing");
        seed(&source, &first, &frame_bytes(8, 8));
        forget(&source, &second);

        // Act
        let hit = alone(0, first.clone(), Duration::ZERO);
        let miss = alone(1, second.clone(), Duration::ZERO);
        let cached = cache_frame(&source, &hit, CueSlot::Warm(0), &|| false);
        let missing = cache_frame(&source, &miss, CueSlot::Warm(0), &|| false);
        let abandoned = cache_frame(&source, &miss, CueSlot::Warm(0), &|| true);

        // Assert
        assert_that!(cached).is_equal_to(CacheOutcome::Cached);
        assert_that!(missing).is_equal_to(CacheOutcome::Failed);
        assert_that!(abandoned).is_equal_to(CacheOutcome::Abandoned);

        // Cleanup
        forget(&source, &first);
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// A frame fills the pane whether the cached picture is larger or smaller than it.
    ///
    /// Regression test: frames used to be grabbed at the pane's own pixel size, so they
    /// always filled it. Grabbing at a fixed size instead — which is what lets one cached
    /// frame serve every pane size — broke that, because `Resize::Fit` clamps its target
    /// to `min(pane, image)` and so never enlarges. The preview drew at the cached picture's
    /// own size in the corner of a pane several times larger.
    #[test]
    fn a_frame_should_fill_the_pane_it_is_drawn_in() {
        // Arrange: a pane of 40x20 cells, which at halfblocks is 40x40 pixels.
        let pane = Size::new(40, 20);

        // Act: one picture far smaller than the pane, one far larger.
        let FrameOutcome::Ready(smaller) = encode(&frame_bytes(8, 8), &Picker::halfblocks(), pane)
        else {
            panic!("a readable PNG should encode");
        };
        let FrameOutcome::Ready(larger) =
            encode(&frame_bytes(400, 400), &Picker::halfblocks(), pane)
        else {
            panic!("a readable PNG should encode");
        };

        // Assert: both fill the pane rather than sitting inside it at their own size.
        assert_that!(smaller.size()).is_equal_to(pane);
        assert_that!(larger.size()).is_equal_to(pane);
    }

    /// A picture whose shape differs from the pane's keeps its proportions, filling the
    /// axis that runs out first — a stretched frame would misrepresent where a subtitle
    /// sits in the picture, which is the one thing this page is for.
    #[test]
    fn a_frame_should_keep_its_proportions_while_filling() {
        // Arrange: a pane of 40x10 cells is 40x20 pixels at halfblocks — wider than tall.
        let pane = Size::new(40, 10);

        // Act: a square picture.
        let FrameOutcome::Ready(square) = encode(&frame_bytes(64, 64), &Picker::halfblocks(), pane)
        else {
            panic!("a readable PNG should encode");
        };

        // Assert: it fills the height, and takes only the width its shape allows.
        assert_that!(square.size().height).is_equal_to(10);
        assert_that!(square.size().width).is_equal_to(20);
    }

    /// The endings a real grab cannot be steered into on demand, on the way out to the
    /// page rather than on the way in from `ffmpeg`.
    #[test]
    fn a_frame_the_page_stopped_waiting_for_should_not_be_drawn_or_reported() {
        // Act
        let abandoned = drawn(
            PngOutcome::Abandoned,
            &Picker::halfblocks(),
            Size::new(20, 10),
        );
        let failed = drawn(
            PngOutcome::Failed("ffmpeg was not found in PATH.".to_string()),
            &Picker::halfblocks(),
            Size::new(20, 10),
        );
        let ready = drawn(
            PngOutcome::Ready(frame_bytes(64, 48)),
            &Picker::halfblocks(),
            Size::new(20, 10),
        );

        // Assert: nothing at all for a page that has gone, a reason for one that failed.
        assert_that!(abandoned.is_none()).is_true();
        let Some(FrameOutcome::Failed(message)) = failed else {
            panic!("a grab that failed should say why");
        };
        assert_that!(message.as_str()).contains("not found in PATH");
        assert_that!(matches!(ready, Some(FrameOutcome::Ready(_)))).is_true();
    }

    /// An application that has exited takes its event receiver with it. The pass has to
    /// report that rather than walk a whole track for nobody, and reporting it is what
    /// stops the worker thread — see
    /// `the_background_worker_should_answer_the_live_page_and_stop_when_nobody_is_listening`.
    #[test]
    fn a_pass_with_nobody_listening_should_report_it_before_rendering_anything() {
        // Arrange
        let _guard = cache_guard();
        let directory = scratch("warm-deaf");
        let cues = vec![cue(1000, 2000, "first"), cue(3000, 4000, "second")];
        let request = warm_request(&directory.join("never-existed.mkv"), &directory, cues);
        let (events_tx, events) = mpsc::channel();
        drop(events);

        // Act
        let alive = warm_track(&request, 3, &|| false, &events_tx);

        // Assert: reported, and nothing staged — it gave up before the first grab rather
        // than after the last.
        assert_that!(alive).is_false();
        assert_that!(directory.join(warm_cue_file(0)).exists()).is_false();

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// Leaving the page part-way through the pass stops it where it stands: no further
    /// cue is rendered, and no count is published for a page that is gone.
    #[test]
    fn the_background_pass_should_stop_between_cues_when_the_page_goes() {
        // Arrange: five cues, and a page that closes after the first is dealt with.
        let _guard = cache_guard();
        let directory = scratch("warm-mid-pass");
        let cues: Vec<Cue> = (0..5)
            .map(|index| cue(index * 1000, index * 1000 + 500, &format!("cue-{index}")))
            .collect();
        let request = warm_request(&directory.join("never-existed.mkv"), &directory, cues);
        let (events_tx, events) = mpsc::channel();
        // The pass's own check comes first and finds the page open; every check after it
        // is a worker starting its slice, and by then it has gone. Which worker asks
        // first does not matter — they all get the same answer.
        let checks = AtomicUsize::new(0);
        let closes_after_the_first = || checks.fetch_add(1, Ordering::Relaxed) >= 1;

        // Act
        let alive = warm_track(&request, 3, &closes_after_the_first, &events_tx);

        // Assert: the pass ends without a final count, since the page it counted for is
        // gone — and it ended early, rather than walking the remaining cues. Asserted on
        // the events rather than on how many times the closure was asked, which three
        // workers make a race.
        assert_that!(alive).is_true();
        assert_that!(warmed(&events).as_slice()).contains_exactly_in_given_order([(0, 5)]);

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// Closing the page during a cue's own render is the other half of the same rule:
    /// the running `ffmpeg` is killed and the pass stops, rather than the killed grab
    /// counting as a failure and the next cue starting another one.
    ///
    /// One cue, so the split hands the whole track to one worker and the order the
    /// closure is asked in is fixed. With three workers running it is a race, and the
    /// first draft of this test lost it silently — every worker gave up at the top of its
    /// slice, so nothing was ever rendered and the mid-render path this is named for was
    /// not reached at all.
    #[test]
    fn the_background_pass_should_stop_when_a_cue_is_abandoned_mid_render() {
        // Arrange
        let _guard = cache_guard();
        require_ffmpeg("the_background_pass_should_stop_when_a_cue_is_abandoned_mid_render");
        let directory = scratch("warm-mid-render");
        let media = video(&directory);
        let cues = vec![cue(1000, 2000, "only")];
        let request = warm_request(&media, &directory, cues.clone());
        for cue in &cues {
            forget(&request.source, cue);
        }
        let (events_tx, events) = mpsc::channel();
        // Check one is the pass's own and check two the worker's, both finding the page
        // open. Everything after is `ffmpeg` being polled, and by then it has gone — so a
        // grab already running is killed rather than left to finish.
        let checks = AtomicUsize::new(0);
        let closes_during_the_grab = || checks.fetch_add(1, Ordering::Relaxed) >= 2;

        // Act
        let alive = warm_track(&request, 1, &closes_during_the_grab, &events_tx);

        // Assert: stopped with nothing cached and nothing counted past the start.
        assert_that!(alive).is_true();
        assert_that!(warmed(&events).as_slice()).contains_exactly_in_given_order([(0, 1)]);
        for cue in &cues {
            assert_that!(cached(&request.source, cue)).is_false();
        }
        // And the grab really did start, rather than the worker giving up before it: the
        // cue was staged, which is the step every render begins with.
        assert_that!(directory.join(warm_cue_file(0)).exists()).is_true();

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// A halfblock cell is one pixel across and two down, so the grid is square-pixelled
    /// and a picture keeps its proportions only if the *cell* area is worked out from the
    /// source's aspect ratio. Handing `Halfblocks::new` the whole pane instead stretches
    /// every frame to the pane's shape, which on a preview pane wider than it is tall is a
    /// picture nobody could judge a subtitle's placement against.
    #[test]
    fn a_playback_should_fill_the_pane_without_changing_the_pictures_shape() {
        // Act / Assert: a 4:3 source in a pane far wider than that. The height runs out,
        // so the width gives — 60 cells is 120 pixels tall, and 4:3 of that is 160 across.
        assert_that!(playback_cells(
            Size::new(200, 60),
            (640, 480),
            test_picker().font_size()
        ))
        .is_equal_to(Size::new(160, 60));

        // Act / Assert: the same source in a pane taller than it is wide. Now the width is
        // what runs out, and the height is chosen to match — 80 across is 60 cells of 4:3.
        assert_that!(playback_cells(
            Size::new(80, 90),
            (640, 480),
            test_picker().font_size()
        ))
        .is_equal_to(Size::new(80, 30));

        // Act / Assert: a 16:9 source, which is what most media actually is.
        assert_that!(playback_cells(
            Size::new(160, 60),
            (1920, 1080),
            test_picker().font_size()
        ))
        .is_equal_to(Size::new(160, 45));

        // Act / Assert: and the pixels are the terminal's own cell size per cell exactly,
        // so the encode resamples nothing at all.
        let font = test_picker().font_size();
        assert_that!(playback_pixels(Size::new(160, 45), font))
            .is_equal_to((160 * u32::from(font.width), 45 * u32::from(font.height)));
    }

    /// A terminal offering no image protocol gets no preview at all.
    ///
    /// Halfblocks is what `Picker::from_query_stdio` reports for every terminal that
    /// answered nothing, and two coloured half-cells cannot show a subtitle burned into a
    /// frame — which is the only thing this page is for. Refusing the picker at startup is
    /// what turns that into one honest sentence instead of a page that opens, renders,
    /// caches and plays a picture nobody could read.
    #[test]
    fn a_terminal_with_no_image_protocol_should_get_no_picker() {
        // Act / Assert: the fallback every unanswering terminal lands on is refused.
        assert_that!(drawing_picker(Picker::halfblocks()).is_none()).is_true();

        // Act / Assert: and every protocol that draws real pixels is kept, with its font
        // size intact — that figure is what a playback is decoded at.
        for protocol in [
            ProtocolType::Kitty,
            ProtocolType::Iterm2,
            ProtocolType::Sixel,
        ] {
            let mut picker = Picker::halfblocks();
            picker.set_protocol_type(protocol);
            let font = picker.font_size();
            let kept = drawing_picker(picker).expect("an image protocol should be kept");
            assert_that!(kept.protocol_type()).is_equal_to(protocol);
            assert_that!((kept.font_size().width, kept.font_size().height))
                .is_equal_to((font.width, font.height));
        }
    }

    /// The cells and the font each carry half of the picture's proportions, and the two
    /// halves have to cancel.
    ///
    /// [`playback_cells`] divides by two because a *cell* is about twice as tall as it is
    /// wide, so its answer is deliberately not the source's ratio. [`playback_pixels`] then
    /// multiplies by the font, which is about twice as tall as it is wide for the same
    /// reason — and the product comes back to the source's own proportions. Dropping either
    /// factor, or applying one of them twice, leaves every frame stretched by about four,
    /// which no assertion about counts, keys or command lines would notice.
    #[test]
    fn a_playbacks_cells_and_font_should_cancel_back_to_the_sources_proportions() {
        // Arrange: a 16:9 source given a pane it fits by height.
        let font = test_picker().font_size();
        let cells = playback_cells(Size::new(160, 60), (1920, 1080), test_picker().font_size());
        let (width, height) = playback_pixels(cells, font);

        // Assert: the cells alone are *not* 16:9 — they carry the factor of two.
        assert_that!(u32::from(cells.width) * 9).is_equal_to(u32::from(cells.height) * 2 * 16);

        // Assert: the pixels are exactly the font per cell, each way — so the decode lands
        // on the grid the terminal will draw and nothing is resampled.
        assert_that!(width).is_equal_to(u32::from(cells.width) * u32::from(font.width));
        assert_that!(height).is_equal_to(u32::from(cells.height) * u32::from(font.height));

        // Assert: and the two together are back to 16:9, which is the picture the user sees.
        assert_that!(width * 9).is_equal_to(height * 16);
    }

    /// The renderer has not measured the pane on the first frame after the page opens, and
    /// a container whose resolution would not parse arrives with no size at all. Neither
    /// may become a `scale=0:0` or a division by zero.
    #[test]
    fn a_playback_should_ask_for_no_picture_when_there_is_nothing_to_size_it_against() {
        // Act / Assert
        assert_that!(playback_cells(
            Size::new(0, 60),
            (640, 480),
            test_picker().font_size()
        ))
        .is_equal_to(Size::new(0, 0));
        assert_that!(playback_cells(
            Size::new(200, 0),
            (640, 480),
            test_picker().font_size()
        ))
        .is_equal_to(Size::new(0, 0));
        assert_that!(playback_cells(
            Size::new(200, 60),
            (0, 480),
            test_picker().font_size()
        ))
        .is_equal_to(Size::new(0, 0));
        assert_that!(playback_cells(
            Size::new(200, 60),
            (640, 0),
            test_picker().font_size()
        ))
        .is_equal_to(Size::new(0, 0));
    }

    /// A pane so small that the proportional fit rounds to nothing still has to produce a
    /// picture: one cell of video is useless, but `scale=0:120` is a command that fails.
    #[test]
    fn a_playback_in_a_sliver_of_a_pane_should_still_have_a_size() {
        // Act / Assert: a very wide source in two cells rounds its height to zero.
        assert_that!(playback_cells(
            Size::new(2, 40),
            (4000, 100),
            test_picker().font_size()
        ))
        .is_equal_to(Size::new(2, 1));

        // Act / Assert: and a very tall one in a single row rounds its width to zero.
        assert_that!(playback_cells(
            Size::new(200, 1),
            (100, 4000),
            test_picker().font_size()
        ))
        .is_equal_to(Size::new(1, 1));
    }

    /// The span is `pad` either side of the cue, and both ends are real edges: a cue near
    /// the start of the media has nothing before it, and one at the end has nothing after.
    #[test]
    fn a_span_should_be_the_cue_with_padding_that_stops_at_the_medias_edges() {
        // Act / Assert: an ordinary cue in the middle of the file.
        let (start, end) = playback_span(
            &cue(12_000, 14_500, "line"),
            Duration::from_secs(2),
            Duration::from_secs(60),
        );
        assert_that!(start).is_equal_to(Duration::from_secs(10));
        assert_that!(end).is_equal_to(Duration::from_millis(16_500));

        // Act / Assert: a cue closer to the start than the pad is long.
        let (start, end) = playback_span(
            &cue(500, 1500, "line"),
            Duration::from_secs(2),
            Duration::from_secs(60),
        );
        assert_that!(start).is_equal_to(Duration::ZERO);
        assert_that!(end).is_equal_to(Duration::from_millis(3500));

        // Act / Assert: and one running to the end of the media.
        let (start, end) = playback_span(
            &cue(58_000, 59_500, "line"),
            Duration::from_secs(2),
            Duration::from_secs(60),
        );
        assert_that!(start).is_equal_to(Duration::from_secs(56));
        assert_that!(end).is_equal_to(Duration::from_secs(60));
    }

    /// A duration that would not parse arrives here as zero, and clamping against that
    /// would make every span empty — the same reason [`seek_for`] only clamps a known one.
    #[test]
    fn a_span_should_not_be_clamped_against_a_duration_that_is_not_known() {
        // Act
        let (start, end) = playback_span(
            &cue(12_000, 14_500, "line"),
            Duration::from_secs(2),
            Duration::ZERO,
        );

        // Assert
        assert_that!(start).is_equal_to(Duration::from_secs(10));
        assert_that!(end).is_equal_to(Duration::from_millis(16_500));
    }

    /// A cue whose times sit past the end of the media — which a hand-edited subtitle file
    /// can perfectly well hold — must not produce a span that ends before it starts. An
    /// inverted one becomes a negative `-t`, which `ffmpeg` takes as "no limit" and turns a
    /// two-second playback into the rest of the file decoded into memory.
    #[test]
    fn a_span_past_the_end_of_the_media_should_collapse_rather_than_invert() {
        // Act
        let (start, end) = playback_span(
            &cue(90_000, 95_000, "line"),
            Duration::from_secs(2),
            Duration::from_secs(60),
        );

        // Assert: empty rather than backwards. Nothing decodes, and the page says there is
        // no video at this cue — which is the truth about a cue timed past the end.
        assert_that!(start).is_equal_to(Duration::from_secs(88));
        assert_that!(end).is_equal_to(start);
    }

    /// The whole span is decoded before a frame of it is shown — that is what buys a
    /// slideshow that never stutters — so it is real resident memory, and the frame rate is
    /// the only one of its three inputs that is not the user's to choose.
    #[test]
    fn a_span_too_large_to_hold_should_be_decoded_at_a_lower_rate() {
        // Arrange: an ordinary three-second span on a modest pane, well inside budget.
        let ordinary = affordable_fps(
            30,
            Duration::from_secs(3),
            PlaybackSpeed::NORMAL,
            (640, 360),
        );

        // Act / Assert: nothing is taken away from it.
        assert_that!(ordinary).is_equal_to(30);

        // Act / Assert: a full-screen terminal, where a playback is decoded at the cell's
        // own pixels — a 1136x680 pane is 2.3 MB a frame, and seven seconds of it at 30 fps
        // is past the budget. The rate comes down, and what comes back does fit.
        let span = Duration::from_secs(7);
        let squeezed = affordable_fps(30, span, PlaybackSpeed::NORMAL, (1136, 680));
        assert_that!(squeezed < 30).is_true();
        assert_that!(squeezed > MIN_PLAYBACK_FPS).is_true();
        assert_that!(u64::from(squeezed) * 7 * 1136 * 680 * 3 <= PLAYBACK_BUDGET).is_true();
    }

    /// Below about five frames a second consecutive frames stop reading as motion, and the
    /// thing being judged gets harder to see rather than easier — so a span that cannot fit
    /// even at the floor goes over budget instead. Being over budget is recoverable; a
    /// playback that cannot be watched is not.
    #[test]
    fn a_playback_should_never_be_slowed_past_the_point_of_being_watchable() {
        // Act / Assert: a span nothing could hold.
        assert_that!(affordable_fps(
            30,
            Duration::from_secs(600),
            PlaybackSpeed::NORMAL,
            (1920, 1080)
        ))
        .is_equal_to(MIN_PLAYBACK_FPS);

        // Act / Assert: and a user who asked for less than the floor gets what they asked
        // for — this lowers a rate, it never raises one.
        assert_that!(affordable_fps(
            2,
            Duration::from_secs(1),
            PlaybackSpeed::NORMAL,
            (16, 16)
        ))
        .is_equal_to(2);
    }

    /// A pane the renderer has not measured yet has no pixels to charge for, and dividing
    /// the budget by nothing would panic rather than merely misbehave.
    #[test]
    fn a_span_with_no_size_should_be_charged_nothing_rather_than_divided_by_it() {
        // Act / Assert
        assert_that!(affordable_fps(
            30,
            Duration::from_secs(7),
            PlaybackSpeed::NORMAL,
            (0, 0)
        ))
        .is_equal_to(30);
        // A span of no length is still charged for the one frame it holds, rather than
        // dividing by zero seconds.
        assert_that!(affordable_fps(
            30,
            Duration::ZERO,
            PlaybackSpeed::NORMAL,
            (200, 120)
        ))
        .is_equal_to(30);
    }

    /// The `fps` filter duplicates frames to reach a rate the source does not hold, so a
    /// 24 fps film asked for 60 comes back two and a half times the size for exactly the
    /// same picture — memory that comes straight out of the playback budget. Nothing about
    /// the command, the frame count or the cache would look wrong; only the picture would
    /// fail to be any smoother.
    #[test]
    fn a_source_should_never_be_asked_for_frames_it_does_not_hold() {
        // Act / Assert: film asked for more than it has, at normal speed.
        assert_that!(source_capped_fps(60, Some(24.0), PlaybackSpeed::NORMAL)).is_equal_to(24);
        // Rounded up, so 23.976 is asked for 24 rather than 23.
        assert_that!(source_capped_fps(
            60,
            Some(24_000.0 / 1_001.0),
            PlaybackSpeed::NORMAL
        ))
        .is_equal_to(24);

        // Act / Assert: a rate the source can meet is left alone — this only ever lowers.
        assert_that!(source_capped_fps(24, Some(60.0), PlaybackSpeed::NORMAL)).is_equal_to(24);
        assert_that!(source_capped_fps(30, Some(30.0), PlaybackSpeed::NORMAL)).is_equal_to(30);

        // Act / Assert: scaled by the speed, because that is how many source frames a second
        // of *output* covers. Half speed halves what the source can give; double doubles it.
        assert_that!(source_capped_fps(60, Some(24.0), PlaybackSpeed::HALF)).is_equal_to(12);
        assert_that!(source_capped_fps(60, Some(24.0), PlaybackSpeed::FASTEST)).is_equal_to(48);
        assert_that!(source_capped_fps(30, Some(24.0), PlaybackSpeed::FASTEST)).is_equal_to(30);

        // Act / Assert: a source ffprobe would not describe caps nothing — guessing a rate
        // would make a playback choppier for a file that was merely unreadable.
        assert_that!(source_capped_fps(30, None, PlaybackSpeed::NORMAL)).is_equal_to(30);
        assert_that!(source_capped_fps(30, Some(0.0), PlaybackSpeed::NORMAL)).is_equal_to(30);
        assert_that!(source_capped_fps(30, Some(f64::NAN), PlaybackSpeed::NORMAL)).is_equal_to(30);
        assert_that!(source_capped_fps(
            30,
            Some(f64::INFINITY),
            PlaybackSpeed::NORMAL
        ))
        .is_equal_to(30);
        // And a rate too large to be a `u32` leaves the user's own ceiling alone rather than
        // wrapping to something small.
        assert_that!(source_capped_fps(30, Some(1e30), PlaybackSpeed::NORMAL)).is_equal_to(30);

        // Act / Assert: a source slower than one frame a second still yields one, so a
        // slideshow of stills plays rather than decoding nothing at all.
        assert_that!(source_capped_fps(30, Some(0.2), PlaybackSpeed::NORMAL)).is_equal_to(1);
    }

    /// A slowed span is a *longer* span: `setpts` stretches it, so half speed comes back
    /// with twice the frames and costs twice the memory. Charging the media span rather than
    /// the stretched one would let a slow playback overshoot the budget by the speed factor
    /// — invisibly, because every count, key and command line still looks right.
    #[test]
    fn a_slowed_span_should_be_charged_for_the_frames_it_actually_holds() {
        // Arrange: a span and a size that sit just inside the budget at normal speed.
        let span = Duration::from_secs(7);
        let pixels = (1136, 680);
        let ordinary = affordable_fps(30, span, PlaybackSpeed::NORMAL, pixels);

        // Act: the same span at half speed, which yields twice the frames.
        let slowed = affordable_fps(30, span, PlaybackSpeed::HALF, pixels);

        // Assert: the rate comes down further, and what comes back still fits — charged
        // against the fourteen seconds of output rather than the seven of media.
        assert_that!(slowed < ordinary).is_true();
        assert_that!(u64::from(slowed) * 14 * 1136 * 680 * 3 <= PLAYBACK_BUDGET).is_true();

        // Act / Assert: and in the other direction a squeezed span is cheaper, so a rate the
        // budget had lowered can come back up.
        assert_that!(affordable_fps(30, span, PlaybackSpeed::FASTEST, pixels) > ordinary).is_true();
    }

    /// The speeds are offered fastest first, because that is the order the popup lists them
    /// in — one order for the list and the cursor is what stops the two disagreeing about
    /// which end is which.
    #[test]
    fn every_speed_should_read_as_a_value_and_be_offered_fastest_first() {
        // Act / Assert: the list, in the order a reader sees it.
        let labels: Vec<String> = PlaybackSpeed::STEPS
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_that!(labels).is_equal_to(vec![
            "2x".to_string(),
            "1.5x".to_string(),
            "1.25x".to_string(),
            "1x".to_string(),
            "0.75x".to_string(),
            "0.5x".to_string(),
            "0.25x".to_string(),
        ]);

        // Assert: the named speeds are the ends and the middle of that list, so the constants
        // the rest of the module reasons about cannot drift from what is offered.
        assert_that!(PlaybackSpeed::STEPS[0]).is_equal_to(PlaybackSpeed::FASTEST);
        assert_that!(PlaybackSpeed::STEPS[6]).is_equal_to(PlaybackSpeed::SLOWEST);
        assert_that!(PlaybackSpeed::STEPS.contains(&PlaybackSpeed::NORMAL)).is_true();
        assert_that!(PlaybackSpeed::STEPS.contains(&PlaybackSpeed::HALF)).is_true();

        // Assert: normal speed says so, and the multiplier is the fraction it reads as.
        assert_that!(PlaybackSpeed::NORMAL.is_normal()).is_true();
        assert_that!(PlaybackSpeed::HALF.is_normal()).is_false();
        assert_that!(PlaybackSpeed::default()).is_equal_to(PlaybackSpeed::NORMAL);
        assert_that!(PlaybackSpeed::HALF.as_f64()).is_equal_to(0.5);
        assert_that!(PlaybackSpeed::SLOWEST.as_f64()).is_equal_to(0.25);
        assert_that!(PlaybackSpeed::FASTEST.as_f64()).is_equal_to(2.0);
    }

    /// The picture and the sound are retimed by the *same* decode, which is what keeps them
    /// describing the same stretch of media — and what leaves `crate::audio` knowing nothing
    /// about speeds at all.
    #[test]
    fn a_speed_should_retime_both_the_picture_and_the_sound() {
        // Arrange
        let directory = PathBuf::from("/tmp/reel-tui-preview/playback-speed");
        let mut request = playback_request_at(
            Path::new("/media/show.mkv"),
            &directory,
            cue(2000, 3000, "line"),
            PlaybackSpeed::HALF,
        );
        request.audio = Some(crate::audio::OutputFormat::FALLBACK);

        // Act
        let arguments = arguments_of(&playback_command(
            &request,
            Duration::from_secs(3),
            "playback.srt",
        ));

        // Assert: `setpts` sits between the burn and the rate filter — behind `subtitles`,
        // so libass still lays the text out against the source's own timestamps, and ahead
        // of `fps`, so ten frames come out per second of *output*.
        let filters = filter_graph(&arguments);
        assert_that!(filters.as_str())
            .is_equal_to("subtitles=playback.srt,setpts=PTS/0.5,fps=10,scale=320:240");

        // Assert: the sound is stretched to match, pitch-preserving, in the same pass.
        assert_that!(argument_after(&arguments, "-af")).is_equal_to(Some("atempo=0.5".to_string()));

        // Act / Assert: at normal speed neither filter is emitted at all, so the ordinary
        // command is the one this feature was built on top of.
        request.speed = PlaybackSpeed::NORMAL;
        let ordinary = arguments_of(&playback_command(
            &request,
            Duration::from_secs(3),
            "playback.srt",
        ));
        assert_that!(filter_graph(&ordinary).as_str())
            .is_equal_to("subtitles=playback.srt,fps=10,scale=320:240");
        assert_that!(ordinary.iter().any(|argument| argument == "-af")).is_false();
    }

    /// One `atempo` cannot halve a span twice, so a quarter speed has to come out as two of
    /// them. A single `atempo=0.25` is not merely wrong — `ffmpeg` refuses it, and the
    /// playback fails with a message about a filter rather than about a speed.
    #[test]
    fn a_quarter_speed_should_chain_the_filters_one_atempo_cannot_do_alone() {
        // Act / Assert
        assert_that!(atempo_chain(PlaybackSpeed::SLOWEST).as_str())
            .is_equal_to("atempo=0.5,atempo=0.5");
        assert_that!(atempo_chain(PlaybackSpeed::HALF).as_str()).is_equal_to("atempo=0.5");
        assert_that!(atempo_chain(PlaybackSpeed::FASTEST).as_str()).is_equal_to("atempo=2");
        assert_that!(atempo_chain(PlaybackSpeed::STEPS[2]).as_str()).is_equal_to("atempo=1.25");

        // Assert: every speed on offer produces a chain `ffmpeg` will take — each link
        // inside the range one filter is reliable over, and their product the speed asked
        // for.
        for speed in PlaybackSpeed::STEPS {
            let links: Vec<f64> = atempo_chain(speed)
                .split(',')
                .map(|link| {
                    link.strip_prefix("atempo=")
                        .expect("every link should be an atempo")
                        .parse()
                        .expect("every link should carry a number")
                })
                .collect();
            for link in &links {
                assert_that!(*link >= ATEMPO_MIN && *link <= ATEMPO_MAX).is_true();
            }
            let product: f64 = links.iter().product();
            assert_that!((product - speed.as_f64()).abs() < 1e-9).is_true();
        }
    }

    /// Muting is asked for by wanting no sound rather than by turning a device down, which
    /// puts a muted playback on exactly the path media with no audio track already takes.
    #[test]
    fn a_muted_playback_should_ask_ffmpeg_for_no_sound_at_all() {
        // Arrange: a request carrying no audio format, which is what muting produces.
        let directory = PathBuf::from("/tmp/reel-tui-preview/playback-muted");
        let request = playback_request_at(
            Path::new("/media/show.mkv"),
            &directory,
            cue(2000, 3000, "line"),
            PlaybackSpeed::HALF,
        );

        // Act
        let arguments = arguments_of(&playback_command(
            &request,
            Duration::from_secs(3),
            "playback.srt",
        ));

        // Assert: no second output at all — not a silenced one.
        assert_that!(arguments.iter().any(|argument| argument == "0:a:0")).is_false();
        assert_that!(arguments.iter().any(|argument| argument == "-af")).is_false();
        assert_that!(arguments.iter().any(|argument| argument == AUDIO_FILE)).is_false();

        // Assert: and the picture is still retimed, so muting costs the sound and nothing
        // else.
        assert_that!(filter_graph(&arguments).contains("setpts=PTS/0.5")).is_true();
    }

    fn arguments_of(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|argument| argument.to_string_lossy().to_string())
            .collect()
    }

    fn filter_graph(arguments: &[String]) -> String {
        arguments
            .iter()
            .find(|argument| argument.starts_with("subtitles="))
            .expect("the filter graph should burn the staged cue in")
            .clone()
    }

    fn argument_after(arguments: &[String], flag: &str) -> Option<String> {
        let position = arguments.iter().position(|argument| argument == flag)?;
        arguments.get(position + 1).cloned()
    }

    /// The span is one buffer and the frames are read out of it by arithmetic, so the
    /// arithmetic is the thing that has to be right: an off-by-one in the stride does not
    /// crop the picture, it shears every frame after the first.
    #[test]
    fn a_span_should_be_sliced_into_the_frames_it_actually_holds() {
        // Arrange: three frames of a 2x2 picture, each byte naming its frame.
        let pixels = (2u32, 2u32);
        let stride = 2 * 2 * 3;
        let mut bytes = Vec::new();
        for frame in 0..3u8 {
            bytes.extend(std::iter::repeat_n(frame, stride));
        }
        let frames = PlaybackFrames::new(
            bytes,
            SpanShape {
                pixels,

                cells: Size::new(2, 1),
                picker: test_picker(),
            },
            10,
            PlaybackSpeed::NORMAL,
            Duration::from_secs(5),
            Vec::new(),
        );

        // Act / Assert
        assert_that!(frames.stride()).is_equal_to(stride);
        assert_that!(frames.count()).is_equal_to(3);
        assert_that!(frames.frame(0).unwrap()).is_equal_to([0u8; 12].as_slice());
        assert_that!(frames.frame(2).unwrap()).is_equal_to([2u8; 12].as_slice());
        // Past the end is nothing, rather than a slice straddling two frames.
        assert_that!(frames.frame(3).is_none()).is_true();
        assert_that!(frames.frame(usize::MAX).is_none()).is_true();
        // And the span lasts as long as the frames say it does, not as long as it was
        // asked to: `ffmpeg` decides how many a seek actually yields.
        assert_that!(frames.duration()).is_equal_to(Duration::from_millis(300));
    }

    /// A degenerate size would make the slicing divide by zero. Unreachable through
    /// `toggle_playback`, which refuses a pane with no cells, and cheap to rule out.
    #[test]
    fn a_span_with_no_frame_size_should_hold_no_frames_rather_than_divide_by_it() {
        // Arrange
        let frames = PlaybackFrames::new(
            vec![1, 2, 3, 4],
            SpanShape {
                pixels: (0, 0),

                cells: Size::new(0, 0),
                picker: test_picker(),
            },
            0,
            PlaybackSpeed::NORMAL,
            Duration::ZERO,
            Vec::new(),
        );

        // Act / Assert
        assert_that!(frames.stride()).is_equal_to(0);
        assert_that!(frames.count()).is_equal_to(0);
        assert_that!(frames.frame(0).is_none()).is_true();
        assert_that!(frames.duration()).is_equal_to(Duration::ZERO);
    }

    /// The frame buffer is tens of megabytes of pixels and `Protocol` has no `Debug` of its
    /// own, so a derived one would put the whole span into every failure message.
    #[test]
    fn a_span_should_describe_itself_by_its_shape_rather_than_its_pixels() {
        // Arrange
        let frames = PlaybackFrames::new(
            vec![7; 2 * 2 * 3 * 4],
            SpanShape {
                pixels: (2, 2),

                cells: Size::new(2, 1),
                picker: test_picker(),
            },
            25,
            PlaybackSpeed::NORMAL,
            Duration::from_secs(9),
            vec![0.5; 8],
        );

        // Act
        let described = format!("{frames:?}");

        // Assert
        assert_that!(described.as_str()).contains("frames: 4");
        assert_that!(described.as_str()).contains("fps: 25");
        assert_that!(described.contains("7, 7, 7")).is_false();
    }

    /// A playback runs while the background pass is still walking the track, and what it
    /// stages is a *differently timed* copy of the very cue the interactive grab is drawing.
    /// Sharing a filename with either would have each burning the other's timing in.
    #[test]
    fn the_playback_should_stage_its_cue_under_a_name_of_its_own() {
        // Act / Assert
        assert_that!(CueSlot::Playback.file(&CueStyle::SubRip).as_str())
            .is_equal_to("playback.srt");
        assert_that!(CueSlot::Playback.file(&ass_style()).as_str()).is_equal_to("playback.ass");
        // And distinct from every other slot's, which is the whole point.
        let names = [
            CueSlot::Interactive.file(&CueStyle::SubRip),
            CueSlot::Warm(0).file(&CueStyle::SubRip),
            CueSlot::Playback.file(&CueStyle::SubRip),
        ];
        assert_that!(
            names
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
        )
        .is_equal_to(3);
    }

    fn playback_request(media: &Path, workspace: &Path, cue: Cue) -> PlaybackRequest {
        playback_request_at(media, workspace, cue, PlaybackSpeed::NORMAL)
    }

    fn playback_request_at(
        media: &Path,
        workspace: &Path,
        cue: Cue,
        speed: PlaybackSpeed,
    ) -> PlaybackRequest {
        let cells = Size::new(32, 12);
        PlaybackRequest {
            generation: 1,
            source: source(media, workspace),
            cue_index: 0,
            on_screen: vec![cue.clone()],
            cue,
            span_start: Duration::from_secs(1),
            span_end: Duration::from_secs(4),
            fps: 10,
            speed,
            pixels: playback_pixels(cells, test_picker().font_size()),
            cells,
            audio: None,
        }
    }

    /// The picker a test's playbacks are sized and encoded for.
    ///
    /// Kitty, because that is what production looks like: [`drawing_picker`] refuses a
    /// halfblocks terminal, so a span is always sized and drawn for a real image protocol.
    /// Built from `Picker::halfblocks` and switched, since that is the one constructor that
    /// does not query the terminal — a test runner has none to query.
    fn test_picker() -> Picker {
        let mut picker = Picker::halfblocks();
        picker.set_protocol_type(ProtocolType::Kitty);
        picker
    }

    /// The command is what the whole feature rests on, and three of its parts are load
    /// bearing in ways that fail invisibly: the seek and the length that bound the span,
    /// the literal `scale` the slicing depends on, and the filter order that lets libass
    /// lay the text out before anything shrinks it.
    #[test]
    fn the_playback_command_should_bound_the_span_and_scale_it_exactly() {
        // Arrange
        let directory = PathBuf::from("/tmp/reel-tui-preview/playback-command");
        let request = playback_request(
            Path::new("/media/it's a show; [2024].mkv"),
            &directory,
            cue(2000, 3000, "line"),
        );

        // Act
        let command = playback_command(&request, Duration::from_secs(3), "playback.srt");
        let arguments: Vec<String> = command
            .get_args()
            .map(|argument| argument.to_string_lossy().to_string())
            .collect();

        // Assert: the span, seeked to before the input so the decoder starts there.
        let seek = arguments
            .iter()
            .position(|argument| argument == "-ss")
            .unwrap();
        assert_that!(arguments[seek + 1].as_str()).is_equal_to("1.000");
        assert_that!(arguments[seek + 2].as_str()).is_equal_to("-t");
        assert_that!(arguments[seek + 3].as_str()).is_equal_to("3.000");
        assert_that!(
            arguments
                .iter()
                .position(|argument| argument == "-i")
                .unwrap()
                > seek + 3
        )
        .is_true();

        // Assert: `scale` is a literal size with no aspect-ratio option on it. A frame that
        // came back one row short would shear every frame after the first, not crop.
        let filters = arguments
            .iter()
            .find(|argument| argument.starts_with("subtitles="))
            .expect("the filter graph should burn the staged cue in");
        assert_that!(filters.as_str()).is_equal_to("subtitles=playback.srt,fps=10,scale=320:240");
        assert_that!(filters.contains("force_original_aspect_ratio")).is_false();

        // Assert: raw pixels out, and the workspace as the working directory so the staged
        // name inside the filter graph needs no escaping.
        assert_that!(arguments.iter().any(|argument| argument == "rawvideo")).is_true();
        assert_that!(arguments.iter().any(|argument| argument == "rgb24")).is_true();
        assert_that!(command.get_current_dir()).is_equal_to(Some(directory.as_path()));

        // Assert: no audio output at all when the media has none. `-map 0:a:0` on such a
        // file fails the whole run, and the optional `0:a:0?` form leaves an output with no
        // streams in it, which fails just as hard — so it is left out rather than made
        // conditional inside `ffmpeg`.
        assert_that!(arguments.iter().any(|argument| argument == "f32le")).is_false();
    }

    /// The sound comes out of the same seek and the same decode as the picture, which is
    /// what makes them describe the same stretch of media by construction rather than by
    /// two commands agreeing about a timestamp — and it is emitted at the device's own
    /// format, so the audio callback copies rather than resamples.
    #[test]
    fn the_playback_command_should_take_its_sound_from_the_same_pass_as_its_picture() {
        // Arrange
        let directory = PathBuf::from("/tmp/reel-tui-preview/playback-audio-command");
        let mut request = playback_request(
            Path::new("/media/show.mkv"),
            &directory,
            cue(2000, 3000, "line"),
        );
        request.audio = Some(crate::audio::OutputFormat {
            sample_rate: 44_100,
            channels: 1,
        });

        // Act
        let command = playback_command(&request, Duration::from_secs(3), "playback.srt");
        let arguments: Vec<String> = command
            .get_args()
            .map(|argument| argument.to_string_lossy().to_string())
            .collect();

        // Assert: one input, so one seek and one decode feeding both outputs.
        assert_that!(
            arguments
                .iter()
                .filter(|argument| *argument == "-i")
                .count()
        )
        .is_equal_to(1);

        // Assert: the device's rate and channel count, verbatim.
        let rate = arguments
            .iter()
            .position(|argument| argument == "-ar")
            .unwrap();
        assert_that!(arguments[rate + 1].as_str()).is_equal_to("44100");
        let channels = arguments
            .iter()
            .position(|argument| argument == "-ac")
            .unwrap();
        assert_that!(arguments[channels + 1].as_str()).is_equal_to("1");

        // Assert: raw little-endian floats, to a file rather than a second pipe — two pipes
        // into one process deadlock the moment either reader falls behind, and the video
        // pipe is drained on a thread that knows nothing about the other.
        assert_that!(arguments.iter().any(|argument| argument == "f32le")).is_true();
        assert_that!(arguments.last().map(String::as_str)).is_equal_to(Some(AUDIO_FILE));
    }

    /// Media with no audio track, and a run whose audio file never appeared, are the same
    /// thing as far as the page is concerned: a silent slideshow at the right rate, rather
    /// than a failure or a panic on a short read.
    #[test]
    fn sound_that_is_not_there_should_read_as_silence_rather_than_fail() {
        // Arrange
        let directory = scratch("playback-samples");

        // Act / Assert: a file that was never written.
        assert_that!(read_samples(&directory.join("playback.f32")).is_empty()).is_true();

        // Act / Assert: and one whose length is not a whole number of floats, which is what
        // a run killed part-way through writing leaves behind.
        let path = directory.join("ragged.f32");
        let mut bytes = 1.0f32.to_le_bytes().to_vec();
        bytes.extend([0u8, 1, 2]);
        std::fs::write(&path, &bytes).unwrap();
        assert_that!(read_samples(&path).as_slice()).is_equal_to([1.0f32].as_slice());

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// The end-to-end shape of the sound: a real decode of media that has some, read back
    /// as the floats the device will be handed.
    #[test]
    fn a_decoded_span_should_bring_back_the_sound_that_goes_with_it() {
        // Arrange: six seconds of tone against six seconds of blue, and a two-second span.
        require_ffmpeg("a_decoded_span_should_bring_back_the_sound_that_goes_with_it");
        let directory = scratch("playback-sound");
        let media = directory.join("clip.mkv");
        let built = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-nostdin",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:s=320x240:r=10:d=6",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=6",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-preset",
                "ultrafast",
                "-c:a",
                "aac",
            ])
            .arg(&media)
            .output()
            .expect("ffmpeg should be runnable");
        assert!(
            built.status.success(),
            "failed to build the fixture: {}",
            String::from_utf8_lossy(&built.stderr)
        );
        let mut request = playback_request(&media, &directory, cue(2000, 3000, "line"));
        request.span_start = Duration::from_secs(1);
        request.span_end = Duration::from_secs(3);
        request.audio = Some(crate::audio::OutputFormat {
            sample_rate: 48_000,
            channels: 2,
        });
        let never = || false;

        // Act
        let outcome =
            decoded(&request, &test_picker(), &never).expect("a live playback reports its outcome");

        // Assert
        let PlaybackOutcome::Ready(mut frames) = outcome else {
            panic!("a clip with a tone in it should decode");
        };
        let samples = frames.take_samples();
        // Two seconds of stereo at 48 kHz, give or take how the seek lands on a frame.
        let format = request.audio.unwrap();
        let played = format.duration_of(samples.len());
        assert_that!(played >= Duration::from_millis(1900)).is_true();
        assert_that!(played <= Duration::from_millis(2100)).is_true();
        // And it is a tone rather than silence, which is what a mapping that pulled the
        // wrong stream — or no stream — would leave behind. The threshold is well under
        // `sine`'s own peak, which lavfi produces at around a tenth of full scale, and well
        // over the exact zero that digital silence is.
        assert_that!(samples.iter().any(|sample| sample.abs() > 0.01)).is_true();
        // Taken, not copied: the device owns what it plays, and a span's samples are
        // megabytes nothing else looks at again.
        assert_that!(frames.take_samples().is_empty()).is_true();

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// The end-to-end shape of a playback, against a real decode: the span comes back as
    /// whole frames at the size that was asked for, and — the part no assertion short of
    /// the pixels would notice — the cue is burned in *only where it falls inside the span*.
    ///
    /// A staging that kept the cue's original times passes every count, every size and
    /// every key here while showing a span with no subtitle in it at all, which is why this
    /// looks at what the frames actually hold.
    #[test]
    fn a_decoded_span_should_carry_its_cue_only_where_the_cue_falls_in_it() {
        // Arrange: six seconds of flat blue, and a cue running 3.0–4.0 s. With a one-second
        // span start the line belongs 2.0–3.0 s in, and nowhere else.
        //
        // Rendered at 160x120 rather than the tiny size the other tests use, because that
        // is about where a burned-in line survives being scaled down at all: libass sizes
        // text as a fraction of the frame, so at 96x72 the strokes average away into the
        // background entirely. Measured, not guessed.
        require_ffmpeg("a_decoded_span_should_carry_its_cue_only_where_the_cue_falls_in_it");
        let directory = scratch("playback-decode");
        let media = video(&directory);
        let mut request = playback_request(&media, &directory, cue(3000, 4000, "BURNED IN"));
        request.span_start = Duration::from_secs(1);
        request.span_end = Duration::from_secs(5);
        // Small deliberately: `playback_pixels` is eight subcells across *and* down, so a
        // pane-sized cell area here would hold the whole span in tens of megabytes of RGB
        // for a comparison that only needs two frames to differ.
        request.cells = Size::new(16, 6);
        request.pixels = playback_pixels(request.cells, test_picker().font_size());
        let never = || false;

        // Act
        let outcome =
            decoded(&request, &test_picker(), &never).expect("a live playback reports its outcome");

        // Assert
        let PlaybackOutcome::Ready(frames) = outcome else {
            panic!("a black clip with a cue in it should decode");
        };
        // Four seconds at ten frames a second, give or take how the seek lands.
        assert_that!(frames.count() >= 38 && frames.count() <= 42).is_true();
        assert_that!(frames.pixels).is_equal_to((160, 120));
        // Whole frames, with nothing left over — which is what says the stride agrees with
        // what `ffmpeg` was told to produce.
        assert_that!(frames.frame(frames.count() - 1).is_some()).is_true();
        assert_that!(frames.frame(frames.count()).is_none()).is_true();

        // Assert: the burn is inside the cue's own second of the span and nowhere else.
        // Measured against another frame of the same flat clip rather than against a
        // brightness threshold, because the only thing that can differ between two frames
        // of a solid colour is the text drawn onto one of them.
        let flat = frames.frame(2).expect("frame inside the span").to_vec();
        let burned = |index: usize| {
            frames
                .frame(index)
                .expect("frame inside the span")
                .iter()
                .zip(&flat)
                .filter(|(drawn, plain)| drawn != plain)
                .count()
        };
        assert_that!(burned(5)).is_equal_to(0);
        assert_that!(burned(15)).is_equal_to(0);
        assert_that!(burned(25) > 0).is_true();
        assert_that!(burned(35)).is_equal_to(0);

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// An exit status without a subprocess to produce it.
    ///
    /// `ExitStatus` cannot be constructed portably, and this crate already only builds
    /// where `/proc/mounts` exists (`mount.rs`), so the Unix extension is no narrower than
    /// the rest of it.
    fn ok_status() -> std::process::ExitStatus {
        std::os::unix::process::ExitStatusExt::from_raw(0)
    }

    fn failed_status() -> std::process::ExitStatus {
        std::os::unix::process::ExitStatusExt::from_raw(256)
    }

    /// The three ways a playback run ends, from the outcome itself: giving up part-way and
    /// failing to start are not endings a test can reliably steer a real `ffmpeg` into, and
    /// the first must not be mistaken for a span that decoded nothing.
    #[test]
    fn every_ending_a_playback_run_can_have_should_be_told_apart() {
        // Act / Assert: the page went away mid-decode. Reported as neither ready nor
        // failed, so nothing is sent and nothing starts playing under a page that stopped.
        assert!(matches!(
            played(RunOutcome::Abandoned),
            SpanOutcome::Abandoned
        ));

        // Act / Assert: `ffmpeg` could not be started at all.
        let SpanOutcome::Failed(message) = played(RunOutcome::Failed(
            "ffmpeg was not found in PATH.".to_string(),
        )) else {
            panic!("a run that could not start is a failure");
        };
        assert_that!(message.as_str()).is_equal_to("ffmpeg was not found in PATH.");

        // Act / Assert: `ffmpeg` ran and refused, and its reason reaches the page rather
        // than only the heading — which on its own reads as a bug in reel.
        let SpanOutcome::Failed(message) = played(RunOutcome::Finished(Output {
            status: failed_status(),
            stdout: Vec::new(),
            stderr: b"Invalid data found when processing input\n".to_vec(),
        })) else {
            panic!("a run ffmpeg refused is a failure");
        };
        assert_that!(message.as_str()).contains("Could not play this cue");
        assert_that!(message.as_str()).contains("Invalid data found");

        // Act / Assert: and a run that worked hands its pixels straight back.
        let SpanOutcome::Ready(pixels) = played(RunOutcome::Finished(Output {
            status: ok_status(),
            stdout: vec![1, 2, 3],
            stderr: Vec::new(),
        })) else {
            panic!("a successful run yields its pixels");
        };
        assert_that!(pixels.as_slice()).is_equal_to([1u8, 2, 3].as_slice());
    }

    /// **The property the whole feature rests on: the picture and the sound describe the
    /// same instant.**
    ///
    /// A *constant* offset between them is the one failure this page cannot have and the
    /// one nothing else here would catch — every count, every size, every key and every
    /// frame is right, and the playback simply reports every subtitle as late by the same
    /// amount. Once the page can edit times, that reading gets written to the file.
    ///
    /// `-ss` before `-i` is where such an offset would come from: it resets output
    /// timestamps to about zero, and it does so for both outputs of the pass. This measures
    /// that rather than trusting it, with a fixture whose picture turns white at the exact
    /// instant a tone starts — so the two streams carry the same event and the decoded span
    /// can be asked where each of them put it.
    ///
    /// Every playback of a page writes its sound to the same name in the same workspace, and
    /// a muted one writes nothing at all — so reading that file back unconditionally hands
    /// the page whatever the *last* unmuted playback left there. The user turns the sound
    /// off, presses play, and hears the previous cue under the new picture.
    ///
    /// Reproduced in the order that causes it: an unmuted run, then a muted one against the
    /// same workspace. A muted run on its own passes either way, which is why this asserts on
    /// the second of two.
    #[test]
    fn a_muted_playback_should_not_inherit_the_sound_of_the_one_before_it() {
        // Arrange: a second of audible media, and a workspace both runs share.
        require_ffmpeg("a_muted_playback_should_not_inherit_the_sound_of_the_one_before_it");
        let directory = scratch("playback-muted-stale");
        let media = directory.join("clip.mkv");
        let built = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-nostdin",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=64x64:r=10:d=3",
                "-f",
                "lavfi",
                "-i",
                "aevalsrc='0.5*sin(2*PI*440*t)':d=3:s=48000",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "pcm_s16le",
            ])
            .arg(&media)
            .output()
            .expect("ffmpeg should be runnable");
        assert!(
            built.status.success(),
            "failed to build the fixture: {}",
            String::from_utf8_lossy(&built.stderr)
        );
        let cells = Size::new(16, 8);
        let mut request = playback_request(&media, &directory, cue(0, 1000, "X"));
        request.span_start = Duration::ZERO;
        request.span_end = Duration::from_secs(2);
        request.fps = 10;
        request.cells = cells;
        request.pixels = playback_pixels(cells, test_picker().font_size());
        request.audio = Some(crate::audio::OutputFormat {
            sample_rate: 48_000,
            channels: 2,
        });
        let never = || false;

        // Arrange: an ordinary playback first, which leaves its sound in the workspace.
        let outcome = decoded(&request, &test_picker(), &never).expect("the first run reports");
        let PlaybackOutcome::Ready(mut frames) = outcome else {
            panic!("the fixture should decode");
        };
        assert_that!(frames.take_samples().is_empty()).is_false();
        assert_that!(directory.join(AUDIO_FILE).exists()).is_true();

        // Act: the same page, muted.
        request.audio = None;
        let outcome = decoded(&request, &test_picker(), &never).expect("the second run reports");
        let PlaybackOutcome::Ready(mut frames) = outcome else {
            panic!("the muted run should decode");
        };

        // Assert: silent, and the stale file is gone rather than waiting for the next one.
        assert_that!(frames.take_samples().is_empty()).is_true();
        assert_that!(directory.join(AUDIO_FILE).exists()).is_false();

        // Cleanup
        std::fs::remove_dir_all(&directory).unwrap();
    }

    /// Measured at 0.1 ms on the machine this was written on. The tolerance is one frame
    /// period, which is the finest the picture can resolve anyway.
    #[test]
    fn the_sound_and_the_picture_of_a_span_should_describe_the_same_instant() {
        // Arrange: eight seconds that go from black-and-silent to white-and-audible at
        // exactly three seconds in.
        require_ffmpeg("the_sound_and_the_picture_of_a_span_should_describe_the_same_instant");
        let directory = scratch("playback-sync");
        let media = directory.join("clip.mkv");
        let built = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-nostdin",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=320x240:r=25:d=8",
                "-f",
                "lavfi",
                "-i",
                "aevalsrc='0.5*sin(2*PI*440*t)*gte(t,3)':d=8:s=48000",
                "-vf",
                "drawbox=x=0:y=0:w=iw:h=ih:color=white:t=fill:enable='gte(t,3)'",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "pcm_s16le",
            ])
            .arg(&media)
            .output()
            .expect("ffmpeg should be runnable");
        assert!(
            built.status.success(),
            "failed to build the fixture: {}",
            String::from_utf8_lossy(&built.stderr)
        );

        // Arrange: a span starting a second in, so the event lands two seconds into it —
        // which is what makes a constant offset visible rather than hidden at zero.
        let format = crate::audio::OutputFormat {
            sample_rate: 48_000,
            channels: 2,
        };
        let cells = Size::new(160, 60);
        let mut request = playback_request(&media, &directory, cue(3000, 4000, "X"));
        request.span_start = Duration::from_secs(1);
        request.span_end = Duration::from_secs(6);
        request.fps = 25;
        request.cells = cells;
        request.pixels = playback_pixels(cells, test_picker().font_size());
        request.audio = Some(format);
        let never = || false;

        // Act
        let outcome =
            decoded(&request, &test_picker(), &never).expect("a live playback reports its outcome");
        let PlaybackOutcome::Ready(mut frames) = outcome else {
            panic!("the fixture should decode");
        };
        let samples = frames.take_samples();

        // Assert: where the picture turns white.
        let brightened = (0..frames.count())
            .find(|index| {
                let frame = frames.frame(*index).expect("frame inside the span");
                frame
                    .iter()
                    .take(3000)
                    .map(|byte| u32::from(*byte))
                    .sum::<u32>()
                    / 3000
                    > 128
            })
            .expect("the picture should turn white inside the span");
        let picture_at = Duration::from_secs_f64(brightened as f64 / f64::from(request.fps));

        // Assert: and where the tone starts. Stepped a frame at a time so the answer is in
        // frames of sound rather than in interleaved samples.
        let channels = usize::from(format.channels);
        let onset = samples
            .chunks_exact(channels)
            .position(|frame| frame[0].abs() > 0.05)
            .expect("the tone should start inside the span");
        let sound_at = format.duration_of(onset * channels);

        // Assert: both put the event two seconds into the span, and — the part that matters
        // — they agree with each other to within a frame.
        let frame_period = Duration::from_secs(1) / request.fps;
        assert_that!(picture_at.abs_diff(Duration::from_secs(2)) <= frame_period).is_true();
        assert_that!(sound_at.abs_diff(Duration::from_secs(2)) <= frame_period).is_true();
        assert_that!(picture_at.abs_diff(sound_at) <= frame_period).is_true();

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// A span the user has stopped waiting for is not reported at all — a span arriving
    /// after `p` was pressed again would start playing under a page that has gone back to
    /// its still frame.
    #[test]
    fn a_playback_stopped_before_it_started_should_report_nothing() {
        // Arrange
        let directory = scratch("playback-abandoned");
        let request = playback_request(&directory.join("clip.mkv"), &directory, cue(0, 1000, "x"));
        let always = || true;

        // Act / Assert
        assert_that!(decoded(&request, &test_picker(), &always).is_none()).is_true();
        // And nothing was staged, since it gave up before writing anything.
        assert_that!(directory.join("playback.srt").exists()).is_false();

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// A workspace that has gone — the page closed while the key was in flight — must
    /// report why rather than spawning an `ffmpeg` that cannot find its subtitle.
    #[test]
    fn a_playback_that_cannot_stage_its_cue_should_say_so() {
        // Arrange
        let request = playback_request(
            Path::new("/media/show.mkv"),
            Path::new("/tmp/reel-tui-preview/does-not-exist"),
            cue(0, 1000, "x"),
        );
        let never = || false;

        // Act
        let outcome =
            decoded(&request, &test_picker(), &never).expect("a live playback reports its outcome");

        // Assert
        let PlaybackOutcome::Failed(message) = outcome else {
            panic!("staging into a missing directory cannot succeed");
        };
        assert_that!(message.as_str()).contains("Could not stage the cue to burn in");
    }

    /// `ffmpeg` refusing the media is the ordinary failure — a file that is not video, a
    /// codec this build cannot decode — and the page has to say which, not just that.
    #[test]
    fn a_playback_of_something_that_is_not_video_should_report_what_ffmpeg_said() {
        // Arrange
        require_ffmpeg("a_playback_of_something_that_is_not_video_should_report_what_ffmpeg_said");
        let directory = scratch("playback-not-video");
        let media = directory.join("clip.mkv");
        std::fs::write(&media, b"this is not a matroska file").unwrap();
        let request = playback_request(&media, &directory, cue(0, 1000, "x"));
        let never = || false;

        // Act
        let outcome =
            decoded(&request, &test_picker(), &never).expect("a live playback reports its outcome");

        // Assert
        let PlaybackOutcome::Failed(message) = outcome else {
            panic!("a text file is not something ffmpeg can decode");
        };
        assert_that!(message.as_str()).contains("Could not play this cue");
        assert_that!(message.len() > "Could not play this cue.".len()).is_true();

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// A run that succeeded without writing a whole frame is what seeking past the end of
    /// the media looks like. Nothing to show, and — without this — no reason on screen for
    /// it either: the page would return to its still frame as though `p` had done nothing.
    #[test]
    fn a_span_that_decoded_no_frames_should_report_that_rather_than_play_silence() {
        // Arrange: a span starting well past the end of a six-second clip.
        require_ffmpeg("a_span_that_decoded_no_frames_should_report_that_rather_than_play_silence");
        let directory = scratch("playback-past-end");
        let media = video(&directory);
        let mut request = playback_request(&media, &directory, cue(0, 1000, "x"));
        request.span_start = Duration::from_secs(30);
        request.span_end = Duration::from_secs(33);
        let never = || false;

        // Act
        let outcome =
            decoded(&request, &test_picker(), &never).expect("a live playback reports its outcome");

        // Assert
        let PlaybackOutcome::Failed(message) = outcome else {
            panic!("there is no video thirty seconds into a six-second clip");
        };
        assert_that!(message.as_str()).contains("no video at this cue");

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// The worker answers on the same channel as the other three, keyed by the generation
    /// the request carried — and a result for a playback that has been stopped is dropped
    /// rather than sent, so it cannot start playing under a page that has moved on.
    #[test]
    fn the_playback_worker_should_answer_only_for_the_playback_still_wanted() {
        // Arrange
        require_ffmpeg("the_playback_worker_should_answer_only_for_the_playback_still_wanted");
        let directory = scratch("playback-worker");
        let media = video(&directory);
        let request = playback_request(&media, &directory, cue(1000, 2000, "line"));
        let (events_tx, events) = mpsc::channel();
        let live = AtomicU64::new(request.generation);

        // Act
        let alive = playback(&request, &test_picker(), &live, &events_tx);

        // Assert
        assert_that!(alive).is_true();
        let event = events.try_recv().expect("a live playback reports its span");
        let PreviewEvent::Playback {
            generation,
            cue_index,
            outcome: PlaybackOutcome::Ready(frames),
        } = event
        else {
            panic!("the span should have decoded");
        };
        assert_that!(generation).is_equal_to(1);
        assert_that!(cue_index).is_equal_to(0);
        assert_that!(frames.count() > 0).is_true();

        // Act / Assert: the same request against a generation that has moved on says
        // nothing at all, and the worker stays up for the next one.
        let stale = AtomicU64::new(request.generation + 1);
        assert_that!(playback(&request, &test_picker(), &stale, &events_tx)).is_true();
        assert_that!(events.try_recv().is_err()).is_true();

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// The worker stops only when nobody is listening for events any more, which means the
    /// application itself has gone.
    #[test]
    fn the_playback_worker_should_stop_once_nobody_is_listening() {
        // Arrange
        let directory = scratch("playback-deaf");
        let request = playback_request(&directory.join("clip.mkv"), &directory, cue(0, 1000, "x"));
        let (events_tx, events) = mpsc::channel();
        drop(events);
        let live = AtomicU64::new(request.generation);

        // Act / Assert
        assert_that!(playback(&request, &test_picker(), &live, &events_tx)).is_false();

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }
}
