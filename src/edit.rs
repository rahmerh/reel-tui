use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::Value;

use crate::{
    app::TrackRef,
    files::FileFingerprint,
    probe::{
        MediaInfo, ProbeOutcome, is_attached_picture, is_chapter_track, probe_any_file, probe_file,
    },
    subtitle::{
        CueEdit, SidecarEntry, SubtitleChange, SubtitleFlag, SubtitleFormat, SubtitleMetadata,
        SubtitleSource, canonical_language_code, language_choice, sidecar_filename, stream_cc,
        stream_commentary, stream_forced, stream_hearing_impaired, stream_language,
        stream_original, stream_title,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoCodec {
    Original,
    H264,
    Hevc,
    Av1,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum AudioCodec {
    Original,
    Aac,
    Ac3,
    Eac3,
    Opus,
    Flac,
    Alac,
    Mp3,
    Vorbis,
}

impl AudioCodec {
    pub const TARGETS: [Self; 8] = [
        Self::Aac,
        Self::Ac3,
        Self::Eac3,
        Self::Opus,
        Self::Flac,
        Self::Alac,
        Self::Mp3,
        Self::Vorbis,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Original => "Original",
            Self::Aac => "AAC",
            Self::Ac3 => "Dolby Digital (AC-3)",
            Self::Eac3 => "Dolby Digital Plus (E-AC-3)",
            Self::Opus => "Opus",
            Self::Flac => "FLAC",
            Self::Alac => "ALAC",
            Self::Mp3 => "MP3",
            Self::Vorbis => "Vorbis",
        }
    }

    pub(crate) fn codec_name(self) -> Option<&'static str> {
        match self {
            Self::Original => None,
            Self::Aac => Some("aac"),
            Self::Ac3 => Some("ac3"),
            Self::Eac3 => Some("eac3"),
            Self::Opus => Some("opus"),
            Self::Flac => Some("flac"),
            Self::Alac => Some("alac"),
            Self::Mp3 => Some("mp3"),
            Self::Vorbis => Some("vorbis"),
        }
    }

    pub(crate) fn encoder(self) -> Option<&'static str> {
        match self {
            Self::Original => None,
            Self::Aac => Some("aac"),
            Self::Ac3 => Some("ac3"),
            Self::Eac3 => Some("eac3"),
            Self::Opus => Some("libopus"),
            Self::Flac => Some("flac"),
            Self::Alac => Some("alac"),
            Self::Mp3 => Some("libmp3lame"),
            Self::Vorbis => Some("libvorbis"),
        }
    }

    pub fn is_lossless(self) -> bool {
        matches!(self, Self::Flac | Self::Alac)
    }

    pub(crate) fn from_codec_name(codec: &str) -> Option<Self> {
        match codec {
            "aac" => Some(Self::Aac),
            "ac3" => Some(Self::Ac3),
            "eac3" => Some(Self::Eac3),
            "opus" => Some(Self::Opus),
            "flac" => Some(Self::Flac),
            "alac" => Some(Self::Alac),
            "mp3" => Some(Self::Mp3),
            "vorbis" => Some(Self::Vorbis),
            _ => None,
        }
    }

    pub(crate) fn supports_channels(self, channels: u8) -> bool {
        match self {
            Self::Original => true,
            Self::Ac3 | Self::Eac3 => channels <= 6,
            Self::Mp3 => channels <= 2,
            Self::Aac | Self::Opus | Self::Flac | Self::Alac | Self::Vorbis => channels <= 8,
        }
    }

    pub(crate) fn supports_sample_rate(self, rate: u32) -> bool {
        match self {
            Self::Original | Self::Flac | Self::Alac | Self::Vorbis => matches!(
                rate,
                7_350
                    | 8_000
                    | 11_025
                    | 12_000
                    | 16_000
                    | 22_050
                    | 24_000
                    | 32_000
                    | 44_100
                    | 48_000
                    | 64_000
                    | 88_200
                    | 96_000
                    | 176_400
                    | 192_000
            ),
            Self::Aac => matches!(
                rate,
                7_350
                    | 8_000
                    | 11_025
                    | 12_000
                    | 16_000
                    | 22_050
                    | 24_000
                    | 32_000
                    | 44_100
                    | 48_000
                    | 64_000
                    | 88_200
                    | 96_000
            ),
            Self::Ac3 | Self::Eac3 => matches!(rate, 32_000 | 44_100 | 48_000),
            Self::Opus => matches!(rate, 8_000 | 12_000 | 16_000 | 24_000 | 48_000),
            Self::Mp3 => matches!(
                rate,
                8_000 | 11_025 | 12_000 | 16_000 | 22_050 | 24_000 | 32_000 | 44_100 | 48_000
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioChannelLayout {
    Original,
    Surround71,
    Surround51,
    Stereo,
    Mono,
}

impl AudioChannelLayout {
    pub const TARGETS: [Self; 4] = [Self::Surround71, Self::Surround51, Self::Stereo, Self::Mono];

    pub fn label(self) -> &'static str {
        match self {
            Self::Original => "Original",
            Self::Surround71 => "7.1 surround",
            Self::Surround51 => "5.1 surround",
            Self::Stereo => "Stereo",
            Self::Mono => "Mono",
        }
    }

    pub(crate) fn channels(self) -> Option<u8> {
        match self {
            Self::Original => None,
            Self::Surround71 => Some(8),
            Self::Surround51 => Some(6),
            Self::Stereo => Some(2),
            Self::Mono => Some(1),
        }
    }
}

/// Sample rates an encoder may be asked for, highest first. Reel never offers these to
/// the user: `resolved_audio_sample_rate` keeps the source rate whenever the chosen codec
/// accepts it, and otherwise steps down to the highest rate the codec does accept, which
/// is the safe automatic behaviour the readme promises.
const AUDIO_SAMPLE_RATE_CANDIDATES: [u32; 15] = [
    192_000, 176_400, 96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000,
    12_000, 11_025, 8_000, 7_350,
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioMetadata {
    pub language: String,
    pub title: Option<String>,
    pub commentary: bool,
    pub hearing_impaired: bool,
    pub audio_description: bool,
    pub original: bool,
    pub dubbed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioRole {
    Commentary,
    HearingImpaired,
    AudioDescription,
    Original,
    Dubbed,
}

impl AudioRole {
    pub const ALL: [Self; 5] = [
        Self::Commentary,
        Self::HearingImpaired,
        Self::AudioDescription,
        Self::Original,
        Self::Dubbed,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Commentary => "Commentary",
            Self::HearingImpaired => "Hearing impaired",
            Self::AudioDescription => "Audio description",
            Self::Original => "Original",
            Self::Dubbed => "Dubbed",
        }
    }
}

impl AudioMetadata {
    pub fn get_role(&self, role: AudioRole) -> bool {
        match role {
            AudioRole::Commentary => self.commentary,
            AudioRole::HearingImpaired => self.hearing_impaired,
            AudioRole::AudioDescription => self.audio_description,
            AudioRole::Original => self.original,
            AudioRole::Dubbed => self.dubbed,
        }
    }

    pub fn set_role(&mut self, role: AudioRole, value: bool) {
        match role {
            AudioRole::Commentary => self.commentary = value,
            AudioRole::HearingImpaired => self.hearing_impaired = value,
            AudioRole::AudioDescription => self.audio_description = value,
            AudioRole::Original => {
                self.original = value;
                if value {
                    self.dubbed = false;
                }
            }
            AudioRole::Dubbed => {
                self.dubbed = value;
                if value {
                    self.original = false;
                }
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioSettings {
    pub codec: AudioCodec,
    pub channel_layout: AudioChannelLayout,
    pub metadata: AudioMetadata,
}

pub(crate) const CHANNEL_UPMIX_NOT_IMPLEMENTED: &str = "Channel upmixing is not implemented.";

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ContainerFormat {
    Matroska,
    Mp4,
    Mov,
    WebM,
}

impl ContainerFormat {
    pub const TARGETS: [Self; 4] = [Self::Matroska, Self::Mp4, Self::Mov, Self::WebM];

    pub fn label(self) -> &'static str {
        match self {
            Self::Matroska => "MKV",
            Self::Mp4 => "MP4",
            Self::Mov => "MOV",
            Self::WebM => "WebM",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Matroska => "mkv",
            Self::Mp4 => "mp4",
            Self::Mov => "mov",
            Self::WebM => "webm",
        }
    }

    pub fn muxer(self) -> &'static str {
        match self {
            Self::Matroska => "matroska",
            Self::Mp4 => "mp4",
            Self::Mov => "mov",
            Self::WebM => "webm",
        }
    }

    pub fn from_path(path: &Path) -> Option<Self> {
        match path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("mkv" | "mka" | "mks") => Some(Self::Matroska),
            Some("mp4" | "m4v") => Some(Self::Mp4),
            Some("mov") => Some(Self::Mov),
            Some("webm") => Some(Self::WebM),
            _ => None,
        }
    }

    /// Whether ffprobe's raw `format.format_name` (e.g. `"matroska,webm"` or
    /// `"mov,mp4,m4a,3gp,3g2,mj2"`) is consistent with this specific container. MKV
    /// and WebM share one demuxer, as do MP4 and MOV, so `format_name` alone can only
    /// confirm which *family* a file is really in, not the exact member — this checks
    /// family membership.
    fn matches_format_name(self, format_name: &str) -> bool {
        let mut tokens = format_name.split(',');
        match self {
            Self::Matroska | Self::WebM => {
                tokens.any(|token| token == "matroska" || token == "webm")
            }
            Self::Mp4 | Self::Mov => {
                tokens.any(|token| matches!(token, "mov" | "mp4" | "m4a" | "3gp" | "3g2" | "mj2"))
            }
        }
    }

    /// The container a file is actually in, cross-checking the extension against
    /// ffprobe's own `format.format_name` when it's available (`None` when the file
    /// hasn't been probed yet, in which case the extension is all there is to go on).
    /// Renaming a file's extension doesn't change what's actually inside it — a
    /// `movie.mkv` renamed to `movie.mp4` still probes as `"matroska,webm"`, so this
    /// reports it as MKV rather than trusting the now-misleading name. Within a
    /// family ffprobe can't tell apart (MKV vs. WebM, MP4 vs. MOV), the extension —
    /// when it does at least agree on the family — still picks the exact member;
    /// only a genuine mismatch defers to the family ffprobe actually detected.
    pub fn detect(path: &Path, format_name: Option<&str>) -> Option<Self> {
        let by_extension = Self::from_path(path);
        let Some(format_name) = format_name else {
            return by_extension;
        };
        if by_extension.is_some_and(|container| container.matches_format_name(format_name)) {
            return by_extension;
        }
        if Self::Matroska.matches_format_name(format_name) {
            Some(Self::Matroska)
        } else if Self::Mp4.matches_format_name(format_name) {
            Some(Self::Mp4)
        } else {
            by_extension
        }
    }

    pub fn supports_codec(self, kind: &str, codec: &str, attached_picture: bool) -> bool {
        if attached_picture {
            return match self {
                Self::Matroska => true,
                Self::Mp4 | Self::Mov => matches!(codec, "mjpeg" | "png"),
                Self::WebM => false,
            };
        }
        match (self, kind) {
            (Self::Matroska, "video" | "audio" | "attachment") => true,
            (Self::Matroska, "subtitle") => matches!(
                codec,
                "subrip" | "ass" | "ssa" | "webvtt" | "hdmv_pgs_subtitle" | "dvd_subtitle"
            ),
            (Self::Mp4, "video") => {
                matches!(codec, "h264" | "hevc" | "av1" | "mpeg4" | "mjpeg")
            }
            (Self::Mp4, "audio") => {
                matches!(codec, "aac" | "alac" | "ac3" | "eac3" | "mp3" | "opus")
            }
            (Self::Mp4, "subtitle") => codec == "mov_text",
            // ISO-BMFF stores an opaque timed stream as a `gpmd` data track, so a data
            // stream copied out of one MP4 goes back into another — measured, not
            // assumed. Matroska takes none at all ("Only audio, video, and subtitles are
            // supported for Matroska"), which is why this is not simply `true` for both.
            (Self::Mp4 | Self::Mov, "data") => codec == "bin_data",
            (Self::Mov, "video") => matches!(
                codec,
                "h264" | "hevc" | "av1" | "mpeg4" | "mjpeg" | "prores"
            ),
            (Self::Mov, "audio") => {
                matches!(codec, "aac" | "alac" | "ac3" | "eac3" | "mp3" | "opus")
                    || codec.starts_with("pcm_")
            }
            (Self::Mov, "subtitle") => codec == "mov_text",
            (Self::WebM, "video") => matches!(codec, "vp8" | "vp9" | "av1"),
            (Self::WebM, "audio") => matches!(codec, "opus" | "vorbis"),
            (Self::WebM, "subtitle") => codec == "webvtt",
            _ => false,
        }
    }

    pub fn supports_subtitle_flag(self, flag: crate::subtitle::SubtitleFlag) -> bool {
        match self {
            Self::Matroska => flag != SubtitleFlag::Cc,
            Self::Mp4 => !matches!(flag, SubtitleFlag::Original),
            Self::Mov => false,
            Self::WebM => matches!(flag, SubtitleFlag::Forced | SubtitleFlag::Cc),
        }
    }

    pub fn retain_supported_subtitle_metadata(self, metadata: &mut SubtitleMetadata) {
        for flag in SubtitleFlag::ALL {
            if !self.supports_subtitle_flag(flag) {
                metadata.set_flag(flag, false);
            }
        }
    }

    /// Whether this container supports a stream-level `language` tag at all. This is a
    /// container property, not an audio property — `Mov` (plain QuickTime, as opposed to
    /// `Mp4`) doesn't carry it for any stream kind — so video metadata reuses this
    /// unchanged rather than duplicating the exclusion rule.
    pub fn supports_stream_language(self) -> bool {
        !matches!(self, Self::Mov)
    }

    pub fn supports_audio_role(self, role: AudioRole) -> bool {
        match self {
            Self::Matroska => true,
            Self::Mp4 => matches!(
                role,
                AudioRole::Commentary
                    | AudioRole::HearingImpaired
                    | AudioRole::AudioDescription
                    | AudioRole::Dubbed
            ),
            Self::Mov | Self::WebM => false,
        }
    }

    pub fn retain_supported_audio_metadata(self, metadata: &mut AudioMetadata) {
        if !self.supports_stream_language() {
            metadata.language = "und".to_string();
        }
        for role in AudioRole::ALL {
            if !self.supports_audio_role(role) {
                metadata.set_role(role, false);
            }
        }
    }

    /// Whether this container stores a commentary flag on a track at all. Measured to be
    /// the same capability for a picture track as for a soundtrack — Matroska and MP4
    /// round-trip `-disposition:v:0 comment`, MOV and WebM drop it — so video reuses the
    /// audio rule rather than restating it.
    pub fn supports_commentary_flag(self) -> bool {
        self.supports_audio_role(AudioRole::Commentary)
    }

    /// Whether this container stores a display matrix. Measured: Matroska, MP4 and MOV
    /// all round-trip `-display_rotation`. WebM is excluded because reaching it always
    /// re-encodes to VP9, and a re-encode bakes the rotation into the pixels instead of
    /// tagging it — there is no matrix left to store.
    pub fn supports_display_rotation(self) -> bool {
        !matches!(self, Self::WebM)
    }

    pub fn retain_supported_video_metadata(self, metadata: &mut VideoMetadata) {
        if !self.supports_stream_language() {
            metadata.language = "und".to_string();
        }
        if !self.supports_commentary_flag() {
            metadata.commentary = false;
        }
    }
}

impl VideoCodec {
    pub const TARGETS: [Self; 3] = [Self::H264, Self::Hevc, Self::Av1];

    pub fn label(self) -> &'static str {
        match self {
            Self::Original => "Original",
            Self::H264 => "H.264",
            Self::Hevc => "HEVC / H.265",
            Self::Av1 => "AV1",
        }
    }

    pub(crate) fn codec_name(self) -> Option<&'static str> {
        match self {
            Self::Original => None,
            Self::H264 => Some("h264"),
            Self::Hevc => Some("hevc"),
            Self::Av1 => Some("av1"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CustomScaling {
    FitPad,
    Stretch,
}

impl CustomScaling {
    pub const OPTIONS: [Self; 2] = [Self::FitPad, Self::Stretch];

    pub fn label(self) -> &'static str {
        match self {
            Self::FitPad => "Fit & pad",
            Self::Stretch => "Stretch",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CustomResolution {
    pub width: u64,
    pub height: u64,
    pub scaling: CustomScaling,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoResolution {
    Original,
    P2160,
    P1440,
    P1080,
    P960,
    P720,
    P480,
    Custom(CustomResolution),
}

impl VideoResolution {
    pub const PRESETS: [Self; 6] = [
        Self::P2160,
        Self::P1440,
        Self::P1080,
        Self::P960,
        Self::P720,
        Self::P480,
    ];

    pub fn dimensions(self) -> Option<(u64, u64)> {
        match self {
            Self::Original => None,
            Self::P2160 => Some((3840, 2160)),
            Self::P1440 => Some((2560, 1440)),
            Self::P1080 => Some((1920, 1080)),
            Self::P960 => Some((1920, 960)),
            Self::P720 => Some((1280, 720)),
            Self::P480 => Some((854, 480)),
            Self::Custom(custom) => Some((custom.width, custom.height)),
        }
    }

    pub fn label(self) -> String {
        match self {
            Self::Original => "Original".to_string(),
            Self::Custom(custom) => format!(
                "Custom ({}×{} · {})",
                custom.width,
                custom.height,
                custom.scaling.label()
            ),
            preset => {
                let (width, height) = preset.dimensions().unwrap();
                let aspect_ratio = if preset == Self::P960 { "2:1" } else { "16:9" };
                format!("{width}×{height} / {aspect_ratio}")
            }
        }
    }
}

/// How a player should turn a picture before drawing it, stored as a display matrix
/// beside the stream rather than baked into the pixels.
///
/// Only the four right angles. ffmpeg accepts any angle and the matrix stores it, but
/// `ffprobe` reports the angle as a truncated integer (30° reads back as 29, 359° as 0),
/// so reel could not verify what it wrote; and the matrix rotates a frame inside its own
/// declared size, which has no sensible meaning off the right angles — renderers that
/// implement it at all disagree about what to do with the corners.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum VideoRotation {
    #[default]
    None,
    Cw90,
    Cw180,
    Cw270,
}

impl VideoRotation {
    pub const ALL: [Self; 4] = [Self::None, Self::Cw90, Self::Cw180, Self::Cw270];

    pub fn degrees(self) -> u32 {
        match self {
            Self::None => 0,
            Self::Cw90 => 90,
            Self::Cw180 => 180,
            Self::Cw270 => 270,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Cw90 => "90° clockwise",
            Self::Cw180 => "180°",
            Self::Cw270 => "270° clockwise",
        }
    }

    /// The angle ffmpeg wrote, read back. It normalizes on write — 270 comes back as
    /// `-90` and 180 as `-180` — so anything but a multiple of 360 apart from a right
    /// angle is a file reel did not write, and reads as unrotated.
    pub fn from_degrees(degrees: i64) -> Self {
        match degrees.rem_euclid(360) {
            90 => Self::Cw90,
            180 => Self::Cw180,
            270 => Self::Cw270,
            _ => Self::None,
        }
    }

    /// Whether the picture's width and height swap when this rotation is applied.
    pub fn swaps_dimensions(self) -> bool {
        matches!(self, Self::Cw90 | Self::Cw270)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoMetadata {
    pub language: String,
    pub title: Option<String>,
    /// The one role a picture track can hold: a picture-in-picture commentary angle.
    /// The language and accessibility roles audio offers describe a soundtrack, not a
    /// picture, so video does not carry them.
    pub commentary: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoSettings {
    pub codec: VideoCodec,
    pub resolution: VideoResolution,
    pub rotation: VideoRotation,
    pub metadata: VideoMetadata,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SaveDestination {
    #[default]
    ReplaceOriginal,
    // The batch flow always replaces in place — there's no UI path offering a
    // destination choice anymore, so this is only constructed directly by
    // `apply_edits`'s own tests exercising copy semantics.
    #[allow(dead_code)]
    CreateCopy,
}

impl Default for VideoSettings {
    fn default() -> Self {
        Self {
            codec: VideoCodec::Original,
            resolution: VideoResolution::Original,
            rotation: VideoRotation::None,
            metadata: VideoMetadata {
                language: "und".to_string(),
                title: None,
                commentary: false,
            },
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ContainerMetadata {
    pub title: Option<String>,
    pub comment: Option<String>,
    pub date: Option<String>,
    pub genre: Option<String>,
    pub artist: Option<String>,
}

impl ContainerMetadata {
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.comment.is_none()
            && self.date.is_none()
            && self.genre.is_none()
            && self.artist.is_none()
    }
}

#[derive(Clone, Debug)]
pub struct EditRequest {
    pub path: PathBuf,
    pub destination: SaveDestination,
    pub container: Option<ContainerFormat>,
    pub container_metadata: Option<ContainerMetadata>,
    pub stream_order: Vec<u64>,
    pub deleted_streams: BTreeSet<u64>,
    pub default_streams: BTreeSet<u64>,
    pub default_sidecars: BTreeSet<usize>,
    pub audio_settings: BTreeMap<u64, AudioSettings>,
    pub video_settings: BTreeMap<u64, VideoSettings>,
    pub subtitle_changes: Vec<SubtitleChange>,
    pub left_subtitle_order: Vec<TrackRef>,
    pub sidecars: Vec<SidecarEntry>,
    pub cancelled: Arc<AtomicBool>,
}

#[derive(Clone, Debug)]
pub enum EditEvent {
    /// Carries `path` because, once more than one file can be in flight at once
    /// (across the transcode/remux worker pools), the UI needs to know which file a
    /// progress update belongs to rather than assuming there's only ever one —
    /// `App::apply_batch_event` routes each update to its `BatchItem` by this field.
    Progress {
        path: PathBuf,
        progress: EditProgress,
    },
    Finished {
        path: PathBuf,
        outcome: EditOutcome,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct EditProgress {
    pub phase: EditPhase,
    pub fraction: Option<f64>,
}

impl EditProgress {
    pub(crate) fn new(phase: EditPhase) -> Self {
        Self {
            phase,
            fraction: None,
        }
    }

    pub(crate) fn measured(phase: EditPhase, fraction: f64) -> Self {
        Self {
            phase,
            fraction: Some(fraction.clamp(0.0, 1.0)),
        }
    }

    pub fn label(&self) -> String {
        self.phase.label()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EditPhase {
    InspectSource,
    ValidateChanges,
    CreateWorkspace,
    ExtractSubtitle(String),
    CopySubtitle(String),
    UpdateSubtitle(String),
    ConvertSubtitle {
        subject: String,
        target: String,
    },
    OcrSubtitle {
        subject: String,
        target: String,
        language: String,
    },
    ValidateSubtitle(String),
    /// Writing the subtitle edit page's rewritten cue text into a staged subtitle file. Its own
    /// phase rather than folded into `UpdateSubtitle`, because it is the one step of a save
    /// that can fail on something the reader did rather than on a tool: a cue edited here
    /// and then changed on disk elsewhere stops the save, and the phase is what names the
    /// step that stopped.
    RewriteCues(String),
    ResolveNames,
    WriteMedia(String),
    ValidateOutput,
    RecheckSources,
    PreservePermissions,
    PreparePublication,
    BackupFile(String),
    PublishFile(String),
    RemoveBackups,
    Rollback,
    StopTools,
    Cleanup,
    Finished,
}

impl EditPhase {
    pub fn label(&self) -> String {
        match self {
            Self::InspectSource => "Checking source".to_string(),
            Self::ValidateChanges => "Checking edits".to_string(),
            Self::CreateWorkspace => "Preparing files".to_string(),
            Self::ExtractSubtitle(subject) => format!("Extracting {}", compact_subject(subject)),
            Self::CopySubtitle(subject) => format!("Copying {}", compact_subject(subject)),
            Self::UpdateSubtitle(subject) => format!("Updating {}", compact_subject(subject)),
            Self::ConvertSubtitle { subject, target } => {
                format!("Converting {} → {target}", compact_subject(subject))
            }
            Self::OcrSubtitle {
                subject,
                target,
                language,
            } => format!(
                "Running OCR on {} → {target} ({language})",
                compact_subject(subject)
            ),
            Self::ValidateSubtitle(subject) => format!("Checking {}", compact_subject(subject)),
            Self::RewriteCues(subject) => format!("Rewriting cues in {}", compact_subject(subject)),
            Self::ResolveNames => "Choosing filenames".to_string(),
            Self::WriteMedia(operation) => operation.clone(),
            Self::ValidateOutput => "Checking output".to_string(),
            Self::RecheckSources => "Checking source files".to_string(),
            Self::PreservePermissions => "Preserving permissions".to_string(),
            Self::PreparePublication => "Preparing to save".to_string(),
            Self::BackupFile(name) => format!("Backing up {}", compact_subject(name)),
            Self::PublishFile(name) => format!("Saving {}", compact_subject(name)),
            Self::RemoveBackups => "Removing backups".to_string(),
            Self::Rollback => "Restoring original files".to_string(),
            Self::StopTools => "Stopping tools".to_string(),
            Self::Cleanup => "Cleaning up".to_string(),
            Self::Finished => "Done".to_string(),
        }
    }
}

fn compact_subject(value: &str) -> String {
    const MAX: usize = 24;
    if value.chars().count() <= MAX {
        return value.to_string();
    }
    let head = value.chars().take(15).collect::<String>();
    let tail = value
        .chars()
        .rev()
        .take(8)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("{head}…{tail}")
}

type ProgressReporter<'a> = dyn FnMut(EditProgress) + 'a;

#[derive(Clone, Debug)]
pub enum EditOutcome {
    Completed {
        output_path: PathBuf,
        // Distinguished a "Subtitle changes saved." vs "Media edits saved." notice in
        // the old single-file flow; the batch flow's completion notice doesn't surface
        // this distinction per-item. Kept since `apply_edits` already computes it
        // correctly and it may be worth resurfacing later.
        #[allow(dead_code)]
        media_changed: bool,
    },
    Cancelled,
    SourceChanged(String),
    Failed(String),
}

/// Generalizes the per-stream `requires_transcode` check to a whole edit plan: does
/// applying `video_settings` to any surviving video stream in `stream_order` actually
/// require a real encode (`run_ffmpeg` runs this same check internally per-stream,
/// edit.rs `run_ffmpeg`), as opposed to every stream being handled by `-c copy`. Used
/// to route a staged file's `EditRequest` to the transcode pool (CPU-bound, kept to a
/// low worker count to avoid several encodes contending for cores) or the remux pool
/// (I/O-bound, safe to run with much higher concurrency) — see
/// `spawn_edit_worker_pools`.
pub(crate) fn plan_requires_transcode(
    info: &MediaInfo,
    stream_order: &[u64],
    audio_settings: &BTreeMap<u64, AudioSettings>,
    video_settings: &BTreeMap<u64, VideoSettings>,
) -> bool {
    stream_order.iter().any(|source_index| {
        info.streams
            .iter()
            .find(|stream| stream_index(stream) == Some(*source_index))
            .is_some_and(|stream| {
                (stream_kind(stream) == Some("video")
                    && video_settings
                        .get(source_index)
                        .is_some_and(|settings| requires_transcode(stream, settings)))
                    || (stream_kind(stream) == Some("audio")
                        && audio_settings
                            .get(source_index)
                            .is_some_and(|settings| audio_requires_transcode(stream, settings)))
            })
    })
}

/// Appends one line to `~/.cache/reel-tui/edit_errors.log` for every edit that fails
/// or gets abandoned because the source changed underneath it. The batch UI only ever
/// shows a terse one-line notice for these (and it's gone the moment the next notice
/// replaces it), so this is the only place the actual reason — plus enough of the
/// request to tell what was being attempted — survives past that.
fn log_edit_failure(request: &EditRequest, kind: &str, message: &str) {
    let Some(dir) = crate::cache::DiskCache::cache_dir() else {
        return;
    };
    append_edit_failure_log(&dir.join("edit_errors.log"), request, kind, message);
}

/// Does the actual writing for `log_edit_failure`, split out so tests can point it at
/// a temp file instead of the real `~/.cache/reel-tui` directory. Best-effort: unable
/// to write the log is never itself treated as an error.
fn append_edit_failure_log(log_path: &Path, request: &EditRequest, kind: &str, message: &str) {
    if let Some(parent) = log_path.parent()
        && fs::create_dir_all(parent).is_err()
    {
        return;
    }
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let container = request
        .container
        .map(ContainerFormat::label)
        .unwrap_or("unchanged");
    let subtitle_changes = request
        .subtitle_changes
        .iter()
        .map(|change| {
            let source = match &change.source {
                SubtitleSource::Embedded(index) => format!("#{index}"),
                SubtitleSource::Sidecar(path) => path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("sidecar")
                    .to_string(),
            };
            format!(
                "{source}(embedded_target={:?}, export_target={:?}, import={}, metadata={})",
                change.embedded_target,
                change.export_target,
                change.import_into_media,
                change.metadata.is_some(),
            )
        })
        .collect::<Vec<_>>();
    let line = format!(
        "[{timestamp}] {kind}: {} (destination: {:?}, container: {container}, \
         stream_order: {:?}, deleted_streams: {:?}, default_streams: {:?}, \
         audio_settings: {}, video_settings: {}, subtitle_changes: [{}]) — {message}\n",
        request.path.display(),
        request.destination,
        request.stream_order,
        request.deleted_streams,
        request.default_streams,
        request.audio_settings.len(),
        request.video_settings.len(),
        subtitle_changes.join(", "),
    );
    if let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
    {
        let _ = file.write_all(line.as_bytes());
    }
}

/// The text of a caught panic, for the failure the user sees and the entry
/// `log_edit_failure` writes. `Box<dyn Any>` carries the payload `panic!` was given,
/// which is a `&str` for a literal and a `String` once it has been formatted.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "an unknown internal error".to_string())
}

fn run_edit_worker_pool(
    request_rx: Arc<Mutex<Receiver<EditRequest>>>,
    result_tx: Sender<EditEvent>,
    worker_count: usize,
) {
    for _ in 0..worker_count.max(1) {
        let request_rx = Arc::clone(&request_rx);
        let result_tx = result_tx.clone();
        std::thread::spawn(move || {
            loop {
                let request = {
                    let Ok(receiver) = request_rx.lock() else {
                        break;
                    };
                    receiver.recv()
                };
                let Ok(request) = request else {
                    break;
                };
                let progress_tx = result_tx.clone();
                let progress_path = request.path.clone();
                // A panic here — a broken invariant in the plan, a `.expect` in the
                // ffmpeg command builder — would otherwise unwind the whole worker,
                // poisoning `request_rx` so every *other* worker in this pool breaks out
                // of its loop too, and leaving the batch waiting on an `EditEvent` that
                // can never arrive. One request failing is recoverable; the pool silently
                // dying while the UI reports "processing" is not.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    apply_edits(
                        EditTarget {
                            source: &request.path,
                            destination: request.destination,
                            container: request.container,
                            container_metadata: request.container_metadata.as_ref(),
                        },
                        TrackEdits {
                            stream_order: &request.stream_order,
                            deleted_streams: &request.deleted_streams,
                            default_streams: &request.default_streams,
                            default_sidecars: &request.default_sidecars,
                            audio_settings: &request.audio_settings,
                            video_settings: &request.video_settings,
                            subtitle_changes: &request.subtitle_changes,
                            left_subtitle_order: &request.left_subtitle_order,
                            sidecars: &request.sidecars,
                        },
                        &request.cancelled,
                        |progress| {
                            let _ = progress_tx.send(EditEvent::Progress {
                                path: progress_path.clone(),
                                progress,
                            });
                        },
                    )
                }))
                .unwrap_or_else(|payload| {
                    Err(EditError::Failed(format!(
                        "The media edit failed unexpectedly: {}. The original file was not \
                         replaced.",
                        panic_message(&payload)
                    )))
                });
                let outcome = match result {
                    Ok(result) => EditOutcome::Completed {
                        output_path: result.output_path,
                        media_changed: result.media_changed,
                    },
                    Err(EditError::Cancelled) => EditOutcome::Cancelled,
                    Err(EditError::SourceChanged(error)) => {
                        log_edit_failure(&request, "SourceChanged", &error);
                        EditOutcome::SourceChanged(error)
                    }
                    Err(EditError::Failed(error)) => {
                        log_edit_failure(&request, "Failed", &error);
                        EditOutcome::Failed(error)
                    }
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
    }
}

/// Two independent worker pools sharing one result channel: a small (typically 1)
/// `transcode` pool for CPU-bound re-encodes, and a larger `remux` pool for cheap,
/// I/O-bound `-c copy` operations — see `plan_requires_transcode` for how a request is
/// routed to one or the other, and the "Parallelism analysis" section of the
/// multi-file-staging design for why they're split rather than sharing one limit.
pub fn spawn_edit_worker_pools(
    transcode_workers: usize,
    remux_workers: usize,
) -> (
    Sender<EditRequest>,
    Sender<EditRequest>,
    Receiver<EditEvent>,
) {
    let (transcode_tx, transcode_rx) = mpsc::channel::<EditRequest>();
    let (remux_tx, remux_rx) = mpsc::channel::<EditRequest>();
    let (result_tx, result_rx) = mpsc::channel();

    run_edit_worker_pool(
        Arc::new(Mutex::new(transcode_rx)),
        result_tx.clone(),
        transcode_workers,
    );
    run_edit_worker_pool(Arc::new(Mutex::new(remux_rx)), result_tx, remux_workers);

    (transcode_tx, remux_tx, result_rx)
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

/// Renders a set of stream indices for the "tracks changed" diagnostic — `"none"`
/// when empty (the common, unremarkable case for one side of the diff), the raw
/// indices otherwise, so a mismatch actually says *which* tracks disagree rather than
/// just that something, somewhere, no longer lines up.
fn describe_index_diff(indices: &[&u64]) -> String {
    if indices.is_empty() {
        "none".to_string()
    } else {
        format!("track(s) {indices:?}")
    }
}

pub(crate) fn validate_edit(
    info: &MediaInfo,
    stream_order: &[u64],
    deleted_streams: &BTreeSet<u64>,
    default_streams: &BTreeSet<u64>,
    audio_settings: &BTreeMap<u64, AudioSettings>,
    video_settings: &BTreeMap<u64, VideoSettings>,
) -> Result<(), String> {
    let available: BTreeSet<_> = info.streams.iter().filter_map(stream_index).collect();
    if available.len() != info.streams.len() {
        return Err("One or more tracks have no usable stream index.".to_string());
    }
    let ordered: BTreeSet<_> = stream_order.iter().copied().collect();
    if ordered.len() != stream_order.len() {
        return Err(
            "The file's tracks changed: the same track appears twice in the staged order. \
             Reopen it and try again."
                .to_string(),
        );
    }
    if !ordered.is_disjoint(deleted_streams) {
        let both: Vec<_> = ordered.intersection(deleted_streams).collect();
        return Err(format!(
            "The file's tracks changed: track(s) {both:?} are both kept and marked for \
             deletion. Reopen it and try again."
        ));
    }
    let staged: BTreeSet<_> = ordered.union(deleted_streams).copied().collect();
    if staged != available {
        let missing: Vec<_> = available.difference(&staged).collect();
        let extra: Vec<_> = staged.difference(&available).collect();
        return Err(format!(
            "The file's tracks changed: {} in the file but not in the staged edit, {} in the \
             staged edit but no longer in the file. Reopen it and try again.",
            describe_index_diff(&missing),
            describe_index_diff(&extra),
        ));
    }
    if !default_streams.is_subset(&ordered) {
        return Err("A default track is also marked for deletion.".to_string());
    }
    if !audio_settings.keys().all(|index| ordered.contains(index)) {
        return Err("Audio settings refer to a missing or deleted track.".to_string());
    }
    for (index, settings) in audio_settings {
        let stream = info
            .streams
            .iter()
            .find(|stream| stream_index(stream) == Some(*index))
            .expect("validated stream order contains every audio-settings index");
        if stream_kind(stream) != Some("audio") {
            return Err("Audio settings can only be applied to audio tracks.".to_string());
        }
        if !audio_requires_transcode(stream, settings) {
            continue;
        }
        let source_channels = stream_channels(stream).ok_or_else(|| {
            format!("The channel layout for audio track #{index} is unavailable.")
        })?;
        let channels = settings
            .channel_layout
            .channels()
            .unwrap_or(source_channels);
        if channels > source_channels {
            return Err(CHANNEL_UPMIX_NOT_IMPLEMENTED.to_string());
        }
        let codec = effective_audio_codec(stream, settings).ok_or_else(|| {
            format!(
                "Can't encode the original {} codec; choose a supported audio codec.",
                source_codec(stream)
                    .unwrap_or("unknown")
                    .to_ascii_uppercase()
            )
        })?;
        if !codec.supports_channels(channels) {
            return Err(format!(
                "{} does not support {}-channel audio; choose another layout or codec.",
                codec.label(),
                channels
            ));
        }
        let source_rate = stream_sample_rate(stream)
            .ok_or_else(|| format!("The sample rate for audio track #{index} is unavailable."))?;
        resolved_audio_sample_rate(codec, source_rate).ok_or_else(|| {
            format!(
                "{} cannot use the source sample rate; choose another codec.",
                codec.label()
            )
        })?;
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
        let source_width = stream_dimension(stream, "width");
        let source_height = stream_dimension(stream, "height");
        match settings.resolution {
            VideoResolution::Custom(custom) => {
                if custom.width == 0
                    || custom.height == 0
                    || custom.width % 2 != 0
                    || custom.height % 2 != 0
                {
                    return Err(
                        "Custom width and height must be positive even numbers.".to_string()
                    );
                }
                if custom.width < MINIMUM_CUSTOM_DIMENSION
                    || custom.height < MINIMUM_CUSTOM_DIMENSION
                {
                    return Err(format!(
                        "Custom width and height must be at least {MINIMUM_CUSTOM_DIMENSION} \
                         pixels."
                    ));
                }
                let Some((source_width, source_height)) = source_width.zip(source_height) else {
                    return Err(
                        "The source resolution is unavailable; custom scaling cannot be applied."
                            .to_string(),
                    );
                };
                if custom.width > source_width || custom.height > source_height {
                    return Err("Upscaling isn't possible yet.".to_string());
                }
            }
            VideoResolution::Original => {}
            resolution => {
                if resolution
                    .dimensions()
                    .zip(source_width.zip(source_height))
                    .is_some_and(
                        |((target_width, target_height), (source_width, source_height))| {
                            target_width > source_width || target_height > source_height
                        },
                    )
                {
                    return Err(
                        "The selected resolution must be lower than the original.".to_string()
                    );
                }
            }
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

pub(crate) fn container_conflicts(
    info: &MediaInfo,
    stream_order: &[u64],
    audio_settings: &BTreeMap<u64, AudioSettings>,
    video_settings: &BTreeMap<u64, VideoSettings>,
    subtitle_changes: &[SubtitleChange],
    target: ContainerFormat,
) -> Vec<String> {
    container_conflict_entries(
        info,
        stream_order,
        audio_settings,
        video_settings,
        subtitle_changes,
        target,
    )
    .into_iter()
    .map(|(_, message)| message)
    .collect()
}

pub(crate) fn container_conflict_streams(
    info: &MediaInfo,
    stream_order: &[u64],
    audio_settings: &BTreeMap<u64, AudioSettings>,
    video_settings: &BTreeMap<u64, VideoSettings>,
    subtitle_changes: &[SubtitleChange],
    target: ContainerFormat,
) -> BTreeSet<u64> {
    container_conflict_entries(
        info,
        stream_order,
        audio_settings,
        video_settings,
        subtitle_changes,
        target,
    )
    .into_iter()
    .map(|(index, _)| index)
    .collect()
}

pub(crate) fn imported_subtitle_conflicts(
    changes: &[SubtitleChange],
    sidecars: &[SidecarEntry],
    target: ContainerFormat,
) -> Vec<String> {
    changes
        .iter()
        .filter(|change| change.import_into_media)
        .filter_map(|change| {
            let SubtitleSource::Sidecar(path) = &change.source else {
                return None;
            };
            let sidecar = sidecars.iter().find(|sidecar| &sidecar.path == path)?;
            let format = change.embedded_target.unwrap_or(change.source_format);
            if target.supports_codec("subtitle", format.ffmpeg_codec(), false) {
                return None;
            }
            let targets = SubtitleFormat::COMMON_TARGETS
                .into_iter()
                .filter(|candidate| {
                    target.supports_codec("subtitle", candidate.ffmpeg_codec(), false)
                })
                .map(SubtitleFormat::label)
                .collect::<Vec<_>>();
            let resolution = if targets.is_empty() {
                "Choose another container.".to_string()
            } else {
                format!("Convert it to {}.", targets.join(" or "))
            };
            Some(format!(
                "{} can't import {} subtitle {}. {resolution}",
                target.label(),
                format.label(),
                sidecar.display_name
            ))
        })
        .collect()
}

pub(crate) fn subtitle_metadata_conflicts(
    info: &MediaInfo,
    changes: &[SubtitleChange],
    sidecars: &[SidecarEntry],
    target: ContainerFormat,
    include_unchanged_embedded: bool,
) -> Vec<String> {
    let mut conflicts = Vec::new();
    for stream in info
        .streams
        .iter()
        .filter(|stream| stream_kind(stream) == Some("subtitle"))
    {
        let Some(index) = stream_index(stream) else {
            continue;
        };
        let change = changes
            .iter()
            .find(|change| change.source == SubtitleSource::Embedded(index));
        if change.is_some_and(SubtitleChange::removes_from_media) {
            continue;
        }
        if !include_unchanged_embedded && change.is_none_or(|change| change.metadata.is_none()) {
            continue;
        }
        let mut metadata = change
            .map(|change| effective_subtitle_metadata(change, stream_metadata(stream)))
            .unwrap_or_else(|| stream_metadata(stream));
        target.retain_supported_subtitle_metadata(&mut metadata);
        conflicts.extend(subtitle_flag_conflicts(
            &metadata,
            target,
            &format!("subtitle track #{index}"),
        ));
    }
    for change in changes.iter().filter(|change| change.import_into_media) {
        let SubtitleSource::Sidecar(path) = &change.source else {
            continue;
        };
        let Some(sidecar) = sidecars.iter().find(|sidecar| &sidecar.path == path) else {
            continue;
        };
        let mut metadata = effective_subtitle_metadata(
            change,
            SubtitleMetadata {
                language: sidecar.language.clone(),
                title: None,
                forced: sidecar.forced,
                cc: false,
                hearing_impaired: sidecar.hearing_impaired,
                original: false,
                commentary: false,
            },
        );
        target.retain_supported_subtitle_metadata(&mut metadata);
        conflicts.extend(subtitle_flag_conflicts(
            &metadata,
            target,
            &format!("subtitle {}", sidecar.display_name),
        ));
    }
    conflicts
}

fn subtitle_flag_conflicts(
    metadata: &SubtitleMetadata,
    target: ContainerFormat,
    track: &str,
) -> Vec<String> {
    SubtitleFlag::ALL
        .into_iter()
        .filter(|flag| metadata.get_flag(*flag) && !target.supports_subtitle_flag(*flag))
        .map(|flag| {
            format!(
                "{} can't store the {} flag on {track}. Clear it or choose another container.",
                target.label(),
                flag.label()
            )
        })
        .collect()
}

fn container_conflict_entries(
    info: &MediaInfo,
    stream_order: &[u64],
    audio_settings: &BTreeMap<u64, AudioSettings>,
    video_settings: &BTreeMap<u64, VideoSettings>,
    subtitle_changes: &[SubtitleChange],
    target: ContainerFormat,
) -> Vec<(u64, String)> {
    let mut conflicts = Vec::new();
    for index in stream_order {
        if subtitle_changes.iter().any(|change| {
            change.source == SubtitleSource::Embedded(*index) && change.removes_from_media()
        }) {
            continue;
        }
        let Some(stream) = info
            .streams
            .iter()
            .find(|stream| stream_index(stream) == Some(*index))
        else {
            continue;
        };
        // The chapter track is never mapped into the output (`output_track_plan`), so
        // there is nothing for the target container to refuse. Left in, an ordinary MP4
        // with chapters reported a conflict against the container it was already in.
        if is_chapter_track(info, stream) {
            continue;
        }
        let kind = stream_kind(stream).unwrap_or("other");
        let source_codec = stream
            .get("codec_name")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let codec = if kind == "video" && !is_attached_picture(stream) {
            video_settings
                .get(index)
                .and_then(|settings| settings.codec.codec_name())
                .unwrap_or(source_codec)
        } else if kind == "audio" {
            audio_settings
                .get(index)
                .and_then(|settings| settings.codec.codec_name())
                .unwrap_or(source_codec)
        } else if kind == "subtitle" {
            subtitle_changes
                .iter()
                .find_map(|change| match change.source {
                    SubtitleSource::Embedded(source_index) if source_index == *index => {
                        change.embedded_target.map(SubtitleFormat::ffmpeg_codec)
                    }
                    _ => None,
                })
                .unwrap_or(source_codec)
        } else {
            source_codec
        };
        // Audio metadata deliberately raises no conflict. Whatever the target container
        // cannot store is dropped silently by `retain_supported_audio_metadata` on the
        // way out, exactly as subtitle flags are, and `App::audio_field_visible` hides
        // those fields from the dialog in the first place — so there is nothing for the
        // user to act on and no conflict to report.
        if !target.supports_codec(kind, codec, is_attached_picture(stream)) {
            conflicts.push((
                *index,
                container_conflict_message(target, *index, kind, codec),
            ));
        }
    }
    conflicts
}

fn container_conflict_message(
    target: ContainerFormat,
    index: u64,
    kind: &str,
    codec: &str,
) -> String {
    let source_codec = codec;
    let codec = codec.to_ascii_uppercase();
    let resolution = match kind {
        "video" => {
            let targets = VideoCodec::TARGETS
                .into_iter()
                .filter_map(|candidate| {
                    let codec = candidate.codec_name()?;
                    target
                        .supports_codec("video", codec, false)
                        .then_some(candidate.label())
                })
                .collect::<Vec<_>>();
            if targets.is_empty() {
                "Choose another container or remove the track.".to_string()
            } else {
                format!("Encode it as {} or remove the track.", targets.join(" or "))
            }
        }
        "subtitle" => {
            let targets = SubtitleFormat::COMMON_TARGETS
                .into_iter()
                .filter(|candidate| {
                    target.supports_codec("subtitle", candidate.ffmpeg_codec(), false)
                })
                .map(SubtitleFormat::label)
                .collect::<Vec<_>>();
            if targets.is_empty() {
                "Choose another container or remove the track.".to_string()
            } else {
                format!(
                    "Convert it to {} or remove the track.",
                    targets.join(" or ")
                )
            }
        }
        "audio" => {
            let targets = AudioCodec::TARGETS
                .into_iter()
                .filter_map(|candidate| {
                    let codec = candidate.codec_name()?;
                    target
                        .supports_codec("audio", codec, false)
                        .then_some(candidate.label())
                })
                .collect::<Vec<_>>();
            format!("Encode it as {} or remove the track.", targets.join(" or "))
        }
        // `data` and `attachment` streams have no row of their own: nothing to convert
        // and nothing to delete, so the only resolution left is a container that takes
        // them. Naming which one matters, because the fixed "Choose MKV" this replaced
        // was advice given to a reader who had *already* chosen MKV — MKV is the
        // container that takes attachments and refuses data streams, and MP4 is the one
        // that does the reverse, so a single suggestion is exactly backwards half the
        // time.
        _ => {
            let targets = ContainerFormat::TARGETS
                .into_iter()
                .filter(|candidate| {
                    *candidate != target && candidate.supports_codec(kind, source_codec, false)
                })
                .map(ContainerFormat::label)
                .collect::<Vec<_>>();
            if targets.is_empty() {
                "No available container can store it.".to_string()
            } else {
                format!("Choose {} instead.", targets.join(" or "))
            }
        }
    };
    format!(
        "{} can't contain {codec} {kind} track #{index}. {resolution}",
        target.label()
    )
}

fn validate_subtitle_sources(
    info: &MediaInfo,
    changes: &[SubtitleChange],
    sidecars: &[SidecarEntry],
) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for change in changes {
        if !change.has_effect() {
            continue;
        }
        if !seen.insert(change.source.clone()) {
            return Err("A subtitle source has more than one pending conversion.".to_string());
        }
        match &change.source {
            SubtitleSource::Embedded(index) => {
                let stream = info
                    .streams
                    .iter()
                    .find(|stream| stream_index(stream) == Some(*index))
                    .ok_or_else(|| {
                        "An embedded subtitle track changed. Reopen the file and try again."
                            .to_string()
                    })?;
                if stream_kind(stream) != Some("subtitle")
                    || stream
                        .get("codec_name")
                        .and_then(Value::as_str)
                        .and_then(SubtitleFormat::from_codec)
                        != Some(change.source_format)
                {
                    return Err(
                        "An embedded subtitle track changed. Reopen the file and try again."
                            .to_string(),
                    );
                }
            }
            SubtitleSource::Sidecar(path) => {
                let sidecar = sidecars
                    .iter()
                    .find(|sidecar| &sidecar.path == path)
                    .ok_or_else(|| "A subtitle sidecar is no longer available.".to_string())?;
                if sidecar.format != change.source_format
                    || FileFingerprint::for_path(path).ok() != Some(sidecar.fingerprint)
                    || sidecar.companion.as_ref().is_some_and(|companion| {
                        FileFingerprint::for_path(companion).ok() != sidecar.companion_fingerprint
                    })
                {
                    return Err(
                        "A subtitle sidecar changed; reload it before converting.".to_string()
                    );
                }
            }
        }
        if change.needs_ocr() && change.ocr_language.as_deref().unwrap_or("").is_empty() {
            return Err("Choose a Tesseract language for OCR.".to_string());
        }
    }
    Ok(())
}

fn validate_subtitle_languages(
    info: &MediaInfo,
    changes: &[SubtitleChange],
    sidecars: &[SidecarEntry],
    deleted_streams: &BTreeSet<u64>,
) -> Result<(), String> {
    for stream in info
        .streams
        .iter()
        .filter(|stream| stream_kind(stream) == Some("subtitle"))
    {
        let Some(index) = stream_index(stream) else {
            continue;
        };
        if deleted_streams.contains(&index) {
            continue;
        }
        let metadata = changes
            .iter()
            .find(|change| change.source == SubtitleSource::Embedded(index))
            .map(|change| effective_subtitle_metadata(change, stream_metadata(stream)))
            .unwrap_or_else(|| stream_metadata(stream));
        if language_choice(&metadata.language).is_none() {
            return Err(format!(
                "Choose a language for subtitle track #{index}; Undetermined is not allowed."
            ));
        }
    }
    for sidecar in sidecars {
        let original = SubtitleMetadata {
            language: sidecar.language.clone(),
            title: None,
            forced: sidecar.forced,
            cc: false,
            hearing_impaired: sidecar.hearing_impaired,
            original: false,
            commentary: false,
        };
        let metadata = changes
            .iter()
            .find(|change| change.source == SubtitleSource::Sidecar(sidecar.path.clone()))
            .map_or(original.clone(), |change| {
                effective_subtitle_metadata(change, original)
            });
        if language_choice(&metadata.language).is_none() {
            return Err(format!(
                "Choose a language for {}; Undetermined is not allowed.",
                sidecar.display_name
            ));
        }
    }
    Ok(())
}

/// Whether a `validate_edit`/`validate_subtitle_sources`/`validate_subtitle_languages`
/// failure means the staged edit is *stale* — the file's actual tracks/subtitles no
/// longer match what was staged (typically because the probe the UI staged the edit
/// against is out of date relative to the file, e.g. `App::staged_edits`'s staleness
/// check missing a change, or the disk cache serving an outdated probe) — rather than
/// the edit itself being invalid in a way the user needs to fix by choosing different
/// settings. Routed to `EditError::SourceChanged` instead of `EditError::Failed` so
/// the caller discards the stale staged edit (mirroring what already happens for a
/// genuinely-removed source file), instead of leaving the user to retry the exact
/// same broken request forever — every one of these messages literally says "reopen
/// it and try again," which is precisely what discarding the stale staged edit forces.
/// A message-based check, not a typed error, is a deliberate trade-off against a
/// wider refactor of those three validation functions; each pattern here matches a
/// small, fixed set of literals used at one or two call sites within them.
fn classify_edit_error(message: String) -> EditError {
    let is_stale = message.contains("tracks changed")
        || message.contains("track changed")
        || message.contains("sidecar changed")
        || message.contains("sidecar is no longer available")
        || message.contains("refer to a missing or deleted track");
    if is_stale {
        EditError::SourceChanged(message)
    } else {
        EditError::Failed(message)
    }
}

fn apply_edits(
    target: EditTarget<'_>,
    edits: TrackEdits<'_>,
    cancelled: &AtomicBool,
    mut report_progress: impl FnMut(EditProgress),
) -> Result<EditResult, EditError> {
    let TrackEdits {
        stream_order,
        left_subtitle_order,
        deleted_streams,
        default_streams,
        default_sidecars,
        audio_settings,
        video_settings,
        subtitle_changes,
        sidecars,
    } = edits;
    let mut workspace_cleanup = DirectoryCleanup(None);
    let mut media_cleanup = TempCleanup(None);
    let result = (|| -> Result<EditResult, EditError> {
        let path = target.source;
        let destination = target.destination;
        let target_container = target.container;
        report_progress(EditProgress::new(EditPhase::InspectSource));
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
        report_progress(EditProgress::new(EditPhase::ValidateChanges));
        validate_edit(
            &source_info,
            stream_order,
            deleted_streams,
            default_streams,
            audio_settings,
            video_settings,
        )
        .map_err(classify_edit_error)?;
        validate_subtitle_sources(&source_info, subtitle_changes, sidecars)
            .map_err(classify_edit_error)?;
        validate_subtitle_languages(&source_info, subtitle_changes, sidecars, deleted_streams)
            .map_err(classify_edit_error)?;
        let output_stream_order = stream_order
            .iter()
            .copied()
            .filter(|index| {
                !subtitle_changes.iter().any(|change| {
                    change.source == SubtitleSource::Embedded(*index) && change.removes_from_media()
                })
            })
            // The chapter track is not a track the output carries — `output_track_plan`
            // never maps it, and `-map_chapters 0` writes the chapters out on its own —
            // so counting it here would expect one more stream than was asked for.
            .filter(|index| {
                source_info
                    .streams
                    .iter()
                    .find(|stream| stream_index(stream) == Some(*index))
                    .is_none_or(|stream| !is_chapter_track(&source_info, stream))
            })
            .collect::<Vec<_>>();
        // Cross-checks the extension against ffprobe's own `format_name` (just
        // reprobed into `source_info` above) rather than trusting a possibly
        // misleading extension — a `.mkv` renamed to `.mp4` (or vice versa) is still
        // genuinely whatever it actually is, and both the compatibility checks below
        // and the muxer `run_ffmpeg` is told to use need to agree with reality, not
        // the filename.
        let detected_container = ContainerFormat::detect(
            path,
            source_info
                .format
                .get("format_name")
                .and_then(Value::as_str),
        );
        let effective_container = target_container.or(detected_container);
        if let Some(container) = effective_container {
            let mut conflicts = if target_container.is_some() {
                container_conflicts(
                    &source_info,
                    &output_stream_order,
                    audio_settings,
                    video_settings,
                    subtitle_changes,
                    container,
                )
            } else {
                Vec::new()
            };
            conflicts.extend(imported_subtitle_conflicts(
                subtitle_changes,
                sidecars,
                container,
            ));
            conflicts.extend(subtitle_metadata_conflicts(
                &source_info,
                subtitle_changes,
                sidecars,
                container,
                target_container.is_some(),
            ));
            if !conflicts.is_empty() {
                return Err(EditError::Failed(format!(
                    "The selected container is incompatible:\n{}",
                    conflicts.join("\n")
                )));
            }
        } else if subtitle_changes
            .iter()
            .any(|change| change.import_into_media)
        {
            return Err(EditError::Failed(
                "Choose a supported container before importing subtitles.".to_string(),
            ));
        }
        let duration = media_duration(&source_info);
        let container_changed =
            target_container.is_some_and(|container| detected_container != Some(container));
        let container_metadata_changed = target.container_metadata.is_some();
        let media_changed = media_changes_required(
            &source_info,
            &output_stream_order,
            deleted_streams,
            default_streams,
            audio_settings,
            video_settings,
            subtitle_changes,
            container_changed || container_metadata_changed,
        );
        if cancelled.load(Ordering::Relaxed) {
            return Err(EditError::Cancelled);
        }
        report_progress(EditProgress::new(EditPhase::CreateWorkspace));
        let workspace_path = temporary_workspace(path).map_err(EditError::Failed)?;
        fs::create_dir(&workspace_path).map_err(|error| {
            EditError::Failed(format!("Could not create subtitle workspace: {error}"))
        })?;
        workspace_cleanup.0 = Some(workspace_path.clone());
        let mut prepared = prepare_subtitle_changes(
            path,
            &source_info,
            subtitle_changes,
            sidecars,
            default_sidecars,
            &workspace_path,
            cancelled,
            &mut report_progress,
        )?;
        if let Some(container) = effective_container {
            for import in &mut prepared.imports {
                container.retain_supported_subtitle_metadata(&mut import.metadata);
            }
        }
        if !media_changed {
            report_progress(EditProgress::new(EditPhase::RecheckSources));
            validate_subtitle_sources(&source_info, subtitle_changes, sidecars).map_err(|_| {
                EditError::SourceChanged(
                    "A subtitle sidecar changed; no subtitle output was saved.".to_string(),
                )
            })?;
            match source_matches_fingerprint(path, source_fingerprint) {
                Ok(true) => {}
                Ok(false) => {
                    return Err(EditError::SourceChanged(
                        "Source file changed; no subtitle output was saved.".to_string(),
                    ));
                }
                Err(_) => {
                    return Err(EditError::SourceChanged(
                        "Source file was removed; no subtitle output was saved.".to_string(),
                    ));
                }
            }
            if cancelled.load(Ordering::Relaxed) {
                return Err(EditError::Cancelled);
            }
            let mut publications = prepared.publications;
            report_progress(EditProgress::new(EditPhase::ResolveNames));
            resolve_export_duplicates(&mut publications, &workspace_path)?;
            publish_transaction_with_progress(
                None,
                None,
                &publications,
                cancelled,
                &mut report_progress,
            )?;
            return Ok(EditResult {
                output_path: path.to_path_buf(),
                media_changed: false,
            });
        }

        let audio_encode = output_stream_order.iter().any(|index| {
            source_info
                .streams
                .iter()
                .find(|stream| stream_index(stream) == Some(*index))
                .zip(audio_settings.get(index))
                .is_some_and(|(stream, settings)| audio_requires_transcode(stream, settings))
        });
        let video_encode = output_stream_order.iter().any(|index| {
            source_info
                .streams
                .iter()
                .find(|stream| stream_index(stream) == Some(*index))
                .zip(video_settings.get(index))
                .is_some_and(|(stream, settings)| requires_transcode(stream, settings))
        });
        let media_operation = media_write_label(
            target_container,
            audio_encode,
            video_encode,
            container_metadata_changed,
        );

        let temporary = temporary_path(path, target_container).map_err(EditError::Failed)?;
        media_cleanup.0 = Some(temporary.clone());
        let output = run_ffmpeg(
            FfmpegPlan {
                source: path,
                temporary: &temporary,
                source_info: &source_info,
                stream_order: &output_stream_order,
                left_subtitle_order,
                default_streams,
                audio_settings,
                video_settings,
                replacements: &prepared.replacements,
                imports: &prepared.imports,
                subtitle_changes,
                sidecars,
                // Always the *real* muxer to write — `target_container` when the user
                // asked for a conversion, otherwise `detected_container` (not the
                // extension-only guess `run_ffmpeg` would otherwise fall back to) —
                // so ffmpeg is never left to infer the format from a filename that
                // might not match what's actually being written into it.
                container: effective_container,
                container_metadata: target.container_metadata,
                duration,
                cancelled,
            },
            EditPhase::WriteMedia(media_operation),
            &mut report_progress,
        )?;
        if !output.status.success() {
            return Err(EditError::Failed(command_error(
                "ffmpeg could not apply the track edits",
                &output.stderr,
            )));
        }
        report_progress(EditProgress::new(EditPhase::ValidateOutput));

        if cancelled.load(Ordering::Relaxed) {
            report_progress(EditProgress::new(EditPhase::Cleanup));
            return Err(EditError::Cancelled);
        }
        let output_info = media_info(&temporary).map_err(EditError::Failed)?;
        if let Some(container) = target_container {
            validate_output_container(&output_info, &temporary, container)
                .map_err(EditError::Failed)?;
        }
        let expected_count = output_stream_order.len() + prepared.imports.len();
        // The muxer's own chapter track is left out on both sides of the count, exactly
        // as `validate_result` leaves it out: the `mov` muxer writes one from
        // `-map_chapters 0` whether or not anything was mapped to it.
        let written_count = output_info
            .streams
            .iter()
            .filter(|stream| !is_chapter_track(&output_info, stream))
            .count();
        if written_count != expected_count {
            return Err(EditError::Failed(format!(
                "The remuxed file has {written_count} tracks; expected {expected_count}.",
            )));
        }
        validate_result(
            &source_info,
            &output_info,
            &output_stream_order,
            left_subtitle_order,
            default_streams,
            audio_settings,
            video_settings,
            &prepared.replacements,
            &prepared.imports,
            subtitle_changes,
            sidecars,
            effective_container,
        )
        .map_err(EditError::Failed)?;
        report_progress(EditProgress::new(EditPhase::RecheckSources));
        validate_subtitle_sources(&source_info, subtitle_changes, sidecars).map_err(|_| {
            EditError::SourceChanged(
                "A subtitle sidecar changed; no media or subtitle output was saved.".to_string(),
            )
        })?;

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
        report_progress(EditProgress::new(EditPhase::PreservePermissions));
        fs::set_permissions(&temporary, source_permissions).map_err(|error| {
            EditError::Failed(format!("Could not preserve source permissions: {error}"))
        })?;
        if cancelled.load(Ordering::Relaxed) {
            return Err(EditError::Cancelled);
        }

        report_progress(EditProgress::new(EditPhase::PreparePublication));
        let output_path = match destination {
            SaveDestination::ReplaceOriginal => {
                let mut publications = prepared.publications.clone();
                report_progress(EditProgress::new(EditPhase::ResolveNames));
                resolve_export_duplicates(&mut publications, &workspace_path)?;
                let replacement = replacement_path(path, target_container)?;
                if replacement != path && replacement.exists() {
                    return Err(EditError::Failed(format!(
                        "{} already exists; choose Create a copy or rename it.",
                        replacement.display()
                    )));
                }
                publish_transaction_with_progress(
                    Some((&temporary, &replacement)),
                    Some(path),
                    &publications,
                    cancelled,
                    &mut report_progress,
                )?;
                media_cleanup.0 = None;
                replacement
            }
            SaveDestination::CreateCopy => {
                let copy = next_copy_path(path, target_container)?;
                let mut publications =
                    retarget_publications_for_copy(&prepared.publications, path, &copy)?;
                report_progress(EditProgress::new(EditPhase::ResolveNames));
                resolve_export_duplicates(&mut publications, &workspace_path)?;
                publish_transaction_with_progress(
                    Some((&temporary, &copy)),
                    None,
                    &publications,
                    cancelled,
                    &mut report_progress,
                )?;
                media_cleanup.0 = None;
                copy
            }
        };
        Ok(EditResult {
            output_path,
            media_changed: true,
        })
    })();
    if workspace_cleanup.0.is_some() || media_cleanup.0.is_some() {
        report_progress(EditProgress::new(EditPhase::Cleanup));
    }
    drop(media_cleanup);
    drop(workspace_cleanup);
    if result.is_ok() {
        report_progress(EditProgress::measured(EditPhase::Finished, 1.0));
    }
    result
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EditResult {
    output_path: PathBuf,
    media_changed: bool,
}

fn media_write_label(
    target_container: Option<ContainerFormat>,
    audio_encode: bool,
    video_encode: bool,
    container_metadata_changed: bool,
) -> String {
    let encode = match (video_encode, audio_encode) {
        (true, true) => Some("video and audio"),
        (true, false) => Some("video"),
        (false, true) => Some("audio"),
        (false, false) => None,
    };
    match (encode, target_container) {
        (Some(kind), Some(container)) => {
            format!("Encoding {kind} and remuxing to {}", container.label())
        }
        (Some(kind), None) => format!("Encoding {kind}"),
        (None, Some(container)) => format!("Remuxing to {}", container.label()),
        (None, None) if container_metadata_changed => "Updating container metadata".to_string(),
        (None, None) => "Remuxing media".to_string(),
    }
}

#[expect(clippy::too_many_arguments)]
fn media_changes_required(
    source_info: &MediaInfo,
    stream_order: &[u64],
    deleted_streams: &BTreeSet<u64>,
    default_streams: &BTreeSet<u64>,
    audio_settings: &BTreeMap<u64, AudioSettings>,
    video_settings: &BTreeMap<u64, VideoSettings>,
    subtitle_changes: &[SubtitleChange],
    container_changed: bool,
) -> bool {
    let source_order = source_info
        .streams
        .iter()
        .filter_map(stream_index)
        .collect::<Vec<_>>();
    let source_defaults = source_info
        .streams
        .iter()
        .filter(|stream| is_default(stream))
        .filter_map(stream_index)
        .collect::<BTreeSet<_>>();
    container_changed
        || source_order != stream_order
        || !deleted_streams.is_empty()
        || source_defaults != *default_streams
        || !audio_settings.is_empty()
        || !video_settings.is_empty()
        || subtitle_changes.iter().any(SubtitleChange::changes_media)
}

#[derive(Clone, Debug)]
struct SubtitleReplacement {
    source_index: u64,
    target: SubtitleFormat,
    path: PathBuf,
}

#[derive(Clone, Debug)]
struct SubtitleImport {
    source_path: PathBuf,
    target: SubtitleFormat,
    path: PathBuf,
    metadata: SubtitleMetadata,
    default: bool,
}

#[derive(Clone, Debug)]
struct Publication {
    staged: Vec<(PathBuf, PathBuf)>,
    remove: Vec<PathBuf>,
}

#[derive(Debug, Default)]
struct PreparedSubtitles {
    replacements: Vec<SubtitleReplacement>,
    imports: Vec<SubtitleImport>,
    publications: Vec<Publication>,
}

fn stream_metadata(stream: &BTreeMap<String, Value>) -> SubtitleMetadata {
    SubtitleMetadata {
        language: stream_language(stream),
        title: stream_title(stream),
        forced: stream_forced(stream),
        cc: stream_cc(stream),
        hearing_impaired: stream_hearing_impaired(stream),
        original: stream_original(stream),
        commentary: stream_commentary(stream),
    }
}

fn effective_subtitle_metadata(
    change: &SubtitleChange,
    original: SubtitleMetadata,
) -> SubtitleMetadata {
    let mut metadata = change.metadata.clone().unwrap_or(original);
    if let Some(language) = canonical_language_code(&metadata.language) {
        metadata.language = language;
    }
    metadata
}

#[expect(clippy::too_many_arguments)]
fn prepare_subtitle_changes(
    media_path: &Path,
    info: &MediaInfo,
    changes: &[SubtitleChange],
    sidecars: &[SidecarEntry],
    default_sidecars: &BTreeSet<usize>,
    workspace: &Path,
    cancelled: &AtomicBool,
    report_progress: &mut ProgressReporter<'_>,
) -> Result<PreparedSubtitles, EditError> {
    let mut prepared = PreparedSubtitles::default();
    let media_stem = media_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| EditError::Failed("The media filename is not valid UTF-8.".to_string()))?;
    let parent = media_path
        .parent()
        .ok_or_else(|| EditError::Failed("The media file has no parent directory.".to_string()))?;
    let resolution = primary_video_resolution(info).unwrap_or((1920, 1080));

    for (job, change) in changes
        .iter()
        .filter(|change| change.has_effect())
        .enumerate()
    {
        if cancelled.load(Ordering::Relaxed) {
            report_progress(EditProgress::new(EditPhase::Cleanup));
            return Err(EditError::Cancelled);
        }
        match &change.source {
            SubtitleSource::Embedded(index) => {
                let stream = info
                    .streams
                    .iter()
                    .find(|stream| stream_index(stream) == Some(*index))
                    .expect("subtitle sources are validated before preparation");
                let metadata = effective_subtitle_metadata(change, stream_metadata(stream));
                let mut replacement_artifact = None;
                // Cue edits reach an embedded track the only way anything does: the track
                // comes out of the container, is rewritten, and goes back in as a
                // replacement stream in the remux that was going to happen anyway. Copied
                // out rather than transcoded (`-c:s copy`), so a track this page could open
                // is a track this can rewrite.
                if !change.cue_edits.is_empty() {
                    let subject = format!("subtitle #{index}");
                    let extracted = workspace.join(format!("cues-{job}-in.srt"));
                    extract_subtitle(
                        ConversionInput::Embedded {
                            media: media_path,
                            index: *index,
                        },
                        &extracted,
                        SubtitleFormat::SubRip,
                        &subject,
                        cancelled,
                        report_progress,
                    )?;
                    let staged = workspace.join(format!("cues-{job}.srt"));
                    rewrite_cue_file(
                        &extracted,
                        &change.cue_edits,
                        &staged,
                        &subject,
                        report_progress,
                    )?;
                    report_progress(EditProgress::new(EditPhase::ValidateSubtitle(
                        subject.clone(),
                    )));
                    validate_subtitle_output(&staged, SubtitleFormat::SubRip)?;
                    prepared.replacements.push(SubtitleReplacement {
                        source_index: *index,
                        target: SubtitleFormat::SubRip,
                        path: staged.clone(),
                    });
                    replacement_artifact = Some((SubtitleFormat::SubRip, staged));
                }
                if let Some(target) = change
                    .embedded_target
                    .filter(|_| change.export_target.is_none())
                {
                    report_progress(EditProgress::new(subtitle_preparation_phase(
                        &format!("subtitle #{index}"),
                        change,
                        target,
                        SubtitlePreparation::Embed,
                    )));
                    let staged =
                        workspace.join(format!("embedded-{job}-{}.{}", index, target.extension()));
                    convert_subtitle(
                        ConversionInput::Embedded {
                            media: media_path,
                            index: *index,
                        },
                        change,
                        target,
                        &staged,
                        resolution,
                        &format!("subtitle #{index}"),
                        cancelled,
                        report_progress,
                    )?;
                    report_progress(EditProgress::new(EditPhase::ValidateSubtitle(format!(
                        "subtitle #{index}"
                    ))));
                    validate_subtitle_output(&staged, target)?;
                    replacement_artifact = Some((target, staged.clone()));
                    prepared.replacements.push(SubtitleReplacement {
                        source_index: *index,
                        target,
                        path: staged,
                    });
                }
                if let Some(target) = change.export_target {
                    report_progress(EditProgress::new(subtitle_preparation_phase(
                        &format!("subtitle #{index}"),
                        change,
                        target,
                        SubtitlePreparation::Export,
                    )));
                    let filename = sidecar_filename(
                        media_stem,
                        &metadata.language,
                        metadata.forced,
                        metadata.cc || metadata.hearing_impaired,
                        None,
                        target,
                    );
                    let staged = workspace.join(format!("export-{job}.{}", target.extension()));
                    if let Some((converted, converted_path)) = &replacement_artifact
                        && *converted == target
                    {
                        copy_subtitle_artifact(
                            converted_path,
                            &staged,
                            target,
                            &format!("subtitle #{index}"),
                            cancelled,
                            report_progress,
                        )?;
                    } else {
                        convert_subtitle(
                            ConversionInput::Embedded {
                                media: media_path,
                                index: *index,
                            },
                            change,
                            target,
                            &staged,
                            resolution,
                            &format!("subtitle #{index}"),
                            cancelled,
                            report_progress,
                        )?;
                    }
                    report_progress(EditProgress::new(EditPhase::ValidateSubtitle(format!(
                        "subtitle #{index}"
                    ))));
                    validate_subtitle_output(&staged, target)?;
                    prepared.publications.push(Publication {
                        staged: subtitle_artifact_pairs(&staged, &parent.join(filename), target)?,
                        remove: Vec::new(),
                    });
                }
            }
            SubtitleSource::Sidecar(path) => {
                let (sidecar_index, sidecar) = sidecars
                    .iter()
                    .enumerate()
                    .find(|(_, sidecar)| &sidecar.path == path)
                    .expect("subtitle sources are validated before preparation");
                let metadata = effective_subtitle_metadata(
                    change,
                    SubtitleMetadata {
                        language: sidecar.language.clone(),
                        title: None,
                        forced: sidecar.forced,
                        cc: false,
                        hearing_impaired: sidecar.hearing_impaired,
                        original: false,
                        commentary: false,
                    },
                );
                // Cue edits are applied to the file *first*, and everything after this
                // works from the rewritten copy: a track that is also being converted or
                // imported must carry the new words into whatever it becomes, and the only
                // way to guarantee that is for there to be one edited file that the rest of
                // the branch cannot tell from the original.
                let rewritten = if change.cue_edits.is_empty() {
                    None
                } else {
                    let staged = workspace.join(format!("cues-{job}.srt"));
                    rewrite_cue_file(
                        path,
                        &change.cue_edits,
                        &staged,
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("sidecar"),
                        report_progress,
                    )?;
                    Some(staged)
                };
                let path = rewritten.as_deref().unwrap_or(path.as_path());
                // Cue edits alone leave the file where it is and under the name it has.
                // The conversion path below derives a filename from the track's language
                // and flags, which is right when the format or the metadata changed and
                // wrong here — rewriting a line inside `clip.English.srt` must not also
                // rename it to `clip.eng.srt`.
                if rewritten.is_some()
                    && !change.import_into_media
                    && change.metadata.is_none()
                    && change
                        .embedded_target
                        .is_none_or(|target| target == change.source_format)
                {
                    // The original is listed as removed even though the new file lands on
                    // the same name: that is what makes this a *replacement* rather than
                    // an export. The transaction backs a removed file up before anything
                    // is published, so a failure half way puts the reader's sidecar back —
                    // and `resolve_export_duplicates`, which renumbers exports that would
                    // land on an existing name, leaves replacements alone.
                    prepared.publications.push(Publication {
                        staged: vec![(path.to_path_buf(), sidecar.path.clone())],
                        remove: sidecar.source_paths().cloned().collect(),
                    });
                    continue;
                }
                if change.import_into_media {
                    let target = change.embedded_target.unwrap_or(change.source_format);
                    let filename = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("sidecar");
                    report_progress(EditProgress::new(subtitle_preparation_phase(
                        filename,
                        change,
                        target,
                        SubtitlePreparation::Import,
                    )));
                    let staged = workspace.join(format!("import-{job}.{}", target.extension()));
                    if target == change.source_format {
                        copy_subtitle_artifact(
                            path,
                            &staged,
                            target,
                            filename,
                            cancelled,
                            report_progress,
                        )?;
                    } else {
                        convert_subtitle(
                            ConversionInput::File(path),
                            change,
                            target,
                            &staged,
                            resolution,
                            filename,
                            cancelled,
                            report_progress,
                        )?;
                    }
                    let input = subtitle_input_path(&staged, target);
                    report_progress(EditProgress::new(EditPhase::ValidateSubtitle(
                        filename.to_string(),
                    )));
                    validate_subtitle_output(&input, target)?;
                    prepared.imports.push(SubtitleImport {
                        source_path: sidecar.path.clone(),
                        target,
                        path: input,
                        metadata,
                        default: default_sidecars.contains(&sidecar_index),
                    });
                    prepared.publications.push(Publication {
                        staged: Vec::new(),
                        remove: sidecar.source_paths().cloned().collect(),
                    });
                    continue;
                }
                let target = change.embedded_target.unwrap_or(change.source_format);
                let filename = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("sidecar");
                report_progress(EditProgress::new(subtitle_preparation_phase(
                    filename,
                    change,
                    target,
                    SubtitlePreparation::Sidecar,
                )));
                let base_filename = sidecar_filename(
                    media_stem,
                    &metadata.language,
                    metadata.forced,
                    metadata.cc || metadata.hearing_impaired,
                    change
                        .embedded_target
                        .is_none()
                        .then_some(sidecar.number)
                        .flatten(),
                    target,
                );
                let destination = sidecar_conversion_destination(
                    &parent.join(base_filename),
                    sidecar,
                    target,
                    &prepared.publications,
                )?;
                let staged = workspace.join(format!("sidecar-{job}.{}", target.extension()));
                if target == change.source_format {
                    copy_subtitle_artifact(
                        path,
                        &staged,
                        target,
                        filename,
                        cancelled,
                        report_progress,
                    )?;
                } else {
                    convert_subtitle(
                        ConversionInput::File(path),
                        change,
                        target,
                        &staged,
                        resolution,
                        filename,
                        cancelled,
                        report_progress,
                    )?;
                }
                report_progress(EditProgress::new(EditPhase::ValidateSubtitle(
                    filename.to_string(),
                )));
                validate_subtitle_output(&staged, target)?;
                prepared.publications.push(Publication {
                    staged: subtitle_artifact_pairs(&staged, &destination, target)?,
                    remove: sidecar.source_paths().cloned().collect(),
                });
            }
        }
    }
    Ok(prepared)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SubtitlePreparation {
    Embed,
    Export,
    Import,
    Sidecar,
}

fn subtitle_preparation_phase(
    subject: &str,
    change: &SubtitleChange,
    target: SubtitleFormat,
    preparation: SubtitlePreparation,
) -> EditPhase {
    if change.source_format.is_image() && target.is_text() {
        let language = change.ocr_language.as_deref().unwrap_or("eng");
        return EditPhase::OcrSubtitle {
            subject: subject.to_string(),
            target: target.overview_label().to_string(),
            language: language.to_string(),
        };
    }
    if target != change.source_format {
        return EditPhase::ConvertSubtitle {
            subject: subject.to_string(),
            target: target.overview_label().to_string(),
        };
    }
    match preparation {
        SubtitlePreparation::Embed | SubtitlePreparation::Export => {
            EditPhase::ExtractSubtitle(subject.to_string())
        }
        SubtitlePreparation::Import => EditPhase::CopySubtitle(subject.to_string()),
        SubtitlePreparation::Sidecar => EditPhase::UpdateSubtitle(subject.to_string()),
    }
}

fn subtitle_input_path(path: &Path, format: SubtitleFormat) -> PathBuf {
    if format == SubtitleFormat::VobSub {
        path.with_extension("idx")
    } else {
        path.to_path_buf()
    }
}

fn sidecar_conversion_destination(
    base: &Path,
    sidecar: &SidecarEntry,
    target: SubtitleFormat,
    publications: &[Publication],
) -> Result<PathBuf, EditError> {
    if sidecar_destination_available(base, sidecar, target, publications) {
        return Ok(base.to_path_buf());
    }

    let mut number = 2;
    loop {
        let candidate = numbered_subtitle_path(base, number)?;
        if sidecar_destination_available(&candidate, sidecar, target, publications) {
            return Ok(candidate);
        }
        number = number.checked_add(1).ok_or_else(|| {
            EditError::Failed("Subtitle duplicate number is too large.".to_string())
        })?;
    }
}

fn sidecar_destination_available(
    destination: &Path,
    sidecar: &SidecarEntry,
    target: SubtitleFormat,
    publications: &[Publication],
) -> bool {
    std::iter::once(destination.to_path_buf())
        .chain((target == SubtitleFormat::VobSub).then(|| destination.with_extension("idx")))
        .all(|candidate| {
            let replaces_source = sidecar.source_paths().any(|source| source == &candidate);
            let reserved = publications.iter().any(|publication| {
                publication
                    .staged
                    .iter()
                    .any(|(_, published)| published == &candidate)
            });
            (!candidate.exists() || replaces_source) && !reserved
        })
}

#[derive(Clone, Copy)]
enum ConversionInput<'a> {
    Embedded { media: &'a Path, index: u64 },
    File(&'a Path),
}

#[expect(clippy::too_many_arguments)]
fn convert_subtitle(
    input: ConversionInput<'_>,
    change: &SubtitleChange,
    target: SubtitleFormat,
    output: &Path,
    resolution: (u64, u64),
    subject: &str,
    cancelled: &AtomicBool,
    report_progress: &mut ProgressReporter<'_>,
) -> Result<(), EditError> {
    if cancelled.load(Ordering::Relaxed) {
        report_progress(EditProgress::new(EditPhase::Cleanup));
        return Err(EditError::Cancelled);
    }
    if change.source_format == target
        && !(matches!(input, ConversionInput::Embedded { .. }) && target == SubtitleFormat::VobSub)
    {
        report_progress(EditProgress::new(match input {
            ConversionInput::Embedded { .. } => EditPhase::ExtractSubtitle(subject.to_string()),
            ConversionInput::File(_) => EditPhase::CopySubtitle(subject.to_string()),
        }));
        return extract_subtitle(input, output, target, subject, cancelled, report_progress);
    }
    if change.source_format.is_image() && target == SubtitleFormat::MovText {
        let intermediate = output.with_extension("ocr.srt");
        convert_subtitle(
            input,
            change,
            SubtitleFormat::SubRip,
            &intermediate,
            resolution,
            subject,
            cancelled,
            report_progress,
        )?;
        let text_change = SubtitleChange {
            cue_edits: Default::default(),
            source: change.source.clone(),
            source_format: SubtitleFormat::SubRip,
            embedded_target: Some(SubtitleFormat::MovText),
            export_target: None,
            import_into_media: false,
            ocr_language: None,

            metadata: None,
        };
        return convert_subtitle(
            ConversionInput::File(&intermediate),
            &text_change,
            SubtitleFormat::MovText,
            output,
            resolution,
            subject,
            cancelled,
            report_progress,
        );
    }
    let use_seconv = change.source_format.is_image() || target.is_image();
    let result = if use_seconv {
        let (extracted, file_input) = match input {
            ConversionInput::Embedded { media, index } => {
                report_progress(EditProgress::new(EditPhase::ExtractSubtitle(
                    subject.to_string(),
                )));
                let name = output
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("subtitle");
                let extracted_path = output.with_file_name(format!("{name}.source.mks"));
                let mut command = Command::new("ffmpeg");
                command
                    .args(["-v", "error", "-nostdin", "-y", "-i"])
                    .arg(media)
                    .args([
                        "-map",
                        &format!("0:{index}"),
                        "-c",
                        "copy",
                        "-f",
                        "matroska",
                    ])
                    .arg(&extracted_path);
                let extraction = run_cancellable_output(&mut command, cancelled, report_progress)?;
                if !extraction.status.success() {
                    return Err(EditError::Failed(command_error(
                        "Could not extract subtitle for conversion",
                        &extraction.stderr,
                    )));
                }
                (Some(extracted_path), None)
            }
            ConversionInput::File(path) => (None, Some(path)),
        };
        let path = extracted
            .as_deref()
            .or(file_input)
            .expect("subtitle conversion always has an input");
        let parent = output
            .parent()
            .expect("staged subtitle output always has a parent");
        let filename = output
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| EditError::Failed("Subtitle output name is not valid UTF-8.".into()))?;
        let mut command = Command::new("seconv");
        command
            .arg(path)
            .arg(target.seconv_name())
            .arg(format!("--output-folder:{}", parent.to_string_lossy()));
        command
            .arg(format!("--output-filename:{filename}"))
            .arg("--overwrite");
        if change.source_format.is_image() && target.is_text() {
            report_progress(EditProgress::new(EditPhase::OcrSubtitle {
                subject: subject.to_string(),
                target: target.overview_label().to_string(),
                language: change.ocr_language.as_deref().unwrap_or("eng").to_string(),
            }));
            command.arg("--ocr-engine:tesseract").arg(format!(
                "--ocr-language:{}",
                change.ocr_language.as_deref().unwrap_or("eng")
            ));
        } else {
            report_progress(EditProgress::new(EditPhase::ConvertSubtitle {
                subject: subject.to_string(),
                target: target.overview_label().to_string(),
            }));
        }
        if target.is_image() {
            command.arg(format!("--resolution:{}x{}", resolution.0, resolution.1));
        }
        run_cancellable_output(&mut command, cancelled, report_progress)
    } else {
        report_progress(EditProgress::new(EditPhase::ConvertSubtitle {
            subject: subject.to_string(),
            target: target.overview_label().to_string(),
        }));
        let encoder = target
            .ffmpeg_encoder()
            .expect("text subtitle targets have an FFmpeg encoder");
        let mut command = Command::new("ffmpeg");
        command.args(["-v", "error", "-nostdin", "-y", "-i"]);
        match input {
            ConversionInput::Embedded { media, index } => {
                command.arg(media).args(["-map", &format!("0:{index}")]);
            }
            ConversionInput::File(path) => {
                command.arg(path).args(["-map", "0:0"]);
            }
        }
        command.args(["-c:s", encoder]).arg(output);
        run_cancellable_output(&mut command, cancelled, report_progress)
    }?;
    if result.status.success() {
        Ok(())
    } else {
        Err(EditError::Failed(command_error(
            "Subtitle conversion failed",
            &result.stderr,
        )))
    }
}

fn extract_subtitle(
    input: ConversionInput<'_>,
    output: &Path,
    format: SubtitleFormat,
    subject: &str,
    cancelled: &AtomicBool,
    report_progress: &mut ProgressReporter<'_>,
) -> Result<(), EditError> {
    match input {
        ConversionInput::File(path) => {
            copy_subtitle_artifact(path, output, format, subject, cancelled, report_progress)
        }
        ConversionInput::Embedded { media, index } => {
            report_progress(EditProgress::new(EditPhase::ExtractSubtitle(
                subject.to_string(),
            )));
            let mut command = Command::new("ffmpeg");
            command
                .args(["-v", "error", "-nostdin", "-y", "-i"])
                .arg(media)
                .args(["-map", &format!("0:{index}"), "-c:s", "copy"])
                .arg(output);
            let result = run_cancellable_output(&mut command, cancelled, report_progress)?;
            if result.status.success() {
                Ok(())
            } else {
                Err(EditError::Failed(command_error(
                    "Subtitle export failed",
                    &result.stderr,
                )))
            }
        }
    }
}

/// Writes a SubRip file with the subtitle edit page's cue edits applied, into the workspace.
///
/// Staged rather than written over the original, because that is how every other artifact a
/// save produces is handled: the publication step is what puts files in the user's directory,
/// with the backup and rollback that go with it. A rewrite that patched the sidecar in place
/// would be the one edit in the application that could not be rolled back.
fn rewrite_cue_file(
    source: &Path,
    edits: &BTreeMap<usize, CueEdit>,
    destination: &Path,
    subject: &str,
    report_progress: &mut ProgressReporter<'_>,
) -> Result<(), EditError> {
    report_progress(EditProgress::new(EditPhase::RewriteCues(
        subject.to_string(),
    )));
    let text = std::fs::read_to_string(source).map_err(|error| {
        EditError::Failed(format!(
            "Could not read {} to rewrite its cues: {error}",
            source.display()
        ))
    })?;
    let rewritten = crate::subtitle::rewrite_srt_cues(&text, edits).map_err(EditError::Failed)?;
    std::fs::write(destination, rewritten).map_err(|error| {
        EditError::Failed(format!(
            "Could not stage the rewritten cues: {}: {error}",
            destination.display()
        ))
    })
}

fn copy_subtitle_artifact(
    source: &Path,
    destination: &Path,
    format: SubtitleFormat,
    subject: &str,
    cancelled: &AtomicBool,
    report_progress: &mut ProgressReporter<'_>,
) -> Result<(), EditError> {
    let phase = EditPhase::CopySubtitle(subject.to_string());
    copy_file_with_progress(
        source,
        destination,
        phase.clone(),
        cancelled,
        report_progress,
    )
    .map_err(|error| copy_edit_error("Could not stage subtitle export", error))?;
    if format == SubtitleFormat::VobSub {
        copy_file_with_progress(
            source.with_extension("idx"),
            destination.with_extension("idx"),
            phase,
            cancelled,
            report_progress,
        )
        .map_err(|error| copy_edit_error("Could not stage the VobSub .idx companion", error))?;
    }
    Ok(())
}

fn copy_edit_error(context: &str, error: std::io::Error) -> EditError {
    if error.kind() == std::io::ErrorKind::Interrupted {
        EditError::Cancelled
    } else {
        EditError::Failed(format!("{context}: {error}"))
    }
}

fn validate_subtitle_output(path: &Path, target: SubtitleFormat) -> Result<(), EditError> {
    let info = probe_any_file(path).map_err(EditError::Failed)?;
    let subtitles = info
        .streams
        .iter()
        .filter(|stream| stream_kind(stream) == Some("subtitle"))
        .collect::<Vec<_>>();
    if subtitles.len() != 1
        || subtitles[0].get("codec_name").and_then(Value::as_str) != Some(target.ffmpeg_codec())
    {
        return Err(EditError::Failed(format!(
            "Converted subtitle did not validate as {}.",
            target.label()
        )));
    }
    Ok(())
}

fn subtitle_artifact_pairs(
    staged: &Path,
    destination: &Path,
    target: SubtitleFormat,
) -> Result<Vec<(PathBuf, PathBuf)>, EditError> {
    let mut pairs = vec![(staged.to_path_buf(), destination.to_path_buf())];
    if target == SubtitleFormat::VobSub {
        let staged_idx = staged.with_extension("idx");
        if !staged_idx.exists() {
            return Err(EditError::Failed(
                "VobSub conversion did not create its required .idx companion.".to_string(),
            ));
        }
        pairs.push((staged_idx, destination.with_extension("idx")));
    }
    Ok(pairs)
}

pub(crate) fn primary_video_resolution(info: &MediaInfo) -> Option<(u64, u64)> {
    info.streams
        .iter()
        .find(|stream| stream_kind(stream) == Some("video") && !is_attached_picture(stream))
        .and_then(|stream| {
            stream_dimension(stream, "width").zip(stream_dimension(stream, "height"))
        })
}

#[derive(Clone, Copy, Debug)]
struct EditTarget<'a> {
    source: &'a Path,
    destination: SaveDestination,
    container: Option<ContainerFormat>,
    container_metadata: Option<&'a ContainerMetadata>,
}

#[derive(Clone, Copy)]
struct TrackEdits<'a> {
    stream_order: &'a [u64],
    deleted_streams: &'a BTreeSet<u64>,
    default_streams: &'a BTreeSet<u64>,
    default_sidecars: &'a BTreeSet<usize>,
    audio_settings: &'a BTreeMap<u64, AudioSettings>,
    video_settings: &'a BTreeMap<u64, VideoSettings>,
    subtitle_changes: &'a [SubtitleChange],
    left_subtitle_order: &'a [TrackRef],
    sidecars: &'a [SidecarEntry],
}

fn next_copy_path(source: &Path, container: Option<ContainerFormat>) -> Result<PathBuf, EditError> {
    for number in 1.. {
        let candidate = copy_path(source, number, container).map_err(EditError::Failed)?;
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    unreachable!("copy suffix counter cannot be exhausted")
}

fn retarget_publications_for_copy(
    publications: &[Publication],
    source: &Path,
    copy: &Path,
) -> Result<Vec<Publication>, EditError> {
    let source_stem = source
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| EditError::Failed("The source filename is not valid UTF-8.".to_string()))?;
    let copy_stem = copy
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| EditError::Failed("The copy filename is not valid UTF-8.".to_string()))?;
    publications
        .iter()
        .map(|publication| {
            let staged = publication
                .staged
                .iter()
                .map(|(staged, destination)| {
                    let name = destination
                        .file_name()
                        .and_then(|name| name.to_str())
                        .ok_or_else(|| {
                            EditError::Failed(
                                "A subtitle destination is not valid UTF-8.".to_string(),
                            )
                        })?;
                    let name = name
                        .strip_prefix(source_stem)
                        .map(|suffix| format!("{copy_stem}{suffix}"))
                        .unwrap_or_else(|| name.to_string());
                    Ok((staged.clone(), destination.with_file_name(name)))
                })
                .collect::<Result<Vec<_>, EditError>>()?;
            Ok(Publication {
                staged,
                remove: publication.remove.clone(),
            })
        })
        .collect()
}

fn resolve_export_duplicates(
    publications: &mut Vec<Publication>,
    workspace: &Path,
) -> Result<(), EditError> {
    let mut groups = BTreeMap::<PathBuf, Vec<usize>>::new();
    for (index, publication) in publications.iter().enumerate() {
        if publication.remove.is_empty() && !publication.staged.is_empty() {
            groups
                .entry(publication.staged[0].1.clone())
                .or_default()
                .push(index);
        }
    }
    let mut additional = Vec::new();
    let mut reserved = publications
        .iter()
        .filter(|publication| !publication.remove.is_empty())
        .flat_map(|publication| {
            publication
                .staged
                .iter()
                .map(|(_, destination)| destination.clone())
        })
        .collect::<BTreeSet<_>>();
    for (base, indices) in groups {
        let existing = base.exists();
        let first = numbered_subtitle_path(&base, 1)?;
        if existing && (first.exists() || reserved.contains(&first)) {
            return Err(EditError::Failed(format!(
                "Cannot normalize duplicate subtitle {} because {} already exists.",
                base.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("file"),
                first
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("the .1 target")
            )));
        }
        if existing {
            let first_publication = indices[0];
            let has_companion = publications[first_publication].staged.len() == 2;
            let existing_targets = if has_companion {
                vec![
                    (base.clone(), first.clone()),
                    (base.with_extension("idx"), first.with_extension("idx")),
                ]
            } else {
                vec![(base.clone(), first.clone())]
            };
            let mut normalized_staged = Vec::new();
            for (pair_index, (source, destination)) in existing_targets.into_iter().enumerate() {
                let staged = workspace.join(format!("normalize-{first_publication}-{pair_index}"));
                fs::copy(&source, &staged).map_err(|error| {
                    EditError::Failed(format!(
                        "Could not stage existing subtitle duplicate: {error}"
                    ))
                })?;
                normalized_staged.push((staged, destination.clone()));
                publications[first_publication].remove.push(source);
                reserved.insert(destination);
            }
            additional.push(Publication {
                staged: normalized_staged,
                remove: Vec::new(),
            });
        }

        if !existing && indices.len() == 1 {
            reserved.insert(base);
            continue;
        }
        let mut number = if existing { 2 } else { 1 };
        for index in indices {
            let destination = loop {
                let candidate = numbered_subtitle_path(&base, number)?;
                number += 1;
                if !candidate.exists() && !reserved.contains(&candidate) {
                    break candidate;
                }
            };
            for (_, target) in &mut publications[index].staged {
                let is_idx =
                    target.extension().and_then(|extension| extension.to_str()) == Some("idx");
                *target = if is_idx {
                    destination.with_extension("idx")
                } else {
                    destination.clone()
                };
                reserved.insert(target.clone());
            }
        }
    }
    publications.extend(additional);
    Ok(())
}

fn numbered_subtitle_path(path: &Path, number: usize) -> Result<PathBuf, EditError> {
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| EditError::Failed("Subtitle filename is not valid UTF-8.".to_string()))?;
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .ok_or_else(|| EditError::Failed("Subtitle filename has no extension.".to_string()))?;
    Ok(path.with_file_name(format!("{stem}.{number}.{extension}")))
}

#[cfg(test)]
fn publish_transaction(
    media: Option<(&Path, &Path)>,
    removed_media: Option<&Path>,
    publications: &[Publication],
    cancelled: &AtomicBool,
) -> Result<(), EditError> {
    publish_transaction_with_progress(media, removed_media, publications, cancelled, &mut |_| {})
}

fn publish_transaction_with_progress(
    media: Option<(&Path, &Path)>,
    removed_media: Option<&Path>,
    publications: &[Publication],
    cancelled: &AtomicBool,
    report_progress: &mut ProgressReporter<'_>,
) -> Result<(), EditError> {
    report_progress(EditProgress::new(EditPhase::PreparePublication));
    if cancelled.load(Ordering::Relaxed) {
        report_progress(EditProgress::new(EditPhase::Cleanup));
        return Err(EditError::Cancelled);
    }
    let backup_parent = media
        .and_then(|(staged, _)| staged.parent())
        .or_else(|| {
            publications
                .iter()
                .flat_map(|publication| publication.staged.iter())
                .next()
                .and_then(|(staged, _)| staged.parent())
        })
        .ok_or_else(|| EditError::Failed("No staging directory is available.".to_string()))?;
    let mut old_paths = publications
        .iter()
        .flat_map(|publication| publication.remove.iter().cloned())
        .collect::<BTreeSet<_>>();
    old_paths.extend(removed_media.map(Path::to_path_buf));
    let destinations = media
        .into_iter()
        .map(|(_, destination)| destination.to_path_buf())
        .chain(publications.iter().flat_map(|publication| {
            publication
                .staged
                .iter()
                .map(|(_, destination)| destination.clone())
        }))
        .collect::<Vec<_>>();
    let mut unique = BTreeSet::new();
    for destination in &destinations {
        if !unique.insert(destination.clone()) {
            return Err(EditError::Failed(format!(
                "Two subtitle outputs resolve to {}.",
                destination.display()
            )));
        }
        if destination.exists() && !old_paths.contains(destination) {
            return Err(EditError::Failed(format!(
                "{} already exists; no files were changed.",
                destination.display()
            )));
        }
    }

    let mut backups = Vec::new();
    for (number, old) in old_paths.iter().enumerate() {
        if !old.exists() {
            continue;
        }
        let backup = backup_parent.join(format!("transaction-backup-{number}"));
        let phase = EditPhase::BackupFile(display_file_name(old));
        if let Err(error) =
            move_or_copy_file_with_progress(old, &backup, phase, cancelled, report_progress)
        {
            report_progress(EditProgress::new(EditPhase::Rollback));
            rollback_transaction(&[], &backups);
            if error.kind() == std::io::ErrorKind::Interrupted {
                report_progress(EditProgress::new(EditPhase::Cleanup));
                return Err(EditError::Cancelled);
            }
            return Err(EditError::Failed(format!(
                "Could not stage existing file for replacement: {error}"
            )));
        }
        backups.push((backup, old.clone()));
    }

    let mut published = Vec::new();
    let staged_pairs = media
        .into_iter()
        .map(|(staged, destination)| (staged.to_path_buf(), destination.to_path_buf()))
        .chain(
            publications
                .iter()
                .flat_map(|publication| publication.staged.iter().cloned()),
        );
    for (staged, destination) in staged_pairs {
        let phase = EditPhase::PublishFile(display_file_name(&destination));
        if let Err(error) = move_or_copy_file_with_progress(
            &staged,
            &destination,
            phase,
            cancelled,
            report_progress,
        ) {
            report_progress(EditProgress::new(EditPhase::Rollback));
            rollback_transaction(&published, &backups);
            if error.kind() == std::io::ErrorKind::Interrupted {
                report_progress(EditProgress::new(EditPhase::Cleanup));
                return Err(EditError::Cancelled);
            }
            return Err(EditError::Failed(format!(
                "Could not publish the completed edit: {error}"
            )));
        }
        published.push(destination);
    }
    report_progress(EditProgress::new(EditPhase::RemoveBackups));
    for (backup, _) in backups {
        let _ = fs::remove_file(backup);
    }
    Ok(())
}

fn display_file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file")
        .to_string()
}

fn move_or_copy_file_with_progress(
    src: &Path,
    dst: &Path,
    phase: EditPhase,
    cancelled: &AtomicBool,
    report_progress: &mut ProgressReporter<'_>,
) -> std::io::Result<()> {
    report_progress(EditProgress::new(phase.clone()));
    if fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    let metadata = fs::metadata(src)?;
    let total = metadata.len();
    let mut source = fs::File::open(src)?;
    let mut destination = fs::File::create(dst)?;
    let mut buffer = [0_u8; 1024 * 1024];
    let mut copied = 0_u64;
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "media edit cancelled",
            ));
        }
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        use std::io::Write as _;
        destination.write_all(&buffer[..read])?;
        copied = copied.saturating_add(read as u64);
        if total > 0 {
            report_progress(EditProgress::measured(
                phase.clone(),
                copied as f64 / total as f64,
            ));
        }
    }
    destination.sync_all()?;
    fs::set_permissions(dst, metadata.permissions())?;
    fs::remove_file(src)
}

fn copy_file_with_progress(
    src: impl AsRef<Path>,
    dst: impl AsRef<Path>,
    phase: EditPhase,
    cancelled: &AtomicBool,
    report_progress: &mut ProgressReporter<'_>,
) -> std::io::Result<()> {
    let src = src.as_ref();
    let dst = dst.as_ref();
    let metadata = fs::metadata(src)?;
    let total = metadata.len();
    let mut source = fs::File::open(src)?;
    let mut destination = fs::File::create(dst)?;
    let mut buffer = [0_u8; 1024 * 1024];
    let mut copied = 0_u64;
    report_progress(EditProgress::new(phase.clone()));
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "media edit cancelled",
            ));
        }
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        use std::io::Write as _;
        destination.write_all(&buffer[..read])?;
        copied = copied.saturating_add(read as u64);
        if total > 0 {
            report_progress(EditProgress::measured(
                phase.clone(),
                copied as f64 / total as f64,
            ));
        }
    }
    destination.sync_all()?;
    fs::set_permissions(dst, metadata.permissions())
}

fn rollback_transaction(published: &[PathBuf], backups: &[(PathBuf, PathBuf)]) {
    for path in published.iter().rev() {
        let _ = fs::remove_file(path);
    }
    for (backup, original) in backups.iter().rev() {
        let _ = fs::rename(backup, original);
    }
}

fn copy_path(
    source: &Path,
    number: usize,
    container: Option<ContainerFormat>,
) -> Result<PathBuf, String> {
    let parent = source
        .parent()
        .ok_or_else(|| "The source file has no parent directory.".to_string())?;
    let stem = source
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| "The source filename is not valid UTF-8.".to_string())?;
    let suffix = if number == 1 {
        "-reel-edit".to_string()
    } else {
        format!("-reel-edit-{number}")
    };
    let target_extension = container
        .map(ContainerFormat::extension)
        .or_else(|| source.extension().and_then(|extension| extension.to_str()));
    let name = match target_extension {
        Some(extension) => format!("{stem}{suffix}.{extension}"),
        None => format!("{stem}{suffix}"),
    };
    Ok(parent.join(name))
}

fn replacement_path(
    source: &Path,
    container: Option<ContainerFormat>,
) -> Result<PathBuf, EditError> {
    if let Some(container) = container {
        let parent = source.parent().ok_or_else(|| {
            EditError::Failed("The source file has no parent directory.".to_string())
        })?;
        let stem = source
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| {
                EditError::Failed("The source filename is not valid UTF-8.".to_string())
            })?;
        Ok(parent.join(format!("{stem}.{}", container.extension())))
    } else {
        Ok(source.to_path_buf())
    }
}

fn source_matches_fingerprint(path: &Path, expected: FileFingerprint) -> std::io::Result<bool> {
    FileFingerprint::for_path(path).map(|current| current == expected)
}

fn media_info(path: &Path) -> Result<MediaInfo, String> {
    match probe_file(path, crate::mount::is_network_mount(path)) {
        ProbeOutcome::Video(info) => Ok(info),
        ProbeOutcome::NotVideo(reason) | ProbeOutcome::Error(reason) => Err(reason),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutputTrack {
    Existing(u64),
    Imported(usize),
}

fn output_track_plan(
    source: &MediaInfo,
    stream_order: &[u64],
    left_subtitle_order: &[TrackRef],
    subtitle_imports: &[SubtitleImport],
    sidecars: &[SidecarEntry],
) -> Vec<OutputTrack> {
    // The chapter track is dropped before anything else looks at the order: `ffmpeg`
    // is told `-map_chapters 0`, which writes the chapters back out as a text track of
    // its own, so mapping the source's would store them twice — and Matroska refuses
    // the stream outright, which is what stopped an MP4 with chapters converting to MKV.
    let stream_order = stream_order
        .iter()
        .copied()
        .filter(|index| {
            source
                .streams
                .iter()
                .find(|stream| stream_index(stream) == Some(*index))
                .is_none_or(|stream| !is_chapter_track(source, stream))
        })
        .collect::<Vec<_>>();
    let stream_order = stream_order.as_slice();
    if left_subtitle_order.is_empty() {
        let insert_at = stream_order
            .iter()
            .rposition(|index| {
                source
                    .streams
                    .iter()
                    .find(|stream| stream_index(stream) == Some(*index))
                    .is_some_and(|stream| stream_kind(stream) == Some("subtitle"))
            })
            .map(|position| position + 1)
            .unwrap_or_else(|| {
                stream_order
                    .iter()
                    .position(|index| {
                        source
                            .streams
                            .iter()
                            .find(|stream| stream_index(stream) == Some(*index))
                            .is_some_and(|stream| {
                                !matches!(stream_kind(stream), Some("video" | "audio"))
                            })
                    })
                    .unwrap_or(stream_order.len())
            });
        let mut tracks = Vec::with_capacity(stream_order.len() + subtitle_imports.len());
        for (position, index) in stream_order.iter().enumerate() {
            if position == insert_at {
                tracks.extend((0..subtitle_imports.len()).map(OutputTrack::Imported));
            }
            tracks.push(OutputTrack::Existing(*index));
        }
        if insert_at == stream_order.len() {
            tracks.extend((0..subtitle_imports.len()).map(OutputTrack::Imported));
        }
        return tracks;
    }

    let mut tracks = Vec::with_capacity(stream_order.len() + subtitle_imports.len());

    for index in stream_order {
        if let Some(stream) = source
            .streams
            .iter()
            .find(|stream| stream_index(stream) == Some(*index))
            && matches!(stream_kind(stream), Some("video" | "audio"))
        {
            tracks.push(OutputTrack::Existing(*index));
        }
    }

    let mut placed_embedded_subtitles = BTreeSet::new();
    let mut placed_imports = BTreeSet::new();

    for track in left_subtitle_order {
        match track {
            TrackRef::Embedded(index) => {
                if stream_order.contains(index)
                    && source.streams.iter().any(|stream| {
                        stream_index(stream) == Some(*index)
                            && stream_kind(stream) == Some("subtitle")
                    })
                {
                    tracks.push(OutputTrack::Existing(*index));
                    placed_embedded_subtitles.insert(*index);
                }
            }
            TrackRef::Sidecar(sidecar_index) => {
                if let Some(sidecar) = sidecars.get(*sidecar_index)
                    && let Some(import_idx) = subtitle_imports
                        .iter()
                        .position(|import| import.source_path == sidecar.path)
                    && !placed_imports.contains(&import_idx)
                {
                    tracks.push(OutputTrack::Imported(import_idx));
                    placed_imports.insert(import_idx);
                }
            }
            _ => {}
        }
    }

    for index in stream_order {
        if source.streams.iter().any(|stream| {
            stream_index(stream) == Some(*index) && stream_kind(stream) == Some("subtitle")
        }) && !placed_embedded_subtitles.contains(index)
        {
            tracks.push(OutputTrack::Existing(*index));
        }
    }

    for import_idx in 0..subtitle_imports.len() {
        if !placed_imports.contains(&import_idx) {
            tracks.push(OutputTrack::Imported(import_idx));
        }
    }

    for index in stream_order {
        if let Some(stream) = source
            .streams
            .iter()
            .find(|stream| stream_index(stream) == Some(*index))
            && !matches!(stream_kind(stream), Some("video" | "audio" | "subtitle"))
        {
            tracks.push(OutputTrack::Existing(*index));
        }
    }

    tracks
}

#[expect(clippy::too_many_arguments)]
fn validate_result(
    source: &MediaInfo,
    output: &MediaInfo,
    stream_order: &[u64],
    left_subtitle_order: &[TrackRef],
    default_streams: &BTreeSet<u64>,
    audio_settings: &BTreeMap<u64, AudioSettings>,
    video_settings: &BTreeMap<u64, VideoSettings>,
    subtitle_replacements: &[SubtitleReplacement],
    subtitle_imports: &[SubtitleImport],
    subtitle_changes: &[SubtitleChange],
    sidecars: &[SidecarEntry],
    container: Option<ContainerFormat>,
) -> Result<(), String> {
    let has_video = output
        .streams
        .iter()
        .any(|stream| stream_kind(stream) == Some("video") && !is_attached_picture(stream));
    if !has_video {
        return Err("The remuxed file has no playable video track.".to_string());
    }
    let output_tracks = output_track_plan(
        source,
        stream_order,
        left_subtitle_order,
        subtitle_imports,
        sidecars,
    );
    let expected_kinds = output_tracks
        .iter()
        .filter_map(|track| match track {
            OutputTrack::Existing(index) => source
                .streams
                .iter()
                .find(|stream| stream_index(stream) == Some(*index))
                .and_then(stream_kind),
            OutputTrack::Imported(_) => Some("subtitle"),
        })
        .collect::<Vec<_>>();
    // The output's own chapter track is left out for the reason the source's is: the
    // `mov` muxer writes one from `-map_chapters 0` whether or not anything was mapped
    // to it, so counting it here would report every chaptered MP4 as having grown a
    // track the plan never asked for.
    let output_streams = output
        .streams
        .iter()
        .filter(|stream| !is_chapter_track(output, stream))
        .collect::<Vec<_>>();
    let output_kinds = output_streams
        .iter()
        .filter_map(|stream| stream_kind(stream))
        .collect::<Vec<_>>();
    if output_kinds != expected_kinds {
        return Err("The remuxed tracks are not in the requested order.".to_string());
    }
    let expected_defaults = output_tracks
        .iter()
        .map(|track| match track {
            OutputTrack::Existing(index) => default_streams.contains(index),
            OutputTrack::Imported(import_index) => subtitle_imports[*import_index].default,
        })
        .collect::<Vec<_>>();
    let muxer_defaults = muxer_forced_default_positions(output, &expected_defaults, container);
    for (position, stream) in output_streams.iter().copied().enumerate() {
        let Some(track) = output_tracks.get(position) else {
            return Err("The remuxed file has an unexpected extra track.".to_string());
        };
        let expected = expected_defaults[position];
        if is_default(stream) != expected
            && !(is_default(stream) && muxer_defaults.contains(&position))
        {
            let source_label = match track {
                OutputTrack::Existing(index) => format!("source track #{index}"),
                OutputTrack::Imported(import_index) => {
                    format!("imported subtitle #{import_index}")
                }
            };
            return Err(format!(
                "The remuxed track at position {position} ({source_label}) has the wrong \
                 default flag: expected {expected}, found {}. Staged defaults: {default_streams:?}.",
                is_default(stream),
            ));
        }
        let source_index = match track {
            OutputTrack::Existing(index) => *index,
            OutputTrack::Imported(import_index) => {
                let import = &subtitle_imports[*import_index];
                if stream.get("codec_name").and_then(Value::as_str)
                    != Some(import.target.ffmpeg_codec())
                {
                    return Err(format!(
                        "The imported subtitle track at position {position} has the wrong codec."
                    ));
                }
                if !subtitle_metadata_matches(stream, &import.metadata) {
                    return Err(format!(
                        "The imported subtitle track at position {position} has the wrong metadata."
                    ));
                }
                continue;
            }
        };
        let source_stream = source
            .streams
            .iter()
            .find(|candidate| stream_index(candidate) == Some(source_index));
        if let Some(settings) = audio_settings.get(&source_index) {
            let source_stream = source_stream
                .expect("the validated output plan contains every configured audio source");
            let expected_codec =
                effective_audio_codec(source_stream, settings).and_then(AudioCodec::codec_name);
            if audio_requires_transcode(source_stream, settings)
                && expected_codec != stream.get("codec_name").and_then(Value::as_str)
            {
                return Err(format!(
                    "The encoded audio track at position {position} has the wrong codec."
                ));
            }
            if let Some(channels) = settings.channel_layout.channels()
                && stream_channels(stream) != Some(channels)
            {
                return Err(format!(
                    "The encoded audio track at position {position} has the wrong channel layout."
                ));
            }
            let expected_sample_rate = effective_audio_codec(source_stream, settings)
                .zip(stream_sample_rate(source_stream))
                .and_then(|(codec, source_rate)| resolved_audio_sample_rate(codec, source_rate));
            if audio_requires_transcode(source_stream, settings)
                && expected_sample_rate.is_some_and(|rate| stream_sample_rate(stream) != Some(rate))
            {
                return Err(format!(
                    "The encoded audio track at position {position} has the wrong sample rate."
                ));
            }
            let mut expected_metadata = settings.metadata.clone();
            if let Some(container) = container {
                container.retain_supported_audio_metadata(&mut expected_metadata);
            }
            if !audio_metadata_matches(stream, &expected_metadata) {
                return Err(format!(
                    "The audio track at position {position} has the wrong metadata."
                ));
            }
        }
        let metadata_change = subtitle_changes.iter().find(|change| {
            change.source == SubtitleSource::Embedded(source_index) && change.metadata.is_some()
        });
        if let Some(source_stream) = source_stream
            && stream_kind(source_stream) == Some("subtitle")
            && (metadata_change.is_some()
                || subtitle_replacements
                    .iter()
                    .any(|replacement| replacement.source_index == source_index))
        {
            let mut expected = metadata_change
                .map(|change| effective_subtitle_metadata(change, stream_metadata(source_stream)))
                .unwrap_or_else(|| stream_metadata(source_stream));
            if let Some(container) = container {
                container.retain_supported_subtitle_metadata(&mut expected);
            }
            if !subtitle_metadata_matches(stream, &expected) {
                return Err(format!(
                    "The subtitle track at position {position} has the wrong metadata."
                ));
            }
        }
        if let Some(replacement) = subtitle_replacements
            .iter()
            .find(|replacement| replacement.source_index == source_index)
        {
            if stream.get("codec_name").and_then(Value::as_str)
                != Some(replacement.target.ffmpeg_codec())
            {
                return Err(format!(
                    "The converted subtitle track at position {position} has the wrong codec."
                ));
            }
            continue;
        }
        let Some(settings) = video_settings.get(&source_index) else {
            continue;
        };
        let mut expected_metadata = settings.metadata.clone();
        if let Some(container) = container {
            container.retain_supported_video_metadata(&mut expected_metadata);
        }
        if !video_metadata_matches(stream, &expected_metadata) {
            return Err(format!(
                "The video track at position {position} has the wrong metadata."
            ));
        }
        let transcoded = source_stream.is_none_or(|stream| requires_transcode(stream, settings));
        // An encode applies the rotation to the picture itself and leaves no matrix
        // behind, so the tag is only expected on the copy path.
        let expected_rotation = if transcoded {
            VideoRotation::None
        } else {
            settings.rotation
        };
        if stream_rotation(stream) != expected_rotation {
            return Err(format!(
                "The video track at position {position} has the wrong rotation: expected {}, found {}.",
                expected_rotation.degrees(),
                stream_rotation(stream).degrees(),
            ));
        }
        if !transcoded {
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
        if !output_resolution_matches(stream, source_stream, settings) {
            return Err(format!(
                "The encoded video track at position {position} has the wrong resolution."
            ));
        }
    }
    Ok(())
}

/// Positions the muxer is allowed to flag as default even though nothing staged asked
/// for it.
///
/// MP4 and MOV store "default" as the `tkhd` *enabled* flag, and `movenc` enables the
/// first track of a media type whenever no track of that type is flagged — there is no
/// way to write an ISO-BMFF file where a whole track group is disabled, and
/// `-disposition:N 0` does not change that. Matroska has no such rule, so a track group
/// with no default stays that way there. Without this allowance, dropping every default
/// subtitle (or keeping a single non-default one, the common case when converting one
/// track to MOV Text) fails validation on a file the muxer wrote exactly as asked.
fn muxer_forced_default_positions(
    output: &MediaInfo,
    expected_defaults: &[bool],
    container: Option<ContainerFormat>,
) -> BTreeSet<usize> {
    if !matches!(
        container,
        Some(ContainerFormat::Mp4) | Some(ContainerFormat::Mov)
    ) {
        return BTreeSet::new();
    }
    let mut first_of_kind: BTreeMap<&str, usize> = BTreeMap::new();
    let mut kinds_with_a_default: BTreeSet<&str> = BTreeSet::new();
    // Positions are the plan's, so the muxer's own chapter track — which the plan never
    // asked for — is skipped here exactly as `validate_result` skips it.
    for (position, stream) in output
        .streams
        .iter()
        .filter(|stream| !is_chapter_track(output, stream))
        .enumerate()
    {
        let Some(kind) = stream_kind(stream) else {
            continue;
        };
        first_of_kind.entry(kind).or_insert(position);
        if expected_defaults.get(position).copied().unwrap_or_default() {
            kinds_with_a_default.insert(kind);
        }
    }
    first_of_kind
        .into_iter()
        .filter(|(kind, _)| !kinds_with_a_default.contains(kind))
        .map(|(_, position)| position)
        .collect()
}

fn subtitle_metadata_matches(
    stream: &BTreeMap<String, Value>,
    expected: &SubtitleMetadata,
) -> bool {
    stream_language(stream) == expected.language
        && stream_title(stream) == expected.title
        && stream_forced(stream) == expected.forced
        && stream_cc(stream) == expected.cc
        && stream_hearing_impaired(stream) == expected.hearing_impaired
        && stream_original(stream) == expected.original
        && stream_commentary(stream) == expected.commentary
}

fn audio_metadata_matches(stream: &BTreeMap<String, Value>, expected: &AudioMetadata) -> bool {
    stream_language(stream) == expected.language
        && audio_stream_title(stream) == expected.title
        && stream_commentary(stream) == expected.commentary
        && stream_hearing_impaired(stream) == expected.hearing_impaired
        && stream_disposition(stream, "visual_impaired") == expected.audio_description
        && stream_original(stream) == expected.original
        && stream_disposition(stream, "dub") == expected.dubbed
}

pub(crate) fn audio_stream_title(stream: &BTreeMap<String, Value>) -> Option<String> {
    let tags = stream.get("tags").and_then(Value::as_object)?;
    ["title", "name"]
        .into_iter()
        .filter_map(|key| tags.get(key).and_then(Value::as_str))
        .map(str::trim)
        .find(|title| !title.is_empty())
        .map(str::to_string)
        .or_else(|| {
            tags.get("handler_name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|title| !title.is_empty() && !title.eq_ignore_ascii_case("SoundHandler"))
                .map(str::to_string)
        })
}

fn video_metadata_matches(stream: &BTreeMap<String, Value>, expected: &VideoMetadata) -> bool {
    stream_language(stream) == expected.language
        && video_stream_title(stream) == expected.title
        && stream_commentary(stream) == expected.commentary
}

/// Mirrors `audio_stream_title`, but ignores ffmpeg's default MP4/MOV *video* handler name
/// (`VideoHandler`) rather than its audio one (`SoundHandler`).
pub(crate) fn video_stream_title(stream: &BTreeMap<String, Value>) -> Option<String> {
    let tags = stream.get("tags").and_then(Value::as_object)?;
    ["title", "name"]
        .into_iter()
        .filter_map(|key| tags.get(key).and_then(Value::as_str))
        .map(str::trim)
        .find(|title| !title.is_empty())
        .map(str::to_string)
        .or_else(|| {
            tags.get("handler_name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|title| !title.is_empty() && !title.eq_ignore_ascii_case("VideoHandler"))
                .map(str::to_string)
        })
}

pub(crate) fn stream_disposition(stream: &BTreeMap<String, Value>, name: &str) -> bool {
    stream
        .get("disposition")
        .and_then(Value::as_object)
        .and_then(|values| values.get(name))
        .and_then(Value::as_i64)
        == Some(1)
}

/// The rotation a stream's display matrix asks for, or `None` when it carries no matrix.
///
/// `ffprobe -show_streams` already reports this under `side_data_list`, so nothing about
/// probing or the on-disk probe cache has to change to read it.
pub(crate) fn stream_rotation(stream: &BTreeMap<String, Value>) -> VideoRotation {
    stream
        .get("side_data_list")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("rotation"))
        .find_map(|rotation| {
            rotation
                .as_i64()
                .or_else(|| rotation.as_f64().map(|degrees| degrees.round() as i64))
        })
        .map_or(VideoRotation::None, VideoRotation::from_degrees)
}

fn validate_output_container(
    output: &MediaInfo,
    path: &Path,
    expected: ContainerFormat,
) -> Result<(), String> {
    let extension_matches = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case(expected.extension()));
    let format_name = output
        .format
        .get("format_name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let format_matches = match expected {
        ContainerFormat::Matroska | ContainerFormat::WebM => format_name
            .split(',')
            .any(|name| name == "matroska" || name == "webm"),
        ContainerFormat::Mp4 | ContainerFormat::Mov => format_name
            .split(',')
            .any(|name| name == "mov" || name == "mp4"),
    };
    if extension_matches && format_matches {
        Ok(())
    } else {
        Err(format!(
            "The completed file is not a valid {} container.",
            expected.label()
        ))
    }
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
    left_subtitle_order: &'a [TrackRef],
    default_streams: &'a BTreeSet<u64>,
    audio_settings: &'a BTreeMap<u64, AudioSettings>,
    video_settings: &'a BTreeMap<u64, VideoSettings>,
    replacements: &'a [SubtitleReplacement],
    imports: &'a [SubtitleImport],
    subtitle_changes: &'a [SubtitleChange],
    sidecars: &'a [SidecarEntry],
    container: Option<ContainerFormat>,
    container_metadata: Option<&'a ContainerMetadata>,
    duration: Option<f64>,
    cancelled: &'a AtomicBool,
}

fn run_ffmpeg(
    plan: FfmpegPlan<'_>,
    phase: EditPhase,
    report_progress: &mut ProgressReporter<'_>,
) -> Result<FfmpegOutput, EditError> {
    report_progress(EditProgress::new(phase.clone()));
    let metadata_container = plan.container.or_else(|| {
        ContainerFormat::detect(
            plan.source,
            plan.source_info
                .format
                .get("format_name")
                .and_then(Value::as_str),
        )
    });
    let mut command = Command::new("ffmpeg");
    command.args([
        "-v",
        "error",
        "-nostdin",
        "-y",
        "-progress",
        "pipe:1",
        "-nostats",
    ]);
    // `-display_rotation` is an input option, so unlike everything else below it has to
    // be emitted before the source it applies to.
    for (specifier, rotation) in display_rotation_args(plan.source_info, plan.video_settings) {
        command
            .arg(format!("-display_rotation:{specifier}"))
            .arg(rotation);
    }
    command.arg("-i").arg(plan.source);
    for replacement in plan.replacements {
        command.arg("-i").arg(&replacement.path);
    }
    for import in plan.imports {
        command.arg("-i").arg(&import.path);
    }
    let output_tracks = output_track_plan(
        plan.source_info,
        plan.stream_order,
        plan.left_subtitle_order,
        plan.imports,
        plan.sidecars,
    );
    for track in &output_tracks {
        match track {
            OutputTrack::Existing(index) => {
                if let Some(replacement_index) = plan
                    .replacements
                    .iter()
                    .position(|replacement| replacement.source_index == *index)
                {
                    command.args(["-map", &format!("{}:0", replacement_index + 1)]);
                } else {
                    command.args(["-map", &format!("0:{index}")]);
                }
            }
            OutputTrack::Imported(import_index) => {
                let input_index = 1 + plan.replacements.len() + import_index;
                command.args(["-map", &format!("{input_index}:0")]);
            }
        }
    }
    command.args(["-map_metadata", "0", "-map_chapters", "0", "-c", "copy"]);
    if let Some(metadata) = plan.container_metadata {
        if let Some(title) = &metadata.title {
            command.arg("-metadata").arg(format!("title={title}"));
        }
        if let Some(comment) = &metadata.comment {
            command.arg("-metadata").arg(format!("comment={comment}"));
        }
        if let Some(date) = &metadata.date {
            command.arg("-metadata").arg(format!("date={date}"));
        }
        if let Some(genre) = &metadata.genre {
            command.arg("-metadata").arg(format!("genre={genre}"));
        }
        if let Some(artist) = &metadata.artist {
            command.arg("-metadata").arg(format!("artist={artist}"));
        }
    }
    let mut video_output_index = 0;
    for source_index in plan.stream_order {
        let stream = plan
            .source_info
            .streams
            .iter()
            .find(|stream| stream_index(stream) == Some(*source_index))
            .expect("validated stream order contains every video source");
        if stream_kind(stream) != Some("video") {
            continue;
        }
        if let Some(settings) = plan
            .video_settings
            .get(source_index)
            .filter(|settings| requires_transcode(stream, settings))
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
            if let Some(filter) = resolution_filter(settings.resolution) {
                command
                    .arg(format!("-filter:v:{video_output_index}"))
                    .arg(filter);
            }
        }
        video_output_index += 1;
    }
    let mut audio_output_index = 0;
    for source_index in plan.stream_order {
        let stream = plan
            .source_info
            .streams
            .iter()
            .find(|stream| stream_index(stream) == Some(*source_index))
            .expect("validated stream order contains every audio source");
        if stream_kind(stream) != Some("audio") {
            continue;
        }
        if let Some(settings) = plan
            .audio_settings
            .get(source_index)
            .filter(|settings| audio_requires_transcode(stream, settings))
        {
            let codec = effective_audio_codec(stream, settings)
                .expect("audio settings are validated before building the ffmpeg command");
            let encoder = codec
                .encoder()
                .expect("supported audio target codecs have an encoder");
            command
                .arg(format!("-c:a:{audio_output_index}"))
                .arg(encoder);
            if let Some(channels) = settings.channel_layout.channels() {
                command
                    .arg(format!("-ac:a:{audio_output_index}"))
                    .arg(channels.to_string());
            }
            let source_rate = stream_sample_rate(stream)
                .expect("audio settings with a transcode have a validated source sample rate");
            let rate = resolved_audio_sample_rate(codec, source_rate)
                .expect("audio settings have a validated target sample rate");
            if rate != source_rate {
                command
                    .arg(format!("-ar:a:{audio_output_index}"))
                    .arg(rate.to_string());
            }
            // No bitrate for a channel count Reel has no preset for: the encoder's own
            // default is a far better outcome than killing the worker thread, which would
            // strand the whole pool and leave the batch reporting "processing" forever.
            if let Some(bitrate) = settings
                .channel_layout
                .channels()
                .or_else(|| stream_channels(stream))
                .filter(|_| !codec.is_lossless())
                .and_then(|channels| audio_bitrate_kbps(codec, channels))
            {
                command
                    .arg(format!("-b:a:{audio_output_index}"))
                    .arg(format!("{bitrate}k"));
            }
        }
        audio_output_index += 1;
    }
    for (output_index, track) in output_tracks.iter().enumerate() {
        match track {
            OutputTrack::Existing(source_index) => {
                command
                    .arg(format!("-map_metadata:s:{output_index}"))
                    .arg(format!("0:s:{source_index}"));
                let replacement = plan
                    .replacements
                    .iter()
                    .any(|replacement| replacement.source_index == *source_index);
                let stream = plan
                    .source_info
                    .streams
                    .iter()
                    .find(|stream| stream_index(stream) == Some(*source_index))
                    .expect("validated output tracks refer to existing source streams");
                let metadata_change = plan.subtitle_changes.iter().find(|change| {
                    change.source == SubtitleSource::Embedded(*source_index)
                        && change.metadata.is_some()
                });
                if stream_kind(stream) == Some("subtitle") {
                    command
                        .arg(format!("-metadata:s:{output_index}"))
                        .arg(format!("language={}", stream_language(stream)));
                }
                if should_write_audio_metadata(
                    stream_kind(stream),
                    plan.audio_settings.contains_key(source_index),
                    plan.container.is_some(),
                ) {
                    let metadata = audio_metadata_for_output(
                        stream,
                        plan.audio_settings.get(source_index),
                        metadata_container,
                    );
                    command
                        .arg(format!("-metadata:s:{output_index}"))
                        .arg(format!("language={}", metadata.language));
                    let title_key = if matches!(
                        metadata_container,
                        Some(ContainerFormat::Mp4 | ContainerFormat::Mov)
                    ) {
                        "handler_name"
                    } else {
                        "title"
                    };
                    command
                        .arg(format!("-metadata:s:{output_index}"))
                        .arg(format!(
                            "{title_key}={}",
                            metadata.title.as_deref().unwrap_or("")
                        ))
                        .arg(format!("-disposition:{output_index}"))
                        .arg(audio_disposition(
                            stream,
                            plan.default_streams.contains(source_index),
                            &metadata,
                        ));
                    continue;
                }
                if should_write_video_metadata(
                    stream_kind(stream),
                    plan.video_settings.contains_key(source_index),
                    plan.container.is_some(),
                ) {
                    let metadata = video_metadata_for_output(
                        stream,
                        plan.video_settings.get(source_index),
                        metadata_container,
                    );
                    command
                        .arg(format!("-metadata:s:{output_index}"))
                        .arg(format!("language={}", metadata.language));
                    let title_key = if matches!(
                        metadata_container,
                        Some(ContainerFormat::Mp4 | ContainerFormat::Mov)
                    ) {
                        "handler_name"
                    } else {
                        "title"
                    };
                    command
                        .arg(format!("-metadata:s:{output_index}"))
                        .arg(format!(
                            "{title_key}={}",
                            metadata.title.as_deref().unwrap_or("")
                        ))
                        .arg(format!("-disposition:{output_index}"))
                        .arg(video_disposition(
                            stream,
                            plan.default_streams.contains(source_index),
                            &metadata,
                        ));
                    continue;
                }
                if replacement || metadata_change.is_some() {
                    let mut metadata = metadata_change
                        .map(|change| effective_subtitle_metadata(change, stream_metadata(stream)))
                        .unwrap_or_else(|| stream_metadata(stream));
                    if let Some(container) = metadata_container {
                        container.retain_supported_subtitle_metadata(&mut metadata);
                    }
                    command
                        .arg(format!("-metadata:s:{output_index}"))
                        .arg(format!("language={}", metadata.language))
                        .arg(format!("-metadata:s:{output_index}"))
                        .arg(format!("title={}", metadata.title.as_deref().unwrap_or("")))
                        .arg(format!("-disposition:{output_index}"))
                        .arg(subtitle_disposition(
                            Some(stream),
                            plan.default_streams.contains(source_index),
                            &metadata,
                        ));
                } else {
                    command.arg(format!("-disposition:{output_index}")).arg(
                        if plan.default_streams.contains(source_index) {
                            "+default"
                        } else {
                            "-default"
                        },
                    );
                }
            }
            OutputTrack::Imported(import_index) => {
                let import = &plan.imports[*import_index];
                command
                    .arg(format!("-metadata:s:{output_index}"))
                    .arg(format!("language={}", import.metadata.language))
                    .arg(format!("-metadata:s:{output_index}"))
                    .arg(format!(
                        "title={}",
                        import.metadata.title.as_deref().unwrap_or("")
                    ))
                    .arg(format!("-disposition:{output_index}"))
                    .arg(subtitle_disposition(None, import.default, &import.metadata));
            }
        }
    }
    if let Some(container) = plan.container {
        // MOV shares MP4's ISO-BMFF/QuickTime muxer family and benefits identically:
        // without this, the `moov` atom lands at the end of the file, so opening or
        // seeking it (especially over the network mounts this app targets) requires a
        // full trailing scan instead of reading a few bytes from the front.
        if matches!(container, ContainerFormat::Mp4 | ContainerFormat::Mov) {
            command.args(["-movflags", "+faststart"]);
        }
        command.args(["-f", container.muxer()]);
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
            report_progress(EditProgress::new(EditPhase::StopTools));
            let _ = child.kill();
            was_cancelled = true;
            break;
        }
        if let Some(microseconds) = line
            .strip_prefix("out_time_us=")
            .and_then(|value| value.parse::<f64>().ok())
        {
            match plan.duration {
                Some(total) => report_progress(EditProgress::measured(
                    phase.clone(),
                    (microseconds / 1_000_000.0 / total).clamp(0.0, 0.97),
                )),
                None => report_progress(EditProgress::new(phase.clone())),
            }
        }
    }
    let status = child
        .wait()
        .map_err(|error| EditError::Failed(format!("Could not wait for ffmpeg: {error}")))?;
    let stderr = stderr_reader.join().unwrap_or_default();
    if was_cancelled || plan.cancelled.load(Ordering::Relaxed) {
        report_progress(EditProgress::new(EditPhase::Cleanup));
        return Err(EditError::Cancelled);
    }
    Ok(FfmpegOutput { status, stderr })
}

fn subtitle_disposition(
    source: Option<&BTreeMap<String, Value>>,
    default: bool,
    metadata: &SubtitleMetadata,
) -> String {
    let replaced = [
        "default",
        "forced",
        "hearing_impaired",
        "captions",
        "original",
        "comment",
    ];
    let mut dispositions = source
        .and_then(|stream| stream.get("disposition"))
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|values| values.iter())
        .filter(|(name, value)| value.as_i64() == Some(1) && !replaced.contains(&name.as_str()))
        .map(|(name, _)| name.clone())
        .collect::<BTreeSet<_>>();
    if default {
        dispositions.insert("default".to_string());
    }
    if metadata.forced {
        dispositions.insert("forced".to_string());
    }
    if metadata.cc {
        dispositions.insert("captions".to_string());
    }
    if metadata.hearing_impaired {
        dispositions.insert("hearing_impaired".to_string());
    }
    if metadata.original {
        dispositions.insert("original".to_string());
    }
    if metadata.commentary {
        dispositions.insert("comment".to_string());
    }
    if dispositions.is_empty() {
        "0".to_string()
    } else {
        dispositions.into_iter().collect::<Vec<_>>().join("+")
    }
}

fn audio_disposition(
    source: &BTreeMap<String, Value>,
    default: bool,
    metadata: &AudioMetadata,
) -> String {
    let replaced = [
        "default",
        "hearing_impaired",
        "visual_impaired",
        "original",
        "comment",
        "dub",
    ];
    let mut dispositions = source
        .get("disposition")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|values| values.iter())
        .filter(|(name, value)| value.as_i64() == Some(1) && !replaced.contains(&name.as_str()))
        .map(|(name, _)| name.clone())
        .collect::<BTreeSet<_>>();
    for (enabled, name) in [
        (default, "default"),
        (metadata.commentary, "comment"),
        (metadata.hearing_impaired, "hearing_impaired"),
        (metadata.audio_description, "visual_impaired"),
        (metadata.original, "original"),
        (metadata.dubbed, "dub"),
    ] {
        if enabled {
            dispositions.insert(name.to_string());
        }
    }
    if dispositions.is_empty() {
        "0".to_string()
    } else {
        dispositions.into_iter().collect::<Vec<_>>().join("+")
    }
}

/// Mirrors `audio_disposition`/`subtitle_disposition`, but video models only `default`
/// and `comment` — the roles audio offers besides those describe a soundtrack's language
/// or its accessibility variant, neither of which a picture has. Any other flag already
/// on the source stream (e.g. `attached_pic`) is preserved.
fn video_disposition(
    stream: &BTreeMap<String, Value>,
    default: bool,
    metadata: &VideoMetadata,
) -> String {
    let replaced = ["default", "comment"];
    let mut dispositions = stream
        .get("disposition")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|values| values.iter())
        .filter(|(name, value)| value.as_i64() == Some(1) && !replaced.contains(&name.as_str()))
        .map(|(name, _)| name.clone())
        .collect::<BTreeSet<_>>();
    for (enabled, name) in [(default, "default"), (metadata.commentary, "comment")] {
        if enabled {
            dispositions.insert(name.to_string());
        }
    }
    if dispositions.is_empty() {
        "0".to_string()
    } else {
        dispositions.into_iter().collect::<Vec<_>>().join("+")
    }
}

pub(crate) fn media_duration(info: &MediaInfo) -> Option<f64> {
    info.format
        .get("duration")
        .and_then(|value| match value {
            Value::String(value) => value.parse().ok(),
            Value::Number(value) => value.as_f64(),
            _ => None,
        })
        .filter(|duration| duration.is_finite() && *duration > 0.0)
}

/// Where an edit's intermediate files are written. Beside the source on a local disk,
/// so publishing is a rename; on local scratch for a network mount, so the remux does
/// not stream every byte back and forth over the wire before the single final publish.
fn work_parent(path: &Path, is_network: bool) -> Result<PathBuf, String> {
    if is_network {
        let scratch = std::env::temp_dir().join("reel-tui-scratch");
        let _ = fs::create_dir_all(&scratch);
        return Ok(scratch);
    }
    path.parent()
        .ok_or_else(|| "The source file has no parent directory.".to_string())
        .map(Path::to_path_buf)
}

fn temporary_path(path: &Path, container: Option<ContainerFormat>) -> Result<PathBuf, String> {
    let parent = work_parent(path, crate::mount::is_network_mount(path))?;
    let stem = path
        .file_stem()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "The source filename is not valid UTF-8.".to_string())?;
    let extension = container
        .map(ContainerFormat::extension)
        .or_else(|| path.extension().and_then(|extension| extension.to_str()));
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Ok(parent.join(match extension {
        Some(extension) => format!(".reel-tui-{nonce}-{stem}.{extension}"),
        None => format!(".reel-tui-{nonce}-{stem}"),
    }))
}

fn temporary_workspace(path: &Path) -> Result<PathBuf, String> {
    let parent = work_parent(path, crate::mount::is_network_mount(path))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Ok(parent.join(format!(".reel-tui-{nonce}-subtitle-work")))
}

fn run_cancellable_output(
    command: &mut Command,
    cancelled: &AtomicBool,
    report_progress: &mut ProgressReporter<'_>,
) -> Result<std::process::Output, EditError> {
    let program = command.get_program().to_string_lossy().to_string();
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| {
        EditError::Failed(if error.kind() == std::io::ErrorKind::NotFound {
            format!("{program} was not found in PATH.")
        } else {
            format!("Could not start {program}: {error}")
        })
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| EditError::Failed(format!("Could not capture {program} output.")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| EditError::Failed(format!("Could not capture {program} errors.")))?;
    let stdout_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = BufReader::new(stdout).read_to_end(&mut bytes);
        bytes
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = BufReader::new(stderr).read_to_end(&mut bytes);
        bytes
    });
    let status = loop {
        if cancelled.load(Ordering::Relaxed) {
            report_progress(EditProgress::new(EditPhase::StopTools));
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            report_progress(EditProgress::new(EditPhase::Cleanup));
            return Err(EditError::Cancelled);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(EditError::Failed(format!(
                    "Could not wait for {program}: {error}"
                )));
            }
        }
    };
    Ok(std::process::Output {
        status,
        stdout: stdout_reader.join().unwrap_or_default(),
        stderr: stderr_reader.join().unwrap_or_default(),
    })
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

fn requires_transcode(stream: &BTreeMap<String, Value>, settings: &VideoSettings) -> bool {
    settings.resolution != VideoResolution::Original
        || settings
            .codec
            .codec_name()
            .is_some_and(|target| stream.get("codec_name").and_then(Value::as_str) != Some(target))
}

pub(crate) fn effective_audio_codec(
    stream: &BTreeMap<String, Value>,
    settings: &AudioSettings,
) -> Option<AudioCodec> {
    match settings.codec {
        AudioCodec::Original => stream
            .get("codec_name")
            .and_then(Value::as_str)
            .and_then(AudioCodec::from_codec_name),
        codec => Some(codec),
    }
}

pub(crate) fn stream_channels(stream: &BTreeMap<String, Value>) -> Option<u8> {
    stream
        .get("channels")
        .and_then(Value::as_u64)
        .and_then(|channels| u8::try_from(channels).ok())
}

pub(crate) fn stream_sample_rate(stream: &BTreeMap<String, Value>) -> Option<u32> {
    stream.get("sample_rate").and_then(|value| match value {
        Value::String(rate) => rate.parse().ok(),
        Value::Number(rate) => rate.as_u64().and_then(|rate| u32::try_from(rate).ok()),
        _ => None,
    })
}

/// The rate the encoder is actually asked for: the source rate when the codec accepts it,
/// otherwise the highest candidate below it that the codec does accept. `None` means no
/// candidate fits, which validation turns into an actionable "choose another codec".
fn resolved_audio_sample_rate(codec: AudioCodec, source_rate: u32) -> Option<u32> {
    if codec.supports_sample_rate(source_rate) {
        return Some(source_rate);
    }
    AUDIO_SAMPLE_RATE_CANDIDATES
        .into_iter()
        .find(|rate| *rate <= source_rate && codec.supports_sample_rate(*rate))
}

pub(crate) fn audio_requires_transcode(
    stream: &BTreeMap<String, Value>,
    settings: &AudioSettings,
) -> bool {
    settings
        .codec
        .codec_name()
        .is_some_and(|target| stream.get("codec_name").and_then(Value::as_str) != Some(target))
        || settings.channel_layout != AudioChannelLayout::Original
}

#[cfg(test)]
fn audio_settings_require_encode(settings: &AudioSettings) -> bool {
    settings.codec != AudioCodec::Original
        || settings.channel_layout != AudioChannelLayout::Original
}

fn audio_metadata(stream: &BTreeMap<String, Value>) -> AudioMetadata {
    AudioMetadata {
        language: stream_language(stream),
        title: audio_stream_title(stream),
        commentary: stream_commentary(stream),
        hearing_impaired: stream_hearing_impaired(stream),
        audio_description: stream_disposition(stream, "visual_impaired"),
        original: stream_original(stream),
        dubbed: stream_disposition(stream, "dub"),
    }
}

fn audio_metadata_for_output(
    stream: &BTreeMap<String, Value>,
    settings: Option<&AudioSettings>,
    container: Option<ContainerFormat>,
) -> AudioMetadata {
    let mut metadata = settings
        .map(|settings| settings.metadata.clone())
        .unwrap_or_else(|| audio_metadata(stream));
    if let Some(container) = container {
        container.retain_supported_audio_metadata(&mut metadata);
    }
    metadata
}

fn should_write_audio_metadata(
    stream_kind: Option<&str>,
    has_settings: bool,
    changing_container: bool,
) -> bool {
    stream_kind == Some("audio") && (has_settings || changing_container)
}

fn video_metadata(stream: &BTreeMap<String, Value>) -> VideoMetadata {
    VideoMetadata {
        language: stream_language(stream),
        title: video_stream_title(stream),
        commentary: stream_commentary(stream),
    }
}

fn video_metadata_for_output(
    stream: &BTreeMap<String, Value>,
    settings: Option<&VideoSettings>,
    container: Option<ContainerFormat>,
) -> VideoMetadata {
    let mut metadata = settings
        .map(|settings| settings.metadata.clone())
        .unwrap_or_else(|| video_metadata(stream));
    if let Some(container) = container {
        container.retain_supported_video_metadata(&mut metadata);
    }
    metadata
}

fn should_write_video_metadata(
    stream_kind: Option<&str>,
    has_settings: bool,
    changing_container: bool,
) -> bool {
    stream_kind == Some("video") && (has_settings || changing_container)
}

/// The `-display_rotation` arguments for every video track carrying staged settings,
/// paired with the stream specifier they apply to.
///
/// ffmpeg counts video tracks separately here (`v:0`, `v:1`, ...) rather than using the
/// absolute stream index reel keys its plans on, and the count is over the *input* file,
/// so it follows the source's own ordering rather than the output track plan.
///
/// Only staged tracks get an argument. A copy remux carries an existing display matrix
/// across untouched, so a container change alone is no reason to rewrite one — unlike the
/// metadata `should_write_video_metadata` gates, which a container change does rewrite.
fn display_rotation_args(
    info: &MediaInfo,
    video_settings: &BTreeMap<u64, VideoSettings>,
) -> Vec<(String, String)> {
    let mut args = Vec::new();
    let mut video_index = 0;
    for stream in &info.streams {
        if stream_kind(stream) != Some("video") {
            continue;
        }
        if let Some(settings) = stream_index(stream).and_then(|index| video_settings.get(&index)) {
            args.push((
                format!("v:{video_index}"),
                settings.rotation.degrees().to_string(),
            ));
        }
        video_index += 1;
    }
    args
}

/// The bitrate Reel picks for a lossy encode. Deliberately not user-selectable: the readme
/// promises safe automatic values, so there is one well-tempered rate per codec and channel
/// count rather than a quality dial. `None` means the pair is not a lossy encode Reel offers,
/// which validation rejects before the command is ever built.
pub(crate) fn audio_bitrate_kbps(codec: AudioCodec, channels: u8) -> Option<u32> {
    Some(match (codec, channels) {
        (AudioCodec::Aac, 1) => 96,
        (AudioCodec::Aac, 2) => 192,
        (AudioCodec::Aac, 3..=6) => 512,
        (AudioCodec::Aac, 7..=8) => 640,
        (AudioCodec::Ac3, 1) | (AudioCodec::Eac3, 1) => 128,
        (AudioCodec::Ac3, 2) => 256,
        (AudioCodec::Ac3, 3..=6) => 448,
        (AudioCodec::Eac3, 2) => 192,
        (AudioCodec::Eac3, 3..=6) => 640,
        (AudioCodec::Opus, 1) => 64,
        (AudioCodec::Opus, 2) => 128,
        (AudioCodec::Opus, 3..=6) => 384,
        (AudioCodec::Opus, 7..=8) => 512,
        (AudioCodec::Mp3, 1) => 96,
        (AudioCodec::Mp3, 2) => 192,
        (AudioCodec::Vorbis, 1) => 80,
        (AudioCodec::Vorbis, 2) => 160,
        (AudioCodec::Vorbis, 3..=6) => 384,
        (AudioCodec::Vorbis, 7..=8) => 512,
        _ => return None,
    })
}

fn resolution_filter(resolution: VideoResolution) -> Option<String> {
    match resolution {
        VideoResolution::Original => None,
        VideoResolution::Custom(custom) => {
            let width = custom.width;
            let height = custom.height;
            Some(match custom.scaling {
                CustomScaling::FitPad => format!(
                    "scale={width}:{height}:force_original_aspect_ratio=decrease:force_divisible_by=2,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2:black,setsar=1"
                ),
                CustomScaling::Stretch => format!("scale={width}:{height},setsar=1"),
            })
        }
        preset => preset.dimensions().map(|(width, height)| {
            format!(
                "scale={width}:{height}:force_original_aspect_ratio=decrease:force_divisible_by=2,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2:black,setsar=1"
            )
        }),
    }
}

/// Whether an encoded track came out the size the settings asked for.
///
/// A preset or custom size is the whole answer on its own: the scale filter fits the
/// picture into exactly that frame, padding it if a rotation made it portrait. Keeping
/// the original size is the case rotation changes — an encode bakes a 90°/270° rotation
/// into the picture, so the output is the source's dimensions swapped.
fn output_resolution_matches(
    stream: &BTreeMap<String, Value>,
    source: Option<&BTreeMap<String, Value>>,
    settings: &VideoSettings,
) -> bool {
    let width = stream_dimension(stream, "width");
    let height = stream_dimension(stream, "height");
    match settings.resolution {
        VideoResolution::Original => {
            let Some(source) = source.filter(|_| settings.rotation.swaps_dimensions()) else {
                return true;
            };
            match (
                stream_dimension(source, "width"),
                stream_dimension(source, "height"),
            ) {
                (Some(source_width), Some(source_height)) => {
                    width == Some(source_height) && height == Some(source_width)
                }
                _ => true,
            }
        }
        VideoResolution::Custom(custom) => {
            width == Some(custom.width) && height == Some(custom.height)
        }
        preset => preset
            .dimensions()
            .is_some_and(|(target_width, target_height)| {
                width == Some(target_width) && height == Some(target_height)
            }),
    }
}

/// The smallest custom width or height any of the encoders below will accept.
///
/// `libx265` refuses anything under 16 pixels in either direction ("Image size is too
/// small"), and it only says so once the encoder is opening — after the whole pipeline
/// has already run. `libx264` and `libsvtav1` go lower, but a single floor keeps the
/// rule explainable, and 16 is well beneath any real video.
pub const MINIMUM_CUSTOM_DIMENSION: u64 = 16;

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

struct DirectoryCleanup(Option<PathBuf>);

impl Drop for DirectoryCleanup {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = fs::remove_dir_all(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use kernal::prelude::*;

    use super::*;
    use crate::subtitle::CueSnapshot;
    use std::process::Stdio;

    fn media(streams: Value) -> MediaInfo {
        MediaInfo::from_json(serde_json::json!({"streams": streams})).unwrap()
    }

    /// A staged rewrite of one cue's words, leaving its timing exactly as the file has it.
    ///
    /// The timing is supplied rather than defaulted because the writer checks the whole cue
    /// against the file before it applies anything — see `subtitle::CueSnapshot`.
    fn cue_words(original: &str, text: &str, start: u64, end: u64) -> CueEdit {
        let (start, end) = (Duration::from_secs(start), Duration::from_secs(end));
        CueEdit {
            original: CueSnapshot {
                text: original.to_string(),
                start,
                end,
            },
            text: text.to_string(),
            start,
            end,
        }
    }

    /// Each entry is a program name, optionally narrowed to one encoder as
    /// `"ffmpeg:libx264"`.
    fn tool_available(tool: &str) -> bool {
        let (program, encoder) = match tool.split_once(':') {
            Some((program, encoder)) => (program, Some(encoder)),
            None => (tool, None),
        };
        let succeeds = |arguments: &[&str]| {
            Command::new(program)
                .args(arguments)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        };
        // ffmpeg spells it `-version`, most other tools `--version`. The encoder
        // form has to consult `-encoders`: `ffmpeg -h encoder=<name>` exits 0 for
        // any name, including ones it has never heard of.
        match encoder {
            Some(encoder) => Command::new(program)
                .args(["-hide_banner", "-encoders"])
                .output()
                .is_ok_and(|output| {
                    output.status.success()
                        && String::from_utf8_lossy(&output.stdout)
                            .lines()
                            .filter_map(|line| line.split_whitespace().nth(1))
                            .any(|name| name == encoder)
                }),
            None => succeeds(&["-version"]) || succeeds(&["--version"]),
        }
    }

    fn require_tools(test: &str, tools: &[&str]) {
        for tool in tools {
            assert!(
                tool_available(tool),
                "{test} requires {tool}; install the missing test prerequisite"
            );
        }
    }

    /// Regression test for a check that never checked: the old helper narrowed to an
    /// encoder by running `ffmpeg -h encoder=<name>`, which exits 0 for names ffmpeg
    /// has never heard of.
    #[test]
    fn tool_availability_should_reject_an_encoder_ffmpeg_does_not_have() {
        // Arrange / Act: a name no build ships, checked alongside the suite's real
        // ffmpeg prerequisite.
        require_tools(
            "tool_availability_should_reject_an_encoder_ffmpeg_does_not_have",
            &["ffmpeg"],
        );
        let bogus = tool_available("ffmpeg:definitely_not_a_real_encoder");
        let missing_program = tool_available("definitely-not-a-real-program");

        // Assert
        assert_that!(bogus).is_false();
        assert_that!(missing_program).is_false();
        assert_that!(tool_available("ffmpeg")).is_true();
    }

    fn english_subtitle_metadata() -> SubtitleMetadata {
        SubtitleMetadata {
            language: "eng".to_string(),
            title: None,
            forced: false,
            cc: false,
            hearing_impaired: false,
            original: false,
            commentary: false,
        }
    }

    fn subtitle_change(
        source_format: SubtitleFormat,
        embedded_target: Option<SubtitleFormat>,
        export_target: Option<SubtitleFormat>,
        import_into_media: bool,
        ocr_language: Option<&str>,
    ) -> SubtitleChange {
        SubtitleChange {
            cue_edits: Default::default(),
            source: SubtitleSource::Sidecar(PathBuf::from("movie.eng.sup")),
            source_format,
            embedded_target,
            export_target,
            import_into_media,
            ocr_language: ocr_language.map(str::to_string),
            metadata: None,
        }
    }

    #[test]
    fn subtitle_preparation_progress_should_describe_the_actual_conversion_phase() {
        // Arrange
        let ocr_import = subtitle_change(
            SubtitleFormat::Pgs,
            Some(SubtitleFormat::SubRip),
            None,
            true,
            Some("dan"),
        );
        let converted_import = subtitle_change(
            SubtitleFormat::Ass,
            Some(SubtitleFormat::SubRip),
            None,
            true,
            None,
        );
        let copied_import = subtitle_change(
            SubtitleFormat::SubRip,
            Some(SubtitleFormat::SubRip),
            None,
            true,
            None,
        );
        let ocr_export = subtitle_change(
            SubtitleFormat::VobSub,
            None,
            Some(SubtitleFormat::SubRip),
            false,
            Some("nld"),
        );

        // Act
        let labels = [
            subtitle_preparation_phase(
                "movie.dan.sup",
                &ocr_import,
                SubtitleFormat::SubRip,
                SubtitlePreparation::Import,
            ),
            subtitle_preparation_phase(
                "movie.eng.ass",
                &converted_import,
                SubtitleFormat::SubRip,
                SubtitlePreparation::Import,
            ),
            subtitle_preparation_phase(
                "movie.eng.srt",
                &copied_import,
                SubtitleFormat::SubRip,
                SubtitlePreparation::Import,
            ),
            subtitle_preparation_phase(
                "subtitle track #3",
                &ocr_export,
                SubtitleFormat::SubRip,
                SubtitlePreparation::Export,
            ),
            subtitle_preparation_phase(
                "subtitle #4",
                &copied_import,
                SubtitleFormat::SubRip,
                SubtitlePreparation::Embed,
            ),
            subtitle_preparation_phase(
                "subtitle #5",
                &copied_import,
                SubtitleFormat::SubRip,
                SubtitlePreparation::Export,
            ),
            subtitle_preparation_phase(
                "movie.eng.srt",
                &copied_import,
                SubtitleFormat::SubRip,
                SubtitlePreparation::Sidecar,
            ),
            subtitle_preparation_phase(
                "subtitle #6",
                &converted_import,
                SubtitleFormat::SubRip,
                SubtitlePreparation::Embed,
            ),
            subtitle_preparation_phase(
                "subtitle #7",
                &ocr_import,
                SubtitleFormat::SubRip,
                SubtitlePreparation::Embed,
            ),
            subtitle_preparation_phase(
                "movie.eng.ass",
                &converted_import,
                SubtitleFormat::SubRip,
                SubtitlePreparation::Sidecar,
            ),
        ]
        .map(|phase| phase.label());

        // Assert
        assert_that!(labels).contains_exactly_in_given_order([
            "Running OCR on movie.dan.sup → SRT (dan)".to_string(),
            "Converting movie.eng.ass → SRT".to_string(),
            "Copying movie.eng.srt".to_string(),
            "Running OCR on subtitle track #3 → SRT (nld)".to_string(),
            "Extracting subtitle #4".to_string(),
            "Extracting subtitle #5".to_string(),
            "Updating movie.eng.srt".to_string(),
            "Converting subtitle #6 → SRT".to_string(),
            "Running OCR on subtitle #7 → SRT (dan)".to_string(),
            "Converting movie.eng.ass → SRT".to_string(),
        ]);
    }

    #[test]
    fn every_edit_phase_should_have_a_concise_action_label() {
        let phases = [
            EditPhase::InspectSource,
            EditPhase::ValidateChanges,
            EditPhase::CreateWorkspace,
            EditPhase::ExtractSubtitle("subtitle #3".to_string()),
            EditPhase::CopySubtitle("movie.eng.srt".to_string()),
            EditPhase::UpdateSubtitle("movie.eng.srt".to_string()),
            EditPhase::ConvertSubtitle {
                subject: "movie.eng.ass".to_string(),
                target: "SRT".to_string(),
            },
            EditPhase::OcrSubtitle {
                subject: "movie.dan.sup".to_string(),
                target: "SRT".to_string(),
                language: "dan".to_string(),
            },
            EditPhase::OcrSubtitle {
                subject: "movie.with.an.extremely.long.descriptive.subtitle.filename.dan.sup"
                    .to_string(),
                target: "SRT".to_string(),
                language: "dan".to_string(),
            },
            EditPhase::ValidateSubtitle("movie.eng.srt".to_string()),
            EditPhase::ResolveNames,
            EditPhase::WriteMedia("Encoding video".to_string()),
            EditPhase::ValidateOutput,
            EditPhase::RecheckSources,
            EditPhase::PreservePermissions,
            EditPhase::PreparePublication,
            EditPhase::BackupFile("movie.mkv".to_string()),
            EditPhase::PublishFile("movie.mkv".to_string()),
            EditPhase::RemoveBackups,
            EditPhase::Rollback,
            EditPhase::StopTools,
            EditPhase::Cleanup,
            EditPhase::Finished,
        ];

        let labels = phases.map(|phase| phase.label());

        assert_that!(labels.iter().all(|label| !label.is_empty())).is_true();
        assert_that!(labels.iter().all(|label| !label.contains("for import"))).is_true();
        assert_that!(labels.iter().all(|label| !label.contains("for export"))).is_true();
        assert_that!(labels.iter().all(|label| label.chars().count() <= 60)).is_true();
    }

    #[test]
    fn media_write_progress_should_describe_the_actual_operation() {
        let settings = BTreeMap::from([(
            0,
            VideoSettings {
                codec: VideoCodec::H264,
                resolution: VideoResolution::P720,
                metadata: VideoMetadata {
                    language: "und".to_string(),
                    title: None,
                    commentary: false,
                },
                rotation: VideoRotation::None,
            },
        )]);

        let video_encode = !settings.is_empty();

        assert_that!(media_write_label(None, false, false, false))
            .is_equal_to("Remuxing media".to_string());
        assert_that!(media_write_label(None, false, false, true))
            .is_equal_to("Updating container metadata".to_string());
        assert_that!(media_write_label(
            Some(ContainerFormat::Mp4),
            false,
            false,
            false
        ))
        .is_equal_to("Remuxing to MP4".to_string());
        assert_that!(media_write_label(None, false, video_encode, false))
            .is_equal_to("Encoding video".to_string());
        assert_that!(media_write_label(
            Some(ContainerFormat::Mp4),
            false,
            video_encode,
            false
        ))
        .is_equal_to("Encoding video and remuxing to MP4".to_string());
        assert_that!(media_write_label(None, true, false, false))
            .is_equal_to("Encoding audio".to_string());
        assert_that!(media_write_label(None, true, true, false))
            .is_equal_to("Encoding video and audio".to_string());
    }

    #[test]
    fn staged_file_copy_should_report_measured_byte_progress() {
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-copy-progress-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("source.bin");
        let destination = directory.join("destination.bin");
        fs::write(&source, vec![7_u8; 2 * 1024 * 1024 + 17]).unwrap();
        let mut progress = Vec::new();

        copy_file_with_progress(
            &source,
            &destination,
            EditPhase::PublishFile("destination.bin".to_string()),
            &AtomicBool::new(false),
            &mut |update| progress.push(update),
        )
        .unwrap();

        assert_that!(fs::read(&destination).unwrap()).is_equal_to(fs::read(&source).unwrap());
        assert_that!(progress.first().unwrap().fraction).is_none();
        assert_that!(progress.last().unwrap().fraction).contains(1.0);
        assert_that!(progress
            .iter()
            .all(|update| update.phase == EditPhase::PublishFile("destination.bin".to_string())))
        .is_true();

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn container_format_should_detect_extensions_and_enforce_codec_compatibility() {
        // Act
        let matroska = ContainerFormat::from_path(Path::new("/videos/movie.MKV"));
        let mp4 = ContainerFormat::from_path(Path::new("/videos/movie.m4v"));
        let mov = ContainerFormat::from_path(Path::new("/videos/movie.mov"));
        let webm = ContainerFormat::from_path(Path::new("/videos/movie.webm"));

        // Assert
        assert_that!(matroska).contains(ContainerFormat::Matroska);
        assert_that!(mp4).contains(ContainerFormat::Mp4);
        assert_that!(mov).contains(ContainerFormat::Mov);
        assert_that!(webm).contains(ContainerFormat::WebM);
        assert_that!(ContainerFormat::Mp4.supports_codec("video", "h264", false)).is_true();
        assert_that!(ContainerFormat::Mp4.supports_codec("subtitle", "subrip", false)).is_false();
        assert_that!(ContainerFormat::Mov.supports_codec("audio", "pcm_s16le", false)).is_true();
        assert_that!(ContainerFormat::WebM.supports_codec("video", "h264", false)).is_false();
        assert_that!(ContainerFormat::WebM.supports_codec("video", "av1", false)).is_true();
    }

    #[test]
    fn output_container_validation_should_require_both_the_extension_and_the_real_format() {
        // Arrange: this runs after ffmpeg finishes and before the result replaces the
        // user's file. Checking only the extension would pass a file ffmpeg wrote in the
        // wrong container; checking only the probed format would pass a valid MP4 left
        // sitting at a `.mkv` name that players then mis-handle. Both must hold.
        let matroska = media(serde_json::json!([{"index": 0, "codec_type": "video"}]));
        let mut matroska = matroska;
        matroska.format = BTreeMap::from([(
            "format_name".to_string(),
            serde_json::json!("matroska,webm"),
        )]);
        let mut mp4 = media(serde_json::json!([{"index": 0, "codec_type": "video"}]));
        mp4.format = BTreeMap::from([(
            "format_name".to_string(),
            serde_json::json!("mov,mp4,m4a,3gp,3g2,mj2"),
        )]);

        // Act / Assert: matching name and contents pass.
        assert_that!(validate_output_container(
            &matroska,
            Path::new("/videos/movie.mkv"),
            ContainerFormat::Matroska,
        ))
        .is_ok();
        assert_that!(validate_output_container(
            &mp4,
            Path::new("/videos/movie.mp4"),
            ContainerFormat::Mp4,
        ))
        .is_ok();
        // WebM and MOV share their families' probed names.
        assert_that!(validate_output_container(
            &matroska,
            Path::new("/videos/movie.webm"),
            ContainerFormat::WebM,
        ))
        .is_ok();
        assert_that!(validate_output_container(
            &mp4,
            Path::new("/videos/movie.mov"),
            ContainerFormat::Mov,
        ))
        .is_ok();

        // Act / Assert: right extension, wrong contents.
        let wrong_contents = validate_output_container(
            &matroska,
            Path::new("/videos/movie.mp4"),
            ContainerFormat::Mp4,
        );
        assert_that!(wrong_contents)
            .contains_error("The completed file is not a valid MP4 container.".to_string());

        // Act / Assert: right contents, wrong extension.
        assert_that!(validate_output_container(
            &mp4,
            Path::new("/videos/movie.mkv"),
            ContainerFormat::Mp4,
        ))
        .is_err();

        // Act / Assert: a file ffprobe could not identify at all.
        let unknown = media(serde_json::json!([{"index": 0, "codec_type": "video"}]));
        assert_that!(validate_output_container(
            &unknown,
            Path::new("/videos/movie.mkv"),
            ContainerFormat::Matroska,
        ))
        .is_err();
    }

    #[test]
    fn subtitle_metadata_verification_should_notice_any_single_field_disagreeing() {
        // Arrange: after a remux the written subtitle metadata is compared against what
        // was staged. Every field is checked, and a field dropped from the comparison
        // would let a save report success while silently discarding that one setting —
        // the user only finds out later, in a player.
        let expected = SubtitleMetadata {
            language: "nld".to_string(),
            title: Some("Dutch".to_string()),
            forced: true,
            cc: false,
            hearing_impaired: true,
            original: false,
            commentary: true,
        };
        let matching = serde_json::from_value::<BTreeMap<String, Value>>(serde_json::json!({
            "tags": {"language": "nld", "title": "Dutch"},
            "disposition": {
                "forced": 1,
                "captions": 0,
                "hearing_impaired": 1,
                "original": 0,
                "comment": 1
            }
        }))
        .unwrap();

        // Act / Assert: the fully-agreeing stream passes.
        assert!(subtitle_metadata_matches(&matching, &expected));

        // Act / Assert: changing any one expected field is detected.
        type Mutation = (&'static str, fn(&mut SubtitleMetadata));
        let mutations: [Mutation; 8] = [
            ("language", |m| m.language = "eng".to_string()),
            ("title", |m| m.title = Some("English".to_string())),
            ("cleared title", |m| m.title = None),
            ("forced", |m| m.forced = false),
            ("cc", |m| m.cc = true),
            ("hearing impaired", |m| m.hearing_impaired = false),
            ("original", |m| m.original = true),
            ("commentary", |m| m.commentary = false),
        ];
        for (label, mutate) in mutations {
            let mut altered = expected.clone();
            mutate(&mut altered);
            assert!(
                !subtitle_metadata_matches(&matching, &altered),
                "a disagreeing {label} must be detected",
            );
        }
    }

    #[test]
    fn detect_should_recognise_an_mp4_hiding_behind_a_matroska_extension() {
        // Arrange: the mirror of the case below — an MP4 renamed to `.mkv`. The Matroska
        // arm is checked first, so only this direction proves the MP4 fallback is reached
        // at all. Getting it wrong means offering Matroska-only subtitle codecs for a file
        // ffmpeg will then refuse to mux.
        let renamed = Path::new("/videos/movie.mkv");

        // Act / Assert
        assert_that!(ContainerFormat::detect(
            renamed,
            Some("mov,mp4,m4a,3gp,3g2,mj2")
        ))
        .contains(ContainerFormat::Mp4);

        // A `.webm` holding real MP4 bytes resolves the same way.
        assert_that!(ContainerFormat::detect(
            Path::new("/videos/movie.webm"),
            Some("mov,mp4,m4a,3gp,3g2,mj2")
        ))
        .contains(ContainerFormat::Mp4);
    }

    #[test]
    fn cover_art_should_be_accepted_only_where_the_container_can_carry_it() {
        // Arrange / Act / Assert: an attached picture is judged by different rules than a
        // real video track — Matroska takes any codec as an attachment, ISO-BMFF only the
        // two image codecs it defines, and WebM has no attached-picture concept at all.
        // Judging cover art by the ordinary video rules would let a save through that
        // ffmpeg rejects at mux time, after the encode has already run.
        assert!(ContainerFormat::Matroska.supports_codec("video", "mjpeg", true));
        assert!(ContainerFormat::Matroska.supports_codec("video", "webp", true));
        assert!(ContainerFormat::Mp4.supports_codec("video", "mjpeg", true));
        assert!(ContainerFormat::Mp4.supports_codec("video", "png", true));
        // ISO-BMFF refuses anything else as cover art, even codecs it takes as video.
        assert!(!ContainerFormat::Mp4.supports_codec("video", "h264", true));
        assert!(!ContainerFormat::Mov.supports_codec("video", "webp", true));
        // WebM carries no cover art at all.
        assert!(!ContainerFormat::WebM.supports_codec("video", "png", true));
        assert!(!ContainerFormat::WebM.supports_codec("video", "mjpeg", true));
    }

    #[test]
    fn detect_should_prefer_the_probed_format_over_a_mismatched_extension() {
        // Arrange: a `.mkv` renamed to `.mp4` — the bytes on disk, and so ffprobe's
        // `format_name`, are still genuinely Matroska.
        let renamed = Path::new("/videos/movie.mp4");

        // Act / Assert: a mismatched extension loses to what's actually on disk.
        assert_that!(ContainerFormat::detect(renamed, Some("matroska,webm")))
            .contains(ContainerFormat::Matroska);

        // An extension that does agree with the probed family still picks the exact
        // member within that family — ffprobe alone can't tell MKV from WebM, or MP4
        // from MOV, so the extension remains the tiebreaker when it isn't lying.
        assert_that!(ContainerFormat::detect(
            Path::new("/videos/movie.webm"),
            Some("matroska,webm")
        ))
        .contains(ContainerFormat::WebM);
        assert_that!(ContainerFormat::detect(
            Path::new("/videos/movie.mov"),
            Some("mov,mp4,m4a,3gp,3g2,mj2")
        ))
        .contains(ContainerFormat::Mov);

        // No probe data yet (file not opened) — extension is all there is to go on.
        assert_that!(ContainerFormat::detect(renamed, None)).contains(ContainerFormat::Mp4);

        // An unrecognized format_name (e.g. a non-video file) falls back to whatever
        // the extension says rather than reporting nothing.
        assert_that!(ContainerFormat::detect(renamed, Some("wav"))).contains(ContainerFormat::Mp4);
    }

    #[test]
    fn container_conflicts_should_consider_final_order_and_staged_conversions() {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264"},
            {"index": 1, "codec_type": "audio", "codec_name": "aac"},
            {"index": 2, "codec_type": "subtitle", "codec_name": "subrip"},
            {"index": 3, "codec_type": "subtitle", "codec_name": "ass"}
        ]));
        let video_settings = BTreeMap::from([(
            0,
            VideoSettings {
                codec: VideoCodec::Av1,
                resolution: VideoResolution::Original,
                metadata: VideoMetadata {
                    language: "und".to_string(),
                    title: None,
                    commentary: false,
                },
                rotation: VideoRotation::None,
            },
        )]);
        let subtitle_changes = [SubtitleChange {
            cue_edits: Default::default(),
            source: SubtitleSource::Embedded(2),
            source_format: SubtitleFormat::SubRip,
            embedded_target: Some(SubtitleFormat::MovText),
            export_target: None,
            import_into_media: false,
            ocr_language: None,

            metadata: None,
        }];

        // Act
        let conflicts = container_conflicts(
            &info,
            &[0, 1, 2],
            &BTreeMap::new(),
            &video_settings,
            &subtitle_changes,
            ContainerFormat::Mp4,
        );

        // Assert
        assert_that!(conflicts).is_empty();
    }

    #[test]
    fn container_conflicts_should_ignore_an_incompatible_subtitle_staged_for_export() {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264"},
            {"index": 1, "codec_type": "audio", "codec_name": "aac"},
            {"index": 2, "codec_type": "subtitle", "codec_name": "subrip"}
        ]));
        let subtitle_changes = [SubtitleChange {
            cue_edits: Default::default(),
            source: SubtitleSource::Embedded(2),
            source_format: SubtitleFormat::SubRip,
            embedded_target: Some(SubtitleFormat::VobSub),
            export_target: Some(SubtitleFormat::VobSub),
            import_into_media: false,
            ocr_language: None,
            metadata: Some(SubtitleMetadata {
                language: "dan".to_string(),
                title: None,
                forced: true,
                cc: true,
                hearing_impaired: true,
                original: true,
                commentary: true,
            }),
        }];

        // Act
        let conflicts = container_conflicts(
            &info,
            &[0, 1, 2],
            &BTreeMap::new(),
            &BTreeMap::new(),
            &subtitle_changes,
            ContainerFormat::Mp4,
        );
        let metadata_conflicts =
            subtitle_metadata_conflicts(&info, &subtitle_changes, &[], ContainerFormat::Mp4, true);

        // Assert
        assert_that!(conflicts).is_empty();
        assert_that!(metadata_conflicts).is_empty();
    }

    #[test]
    fn imported_subtitle_conflicts_should_clear_after_a_compatible_conversion() {
        // Arrange
        let path = PathBuf::from("/videos/movie.eng.srt");
        let sidecars = [SidecarEntry {
            path: path.clone(),
            companion: None,
            display_name: "movie.eng.srt".to_string(),
            format: SubtitleFormat::SubRip,
            language: "eng".to_string(),
            forced: false,
            hearing_impaired: false,
            number: None,
            fingerprint: FileFingerprint {
                length: 0,
                modified: None,
            },
            companion_fingerprint: None,
        }];
        let mut changes = [SubtitleChange {
            cue_edits: Default::default(),
            source: SubtitleSource::Sidecar(path),
            source_format: SubtitleFormat::SubRip,
            embedded_target: None,
            export_target: None,
            import_into_media: true,
            ocr_language: None,

            metadata: None,
        }];

        // Act
        let incompatible = imported_subtitle_conflicts(&changes, &sidecars, ContainerFormat::Mp4);
        changes[0].embedded_target = Some(SubtitleFormat::MovText);
        let compatible = imported_subtitle_conflicts(&changes, &sidecars, ContainerFormat::Mp4);

        // Assert
        assert_that!(incompatible).contains(
            "MP4 can't import SubRip / SRT subtitle movie.eng.srt. Convert it to MOV Text."
                .to_string(),
        );
        assert_that!(compatible).is_empty();
    }

    /// A file shaped like the MP4 that exposed all of this: an H.264 film with three
    /// AAC tracks, a VobSub subtitle, and the QuickTime chapter track the `mov` demuxer
    /// hands back as an opaque `bin_data` stream tagged `text` alongside the chapters it
    /// read out of it.
    fn chaptered_mp4(data_tag: &str) -> MediaInfo {
        MediaInfo::from_json(serde_json::json!({
            "streams": [
                track(0, "video", "h264"),
                track(1, "audio", "aac"),
                track(2, "audio", "aac"),
                track(3, "audio", "aac"),
                track(4, "subtitle", "dvd_subtitle"),
                {
                    "index": 5,
                    "codec_type": "data",
                    "codec_name": "bin_data",
                    "codec_tag_string": data_tag,
                },
            ],
            "chapters": [{"id": 0, "start_time": "0.0"}],
        }))
        .unwrap()
    }

    /// Reel used to report a conflict against the container the file was *already* in —
    /// "MP4 can't contain BIN_DATA data track #5" on an untouched MP4 — and then refuse
    /// the conversion to MKV outright, advising the reader to "Choose MKV or remove the
    /// track": a container they had already chosen and a track the page does not list,
    /// so neither half could be acted on. The stream is the chapter list, which
    /// `-map_chapters 0` writes out on its own.
    #[test]
    fn a_chapter_track_should_be_left_out_of_the_remux_and_raise_no_conflict() {
        // Arrange: the same film twice, differing only in what the data stream is —
        // the chapter list, or GoPro telemetry, which is a real track MP4 does store.
        let chapters = chaptered_mp4("text");
        let telemetry = chaptered_mp4("gpmd");
        let order = [0, 1, 2, 3, 4, 5];
        let conflicts = |info: &MediaInfo, target| {
            container_conflicts(
                info,
                &order,
                &BTreeMap::new(),
                &BTreeMap::new(),
                &[],
                target,
            )
        };
        // Which streams are flagged, rather than every message: MP4's shortlist has its
        // own opinion about the VobSub track, and this is about the data one.
        let flagged = |info: &MediaInfo, target| {
            container_conflict_streams(
                info,
                &order,
                &BTreeMap::new(),
                &BTreeMap::new(),
                &[],
                target,
            )
        };

        // Act
        let mapped = output_track_plan(&chapters, &order, &[], &[], &[]);
        let mapped_telemetry = output_track_plan(&telemetry, &order, &[], &[], &[]);

        // Assert: the chapter track is neither mapped nor complained about, in the
        // container the file is already in or in the one it is being converted to.
        assert_that!(mapped).is_equal_to(
            (0..=4)
                .map(OutputTrack::Existing)
                .collect::<Vec<OutputTrack>>(),
        );
        assert_that!(flagged(&chapters, ContainerFormat::Mp4)).is_equal_to(BTreeSet::from([4]));
        assert_that!(conflicts(&chapters, ContainerFormat::Matroska)).is_empty();

        // Assert: telemetry is a track like any other — carried into the remux, stored
        // by MP4, and reported once against the container that genuinely refuses it.
        assert_that!(mapped_telemetry).is_equal_to(
            (0..=5)
                .map(OutputTrack::Existing)
                .collect::<Vec<OutputTrack>>(),
        );
        assert_that!(flagged(&telemetry, ContainerFormat::Mp4)).is_equal_to(BTreeSet::from([4]));
        assert_that!(conflicts(&telemetry, ContainerFormat::Matroska)).is_equal_to(vec![
            "MKV can't contain BIN_DATA data track #5. Choose MP4 or MOV instead.".to_string(),
        ]);
    }

    /// The other half of the same defect: the `mov` muxer writes the chapters back out
    /// as a text track of its own, so an MP4 remuxed to MP4 comes back with a data
    /// stream the plan never asked for. Counted, it read as "The remuxed tracks are not
    /// in the requested order." and threw away a save that had in fact succeeded.
    #[test]
    fn validation_should_ignore_the_chapter_track_the_muxer_writes_for_itself() {
        // Arrange: five mapped tracks in, five mapped tracks out — plus the muxer's own
        // chapter track, which no plan can contain.
        let source = chaptered_mp4("text");
        let output = MediaInfo::from_json(serde_json::json!({
            "streams": [
                defaulted(track(0, "video", "h264")),
                defaulted(track(1, "audio", "aac")),
                track(2, "audio", "aac"),
                track(3, "audio", "aac"),
                defaulted(track(4, "subtitle", "dvd_subtitle")),
                {
                    "index": 5,
                    "codec_type": "data",
                    "codec_name": "bin_data",
                    "codec_tag_string": "text",
                },
            ],
            "chapters": [{"id": 0, "start_time": "0.0"}],
        }))
        .unwrap();

        // Act
        let result = validate_plain(
            &source,
            &output,
            &[0, 1, 2, 3, 4, 5],
            &BTreeSet::from([0, 1, 4]),
        );

        // Assert
        assert_that!(result).is_ok();
    }

    #[test]
    fn output_track_plan_should_insert_imports_after_embedded_subtitles() {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video"},
            {"index": 1, "codec_type": "audio"},
            {"index": 2, "codec_type": "subtitle"},
            {"index": 3, "codec_type": "attachment"}
        ]));

        let dummy_imports = vec![
            SubtitleImport {
                source_path: PathBuf::new(),
                target: SubtitleFormat::SubRip,
                path: PathBuf::new(),
                metadata: english_subtitle_metadata(),
                default: false,
            },
            SubtitleImport {
                source_path: PathBuf::new(),
                target: SubtitleFormat::SubRip,
                path: PathBuf::new(),
                metadata: english_subtitle_metadata(),
                default: false,
            },
        ];

        // Act
        let tracks = output_track_plan(&info, &[0, 1, 2, 3], &[], &dummy_imports, &[]);

        // Assert
        assert_eq!(
            tracks,
            vec![
                OutputTrack::Existing(0),
                OutputTrack::Existing(1),
                OutputTrack::Existing(2),
                OutputTrack::Imported(0),
                OutputTrack::Imported(1),
                OutputTrack::Existing(3),
            ]
        );
    }

    #[test]
    fn output_track_plan_should_put_imports_after_audio_without_embedded_subtitles() {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video"},
            {"index": 1, "codec_type": "audio"},
            {"index": 2, "codec_type": "attachment"}
        ]));
        let dummy_imports = vec![SubtitleImport {
            source_path: PathBuf::new(),
            target: SubtitleFormat::SubRip,
            path: PathBuf::new(),
            metadata: english_subtitle_metadata(),
            default: false,
        }];

        // Act
        let tracks = output_track_plan(&info, &[0, 1, 2], &[], &dummy_imports, &[]);

        // Assert
        assert_eq!(
            tracks,
            vec![
                OutputTrack::Existing(0),
                OutputTrack::Existing(1),
                OutputTrack::Imported(0),
                OutputTrack::Existing(2),
            ]
        );
    }

    #[test]
    fn output_track_plan_should_respect_custom_left_subtitle_order() {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video"},
            {"index": 1, "codec_type": "audio"},
            {"index": 2, "codec_type": "subtitle"},
            {"index": 3, "codec_type": "subtitle"}
        ]));
        let sidecars = vec![SidecarEntry {
            path: PathBuf::from("/tmp/sidecar.srt"),
            companion: None,
            display_name: "sidecar.srt".to_string(),
            format: SubtitleFormat::SubRip,
            language: "eng".to_string(),
            forced: false,
            hearing_impaired: false,
            number: None,
            fingerprint: FileFingerprint {
                length: 10,
                modified: None,
            },
            companion_fingerprint: None,
        }];
        let imports = vec![SubtitleImport {
            source_path: PathBuf::from("/tmp/sidecar.srt"),
            target: SubtitleFormat::SubRip,
            path: PathBuf::from("/tmp/staged.srt"),
            metadata: english_subtitle_metadata(),
            default: false,
        }];
        let left_subtitle_order = vec![
            TrackRef::Sidecar(0),
            TrackRef::Embedded(3),
            TrackRef::Embedded(2),
        ];

        // Act
        let tracks = output_track_plan(
            &info,
            &[0, 1, 2, 3],
            &left_subtitle_order,
            &imports,
            &sidecars,
        );

        // Assert
        assert_eq!(
            tracks,
            vec![
                OutputTrack::Existing(0),
                OutputTrack::Existing(1),
                OutputTrack::Imported(0),
                OutputTrack::Existing(3),
                OutputTrack::Existing(2),
            ]
        );
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
            &BTreeMap::new(),
        );

        // Assert
        assert_that!(result).is_ok();
    }

    #[test]
    fn validate_edit_should_reject_resizing_an_unsupported_original_codec() {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "ffv1", "width": 1280, "height": 1024}
        ]));
        let settings = BTreeMap::from([(
            0,
            VideoSettings {
                codec: VideoCodec::Original,
                resolution: VideoResolution::P480,
                metadata: VideoMetadata {
                    language: "und".to_string(),
                    title: None,
                    commentary: false,
                },
                rotation: VideoRotation::None,
            },
        )]);

        // Act
        let result = validate_edit(
            &info,
            &[0],
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeMap::new(),
            &settings,
        );

        // Assert
        assert_that!(result).contains_error(
            "Can't resize the original FFV1 codec; choose H.264, HEVC, or AV1.".to_string(),
        );
    }

    #[test]
    fn validate_edit_should_reject_a_preset_that_would_upscale_either_dimension() {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264", "width": 1000, "height": 800}
        ]));
        let settings = BTreeMap::from([(
            0,
            VideoSettings {
                codec: VideoCodec::Original,
                resolution: VideoResolution::P720,
                metadata: VideoMetadata {
                    language: "und".to_string(),
                    title: None,
                    commentary: false,
                },
                rotation: VideoRotation::None,
            },
        )]);

        // Act
        let result = validate_edit(
            &info,
            &[0],
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeMap::new(),
            &settings,
        );

        // Assert
        assert_that!(result)
            .contains_error("The selected resolution must be lower than the original.".to_string());
    }

    #[test]
    fn validate_edit_should_reject_custom_upscaling_in_either_dimension() {
        for (width, height) in [(1922, 720), (1280, 1082)] {
            // Arrange
            let info = media(serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080}
            ]));
            let settings = BTreeMap::from([(
                0,
                VideoSettings {
                    codec: VideoCodec::Original,
                    resolution: VideoResolution::Custom(CustomResolution {
                        width,
                        height,
                        scaling: CustomScaling::FitPad,
                    }),
                    metadata: VideoMetadata {
                        language: "und".to_string(),
                        title: None,
                        commentary: false,
                    },
                    rotation: VideoRotation::None,
                },
            )]);

            // Act
            let result = validate_edit(
                &info,
                &[0],
                &BTreeSet::new(),
                &BTreeSet::new(),
                &BTreeMap::new(),
                &settings,
            );

            // Assert
            assert_that!(result).contains_error("Upscaling isn't possible yet.".to_string());
        }
    }

    #[test]
    fn validate_edit_should_reject_custom_dimensions_no_encoder_accepts() {
        // Regression test for a real report: a 1920x10 custom resolution passed
        // validation (positive, even, not upscaling) and only failed once libx265 was
        // opening, minutes into the encode, with "Image size is too small".
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080}
        ]));
        let settings = |width, height| {
            BTreeMap::from([(
                0,
                VideoSettings {
                    codec: VideoCodec::Original,
                    resolution: VideoResolution::Custom(CustomResolution {
                        width,
                        height,
                        scaling: CustomScaling::FitPad,
                    }),
                    metadata: VideoMetadata {
                        language: "und".to_string(),
                        title: None,
                        commentary: false,
                    },
                    rotation: VideoRotation::None,
                },
            )])
        };

        // Act
        let short = validate_edit(
            &info,
            &[0],
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeMap::new(),
            &settings(1920, 10),
        );
        let narrow = validate_edit(
            &info,
            &[0],
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeMap::new(),
            &settings(10, 1080),
        );
        let smallest_allowed = validate_edit(
            &info,
            &[0],
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeMap::new(),
            &settings(16, 16),
        );

        // Assert: both directions are caught, and the floor itself still passes.
        assert_that!(short)
            .contains_error("Custom width and height must be at least 16 pixels.".to_string());
        assert_that!(narrow)
            .contains_error("Custom width and height must be at least 16 pixels.".to_string());
        assert_that!(smallest_allowed).is_ok();
    }

    #[test]
    fn validate_deletion_should_reject_an_empty_selection() {
        // Arrange: Save reached with nothing actually marked for deletion. Letting this
        // through would launch a full remux that produces a byte-identical file — minutes
        // of encoding, and a republish over the original, for no change at all.
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video"},
            {"index": 1, "codec_type": "audio"}
        ]));

        // Act
        let result = validate_deletion(&info, &BTreeSet::new());

        // Assert
        assert_that!(result).contains_error("No tracks are selected for deletion.".to_string());
    }

    #[test]
    fn validate_deletion_should_reject_a_selection_the_file_no_longer_contains() {
        // Arrange: the file was re-encoded by something else while it sat staged, so the
        // indices the user marked no longer exist. Deleting by stale index would remove
        // whichever tracks now happen to hold those numbers — a silent wrong deletion,
        // which is exactly the class of failure the "tracks changed" guard exists for.
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video"},
            {"index": 1, "codec_type": "audio"}
        ]));

        // Act
        let result = validate_deletion(&info, &BTreeSet::from([7]));

        // Assert
        assert_that!(result).contains_error(
            "The file's tracks changed. Reopen it and select them again.".to_string(),
        );
    }

    #[test]
    fn validate_edit_should_reject_a_stream_that_has_no_usable_index() {
        // Arrange: ffprobe returned a stream with no `index`. Every downstream mapping is
        // keyed by index, so proceeding would silently drop that track from the output.
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video"},
            {"codec_type": "audio"}
        ]));

        // Act
        let result = validate_edit(
            &info,
            &[0],
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
        );

        // Assert
        assert_that!(result)
            .contains_error("One or more tracks have no usable stream index.".to_string());
    }

    #[test]
    fn validate_edit_should_reject_a_staged_order_listing_the_same_track_twice() {
        // Arrange: a duplicate in the staged order means the reorder bookkeeping has come
        // adrift. ffmpeg would happily map the same input stream to two output tracks,
        // producing a file with a track the user never asked to duplicate.
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video"},
            {"index": 1, "codec_type": "audio"}
        ]));

        // Act
        let result = validate_edit(
            &info,
            &[0, 1, 1],
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
        );

        // Assert
        let Err(message) = result else {
            panic!("a duplicated track in the staged order must not validate");
        };
        assert!(
            message.contains("appears twice"),
            "the message must name the actual problem, got {message:?}",
        );
    }

    #[test]
    fn validate_edit_should_reject_a_track_that_is_both_kept_and_deleted() {
        // Arrange: contradictory staging — the track is in the keep order and in the
        // delete set. Whichever one won silently would be a coin flip over the user's
        // data, so the edit must refuse rather than pick.
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video"},
            {"index": 1, "codec_type": "audio"}
        ]));

        // Act
        let result = validate_edit(
            &info,
            &[0, 1],
            &BTreeSet::from([1]),
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
        );

        // Assert
        let Err(message) = result else {
            panic!("a track both kept and deleted must not validate");
        };
        assert!(
            message.contains("both kept and marked for deletion"),
            "the message must name the contradiction, got {message:?}",
        );
        assert!(
            message.contains('1'),
            "the message must name the offending track, got {message:?}",
        );
    }

    #[test]
    fn validate_edit_should_reject_encoding_settings_attached_to_a_deleted_track() {
        // Arrange: the user set an encode on a track, then deleted it. The settings are
        // now orphaned — carrying them into the plan would apply an encode to whichever
        // track ends up at that index.
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080},
            {"index": 1, "codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080}
        ]));
        let settings = BTreeMap::from([(
            1,
            VideoSettings {
                codec: VideoCodec::Original,
                resolution: VideoResolution::P720,
                metadata: VideoMetadata {
                    language: "und".to_string(),
                    title: None,
                    commentary: false,
                },
                rotation: VideoRotation::None,
            },
        )]);

        // Act
        let result = validate_edit(
            &info,
            &[0],
            &BTreeSet::from([1]),
            &BTreeSet::new(),
            &BTreeMap::new(),
            &settings,
        );

        // Assert
        assert_that!(result)
            .contains_error("Video settings refer to a missing or deleted track.".to_string());
    }

    #[test]
    fn validate_edit_should_reject_encoding_settings_on_a_track_that_is_not_playable_video() {
        // Arrange: encode settings pointed at an audio track, and at cover art. Neither
        // can be scaled or re-encoded as video — ffmpeg would fail deep into the run, so
        // this must be caught before any work starts.
        let audio_info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080},
            {"index": 1, "codec_type": "audio", "codec_name": "aac"}
        ]));
        let cover_info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080},
            {
                "index": 1,
                "codec_type": "video",
                "codec_name": "mjpeg",
                "width": 600,
                "height": 600,
                "disposition": {"attached_pic": 1}
            }
        ]));
        let settings = BTreeMap::from([(
            1,
            VideoSettings {
                codec: VideoCodec::Original,
                resolution: VideoResolution::P720,
                metadata: VideoMetadata {
                    language: "und".to_string(),
                    title: None,
                    commentary: false,
                },
                rotation: VideoRotation::None,
            },
        )]);

        // Act
        let on_audio = validate_edit(
            &audio_info,
            &[0, 1],
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeMap::new(),
            &settings,
        );
        let on_cover = validate_edit(
            &cover_info,
            &[0, 1],
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeMap::new(),
            &settings,
        );

        // Assert
        let expected = "Encoding settings can only be applied to playable video tracks.";
        assert_that!(on_audio).contains_error(expected.to_string());
        assert_that!(on_cover).contains_error(expected.to_string());
    }

    #[test]
    fn validate_edit_should_reject_zero_and_odd_custom_dimensions_in_either_axis() {
        // Arrange: each half of the positive-and-even check is its own condition, so an
        // axis left unguarded would only show up as an ffmpeg failure mid-encode. Zero
        // comes from an emptied field; odd from typing a real-looking number like 1281.
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080}
        ]));
        let settings = |width, height| {
            BTreeMap::from([(
                0,
                VideoSettings {
                    codec: VideoCodec::Original,
                    resolution: VideoResolution::Custom(CustomResolution {
                        width,
                        height,
                        scaling: CustomScaling::FitPad,
                    }),
                    metadata: VideoMetadata {
                        language: "und".to_string(),
                        title: None,
                        commentary: false,
                    },
                    rotation: VideoRotation::None,
                },
            )])
        };

        // Act / Assert
        for (width, height, case) in [
            (0, 720, "zero width"),
            (1280, 0, "zero height"),
            (0, 0, "both zero"),
            (1280, 721, "odd height"),
            (1281, 720, "odd width"),
        ] {
            let result = validate_edit(
                &info,
                &[0],
                &BTreeSet::new(),
                &BTreeSet::new(),
                &BTreeMap::new(),
                &settings(width, height),
            );
            assert_that!(result).contains_error(
                "Custom width and height must be positive even numbers.".to_string(),
            );
            assert!(
                validate_edit(
                    &info,
                    &[0],
                    &BTreeSet::new(),
                    &BTreeSet::new(),
                    &BTreeMap::new(),
                    &settings(1280, 720)
                )
                .is_ok(),
                "the valid control case must still pass while checking {case}",
            );
        }
    }

    #[test]
    fn validate_edit_should_refuse_custom_scaling_when_the_source_resolution_is_unknown() {
        // Arrange: a video stream ffprobe reported without width/height. The upscaling
        // check has nothing to compare against, and guessing would either block a legal
        // downscale or let an upscale through — so custom scaling is refused outright.
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264"}
        ]));
        let settings = BTreeMap::from([(
            0,
            VideoSettings {
                codec: VideoCodec::Original,
                resolution: VideoResolution::Custom(CustomResolution {
                    width: 1280,
                    height: 720,
                    scaling: CustomScaling::FitPad,
                }),
                metadata: VideoMetadata {
                    language: "und".to_string(),
                    title: None,
                    commentary: false,
                },
                rotation: VideoRotation::None,
            },
        )]);

        // Act
        let result = validate_edit(
            &info,
            &[0],
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeMap::new(),
            &settings,
        );

        // Assert
        assert_that!(result).contains_error(
            "The source resolution is unavailable; custom scaling cannot be applied.".to_string(),
        );
    }

    #[test]
    fn validate_edit_should_reject_odd_custom_dimensions() {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080}
        ]));
        let settings = BTreeMap::from([(
            0,
            VideoSettings {
                codec: VideoCodec::Original,
                resolution: VideoResolution::Custom(CustomResolution {
                    width: 1279,
                    height: 720,
                    scaling: CustomScaling::FitPad,
                }),
                metadata: VideoMetadata {
                    language: "und".to_string(),
                    title: None,
                    commentary: false,
                },
                rotation: VideoRotation::None,
            },
        )]);

        // Act
        let result = validate_edit(
            &info,
            &[0],
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeMap::new(),
            &settings,
        );

        // Assert
        assert_that!(result)
            .contains_error("Custom width and height must be positive even numbers.".to_string());
    }

    #[test]
    fn muxer_forced_default_positions_should_only_excuse_iso_bmff_track_groups_without_a_default() {
        // Arrange: video, two audio, two subtitles — the audio group has a staged
        // default, the other two groups have none.
        let output = media(serde_json::json!([
            {"index": 0, "codec_type": "video"},
            {"index": 1, "codec_type": "audio"},
            {"index": 2, "codec_type": "audio"},
            {"index": 3, "codec_type": "subtitle"},
            {"index": 4, "codec_type": "subtitle"},
        ]));
        let expected = [false, false, true, false, false];

        // Act
        let mp4 = muxer_forced_default_positions(&output, &expected, Some(ContainerFormat::Mp4));
        let mkv =
            muxer_forced_default_positions(&output, &expected, Some(ContainerFormat::Matroska));

        // Assert: only the first track of each group the muxer has to enable anyway —
        // never the second audio track, whose group already has its default.
        assert_that!(mp4).is_equal_to(BTreeSet::from([0, 3]));
        // Matroska can leave a whole group undefaulted, so nothing is excused there.
        assert_that!(mkv).is_equal_to(BTreeSet::new());
    }

    #[test]
    fn resolution_filter_should_generate_each_custom_scaling_mode() {
        // Act
        let preset = resolution_filter(VideoResolution::P720);
        let fit_pad = resolution_filter(VideoResolution::Custom(CustomResolution {
            width: 1280,
            height: 720,
            scaling: CustomScaling::FitPad,
        }));
        let stretch = resolution_filter(VideoResolution::Custom(CustomResolution {
            width: 1280,
            height: 720,
            scaling: CustomScaling::Stretch,
        }));
        // Assert
        assert_that!(preset.as_deref()).contains(
            "scale=1280:720:force_original_aspect_ratio=decrease:force_divisible_by=2,pad=1280:720:(ow-iw)/2:(oh-ih)/2:black,setsar=1",
        );
        assert_that!(fit_pad.as_deref()).contains(
            "scale=1280:720:force_original_aspect_ratio=decrease:force_divisible_by=2,pad=1280:720:(ow-iw)/2:(oh-ih)/2:black,setsar=1",
        );
        assert_that!(stretch.as_deref()).contains("scale=1280:720,setsar=1");
    }

    #[test]
    fn output_resolution_should_require_exact_custom_and_preset_dimensions() {
        // Arrange
        let exact = BTreeMap::from([
            ("width".to_string(), Value::from(1280)),
            ("height".to_string(), Value::from(720)),
        ]);
        let bounded = BTreeMap::from([
            ("width".to_string(), Value::from(960)),
            ("height".to_string(), Value::from(720)),
        ]);

        // Act / Assert
        let sized = |resolution| VideoSettings {
            resolution,
            ..VideoSettings::default()
        };
        let custom = |scaling| {
            sized(VideoResolution::Custom(CustomResolution {
                width: 1280,
                height: 720,
                scaling,
            }))
        };

        // Act / Assert
        assert_that!(output_resolution_matches(
            &exact,
            None,
            &custom(CustomScaling::FitPad)
        ))
        .is_true();
        assert_that!(output_resolution_matches(
            &exact,
            None,
            &custom(CustomScaling::Stretch)
        ))
        .is_true();
        assert_that!(output_resolution_matches(
            &bounded,
            None,
            &custom(CustomScaling::FitPad)
        ))
        .is_false();
        assert_that!(output_resolution_matches(
            &exact,
            None,
            &sized(VideoResolution::P720)
        ))
        .is_true();
        assert_that!(output_resolution_matches(
            &bounded,
            None,
            &sized(VideoResolution::P720)
        ))
        .is_false();
    }

    /// Only the messages that mean "what you staged no longer matches the file" may be
    /// routed to `SourceChanged`, because that verdict silently discards the staged
    /// edit. Anything else has to stay `Failed` so the user gets to fix it.
    #[test]
    fn every_staleness_phrase_should_discard_the_staged_edit_and_nothing_else_should() {
        // Arrange
        let stale = [
            "The file's tracks changed: reopen it and try again.",
            "An embedded subtitle track changed. Reopen the file and try again.",
            "A subtitle sidecar changed; reload it before converting.",
            "A subtitle sidecar is no longer available.",
            "The staged defaults refer to a missing or deleted track.",
        ];
        let fixable = [
            "The selected resolution must be lower than the original.",
            "Choose a Tesseract language for OCR.",
            "MP4 can't contain VP9 video track #0.",
        ];

        // Act / Assert
        for message in stale {
            assert!(
                matches!(
                    classify_edit_error(message.to_string()),
                    EditError::SourceChanged(_)
                ),
                "{message:?} should discard the staged edit",
            );
        }
        for message in fixable {
            assert!(
                matches!(
                    classify_edit_error(message.to_string()),
                    EditError::Failed(_)
                ),
                "{message:?} is the user's to fix and must survive as a failure",
            );
        }
    }

    fn embedded_change(index: u64, source_format: SubtitleFormat) -> SubtitleChange {
        SubtitleChange {
            cue_edits: Default::default(),
            source: SubtitleSource::Embedded(index),
            source_format,
            embedded_target: None,
            export_target: Some(SubtitleFormat::SubRip),
            import_into_media: false,
            ocr_language: None,
            metadata: None,
        }
    }

    #[test]
    fn a_subtitle_conversion_should_be_refused_once_its_embedded_source_has_moved() {
        // Arrange
        let info = media(serde_json::json!([
            track(0, "video", "h264"),
            subtitle_track(1, "subrip", "eng"),
            track(2, "audio", "aac"),
        ]));

        // Act
        let matching =
            validate_subtitle_sources(&info, &[embedded_change(1, SubtitleFormat::SubRip)], &[]);
        let gone =
            validate_subtitle_sources(&info, &[embedded_change(9, SubtitleFormat::SubRip)], &[]);
        let not_a_subtitle =
            validate_subtitle_sources(&info, &[embedded_change(2, SubtitleFormat::SubRip)], &[]);
        let different_format =
            validate_subtitle_sources(&info, &[embedded_change(1, SubtitleFormat::Ass)], &[]);
        let twice = validate_subtitle_sources(
            &info,
            &[
                embedded_change(1, SubtitleFormat::SubRip),
                embedded_change(1, SubtitleFormat::SubRip),
            ],
            &[],
        );
        // A change that asks for nothing is skipped before any of those checks.
        let mut inert = embedded_change(9, SubtitleFormat::SubRip);
        inert.export_target = None;
        let no_effect = validate_subtitle_sources(&info, &[inert], &[]);

        // Assert
        assert_that!(matching).is_ok();
        assert_that!(gone.unwrap_err().as_str()).contains("embedded subtitle track changed");
        assert_that!(not_a_subtitle.unwrap_err().as_str())
            .contains("embedded subtitle track changed");
        assert_that!(different_format.unwrap_err().as_str())
            .contains("embedded subtitle track changed");
        assert_that!(twice.unwrap_err().as_str()).contains("more than one pending conversion");
        assert_that!(no_effect).is_ok();
    }

    #[test]
    fn a_subtitle_conversion_should_be_refused_once_its_sidecar_has_changed_on_disk() {
        // Arrange
        let directory = scratch_directory("sidecar-sources");
        let _cleanup = DirectoryCleanup(Some(directory.clone()));
        let info = media(serde_json::json!([track(0, "video", "h264")]));
        let path = directory.join("movie.eng.srt");
        fs::write(&path, "1\n").unwrap();
        let companion = directory.join("movie.eng.idx");
        fs::write(&companion, "1\n").unwrap();
        let mut sidecar = sidecar_entry(&path, None, SubtitleFormat::SubRip);
        sidecar.fingerprint = FileFingerprint::for_path(&path).unwrap();
        let change = SubtitleChange {
            cue_edits: Default::default(),
            source: SubtitleSource::Sidecar(path.clone()),
            source_format: SubtitleFormat::SubRip,
            embedded_target: Some(SubtitleFormat::Ass),
            export_target: None,
            import_into_media: true,
            ocr_language: None,
            metadata: None,
        };
        let mut stale_fingerprint = sidecar.clone();
        stale_fingerprint.fingerprint = FileFingerprint {
            length: 0,
            modified: None,
        };
        let mut different_format = sidecar.clone();
        different_format.format = SubtitleFormat::Ass;
        let mut stale_companion = sidecar.clone();
        stale_companion.companion = Some(companion);
        stale_companion.companion_fingerprint = Some(FileFingerprint {
            length: 0,
            modified: None,
        });

        // Act
        let matching = validate_subtitle_sources(&info, std::slice::from_ref(&change), &[sidecar]);
        let missing = validate_subtitle_sources(&info, std::slice::from_ref(&change), &[]);
        let rewritten =
            validate_subtitle_sources(&info, std::slice::from_ref(&change), &[stale_fingerprint]);
        let retyped =
            validate_subtitle_sources(&info, std::slice::from_ref(&change), &[different_format]);
        let companion_rewritten = validate_subtitle_sources(&info, &[change], &[stale_companion]);

        // Assert
        assert_that!(matching).is_ok();
        assert_that!(missing.unwrap_err().as_str()).contains("no longer available");
        assert_that!(rewritten.unwrap_err().as_str()).contains("sidecar changed");
        assert_that!(retyped.unwrap_err().as_str()).contains("sidecar changed");
        assert_that!(companion_rewritten.unwrap_err().as_str()).contains("sidecar changed");
    }

    #[test]
    fn an_ocr_conversion_should_be_refused_until_a_tesseract_language_is_chosen() {
        // Arrange
        let info = media(serde_json::json!([
            track(0, "video", "h264"),
            subtitle_track(1, "hdmv_pgs_subtitle", "eng"),
        ]));
        let mut change = embedded_change(1, SubtitleFormat::Pgs);
        change.export_target = Some(SubtitleFormat::SubRip);

        // Act
        let unset = validate_subtitle_sources(&info, std::slice::from_ref(&change), &[]);
        let mut blank = change.clone();
        blank.ocr_language = Some(String::new());
        let empty = validate_subtitle_sources(&info, &[blank], &[]);
        let mut chosen = change;
        chosen.ocr_language = Some("eng".to_string());
        let set = validate_subtitle_sources(&info, &[chosen], &[]);

        // Assert
        assert_that!(unset.unwrap_err().as_str()).contains("Choose a Tesseract language");
        assert_that!(empty.unwrap_err().as_str()).contains("Choose a Tesseract language");
        assert_that!(set).is_ok();
    }

    #[test]
    fn an_undetermined_subtitle_language_should_block_the_save_unless_the_track_is_going_away() {
        // Arrange
        let info = media(serde_json::json!([
            track(0, "video", "h264"),
            subtitle_track(1, "subrip", "und"),
        ]));
        let mut retagged = embedded_change(1, SubtitleFormat::SubRip);
        let mut metadata = english_subtitle_metadata();
        metadata.language = "dan".to_string();
        retagged.metadata = Some(metadata);

        // Act
        let undetermined = validate_subtitle_languages(&info, &[], &[], &BTreeSet::new());
        let deleted = validate_subtitle_languages(&info, &[], &[], &BTreeSet::from([1_u64]));
        let relabelled = validate_subtitle_languages(&info, &[retagged], &[], &BTreeSet::new());

        // Assert
        assert_that!(undetermined.unwrap_err().as_str())
            .is_equal_to("Choose a language for subtitle track #1; Undetermined is not allowed.");
        assert_that!(deleted).is_ok();
        assert_that!(relabelled).is_ok();
    }

    #[test]
    fn an_undetermined_sidecar_language_should_block_the_save_by_name() {
        // Arrange
        let info = media(serde_json::json!([track(0, "video", "h264")]));
        let path = PathBuf::from("/videos/movie.und.srt");
        let mut sidecar = sidecar_entry(&path, None, SubtitleFormat::SubRip);
        sidecar.language = "und".to_string();
        let mut retagged = SubtitleChange {
            cue_edits: Default::default(),
            source: SubtitleSource::Sidecar(path),
            source_format: SubtitleFormat::SubRip,
            embedded_target: None,
            export_target: None,
            import_into_media: true,
            ocr_language: None,
            metadata: None,
        };
        let mut metadata = english_subtitle_metadata();
        metadata.language = "nld".to_string();
        retagged.metadata = Some(metadata);

        // Act
        let undetermined = validate_subtitle_languages(
            &info,
            &[],
            std::slice::from_ref(&sidecar),
            &BTreeSet::new(),
        );
        let relabelled =
            validate_subtitle_languages(&info, &[retagged], &[sidecar], &BTreeSet::new());

        // Assert
        assert_that!(undetermined.unwrap_err().as_str())
            .is_equal_to("Choose a language for movie.und.srt; Undetermined is not allowed.");
        assert_that!(relabelled).is_ok();
    }

    /// A throwaway directory of its own per test, since several of these run real
    /// filesystem transactions and must not see each other's leftovers.
    fn scratch_directory(label: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn a_long_progress_subject_should_be_elided_in_the_middle() {
        // Act
        let short = compact_subject("movie.eng.srt");
        let exactly_at_the_cap = compact_subject("123456789012345678901234");
        let long = compact_subject("a-really-long-subtitle-name.eng.forced.srt");
        // Multi-byte characters must be counted, not bytes, or the elision panics.
        let japanese = compact_subject(&"字".repeat(40));

        // Assert
        assert_that!(short.as_str()).is_equal_to("movie.eng.srt");
        assert_that!(exactly_at_the_cap.as_str()).is_equal_to("123456789012345678901234");
        assert_that!(long.as_str()).is_equal_to("a-really-long-s…rced.srt");
        assert_that!(japanese.chars().count()).is_equal_to(24);
    }

    #[test]
    fn a_source_mismatch_should_name_the_tracks_that_disagree() {
        // Act
        let nothing = describe_index_diff(&[]);
        let some = describe_index_diff(&[&1, &3]);

        // Assert
        assert_that!(nothing.as_str()).is_equal_to("none");
        assert_that!(some.as_str()).is_equal_to("track(s) [1, 3]");
    }

    #[test]
    fn a_progress_label_should_fall_back_to_a_placeholder_for_a_nameless_path() {
        // Act / Assert
        assert_that!(display_file_name(Path::new("/videos/movie.mkv")).as_str())
            .is_equal_to("movie.mkv");
        assert_that!(display_file_name(Path::new("/")).as_str()).is_equal_to("file");
    }

    #[test]
    fn the_primary_video_resolution_should_ignore_cover_art_and_undimensioned_tracks() {
        // Arrange
        let behind_cover_art = media(serde_json::json!([
            {
                "index": 0,
                "codec_type": "video",
                "codec_name": "mjpeg",
                "width": 600,
                "height": 600,
                "disposition": {"attached_pic": 1},
            },
            {"index": 1, "codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080},
        ]));
        let no_dimensions = media(serde_json::json!([track(0, "video", "h264")]));

        // Act / Assert
        assert_that!(primary_video_resolution(&behind_cover_art)).contains((1920, 1080));
        assert_that!(primary_video_resolution(&no_dimensions)).is_none();
    }

    #[test]
    fn only_the_three_re_encodable_codecs_should_resolve_to_an_encoder() {
        // Arrange
        let of = |codec: &str| BTreeMap::from([("codec_name".to_string(), Value::from(codec))]);

        // Act / Assert: a source codec reel cannot re-encode has no encoder either, so
        // a resize that would have to keep the original codec is refused up front.
        assert_that!(source_codec(&of("h264"))).contains("h264");
        assert_that!(source_codec(&of("hevc"))).contains("hevc");
        assert_that!(source_codec(&of("av1"))).contains("av1");
        assert_that!(source_codec(&of("vp9"))).is_none();
        assert_that!(source_codec(&BTreeMap::new())).is_none();
        assert_that!(encoder_settings("h264")).contains(("libx264", "22", "medium"));
        assert_that!(encoder_settings("hevc")).contains(("libx265", "24", "medium"));
        assert_that!(encoder_settings("av1")).contains(("libsvtav1", "30", "8"));
        assert_that!(encoder_settings("vp9")).is_none();
    }

    #[test]
    fn a_container_conflict_should_suggest_a_way_out_for_every_kind_of_track() {
        // Act
        let video = container_conflict_message(ContainerFormat::Mp4, 0, "video", "vp9");
        let audio = container_conflict_message(ContainerFormat::Mp4, 1, "audio", "vorbis");
        let subtitle = container_conflict_message(ContainerFormat::Mp4, 2, "subtitle", "subrip");
        // A track with no editor of its own: the advice can only be another container,
        // and it must never be the one already chosen.
        let data = container_conflict_message(ContainerFormat::Matroska, 3, "data", "bin_data");
        let attachment = container_conflict_message(ContainerFormat::Mp4, 4, "attachment", "ttf");
        let nowhere = container_conflict_message(ContainerFormat::WebM, 5, "data", "klv");

        // Assert
        assert_that!(video.as_str())
            .is_equal_to("MP4 can't contain VP9 video track #0. Encode it as H.264 or HEVC / H.265 or AV1 or remove the track.");
        assert_that!(audio.as_str()).contains("Encode it as AAC");
        assert_that!(subtitle.as_str()).contains("Convert it to ");
        assert_that!(subtitle.as_str()).contains("MOV Text");
        assert_that!(data.as_str())
            .is_equal_to("MKV can't contain BIN_DATA data track #3. Choose MP4 or MOV instead.");
        assert_that!(attachment.as_str())
            .is_equal_to("MP4 can't contain TTF attachment track #4. Choose MKV instead.");
        assert_that!(nowhere.as_str()).is_equal_to(
            "WebM can't contain KLV data track #5. No available container can store it.",
        );
    }

    #[test]
    fn a_container_that_cannot_store_a_subtitle_flag_should_say_so_per_flag() {
        // Arrange: every flag set, so each container's own gaps are the only thing
        // deciding what gets reported.
        let metadata = SubtitleMetadata {
            language: "eng".to_string(),
            title: None,
            forced: true,
            cc: true,
            hearing_impaired: true,
            original: true,
            commentary: true,
        };

        // Act
        let matroska = subtitle_flag_conflicts(&metadata, ContainerFormat::Matroska, "track #2");
        let mp4 = subtitle_flag_conflicts(&metadata, ContainerFormat::Mp4, "track #2");
        let mov = subtitle_flag_conflicts(&metadata, ContainerFormat::Mov, "track #2");
        let webm = subtitle_flag_conflicts(&metadata, ContainerFormat::WebM, "track #2");
        let nothing_set = subtitle_flag_conflicts(
            &english_subtitle_metadata(),
            ContainerFormat::Mov,
            "track #2",
        );

        // Assert
        assert_that!(matroska.len()).is_equal_to(1);
        assert_that!(matroska[0].as_str()).is_equal_to(
            "MKV can't store the CC flag on track #2. Clear it or choose another container.",
        );
        assert_that!(mp4.len()).is_equal_to(1);
        assert_that!(mp4[0].as_str()).contains("Original flag");
        assert_that!(mov.len()).is_equal_to(SubtitleFlag::ALL.len());
        assert_that!(webm.len()).is_equal_to(3);
        assert_that!(nothing_set).is_empty();
    }

    #[test]
    fn a_container_change_should_not_rewrite_the_path_when_nothing_asked_for_one() {
        // Act
        let unchanged = replacement_path(Path::new("/videos/movie.mkv"), None).unwrap();
        let converted =
            replacement_path(Path::new("/videos/movie.mkv"), Some(ContainerFormat::Mp4)).unwrap();
        let no_parent = replacement_path(Path::new("/"), Some(ContainerFormat::Mp4));

        // Assert
        assert_that!(unchanged).is_equal_to(PathBuf::from("/videos/movie.mkv"));
        assert_that!(converted).is_equal_to(PathBuf::from("/videos/movie.mp4"));
        assert_that!(matches!(no_parent, Err(EditError::Failed(_)))).is_true();
    }

    fn sidecar_entry(
        path: &Path,
        companion: Option<&Path>,
        format: SubtitleFormat,
    ) -> SidecarEntry {
        SidecarEntry {
            path: path.to_path_buf(),
            companion: companion.map(Path::to_path_buf),
            display_name: display_file_name(path),
            format,
            language: "eng".to_string(),
            forced: false,
            hearing_impaired: false,
            number: None,
            fingerprint: FileFingerprint {
                length: 0,
                modified: None,
            },
            companion_fingerprint: None,
        }
    }

    #[test]
    fn an_export_destination_should_be_refused_when_anything_else_already_owns_it() {
        // Arrange
        let directory = scratch_directory("sidecar-destination");
        let _cleanup = DirectoryCleanup(Some(directory.clone()));
        let source = directory.join("movie.eng.srt");
        fs::write(&source, "1\n").unwrap();
        fs::write(directory.join("occupied.srt"), "1\n").unwrap();
        fs::write(directory.join("pair.idx"), "1\n").unwrap();
        let sidecar = sidecar_entry(&source, None, SubtitleFormat::SubRip);
        let reserved = [Publication {
            staged: vec![(directory.join("work.srt"), directory.join("claimed.srt"))],
            remove: Vec::new(),
        }];
        let available = |name: &str, target, publications: &[Publication]| {
            sidecar_destination_available(&directory.join(name), &sidecar, target, publications)
        };

        // Act / Assert
        assert_that!(available("free.srt", SubtitleFormat::SubRip, &[])).is_true();
        assert_that!(available("occupied.srt", SubtitleFormat::SubRip, &[])).is_false();
        // Overwriting the sidecar the export came from is the one allowed collision.
        assert_that!(available("movie.eng.srt", SubtitleFormat::SubRip, &[])).is_true();
        // A path another publication already claimed is taken even though nothing is
        // there yet — the two would otherwise publish over each other.
        assert_that!(available("claimed.srt", SubtitleFormat::SubRip, &reserved)).is_false();
        // VobSub publishes a `.sub`/`.idx` pair, so a free `.sub` is not enough.
        assert_that!(available("pair.sub", SubtitleFormat::VobSub, &[])).is_false();
        assert_that!(available("pair.sub", SubtitleFormat::SubRip, &[])).is_true();
    }

    #[test]
    fn a_vobsub_conversion_should_refuse_to_publish_without_its_idx_companion() {
        // Arrange
        let directory = scratch_directory("subtitle-artifacts");
        let _cleanup = DirectoryCleanup(Some(directory.clone()));
        let staged = directory.join("staged.sub");
        fs::write(&staged, "1\n").unwrap();
        let destination = directory.join("movie.eng.sub");

        // Act
        let lone_sub = subtitle_artifact_pairs(&staged, &destination, SubtitleFormat::VobSub);
        let text = subtitle_artifact_pairs(
            &directory.join("staged.srt"),
            &directory.join("movie.eng.srt"),
            SubtitleFormat::SubRip,
        );
        fs::write(directory.join("staged.idx"), "1\n").unwrap();
        let complete = subtitle_artifact_pairs(&staged, &destination, SubtitleFormat::VobSub);

        // Assert
        assert_that!(matches!(lone_sub, Err(EditError::Failed(_)))).is_true();
        assert_that!(text.unwrap().len()).is_equal_to(1);
        assert_that!(complete.unwrap()).is_equal_to(vec![
            (staged.clone(), destination.clone()),
            (
                directory.join("staged.idx"),
                directory.join("movie.eng.idx"),
            ),
        ]);
    }

    #[test]
    fn a_rollback_should_undo_the_publications_and_put_the_backups_back() {
        // Arrange
        let directory = scratch_directory("rollback");
        let _cleanup = DirectoryCleanup(Some(directory.clone()));
        let published = [directory.join("movie.mkv"), directory.join("movie.eng.srt")];
        for path in &published {
            fs::write(path, "new").unwrap();
        }
        let backup = directory.join(".movie.mkv.backup");
        fs::write(&backup, "old").unwrap();

        // Act
        rollback_transaction(&published, &[(backup.clone(), published[0].clone())]);

        // Assert: the published subtitle is gone, and the media file is the original
        // again rather than the version the edit wrote.
        assert_that!(published[1].exists()).is_false();
        assert_that!(backup.exists()).is_false();
        assert_that!(fs::read_to_string(&published[0]).unwrap().as_str()).is_equal_to("old");
    }

    /// A rollback runs when something has already gone wrong, so it must not compound
    /// the failure by panicking on the half of the transaction that never happened.
    #[test]
    fn a_rollback_should_ignore_files_that_were_never_written() {
        // Arrange
        let directory = scratch_directory("rollback-partial");
        let _cleanup = DirectoryCleanup(Some(directory.clone()));

        // Act
        rollback_transaction(
            &[directory.join("never-published.mkv")],
            &[(
                directory.join("missing.backup"),
                directory.join("original.mkv"),
            )],
        );

        // Assert
        assert_that!(directory.join("original.mkv").exists()).is_false();
    }

    #[test]
    fn publishing_across_filesystems_should_copy_with_progress_and_still_move_the_file() {
        // Arrange: `fs::rename` only fails with EXDEV across mounts, which is the whole
        // reason the copy fallback exists — so the test needs two real filesystems.
        let shared_memory = Path::new("/dev/shm");
        assert!(
            shared_memory.is_dir(),
            "publishing_across_filesystems_should_copy_with_progress_and_still_move_the_file \
             requires /dev/shm"
        );
        let directory = scratch_directory("publish-cross-device");
        let _cleanup = DirectoryCleanup(Some(directory.clone()));
        let source = shared_memory.join(format!(
            "reel-tui-cross-device-{}-{}.mkv",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _source_cleanup = TempCleanup(Some(source.clone()));
        // Two buffers' worth, so the copy loop reports more than once.
        let contents = vec![7_u8; 2 * 1024 * 1024 + 17];
        fs::write(&source, &contents).unwrap();
        use std::os::unix::fs::MetadataExt as _;
        assert_ne!(
            fs::metadata(&source).unwrap().dev(),
            fs::metadata(&directory).unwrap().dev(),
            "publishing_across_filesystems_should_copy_with_progress_and_still_move_the_file \
             requires /dev/shm and the temp directory to use different filesystems"
        );
        let destination = directory.join("movie.mkv");
        let mut reported = Vec::new();

        // Act
        let result = move_or_copy_file_with_progress(
            &source,
            &destination,
            EditPhase::PublishFile("movie.mkv".to_string()),
            &AtomicBool::new(false),
            &mut |progress| reported.push(progress),
        );

        // Assert
        result.unwrap();
        assert_that!(fs::read(&destination).unwrap()).is_equal_to(contents);
        assert_that!(source.exists()).is_false();
        assert_that!(reported[0].fraction).is_none();
        assert_that!(reported.len() > 2).is_true();
        assert_that!(reported.last().unwrap().fraction).contains(1.0);
        assert_that!(
            reported
                .iter()
                .all(|progress| progress.label() == "Saving movie.mkv")
        )
        .is_true();
    }

    #[test]
    fn a_cross_filesystem_copy_should_stop_the_moment_the_edit_is_cancelled() {
        // Arrange
        let shared_memory = Path::new("/dev/shm");
        assert!(
            shared_memory.is_dir(),
            "a_cross_filesystem_copy_should_stop_the_moment_the_edit_is_cancelled requires \
             /dev/shm"
        );
        let directory = scratch_directory("publish-cancelled");
        let _cleanup = DirectoryCleanup(Some(directory.clone()));
        let source = shared_memory.join(format!(
            "reel-tui-cancelled-{}-{}.mkv",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _source_cleanup = TempCleanup(Some(source.clone()));
        fs::write(&source, vec![7_u8; 1024]).unwrap();
        use std::os::unix::fs::MetadataExt as _;
        assert_ne!(
            fs::metadata(&source).unwrap().dev(),
            fs::metadata(&directory).unwrap().dev(),
            "a_cross_filesystem_copy_should_stop_the_moment_the_edit_is_cancelled requires \
             /dev/shm and the temp directory to use different filesystems"
        );

        // Act
        let result = move_or_copy_file_with_progress(
            &source,
            &directory.join("movie.mkv"),
            EditPhase::PublishFile("movie.mkv".to_string()),
            &AtomicBool::new(true),
            &mut |_| {},
        );

        // Assert: the source survives a cancellation, which is what lets the caller
        // roll the whole transaction back.
        assert_that!(result.unwrap_err().kind()).is_equal_to(std::io::ErrorKind::Interrupted);
        assert_that!(source.exists()).is_true();
    }

    #[test]
    fn saving_to_a_copy_should_rename_the_sidecars_onto_the_copys_name() {
        // Arrange
        let publications = [Publication {
            staged: vec![
                (
                    PathBuf::from("/work/staged-1.srt"),
                    PathBuf::from("/videos/movie.eng.srt"),
                ),
                (
                    PathBuf::from("/work/staged-2.srt"),
                    // A destination that does not start with the source stem keeps its
                    // own name — it was never derived from the media file.
                    PathBuf::from("/videos/extras.dan.srt"),
                ),
            ],
            remove: vec![PathBuf::from("/videos/movie.old.srt")],
        }];

        // Act
        let retargeted = retarget_publications_for_copy(
            &publications,
            Path::new("/videos/movie.mkv"),
            Path::new("/videos/movie-reel-edit.mkv"),
        )
        .unwrap();

        // Assert
        assert_that!(retargeted[0].staged[0].1.clone())
            .is_equal_to(PathBuf::from("/videos/movie-reel-edit.eng.srt"));
        assert_that!(retargeted[0].staged[1].1.clone())
            .is_equal_to(PathBuf::from("/videos/extras.dan.srt"));
        // The staged work files and the removal list are the copy's business as much as
        // the original's, so they come through untouched.
        assert_that!(retargeted[0].staged[0].0.clone())
            .is_equal_to(PathBuf::from("/work/staged-1.srt"));
        assert_that!(retargeted[0].remove.clone())
            .is_equal_to(vec![PathBuf::from("/videos/movie.old.srt")]);
    }

    #[test]
    fn saving_to_a_copy_should_refuse_a_filename_that_is_not_valid_text() {
        // Arrange
        use std::os::unix::ffi::OsStrExt as _;
        let invalid = PathBuf::from(std::ffi::OsStr::from_bytes(b"/videos/mo\xffvie.mkv"));
        let publications = [Publication {
            staged: vec![(
                PathBuf::from("/work/staged.srt"),
                PathBuf::from("/videos/movie.eng.srt"),
            )],
            remove: Vec::new(),
        }];

        // Act
        let bad_source = retarget_publications_for_copy(
            &publications,
            &invalid,
            Path::new("/videos/movie-reel-edit.mkv"),
        );
        let bad_copy =
            retarget_publications_for_copy(&publications, Path::new("/videos/movie.mkv"), &invalid);

        // Assert
        assert_that!(matches!(bad_source, Err(EditError::Failed(ref message)) if message.contains("source filename")))
            .is_true();
        assert_that!(matches!(bad_copy, Err(EditError::Failed(ref message)) if message.contains("copy filename")))
            .is_true();
    }

    fn track(index: u64, kind: &str, codec: &str) -> Value {
        serde_json::json!({"index": index, "codec_type": kind, "codec_name": codec})
    }

    fn audio_settings() -> AudioSettings {
        AudioSettings {
            codec: AudioCodec::Original,
            channel_layout: AudioChannelLayout::Original,
            metadata: AudioMetadata {
                language: "eng".to_string(),
                title: None,
                commentary: false,
                hearing_impaired: false,
                audio_description: false,
                original: false,
                dubbed: false,
            },
        }
    }

    #[test]
    fn audio_title_should_ignore_only_the_generated_mp4_handler_name() {
        let stream =
            |tags: Value| serde_json::from_value(serde_json::json!({"tags": tags})).unwrap();
        let generated = audio_stream_title(&stream(serde_json::json!({
            "handler_name": "SoundHandler"
        })));
        let generated_lowercase = audio_stream_title(&stream(serde_json::json!({
            "handler_name": "soundhandler"
        })));
        let custom = audio_stream_title(&stream(serde_json::json!({
            "handler_name": "Director commentary"
        })));
        let explicit = audio_stream_title(&stream(serde_json::json!({
            "title": "SoundHandler",
            "handler_name": "SoundHandler"
        })));
        let named = audio_stream_title(&stream(serde_json::json!({
            "title": "  ",
            "name": " Alternate name ",
            "handler_name": "Ignored fallback"
        })));
        let blank = audio_stream_title(&stream(serde_json::json!({
            "handler_name": "  "
        })));
        let no_tags = audio_stream_title(&BTreeMap::new());

        assert_that!(generated).is_none();
        assert_that!(generated_lowercase).is_none();
        assert_that!(custom.as_deref()).contains("Director commentary");
        assert_that!(explicit).contains("SoundHandler".to_string());
        assert_that!(named).contains("Alternate name".to_string());
        assert_that!(blank).is_none();
        assert_that!(no_tags).is_none();
    }

    #[test]
    fn audio_metadata_matching_should_compare_every_field() {
        let stream: BTreeMap<String, Value> = serde_json::from_value(serde_json::json!({
            "tags": {"language": "eng", "title": "Accessible original dub"},
            "disposition": {
                "comment": 1,
                "hearing_impaired": 1,
                "visual_impaired": 1,
                "original": 1,
                "dub": 1
            }
        }))
        .unwrap();
        let expected = AudioMetadata {
            language: "eng".to_string(),
            title: Some("Accessible original dub".to_string()),
            commentary: true,
            hearing_impaired: true,
            audio_description: true,
            original: true,
            dubbed: true,
        };
        assert!(audio_metadata_matches(&stream, &expected));

        for changed in [
            AudioMetadata {
                language: "nld".to_string(),
                ..expected.clone()
            },
            AudioMetadata {
                title: Some("Other".to_string()),
                ..expected.clone()
            },
            AudioMetadata {
                commentary: false,
                ..expected.clone()
            },
            AudioMetadata {
                hearing_impaired: false,
                ..expected.clone()
            },
            AudioMetadata {
                audio_description: false,
                ..expected.clone()
            },
            AudioMetadata {
                original: false,
                ..expected.clone()
            },
            AudioMetadata {
                dubbed: false,
                ..expected.clone()
            },
        ] {
            assert!(!audio_metadata_matches(&stream, &changed));
        }
    }

    #[test]
    fn audio_output_validation_should_reject_each_mismatch_and_normalize_for_container() {
        let source = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264"},
            {"index": 1, "codec_type": "audio", "codec_name": "aac",
             "channels": 2, "sample_rate": "96000", "tags": {"language": "eng"}}
        ]));
        let output = |codec: &str, channels: u8, rate: u32, original: bool| {
            media(serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "audio", "codec_name": codec,
                 "channels": channels, "sample_rate": rate.to_string(),
                 "tags": {"language": "eng", "handler_name": "SoundHandler"},
                 "disposition": {"original": u8::from(original)}}
            ]))
        };
        let validate =
            |settings: AudioSettings, output: MediaInfo, container: Option<ContainerFormat>| {
                validate_result(
                    &source,
                    &output,
                    &[0, 1],
                    &[],
                    &BTreeSet::new(),
                    &BTreeMap::from([(1, settings)]),
                    &BTreeMap::new(),
                    &[],
                    &[],
                    &[],
                    &[],
                    container,
                )
            };

        let mut encoded = audio_settings();
        encoded.codec = AudioCodec::Ac3;
        assert_that!(validate(encoded.clone(), output("aac", 2, 48_000, false), None).unwrap_err())
            .contains("wrong codec");

        encoded.channel_layout = AudioChannelLayout::Mono;
        assert_that!(validate(encoded.clone(), output("ac3", 2, 48_000, false), None).unwrap_err())
            .contains("wrong channel layout");

        encoded.channel_layout = AudioChannelLayout::Original;
        assert_that!(validate(encoded.clone(), output("ac3", 2, 96_000, false), None).unwrap_err())
            .contains("wrong sample rate");

        encoded.metadata.commentary = true;
        assert_that!(validate(encoded.clone(), output("ac3", 2, 48_000, false), None).unwrap_err())
            .contains("wrong metadata");

        encoded.metadata.commentary = false;
        encoded.metadata.original = true;
        assert_that!(validate(
            encoded,
            output("ac3", 2, 48_000, false),
            Some(ContainerFormat::Mp4),
        ))
        .is_ok();
    }

    fn defaulted(mut stream: Value) -> Value {
        stream["disposition"] = serde_json::json!({"default": 1});
        stream
    }

    /// `media` refuses a document with no playable video track, which is exactly the
    /// shape some of these rejections need to hand `validate_result`.
    fn raw_media(streams: Value) -> MediaInfo {
        MediaInfo::from_json_unchecked(serde_json::json!({"streams": streams})).unwrap()
    }

    fn validate_plain(
        source: &MediaInfo,
        output: &MediaInfo,
        stream_order: &[u64],
        default_streams: &BTreeSet<u64>,
    ) -> Result<(), String> {
        validate_result(
            source,
            output,
            stream_order,
            &[],
            default_streams,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &[],
            &[],
            &[],
            &[],
            None,
        )
    }

    /// `validate_result` is the last gate before a remuxed file replaces the original,
    /// so each refusal below is a wrong file that would otherwise be published with
    /// nothing left to catch it. A real `ffmpeg` run only ever reaches the accept path,
    /// which is why the rejections are pinned directly.
    #[test]
    fn validation_should_reject_an_output_left_without_a_playable_video_track() {
        // Arrange
        let source = media(serde_json::json!([
            track(0, "video", "h264"),
            track(1, "audio", "aac"),
        ]));
        let audio_only = raw_media(serde_json::json!([track(1, "audio", "aac")]));
        let cover_art_only = raw_media(serde_json::json!([
            {
                "index": 0,
                "codec_type": "video",
                "codec_name": "mjpeg",
                "disposition": {"attached_pic": 1},
            },
            track(1, "audio", "aac"),
        ]));

        // Act
        let dropped = validate_plain(&source, &audio_only, &[0, 1], &BTreeSet::new());
        let cover_art = validate_plain(&source, &cover_art_only, &[0, 1], &BTreeSet::new());

        // Assert
        assert_that!(dropped.unwrap_err().as_str()).contains("no playable video track");
        assert_that!(cover_art.unwrap_err().as_str()).contains("no playable video track");
    }

    #[test]
    fn validation_should_reject_an_output_whose_tracks_came_back_in_a_different_order() {
        // Arrange
        let streams = serde_json::json!([
            track(0, "video", "h264"),
            track(1, "audio", "aac"),
            track(2, "subtitle", "subrip"),
        ]);
        let source = media(streams.clone());
        // The muxer wrote source order; the request asked for the subtitle in the middle.
        let output = media(streams);

        // Act
        let result = validate_plain(&source, &output, &[0, 2, 1], &BTreeSet::new());

        // Assert
        assert_that!(result.unwrap_err().as_str()).contains("not in the requested order");
    }

    #[test]
    fn validation_should_reject_an_output_carrying_a_track_nothing_asked_for() {
        // Arrange
        let source = media(serde_json::json!([track(0, "video", "h264")]));
        // A track with no `codec_type` slips past the kind comparison, so only the
        // per-position walk can notice the file has one track too many.
        let output = media(serde_json::json!([
            track(0, "video", "h264"),
            {"index": 1, "codec_name": "bin_data"},
        ]));

        // Act
        let result = validate_plain(&source, &output, &[0], &BTreeSet::new());

        // Assert
        assert_that!(result.unwrap_err().as_str()).contains("unexpected extra track");
    }

    #[test]
    fn validation_should_reject_an_output_whose_default_flags_do_not_match_the_request() {
        // Arrange
        let source = media(serde_json::json!([
            track(0, "video", "h264"),
            track(1, "audio", "aac"),
        ]));
        let audio_undefaulted = media(serde_json::json!([
            track(0, "video", "h264"),
            track(1, "audio", "aac"),
        ]));
        let audio_defaulted = media(serde_json::json!([
            track(0, "video", "h264"),
            defaulted(track(1, "audio", "aac")),
        ]));

        // Act
        let missing = validate_plain(
            &source,
            &audio_undefaulted,
            &[0, 1],
            &BTreeSet::from([1_u64]),
        );
        let unrequested = validate_plain(&source, &audio_defaulted, &[0, 1], &BTreeSet::new());

        // Assert: both directions are failures, and the message names the source track
        // so the log line is enough to tell which one drifted.
        let missing = missing.unwrap_err();
        assert_that!(missing.as_str()).contains("wrong default flag");
        assert_that!(missing.as_str()).contains("source track #1");
        assert_that!(unrequested.unwrap_err().as_str()).contains("wrong default flag");
    }

    /// A save that touches no media stream skips `ffmpeg` entirely and only publishes
    /// sidecars, so each of these has to be enough on its own to force the remux —
    /// missing one means the file silently keeps the old tracks.
    #[test]
    fn any_single_media_change_should_be_enough_to_require_a_remux() {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264",
             "disposition": {"default": 1}},
            {"index": 1, "codec_type": "audio", "codec_name": "aac"},
            {"index": 2, "codec_type": "subtitle", "codec_name": "subrip",
             "tags": {"language": "eng"}},
        ]));
        let untouched = |deleted, defaults, settings, changes: &[SubtitleChange], container| {
            media_changes_required(
                &info,
                &[0, 1, 2],
                deleted,
                defaults,
                &BTreeMap::new(),
                settings,
                changes,
                container,
            )
        };
        let defaults = BTreeSet::from([0_u64]);
        let no_deletions = BTreeSet::new();
        let no_defaults = BTreeSet::new();
        let no_settings = BTreeMap::new();
        let recoded = BTreeMap::from([(
            0_u64,
            VideoSettings {
                codec: VideoCodec::Hevc,
                resolution: VideoResolution::Original,
                metadata: VideoMetadata {
                    language: "und".to_string(),
                    title: None,
                    commentary: false,
                },
                rotation: VideoRotation::None,
            },
        )]);
        let exported = [SubtitleChange {
            cue_edits: Default::default(),
            source: SubtitleSource::Embedded(2),
            source_format: SubtitleFormat::SubRip,
            embedded_target: None,
            export_target: Some(SubtitleFormat::SubRip),
            import_into_media: false,
            ocr_language: None,
            metadata: None,
        }];
        let deleted = BTreeSet::from([1_u64]);

        // Act / Assert
        assert_that!(untouched(
            &no_deletions,
            &defaults,
            &no_settings,
            &[],
            false
        ))
        .is_false();
        assert_that!(untouched(&deleted, &defaults, &no_settings, &[], false)).is_true();
        // A different default set counts even when the order is untouched.
        assert_that!(untouched(
            &no_deletions,
            &no_defaults,
            &no_settings,
            &[],
            false
        ))
        .is_true();
        assert_that!(untouched(&no_deletions, &defaults, &recoded, &[], false)).is_true();
        assert_that!(untouched(
            &no_deletions,
            &defaults,
            &no_settings,
            &exported,
            false
        ))
        .is_true();
        assert_that!(untouched(&no_deletions, &defaults, &no_settings, &[], true)).is_true();
        // A reordered request is a change even with the same members.
        assert_that!(media_changes_required(
            &info,
            &[0, 2, 1],
            &BTreeSet::new(),
            &defaults,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &[],
            false
        ))
        .is_true();
    }

    /// The left column's order is the output order, and imports have to land where the
    /// user put them rather than being appended — anything the column does not mention
    /// still has to come out, in source order, after what it does.
    #[test]
    fn the_output_plan_should_follow_the_left_column_and_then_pick_up_the_leftovers() {
        // Arrange
        let source = media(serde_json::json!([
            track(0, "video", "h264"),
            track(1, "audio", "aac"),
            subtitle_track(2, "subrip", "eng"),
            subtitle_track(3, "subrip", "dan"),
            {"index": 4, "codec_type": "attachment", "codec_name": "ttf"},
        ]));
        let sidecars = [
            sidecar_entry(
                Path::new("/videos/movie.nld.srt"),
                None,
                SubtitleFormat::SubRip,
            ),
            sidecar_entry(
                Path::new("/videos/movie.fra.srt"),
                None,
                SubtitleFormat::SubRip,
            ),
        ];
        let imports = sidecars
            .iter()
            .map(|sidecar| SubtitleImport {
                source_path: sidecar.path.clone(),
                target: SubtitleFormat::SubRip,
                path: sidecar.path.clone(),
                metadata: english_subtitle_metadata(),
                default: false,
            })
            .collect::<Vec<_>>();
        // The column puts the Danish track first and the Dutch sidecar after it. The
        // English track and the French import are not mentioned at all, and the stale
        // `Embedded(9)` refers to a track this file no longer has.
        let left = [
            TrackRef::Embedded(3),
            TrackRef::Sidecar(0),
            TrackRef::Embedded(9),
            TrackRef::Sidecar(7),
        ];

        // Act
        let plan = output_track_plan(&source, &[0, 1, 2, 3, 4], &left, &imports, &sidecars);

        // Assert
        assert_that!(plan).is_equal_to(vec![
            OutputTrack::Existing(0),
            OutputTrack::Existing(1),
            OutputTrack::Existing(3),
            OutputTrack::Imported(0),
            OutputTrack::Existing(2),
            OutputTrack::Imported(1),
            OutputTrack::Existing(4),
        ]);
    }

    /// Remuxing beside the source on a network share would stream every intermediate
    /// byte back over the wire; remuxing a local file into `/tmp` would turn a rename
    /// into a full copy. The mount type is the only thing that decides which.
    #[test]
    fn intermediate_files_should_land_on_local_scratch_only_for_a_network_source() {
        // Act
        let local = work_parent(Path::new("/videos/movie.mkv"), false).unwrap();
        let network = work_parent(Path::new("/mnt/share/movie.mkv"), true).unwrap();
        let no_parent = work_parent(Path::new("/"), false);

        // Assert
        assert_that!(local).is_equal_to(PathBuf::from("/videos"));
        assert_that!(network.clone()).is_equal_to(std::env::temp_dir().join("reel-tui-scratch"));
        assert_that!(network.is_dir()).is_true();
        assert_that!(no_parent.unwrap_err().as_str()).contains("no parent directory");
    }

    /// The publish step is the only moment the user's files are touched, and it is
    /// all-or-nothing: anything it cannot do safely has to be refused *before* the
    /// first rename, while the originals are still where they were.
    #[test]
    fn publishing_should_refuse_before_touching_anything_it_cannot_do_safely() {
        // Arrange
        let directory = scratch_directory("publish-refusals");
        let _cleanup = DirectoryCleanup(Some(directory.clone()));
        let staged_one = directory.join("staged-1.srt");
        let staged_two = directory.join("staged-2.srt");
        fs::write(&staged_one, "one").unwrap();
        fs::write(&staged_two, "two").unwrap();
        let occupied = directory.join("already-there.srt");
        fs::write(&occupied, "existing").unwrap();
        let publish = |publications: &[Publication]| {
            publish_transaction_with_progress(
                None,
                None,
                publications,
                &AtomicBool::new(false),
                &mut |_| {},
            )
        };

        // Act
        let collision = publish(&[
            Publication {
                staged: vec![(staged_one.clone(), directory.join("movie.eng.srt"))],
                remove: Vec::new(),
            },
            Publication {
                staged: vec![(staged_two.clone(), directory.join("movie.eng.srt"))],
                remove: Vec::new(),
            },
        ]);
        let overwrite = publish(&[Publication {
            staged: vec![(staged_one.clone(), occupied.clone())],
            remove: Vec::new(),
        }]);
        let nowhere_to_stage = publish(&[]);
        let cancelled = publish_transaction_with_progress(
            None,
            None,
            &[Publication {
                staged: vec![(staged_one.clone(), directory.join("movie.eng.srt"))],
                remove: Vec::new(),
            }],
            &AtomicBool::new(true),
            &mut |_| {},
        );

        // Assert: every refusal left the staged work and the existing file untouched.
        let Err(EditError::Failed(collision)) = collision else {
            panic!("two outputs on one path must be refused");
        };
        assert_that!(collision.as_str()).contains("Two subtitle outputs resolve to");
        let Err(EditError::Failed(overwrite)) = overwrite else {
            panic!("an occupied destination must be refused");
        };
        assert_that!(overwrite.as_str()).contains("already exists; no files were changed.");
        let Err(EditError::Failed(nowhere_to_stage)) = nowhere_to_stage else {
            panic!("a publish with nothing staged has no backup directory");
        };
        assert_that!(nowhere_to_stage.as_str()).contains("No staging directory is available.");
        assert_that!(matches!(cancelled, Err(EditError::Cancelled))).is_true();
        assert_that!(fs::read_to_string(&occupied).unwrap().as_str()).is_equal_to("existing");
        assert_that!(staged_one.exists()).is_true();
        assert_that!(staged_two.exists()).is_true();
    }

    #[test]
    fn publishing_should_replace_the_media_and_its_sidecars_in_one_transaction() {
        // Arrange
        let directory = scratch_directory("publish-transaction");
        let _cleanup = DirectoryCleanup(Some(directory.clone()));
        let staged_media = directory.join(".reel-tui-staged.mkv");
        let staged_subtitle = directory.join(".reel-tui-staged.srt");
        fs::write(&staged_media, "new media").unwrap();
        fs::write(&staged_subtitle, "new subtitle").unwrap();
        let media = directory.join("movie.mkv");
        let subtitle = directory.join("movie.eng.srt");
        fs::write(&media, "old media").unwrap();
        let mut reported = Vec::new();

        // Act
        let result = publish_transaction_with_progress(
            Some((&staged_media, &media)),
            None,
            &[Publication {
                staged: vec![(staged_subtitle.clone(), subtitle.clone())],
                // A removal target that was never written is skipped rather than
                // failing the whole transaction.
                remove: vec![media.clone(), directory.join("movie.old.srt")],
            }],
            &AtomicBool::new(false),
            &mut |progress| reported.push(progress.label()),
        );

        // Assert
        result.unwrap();
        assert_that!(fs::read_to_string(&media).unwrap().as_str()).is_equal_to("new media");
        assert_that!(fs::read_to_string(&subtitle).unwrap().as_str()).is_equal_to("new subtitle");
        assert_that!(staged_media.exists()).is_false();
        // No backup is left behind once the transaction commits.
        assert_that!(
            fs::read_dir(&directory)
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .contains("transaction-backup"))
        )
        .is_false();
        assert_that!(reported.first().map(String::as_str)).contains("Preparing to save");
        assert_that!(reported.iter().any(|label| label.starts_with("Backing up"))).is_true();
        assert_that!(reported.last().map(String::as_str)).contains("Removing backups");
    }

    /// A VobSub subtitle is two files. Staging only the `.sub` produces a subtitle
    /// track that decodes to nothing, and `seconv` reads the `.idx` rather than the
    /// `.sub` when the pair is used as an input.
    #[test]
    fn a_vobsub_artifact_should_be_staged_and_read_as_a_sub_idx_pair() {
        // Arrange
        let directory = scratch_directory("vobsub-artifact");
        let _cleanup = DirectoryCleanup(Some(directory.clone()));
        let source = directory.join("movie.eng.sub");
        fs::write(&source, "sub").unwrap();
        fs::write(directory.join("movie.eng.idx"), "idx").unwrap();
        let text = directory.join("movie.dan.srt");
        fs::write(&text, "1\n").unwrap();
        let mut reported = Vec::new();

        // Act
        copy_subtitle_artifact(
            &source,
            &directory.join("staged.sub"),
            SubtitleFormat::VobSub,
            "movie.eng.sub",
            &AtomicBool::new(false),
            &mut |progress| reported.push(progress.label()),
        )
        .unwrap();
        copy_subtitle_artifact(
            &text,
            &directory.join("staged.srt"),
            SubtitleFormat::SubRip,
            "movie.dan.srt",
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();

        // Assert
        assert_that!(
            fs::read_to_string(directory.join("staged.sub"))
                .unwrap()
                .as_str()
        )
        .is_equal_to("sub");
        assert_that!(
            fs::read_to_string(directory.join("staged.idx"))
                .unwrap()
                .as_str()
        )
        .is_equal_to("idx");
        assert_that!(directory.join("staged.srt").exists()).is_true();
        // A text subtitle has no companion to stage.
        assert_that!(directory.join("staged.idx.srt").exists()).is_false();
        assert_that!(
            reported
                .iter()
                .all(|label| label == "Copying movie.eng.sub")
        )
        .is_true();
        // And the pair is fed to the converter by its `.idx`, not its `.sub`.
        assert_that!(subtitle_input_path(&source, SubtitleFormat::VobSub))
            .is_equal_to(directory.join("movie.eng.idx"));
        assert_that!(subtitle_input_path(&text, SubtitleFormat::SubRip)).is_equal_to(text);
    }

    #[test]
    fn a_missing_vobsub_companion_should_fail_the_export_rather_than_half_publish_it() {
        // Arrange
        let directory = scratch_directory("vobsub-artifact-missing");
        let _cleanup = DirectoryCleanup(Some(directory.clone()));
        let source = directory.join("movie.eng.sub");
        fs::write(&source, "sub").unwrap();

        // Act
        let result = copy_subtitle_artifact(
            &source,
            &directory.join("staged.sub"),
            SubtitleFormat::VobSub,
            "movie.eng.sub",
            &AtomicBool::new(false),
            &mut |_| {},
        );

        // Assert
        let Err(EditError::Failed(message)) = result else {
            panic!("a VobSub export without its .idx must fail");
        };
        assert_that!(message.as_str()).contains("VobSub .idx companion");
    }

    /// Cancelling has to actually stop the tool rather than wait for it to finish, and
    /// it must say so through the progress channel — the Save dialog is still on screen
    /// showing whatever the last phase was until this reports the stop.
    #[test]
    fn cancelling_should_kill_the_running_tool_and_report_the_stop() {
        // Arrange: `sleep` would otherwise run far longer than the test.
        let mut command = Command::new("sleep");
        command.arg("30");
        let mut reported = Vec::new();

        // Act
        let started = SystemTime::now();
        let result =
            run_cancellable_output(&mut command, &AtomicBool::new(true), &mut |progress| {
                reported.push(progress.label())
            });

        // Assert
        assert_that!(matches!(result, Err(EditError::Cancelled))).is_true();
        assert!(
            started.elapsed().unwrap() < Duration::from_secs(5),
            "cancelling must not wait for the tool to finish on its own",
        );
        assert_that!(reported).is_equal_to(vec![
            "Stopping tools".to_string(),
            "Cleaning up".to_string(),
        ]);
    }

    #[test]
    fn cancelling_a_subtitle_copy_should_leave_the_destination_unfinished_and_say_so() {
        // Arrange
        let directory = scratch_directory("copy-cancelled");
        let _cleanup = DirectoryCleanup(Some(directory.clone()));
        let source = directory.join("movie.eng.srt");
        fs::write(&source, vec![7_u8; 4096]).unwrap();

        // Act
        let result = copy_file_with_progress(
            &source,
            directory.join("staged.srt"),
            EditPhase::CopySubtitle("movie.eng.srt".to_string()),
            &AtomicBool::new(true),
            &mut |_| {},
        );

        // Assert: the caller sees `Interrupted`, which is what routes it to
        // `EditError::Cancelled` rather than a failure the user has to fix.
        assert_that!(result.unwrap_err().kind()).is_equal_to(std::io::ErrorKind::Interrupted);
        assert_that!(source.exists()).is_true();
    }

    #[test]
    fn copying_an_empty_file_should_report_the_phase_without_a_bogus_fraction() {
        // Arrange
        let directory = scratch_directory("copy-empty");
        let _cleanup = DirectoryCleanup(Some(directory.clone()));
        let source = directory.join("empty.srt");
        fs::write(&source, b"").unwrap();
        let destination = directory.join("copy.srt");
        let mut reported = Vec::new();

        // Act
        copy_file_with_progress(
            &source,
            &destination,
            EditPhase::CopySubtitle("empty.srt".to_string()),
            &AtomicBool::new(false),
            &mut |progress| reported.push(progress),
        )
        .unwrap();

        // Assert: dividing by a zero total would have produced NaN, so the only report
        // is the indeterminate one.
        assert_that!(destination.exists()).is_true();
        assert_that!(reported.len()).is_equal_to(1);
        assert_that!(reported[0].fraction).is_none();
    }

    #[test]
    fn a_converted_subtitle_should_be_rejected_when_it_is_not_the_format_that_was_asked_for() {
        // Arrange
        require_tools(
            "a_converted_subtitle_should_be_rejected_when_it_is_not_the_format_that_was_asked_for",
            &["ffprobe"],
        );
        let directory = scratch_directory("validate-subtitle-output");
        let _cleanup = DirectoryCleanup(Some(directory.clone()));
        let subtitle = directory.join("movie.eng.srt");
        fs::write(
            &subtitle,
            "1\n00:00:00,000 --> 00:00:01,000\nHello\n\n2\n00:00:01,000 --> 00:00:02,000\nAgain\n",
        )
        .unwrap();

        // Act
        let matching = validate_subtitle_output(&subtitle, SubtitleFormat::SubRip);
        let wrong_format = validate_subtitle_output(&subtitle, SubtitleFormat::Ass);

        // Assert
        assert_that!(matching.is_ok()).is_true();
        let Err(EditError::Failed(message)) = wrong_format else {
            panic!("a SubRip file must not validate as ASS");
        };
        assert_that!(message.as_str()).contains("did not validate as ASS");
    }

    fn subtitle_track(index: u64, codec: &str, language: &str) -> Value {
        serde_json::json!({
            "index": index,
            "codec_type": "subtitle",
            "codec_name": codec,
            "tags": {"language": language},
        })
    }

    fn validate_subtitles(
        source: &MediaInfo,
        output: &MediaInfo,
        stream_order: &[u64],
        replacements: &[SubtitleReplacement],
        imports: &[SubtitleImport],
        changes: &[SubtitleChange],
    ) -> Result<(), String> {
        validate_result(
            source,
            output,
            stream_order,
            &[],
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            replacements,
            imports,
            changes,
            &[],
            None,
        )
    }

    #[test]
    fn validation_should_reject_an_imported_subtitle_with_the_wrong_codec_or_metadata() {
        // Arrange
        let source = media(serde_json::json!([
            track(0, "video", "h264"),
            track(1, "audio", "aac"),
        ]));
        let imports = [SubtitleImport {
            source_path: PathBuf::from("movie.eng.srt"),
            target: SubtitleFormat::SubRip,
            path: PathBuf::from("movie.eng.srt"),
            metadata: english_subtitle_metadata(),
            default: false,
        }];
        let output = |subtitle: Value| {
            media(serde_json::json!([
                track(0, "video", "h264"),
                track(1, "audio", "aac"),
                subtitle,
            ]))
        };

        // Act: the import lands last, so position 2 is the one under test.
        let wrong_codec = validate_subtitles(
            &source,
            &output(subtitle_track(2, "ass", "eng")),
            &[0, 1],
            &[],
            &imports,
            &[],
        );
        let wrong_language = validate_subtitles(
            &source,
            &output(subtitle_track(2, "subrip", "dan")),
            &[0, 1],
            &[],
            &imports,
            &[],
        );
        let matching = validate_subtitles(
            &source,
            &output(subtitle_track(2, "subrip", "eng")),
            &[0, 1],
            &[],
            &imports,
            &[],
        );

        // Assert
        assert_that!(wrong_codec.unwrap_err().as_str())
            .contains("imported subtitle track at position 2 has the wrong codec");
        assert_that!(wrong_language.unwrap_err().as_str())
            .contains("imported subtitle track at position 2 has the wrong metadata");
        assert_that!(matching).is_ok();
    }

    #[test]
    fn validation_should_reject_an_embedded_subtitle_that_kept_its_old_metadata() {
        // Arrange
        let source = media(serde_json::json!([
            track(0, "video", "h264"),
            subtitle_track(1, "subrip", "eng"),
        ]));
        let mut retagged = english_subtitle_metadata();
        retagged.language = "dan".to_string();
        retagged.title = Some("Dansk".to_string());
        let changes = [SubtitleChange {
            cue_edits: Default::default(),
            source: SubtitleSource::Embedded(1),
            source_format: SubtitleFormat::SubRip,
            embedded_target: None,
            export_target: None,
            import_into_media: false,
            ocr_language: None,
            metadata: Some(retagged),
        }];
        let unchanged = media(serde_json::json!([
            track(0, "video", "h264"),
            subtitle_track(1, "subrip", "eng"),
        ]));
        let retagged_output = media(serde_json::json!([
            track(0, "video", "h264"),
            {
                "index": 1,
                "codec_type": "subtitle",
                "codec_name": "subrip",
                "tags": {"language": "dan", "title": "Dansk"},
            },
        ]));

        // Act
        let stale = validate_subtitles(&source, &unchanged, &[0, 1], &[], &[], &changes);
        let applied = validate_subtitles(&source, &retagged_output, &[0, 1], &[], &[], &changes);

        // Assert
        assert_that!(stale.unwrap_err().as_str())
            .contains("subtitle track at position 1 has the wrong metadata");
        assert_that!(applied).is_ok();
    }

    #[test]
    fn validation_should_reject_a_converted_subtitle_still_in_its_source_format() {
        // Arrange
        let source = media(serde_json::json!([
            track(0, "video", "h264"),
            subtitle_track(1, "subrip", "eng"),
        ]));
        let replacements = [SubtitleReplacement {
            source_index: 1,
            target: SubtitleFormat::Ass,
            path: PathBuf::from("movie.eng.ass"),
        }];
        let unconverted = media(serde_json::json!([
            track(0, "video", "h264"),
            subtitle_track(1, "subrip", "eng"),
        ]));
        let converted = media(serde_json::json!([
            track(0, "video", "h264"),
            subtitle_track(1, "ass", "eng"),
        ]));

        // Act
        let stale = validate_subtitles(&source, &unconverted, &[0, 1], &replacements, &[], &[]);
        let applied = validate_subtitles(&source, &converted, &[0, 1], &replacements, &[], &[]);

        // Assert
        assert_that!(stale.unwrap_err().as_str())
            .contains("converted subtitle track at position 1 has the wrong codec");
        assert_that!(applied).is_ok();
    }

    #[test]
    fn validation_should_reject_an_encode_that_missed_its_codec_or_resolution() {
        // Arrange
        let source = media(serde_json::json!([{
            "index": 0,
            "codec_type": "video",
            "codec_name": "h264",
            "width": 1920,
            "height": 1080,
        }]));
        let output = |codec: &str, width: u64, height: u64| {
            media(serde_json::json!([{
                "index": 0,
                "codec_type": "video",
                "codec_name": codec,
                "width": width,
                "height": height,
            }]))
        };
        let validate = |output: &MediaInfo, settings: VideoSettings| {
            validate_result(
                &source,
                output,
                &[0],
                &[],
                &BTreeSet::new(),
                &BTreeMap::new(),
                &BTreeMap::from([(0_u64, settings)]),
                &[],
                &[],
                &[],
                &[],
                None,
            )
        };
        let to_hevc = VideoSettings {
            codec: VideoCodec::Hevc,
            resolution: VideoResolution::Original,
            metadata: VideoMetadata {
                language: "und".to_string(),
                title: None,
                commentary: false,
            },
            rotation: VideoRotation::None,
        };
        let to_720p = VideoSettings {
            codec: VideoCodec::Original,
            resolution: VideoResolution::P720,
            metadata: VideoMetadata {
                language: "und".to_string(),
                title: None,
                commentary: false,
            },
            rotation: VideoRotation::None,
        };

        // Act
        let not_encoded = validate(&output("h264", 1920, 1080), to_hevc.clone());
        let encoded = validate(&output("hevc", 1920, 1080), to_hevc);
        let not_scaled = validate(&output("h264", 1920, 1080), to_720p.clone());
        let scaled = validate(&output("h264", 1280, 720), to_720p);

        // Assert: a resize with no codec change still has to come back as the source
        // codec, which is why `scaled` passes while `not_scaled` fails on size alone.
        assert_that!(not_encoded.unwrap_err().as_str())
            .contains("encoded video track at position 0 has the wrong codec");
        assert_that!(encoded).is_ok();
        assert_that!(not_scaled.unwrap_err().as_str())
            .contains("encoded video track at position 0 has the wrong resolution");
        assert_that!(scaled).is_ok();
    }

    /// Settings that ask for nothing the source does not already have mean `ffmpeg` was
    /// told to `-c copy` that track, so validation must not hold the output to the
    /// requested codec — checking it would fail files the muxer wrote exactly as asked.
    #[test]
    fn validation_should_skip_a_video_track_no_setting_actually_changed() {
        // Arrange
        let source = media(serde_json::json!([{
            "index": 0,
            "codec_type": "video",
            "codec_name": "h264",
            "width": 1920,
            "height": 1080,
        }]));
        let output = media(serde_json::json!([{
            "index": 0,
            "codec_type": "video",
            "codec_name": "hevc",
            "width": 1920,
            "height": 1080,
        }]));

        // Act
        let result = validate_result(
            &source,
            &output,
            &[0],
            &[],
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeMap::from([(
                0_u64,
                VideoSettings {
                    codec: VideoCodec::H264,
                    resolution: VideoResolution::Original,
                    metadata: VideoMetadata {
                        language: "und".to_string(),
                        title: None,
                        commentary: false,
                    },
                    rotation: VideoRotation::None,
                },
            )]),
            &[],
            &[],
            &[],
            &[],
            None,
        );

        // Assert
        assert_that!(result).is_ok();
    }

    #[test]
    fn validation_should_accept_an_output_that_matches_the_request_exactly() {
        // Arrange
        let source = media(serde_json::json!([
            track(0, "video", "h264"),
            track(1, "audio", "aac"),
        ]));
        let output = media(serde_json::json!([
            defaulted(track(0, "video", "h264")),
            track(1, "audio", "aac"),
        ]));

        // Act
        let result = validate_plain(&source, &output, &[0, 1], &BTreeSet::from([0_u64]));

        // Assert
        assert_that!(result).is_ok();
    }

    #[test]
    fn apply_edits_should_remux_order_defaults_and_deletions_when_source_contains_multiple_tracks()
    {
        // Arrange
        require_tools(
            "apply_edits_should_remux_order_defaults_and_deletions_when_source_contains_multiple_tracks",
            &["ffmpeg"],
        );

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
            EditTarget {
                source: &source,
                destination: SaveDestination::ReplaceOriginal,
                container: None,
                container_metadata: None,
            },
            TrackEdits {
                stream_order: &[1, 0, 3],
                deleted_streams: &BTreeSet::from([2]),
                default_streams: &BTreeSet::from([1, 3]),
                default_sidecars: &BTreeSet::new(),
                audio_settings: &BTreeMap::new(),
                video_settings: &BTreeMap::new(),
                subtitle_changes: &[],
                left_subtitle_order: &[],
                sidecars: &[],
            },
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
        assert_that!(progress.last().unwrap().fraction).contains(1.0);
        let labels = progress.iter().map(EditProgress::label).collect::<Vec<_>>();
        let expected = [
            "Checking source",
            "Checking edits",
            "Preparing files",
            "Remuxing media",
            "Checking output",
            "Checking source files",
            "Preserving permissions",
            "Preparing to save",
            "Backing up tracks.mkv",
            "Saving tracks.mkv",
            "Removing backups",
            "Cleaning up",
            "Done",
        ];
        let mut next = 0;
        for expected_label in expected {
            let offset = labels[next..]
                .iter()
                .position(|label| label == expected_label)
                .unwrap_or_else(|| panic!("missing progress phase {expected_label:?}: {labels:?}"));
            next += offset + 1;
        }

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apply_edits_should_accept_mp4s_generated_handler_for_untitled_audio() {
        // Arrange
        require_tools(
            "apply_edits_should_accept_mp4s_generated_handler_for_untitled_audio",
            &["ffmpeg:aac"],
        );
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-container-copy-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("movie.mkv");
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
                "mpeg4",
                "-c:a",
                "aac",
                "-disposition:a:0",
                "default+original+comment",
            ])
            .arg(&source)
            .status()
            .unwrap();
        assert_that!(status.success()).is_true();
        let mut settings = audio_settings();
        settings.metadata.language = "eng".to_string();
        settings.metadata.commentary = true;
        let audio_settings = BTreeMap::from([(1, settings)]);

        // Act
        let output = apply_edits(
            EditTarget {
                source: &source,
                destination: SaveDestination::CreateCopy,
                container: Some(ContainerFormat::Mp4),
                container_metadata: None,
            },
            TrackEdits {
                stream_order: &[0, 1],
                deleted_streams: &BTreeSet::new(),
                default_streams: &BTreeSet::from([0, 1]),
                default_sidecars: &BTreeSet::new(),
                audio_settings: &audio_settings,
                video_settings: &BTreeMap::new(),
                subtitle_changes: &[],
                left_subtitle_order: &[],
                sidecars: &[],
            },
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        let output_info = media_info(&output.output_path).unwrap();

        // Assert
        assert_that!(source.exists()).is_true();
        assert_that!(output.output_path.file_name().unwrap().to_str().unwrap())
            .is_equal_to("movie-reel-edit.mp4");
        assert_that!(output_info.format["format_name"].as_str().unwrap()).contains("mp4");
        assert_that!(output_info.streams[0]["codec_name"].as_str()).contains("mpeg4");
        assert_that!(output_info.streams[1]["codec_name"].as_str()).contains("aac");
        assert_that!(audio_stream_title(&output_info.streams[1])).is_none();
        assert_that!(stream_original(&output_info.streams[1])).is_false();
        assert_that!(stream_commentary(&output_info.streams[1])).is_true();

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apply_edits_should_treat_a_stream_order_mismatch_as_stale_rather_than_a_hard_failure() {
        // Regression test: a staged edit computed against tracks that no longer match
        // the file's actual current tracks (`validate_edit`'s "tracks changed" case)
        // must come back as `EditError::SourceChanged`, not a generic
        // `EditError::Failed` — otherwise the caller has no way to tell "discard this
        // stale staged edit" apart from "the edit itself is invalid," and a retry
        // just fails identically forever instead of prompting a fresh re-stage.
        require_tools(
            "apply_edits_should_treat_a_stream_order_mismatch_as_stale_rather_than_a_hard_failure",
            &["ffmpeg"],
        );
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-stale-edit-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("movie.mkv");
        let status = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=16x16:r=1:d=1",
                "-c:v",
                "ffv1",
            ])
            .arg(&source)
            .status()
            .unwrap();
        assert_that!(status.success()).is_true();

        // Act: stage an edit that references stream index 5, which doesn't exist —
        // as if the file had more tracks when this was staged than it actually does
        // now.
        let result = apply_edits(
            EditTarget {
                source: &source,
                destination: SaveDestination::ReplaceOriginal,
                container: None,
                container_metadata: None,
            },
            TrackEdits {
                stream_order: &[0, 5],
                deleted_streams: &BTreeSet::new(),
                default_streams: &BTreeSet::new(),
                default_sidecars: &BTreeSet::new(),
                audio_settings: &BTreeMap::new(),
                video_settings: &BTreeMap::new(),
                subtitle_changes: &[],
                left_subtitle_order: &[],
                sidecars: &[],
            },
            &AtomicBool::new(false),
            |_| {},
        );

        // Assert
        match result {
            Err(EditError::SourceChanged(message)) => {
                assert_that!(message.as_str()).contains("tracks changed");
            }
            other => panic!("expected EditError::SourceChanged, got {other:?}"),
        }

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apply_edits_should_keep_a_genuinely_invalid_edit_as_a_hard_failure() {
        // Regression test: `classify_edit_error` must not sweep every validation
        // failure into `SourceChanged` — an edit that's invalid on its own terms
        // (here: marking a track default while also deleting it) is something the
        // user needs to actively fix, not something reopening the file resolves, so
        // it must stay `EditError::Failed` and the staged edit must survive.
        require_tools(
            "apply_edits_should_keep_a_genuinely_invalid_edit_as_a_hard_failure",
            &["ffmpeg"],
        );
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-invalid-edit-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("movie.mkv");
        let status = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=16x16:r=1:d=1",
                "-c:v",
                "ffv1",
            ])
            .arg(&source)
            .status()
            .unwrap();
        assert_that!(status.success()).is_true();

        // Act: track 0 is both deleted and marked default, a self-contradiction that
        // has nothing to do with the file's actual state.
        let result = apply_edits(
            EditTarget {
                source: &source,
                destination: SaveDestination::ReplaceOriginal,
                container: None,
                container_metadata: None,
            },
            TrackEdits {
                stream_order: &[],
                deleted_streams: &BTreeSet::from([0]),
                default_streams: &BTreeSet::from([0]),
                default_sidecars: &BTreeSet::new(),
                audio_settings: &BTreeMap::new(),
                video_settings: &BTreeMap::new(),
                subtitle_changes: &[],
                left_subtitle_order: &[],
                sidecars: &[],
            },
            &AtomicBool::new(false),
            |_| {},
        );

        // Assert
        match result {
            Err(EditError::Failed(message)) => {
                assert_that!(message.as_str()).contains("also marked for deletion");
            }
            other => panic!("expected EditError::Failed, got {other:?}"),
        }

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apply_edits_should_write_genuine_matroska_when_a_mkv_file_is_misnamed_with_an_mp4_extension()
    {
        // Regression test: renaming a `.mkv` to `.mp4` doesn't change what's actually
        // inside it. Editing such a file without requesting a container conversion
        // must still write real Matroska (matching what's genuinely there), not let
        // `run_ffmpeg` infer "mp4" purely from the (lying) output extension and then
        // have ffmpeg itself reject the SubRip subtitle MP4 can't hold.
        require_tools(
            "apply_edits_should_write_genuine_matroska_when_a_mkv_file_is_misnamed_with_an_mp4_extension",
            &["ffmpeg"],
        );
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-misnamed-mkv-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        // The extension says MP4; `-f matroska` below makes sure the actual bytes
        // written are genuinely Matroska regardless — exactly what a renamed file
        // looks like.
        let source = directory.join("movie.mp4");
        let subtitles = directory.join("movie.eng.srt");
        fs::write(&subtitles, "1\n00:00:00,000 --> 00:00:00,800\nHello\n").unwrap();
        let status = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=16x16:r=1:d=1",
                "-i",
            ])
            .arg(&subtitles)
            .args([
                "-map",
                "0:v:0",
                "-map",
                "1:s:0",
                "-c:v",
                "mpeg4",
                "-c:s",
                "subrip",
                "-metadata:s:s:0",
                "language=eng",
                "-f",
                "matroska",
            ])
            .arg(&source)
            .status()
            .unwrap();
        assert_that!(status.success()).is_true();

        // Sanity check the fixture really is Matroska despite its name.
        let source_info = media_info(&source).unwrap();
        assert_that!(source_info.format["format_name"].as_str().unwrap()).contains("matroska");

        // Act: no container conversion requested — just reorder the tracks, enough to
        // force a real remux (`media_changed`) without touching the subtitle at all.
        let output = apply_edits(
            EditTarget {
                source: &source,
                destination: SaveDestination::ReplaceOriginal,
                container: None,
                container_metadata: None,
            },
            TrackEdits {
                stream_order: &[1, 0],
                deleted_streams: &BTreeSet::new(),
                default_streams: &BTreeSet::new(),
                default_sidecars: &BTreeSet::new(),
                audio_settings: &BTreeMap::new(),
                video_settings: &BTreeMap::new(),
                subtitle_changes: &[],
                left_subtitle_order: &[],
                sidecars: &[],
            },
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();

        // Assert: the output is still genuinely Matroska, subtitle intact, even
        // though its name still says `.mp4`.
        let output_info = media_info(&output.output_path).unwrap();
        assert_that!(output_info.format["format_name"].as_str().unwrap()).contains("matroska");
        assert_that!(output_info.streams.iter().any(|stream| {
            stream.get("codec_name").and_then(|value| value.as_str()) == Some("subrip")
        }))
        .is_true();

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apply_edits_should_place_moov_before_mdat_when_converting_to_mov() {
        // Regression test: `-movflags +faststart` was only applied for the Mp4
        // container target, silently skipping it for Mov even though Mov shares the
        // same ISO-BMFF/QuickTime muxer family and benefits identically. Without it,
        // the `moov` atom lands after `mdat`, requiring a full trailing scan to open
        // or seek the file instead of reading a few bytes from the front.
        require_tools(
            "apply_edits_should_place_moov_before_mdat_when_converting_to_mov",
            &["ffmpeg:aac"],
        );
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-mov-faststart-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("movie.mkv");
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
                "mpeg4",
                "-c:a",
                "aac",
            ])
            .arg(&source)
            .status()
            .unwrap();
        assert_that!(status.success()).is_true();

        // Act
        let output = apply_edits(
            EditTarget {
                source: &source,
                destination: SaveDestination::CreateCopy,
                container: Some(ContainerFormat::Mov),
                container_metadata: None,
            },
            TrackEdits {
                stream_order: &[0, 1],
                deleted_streams: &BTreeSet::new(),
                default_streams: &BTreeSet::from([0, 1]),
                default_sidecars: &BTreeSet::new(),
                audio_settings: &BTreeMap::new(),
                video_settings: &BTreeMap::new(),
                subtitle_changes: &[],
                left_subtitle_order: &[],
                sidecars: &[],
            },
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();

        // Assert
        let bytes = fs::read(&output.output_path).unwrap();
        let find_atom = |needle: &[u8]| bytes.windows(needle.len()).position(|w| w == needle);
        let moov_offset = find_atom(b"moov").expect("output should contain a moov atom");
        let mdat_offset = find_atom(b"mdat").expect("output should contain an mdat atom");
        assert_that!(moov_offset).is_less_than(mdat_offset);

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apply_edits_should_keep_captions_and_hearing_impaired_independent_in_mp4() {
        require_tools(
            "apply_edits_should_keep_captions_and_hearing_impaired_independent_in_mp4",
            &["ffmpeg:mov_text"],
        );
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-independent-accessibility-flags-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("movie.mkv");
        let subtitles = directory.join("movie.eng.srt");
        fs::write(
            &subtitles,
            "1\n00:00:00,000 --> 00:00:00,800\nAccessible dialogue\n",
        )
        .unwrap();
        let status = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=16x16:r=1:d=1",
                "-i",
            ])
            .arg(&subtitles)
            .args([
                "-map",
                "0:v:0",
                "-map",
                "1:s:0",
                "-c:v",
                "mpeg4",
                "-c:s",
                "subrip",
                "-metadata:s:s:0",
                "language=eng",
            ])
            .arg(&source)
            .status()
            .unwrap();
        assert_that!(status.success()).is_true();
        let change = SubtitleChange {
            cue_edits: Default::default(),
            source: SubtitleSource::Embedded(1),
            source_format: SubtitleFormat::SubRip,
            embedded_target: Some(SubtitleFormat::MovText),
            export_target: None,
            import_into_media: false,
            ocr_language: None,
            metadata: Some(SubtitleMetadata {
                language: "eng".to_string(),
                title: None,
                forced: false,
                cc: true,
                hearing_impaired: true,
                original: false,
                commentary: false,
            }),
        };

        let result = apply_edits(
            EditTarget {
                source: &source,
                destination: SaveDestination::CreateCopy,
                container: Some(ContainerFormat::Mp4),
                container_metadata: None,
            },
            TrackEdits {
                stream_order: &[0, 1],
                deleted_streams: &BTreeSet::new(),
                default_streams: &BTreeSet::from([0, 1]),
                default_sidecars: &BTreeSet::new(),
                audio_settings: &BTreeMap::new(),
                video_settings: &BTreeMap::new(),
                subtitle_changes: &[change],
                left_subtitle_order: &[],
                sidecars: &[],
            },
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        let output = media_info(&result.output_path).unwrap();
        let subtitle = output
            .streams
            .iter()
            .find(|stream| stream_kind(stream) == Some("subtitle"))
            .unwrap();

        assert_that!(stream_cc(subtitle)).is_true();
        assert_that!(stream_hearing_impaired(subtitle)).is_true();

        let back_to_mkv = apply_edits(
            EditTarget {
                source: &result.output_path,
                destination: SaveDestination::CreateCopy,
                container: Some(ContainerFormat::Matroska),
                container_metadata: None,
            },
            TrackEdits {
                stream_order: &[0, 1],
                deleted_streams: &BTreeSet::new(),
                default_streams: &BTreeSet::from([0, 1]),
                default_sidecars: &BTreeSet::new(),
                audio_settings: &BTreeMap::new(),
                video_settings: &BTreeMap::new(),
                subtitle_changes: &[SubtitleChange {
                    cue_edits: Default::default(),
                    source: SubtitleSource::Embedded(1),
                    source_format: SubtitleFormat::MovText,
                    embedded_target: Some(SubtitleFormat::SubRip),
                    export_target: None,
                    import_into_media: false,
                    ocr_language: None,
                    metadata: None,
                }],
                left_subtitle_order: &[],
                sidecars: &[],
            },
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        let mkv = media_info(&back_to_mkv.output_path).unwrap();
        let subtitle = mkv
            .streams
            .iter()
            .find(|stream| stream_kind(stream) == Some("subtitle"))
            .unwrap();
        assert_that!(stream_cc(subtitle)).is_false();
        assert_that!(stream_hearing_impaired(subtitle)).is_true();

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apply_edits_should_create_an_edited_copy_without_changing_the_source() {
        // Arrange
        require_tools(
            "apply_edits_should_create_an_edited_copy_without_changing_the_source",
            &["ffmpeg:libx264"],
        );
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
                "color=c=black:s=1280x1024:r=1:d=1",
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
                metadata: VideoMetadata {
                    language: "und".to_string(),
                    title: None,
                    commentary: false,
                },
                rotation: VideoRotation::None,
            },
        )]);

        // Act
        let output = apply_edits(
            EditTarget {
                source: &source,
                destination: SaveDestination::CreateCopy,
                container: None,
                container_metadata: None,
            },
            TrackEdits {
                stream_order: &[0, 1],
                deleted_streams: &BTreeSet::new(),
                default_streams: &BTreeSet::new(),
                default_sidecars: &BTreeSet::new(),
                audio_settings: &BTreeMap::new(),
                video_settings: &settings,
                subtitle_changes: &[],
                left_subtitle_order: &[],
                sidecars: &[],
            },
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        let info = media_info(&output.output_path).unwrap();
        let source_info = media_info(&source).unwrap();

        // Assert
        assert_that!(output.output_path.file_name().unwrap().to_str().unwrap())
            .is_equal_to("transcode-reel-edit.mkv");
        assert_that!(source_info.streams[0]["codec_name"].as_str()).contains("ffv1");
        assert_that!(info.streams[0]["codec_name"].as_str()).contains("h264");
        assert_that!(info.streams[0]["height"].as_u64()).contains(480);
        assert_that!(info.streams[0]["width"].as_u64()).contains(854);
        assert_that!(info.streams[1]["codec_name"].as_str()).contains("pcm_s16le");

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apply_edits_should_honor_each_custom_scaling_mode() {
        // Arrange
        require_tools(
            "apply_edits_should_honor_each_custom_scaling_mode",
            &["ffmpeg:libx264"],
        );
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-custom-scale-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("custom-scale.mkv");
        let status = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=white:s=64x48:r=1:d=1",
                "-c:v",
                "libx264",
            ])
            .arg(&source)
            .status()
            .unwrap();
        assert_that!(status.success()).is_true();

        for (scaling, expected_width, expected_height) in [
            (CustomScaling::FitPad, 32, 18),
            (CustomScaling::Stretch, 32, 18),
        ] {
            let settings = BTreeMap::from([(
                0,
                VideoSettings {
                    codec: VideoCodec::Original,
                    resolution: VideoResolution::Custom(CustomResolution {
                        width: 32,
                        height: 18,
                        scaling,
                    }),
                    metadata: VideoMetadata {
                        language: "und".to_string(),
                        title: None,
                        commentary: false,
                    },
                    rotation: VideoRotation::None,
                },
            )]);

            // Act
            let output = apply_edits(
                EditTarget {
                    source: &source,
                    destination: SaveDestination::CreateCopy,
                    container: None,
                    container_metadata: None,
                },
                TrackEdits {
                    stream_order: &[0],
                    deleted_streams: &BTreeSet::new(),
                    default_streams: &BTreeSet::new(),
                    default_sidecars: &BTreeSet::new(),
                    audio_settings: &BTreeMap::new(),
                    video_settings: &settings,
                    subtitle_changes: &[],
                    left_subtitle_order: &[],
                    sidecars: &[],
                },
                &AtomicBool::new(false),
                |_| {},
            )
            .unwrap();
            let info = media_info(&output.output_path).unwrap();

            // Assert
            assert_that!(info.streams[0]["width"].as_u64()).contains(expected_width);
            assert_that!(info.streams[0]["height"].as_u64()).contains(expected_height);
        }

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apply_edits_should_export_converted_subtitle_without_retaining_an_embedded_copy() {
        // Arrange
        require_tools(
            "apply_edits_should_export_converted_subtitle_without_retaining_an_embedded_copy",
            &["ffmpeg"],
        );

        let directory = std::env::temp_dir().join(format!(
            "reel-tui-subtitle-convert-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("subtitles.mkv");
        let subtitle = directory.join("fixture.srt");
        fs::write(
            &subtitle,
            "1\n00:00:00,000 --> 00:00:00,800\nHello subtitles\n",
        )
        .unwrap();
        let status = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=320x240:d=1",
                "-i",
            ])
            .arg(&subtitle)
            .args([
                "-map",
                "0:v:0",
                "-map",
                "1:0",
                "-c:v",
                "ffv1",
                "-c:s",
                "subrip",
                "-metadata:s:s:0",
                "language=eng",
            ])
            .arg(&source)
            .status()
            .unwrap();
        assert_that!(status.success()).is_true();
        let changes = [SubtitleChange {
            cue_edits: Default::default(),
            source: SubtitleSource::Embedded(1),
            source_format: SubtitleFormat::SubRip,
            embedded_target: Some(SubtitleFormat::Ass),
            export_target: Some(SubtitleFormat::Ass),
            import_into_media: false,
            ocr_language: None,

            metadata: None,
        }];

        // Act
        let edited_copy = apply_edits(
            EditTarget {
                source: &source,
                destination: SaveDestination::CreateCopy,
                container: None,
                container_metadata: None,
            },
            TrackEdits {
                stream_order: &[0, 1],
                deleted_streams: &BTreeSet::new(),
                default_streams: &BTreeSet::new(),
                default_sidecars: &BTreeSet::new(),
                audio_settings: &BTreeMap::new(),
                video_settings: &BTreeMap::new(),
                subtitle_changes: &changes,
                left_subtitle_order: &[],
                sidecars: &[],
            },
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        let output = media_info(&edited_copy.output_path).unwrap();
        let original = media_info(&source).unwrap();
        let sidecar = directory.join("subtitles-reel-edit.eng.ass");

        // Assert
        assert_that!(edited_copy.media_changed).is_true();
        assert_that!(original.streams[1]["codec_name"].as_str()).contains("subrip");
        assert_that!(output.streams.len()).is_equal_to(1);
        assert_that!(stream_kind(&output.streams[0])).contains("video");
        assert_that!(sidecar.exists()).is_true();
        assert_that!(fs::read_to_string(sidecar).unwrap()).contains("Hello subtitles");

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apply_edits_should_export_vobsub_with_flags_and_remove_the_embedded_subtitle() {
        require_tools(
            "apply_edits_should_export_vobsub_with_flags_and_remove_the_embedded_subtitle",
            &["ffmpeg", "seconv"],
        );

        let directory = std::env::temp_dir().join(format!(
            "reel-tui-vobsub-export-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("subtitles.mkv");
        let subtitle = directory.join("fixture.srt");
        fs::write(
            &subtitle,
            "1\n00:00:00,000 --> 00:00:00,800\nDanske undertekster\n",
        )
        .unwrap();
        let status = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=320x240:d=1",
                "-i",
            ])
            .arg(&subtitle)
            .args([
                "-map",
                "0:v:0",
                "-map",
                "1:0",
                "-c:v",
                "ffv1",
                "-c:s",
                "subrip",
                "-metadata:s:s:0",
                "language=dan",
            ])
            .arg(&source)
            .status()
            .unwrap();
        assert_that!(status.success()).is_true();
        let changes = [SubtitleChange {
            cue_edits: Default::default(),
            source: SubtitleSource::Embedded(1),
            source_format: SubtitleFormat::SubRip,
            embedded_target: Some(SubtitleFormat::VobSub),
            export_target: Some(SubtitleFormat::VobSub),
            import_into_media: false,
            ocr_language: None,
            metadata: Some(SubtitleMetadata {
                language: "dan".to_string(),
                title: None,
                forced: true,
                cc: true,
                hearing_impaired: true,
                original: false,
                commentary: false,
            }),
        }];

        let edited_copy = apply_edits(
            EditTarget {
                source: &source,
                destination: SaveDestination::CreateCopy,
                container: None,
                container_metadata: None,
            },
            TrackEdits {
                stream_order: &[0, 1],
                deleted_streams: &BTreeSet::new(),
                default_streams: &BTreeSet::new(),
                default_sidecars: &BTreeSet::new(),
                audio_settings: &BTreeMap::new(),
                video_settings: &BTreeMap::new(),
                subtitle_changes: &changes,
                left_subtitle_order: &[],
                sidecars: &[],
            },
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        let output = media_info(&edited_copy.output_path).unwrap();
        let original = media_info(&source).unwrap();
        let exported_sub = directory.join("subtitles-reel-edit.dan.forced.sdh.sub");
        let exported_idx = directory.join("subtitles-reel-edit.dan.forced.sdh.idx");

        assert_that!(edited_copy.media_changed).is_true();
        assert_that!(original.streams.len()).is_equal_to(2);
        assert_that!(stream_language(&original.streams[1])).is_equal_to("dan".to_string());
        assert_that!(output.streams.len()).is_equal_to(1);
        assert_that!(stream_kind(&output.streams[0])).contains("video");
        assert_that!(exported_sub.exists()).is_true();
        assert_that!(exported_idx.exists()).is_true();

        fs::remove_dir_all(directory).unwrap();
    }

    /// Cancelling is only meaningful if the file the user was editing is still exactly
    /// as it was. This cancels *after* a real remux has already written a complete
    /// output — the worst moment, because everything needed to publish is sitting on
    /// disk — and requires that nothing is published, nothing is left behind, and the
    /// original is byte-identical.
    ///
    /// A contract lock rather than a single-guard regression test, verified as such:
    /// three independent checks stand between the finished remux and publication (after
    /// `ValidateOutput`, before `PreparePublication`, and at the top of
    /// `publish_transaction_with_progress`), and removing any *two* of them still leaves
    /// this passing. It fails only when all three are gone — which is exactly the
    /// property worth pinning, since any one of them is what keeps a cancelled edit from
    /// overwriting the user's file.
    #[test]
    fn cancelling_after_the_remux_finishes_should_still_leave_the_original_untouched() {
        // Arrange
        require_tools(
            "cancelling_after_the_remux_finishes_should_still_leave_the_original_untouched",
            &["ffmpeg", "ffmpeg:libx264", "ffmpeg:aac"],
        );
        let directory = scratch_directory("cancel-after-remux");
        let _cleanup = DirectoryCleanup(Some(directory.clone()));
        let source = directory.join("movie.mkv");
        let status = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("color=c=black:s=320x240:d=1")
            .args([
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=48000:cl=stereo",
                "-map",
                "0:v",
                "-map",
                "1:a",
                "-c:v",
                "libx264",
                "-c:a",
                "aac",
                "-t",
                "1",
            ])
            .arg(&source)
            .status()
            .unwrap();
        assert_that!(status.success()).is_true();
        let before = fs::read(&source).unwrap();
        let cancelled = AtomicBool::new(false);
        let mut phases = Vec::new();

        // Act: pull the plug the instant the remux is done and its output is about to
        // be checked — the last moment at which cancelling still has to be honoured.
        let result = apply_edits(
            EditTarget {
                source: &source,
                destination: SaveDestination::ReplaceOriginal,
                container: None,
                container_metadata: None,
            },
            TrackEdits {
                stream_order: &[1, 0],
                deleted_streams: &BTreeSet::new(),
                default_streams: &BTreeSet::new(),
                default_sidecars: &BTreeSet::new(),
                audio_settings: &BTreeMap::new(),
                video_settings: &BTreeMap::new(),
                subtitle_changes: &[],
                left_subtitle_order: &[],
                sidecars: &[],
            },
            &cancelled,
            |progress| {
                let label = progress.label();
                if label == "Checking output" {
                    cancelled.store(true, Ordering::Relaxed);
                }
                phases.push(label);
            },
        );

        // Assert
        assert_that!(matches!(result, Err(EditError::Cancelled))).is_true();
        assert_that!(fs::read(&source).unwrap()).is_equal_to(before);
        assert!(
            phases.iter().any(|label| label == "Cleaning up"),
            "a cancelled edit must report its cleanup: {phases:?}",
        );
        // Nothing half-written survives beside the original.
        let leftovers = fs::read_dir(&directory)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "movie.mkv")
            .collect::<Vec<_>>();
        assert!(
            leftovers.is_empty(),
            "a cancelled edit must leave no work files: {leftovers:?}",
        );
    }

    /// The reverse of the VobSub export above, and the only path that runs Tesseract:
    /// an embedded *image* subtitle exported as text has to be extracted, OCR'd, and
    /// validated as the requested text format before anything is published. The image
    /// track is unreadable to `ffmpeg`'s text encoders, so a regression here does not
    /// produce a wrong `.srt` — it produces no usable subtitle at all.
    #[test]
    fn apply_edits_should_ocr_an_embedded_image_subtitle_into_a_text_sidecar() {
        // Arrange
        require_tools(
            "apply_edits_should_ocr_an_embedded_image_subtitle_into_a_text_sidecar",
            &["ffmpeg", "seconv", "tesseract"],
        );
        let directory = scratch_directory("vobsub-ocr");
        let _cleanup = DirectoryCleanup(Some(directory.clone()));

        // `ffmpeg` refuses text→bitmap ("only possible from text to text or bitmap to
        // bitmap"), so the image subtitle is rendered by `seconv` — the same tool reel
        // uses for that direction — and muxed in as a real `dvd_subtitle` track.
        let text = directory.join("fixture.srt");
        fs::write(&text, "1\n00:00:00,200 --> 00:00:01,800\nHELLO WORLD\n").unwrap();
        let rendered = Command::new("seconv")
            .arg(&text)
            .arg("vobsub")
            .arg("--overwrite")
            .current_dir(&directory)
            .output()
            .unwrap();
        assert!(
            rendered.status.success() && directory.join("fixture.idx").exists(),
            "seconv must render the fixture to VobSub: {}",
            String::from_utf8_lossy(&rendered.stderr),
        );
        let source = directory.join("movie.mkv");
        let status = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("color=c=black:s=320x240:d=2")
            .args(["-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo", "-i"])
            .arg(directory.join("fixture.idx"))
            .args([
                "-map",
                "0:v",
                "-map",
                "1:a",
                "-map",
                "2:s",
                "-c:v",
                "libx264",
                "-c:a",
                "aac",
                "-c:s",
                "copy",
                "-metadata:s:s:0",
                "language=eng",
                "-t",
                "2",
            ])
            .arg(&source)
            .status()
            .unwrap();
        assert_that!(status.success()).is_true();
        let original = media_info(&source).unwrap();
        assert_that!(original.streams[2]["codec_name"].as_str()).contains("dvd_subtitle");

        let changes = [SubtitleChange {
            cue_edits: Default::default(),
            source: SubtitleSource::Embedded(2),
            source_format: SubtitleFormat::VobSub,
            embedded_target: None,
            export_target: Some(SubtitleFormat::SubRip),
            import_into_media: false,
            ocr_language: Some("eng".to_string()),
            metadata: None,
        }];
        let mut phases = Vec::new();

        // Act
        let result = apply_edits(
            EditTarget {
                source: &source,
                destination: SaveDestination::ReplaceOriginal,
                container: None,
                container_metadata: None,
            },
            TrackEdits {
                stream_order: &[0, 1, 2],
                deleted_streams: &BTreeSet::new(),
                default_streams: &BTreeSet::new(),
                default_sidecars: &BTreeSet::new(),
                audio_settings: &BTreeMap::new(),
                video_settings: &BTreeMap::new(),
                subtitle_changes: &changes,
                left_subtitle_order: &[],
                sidecars: &[],
            },
            &AtomicBool::new(false),
            |progress| phases.push(progress.label()),
        )
        .unwrap();

        // Assert: the image track left the media and a real SubRip sidecar took its
        // place. The OCR'd words themselves are Tesseract's business, not reel's, so
        // this checks the file is genuinely SubRip rather than matching exact text.
        let exported = directory.join("movie.eng.srt");
        assert_that!(exported.exists()).is_true();
        assert_that!(fs::metadata(&exported).unwrap().len() > 0).is_true();
        assert!(
            validate_subtitle_output(&exported, SubtitleFormat::SubRip).is_ok(),
            "the export must validate as SubRip, got {:?}",
            fs::read_to_string(&exported),
        );
        let output = media_info(&source).unwrap();
        assert_that!(output.streams.len()).is_equal_to(2);
        assert!(
            !output
                .streams
                .iter()
                .any(|stream| stream_kind(stream) == Some("subtitle")),
            "the OCR'd track must be gone from the media",
        );
        assert_that!(result.media_changed).is_true();

        // Assert: the Save dialog said what it was doing before the slowest step in
        // the whole pipeline, per the edit progress contract.
        assert!(
            phases
                .iter()
                .any(|label| label.starts_with("Running OCR on") && label.contains("(eng)")),
            "an OCR run must announce itself and name its language: {phases:?}",
        );
    }

    #[test]
    fn apply_edits_should_export_and_remove_incompatible_subtitle_during_mp4_conversion() {
        // Arrange
        require_tools(
            "apply_edits_should_export_and_remove_incompatible_subtitle_during_mp4_conversion",
            &["ffmpeg"],
        );

        let directory = std::env::temp_dir().join(format!(
            "reel-tui-subtitle-export-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("original.mkv");
        let subtitle = directory.join("fixture.srt");
        fs::write(
            &subtitle,
            "1\n00:00:00,000 --> 00:00:00,800\nOriginal codec\n",
        )
        .unwrap();
        let status = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=320x240:d=1",
                "-i",
            ])
            .arg(&subtitle)
            .args([
                "-map",
                "0:v:0",
                "-map",
                "1:0",
                "-c:v",
                "mpeg4",
                "-c:s",
                "subrip",
                "-metadata:s:s:0",
                "language=eng",
            ])
            .arg(&source)
            .status()
            .unwrap();
        assert_that!(status.success()).is_true();
        let changes = [SubtitleChange {
            cue_edits: Default::default(),
            source: SubtitleSource::Embedded(1),
            source_format: SubtitleFormat::SubRip,
            embedded_target: None,
            export_target: Some(SubtitleFormat::SubRip),
            import_into_media: false,
            ocr_language: None,

            metadata: None,
        }];

        // Act
        let result = apply_edits(
            EditTarget {
                source: &source,
                destination: SaveDestination::CreateCopy,
                container: Some(ContainerFormat::Mp4),
                container_metadata: None,
            },
            TrackEdits {
                stream_order: &[0, 1],
                deleted_streams: &BTreeSet::new(),
                default_streams: &BTreeSet::from([0]),
                default_sidecars: &BTreeSet::new(),
                audio_settings: &BTreeMap::new(),
                video_settings: &BTreeMap::new(),
                subtitle_changes: &changes,
                left_subtitle_order: &[],
                sidecars: &[],
            },
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        let media = media_info(&result.output_path).unwrap();
        let exported = directory.join("original-reel-edit.eng.srt");

        // Assert
        assert_that!(source.exists()).is_true();
        assert_that!(result.media_changed).is_true();
        assert_that!(result.output_path.file_name().unwrap().to_str().unwrap())
            .is_equal_to("original-reel-edit.mp4");
        assert_that!(media.streams.len()).is_equal_to(1);
        assert_that!(media.streams[0]["codec_name"].as_str()).contains("mpeg4");
        assert_that!(exported.exists()).is_true();
        assert_that!(fs::read_to_string(exported).unwrap()).contains("Original codec");

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apply_edits_should_default_the_one_surviving_converted_subtitle_after_bulk_deletion() {
        // Regression test reproducing a real report: deleting most of a file's
        // subtitle tracks, converting the one survivor for an MP4 target, and marking
        // it default failed with "the wrong default flag" even though the request
        // was entirely valid.
        require_tools(
            "apply_edits_should_default_the_one_surviving_converted_subtitle_after_bulk_deletion",
            &["ffmpeg:mov_text"],
        );
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-bulk-delete-default-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("movie.mkv");
        let subtitle = directory.join("fixture.srt");
        fs::write(&subtitle, "1\n00:00:00,000 --> 00:00:00,800\nSurvivor\n").unwrap();
        let status = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=16x16:r=1:d=1",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=8000:cl=mono:d=1",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=8000:cl=mono:d=1",
                "-i",
            ])
            .arg(&subtitle)
            .arg("-i")
            .arg(&subtitle)
            .arg("-i")
            .arg(&subtitle)
            .args([
                "-map",
                "0:v:0",
                "-map",
                "1:a:0",
                "-map",
                "2:a:0",
                "-map",
                "3:s:0",
                "-map",
                "4:s:0",
                "-map",
                "5:s:0",
                "-c:v",
                "mpeg4",
                "-c:a",
                "aac",
                "-c:s",
                "subrip",
                "-disposition:a:0",
                "default",
                "-disposition:a:1",
                "0",
                "-metadata:s:s:0",
                "language=eng",
                "-metadata:s:s:1",
                "language=nld",
                "-metadata:s:s:2",
                "language=fra",
            ])
            .arg(&source)
            .status()
            .unwrap();
        assert_that!(status.success()).is_true();

        // Delete tracks 3 and 4 (eng, nld), keep track 5 (fra) — converted to
        // MovText, since that's what MP4 requires — and mark it default. Two audio
        // tracks (only one default) between the video and subtitles, matching the
        // real report's group structure (video, 2 audio, then subtitles) — "position
        // 3" in the output is the surviving subtitle.
        let change = SubtitleChange {
            cue_edits: Default::default(),
            source: SubtitleSource::Embedded(5),
            source_format: SubtitleFormat::SubRip,
            embedded_target: Some(SubtitleFormat::MovText),
            export_target: None,
            import_into_media: false,
            ocr_language: None,
            metadata: None,
        };

        // Act
        let output = apply_edits(
            EditTarget {
                source: &source,
                destination: SaveDestination::ReplaceOriginal,
                container: Some(ContainerFormat::Mp4),
                container_metadata: None,
            },
            TrackEdits {
                stream_order: &[0, 1, 2, 5],
                deleted_streams: &BTreeSet::from([3, 4]),
                default_streams: &BTreeSet::from([0, 1, 5]),
                default_sidecars: &BTreeSet::new(),
                audio_settings: &BTreeMap::new(),
                video_settings: &BTreeMap::new(),
                subtitle_changes: &[change],
                left_subtitle_order: &[],
                sidecars: &[],
            },
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();

        // Assert: the surviving subtitle track really is marked default in the
        // output.
        let output_info = media_info(&output.output_path).unwrap();
        let subtitle_stream = output_info
            .streams
            .iter()
            .find(|stream| stream_kind(stream) == Some("subtitle"))
            .unwrap();
        assert_that!(is_default(subtitle_stream)).is_true();

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apply_edits_should_accept_the_default_flag_mp4_forces_onto_a_lone_subtitle() {
        // Regression test for a real report: keeping a single *non-default* subtitle,
        // converted to MOV Text for an MP4 target, failed with "the wrong default
        // flag". MP4 stores "default" as the `tkhd` enabled flag and `movenc` enables
        // the first track of a media type when none is flagged, so the muxer wrote the
        // only file it can write and validation rejected it. The track's title also has
        // to survive, since ffprobe reports an MP4 track name under `name`, not
        // `title`.
        require_tools(
            "apply_edits_should_accept_the_default_flag_mp4_forces_onto_a_lone_subtitle",
            &["ffmpeg:mov_text"],
        );
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-mp4-forced-default-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("movie.mkv");
        let subtitle = directory.join("fixture.srt");
        fs::write(&subtitle, "1\n00:00:00,000 --> 00:00:00,800\nSurvivor\n").unwrap();
        let status = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=16x16:r=1:d=1",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=8000:cl=mono:d=1",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=8000:cl=mono:d=1",
                "-i",
            ])
            .arg(&subtitle)
            .arg("-i")
            .arg(&subtitle)
            .args([
                "-map",
                "0:v:0",
                "-map",
                "1:a:0",
                "-map",
                "2:a:0",
                "-map",
                "3:s:0",
                "-map",
                "4:s:0",
                "-c:v",
                "mpeg4",
                "-c:a",
                "aac",
                "-c:s",
                "subrip",
                "-disposition:a:0",
                "default",
                "-disposition:a:1",
                "0",
                // The survivor carries a title and no default flag — exactly the shape
                // the report's source file had.
                "-disposition:s:0",
                "0",
                "-disposition:s:1",
                "0",
                "-metadata:s:s:0",
                "language=eng",
                "-metadata:s:s:1",
                "language=kor",
                "-metadata:s:s:1",
                "title=Forced",
            ])
            .arg(&source)
            .status()
            .unwrap();
        assert_that!(status.success()).is_true();

        // Delete subtitle 3, keep 4 converted to MOV Text, and leave it non-default.
        let change = SubtitleChange {
            cue_edits: Default::default(),
            source: SubtitleSource::Embedded(4),
            source_format: SubtitleFormat::SubRip,
            embedded_target: Some(SubtitleFormat::MovText),
            export_target: None,
            import_into_media: false,
            ocr_language: None,
            metadata: None,
        };

        // Act
        let output = apply_edits(
            EditTarget {
                source: &source,
                destination: SaveDestination::ReplaceOriginal,
                container: Some(ContainerFormat::Mp4),
                container_metadata: None,
            },
            TrackEdits {
                stream_order: &[0, 1, 2, 4],
                deleted_streams: &BTreeSet::from([3]),
                default_streams: &BTreeSet::from([0, 1]),
                default_sidecars: &BTreeSet::new(),
                audio_settings: &BTreeMap::new(),
                video_settings: &BTreeMap::new(),
                subtitle_changes: &[change],
                left_subtitle_order: &[],
                sidecars: &[],
            },
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();

        // Assert: the edit went through, and the title the muxer stored under `name`
        // is still readable as the track's title.
        let output_info = media_info(&output.output_path).unwrap();
        let subtitle_stream = output_info
            .streams
            .iter()
            .find(|stream| stream_kind(stream) == Some("subtitle"))
            .unwrap();
        assert_that!(stream_title(subtitle_stream)).is_equal_to(Some("Forced".to_string()));

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apply_edits_should_import_sidecars_after_embedded_subtitles_and_remove_the_sources() {
        // Arrange
        require_tools(
            "apply_edits_should_import_sidecars_after_embedded_subtitles_and_remove_the_sources",
            &["ffmpeg"],
        );
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-sidecar-import-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("movie.mkv");
        let embedded_path = directory.join("embedded.srt");
        let english_path = directory.join("movie.eng.forced.sdh.srt");
        let dutch_path = directory.join("movie.nld.srt");
        fs::write(
            &embedded_path,
            "1\n00:00:00,000 --> 00:00:00,800\nEmbedded subtitle\n",
        )
        .unwrap();
        fs::write(
            &english_path,
            "1\n00:00:00,000 --> 00:00:00,800\nEnglish external\n",
        )
        .unwrap();
        fs::write(
            &dutch_path,
            "1\n00:00:00,000 --> 00:00:00,800\nDutch external\n",
        )
        .unwrap();
        let status = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=320x240:d=1",
                "-i",
            ])
            .arg(&embedded_path)
            .args([
                "-map",
                "0:v:0",
                "-map",
                "1:0",
                "-c:v",
                "ffv1",
                "-c:s",
                "subrip",
                "-metadata:s:s:0",
                "language=fra",
                "-disposition:v:0",
                "default",
                "-disposition:s:0",
                "default",
            ])
            .arg(&source)
            .status()
            .unwrap();
        assert_that!(status.success()).is_true();
        let sidecars = [
            SidecarEntry {
                path: english_path.clone(),
                companion: None,
                display_name: "movie.eng.forced.sdh.srt".to_string(),
                format: SubtitleFormat::SubRip,
                language: "eng".to_string(),
                forced: true,
                hearing_impaired: true,
                number: None,
                fingerprint: FileFingerprint::for_path(&english_path).unwrap(),
                companion_fingerprint: None,
            },
            SidecarEntry {
                path: dutch_path.clone(),
                companion: None,
                display_name: "movie.nld.srt".to_string(),
                format: SubtitleFormat::SubRip,
                language: "nld".to_string(),
                forced: false,
                hearing_impaired: false,
                number: None,
                fingerprint: FileFingerprint::for_path(&dutch_path).unwrap(),
                companion_fingerprint: None,
            },
        ];
        let changes = sidecars
            .iter()
            .map(|sidecar| SubtitleChange {
                cue_edits: Default::default(),
                source: SubtitleSource::Sidecar(sidecar.path.clone()),
                source_format: SubtitleFormat::SubRip,
                embedded_target: None,
                export_target: None,
                import_into_media: true,
                ocr_language: None,

                metadata: None,
            })
            .collect::<Vec<_>>();

        // Act
        let result = apply_edits(
            EditTarget {
                source: &source,
                destination: SaveDestination::CreateCopy,
                container: None,
                container_metadata: None,
            },
            TrackEdits {
                stream_order: &[0, 1],
                deleted_streams: &BTreeSet::new(),
                default_streams: &BTreeSet::from([0, 1]),
                default_sidecars: &BTreeSet::new(),
                audio_settings: &BTreeMap::new(),
                video_settings: &BTreeMap::new(),
                subtitle_changes: &changes,
                left_subtitle_order: &[],
                sidecars: &sidecars,
            },
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        let output = media_info(&result.output_path).unwrap();

        // Assert
        assert_that!(source.exists()).is_true();
        assert_that!(english_path.exists()).is_false();
        assert_that!(dutch_path.exists()).is_false();
        assert_that!(result.media_changed).is_true();
        assert_that!(result.output_path.file_name().unwrap().to_str().unwrap())
            .is_equal_to("movie-reel-edit.mkv");
        assert_eq!(
            output
                .streams
                .iter()
                .filter_map(stream_kind)
                .collect::<Vec<_>>(),
            vec!["video", "subtitle", "subtitle", "subtitle"]
        );
        assert_that!(stream_language(&output.streams[1])).is_equal_to("fra".to_string());
        assert_that!(stream_language(&output.streams[2])).is_equal_to("eng".to_string());
        assert_that!(stream_language(&output.streams[3])).is_equal_to("nld".to_string());
        assert_that!(is_default(&output.streams[2])).is_false();
        assert_that!(is_default(&output.streams[3])).is_false();
        assert_that!(stream_forced(&output.streams[2])).is_true();
        assert_that!(stream_cc(&output.streams[2])).is_false();
        assert_that!(stream_hearing_impaired(&output.streams[2])).is_true();

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn failed_or_cancelled_sidecar_import_should_leave_media_and_sidecar_untouched() {
        // Arrange
        require_tools(
            "failed_or_cancelled_sidecar_import_should_leave_media_and_sidecar_untouched",
            &["ffmpeg"],
        );
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-cancelled-sidecar-import-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("movie.mkv");
        let sidecar_path = directory.join("movie.eng.srt");
        let status = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=16x16:d=1",
                "-c:v",
                "mpeg4",
            ])
            .arg(&source)
            .status()
            .unwrap();
        assert_that!(status.success()).is_true();
        fs::write(
            &sidecar_path,
            "1\n00:00:00,000 --> 00:00:00,800\nExternal subtitle\n",
        )
        .unwrap();
        let source_before = fs::read(&source).unwrap();
        let sidecar_before = fs::read(&sidecar_path).unwrap();
        let sidecar = SidecarEntry {
            path: sidecar_path.clone(),
            companion: None,
            display_name: "movie.eng.srt".to_string(),
            format: SubtitleFormat::SubRip,
            language: "eng".to_string(),
            forced: false,
            hearing_impaired: false,
            number: None,
            fingerprint: FileFingerprint::for_path(&sidecar_path).unwrap(),
            companion_fingerprint: None,
        };
        let changes = [SubtitleChange {
            cue_edits: Default::default(),
            source: SubtitleSource::Sidecar(sidecar_path.clone()),
            source_format: SubtitleFormat::SubRip,
            embedded_target: None,
            export_target: None,
            import_into_media: true,
            ocr_language: None,

            metadata: None,
        }];

        // Act
        let incompatible = apply_edits(
            EditTarget {
                source: &source,
                destination: SaveDestination::ReplaceOriginal,
                container: Some(ContainerFormat::Mp4),
                container_metadata: None,
            },
            TrackEdits {
                stream_order: &[0],
                deleted_streams: &BTreeSet::new(),
                default_streams: &BTreeSet::from([0]),
                default_sidecars: &BTreeSet::new(),
                audio_settings: &BTreeMap::new(),
                video_settings: &BTreeMap::new(),
                subtitle_changes: &changes,
                left_subtitle_order: &[],
                sidecars: std::slice::from_ref(&sidecar),
            },
            &AtomicBool::new(false),
            |_| {},
        );
        let cancelled = apply_edits(
            EditTarget {
                source: &source,
                destination: SaveDestination::ReplaceOriginal,
                container: None,
                container_metadata: None,
            },
            TrackEdits {
                stream_order: &[0],
                deleted_streams: &BTreeSet::new(),
                default_streams: &BTreeSet::from([0]),
                default_sidecars: &BTreeSet::new(),
                audio_settings: &BTreeMap::new(),
                video_settings: &BTreeMap::new(),
                subtitle_changes: &changes,
                left_subtitle_order: &[],
                sidecars: &[sidecar],
            },
            &AtomicBool::new(true),
            |_| {},
        );

        // Assert
        assert_that!(matches!(
            incompatible,
            Err(EditError::Failed(ref error)) if error.contains(
                "MP4 can't import SubRip / SRT subtitle movie.eng.srt"
            )
        ))
        .is_true();
        assert_that!(matches!(cancelled, Err(EditError::Cancelled))).is_true();
        assert_that!(fs::read(&source).unwrap()).is_equal_to(source_before);
        assert_that!(fs::read(&sidecar_path).unwrap()).is_equal_to(sidecar_before);

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apply_edits_should_drop_source_number_when_converted_target_has_no_duplicate() {
        // Arrange
        require_tools(
            "apply_edits_should_drop_source_number_when_converted_target_has_no_duplicate",
            &["ffmpeg"],
        );

        let directory = std::env::temp_dir().join(format!(
            "reel-tui-sidecar-convert-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("movie.mkv");
        let sidecar_path = directory.join("movie.eng.2.srt");
        let status = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=320x240:d=1",
                "-c:v",
                "ffv1",
            ])
            .arg(&source)
            .status()
            .unwrap();
        assert_that!(status.success()).is_true();
        fs::write(
            &sidecar_path,
            "1\n00:00:00,000 --> 00:00:00,800\nExternal subtitle\n",
        )
        .unwrap();
        let sidecar = SidecarEntry {
            path: sidecar_path.clone(),
            companion: None,
            display_name: "movie.eng.2.srt".to_string(),
            format: SubtitleFormat::SubRip,
            language: "eng".to_string(),
            forced: false,
            hearing_impaired: false,
            number: Some(2),
            fingerprint: FileFingerprint::for_path(&sidecar_path).unwrap(),
            companion_fingerprint: None,
        };
        let changes = [SubtitleChange {
            cue_edits: Default::default(),
            source: SubtitleSource::Sidecar(sidecar_path.clone()),
            source_format: SubtitleFormat::SubRip,
            embedded_target: Some(SubtitleFormat::Ass),
            export_target: None,
            import_into_media: false,
            ocr_language: None,

            metadata: None,
        }];
        let source_before = fs::read(&source).unwrap();

        // Act
        let result = apply_edits(
            EditTarget {
                source: &source,
                destination: SaveDestination::ReplaceOriginal,
                container: None,
                container_metadata: None,
            },
            TrackEdits {
                stream_order: &[0],
                deleted_streams: &BTreeSet::new(),
                default_streams: &BTreeSet::new(),
                default_sidecars: &BTreeSet::new(),
                audio_settings: &BTreeMap::new(),
                video_settings: &BTreeMap::new(),
                subtitle_changes: &changes,
                left_subtitle_order: &[],
                sidecars: &[sidecar],
            },
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        let converted = directory.join("movie.eng.ass");

        // Assert
        assert_that!(result.media_changed).is_false();
        assert_that!(fs::read(&source).unwrap()).is_equal_to(source_before);
        assert_that!(sidecar_path.exists()).is_false();
        assert_that!(converted.exists()).is_true();
        assert_that!(fs::read_to_string(converted).unwrap()).contains("External subtitle");

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apply_edits_should_number_converted_sidecar_when_matching_target_already_exists() {
        // Arrange
        require_tools(
            "apply_edits_should_number_converted_sidecar_when_matching_target_already_exists",
            &["ffmpeg"],
        );

        let directory = std::env::temp_dir().join(format!(
            "reel-tui-sidecar-collision-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("movie.mkv");
        let existing_path = directory.join("movie.eng.srt");
        let ass_path = directory.join("movie.eng.ass");
        let numbered_path = directory.join("movie.eng.2.srt");
        let status = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=320x240:d=1",
                "-c:v",
                "ffv1",
            ])
            .arg(&source)
            .status()
            .unwrap();
        assert_that!(status.success()).is_true();
        fs::write(
            &existing_path,
            "1\n00:00:00,000 --> 00:00:00,800\nExisting subtitle\n",
        )
        .unwrap();
        fs::write(
            &ass_path,
            "[Script Info]\nScriptType: v4.00+\n\n[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\nStyle: Default,Arial,20,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,0,2,10,10,10,1\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:00.00,0:00:00.80,Default,,0,0,0,,Converted subtitle\n",
        )
        .unwrap();
        let existing_sidecar = SidecarEntry {
            path: existing_path.clone(),
            companion: None,
            display_name: "movie.eng.srt".to_string(),
            format: SubtitleFormat::SubRip,
            language: "eng".to_string(),
            forced: false,
            hearing_impaired: false,
            number: None,
            fingerprint: FileFingerprint::for_path(&existing_path).unwrap(),
            companion_fingerprint: None,
        };
        let ass_sidecar = SidecarEntry {
            path: ass_path.clone(),
            companion: None,
            display_name: "movie.eng.ass".to_string(),
            format: SubtitleFormat::Ass,
            language: "eng".to_string(),
            forced: false,
            hearing_impaired: false,
            number: None,
            fingerprint: FileFingerprint::for_path(&ass_path).unwrap(),
            companion_fingerprint: None,
        };
        let changes = [SubtitleChange {
            cue_edits: Default::default(),
            source: SubtitleSource::Sidecar(ass_path.clone()),
            source_format: SubtitleFormat::Ass,
            embedded_target: Some(SubtitleFormat::SubRip),
            export_target: None,
            import_into_media: false,
            ocr_language: None,

            metadata: None,
        }];
        let sidecars = [existing_sidecar, ass_sidecar];

        // Act
        let result = apply_edits(
            EditTarget {
                source: &source,
                destination: SaveDestination::ReplaceOriginal,
                container: None,
                container_metadata: None,
            },
            TrackEdits {
                stream_order: &[0],
                deleted_streams: &BTreeSet::new(),
                default_streams: &BTreeSet::new(),
                default_sidecars: &BTreeSet::new(),
                audio_settings: &BTreeMap::new(),
                video_settings: &BTreeMap::new(),
                subtitle_changes: &changes,
                left_subtitle_order: &[],
                sidecars: &sidecars,
            },
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();

        // Assert
        assert_that!(result.media_changed).is_false();
        assert_that!(ass_path.exists()).is_false();
        assert_that!(fs::read_to_string(&existing_path).unwrap()).contains("Existing subtitle");
        assert_that!(fs::read_to_string(&numbered_path).unwrap()).contains("Converted subtitle");

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn publish_copy_should_increment_the_suffix_instead_of_overwriting_a_copy() {
        // Arrange
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-copy-name-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("video.mkv");
        let temporary = directory.join(".temporary-video.mkv");
        let existing = directory.join("video-reel-edit.mkv");
        fs::write(&source, b"source").unwrap();
        fs::write(&temporary, b"new copy").unwrap();
        fs::write(&existing, b"existing copy").unwrap();

        // Act
        let output = next_copy_path(&source, None).unwrap();
        publish_transaction(
            Some((&temporary, &output)),
            None,
            &[],
            &AtomicBool::new(false),
        )
        .unwrap();

        // Assert
        assert_that!(output.file_name().unwrap().to_str().unwrap())
            .is_equal_to("video-reel-edit-2.mkv");
        assert_that!(fs::read(&existing).unwrap()).is_equal_to(b"existing copy".to_vec());
        assert_that!(fs::read(&output).unwrap()).is_equal_to(b"new copy".to_vec());

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn copy_path_should_use_the_target_container_extension() {
        // Arrange
        let source = Path::new("/videos/movie.mkv");

        // Act
        let first = copy_path(source, 1, Some(ContainerFormat::Mp4)).unwrap();
        let second = copy_path(source, 2, Some(ContainerFormat::WebM)).unwrap();

        // Assert
        assert_that!(first).is_equal_to(PathBuf::from("/videos/movie-reel-edit.mp4"));
        assert_that!(second).is_equal_to(PathBuf::from("/videos/movie-reel-edit-2.webm"));
    }

    #[test]
    fn cross_extension_replacement_should_publish_target_and_remove_source_transactionally() {
        // Arrange
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-cross-extension-replace-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("movie.mkv");
        let staged = directory.join(".reel-tui-movie.mp4");
        let target = directory.join("movie.mp4");
        fs::write(&source, b"old media").unwrap();
        fs::write(&staged, b"new media").unwrap();

        // Act
        publish_transaction(
            Some((&staged, &target)),
            Some(&source),
            &[],
            &AtomicBool::new(false),
        )
        .unwrap();

        // Assert
        assert_that!(source.exists()).is_false();
        assert_that!(staged.exists()).is_false();
        assert_that!(fs::read(&target).unwrap()).is_equal_to(b"new media".to_vec());

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apply_edits_should_write_container_metadata_and_leave_the_tracks_alone() {
        // Arrange: a title, comment, date, genre and artist staged on the container. Each
        // is written by its own ffmpeg argument, and a metadata-only save must not disturb
        // the streams it is not about.
        require_tools(
            "apply_edits_should_write_container_metadata_and_leave_the_tracks_alone",
            &["ffmpeg"],
        );
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-container-metadata-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("movie.mkv");
        let status = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=16x16:r=1:d=1",
                "-c:v",
                "ffv1",
                "-metadata",
                "title=Old title",
            ])
            .arg(&source)
            .status()
            .unwrap();
        assert_that!(status.success()).is_true();
        let info = media_info(&source).unwrap();
        let stream_order = info
            .streams
            .iter()
            .filter_map(stream_index)
            .collect::<Vec<_>>();
        let metadata = ContainerMetadata {
            title: Some("Big Buck Bunny".to_string()),
            comment: Some("Open movie".to_string()),
            date: Some("2008".to_string()),
            genre: Some("Animation".to_string()),
            artist: Some("Blender Foundation".to_string()),
        };

        // Act
        apply_edits(
            EditTarget {
                source: &source,
                destination: SaveDestination::ReplaceOriginal,
                container: None,
                container_metadata: Some(&metadata),
            },
            TrackEdits {
                stream_order: &stream_order,
                deleted_streams: &BTreeSet::new(),
                default_streams: &BTreeSet::new(),
                default_sidecars: &BTreeSet::new(),
                audio_settings: &BTreeMap::new(),
                video_settings: &BTreeMap::new(),
                subtitle_changes: &[],
                left_subtitle_order: &[],
                sidecars: &[],
            },
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();

        // Assert: every field is written, the old title is replaced rather than kept
        // alongside, and the video track survives untouched.
        let output = media_info(&source).unwrap();
        assert_that!(output.container_title().as_deref()).contains("Big Buck Bunny");
        assert_that!(output.container_comment().as_deref()).contains("Open movie");
        assert_that!(output.container_date().as_deref()).contains("2008");
        assert_that!(output.container_genre().as_deref()).contains("Animation");
        assert_that!(output.container_artist().as_deref()).contains("Blender Foundation");
        assert_that!(
            output
                .streams
                .iter()
                .filter(|stream| stream_kind(stream) == Some("video"))
                .count()
        )
        .is_equal_to(1);

        fs::remove_dir_all(directory).unwrap();
    }

    fn edit_request(path: PathBuf, cancelled: Arc<AtomicBool>) -> EditRequest {
        EditRequest {
            path,
            destination: SaveDestination::ReplaceOriginal,
            container: None,
            container_metadata: None,
            stream_order: vec![0, 1],
            deleted_streams: BTreeSet::from([1]),
            default_streams: BTreeSet::new(),
            default_sidecars: BTreeSet::new(),
            audio_settings: BTreeMap::new(),
            video_settings: BTreeMap::new(),
            subtitle_changes: Vec::new(),
            left_subtitle_order: Vec::new(),
            sidecars: Vec::new(),
            cancelled,
        }
    }

    #[test]
    fn apply_edits_should_encode_one_audio_track_and_preserve_its_neighbor() {
        require_tools(
            "apply_edits_should_encode_one_audio_track_and_preserve_its_neighbor",
            &["ffmpeg", "ffmpeg:aac", "ffmpeg:ac3"],
        );
        let directory = scratch_directory("encode-audio-track");
        let _cleanup = DirectoryCleanup(Some(directory.clone()));
        let source = directory.join("movie.mkv");
        let status = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("color=c=black:s=64x64:r=1:d=1")
            .args(["-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo:d=1"])
            .args(["-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo:d=1"])
            .args([
                "-map",
                "0:v",
                "-map",
                "1:a",
                "-map",
                "2:a",
                "-c:v",
                "ffv1",
                "-c:a",
                "aac",
                "-metadata:s:a:0",
                "language=eng",
                "-metadata:s:a:0",
                "title=Main audio",
                "-metadata:s:a:1",
                "language=fra",
                "-metadata:s:a:1",
                "title=French audio",
                "-disposition:a:0",
                "default",
                "-disposition:a:1",
                "0",
            ])
            .arg(&source)
            .status()
            .unwrap();
        assert_that!(status.success()).is_true();
        let before = media_info(&source).unwrap();
        let mut settings = audio_settings();
        settings.codec = AudioCodec::Ac3;
        settings.channel_layout = AudioChannelLayout::Mono;
        settings.metadata = AudioMetadata {
            language: "nld".to_string(),
            title: Some("Director commentary".to_string()),
            commentary: true,
            hearing_impaired: true,
            audio_description: true,
            original: false,
            dubbed: false,
        };
        let audio_settings = BTreeMap::from([(2, settings)]);
        let defaults = BTreeSet::from([2]);
        let mut phases = Vec::new();

        apply_edits(
            EditTarget {
                source: &source,
                destination: SaveDestination::ReplaceOriginal,
                container: None,
                container_metadata: None,
            },
            TrackEdits {
                stream_order: &[0, 1, 2],
                deleted_streams: &BTreeSet::new(),
                default_streams: &defaults,
                default_sidecars: &BTreeSet::new(),
                audio_settings: &audio_settings,
                video_settings: &BTreeMap::new(),
                subtitle_changes: &[],
                left_subtitle_order: &[],
                sidecars: &[],
            },
            &AtomicBool::new(false),
            |progress| phases.push(progress.label()),
        )
        .unwrap();

        let output = media_info(&source).unwrap();
        assert_that!(output.streams.len()).is_equal_to(3);
        assert_that!(output.streams[0].get("codec_name"))
            .is_equal_to(before.streams[0].get("codec_name"));
        assert_that!(output.streams[1].get("codec_name"))
            .is_equal_to(before.streams[1].get("codec_name"));
        assert_that!(stream_language(&output.streams[1])).is_equal_to("eng".to_string());
        assert_that!(audio_stream_title(&output.streams[1]).as_deref()).contains("Main audio");
        assert_that!(is_default(&output.streams[1])).is_false();
        assert_that!(output.streams[2].get("codec_name").and_then(Value::as_str)).contains("ac3");
        assert_that!(stream_channels(&output.streams[2])).contains(1);
        assert_that!(stream_sample_rate(&output.streams[2])).contains(48_000);
        assert_that!(stream_language(&output.streams[2])).is_equal_to("nld".to_string());
        assert_that!(audio_stream_title(&output.streams[2]).as_deref())
            .contains("Director commentary");
        assert_that!(stream_commentary(&output.streams[2])).is_true();
        assert_that!(stream_hearing_impaired(&output.streams[2])).is_true();
        assert_that!(stream_disposition(&output.streams[2], "visual_impaired")).is_true();
        assert_that!(is_default(&output.streams[2])).is_true();
        let phases = phases.join("\n");
        assert_that!(phases.as_str())
            .contains("Encoding audio")
            .contains("Checking output")
            .contains("Cleaning up");
        let leftovers = fs::read_dir(&directory)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "movie.mkv")
            .collect::<Vec<_>>();
        assert_that!(leftovers).is_empty();
    }

    #[test]
    fn apply_edits_should_write_video_metadata_and_preserve_its_neighbor() {
        require_tools(
            "apply_edits_should_write_video_metadata_and_preserve_its_neighbor",
            &["ffmpeg"],
        );
        let directory = scratch_directory("video-metadata");
        let _cleanup = DirectoryCleanup(Some(directory.clone()));
        let source = directory.join("movie.mkv");
        let status = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("color=c=black:s=64x64:r=1:d=1")
            .args(["-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo:d=1"])
            .args([
                "-map",
                "0:v",
                "-map",
                "1:a",
                "-c:v",
                "ffv1",
                "-c:a",
                "aac",
                "-metadata:s:a:0",
                "language=eng",
                "-metadata:s:a:0",
                "title=Main audio",
                "-disposition:v:0",
                "0",
                "-disposition:a:0",
                "default",
            ])
            .arg(&source)
            .status()
            .unwrap();
        assert_that!(status.success()).is_true();

        let settings = VideoSettings {
            metadata: VideoMetadata {
                language: "nld".to_string(),
                title: Some("Director's cut".to_string()),
                commentary: true,
            },
            ..VideoSettings::default()
        };
        let video_settings = BTreeMap::from([(0, settings)]);
        // Video and audio defaults are tracked independently (each kind gets its own
        // exclusive default in the UI layer), so both stay flagged here.
        let defaults = BTreeSet::from([0, 1]);
        let mut phases = Vec::new();

        apply_edits(
            EditTarget {
                source: &source,
                destination: SaveDestination::ReplaceOriginal,
                container: None,
                container_metadata: None,
            },
            TrackEdits {
                stream_order: &[0, 1],
                deleted_streams: &BTreeSet::new(),
                default_streams: &defaults,
                default_sidecars: &BTreeSet::new(),
                audio_settings: &BTreeMap::new(),
                video_settings: &video_settings,
                subtitle_changes: &[],
                left_subtitle_order: &[],
                sidecars: &[],
            },
            &AtomicBool::new(false),
            |progress| phases.push(progress.label()),
        )
        .unwrap();

        let output = media_info(&source).unwrap();
        assert_that!(output.streams.len()).is_equal_to(2);
        assert_that!(output.streams[0].get("codec_name").and_then(Value::as_str)).contains("ffv1");
        assert_that!(stream_language(&output.streams[0])).is_equal_to("nld".to_string());
        assert_that!(video_stream_title(&output.streams[0]).as_deref()).contains("Director's cut");
        assert_that!(is_default(&output.streams[0])).is_true();
        assert_that!(stream_commentary(&output.streams[0])).is_true();
        // The neighboring audio track's metadata and default flag are untouched by the
        // video-only edit, including the commentary flag the video track just gained.
        assert_that!(stream_language(&output.streams[1])).is_equal_to("eng".to_string());
        assert_that!(audio_stream_title(&output.streams[1]).as_deref()).contains("Main audio");
        assert_that!(is_default(&output.streams[1])).is_true();
        assert_that!(stream_commentary(&output.streams[1])).is_false();
        let leftovers = fs::read_dir(&directory)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "movie.mkv")
            .collect::<Vec<_>>();
        assert_that!(leftovers).is_empty();
    }

    /// Rotation rides on the copy path: the tag is written, the picture is not touched,
    /// and the neighboring video track keeps the rotation it already had — the specifier
    /// counts video tracks, so addressing the wrong one is a real failure mode.
    #[test]
    fn apply_edits_should_write_the_staged_rotation_without_re_encoding() {
        require_tools(
            "apply_edits_should_write_the_staged_rotation_without_re_encoding",
            &["ffmpeg"],
        );
        let directory = scratch_directory("video-rotation");
        let _cleanup = DirectoryCleanup(Some(directory.clone()));
        let source = directory.join("movie.mkv");
        let plain = directory.join("plain.mkv");
        let status = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("color=c=black:s=64x32:r=1:d=1")
            .args(["-f", "lavfi", "-i"])
            .arg("color=c=red:s=64x32:r=1:d=1")
            .args(["-map", "0:v", "-map", "1:v", "-c:v", "ffv1"])
            .arg(&plain)
            .status()
            .unwrap();
        assert_that!(status.success()).is_true();
        // `-display_rotation` is an input option, so tagging the second video track is a
        // separate pass over the finished file.
        let status = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-display_rotation:v:1", "180", "-i"])
            .arg(&plain)
            .args(["-map", "0", "-c", "copy"])
            .arg(&source)
            .status()
            .unwrap();
        assert_that!(status.success()).is_true();
        fs::remove_file(&plain).unwrap();
        let before = media_info(&source).unwrap();
        assert_that!(stream_rotation(&before.streams[0])).is_equal_to(VideoRotation::None);
        assert_that!(stream_rotation(&before.streams[1])).is_equal_to(VideoRotation::Cw180);

        let video_settings = BTreeMap::from([(
            0,
            VideoSettings {
                rotation: VideoRotation::Cw90,
                ..VideoSettings::default()
            },
        )]);
        apply_edits(
            EditTarget {
                source: &source,
                destination: SaveDestination::ReplaceOriginal,
                container: None,
                container_metadata: None,
            },
            TrackEdits {
                stream_order: &[0, 1],
                deleted_streams: &BTreeSet::new(),
                default_streams: &BTreeSet::new(),
                default_sidecars: &BTreeSet::new(),
                audio_settings: &BTreeMap::new(),
                video_settings: &video_settings,
                subtitle_changes: &[],
                left_subtitle_order: &[],
                sidecars: &[],
            },
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();

        let output = media_info(&source).unwrap();
        assert_that!(stream_rotation(&output.streams[0])).is_equal_to(VideoRotation::Cw90);
        assert_that!(stream_rotation(&output.streams[1])).is_equal_to(VideoRotation::Cw180);
        // Metadata only: the codec is untouched and the picture keeps its own dimensions
        // rather than being transposed by an encode.
        assert_that!(output.streams[0].get("codec_name").and_then(Value::as_str)).contains("ffv1");
        assert_that!(stream_dimension(&output.streams[0], "width")).contains(64);
        assert_that!(stream_dimension(&output.streams[0], "height")).contains(32);
    }

    /// Clearing a rotation has to remove the matrix rather than leave the old angle in
    /// place: `-display_rotation 0` is what does that, and a missing argument would let
    /// the copy remux carry the source's rotation straight through.
    #[test]
    fn apply_edits_should_clear_a_rotation_the_source_already_had() {
        require_tools(
            "apply_edits_should_clear_a_rotation_the_source_already_had",
            &["ffmpeg"],
        );
        let directory = scratch_directory("video-rotation-clear");
        let _cleanup = DirectoryCleanup(Some(directory.clone()));
        let source = directory.join("movie.mkv");
        let plain = directory.join("plain.mkv");
        let status = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("color=c=black:s=64x32:r=1:d=1")
            .args(["-c:v", "ffv1"])
            .arg(&plain)
            .status()
            .unwrap();
        assert_that!(status.success()).is_true();
        let status = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-display_rotation:v:0", "90", "-i"])
            .arg(&plain)
            .args(["-c", "copy"])
            .arg(&source)
            .status()
            .unwrap();
        assert_that!(status.success()).is_true();
        fs::remove_file(&plain).unwrap();
        assert_that!(stream_rotation(&media_info(&source).unwrap().streams[0]))
            .is_equal_to(VideoRotation::Cw90);

        let video_settings = BTreeMap::from([(0, VideoSettings::default())]);
        apply_edits(
            EditTarget {
                source: &source,
                destination: SaveDestination::ReplaceOriginal,
                container: None,
                container_metadata: None,
            },
            TrackEdits {
                stream_order: &[0],
                deleted_streams: &BTreeSet::new(),
                default_streams: &BTreeSet::new(),
                default_sidecars: &BTreeSet::new(),
                audio_settings: &BTreeMap::new(),
                video_settings: &video_settings,
                subtitle_changes: &[],
                left_subtitle_order: &[],
                sidecars: &[],
            },
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();

        let output = media_info(&source).unwrap();
        assert_that!(stream_rotation(&output.streams[0])).is_equal_to(VideoRotation::None);
    }

    #[test]
    fn apply_edits_should_encode_lossless_audio_with_automatic_channels_and_rate() {
        require_tools(
            "apply_edits_should_encode_lossless_audio_with_automatic_channels_and_rate",
            &["ffmpeg", "ffmpeg:aac", "ffmpeg:alac"],
        );
        let directory = scratch_directory("encode-lossless-audio");
        let _cleanup = DirectoryCleanup(Some(directory.clone()));
        let source = directory.join("movie.mkv");
        let status = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("color=c=black:s=64x64:r=1:d=1")
            .args(["-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo:d=1"])
            .args(["-map", "0:v", "-map", "1:a", "-c:v", "mpeg4", "-c:a", "aac"])
            .arg(&source)
            .status()
            .unwrap();
        assert_that!(status.success()).is_true();

        let mut settings = audio_settings();
        settings.codec = AudioCodec::Alac;
        settings.metadata.language = "und".to_string();
        apply_edits(
            EditTarget {
                source: &source,
                destination: SaveDestination::ReplaceOriginal,
                container: None,
                container_metadata: None,
            },
            TrackEdits {
                stream_order: &[0, 1],
                deleted_streams: &BTreeSet::new(),
                default_streams: &BTreeSet::new(),
                default_sidecars: &BTreeSet::new(),
                audio_settings: &BTreeMap::from([(1, settings)]),
                video_settings: &BTreeMap::new(),
                subtitle_changes: &[],
                left_subtitle_order: &[],
                sidecars: &[],
            },
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();

        let output = media_info(&source).unwrap();
        assert_eq!(
            output.streams[1].get("codec_name").and_then(Value::as_str),
            Some("alac")
        );
        assert_eq!(stream_channels(&output.streams[1]), Some(2));
        assert_eq!(stream_sample_rate(&output.streams[1]), Some(48_000));

        let converted = apply_edits(
            EditTarget {
                source: &source,
                destination: SaveDestination::ReplaceOriginal,
                container: Some(ContainerFormat::Mp4),
                container_metadata: None,
            },
            TrackEdits {
                stream_order: &[0, 1],
                deleted_streams: &BTreeSet::new(),
                default_streams: &BTreeSet::new(),
                default_sidecars: &BTreeSet::new(),
                audio_settings: &BTreeMap::new(),
                video_settings: &BTreeMap::new(),
                subtitle_changes: &[],
                left_subtitle_order: &[],
                sidecars: &[],
            },
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        assert_eq!(
            media_info(&converted.output_path).unwrap().streams[1]
                .get("codec_name")
                .and_then(Value::as_str),
            Some("alac")
        );
    }

    #[test]
    fn plan_requires_transcode_should_detect_only_a_genuine_codec_or_resolution_change() {
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264"},
            {"index": 1, "codec_type": "audio", "codec_name": "aac"}
        ]));

        // No video_settings entry at all: nothing to transcode.
        assert_that!(plan_requires_transcode(
            &info,
            &[0, 1],
            &BTreeMap::new(),
            &BTreeMap::new()
        ))
        .is_false();

        // Settings present but requesting the source's own codec at original
        // resolution: still just a copy, not a transcode.
        let no_op_settings = BTreeMap::from([(
            0,
            VideoSettings {
                codec: VideoCodec::H264,
                resolution: VideoResolution::Original,
                metadata: VideoMetadata {
                    language: "und".to_string(),
                    title: None,
                    commentary: false,
                },
                rotation: VideoRotation::None,
            },
        )]);
        assert_that!(plan_requires_transcode(
            &info,
            &[0, 1],
            &BTreeMap::new(),
            &no_op_settings
        ))
        .is_false();

        // A genuine codec change on the video stream: this is a real transcode.
        let transcode_settings = BTreeMap::from([(
            0,
            VideoSettings {
                codec: VideoCodec::Hevc,
                resolution: VideoResolution::Original,
                metadata: VideoMetadata {
                    language: "und".to_string(),
                    title: None,
                    commentary: false,
                },
                rotation: VideoRotation::None,
            },
        )]);
        assert_that!(plan_requires_transcode(
            &info,
            &[0, 1],
            &BTreeMap::new(),
            &transcode_settings
        ))
        .is_true();

        // Settings keyed to the AUDIO stream's index must never count, even with a
        // "transcode-shaped" value — only video streams can trigger a transcode.
        let audio_keyed_settings = BTreeMap::from([(
            1,
            VideoSettings {
                codec: VideoCodec::Hevc,
                resolution: VideoResolution::Original,
                metadata: VideoMetadata {
                    language: "und".to_string(),
                    title: None,
                    commentary: false,
                },
                rotation: VideoRotation::None,
            },
        )]);
        assert_that!(plan_requires_transcode(
            &info,
            &[0, 1],
            &BTreeMap::new(),
            &audio_keyed_settings
        ))
        .is_false();
    }

    #[test]
    fn automatic_audio_bitrates_should_scale_with_the_codec_and_channel_count() {
        // Reel picks the bitrate itself — there is no quality dial — so every lossy
        // codec/layout pair it will encode needs a value, and every pair it refuses
        // must have none rather than a silently wrong default.
        assert_that!(audio_bitrate_kbps(AudioCodec::Aac, 1)).is_equal_to(Some(96));
        assert_that!(audio_bitrate_kbps(AudioCodec::Aac, 2)).is_equal_to(Some(192));
        assert_that!(audio_bitrate_kbps(AudioCodec::Aac, 6)).is_equal_to(Some(512));
        assert_that!(audio_bitrate_kbps(AudioCodec::Aac, 8)).is_equal_to(Some(640));
        assert_that!(audio_bitrate_kbps(AudioCodec::Ac3, 6)).is_equal_to(Some(448));
        assert_that!(audio_bitrate_kbps(AudioCodec::Eac3, 6)).is_equal_to(Some(640));
        assert_that!(audio_bitrate_kbps(AudioCodec::Opus, 2)).is_equal_to(Some(128));
        assert_that!(audio_bitrate_kbps(AudioCodec::Mp3, 2)).is_equal_to(Some(192));
        assert_that!(audio_bitrate_kbps(AudioCodec::Vorbis, 8)).is_equal_to(Some(512));

        // Layouts the codec cannot carry, which `supports_channels` rejects first.
        assert_that!(audio_bitrate_kbps(AudioCodec::Ac3, 8)).is_none();
        assert_that!(audio_bitrate_kbps(AudioCodec::Mp3, 6)).is_none();
        // A stream reporting no channels at all must not panic the worker.
        assert_that!(audio_bitrate_kbps(AudioCodec::Aac, 0)).is_none();
        // Lossless codecs are never given a bitrate.
        assert_that!(audio_bitrate_kbps(AudioCodec::Flac, 2)).is_none();
        assert_that!(audio_bitrate_kbps(AudioCodec::Alac, 2)).is_none();
    }

    #[test]
    fn audio_value_contracts_should_cover_every_variant_and_boundary() {
        let codecs = [
            (AudioCodec::Original, "Original", None, None),
            (AudioCodec::Aac, "AAC", Some("aac"), Some("aac")),
            (
                AudioCodec::Ac3,
                "Dolby Digital (AC-3)",
                Some("ac3"),
                Some("ac3"),
            ),
            (
                AudioCodec::Eac3,
                "Dolby Digital Plus (E-AC-3)",
                Some("eac3"),
                Some("eac3"),
            ),
            (AudioCodec::Opus, "Opus", Some("opus"), Some("libopus")),
            (AudioCodec::Flac, "FLAC", Some("flac"), Some("flac")),
            (AudioCodec::Alac, "ALAC", Some("alac"), Some("alac")),
            (AudioCodec::Mp3, "MP3", Some("mp3"), Some("libmp3lame")),
            (
                AudioCodec::Vorbis,
                "Vorbis",
                Some("vorbis"),
                Some("libvorbis"),
            ),
        ];
        for (codec, label, name, encoder) in codecs {
            assert_eq!(codec.label(), label);
            assert_eq!(codec.codec_name(), name);
            assert_eq!(codec.encoder(), encoder);
            if let Some(name) = name {
                assert_eq!(AudioCodec::from_codec_name(name), Some(codec));
            }
        }
        assert_eq!(AudioCodec::from_codec_name("dts"), None);
        assert!(AudioCodec::Flac.is_lossless() && AudioCodec::Alac.is_lossless());
        assert!(!AudioCodec::Aac.is_lossless());

        assert!(AudioCodec::Original.supports_channels(u8::MAX));
        for codec in [AudioCodec::Ac3, AudioCodec::Eac3] {
            assert!(codec.supports_channels(6));
            assert!(!codec.supports_channels(7));
        }
        assert!(AudioCodec::Mp3.supports_channels(2));
        assert!(!AudioCodec::Mp3.supports_channels(3));
        for codec in [
            AudioCodec::Aac,
            AudioCodec::Opus,
            AudioCodec::Flac,
            AudioCodec::Alac,
            AudioCodec::Vorbis,
        ] {
            assert!(codec.supports_channels(8));
            assert!(!codec.supports_channels(9));
        }

        for codec in [
            AudioCodec::Original,
            AudioCodec::Flac,
            AudioCodec::Alac,
            AudioCodec::Vorbis,
        ] {
            assert!(codec.supports_sample_rate(192_000));
            assert!(!codec.supports_sample_rate(10_000));
        }
        assert!(AudioCodec::Aac.supports_sample_rate(96_000));
        assert!(!AudioCodec::Aac.supports_sample_rate(192_000));
        assert!(AudioCodec::Ac3.supports_sample_rate(32_000));
        assert!(!AudioCodec::Eac3.supports_sample_rate(24_000));
        assert!(AudioCodec::Opus.supports_sample_rate(48_000));
        assert!(!AudioCodec::Opus.supports_sample_rate(44_100));
        assert!(AudioCodec::Mp3.supports_sample_rate(44_100));
        assert!(!AudioCodec::Mp3.supports_sample_rate(96_000));

        for (layout, label, channels) in [
            (AudioChannelLayout::Original, "Original", None),
            (AudioChannelLayout::Surround71, "7.1 surround", Some(8)),
            (AudioChannelLayout::Surround51, "5.1 surround", Some(6)),
            (AudioChannelLayout::Stereo, "Stereo", Some(2)),
            (AudioChannelLayout::Mono, "Mono", Some(1)),
        ] {
            assert_eq!(layout.label(), label);
            assert_eq!(layout.channels(), channels);
        }
        let mut metadata = audio_settings().metadata;
        for role in AudioRole::ALL {
            assert!(!metadata.get_role(role));
            metadata.set_role(role, true);
            assert!(metadata.get_role(role));
            assert!(!role.label().is_empty());
            metadata.set_role(role, false);
        }

        let string_rate =
            serde_json::from_value(serde_json::json!({"sample_rate": "48000"})).unwrap();
        let number_rate =
            serde_json::from_value(serde_json::json!({"sample_rate": 44100})).unwrap();
        let overflow =
            serde_json::from_value(serde_json::json!({"sample_rate": 4294967296_u64})).unwrap();
        let invalid = serde_json::from_value(serde_json::json!({"sample_rate": true})).unwrap();
        assert_eq!(stream_sample_rate(&string_rate), Some(48_000));
        assert_eq!(stream_sample_rate(&number_rate), Some(44_100));
        assert_eq!(stream_sample_rate(&overflow), None);
        assert_eq!(stream_sample_rate(&invalid), None);

        let stream: BTreeMap<String, Value> = serde_json::from_value(serde_json::json!({
            "codec_name": "aac", "channels": 2, "sample_rate": "48000"
        }))
        .unwrap();
        let automatic = audio_settings();
        assert!(!audio_requires_transcode(&stream, &automatic));
        assert!(!audio_settings_require_encode(&automatic));
        // The only two staged values that can force an encode: everything else about an
        // audio track is metadata, which a `-c copy` remux carries just as well.
        for changed in [
            AudioSettings {
                codec: AudioCodec::Ac3,
                ..automatic.clone()
            },
            AudioSettings {
                channel_layout: AudioChannelLayout::Mono,
                ..automatic.clone()
            },
        ] {
            assert!(audio_requires_transcode(&stream, &changed));
            assert!(audio_settings_require_encode(&changed));
        }
        assert!(!should_write_audio_metadata(Some("video"), true, true));
        assert!(should_write_audio_metadata(Some("audio"), true, false));
        assert!(should_write_audio_metadata(Some("audio"), false, true));
        assert!(!should_write_audio_metadata(Some("audio"), false, false));
        assert!(!should_write_audio_metadata(None, false, false));
    }

    #[test]
    fn should_write_video_metadata_should_gate_on_kind_and_reason() {
        assert!(!should_write_video_metadata(Some("audio"), true, true));
        assert!(should_write_video_metadata(Some("video"), true, false));
        assert!(should_write_video_metadata(Some("video"), false, true));
        assert!(!should_write_video_metadata(Some("video"), false, false));
        assert!(!should_write_video_metadata(None, false, false));
    }

    #[test]
    fn video_disposition_should_preserve_unrelated_flags_and_toggle_default() {
        let stream = serde_json::from_value::<BTreeMap<String, Value>>(serde_json::json!({
            "disposition": {"default": 0, "attached_pic": 1, "forced": 1}
        }))
        .unwrap();
        let plain = video_metadata_of(false);
        // An unrelated flag ("attached_pic") already on the source survives regardless
        // of the default flag Reel is asked to write.
        assert_that!(video_disposition(&stream, false, &plain).as_str())
            .is_equal_to("attached_pic+forced");
        assert_that!(video_disposition(&stream, true, &plain).as_str())
            .is_equal_to("attached_pic+default+forced");
    }

    /// The commentary flag is written from the staged metadata, not carried over from the
    /// source, so clearing it has to actually drop it — a `-disposition` argument replaces
    /// the whole set, and a flag left out of the string is a flag removed.
    #[test]
    fn video_disposition_should_write_the_staged_commentary_flag() {
        let commented = serde_json::from_value::<BTreeMap<String, Value>>(serde_json::json!({
            "disposition": {"comment": 1, "attached_pic": 1}
        }))
        .unwrap();
        assert_that!(video_disposition(&commented, false, &video_metadata_of(false)).as_str())
            .is_equal_to("attached_pic");
        assert_that!(video_disposition(&commented, true, &video_metadata_of(true)).as_str())
            .is_equal_to("attached_pic+comment+default");

        let plain =
            serde_json::from_value::<BTreeMap<String, Value>>(serde_json::json!({})).unwrap();
        assert_that!(video_disposition(&plain, false, &video_metadata_of(true)).as_str())
            .is_equal_to("comment");
    }

    #[test]
    fn video_disposition_should_report_zero_when_nothing_is_set() {
        let stream =
            serde_json::from_value::<BTreeMap<String, Value>>(serde_json::json!({})).unwrap();
        assert_that!(video_disposition(&stream, false, &video_metadata_of(false)).as_str())
            .is_equal_to("0");
    }

    #[test]
    fn stream_rotation_should_read_the_display_matrix_and_normalize_the_angle() {
        let rotated = |value: Value| {
            serde_json::from_value::<BTreeMap<String, Value>>(serde_json::json!({
                "side_data_list": [{"side_data_type": "Display Matrix", "rotation": value}]
            }))
            .unwrap()
        };

        // ffmpeg normalizes on write: a 270° request reads back as -90, 180 as -180.
        for (reported, expected) in [
            (serde_json::json!(90), VideoRotation::Cw90),
            (serde_json::json!(-90), VideoRotation::Cw270),
            (serde_json::json!(270), VideoRotation::Cw270),
            (serde_json::json!(-180), VideoRotation::Cw180),
            (serde_json::json!(180), VideoRotation::Cw180),
            (serde_json::json!(0), VideoRotation::None),
            (serde_json::json!(-360), VideoRotation::None),
            // Written by something other than reel, and unrepresentable in the dialog.
            (serde_json::json!(45), VideoRotation::None),
            // Some builds report the angle as a double.
            (serde_json::json!(90.0), VideoRotation::Cw90),
        ] {
            assert_eq!(
                stream_rotation(&rotated(reported.clone())),
                expected,
                "a reported rotation of {reported} should read as {expected:?}",
            );
        }

        // A stream with no matrix, an unrelated side-data entry, or a malformed list all
        // read as unrotated rather than failing.
        let bare =
            serde_json::from_value::<BTreeMap<String, Value>>(serde_json::json!({})).unwrap();
        assert_eq!(stream_rotation(&bare), VideoRotation::None);
        let unrelated = serde_json::from_value::<BTreeMap<String, Value>>(serde_json::json!({
            "side_data_list": [{"side_data_type": "Stereo 3D"}]
        }))
        .unwrap();
        assert_eq!(stream_rotation(&unrelated), VideoRotation::None);
        let malformed = serde_json::from_value::<BTreeMap<String, Value>>(serde_json::json!({
            "side_data_list": "not an array"
        }))
        .unwrap();
        assert_eq!(stream_rotation(&malformed), VideoRotation::None);
    }

    /// `-display_rotation` counts video tracks on its own (`v:0`, `v:1`), so the specifier
    /// is not the absolute stream index reel keys its plans on. A file whose second video
    /// track is stream #3 has to be addressed as `v:1`.
    #[test]
    fn display_rotation_args_should_address_staged_tracks_by_their_video_index() {
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264"},
            {"index": 1, "codec_type": "audio", "codec_name": "aac"},
            {"index": 2, "codec_type": "subtitle", "codec_name": "subrip"},
            {"index": 3, "codec_type": "video", "codec_name": "hevc"}
        ]));
        let rotated = |rotation| VideoSettings {
            rotation,
            ..VideoSettings::default()
        };

        // Nothing staged: no arguments at all, so a copy remux carries any existing
        // matrix across untouched.
        assert_that!(display_rotation_args(&info, &BTreeMap::new())).is_empty();

        // The second video track, addressed by video index rather than stream index.
        let settings = BTreeMap::from([(3, rotated(VideoRotation::Cw90))]);
        assert_that!(display_rotation_args(&info, &settings))
            .is_equal_to(vec![("v:1".to_string(), "90".to_string())]);

        // Both tracks staged, including one cleared back to upright — which is written as
        // an explicit 0, the argument that removes an existing matrix.
        let settings = BTreeMap::from([
            (0, rotated(VideoRotation::None)),
            (3, rotated(VideoRotation::Cw270)),
        ]);
        assert_that!(display_rotation_args(&info, &settings)).is_equal_to(vec![
            ("v:0".to_string(), "0".to_string()),
            ("v:1".to_string(), "270".to_string()),
        ]);

        // A staged audio track is not a video track and never produces an argument.
        let settings = BTreeMap::from([(1, rotated(VideoRotation::Cw90))]);
        assert_that!(display_rotation_args(&info, &settings)).is_empty();
    }

    #[test]
    fn output_resolution_matches_should_expect_swapped_dimensions_for_a_baked_rotation() {
        let source = BTreeMap::from([
            ("width".to_string(), Value::from(1920)),
            ("height".to_string(), Value::from(1080)),
        ]);
        let portrait = BTreeMap::from([
            ("width".to_string(), Value::from(1080)),
            ("height".to_string(), Value::from(1920)),
        ]);
        let landscape = source.clone();
        let rotated = |rotation| VideoSettings {
            rotation,
            ..VideoSettings::default()
        };

        // An encode applies the rotation to the picture, so keeping the original size
        // means the source's dimensions swapped.
        assert_that!(output_resolution_matches(
            &portrait,
            Some(&source),
            &rotated(VideoRotation::Cw90)
        ))
        .is_true();
        assert_that!(output_resolution_matches(
            &landscape,
            Some(&source),
            &rotated(VideoRotation::Cw90)
        ))
        .is_false();
        assert_that!(output_resolution_matches(
            &portrait,
            Some(&source),
            &rotated(VideoRotation::Cw270)
        ))
        .is_true();

        // A half turn keeps the frame shape, and an unrotated encode is unconstrained.
        assert_that!(output_resolution_matches(
            &landscape,
            Some(&source),
            &rotated(VideoRotation::Cw180)
        ))
        .is_true();
        assert_that!(output_resolution_matches(
            &portrait,
            Some(&source),
            &rotated(VideoRotation::None)
        ))
        .is_true();

        // Without dimensions to compare against there is nothing to check.
        let sizeless = BTreeMap::new();
        assert_that!(output_resolution_matches(
            &portrait,
            Some(&sizeless),
            &rotated(VideoRotation::Cw90)
        ))
        .is_true();
        assert_that!(output_resolution_matches(
            &portrait,
            None,
            &rotated(VideoRotation::Cw90)
        ))
        .is_true();
    }

    fn video_metadata_of(commentary: bool) -> VideoMetadata {
        VideoMetadata {
            language: "und".to_string(),
            title: None,
            commentary,
        }
    }

    #[test]
    fn video_stream_title_should_prefer_a_real_title_over_the_generic_mp4_handler_name() {
        let named = serde_json::from_value::<BTreeMap<String, Value>>(serde_json::json!({
            "tags": {"title": "Director's cut"}
        }))
        .unwrap();
        assert_that!(video_stream_title(&named)).is_equal_to(Some("Director's cut".to_string()));

        // The generic ISO-BMFF handler name ffmpeg writes when no real title was set is
        // not a title — mirrors "SoundHandler" for audio.
        let generic_handler =
            serde_json::from_value::<BTreeMap<String, Value>>(serde_json::json!({
                "tags": {"handler_name": "VideoHandler"}
            }))
            .unwrap();
        assert_that!(video_stream_title(&generic_handler)).is_none();

        let real_handler_name =
            serde_json::from_value::<BTreeMap<String, Value>>(serde_json::json!({
                "tags": {"handler_name": "Extended cut"}
            }))
            .unwrap();
        assert_that!(video_stream_title(&real_handler_name))
            .is_equal_to(Some("Extended cut".to_string()));

        let untagged =
            serde_json::from_value::<BTreeMap<String, Value>>(serde_json::json!({})).unwrap();
        assert_that!(video_stream_title(&untagged)).is_none();
    }

    #[test]
    fn video_metadata_matches_should_compare_language_title_and_commentary() {
        let stream = serde_json::from_value::<BTreeMap<String, Value>>(serde_json::json!({
            "tags": {"language": "nld", "title": "Director's cut"}
        }))
        .unwrap();
        let matching = VideoMetadata {
            language: "nld".to_string(),
            title: Some("Director's cut".to_string()),
            commentary: false,
        };
        assert_that!(video_metadata_matches(&stream, &matching)).is_true();

        let wrong_language = VideoMetadata {
            language: "eng".to_string(),
            ..matching.clone()
        };
        assert_that!(video_metadata_matches(&stream, &wrong_language)).is_false();

        let wrong_title = VideoMetadata {
            title: Some("Theatrical cut".to_string()),
            ..matching.clone()
        };
        assert_that!(video_metadata_matches(&stream, &wrong_title)).is_false();

        // The commentary flag is compared too, so a track that came back without it is
        // not mistaken for one that kept it.
        let wrong_commentary = VideoMetadata {
            commentary: true,
            ..matching
        };
        assert_that!(video_metadata_matches(&stream, &wrong_commentary)).is_false();

        let flagged = serde_json::from_value::<BTreeMap<String, Value>>(serde_json::json!({
            "tags": {"language": "nld", "title": "Director's cut"},
            "disposition": {"comment": 1}
        }))
        .unwrap();
        assert_that!(video_metadata_matches(&flagged, &wrong_commentary)).is_true();
    }

    #[test]
    fn retain_supported_video_metadata_should_clear_what_the_container_cannot_store() {
        let mut metadata = VideoMetadata {
            language: "nld".to_string(),
            title: Some("Director's cut".to_string()),
            commentary: true,
        };
        ContainerFormat::Mov.retain_supported_video_metadata(&mut metadata);
        assert_that!(metadata.language.as_str()).is_equal_to("und");
        assert_that!(metadata.title.as_deref()).contains("Director's cut");
        assert_that!(metadata.commentary).is_false();

        // Measured per container: MOV and WebM drop `-disposition:v:0 comment`, MKV and
        // MP4 write it back out.
        for (container, expected_language, expected_commentary) in [
            (ContainerFormat::Matroska, "nld", true),
            (ContainerFormat::Mp4, "nld", true),
            (ContainerFormat::WebM, "nld", false),
        ] {
            let mut metadata = VideoMetadata {
                language: "nld".to_string(),
                title: None,
                commentary: true,
            };
            container.retain_supported_video_metadata(&mut metadata);
            assert_that!(metadata.language.as_str()).is_equal_to(expected_language);
            assert_eq!(
                metadata.commentary,
                expected_commentary,
                "{container:?} should {} the commentary flag",
                if expected_commentary { "keep" } else { "clear" }
            );
        }
    }

    #[test]
    fn audio_validation_should_report_every_invalid_technical_shape() {
        let validate = |stream: Value, index: u64, settings: AudioSettings| {
            let info = media(serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                stream
            ]));
            validate_edit(
                &info,
                &[0, 1],
                &BTreeSet::new(),
                &BTreeSet::new(),
                &BTreeMap::from([(index, settings)]),
                &BTreeMap::new(),
            )
            .unwrap_err()
        };
        let base = || {
            serde_json::json!({
                "index": 1, "codec_type": "audio", "codec_name": "aac",
                "channels": 2, "sample_rate": "48000"
            })
        };

        assert_that!(validate(base(), 2, audio_settings()).as_str()).contains("missing or deleted");
        assert_that!(validate(track(1, "subtitle", "subrip"), 1, audio_settings()).as_str())
            .contains("only be applied to audio");
        let mut technical = audio_settings();
        technical.codec = AudioCodec::Ac3;
        assert_that!(validate(track(1, "audio", "aac"), 1, technical.clone()).as_str())
            .contains("channel layout");

        let mut upmix = technical.clone();
        upmix.channel_layout = AudioChannelLayout::Surround51;
        assert_that!(validate(base(), 1, upmix).as_str()).contains(CHANNEL_UPMIX_NOT_IMPLEMENTED);

        let mut unknown = audio_settings();
        unknown.channel_layout = AudioChannelLayout::Mono;
        let unknown_stream = serde_json::json!({
            "index": 1, "codec_type": "audio", "codec_name": "dts",
            "channels": 2, "sample_rate": "48000"
        });
        assert_that!(validate(unknown_stream, 1, unknown).as_str())
            .contains("original UNKNOWN codec");

        let mut unsupported_channels = technical.clone();
        unsupported_channels.codec = AudioCodec::Mp3;
        unsupported_channels.channel_layout = AudioChannelLayout::Surround51;
        let eight_channels = serde_json::json!({
            "index": 1, "codec_type": "audio", "codec_name": "aac",
            "channels": 8, "sample_rate": "48000"
        });
        assert_that!(validate(eight_channels, 1, unsupported_channels).as_str())
            .contains("does not support 6-channel");

        let missing_rate = serde_json::json!({
            "index": 1, "codec_type": "audio", "codec_name": "aac", "channels": 2
        });
        assert_that!(validate(missing_rate, 1, technical.clone()).as_str()).contains("sample rate");

        let low_rate = serde_json::json!({
            "index": 1, "codec_type": "audio", "codec_name": "aac",
            "channels": 2, "sample_rate": "16000"
        });
        assert_that!(validate(low_rate, 1, technical.clone()).as_str())
            .contains("cannot use the source sample rate");

        // A source rate above everything the codec accepts is resolved downward rather
        // than refused: AC-3 tops out at 48 kHz, so a 192 kHz source is simply encoded
        // at 48 and the edit is allowed through.
        let high_rate = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264"},
            {"index": 1, "codec_type": "audio", "codec_name": "aac",
             "channels": 2, "sample_rate": "192000"}
        ]));
        assert_that!(validate_edit(
            &high_rate,
            &[0, 1],
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeMap::from([(1, technical.clone())]),
            &BTreeMap::new(),
        ))
        .is_ok();

        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264"},
            base()
        ]));
        let mut lossless = audio_settings();
        lossless.codec = AudioCodec::Flac;
        assert_that!(validate_edit(
            &info,
            &[0, 1],
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeMap::from([(1, lossless)]),
            &BTreeMap::new(),
        ))
        .is_ok();
    }

    #[test]
    fn an_unsupported_audio_codec_should_name_the_codecs_the_container_can_hold() {
        assert_that!(container_conflict_message(ContainerFormat::Mp4, 1, "audio", "opus").as_str())
            .contains("Encode it as AAC");
    }

    #[test]
    fn the_sample_rate_should_resolve_to_the_best_rate_the_codec_accepts() {
        // Kept as-is when the codec can take it.
        assert_that!(resolved_audio_sample_rate(AudioCodec::Ac3, 48_000)).is_equal_to(Some(48_000));
        // Stepped down to the highest candidate the codec does accept.
        assert_that!(resolved_audio_sample_rate(AudioCodec::Ac3, 96_000)).is_equal_to(Some(48_000));
        assert_that!(resolved_audio_sample_rate(AudioCodec::Opus, 44_100))
            .is_equal_to(Some(24_000));
        // Nothing fits below the source rate, which validation reports as actionable.
        assert_that!(resolved_audio_sample_rate(AudioCodec::Ac3, 16_000)).is_none();
    }

    #[test]
    fn validation_should_accept_automatic_downsampling_for_the_selected_codec() {
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264"},
            {"index": 1, "codec_type": "audio", "codec_name": "aac",
             "channels": 2, "sample_rate": "96000", "tags": {"language": "eng"}}
        ]));
        let mut settings = audio_settings();
        settings.codec = AudioCodec::Ac3;

        assert_that!(validate_edit(
            &info,
            &[0, 1],
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeMap::from([(1, settings)]),
            &BTreeMap::new(),
        ))
        .is_ok();
    }

    #[test]
    fn audio_validation_should_keep_incompatible_cross_field_choices_staged_but_unsavable() {
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264"},
            {"index": 1, "codec_type": "audio", "codec_name": "aac",
             "channels": 8, "sample_rate": "48000", "tags": {"language": "eng"}}
        ]));
        let mut settings = audio_settings();
        settings.codec = AudioCodec::Mp3;
        settings.channel_layout = AudioChannelLayout::Surround51;
        let staged = BTreeMap::from([(1, settings.clone())]);

        let result = validate_edit(
            &info,
            &[0, 1],
            &BTreeSet::new(),
            &BTreeSet::new(),
            &staged,
            &BTreeMap::new(),
        );

        assert_that!(result).contains_error(
            "MP3 does not support 6-channel audio; choose another layout or codec.".to_string(),
        );
        assert_that!(staged[&1].codec).is_equal_to(AudioCodec::Mp3);
        assert_that!(staged[&1].channel_layout).is_equal_to(AudioChannelLayout::Surround51);

        // A layout the codec does support passes, so the refusal above is about the
        // channel count and not about MP3 being rejected outright.
        settings.codec = AudioCodec::Aac;
        settings.channel_layout = AudioChannelLayout::Stereo;
        assert_that!(validate_edit(
            &info,
            &[0, 1],
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeMap::from([(1, settings)]),
            &BTreeMap::new(),
        ))
        .is_ok();
    }

    #[test]
    fn metadata_only_audio_edits_should_not_require_an_encoder_or_probe_technical_fields() {
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264"},
            {"index": 1, "codec_type": "audio", "codec_name": "dts",
             "tags": {"language": "eng"}}
        ]));
        let mut settings = audio_settings();
        settings.metadata.title = Some("Director commentary".to_string());
        settings.metadata.commentary = true;
        let staged = BTreeMap::from([(1, settings)]);

        assert_that!(validate_edit(
            &info,
            &[0, 1],
            &BTreeSet::new(),
            &BTreeSet::new(),
            &staged,
            &BTreeMap::new(),
        ))
        .is_ok();
        assert_that!(plan_requires_transcode(
            &info,
            &[0, 1],
            &staged,
            &BTreeMap::new(),
        ))
        .is_false();
        assert_that!(media_write_label(None, false, false, false))
            .is_equal_to("Remuxing media".to_string());
    }

    #[test]
    fn technical_audio_edits_should_route_to_the_transcode_pool_and_name_the_phase() {
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264"},
            {"index": 1, "codec_type": "audio", "codec_name": "aac",
             "channels": 2, "sample_rate": "48000"}
        ]));
        let mut settings = audio_settings();
        settings.codec = AudioCodec::Ac3;
        settings.channel_layout = AudioChannelLayout::Mono;
        let staged = BTreeMap::from([(1, settings)]);

        assert_that!(plan_requires_transcode(
            &info,
            &[0, 1],
            &staged,
            &BTreeMap::new(),
        ))
        .is_true();
        assert_that!(media_write_label(
            Some(ContainerFormat::Matroska),
            true,
            false,
            false
        ))
        .is_equal_to("Encoding audio and remuxing to MKV".to_string());
        assert_that!(media_changes_required(
            &info,
            &[0, 1],
            &BTreeSet::new(),
            &BTreeSet::new(),
            &staged,
            &BTreeMap::new(),
            &[],
            false,
        ))
        .is_true();
    }

    #[test]
    fn container_conversion_should_discard_unsupported_audio_metadata_roles() {
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "vp9"},
            {"index": 1, "codec_type": "audio", "codec_name": "opus",
             "tags": {"language": "eng"}, "disposition": {"comment": 1}}
        ]));

        let converting = container_conflicts(
            &info,
            &[0, 1],
            &BTreeMap::new(),
            &BTreeMap::new(),
            &[],
            ContainerFormat::WebM,
        );

        // WebM stores no audio role at all, and the commentary flag on track #1 is simply
        // dropped on the way out rather than reported: the dialog never offers a field the
        // container can't store, so a conflict here would name something the user cannot act on.
        assert_that!(converting).is_empty();
    }

    #[test]
    fn audio_metadata_should_retain_only_what_the_target_container_can_store() {
        let metadata = AudioMetadata {
            language: "eng".to_string(),
            title: Some("Original commentary".to_string()),
            commentary: true,
            hearing_impaired: true,
            audio_description: true,
            original: true,
            dubbed: false,
        };

        let mut matroska = metadata.clone();
        ContainerFormat::Matroska.retain_supported_audio_metadata(&mut matroska);
        assert_that!(matroska).is_equal_to(metadata.clone());

        let mut mp4 = metadata.clone();
        ContainerFormat::Mp4.retain_supported_audio_metadata(&mut mp4);
        assert_that!(mp4.language.as_str()).is_equal_to("eng");
        assert_that!(mp4.title.as_deref()).contains("Original commentary");
        assert!(mp4.commentary && mp4.hearing_impaired && mp4.audio_description);
        assert_that!(mp4.original).is_false();

        for (container, language) in [
            (ContainerFormat::Mov, "und"),
            (ContainerFormat::WebM, "eng"),
        ] {
            let mut retained = metadata.clone();
            container.retain_supported_audio_metadata(&mut retained);
            assert_that!(retained.language.as_str()).is_equal_to(language);
            assert_that!(retained.title.as_deref()).contains("Original commentary");
            for role in AudioRole::ALL {
                assert_that!(retained.get_role(role)).is_false();
            }
        }

        let stream: BTreeMap<String, Value> = serde_json::from_value(serde_json::json!({
            "tags": {"language": "eng", "title": "Source"},
            "disposition": {"original": 1}
        }))
        .unwrap();
        let from_source = audio_metadata_for_output(&stream, None, None);
        assert_eq!(from_source.title.as_deref(), Some("Source"));
        assert!(from_source.original);
        let normalized = audio_metadata_for_output(
            &stream,
            Some(&AudioSettings {
                metadata: metadata.clone(),
                ..audio_settings()
            }),
            Some(ContainerFormat::Mov),
        );
        assert_eq!(normalized.language, "und");
        assert!(
            AudioRole::ALL
                .into_iter()
                .all(|role| !normalized.get_role(role))
        );
    }

    #[test]
    fn matroska_should_accept_original_with_other_audio_roles() {
        for role in AudioRole::ALL {
            assert!(
                ContainerFormat::Matroska.supports_audio_role(role),
                "Matroska should support {}",
                role.label()
            );
        }
        assert_that!(ContainerFormat::Mp4.supports_audio_role(AudioRole::Original)).is_false();
        assert_that!(ContainerFormat::Mp4.supports_audio_role(AudioRole::Dubbed)).is_true();
        for role in AudioRole::ALL {
            assert_that!(ContainerFormat::WebM.supports_audio_role(role)).is_false();
            assert_that!(ContainerFormat::Mov.supports_audio_role(role)).is_false();
        }
    }

    #[test]
    fn original_and_dubbed_audio_roles_should_be_mutually_exclusive() {
        let mut metadata = audio_settings().metadata;
        metadata.set_role(AudioRole::Original, true);
        assert!(metadata.original && !metadata.dubbed);

        metadata.set_role(AudioRole::Dubbed, true);
        assert!(metadata.dubbed && !metadata.original);

        // Clearing one is not enough to set the other: each direction has to hold on its
        // own, because `set_role` is the only way the dialog ever changes a role and it is
        // what keeps the pair from ever both being staged.
        metadata.set_role(AudioRole::Original, true);
        assert!(metadata.original && !metadata.dubbed);
        metadata.set_role(AudioRole::Original, false);
        assert!(!metadata.original && !metadata.dubbed);
    }

    #[test]
    fn append_edit_failure_log_should_record_enough_context_to_diagnose_a_failure() {
        // Arrange: a batch notice is terse and gone the moment the next one replaces
        // it, so the log is the only place left to see what was actually attempted.
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-edit-failure-log-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let log_path = directory.join("edit_errors.log");
        let mut request = edit_request(
            PathBuf::from("/videos/movie.mkv"),
            Arc::new(AtomicBool::new(false)),
        );
        request.container = Some(ContainerFormat::Mp4);
        request.video_settings.insert(
            0,
            VideoSettings {
                codec: VideoCodec::H264,
                resolution: VideoResolution::Original,
                metadata: VideoMetadata {
                    language: "und".to_string(),
                    title: None,
                    commentary: false,
                },
                rotation: VideoRotation::None,
            },
        );
        request.subtitle_changes.push(SubtitleChange {
            cue_edits: Default::default(),
            source: SubtitleSource::Embedded(3),
            source_format: SubtitleFormat::SubRip,
            embedded_target: Some(SubtitleFormat::MovText),
            export_target: None,
            import_into_media: false,
            ocr_language: None,
            metadata: None,
        });

        // Act
        append_edit_failure_log(
            &log_path,
            &request,
            "Failed",
            "An embedded subtitle track changed. Reopen the file and try again.",
        );

        // Assert: the log file itself (and its parent directory) got created on
        // demand, and the line names the file, what was being attempted (including
        // the actual staged indices, not just counts), and why it failed.
        let contents = fs::read_to_string(&log_path).unwrap();
        assert_that!(contents.as_str()).contains("/videos/movie.mkv");
        assert_that!(contents.as_str()).contains("Failed");
        assert_that!(contents.as_str())
            .contains("An embedded subtitle track changed. Reopen the file and try again.");
        assert_that!(contents.as_str()).contains("container: MP4");
        assert_that!(contents.as_str()).contains("video_settings: 1");
        assert_that!(contents.as_str()).contains("stream_order: [0, 1]");
        assert_that!(contents.as_str()).contains("deleted_streams: {1}");
        assert_that!(contents.as_str()).contains("#3(embedded_target=Some(MovText)");

        // Act: a second failure appends rather than overwriting the first.
        append_edit_failure_log(&log_path, &request, "SourceChanged", "second failure");
        let contents = fs::read_to_string(&log_path).unwrap();
        assert_that!(contents.lines().count()).is_equal_to(2);

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn the_failure_log_should_name_a_sidecar_change_by_its_filename() {
        // Arrange: sidecar-sourced changes are logged by filename rather than by index —
        // an absolute path would bury the useful part of an already long line, and
        // "sidecar" alone would not say *which* one. Failures involving sidecar
        // conversion are among the most common in the log, so this arm matters as much
        // as the embedded one already covered above.
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-edit-failure-log-sidecar-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let log_path = directory.join("edit_errors.log");
        let mut request = edit_request(
            PathBuf::from("/videos/movie.mkv"),
            Arc::new(AtomicBool::new(false)),
        );
        // Left unset, so the "no container change" wording is exercised too.
        request.container = None;
        request.subtitle_changes.push(SubtitleChange {
            cue_edits: Default::default(),
            source: SubtitleSource::Sidecar(PathBuf::from("/videos/movie.eng.srt")),
            source_format: SubtitleFormat::SubRip,
            embedded_target: None,
            export_target: Some(SubtitleFormat::Ass),
            import_into_media: true,
            ocr_language: None,
            metadata: None,
        });

        // Act
        append_edit_failure_log(&log_path, &request, "Failed", "conversion failed");

        // Assert
        let contents = fs::read_to_string(&log_path).unwrap();
        assert_that!(contents.as_str()).contains("movie.eng.srt(embedded_target=None");
        assert_that!(contents.as_str()).contains("export_target=Some(Ass)");
        assert_that!(contents.as_str()).contains("import=true");
        assert_that!(contents.as_str()).contains("container: unchanged");
        // The full path is reduced to the filename rather than repeated in full.
        assert!(
            !contents.contains("/videos/movie.eng.srt"),
            "the sidecar must be named by filename, got {contents:?}",
        );

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_log_directory_that_cannot_be_created_should_not_take_the_edit_down_with_it() {
        // Arrange: the log lives under `$XDG_CACHE_HOME`, which can be unwritable, full,
        // or — as simulated here — blocked by a regular file sitting where the directory
        // needs to be. Logging is diagnostics; a failure to record a failure must stay
        // silent rather than panic on top of the error being reported.
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-edit-failure-log-blocked-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        // A file where the log's parent directory would have to be.
        let blocker = directory.join("cache");
        fs::write(&blocker, b"not a directory").unwrap();
        let log_path = blocker.join("reel-tui").join("edit_errors.log");
        let request = edit_request(
            PathBuf::from("/videos/movie.mkv"),
            Arc::new(AtomicBool::new(false)),
        );

        // Act / Assert: returns normally, writes nothing, and leaves the blocker intact.
        append_edit_failure_log(&log_path, &request, "Failed", "some failure");
        assert!(!log_path.exists());
        assert_that!(fs::read(&blocker).unwrap()).is_equal_to(b"not a directory".to_vec());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_panicking_edit_should_become_a_failure_message_rather_than_killing_the_pool() {
        // The worker catches panics so one broken request cannot poison the shared
        // receiver and silently stop every other worker in its pool. What the user ends
        // up reading is whatever the panic carried, so both payload shapes have to
        // survive: `panic!("literal")` hands over a `&str`, `panic!("{x}")` a `String`.
        let literal = std::panic::catch_unwind(|| panic!("a broken invariant")).unwrap_err();
        assert_that!(panic_message(&*literal).as_str()).is_equal_to("a broken invariant");

        let formatted = std::panic::catch_unwind(|| panic!("track {} is missing", 7)).unwrap_err();
        assert_that!(panic_message(&*formatted).as_str()).is_equal_to("track 7 is missing");

        // A payload that is neither still has to produce something printable.
        let exotic = std::panic::catch_unwind(|| std::panic::panic_any(42_u8)).unwrap_err();
        assert_that!(panic_message(&*exotic).as_str()).is_equal_to("an unknown internal error");
    }

    #[test]
    fn spawn_edit_worker_pools_should_run_both_pools_concurrently_and_attribute_progress_by_path() {
        // Regression test for the transcode/remux pool split: requests sent to
        // *either* pool must be answered (not just the one historically exercised),
        // and `EditEvent::Progress` must carry the path it's about now that more than
        // one file can be in flight at once.
        let (transcode_requests, remux_requests, events) = spawn_edit_worker_pools(1, 1);
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-edit-worker-pools-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let transcode_target = directory.join("not-a-file-transcode.mkv");
        let remux_target = directory.join("not-a-file-remux.mkv");

        transcode_requests
            .send(edit_request(
                transcode_target.clone(),
                Arc::new(AtomicBool::new(false)),
            ))
            .unwrap();
        remux_requests
            .send(edit_request(
                remux_target.clone(),
                Arc::new(AtomicBool::new(false)),
            ))
            .unwrap();

        // Both requests point at a source that doesn't exist, so each fails fast as
        // `SourceChanged` without needing ffmpeg — but a Finished event, correctly
        // attributed by path, must arrive for both regardless of which pool ran it.
        let mut finished: BTreeSet<PathBuf> = BTreeSet::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while finished.len() < 2 && std::time::Instant::now() < deadline {
            match events.recv_timeout(std::time::Duration::from_millis(500)) {
                Ok(EditEvent::Finished { path, outcome }) => {
                    assert!(
                        matches!(outcome, EditOutcome::SourceChanged(_)),
                        "expected SourceChanged for a missing source, got {outcome:?}",
                    );
                    finished.insert(path);
                }
                Ok(EditEvent::Progress { path, .. }) => {
                    assert!(
                        path == transcode_target || path == remux_target,
                        "progress event named an unexpected path: {path:?}",
                    );
                }
                Err(_) => break,
            }
        }
        assert_that!(&finished).is_equal_to(&BTreeSet::from([
            transcode_target.clone(),
            remux_target.clone(),
        ]));

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn edit_worker_should_report_progress_then_a_finished_event_for_each_request() {
        // Arrange: the worker is the only thing standing between the UI thread and a
        // multi-minute ffmpeg run, so it has to answer every request exactly once —
        // including the ones that fail.
        let (requests, _remux_requests, events) = spawn_edit_worker_pools(1, 1);
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-edit-worker-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let missing = directory.join("not-a-file.mkv");

        // Act: a source that does not exist, so the run fails without needing ffmpeg.
        requests
            .send(edit_request(
                missing.clone(),
                Arc::new(AtomicBool::new(false)),
            ))
            .unwrap();

        // Assert: progress may or may not arrive first, but a Finished event always does,
        // naming the file it was about.
        let mut outcome = None;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while outcome.is_none() && std::time::Instant::now() < deadline {
            match events.recv_timeout(std::time::Duration::from_millis(500)) {
                Ok(EditEvent::Finished { path, outcome: got }) => {
                    assert_eq!(path, missing, "the event must name its request");
                    outcome = Some(got);
                }
                Ok(EditEvent::Progress { .. }) => {}
                Err(_) => break,
            }
        }
        assert!(
            matches!(outcome, Some(EditOutcome::SourceChanged(_))),
            "a source that vanished should be reported as such, got {outcome:?}",
        );

        // Act: an existing file, with the request cancelled before it starts.
        let existing = directory.join("movie.mkv");
        fs::write(&existing, b"not really media").unwrap();
        requests
            .send(edit_request(existing, Arc::new(AtomicBool::new(true))))
            .unwrap();

        // Assert: still answered, as Cancelled — a request the worker swallows would
        // leave the progress dialog up forever.
        let mut outcome = None;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while outcome.is_none() && std::time::Instant::now() < deadline {
            match events.recv_timeout(std::time::Duration::from_millis(500)) {
                Ok(EditEvent::Finished { outcome: got, .. }) => outcome = Some(got),
                Ok(EditEvent::Progress { .. }) => {}
                Err(_) => break,
            }
        }
        // The source is inspected before the cancellation flag is consulted, so an
        // unreadable file reports the read failure; either way the request is answered.
        assert!(
            matches!(
                outcome,
                Some(EditOutcome::Cancelled | EditOutcome::Failed(_))
            ),
            "a second request must be answered too, got {outcome:?}",
        );

        drop(requests);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn publish_transaction_should_restore_every_backup_when_one_publish_fails() {
        // Arrange: two sidecars publishing together, the second of which cannot be moved
        // because its staged file is gone. This is the transaction's whole reason for
        // existing: a half-published edit would leave one subtitle replaced and the other
        // missing, with no way for the user to tell what happened.
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-rollback-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let workspace = directory.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let original = directory.join("movie.eng.srt");
        let second = directory.join("movie.nld.srt");
        let staged = workspace.join("staged-eng");
        fs::write(&original, b"old subtitle").unwrap();
        fs::write(&staged, b"new subtitle").unwrap();
        let publications = [
            Publication {
                staged: vec![(staged.clone(), original.clone())],
                remove: vec![original.clone()],
            },
            Publication {
                // Never written, so publishing it fails partway through.
                staged: vec![(workspace.join("staged-nld"), second.clone())],
                remove: Vec::new(),
            },
        ];

        // Act
        let result = publish_transaction(None, None, &publications, &AtomicBool::new(false));

        // Assert: the transaction reports failure and the directory is exactly as it was.
        let Err(EditError::Failed(error)) = result else {
            panic!("a missing staged file should fail the transaction");
        };
        assert_that!(error.as_str()).contains("Could not publish");
        assert_that!(fs::read(&original).unwrap()).is_equal_to(b"old subtitle".to_vec());
        assert!(
            !second.exists(),
            "the second sidecar was never published and must not exist",
        );
        // The backup is rolled back into place rather than left beside the original.
        let leftovers: Vec<_> = fs::read_dir(&workspace)
            .unwrap()
            .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
            .filter(|name| name.to_string_lossy().starts_with("transaction-backup"))
            .collect();
        assert!(leftovers.is_empty(), "backups left behind: {leftovers:?}");

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn command_error_should_summarise_stderr_without_letting_it_run_away() {
        // Arrange / Act / Assert: ffmpeg's stderr goes straight into an error dialog, so
        // it has to be one line and it has to fit.
        assert_that!(command_error("Could not remux", b"").as_str()).is_equal_to("Could not remux");
        assert_that!(
            command_error("Could not remux", b"\n  first line  \n\n  second line\n").as_str()
        )
        .is_equal_to("Could not remux: first line second line");

        let long = command_error("Failed", "é".repeat(500).as_bytes());
        let detail = long.strip_prefix("Failed: ").expect("the heading is kept");
        assert_that!(detail.chars().count()).is_equal_to(361);
        assert!(
            detail.ends_with('…'),
            "a truncated detail is marked as such"
        );
    }

    #[test]
    fn container_metadata_should_only_be_empty_when_no_field_is_set() {
        // Arrange / Act / Assert: an empty metadata block means "write nothing", so a
        // single set field has to be enough to make it non-empty.
        assert!(ContainerMetadata::default().is_empty());
        for metadata in [
            ContainerMetadata {
                title: Some("Big Buck Bunny".to_string()),
                ..ContainerMetadata::default()
            },
            ContainerMetadata {
                comment: Some("a note".to_string()),
                ..ContainerMetadata::default()
            },
            ContainerMetadata {
                date: Some("2008".to_string()),
                ..ContainerMetadata::default()
            },
            ContainerMetadata {
                genre: Some("Animation".to_string()),
                ..ContainerMetadata::default()
            },
            ContainerMetadata {
                artist: Some("Blender Foundation".to_string()),
                ..ContainerMetadata::default()
            },
        ] {
            assert!(!metadata.is_empty(), "{metadata:?} sets a field");
        }
    }

    #[test]
    fn cross_extension_replacement_should_preserve_every_file_when_target_exists() {
        // Arrange
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-cross-extension-collision-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("movie.mkv");
        let staged = directory.join(".reel-tui-movie.mp4");
        let target = directory.join("movie.mp4");
        fs::write(&source, b"old media").unwrap();
        fs::write(&staged, b"new media").unwrap();
        fs::write(&target, b"existing target").unwrap();

        // Act
        let result = publish_transaction(
            Some((&staged, &target)),
            Some(&source),
            &[],
            &AtomicBool::new(false),
        );

        // Assert
        let Err(EditError::Failed(error)) = result else {
            panic!("existing target should reject the transaction");
        };
        assert_that!(error).is_equal_to(format!(
            "{} already exists; no files were changed.",
            target.display()
        ));
        assert_that!(fs::read(&source).unwrap()).is_equal_to(b"old media".to_vec());
        assert_that!(fs::read(&staged).unwrap()).is_equal_to(b"new media".to_vec());
        assert_that!(fs::read(&target).unwrap()).is_equal_to(b"existing target".to_vec());

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    fn duplicates_scratch(tag: &str) -> (PathBuf, PathBuf) {
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let workspace = directory.join(".reel-tui-work");
        fs::create_dir_all(&workspace).unwrap();
        (directory, workspace)
    }

    #[test]
    fn numbered_subtitle_path_should_refuse_a_filename_it_cannot_number() {
        // Arrange / Act / Assert: numbering splices between stem and extension, so a name
        // with no extension has nowhere to put the number. Falling back to appending would
        // produce a file ffmpeg no longer recognises as a subtitle.
        let missing_extension = numbered_subtitle_path(Path::new("/media/movie"), 1);
        let Err(EditError::Failed(message)) = missing_extension else {
            panic!("a filename with no extension must not be numbered");
        };
        assert_that!(message).is_equal_to("Subtitle filename has no extension.".to_string());

        // And the ordinary case still numbers between the stem and the extension.
        assert_eq!(
            numbered_subtitle_path(Path::new("/media/movie.eng.srt"), 3).unwrap(),
            PathBuf::from("/media/movie.eng.3.srt"),
        );
    }

    #[test]
    fn two_new_exports_landing_on_one_name_should_be_numbered_from_one() {
        // Arrange: two embedded subtitles exported in the same save that both resolve to
        // `movie.eng.srt`, with no such file on disk yet. Without numbering, the second
        // publish silently overwrites the first and the user loses a track they asked to
        // keep. Numbering starts at 1 here because there is no unnumbered file to preserve.
        let (directory, workspace) = duplicates_scratch("export-two-new");
        let base = directory.join("movie.eng.srt");
        let first_staged = workspace.join("first.srt");
        let second_staged = workspace.join("second.srt");
        fs::write(&first_staged, b"first").unwrap();
        fs::write(&second_staged, b"second").unwrap();
        let mut publications = vec![
            Publication {
                staged: vec![(first_staged, base.clone())],
                remove: Vec::new(),
            },
            Publication {
                staged: vec![(second_staged, base.clone())],
                remove: Vec::new(),
            },
        ];

        // Act
        resolve_export_duplicates(&mut publications, &workspace).unwrap();
        publish_transaction(None, None, &publications, &AtomicBool::new(false)).unwrap();

        // Assert: both survive under distinct names, and neither took the bare name.
        assert!(!base.exists(), "the unnumbered name must not be claimed");
        assert_that!(fs::read(directory.join("movie.eng.1.srt")).unwrap())
            .is_equal_to(b"first".to_vec());
        assert_that!(fs::read(directory.join("movie.eng.2.srt")).unwrap())
            .is_equal_to(b"second".to_vec());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_lone_new_export_should_keep_its_unnumbered_name() {
        // Arrange: the ordinary case — one export, nothing already at that name. Numbering
        // it anyway would leave the user with `movie.eng.1.srt` for a file that had no
        // duplicate, and players that match sidecars by name would stop finding it.
        let (directory, workspace) = duplicates_scratch("export-lone");
        let base = directory.join("movie.eng.srt");
        let staged = workspace.join("only.srt");
        fs::write(&staged, b"only").unwrap();
        let mut publications = vec![Publication {
            staged: vec![(staged, base.clone())],
            remove: Vec::new(),
        }];

        // Act
        resolve_export_duplicates(&mut publications, &workspace).unwrap();
        publish_transaction(None, None, &publications, &AtomicBool::new(false)).unwrap();

        // Assert
        assert_that!(fs::read(&base).unwrap()).is_equal_to(b"only".to_vec());
        assert!(!directory.join("movie.eng.1.srt").exists());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_duplicate_export_should_refuse_rather_than_overwrite_an_existing_numbered_sidecar() {
        // Arrange: `movie.eng.srt` and `movie.eng.1.srt` both already exist. Renaming the
        // unnumbered one to `.1` to make room would destroy the user's existing `.1` file,
        // so the whole save must refuse before touching anything.
        let (directory, workspace) = duplicates_scratch("export-occupied");
        let base = directory.join("movie.eng.srt");
        let occupied = directory.join("movie.eng.1.srt");
        let staged = workspace.join("new.srt");
        fs::write(&base, b"existing base").unwrap();
        fs::write(&occupied, b"existing one").unwrap();
        fs::write(&staged, b"new").unwrap();
        let mut publications = vec![Publication {
            staged: vec![(staged, base.clone())],
            remove: Vec::new(),
        }];

        // Act
        let result = resolve_export_duplicates(&mut publications, &workspace);

        // Assert: refused, naming both files, and nothing on disk was altered.
        let Err(EditError::Failed(message)) = result else {
            panic!("an occupied .1 target must refuse the export");
        };
        assert!(
            message.contains("movie.eng.srt") && message.contains("movie.eng.1.srt"),
            "the message must name both files, got {message:?}",
        );
        assert_that!(fs::read(&base).unwrap()).is_equal_to(b"existing base".to_vec());
        assert_that!(fs::read(&occupied).unwrap()).is_equal_to(b"existing one".to_vec());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn renumbering_a_vobsub_export_should_keep_its_sub_and_idx_halves_on_the_same_number() {
        // Arrange: VobSub is two files that must agree — `movie.eng.sub` is meaningless
        // without a `movie.eng.idx` of the same name. Renumbering the pair independently
        // (or numbering the `.idx` as if it were the subtitle) splits them apart and the
        // subtitle stops loading entirely.
        let (directory, workspace) = duplicates_scratch("export-vobsub");
        let base = directory.join("movie.eng.sub");
        let base_idx = directory.join("movie.eng.idx");
        fs::write(&base, b"old sub").unwrap();
        fs::write(&base_idx, b"old idx").unwrap();
        let staged_sub = workspace.join("new.sub");
        let staged_idx = workspace.join("new.idx");
        fs::write(&staged_sub, b"new sub").unwrap();
        fs::write(&staged_idx, b"new idx").unwrap();
        let mut publications = vec![Publication {
            staged: vec![(staged_sub, base.clone()), (staged_idx, base_idx.clone())],
            remove: Vec::new(),
        }];

        // Act
        resolve_export_duplicates(&mut publications, &workspace).unwrap();
        publish_transaction(None, None, &publications, &AtomicBool::new(false)).unwrap();

        // Assert: the existing pair moved to .1 together, the new pair landed on .2
        // together, and neither half kept the unnumbered name.
        assert!(!base.exists() && !base_idx.exists());
        assert_that!(fs::read(directory.join("movie.eng.1.sub")).unwrap())
            .is_equal_to(b"old sub".to_vec());
        assert_that!(fs::read(directory.join("movie.eng.1.idx")).unwrap())
            .is_equal_to(b"old idx".to_vec());
        assert_that!(fs::read(directory.join("movie.eng.2.sub")).unwrap())
            .is_equal_to(b"new sub".to_vec());
        assert_that!(fs::read(directory.join("movie.eng.2.idx")).unwrap())
            .is_equal_to(b"new idx".to_vec());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn duplicate_export_should_rename_unnumbered_sidecar_to_one_and_publish_new_as_two() {
        // Arrange
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-subtitle-duplicate-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let workspace = directory.join(".reel-tui-work");
        fs::create_dir_all(&workspace).unwrap();
        let base = directory.join("movie.eng.srt");
        let staged = workspace.join("new.srt");
        fs::write(&base, b"old").unwrap();
        fs::write(&staged, b"new").unwrap();
        let mut publications = vec![Publication {
            staged: vec![(staged, base.clone())],
            remove: Vec::new(),
        }];

        // Act
        resolve_export_duplicates(&mut publications, &workspace).unwrap();
        publish_transaction(None, None, &publications, &AtomicBool::new(false)).unwrap();

        // Assert
        assert_that!(base.exists()).is_false();
        assert_that!(fs::read(directory.join("movie.eng.1.srt")).unwrap())
            .is_equal_to(b"old".to_vec());
        assert_that!(fs::read(directory.join("movie.eng.2.srt")).unwrap())
            .is_equal_to(b"new".to_vec());

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn sidecar_conversion_destination_should_use_next_free_number_when_duplicates_exist() {
        // Arrange
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-sidecar-next-number-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("movie.eng.ass");
        let base = directory.join("movie.eng.srt");
        fs::write(&source, b"source").unwrap();
        fs::write(&base, b"first").unwrap();
        fs::write(directory.join("movie.eng.2.srt"), b"second").unwrap();
        let sidecar = SidecarEntry {
            path: source.clone(),
            companion: None,
            display_name: "movie.eng.ass".to_string(),
            format: SubtitleFormat::Ass,
            language: "eng".to_string(),
            forced: false,
            hearing_impaired: false,
            number: None,
            fingerprint: FileFingerprint::for_path(&source).unwrap(),
            companion_fingerprint: None,
        };

        // Act
        let destination =
            sidecar_conversion_destination(&base, &sidecar, SubtitleFormat::SubRip, &[]).unwrap();

        // Assert
        assert_that!(destination).is_equal_to(directory.join("movie.eng.3.srt"));

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn sidecar_conversion_destination_should_drop_source_number_when_target_has_no_duplicate() {
        // Arrange
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-sidecar-drop-number-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("movie.eng.2.srt");
        let base = directory.join("movie.eng.ass");
        fs::write(&source, b"source").unwrap();
        let sidecar = SidecarEntry {
            path: source.clone(),
            companion: None,
            display_name: "movie.eng.2.srt".to_string(),
            format: SubtitleFormat::SubRip,
            language: "eng".to_string(),
            forced: false,
            hearing_impaired: false,
            number: Some(2),
            fingerprint: FileFingerprint::for_path(&source).unwrap(),
            companion_fingerprint: None,
        };

        // Act
        let destination =
            sidecar_conversion_destination(&base, &sidecar, SubtitleFormat::Ass, &[]).unwrap();

        // Assert
        assert_that!(destination).is_equal_to(base);

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn copy_path_should_support_files_without_an_extension() {
        // Arrange
        let source = Path::new("/videos/movie");

        // Act
        let output = copy_path(source, 1, None).unwrap();

        // Assert
        assert_that!(output).is_equal_to(PathBuf::from("/videos/movie-reel-edit"));
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

    #[test]
    fn subtitle_disposition_should_preserve_unrelated_flags_and_apply_all_editable_roles() {
        let stream = serde_json::from_value::<BTreeMap<String, Value>>(serde_json::json!({
            "disposition": {"default": 0, "forced": 0, "dub": 1, "comment": 0}
        }))
        .unwrap();
        let metadata = SubtitleMetadata {
            language: "eng".to_string(),
            title: Some("English CC".to_string()),
            forced: true,
            cc: true,
            hearing_impaired: true,
            original: true,
            commentary: true,
        };

        let disposition = subtitle_disposition(Some(&stream), true, &metadata);

        let dispositions = disposition.split('+').collect::<Vec<_>>();
        for flag in [
            "default",
            "forced",
            "captions",
            "hearing_impaired",
            "original",
            "comment",
            "dub",
        ] {
            assert_that!(&dispositions).contains(flag);
        }
    }

    #[test]
    fn subtitle_metadata_matrix_should_match_supported_container_roles() {
        use crate::subtitle::SubtitleFlag;

        assert_that!(ContainerFormat::Matroska.supports_subtitle_flag(SubtitleFlag::Original))
            .is_true();
        assert_that!(ContainerFormat::Matroska.supports_subtitle_flag(SubtitleFlag::Cc)).is_false();
        assert_that!(
            ContainerFormat::Matroska.supports_subtitle_flag(SubtitleFlag::HearingImpaired)
        )
        .is_true();
        assert_that!(ContainerFormat::Mp4.supports_subtitle_flag(SubtitleFlag::Commentary))
            .is_true();
        assert_that!(ContainerFormat::Mp4.supports_subtitle_flag(SubtitleFlag::Cc)).is_true();
        assert_that!(ContainerFormat::Mp4.supports_subtitle_flag(SubtitleFlag::HearingImpaired))
            .is_true();
        assert_that!(ContainerFormat::Mp4.supports_subtitle_flag(SubtitleFlag::Original))
            .is_false();
        assert_that!(ContainerFormat::Mov.supports_subtitle_flag(SubtitleFlag::Forced)).is_false();
        assert_that!(ContainerFormat::WebM.supports_subtitle_flag(SubtitleFlag::Cc)).is_true();
        assert_that!(ContainerFormat::WebM.supports_subtitle_flag(SubtitleFlag::HearingImpaired))
            .is_false();
        assert_that!(ContainerFormat::WebM.supports_subtitle_flag(SubtitleFlag::Commentary))
            .is_false();
    }

    #[test]
    fn converting_containers_should_clear_subtitle_flags_the_target_cannot_hold() {
        // Arrange: a subtitle carrying every flag, being moved into a container that
        // supports only some of them. The unsupported ones must be cleared rather than
        // written anyway — ffmpeg accepts the disposition arguments regardless, so a flag
        // the container has no field for is silently lost at mux time and the staged
        // metadata then disagrees with the file that was actually produced.
        let full = || SubtitleMetadata {
            language: "eng".to_string(),
            title: Some("English".to_string()),
            forced: true,
            cc: true,
            hearing_impaired: true,
            original: true,
            commentary: true,
        };

        // Act / Assert: Matroska keeps everything except CC.
        let mut matroska = full();
        ContainerFormat::Matroska.retain_supported_subtitle_metadata(&mut matroska);
        assert!(!matroska.cc, "Matroska has no CC disposition");
        assert!(matroska.forced && matroska.hearing_impaired);
        assert!(matroska.original && matroska.commentary);

        // Act / Assert: MP4 keeps everything except Original.
        let mut mp4 = full();
        ContainerFormat::Mp4.retain_supported_subtitle_metadata(&mut mp4);
        assert!(!mp4.original, "MP4 has no Original disposition");
        assert!(mp4.forced && mp4.cc && mp4.hearing_impaired && mp4.commentary);

        // Act / Assert: WebM keeps only Forced and CC.
        let mut webm = full();
        ContainerFormat::WebM.retain_supported_subtitle_metadata(&mut webm);
        assert!(webm.forced && webm.cc);
        assert!(!webm.hearing_impaired && !webm.original && !webm.commentary);

        // Act / Assert: MOV supports no subtitle flags at all.
        let mut mov = full();
        ContainerFormat::Mov.retain_supported_subtitle_metadata(&mut mov);
        assert!(!mov.forced && !mov.cc && !mov.hearing_impaired);
        assert!(!mov.original && !mov.commentary);

        // The language and title are not flags and survive every container.
        assert_eq!(mov.language, "eng");
        assert_eq!(mov.title.as_deref(), Some("English"));
    }

    /// A cue edited on the subtitle edit page is written back into the sidecar it came from, under
    /// the name it already has. The conversion path below derives a filename from the
    /// track's language and flags, which is right when the format or the metadata changed
    /// and wrong here — rewriting a line must not also rename the file.
    #[test]
    fn a_cue_edit_should_rewrite_the_sidecar_in_place() {
        // Arrange: a sidecar named nothing like what `sidecar_filename` would choose.
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-cue-edit-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let workspace = directory.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let media_path = directory.join("movie.mkv");
        let source = directory.join("movie.English.srt");
        fs::write(
            &source,
            b"1\n00:00:01,000 --> 00:00:02,000\nHello\n\n2\n00:00:03,000 --> 00:00:04,000\nWorld\n\n",
        )
        .unwrap();
        let sidecar = SidecarEntry {
            path: source.clone(),
            companion: None,
            display_name: "movie.English.srt".to_string(),
            format: SubtitleFormat::SubRip,
            language: "eng".to_string(),
            forced: false,
            hearing_impaired: false,
            number: None,
            fingerprint: FileFingerprint::for_path(&source).unwrap(),
            companion_fingerprint: None,
        };
        let change = SubtitleChange {
            cue_edits: BTreeMap::from([(1, cue_words("World", "World, rewritten", 3, 4))]),
            source: SubtitleSource::Sidecar(source.clone()),
            source_format: SubtitleFormat::SubRip,
            embedded_target: None,
            export_target: None,
            import_into_media: false,
            ocr_language: None,
            metadata: None,
        };

        // Act
        let prepared = prepare_subtitle_changes(
            &media_path,
            &media(serde_json::json!([{"index": 0, "codec_type": "video"}])),
            &[change],
            &[sidecar],
            &BTreeSet::new(),
            &workspace,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();

        // Assert: published over the file it came from, with the edit applied and the
        // untouched cue exactly as it was.
        assert_that!(prepared.publications.len()).is_equal_to(1);
        let (staged, destination) = prepared.publications[0].staged[0].clone();
        assert_that!(destination).is_equal_to(source.clone());
        assert_that!(prepared.publications[0].remove.clone()).contains(source.clone());
        let written = fs::read_to_string(&staged).unwrap();
        assert_that!(written.as_str()).contains("World, rewritten");
        assert_that!(written.as_str()).contains("00:00:01,000 --> 00:00:02,000\nHello");
        // Nothing to remux for a sidecar's own text.
        assert_that!(prepared.replacements.is_empty()).is_true();

        fs::remove_dir_all(directory).unwrap();
    }

    /// An embedded track's cues are rewritten the only way anything in a container is: the
    /// track comes out, is rewritten, and goes back in as a replacement stream in the remux
    /// that the change already requires. Asserted end to end through `apply_edits`, because
    /// the halves — extract, rewrite, map the replacement into the output — each look right
    /// on their own while the file still holds the old words.
    #[test]
    fn a_cue_edit_should_rewrite_an_embedded_track_through_the_remux() {
        require_tools(
            "a_cue_edit_should_rewrite_an_embedded_track_through_the_remux",
            &["ffmpeg", "ffprobe"],
        );
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-cue-edit-embedded-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let cues = directory.join("cues.srt");
        fs::write(
            &cues,
            b"1\n00:00:00,000 --> 00:00:01,000\nHello\n\n2\n00:00:01,000 --> 00:00:02,000\nWorld\n\n",
        )
        .unwrap();
        let source = directory.join("movie.mkv");
        let status = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=16x16:r=1:d=2",
                "-i",
            ])
            .arg(&cues)
            .args([
                "-c:v",
                "ffv1",
                "-c:s",
                "srt",
                "-metadata:s:s:0",
                "language=eng",
            ])
            .arg(&source)
            .status()
            .unwrap();
        assert_that!(status.success()).is_true();

        // Act: rewrite the second cue and save.
        let change = SubtitleChange {
            cue_edits: BTreeMap::from([(1, cue_words("World", "World, rewritten", 1, 2))]),
            source: SubtitleSource::Embedded(1),
            source_format: SubtitleFormat::SubRip,
            embedded_target: None,
            export_target: None,
            import_into_media: false,
            ocr_language: None,
            metadata: None,
        };
        let mut phases = Vec::new();
        apply_edits(
            EditTarget {
                source: &source,
                destination: SaveDestination::ReplaceOriginal,
                container: None,
                container_metadata: None,
            },
            TrackEdits {
                stream_order: &[0, 1],
                deleted_streams: &BTreeSet::new(),
                default_streams: &BTreeSet::new(),
                default_sidecars: &BTreeSet::new(),
                audio_settings: &BTreeMap::new(),
                video_settings: &BTreeMap::new(),
                subtitle_changes: &[change],
                left_subtitle_order: &[],
                sidecars: &[],
            },
            &AtomicBool::new(false),
            |progress| phases.push(progress.phase.label()),
        )
        .expect("the save should succeed");

        // Assert: the words in the container itself changed, and the cue left alone did not.
        let extracted = directory.join("out.srt");
        let status = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-i"])
            .arg(&source)
            .args(["-map", "0:s:0", "-c:s", "copy"])
            .arg(&extracted)
            .status()
            .unwrap();
        assert_that!(status.success()).is_true();
        let written = fs::read_to_string(&extracted).unwrap();
        assert_that!(written.as_str()).contains("World, rewritten");
        assert_that!(written.as_str()).contains("Hello");

        // Assert: and the step announced itself, so a save that stalls there says where.
        assert_that!(
            phases
                .iter()
                .any(|phase| phase.starts_with("Rewriting cues"))
        )
        .is_true();

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    /// The same edit against a file that changed underneath it stops the save rather than
    /// landing the text on whichever line moved into the slot.
    #[test]
    fn a_cue_edit_should_refuse_a_sidecar_that_changed_underneath_it() {
        // Arrange: the file says something else at that position now.
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-cue-edit-stale-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let workspace = directory.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let source = directory.join("movie.eng.srt");
        fs::write(
            &source,
            b"1\n00:00:01,000 --> 00:00:02,000\nSomething else\n\n",
        )
        .unwrap();
        let sidecar = SidecarEntry {
            path: source.clone(),
            companion: None,
            display_name: "movie.eng.srt".to_string(),
            format: SubtitleFormat::SubRip,
            language: "eng".to_string(),
            forced: false,
            hearing_impaired: false,
            number: None,
            fingerprint: FileFingerprint::for_path(&source).unwrap(),
            companion_fingerprint: None,
        };
        let change = SubtitleChange {
            cue_edits: BTreeMap::from([(0, cue_words("Hello", "Hello, rewritten", 1, 2))]),
            source: SubtitleSource::Sidecar(source.clone()),
            source_format: SubtitleFormat::SubRip,
            embedded_target: None,
            export_target: None,
            import_into_media: false,
            ocr_language: None,
            metadata: None,
        };

        // Act
        let error = prepare_subtitle_changes(
            &directory.join("movie.mkv"),
            &media(serde_json::json!([{"index": 0, "codec_type": "video"}])),
            &[change],
            &[sidecar],
            &BTreeSet::new(),
            &workspace,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap_err();

        // Assert: refused, saying why, and the file is untouched.
        assert_that!(format!("{error:?}")).contains("changed elsewhere");
        assert_that!(fs::read_to_string(&source).unwrap().as_str()).contains("Something else");

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn metadata_only_sidecar_change_should_stage_a_collision_safe_filename_rename() {
        require_tools(
            "metadata_only_sidecar_change_should_stage_a_collision_safe_filename_rename",
            &["ffprobe"],
        );
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-sidecar-metadata-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let workspace = directory.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let media_path = directory.join("movie.mkv");
        let source = directory.join("movie.eng.srt");
        fs::write(&source, b"1\n00:00:00,000 --> 00:00:01,000\nHello\n").unwrap();
        let sidecar = SidecarEntry {
            path: source.clone(),
            companion: None,
            display_name: "movie.eng.srt".to_string(),
            format: SubtitleFormat::SubRip,
            language: "eng".to_string(),
            forced: false,
            hearing_impaired: false,
            number: None,
            fingerprint: FileFingerprint::for_path(&source).unwrap(),
            companion_fingerprint: None,
        };
        let change = SubtitleChange {
            cue_edits: Default::default(),
            source: SubtitleSource::Sidecar(source.clone()),
            source_format: SubtitleFormat::SubRip,
            embedded_target: None,
            export_target: None,
            import_into_media: false,
            ocr_language: None,
            metadata: Some(SubtitleMetadata {
                language: "nld".to_string(),
                title: None,
                forced: true,
                cc: false,
                hearing_impaired: false,
                original: false,
                commentary: false,
            }),
        };

        let prepared = prepare_subtitle_changes(
            &media_path,
            &media(serde_json::json!([{"index": 0, "codec_type": "video"}])),
            &[change],
            &[sidecar],
            &BTreeSet::new(),
            &workspace,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();

        assert_that!(prepared.publications.len()).is_equal_to(1);
        assert_that!(prepared.publications[0].staged[0].1.clone())
            .is_equal_to(directory.join("movie.nld.forced.srt"));
        assert_that!(prepared.publications[0].remove.clone()).contains(source);

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apply_edits_should_write_and_validate_embedded_subtitle_metadata() {
        require_tools(
            "apply_edits_should_write_and_validate_embedded_subtitle_metadata",
            &["ffmpeg"],
        );
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-embedded-metadata-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let captions = directory.join("captions.srt");
        let source = directory.join("movie.mkv");
        fs::write(&captions, b"1\n00:00:00,000 --> 00:00:01,000\nHello\n").unwrap();
        let status = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=16x16:r=1:d=1",
                "-i",
            ])
            .arg(&captions)
            .arg("-i")
            .arg(&captions)
            .arg("-i")
            .arg(&captions)
            .args([
                "-map",
                "0:v:0",
                "-map",
                "1:s:0",
                "-map",
                "2:s:0",
                "-map",
                "3:s:0",
                "-c:v",
                "ffv1",
                "-c:s",
                "subrip",
                "-metadata:s:s:0",
                "language=cze",
                "-metadata:s:s:1",
                "language=ger",
                "-metadata:s:s:1",
                "title=Old title",
                "-metadata:s:s:2",
                "language=chi",
            ])
            .arg(&source)
            .status()
            .unwrap();
        assert_that!(status.success()).is_true();
        let info = media_info(&source).unwrap();
        let subtitle_index = info
            .streams
            .iter()
            .find(|stream| {
                stream_kind(stream) == Some("subtitle") && stream_language(stream) == "deu"
            })
            .and_then(stream_index)
            .unwrap();
        let stream_order = info
            .streams
            .iter()
            .filter_map(stream_index)
            .collect::<Vec<_>>();
        let mut defaults = info
            .streams
            .iter()
            .filter(|stream| is_default(stream))
            .filter_map(stream_index)
            .collect::<BTreeSet<_>>();
        defaults.insert(subtitle_index);
        let change = SubtitleChange {
            cue_edits: Default::default(),
            source: SubtitleSource::Embedded(subtitle_index),
            source_format: SubtitleFormat::SubRip,
            embedded_target: None,
            export_target: None,
            import_into_media: false,
            ocr_language: None,
            metadata: Some(SubtitleMetadata {
                language: "fra".to_string(),
                title: Some("Sous-titres français".to_string()),
                forced: true,
                cc: false,
                hearing_impaired: true,
                original: true,
                commentary: true,
            }),
        };

        apply_edits(
            EditTarget {
                source: &source,
                destination: SaveDestination::ReplaceOriginal,
                container: None,
                container_metadata: None,
            },
            TrackEdits {
                stream_order: &stream_order,
                deleted_streams: &BTreeSet::new(),
                default_streams: &defaults,
                default_sidecars: &BTreeSet::new(),
                audio_settings: &BTreeMap::new(),
                video_settings: &BTreeMap::new(),
                subtitle_changes: &[change],
                left_subtitle_order: &[],
                sidecars: &[],
            },
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();

        let output = media_info(&source).unwrap();
        let languages = output
            .streams
            .iter()
            .filter(|stream| stream_kind(stream) == Some("subtitle"))
            .map(stream_language)
            .collect::<Vec<_>>();
        let subtitle = output
            .streams
            .iter()
            .find(|stream| {
                stream_kind(stream) == Some("subtitle") && stream_language(stream) == "fra"
            })
            .unwrap();
        assert_that!(languages).contains_exactly_in_given_order([
            "ces".to_string(),
            "fra".to_string(),
            "zho".to_string(),
        ]);
        assert_that!(stream_title(subtitle).as_deref()).contains("Sous-titres français");
        assert_that!(stream_forced(subtitle)).is_true();
        assert_that!(stream_cc(subtitle)).is_false();
        assert_that!(stream_hearing_impaired(subtitle)).is_true();
        assert_that!(stream_original(subtitle)).is_true();
        assert_that!(stream_commentary(subtitle)).is_true();

        fs::remove_dir_all(directory).unwrap();
    }
}
