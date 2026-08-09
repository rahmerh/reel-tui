//! Synthetic media fixtures built with `lavfi`, in the style of the `src/edit.rs`
//! ffmpeg tests but parameterised over codec and container.
//!
//! Codec/container realism is the point. Every failure these tests reproduce came
//! from a genuine multi-track file whose subtitle codec was legal in the source
//! container and illegal in the target (`subrip` into MP4, `mov_text` into Matroska),
//! which is precisely what the existing hand-built `ffv1`/`pcm_s16le` fixtures cannot
//! express.

use std::fs;
use std::path::Path;
use std::process::Command;

/// One subtitle track: the language tag it carries, the codec it is muxed as, and
/// whether the container should mark it default.
pub struct SubtitleSpec {
    pub language: &'static str,
    pub codec: &'static str,
    pub default: bool,
}

impl SubtitleSpec {
    pub fn new(language: &'static str, codec: &'static str) -> Self {
        Self {
            language,
            codec,
            default: false,
        }
    }
}

pub struct MediaSpec {
    /// ffmpeg muxer name — `matroska` or `mp4`. Deliberately independent of the
    /// output file's extension, so a misnamed container can be produced on purpose.
    pub muxer: &'static str,
    pub width: u32,
    pub height: u32,
    pub duration: f32,
    pub video_codec: &'static str,
    /// Language tag per audio track; the first is marked default.
    pub audio_languages: Vec<&'static str>,
    pub audio_codec: &'static str,
    pub subtitles: Vec<SubtitleSpec>,
}

impl Default for MediaSpec {
    fn default() -> Self {
        Self {
            muxer: "matroska",
            width: 64,
            height: 48,
            duration: 1.0,
            video_codec: "libx264",
            audio_languages: vec!["eng"],
            audio_codec: "aac",
            subtitles: Vec::new(),
        }
    }
}

impl MediaSpec {
    pub fn mkv() -> Self {
        Self::default()
    }

    pub fn mp4() -> Self {
        Self {
            muxer: "mp4",
            ..Self::default()
        }
    }

    pub fn size(mut self, width: u32, height: u32) -> Self {
        self.width = width;
        self.height = height;
        self
    }

    pub fn audio(mut self, languages: &[&'static str]) -> Self {
        self.audio_languages = languages.to_vec();
        self
    }

    pub fn subtitles(mut self, subtitles: Vec<SubtitleSpec>) -> Self {
        self.subtitles = subtitles;
        self
    }
}

/// Renders `spec` to `path`. Subtitle inputs are written as sidecar `.srt` files next
/// to the output and removed afterwards, so the directory the app scans contains only
/// the media file itself unless a test wants otherwise.
pub fn write_media(path: &Path, spec: &MediaSpec) {
    let parent = path.parent().expect("fixture path needs a parent");
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .expect("fixture path needs a stem");

    let mut srt_paths = Vec::new();
    for (index, subtitle) in spec.subtitles.iter().enumerate() {
        let srt_path = parent.join(format!(".fixture-{stem}-{index}.srt"));
        fs::write(&srt_path, srt_body(subtitle.language, spec.duration)).unwrap();
        srt_paths.push(srt_path);
    }

    let mut command = Command::new("ffmpeg");
    command.args(["-v", "error", "-nostdin", "-y"]);

    let video_source = format!(
        "color=c=black:s={}x{}:r=10:d={}",
        spec.width, spec.height, spec.duration
    );
    command.args(["-f", "lavfi", "-i", &video_source]);

    let audio_source = format!("anullsrc=r=48000:cl=stereo:d={}", spec.duration);
    for _ in &spec.audio_languages {
        command.args(["-f", "lavfi", "-i", &audio_source]);
    }
    for srt_path in &srt_paths {
        command.arg("-i").arg(srt_path);
    }

    command.args(["-map", "0:v:0"]);
    for index in 0..spec.audio_languages.len() {
        command.args(["-map", &format!("{}:a:0", index + 1)]);
    }
    let subtitle_input_offset = 1 + spec.audio_languages.len();
    for index in 0..spec.subtitles.len() {
        command.args(["-map", &format!("{}:s:0", subtitle_input_offset + index)]);
    }

    // yuv420p keeps the output playable by anything, and matters for MP4 in
    // particular; ultrafast keeps fixture creation off the critical path.
    command.args(["-c:v", spec.video_codec]);
    if spec.video_codec.starts_with("libx26") {
        command.args(["-pix_fmt", "yuv420p", "-preset", "ultrafast"]);
    }
    command.args(["-c:a", spec.audio_codec]);

    for (index, subtitle) in spec.subtitles.iter().enumerate() {
        command.args([&format!("-c:s:{index}"), subtitle.codec]);
    }

    for (index, language) in spec.audio_languages.iter().enumerate() {
        command
            .arg(format!("-metadata:s:a:{index}"))
            .arg(format!("language={language}"));
        command
            .arg(format!("-disposition:a:{index}"))
            .arg(if index == 0 { "default" } else { "0" });
    }
    for (index, subtitle) in spec.subtitles.iter().enumerate() {
        command
            .arg(format!("-metadata:s:s:{index}"))
            .arg(format!("language={}", subtitle.language));
        command
            .arg(format!("-disposition:s:{index}"))
            .arg(if subtitle.default { "default" } else { "0" });
    }

    command.args(["-f", spec.muxer]);
    command.arg(path);

    let output = command.output().expect("ffmpeg should be runnable");
    assert!(
        output.status.success(),
        "failed to build fixture {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );

    for srt_path in srt_paths {
        let _ = fs::remove_file(srt_path);
    }
}

fn srt_body(language: &str, duration: f32) -> String {
    let end = duration.max(0.5);
    let whole = end as u32;
    let millis = ((end - whole as f32) * 1000.0) as u32;
    format!("1\n00:00:00,000 --> 00:00:{whole:02},{millis:03}\n{language} subtitle line\n\n")
}
