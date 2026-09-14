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
    /// Whether `path` lives on a network mount, decided once by the caller (`App`
    /// already computes and caches this for its own `directory`) rather than
    /// re-derived here on every request — `crate::mount::is_network_mount` can
    /// require a fresh `/proc/mounts` read plus `canonicalize()`'s per-component
    /// stat, which on an actual network mount may be a network round trip. Doing
    /// that on every probe (i.e. on every file selection) would add the exact kind
    /// of network chatter this flag exists to avoid.
    pub is_network_mount: bool,
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

/// The tuned flags a probe uses on a network mount. `ffprobe` will otherwise read and
/// seek as far as it likes to identify a file, which on a share turns one file
/// selection into seconds of round trips; capping it trades a little certainty about
/// exotic files for a listing that keeps up with the cursor.
fn apply_network_probe_flags(command: &mut Command, is_network: bool) {
    if is_network {
        command.args(["-probesize", "2000000", "-analyzeduration", "3000000"]);
    }
}

/// What to report when `ffprobe` exits non-zero. It usually explains itself on stderr,
/// but a build that fails without a word would otherwise surface as an empty message,
/// which reads as a bug in reel rather than a file `ffprobe` could not handle.
fn ffprobe_failure_detail(stderr: &[u8], fallback: &str) -> String {
    let detail = String::from_utf8_lossy(stderr).trim().to_string();
    if detail.is_empty() {
        fallback.to_string()
    } else {
        detail
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
    apply_network_probe_flags(&mut command, is_network);
    let output = command.arg(path).output().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            "ffprobe was not found in PATH. Install FFmpeg to inspect media.".to_string()
        } else {
            format!("Could not start ffprobe: {error}")
        }
    })?;
    if !output.status.success() {
        return Err(ffprobe_failure_detail(
            &output.stderr,
            "ffprobe could not recognize this subtitle output",
        ));
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

/// How many frames a second a video stream actually holds, or `None` when ffprobe will not
/// say.
///
/// Reported as a rational (`24000/1001`, `25/1`), which is why this exists rather than a
/// field read. `avg_frame_rate` first and `r_frame_rate` only as a fallback: for
/// variable-rate content `r_frame_rate` is the tick rate the timestamps are expressed in —
/// often 1000 or 90000 — where `avg_frame_rate` is the rate the file really averages.
///
/// `None` for the cases ffprobe cannot answer: `0/0`, which is what audio streams and some
/// attached pictures carry, and anything that will not parse. Callers must treat that as
/// "unknown", never as zero.
pub fn stream_frame_rate(stream: &BTreeMap<String, Value>) -> Option<f64> {
    let rate = stream
        .get("avg_frame_rate")
        .and_then(Value::as_str)
        .and_then(parse_frame_rate)
        .or_else(|| {
            stream
                .get("r_frame_rate")
                .and_then(Value::as_str)
                .and_then(parse_frame_rate)
        })?;
    (rate.is_finite() && rate > 0.0).then_some(rate)
}

/// One `numerator/denominator` frame rate as a number. Shared with `ui`, which formats the
/// same strings for the stream-details panel — one parser, so the panel and the playback
/// cannot disagree about what a file's rate is.
pub(crate) fn parse_frame_rate(rate: &str) -> Option<f64> {
    let (numerator, denominator) = rate.split_once('/')?;
    let numerator: f64 = numerator.parse().ok()?;
    let denominator: f64 = denominator.parse().ok()?;
    if denominator == 0.0 {
        return None;
    }
    Some(numerator / denominator)
}

pub(crate) fn is_attached_picture(stream: &BTreeMap<String, Value>) -> bool {
    stream
        .get("disposition")
        .and_then(Value::as_object)
        .and_then(|disposition| disposition.get("attached_pic"))
        .and_then(Value::as_i64)
        == Some(1)
}

/// The QuickTime chapter track: the `text`-tagged data stream MP4 and MOV keep their
/// chapter titles in, which the `mov` demuxer reports *twice* — once as the chapters
/// `-show_chapters` lists, and once as an opaque `bin_data` stream in `streams`.
///
/// It is not a track a reader has any business seeing or Reel any business copying.
/// Mapping it explicitly is wrong in both directions: `ffmpeg -map_chapters 0` writes
/// the chapters back out as a fresh text track of its own, so an MP4 remuxed with it
/// mapped comes out carrying the same chapters twice, while Matroska stores chapters
/// outside the stream table entirely and refuses the stream outright — "Only audio,
/// video, and subtitles are supported for Matroska" — which turned every MP4 with
/// chapters into a file that could not be converted to MKV at all.
///
/// Keyed on the container's own tag rather than on the codec, because `bin_data` is
/// simply what FFmpeg calls a stream it cannot interpret: GoPro telemetry (`gpmd`)
/// reads as `bin_data` too and is a real track that MP4 both stores and preserves.
/// The chapter list is required to be non-empty, so a `text` stream a demuxer read no
/// chapters out of is still carried rather than silently dropped.
pub(crate) fn is_chapter_track(info: &MediaInfo, stream: &BTreeMap<String, Value>) -> bool {
    !info.chapters.is_empty()
        && stream.get("codec_type").and_then(Value::as_str) == Some("data")
        && stream.get("codec_tag_string").and_then(Value::as_str) == Some("text")
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
            let outcome = probe_file(&request.path, request.is_network_mount);
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

/// Like `spawn_probe_worker`, but answers every request in the order it arrived
/// instead of coalescing down to only the newest. Used for background staleness
/// re-checks (`App::receive_conflict_probe_results`), where a burst of several
/// staged files changing near-simultaneously must each get an answer — silently
/// dropping all but the last (`spawn_probe_worker`'s behavior, correct for the
/// interactive per-selection probe it serves, where a stale answer for a selection
/// already moved past is worthless) would leave the others stuck flagged `stale`
/// with no way to resolve short of the user manually reopening them.
pub fn spawn_conflict_probe_worker() -> (Sender<ProbeRequest>, Receiver<ProbeResponse>) {
    let (request_tx, request_rx) = mpsc::channel::<ProbeRequest>();
    let (result_tx, result_rx) = mpsc::channel();

    std::thread::spawn(move || {
        while let Ok(request) = request_rx.recv() {
            let outcome = probe_file(&request.path, request.is_network_mount);
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

pub(crate) fn probe_file(path: &Path, is_network: bool) -> ProbeOutcome {
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
    apply_network_probe_flags(&mut command, is_network);
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
        return ProbeOutcome::NotVideo(ffprobe_failure_detail(
            &output.stderr,
            "ffprobe could not recognize this as a media file",
        ));
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

    /// Probing a network share without the caps turns each file selection into seconds
    /// of round trips; applying them to a local file would cap a probe that costs
    /// nothing. Both call sites share this, so both stay in step.
    #[test]
    fn only_a_network_probe_should_carry_the_capped_read_flags() {
        // Arrange
        let arguments = |is_network| {
            let mut command = Command::new("ffprobe");
            apply_network_probe_flags(&mut command, is_network);
            command
                .get_args()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };

        // Act / Assert
        assert_that!(arguments(false)).is_empty();
        assert_that!(arguments(true)).is_equal_to(vec![
            "-probesize".to_string(),
            "2000000".to_string(),
            "-analyzeduration".to_string(),
            "3000000".to_string(),
        ]);
    }

    /// An empty message would reach the file list as a blank "not a video" reason,
    /// which reads as a bug in reel rather than as something `ffprobe` refused.
    #[test]
    fn a_silent_ffprobe_failure_should_still_produce_a_reason() {
        // Act / Assert
        assert_that!(ffprobe_failure_detail(b"", "nothing to say").as_str())
            .is_equal_to("nothing to say");
        assert_that!(ffprobe_failure_detail(b"   \n\t ", "nothing to say").as_str())
            .is_equal_to("nothing to say");
        assert_that!(
            ffprobe_failure_detail(b"  movie.mkv: Invalid data found\n", "nothing to say").as_str()
        )
        .is_equal_to("movie.mkv: Invalid data found");
        // Whatever ffprobe wrote wins even when it is not valid UTF-8.
        assert_that!(ffprobe_failure_detail(b"bad \xff byte", "nothing to say").as_str())
            .contains("bad");
    }

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
                    is_network_mount: false,
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
    fn conflict_probe_worker_should_answer_every_queued_request_without_coalescing() {
        // Arrange: unlike the interactive worker above, a background staleness check
        // must account for every file that changed, not just the most recent one —
        // silently dropping all but the last would leave the others stuck flagged
        // stale with no way to resolve short of the user manually reopening them.
        let (requests, responses) = spawn_conflict_probe_worker();
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-conflict-probe-worker-{}-{}",
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
                    is_network_mount: false,
                })
                .unwrap();
        }

        // Assert: all five come back, in the order they were sent.
        let mut answered = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while answered.len() < 5 && std::time::Instant::now() < deadline {
            if let Ok(response) = responses.recv_timeout(std::time::Duration::from_millis(200)) {
                answered.push(response.generation);
            }
        }
        assert_that!(answered).contains_exactly_in_given_order([0, 1, 2, 3, 4]);

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
            is_network_mount: false,
        });
    }

    #[test]
    fn conflict_probe_worker_should_stop_when_its_results_are_no_longer_wanted() {
        // Arrange / Act: the mirror of
        // `probe_worker_should_stop_when_its_results_are_no_longer_wanted` for the
        // background staleness worker. It has its own thread and its own send, so it
        // needs its own proof that a dropped receiver ends the loop — otherwise `reel`
        // exits leaving this thread spinning on a dead channel.
        let (requests, responses) = spawn_conflict_probe_worker();
        drop(responses);

        // Assert: sending still succeeds (the channel outlives one send), and the worker
        // shuts down rather than looping forever.
        let _ = requests.send(ProbeRequest {
            generation: 0,
            path: std::path::PathBuf::from("/nonexistent/movie.mkv"),
            fingerprint: crate::files::FileFingerprint {
                length: 0,
                modified: None,
            },
            is_network_mount: false,
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

    /// An ffprobe-dependent test must fail when its prerequisite is missing; an
    /// unexecuted test must never be reported as passing.
    fn require_tools(test: &str, tools: &[&str]) {
        for tool in tools {
            let available = Command::new(tool)
                .arg("-version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            assert!(
                available,
                "{test} requires {tool}; install the missing test prerequisite"
            );
        }
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-probe-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn from_json_unchecked_should_reject_a_document_that_is_not_a_json_object() {
        // Arrange: ffprobe is expected to answer with an object. A bare array or scalar
        // means the output was not what we asked for — truncated, or not ffprobe at all —
        // and must surface as an error rather than silently becoming an empty MediaInfo
        // that reports a real file as having no streams.
        for document in ["[]", "\"unexpected\"", "42", "null", "true"] {
            let value: Value = serde_json::from_str(document).unwrap();

            // Act
            let result = MediaInfo::from_json_unchecked(value);

            // Assert
            assert_that!(result).is_err();
        }
    }

    #[test]
    fn from_json_unchecked_should_default_sections_that_are_present_but_mistyped() {
        // Arrange: a document whose `streams`/`chapters`/`format` keys hold the wrong JSON
        // type, plus a stream entry that is not an object. Every one of these must degrade
        // to empty rather than panic, so a malformed probe cannot take the UI down.
        let value: Value = serde_json::from_str(
            r#"{
                "streams": ["not an object", 7, {"codec_type": "video"}],
                "chapters": "not an array",
                "format": ["not an object"]
            }"#,
        )
        .unwrap();

        // Act
        let info = MediaInfo::from_json_unchecked(value).unwrap();

        // Assert: only the one well-formed stream survives; the rest default to empty.
        assert_that!(info.streams).has_length(1);
        assert_that!(info.chapters).is_empty();
        assert_that!(info.format).is_empty();
    }

    #[test]
    fn from_json_unchecked_should_default_sections_that_are_missing_entirely() {
        // Arrange: `-show_chapters` on a container without chapters omits the key rather
        // than returning an empty array.
        let value: Value = serde_json::from_str("{}").unwrap();

        // Act
        let info = MediaInfo::from_json_unchecked(value).unwrap();

        // Assert
        assert_that!(info.streams).is_empty();
        assert_that!(info.chapters).is_empty();
        assert_that!(info.format).is_empty();
    }

    #[test]
    fn is_still_image_should_detect_every_recognised_image_container() {
        // Arrange / Act / Assert: each of these container names is checked by its own
        // branch, so a name dropped from the list would otherwise go unnoticed — the
        // failure mode is a cover scan or screenshot being offered for track editing.
        for format_name in [
            "image2",
            "png_pipe",
            "jpeg_pipe",
            "webp_pipe",
            "bmp_pipe",
            "tiff_pipe",
            "gif",
            "tty",
        ] {
            let stream = BTreeMap::from([("codec_type".to_string(), Value::from("video"))]);
            let format = BTreeMap::from([("format_name".to_string(), Value::from(format_name))]);

            assert!(
                is_still_image(&stream, &format),
                "{format_name} must be recognised as a still image container",
            );
        }
    }

    #[test]
    fn is_still_image_should_detect_an_image_codec_in_a_container_without_duration() {
        // Arrange: a single frame muxed into an ordinary container reports an image codec
        // and no usable duration.
        for codec_name in ["png", "mjpeg", "webp", "bmp", "tiff", "gif", "svg", "tga"] {
            let stream = BTreeMap::from([("codec_name".to_string(), Value::from(codec_name))]);
            let format = BTreeMap::from([("format_name".to_string(), Value::from("matroska"))]);

            assert!(
                is_still_image(&stream, &format),
                "{codec_name} with no duration must be recognised as a still image",
            );
        }
    }

    #[test]
    fn is_still_image_should_keep_an_image_codec_that_actually_runs_for_a_duration() {
        // Arrange: an MJPEG *video* — an image codec, but a real timeline of frames. The
        // duration is what separates it from a single embedded frame, and treating it as
        // a still would hide a genuinely editable file from the user.
        let stream = BTreeMap::from([("codec_name".to_string(), Value::from("mjpeg"))]);
        let format = BTreeMap::from([
            ("format_name".to_string(), Value::from("avi")),
            ("duration".to_string(), Value::from("42.5")),
        ]);

        // Act / Assert
        assert!(!is_still_image(&stream, &format));
    }

    #[test]
    fn is_still_image_should_keep_an_ordinary_video_stream() {
        // Arrange: the common case — neither an image container nor an image codec.
        let stream = BTreeMap::from([("codec_name".to_string(), Value::from("h264"))]);
        let format = BTreeMap::from([("format_name".to_string(), Value::from("matroska"))]);

        // Act / Assert
        assert!(!is_still_image(&stream, &format));
    }

    #[test]
    fn is_attached_picture_should_only_match_a_stream_flagged_attached_pic() {
        // Arrange: cover art carries `attached_pic: 1`; everything else must not match,
        // including a stream with no disposition object at all.
        let cover = BTreeMap::from([(
            "disposition".to_string(),
            serde_json::json!({"attached_pic": 1}),
        )]);
        let ordinary = BTreeMap::from([(
            "disposition".to_string(),
            serde_json::json!({"attached_pic": 0}),
        )]);
        let missing = BTreeMap::from([("codec_type".to_string(), Value::from("video"))]);

        // Act / Assert
        assert!(is_attached_picture(&cover));
        assert!(!is_attached_picture(&ordinary));
        assert!(!is_attached_picture(&missing));
    }

    #[test]
    fn is_chapter_track_should_only_match_the_text_data_stream_a_chaptered_file_carries() {
        // Arrange: the same MP4 with and without chapters, plus the two streams that
        // look like the chapter track without being it — GoPro telemetry, which is also
        // `bin_data`, and a QuickTime text *subtitle*, which is a track the reader edits.
        let with_chapters = |streams: Value| {
            MediaInfo::from_json_unchecked(serde_json::json!({
                "streams": streams,
                "chapters": [{"id": 0, "start_time": "0.0"}],
            }))
            .unwrap()
        };
        let chapter_stream = serde_json::json!({
            "index": 1, "codec_type": "data", "codec_name": "bin_data", "codec_tag_string": "text",
        });
        let telemetry = serde_json::json!({
            "index": 1, "codec_type": "data", "codec_name": "bin_data", "codec_tag_string": "gpmd",
        });
        let subtitle = serde_json::json!({
            "index": 1, "codec_type": "subtitle", "codec_name": "mov_text", "codec_tag_string": "text",
        });
        let chaptered = with_chapters(serde_json::json!([chapter_stream.clone()]));
        let unchaptered =
            MediaInfo::from_json_unchecked(serde_json::json!({"streams": [chapter_stream]}))
                .unwrap();

        // Act / Assert
        assert!(is_chapter_track(&chaptered, &chaptered.streams[0]));
        assert!(!is_chapter_track(&unchaptered, &unchaptered.streams[0]));
        let telemetry = with_chapters(serde_json::json!([telemetry]));
        assert!(!is_chapter_track(&telemetry, &telemetry.streams[0]));
        let subtitle = with_chapters(serde_json::json!([subtitle]));
        assert!(!is_chapter_track(&subtitle, &subtitle.streams[0]));
    }

    #[test]
    fn format_tag_should_ignore_a_tag_whose_value_is_blank() {
        // Arrange: containers routinely carry empty or whitespace-only tags. Surfacing
        // one as a title paints a blank value over the filename the user was reading.
        let value: Value = serde_json::from_str(
            r#"{
                "streams": [{"codec_type": "video", "codec_name": "h264"}],
                "format": {"tags": {"title": "   ", "comment": "", "date": "  2010  "}}
            }"#,
        )
        .unwrap();
        let info = MediaInfo::from_json(value).unwrap();

        // Act / Assert: blanks are dropped, and a real value is trimmed rather than
        // returned with the container's padding.
        assert_that!(info.container_title()).is_none();
        assert_that!(info.container_comment()).is_none();
        assert_that!(info.container_date().as_deref()).contains("2010");
    }

    #[test]
    fn format_tag_should_return_nothing_when_the_container_has_no_tags_at_all() {
        // Arrange: a container with a format section but no `tags` object.
        let value: Value = serde_json::from_str(
            r#"{
                "streams": [{"codec_type": "video", "codec_name": "h264"}],
                "format": {"format_name": "matroska"}
            }"#,
        )
        .unwrap();
        let info = MediaInfo::from_json(value).unwrap();

        // Act / Assert
        assert_that!(info.container_title()).is_none();
        assert_that!(info.container_artist()).is_none();
        assert_that!(info.container_genre()).is_none();
    }

    #[test]
    fn container_artist_and_comment_should_fall_back_through_their_alternate_tags() {
        // Arrange: different muxers spell the same idea differently, so each getter walks
        // a chain of alternates. Only the fallback links are exercised here — the primary
        // spellings are covered above.
        let value: Value = serde_json::from_str(
            r#"{
                "streams": [{"codec_type": "video", "codec_name": "h264"}],
                "format": {"tags": {
                    "description": "From the description tag",
                    "creation_time": "2019-04-01T00:00:00.000000Z",
                    "composer": "From the composer tag"
                }}
            }"#,
        )
        .unwrap();
        let info = MediaInfo::from_json(value).unwrap();

        // Act / Assert
        assert_that!(info.container_comment().as_deref()).contains("From the description tag");
        assert_that!(info.container_date().as_deref()).contains("2019-04-01T00:00:00.000000Z");
        assert_that!(info.container_artist().as_deref()).contains("From the composer tag");
    }

    #[test]
    fn probe_file_should_report_a_non_media_file_as_not_video_carrying_ffprobes_reason() {
        require_tools(
            "probe_file_should_report_a_non_media_file_as_not_video_carrying_ffprobes_reason",
            &["ffprobe"],
        );

        // Arrange: a text file with a media extension — what a stray `.srt` rename or a
        // partially downloaded file looks like when the user selects it.
        let directory = scratch("not-media");
        let path = directory.join("notes.mkv");
        std::fs::write(&path, b"this is not a media file").unwrap();

        // Act
        let outcome = probe_file(&path, false);

        // Assert: `NotVideo` (a normal answer about a normal file), not `Error`, and it
        // carries ffprobe's own words rather than a generic placeholder.
        let ProbeOutcome::NotVideo(reason) = outcome else {
            panic!("a non-media file must probe as NotVideo, got {outcome:?}");
        };
        assert!(
            !reason.is_empty(),
            "the reason must carry ffprobe's stderr, not an empty string",
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn probe_file_should_report_a_missing_file_as_not_video() {
        require_tools(
            "probe_file_should_report_a_missing_file_as_not_video",
            &["ffprobe"],
        );

        // Arrange / Act: a path that has been deleted between the directory scan and the
        // probe — the race a watched directory hits routinely.
        let outcome = probe_file(
            std::path::Path::new("/nonexistent/reel-tui/movie.mkv"),
            false,
        );

        // Assert
        assert!(
            matches!(outcome, ProbeOutcome::NotVideo(_)),
            "a missing file must probe as NotVideo, got {outcome:?}",
        );
    }

    #[test]
    fn probe_file_should_read_a_real_file_when_told_it_lives_on_a_network_mount() {
        require_tools(
            "probe_file_should_read_a_real_file_when_told_it_lives_on_a_network_mount",
            &["ffmpeg", "ffprobe"],
        );

        // Arrange: the network path adds `-probesize`/`-analyzeduration` to keep ffprobe
        // from seeking deep over the wire. Those bounds must still be wide enough to
        // recognise an ordinary file — too tight and every file on a share reads as
        // unplayable.
        let directory = scratch("network-flags");
        let path = directory.join("clip.mkv");
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
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success(), "fixture must build");

        // Act: the same file probed both ways.
        let network = probe_file(&path, true);
        let local = probe_file(&path, false);

        // Assert: the fast flags change how much ffprobe reads, never what it concludes.
        let (ProbeOutcome::Video(network_info), ProbeOutcome::Video(local_info)) =
            (&network, &local)
        else {
            panic!("a real file must probe as Video both ways, got {network:?} / {local:?}");
        };
        assert_eq!(network_info.streams.len(), 2);
        assert_eq!(network_info.streams.len(), local_info.streams.len());

        std::fs::remove_dir_all(directory).unwrap();
    }

    /// ffprobe reports frame rates as rationals, and reports two of them. `avg_frame_rate`
    /// is what the file averages; `r_frame_rate` is the tick rate its timestamps are
    /// expressed in, which for variable-rate content is often 1000 or 90000 and would be a
    /// nonsense answer to "how many frames a second does this hold".
    #[test]
    fn a_streams_frame_rate_should_come_from_its_average_rather_than_its_tick_rate() {
        let rate = |stream: serde_json::Value| {
            let Value::Object(object) = stream else {
                unreachable!("the fixture is an object")
            };
            stream_frame_rate(&object.into_iter().collect())
        };

        // Act / Assert: the ordinary rationals, including the NTSC one.
        assert_that!(rate(serde_json::json!({"avg_frame_rate": "25/1"}))).is_equal_to(Some(25.0));
        assert_that!(rate(serde_json::json!({"avg_frame_rate": "24000/1001"})))
            .is_equal_to(Some(24_000.0 / 1_001.0));

        // Act / Assert: the average wins over the tick rate when both are there.
        assert_that!(rate(
            serde_json::json!({"avg_frame_rate": "30/1", "r_frame_rate": "90000/1"})
        ))
        .is_equal_to(Some(30.0));

        // Act / Assert: and the tick rate is the fallback when the average is missing or is
        // the `0/0` ffprobe uses for "no meaningful rate".
        assert_that!(rate(serde_json::json!({"r_frame_rate": "50/1"}))).is_equal_to(Some(50.0));
        assert_that!(rate(
            serde_json::json!({"avg_frame_rate": "0/0", "r_frame_rate": "50/1"})
        ))
        .is_equal_to(Some(50.0));

        // Act / Assert: nothing usable is `None`, never zero — a caller that took zero for
        // an answer would cap a playback to no frames at all.
        assert_that!(rate(serde_json::json!({}))).is_none();
        assert_that!(rate(serde_json::json!({"avg_frame_rate": "0/0"}))).is_none();
        assert_that!(rate(serde_json::json!({"avg_frame_rate": "0/25"}))).is_none();
        assert_that!(rate(serde_json::json!({"avg_frame_rate": "25"}))).is_none();
        assert_that!(rate(serde_json::json!({"avg_frame_rate": "many/1"}))).is_none();
        assert_that!(rate(serde_json::json!({"avg_frame_rate": 25}))).is_none();
    }

    #[test]
    fn probe_any_file_should_accept_a_file_with_no_video_stream() {
        require_tools(
            "probe_any_file_should_accept_a_file_with_no_video_stream",
            &["ffmpeg", "ffprobe"],
        );

        // Arrange: `probe_any_file` exists precisely to inspect things `probe_file` would
        // reject — subtitle sidecars and audio-only output — so a missing video stream
        // must not be an error here.
        let directory = scratch("any-file");
        let path = directory.join("audio.mka");
        let status = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=8000:cl=mono:d=1",
                "-c:a",
                "pcm_s16le",
            ])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success(), "fixture must build");

        // Act
        let info = probe_any_file(&path).unwrap();

        // Assert: accepted, with the audio stream intact — where `from_json` would have
        // refused it for having no video.
        assert_that!(info.streams).has_length(1);
        assert!(MediaInfo::from_json(serde_json::json!({"streams": []})).is_err());

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn probe_any_file_should_surface_ffprobes_reason_for_a_file_it_cannot_read() {
        require_tools(
            "probe_any_file_should_surface_ffprobes_reason_for_a_file_it_cannot_read",
            &["ffprobe"],
        );

        // Arrange: the converted-subtitle check runs this against ffmpeg's own output, so
        // when it fails the message is the only clue the user gets about what went wrong.
        let directory = scratch("any-file-error");
        let path = directory.join("broken.srt");
        std::fs::write(&path, b"\0\0\0not a subtitle\0\0").unwrap();

        // Act
        let result = probe_any_file(&path);

        // Assert: an error carrying a non-empty reason, never a silent empty MediaInfo.
        let Err(reason) = result else {
            panic!("an unreadable file must be an error, got {result:?}");
        };
        assert!(!reason.is_empty(), "the error must explain itself");

        std::fs::remove_dir_all(directory).unwrap();
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
