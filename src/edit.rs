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
    pub stream_indices: BTreeSet<u64>,
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
            let result = delete_streams(
                &request.path,
                &request.stream_indices,
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

fn delete_streams(
    path: &Path,
    selected: &BTreeSet<u64>,
    cancelled: &AtomicBool,
    mut report_progress: impl FnMut(Option<f64>),
) -> Result<(), EditError> {
    let source_info = media_info(path).map_err(EditError::Failed)?;
    validate_deletion(&source_info, selected).map_err(EditError::Failed)?;
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
        selected,
        duration,
        cancelled,
        &mut report_progress,
    )?;
    if !output.status.success() {
        return Err(EditError::Failed(command_error(
            "ffmpeg could not remove the selected tracks",
            &output.stderr,
        )));
    }
    report_progress(duration.map(|_| 0.98));

    if cancelled.load(Ordering::Relaxed) {
        return Err(EditError::Cancelled);
    }
    let output_info = media_info(&temporary).map_err(EditError::Failed)?;
    let expected_count = source_info.streams.len().saturating_sub(selected.len());
    if output_info.streams.len() != expected_count {
        return Err(EditError::Failed(format!(
            "The remuxed file has {} tracks; expected {expected_count}.",
            output_info.streams.len()
        )));
    }
    validate_result(&output_info).map_err(EditError::Failed)?;

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

fn validate_result(info: &MediaInfo) -> Result<(), String> {
    let has_video = info
        .streams
        .iter()
        .any(|stream| stream_kind(stream) == Some("video") && !is_attached_picture(stream));
    if !has_video {
        return Err("The remuxed file has no playable video track.".to_string());
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
    selected: &BTreeSet<u64>,
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
        .arg(source)
        .args(["-map", "0"]);
    for index in selected {
        command.args(["-map", &format!("-0:{index}")]);
    }
    command
        .args(["-map_metadata", "0", "-map_chapters", "0", "-c", "copy"])
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
    use super::*;
    use std::process::Stdio;

    fn media(streams: Value) -> MediaInfo {
        MediaInfo::from_json(serde_json::json!({"streams": streams})).unwrap()
    }

    #[test]
    fn protects_last_video_and_last_audio() {
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video"},
            {"index": 1, "codec_type": "audio"},
            {"index": 2, "codec_type": "subtitle"}
        ]));

        assert_eq!(
            validate_deletion(&info, &BTreeSet::from([0])).unwrap_err(),
            "Can't delete the last remaining video track."
        );
        assert_eq!(
            validate_deletion(&info, &BTreeSet::from([1])).unwrap_err(),
            "Can't delete the last remaining audio track."
        );
        assert!(validate_deletion(&info, &BTreeSet::from([2])).is_ok());
    }

    #[test]
    fn permits_one_of_multiple_video_and_audio_tracks() {
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video"},
            {"index": 4, "codec_type": "video"},
            {"index": 7, "codec_type": "audio"},
            {"index": 9, "codec_type": "audio"}
        ]));

        assert!(validate_deletion(&info, &BTreeSet::from([0, 7])).is_ok());
        assert!(validate_deletion(&info, &BTreeSet::from([0, 4])).is_err());
        assert!(validate_deletion(&info, &BTreeSet::from([7, 9])).is_err());
    }

    #[test]
    fn ffmpeg_remux_removes_selected_tracks_and_replaces_source() {
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
            ])
            .arg(&source)
            .status()
            .unwrap();
        assert!(status.success());

        let mut progress = Vec::new();
        delete_streams(
            &source,
            &BTreeSet::from([1, 3]),
            &AtomicBool::new(false),
            |value| progress.push(value),
        )
        .unwrap();
        let info = media_info(&source).unwrap();
        let kinds: Vec<_> = info.streams.iter().filter_map(stream_kind).collect();
        assert_eq!(kinds, vec!["video", "audio"]);
        assert!(fs::read_dir(&directory).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".reel-tui-")
        }));
        assert_eq!(progress.last(), Some(&Some(1.0)));

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn reads_duration_from_string_or_number() {
        let mut info = media(serde_json::json!([{"index": 0, "codec_type": "video"}]));
        info.format
            .insert("duration".to_string(), Value::String("42.5".to_string()));
        assert_eq!(media_duration(&info), Some(42.5));
        info.format
            .insert("duration".to_string(), serde_json::json!(12.0));
        assert_eq!(media_duration(&info), Some(12.0));
    }
}
