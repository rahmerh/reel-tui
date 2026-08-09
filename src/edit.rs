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
    probe::{MediaInfo, ProbeOutcome, is_attached_picture, probe_any_file, probe_file},
    subtitle::{
        SidecarEntry, SubtitleChange, SubtitleFlag, SubtitleFormat, SubtitleMetadata,
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
    video_settings: &BTreeMap<u64, VideoSettings>,
) -> bool {
    stream_order.iter().any(|source_index| {
        info.streams
            .iter()
            .find(|stream| stream_index(stream) == Some(*source_index))
            .is_some_and(|stream| {
                stream_kind(stream) == Some("video")
                    && video_settings
                        .get(source_index)
                        .is_some_and(|settings| requires_transcode(stream, *settings))
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
         video_settings: {}, subtitle_changes: [{}]) — {message}\n",
        request.path.display(),
        request.destination,
        request.stream_order,
        request.deleted_streams,
        request.default_streams,
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
                let result = apply_edits(
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
                );
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

        let media_operation =
            media_write_label(target_container, video_settings, container_metadata_changed);

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
            left_subtitle_order,
            default_streams,
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
    video_settings: &BTreeMap<u64, VideoSettings>,
    container_metadata_changed: bool,
) -> String {
    match (!video_settings.is_empty(), target_container) {
        (true, Some(container)) => {
            format!("Encoding video and remuxing to {}", container.label())
        }
        (true, None) => "Encoding video".to_string(),
        (false, Some(container)) => format!("Remuxing to {}", container.label()),
        (false, None) if container_metadata_changed => "Updating container metadata".to_string(),
        (false, None) => "Remuxing media".to_string(),
    }
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
                        source_path: path.clone(),
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
    container_metadata: Option<&'a ContainerMetadata>,
}

#[derive(Clone, Copy)]
struct TrackEdits<'a> {
    stream_order: &'a [u64],
    deleted_streams: &'a BTreeSet<u64>,
    default_streams: &'a BTreeSet<u64>,
    default_sidecars: &'a BTreeSet<usize>,
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
    match probe_file(path) {
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
    let output_kinds = output
        .streams
        .iter()
        .filter_map(stream_kind)
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
    for (position, stream) in output.streams.iter().enumerate() {
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
    for (position, stream) in output.streams.iter().enumerate() {
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
                let source_stream = plan
                    .source_info
                    .streams
                    .iter()
                    .find(|stream| stream_index(stream) == Some(*source_index));
                let metadata_change = plan.subtitle_changes.iter().find(|change| {
                    change.source == SubtitleSource::Embedded(*source_index)
                        && change.metadata.is_some()
                });
                if let Some(stream) = source_stream
                    && stream_kind(stream) == Some("subtitle")
                {
                    command
                        .arg(format!("-metadata:s:{output_index}"))
                        .arg(format!("language={}", stream_language(stream)));
                }
                if replacement || metadata_change.is_some() {
                    let Some(stream) = source_stream else {
                        continue;
                    };
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
    let parent = if crate::mount::is_network_mount(path) {
        let scratch = std::env::temp_dir().join("reel-tui-scratch");
        let _ = fs::create_dir_all(&scratch);
        scratch
    } else {
        path.parent()
            .ok_or_else(|| "The source file has no parent directory.".to_string())?
            .to_path_buf()
    };
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
    let parent = if crate::mount::is_network_mount(path) {
        let scratch = std::env::temp_dir().join("reel-tui-scratch");
        let _ = fs::create_dir_all(&scratch);
        scratch
    } else {
        path.parent()
            .ok_or_else(|| "The source file has no parent directory.".to_string())?
            .to_path_buf()
    };
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
    use std::process::Stdio;

    fn media(streams: Value) -> MediaInfo {
        MediaInfo::from_json(serde_json::json!({"streams": streams})).unwrap()
    }

    /// Reports a tool-gated test as skipped rather than letting it pass silently, so a
    /// green run on a machine without `ffmpeg` is not mistaken for a run that actually
    /// exercised the pipeline. Each entry is a program name, optionally narrowed to one
    /// encoder as `"ffmpeg:libx264"`.
    fn require_tools(test: &str, tools: &[&str]) -> bool {
        for tool in tools {
            let (program, encoder) = match tool.split_once(':') {
                Some((program, encoder)) => (program, Some(encoder)),
                None => (*tool, None),
            };
            let succeeds = |arguments: &[&str]| {
                Command::new(program)
                    .args(arguments)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .is_ok_and(|status| status.success())
            };
            // ffmpeg spells it `-version`, most other tools `--version`.
            let available = match encoder {
                Some(encoder) => succeeds(&["-v", "error", "-h", &format!("encoder={encoder}")]),
                None => succeeds(&["-version"]) || succeeds(&["--version"]),
            };
            if !available {
                eprintln!("SKIPPED {test}: {tool} is not available");
                return false;
            }
        }
        true
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
            },
        )]);

        assert_that!(media_write_label(None, &BTreeMap::new(), false))
            .is_equal_to("Remuxing media".to_string());
        assert_that!(media_write_label(None, &BTreeMap::new(), true))
            .is_equal_to("Updating container metadata".to_string());
        assert_that!(media_write_label(
            Some(ContainerFormat::Mp4),
            &BTreeMap::new(),
            false
        ))
        .is_equal_to("Remuxing to MP4".to_string());
        assert_that!(media_write_label(None, &settings, false))
            .is_equal_to("Encoding video".to_string());
        assert_that!(media_write_label(
            Some(ContainerFormat::Mp4),
            &settings,
            false
        ))
        .is_equal_to("Encoding video and remuxing to MP4".to_string());
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
            },
        )]);
        let subtitle_changes = [SubtitleChange {
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
                },
            )])
        };

        // Act
        let short = validate_edit(
            &info,
            &[0],
            &BTreeSet::new(),
            &BTreeSet::new(),
            &settings(1920, 10),
        );
        let narrow = validate_edit(
            &info,
            &[0],
            &BTreeSet::new(),
            &BTreeSet::new(),
            &settings(10, 1080),
        );
        let smallest_allowed = validate_edit(
            &info,
            &[0],
            &BTreeSet::new(),
            &BTreeSet::new(),
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
        if !require_tools(
            "apply_edits_should_remux_order_defaults_and_deletions_when_source_contains_multiple_tracks",
            &["ffmpeg"],
        ) {
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
                container_metadata: None,
            },
            TrackEdits {
                stream_order: &[1, 0, 3],
                deleted_streams: &BTreeSet::from([2]),
                default_streams: &BTreeSet::from([1, 3]),
                default_sidecars: &BTreeSet::new(),
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
    fn apply_edits_should_create_a_valid_mp4_copy_when_only_the_container_changes() {
        // Arrange
        if !require_tools(
            "apply_edits_should_create_a_valid_mp4_copy_when_only_the_container_changes",
            &["ffmpeg:aac"],
        ) {
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
                container_metadata: None,
            },
            TrackEdits {
                stream_order: &[0, 1],
                deleted_streams: &BTreeSet::new(),
                default_streams: &BTreeSet::from([0, 1]),
                default_sidecars: &BTreeSet::new(),
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
        if !require_tools(
            "apply_edits_should_treat_a_stream_order_mismatch_as_stale_rather_than_a_hard_failure",
            &["ffmpeg"],
        ) {
            return;
        }
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
        if !require_tools(
            "apply_edits_should_keep_a_genuinely_invalid_edit_as_a_hard_failure",
            &["ffmpeg"],
        ) {
            return;
        }
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
        if !require_tools(
            "apply_edits_should_write_genuine_matroska_when_a_mkv_file_is_misnamed_with_an_mp4_extension",
            &["ffmpeg"],
        ) {
            return;
        }
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
        if !require_tools(
            "apply_edits_should_place_moov_before_mdat_when_converting_to_mov",
            &["ffmpeg:aac"],
        ) {
            return;
        }
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
        if !require_tools(
            "apply_edits_should_keep_captions_and_hearing_impaired_independent_in_mp4",
            &["ffmpeg:mov_text"],
        ) {
            return;
        }
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
                video_settings: &BTreeMap::new(),
                subtitle_changes: &[SubtitleChange {
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
        if !require_tools(
            "apply_edits_should_create_an_edited_copy_without_changing_the_source",
            &["ffmpeg:libx264"],
        ) {
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
                container_metadata: None,
            },
            TrackEdits {
                stream_order: &[0, 1],
                deleted_streams: &BTreeSet::new(),
                default_streams: &BTreeSet::new(),
                default_sidecars: &BTreeSet::new(),
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
        if !require_tools(
            "apply_edits_should_honor_each_custom_scaling_mode",
            &["ffmpeg:libx264"],
        ) {
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
                    container_metadata: None,
                },
                TrackEdits {
                    stream_order: &[0],
                    deleted_streams: &BTreeSet::new(),
                    default_streams: &BTreeSet::new(),
                    default_sidecars: &BTreeSet::new(),
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
        if !require_tools(
            "apply_edits_should_export_converted_subtitle_without_retaining_an_embedded_copy",
            &["ffmpeg"],
        ) {
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
        if !require_tools(
            "apply_edits_should_export_vobsub_with_flags_and_remove_the_embedded_subtitle",
            &["ffmpeg", "seconv"],
        ) {
            return;
        }

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

    #[test]
    fn apply_edits_should_export_and_remove_incompatible_subtitle_during_mp4_conversion() {
        // Arrange
        if !require_tools(
            "apply_edits_should_export_and_remove_incompatible_subtitle_during_mp4_conversion",
            &["ffmpeg"],
        ) {
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
        if !require_tools(
            "apply_edits_should_default_the_one_surviving_converted_subtitle_after_bulk_deletion",
            &["ffmpeg:mov_text"],
        ) {
            return;
        }
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
        if !require_tools(
            "apply_edits_should_accept_the_default_flag_mp4_forces_onto_a_lone_subtitle",
            &["ffmpeg:mov_text"],
        ) {
            return;
        }
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
        if !require_tools(
            "apply_edits_should_import_sidecars_after_embedded_subtitles_and_remove_the_sources",
            &["ffmpeg"],
        ) {
            return;
        }
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
        if !require_tools(
            "failed_or_cancelled_sidecar_import_should_leave_media_and_sidecar_untouched",
            &["ffmpeg"],
        ) {
            return;
        }
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
        if !require_tools(
            "apply_edits_should_drop_source_number_when_converted_target_has_no_duplicate",
            &["ffmpeg"],
        ) {
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
            hearing_impaired: false,
            number: Some(2),
            fingerprint: FileFingerprint::for_path(&sidecar_path).unwrap(),
            companion_fingerprint: None,
        };
        let changes = [SubtitleChange {
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
        if !require_tools(
            "apply_edits_should_number_converted_sidecar_when_matching_target_already_exists",
            &["ffmpeg"],
        ) {
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
        if !require_tools(
            "apply_edits_should_write_container_metadata_and_leave_the_tracks_alone",
            &["ffmpeg"],
        ) {
            return;
        }
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
            video_settings: BTreeMap::new(),
            subtitle_changes: Vec::new(),
            left_subtitle_order: Vec::new(),
            sidecars: Vec::new(),
            cancelled,
        }
    }

    #[test]
    fn plan_requires_transcode_should_detect_only_a_genuine_codec_or_resolution_change() {
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264"},
            {"index": 1, "codec_type": "audio", "codec_name": "aac"}
        ]));

        // No video_settings entry at all: nothing to transcode.
        assert_that!(plan_requires_transcode(&info, &[0, 1], &BTreeMap::new())).is_false();

        // Settings present but requesting the source's own codec at original
        // resolution: still just a copy, not a transcode.
        let no_op_settings = BTreeMap::from([(
            0,
            VideoSettings {
                codec: VideoCodec::H264,
                resolution: VideoResolution::Original,
            },
        )]);
        assert_that!(plan_requires_transcode(&info, &[0, 1], &no_op_settings)).is_false();

        // A genuine codec change on the video stream: this is a real transcode.
        let transcode_settings = BTreeMap::from([(
            0,
            VideoSettings {
                codec: VideoCodec::Hevc,
                resolution: VideoResolution::Original,
            },
        )]);
        assert_that!(plan_requires_transcode(&info, &[0, 1], &transcode_settings)).is_true();

        // Settings keyed to the AUDIO stream's index must never count, even with a
        // "transcode-shaped" value — only video streams can trigger a transcode.
        let audio_keyed_settings = BTreeMap::from([(
            1,
            VideoSettings {
                codec: VideoCodec::Hevc,
                resolution: VideoResolution::Original,
            },
        )]);
        assert_that!(plan_requires_transcode(
            &info,
            &[0, 1],
            &audio_keyed_settings
        ))
        .is_false();
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
            },
        );
        request.subtitle_changes.push(SubtitleChange {
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
    fn metadata_only_sidecar_change_should_stage_a_collision_safe_filename_rename() {
        if !require_tools(
            "metadata_only_sidecar_change_should_stage_a_collision_safe_filename_rename",
            &["ffprobe"],
        ) {
            return;
        }
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
        if !require_tools(
            "apply_edits_should_write_and_validate_embedded_subtitle_metadata",
            &["ffmpeg"],
        ) {
            return;
        }
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
