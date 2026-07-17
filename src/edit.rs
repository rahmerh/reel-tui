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

use crate::{
    files::FileFingerprint,
    probe::{MediaInfo, ProbeOutcome, is_attached_picture, probe_file},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoCodec {
    Original,
    H264,
    Hevc,
    Av1,
}

impl VideoCodec {
    pub const OPTIONS: [Self; 4] = [Self::Original, Self::H264, Self::Hevc, Self::Av1];

    pub fn label(self) -> &'static str {
        match self {
            Self::Original => "Original",
            Self::H264 => "H.264",
            Self::Hevc => "HEVC / H.265",
            Self::Av1 => "AV1",
        }
    }

    fn codec_name(self) -> Option<&'static str> {
        match self {
            Self::Original => None,
            Self::H264 => Some("h264"),
            Self::Hevc => Some("hevc"),
            Self::Av1 => Some("av1"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoResolution {
    Original,
    P2160,
    P1440,
    P1080,
    P720,
    P480,
}

impl VideoResolution {
    pub const PRESETS: [Self; 5] = [
        Self::P2160,
        Self::P1440,
        Self::P1080,
        Self::P720,
        Self::P480,
    ];

    pub fn height(self) -> Option<u64> {
        match self {
            Self::Original => None,
            Self::P2160 => Some(2160),
            Self::P1440 => Some(1440),
            Self::P1080 => Some(1080),
            Self::P720 => Some(720),
            Self::P480 => Some(480),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Original => "Original",
            Self::P2160 => "2160p",
            Self::P1440 => "1440p",
            Self::P1080 => "1080p",
            Self::P720 => "720p",
            Self::P480 => "480p",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoSettings {
    pub codec: VideoCodec,
    pub resolution: VideoResolution,
}

impl Default for VideoSettings {
    fn default() -> Self {
        Self {
            codec: VideoCodec::Original,
            resolution: VideoResolution::Original,
        }
    }
}

#[derive(Clone, Debug)]
pub struct EditRequest {
    pub path: PathBuf,
    pub stream_order: Vec<u64>,
    pub deleted_streams: BTreeSet<u64>,
    pub default_streams: BTreeSet<u64>,
    pub video_settings: BTreeMap<u64, VideoSettings>,
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
    SourceChanged(String),
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
                &request.video_settings,
                &request.cancelled,
                |progress| {
                    let _ = progress_tx.send(EditEvent::Progress(progress));
                },
            );
            let outcome = match result {
                Ok(()) => EditOutcome::Completed,
                Err(EditError::Cancelled) => EditOutcome::Cancelled,
                Err(EditError::SourceChanged(error)) => EditOutcome::SourceChanged(error),
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
    video_settings: &BTreeMap<u64, VideoSettings>,
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
    if !video_settings.keys().all(|index| ordered.contains(index)) {
        return Err("Video settings refer to a missing or deleted track.".to_string());
    }
    for (index, settings) in video_settings {
        let Some(stream) = info
            .streams
            .iter()
            .find(|stream| stream_index(stream) == Some(*index))
        else {
            return Err("The file's tracks changed. Reopen it and try again.".to_string());
        };
        if stream_kind(stream) != Some("video") || is_attached_picture(stream) {
            return Err(
                "Encoding settings can only be applied to playable video tracks.".to_string(),
            );
        }
        let source_height = stream_dimension(stream, "height");
        if settings
            .resolution
            .height()
            .zip(source_height)
            .is_some_and(|(target, source)| target >= source)
        {
            return Err("The selected resolution must be lower than the original.".to_string());
        }
        if settings.resolution != VideoResolution::Original
            && settings.codec == VideoCodec::Original
            && source_codec(stream).is_none()
        {
            return Err(format!(
                "Can't resize the original {} codec; choose H.264, HEVC, or AV1.",
                stream
                    .get("codec_name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_uppercase()
            ));
        }
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
    video_settings: &BTreeMap<u64, VideoSettings>,
    cancelled: &AtomicBool,
    mut report_progress: impl FnMut(Option<f64>),
) -> Result<(), EditError> {
    let source_metadata = fs::metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            EditError::SourceChanged(
                "Source file was removed; the media edit was not saved.".to_string(),
            )
        } else {
            EditError::Failed(format!("Could not read source metadata: {error}"))
        }
    })?;
    let source_fingerprint = FileFingerprint {
        length: source_metadata.len(),
        modified: source_metadata.modified().ok(),
    };
    let source_permissions = source_metadata.permissions();
    let source_info = media_info(path).map_err(|error| {
        if FileFingerprint::for_path(path).is_err() {
            EditError::SourceChanged(
                "Source file was removed; the media edit was not saved.".to_string(),
            )
        } else {
            EditError::Failed(error)
        }
    })?;
    validate_edit(
        &source_info,
        stream_order,
        deleted_streams,
        default_streams,
        video_settings,
    )
    .map_err(EditError::Failed)?;
    let duration = media_duration(&source_info);
    report_progress(duration.map(|_| 0.0));

    if cancelled.load(Ordering::Relaxed) {
        return Err(EditError::Cancelled);
    }
    let temporary = temporary_path(path).map_err(EditError::Failed)?;
    let mut cleanup = TempCleanup(Some(temporary.clone()));
    let output = run_ffmpeg(
        FfmpegPlan {
            source: path,
            temporary: &temporary,
            source_info: &source_info,
            stream_order,
            default_streams,
            video_settings,
            duration,
            cancelled,
        },
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
    validate_result(
        &source_info,
        &output_info,
        stream_order,
        default_streams,
        video_settings,
    )
    .map_err(EditError::Failed)?;

    match source_matches_fingerprint(path, source_fingerprint) {
        Ok(true) => {}
        Ok(false) => {
            return Err(EditError::SourceChanged(
                "Source file changed; reloaded latest metadata without saving the media edit."
                    .to_string(),
            ));
        }
        Err(_) => {
            return Err(EditError::SourceChanged(
                "Source file was removed; the media edit was not saved.".to_string(),
            ));
        }
    }
    fs::set_permissions(&temporary, source_permissions).map_err(|error| {
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

fn source_matches_fingerprint(path: &Path, expected: FileFingerprint) -> std::io::Result<bool> {
    FileFingerprint::for_path(path).map(|current| current == expected)
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
    video_settings: &BTreeMap<u64, VideoSettings>,
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
        let Some(source_index) = stream_order.get(position) else {
            continue;
        };
        let Some(settings) = video_settings.get(source_index) else {
            continue;
        };
        let source_stream = source
            .streams
            .iter()
            .find(|candidate| stream_index(candidate) == Some(*source_index));
        if source_stream.is_some_and(|stream| !requires_transcode(stream, *settings)) {
            continue;
        }
        let expected_codec = settings
            .codec
            .codec_name()
            .or_else(|| source_stream.and_then(source_codec));
        if expected_codec != stream.get("codec_name").and_then(Value::as_str) {
            return Err(format!(
                "The encoded video track at position {position} has the wrong codec."
            ));
        }
        if let Some(expected_height) = settings.resolution.height()
            && stream_dimension(stream, "height") != Some(expected_height)
        {
            return Err(format!(
                "The encoded video track at position {position} has the wrong resolution."
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
    SourceChanged(String),
    Failed(String),
}

struct FfmpegPlan<'a> {
    source: &'a Path,
    temporary: &'a Path,
    source_info: &'a MediaInfo,
    stream_order: &'a [u64],
    default_streams: &'a BTreeSet<u64>,
    video_settings: &'a BTreeMap<u64, VideoSettings>,
    duration: Option<f64>,
    cancelled: &'a AtomicBool,
}

fn run_ffmpeg(
    plan: FfmpegPlan<'_>,
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
        .arg(plan.source);
    for index in plan.stream_order {
        command.args(["-map", &format!("0:{index}")]);
    }
    command.args(["-map_metadata", "0", "-map_chapters", "0", "-c", "copy"]);
    let mut video_output_index = 0;
    for source_index in plan.stream_order {
        let Some(stream) = plan
            .source_info
            .streams
            .iter()
            .find(|stream| stream_index(stream) == Some(*source_index))
        else {
            continue;
        };
        if stream_kind(stream) != Some("video") {
            continue;
        }
        if let Some(settings) = plan
            .video_settings
            .get(source_index)
            .filter(|settings| requires_transcode(stream, **settings))
        {
            let codec = settings
                .codec
                .codec_name()
                .or_else(|| source_codec(stream))
                .expect("video settings are validated before building the ffmpeg command");
            let (encoder, quality, preset) =
                encoder_settings(codec).expect("supported target codecs have encoder settings");
            command
                .arg(format!("-c:v:{video_output_index}"))
                .arg(encoder)
                .arg(format!("-crf:v:{video_output_index}"))
                .arg(quality)
                .arg(format!("-preset:v:{video_output_index}"))
                .arg(preset);
            if let Some(height) = settings.resolution.height() {
                command
                    .arg(format!("-filter:v:{video_output_index}"))
                    .arg(format!("scale=-2:{height}"));
            }
        }
        video_output_index += 1;
    }
    for (output_index, source_index) in plan.stream_order.iter().enumerate() {
        command.arg(format!("-disposition:{output_index}")).arg(
            if plan.default_streams.contains(source_index) {
                "+default"
            } else {
                "-default"
            },
        );
    }
    command
        .arg(plan.temporary)
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
        if plan.cancelled.load(Ordering::Relaxed) {
            let _ = child.kill();
            was_cancelled = true;
            break;
        }
        if let Some(microseconds) = line
            .strip_prefix("out_time_us=")
            .and_then(|value| value.parse::<f64>().ok())
        {
            report_progress(
                plan.duration
                    .map(|total| (microseconds / 1_000_000.0 / total).clamp(0.0, 0.97)),
            );
        }
    }
    let status = child
        .wait()
        .map_err(|error| EditError::Failed(format!("Could not wait for ffmpeg: {error}")))?;
    let stderr = stderr_reader.join().unwrap_or_default();
    if was_cancelled || plan.cancelled.load(Ordering::Relaxed) {
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

fn stream_dimension(stream: &BTreeMap<String, Value>, name: &str) -> Option<u64> {
    stream.get(name).and_then(|value| match value {
        Value::Number(number) => number.as_u64(),
        Value::String(number) => number.parse().ok(),
        _ => None,
    })
}

fn source_codec(stream: &BTreeMap<String, Value>) -> Option<&'static str> {
    match stream.get("codec_name").and_then(Value::as_str) {
        Some("h264") => Some("h264"),
        Some("hevc") => Some("hevc"),
        Some("av1") => Some("av1"),
        _ => None,
    }
}

fn requires_transcode(stream: &BTreeMap<String, Value>, settings: VideoSettings) -> bool {
    settings.resolution != VideoResolution::Original
        || settings
            .codec
            .codec_name()
            .is_some_and(|target| stream.get("codec_name").and_then(Value::as_str) != Some(target))
}

fn encoder_settings(codec: &str) -> Option<(&'static str, &'static str, &'static str)> {
    match codec {
        "h264" => Some(("libx264", "22", "medium")),
        "hevc" => Some(("libx265", "24", "medium")),
        "av1" => Some(("libsvtav1", "30", "8")),
        _ => None,
    }
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
        let result = validate_edit(
            &info,
            &[0, 1],
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeMap::new(),
        );

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
        let result = validate_edit(
            &info,
            &[0, 1],
            &BTreeSet::from([2]),
            &BTreeSet::from([2]),
            &BTreeMap::new(),
        );

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
        let result = validate_edit(
            &info,
            &[0, 1, 2],
            &BTreeSet::new(),
            &BTreeSet::from([1]),
            &BTreeMap::new(),
        );

        // Assert
        assert_that!(result).is_ok();
    }

    #[test]
    fn validate_edit_should_reject_resizing_an_unsupported_original_codec() {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "ffv1", "width": 640, "height": 512}
        ]));
        let settings = BTreeMap::from([(
            0,
            VideoSettings {
                codec: VideoCodec::Original,
                resolution: VideoResolution::P480,
            },
        )]);

        // Act
        let result = validate_edit(&info, &[0], &BTreeSet::new(), &BTreeSet::new(), &settings);

        // Assert
        assert_that!(result).contains_error(
            "Can't resize the original FFV1 codec; choose H.264, HEVC, or AV1.".to_string(),
        );
    }

    #[test]
    fn validate_edit_should_reject_a_resolution_that_would_upscale() {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264", "width": 640, "height": 360}
        ]));
        let settings = BTreeMap::from([(
            0,
            VideoSettings {
                codec: VideoCodec::Original,
                resolution: VideoResolution::P480,
            },
        )]);

        // Act
        let result = validate_edit(&info, &[0], &BTreeSet::new(), &BTreeSet::new(), &settings);

        // Assert
        assert_that!(result)
            .contains_error("The selected resolution must be lower than the original.".to_string());
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
            &BTreeMap::new(),
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
    fn apply_edits_should_transcode_and_downscale_only_the_selected_video_stream() {
        // Arrange
        if !Command::new("ffmpeg")
            .args(["-v", "error", "-h", "encoder=libx264"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
        {
            return;
        }
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-transcode-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("transcode.mkv");
        let status = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=640x512:r=1:d=1",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=8000:cl=mono:d=1",
                "-map",
                "0:v:0",
                "-map",
                "1:a:0",
                "-c:v",
                "ffv1",
                "-c:a",
                "pcm_s16le",
            ])
            .arg(&source)
            .status()
            .unwrap();
        assert_that!(status.success()).is_true();
        let settings = BTreeMap::from([(
            0,
            VideoSettings {
                codec: VideoCodec::H264,
                resolution: VideoResolution::P480,
            },
        )]);

        // Act
        apply_edits(
            &source,
            &[0, 1],
            &BTreeSet::new(),
            &BTreeSet::new(),
            &settings,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        let info = media_info(&source).unwrap();

        // Assert
        assert_that!(info.streams[0]["codec_name"].as_str()).contains("h264");
        assert_that!(info.streams[0]["height"].as_u64()).contains(480);
        assert_that!(info.streams[0]["width"].as_u64()).contains(600);
        assert_that!(info.streams[1]["codec_name"].as_str()).contains("pcm_s16le");

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn source_fingerprint_guard_should_reject_changed_and_removed_source() {
        // Arrange
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-source-guard-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("video.mkv");
        fs::write(&source, b"original").unwrap();
        let fingerprint = FileFingerprint::for_path(&source).unwrap();

        // Act / Assert: changed
        fs::write(&source, b"externally changed contents").unwrap();
        assert_that!(source_matches_fingerprint(&source, fingerprint).unwrap()).is_false();

        // Act / Assert: removed
        fs::remove_file(&source).unwrap();
        assert_that!(source_matches_fingerprint(&source, fingerprint)).is_err();

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
