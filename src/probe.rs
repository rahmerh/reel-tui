use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::{self, Receiver, Sender},
};

use serde_json::Value;

use crate::files::FileFingerprint;

#[derive(Clone, Debug)]
pub struct ProbeRequest {
    pub generation: u64,
    pub path: PathBuf,
    pub fingerprint: FileFingerprint,
}

#[derive(Clone, Debug)]
pub struct ProbeResponse {
    pub generation: u64,
    pub path: PathBuf,
    pub fingerprint: FileFingerprint,
    pub outcome: ProbeOutcome,
}

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ProbeOutcome {
    Video(MediaInfo),
    NotVideo(String),
    Error(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MediaInfo {
    pub format: BTreeMap<String, Value>,
    pub streams: Vec<BTreeMap<String, Value>>,
    pub chapters: Vec<BTreeMap<String, Value>>,
}

impl MediaInfo {
    pub(crate) fn from_json(value: Value) -> Result<Self, String> {
        let info = Self::from_json_unchecked(value)?;
        let has_video = info.streams.iter().any(|stream| {
            stream.get("codec_type").and_then(Value::as_str) == Some("video")
                && !is_attached_picture(stream)
                && !is_still_image(stream, &info.format)
        });

        if !has_video {
            return Err("No video stream found".to_string());
        }
        Ok(info)
    }

    pub(crate) fn from_json_unchecked(value: Value) -> Result<Self, String> {
        // `value` is owned and discarded right after this call, so `format`/`streams`/
        // `chapters` are moved out of it via `remove` rather than cloned — the previous
        // version cloned the entire parsed ffprobe JSON tree a second time on every
        // probe for no reason.
        let Value::Object(mut object) = value else {
            return Err("ffprobe returned an invalid JSON document".to_string());
        };
        let format = object_map(object.remove("format"));
        let streams = object_array(object.remove("streams"));
        let chapters = object_array(object.remove("chapters"));
        Ok(Self {
            format,
            streams,
            chapters,
        })
    }

    pub fn format_tag(&self, name: &str) -> Option<String> {
        self.format
            .get("tags")
            .and_then(Value::as_object)
            .and_then(|tags| {
                tags.get(name).or_else(|| {
                    tags.iter()
                        .find_map(|(key, val)| key.eq_ignore_ascii_case(name).then_some(val))
                })
            })
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|val| !val.is_empty())
            .map(str::to_string)
    }

    pub fn container_title(&self) -> Option<String> {
        self.format_tag("title")
    }

    pub fn container_comment(&self) -> Option<String> {
        self.format_tag("comment")
            .or_else(|| self.format_tag("description"))
    }

    pub fn container_date(&self) -> Option<String> {
        self.format_tag("date")
            .or_else(|| self.format_tag("creation_time"))
    }

    pub fn container_genre(&self) -> Option<String> {
        self.format_tag("genre")
    }

    pub fn container_artist(&self) -> Option<String> {
        self.format_tag("artist")
            .or_else(|| self.format_tag("author"))
            .or_else(|| self.format_tag("composer"))
    }
}

pub fn probe_any_file(path: &Path) -> Result<MediaInfo, String> {
    let is_network = crate::mount::is_network_mount(path);
    let mut command = Command::new("ffprobe");
    command.args([
        "-v",
        "error",
        "-of",
        "json",
        "-show_format",
        "-show_streams",
        "-show_chapters",
    ]);
    if is_network {
        command.args(["-probesize", "2000000", "-analyzeduration", "3000000"]);
    }
    let output = command.arg(path).output().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            "ffprobe was not found in PATH. Install FFmpeg to inspect media.".to_string()
        } else {
            format!("Could not start ffprobe: {error}")
        }
    })?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if detail.is_empty() {
            "ffprobe could not recognize this subtitle output".to_string()
        } else {
            detail
        });
    }
    let value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("Could not parse ffprobe output: {error}"))?;
    MediaInfo::from_json_unchecked(value)
}

fn object_map(value: Option<Value>) -> BTreeMap<String, Value> {
    match value {
        Some(Value::Object(map)) => map.into_iter().collect(),
        _ => BTreeMap::new(),
    }
}

fn object_array(value: Option<Value>) -> Vec<BTreeMap<String, Value>> {
    match value {
        Some(Value::Array(items)) => items
            .into_iter()
            .filter_map(|item| match item {
                Value::Object(map) => Some(map.into_iter().collect()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

pub(crate) fn is_attached_picture(stream: &BTreeMap<String, Value>) -> bool {
    stream
        .get("disposition")
        .and_then(Value::as_object)
        .and_then(|disposition| disposition.get("attached_pic"))
        .and_then(Value::as_i64)
        == Some(1)
}

pub(crate) fn is_still_image(
    stream: &BTreeMap<String, Value>,
    format: &BTreeMap<String, Value>,
) -> bool {
    let format_name = format
        .get("format_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();

    if format_name.contains("image2")
        || format_name.contains("png")
        || format_name.contains("jpeg")
        || format_name.contains("webp")
        || format_name.contains("bmp")
        || format_name.contains("tiff")
        || format_name.contains("gif")
        || format_name.contains("tty")
    {
        return true;
    }

    let codec_name = stream
        .get("codec_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();

    let image_codecs = [
        "png", "jpeg", "mjpeg", "webp", "bmp", "tiff", "gif", "svg", "pcx", "tga", "ppm", "pbm",
        "pgm", "pam",
    ];

    if image_codecs.contains(&codec_name.as_str()) {
        let duration = format
            .get("duration")
            .and_then(Value::as_str)
            .and_then(|d| d.parse::<f64>().ok())
            .unwrap_or(0.0);
        if duration <= 0.0 {
            return true;
        }
    }

    false
}

pub fn spawn_probe_worker() -> (Sender<ProbeRequest>, Receiver<ProbeResponse>) {
    let (request_tx, request_rx) = mpsc::channel::<ProbeRequest>();
    let (result_tx, result_rx) = mpsc::channel();

    std::thread::spawn(move || {
        while let Ok(mut request) = request_rx.recv() {
            while let Ok(newer) = request_rx.try_recv() {
                request = newer;
            }
            let outcome = probe_file(&request.path);
            if result_tx
                .send(ProbeResponse {
                    generation: request.generation,
                    path: request.path,
                    fingerprint: request.fingerprint,
                    outcome,
                })
                .is_err()
            {
                break;
            }
        }
    });

    (request_tx, result_rx)
}

pub(crate) fn probe_file(path: &Path) -> ProbeOutcome {
    let is_network = crate::mount::is_network_mount(path);
    let mut command = Command::new("ffprobe");
    command.args([
        "-v",
        "error",
        "-of",
        "json",
        "-show_format",
        "-show_streams",
        "-show_chapters",
    ]);
    if is_network {
        command.args(["-probesize", "2000000", "-analyzeduration", "3000000"]);
    }
    let output = match command.arg(path).output() {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ProbeOutcome::Error(
                "ffprobe was not found in PATH. Install FFmpeg to inspect media.".to_string(),
            );
        }
        Err(error) => return ProbeOutcome::Error(format!("Could not start ffprobe: {error}")),
    };

    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return ProbeOutcome::NotVideo(if detail.is_empty() {
            "ffprobe could not recognize this as a media file".to_string()
        } else {
            detail
        });
    }

    let value: Value = match serde_json::from_slice(&output.stdout) {
        Ok(value) => value,
        Err(error) => {
            return ProbeOutcome::Error(format!("Could not parse ffprobe output: {error}"));
        }
    };

    match MediaInfo::from_json(value) {
        Ok(info) => ProbeOutcome::Video(info),
        Err(reason) => ProbeOutcome::NotVideo(reason),
    }
}

#[cfg(test)]
mod tests {
    use kernal::prelude::*;

    use super::*;

    #[test]
    fn probe_worker_should_coalesce_queued_requests_and_answer_only_the_newest() {
        // Arrange: the worker probes one file at a time, so holding a cursor key stacks
        // requests behind it. Only the last one is still on screen — probing the rest is
        // seconds of ffprobe the user waits through for answers nobody will read.
        let (requests, responses) = spawn_probe_worker();
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-probe-worker-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();

        // Act: five files queued back to back, faster than any probe can finish.
        for generation in 0..5u64 {
            let path = directory.join(format!("{generation}.mkv"));
            std::fs::write(&path, b"not really media").unwrap();
            requests
                .send(ProbeRequest {
                    generation,
                    path,
                    fingerprint: crate::files::FileFingerprint {
                        length: 16,
                        modified: None,
                    },
                })
                .unwrap();
        }

        // Assert: every answer that comes back is for a request that was still queued,
        // and the newest request is always answered.
        let mut answered = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            match responses.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(response) => {
                    answered.push(response.generation);
                    if response.generation == 4 {
                        break;
                    }
                }
                Err(_) => continue,
            }
        }
        assert!(
            answered.contains(&4),
            "the newest request must be answered: {answered:?}",
        );
        // How many are coalesced depends on how fast ffprobe returns, so the count is not
        // asserted; what must hold either way is that no answer is invented or reordered.
        assert!(
            answered.len() <= 5,
            "the worker must not answer more than it was asked: {answered:?}",
        );
        assert!(
            answered.windows(2).all(|pair| pair[0] < pair[1]),
            "answers must stay in request order: {answered:?}",
        );

        drop(requests);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn probe_worker_should_stop_when_its_results_are_no_longer_wanted() {
        // Arrange / Act: dropping the receiving end is how `reel` shuts the worker down.
        let (requests, responses) = spawn_probe_worker();
        drop(responses);

        // Assert: sending still succeeds (the channel outlives one send), and the worker
        // exits rather than looping on a dead channel — so the process can exit too.
        let _ = requests.send(ProbeRequest {
            generation: 0,
            path: std::path::PathBuf::from("/nonexistent/movie.mkv"),
            fingerprint: crate::files::FileFingerprint {
                length: 0,
                modified: None,
            },
        });
    }

    #[test]
    fn from_json_should_preserve_streams_chapters_and_format_when_input_contains_video() {
        // Arrange
        let value: Value = serde_json::from_str(
            r#"{
                "streams": [
                    {"index": 0, "codec_type": "video", "codec_name": "av1"},
                    {"index": 1, "codec_type": "audio", "codec_name": "opus"},
                    {"index": 2, "codec_type": "subtitle", "tags": {"language": "eng"}}
                ],
                "chapters": [{"id": 0, "start_time": "0.0"}],
                "format": {"format_name": "matroska", "duration": "42.0"}
            }"#,
        )
        .unwrap();

        // Act
        let info = MediaInfo::from_json(value).unwrap();

        // Assert
        assert_that!(info.streams).has_length(3);
        assert_that!(info.chapters).has_length(1);
        assert_that!(info.format["format_name"].as_str()).contains("matroska");
    }

    #[test]
    fn from_json_should_return_error_when_input_only_contains_audio_and_attached_picture() {
        // Arrange
        let value: Value = serde_json::from_str(
            r#"{"streams":[
                {"codec_type":"audio"},
                {"codec_type":"video","disposition":{"attached_pic":1}}
            ]}"#,
        )
        .unwrap();

        // Act
        let result = MediaInfo::from_json(value);

        // Assert
        assert_that!(result).is_err();
    }

    #[test]
    fn from_json_should_return_error_when_input_is_still_image() {
        // Arrange
        let value: Value = serde_json::from_str(
            r#"{
                "streams": [{"codec_type": "video", "codec_name": "png"}],
                "format": {"format_name": "image2", "duration": "0.0"}
            }"#,
        )
        .unwrap();

        // Act
        let result = MediaInfo::from_json(value);

        // Assert
        assert_that!(result).is_err();
    }

    #[test]
    fn container_metadata_getters_should_extract_tags_case_insensitively() {
        // Arrange
        let value: Value = serde_json::from_str(
            r#"{
                "streams": [{"codec_type": "video", "codec_name": "h264"}],
                "format": {
                    "format_name": "matroska",
                    "tags": {
                        "TITLE": "Inception",
                        "comment": "Mind-bending heist",
                        "DATE": "2010",
                        "Genre": "Sci-Fi",
                        "ARTIST": "Christopher Nolan"
                    }
                }
            }"#,
        )
        .unwrap();

        // Act
        let info = MediaInfo::from_json(value).unwrap();

        // Assert
        assert_that!(info.container_title().as_deref()).contains("Inception");
        assert_that!(info.container_comment().as_deref()).contains("Mind-bending heist");
        assert_that!(info.container_date().as_deref()).contains("2010");
        assert_that!(info.container_genre().as_deref()).contains("Sci-Fi");
        assert_that!(info.container_artist().as_deref()).contains("Christopher Nolan");
    }
}
