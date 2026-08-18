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

use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use ratatui::layout::Size;
use ratatui_image::Resize;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;

use crate::cue::{Cue, format_srt_timestamp, read_srt};

/// Filename an embedded track is staged under inside the page's workspace.
pub const CUES_FILE: &str = "cues.srt";

/// Filename the one retimed cue being previewed is staged under.
///
/// Deliberately short and constant. `ffmpeg` runs with the workspace as its working
/// directory and is handed this bare relative name, so the value inside the `subtitles=`
/// filter is a literal that needs no escaping at all — the alternative is quoting a user's
/// path through three layers (filtergraph `[],;`, filter-arg `:`, option-parser `\` and
/// `'`), which is a class of bug this feature simply does not have to have.
pub const CUE_FILE: &str = "cue.srt";

/// How often a running `ffmpeg` is checked on, matching `edit.rs`'s runner.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

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
    pub workspace: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrepareOutcome {
    Ready(Vec<Cue>),
    Failed(String),
}

/// One cue to draw a video frame for.
///
/// Carries the cue's own text rather than a pointer into the page's cue list: the worker
/// writes it back out as a one-cue subtitle file, and cannot reach `App` to look it up.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrameRequest {
    pub generation: u64,
    /// Which cue in the page's list this is for, so a frame that arrives after the
    /// selection has moved on can be recognised as stale.
    pub cue_index: usize,
    pub media: PathBuf,
    pub workspace: PathBuf,
    pub text: String,
    /// Where to grab the frame, already clamped inside the media by the caller.
    pub seek: Duration,
    /// The preview pane, in terminal cells, as the renderer measured it.
    pub cells: Size,
}

/// A frame ready to draw, or why there is none.
///
/// `Failed` is not an error dialog: the page keeps showing the cue's text, and this is
/// what it can say underneath.
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

/// Anything either worker has to say about the open page.
///
/// One channel for both, so the event loop keeps a single drain and cannot end up
/// pumping one worker's results and not the other's.
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
}

/// The UI thread's half of the preview worker.
///
/// Wraps the request channel together with the generation cell rather than handing `App`
/// both, because the two must move together: sending a request *is* what makes its
/// generation the live one, and every other write to the cell means "abandon whatever is
/// running".
#[derive(Debug)]
pub struct PreviewHandles {
    prepare_tx: Sender<PrepareRequest>,
    /// `None` when the terminal offered no image protocol, which is also what tests get.
    /// The page then stays on its text preview, the same fallback a build without libass
    /// takes.
    frame_tx: Option<Sender<FrameRequest>>,
    /// The page generation both workers should still be working for, shared with them.
    ///
    /// An `AtomicU64` rather than a cancellation flag because it answers both questions
    /// a worker has — "is this request still wanted" and "should I stop now" — with one
    /// comparison, and because the value is already unique per page opening.
    live_generation: Arc<AtomicU64>,
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

    /// Whether frames are being produced at all, so the page can skip asking for one it
    /// would only have to fall back from.
    pub fn draws_frames(&self) -> bool {
        self.frame_tx.is_some()
    }

    /// Tells the workers that the page they were working for is gone, so a running
    /// `ffmpeg` is killed rather than left demuxing a file nobody is looking at.
    pub fn abandon(&self, generation: u64) {
        self.live_generation.store(generation, Ordering::Relaxed);
    }
}

/// Starts the page's background workers: one that reads a track's cues, and — when the
/// terminal can draw images at all — one that renders the frame at the selected cue.
///
/// Two threads rather than one because they are asked for different things at different
/// rates: cues once per page opening, frames once per settled selection. A single thread
/// would make a held-down `j` queue behind an extraction that is demuxing a container.
pub fn spawn_preview_workers(picker: Option<Picker>) -> (PreviewHandles, Receiver<PreviewEvent>) {
    let (prepare_tx, prepare_rx) = mpsc::channel::<PrepareRequest>();
    let (event_tx, event_rx) = mpsc::channel();
    let live_generation = Arc::new(AtomicU64::new(0));
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

    let frame_tx = picker.map(|picker| {
        let (frame_tx, frame_rx) = mpsc::channel::<FrameRequest>();
        let frame_generation = Arc::clone(&live_generation);
        std::thread::spawn(move || {
            while let Ok(request) = frame_rx.recv() {
                let request = newest(request, &frame_rx);
                let Some(outcome) = frame(&request, &picker, &frame_generation) else {
                    continue;
                };
                if event_tx
                    .send(PreviewEvent::Frame {
                        generation: request.generation,
                        cue_index: request.cue_index,
                        outcome,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        frame_tx
    });

    (
        PreviewHandles {
            prepare_tx,
            frame_tx,
            live_generation,
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

/// Handles with no worker thread behind them, so `App`'s dispatch can be asserted from
/// the request itself instead of through a real extraction.
#[cfg(test)]
pub(crate) struct TestHandles {
    pub handles: PreviewHandles,
    pub prepare_rx: Receiver<PrepareRequest>,
    pub frame_rx: Receiver<FrameRequest>,
    pub live_generation: Arc<AtomicU64>,
}

#[cfg(test)]
pub(crate) fn test_handles() -> TestHandles {
    let (prepare_tx, prepare_rx) = mpsc::channel();
    let (frame_tx, frame_rx) = mpsc::channel();
    let live_generation = Arc::new(AtomicU64::new(0));
    TestHandles {
        handles: PreviewHandles {
            prepare_tx,
            frame_tx: Some(frame_tx),
            live_generation: Arc::clone(&live_generation),
        },
        prepare_rx,
        frame_rx,
        live_generation,
    }
}

/// Reads one track's cues, or `None` if the page it was for closed on the way.
fn prepare(request: &PrepareRequest, live_generation: &AtomicU64) -> Option<PrepareOutcome> {
    let abandoned = || live_generation.load(Ordering::Relaxed) != request.generation;
    if abandoned() {
        return None;
    }
    let Some(index) = request.stream_index else {
        return Some(read_cues(&request.input));
    };

    let staged = request.workspace.join(CUES_FILE);
    let mut command = extract_command(&request.input, index, &staged);
    extracted(run_cancellable(&mut command, &abandoned), &staged)
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

/// Renders the frame at one cue with that cue burned into it, or `None` if the page it
/// was for closed on the way.
fn frame(
    request: &FrameRequest,
    picker: &Picker,
    live_generation: &AtomicU64,
) -> Option<FrameOutcome> {
    let abandoned = || live_generation.load(Ordering::Relaxed) != request.generation;
    if abandoned() {
        return None;
    }
    let staged = request.workspace.join(CUE_FILE);
    if let Err(error) = std::fs::write(&staged, one_cue_srt(&request.text)) {
        return Some(FrameOutcome::Failed(format!(
            "Could not stage the cue to burn in: {error}"
        )));
    }

    let font = picker.font_size();
    let pixels = (
        u32::from(request.cells.width) * u32::from(font.width),
        u32::from(request.cells.height) * u32::from(font.height),
    );
    let mut command = frame_command(&request.media, request.seek, pixels, &request.workspace);
    drawn(
        run_cancellable(&mut command, &abandoned),
        picker,
        request.cells,
    )
}

/// Turns a finished grab into something the page can draw.
///
/// Separated from `frame` for the same reason `extracted` is separated from `prepare`:
/// giving up part-way and failing to start are endings a test cannot reliably steer a
/// real `ffmpeg` into, and they are the two that must not be mistaken for a blank frame.
///
/// The decode and the protocol encode happen here, on the worker thread, rather than on
/// the UI thread — a kitty protocol is base64 over the whole image, which is milliseconds
/// the event loop would otherwise spend not answering keys.
fn drawn(run: RunOutcome, picker: &Picker, cells: Size) -> Option<FrameOutcome> {
    let output = match run {
        RunOutcome::Abandoned => return None,
        RunOutcome::Failed(message) => return Some(FrameOutcome::Failed(message)),
        RunOutcome::Finished(output) if !output.status.success() => {
            return Some(FrameOutcome::Failed(command_failure(
                "Could not draw this frame",
                &output.stderr,
            )));
        }
        RunOutcome::Finished(output) => output,
    };

    let image = match image::load_from_memory_with_format(&output.stdout, image::ImageFormat::Png) {
        Ok(image) => image,
        Err(error) => return Some(FrameOutcome::Failed(format!("Unreadable frame: {error}"))),
    };
    match picker.new_protocol(image, cells, Resize::Fit(None)) {
        Ok(protocol) => Some(FrameOutcome::Ready(Box::new(protocol))),
        Err(error) => Some(FrameOutcome::Failed(format!(
            "Could not draw this frame: {error}"
        ))),
    }
}

/// The selected cue, alone, retimed to cover the whole grab.
///
/// `-ss` before `-i` resets output timestamps to about zero, so a cue starting at zero and
/// running far past the single frame that emerges is burned in whatever the frame rounding
/// or the container's `start_time` turns out to be. Handing libass the original file and
/// original timings instead makes the burn a coin toss on exactly the boundary frames a
/// timing page exists to inspect.
fn one_cue_srt(text: &str) -> String {
    format!(
        "1\n{} --> {}\n{}\n\n",
        format_srt_timestamp(Duration::ZERO),
        format_srt_timestamp(Duration::from_secs(600)),
        text.trim_end()
    )
}

/// Grabs one frame with the staged cue burned into it, scaled to the preview pane.
///
/// Runs with the workspace as its working directory so `subtitles=cue.srt` is a constant:
/// see [`CUE_FILE`]. The filters are ordered `subtitles` then `scale` so libass lays the
/// text out against the source resolution — its `PlayRes` — before anything shrinks it.
fn frame_command(media: &Path, seek: Duration, pixels: (u32, u32), workspace: &Path) -> Command {
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
            "subtitles={CUE_FILE},scale={width}:{height}:force_original_aspect_ratio=decrease"
        ))
        .args(["-f", "image2pipe", "-vcodec", "png", "-"]);
    command
}

/// Copies one subtitle stream out of a container, unchanged.
///
/// `-c:s copy` because the page shows the track as it is actually stored; anything that
/// re-encoded it would be previewing a file the user does not have.
fn extract_command(media: &Path, index: u64, output: &Path) -> Command {
    let mut command = Command::new("ffmpeg");
    command
        .args(["-v", "error", "-nostdin", "-y", "-i"])
        .arg(media)
        .args(["-map", &format!("0:{index}"), "-c:s", "copy"])
        .arg(output);
    command
}

fn read_cues(path: &Path) -> PrepareOutcome {
    match read_srt(path) {
        Ok(cues) => PrepareOutcome::Ready(cues),
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
fn run_cancellable(command: &mut Command, abandoned: &dyn Fn() -> bool) -> RunOutcome {
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

    fn frame_request(media: &Path, workspace: &Path, seek: Duration) -> FrameRequest {
        FrameRequest {
            generation: 1,
            cue_index: 0,
            media: media.to_path_buf(),
            workspace: workspace.to_path_buf(),
            text: "BURNED IN".to_string(),
            seek,
            cells: Size::new(20, 10),
        }
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
            workspace: directory.clone(),
        };

        // Act
        let outcome = prepare(&request, &live(7));

        // Assert
        let PrepareOutcome::Ready(cues) = outcome.expect("the page is still open") else {
            panic!("a readable sidecar should be ready");
        };
        assert_that!(cues.len()).is_equal_to(2);
        assert_that!(cues[1].text.as_str()).is_equal_to("second");
        assert_that!(directory.join(CUES_FILE).exists()).is_false();
        std::fs::remove_dir_all(directory).unwrap();
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
            workspace: workspace.clone(),
        };

        // Act
        let outcome = prepare(&request, &live(3));

        // Assert
        let PrepareOutcome::Ready(cues) = outcome.expect("the page is still open") else {
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
        // No picker means no frame worker, and asking for a frame anyway is a no-op
        // rather than a panic — the page falls back to the cue's text.
        assert_that!(handles.draws_frames()).is_false();
        handles.request_frame(FrameRequest {
            generation: 1,
            cue_index: 0,
            media: sidecar.clone(),
            workspace: directory.clone(),
            text: "unused".to_string(),
            seek: Duration::ZERO,
            cells: Size::new(10, 5),
        });
        let request = |generation| PrepareRequest {
            generation,
            input: sidecar.clone(),
            stream_index: None,
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
        // Act
        let staged = one_cue_srt("Hello\nthere\n");

        // Assert
        assert_that!(staged.as_str())
            .is_equal_to("1\n00:00:00,000 --> 00:10:00,000\nHello\nthere\n\n");
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
                "png",
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
        let outcome = frame(&request, &Picker::halfblocks(), &live(1));

        // Assert
        let Some(FrameOutcome::Ready(protocol)) = outcome else {
            panic!("a video with a burnable cue should produce a frame, got {outcome:?}");
        };
        assert_that!(protocol.size().width <= 20 && protocol.size().height <= 10).is_true();
        assert_that!(protocol.size().width > 0 && protocol.size().height > 0).is_true();
        // The cue is staged where the filter expects to find it: beside the command's
        // working directory, under the bare name it was given.
        let staged = std::fs::read_to_string(directory.join(CUE_FILE)).unwrap();
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
        let outcome = frame(&request, &Picker::halfblocks(), &live(1));

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
        let outcome = frame(&request, &Picker::halfblocks(), &live(1));

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
        let outcome = frame(&request, &Picker::halfblocks(), &live(1));

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
        let outcome = frame(&request, &Picker::halfblocks(), &live(9));

        // Assert: nothing was even staged, let alone spawned.
        assert_that!(outcome).is_none();
        assert_that!(directory.join(CUE_FILE).exists()).is_false();
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// The endings a real grab cannot be steered into on demand, plus the one that looks
    /// like success and is not: `ffmpeg` exiting zero having written nothing.
    #[test]
    fn a_grab_that_produced_no_image_should_not_be_reported_as_a_frame() {
        // Arrange
        let succeeded = Command::new("sh").args(["-c", "exit 0"]).output().unwrap();
        let refused = Command::new("sh")
            .args(["-c", "echo 'Invalid stream specifier' >&2; exit 1"])
            .output()
            .unwrap();
        let cells = Size::new(20, 10);

        // Act
        let abandoned = drawn(RunOutcome::Abandoned, &Picker::halfblocks(), cells);
        let unstartable = drawn(
            RunOutcome::Failed("ffmpeg was not found in PATH.".to_string()),
            &Picker::halfblocks(),
            cells,
        );
        let empty = drawn(
            RunOutcome::Finished(succeeded),
            &Picker::halfblocks(),
            cells,
        );
        let rejected = drawn(RunOutcome::Finished(refused), &Picker::halfblocks(), cells);

        // Assert
        assert_that!(abandoned).is_none();
        let Some(FrameOutcome::Failed(unstartable)) = unstartable else {
            panic!("a command that never started should fail");
        };
        assert_that!(unstartable.as_str()).contains("not found in PATH");
        let Some(FrameOutcome::Failed(empty)) = empty else {
            panic!("an empty grab should fail rather than draw nothing");
        };
        assert_that!(empty.as_str()).contains("Unreadable frame");
        let Some(FrameOutcome::Failed(rejected)) = rejected else {
            panic!("a refused grab should fail");
        };
        assert_that!(rejected.as_str()).contains("Invalid stream specifier");
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

        // Act: two requests, so the second is the one the coalescing loop settles on.
        handles.abandon(1);
        handles.request_frame(FrameRequest {
            cue_index: 1,
            ..frame_request(&media, &directory, Duration::from_millis(1500))
        });
        handles.request_frame(frame_request(
            &media,
            &directory,
            Duration::from_millis(2500),
        ));

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
        assert_that!(cue_index).is_equal_to(0);
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
        assert_that!(matches!(outcome, PrepareOutcome::Ready(ref cues) if cues.len() == 2))
            .is_true();
        assert_that!(events.try_recv().is_err()).is_true();

        // Act: dropping the handles closes the request channel and ends the thread.
        drop(handles);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
