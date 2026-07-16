use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::{self, Receiver, Sender},
};

use serde_json::Value;

#[derive(Clone, Debug)]
pub struct ProbeRequest {
    pub generation: u64,
    pub path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct ProbeResponse {
    pub generation: u64,
    pub path: PathBuf,
    pub outcome: ProbeOutcome,
}

#[derive(Clone, Debug)]
pub enum ProbeOutcome {
    Video(MediaInfo),
    NotVideo(String),
    Error(String),
}

#[derive(Clone, Debug)]
pub struct MediaInfo {
    pub format: BTreeMap<String, Value>,
    pub streams: Vec<BTreeMap<String, Value>>,
    pub chapters: Vec<BTreeMap<String, Value>>,
}

impl MediaInfo {
    fn from_json(value: Value) -> Result<Self, String> {
        let object = value
            .as_object()
            .ok_or_else(|| "ffprobe returned an invalid JSON document".to_string())?;
        let format = object_map(object.get("format"));
        let streams = object_array(object.get("streams"));
        let chapters = object_array(object.get("chapters"));

        let has_video = streams.iter().any(|stream| {
            stream.get("codec_type").and_then(Value::as_str) == Some("video")
                && !is_attached_picture(stream)
        });

        if !has_video {
            return Err("No video stream found".to_string());
        }

        Ok(Self {
            format,
            streams,
            chapters,
        })
    }
}

fn object_map(value: Option<&Value>) -> BTreeMap<String, Value> {
    value
        .and_then(Value::as_object)
        .map(|map| map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default()
}

fn object_array(value: Option<&Value>) -> Vec<BTreeMap<String, Value>> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_object)
                .map(|map| map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .collect()
        })
        .unwrap_or_default()
}

fn is_attached_picture(stream: &BTreeMap<String, Value>) -> bool {
    stream
        .get("disposition")
        .and_then(Value::as_object)
        .and_then(|disposition| disposition.get("attached_pic"))
        .and_then(Value::as_i64)
        == Some(1)
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

fn probe_file(path: &Path) -> ProbeOutcome {
    let output = match Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-of",
            "json",
            "-show_format",
            "-show_streams",
            "-show_chapters",
        ])
        .arg(path)
        .output()
    {
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
    use super::*;

    #[test]
    fn parses_video_and_preserves_all_streams() {
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

        let info = MediaInfo::from_json(value).unwrap();
        assert_eq!(info.streams.len(), 3);
        assert_eq!(info.chapters.len(), 1);
        assert_eq!(info.format["format_name"], "matroska");
    }

    #[test]
    fn rejects_audio_with_attached_cover_art() {
        let value: Value = serde_json::from_str(
            r#"{"streams":[
                {"codec_type":"audio"},
                {"codec_type":"video","disposition":{"attached_pic":1}}
            ]}"#,
        )
        .unwrap();

        assert!(MediaInfo::from_json(value).is_err());
    }
}
