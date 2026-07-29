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
    probe::{MediaInfo, ProbeOutcome, is_attached_picture, probe_any_file, probe_file},
    subtitle::{
        SidecarEntry, SubtitleChange, SubtitleFormat, SubtitleSource, sidecar_filename, stream_cc,
        stream_forced, stream_language,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoSettings {
    pub codec: VideoCodec,
    pub resolution: VideoResolution,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SaveDestination {
    #[default]
    ReplaceOriginal,
    CreateCopy,
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
    pub destination: SaveDestination,
    pub container: Option<ContainerFormat>,
    pub stream_order: Vec<u64>,
    pub deleted_streams: BTreeSet<u64>,
    pub default_streams: BTreeSet<u64>,
    pub video_settings: BTreeMap<u64, VideoSettings>,
    pub subtitle_changes: Vec<SubtitleChange>,
    pub sidecars: Vec<SidecarEntry>,
    pub cancelled: Arc<AtomicBool>,
}

#[derive(Clone, Debug)]
pub enum EditEvent {
    Progress {
        progress: Option<f64>,
        label: String,
    },
    Finished {
        path: PathBuf,
        outcome: EditOutcome,
    },
}

#[derive(Clone, Debug)]
pub enum EditOutcome {
    Completed {
        output_path: PathBuf,
        media_changed: bool,
    },
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
            if !request.subtitle_changes.is_empty() {
                let _ = progress_tx.send(EditEvent::Progress {
                    progress: None,
                    label: "Converting subtitles…".to_string(),
                });
            }
            let result = apply_edits(
                EditTarget {
                    source: &request.path,
                    destination: request.destination,
                    container: request.container,
                },
                TrackEdits {
                    stream_order: &request.stream_order,
                    deleted_streams: &request.deleted_streams,
                    default_streams: &request.default_streams,
                    video_settings: &request.video_settings,
                    subtitle_changes: &request.subtitle_changes,
                    sidecars: &request.sidecars,
                },
                &request.cancelled,
                |progress| {
                    let _ = progress_tx.send(EditEvent::Progress {
                        progress,
                        label: if request.video_settings.is_empty() {
                            "Remuxing with ffmpeg…"
                        } else {
                            "Transcoding with ffmpeg…"
                        }
                        .to_string(),
                    });
                },
            );
            let outcome = match result {
                Ok(result) => EditOutcome::Completed {
                    output_path: result.output_path,
                    media_changed: result.media_changed,
                },
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
    video_settings: &BTreeMap<u64, VideoSettings>,
    subtitle_changes: &[SubtitleChange],
    target: ContainerFormat,
) -> Vec<String> {
    container_conflict_entries(info, stream_order, video_settings, subtitle_changes, target)
        .into_iter()
        .map(|(_, message)| message)
        .collect()
}

pub(crate) fn container_conflict_streams(
    info: &MediaInfo,
    stream_order: &[u64],
    video_settings: &BTreeMap<u64, VideoSettings>,
    subtitle_changes: &[SubtitleChange],
    target: ContainerFormat,
) -> BTreeSet<u64> {
    container_conflict_entries(info, stream_order, video_settings, subtitle_changes, target)
        .into_iter()
        .map(|(index, _)| index)
        .collect()
}

fn container_conflict_entries(
    info: &MediaInfo,
    stream_order: &[u64],
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
        if target.supports_codec(kind, codec, is_attached_picture(stream)) {
            continue;
        }
        conflicts.push((
            *index,
            container_conflict_message(target, *index, kind, codec),
        ));
    }
    conflicts
}

fn container_conflict_message(
    target: ContainerFormat,
    index: u64,
    kind: &str,
    codec: &str,
) -> String {
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
            "Audio conversion is not available; choose another container or remove the track."
                .to_string()
        }
        _ => "Choose MKV or remove the track.".to_string(),
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

fn apply_edits(
    target: EditTarget<'_>,
    edits: TrackEdits<'_>,
    cancelled: &AtomicBool,
    mut report_progress: impl FnMut(Option<f64>),
) -> Result<EditResult, EditError> {
    let TrackEdits {
        stream_order,
        deleted_streams,
        default_streams,
        video_settings,
        subtitle_changes,
        sidecars,
    } = edits;
    let path = target.source;
    let destination = target.destination;
    let target_container = target.container;
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
    validate_subtitle_sources(&source_info, subtitle_changes, sidecars)
        .map_err(EditError::Failed)?;
    let output_stream_order = stream_order
        .iter()
        .copied()
        .filter(|index| {
            !subtitle_changes.iter().any(|change| {
                change.source == SubtitleSource::Embedded(*index) && change.removes_from_media()
            })
        })
        .collect::<Vec<_>>();
    if let Some(container) = target_container {
        let conflicts = container_conflicts(
            &source_info,
            &output_stream_order,
            video_settings,
            subtitle_changes,
            container,
        );
        if !conflicts.is_empty() {
            return Err(EditError::Failed(format!(
                "The selected container is incompatible:\n{}",
                conflicts.join("\n")
            )));
        }
    }
    let duration = media_duration(&source_info);
    let container_changed = target_container
        .is_some_and(|container| ContainerFormat::from_path(path) != Some(container));
    let media_changed = media_changes_required(
        &source_info,
        &output_stream_order,
        deleted_streams,
        default_streams,
        video_settings,
        subtitle_changes,
        container_changed,
    );
    if media_changed {
        report_progress(duration.map(|_| 0.0));
    }

    if cancelled.load(Ordering::Relaxed) {
        return Err(EditError::Cancelled);
    }
    let workspace_path = temporary_workspace(path).map_err(EditError::Failed)?;
    fs::create_dir(&workspace_path).map_err(|error| {
        EditError::Failed(format!("Could not create subtitle workspace: {error}"))
    })?;
    let _workspace_cleanup = DirectoryCleanup(Some(workspace_path.clone()));
    let prepared = prepare_subtitle_changes(
        path,
        &source_info,
        subtitle_changes,
        sidecars,
        &workspace_path,
        cancelled,
    )?;
    if !media_changed {
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
        resolve_export_duplicates(&mut publications, &workspace_path)?;
        publish_transaction(None, None, &publications, cancelled)?;
        return Ok(EditResult {
            output_path: path.to_path_buf(),
            media_changed: false,
        });
    }
    let temporary = temporary_path(path, target_container).map_err(EditError::Failed)?;
    let mut cleanup = TempCleanup(Some(temporary.clone()));
    let output = run_ffmpeg(
        FfmpegPlan {
            source: path,
            temporary: &temporary,
            source_info: &source_info,
            stream_order: &output_stream_order,
            default_streams,
            video_settings,
            replacements: &prepared.replacements,
            container: target_container,
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
    if let Some(container) = target_container {
        validate_output_container(&output_info, &temporary, container)
            .map_err(EditError::Failed)?;
    }
    let expected_count = output_stream_order.len();
    if output_info.streams.len() != expected_count {
        return Err(EditError::Failed(format!(
            "The remuxed file has {} tracks; expected {expected_count}.",
            output_info.streams.len()
        )));
    }
    validate_result(
        &source_info,
        &output_info,
        &output_stream_order,
        default_streams,
        video_settings,
        &prepared.replacements,
    )
    .map_err(EditError::Failed)?;
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
    fs::set_permissions(&temporary, source_permissions).map_err(|error| {
        EditError::Failed(format!("Could not preserve source permissions: {error}"))
    })?;
    if cancelled.load(Ordering::Relaxed) {
        return Err(EditError::Cancelled);
    }
    let output_path = match destination {
        SaveDestination::ReplaceOriginal => {
            let mut publications = prepared.publications.clone();
            resolve_export_duplicates(&mut publications, &workspace_path)?;
            let replacement = replacement_path(path, target_container)?;
            if replacement != path && replacement.exists() {
                return Err(EditError::Failed(format!(
                    "{} already exists; choose Create a copy or rename it.",
                    replacement.display()
                )));
            }
            publish_transaction(
                Some((&temporary, &replacement)),
                Some(path),
                &publications,
                cancelled,
            )?;
            cleanup.0 = None;
            replacement
        }
        SaveDestination::CreateCopy => {
            let copy = next_copy_path(path, target_container)?;
            let mut publications =
                retarget_publications_for_copy(&prepared.publications, path, &copy)?;
            resolve_export_duplicates(&mut publications, &workspace_path)?;
            publish_transaction(Some((&temporary, &copy)), None, &publications, cancelled)?;
            cleanup.0 = None;
            copy
        }
    };
    report_progress(Some(1.0));
    Ok(EditResult {
        output_path,
        media_changed: true,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EditResult {
    output_path: PathBuf,
    media_changed: bool,
}

fn media_changes_required(
    source_info: &MediaInfo,
    stream_order: &[u64],
    deleted_streams: &BTreeSet<u64>,
    default_streams: &BTreeSet<u64>,
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
struct Publication {
    staged: Vec<(PathBuf, PathBuf)>,
    remove: Vec<PathBuf>,
}

#[derive(Debug, Default)]
struct PreparedSubtitles {
    replacements: Vec<SubtitleReplacement>,
    publications: Vec<Publication>,
}

fn prepare_subtitle_changes(
    media_path: &Path,
    info: &MediaInfo,
    changes: &[SubtitleChange],
    sidecars: &[SidecarEntry],
    workspace: &Path,
    cancelled: &AtomicBool,
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
            return Err(EditError::Cancelled);
        }
        match &change.source {
            SubtitleSource::Embedded(index) => {
                let stream = info
                    .streams
                    .iter()
                    .find(|stream| stream_index(stream) == Some(*index))
                    .expect("subtitle sources are validated before preparation");
                let mut replacement_artifact = None;
                if let Some(target) = change.embedded_target {
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
                    )?;
                    validate_subtitle_output(&staged, target)?;
                    replacement_artifact = Some((target, staged.clone()));
                    prepared.replacements.push(SubtitleReplacement {
                        source_index: *index,
                        target,
                        path: staged,
                    });
                }
                if let Some(target) = change.export_target {
                    let filename = sidecar_filename(
                        media_stem,
                        &stream_language(stream),
                        stream_forced(stream),
                        stream_cc(stream),
                        None,
                        target,
                    );
                    let staged = workspace.join(format!("export-{job}.{}", target.extension()));
                    if let Some((converted, converted_path)) = &replacement_artifact
                        && *converted == target
                    {
                        copy_subtitle_artifact(converted_path, &staged, target)?;
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
                        )?;
                    }
                    validate_subtitle_output(&staged, target)?;
                    prepared.publications.push(Publication {
                        staged: subtitle_artifact_pairs(&staged, &parent.join(filename), target)?,
                        remove: Vec::new(),
                    });
                }
            }
            SubtitleSource::Sidecar(path) => {
                let target = change
                    .embedded_target
                    .expect("sidecar changes have a conversion target");
                let sidecar = sidecars
                    .iter()
                    .find(|sidecar| &sidecar.path == path)
                    .expect("subtitle sources are validated before preparation");
                let base_filename = sidecar_filename(
                    media_stem,
                    &sidecar.language,
                    sidecar.forced,
                    sidecar.cc,
                    None,
                    target,
                );
                let destination = sidecar_conversion_destination(
                    &parent.join(base_filename),
                    sidecar,
                    target,
                    &prepared.publications,
                )?;
                let staged = workspace.join(format!("sidecar-{job}.{}", target.extension()));
                convert_subtitle(
                    ConversionInput::File(path),
                    change,
                    target,
                    &staged,
                    resolution,
                )?;
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

fn convert_subtitle(
    input: ConversionInput<'_>,
    change: &SubtitleChange,
    target: SubtitleFormat,
    output: &Path,
    resolution: (u64, u64),
) -> Result<(), EditError> {
    if change.source_format == target
        && !(matches!(input, ConversionInput::Embedded { .. }) && target == SubtitleFormat::VobSub)
    {
        return extract_subtitle(input, output, target);
    }
    if change.source_format.is_image() && target == SubtitleFormat::MovText {
        let intermediate = output.with_extension("ocr.srt");
        convert_subtitle(
            input,
            change,
            SubtitleFormat::SubRip,
            &intermediate,
            resolution,
        )?;
        let text_change = SubtitleChange {
            source: change.source.clone(),
            source_format: SubtitleFormat::SubRip,
            embedded_target: Some(SubtitleFormat::MovText),
            export_target: None,
            ocr_language: None,
        };
        return convert_subtitle(
            ConversionInput::File(&intermediate),
            &text_change,
            SubtitleFormat::MovText,
            output,
            resolution,
        );
    }
    let use_seconv = change.source_format.is_image() || target.is_image();
    let result = if use_seconv {
        let (extracted, file_input) = match input {
            ConversionInput::Embedded { media, index } => {
                let name = output
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("subtitle");
                let extracted_path = output.with_file_name(format!("{name}.source.mks"));
                let extraction = Command::new("ffmpeg")
                    .args(["-v", "error", "-nostdin", "-y", "-i"])
                    .arg(media)
                    .args(["-map", &format!("0:{index}"), "-c", "copy"])
                    .arg(&extracted_path)
                    .output()
                    .map_err(|error| {
                        EditError::Failed(if error.kind() == std::io::ErrorKind::NotFound {
                            "ffmpeg was not found in PATH.".to_string()
                        } else {
                            format!("Could not extract subtitle for conversion: {error}")
                        })
                    })?;
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
            command.arg("--ocr-engine:tesseract").arg(format!(
                "--ocr-language:{}",
                change.ocr_language.as_deref().unwrap_or("eng")
            ));
        }
        if target.is_image() {
            command.arg(format!("--resolution:{}x{}", resolution.0, resolution.1));
        }
        command.output()
    } else {
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
        command.args(["-c:s", encoder]).arg(output).output()
    }
    .map_err(|error| {
        EditError::Failed(if error.kind() == std::io::ErrorKind::NotFound {
            format!(
                "{} was not found in PATH.",
                if use_seconv { "seconv" } else { "ffmpeg" }
            )
        } else {
            format!("Could not start subtitle conversion: {error}")
        })
    })?;
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
) -> Result<(), EditError> {
    match input {
        ConversionInput::File(path) => copy_subtitle_artifact(path, output, format),
        ConversionInput::Embedded { media, index } => {
            let result = Command::new("ffmpeg")
                .args(["-v", "error", "-nostdin", "-y", "-i"])
                .arg(media)
                .args(["-map", &format!("0:{index}"), "-c:s", "copy"])
                .arg(output)
                .output()
                .map_err(|error| {
                    EditError::Failed(if error.kind() == std::io::ErrorKind::NotFound {
                        "ffmpeg was not found in PATH.".to_string()
                    } else {
                        format!("Could not start subtitle export: {error}")
                    })
                })?;
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

fn copy_subtitle_artifact(
    source: &Path,
    destination: &Path,
    format: SubtitleFormat,
) -> Result<(), EditError> {
    fs::copy(source, destination)
        .map_err(|error| EditError::Failed(format!("Could not stage subtitle export: {error}")))?;
    if format == SubtitleFormat::VobSub {
        fs::copy(
            source.with_extension("idx"),
            destination.with_extension("idx"),
        )
        .map_err(|error| {
            EditError::Failed(format!(
                "Could not stage the VobSub .idx companion: {error}"
            ))
        })?;
    }
    Ok(())
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

fn primary_video_resolution(info: &MediaInfo) -> Option<(u64, u64)> {
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
}

#[derive(Clone, Copy)]
struct TrackEdits<'a> {
    stream_order: &'a [u64],
    deleted_streams: &'a BTreeSet<u64>,
    default_streams: &'a BTreeSet<u64>,
    video_settings: &'a BTreeMap<u64, VideoSettings>,
    subtitle_changes: &'a [SubtitleChange],
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

fn publish_transaction(
    media: Option<(&Path, &Path)>,
    removed_media: Option<&Path>,
    publications: &[Publication],
    cancelled: &AtomicBool,
) -> Result<(), EditError> {
    if cancelled.load(Ordering::Relaxed) {
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
        if let Err(error) = fs::rename(old, &backup) {
            rollback_transaction(&[], &backups);
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
        if let Err(error) = fs::rename(&staged, &destination) {
            rollback_transaction(&published, &backups);
            return Err(EditError::Failed(format!(
                "Could not publish the completed edit: {error}"
            )));
        }
        published.push(destination);
    }
    for (backup, _) in backups {
        let _ = fs::remove_file(backup);
    }
    Ok(())
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
    subtitle_replacements: &[SubtitleReplacement],
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
        if let Some(replacement) = subtitle_replacements
            .iter()
            .find(|replacement| replacement.source_index == *source_index)
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
        if !output_resolution_matches(stream, settings.resolution) {
            return Err(format!(
                "The encoded video track at position {position} has the wrong resolution."
            ));
        }
    }
    Ok(())
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
    default_streams: &'a BTreeSet<u64>,
    video_settings: &'a BTreeMap<u64, VideoSettings>,
    replacements: &'a [SubtitleReplacement],
    container: Option<ContainerFormat>,
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
    for replacement in plan.replacements {
        command.arg("-i").arg(&replacement.path);
    }
    for index in plan.stream_order {
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
            if let Some(filter) = resolution_filter(settings.resolution) {
                command
                    .arg(format!("-filter:v:{video_output_index}"))
                    .arg(filter);
            }
        }
        video_output_index += 1;
    }
    for (output_index, source_index) in plan.stream_order.iter().enumerate() {
        let replacement = plan
            .replacements
            .iter()
            .any(|replacement| replacement.source_index == *source_index);
        let source_stream = plan
            .source_info
            .streams
            .iter()
            .find(|stream| stream_index(stream) == Some(*source_index));
        if replacement {
            if let Some(stream) = source_stream {
                command
                    .arg(format!("-metadata:s:{output_index}"))
                    .arg(format!("language={}", stream_language(stream)));
            }
            let mut disposition = Vec::new();
            if plan.default_streams.contains(source_index) {
                disposition.push("default");
            }
            if source_stream.is_some_and(stream_forced) {
                disposition.push("forced");
            }
            if source_stream.is_some_and(stream_cc) {
                disposition.push("hearing_impaired");
            }
            command
                .arg(format!("-disposition:{output_index}"))
                .arg(if disposition.is_empty() {
                    "0".to_string()
                } else {
                    disposition.join("+")
                });
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
    if let Some(container) = plan.container {
        if container == ContainerFormat::Mp4 {
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

fn temporary_path(path: &Path, container: Option<ContainerFormat>) -> Result<PathBuf, String> {
    let parent = path
        .parent()
        .ok_or_else(|| "The source file has no parent directory.".to_string())?;
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
    let parent = path
        .parent()
        .ok_or_else(|| "The source file has no parent directory.".to_string())?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Ok(parent.join(format!(".reel-tui-{nonce}-subtitle-work")))
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

fn output_resolution_matches(
    stream: &BTreeMap<String, Value>,
    resolution: VideoResolution,
) -> bool {
    let width = stream_dimension(stream, "width");
    let height = stream_dimension(stream, "height");
    match resolution {
        VideoResolution::Original => true,
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
    use std::process::Stdio;

    fn media(streams: Value) -> MediaInfo {
        MediaInfo::from_json(serde_json::json!({"streams": streams})).unwrap()
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
            },
        )]);
        let subtitle_changes = [SubtitleChange {
            source: SubtitleSource::Embedded(2),
            source_format: SubtitleFormat::SubRip,
            embedded_target: Some(SubtitleFormat::MovText),
            export_target: None,
            ocr_language: None,
        }];

        // Act
        let conflicts = container_conflicts(
            &info,
            &[0, 1, 2],
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
            source: SubtitleSource::Embedded(2),
            source_format: SubtitleFormat::SubRip,
            embedded_target: None,
            export_target: Some(SubtitleFormat::SubRip),
            ocr_language: None,
        }];

        // Act
        let conflicts = container_conflicts(
            &info,
            &[0, 1, 2],
            &BTreeMap::new(),
            &subtitle_changes,
            ContainerFormat::Mp4,
        );

        // Assert
        assert_that!(conflicts).is_empty();
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
            {"index": 0, "codec_type": "video", "codec_name": "ffv1", "width": 1280, "height": 1024}
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
            },
        )]);

        // Act
        let result = validate_edit(&info, &[0], &BTreeSet::new(), &BTreeSet::new(), &settings);

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
                },
            )]);

            // Act
            let result = validate_edit(&info, &[0], &BTreeSet::new(), &BTreeSet::new(), &settings);

            // Assert
            assert_that!(result).contains_error("Upscaling isn't possible yet.".to_string());
        }
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
            },
        )]);

        // Act
        let result = validate_edit(&info, &[0], &BTreeSet::new(), &BTreeSet::new(), &settings);

        // Assert
        assert_that!(result)
            .contains_error("Custom width and height must be positive even numbers.".to_string());
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
        assert_that!(output_resolution_matches(
            &exact,
            VideoResolution::Custom(CustomResolution {
                width: 1280,
                height: 720,
                scaling: CustomScaling::FitPad,
            }),
        ))
        .is_true();
        assert_that!(output_resolution_matches(
            &exact,
            VideoResolution::Custom(CustomResolution {
                width: 1280,
                height: 720,
                scaling: CustomScaling::Stretch,
            }),
        ))
        .is_true();
        assert_that!(output_resolution_matches(
            &bounded,
            VideoResolution::Custom(CustomResolution {
                width: 1280,
                height: 720,
                scaling: CustomScaling::FitPad,
            }),
        ))
        .is_false();
        assert_that!(output_resolution_matches(&exact, VideoResolution::P720)).is_true();
        assert_that!(output_resolution_matches(&bounded, VideoResolution::P720)).is_false();
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
            EditTarget {
                source: &source,
                destination: SaveDestination::ReplaceOriginal,
                container: None,
            },
            TrackEdits {
                stream_order: &[1, 0, 3],
                deleted_streams: &BTreeSet::from([2]),
                default_streams: &BTreeSet::from([1, 3]),
                video_settings: &BTreeMap::new(),
                subtitle_changes: &[],
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
        assert_that!(progress.last().copied()).contains(Some(1.0));

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apply_edits_should_create_a_valid_mp4_copy_when_only_the_container_changes() {
        // Arrange
        if !Command::new("ffmpeg")
            .args(["-v", "error", "-h", "encoder=aac"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
        {
            return;
        }
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
                container: Some(ContainerFormat::Mp4),
            },
            TrackEdits {
                stream_order: &[0, 1],
                deleted_streams: &BTreeSet::new(),
                default_streams: &BTreeSet::from([0, 1]),
                video_settings: &BTreeMap::new(),
                subtitle_changes: &[],
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

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apply_edits_should_create_an_edited_copy_without_changing_the_source() {
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
            },
        )]);

        // Act
        let output = apply_edits(
            EditTarget {
                source: &source,
                destination: SaveDestination::CreateCopy,
                container: None,
            },
            TrackEdits {
                stream_order: &[0, 1],
                deleted_streams: &BTreeSet::new(),
                default_streams: &BTreeSet::new(),
                video_settings: &settings,
                subtitle_changes: &[],
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
                },
            )]);

            // Act
            let output = apply_edits(
                EditTarget {
                    source: &source,
                    destination: SaveDestination::CreateCopy,
                    container: None,
                },
                TrackEdits {
                    stream_order: &[0],
                    deleted_streams: &BTreeSet::new(),
                    default_streams: &BTreeSet::new(),
                    video_settings: &settings,
                    subtitle_changes: &[],
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
    fn apply_edits_should_convert_embedded_subtitle_in_copy_and_export_with_copy_stem() {
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
            source: SubtitleSource::Embedded(1),
            source_format: SubtitleFormat::SubRip,
            embedded_target: Some(SubtitleFormat::Ass),
            export_target: Some(SubtitleFormat::Ass),
            ocr_language: None,
        }];

        // Act
        let edited_copy = apply_edits(
            EditTarget {
                source: &source,
                destination: SaveDestination::CreateCopy,
                container: None,
            },
            TrackEdits {
                stream_order: &[0, 1],
                deleted_streams: &BTreeSet::new(),
                default_streams: &BTreeSet::new(),
                video_settings: &BTreeMap::new(),
                subtitle_changes: &changes,
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
        assert_that!(output.streams[1]["codec_name"].as_str()).contains("ass");
        assert_that!(sidecar.exists()).is_true();
        assert_that!(fs::read_to_string(sidecar).unwrap()).contains("Hello subtitles");

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apply_edits_should_export_and_remove_incompatible_subtitle_during_mp4_conversion() {
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
            source: SubtitleSource::Embedded(1),
            source_format: SubtitleFormat::SubRip,
            embedded_target: None,
            export_target: Some(SubtitleFormat::SubRip),
            ocr_language: None,
        }];

        // Act
        let result = apply_edits(
            EditTarget {
                source: &source,
                destination: SaveDestination::CreateCopy,
                container: Some(ContainerFormat::Mp4),
            },
            TrackEdits {
                stream_order: &[0, 1],
                deleted_streams: &BTreeSet::new(),
                default_streams: &BTreeSet::from([0]),
                video_settings: &BTreeMap::new(),
                subtitle_changes: &changes,
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
    fn apply_edits_should_drop_source_number_when_converted_target_has_no_duplicate() {
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
            cc: false,
            number: Some(2),
            fingerprint: FileFingerprint::for_path(&sidecar_path).unwrap(),
            companion_fingerprint: None,
        };
        let changes = [SubtitleChange {
            source: SubtitleSource::Sidecar(sidecar_path.clone()),
            source_format: SubtitleFormat::SubRip,
            embedded_target: Some(SubtitleFormat::Ass),
            export_target: None,
            ocr_language: None,
        }];
        let source_before = fs::read(&source).unwrap();

        // Act
        let result = apply_edits(
            EditTarget {
                source: &source,
                destination: SaveDestination::ReplaceOriginal,
                container: None,
            },
            TrackEdits {
                stream_order: &[0],
                deleted_streams: &BTreeSet::new(),
                default_streams: &BTreeSet::new(),
                video_settings: &BTreeMap::new(),
                subtitle_changes: &changes,
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
            cc: false,
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
            cc: false,
            number: None,
            fingerprint: FileFingerprint::for_path(&ass_path).unwrap(),
            companion_fingerprint: None,
        };
        let changes = [SubtitleChange {
            source: SubtitleSource::Sidecar(ass_path.clone()),
            source_format: SubtitleFormat::Ass,
            embedded_target: Some(SubtitleFormat::SubRip),
            export_target: None,
            ocr_language: None,
        }];
        let sidecars = [existing_sidecar, ass_sidecar];

        // Act
        let result = apply_edits(
            EditTarget {
                source: &source,
                destination: SaveDestination::ReplaceOriginal,
                container: None,
            },
            TrackEdits {
                stream_order: &[0],
                deleted_streams: &BTreeSet::new(),
                default_streams: &BTreeSet::new(),
                video_settings: &BTreeMap::new(),
                subtitle_changes: &changes,
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
            cc: false,
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
            cc: false,
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
}
