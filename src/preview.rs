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

use crate::cue::{Cue, read_srt};

/// Filename an embedded track is staged under inside the page's workspace.
pub const CUES_FILE: &str = "cues.srt";

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreviewEvent {
    pub generation: u64,
    pub outcome: PrepareOutcome,
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
    /// The page generation the worker should still be working for, shared with it.
    ///
    /// An `AtomicU64` rather than a cancellation flag because it answers both questions
    /// the worker has — "is this request still wanted" and "should I stop now" — with one
    /// comparison, and because the value is already unique per page opening.
    live_generation: Arc<AtomicU64>,
}

impl PreviewHandles {
    /// Asks for a track's cues, making this the generation the worker works for.
    pub fn request(&self, request: PrepareRequest) {
        self.live_generation
            .store(request.generation, Ordering::Relaxed);
        // A dead worker leaves the page on its loader, which is the same thing that
        // happens if the extraction never finishes; there is nothing better to do here.
        let _ = self.prepare_tx.send(request);
    }

    /// Tells the worker that the page it was working for is gone, so a still-running
    /// `ffmpeg` is killed rather than left demuxing a file nobody is looking at.
    pub fn abandon(&self, generation: u64) {
        self.live_generation.store(generation, Ordering::Relaxed);
    }
}

pub fn spawn_preview_worker() -> (PreviewHandles, Receiver<PreviewEvent>) {
    let (prepare_tx, prepare_rx) = mpsc::channel::<PrepareRequest>();
    let (event_tx, event_rx) = mpsc::channel();
    let live_generation = Arc::new(AtomicU64::new(0));
    let worker_generation = Arc::clone(&live_generation);

    std::thread::spawn(move || {
        // FIFO rather than coalescing like `spawn_probe_worker`: requests arrive one per
        // page opening, not one per cursor movement, so there is no burst to collapse —
        // and a superseded request is already cheap, since `prepare` drops it before
        // spawning anything.
        while let Ok(request) = prepare_rx.recv() {
            let Some(outcome) = prepare(&request, &worker_generation) else {
                continue;
            };
            if event_tx
                .send(PreviewEvent {
                    generation: request.generation,
                    outcome,
                })
                .is_err()
            {
                break;
            }
        }
    });

    (
        PreviewHandles {
            prepare_tx,
            live_generation,
        },
        event_rx,
    )
}

/// Handles with no worker thread behind them, so `App`'s dispatch can be asserted from
/// the request itself instead of through a real extraction.
#[cfg(test)]
pub(crate) fn test_handles() -> (PreviewHandles, Receiver<PrepareRequest>, Arc<AtomicU64>) {
    let (prepare_tx, prepare_rx) = mpsc::channel();
    let live_generation = Arc::new(AtomicU64::new(0));
    (
        PreviewHandles {
            prepare_tx,
            live_generation: Arc::clone(&live_generation),
        },
        prepare_rx,
        live_generation,
    )
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
        let (handles, events) = spawn_preview_worker();
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
        let (handles, events) = spawn_preview_worker();
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
        assert_that!(event.generation).is_equal_to(3);
        assert_that!(matches!(event.outcome, PrepareOutcome::Ready(ref cues) if cues.len() == 2))
            .is_true();
        assert_that!(events.try_recv().is_err()).is_true();

        // Act: dropping the handles closes the request channel and ends the thread.
        drop(handles);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
