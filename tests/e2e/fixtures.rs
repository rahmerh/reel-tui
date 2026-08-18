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
    /// SRT body to mux, or `None` for one cue spanning the whole clip. Scenarios that
    /// look *inside* a track — cue counts, overlapping spans, per-cue selection — need
    /// more than the default single cue can express.
    pub cues: Option<&'static str>,
}

impl SubtitleSpec {
    pub fn new(language: &'static str, codec: &'static str) -> Self {
        Self {
            language,
            codec,
            default: false,
            cues: None,
        }
    }

    pub fn cues(mut self, cues: &'static str) -> Self {
        self.cues = Some(cues);
        self
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
    pub video_disposition: Option<&'static str>,
    /// Degrees of display rotation tagged onto the first video track. Applied in a second
    /// copy pass, because `-display_rotation` is an input option and the fixture's own
    /// inputs are `lavfi` sources with nothing to rotate.
    pub video_rotation: Option<u32>,
    /// Language tag per audio track; the first is marked default unless an explicit
    /// disposition is supplied for it.
    pub audio_languages: Vec<&'static str>,
    pub audio_dispositions: Vec<&'static str>,
    pub audio_codec: &'static str,
    pub audio_codecs: Vec<&'static str>,
    pub audio_titles: Vec<Option<&'static str>>,
    pub audio_sample_rate: u32,
    /// Writes a `tmcd` timecode track, which ffprobe reports as a `data` stream whose
    /// codec Reel's container table does not list. Cameras produce these constantly, so
    /// it is the realistic shape of "content the file has always held and no edit
    /// touches".
    pub timecode: Option<&'static str>,
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
            video_disposition: None,
            video_rotation: None,
            audio_languages: vec!["eng"],
            audio_dispositions: Vec::new(),
            audio_codec: "aac",
            audio_codecs: Vec::new(),
            audio_titles: Vec::new(),
            audio_sample_rate: 48_000,
            timecode: None,
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

    pub fn timecode(mut self, timecode: &'static str) -> Self {
        self.timecode = Some(timecode);
        self
    }

    /// Seconds of video. Scenarios that grab a frame at a cue need the media to actually
    /// last as long as their cues do.
    pub fn duration(mut self, seconds: f32) -> Self {
        self.duration = seconds;
        self
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

    pub fn video_disposition(mut self, disposition: &'static str) -> Self {
        self.video_disposition = Some(disposition);
        self
    }

    pub fn video_rotation(mut self, degrees: u32) -> Self {
        self.video_rotation = Some(degrees);
        self
    }

    pub fn audio_sample_rate(mut self, rate: u32) -> Self {
        self.audio_sample_rate = rate;
        self
    }

    pub fn audio_dispositions(mut self, dispositions: &[&'static str]) -> Self {
        self.audio_dispositions = dispositions.to_vec();
        self
    }

    pub fn audio_codecs(mut self, codecs: &[&'static str]) -> Self {
        self.audio_codecs = codecs.to_vec();
        self
    }

    pub fn audio_titles(mut self, titles: &[Option<&'static str>]) -> Self {
        self.audio_titles = titles.to_vec();
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
    assert!(
        spec.audio_codecs.is_empty() || spec.audio_codecs.len() == spec.audio_languages.len(),
        "per-track audio codecs must match the audio-track count"
    );
    assert!(
        spec.audio_titles.is_empty() || spec.audio_titles.len() == spec.audio_languages.len(),
        "audio titles must match the audio-track count"
    );
    for (index, subtitle) in spec.subtitles.iter().enumerate() {
        let srt_path = parent.join(format!(".fixture-{stem}-{index}.srt"));
        let body = subtitle
            .cues
            .map(str::to_string)
            .unwrap_or_else(|| srt_body(subtitle.language, spec.duration));
        fs::write(&srt_path, body).unwrap();
        srt_paths.push(srt_path);
    }

    let mut command = Command::new("ffmpeg");
    command.args(["-v", "error", "-nostdin", "-y"]);

    let video_source = format!(
        "color=c=black:s={}x{}:r=10:d={}",
        spec.width, spec.height, spec.duration
    );
    command.args(["-f", "lavfi", "-i", &video_source]);

    let audio_source = format!(
        "anullsrc=r={}:cl=stereo:d={}",
        spec.audio_sample_rate, spec.duration
    );
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
    if let Some(disposition) = spec.video_disposition {
        command.args(["-disposition:v:0", disposition]);
    }
    if spec.audio_codecs.is_empty() {
        command.args(["-c:a", spec.audio_codec]);
    } else {
        for (index, codec) in spec.audio_codecs.iter().enumerate() {
            command.arg(format!("-c:a:{index}")).arg(codec);
        }
    }

    for (index, subtitle) in spec.subtitles.iter().enumerate() {
        command.args([&format!("-c:s:{index}"), subtitle.codec]);
    }

    for (index, language) in spec.audio_languages.iter().enumerate() {
        command
            .arg(format!("-metadata:s:a:{index}"))
            .arg(format!("language={language}"));
        command.arg(format!("-disposition:a:{index}")).arg(
            spec.audio_dispositions
                .get(index)
                .copied()
                .unwrap_or(if index == 0 { "default" } else { "0" }),
        );
        if let Some(Some(title)) = spec.audio_titles.get(index) {
            command
                .arg(format!("-metadata:s:a:{index}"))
                .arg(format!("title={title}"));
        }
    }
    for (index, subtitle) in spec.subtitles.iter().enumerate() {
        command
            .arg(format!("-metadata:s:s:{index}"))
            .arg(format!("language={}", subtitle.language));
        command
            .arg(format!("-disposition:s:{index}"))
            .arg(if subtitle.default { "default" } else { "0" });
    }

    if let Some(timecode) = spec.timecode {
        command.args(["-timecode", timecode]);
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

    if let Some(degrees) = spec.video_rotation {
        let rotated = parent.join(format!(".fixture-{stem}-rotated"));
        let output = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-nostdin",
                "-y",
                "-display_rotation:v:0",
                &degrees.to_string(),
                "-i",
            ])
            .arg(path)
            .args(["-map", "0", "-c", "copy", "-f", spec.muxer])
            .arg(&rotated)
            .output()
            .expect("ffmpeg should be runnable");
        assert!(
            output.status.success(),
            "failed to rotate fixture {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        fs::rename(&rotated, path).unwrap();
    }
}

/// Builds a Matroska file carrying a genuine VobSub bitmap subtitle. FFmpeg cannot
/// render text subtitles into bitmap subtitles, so `seconv` creates the `.idx`/`.sub`
/// pair first and FFmpeg muxes that pair unchanged. This is the source shape needed
/// to exercise Reel's real OCR path.
pub fn write_vobsub_media(path: &Path, language: &str) {
    let parent = path.parent().expect("fixture path needs a parent");
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .expect("fixture path needs a stem");
    let text = parent.join(format!(".fixture-{stem}-bitmap.srt"));
    fs::write(&text, "1\n00:00:00,200 --> 00:00:01,800\nHELLO WORLD\n\n").unwrap();
    let rendered = Command::new("seconv")
        .arg(&text)
        .arg("vobsub")
        .arg("--overwrite")
        .current_dir(parent)
        .output()
        .expect("seconv should be runnable");
    let idx = text.with_extension("idx");
    let sub = text.with_extension("sub");
    assert!(
        rendered.status.success() && idx.exists() && sub.exists(),
        "seconv failed to render the VobSub fixture: {}",
        String::from_utf8_lossy(&rendered.stderr)
    );

    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i"])
        .arg("color=c=black:s=320x240:r=10:d=2")
        .args(["-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo:d=2", "-i"])
        .arg(&idx)
        .args([
            "-map",
            "0:v:0",
            "-map",
            "1:a:0",
            "-map",
            "2:s:0",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-preset",
            "ultrafast",
            "-c:a",
            "aac",
            "-c:s",
            "copy",
            "-metadata:s:s:0",
        ])
        .arg(format!("language={language}"))
        .arg(path)
        .output()
        .expect("ffmpeg should be runnable");
    assert!(
        output.status.success(),
        "failed to mux VobSub fixture {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );

    for temporary in [text, idx, sub] {
        fs::remove_file(temporary).unwrap();
    }
}

/// Adds a real chapter and attachment stream to an ordinary Matroska fixture. These
/// structures sit outside the common video/audio/subtitle groups and are therefore
/// easy for an otherwise-successful remux to drop accidentally.
pub fn write_media_with_chapter_and_attachment(path: &Path) {
    write_media(path, &MediaSpec::mkv().audio(&["eng", "nld"]));
    let parent = path.parent().expect("fixture path needs a parent");
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .expect("fixture path needs a stem");
    let chapters = parent.join(format!(".fixture-{stem}-chapters.ffmeta"));
    let attachment = parent.join(format!(".fixture-{stem}-notes.txt"));
    let enhanced = parent.join(format!(".fixture-{stem}-enhanced.mkv"));
    fs::write(
        &chapters,
        ";FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=500\ntitle=Opening\n",
    )
    .unwrap();
    fs::write(&attachment, "attachment must survive the remux\n").unwrap();

    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-y", "-i"])
        .arg(path)
        .args(["-f", "ffmetadata", "-i"])
        .arg(&chapters)
        .args(["-map", "0", "-map_chapters", "1", "-c", "copy", "-attach"])
        .arg(&attachment)
        .args([
            "-metadata:s:t:0",
            "mimetype=text/plain",
            "-metadata:s:t:0",
            "filename=notes.txt",
        ])
        .arg(&enhanced)
        .output()
        .expect("ffmpeg should be runnable");
    assert!(
        output.status.success(),
        "failed to add chapter and attachment to {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    fs::rename(&enhanced, path).unwrap();
    fs::remove_file(chapters).unwrap();
    fs::remove_file(attachment).unwrap();
}

fn srt_body(language: &str, duration: f32) -> String {
    let end = duration.max(0.5);
    let whole = end as u32;
    let millis = ((end - whole as f32) * 1000.0) as u32;
    format!("1\n00:00:00,000 --> 00:00:{whole:02},{millis:03}\n{language} subtitle line\n\n")
}

/// Writes a single solid-colour PNG, for planting in the preview frame cache.
///
/// A cached frame the application could not possibly have rendered from the fixture
/// video, so a page that draws it is a page that read the cache rather than running
/// `ffmpeg` again.
pub fn write_solid_png(path: &Path, color: &str, width: u32, height: u32) {
    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i"])
        .arg(format!("color=c={color}:s={width}x{height}"))
        .args(["-frames:v", "1"])
        .arg(path)
        .output()
        .expect("ffmpeg should be runnable");
    assert!(
        output.status.success() && path.exists(),
        "failed to write the solid frame fixture: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
