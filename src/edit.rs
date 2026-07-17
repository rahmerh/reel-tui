use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::Value;

use crate::probe::{MediaInfo, ProbeOutcome, is_attached_picture, probe_file};

#[derive(Clone, Debug)]
pub struct EditRequest {
    pub path: PathBuf,
    pub stream_order: Vec<u64>,
    pub deleted_streams: BTreeSet<u64>,
    pub default_streams: BTreeSet<u64>,
    pub cancelled: Arc<AtomicBool>,
}

#[derive(Clone, Debug)]
pub enum EditEvent {
    Progress(Option<f64>),
    Finished { path: PathBuf, outcome: EditOutcome },
}

#[derive(Clone, Debug)]
pub enum EditOutcome {
    Completed,
    Cancelled,
    Failed(String),
}

pub fn spawn_edit_worker() -> (Sender<EditRequest>, Receiver<EditEvent>) {
    let (request_tx, request_rx) = mpsc::channel::<EditRequest>();
    let (result_tx, result_rx) = mpsc::channel();

    std::thread::spawn(move || {
        while let Ok(request) = request_rx.recv() {
            let progress_tx = result_tx.clone();
            let result = apply_edits(
                &request.path,
                &request.stream_order,
                &request.deleted_streams,
                &request.default_streams,
                &request.cancelled,
                |progress| {
                    let _ = progress_tx.send(EditEvent::Progress(progress));
                },
            );
            let outcome = match result {
                Ok(()) => EditOutcome::Completed,
                Err(EditError::Cancelled) => EditOutcome::Cancelled,
                Err(EditError::Failed(error)) => EditOutcome::Failed(error),
            };
            let response = EditEvent::Finished {
                path: request.path.clone(),
                outcome,
            };
            if result_tx.send(response).is_err() {
                break;
            }
        }
    });

    (request_tx, result_rx)
}

pub(crate) fn validate_deletion(info: &MediaInfo, selected: &BTreeSet<u64>) -> Result<(), String> {
    if selected.is_empty() {
        return Err("No tracks are selected for deletion.".to_string());
    }

    let available: BTreeSet<_> = info.streams.iter().filter_map(stream_index).collect();
    if !selected.is_subset(&available) {
        return Err("The file's tracks changed. Reopen it and select them again.".to_string());
    }

    let videos: Vec<_> = info
        .streams
        .iter()
        .filter(|stream| stream_kind(stream) == Some("video") && !is_attached_picture(stream))
        .filter_map(stream_index)
        .collect();
    let audio: Vec<_> = info
        .streams
        .iter()
        .filter(|stream| stream_kind(stream) == Some("audio"))
        .filter_map(stream_index)
        .collect();

    if videos.iter().all(|index| selected.contains(index)) {
        return Err(if videos.len() == 1 {
            "Can't delete the last remaining video track.".to_string()
        } else {
            "Can't delete every video track; at least one must remain.".to_string()
        });
    }
    if !audio.is_empty() && audio.iter().all(|index| selected.contains(index)) {
        return Err(if audio.len() == 1 {
            "Can't delete the last remaining audio track.".to_string()
        } else {
            "Can't delete every audio track; at least one must remain.".to_string()
        });
    }
    Ok(())
}

pub(crate) fn validate_edit(
    info: &MediaInfo,
    stream_order: &[u64],
    deleted_streams: &BTreeSet<u64>,
    default_streams: &BTreeSet<u64>,
) -> Result<(), String> {
    let available: BTreeSet<_> = info.streams.iter().filter_map(stream_index).collect();
    if available.len() != info.streams.len() {
        return Err("One or more tracks have no usable stream index.".to_string());
    }
    let ordered: BTreeSet<_> = stream_order.iter().copied().collect();
    if ordered.len() != stream_order.len()
        || !ordered.is_disjoint(deleted_streams)
        || ordered
            .union(deleted_streams)
            .copied()
            .collect::<BTreeSet<_>>()
            != available
    {
        return Err("The file's tracks changed. Reopen it and try again.".to_string());
    }
    if !default_streams.is_subset(&ordered) {
        return Err("A default track is also marked for deletion.".to_string());
    }
    if !deleted_streams.is_empty() {
        validate_deletion(info, deleted_streams)?;
    }
    Ok(())
}

fn apply_edits(
    path: &Path,
    stream_order: &[u64],
    deleted_streams: &BTreeSet<u64>,
    default_streams: &BTreeSet<u64>,
    cancelled: &AtomicBool,
    mut report_progress: impl FnMut(Option<f64>),
) -> Result<(), EditError> {
    let source_info = media_info(path).map_err(EditError::Failed)?;
    validate_edit(&source_info, stream_order, deleted_streams, default_streams)
        .map_err(EditError::Failed)?;
    let duration = media_duration(&source_info);
    report_progress(duration.map(|_| 0.0));

    if cancelled.load(Ordering::Relaxed) {
        return Err(EditError::Cancelled);
    }
    let temporary = temporary_path(path).map_err(EditError::Failed)?;
    let mut cleanup = TempCleanup(Some(temporary.clone()));
    let output = run_ffmpeg(
        path,
        &temporary,
        stream_order,
        default_streams,
        duration,
        cancelled,
        &mut report_progress,
    )?;
    if !output.status.success() {
        return Err(EditError::Failed(command_error(
            "ffmpeg could not apply the track edits",
            &output.stderr,
        )));
    }
    report_progress(duration.map(|_| 0.98));

    if cancelled.load(Ordering::Relaxed) {
        return Err(EditError::Cancelled);
    }
    let output_info = media_info(&temporary).map_err(EditError::Failed)?;
    let expected_count = stream_order.len();
    if output_info.streams.len() != expected_count {
        return Err(EditError::Failed(format!(
            "The remuxed file has {} tracks; expected {expected_count}.",
            output_info.streams.len()
        )));
    }
    validate_result(&source_info, &output_info, stream_order, default_streams)
        .map_err(EditError::Failed)?;

    let permissions = fs::metadata(path)
        .map_err(|error| EditError::Failed(format!("Could not read source permissions: {error}")))?
        .permissions();
    fs::set_permissions(&temporary, permissions).map_err(|error| {
        EditError::Failed(format!("Could not preserve source permissions: {error}"))
    })?;
    if cancelled.load(Ordering::Relaxed) {
        return Err(EditError::Cancelled);
    }
    fs::rename(&temporary, path).map_err(|error| {
        EditError::Failed(format!("Could not replace the original file: {error}"))
    })?;
    cleanup.0 = None;
    report_progress(Some(1.0));
    Ok(())
}

fn media_info(path: &Path) -> Result<MediaInfo, String> {
    match probe_file(path) {
        ProbeOutcome::Video(info) => Ok(info),
        ProbeOutcome::NotVideo(reason) | ProbeOutcome::Error(reason) => Err(reason),
    }
}

fn validate_result(
    source: &MediaInfo,
    output: &MediaInfo,
    stream_order: &[u64],
    default_streams: &BTreeSet<u64>,
) -> Result<(), String> {
    let has_video = output
        .streams
        .iter()
        .any(|stream| stream_kind(stream) == Some("video") && !is_attached_picture(stream));
    if !has_video {
        return Err("The remuxed file has no playable video track.".to_string());
    }
    let expected_kinds = stream_order
        .iter()
        .filter_map(|index| {
            source
                .streams
                .iter()
                .find(|stream| stream_index(stream) == Some(*index))
                .and_then(stream_kind)
        })
        .collect::<Vec<_>>();
    let output_kinds = output
        .streams
        .iter()
        .filter_map(stream_kind)
        .collect::<Vec<_>>();
    if output_kinds != expected_kinds {
        return Err("The remuxed tracks are not in the requested order.".to_string());
    }
    for (position, stream) in output.streams.iter().enumerate() {
        let expected = stream_order
            .get(position)
            .is_some_and(|index| default_streams.contains(index));
        if is_default(stream) != expected {
            return Err(format!(
                "The remuxed track at position {position} has the wrong default flag."
            ));
        }
    }
    Ok(())
}

struct FfmpegOutput {
    status: std::process::ExitStatus,
    stderr: Vec<u8>,
}

#[derive(Debug)]
enum EditError {
    Cancelled,
    Failed(String),
}

fn run_ffmpeg(
    source: &Path,
    temporary: &Path,
    stream_order: &[u64],
    default_streams: &BTreeSet<u64>,
    duration: Option<f64>,
    cancelled: &AtomicBool,
    report_progress: &mut impl FnMut(Option<f64>),
) -> Result<FfmpegOutput, EditError> {
    let mut command = Command::new("ffmpeg");
    command
        .args([
            "-v",
            "error",
            "-nostdin",
            "-y",
            "-progress",
            "pipe:1",
            "-nostats",
            "-i",
        ])
        .arg(source);
    for index in stream_order {
        command.args(["-map", &format!("0:{index}")]);
    }
    command.args(["-map_metadata", "0", "-map_chapters", "0", "-c", "copy"]);
    for (output_index, source_index) in stream_order.iter().enumerate() {
        command.arg(format!("-disposition:{output_index}")).arg(
            if default_streams.contains(source_index) {
                "+default"
            } else {
                "-default"
            },
        );
    }
    command
        .arg(temporary)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| {
        EditError::Failed(if error.kind() == std::io::ErrorKind::NotFound {
            "ffmpeg was not found in PATH. Install FFmpeg to edit media.".to_string()
        } else {
            format!("Could not start ffmpeg: {error}")
        })
    })?;

    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| EditError::Failed("Could not capture ffmpeg errors.".to_string()))?;
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = BufReader::new(stderr).read_to_end(&mut bytes);
        bytes
    });
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| EditError::Failed("Could not capture ffmpeg progress.".to_string()))?;
    let mut was_cancelled = false;
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        if cancelled.load(Ordering::Relaxed) {
            let _ = child.kill();
            was_cancelled = true;
            break;
        }
        if let Some(microseconds) = line
            .strip_prefix("out_time_us=")
            .and_then(|value| value.parse::<f64>().ok())
        {
            report_progress(
                duration.map(|total| (microseconds / 1_000_000.0 / total).clamp(0.0, 0.97)),
            );
        }
    }
    let status = child
        .wait()
        .map_err(|error| EditError::Failed(format!("Could not wait for ffmpeg: {error}")))?;
    let stderr = stderr_reader.join().unwrap_or_default();
    if was_cancelled || cancelled.load(Ordering::Relaxed) {
        return Err(EditError::Cancelled);
    }
    Ok(FfmpegOutput { status, stderr })
}

fn media_duration(info: &MediaInfo) -> Option<f64> {
    info.format
        .get("duration")
        .and_then(|value| match value {
            Value::String(value) => value.parse().ok(),
            Value::Number(value) => value.as_f64(),
            _ => None,
        })
        .filter(|duration| duration.is_finite() && *duration > 0.0)
}

fn temporary_path(path: &Path) -> Result<PathBuf, String> {
    let parent = path
        .parent()
        .ok_or_else(|| "The source file has no parent directory.".to_string())?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "The source filename is not valid UTF-8.".to_string())?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Ok(parent.join(format!(".reel-tui-{nonce}-{name}")))
}

fn command_error(heading: &str, stderr: &[u8]) -> String {
    let detail = String::from_utf8_lossy(stderr)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if detail.is_empty() {
        heading.to_string()
    } else {
        let truncated = detail.chars().count() > 360;
        let mut detail: String = detail.chars().take(360).collect();
        if truncated {
            detail.push('…');
        }
        format!("{heading}: {detail}")
    }
}

pub(crate) fn stream_index(stream: &BTreeMap<String, Value>) -> Option<u64> {
    stream.get("index").and_then(|value| match value {
        Value::Number(number) => number.as_u64(),
        Value::String(number) => number.parse().ok(),
        _ => None,
    })
}

fn stream_kind(stream: &BTreeMap<String, Value>) -> Option<&str> {
    stream.get("codec_type").and_then(Value::as_str)
}

fn is_default(stream: &BTreeMap<String, Value>) -> bool {
    stream
        .get("disposition")
        .and_then(Value::as_object)
        .and_then(|disposition| disposition.get("default"))
        .and_then(Value::as_i64)
        == Some(1)
}

struct TempCleanup(Option<PathBuf>);

impl Drop for TempCleanup {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use kernal::prelude::*;

    use super::*;
    use std::process::Stdio;

    fn media(streams: Value) -> MediaInfo {
        MediaInfo::from_json(serde_json::json!({"streams": streams})).unwrap()
    }

    #[test]
    fn validate_deletion_should_return_error_when_last_video_is_selected() {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video"},
            {"index": 1, "codec_type": "audio"},
            {"index": 2, "codec_type": "subtitle"}
        ]));
        let selected = BTreeSet::from([0]);

        // Act
        let result = validate_deletion(&info, &selected);

        // Assert
        assert_that!(result)
            .contains_error("Can't delete the last remaining video track.".to_string());
    }

    #[test]
    fn validate_deletion_should_return_error_when_last_audio_is_selected() {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video"},
            {"index": 1, "codec_type": "audio"},
            {"index": 2, "codec_type": "subtitle"}
        ]));
        let selected = BTreeSet::from([1]);

        // Act
        let result = validate_deletion(&info, &selected);

        // Assert
        assert_that!(result)
            .contains_error("Can't delete the last remaining audio track.".to_string());
    }

    #[test]
    fn validate_deletion_should_succeed_when_subtitle_is_selected() {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video"},
            {"index": 1, "codec_type": "audio"},
            {"index": 2, "codec_type": "subtitle"}
        ]));
        let selected = BTreeSet::from([2]);

        // Act
        let result = validate_deletion(&info, &selected);

        // Assert
        assert_that!(result).is_ok();
    }

    #[test]
    fn validate_deletion_should_succeed_when_one_of_multiple_video_and_audio_tracks_is_selected() {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video"},
            {"index": 4, "codec_type": "video"},
            {"index": 7, "codec_type": "audio"},
            {"index": 9, "codec_type": "audio"}
        ]));
        let selected = BTreeSet::from([0, 7]);

        // Act
        let result = validate_deletion(&info, &selected);

        // Assert
        assert_that!(result).is_ok();
    }

    #[test]
    fn validate_deletion_should_return_error_when_every_video_track_is_selected() {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video"},
            {"index": 4, "codec_type": "video"},
            {"index": 7, "codec_type": "audio"},
            {"index": 9, "codec_type": "audio"}
        ]));
        let selected = BTreeSet::from([0, 4]);

        // Act
        let result = validate_deletion(&info, &selected);

        // Assert
        assert_that!(result).is_err();
    }

    #[test]
    fn validate_deletion_should_return_error_when_every_audio_track_is_selected() {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video"},
            {"index": 4, "codec_type": "video"},
            {"index": 7, "codec_type": "audio"},
            {"index": 9, "codec_type": "audio"}
        ]));
        let selected = BTreeSet::from([7, 9]);

        // Act
        let result = validate_deletion(&info, &selected);

        // Assert
        assert_that!(result).is_err();
    }

    #[test]
    fn validate_edit_should_return_error_when_request_omits_unmarked_stream() {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video"},
            {"index": 1, "codec_type": "audio"},
            {"index": 2, "codec_type": "subtitle"}
        ]));

        // Act
        let result = validate_edit(&info, &[0, 1], &BTreeSet::new(), &BTreeSet::new());

        // Assert
        assert_that!(result).is_err();
    }

    #[test]
    fn validate_edit_should_return_error_when_default_stream_is_marked_for_deletion() {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video"},
            {"index": 1, "codec_type": "audio"},
            {"index": 2, "codec_type": "subtitle"}
        ]));

        // Act
        let result = validate_edit(&info, &[0, 1], &BTreeSet::from([2]), &BTreeSet::from([2]));

        // Assert
        assert_that!(result).is_err();
    }

    #[test]
    fn validate_edit_should_succeed_when_request_contains_all_streams_and_valid_default() {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video"},
            {"index": 1, "codec_type": "audio"},
            {"index": 2, "codec_type": "subtitle"}
        ]));

        // Act
        let result = validate_edit(&info, &[0, 1, 2], &BTreeSet::new(), &BTreeSet::from([1]));

        // Assert
        assert_that!(result).is_ok();
    }

    #[test]
    fn apply_edits_should_remux_order_defaults_and_deletions_when_source_contains_multiple_tracks()
    {
        // Arrange
        if Command::new("ffmpeg")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_err()
        {
            return;
        }

        let directory = std::env::temp_dir().join(format!(
            "reel-tui-edit-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("tracks.mkv");
        let status = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=16x16:r=1:d=1",
                "-f",
                "lavfi",
                "-i",
                "color=c=white:s=16x16:r=1:d=1",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=8000:cl=mono:d=1",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=8000:cl=mono:d=1",
                "-map",
                "0:v:0",
                "-map",
                "1:v:0",
                "-map",
                "2:a:0",
                "-map",
                "3:a:0",
                "-c:v",
                "ffv1",
                "-c:a",
                "pcm_s16le",
                "-metadata:s:v:0",
                "title=Black",
                "-metadata:s:v:1",
                "title=White",
                "-metadata:s:a:0",
                "title=Main",
                "-metadata:s:a:1",
                "title=Commentary",
                "-disposition:v:0",
                "default",
                "-disposition:v:1",
                "0",
                "-disposition:a:0",
                "default+original",
                "-disposition:a:1",
                "comment",
            ])
            .arg(&source)
            .status()
            .unwrap();
        status
            .success()
            .then_some(())
            .expect("ffmpeg should create the test fixture");

        // Act
        let mut progress = Vec::new();
        apply_edits(
            &source,
            &[1, 0, 3],
            &BTreeSet::from([2]),
            &BTreeSet::from([1, 3]),
            &AtomicBool::new(false),
            |value| progress.push(value),
        )
        .unwrap();
        let info = media_info(&source).unwrap();
        let kinds: Vec<_> = info.streams.iter().filter_map(stream_kind).collect();
        let titles = info
            .streams
            .iter()
            .map(|stream| {
                stream
                    .get("tags")
                    .and_then(Value::as_object)
                    .and_then(|tags| tags.get("title"))
                    .and_then(Value::as_str)
            })
            .collect::<Vec<_>>();
        let comment_disposition = info.streams[2]
            .get("disposition")
            .and_then(Value::as_object)
            .and_then(|flags| flags.get("comment"))
            .and_then(Value::as_i64);
        let temporary_files_removed = fs::read_dir(&directory).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".reel-tui-")
        });

        // Assert
        assert_that!(kinds).contains_exactly_in_given_order(["video", "video", "audio"]);
        assert_that!(titles).contains_exactly_in_given_order([
            Some("White"),
            Some("Black"),
            Some("Commentary"),
        ]);
        assert_that!(is_default(&info.streams[0])).is_true();
        assert_that!(is_default(&info.streams[1])).is_false();
        assert_that!(is_default(&info.streams[2])).is_true();
        assert_that!(comment_disposition).contains(1);
        assert_that!(temporary_files_removed).is_true();
        assert_that!(progress.last().copied()).contains(Some(1.0));

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn media_duration_should_return_seconds_when_duration_is_string() {
        // Arrange
        let mut info = media(serde_json::json!([{"index": 0, "codec_type": "video"}]));
        info.format
            .insert("duration".to_string(), Value::String("42.5".to_string()));

        // Act
        let result = media_duration(&info);

        // Assert
        assert_that!(result).contains(42.5);
    }

    #[test]
    fn media_duration_should_return_seconds_when_duration_is_number() {
        // Arrange
        let mut info = media(serde_json::json!([{"index": 0, "codec_type": "video"}]));
        info.format
            .insert("duration".to_string(), serde_json::json!(12.0));

        // Act
        let result = media_duration(&info);

        // Assert
        assert_that!(result).contains(12.0);
    }
}
