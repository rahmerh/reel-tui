use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, Sender},
    },
    time::{Duration, Instant},
};

use anyhow::Result;
use ratatui::widgets::ListState;

use crate::{
    edit::{
        ContainerFormat, CustomResolution, CustomScaling, EditEvent, EditOutcome, EditRequest,
        SaveDestination, VideoCodec, VideoResolution, VideoSettings, container_conflict_streams,
        container_conflicts, imported_subtitle_conflicts, stream_index,
        subtitle_metadata_conflicts, validate_edit,
    },
    files::{DirectorySnapshot, FileEntry, scan_directory},
    probe::{MediaInfo, ProbeOutcome, ProbeRequest, ProbeResponse},
    subtitle::{
        FormatChoice, LanguageChoice, SidecarEntry, SubtitleChange, SubtitleFlag, SubtitleFormat,
        SubtitleMetadata, SubtitleSource, ToolCapabilities, canonical_language_code,
        common_language_choices, language_choice, partition_sidecars, path_extension, stream_cc,
        stream_commentary, stream_forced, stream_hearing_impaired, stream_language,
        stream_original, stream_title,
    },
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Layer {
    #[default]
    Files,
    Streams,
    StreamDetails,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TextInputState {
    pub value: String,
    pub cursor: usize,
    pub view_offset: usize,
    pub is_active: bool,
}

impl TextInputState {
    pub fn new(value: String) -> Self {
        let cursor = value.chars().count();
        Self {
            value,
            cursor,
            view_offset: 0,
            is_active: false,
        }
    }

    pub fn activate(&mut self) {
        self.is_active = true;
        self.cursor = self.cursor.min(self.value.chars().count());
    }

    pub fn deactivate(&mut self) {
        self.is_active = false;
    }

    pub fn clear(&mut self) {
        self.value.clear();
        self.cursor = 0;
        self.view_offset = 0;
        self.is_active = false;
    }

    pub fn insert(&mut self, character: char, maximum: usize, accepts: impl Fn(char) -> bool) {
        if !self.is_active || !accepts(character) || self.value.chars().count() >= maximum {
            return;
        }
        let byte = char_byte_index(&self.value, self.cursor);
        self.value.insert(byte, character);
        self.cursor += 1;
    }

    pub fn backspace(&mut self) {
        if !self.is_active || self.cursor == 0 {
            return;
        }
        let start = char_byte_index(&self.value, self.cursor - 1);
        let end = char_byte_index(&self.value, self.cursor);
        self.value.replace_range(start..end, "");
        self.cursor -= 1;
    }

    pub fn delete(&mut self) {
        if !self.is_active || self.cursor >= self.value.chars().count() {
            return;
        }
        let start = char_byte_index(&self.value, self.cursor);
        let end = char_byte_index(&self.value, self.cursor + 1);
        self.value.replace_range(start..end, "");
    }

    pub fn move_cursor(&mut self, direction: isize) {
        if !self.is_active {
            return;
        }
        let count = self.value.chars().count();
        self.cursor = if direction.is_negative() {
            self.cursor.saturating_sub(1)
        } else {
            self.cursor.saturating_add(1).min(count)
        };
    }

    pub fn move_home(&mut self, end: bool) {
        if self.is_active {
            self.cursor = if end { self.value.chars().count() } else { 0 };
        }
    }

    pub fn keep_cursor_visible(&mut self, visible_width: usize) {
        if self.cursor < self.view_offset {
            self.view_offset = self.cursor;
        } else if self.cursor >= self.view_offset + visible_width {
            self.view_offset = self.cursor.saturating_sub(visible_width.saturating_sub(1));
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SearchState {
    pub input: TextInputState,
    pub match_count: usize,
}

impl Deref for SearchState {
    type Target = TextInputState;

    fn deref(&self) -> &Self::Target {
        &self.input
    }
}

impl DerefMut for SearchState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.input
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilePanelEntry {
    pub file_index: usize,
    pub sidecar_indices: Vec<usize>,
}

impl SearchState {
    pub fn activate(&mut self) {
        self.input.activate();
    }

    pub fn deactivate(&mut self) {
        self.input.deactivate();
    }

    pub fn clear(&mut self) {
        self.input.clear();
        self.match_count = 0;
    }

    pub fn push_char(&mut self, c: char) {
        self.input
            .insert(c, 256, |character| !character.is_control());
    }

    pub fn pop_char(&mut self) {
        if self.input.value.is_empty() {
            self.input.deactivate();
        } else {
            self.input.backspace();
        }
    }

    pub fn delete_char(&mut self) {
        self.input.delete();
    }

    pub fn move_cursor(&mut self, direction: isize) {
        self.input.move_cursor(direction);
    }

    pub fn move_home(&mut self, end: bool) {
        self.input.move_home(end);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Dialog {
    Keybindings,
    ContainerSettings,
    VideoSettings,
    SubtitleSettings,
    ConfirmSave,
    Processing,
    ConfirmCancel,
    Error,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CancelEditChoice {
    #[default]
    KeepProcessing,
    CancelProcessing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrackRef {
    Container,
    Embedded(u64),
    Sidecar(usize),
}

#[derive(Clone, Debug)]
pub struct SubtitleSettingsPopup {
    pub source: SubtitleSource,
    pub source_format: SubtitleFormat,
    pub field: SubtitleSettingsField,
    pub mode: SubtitleSettingsMode,
    pub help_visible: bool,
    pub codec_cursor: usize,
    pub language_cursor: usize,
    pub language_search: SearchState,
    pub title_input: TextInputState,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SubtitleSettingsField {
    #[default]
    Codec,
    Language,
    Title,
    Default,
    Forced,
    Cc,
    HearingImpaired,
    Original,
    Commentary,
}

impl SubtitleSettingsField {
    pub const ALL: [Self; 9] = [
        Self::Codec,
        Self::Language,
        Self::Title,
        Self::Default,
        Self::Forced,
        Self::Cc,
        Self::HearingImpaired,
        Self::Original,
        Self::Commentary,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Codec => "Codec",
            Self::Language => "Language",
            Self::Title => "Title",
            Self::Default => "Default",
            Self::Forced => "Forced",
            Self::Cc => "CC",
            Self::HearingImpaired => "Hearing impaired",
            Self::Original => "Original",
            Self::Commentary => "Commentary",
        }
    }

    fn requires_embedded_subtitle(self) -> bool {
        matches!(
            self,
            Self::Title | Self::Default | Self::Original | Self::Commentary
        )
    }

    pub fn subtitle_flag(self) -> Option<SubtitleFlag> {
        match self {
            Self::Forced => Some(SubtitleFlag::Forced),
            Self::Cc => Some(SubtitleFlag::Cc),
            Self::HearingImpaired => Some(SubtitleFlag::HearingImpaired),
            Self::Original => Some(SubtitleFlag::Original),
            Self::Commentary => Some(SubtitleFlag::Commentary),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubtitleDisplayState {
    pub format: SubtitleFormat,
    pub metadata: SubtitleMetadata,
    pub default: bool,
    pub external: bool,
    container: Option<ContainerFormat>,
    source_format: SubtitleFormat,
    original_metadata: SubtitleMetadata,
    original_default: bool,
}

impl SubtitleDisplayState {
    pub fn original_metadata(&self) -> &SubtitleMetadata {
        &self.original_metadata
    }

    pub fn field_visible(&self, field: SubtitleSettingsField) -> bool {
        if self.external {
            return field != SubtitleSettingsField::Cc && !field.requires_embedded_subtitle();
        }
        field.subtitle_flag().is_none_or(|flag| {
            self.container
                .is_some_and(|container| container.supports_subtitle_flag(flag))
        })
    }

    pub fn field_changed(&self, field: SubtitleSettingsField) -> bool {
        match field {
            SubtitleSettingsField::Codec => self.format != self.source_format,
            SubtitleSettingsField::Language => {
                self.metadata.language != self.original_metadata.language
            }
            SubtitleSettingsField::Title => self.metadata.title != self.original_metadata.title,
            SubtitleSettingsField::Default => self.default != self.original_default,
            SubtitleSettingsField::Forced => self.metadata.forced != self.original_metadata.forced,
            SubtitleSettingsField::Cc => self.metadata.cc != self.original_metadata.cc,
            SubtitleSettingsField::HearingImpaired => {
                self.metadata.hearing_impaired != self.original_metadata.hearing_impaired
            }
            SubtitleSettingsField::Original => {
                self.metadata.original != self.original_metadata.original
            }
            SubtitleSettingsField::Commentary => {
                self.metadata.commentary != self.original_metadata.commentary
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SubtitleSettingsMode {
    #[default]
    Summary,
    CodecDropdown,
    LanguageDropdown,
    TitleEdit,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum VideoSettingsField {
    #[default]
    Codec,
    Resolution,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SaveDialogField {
    Destination,
    #[default]
    Start,
}

#[derive(Clone, Debug)]
pub struct VideoSettingsPopup {
    pub stream_index: u64,
    pub field: VideoSettingsField,
    pub mode: VideoSettingsMode,
    pub codec_cursor: usize,
    pub resolution_cursor: usize,
    pub custom_resolution: Option<CustomResolutionDraft>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum VideoSettingsMode {
    #[default]
    Summary,
    Dropdown,
    CustomResolution,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CustomResolutionField {
    #[default]
    Width,
    Height,
    Scaling,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CustomResolutionDraft {
    pub width: TextInputState,
    pub height: TextInputState,
    pub scaling: CustomScaling,
    pub field: CustomResolutionField,
    pub scaling_cursor: usize,
    pub scaling_dropdown_open: bool,
}

#[derive(Clone, Debug)]
pub struct ContainerSettingsPopup {
    pub cursor: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContainerChoice {
    pub value: Option<ContainerFormat>,
    pub label: String,
    pub current: bool,
    pub staged: bool,
    pub conflicts: Vec<String>,
}

impl ContainerChoice {
    pub fn warning(&self) -> Option<String> {
        if self.conflicts.is_empty() {
            return None;
        }
        if self
            .conflicts
            .iter()
            .any(|conflict| conflict.contains("FFmpeg is not available"))
        {
            return Some("Can't convert: FFmpeg is not available.".to_string());
        }
        if self
            .conflicts
            .iter()
            .any(|conflict| conflict.contains("muxer"))
        {
            return Some("Can't convert: FFmpeg does not support this container.".to_string());
        }

        let incompatibilities = [
            ("video", "video"),
            ("audio", "audio"),
            ("subtitle", "subtitles"),
            ("attachment", "attachments"),
        ]
        .into_iter()
        .filter_map(|(kind, label)| {
            let prefix = format!("{} can't contain ", self.label);
            let marker = format!(" {kind} track #");
            let mut codecs = Vec::new();
            for conflict in &self.conflicts {
                let Some(codec) = conflict
                    .strip_prefix(&prefix)
                    .and_then(|details| details.split_once(&marker))
                    .map(|(codec, _)| warning_codec_label(codec))
                else {
                    continue;
                };
                if !codecs.contains(&codec) {
                    codecs.push(codec);
                }
            }
            (!codecs.is_empty()).then(|| format!("{} {label}", codecs.join(" or ")))
        })
        .collect::<Vec<_>>();
        if incompatibilities.is_empty() {
            Some(format!("{} can't contain one or more tracks.", self.label))
        } else {
            Some(format!(
                "{} can't contain {}.",
                self.label,
                incompatibilities.join(" or ")
            ))
        }
    }
}

fn warning_codec_label(codec: &str) -> String {
    match codec.to_ascii_lowercase().as_str() {
        "subrip" => "SubRip/SRT".to_string(),
        "ass" | "ssa" => "ASS".to_string(),
        "webvtt" => "WebVTT".to_string(),
        "mov_text" => "MOV Text".to_string(),
        "hdmv_pgs_subtitle" => "PGS".to_string(),
        "dvd_subtitle" => "VobSub".to_string(),
        "h264" => "H.264".to_string(),
        "hevc" => "HEVC/H.265".to_string(),
        "av1" => "AV1".to_string(),
        "vp8" => "VP8".to_string(),
        "vp9" => "VP9".to_string(),
        other => other.to_ascii_uppercase(),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolutionChoice {
    pub value: ResolutionChoiceValue,
    pub label: String,
    pub enabled: bool,
    pub current: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResolutionChoiceValue {
    Resolution(VideoResolution),
    Custom,
}

impl ResolutionChoice {
    pub fn selected(&self, resolution: VideoResolution) -> bool {
        match (self.value, resolution) {
            (_, VideoResolution::Original) => self.current,
            (ResolutionChoiceValue::Resolution(choice_resolution), selected_resolution) => {
                choice_resolution == selected_resolution
            }
            (ResolutionChoiceValue::Custom, VideoResolution::Custom(_)) => true,
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoCodecChoice {
    pub value: VideoCodec,
    pub label: String,
    pub current: bool,
    pub enabled: bool,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct CacheKey {
    path: PathBuf,
    length: u64,
    modified: Option<std::time::SystemTime>,
}

impl CacheKey {
    fn for_file(file: &FileEntry) -> Self {
        Self {
            path: file.path.clone(),
            length: file.fingerprint.length,
            modified: file.fingerprint.modified,
        }
    }

    fn matches_file(&self, file: &FileEntry) -> bool {
        self.path == file.path
            && self.length == file.fingerprint.length
            && self.modified == file.fingerprint.modified
    }
}

pub struct App {
    pub directory: PathBuf,
    pub files: Vec<FileEntry>,
    pub list_state: ListState,
    pub outcome: Option<ProbeOutcome>,
    pub loading: bool,
    pub details_scroll: u16,
    pub details_max_scroll: u16,
    pub keybindings_scroll: u16,
    pub keybindings_max_scroll: u16,
    pub layer: Layer,
    pub selected_stream: usize,
    pub stream_order: Vec<u64>,
    moved_streams: BTreeSet<u64>,
    pub deleted_streams: BTreeSet<u64>,
    pub default_streams: BTreeSet<u64>,
    pub default_sidecars: BTreeSet<usize>,
    pub video_settings: BTreeMap<u64, VideoSettings>,
    pub video_settings_popup: Option<VideoSettingsPopup>,
    pub sidecars: Vec<SidecarEntry>,
    pub subtitle_columns_side_by_side: bool,
    pub subtitle_changes: BTreeMap<SubtitleSource, SubtitleChange>,
    pub left_subtitle_order: Vec<TrackRef>,
    pub subtitle_settings_popup: Option<SubtitleSettingsPopup>,
    pub subtitle_capabilities: ToolCapabilities,
    pub container_target: Option<ContainerFormat>,
    pub container_settings_popup: Option<ContainerSettingsPopup>,
    pub save_destination: SaveDestination,
    pub save_dialog_field: SaveDialogField,
    pub dialog: Option<Dialog>,
    pub notice: Option<String>,
    pub edit_error: Option<String>,
    pub edit_progress: Option<f64>,
    pub edit_progress_label: Option<String>,
    pub edit_started: Option<Instant>,
    pub cancel_edit_choice: CancelEditChoice,
    pub scan_error: Option<String>,
    request_tx: Sender<ProbeRequest>,
    edit_tx: Sender<EditRequest>,
    edit_cancel: Option<Arc<AtomicBool>>,
    generation: u64,
    pending_since: Option<Instant>,
    cache: HashMap<CacheKey, ProbeOutcome>,
    original_stream_order: Vec<u64>,
    original_default_streams: BTreeSet<u64>,
    sidecars_by_media: HashMap<PathBuf, Vec<SidecarEntry>>,
    unfolded_files: BTreeSet<PathBuf>,
    pub is_network_mount: bool,
    pub disk_cache: crate::cache::DiskCache,
    pub keybindings_search: SearchState,
    pub file_search: SearchState,
    file_search_origin: Option<PathBuf>,
}

impl App {
    pub fn new(
        directory: PathBuf,
        request_tx: Sender<ProbeRequest>,
        edit_tx: Sender<EditRequest>,
    ) -> Result<Self> {
        let is_network_mount = crate::mount::is_network_mount(&directory);
        let disk_cache = crate::cache::DiskCache::load();
        let mut in_memory_cache = HashMap::new();
        for (path, entry) in &disk_cache.entries {
            let key = CacheKey {
                path: path.clone(),
                length: entry.length,
                modified: entry.modified,
            };
            in_memory_cache.insert(key, entry.outcome.clone());
        }

        let mut app = Self {
            directory,
            files: Vec::new(),
            list_state: ListState::default(),
            outcome: None,
            loading: false,
            details_scroll: 0,
            details_max_scroll: 0,
            keybindings_scroll: 0,
            keybindings_max_scroll: 0,
            layer: Layer::Files,
            selected_stream: 0,
            stream_order: Vec::new(),
            moved_streams: BTreeSet::new(),
            deleted_streams: BTreeSet::new(),
            default_streams: BTreeSet::new(),
            default_sidecars: BTreeSet::new(),
            video_settings: BTreeMap::new(),
            video_settings_popup: None,
            sidecars: Vec::new(),
            subtitle_columns_side_by_side: false,
            subtitle_changes: BTreeMap::new(),
            left_subtitle_order: Vec::new(),
            subtitle_settings_popup: None,
            subtitle_capabilities: ToolCapabilities::detect_cached(),
            container_target: None,
            container_settings_popup: None,
            save_destination: SaveDestination::ReplaceOriginal,
            save_dialog_field: SaveDialogField::Start,
            dialog: None,
            notice: None,
            edit_error: None,
            edit_progress: None,
            edit_progress_label: None,
            edit_started: None,
            cancel_edit_choice: CancelEditChoice::KeepProcessing,
            scan_error: None,
            request_tx,
            edit_tx,
            edit_cancel: None,
            generation: 0,
            pending_since: None,
            cache: in_memory_cache,
            original_stream_order: Vec::new(),
            original_default_streams: BTreeSet::new(),
            sidecars_by_media: HashMap::new(),
            unfolded_files: BTreeSet::new(),
            is_network_mount,
            disk_cache,
            keybindings_search: SearchState::default(),
            file_search: SearchState::default(),
            file_search_origin: None,
        };
        let snapshot = match scan_directory(&app.directory) {
            Ok(files) => DirectorySnapshot::Files(files),
            Err(error) => DirectorySnapshot::Error(error.to_string()),
        };
        app.apply_directory_snapshot(snapshot);
        Ok(app)
    }

    pub fn receive_directory_snapshots(&mut self, receiver: &Receiver<DirectorySnapshot>) {
        while let Ok(snapshot) = receiver.try_recv() {
            self.apply_directory_snapshot(snapshot);
        }
    }

    fn apply_directory_snapshot(&mut self, snapshot: DirectorySnapshot) {
        match snapshot {
            DirectorySnapshot::Files(files) => {
                self.scan_error = None;
                self.reconcile_files(files);
            }
            DirectorySnapshot::Error(error) => {
                self.scan_error = Some(error);
                self.reconcile_files(Vec::new());
            }
        }
    }

    fn reconcile_files(&mut self, files: Vec<FileEntry>) {
        let (files, sidecars_by_media) = partition_sidecars(files);
        let old_selection = self.list_state.selected();
        let old_file = self.selected_file().cloned();
        let old_path = old_file.as_ref().map(|file| file.path.clone());
        let old_sidecars = old_path
            .as_ref()
            .and_then(|path| self.sidecars_by_media.get(path))
            .cloned()
            .unwrap_or_default();
        let source_position = old_path
            .as_ref()
            .and_then(|path| files.iter().position(|file| &file.path == path));
        let selected_changed = source_position
            .zip(old_file.as_ref())
            .is_some_and(|(index, old)| files[index].fingerprint != old.fingerprint);
        let selected_removed = old_path.is_some() && source_position.is_none();
        let was_processing = matches!(
            self.dialog,
            Some(Dialog::Processing | Dialog::ConfirmCancel)
        );
        if selected_removed && was_processing {
            return;
        }

        self.files = files;
        self.sidecars_by_media = sidecars_by_media;
        self.unfolded_files
            .retain(|path| self.files.iter().any(|file| &file.path == path));
        self.cache
            .retain(|key, _| self.files.iter().any(|file| key.matches_file(file)));

        if was_processing
            && source_position.is_some()
            && old_path
                .as_deref()
                .and_then(|path| self.file_panel_position(path))
                .is_none()
        {
            self.file_search.clear();
            self.file_search_origin = None;
        }

        if let Some(position) = old_path
            .as_deref()
            .and_then(|path| self.file_panel_position(path))
        {
            self.list_state.select(Some(position));
            self.sidecars = old_path
                .as_ref()
                .and_then(|path| self.sidecars_by_media.get(path))
                .cloned()
                .unwrap_or_default();
            let sidecars_changed = old_sidecars != self.sidecars;
            if selected_changed && !was_processing {
                self.clear_edit_state();
                self.notice = Some("Selected file changed; reloaded latest metadata.".to_string());
                self.queue_probe();
            } else if sidecars_changed && !was_processing {
                self.subtitle_changes.clear();
                self.subtitle_settings_popup = None;
                self.selected_stream = self
                    .selected_stream
                    .min(self.stream_count().saturating_sub(1));
                self.notice =
                    Some("Matching subtitle sidecars changed; reloaded them.".to_string());
            }
            return;
        }

        let result_count = self.file_panel_entries().len();
        let selection = (result_count > 0).then(|| {
            old_selection
                .unwrap_or(0)
                .min(result_count.saturating_sub(1))
        });
        self.list_state.select(selection);
        self.sidecars = self
            .selected_file()
            .and_then(|file| self.sidecars_by_media.get(&file.path))
            .cloned()
            .unwrap_or_default();

        if selected_removed {
            if let Some(cancelled) = self.edit_cancel.take() {
                cancelled.store(true, Ordering::Relaxed);
            }
            self.clear_edit_state();
            self.notice = Some(
                if was_processing {
                    "Selected file was removed; media edit cancelled."
                } else {
                    "Selected file was removed; returned to the file list."
                }
                .to_string(),
            );
            self.queue_probe();
        } else {
            self.queue_probe();
        }
    }

    pub fn selected_file(&self) -> Option<&FileEntry> {
        let position = self.list_state.selected()?;
        if !self.file_search_has_query() {
            return self.files.get(position);
        }
        self.file_panel_entries()
            .get(position)
            .and_then(|entry| self.files.get(entry.file_index))
    }

    pub fn file_panel_entries(&self) -> Vec<FilePanelEntry> {
        let query = self.file_search.value.trim().to_lowercase();
        self.files
            .iter()
            .enumerate()
            .filter_map(|(file_index, file)| {
                let sidecars = self.sidecars_for_media(&file.path);
                let sidecar_indices = if query.is_empty() {
                    (0..sidecars.len()).collect::<Vec<_>>()
                } else {
                    sidecars
                        .iter()
                        .enumerate()
                        .filter_map(|(index, sidecar)| {
                            sidecar
                                .display_name
                                .to_lowercase()
                                .contains(&query)
                                .then_some(index)
                        })
                        .collect::<Vec<_>>()
                };
                let file_matches =
                    query.is_empty() || file.display_name.to_lowercase().contains(&query);
                (file_matches || !sidecar_indices.is_empty()).then_some(FilePanelEntry {
                    file_index,
                    sidecar_indices,
                })
            })
            .collect()
    }

    pub fn file_search_has_query(&self) -> bool {
        !self.file_search.value.trim().is_empty()
    }

    fn file_panel_position(&self, path: &Path) -> Option<usize> {
        if !self.file_search_has_query() {
            return self.files.iter().position(|file| file.path == path);
        }
        self.file_panel_entries().iter().position(|entry| {
            self.files
                .get(entry.file_index)
                .is_some_and(|file| file.path == path)
        })
    }

    pub fn sidecars_for_media(&self, path: &Path) -> &[SidecarEntry] {
        self.sidecars_by_media
            .get(path)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub fn is_file_folded(&self, path: &Path) -> bool {
        !self.unfolded_files.contains(path)
    }

    pub fn toggle_fold_selected_file(&mut self) {
        if self.layer != Layer::Files {
            return;
        }
        if let Some(file) = self.selected_file() {
            let path = file.path.clone();
            if !self.unfolded_files.remove(&path) {
                self.unfolded_files.insert(path);
            }
        }
    }

    pub fn fold_selected_file(&mut self) {
        if self.layer != Layer::Files {
            return;
        }
        if let Some(path) = self.selected_file().map(|file| file.path.clone()) {
            self.unfolded_files.remove(&path);
        }
    }

    pub fn unfold_selected_file(&mut self) {
        if self.layer != Layer::Files {
            return;
        }
        if let Some(path) = self.selected_file().map(|file| file.path.clone()) {
            self.unfolded_files.insert(path);
        }
    }

    pub fn fold_all_files(&mut self) {
        if self.layer != Layer::Files {
            return;
        }
        self.unfolded_files.clear();
    }

    pub fn unfold_all_files(&mut self) {
        if self.layer != Layer::Files {
            return;
        }
        for file in &self.files {
            self.unfolded_files.insert(file.path.clone());
        }
    }

    pub fn select_next(&mut self) {
        self.notice = None;
        if self.layer == Layer::Streams {
            if self.move_within_subtitle_column(1, 1, false) {
                return;
            }
            let count = self.stream_count();
            if count > 0 {
                self.selected_stream = (self.selected_stream + 1).min(count - 1);
            }
            return;
        }
        if self.layer == Layer::StreamDetails {
            self.scroll_details_down(1);
            return;
        }
        let result_count = self.file_panel_entries().len();
        if result_count == 0 {
            return;
        }
        let next = self
            .list_state
            .selected()
            .map(|index| (index + 1).min(result_count - 1))
            .unwrap_or(0);
        self.select(next);
    }

    pub fn select_previous(&mut self) {
        self.notice = None;
        if self.layer == Layer::Streams {
            if self.move_within_subtitle_column(-1, 1, false) {
                return;
            }
            self.selected_stream = self.selected_stream.saturating_sub(1);
            return;
        }
        if self.layer == Layer::StreamDetails {
            self.scroll_details_up(1);
            return;
        }
        let previous = self
            .list_state
            .selected()
            .map(|index| index.saturating_sub(1))
            .unwrap_or(0);
        self.select(previous);
    }

    pub fn select_first(&mut self) {
        self.notice = None;
        if self.layer == Layer::StreamDetails {
            self.details_scroll = 0;
            return;
        }
        if self.layer == Layer::Streams {
            self.selected_stream = 0;
            return;
        }
        if !self.file_panel_entries().is_empty() {
            self.select(0);
        }
    }

    pub fn select_last(&mut self) {
        self.notice = None;
        if self.layer == Layer::StreamDetails {
            self.details_scroll = self.details_max_scroll;
            return;
        }
        if self.layer == Layer::Streams {
            if self.subtitle_columns_side_by_side {
                let rows = self.track_rows();
                let column = match self.selected_track() {
                    Some(TrackRef::Embedded(index))
                        if self
                            .media_info()
                            .and_then(|info| stream_by_index(info, index))
                            .is_some_and(|stream| stream_kind(stream) == Some("subtitle")) =>
                    {
                        self.embedded_subtitle_positions(&rows)
                    }
                    Some(TrackRef::Sidecar(_)) => self.sidecar_positions(&rows),
                    _ => self.embedded_subtitle_positions(&rows),
                };
                if let Some(last) = column.last() {
                    self.selected_stream = *last;
                    return;
                }
            }
            self.selected_stream = self.stream_count().saturating_sub(1);
            return;
        }
        let result_count = self.file_panel_entries().len();
        if result_count > 0 {
            self.select(result_count - 1);
        }
    }

    fn select(&mut self, index: usize) {
        self.select_file_position(Some(index));
    }

    fn select_file_position(&mut self, position: Option<usize>) {
        self.select_file_position_with_force(position, false);
    }

    fn select_file_position_with_force(&mut self, position: Option<usize>, force: bool) {
        if force || self.list_state.selected() != position {
            self.clear_edit_state();
            self.list_state.select(position);
            self.sidecars = self
                .selected_file()
                .and_then(|file| self.sidecars_by_media.get(&file.path))
                .cloned()
                .unwrap_or_default();
            self.queue_probe();
        }
    }

    fn queue_probe(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.details_scroll = 0;
        self.details_max_scroll = 0;
        self.layer = Layer::Files;
        self.selected_stream = 0;
        self.outcome = None;
        self.loading = self.selected_file().is_some();
        self.pending_since = self.loading.then(Instant::now);

        if let Some(key) = self.selected_file().map(CacheKey::for_file)
            && let Some(cached) = self.cache.get(&key)
        {
            self.outcome = Some(cached.clone());
            self.loading = false;
            self.pending_since = None;
            self.reset_track_edits();
        }
    }

    pub fn start_pending_probe(&mut self) {
        let Some(since) = self.pending_since else {
            return;
        };
        if since.elapsed() < Duration::from_millis(120) {
            return;
        }
        let Some(file) = self.selected_file() else {
            self.pending_since = None;
            return;
        };

        let _ = self.request_tx.send(ProbeRequest {
            generation: self.generation,
            path: file.path.clone(),
            fingerprint: file.fingerprint,
        });
        self.pending_since = None;
    }

    pub fn receive_probe_results(&mut self, receiver: &Receiver<ProbeResponse>) {
        while let Ok(response) = receiver.try_recv() {
            let key = CacheKey {
                path: response.path.clone(),
                length: response.fingerprint.length,
                modified: response.fingerprint.modified,
            };
            if self.files.iter().any(|file| key.matches_file(file)) {
                self.cache.insert(key, response.outcome.clone());
                self.disk_cache.insert(
                    response.path.clone(),
                    response.fingerprint.length,
                    response.fingerprint.modified,
                    response.outcome.clone(),
                );
                let _ = self.disk_cache.save();
            }
            if response.generation == self.generation
                && self.selected_file().is_some_and(|file| {
                    file.path == response.path && file.fingerprint == response.fingerprint
                })
            {
                self.outcome = Some(response.outcome);
                self.loading = false;
                self.selected_stream = 0;
                self.reset_track_edits();
            }
        }
    }

    pub fn receive_edit_results(&mut self, receiver: &Receiver<EditEvent>) {
        while let Ok(event) = receiver.try_recv() {
            if !matches!(
                self.dialog,
                Some(Dialog::Processing | Dialog::ConfirmCancel)
            ) {
                continue;
            }
            match event {
                EditEvent::Progress(progress) => {
                    self.edit_progress = progress.fraction;
                    self.edit_progress_label = Some(progress.label());
                }
                EditEvent::Finished { path, outcome } => match outcome {
                    EditOutcome::Completed {
                        output_path,
                        media_changed,
                    } => {
                        self.edit_cancel = None;
                        self.dialog = None;
                        if let Ok(files) = scan_directory(&self.directory) {
                            self.reconcile_files(files);
                        }
                        if self.files.iter().any(|file| file.path == output_path)
                            && self.file_panel_position(&output_path).is_none()
                        {
                            self.file_search.clear();
                            self.file_search_origin = None;
                        }
                        if let Some(position) = self.file_panel_position(&output_path) {
                            self.list_state.select(Some(position));
                            self.sidecars = self
                                .selected_file()
                                .and_then(|file| self.sidecars_by_media.get(&file.path))
                                .cloned()
                                .unwrap_or_default();
                        }
                        self.clear_track_edits();
                        self.edit_error = None;
                        self.edit_progress = None;
                        self.edit_progress_label = None;
                        self.edit_started = None;
                        self.cancel_edit_choice = CancelEditChoice::KeepProcessing;
                        self.notice = Some(if !media_changed {
                            "Subtitle changes saved.".to_string()
                        } else if output_path == path {
                            "Media edits saved.".to_string()
                        } else {
                            format!(
                                "Media edits saved to {}.",
                                output_path
                                    .file_name()
                                    .and_then(|name| name.to_str())
                                    .unwrap_or("edited copy")
                            )
                        });
                        self.cache
                            .retain(|key, _| key.path != path && key.path != output_path);
                        self.queue_probe();
                        self.layer = Layer::Streams;
                    }
                    EditOutcome::Cancelled => {
                        self.edit_cancel = None;
                        self.dialog = None;
                        self.edit_progress = None;
                        self.edit_progress_label = None;
                        self.edit_started = None;
                        self.cancel_edit_choice = CancelEditChoice::KeepProcessing;
                        self.notice = Some("Media edit cancelled.".to_string());
                        self.layer = Layer::Streams;
                    }
                    EditOutcome::SourceChanged(error) => {
                        self.edit_cancel = None;
                        self.dialog = None;
                        self.cancel_edit_choice = CancelEditChoice::KeepProcessing;
                        let snapshot = match scan_directory(&self.directory) {
                            Ok(files) => DirectorySnapshot::Files(files),
                            Err(error) => DirectorySnapshot::Error(error.to_string()),
                        };
                        self.apply_directory_snapshot(snapshot);
                        self.clear_edit_state();
                        self.notice = Some(error);
                        self.queue_probe();
                    }
                    EditOutcome::Failed(error) => {
                        self.edit_cancel = None;
                        self.cancel_edit_choice = CancelEditChoice::KeepProcessing;
                        self.dialog = Some(Dialog::Error);
                        self.edit_error = Some(error);
                        self.edit_progress = None;
                        self.edit_progress_label = None;
                        self.edit_started = None;
                    }
                },
            }
        }
    }

    pub fn enter(&mut self) {
        if self.dialog.is_some() {
            return;
        }
        if self.layer == Layer::Files && self.stream_count() > 0 {
            self.layer = Layer::Streams;
            self.selected_stream = 0;
        }
    }

    pub fn open_stream_details(&mut self) {
        let details_available = match self.selected_track() {
            Some(TrackRef::Container) => self.media_info().is_some(),
            Some(TrackRef::Embedded(_)) => self.selected_stream_info().is_some(),
            Some(TrackRef::Sidecar(index)) => self.sidecars.get(index).is_some(),
            None => false,
        };
        if self.dialog.is_none() && self.layer == Layer::Streams && details_available {
            self.layer = Layer::StreamDetails;
            self.details_scroll = 0;
            self.details_max_scroll = 0;
        }
    }

    pub fn back(&mut self) -> bool {
        match self.layer {
            Layer::StreamDetails => {
                self.layer = Layer::Streams;
                self.details_scroll = 0;
                self.details_max_scroll = 0;
                true
            }
            Layer::Streams => {
                self.layer = Layer::Files;
                true
            }
            Layer::Files => false,
        }
    }

    pub fn media_info(&self) -> Option<&MediaInfo> {
        match &self.outcome {
            Some(ProbeOutcome::Video(info)) => Some(info),
            _ => None,
        }
    }

    pub fn stream_count(&self) -> usize {
        if self.media_info().is_some() {
            self.track_rows().len()
        } else {
            0
        }
    }

    pub fn set_subtitle_columns_side_by_side(&mut self, side_by_side: bool) {
        self.subtitle_columns_side_by_side = side_by_side;
    }

    pub fn is_stream_exported(&self, index: u64) -> bool {
        self.subtitle_changes
            .get(&SubtitleSource::Embedded(index))
            .is_some_and(|c| c.export_target.is_some())
    }

    pub fn is_sidecar_imported(&self, sidecar_index: usize) -> bool {
        let Some(sidecar) = self.sidecars.get(sidecar_index) else {
            return false;
        };
        self.subtitle_changes
            .get(&SubtitleSource::Sidecar(sidecar.path.clone()))
            .is_some_and(|c| c.import_into_media)
    }

    pub fn move_subtitle_column(&mut self, direction: isize) -> bool {
        if self.layer != Layer::Streams
            || self.dialog.is_some()
            || !self.subtitle_columns_side_by_side
            || direction == 0
        {
            return false;
        }
        let rows = self.track_rows();
        let embedded = self.embedded_subtitle_positions(&rows);
        let external = self.sidecar_positions(&rows);
        if embedded.is_empty() || external.is_empty() {
            return false;
        }
        let (source, target) = match self.selected_track() {
            Some(TrackRef::Embedded(index))
                if self
                    .media_info()
                    .and_then(|info| stream_by_index(info, index))
                    .is_some_and(|stream| stream_kind(stream) == Some("subtitle")) =>
            {
                if self.is_stream_exported(index) {
                    if direction.is_negative() {
                        (&external, &embedded)
                    } else {
                        return false;
                    }
                } else if direction.is_positive() {
                    (&embedded, &external)
                } else {
                    return false;
                }
            }
            Some(TrackRef::Sidecar(sidecar_index)) => {
                if self.is_sidecar_imported(sidecar_index) {
                    if direction.is_positive() {
                        (&embedded, &external)
                    } else {
                        return false;
                    }
                } else if direction.is_negative() {
                    (&external, &embedded)
                } else {
                    return false;
                }
            }
            _ => return false,
        };
        let row = source
            .iter()
            .position(|position| *position == self.selected_stream)
            .unwrap_or(0);
        self.selected_stream = target[row.min(target.len() - 1)];
        self.notice = None;
        true
    }

    fn subtitle_source_format(&self, source: &SubtitleSource) -> Option<SubtitleFormat> {
        match source {
            SubtitleSource::Embedded(index) => self
                .media_info()
                .and_then(|info| stream_by_index(info, *index))
                .and_then(|stream| stream.get("codec_name"))
                .and_then(serde_json::Value::as_str)
                .and_then(SubtitleFormat::from_codec),
            SubtitleSource::Sidecar(path) => self
                .sidecars
                .iter()
                .find(|sidecar| &sidecar.path == path)
                .map(|sidecar| sidecar.format),
        }
    }

    pub fn transfer_subtitle(&mut self, direction: isize) -> bool {
        if self.layer != Layer::Streams || self.dialog.is_some() || direction == 0 {
            return false;
        }
        let track = match self.selected_track() {
            Some(track) => track,
            None => return false,
        };

        match track {
            TrackRef::Embedded(index) => {
                let Some(info) = self.media_info() else {
                    return false;
                };
                let Some(stream) = stream_by_index(info, index) else {
                    return false;
                };
                if stream_kind(stream) != Some("subtitle") {
                    return false;
                }
                let source = SubtitleSource::Embedded(index);
                let Some(source_format) = self.subtitle_source_format(&source) else {
                    self.notice = Some("This subtitle format is not supported yet.".to_string());
                    return false;
                };
                let mut change = self.subtitle_change(&source, source_format);

                if direction > 0 {
                    if change.export_target.is_some() {
                        return true;
                    }
                    let preferred = change.embedded_target.unwrap_or(source_format);
                    let choices = self.subtitle_export_choices(source_format);
                    let Some(choice) = choices
                        .iter()
                        .find(|choice| choice.format == preferred && choice.enabled)
                        .or_else(|| choices.iter().find(|choice| choice.enabled))
                    else {
                        let reason = choices
                            .iter()
                            .find(|choice| choice.format == preferred)
                            .and_then(|choice| choice.reason.as_deref())
                            .unwrap_or("the selected codec cannot be written as a sidecar");
                        self.notice =
                            Some(format!("Cannot export {}: {reason}.", preferred.label()));
                        return false;
                    };
                    change.embedded_target = None;
                    change.export_target = Some(choice.format);
                    self.store_subtitle_change(source, change);
                    self.default_streams.remove(&index);
                    self.notice = None;
                    true
                } else {
                    if change.export_target.is_none() {
                        return true;
                    }
                    change.export_target = None;
                    self.store_subtitle_change(source, change);
                    self.notice = None;
                    true
                }
            }
            TrackRef::Sidecar(sidecar_index) => {
                let Some(sidecar) = self.sidecars.get(sidecar_index) else {
                    return false;
                };
                let source = SubtitleSource::Sidecar(sidecar.path.clone());
                let source_format = sidecar.format;
                let mut change = self.subtitle_change(&source, source_format);

                if direction < 0 {
                    if change.import_into_media {
                        return true;
                    }
                    change.import_into_media = true;
                    self.store_subtitle_change(source, change);
                    self.notice = None;
                    true
                } else {
                    if !change.import_into_media {
                        return true;
                    }
                    change.import_into_media = false;
                    self.default_sidecars.remove(&sidecar_index);
                    if let Some(metadata) = change.metadata.as_mut() {
                        metadata.title = None;
                        metadata.original = false;
                        metadata.commentary = false;
                    }
                    if let Some(target) = change.embedded_target {
                        let external_choices = self.subtitle_capabilities.format_choices(
                            source_format,
                            None,
                            true,
                            false,
                        );
                        if external_choices
                            .iter()
                            .find(|choice| choice.format == target)
                            .is_none_or(|choice| !choice.enabled)
                        {
                            change.embedded_target = None;
                        }
                    }
                    self.store_subtitle_change(source, change);
                    self.left_subtitle_order = self.active_left_subtitle_tracks();
                    self.notice = None;
                    true
                }
            }
            _ => false,
        }
    }

    pub fn active_left_subtitle_tracks(&self) -> Vec<TrackRef> {
        let Some(info) = self.media_info() else {
            return Vec::new();
        };
        let is_active_left = |track: &TrackRef| -> bool {
            match track {
                TrackRef::Embedded(index) => {
                    stream_by_index(info, *index)
                        .is_some_and(|s| stream_kind(s) == Some("subtitle"))
                        && !self.is_stream_exported(*index)
                }
                TrackRef::Sidecar(sidecar_index) => self.is_sidecar_imported(*sidecar_index),
                _ => false,
            }
        };

        let mut active = Vec::new();
        for track in &self.left_subtitle_order {
            if is_active_left(track) && !active.contains(track) {
                active.push(*track);
            }
        }
        for index in &self.stream_order {
            let track = TrackRef::Embedded(*index);
            if is_active_left(&track) && !active.contains(&track) {
                active.push(track);
            }
        }
        for sidecar_index in 0..self.sidecars.len() {
            let track = TrackRef::Sidecar(sidecar_index);
            if is_active_left(&track) && !active.contains(&track) {
                active.push(track);
            }
        }
        active
    }

    fn embedded_subtitle_positions(&self, rows: &[TrackRef]) -> Vec<usize> {
        let Some(info) = self.media_info() else {
            return Vec::new();
        };
        rows.iter()
            .enumerate()
            .filter_map(|(position, row)| match row {
                TrackRef::Embedded(index)
                    if stream_by_index(info, *index)
                        .is_some_and(|stream| stream_kind(stream) == Some("subtitle"))
                        && !self.is_stream_exported(*index) =>
                {
                    Some(position)
                }
                TrackRef::Sidecar(sidecar_index) if self.is_sidecar_imported(*sidecar_index) => {
                    Some(position)
                }
                _ => None,
            })
            .collect()
    }

    fn sidecar_positions(&self, rows: &[TrackRef]) -> Vec<usize> {
        let Some(info) = self.media_info() else {
            return Vec::new();
        };
        rows.iter()
            .enumerate()
            .filter_map(|(position, row)| match row {
                TrackRef::Sidecar(sidecar_index) if !self.is_sidecar_imported(*sidecar_index) => {
                    Some(position)
                }
                TrackRef::Embedded(index)
                    if stream_by_index(info, *index)
                        .is_some_and(|stream| stream_kind(stream) == Some("subtitle"))
                        && self.is_stream_exported(*index) =>
                {
                    Some(position)
                }
                _ => None,
            })
            .collect()
    }

    fn move_within_subtitle_column(
        &mut self,
        direction: isize,
        amount: usize,
        clamp_within_column: bool,
    ) -> bool {
        if self.layer != Layer::Streams || !self.subtitle_columns_side_by_side || direction == 0 {
            return false;
        }
        let rows = self.track_rows();
        let embedded = self.embedded_subtitle_positions(&rows);
        let external = self.sidecar_positions(&rows);
        let column = match self.selected_track() {
            Some(TrackRef::Embedded(index))
                if self
                    .media_info()
                    .and_then(|info| stream_by_index(info, index))
                    .is_some_and(|stream| stream_kind(stream) == Some("subtitle")) =>
            {
                if self.is_stream_exported(index) {
                    &external
                } else {
                    &embedded
                }
            }
            Some(TrackRef::Sidecar(sidecar_index)) => {
                if self.is_sidecar_imported(sidecar_index) {
                    &embedded
                } else {
                    &external
                }
            }
            _ => return false,
        };
        let Some(row) = column
            .iter()
            .position(|position| *position == self.selected_stream)
        else {
            return false;
        };
        let target_row = if direction.is_positive() {
            row.saturating_add(amount).min(column.len() - 1)
        } else {
            row.saturating_sub(amount)
        };
        if target_row != row || clamp_within_column {
            self.selected_stream = column[target_row];
            return true;
        }

        let first_subtitle = embedded
            .first()
            .into_iter()
            .chain(external.first())
            .copied()
            .min()
            .unwrap_or(self.selected_stream);
        let last_subtitle = embedded
            .last()
            .into_iter()
            .chain(external.last())
            .copied()
            .max()
            .unwrap_or(self.selected_stream);
        self.selected_stream = if direction.is_positive() {
            if last_subtitle + 1 < rows.len() {
                last_subtitle + 1
            } else {
                self.selected_stream
            }
        } else {
            first_subtitle.saturating_sub(1)
        };
        true
    }

    pub fn track_rows(&self) -> Vec<TrackRef> {
        let Some(info) = self.media_info() else {
            return Vec::new();
        };
        let mut rows = Vec::with_capacity(self.stream_order.len() + self.sidecars.len() + 1);
        rows.push(TrackRef::Container);
        for kind in ["video", "audio"] {
            rows.extend(self.stream_order.iter().filter_map(|index| {
                stream_by_index(info, *index)
                    .filter(|stream| stream_kind(stream) == Some(kind))
                    .map(|_| TrackRef::Embedded(*index))
            }));
        }
        rows.extend(self.active_left_subtitle_tracks());
        let active_left = self.active_left_subtitle_tracks();
        rows.extend((0..self.sidecars.len()).filter_map(|sidecar_index| {
            let track = TrackRef::Sidecar(sidecar_index);
            (!active_left.contains(&track)).then_some(track)
        }));
        rows.extend(self.stream_order.iter().filter_map(|index| {
            let track = TrackRef::Embedded(*index);
            (stream_by_index(info, *index)
                .is_some_and(|stream| stream_kind(stream) == Some("subtitle"))
                && !active_left.contains(&track))
            .then_some(track)
        }));
        rows.extend(self.stream_order.iter().filter_map(|index| {
            stream_by_index(info, *index)
                .filter(|stream| {
                    !matches!(stream_kind(stream), Some("video" | "audio" | "subtitle"))
                })
                .map(|_| TrackRef::Embedded(*index))
        }));
        rows
    }

    pub fn selected_track(&self) -> Option<TrackRef> {
        self.track_rows().get(self.selected_stream).cloned()
    }

    pub fn selected_stream_info(
        &self,
    ) -> Option<&std::collections::BTreeMap<String, serde_json::Value>> {
        let info = self.media_info()?;
        let TrackRef::Embedded(index) = self.selected_track()? else {
            return None;
        };
        stream_by_index(info, index)
    }

    pub fn selected_stream_index(&self) -> Option<u64> {
        match self.selected_track()? {
            TrackRef::Container => None,
            TrackRef::Embedded(index) => Some(index),
            TrackRef::Sidecar(_) => None,
        }
    }

    pub fn toggle_delete_selected_stream(&mut self) {
        if self.layer != Layer::Streams || self.dialog.is_some() {
            return;
        }
        if self.selected_track() == Some(TrackRef::Container) {
            self.notice = Some("The container can be changed, but not deleted.".into());
            return;
        }
        let Some(index) = self.selected_stream_index() else {
            self.notice = Some("Sidecar subtitles are converted rather than deleted here.".into());
            return;
        };
        if self.deleted_streams.remove(&index) {
            self.notice = None;
            return;
        }

        self.deleted_streams.insert(index);
        self.video_settings.remove(&index);
        self.subtitle_changes
            .remove(&SubtitleSource::Embedded(index));
        self.notice = None;
        self.selected_stream =
            (self.selected_stream + 1).min(self.stream_count().saturating_sub(1));
    }

    pub fn move_selected_stream(&mut self, direction: isize) {
        if self.layer != Layer::Streams || self.dialog.is_some() || direction == 0 {
            return;
        }
        let Some(selected_track) = self.selected_track() else {
            return;
        };
        let rows = self.track_rows();
        let left_positions = self.embedded_subtitle_positions(&rows);

        if let Some(left_idx) = left_positions
            .iter()
            .position(|&p| p == self.selected_stream)
        {
            let target_left_idx = if direction > 0 {
                left_idx + 1
            } else {
                left_idx.checked_sub(1).unwrap_or(left_idx)
            };
            if target_left_idx >= left_positions.len() || target_left_idx == left_idx {
                return;
            }
            let active = self.active_left_subtitle_tracks();
            if left_idx >= active.len() || target_left_idx >= active.len() {
                return;
            }
            let track_a = active[left_idx];
            let track_b = active[target_left_idx];

            self.left_subtitle_order = active;
            self.left_subtitle_order.swap(left_idx, target_left_idx);

            if let (TrackRef::Embedded(index_a), TrackRef::Embedded(index_b)) = (track_a, track_b)
                && let (Some(pos_a), Some(pos_b)) = (
                    self.stream_order.iter().position(|&i| i == index_a),
                    self.stream_order.iter().position(|&i| i == index_b),
                )
            {
                self.stream_order.swap(pos_a, pos_b);
                self.moved_streams.insert(index_a);
                self.moved_streams = self
                    .moved_streams
                    .iter()
                    .copied()
                    .filter(|candidate| {
                        stream_position_changed(
                            &self.original_stream_order,
                            &self.stream_order,
                            &self.deleted_streams,
                            self.media_info(),
                            *candidate,
                        )
                    })
                    .collect();
            }

            let new_rows = self.track_rows();
            if let Some(new_pos) = new_rows.iter().position(|&r| r == selected_track) {
                self.selected_stream = new_pos;
            }
            self.notice = None;
            return;
        }

        let Some(index) = self.selected_stream_index() else {
            return;
        };
        if self.deleted_streams.contains(&index) {
            self.notice = Some("Unmark this track for deletion before moving it.".to_string());
            return;
        }
        let Some(current_position) = self
            .stream_order
            .iter()
            .position(|candidate| *candidate == index)
        else {
            return;
        };
        let Some(target) = current_position.checked_add_signed(direction) else {
            return;
        };
        if target >= self.stream_order.len() {
            return;
        }
        let same_group = self.media_info().is_some_and(|info| {
            let current = stream_by_index(info, self.stream_order[current_position]);
            let target_stream = stream_by_index(info, self.stream_order[target]);
            current
                .zip(target_stream)
                .is_some_and(|(current, target)| stream_group(current) == stream_group(target))
        });
        if !same_group {
            return;
        }
        self.stream_order.swap(current_position, target);
        self.moved_streams.insert(index);
        self.moved_streams = self
            .moved_streams
            .iter()
            .copied()
            .filter(|candidate| {
                stream_position_changed(
                    &self.original_stream_order,
                    &self.stream_order,
                    &self.deleted_streams,
                    self.media_info(),
                    *candidate,
                )
            })
            .collect();
        self.selected_stream = self
            .track_rows()
            .iter()
            .position(|track| *track == TrackRef::Embedded(index))
            .unwrap_or(self.selected_stream);
        self.notice = None;
    }

    pub fn source_container(&self) -> Option<ContainerFormat> {
        self.selected_file()
            .and_then(|file| ContainerFormat::from_path(&file.path))
    }

    pub fn effective_container(&self) -> Option<ContainerFormat> {
        self.container_target.or_else(|| self.source_container())
    }

    fn original_container_label(&self) -> String {
        let name = self
            .media_info()
            .and_then(|info| {
                info.format
                    .get("format_long_name")
                    .or_else(|| info.format.get("format_name"))
            })
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown container");
        format!("Original ({name})")
    }

    pub fn container_conflicts_for(&self, target: ContainerFormat) -> Vec<String> {
        let mut conflicts = Vec::new();
        if !self.subtitle_capabilities.ffmpeg {
            conflicts.push("FFmpeg is not available in PATH.".to_string());
        } else if !self
            .subtitle_capabilities
            .ffmpeg_muxers
            .contains(target.muxer())
        {
            conflicts.push(format!(
                "The installed FFmpeg build does not provide the {} muxer.",
                target.label()
            ));
        }
        let Some(info) = self.media_info() else {
            return conflicts;
        };
        let stream_order = final_stream_order(info, &self.stream_order, &self.deleted_streams);
        let subtitle_changes = self.subtitle_changes.values().cloned().collect::<Vec<_>>();
        conflicts.extend(container_conflicts(
            info,
            &stream_order,
            &self.video_settings,
            &subtitle_changes,
            target,
        ));
        conflicts.extend(imported_subtitle_conflicts(
            &subtitle_changes,
            &self.sidecars,
            target,
        ));
        conflicts.extend(subtitle_metadata_conflicts(
            info,
            &subtitle_changes,
            &self.sidecars,
            target,
            true,
        ));
        conflicts
    }

    pub fn selected_container_conflicts(&self) -> Vec<String> {
        if let Some(target) = self.container_target {
            return self.container_conflicts_for(target);
        }
        let Some(target) = self.source_container() else {
            return if self
                .subtitle_changes
                .values()
                .any(|change| change.import_into_media)
            {
                vec![
                    "The current container is unknown; choose a container before importing subtitles."
                        .to_string(),
                ]
            } else {
                Vec::new()
            };
        };
        let subtitle_changes = self.subtitle_changes.values().cloned().collect::<Vec<_>>();
        let mut conflicts = imported_subtitle_conflicts(&subtitle_changes, &self.sidecars, target);
        if let Some(info) = self.media_info() {
            conflicts.extend(subtitle_metadata_conflicts(
                info,
                &subtitle_changes,
                &self.sidecars,
                target,
                false,
            ));
        }
        conflicts
    }

    pub fn selected_container_conflict_streams(&self) -> BTreeSet<u64> {
        let (Some(target), Some(info)) = (self.container_target, self.media_info()) else {
            return BTreeSet::new();
        };
        let stream_order = final_stream_order(info, &self.stream_order, &self.deleted_streams);
        let subtitle_changes = self.subtitle_changes.values().cloned().collect::<Vec<_>>();
        container_conflict_streams(
            info,
            &stream_order,
            &self.video_settings,
            &subtitle_changes,
            target,
        )
    }

    pub fn container_choices(&self) -> Vec<ContainerChoice> {
        let source = self.source_container();
        let mut choices = Vec::new();
        if source.is_none() {
            choices.push(ContainerChoice {
                value: None,
                label: self.original_container_label(),
                current: true,
                staged: self.container_target.is_none(),
                conflicts: Vec::new(),
            });
        }
        choices.extend(ContainerFormat::TARGETS.into_iter().map(|format| {
            let current = source == Some(format);
            let value = (!current).then_some(format);
            ContainerChoice {
                value,
                label: format.label().to_string(),
                current,
                staged: if current {
                    self.container_target.is_none()
                } else {
                    self.container_target == Some(format)
                },
                conflicts: if current {
                    Vec::new()
                } else {
                    self.container_conflicts_for(format)
                },
            }
        }));
        choices
    }

    pub fn open_container_settings(&mut self) {
        if self.layer != Layer::Streams
            || self.dialog.is_some()
            || self.selected_track() != Some(TrackRef::Container)
        {
            return;
        }
        let choices = self.container_choices();
        let cursor = choices.iter().position(|choice| choice.staged).unwrap_or(0);
        self.container_settings_popup = Some(ContainerSettingsPopup { cursor });
        self.notice = None;
        self.dialog = Some(Dialog::ContainerSettings);
    }

    pub fn move_container_settings_cursor(&mut self, direction: isize) {
        if self.dialog != Some(Dialog::ContainerSettings) || direction == 0 {
            return;
        }
        let length = self.container_choices().len();
        let popup = self.container_settings_popup.as_mut().unwrap();
        popup.cursor = move_cursor(popup.cursor, length, direction, |_| true);
    }

    pub fn move_container_settings_to_endpoint(&mut self, end: bool) {
        if self.dialog != Some(Dialog::ContainerSettings) {
            return;
        }
        let length = self.container_choices().len();
        if let Some(position) = cursor_endpoint(length, end, |_| true) {
            self.container_settings_popup.as_mut().unwrap().cursor = position;
        }
    }

    pub fn activate_container_settings(&mut self) {
        if self.dialog != Some(Dialog::ContainerSettings) {
            return;
        }
        let cursor = self.container_settings_popup.as_ref().unwrap().cursor;
        let Some(choice) = self.container_choices().get(cursor).cloned() else {
            return;
        };
        self.container_target = choice.value;
        self.notice = choice.warning();
        self.container_settings_popup = None;
        self.dialog = None;
    }

    pub fn close_container_settings(&mut self) {
        self.container_settings_popup = None;
        self.dialog = None;
    }

    pub fn open_video_settings(&mut self) {
        if self.layer != Layer::Streams || self.dialog.is_some() {
            return;
        }
        let Some(index) = self.selected_stream_index() else {
            return;
        };
        if self.deleted_streams.contains(&index) {
            self.notice =
                Some("Unmark this track for deletion before changing its video settings.".into());
            return;
        }
        let playable_video = self.selected_stream_info().is_some_and(|stream| {
            stream_kind(stream) == Some("video") && !crate::probe::is_attached_picture(stream)
        });
        if !playable_video {
            self.notice = Some("Encoding settings are only available for video tracks.".into());
            return;
        }

        let settings = self.video_settings.get(&index).copied().unwrap_or_default();
        let codecs = self.video_codec_choices(index);
        let resolutions = self.resolution_choices(index);
        self.video_settings_popup = Some(VideoSettingsPopup {
            stream_index: index,
            field: VideoSettingsField::Codec,
            mode: VideoSettingsMode::Summary,
            codec_cursor: codecs
                .iter()
                .position(|choice| choice.value == settings.codec)
                .unwrap_or(0),
            resolution_cursor: resolutions
                .iter()
                .position(|choice| choice.selected(settings.resolution))
                .unwrap_or(0),
            custom_resolution: None,
        });
        self.notice = None;
        self.dialog = Some(Dialog::VideoSettings);
    }

    pub fn open_track_settings(&mut self) {
        match self.selected_track() {
            Some(TrackRef::Container) => self.open_container_settings(),
            Some(TrackRef::Embedded(index))
                if self
                    .selected_stream_info()
                    .is_some_and(|stream| stream_kind(stream) == Some("subtitle")) =>
            {
                self.open_subtitle_settings(SubtitleSource::Embedded(index));
            }
            Some(TrackRef::Sidecar(index)) => {
                if let Some(sidecar) = self.sidecars.get(index) {
                    self.open_subtitle_settings(SubtitleSource::Sidecar(sidecar.path.clone()));
                }
            }
            _ => self.open_video_settings(),
        }
    }

    fn open_subtitle_settings(&mut self, source: SubtitleSource) {
        if self.layer != Layer::Streams || self.dialog.is_some() {
            return;
        }
        if let SubtitleSource::Embedded(index) = source
            && self.deleted_streams.contains(&index)
        {
            self.notice =
                Some("Unmark this subtitle track for deletion before converting it.".into());
            return;
        }
        let source_format = match &source {
            SubtitleSource::Embedded(index) => self
                .media_info()
                .and_then(|info| stream_by_index(info, *index))
                .and_then(|stream| stream.get("codec_name"))
                .and_then(serde_json::Value::as_str)
                .and_then(SubtitleFormat::from_codec),
            SubtitleSource::Sidecar(path) => self
                .sidecars
                .iter()
                .find(|sidecar| &sidecar.path == path)
                .map(|sidecar| sidecar.format),
        };
        let Some(source_format) = source_format else {
            self.notice =
                Some("This subtitle format is not supported for conversion yet.".to_string());
            return;
        };
        let change = self
            .subtitle_changes
            .get(&source)
            .cloned()
            .unwrap_or(SubtitleChange {
                source: source.clone(),
                source_format,
                embedded_target: None,
                export_target: None,
                import_into_media: false,
                ocr_language: None,

                metadata: None,
            });
        let codec_choices = self.subtitle_choices(&source, source_format);
        let selected_codec = change
            .export_target
            .or(change.embedded_target)
            .unwrap_or(source_format);
        let metadata = self
            .subtitle_display_state(&source, source_format)
            .map(|state| state.metadata)
            .unwrap_or_else(|| SubtitleMetadata {
                language: String::new(),
                title: None,
                forced: false,
                cc: false,
                hearing_impaired: false,
                original: false,
                commentary: false,
            });
        let language_choices = self.subtitle_language_choices_for(&source, "");
        let title_draft = metadata.title.unwrap_or_default();
        self.subtitle_settings_popup = Some(SubtitleSettingsPopup {
            source,
            source_format,
            field: SubtitleSettingsField::Codec,
            mode: SubtitleSettingsMode::Summary,
            help_visible: false,
            codec_cursor: codec_choices
                .iter()
                .position(|choice| choice.format == selected_codec)
                .unwrap_or(0),
            language_cursor: language_choices
                .iter()
                .position(|choice| choice.code.eq_ignore_ascii_case(&metadata.language))
                .unwrap_or(0),
            language_search: SearchState::default(),
            title_input: TextInputState::new(title_draft),
        });
        self.notice = None;
        self.dialog = Some(Dialog::SubtitleSettings);
    }

    pub fn toggle_subtitle_field_help(&mut self) {
        if let Some(popup) = self.subtitle_settings_popup.as_mut() {
            popup.help_visible = !popup.help_visible;
        }
    }

    pub fn subtitle_choices(
        &self,
        source: &SubtitleSource,
        source_format: SubtitleFormat,
    ) -> Vec<FormatChoice> {
        let sidecar = matches!(source, SubtitleSource::Sidecar(_));
        let importing = sidecar
            && self
                .subtitle_changes
                .get(source)
                .is_some_and(|change| change.import_into_media);
        let mut choices = self.subtitle_capabilities.format_choices(
            source_format,
            (!sidecar || importing)
                .then(|| {
                    self.container_target
                        .map(ContainerFormat::extension)
                        .or_else(|| {
                            self.selected_file()
                                .and_then(|file| path_extension(&file.path))
                        })
                })
                .flatten(),
            sidecar && !importing,
            false,
        );
        let is_exported = match source {
            SubtitleSource::Embedded(idx) => self.is_stream_exported(*idx),
            SubtitleSource::Sidecar(_) => false,
        };
        if !sidecar && is_exported {
            let export_choices = self.subtitle_export_choices(source_format);
            for choice in &mut choices {
                let export_choice = export_choices
                    .iter()
                    .find(|export| export.format == choice.format);
                if let Some(reason) = export_choice
                    .filter(|export| !export.enabled)
                    .and_then(|export| export.reason.as_deref())
                {
                    choice.enabled = false;
                    choice.reason = Some(format!("Cannot export: {reason}"));
                }
            }
        }
        choices
    }

    fn subtitle_export_choices(&self, source_format: SubtitleFormat) -> Vec<FormatChoice> {
        self.subtitle_capabilities
            .format_choices(source_format, None, true, true)
    }

    fn store_subtitle_change(&mut self, source: SubtitleSource, mut change: SubtitleChange) {
        change.ocr_language = change
            .needs_ocr()
            .then(|| self.automatic_ocr_language(&source))
            .flatten();
        if change.has_effect() {
            self.subtitle_changes.insert(source, change);
        } else {
            self.subtitle_changes.remove(&source);
        }
    }

    fn subtitle_change(
        &self,
        source: &SubtitleSource,
        source_format: SubtitleFormat,
    ) -> SubtitleChange {
        self.subtitle_changes
            .get(source)
            .cloned()
            .unwrap_or(SubtitleChange {
                source: source.clone(),
                source_format,
                embedded_target: None,
                export_target: None,
                import_into_media: false,
                ocr_language: None,

                metadata: None,
            })
    }

    fn original_subtitle_metadata(&self, source: &SubtitleSource) -> Option<SubtitleMetadata> {
        match source {
            SubtitleSource::Embedded(index) => {
                let stream = self
                    .media_info()
                    .and_then(|info| stream_by_index(info, *index))?;
                Some(SubtitleMetadata {
                    language: stream_language(stream),
                    title: stream_title(stream),
                    forced: stream_forced(stream),
                    cc: stream_cc(stream),
                    hearing_impaired: stream_hearing_impaired(stream),
                    original: stream_original(stream),
                    commentary: stream_commentary(stream),
                })
            }
            SubtitleSource::Sidecar(path) => {
                let sidecar = self.sidecars.iter().find(|sidecar| &sidecar.path == path)?;
                Some(SubtitleMetadata {
                    language: sidecar.language.clone(),
                    title: None,
                    forced: sidecar.forced,
                    cc: false,
                    hearing_impaired: sidecar.hearing_impaired,
                    original: false,
                    commentary: false,
                })
            }
        }
    }

    pub fn subtitle_metadata_for(&self, source: &SubtitleSource) -> Option<SubtitleMetadata> {
        self.subtitle_changes
            .get(source)
            .and_then(|change| change.metadata.clone())
            .or_else(|| self.original_subtitle_metadata(source))
    }

    pub fn subtitle_display_state(
        &self,
        source: &SubtitleSource,
        source_format: SubtitleFormat,
    ) -> Option<SubtitleDisplayState> {
        let mut original_metadata = self.original_subtitle_metadata(source)?;
        let mut metadata = self.subtitle_metadata_for(source)?;
        let format = self
            .subtitle_changes
            .get(source)
            .and_then(|change| change.export_target.or(change.embedded_target))
            .unwrap_or(source_format);
        let default = self.subtitle_default_for(source);
        let original_default = match source {
            SubtitleSource::Embedded(index) => self
                .media_info()
                .and_then(|info| stream_by_index(info, *index))
                .is_some_and(is_default),
            SubtitleSource::Sidecar(_) => false,
        };
        let external = self.subtitle_source_external(source);
        if external {
            for metadata in [&mut original_metadata, &mut metadata] {
                metadata.hearing_impaired |= metadata.cc;
                metadata.cc = false;
            }
        } else if let Some(container) = self.effective_container() {
            container.retain_supported_subtitle_metadata(&mut metadata);
        }
        Some(SubtitleDisplayState {
            format,
            metadata,
            default,
            external,
            container: self.effective_container(),
            source_format,
            original_metadata,
            original_default,
        })
    }

    pub fn subtitle_popup_metadata(&self) -> Option<SubtitleMetadata> {
        self.subtitle_settings_popup.as_ref().and_then(|popup| {
            self.subtitle_display_state(&popup.source, popup.source_format)
                .map(|state| state.metadata)
        })
    }

    fn store_subtitle_metadata(
        &mut self,
        source: SubtitleSource,
        source_format: SubtitleFormat,
        mut metadata: SubtitleMetadata,
    ) {
        if let Some(language) = canonical_language_code(&metadata.language) {
            metadata.language = language;
        }
        let original = self.original_subtitle_metadata(&source);
        let mut change = self.subtitle_change(&source, source_format);
        change.metadata = (original.as_ref() != Some(&metadata)).then_some(metadata);
        self.store_subtitle_change(source, change);
    }

    fn subtitle_language_choices_for(
        &self,
        source: &SubtitleSource,
        query: &str,
    ) -> Vec<LanguageChoice> {
        let mut choices = common_language_choices();
        if let Some(current) = self
            .subtitle_metadata_for(source)
            .and_then(|metadata| language_choice(&metadata.language))
            && !choices.iter().any(|choice| choice.code == current.code)
        {
            choices.push(current);
            choices.sort_by(|left, right| {
                left.name
                    .to_ascii_lowercase()
                    .cmp(&right.name.to_ascii_lowercase())
                    .then_with(|| left.code.cmp(&right.code))
            });
        }
        choices.retain(|choice| choice.matches(query));
        choices
    }

    pub fn filtered_subtitle_languages(&self) -> Vec<LanguageChoice> {
        let Some(popup) = self.subtitle_settings_popup.as_ref() else {
            return Vec::new();
        };
        self.subtitle_language_choices_for(&popup.source, &popup.language_search.value)
    }

    fn subtitle_sidecar_index(&self, source: &SubtitleSource) -> Option<usize> {
        let SubtitleSource::Sidecar(path) = source else {
            return None;
        };
        self.sidecars
            .iter()
            .position(|sidecar| &sidecar.path == path)
    }

    fn subtitle_source_imported(&self, source: &SubtitleSource) -> bool {
        self.subtitle_sidecar_index(source)
            .is_some_and(|index| self.is_sidecar_imported(index))
    }

    fn subtitle_source_external(&self, source: &SubtitleSource) -> bool {
        match source {
            SubtitleSource::Embedded(index) => self.is_stream_exported(*index),
            SubtitleSource::Sidecar(_) => !self.subtitle_source_imported(source),
        }
    }

    pub fn subtitle_field_visible(&self, field: SubtitleSettingsField) -> bool {
        let Some(popup) = self.subtitle_settings_popup.as_ref() else {
            return false;
        };
        self.subtitle_display_state(&popup.source, popup.source_format)
            .is_some_and(|state| state.field_visible(field))
    }

    pub fn visible_subtitle_fields(&self) -> Vec<SubtitleSettingsField> {
        SubtitleSettingsField::ALL
            .into_iter()
            .filter(|field| self.subtitle_field_visible(*field))
            .collect()
    }

    pub fn subtitle_field_reason(&self, field: SubtitleSettingsField) -> Option<String> {
        let popup = self.subtitle_settings_popup.as_ref()?;
        if !self.subtitle_field_visible(field) {
            return None;
        }
        let external = self.subtitle_source_external(&popup.source);
        let flag = field.subtitle_flag()?;
        if external
            && matches!(
                flag,
                SubtitleFlag::Forced | SubtitleFlag::Cc | SubtitleFlag::HearingImpaired
            )
        {
            return None;
        }
        let Some(container) = self.effective_container() else {
            return Some("Choose a known container to set this flag.".to_string());
        };
        (!container.supports_subtitle_flag(flag))
            .then(|| format!("{} does not support this subtitle flag.", container.label()))
    }

    pub fn subtitle_popup_default(&self) -> bool {
        let Some(popup) = self.subtitle_settings_popup.as_ref() else {
            return false;
        };
        self.subtitle_default_for(&popup.source)
    }

    fn subtitle_default_for(&self, source: &SubtitleSource) -> bool {
        match source {
            SubtitleSource::Embedded(index) => self.default_streams.contains(index),
            SubtitleSource::Sidecar(_) => self
                .subtitle_sidecar_index(source)
                .is_some_and(|index| self.default_sidecars.contains(&index)),
        }
    }

    pub fn subtitle_popup_metadata_changed(&self, field: SubtitleSettingsField) -> bool {
        let Some(popup) = self.subtitle_settings_popup.as_ref() else {
            return false;
        };
        self.subtitle_display_state(&popup.source, popup.source_format)
            .is_some_and(|state| state.field_changed(field))
    }

    pub fn subtitle_popup_codec(&self) -> Option<SubtitleFormat> {
        let popup = self.subtitle_settings_popup.as_ref()?;
        self.subtitle_display_state(&popup.source, popup.source_format)
            .map(|state| state.format)
    }

    fn toggle_subtitle_checkbox(&mut self, field: SubtitleSettingsField) {
        let Some(popup) = self.subtitle_settings_popup.as_ref() else {
            return;
        };
        let source = popup.source.clone();
        let source_format = popup.source_format;
        let external = self.subtitle_source_external(&source);
        if field == SubtitleSettingsField::Default {
            if let Some(reason) = self.subtitle_field_reason(field) {
                self.notice = Some(reason);
                return;
            }
            self.toggle_subtitle_default(&source);
            return;
        }
        let Some(mut metadata) = self.subtitle_metadata_for(&source) else {
            return;
        };
        let current = match field {
            SubtitleSettingsField::Forced => metadata.forced,
            SubtitleSettingsField::Cc => metadata.cc,
            SubtitleSettingsField::HearingImpaired if external => {
                metadata.cc || metadata.hearing_impaired
            }
            SubtitleSettingsField::HearingImpaired => metadata.hearing_impaired,
            SubtitleSettingsField::Original => metadata.original,
            SubtitleSettingsField::Commentary => metadata.commentary,
            _ => return,
        };
        if !current && let Some(reason) = self.subtitle_field_reason(field) {
            self.notice = Some(reason);
            return;
        }
        match field {
            SubtitleSettingsField::Forced => metadata.forced = !metadata.forced,
            SubtitleSettingsField::Cc => metadata.cc = !metadata.cc,
            SubtitleSettingsField::HearingImpaired if external => {
                metadata.cc = false;
                metadata.hearing_impaired = !current;
            }
            SubtitleSettingsField::HearingImpaired => {
                metadata.hearing_impaired = !metadata.hearing_impaired;
            }
            SubtitleSettingsField::Original => metadata.original = !metadata.original,
            SubtitleSettingsField::Commentary => metadata.commentary = !metadata.commentary,
            _ => return,
        }
        self.store_subtitle_metadata(source, source_format, metadata);
        self.notice = None;
    }

    pub fn start_subtitle_language_search(&mut self) {
        if let Some(popup) = self
            .subtitle_settings_popup
            .as_mut()
            .filter(|popup| popup.mode == SubtitleSettingsMode::LanguageDropdown)
        {
            popup.language_search.activate();
        }
    }

    pub fn cancel_subtitle_language_search(&mut self) {
        if let Some(popup) = self
            .subtitle_settings_popup
            .as_mut()
            .filter(|popup| popup.mode == SubtitleSettingsMode::LanguageDropdown)
        {
            popup.language_search.clear();
            popup.language_cursor = 0;
        }
    }

    pub fn start_subtitle_title_input(&mut self) {
        let Some(popup) = self.subtitle_settings_popup.as_ref() else {
            return;
        };
        if popup.mode != SubtitleSettingsMode::Summary
            || popup.field != SubtitleSettingsField::Title
        {
            return;
        }
        if let Some(reason) = self.subtitle_field_reason(SubtitleSettingsField::Title) {
            self.notice = Some(reason);
            return;
        }
        let popup = self.subtitle_settings_popup.as_mut().unwrap();
        popup.title_input.activate();
        popup.mode = SubtitleSettingsMode::TitleEdit;
        self.notice = None;
    }

    fn toggle_subtitle_default(&mut self, source: &SubtitleSource) {
        match source {
            SubtitleSource::Embedded(index) => {
                if self.default_streams.remove(index) {
                    return;
                }
                if let Some(info) = self.media_info() {
                    let subtitles = info
                        .streams
                        .iter()
                        .filter(|stream| stream_kind(stream) == Some("subtitle"))
                        .filter_map(stream_index)
                        .collect::<Vec<_>>();
                    for subtitle in subtitles {
                        self.default_streams.remove(&subtitle);
                    }
                }
                self.default_sidecars.clear();
                self.default_streams.insert(*index);
            }
            SubtitleSource::Sidecar(_) => {
                let Some(index) = self.subtitle_sidecar_index(source) else {
                    return;
                };
                if self.default_sidecars.remove(&index) {
                    return;
                }
                if let Some(info) = self.media_info() {
                    let subtitles = info
                        .streams
                        .iter()
                        .filter(|stream| stream_kind(stream) == Some("subtitle"))
                        .filter_map(stream_index)
                        .collect::<Vec<_>>();
                    for subtitle in subtitles {
                        self.default_streams.remove(&subtitle);
                    }
                }
                self.default_sidecars.clear();
                self.default_sidecars.insert(index);
            }
        }
        self.notice = None;
    }

    pub fn input_subtitle_language(&mut self, character: char) {
        let Some(popup) = self.subtitle_settings_popup.as_mut().filter(|popup| {
            popup.mode == SubtitleSettingsMode::LanguageDropdown && popup.language_search.is_active
        }) else {
            return;
        };
        let before = popup.language_search.value.clone();
        popup.language_search.input.insert(character, 64, |value| {
            !value.is_control() && !value.is_whitespace()
        });
        if popup.language_search.value != before {
            popup.language_cursor = 0;
        }
    }

    pub fn backspace_subtitle_language(&mut self) {
        if let Some(popup) = self.subtitle_settings_popup.as_mut().filter(|popup| {
            popup.mode == SubtitleSettingsMode::LanguageDropdown && popup.language_search.is_active
        }) {
            popup.language_search.input.backspace();
            popup.language_cursor = 0;
        }
    }

    pub fn delete_subtitle_language(&mut self) {
        if let Some(popup) = self
            .subtitle_settings_popup
            .as_mut()
            .filter(|popup| popup.language_search.is_active)
        {
            popup.language_search.delete_char();
            popup.language_cursor = 0;
        }
    }

    pub fn move_subtitle_language_cursor(&mut self, direction: isize) {
        if let Some(popup) = self
            .subtitle_settings_popup
            .as_mut()
            .filter(|popup| popup.language_search.is_active)
        {
            popup.language_search.move_cursor(direction);
        }
    }

    pub fn move_subtitle_language_home(&mut self, end: bool) {
        if let Some(popup) = self
            .subtitle_settings_popup
            .as_mut()
            .filter(|popup| popup.language_search.is_active)
        {
            popup.language_search.move_home(end);
        }
    }

    pub fn input_subtitle_title(&mut self, character: char) {
        let Some(popup) = self
            .subtitle_settings_popup
            .as_mut()
            .filter(|popup| popup.mode == SubtitleSettingsMode::TitleEdit)
        else {
            return;
        };
        popup
            .title_input
            .insert(character, 512, |value| !value.is_control());
        popup.title_input.keep_cursor_visible(42);
    }

    pub fn backspace_subtitle_title(&mut self) {
        let Some(popup) = self
            .subtitle_settings_popup
            .as_mut()
            .filter(|popup| popup.mode == SubtitleSettingsMode::TitleEdit)
        else {
            return;
        };
        popup.title_input.backspace();
        popup.title_input.keep_cursor_visible(42);
    }

    pub fn delete_subtitle_title(&mut self) {
        let Some(popup) = self
            .subtitle_settings_popup
            .as_mut()
            .filter(|popup| popup.mode == SubtitleSettingsMode::TitleEdit)
        else {
            return;
        };
        popup.title_input.delete();
    }

    pub fn move_subtitle_title_cursor(&mut self, direction: isize) {
        let Some(popup) = self
            .subtitle_settings_popup
            .as_mut()
            .filter(|popup| popup.mode == SubtitleSettingsMode::TitleEdit)
        else {
            return;
        };
        popup.title_input.move_cursor(direction);
        popup.title_input.keep_cursor_visible(42);
    }

    pub fn move_subtitle_title_home(&mut self, end: bool) {
        if let Some(popup) = self
            .subtitle_settings_popup
            .as_mut()
            .filter(|popup| popup.mode == SubtitleSettingsMode::TitleEdit)
        {
            popup.title_input.move_home(end);
            popup.title_input.keep_cursor_visible(42);
        }
    }

    fn commit_subtitle_title(&mut self) {
        let Some(popup) = self.subtitle_settings_popup.as_ref() else {
            return;
        };
        let source = popup.source.clone();
        let source_format = popup.source_format;
        let title = popup.title_input.value.trim().to_string();
        let Some(mut metadata) = self.subtitle_metadata_for(&source) else {
            return;
        };
        metadata.title = (!title.is_empty()).then_some(title);
        self.store_subtitle_metadata(source, source_format, metadata);
    }

    fn automatic_ocr_language(&self, source: &SubtitleSource) -> Option<String> {
        let preferred = match source {
            SubtitleSource::Embedded(index) => self
                .media_info()
                .and_then(|info| stream_by_index(info, *index))
                .map(stream_language),
            SubtitleSource::Sidecar(path) => self
                .sidecars
                .iter()
                .find(|sidecar| &sidecar.path == path)
                .map(|sidecar| sidecar.language.clone()),
        }
        .unwrap_or_default();
        let base = preferred.split('-').next().unwrap_or_default();
        let iso_three = match base {
            "en" => Some("eng"),
            "nl" => Some("nld"),
            "de" => Some("deu"),
            "fr" => Some("fra"),
            "es" => Some("spa"),
            "it" => Some("ita"),
            "pt" => Some("por"),
            "ja" => Some("jpn"),
            "ko" => Some("kor"),
            _ => None,
        };
        [Some(preferred.as_str()), Some(base), iso_three]
            .into_iter()
            .flatten()
            .find_map(|candidate| {
                self.subtitle_capabilities
                    .tesseract_languages
                    .iter()
                    .find(|language| language.eq_ignore_ascii_case(candidate))
                    .cloned()
            })
            .or_else(|| {
                self.subtitle_capabilities
                    .tesseract_languages
                    .iter()
                    .find(|language| language.as_str() == "eng")
                    .cloned()
            })
            .or_else(|| {
                self.subtitle_capabilities
                    .tesseract_languages
                    .first()
                    .cloned()
            })
    }

    pub fn move_subtitle_settings_cursor(&mut self, direction: isize) {
        let Some(popup) = self.subtitle_settings_popup.as_ref() else {
            return;
        };
        match popup.mode {
            SubtitleSettingsMode::Summary => {
                let fields = self.visible_subtitle_fields();
                let position = fields
                    .iter()
                    .position(|field| *field == popup.field)
                    .unwrap_or(0);
                let next = move_cursor(position, fields.len(), direction, |_| true);
                self.subtitle_settings_popup.as_mut().unwrap().field = fields[next];
            }
            SubtitleSettingsMode::CodecDropdown => {
                let choices = self.subtitle_choices(&popup.source, popup.source_format);
                let popup = self.subtitle_settings_popup.as_mut().unwrap();
                popup.codec_cursor =
                    move_cursor(popup.codec_cursor, choices.len(), direction, |position| {
                        choices[position].enabled
                    });
            }
            SubtitleSettingsMode::LanguageDropdown => {
                let choices = self.filtered_subtitle_languages();
                let popup = self.subtitle_settings_popup.as_mut().unwrap();
                popup.language_cursor =
                    move_cursor(popup.language_cursor, choices.len(), direction, |_| true);
            }
            SubtitleSettingsMode::TitleEdit => {}
        }
    }

    pub fn move_subtitle_settings_to_endpoint(&mut self, end: bool) {
        let Some(popup) = self.subtitle_settings_popup.as_ref() else {
            return;
        };
        match popup.mode {
            SubtitleSettingsMode::Summary => {
                let fields = self.visible_subtitle_fields();
                if let Some(position) = cursor_endpoint(fields.len(), end, |_| true) {
                    self.subtitle_settings_popup.as_mut().unwrap().field = fields[position];
                }
            }
            SubtitleSettingsMode::CodecDropdown => {
                let choices = self.subtitle_choices(&popup.source, popup.source_format);
                if let Some(position) =
                    cursor_endpoint(choices.len(), end, |position| choices[position].enabled)
                {
                    self.subtitle_settings_popup.as_mut().unwrap().codec_cursor = position;
                }
            }
            SubtitleSettingsMode::LanguageDropdown => {
                let choices = self.filtered_subtitle_languages();
                if let Some(position) = cursor_endpoint(choices.len(), end, |_| true) {
                    self.subtitle_settings_popup
                        .as_mut()
                        .unwrap()
                        .language_cursor = position;
                }
            }
            SubtitleSettingsMode::TitleEdit => {}
        }
    }

    pub fn activate_subtitle_settings(&mut self) {
        let Some(popup) = self.subtitle_settings_popup.as_ref() else {
            return;
        };
        let source = popup.source.clone();
        let source_format = popup.source_format;
        if popup.mode == SubtitleSettingsMode::Summary && !self.subtitle_field_visible(popup.field)
        {
            return;
        }
        match popup.mode {
            SubtitleSettingsMode::Summary => match popup.field {
                SubtitleSettingsField::Codec => {
                    self.subtitle_settings_popup.as_mut().unwrap().mode =
                        SubtitleSettingsMode::CodecDropdown;
                }
                SubtitleSettingsField::Language => {
                    let current_language = self
                        .subtitle_metadata_for(&source)
                        .map(|metadata| metadata.language)
                        .unwrap_or_default();
                    let choices = self.subtitle_language_choices_for(&source, "");
                    let cursor = choices
                        .iter()
                        .position(|choice| choice.code.eq_ignore_ascii_case(&current_language))
                        .unwrap_or(0);
                    let popup = self.subtitle_settings_popup.as_mut().unwrap();
                    popup.mode = SubtitleSettingsMode::LanguageDropdown;
                    popup.language_search.clear();
                    popup.language_cursor = cursor;
                }
                SubtitleSettingsField::Title => {
                    if let Some(reason) = self.subtitle_field_reason(SubtitleSettingsField::Title) {
                        self.notice = Some(reason);
                    }
                }
                field => self.toggle_subtitle_checkbox(field),
            },
            SubtitleSettingsMode::CodecDropdown => {
                let choices = self.subtitle_choices(&source, source_format);
                let Some(choice) = choices
                    .get(popup.codec_cursor)
                    .filter(|choice| choice.enabled)
                else {
                    return;
                };
                let mut change = self.subtitle_change(&source, source_format);
                let exporting = change.export_target.is_some();
                if exporting {
                    change.embedded_target = None;
                    change.export_target = Some(choice.format);
                } else {
                    change.embedded_target = choice.value;
                }
                self.store_subtitle_change(source, change);
                self.subtitle_settings_popup.as_mut().unwrap().mode = SubtitleSettingsMode::Summary;
            }
            SubtitleSettingsMode::LanguageDropdown => {
                let choices = self.filtered_subtitle_languages();
                let Some(choice) = choices.get(popup.language_cursor) else {
                    return;
                };
                let mut metadata = self.subtitle_metadata_for(&source).unwrap();
                metadata.language.clone_from(&choice.code);
                self.store_subtitle_metadata(source, source_format, metadata);
                let popup = self.subtitle_settings_popup.as_mut().unwrap();
                popup.mode = SubtitleSettingsMode::Summary;
                popup.language_search.clear();
            }
            SubtitleSettingsMode::TitleEdit => {}
        }
    }

    pub fn escape_subtitle_settings(&mut self) {
        let Some(mode) = self
            .subtitle_settings_popup
            .as_ref()
            .map(|popup| popup.mode)
        else {
            self.dialog = None;
            return;
        };
        match mode {
            SubtitleSettingsMode::TitleEdit => {
                self.commit_subtitle_title();
                let popup = self.subtitle_settings_popup.as_mut().unwrap();
                popup.title_input.deactivate();
                popup.mode = SubtitleSettingsMode::Summary;
                return;
            }
            SubtitleSettingsMode::CodecDropdown | SubtitleSettingsMode::LanguageDropdown => {
                let popup = self.subtitle_settings_popup.as_mut().unwrap();
                popup.mode = SubtitleSettingsMode::Summary;
                popup.language_search.clear();
                return;
            }
            SubtitleSettingsMode::Summary => {}
        }
        self.subtitle_settings_popup = None;
        self.dialog = None;
    }

    pub fn close_subtitle_settings(&mut self) {
        self.subtitle_settings_popup = None;
        self.dialog = None;
    }

    pub fn save_from_subtitle_settings(&mut self) {
        if self
            .subtitle_settings_popup
            .as_ref()
            .is_some_and(|popup| popup.mode == SubtitleSettingsMode::TitleEdit)
        {
            self.commit_subtitle_title();
        }
        self.close_subtitle_settings();
        self.request_save();
    }

    pub fn move_video_settings_cursor(&mut self, direction: isize) {
        let Some(popup) = self.video_settings_popup.as_ref() else {
            return;
        };
        match popup.mode {
            VideoSettingsMode::Summary => {
                let popup = self.video_settings_popup.as_mut().unwrap();
                popup.field = match (popup.field, direction.is_positive()) {
                    (VideoSettingsField::Codec, true) => VideoSettingsField::Resolution,
                    (VideoSettingsField::Resolution, false) => VideoSettingsField::Codec,
                    (field, _) => field,
                };
            }
            VideoSettingsMode::Dropdown => match popup.field {
                VideoSettingsField::Codec => {
                    let choices = self.video_codec_choices(popup.stream_index);
                    let popup = self.video_settings_popup.as_mut().unwrap();
                    popup.codec_cursor =
                        move_cursor(popup.codec_cursor, choices.len(), direction, |position| {
                            choices[position].enabled
                        });
                }
                VideoSettingsField::Resolution => {
                    let choices = self.resolution_choices(popup.stream_index);
                    let popup = self.video_settings_popup.as_mut().unwrap();
                    popup.resolution_cursor = move_cursor(
                        popup.resolution_cursor,
                        choices.len(),
                        direction,
                        |position| choices[position].enabled,
                    );
                }
            },
            VideoSettingsMode::CustomResolution => {
                let Some(draft) = self
                    .video_settings_popup
                    .as_mut()
                    .and_then(|popup| popup.custom_resolution.as_mut())
                else {
                    return;
                };
                if draft.scaling_dropdown_open {
                    draft.scaling_cursor = move_cursor(
                        draft.scaling_cursor,
                        CustomScaling::OPTIONS.len(),
                        direction,
                        |_| true,
                    );
                    return;
                }
                draft.field = match (draft.field, direction.is_positive()) {
                    (CustomResolutionField::Width, true) => CustomResolutionField::Height,
                    (CustomResolutionField::Height, true) => CustomResolutionField::Scaling,
                    (CustomResolutionField::Scaling, false) => CustomResolutionField::Height,
                    (CustomResolutionField::Height, false) => CustomResolutionField::Width,
                    (field, _) => field,
                };
            }
        }
    }

    pub fn move_video_settings_to_endpoint(&mut self, end: bool) {
        let Some(popup) = self.video_settings_popup.as_ref() else {
            return;
        };
        match popup.mode {
            VideoSettingsMode::Summary => {
                self.video_settings_popup.as_mut().unwrap().field = if end {
                    VideoSettingsField::Resolution
                } else {
                    VideoSettingsField::Codec
                };
            }
            VideoSettingsMode::Dropdown => match popup.field {
                VideoSettingsField::Codec => {
                    let choices = self.video_codec_choices(popup.stream_index);
                    if let Some(position) =
                        cursor_endpoint(choices.len(), end, |position| choices[position].enabled)
                    {
                        self.video_settings_popup.as_mut().unwrap().codec_cursor = position;
                    }
                }
                VideoSettingsField::Resolution => {
                    let choices = self.resolution_choices(popup.stream_index);
                    if let Some(position) =
                        cursor_endpoint(choices.len(), end, |position| choices[position].enabled)
                    {
                        self.video_settings_popup
                            .as_mut()
                            .unwrap()
                            .resolution_cursor = position;
                    }
                }
            },
            VideoSettingsMode::CustomResolution => {
                let Some(draft) = self
                    .video_settings_popup
                    .as_mut()
                    .and_then(|popup| popup.custom_resolution.as_mut())
                else {
                    return;
                };
                if draft.scaling_dropdown_open {
                    draft.scaling_cursor = if end {
                        CustomScaling::OPTIONS.len().saturating_sub(1)
                    } else {
                        0
                    };
                } else if !draft.width.is_active && !draft.height.is_active {
                    draft.field = if end {
                        CustomResolutionField::Scaling
                    } else {
                        CustomResolutionField::Width
                    };
                }
            }
        }
    }

    pub fn activate_video_settings(&mut self) {
        let Some(popup) = self.video_settings_popup.as_ref() else {
            return;
        };
        match popup.mode {
            VideoSettingsMode::Summary => {
                self.video_settings_popup.as_mut().unwrap().mode = VideoSettingsMode::Dropdown;
                return;
            }
            VideoSettingsMode::CustomResolution => {
                self.activate_custom_resolution();
                return;
            }
            VideoSettingsMode::Dropdown => {}
        }

        let index = popup.stream_index;
        let field = popup.field;
        let codec_cursor = popup.codec_cursor;
        let resolution_cursor = popup.resolution_cursor;
        let mut settings = self.video_settings.get(&index).copied().unwrap_or_default();
        match field {
            VideoSettingsField::Codec => {
                let choices = self.video_codec_choices(index);
                let Some(choice) = choices.get(codec_cursor).filter(|choice| choice.enabled) else {
                    return;
                };
                settings.codec = choice.value;
            }
            VideoSettingsField::Resolution => {
                let choices = self.resolution_choices(index);
                let Some(choice) = choices
                    .get(resolution_cursor)
                    .filter(|choice| choice.enabled)
                else {
                    return;
                };
                match choice.value {
                    ResolutionChoiceValue::Resolution(value) => settings.resolution = value,
                    ResolutionChoiceValue::Custom => {
                        let source_dimensions = self.video_source_dimensions(index);
                        let custom = match settings.resolution {
                            VideoResolution::Custom(custom) => Some(custom),
                            _ => None,
                        };
                        let popup = self.video_settings_popup.as_mut().unwrap();
                        popup.custom_resolution = Some(CustomResolutionDraft {
                            width: TextInputState::new(
                                custom
                                    .map(|custom| custom.width.to_string())
                                    .or_else(|| {
                                        source_dimensions.map(|(width, _)| width.to_string())
                                    })
                                    .unwrap_or_default(),
                            ),
                            height: TextInputState::new(
                                custom
                                    .map(|custom| custom.height.to_string())
                                    .or_else(|| {
                                        source_dimensions.map(|(_, height)| height.to_string())
                                    })
                                    .unwrap_or_default(),
                            ),
                            scaling: custom
                                .map(|custom| custom.scaling)
                                .unwrap_or(CustomScaling::FitPad),
                            field: CustomResolutionField::Width,
                            scaling_cursor: custom
                                .and_then(|custom| {
                                    CustomScaling::OPTIONS
                                        .iter()
                                        .position(|scaling| *scaling == custom.scaling)
                                })
                                .unwrap_or(0),
                            scaling_dropdown_open: false,
                        });
                        popup.mode = VideoSettingsMode::CustomResolution;
                        return;
                    }
                }
            }
        }
        if self.settings_change_stream(index, settings) {
            self.video_settings.insert(index, settings);
        } else {
            self.video_settings.remove(&index);
        }
        self.video_settings_popup.as_mut().unwrap().mode = VideoSettingsMode::Summary;
    }

    pub fn escape_video_settings(&mut self) {
        let Some(popup) = self.video_settings_popup.as_ref() else {
            self.dialog = None;
            return;
        };
        match popup.mode {
            VideoSettingsMode::CustomResolution => {
                let popup = self.video_settings_popup.as_mut().unwrap();
                if popup
                    .custom_resolution
                    .as_ref()
                    .is_some_and(|draft| draft.scaling_dropdown_open)
                {
                    let draft = popup.custom_resolution.as_mut().unwrap();
                    draft.scaling_dropdown_open = false;
                    draft.scaling_cursor = CustomScaling::OPTIONS
                        .iter()
                        .position(|scaling| *scaling == draft.scaling)
                        .unwrap_or(0);
                    return;
                }
                self.stage_custom_resolution();
                let popup = self.video_settings_popup.as_mut().unwrap();
                popup.custom_resolution = None;
                popup.mode = VideoSettingsMode::Dropdown;
                return;
            }
            VideoSettingsMode::Dropdown => {
                let index = popup.stream_index;
                let field = popup.field;
                let settings = self.video_settings.get(&index).copied().unwrap_or_default();
                let resolution_cursor = (field == VideoSettingsField::Resolution).then(|| {
                    self.resolution_choices(index)
                        .iter()
                        .position(|choice| choice.selected(settings.resolution))
                        .unwrap_or(0)
                });
                let codec_cursor = (field == VideoSettingsField::Codec).then(|| {
                    self.video_codec_choices(index)
                        .iter()
                        .position(|choice| choice.value == settings.codec)
                        .unwrap_or(0)
                });
                let popup = self.video_settings_popup.as_mut().unwrap();
                popup.mode = VideoSettingsMode::Summary;
                match field {
                    VideoSettingsField::Codec => {
                        popup.codec_cursor = codec_cursor.unwrap_or(0);
                    }
                    VideoSettingsField::Resolution => {
                        popup.resolution_cursor = resolution_cursor.unwrap_or(0);
                    }
                }
                return;
            }
            VideoSettingsMode::Summary => {}
        }
        self.video_settings_popup = None;
        self.dialog = None;
    }

    pub fn close_video_settings(&mut self) {
        self.video_settings_popup = None;
        self.dialog = None;
    }

    pub fn save_from_video_settings(&mut self) {
        let custom_open = self
            .video_settings_popup
            .as_ref()
            .is_some_and(|popup| popup.mode == VideoSettingsMode::CustomResolution);
        if custom_open {
            self.commit_scaling_dropdown();
            if !self.apply_custom_resolution() {
                return;
            }
        }
        self.close_video_settings();
        self.request_save();
    }

    pub fn custom_resolution_input_active(&self) -> bool {
        self.video_settings_popup
            .as_ref()
            .filter(|popup| popup.mode == VideoSettingsMode::CustomResolution)
            .and_then(|popup| popup.custom_resolution.as_ref())
            .is_some_and(|draft| match draft.field {
                CustomResolutionField::Width => draft.width.is_active,
                CustomResolutionField::Height => draft.height.is_active,
                CustomResolutionField::Scaling => false,
            })
    }

    pub fn start_custom_resolution_input(&mut self) {
        let Some(draft) = self
            .video_settings_popup
            .as_mut()
            .filter(|popup| popup.mode == VideoSettingsMode::CustomResolution)
            .and_then(|popup| popup.custom_resolution.as_mut())
            .filter(|draft| !draft.scaling_dropdown_open)
        else {
            return;
        };
        match draft.field {
            CustomResolutionField::Width => draft.width.activate(),
            CustomResolutionField::Height => draft.height.activate(),
            CustomResolutionField::Scaling => {}
        }
    }

    pub fn finish_custom_resolution_input(&mut self) {
        let Some(draft) = self
            .video_settings_popup
            .as_mut()
            .filter(|popup| popup.mode == VideoSettingsMode::CustomResolution)
            .and_then(|popup| popup.custom_resolution.as_mut())
        else {
            return;
        };
        match draft.field {
            CustomResolutionField::Width => draft.width.deactivate(),
            CustomResolutionField::Height => draft.height.deactivate(),
            CustomResolutionField::Scaling => {}
        }
    }

    pub fn move_custom_resolution_input_cursor(&mut self, direction: isize) {
        let Some(input) = self.active_custom_resolution_input_mut() else {
            return;
        };
        input.move_cursor(direction);
        input.keep_cursor_visible(14);
    }

    pub fn move_custom_resolution_input_home(&mut self, end: bool) {
        let Some(input) = self.active_custom_resolution_input_mut() else {
            return;
        };
        input.move_home(end);
        input.keep_cursor_visible(14);
    }

    pub fn delete_custom_resolution_input(&mut self) {
        if let Some(input) = self.active_custom_resolution_input_mut() {
            input.delete();
        }
    }

    fn active_custom_resolution_input_mut(&mut self) -> Option<&mut TextInputState> {
        let draft = self
            .video_settings_popup
            .as_mut()
            .filter(|popup| popup.mode == VideoSettingsMode::CustomResolution)?
            .custom_resolution
            .as_mut()?;
        let input = match draft.field {
            CustomResolutionField::Width => &mut draft.width,
            CustomResolutionField::Height => &mut draft.height,
            CustomResolutionField::Scaling => return None,
        };
        input.is_active.then_some(input)
    }

    pub fn input_custom_resolution_digit(&mut self, digit: char) {
        if !digit.is_ascii_digit() {
            return;
        }
        let Some(draft) = self
            .video_settings_popup
            .as_mut()
            .filter(|popup| popup.mode == VideoSettingsMode::CustomResolution)
            .and_then(|popup| popup.custom_resolution.as_mut())
            .filter(|draft| !draft.scaling_dropdown_open)
        else {
            return;
        };
        let input = match draft.field {
            CustomResolutionField::Width => &mut draft.width,
            CustomResolutionField::Height => &mut draft.height,
            CustomResolutionField::Scaling => return,
        };
        input.insert(digit, 20, |value| value.is_ascii_digit());
        input.keep_cursor_visible(14);
    }

    pub fn backspace_custom_resolution(&mut self) {
        let Some(draft) = self
            .video_settings_popup
            .as_mut()
            .filter(|popup| popup.mode == VideoSettingsMode::CustomResolution)
            .and_then(|popup| popup.custom_resolution.as_mut())
            .filter(|draft| !draft.scaling_dropdown_open)
        else {
            return;
        };
        match draft.field {
            CustomResolutionField::Width => {
                draft.width.backspace();
                draft.width.keep_cursor_visible(14);
            }
            CustomResolutionField::Height => {
                draft.height.backspace();
                draft.height.keep_cursor_visible(14);
            }
            CustomResolutionField::Scaling => {}
        }
    }

    fn activate_custom_resolution(&mut self) {
        let Some(draft) = self
            .video_settings_popup
            .as_mut()
            .filter(|popup| popup.mode == VideoSettingsMode::CustomResolution)
            .and_then(|popup| popup.custom_resolution.as_mut())
        else {
            return;
        };
        if draft.scaling_dropdown_open {
            self.commit_scaling_dropdown();
            self.stage_custom_resolution();
            return;
        }
        match draft.field {
            CustomResolutionField::Width => draft.field = CustomResolutionField::Height,
            CustomResolutionField::Height => draft.field = CustomResolutionField::Scaling,
            CustomResolutionField::Scaling => {
                draft.scaling_cursor = CustomScaling::OPTIONS
                    .iter()
                    .position(|scaling| *scaling == draft.scaling)
                    .unwrap_or(0);
                draft.scaling_dropdown_open = true;
            }
        }
    }

    fn commit_scaling_dropdown(&mut self) {
        let Some(draft) = self
            .video_settings_popup
            .as_mut()
            .filter(|popup| popup.mode == VideoSettingsMode::CustomResolution)
            .and_then(|popup| popup.custom_resolution.as_mut())
            .filter(|draft| draft.scaling_dropdown_open)
        else {
            return;
        };
        if let Some(scaling) = CustomScaling::OPTIONS.get(draft.scaling_cursor) {
            draft.scaling = *scaling;
        }
        draft.scaling_dropdown_open = false;
    }

    pub fn custom_resolution_error(&self) -> Option<String> {
        self.custom_resolution_from_draft().err()
    }

    fn apply_custom_resolution(&mut self) -> bool {
        if !self.stage_custom_resolution() {
            return false;
        }
        let popup = self.video_settings_popup.as_mut().unwrap();
        popup.custom_resolution = None;
        popup.mode = VideoSettingsMode::Summary;
        true
    }

    fn stage_custom_resolution(&mut self) -> bool {
        let Ok(resolution) = self.custom_resolution_from_draft() else {
            return false;
        };
        let Some(index) = self
            .video_settings_popup
            .as_ref()
            .map(|popup| popup.stream_index)
        else {
            return false;
        };
        let mut settings = self.video_settings.get(&index).copied().unwrap_or_default();
        settings.resolution = resolution;
        if self.settings_change_stream(index, settings) {
            self.video_settings.insert(index, settings);
        } else {
            self.video_settings.remove(&index);
        }
        true
    }

    fn custom_resolution_from_draft(&self) -> Result<VideoResolution, String> {
        let popup = self
            .video_settings_popup
            .as_ref()
            .filter(|popup| popup.mode == VideoSettingsMode::CustomResolution)
            .ok_or_else(|| "Custom resolution is not being edited.".to_string())?;
        let draft = popup
            .custom_resolution
            .as_ref()
            .ok_or_else(|| "Enter both width and height.".to_string())?;
        if draft.width.value.is_empty() || draft.height.value.is_empty() {
            return Err("Enter both width and height.".to_string());
        }
        let width = draft
            .width
            .value
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| "Width and height must be positive whole numbers.".to_string())?;
        let height = draft
            .height
            .value
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| "Width and height must be positive whole numbers.".to_string())?;
        if width % 2 != 0 || height % 2 != 0 {
            return Err("Width and height must be even.".to_string());
        }
        let Some((source_width, source_height)) = self.video_source_dimensions(popup.stream_index)
        else {
            return Err(
                "The source resolution is unavailable; custom scaling cannot be applied."
                    .to_string(),
            );
        };
        if width > source_width || height > source_height {
            return Err("Upscaling isn't possible yet.".to_string());
        }
        if width == source_width && height == source_height {
            return Ok(VideoResolution::Original);
        }
        Ok(VideoResolution::Custom(CustomResolution {
            width,
            height,
            scaling: draft.scaling,
        }))
    }

    pub fn video_source_dimensions(&self, index: u64) -> Option<(u64, u64)> {
        let stream = self
            .media_info()
            .and_then(|info| stream_by_index(info, index))?;
        stream_number(stream, "width").zip(stream_number(stream, "height"))
    }

    pub fn resolution_choices(&self, index: u64) -> Vec<ResolutionChoice> {
        let stream = self
            .media_info()
            .and_then(|info| stream_by_index(info, index));
        let width = stream.and_then(|stream| stream_number(stream, "width"));
        let height = stream.and_then(|stream| stream_number(stream, "height"));
        let source_dimensions = width.zip(height);
        let source_preset = source_dimensions.and_then(|dimensions| {
            VideoResolution::PRESETS
                .into_iter()
                .find(|preset| preset.dimensions() == Some(dimensions))
        });
        let staged_custom =
            self.video_settings
                .get(&index)
                .and_then(|settings| match settings.resolution {
                    VideoResolution::Custom(custom) => Some(custom),
                    _ => None,
                });
        let custom_is_current = source_preset.is_none() && source_dimensions.is_some();
        let custom = ResolutionChoice {
            value: ResolutionChoiceValue::Custom,
            label: staged_custom
                .map(|custom| format!("Custom ({}×{})", custom.width, custom.height))
                .or_else(|| {
                    custom_is_current
                        .then(|| format!("Custom ({}×{})", width.unwrap(), height.unwrap()))
                })
                .unwrap_or_else(|| "Custom…".to_string()),
            enabled: width.is_some() && height.is_some(),
            current: custom_is_current,
        };

        let mut choices = Vec::with_capacity(VideoResolution::PRESETS.len() + 1);
        for value in VideoResolution::PRESETS {
            let (preset_width, preset_height) = value.dimensions().unwrap();
            let current = source_preset == Some(value);
            choices.push(ResolutionChoice {
                label: value.label(),
                value: ResolutionChoiceValue::Resolution(if current {
                    VideoResolution::Original
                } else {
                    value
                }),
                enabled: source_dimensions.is_some_and(|(source_width, source_height)| {
                    preset_width <= source_width && preset_height <= source_height
                }),
                current,
            });
        }
        choices.push(custom);
        choices
    }

    pub fn video_codec_choices(&self, index: u64) -> Vec<VideoCodecChoice> {
        let source_name = self
            .media_info()
            .and_then(|info| stream_by_index(info, index))
            .and_then(|stream| stream.get("codec_name"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let source_codec = match source_name {
            "h264" => Some(VideoCodec::H264),
            "hevc" => Some(VideoCodec::Hevc),
            "av1" => Some(VideoCodec::Av1),
            _ => None,
        };
        let mut choices = Vec::with_capacity(VideoCodec::TARGETS.len() + 1);
        if source_codec.is_none() {
            let enabled = self
                .effective_container()
                .is_none_or(|container| container.supports_codec("video", source_name, false));
            choices.push(VideoCodecChoice {
                value: VideoCodec::Original,
                label: source_name.to_uppercase(),
                current: true,
                enabled,
                reason: (!enabled).then(|| {
                    format!(
                        "{} cannot contain {} video",
                        self.effective_container().unwrap().label(),
                        source_name.to_uppercase()
                    )
                }),
            });
        }
        choices.extend(VideoCodec::TARGETS.into_iter().map(|codec| {
            let current = source_codec == Some(codec);
            let codec_name = codec.codec_name().unwrap();
            let enabled = self
                .effective_container()
                .is_none_or(|container| container.supports_codec("video", codec_name, false));
            VideoCodecChoice {
                value: if current { VideoCodec::Original } else { codec },
                label: codec.label().to_string(),
                current,
                enabled,
                reason: (!enabled).then(|| {
                    format!(
                        "{} cannot contain {} video",
                        self.effective_container().unwrap().label(),
                        codec.label()
                    )
                }),
            }
        }));
        choices
    }

    fn settings_change_stream(&self, index: u64, settings: VideoSettings) -> bool {
        if settings.resolution != VideoResolution::Original {
            return true;
        }
        let source_codec = self
            .media_info()
            .and_then(|info| stream_by_index(info, index))
            .and_then(|stream| stream.get("codec_name"))
            .and_then(serde_json::Value::as_str);
        match settings.codec {
            VideoCodec::Original => false,
            VideoCodec::H264 => source_codec != Some("h264"),
            VideoCodec::Hevc => source_codec != Some("hevc"),
            VideoCodec::Av1 => source_codec != Some("av1"),
        }
    }

    pub fn request_save(&mut self) {
        if self.layer != Layer::Streams || self.dialog.is_some() {
            return;
        }
        if !self.has_track_edits() {
            self.notice = Some("No media changes to save.".to_string());
            return;
        }
        let Some(info) = self.media_info() else {
            return;
        };
        let order = final_stream_order(info, &self.stream_order, &self.deleted_streams);
        let defaults = self
            .default_streams
            .difference(&self.deleted_streams)
            .copied()
            .collect();
        if let Err(error) = validate_edit(
            info,
            &order,
            &self.deleted_streams,
            &defaults,
            &self.video_settings,
        ) {
            self.show_error(error);
            return;
        }
        if let Some(error) = self.subtitle_language_error() {
            self.show_error(error);
            return;
        }
        let conflicts = self.selected_container_conflicts();
        if !conflicts.is_empty() {
            self.show_error(format!(
                "Resolve the container compatibility issues before saving:\n{}",
                conflicts.join("\n")
            ));
            return;
        }
        self.notice = None;
        self.save_destination = SaveDestination::ReplaceOriginal;
        self.save_dialog_field = SaveDialogField::Start;
        self.dialog = Some(Dialog::ConfirmSave);
    }

    fn subtitle_language_error(&self) -> Option<String> {
        if let Some(info) = self.media_info() {
            for stream in info
                .streams
                .iter()
                .filter(|stream| stream_kind(stream) == Some("subtitle"))
            {
                let index = stream_index(stream)?;
                if self.deleted_streams.contains(&index) {
                    continue;
                }
                let source = SubtitleSource::Embedded(index);
                let language = self
                    .subtitle_metadata_for(&source)
                    .map(|metadata| metadata.language)
                    .unwrap_or_else(|| stream_language(stream));
                if language_choice(&language).is_none() {
                    return Some(format!(
                        "Choose a language for subtitle track #{index}; Undetermined is not allowed."
                    ));
                }
            }
        }
        for sidecar in &self.sidecars {
            let source = SubtitleSource::Sidecar(sidecar.path.clone());
            let language = self
                .subtitle_metadata_for(&source)
                .map(|metadata| metadata.language)
                .unwrap_or_else(|| sidecar.language.clone());
            if language_choice(&language).is_none() {
                return Some(format!(
                    "Choose a language for {}; Undetermined is not allowed.",
                    sidecar.display_name
                ));
            }
        }
        None
    }

    pub fn move_save_dialog_cursor(&mut self, direction: isize) {
        if self.dialog != Some(Dialog::ConfirmSave) || direction == 0 {
            return;
        }
        if !self.media_will_change() {
            self.save_dialog_field = SaveDialogField::Start;
            return;
        }
        self.save_dialog_field = match (self.save_dialog_field, direction.is_positive()) {
            (SaveDialogField::Destination, true) => SaveDialogField::Start,
            (SaveDialogField::Start, false) => SaveDialogField::Destination,
            (field, _) => field,
        };
    }

    pub fn move_save_dialog_to_endpoint(&mut self, end: bool) {
        if self.dialog != Some(Dialog::ConfirmSave) {
            return;
        }
        self.save_dialog_field = if end || !self.media_will_change() {
            SaveDialogField::Start
        } else {
            SaveDialogField::Destination
        };
    }

    pub fn choose_save_destination(&mut self, direction: isize) {
        if self.dialog != Some(Dialog::ConfirmSave)
            || !self.media_will_change()
            || self.save_dialog_field != SaveDialogField::Destination
            || direction == 0
        {
            return;
        }
        self.save_destination = if direction.is_positive() {
            SaveDestination::CreateCopy
        } else {
            SaveDestination::ReplaceOriginal
        };
    }

    pub fn activate_save_dialog(&mut self) {
        if self.dialog != Some(Dialog::ConfirmSave) {
            return;
        }
        match self.save_dialog_field {
            SaveDialogField::Destination => {
                if !self.media_will_change() {
                    self.save_dialog_field = SaveDialogField::Start;
                    return;
                }
                self.save_destination = match self.save_destination {
                    SaveDestination::ReplaceOriginal => SaveDestination::CreateCopy,
                    SaveDestination::CreateCopy => SaveDestination::ReplaceOriginal,
                };
            }
            SaveDialogField::Start => self.confirm_save(),
        }
    }

    pub fn confirm_save(&mut self) {
        if self.dialog != Some(Dialog::ConfirmSave) {
            return;
        }
        let Some(path) = self.selected_file().map(|file| file.path.clone()) else {
            self.dialog = Some(Dialog::Error);
            self.edit_error = Some("The selected file is no longer available.".to_string());
            return;
        };
        let Some(info) = self.media_info() else {
            self.show_error("The selected file no longer has track information.");
            return;
        };
        let stream_order = final_stream_order(info, &self.stream_order, &self.deleted_streams);
        let default_streams = self
            .default_streams
            .difference(&self.deleted_streams)
            .copied()
            .collect();
        let cancelled = Arc::new(AtomicBool::new(false));
        let request = EditRequest {
            path,
            destination: self.save_destination,
            container: self.container_target,
            stream_order,
            deleted_streams: self.deleted_streams.clone(),
            default_streams,
            default_sidecars: self.default_sidecars.clone(),
            video_settings: self.video_settings.clone(),
            subtitle_changes: self.subtitle_changes.values().cloned().collect(),
            left_subtitle_order: self.active_left_subtitle_tracks(),
            sidecars: self.sidecars.clone(),
            cancelled: cancelled.clone(),
        };
        match self.edit_tx.send(request) {
            Ok(()) => {
                self.dialog = Some(Dialog::Processing);
                self.edit_error = None;
                self.edit_progress = None;
                self.edit_progress_label = None;
                self.edit_started = Some(Instant::now());
                self.cancel_edit_choice = CancelEditChoice::KeepProcessing;
                self.edit_cancel = Some(cancelled);
            }
            Err(error) => {
                self.dialog = Some(Dialog::Error);
                self.edit_error = Some(format!("Could not start the edit worker: {error}"));
            }
        }
    }

    pub fn cancel_edit(&mut self) {
        if !matches!(
            self.dialog,
            Some(Dialog::Processing | Dialog::ConfirmCancel)
        ) {
            return;
        }
        if let Some(cancelled) = &self.edit_cancel {
            cancelled.store(true, Ordering::Relaxed);
        }
        self.dialog = Some(Dialog::Processing);
        self.edit_error = None;
        self.edit_progress = None;
        self.edit_progress_label = Some("Stopping active tools".to_string());
        self.cancel_edit_choice = CancelEditChoice::KeepProcessing;
    }

    pub fn request_cancel_edit(&mut self) {
        if self.dialog != Some(Dialog::Processing) {
            return;
        }
        self.cancel_edit_choice = CancelEditChoice::KeepProcessing;
        self.dialog = Some(Dialog::ConfirmCancel);
    }

    pub fn choose_cancel_edit(&mut self, direction: isize) {
        if self.dialog != Some(Dialog::ConfirmCancel) || direction == 0 {
            return;
        }
        self.cancel_edit_choice = if direction.is_positive() {
            CancelEditChoice::CancelProcessing
        } else {
            CancelEditChoice::KeepProcessing
        };
    }

    pub fn choose_cancel_edit_endpoint(&mut self, end: bool) {
        if self.dialog == Some(Dialog::ConfirmCancel) {
            self.cancel_edit_choice = if end {
                CancelEditChoice::CancelProcessing
            } else {
                CancelEditChoice::KeepProcessing
            };
        }
    }

    pub fn activate_cancel_edit(&mut self) {
        if self.dialog != Some(Dialog::ConfirmCancel) {
            return;
        }
        match self.cancel_edit_choice {
            CancelEditChoice::KeepProcessing => self.dismiss_cancel_edit(),
            CancelEditChoice::CancelProcessing => self.cancel_edit(),
        }
    }

    pub fn dismiss_cancel_edit(&mut self) {
        if self.dialog == Some(Dialog::ConfirmCancel) {
            self.cancel_edit_choice = CancelEditChoice::KeepProcessing;
            self.dialog = Some(Dialog::Processing);
        }
    }

    pub fn dismiss_dialog(&mut self) {
        if matches!(
            self.dialog,
            Some(Dialog::Processing | Dialog::ConfirmCancel)
        ) {
            return;
        }
        self.dialog = None;
        self.edit_error = None;
        self.edit_progress = None;
        self.edit_progress_label = None;
        self.edit_started = None;
        self.edit_cancel = None;
        self.keybindings_search.clear();
    }

    pub fn show_keybindings(&mut self) {
        if self.dialog.is_none() {
            self.keybindings_scroll = 0;
            self.keybindings_max_scroll = 0;
            self.keybindings_search.clear();
            self.dialog = Some(Dialog::Keybindings);
        }
    }

    pub fn start_keybindings_search(&mut self) {
        self.keybindings_search.activate();
    }

    pub fn keybindings_search_push(&mut self, ch: char) {
        self.keybindings_search.push_char(ch);
        self.keybindings_scroll = 0;
    }

    pub fn keybindings_search_pop(&mut self) {
        self.keybindings_search.pop_char();
        self.keybindings_scroll = 0;
    }

    pub fn keybindings_search_delete(&mut self) {
        self.keybindings_search.delete_char();
        self.keybindings_scroll = 0;
    }

    pub fn move_keybindings_search_cursor(&mut self, direction: isize) {
        self.keybindings_search.move_cursor(direction);
    }

    pub fn move_keybindings_search_home(&mut self, end: bool) {
        self.keybindings_search.move_home(end);
    }

    pub fn finish_keybindings_search(&mut self) {
        self.keybindings_search.deactivate();
    }

    pub fn clear_keybindings_search(&mut self) {
        self.keybindings_search.clear();
        self.keybindings_scroll = 0;
    }

    pub fn start_file_search(&mut self) {
        if self.layer != Layer::Files {
            return;
        }
        if self.file_search.value.is_empty() {
            self.file_search_origin = self.selected_file().map(|file| file.path.clone());
        }
        self.file_search.activate();
    }

    pub fn file_search_push(&mut self, ch: char) {
        let selected_path = self.selected_file().map(|file| file.path.clone());
        self.file_search.push_char(ch);
        self.reselect_file_after_view_change(selected_path.clone(), selected_path);
    }

    pub fn file_search_pop(&mut self) {
        let selected_path = self.selected_file().map(|file| file.path.clone());
        self.file_search.pop_char();
        if !self.file_search.is_active {
            self.file_search_origin = None;
        }
        self.reselect_file_after_view_change(selected_path.clone(), selected_path);
    }

    pub fn file_search_delete(&mut self) {
        let selected_path = self.selected_file().map(|file| file.path.clone());
        self.file_search.delete_char();
        self.reselect_file_after_view_change(selected_path.clone(), selected_path);
    }

    pub fn move_file_search_cursor(&mut self, direction: isize) {
        self.file_search.move_cursor(direction);
    }

    pub fn move_file_search_home(&mut self, end: bool) {
        self.file_search.move_home(end);
    }

    pub fn finish_file_search(&mut self) {
        self.file_search.deactivate();
        self.file_search_origin = None;
    }

    pub fn cancel_file_search(&mut self) {
        let selected_path = self.selected_file().map(|file| file.path.clone());
        let original_path = self.file_search_origin.take();
        self.file_search.clear();
        self.reselect_file_after_view_change(selected_path, original_path);
    }

    pub fn clear_file_search(&mut self) {
        let selected_path = self.selected_file().map(|file| file.path.clone());
        self.file_search.clear();
        self.file_search_origin = None;
        self.reselect_file_after_view_change(selected_path.clone(), selected_path);
    }

    fn reselect_file_after_view_change(
        &mut self,
        previous_path: Option<PathBuf>,
        preferred_path: Option<PathBuf>,
    ) {
        let preferred_position = preferred_path
            .as_deref()
            .and_then(|path| self.file_panel_position(path));
        let selection =
            preferred_position.or_else(|| (!self.file_panel_entries().is_empty()).then_some(0));
        let next_path = selection
            .and_then(|position| self.file_panel_entries().get(position).cloned())
            .and_then(|entry| self.files.get(entry.file_index))
            .map(|file| file.path.clone());
        let force = previous_path != next_path;
        self.select_file_position_with_force(selection, force);
    }

    pub fn scroll_keybindings_down(&mut self, amount: u16) {
        self.keybindings_scroll =
            scroll_forward(self.keybindings_scroll, self.keybindings_max_scroll, amount);
    }

    pub fn scroll_keybindings_up(&mut self, amount: u16) {
        self.keybindings_scroll = scroll_backward(self.keybindings_scroll, amount);
    }

    pub fn scroll_keybindings_to_start(&mut self) {
        self.keybindings_scroll = 0;
    }

    pub fn scroll_keybindings_to_end(&mut self) {
        self.keybindings_scroll = self.keybindings_max_scroll;
    }

    pub fn set_keybindings_max_scroll(&mut self, maximum: u16) {
        self.keybindings_max_scroll = maximum;
        self.keybindings_scroll = self.keybindings_scroll.min(maximum);
    }

    pub fn scroll_down(&mut self) {
        self.notice = None;
        match self.layer {
            Layer::Files => {
                let result_count = self.file_panel_entries().len();
                if result_count > 0 {
                    let current = self.list_state.selected().unwrap_or(0);
                    self.select(current.saturating_add(10).min(result_count - 1));
                }
            }
            Layer::Streams => {
                if self.move_within_subtitle_column(1, 10, true) {
                    return;
                }
                let count = self.stream_count();
                if count > 0 {
                    self.selected_stream = self.selected_stream.saturating_add(10).min(count - 1);
                }
            }
            Layer::StreamDetails => self.scroll_details_down(10),
        }
    }

    pub fn scroll_up(&mut self) {
        self.notice = None;
        match self.layer {
            Layer::Files => {
                if !self.file_panel_entries().is_empty() {
                    let current = self.list_state.selected().unwrap_or(0);
                    self.select(current.saturating_sub(10));
                }
            }
            Layer::Streams => {
                if self.move_within_subtitle_column(-1, 10, true) {
                    return;
                }
                self.selected_stream = self.selected_stream.saturating_sub(10);
            }
            Layer::StreamDetails => self.scroll_details_up(10),
        }
    }

    fn scroll_details_down(&mut self, amount: u16) {
        self.details_scroll = scroll_forward(self.details_scroll, self.details_max_scroll, amount);
    }

    fn scroll_details_up(&mut self, amount: u16) {
        self.details_scroll = scroll_backward(self.details_scroll, amount);
    }

    pub fn set_details_max_scroll(&mut self, maximum: u16) {
        self.details_max_scroll = maximum;
        self.details_scroll = self.details_scroll.min(maximum);
    }

    fn clear_edit_state(&mut self) {
        self.clear_track_edits();
        self.dialog = None;
        self.notice = None;
        self.edit_error = None;
        self.edit_progress = None;
        self.edit_progress_label = None;
        self.edit_started = None;
    }

    fn reset_track_edits(&mut self) {
        let Some(info) = self.media_info() else {
            self.clear_track_edits();
            return;
        };
        let order = grouped_stream_indices(info)
            .into_iter()
            .filter_map(|position| info.streams.get(position).and_then(stream_index))
            .collect::<Vec<_>>();
        let defaults = info
            .streams
            .iter()
            .filter(|stream| is_default(stream))
            .filter_map(stream_index)
            .collect::<BTreeSet<_>>();
        self.stream_order = order.clone();
        self.original_stream_order = order;
        self.moved_streams.clear();
        self.default_streams = defaults.clone();
        self.original_default_streams = defaults;
        self.default_sidecars.clear();
        self.deleted_streams.clear();
        self.video_settings.clear();
        self.video_settings_popup = None;
        self.subtitle_changes.clear();
        self.subtitle_settings_popup = None;
        self.container_target = None;
        self.container_settings_popup = None;
    }

    fn clear_track_edits(&mut self) {
        self.stream_order.clear();
        self.original_stream_order.clear();
        self.left_subtitle_order.clear();
        self.moved_streams.clear();
        self.deleted_streams.clear();
        self.default_streams.clear();
        self.original_default_streams.clear();
        self.default_sidecars.clear();
        self.video_settings.clear();
        self.video_settings_popup = None;
        self.subtitle_changes.clear();
        self.subtitle_settings_popup = None;
        self.container_target = None;
        self.container_settings_popup = None;
    }

    pub fn changed_streams(&self) -> BTreeSet<u64> {
        let mut changed = self
            .moved_streams
            .iter()
            .copied()
            .filter(|index| {
                stream_position_changed(
                    &self.original_stream_order,
                    &self.stream_order,
                    &self.deleted_streams,
                    self.media_info(),
                    *index,
                )
            })
            .collect::<BTreeSet<_>>();
        changed.extend(changed_default_streams(
            &self.original_stream_order,
            &self.deleted_streams,
            &self.original_default_streams,
            &self.default_streams,
        ));
        changed.extend(self.video_settings.keys().copied());
        changed.extend(
            self.subtitle_changes
                .keys()
                .filter_map(|source| match source {
                    SubtitleSource::Embedded(index) => Some(*index),
                    SubtitleSource::Sidecar(_) => None,
                }),
        );
        changed
    }

    pub fn has_track_edits(&self) -> bool {
        self.container_target.is_some()
            || !self.deleted_streams.is_empty()
            || !self.default_sidecars.is_empty()
            || !changed_streams(
                &self.original_stream_order,
                &self.stream_order,
                &self.deleted_streams,
                &self.original_default_streams,
                &self.default_streams,
                self.media_info(),
            )
            .is_empty()
            || !self.video_settings.is_empty()
            || self
                .subtitle_changes
                .values()
                .any(SubtitleChange::has_effect)
    }

    pub fn media_will_change(&self) -> bool {
        self.container_target.is_some()
            || !self.deleted_streams.is_empty()
            || !self.default_sidecars.is_empty()
            || !changed_streams(
                &self.original_stream_order,
                &self.stream_order,
                &self.deleted_streams,
                &self.original_default_streams,
                &self.default_streams,
                self.media_info(),
            )
            .is_empty()
            || !self.video_settings.is_empty()
            || self
                .subtitle_changes
                .values()
                .any(SubtitleChange::changes_media)
    }

    pub fn processing_description(&self) -> String {
        let mut descriptions = Vec::new();

        if let Some(target) = self.container_target {
            descriptions.push(match self.source_container() {
                Some(source) => {
                    format!("Converting {} to {}", source.label(), target.label())
                }
                None => format!("Converting container to {}", target.label()),
            });
        }

        let imported_subtitles = self
            .subtitle_changes
            .values()
            .filter(|change| change.import_into_media)
            .count();
        if imported_subtitles > 0 {
            descriptions.push(format!(
                "Importing {imported_subtitles} subtitle{}",
                if imported_subtitles == 1 { "" } else { "s" }
            ));
        }

        let exported_subtitles = self
            .subtitle_changes
            .values()
            .filter(|change| change.export_target.is_some())
            .count();
        if exported_subtitles > 0 {
            descriptions.push(format!(
                "Exporting {exported_subtitles} subtitle{}",
                if exported_subtitles == 1 { "" } else { "s" }
            ));
        }

        if descriptions.len() < 2 && !self.video_settings.is_empty() {
            descriptions.push(format!(
                "Transcoding {} video track{}",
                self.video_settings.len(),
                if self.video_settings.len() == 1 {
                    ""
                } else {
                    "s"
                }
            ));
        }

        let converted_subtitles = self
            .subtitle_changes
            .values()
            .filter(|change| {
                change
                    .embedded_target
                    .is_some_and(|target| target != change.source_format)
            })
            .count();
        if descriptions.len() < 2
            && imported_subtitles == 0
            && exported_subtitles == 0
            && converted_subtitles > 0
        {
            descriptions.push(format!(
                "Converting {converted_subtitles} subtitle{}",
                if converted_subtitles == 1 { "" } else { "s" }
            ));
        }

        let metadata_edits = self
            .subtitle_changes
            .values()
            .filter(|change| change.metadata.is_some())
            .count();
        if descriptions.len() < 2 && metadata_edits > 0 {
            descriptions.push(format!(
                "Updating {metadata_edits} subtitle metadata record{}",
                if metadata_edits == 1 { "" } else { "s" }
            ));
        }

        if descriptions.len() < 2
            && let Some(info) = self.media_info()
        {
            descriptions.extend(
                edit_summary(
                    info,
                    &self.original_stream_order,
                    &self.stream_order,
                    &self.moved_streams,
                    &self.deleted_streams,
                    &self.original_default_streams,
                    &self.default_streams,
                )
                .into_iter()
                .take(2 - descriptions.len()),
            );
        }

        if descriptions.is_empty() {
            "Remuxing media".to_string()
        } else {
            descriptions.join(" · ")
        }
    }

    pub fn save_summary(&self) -> Vec<String> {
        let Some(info) = self.media_info() else {
            return Vec::new();
        };
        let mut lines = edit_summary(
            info,
            &self.original_stream_order,
            &self.stream_order,
            &self.moved_streams,
            &self.deleted_streams,
            &self.original_default_streams,
            &self.default_streams,
        );
        if let Some(target) = self.container_target {
            let source = self
                .source_container()
                .map(ContainerFormat::label)
                .unwrap_or("original");
            lines.insert(
                0,
                format!("Changing container from {source} to {}", target.label()),
            );
        }
        for (index, settings) in &self.video_settings {
            let codec = match settings.codec {
                VideoCodec::Original => self
                    .media_info()
                    .and_then(|info| stream_by_index(info, *index))
                    .and_then(|stream| stream.get("codec_name"))
                    .and_then(serde_json::Value::as_str)
                    .map(|codec| codec.to_uppercase())
                    .unwrap_or_else(|| "original codec".to_string()),
                codec => codec.label().to_string(),
            };
            let resolution = settings.resolution.label();
            lines.push(match settings.resolution {
                VideoResolution::Original => {
                    format!("Encoding video track #{index} as {codec}")
                }
                _ => format!("Encoding video track #{index} as {codec} at {resolution}"),
            });
        }
        for change in self.subtitle_changes.values() {
            let source = match &change.source {
                SubtitleSource::Embedded(index) => format!("subtitle track #{index}"),
                SubtitleSource::Sidecar(path) => path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("subtitle sidecar")
                    .to_string(),
            };
            if change.import_into_media {
                let target = change.embedded_target.unwrap_or(change.source_format);
                lines.push(format!("Importing {source} as {}", target.label()));
                continue;
            }
            if let Some(target) = change
                .embedded_target
                .filter(|_| change.export_target.is_none())
            {
                lines.push(match change.source {
                    SubtitleSource::Embedded(_) => {
                        format!("Converting {source} in the media to {}", target.label())
                    }
                    SubtitleSource::Sidecar(_) => {
                        format!("Converting {source} to {}", target.label())
                    }
                });
            }
            if let Some(target) = change.export_target {
                lines.push(format!("Exporting {source} as {}", target.label()));
            }
            if let Some(metadata) = &change.metadata {
                let mut values = vec![format!("language {}", metadata.language.to_uppercase())];
                values.push(match &metadata.title {
                    Some(title) => format!("title “{title}”"),
                    None => "no title".to_string(),
                });
                for (enabled, label) in [
                    (metadata.forced, "Forced"),
                    (metadata.cc, "CC"),
                    (metadata.hearing_impaired, "Hearing impaired"),
                    (metadata.original, "Original"),
                    (metadata.commentary, "Commentary"),
                ] {
                    if enabled {
                        values.push(label.to_string());
                    }
                }
                lines.push(format!("Updating {source} metadata: {}", values.join(", ")));
            }
        }
        lines
    }

    fn show_error(&mut self, error: impl Into<String>) {
        self.notice = None;
        self.edit_error = Some(error.into());
        self.dialog = Some(Dialog::Error);
    }
}

fn scroll_forward(current: u16, maximum: u16, amount: u16) -> u16 {
    current.saturating_add(amount).min(maximum)
}

fn char_byte_index(value: &str, character_index: usize) -> usize {
    value
        .char_indices()
        .nth(character_index)
        .map_or(value.len(), |(index, _)| index)
}

fn move_cursor(
    current: usize,
    length: usize,
    direction: isize,
    enabled: impl Fn(usize) -> bool,
) -> usize {
    if length == 0 || direction == 0 {
        return current;
    }
    let mut position = current;
    loop {
        let Some(next) = position.checked_add_signed(direction.signum()) else {
            return current;
        };
        if next >= length {
            return current;
        }
        position = next;
        if enabled(position) {
            return position;
        }
    }
}

fn cursor_endpoint(length: usize, end: bool, enabled: impl Fn(usize) -> bool) -> Option<usize> {
    if end {
        (0..length).rev().find(|position| enabled(*position))
    } else {
        (0..length).find(|position| enabled(*position))
    }
}

fn stream_number(
    stream: &std::collections::BTreeMap<String, serde_json::Value>,
    name: &str,
) -> Option<u64> {
    stream.get(name).and_then(|value| match value {
        serde_json::Value::Number(number) => number.as_u64(),
        serde_json::Value::String(number) => number.parse().ok(),
        _ => None,
    })
}

fn scroll_backward(current: u16, amount: u16) -> u16 {
    current.saturating_sub(amount)
}

pub fn grouped_stream_indices(info: &MediaInfo) -> Vec<usize> {
    ["video", "audio", "subtitle"]
        .into_iter()
        .flat_map(|kind| {
            info.streams
                .iter()
                .enumerate()
                .filter_map(move |(index, stream)| {
                    (stream.get("codec_type").and_then(serde_json::Value::as_str) == Some(kind))
                        .then_some(index)
                })
        })
        .chain(
            info.streams
                .iter()
                .enumerate()
                .filter_map(|(index, stream)| {
                    (!matches!(
                        stream.get("codec_type").and_then(serde_json::Value::as_str),
                        Some("video" | "audio" | "subtitle")
                    ))
                    .then_some(index)
                }),
        )
        .collect()
}

fn stream_by_index(
    info: &MediaInfo,
    index: u64,
) -> Option<&std::collections::BTreeMap<String, serde_json::Value>> {
    info.streams
        .iter()
        .find(|stream| stream_index(stream) == Some(index))
}

fn stream_kind(stream: &std::collections::BTreeMap<String, serde_json::Value>) -> Option<&str> {
    stream.get("codec_type").and_then(serde_json::Value::as_str)
}

pub(crate) fn stream_group(
    stream: &std::collections::BTreeMap<String, serde_json::Value>,
) -> &'static str {
    match stream_kind(stream) {
        Some("video") => "video",
        Some("audio") => "audio",
        Some("subtitle") => "subtitle",
        _ => "other",
    }
}

fn is_default(stream: &std::collections::BTreeMap<String, serde_json::Value>) -> bool {
    stream
        .get("disposition")
        .and_then(serde_json::Value::as_object)
        .and_then(|disposition| disposition.get("default"))
        .and_then(serde_json::Value::as_i64)
        == Some(1)
}

pub(crate) fn final_stream_order(
    info: &MediaInfo,
    staged_order: &[u64],
    deleted: &BTreeSet<u64>,
) -> Vec<u64> {
    let mut queues: BTreeMap<&'static str, VecDeque<u64>> = BTreeMap::new();
    for index in staged_order.iter().filter(|index| !deleted.contains(index)) {
        if let Some(stream) = stream_by_index(info, *index) {
            queues
                .entry(stream_group(stream))
                .or_default()
                .push_back(*index);
        }
    }

    info.streams
        .iter()
        .filter_map(|stream| {
            let index = stream_index(stream)?;
            if deleted.contains(&index) {
                return None;
            }
            queues.get_mut(stream_group(stream))?.pop_front()
        })
        .collect()
}

fn effective_group_order(
    info: &MediaInfo,
    order: &[u64],
    deleted: &BTreeSet<u64>,
    group: &str,
) -> Vec<u64> {
    order
        .iter()
        .filter(|index| !deleted.contains(index))
        .filter(|index| {
            stream_by_index(info, **index).is_some_and(|stream| stream_group(stream) == group)
        })
        .copied()
        .collect()
}

fn changed_streams(
    original_order: &[u64],
    staged_order: &[u64],
    deleted: &BTreeSet<u64>,
    original_defaults: &BTreeSet<u64>,
    staged_defaults: &BTreeSet<u64>,
    info: Option<&MediaInfo>,
) -> BTreeSet<u64> {
    let Some(info) = info else {
        return BTreeSet::new();
    };
    let mut changed = BTreeSet::new();
    for group in ["video", "audio", "subtitle", "other"] {
        let original = effective_group_order(info, original_order, deleted, group);
        let staged = effective_group_order(info, staged_order, deleted, group);
        for (position, index) in staged.iter().enumerate() {
            if original.get(position) != Some(index) {
                changed.insert(*index);
                if let Some(original_index) = original.get(position) {
                    changed.insert(*original_index);
                }
            }
        }
    }
    changed.extend(changed_default_streams(
        original_order,
        deleted,
        original_defaults,
        staged_defaults,
    ));
    changed
}

fn changed_default_streams(
    original_order: &[u64],
    deleted: &BTreeSet<u64>,
    original_defaults: &BTreeSet<u64>,
    staged_defaults: &BTreeSet<u64>,
) -> BTreeSet<u64> {
    original_order
        .iter()
        .filter(|index| !deleted.contains(index))
        .filter(|index| original_defaults.contains(index) != staged_defaults.contains(index))
        .copied()
        .collect()
}

fn stream_position_changed(
    original_order: &[u64],
    staged_order: &[u64],
    deleted: &BTreeSet<u64>,
    info: Option<&MediaInfo>,
    index: u64,
) -> bool {
    let Some(info) = info else {
        return false;
    };
    let Some(group) = stream_by_index(info, index).map(stream_group) else {
        return false;
    };
    let original = effective_group_order(info, original_order, deleted, group);
    let staged = effective_group_order(info, staged_order, deleted, group);
    original.iter().position(|candidate| *candidate == index)
        != staged.iter().position(|candidate| *candidate == index)
}

fn edit_summary(
    info: &MediaInfo,
    original_order: &[u64],
    staged_order: &[u64],
    moved_streams: &BTreeSet<u64>,
    deleted: &BTreeSet<u64>,
    original_defaults: &BTreeSet<u64>,
    staged_defaults: &BTreeSet<u64>,
) -> Vec<String> {
    let mut lines = Vec::new();
    for group in ["video", "audio", "subtitle", "other"] {
        let moved = moved_streams
            .iter()
            .filter(|index| {
                stream_by_index(info, **index).is_some_and(|stream| stream_group(stream) == group)
                    && stream_position_changed(
                        original_order,
                        staged_order,
                        deleted,
                        Some(info),
                        **index,
                    )
            })
            .count();
        if moved > 0 {
            lines.push(format!(
                "Moving {moved} {}",
                track_count_label(group, moved)
            ));
        }
    }

    for group in ["video", "audio", "subtitle", "other"] {
        let count = deleted
            .iter()
            .filter(|index| {
                stream_by_index(info, **index).is_some_and(|stream| stream_group(stream) == group)
            })
            .count();
        if count > 0 {
            lines.push(format!(
                "Deleting {count} {}",
                track_count_label(group, count)
            ));
        }
    }

    for kind in ["video", "audio", "subtitle"] {
        let original = info
            .streams
            .iter()
            .filter(|stream| stream_kind(stream) == Some(kind))
            .filter_map(stream_index)
            .filter(|index| !deleted.contains(index) && original_defaults.contains(index))
            .collect::<BTreeSet<_>>();
        let staged = info
            .streams
            .iter()
            .filter(|stream| stream_kind(stream) == Some(kind))
            .filter_map(stream_index)
            .filter(|index| !deleted.contains(index) && staged_defaults.contains(index))
            .collect::<BTreeSet<_>>();
        if original != staged {
            lines.push(format!("Changing the default {kind} track"));
        }
    }
    lines
}

fn track_count_label(group: &str, count: usize) -> String {
    format!("{group} track{}", if count == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use kernal::prelude::*;

    use super::*;

    fn media(streams: serde_json::Value) -> MediaInfo {
        MediaInfo::from_json(serde_json::json!({"streams": streams})).unwrap()
    }

    fn test_app(info: MediaInfo) -> App {
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-app-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let (probe_tx, _) = std::sync::mpsc::channel::<ProbeRequest>();
        let (edit_tx, _) = std::sync::mpsc::channel::<EditRequest>();
        let mut app = App::new(directory, probe_tx, edit_tx).unwrap();
        app.outcome = Some(ProbeOutcome::Video(info));
        app.loading = false;
        app.reset_track_edits();
        app.layer = Layer::Streams;
        app
    }

    fn test_file_app(names: &[&str]) -> App {
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-live-app-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        for name in names {
            std::fs::write(directory.join(name), b"media").unwrap();
        }
        let (probe_tx, _) = std::sync::mpsc::channel::<ProbeRequest>();
        let (edit_tx, _) = std::sync::mpsc::channel::<EditRequest>();
        let mut app = App::new(directory, probe_tx, edit_tx).unwrap();
        app.outcome = Some(ProbeOutcome::Video(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "disposition": {"default": 1}},
            {"index": 1, "codec_type": "audio", "disposition": {"default": 1}},
            {"index": 2, "codec_type": "subtitle", "disposition": {"default": 0}}
        ]))));
        app.loading = false;
        app.reset_track_edits();
        app.layer = Layer::Streams;
        app
    }

    fn set_media(app: &mut App, streams: serde_json::Value) {
        app.outcome = Some(ProbeOutcome::Video(media(streams)));
        app.loading = false;
        app.reset_track_edits();
        app.layer = Layer::Streams;
    }

    fn test_sidecar(app: &App, name: &str, language: &str) -> SidecarEntry {
        SidecarEntry {
            path: app.directory.join(name),
            companion: None,
            display_name: name.to_string(),
            format: SubtitleFormat::SubRip,
            language: language.to_string(),
            forced: false,
            hearing_impaired: false,
            number: None,
            fingerprint: crate::files::FileFingerprint {
                length: 0,
                modified: None,
            },
            companion_fingerprint: None,
        }
    }

    #[test]
    fn scroll_forward_should_add_amount_when_result_is_below_maximum() {
        // Arrange
        let current = 4;
        let maximum = 10;
        let amount = 3;

        // Act
        let result = scroll_forward(current, maximum, amount);

        // Assert
        assert_that!(result).is_equal_to(7);
    }

    #[test]
    fn scroll_forward_should_clamp_to_maximum_when_amount_exceeds_remaining_range() {
        // Arrange
        let current = 7;
        let maximum = 10;
        let amount = 10;

        // Act
        let result = scroll_forward(current, maximum, amount);

        // Assert
        assert_that!(result).is_equal_to(10);
    }

    #[test]
    fn scroll_forward_should_clamp_to_maximum_when_current_value_is_above_maximum() {
        // Arrange
        let current = u16::MAX;
        let maximum = 10;
        let amount = 1;

        // Act
        let result = scroll_forward(current, maximum, amount);

        // Assert
        assert_that!(result).is_equal_to(10);
    }

    #[test]
    fn scroll_backward_should_subtract_amount_when_result_is_above_zero() {
        // Arrange
        let current = 7;
        let amount = 3;

        // Act
        let result = scroll_backward(current, amount);

        // Assert
        assert_that!(result).is_equal_to(4);
    }

    #[test]
    fn scroll_backward_should_return_zero_when_amount_exceeds_current_value() {
        // Arrange
        let current = 2;
        let amount = 10;

        // Act
        let result = scroll_backward(current, amount);

        // Assert
        assert_that!(result).is_equal_to(0);
    }

    #[test]
    fn processing_description_should_describe_container_conversion_and_subtitle_exports() {
        // Arrange
        let mut app = test_file_app(&["alpha.mkv"]);
        let directory = app.directory.clone();
        app.container_target = Some(ContainerFormat::Mp4);
        app.subtitle_changes.insert(
            SubtitleSource::Embedded(2),
            SubtitleChange {
                source: SubtitleSource::Embedded(2),
                source_format: SubtitleFormat::SubRip,
                embedded_target: None,
                export_target: Some(SubtitleFormat::SubRip),
                import_into_media: false,
                ocr_language: None,

                metadata: None,
            },
        );
        app.subtitle_changes.insert(
            SubtitleSource::Embedded(3),
            SubtitleChange {
                source: SubtitleSource::Embedded(3),
                source_format: SubtitleFormat::Ass,
                embedded_target: None,
                export_target: Some(SubtitleFormat::Ass),
                import_into_media: false,
                ocr_language: None,

                metadata: None,
            },
        );

        // Act
        let description = app.processing_description();

        // Assert
        assert_that!(description)
            .is_equal_to("Converting MKV to MP4 · Exporting 2 subtitles".to_string());

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn processing_description_should_singularize_one_subtitle_export() {
        // Arrange
        let mut app = test_file_app(&["alpha.mkv"]);
        let directory = app.directory.clone();
        app.subtitle_changes.insert(
            SubtitleSource::Embedded(2),
            SubtitleChange {
                source: SubtitleSource::Embedded(2),
                source_format: SubtitleFormat::SubRip,
                embedded_target: None,
                export_target: Some(SubtitleFormat::SubRip),
                import_into_media: false,
                ocr_language: None,

                metadata: None,
            },
        );

        // Act
        let description = app.processing_description();

        // Assert
        assert_that!(description).is_equal_to("Exporting 1 subtitle".to_string());

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn processing_description_should_describe_a_deleted_track() {
        // Arrange
        let mut app = test_file_app(&["alpha.mkv"]);
        let directory = app.directory.clone();
        app.deleted_streams.insert(1);

        // Act
        let description = app.processing_description();

        // Assert
        assert_that!(description).is_equal_to("Deleting 1 audio track".to_string());

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn processing_description_should_describe_a_default_subtitle_change() {
        // Arrange
        let mut app = test_file_app(&["alpha.mkv"]);
        let directory = app.directory.clone();
        app.default_streams.insert(2);

        // Act
        let description = app.processing_description();

        // Assert
        assert_that!(description).is_equal_to("Changing the default subtitle track".to_string());

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn side_by_side_subtitles_should_use_columns_for_navigation() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video"},
                {"index": 1, "codec_type": "audio"},
                {"index": 2, "codec_type": "subtitle"},
                {"index": 3, "codec_type": "subtitle"},
                {"index": 4, "codec_type": "attachment"}
            ]),
        );
        app.sidecars = vec![
            test_sidecar(&app, "movie.eng.srt", "eng"),
            test_sidecar(&app, "movie.nld.srt", "nld"),
        ];
        app.set_subtitle_columns_side_by_side(true);
        app.selected_stream = 3;

        // Act / Assert
        for track in [
            TrackRef::Container,
            TrackRef::Embedded(0),
            TrackRef::Embedded(1),
        ] {
            app.selected_stream = app
                .track_rows()
                .iter()
                .position(|row| *row == track)
                .unwrap();
            app.select_last();
            assert_that!(app.selected_track()).contains(TrackRef::Embedded(3));
        }
        app.select_last();
        assert_that!(app.selected_track()).contains(TrackRef::Embedded(3));
        app.selected_stream = 3;
        assert_that!(app.move_subtitle_column(1)).is_true();
        assert_that!(app.selected_track()).contains(TrackRef::Sidecar(0));
        app.select_last();
        assert_that!(app.selected_track()).contains(TrackRef::Sidecar(1));
        app.selected_stream = 5;
        app.scroll_down();
        assert_that!(app.selected_track()).contains(TrackRef::Sidecar(1));
        app.scroll_up();
        assert_that!(app.selected_track()).contains(TrackRef::Sidecar(0));
        app.select_next();
        assert_that!(app.selected_track()).contains(TrackRef::Sidecar(1));
        assert_that!(app.move_subtitle_column(-1)).is_true();
        assert_that!(app.selected_track()).contains(TrackRef::Embedded(3));
        app.select_next();
        assert_that!(app.selected_track()).contains(TrackRef::Embedded(4));

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn transfer_subtitle_should_mark_and_unmark_embedded_subtitle_for_export() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video"},
                {"index": 1, "codec_type": "subtitle", "codec_name": "subrip"}
            ]),
        );
        app.selected_stream = 2; // Embedded subtitle (stream index 1)
        let source = SubtitleSource::Embedded(1);
        app.subtitle_changes.insert(
            source.clone(),
            SubtitleChange {
                source: source.clone(),
                source_format: SubtitleFormat::SubRip,
                embedded_target: Some(SubtitleFormat::Ass),
                export_target: None,
                import_into_media: false,
                ocr_language: None,
                metadata: None,
            },
        );

        // Act - Ctrl+l (direction 1): Export
        assert_that!(app.transfer_subtitle(1)).is_true();
        let change = app.subtitle_changes.get(&source).cloned();
        assert_that!(&change).is_some();
        assert_that!(change.as_ref().unwrap().embedded_target).is_none();
        assert_that!(change.unwrap().export_target).contains(SubtitleFormat::Ass);

        // Act - Ctrl+h (direction -1): Cancel export
        assert_that!(app.transfer_subtitle(-1)).is_true();
        assert_that!(app.subtitle_changes.get(&source)).is_none();

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn exported_subtitle_codec_choices_should_only_change_the_sidecar_target() {
        let mut app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264"},
            {"index": 1, "codec_type": "subtitle", "codec_name": "subrip", "tags": {"language": "dan"}}
        ])));
        let directory = app.directory.clone();
        app.container_target = Some(ContainerFormat::Matroska);
        app.subtitle_capabilities = ToolCapabilities {
            ffmpeg: true,
            ffmpeg_encoders: BTreeSet::from([
                "subrip".to_string(),
                "ass".to_string(),
                "webvtt".to_string(),
                "ttml".to_string(),
                "mov_text".to_string(),
            ]),
            seconv: true,
            tesseract_languages: vec!["eng".to_string()],
            ..ToolCapabilities::default()
        };
        app.selected_stream = app
            .track_rows()
            .iter()
            .position(|track| *track == TrackRef::Embedded(1))
            .unwrap();
        assert_that!(app.transfer_subtitle(1)).is_true();
        app.open_track_settings();
        let choices = app.subtitle_choices(&SubtitleSource::Embedded(1), SubtitleFormat::SubRip);

        for (position, choice) in choices
            .iter()
            .enumerate()
            .filter(|(_, choice)| choice.enabled)
        {
            let popup = app.subtitle_settings_popup.as_mut().unwrap();
            popup.mode = SubtitleSettingsMode::CodecDropdown;
            popup.codec_cursor = position;
            app.activate_subtitle_settings();

            let change = app
                .subtitle_changes
                .get(&SubtitleSource::Embedded(1))
                .unwrap();
            assert_that!(change.embedded_target).is_none();
            assert_that!(change.export_target).contains(choice.format);
            assert_that!(app.subtitle_popup_codec()).contains(choice.format);
        }

        let vobsub_position = choices
            .iter()
            .position(|choice| choice.format == SubtitleFormat::VobSub)
            .unwrap();
        let popup = app.subtitle_settings_popup.as_mut().unwrap();
        popup.mode = SubtitleSettingsMode::CodecDropdown;
        popup.codec_cursor = vobsub_position;
        app.activate_subtitle_settings();
        assert_that!(app.save_summary())
            .contains("Exporting subtitle track #1 as VobSub".to_string())
            .does_not_contain("Converting subtitle track #1 in the media to VobSub".to_string());
        app.container_target = None;
        assert_that!(app.subtitle_field_reason(SubtitleSettingsField::Forced)).is_none();
        assert_that!(app.subtitle_field_reason(SubtitleSettingsField::Cc)).is_none();

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn staged_export_should_hide_embedded_only_subtitle_fields_and_skip_them_in_navigation() {
        let mut app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264"},
            {
                "index": 1,
                "codec_type": "subtitle",
                "codec_name": "subrip",
                "tags": {"language": "eng", "title": "English dialogue"},
                "disposition": {"default": 0}
            }
        ])));
        let directory = app.directory.clone();
        app.layer = Layer::Streams;
        app.container_target = Some(ContainerFormat::Matroska);
        app.selected_stream = app
            .track_rows()
            .iter()
            .position(|track| *track == TrackRef::Embedded(1))
            .unwrap();
        assert_that!(app.transfer_subtitle(1)).is_true();
        app.open_track_settings();

        assert_eq!(
            app.visible_subtitle_fields(),
            vec![
                SubtitleSettingsField::Codec,
                SubtitleSettingsField::Language,
                SubtitleSettingsField::Forced,
                SubtitleSettingsField::HearingImpaired,
            ]
        );
        app.move_subtitle_settings_cursor(1);
        app.move_subtitle_settings_cursor(1);
        assert_that!(app.subtitle_settings_popup.as_ref().unwrap().field)
            .is_equal_to(SubtitleSettingsField::Forced);

        app.subtitle_settings_popup.as_mut().unwrap().field = SubtitleSettingsField::Title;
        app.activate_subtitle_settings();
        assert_that!(app.subtitle_settings_popup.as_ref().unwrap().mode)
            .is_equal_to(SubtitleSettingsMode::Summary);
        assert_that!(&app.notice).is_none();
        assert_that!(
            app.subtitle_changes
                .get(&SubtitleSource::Embedded(1))
                .unwrap()
                .metadata
                .as_ref()
        )
        .is_none();

        app.close_subtitle_settings();
        assert_that!(app.transfer_subtitle(-1)).is_true();
        app.open_track_settings();
        assert_eq!(
            app.visible_subtitle_fields(),
            vec![
                SubtitleSettingsField::Codec,
                SubtitleSettingsField::Language,
                SubtitleSettingsField::Title,
                SubtitleSettingsField::Default,
                SubtitleSettingsField::Forced,
                SubtitleSettingsField::HearingImpaired,
                SubtitleSettingsField::Original,
                SubtitleSettingsField::Commentary,
            ]
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn staged_export_should_collapse_accessibility_without_mutating_embedded_flags() {
        let mut app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264"},
            {
                "index": 1,
                "codec_type": "subtitle",
                "codec_name": "subrip",
                "tags": {"language": "eng"},
                "disposition": {"captions": 1, "hearing_impaired": 0}
            }
        ])));
        let directory = app.directory.clone();
        app.layer = Layer::Streams;
        app.container_target = Some(ContainerFormat::Mp4);
        app.selected_stream = app
            .track_rows()
            .iter()
            .position(|track| *track == TrackRef::Embedded(1))
            .unwrap();

        assert_that!(app.transfer_subtitle(1)).is_true();
        app.open_track_settings();
        let exported = app.subtitle_popup_metadata().unwrap();
        assert_that!(app.subtitle_field_visible(SubtitleSettingsField::Cc)).is_false();
        assert_that!(app.subtitle_field_visible(SubtitleSettingsField::HearingImpaired)).is_true();
        assert_that!(exported.cc).is_false();
        assert_that!(exported.hearing_impaired).is_true();

        app.close_subtitle_settings();
        assert_that!(app.transfer_subtitle(-1)).is_true();
        app.open_track_settings();
        let restored = app.subtitle_popup_metadata().unwrap();
        assert_that!(app.subtitle_field_visible(SubtitleSettingsField::Cc)).is_true();
        assert_that!(app.subtitle_field_visible(SubtitleSettingsField::HearingImpaired)).is_true();
        assert_that!(restored.cc).is_true();
        assert_that!(restored.hearing_impaired).is_false();

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn staged_import_should_show_embedded_only_subtitle_fields_until_import_is_cancelled() {
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"}
            ]),
        );
        let mut sidecar = test_sidecar(&app, "movie.eng.sdh.srt", "eng");
        sidecar.hearing_impaired = true;
        app.sidecars = vec![sidecar];
        app.selected_stream = app
            .track_rows()
            .iter()
            .position(|track| *track == TrackRef::Sidecar(0))
            .unwrap();

        app.open_track_settings();
        assert_eq!(
            app.visible_subtitle_fields(),
            vec![
                SubtitleSettingsField::Codec,
                SubtitleSettingsField::Language,
                SubtitleSettingsField::Forced,
                SubtitleSettingsField::HearingImpaired,
            ]
        );
        let imported = app.subtitle_popup_metadata().unwrap();
        assert_that!(imported.cc).is_false();
        assert_that!(imported.hearing_impaired).is_true();
        app.close_subtitle_settings();

        assert_that!(app.transfer_subtitle(-1)).is_true();
        app.open_track_settings();
        assert_eq!(
            app.visible_subtitle_fields(),
            vec![
                SubtitleSettingsField::Codec,
                SubtitleSettingsField::Language,
                SubtitleSettingsField::Title,
                SubtitleSettingsField::Default,
                SubtitleSettingsField::Forced,
                SubtitleSettingsField::HearingImpaired,
                SubtitleSettingsField::Original,
                SubtitleSettingsField::Commentary,
            ]
        );
        let imported = app.subtitle_popup_metadata().unwrap();
        assert_that!(imported.cc).is_false();
        assert_that!(imported.hearing_impaired).is_true();
        app.close_subtitle_settings();

        assert_that!(app.transfer_subtitle(1)).is_true();
        app.open_track_settings();
        assert_eq!(
            app.visible_subtitle_fields(),
            vec![
                SubtitleSettingsField::Codec,
                SubtitleSettingsField::Language,
                SubtitleSettingsField::Forced,
                SubtitleSettingsField::HearingImpaired,
            ]
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn transfer_subtitle_should_mark_and_unmark_sidecar_subtitle_for_import() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video"}
            ]),
        );
        app.sidecars = vec![test_sidecar(&app, "movie.eng.srt", "eng")];
        app.selected_stream = 2; // Sidecar 0

        // Act - Ctrl+h (direction -1): Import
        assert_that!(app.transfer_subtitle(-1)).is_true();
        let source = SubtitleSource::Sidecar(directory.join("movie.eng.srt"));
        let change = app.subtitle_changes.get(&source).cloned();
        assert_that!(&change).is_some();
        assert_that!(change.unwrap().import_into_media).is_true();

        // Act - Ctrl+l (direction 1): Cancel import
        assert_that!(app.transfer_subtitle(1)).is_true();
        assert_that!(app.subtitle_changes.get(&source)).is_none();

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn wide_subtitle_navigation_should_not_cross_columns_at_the_bottom() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video"},
                {"index": 1, "codec_type": "subtitle"},
                {"index": 2, "codec_type": "subtitle"}
            ]),
        );
        app.sidecars = vec![
            test_sidecar(&app, "movie.eng.srt", "eng"),
            test_sidecar(&app, "movie.nld.srt", "nld"),
        ];
        app.set_subtitle_columns_side_by_side(true);
        app.selected_stream = 3;

        // Act
        app.select_next();

        // Assert
        assert_that!(app.selected_track()).contains(TrackRef::Embedded(2));

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn stacked_subtitles_should_follow_the_linear_track_order() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video"},
                {"index": 1, "codec_type": "subtitle"},
                {"index": 2, "codec_type": "subtitle"}
            ]),
        );
        app.sidecars = vec![test_sidecar(&app, "movie.eng.srt", "eng")];
        app.set_subtitle_columns_side_by_side(false);
        app.selected_stream = 3;

        // Act
        app.select_next();

        // Assert
        assert_that!(app.selected_track()).contains(TrackRef::Sidecar(0));
        assert_that!(app.move_subtitle_column(1)).is_false();

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn toggling_sidecar_import_should_stage_a_descriptive_media_edit() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        let sidecar = test_sidecar(&app, "movie.eng.srt", "eng");
        let source = SubtitleSource::Sidecar(sidecar.path.clone());
        app.sidecars.push(sidecar);
        app.selected_stream = 4;
        // Act
        app.transfer_subtitle(-1);

        // Assert
        let change = app.subtitle_changes.get(&source).unwrap();
        assert_that!(change.import_into_media).is_true();
        assert_that!(change.embedded_target).is_none();
        assert_that!(app.media_will_change()).is_true();
        assert_that!(app.processing_description()).is_equal_to("Importing 1 subtitle".to_string());
        assert_that!(app.save_summary())
            .contains("Importing movie.eng.srt as SubRip / SRT".to_string());

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn imported_subtitle_should_block_an_incompatible_container_until_converted() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "audio", "codec_name": "aac"}
            ]),
        );
        let sidecar = test_sidecar(&app, "movie.eng.srt", "eng");
        let source = SubtitleSource::Sidecar(sidecar.path.clone());
        app.sidecars.push(sidecar);
        app.container_target = Some(ContainerFormat::Mp4);
        app.subtitle_changes.insert(
            source.clone(),
            SubtitleChange {
                source: source.clone(),
                source_format: SubtitleFormat::SubRip,
                embedded_target: None,
                export_target: None,
                import_into_media: true,
                ocr_language: None,

                metadata: None,
            },
        );

        // Act
        let incompatible = app.selected_container_conflicts();
        app.subtitle_changes
            .get_mut(&source)
            .unwrap()
            .embedded_target = Some(SubtitleFormat::MovText);
        let compatible = app.selected_container_conflicts();

        // Assert
        assert_that!(incompatible).contains(
            "MP4 can't import SubRip / SRT subtitle movie.eng.srt. Convert it to MOV Text."
                .to_string(),
        );
        assert_that!(compatible).is_empty();

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn final_stream_order_should_change_only_same_group_positions_when_tracks_are_reordered() {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video"},
            {"index": 1, "codec_type": "audio"},
            {"index": 2, "codec_type": "subtitle"},
            {"index": 3, "codec_type": "audio"},
            {"index": 4, "codec_type": "attachment"}
        ]));
        let staged = [0, 3, 1, 2, 4];

        // Act
        let result = final_stream_order(&info, &staged, &BTreeSet::new());

        // Assert
        assert_that!(result).contains_exactly_in_given_order([0, 3, 2, 1, 4]);
    }

    #[test]
    fn final_stream_order_should_preserve_surviving_group_positions_when_reordered_track_is_deleted()
     {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video"},
            {"index": 1, "codec_type": "audio"},
            {"index": 2, "codec_type": "subtitle"},
            {"index": 3, "codec_type": "audio"},
            {"index": 4, "codec_type": "attachment"}
        ]));
        let staged = [0, 3, 1, 2, 4];
        let deleted = BTreeSet::from([3]);

        // Act
        let result = final_stream_order(&info, &staged, &deleted);

        // Assert
        assert_that!(result).contains_exactly_in_given_order([0, 1, 2, 4]);
    }

    #[test]
    fn edit_summary_should_group_actions_by_track_type_when_multiple_edits_are_staged() {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video"},
            {"index": 1, "codec_type": "audio"},
            {"index": 2, "codec_type": "subtitle"},
            {"index": 3, "codec_type": "audio"}
        ]));

        // Act
        let lines = edit_summary(
            &info,
            &[0, 1, 3, 2],
            &[0, 3, 1, 2],
            &BTreeSet::from([3]),
            &BTreeSet::from([2]),
            &BTreeSet::from([1]),
            &BTreeSet::from([3]),
        );

        // Assert
        assert_that!(lines).contains_exactly_in_given_order([
            "Moving 1 audio track".to_string(),
            "Deleting 1 subtitle track".to_string(),
            "Changing the default audio track".to_string(),
        ]);
    }

    #[test]
    fn move_selected_stream_should_reorder_tracks_and_follow_selection_when_neighbor_has_same_type()
    {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "disposition": {"default": 1}},
            {"index": 1, "codec_type": "audio", "disposition": {"default": 1}},
            {"index": 2, "codec_type": "audio", "disposition": {"default": 0}},
            {"index": 3, "codec_type": "subtitle", "disposition": {"default": 0}}
        ]));
        let mut app = test_app(info);
        let directory = app.directory.clone();
        app.selected_stream = 2;

        // Act
        app.move_selected_stream(1);

        // Assert
        assert_that!(&app.stream_order).contains_exactly_in_given_order([0, 2, 1, 3]);
        assert_that!(app.selected_stream).is_equal_to(3);
        assert_that!(app.changed_streams()).is_equal_to(BTreeSet::from([1]));

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn move_selected_stream_should_only_mark_the_explicitly_moved_track_when_crossing_multiple_tracks()
     {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "disposition": {"default": 1}},
            {"index": 1, "codec_type": "subtitle", "disposition": {"default": 0}},
            {"index": 2, "codec_type": "subtitle", "disposition": {"default": 0}},
            {"index": 3, "codec_type": "subtitle", "disposition": {"default": 0}}
        ]));
        let mut app = test_app(info);
        let directory = app.directory.clone();
        app.selected_stream = 2;

        // Act
        app.move_selected_stream(1);
        app.move_selected_stream(1);

        // Assert
        assert_that!(&app.stream_order).contains_exactly_in_given_order([0, 2, 3, 1]);
        assert_that!(app.changed_streams()).is_equal_to(BTreeSet::from([1]));

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn move_selected_stream_should_clear_order_changes_when_track_is_moved_to_original_position() {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "disposition": {"default": 1}},
            {"index": 1, "codec_type": "audio", "disposition": {"default": 1}},
            {"index": 2, "codec_type": "audio", "disposition": {"default": 0}},
            {"index": 3, "codec_type": "subtitle", "disposition": {"default": 0}}
        ]));
        let mut app = test_app(info);
        let directory = app.directory.clone();
        app.selected_stream = 2;
        app.move_selected_stream(1);

        // Act
        app.move_selected_stream(-1);

        // Assert
        assert_that!(&app.stream_order).contains_exactly_in_given_order([0, 1, 2, 3]);
        assert_that!(app.changed_streams()).is_empty();

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn transfer_subtitle_should_clear_default_flag_when_embedded_subtitle_is_exported() {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "disposition": {"default": 1}},
            {"index": 1, "codec_type": "subtitle", "codec_name": "subrip", "disposition": {"default": 1}}
        ]));
        let mut app = test_app(info);
        let directory = app.directory.clone();
        app.layer = Layer::Streams;
        let sub_row = app
            .track_rows()
            .iter()
            .position(|r| *r == TrackRef::Embedded(1))
            .unwrap();
        app.selected_stream = sub_row;

        // Act
        app.transfer_subtitle(1); // Export subtitle to right column

        // Assert
        assert_that!(app.default_streams.contains(&1)).is_false();
        assert_that!(app.is_stream_exported(1)).is_true();

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn toggle_delete_selected_stream_should_mark_track_when_track_is_not_already_marked() {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "disposition": {"default": 1}},
            {"index": 1, "codec_type": "audio", "disposition": {"default": 1}},
            {"index": 2, "codec_type": "audio", "disposition": {"default": 0}}
        ]));
        let mut app = test_app(info);
        let directory = app.directory.clone();
        app.selected_stream = 3;

        // Act
        app.toggle_delete_selected_stream();

        // Assert
        assert_that!(app.deleted_streams).is_equal_to(BTreeSet::from([2]));

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn move_selected_stream_should_show_notice_when_selected_track_is_marked_for_deletion() {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "disposition": {"default": 1}},
            {"index": 1, "codec_type": "audio", "disposition": {"default": 1}},
            {"index": 2, "codec_type": "audio", "disposition": {"default": 0}}
        ]));
        let mut app = test_app(info);
        let directory = app.directory.clone();
        app.selected_stream = 3;
        app.toggle_delete_selected_stream();
        app.selected_stream = 3;

        // Act
        app.move_selected_stream(-1);

        // Assert
        assert_that!(app.notice.as_deref().unwrap()).contains("Unmark");

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn resolution_choices_should_use_preset_label_for_standard_source_resolution() {
        // Arrange
        let app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080}
        ])));
        let directory = app.directory.clone();

        // Act
        let choices = app.resolution_choices(0);

        // Assert
        assert_that!(
            choices
                .iter()
                .map(|choice| choice.label.as_str())
                .collect::<Vec<_>>()
        )
        .contains_exactly_in_given_order([
            "3840×2160 / 16:9",
            "2560×1440 / 16:9",
            "1920×1080 / 16:9",
            "1920×960 / 2:1",
            "1280×720 / 16:9",
            "854×480 / 16:9",
            "Custom…",
        ]);
        assert_that!(
            choices
                .iter()
                .map(|choice| choice.enabled)
                .collect::<Vec<_>>()
        )
        .contains_exactly_in_given_order([false, false, true, true, true, true, true]);
        assert_that!(
            choices
                .iter()
                .map(|choice| choice.selected(VideoResolution::Original))
                .collect::<Vec<_>>()
        )
        .contains_exactly_in_given_order([false, false, true, false, false, false, false]);

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn resolution_choices_should_mark_nonstandard_source_resolution_as_custom() {
        // Arrange
        let mut app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264", "width": 1920, "height": 800}
        ])));
        let directory = app.directory.clone();

        // Act
        let choices = app.resolution_choices(0);
        app.selected_stream = 1;
        app.open_video_settings();

        // Assert
        assert_that!(
            choices
                .iter()
                .map(|choice| choice.label.as_str())
                .collect::<Vec<_>>()
        )
        .contains_exactly_in_given_order([
            "3840×2160 / 16:9",
            "2560×1440 / 16:9",
            "1920×1080 / 16:9",
            "1920×960 / 2:1",
            "1280×720 / 16:9",
            "854×480 / 16:9",
            "Custom (1920×800)",
        ]);
        assert_that!(
            choices
                .iter()
                .map(|choice| choice.enabled)
                .collect::<Vec<_>>()
        )
        .contains_exactly_in_given_order([false, false, false, false, true, true, true]);
        assert_that!(
            choices
                .iter()
                .map(|choice| choice.selected(VideoResolution::Original))
                .collect::<Vec<_>>()
        )
        .contains_exactly_in_given_order([false, false, false, false, false, false, true]);
        assert_that!(app.video_settings_popup.as_ref().unwrap().resolution_cursor)
            .is_equal_to(choices.len() - 1);

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn resolution_choices_should_recognize_a_1920_by_960_source_as_an_exact_preset() {
        // Arrange
        let app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264", "width": 1920, "height": 960}
        ])));
        let directory = app.directory.clone();

        // Act
        let choices = app.resolution_choices(0);

        // Assert
        assert_that!(
            choices
                .iter()
                .map(|choice| choice.label.as_str())
                .collect::<Vec<_>>()
        )
        .contains_exactly_in_given_order([
            "3840×2160 / 16:9",
            "2560×1440 / 16:9",
            "1920×1080 / 16:9",
            "1920×960 / 2:1",
            "1280×720 / 16:9",
            "854×480 / 16:9",
            "Custom…",
        ]);
        assert_that!(
            choices
                .iter()
                .map(|choice| choice.selected(VideoResolution::Original))
                .collect::<Vec<_>>()
        )
        .contains_exactly_in_given_order([false, false, false, true, false, false, false]);

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn resolution_choices_should_keep_a_same_height_wrong_width_source_custom() {
        // Arrange
        let app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264", "width": 1440, "height": 1080}
        ])));
        let directory = app.directory.clone();

        // Act
        let choices = app.resolution_choices(0);

        // Assert
        assert_that!(
            choices
                .iter()
                .map(|choice| choice.enabled)
                .collect::<Vec<_>>()
        )
        .contains_exactly_in_given_order([false, false, false, false, true, true, true]);
        assert_that!(
            choices
                .iter()
                .map(|choice| choice.selected(VideoResolution::Original))
                .collect::<Vec<_>>()
        )
        .contains_exactly_in_given_order([false, false, false, false, false, false, true]);
        assert_that!(choices.last().unwrap().label.as_str()).is_equal_to("Custom (1440×1080)");

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn resolution_choices_should_keep_one_custom_row_when_custom_dimensions_are_staged() {
        // Arrange
        let mut app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080}
        ])));
        let directory = app.directory.clone();
        app.video_settings.insert(
            0,
            VideoSettings {
                resolution: VideoResolution::Custom(CustomResolution {
                    width: 1280,
                    height: 720,
                    scaling: CustomScaling::Stretch,
                }),
                ..VideoSettings::default()
            },
        );

        // Act
        let choices = app.resolution_choices(0);

        // Assert
        assert_that!(
            choices
                .iter()
                .map(|choice| choice.label.as_str())
                .collect::<Vec<_>>()
        )
        .contains_exactly_in_given_order([
            "3840×2160 / 16:9",
            "2560×1440 / 16:9",
            "1920×1080 / 16:9",
            "1920×960 / 2:1",
            "1280×720 / 16:9",
            "854×480 / 16:9",
            "Custom (1280×720)",
        ]);
        assert_that!(
            choices
                .iter()
                .filter(|choice| matches!(choice.value, ResolutionChoiceValue::Custom))
                .count()
        )
        .is_equal_to(1);
        assert_that!(
            choices
                .last()
                .unwrap()
                .selected(VideoResolution::Custom(CustomResolution {
                    width: 1280,
                    height: 720,
                    scaling: CustomScaling::Stretch,
                }))
        )
        .is_true();

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn open_custom_resolution_editor(app: &mut App) {
        app.selected_stream = 1;
        app.open_video_settings();
        app.move_video_settings_cursor(1);
        app.activate_video_settings();
        let custom_cursor = app.resolution_choices(0).len() - 1;
        app.video_settings_popup.as_mut().unwrap().resolution_cursor = custom_cursor;
        app.activate_video_settings();
    }

    fn clear_custom_resolution_inputs(app: &mut App) {
        let draft = app
            .video_settings_popup
            .as_mut()
            .unwrap()
            .custom_resolution
            .as_mut()
            .unwrap();
        draft.width.clear();
        draft.height.clear();
    }

    #[test]
    fn custom_resolution_should_prefill_the_source_dimensions() {
        // Arrange
        let mut app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080}
        ])));
        let directory = app.directory.clone();

        // Act
        open_custom_resolution_editor(&mut app);

        // Assert
        let draft = app
            .video_settings_popup
            .as_ref()
            .unwrap()
            .custom_resolution
            .as_ref()
            .unwrap();
        assert_that!(draft.width.value.as_str()).is_equal_to("1920");
        assert_that!(draft.height.value.as_str()).is_equal_to("1080");

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn valid_custom_resolution_should_stage_dimensions_and_scaling_mode() {
        // Arrange
        let mut app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080}
        ])));
        let directory = app.directory.clone();
        open_custom_resolution_editor(&mut app);
        clear_custom_resolution_inputs(&mut app);
        app.start_custom_resolution_input();
        for digit in "1280".chars() {
            app.input_custom_resolution_digit(digit);
        }
        app.finish_custom_resolution_input();
        app.move_video_settings_cursor(1);
        app.start_custom_resolution_input();
        for digit in "720".chars() {
            app.input_custom_resolution_digit(digit);
        }
        app.finish_custom_resolution_input();
        app.move_video_settings_cursor(1);
        app.activate_video_settings();
        app.move_video_settings_cursor(1);

        // Act
        app.activate_video_settings();

        // Assert
        assert_that!(
            app.video_settings
                .get(&0)
                .map(|settings| settings.resolution)
        )
        .contains(VideoResolution::Custom(CustomResolution {
            width: 1280,
            height: 720,
            scaling: CustomScaling::Stretch,
        }));
        let popup = app.video_settings_popup.as_ref().unwrap();
        assert_that!(popup.mode).is_equal_to(VideoSettingsMode::CustomResolution);
        assert_that!(
            popup
                .custom_resolution
                .as_ref()
                .unwrap()
                .scaling_dropdown_open
        )
        .is_false();

        // Act: back out normally after staging
        app.escape_video_settings();

        // Assert
        assert_that!(app.video_settings_popup.as_ref().unwrap().mode)
            .is_equal_to(VideoSettingsMode::Dropdown);
        assert_that!(
            app.video_settings
                .get(&0)
                .map(|settings| settings.resolution)
        )
        .contains(VideoResolution::Custom(CustomResolution {
            width: 1280,
            height: 720,
            scaling: CustomScaling::Stretch,
        }));

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn escape_video_settings_should_stage_a_valid_custom_resolution_and_enable_save() {
        // Arrange
        let mut app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080}
        ])));
        let directory = app.directory.clone();
        open_custom_resolution_editor(&mut app);
        let draft = app
            .video_settings_popup
            .as_mut()
            .unwrap()
            .custom_resolution
            .as_mut()
            .unwrap();
        draft.width = TextInputState::new("1280".to_string());
        draft.height = TextInputState::new("720".to_string());

        // Act
        app.escape_video_settings();
        let staged = app
            .video_settings
            .get(&0)
            .map(|settings| settings.resolution);
        app.save_from_video_settings();

        // Assert
        assert_that!(staged).contains(VideoResolution::Custom(CustomResolution {
            width: 1280,
            height: 720,
            scaling: CustomScaling::FitPad,
        }));
        assert_that!(app.dialog).contains(Dialog::ConfirmSave);

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn escape_video_settings_should_discard_an_invalid_draft_and_preserve_staged_resolution() {
        // Arrange
        let mut app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080}
        ])));
        let directory = app.directory.clone();
        let staged = CustomResolution {
            width: 1280,
            height: 720,
            scaling: CustomScaling::FitPad,
        };
        app.video_settings.insert(
            0,
            VideoSettings {
                resolution: VideoResolution::Custom(staged),
                ..VideoSettings::default()
            },
        );
        open_custom_resolution_editor(&mut app);
        app.video_settings_popup
            .as_mut()
            .unwrap()
            .custom_resolution
            .as_mut()
            .unwrap()
            .width = TextInputState::new("1279".to_string());

        // Act
        app.escape_video_settings();

        // Assert
        assert_that!(app.video_settings_popup.as_ref().unwrap().mode)
            .is_equal_to(VideoSettingsMode::Dropdown);
        assert_that!(
            app.video_settings
                .get(&0)
                .map(|settings| settings.resolution)
        )
        .contains(VideoResolution::Custom(staged));

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn escape_should_close_scaling_dropdown_before_leaving_custom_editor() {
        // Arrange
        let mut app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080}
        ])));
        let directory = app.directory.clone();
        open_custom_resolution_editor(&mut app);
        app.move_video_settings_cursor(1);
        app.move_video_settings_cursor(1);
        app.activate_video_settings();

        // Act
        app.escape_video_settings();

        // Assert
        let popup = app.video_settings_popup.as_ref().unwrap();
        assert_that!(popup.mode).is_equal_to(VideoSettingsMode::CustomResolution);
        assert_that!(
            popup
                .custom_resolution
                .as_ref()
                .unwrap()
                .scaling_dropdown_open
        )
        .is_false();

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn upscaled_custom_resolution_should_remain_in_editor_and_block_save() {
        // Arrange
        let mut app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080}
        ])));
        let directory = app.directory.clone();
        open_custom_resolution_editor(&mut app);
        clear_custom_resolution_inputs(&mut app);
        app.start_custom_resolution_input();
        for digit in "1922".chars() {
            app.input_custom_resolution_digit(digit);
        }
        app.finish_custom_resolution_input();
        app.move_video_settings_cursor(1);
        app.start_custom_resolution_input();
        for digit in "720".chars() {
            app.input_custom_resolution_digit(digit);
        }
        app.finish_custom_resolution_input();

        // Act
        let error = app.custom_resolution_error();
        app.save_from_video_settings();

        // Assert
        assert_that!(error.as_deref()).contains("Upscaling isn't possible yet.");
        assert_that!(app.dialog).contains(Dialog::VideoSettings);
        assert_that!(app.video_settings_popup.as_ref().unwrap().mode)
            .is_equal_to(VideoSettingsMode::CustomResolution);
        assert_that!(app.video_settings).is_empty();

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn odd_custom_resolution_should_show_validation_and_not_apply() {
        // Arrange
        let mut app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080}
        ])));
        let directory = app.directory.clone();
        open_custom_resolution_editor(&mut app);
        clear_custom_resolution_inputs(&mut app);
        app.start_custom_resolution_input();
        for digit in "1279".chars() {
            app.input_custom_resolution_digit(digit);
        }
        app.finish_custom_resolution_input();
        app.move_video_settings_cursor(1);
        app.start_custom_resolution_input();
        for digit in "720".chars() {
            app.input_custom_resolution_digit(digit);
        }
        app.finish_custom_resolution_input();

        // Act
        app.activate_video_settings();

        // Assert
        assert_that!(app.custom_resolution_error().as_deref())
            .contains("Width and height must be even.");
        assert_that!(app.video_settings_popup.as_ref().unwrap().mode)
            .is_equal_to(VideoSettingsMode::CustomResolution);
        assert_that!(app.video_settings).is_empty();

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn custom_resolution_should_validate_incomplete_zero_overflow_and_height_upscale() {
        // Arrange
        let mut app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080}
        ])));
        let directory = app.directory.clone();
        open_custom_resolution_editor(&mut app);
        let cases = [
            ("", "", "Enter both width and height."),
            (
                "0",
                "720",
                "Width and height must be positive whole numbers.",
            ),
            (
                "18446744073709551616",
                "720",
                "Width and height must be positive whole numbers.",
            ),
            ("1280", "1082", "Upscaling isn't possible yet."),
        ];

        for (width, height, expected) in cases {
            let draft = app
                .video_settings_popup
                .as_mut()
                .unwrap()
                .custom_resolution
                .as_mut()
                .unwrap();
            draft.width = TextInputState::new(width.to_string());
            draft.height = TextInputState::new(height.to_string());

            // Act / Assert
            assert_that!(app.custom_resolution_error().as_deref()).contains(expected);
        }

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn custom_resolution_matching_source_should_normalize_to_original() {
        // Arrange
        let mut app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080}
        ])));
        let directory = app.directory.clone();
        open_custom_resolution_editor(&mut app);
        app.move_video_settings_cursor(1);
        app.move_video_settings_cursor(1);
        app.activate_video_settings();

        // Act
        app.activate_video_settings();

        // Assert
        assert_that!(app.video_settings).is_empty();
        assert_that!(app.video_settings_popup.as_ref().unwrap().mode)
            .is_equal_to(VideoSettingsMode::CustomResolution);

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn selecting_source_codec_should_remain_a_no_op_when_popup_closes() {
        // Arrange
        let mut app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080}
        ])));
        let directory = app.directory.clone();
        app.open_video_settings();
        app.activate_video_settings();
        app.activate_video_settings();

        // Act
        app.escape_video_settings();

        // Assert
        assert_that!(app.dialog).is_none();
        assert_that!(app.video_settings).is_empty();

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn container_should_be_the_first_track_row_and_open_its_selector() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();

        // Act
        let rows = app.track_rows();
        app.open_track_settings();

        // Assert
        assert_that!(rows.first()).contains(&TrackRef::Container);
        assert_that!(app.dialog).contains(Dialog::ContainerSettings);
        assert_that!(&app.container_settings_popup).is_some();

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn container_warning_should_group_multiple_conflicts_by_track_type() {
        // Arrange
        let choice = ContainerChoice {
            value: Some(ContainerFormat::Mp4),
            label: "MP4".to_string(),
            current: false,
            staged: false,
            conflicts: vec![
                "MP4 can't contain SUBRIP subtitle track #2.".to_string(),
                "MP4 can't contain SUBRIP subtitle track #3.".to_string(),
                "MP4 can't contain ASS subtitle track #4.".to_string(),
            ],
        };

        // Act
        let warning = choice.warning();

        // Assert
        assert_that!(warning.as_deref()).contains("MP4 can't contain SubRip/SRT or ASS subtitles.");
    }

    #[test]
    fn selecting_mp4_should_stage_the_container_and_default_to_replacing_the_original() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "audio", "codec_name": "aac"}
            ]),
        );
        app.subtitle_capabilities = ToolCapabilities {
            ffmpeg: true,
            ffmpeg_muxers: BTreeSet::from(["mp4".to_string()]),
            ..ToolCapabilities::default()
        };
        app.open_container_settings();
        app.move_container_settings_cursor(1);

        // Act
        app.activate_container_settings();
        app.request_save();

        // Assert
        assert_that!(app.container_target).contains(ContainerFormat::Mp4);
        assert_that!(app.dialog).contains(Dialog::ConfirmSave);
        assert_that!(app.save_destination).is_equal_to(SaveDestination::ReplaceOriginal);

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn incompatible_subtitle_should_block_save_until_it_is_converted_for_mp4() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "audio", "codec_name": "aac"},
                {"index": 2, "codec_type": "subtitle", "codec_name": "subrip", "tags": {"language": "eng"}}
            ]),
        );
        app.subtitle_capabilities = ToolCapabilities {
            ffmpeg: true,
            ffmpeg_encoders: BTreeSet::from(["mov_text".to_string()]),
            ffmpeg_muxers: BTreeSet::from(["mp4".to_string()]),
            ..ToolCapabilities::default()
        };
        app.container_target = Some(ContainerFormat::Mp4);

        // Act: unresolved conflict
        app.request_save();
        let error = app.edit_error.clone().unwrap();

        // Assert: the reason is actionable
        assert_that!(app.dialog).contains(Dialog::Error);
        assert_that!(error.as_str()).contains("track #2");
        assert_that!(error.as_str()).contains("MOV Text");

        // Act: stage the compatible subtitle conversion and retry
        app.dismiss_dialog();
        app.subtitle_changes.insert(
            SubtitleSource::Embedded(2),
            SubtitleChange {
                source: SubtitleSource::Embedded(2),
                source_format: SubtitleFormat::SubRip,
                embedded_target: Some(SubtitleFormat::MovText),
                export_target: None,
                import_into_media: false,
                ocr_language: None,

                metadata: None,
            },
        );
        app.request_save();

        // Assert
        assert_that!(app.dialog).contains(Dialog::ConfirmSave);
        assert_that!(app.selected_container_conflicts()).is_empty();

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn selected_container_conflict_streams_should_only_include_unresolved_surviving_tracks() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "audio", "codec_name": "aac"},
                {"index": 2, "codec_type": "subtitle", "codec_name": "subrip"},
                {"index": 3, "codec_type": "subtitle", "codec_name": "ass"}
            ]),
        );
        app.container_target = Some(ContainerFormat::Mp4);
        app.deleted_streams.insert(3);

        // Act
        let unresolved = app.selected_container_conflict_streams();
        app.subtitle_changes.insert(
            SubtitleSource::Embedded(2),
            SubtitleChange {
                source: SubtitleSource::Embedded(2),
                source_format: SubtitleFormat::SubRip,
                embedded_target: Some(SubtitleFormat::MovText),
                export_target: None,
                import_into_media: false,
                ocr_language: None,

                metadata: None,
            },
        );
        let resolved = app.selected_container_conflict_streams();

        // Assert
        assert_that!(unresolved).contains_exactly_in_any_order([2]);
        assert_that!(resolved).is_empty();

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn webm_target_should_only_enable_compatible_video_codec_choices() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"}
            ]),
        );
        app.container_target = Some(ContainerFormat::WebM);

        // Act
        let choices = app.video_codec_choices(0);

        // Assert
        assert_that!(
            choices
                .iter()
                .find(|choice| choice.label == "H.264")
                .unwrap()
                .enabled
        )
        .is_false();
        assert_that!(
            choices
                .iter()
                .find(|choice| choice.label == "HEVC / H.265")
                .unwrap()
                .enabled
        )
        .is_false();
        assert_that!(
            choices
                .iter()
                .find(|choice| choice.label == "AV1")
                .unwrap()
                .enabled
        )
        .is_true();

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn subtitle_export_save_dialog_should_allow_selecting_a_copy_destination() {
        // Arrange
        let mut app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264"},
            {"index": 1, "codec_type": "subtitle", "codec_name": "subrip", "tags": {"language": "eng"}}
        ])));
        app.subtitle_changes.insert(
            SubtitleSource::Embedded(1),
            SubtitleChange {
                source: SubtitleSource::Embedded(1),
                source_format: SubtitleFormat::SubRip,
                embedded_target: None,
                export_target: Some(SubtitleFormat::SubRip),
                import_into_media: false,
                ocr_language: None,

                metadata: None,
            },
        );
        app.request_save();

        // Act
        app.move_save_dialog_cursor(-1);
        app.choose_save_destination(1);

        // Assert
        assert_that!(app.save_dialog_field).is_equal_to(SaveDialogField::Destination);
        assert_that!(app.save_destination).is_equal_to(SaveDestination::CreateCopy);
    }

    #[test]
    fn automatic_ocr_language_should_prefer_the_subtitle_track_language() {
        // Arrange
        let mut app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264"},
            {
                "index": 1,
                "codec_type": "subtitle",
                "codec_name": "hdmv_pgs_subtitle",
                "tags": {"language": "nld"}
            }
        ])));
        app.subtitle_capabilities.tesseract_languages = vec!["eng".to_string(), "nld".to_string()];

        // Act
        let language = app.automatic_ocr_language(&SubtitleSource::Embedded(1));

        // Assert
        assert_that!(language.as_deref()).contains("nld");
    }

    #[test]
    fn video_resolution_cursor_should_skip_disabled_higher_presets() {
        // Arrange
        let mut app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080}
        ])));
        let directory = app.directory.clone();
        app.selected_stream = 1;
        app.open_video_settings();
        app.move_video_settings_cursor(1);
        app.activate_video_settings();
        let original_cursor = app.video_settings_popup.as_ref().unwrap().resolution_cursor;

        // Act
        app.move_video_settings_cursor(-1);

        // Assert
        assert_that!(app.video_settings_popup.as_ref().unwrap().resolution_cursor)
            .is_equal_to(original_cursor);

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn changed_codec_should_be_staged_and_mark_the_video_stream_as_changed() {
        // Arrange
        let mut app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080}
        ])));
        let directory = app.directory.clone();
        app.selected_stream = 1;
        app.open_video_settings();
        app.activate_video_settings();
        app.move_video_settings_cursor(1);

        // Act
        app.activate_video_settings();

        // Assert
        assert_that!(app.video_settings.get(&0).map(|settings| settings.codec))
            .contains(VideoCodec::Hevc);
        assert_that!(app.changed_streams()).contains(0);

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn video_codec_choices_should_show_the_source_codec_once_without_original() {
        // Arrange
        let app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080}
        ])));
        let directory = app.directory.clone();

        // Act
        let choices = app.video_codec_choices(0);

        // Assert
        assert_that!(
            choices
                .iter()
                .map(|choice| choice.label.as_str())
                .collect::<Vec<_>>()
        )
        .contains_exactly_in_given_order(["H.264", "HEVC / H.265", "AV1"]);
        assert_that!(choices.iter().filter(|choice| choice.current).count()).is_equal_to(1);
        assert_that!(choices[0].current).is_true();
        assert_that!(choices[0].value).is_equal_to(VideoCodec::Original);
        assert_that!(
            choices
                .iter()
                .any(|choice| choice.label.contains("Original"))
        )
        .is_false();

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn video_codec_choices_should_add_an_unknown_source_without_duplicating_targets() {
        // Arrange
        let app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "ffv1", "width": 1920, "height": 1080}
        ])));
        let directory = app.directory.clone();

        // Act
        let choices = app.video_codec_choices(0);

        // Assert
        assert_that!(
            choices
                .iter()
                .map(|choice| choice.label.as_str())
                .collect::<Vec<_>>()
        )
        .contains_exactly_in_given_order(["FFV1", "H.264", "HEVC / H.265", "AV1"]);
        assert_that!(choices[0].current).is_true();
        assert_that!(choices[0].value).is_equal_to(VideoCodec::Original);

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn directory_update_should_preserve_edit_state_when_unrelated_file_is_added() {
        // Arrange
        let mut app = test_file_app(&["alpha.mkv", "beta.mkv"]);
        let directory = app.directory.clone();
        app.deleted_streams.insert(2);
        std::fs::write(directory.join("gamma.mkv"), b"media").unwrap();

        // Act
        app.apply_directory_snapshot(DirectorySnapshot::Files(
            scan_directory(&directory).unwrap(),
        ));

        // Assert
        assert_that!(app.selected_file().unwrap().display_name.as_str()).is_equal_to("alpha.mkv");
        assert_that!(app.layer).is_equal_to(Layer::Streams);
        assert_that!(&app.deleted_streams).contains(2);

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn directory_update_should_defer_selected_removal_until_the_worker_finishes() {
        // Arrange
        let mut app = test_file_app(&["alpha.mkv", "beta.mkv"]);
        let directory = app.directory.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        app.edit_cancel = Some(cancelled.clone());
        app.dialog = Some(Dialog::Processing);
        std::fs::remove_file(directory.join("alpha.mkv")).unwrap();
        let (result_tx, result_rx) = std::sync::mpsc::channel();

        // Act: the watcher reports the source removal before the worker result
        app.apply_directory_snapshot(DirectorySnapshot::Files(
            scan_directory(&directory).unwrap(),
        ));

        // Assert: keep ownership with the worker to allow cross-extension replacement
        assert_that!(cancelled.load(Ordering::Relaxed)).is_false();
        assert_that!(app.dialog).contains(Dialog::Processing);
        assert_that!(app.selected_file().unwrap().display_name.as_str()).is_equal_to("alpha.mkv");

        // Act: the worker confirms that an external removal changed the source
        result_tx
            .send(EditEvent::Finished {
                path: directory.join("alpha.mkv"),
                outcome: EditOutcome::SourceChanged("Source file was removed.".to_string()),
            })
            .unwrap();
        app.receive_edit_results(&result_rx);

        // Assert
        assert_that!(app.dialog).is_none();
        assert_that!(app.layer).is_equal_to(Layer::Files);
        assert_that!(app.selected_file().unwrap().display_name.as_str()).is_equal_to("beta.mkv");
        assert_that!(app.notice.as_deref().unwrap()).contains("removed");

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn directory_update_should_discard_staged_edits_and_reprobe_when_selected_file_changes() {
        // Arrange
        let mut app = test_file_app(&["alpha.mkv"]);
        let directory = app.directory.clone();
        app.deleted_streams.insert(2);
        std::fs::write(directory.join("alpha.mkv"), b"changed media contents").unwrap();

        // Act
        app.apply_directory_snapshot(DirectorySnapshot::Files(
            scan_directory(&directory).unwrap(),
        ));

        // Assert
        assert_that!(app.layer).is_equal_to(Layer::Files);
        assert_that!(&app.deleted_streams).is_empty();
        assert_that!(app.loading).is_true();
        assert_that!(app.notice.as_deref().unwrap()).contains("reloaded");

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn directory_update_should_treat_selected_rename_as_removal() {
        // Arrange
        let mut app = test_file_app(&["alpha.mkv", "beta.mkv"]);
        let directory = app.directory.clone();
        app.deleted_streams.insert(2);
        std::fs::rename(directory.join("alpha.mkv"), directory.join("renamed.mkv")).unwrap();

        // Act
        app.apply_directory_snapshot(DirectorySnapshot::Files(
            scan_directory(&directory).unwrap(),
        ));

        // Assert
        assert_that!(app.layer).is_equal_to(Layer::Files);
        assert_that!(&app.deleted_streams).is_empty();
        assert_that!(app.selected_file().unwrap().display_name.as_str()).is_equal_to("beta.mkv");
        assert_that!(app.notice.as_deref().unwrap()).contains("removed");

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn directory_update_should_recover_automatically_after_scan_error() {
        // Arrange
        let mut app = test_file_app(&["alpha.mkv"]);
        let directory = app.directory.clone();

        // Act: scan failure
        app.apply_directory_snapshot(DirectorySnapshot::Error(
            "Directory is temporarily unavailable".to_string(),
        ));

        // Assert
        assert_that!(&app.files).is_empty();
        assert_that!(app.scan_error.as_deref().unwrap()).contains("temporarily unavailable");

        // Act: automatic retry succeeds
        app.apply_directory_snapshot(DirectorySnapshot::Files(
            scan_directory(&directory).unwrap(),
        ));

        // Assert
        assert_that!(&app.scan_error).is_none();
        assert_that!(app.selected_file().unwrap().display_name.as_str()).is_equal_to("alpha.mkv");

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn save_dialog_should_open_on_replace_with_start_selected_and_allow_copy_selection() {
        // Arrange
        let mut app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "disposition": {"default": 1}},
            {"index": 1, "codec_type": "audio", "disposition": {"default": 1}},
            {"index": 2, "codec_type": "subtitle", "disposition": {"default": 0}}
        ])));
        let directory = app.directory.clone();
        app.deleted_streams.insert(2);

        // Act
        app.request_save();
        let initial_field = app.save_dialog_field;
        app.move_save_dialog_cursor(-1);
        app.activate_save_dialog();

        // Assert
        assert_that!(app.dialog).contains(Dialog::ConfirmSave);
        assert_that!(initial_field).is_equal_to(SaveDialogField::Start);
        assert_that!(app.save_destination).is_equal_to(SaveDestination::CreateCopy);
        assert_that!(app.save_dialog_field).is_equal_to(SaveDialogField::Destination);

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn activating_start_should_send_the_selected_destination_to_the_worker() {
        // Arrange
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-save-dialog-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("alpha.mkv"), b"media").unwrap();
        let (probe_tx, _) = std::sync::mpsc::channel::<ProbeRequest>();
        let (edit_tx, edit_rx) = std::sync::mpsc::channel::<EditRequest>();
        let mut app = App::new(directory.clone(), probe_tx, edit_tx).unwrap();
        app.outcome = Some(ProbeOutcome::Video(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264", "disposition": {"default": 1}},
            {"index": 1, "codec_type": "audio", "codec_name": "aac", "disposition": {"default": 1}},
            {"index": 2, "codec_type": "subtitle", "codec_name": "subrip", "disposition": {"default": 0}}
        ]))));
        app.loading = false;
        app.reset_track_edits();
        app.layer = Layer::Streams;
        app.subtitle_capabilities = ToolCapabilities {
            ffmpeg: true,
            ffmpeg_muxers: BTreeSet::from(["mp4".to_string()]),
            ..ToolCapabilities::default()
        };
        app.container_target = Some(ContainerFormat::Mp4);
        app.deleted_streams.insert(2);
        app.request_save();
        app.move_save_dialog_cursor(-1);
        app.choose_save_destination(-1);
        app.choose_save_destination(1);
        app.move_save_dialog_cursor(1);

        // Act
        app.activate_save_dialog();
        let request = edit_rx.try_recv().unwrap();

        // Assert
        assert_that!(request.destination).is_equal_to(SaveDestination::CreateCopy);
        assert_that!(request.container).contains(ContainerFormat::Mp4);
        assert_that!(app.dialog).contains(Dialog::Processing);

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn completed_copy_should_refresh_and_select_the_output_file() {
        // Arrange
        let mut app = test_file_app(&["alpha.mkv"]);
        let directory = app.directory.clone();
        let source = directory.join("alpha.mkv");
        let output = directory.join("alpha-edited.mkv");
        std::fs::write(&output, b"edited media").unwrap();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        app.dialog = Some(Dialog::Processing);
        result_tx
            .send(EditEvent::Finished {
                path: source,
                outcome: EditOutcome::Completed {
                    output_path: output,
                    media_changed: true,
                },
            })
            .unwrap();

        // Act
        app.receive_edit_results(&result_rx);

        // Assert
        assert_that!(app.selected_file().unwrap().display_name.as_str())
            .is_equal_to("alpha-edited.mkv");
        assert_that!(app.notice.as_deref().unwrap()).contains("alpha-edited.mkv");

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn completed_cross_extension_replacement_should_win_over_watcher_removal() {
        // Arrange
        let mut app = test_file_app(&["alpha.mkv"]);
        let directory = app.directory.clone();
        let source = directory.join("alpha.mkv");
        let output = directory.join("alpha.mp4");
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        app.dialog = Some(Dialog::Processing);
        std::fs::write(&output, b"edited media").unwrap();
        std::fs::remove_file(&source).unwrap();
        app.apply_directory_snapshot(DirectorySnapshot::Files(
            scan_directory(&directory).unwrap(),
        ));

        // Act
        result_tx
            .send(EditEvent::Finished {
                path: source,
                outcome: EditOutcome::Completed {
                    output_path: output,
                    media_changed: true,
                },
            })
            .unwrap();
        app.receive_edit_results(&result_rx);

        // Assert
        assert_that!(app.dialog).is_none();
        assert_that!(app.selected_file().unwrap().display_name.as_str()).is_equal_to("alpha.mp4");
        assert_that!(app.notice.as_deref().unwrap()).contains("alpha.mp4");

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn processing_updates_should_continue_while_cancel_confirmation_is_open() {
        // Arrange
        let mut app = test_file_app(&["alpha.mkv"]);
        let directory = app.directory.clone();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        app.dialog = Some(Dialog::ConfirmCancel);
        result_tx
            .send(EditEvent::Progress(crate::edit::EditProgress::measured(
                crate::edit::EditPhase::WriteMedia("Encoding video".to_string()),
                0.5,
            )))
            .unwrap();

        // Act
        app.receive_edit_results(&result_rx);

        // Assert
        assert_that!(app.dialog).contains(Dialog::ConfirmCancel);
        assert_that!(app.edit_progress).contains(0.5);
        assert_that!(app.edit_progress_label.as_deref()).contains("Encoding video");

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn open_stream_details_should_open_one_layer_and_back_should_return_to_tracks() {
        // Arrange
        let mut app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "disposition": {"default": 1}}
        ])));
        let directory = app.directory.clone();
        app.selected_stream = 1;

        // Act
        app.open_stream_details();
        let opened_layer = app.layer;
        let went_back = app.back();

        // Assert
        assert_that!(opened_layer).is_equal_to(Layer::StreamDetails);
        assert_that!(went_back).is_true();
        assert_that!(app.layer).is_equal_to(Layer::Streams);

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn open_stream_details_should_open_when_the_container_is_selected() {
        // Arrange
        let mut app = test_app(
            MediaInfo::from_json(serde_json::json!({
                "format": {
                    "format_name": "matroska,webm",
                    "duration": "60.0"
                },
                "streams": [
                    {"index": 0, "codec_type": "video", "disposition": {"default": 1}}
                ]
            }))
            .unwrap(),
        );
        let directory = app.directory.clone();
        app.selected_stream = 0;

        // Act
        app.open_stream_details();

        // Assert
        assert_that!(app.layer).is_equal_to(Layer::StreamDetails);
        assert_that!(app.details_scroll).is_equal_to(0);
        assert_that!(app.details_max_scroll).is_equal_to(0);

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn reorder_imported_sidecar_should_swap_with_embedded_subtitles() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "subtitle", "codec_name": "subrip"}
            ]),
        );
        let sidecar = test_sidecar(&app, "movie.eng.srt", "eng");
        app.sidecars.push(sidecar);
        app.layer = Layer::Streams;
        // Focus on sidecar in Right column (row 2)
        let sidecar_row = app
            .track_rows()
            .iter()
            .position(|r| matches!(r, TrackRef::Sidecar(0)))
            .unwrap();
        app.selected_stream = sidecar_row;
        // Import sidecar (moves to Left column)
        app.transfer_subtitle(-1);

        // Act: move imported sidecar UP (Ctrl+k)
        app.move_selected_stream(-1);

        // Assert
        let active = app.active_left_subtitle_tracks();
        assert_that!(active)
            .contains_exactly_in_given_order([TrackRef::Sidecar(0), TrackRef::Embedded(1)]);
        assert_that!(app.selected_stream).is_equal_to(
            app.track_rows()
                .iter()
                .position(|r| matches!(r, TrackRef::Sidecar(0)))
                .unwrap(),
        );

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn open_stream_details_should_open_for_an_external_subtitle() {
        // Arrange
        let mut app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "disposition": {"default": 1}}
        ])));
        let directory = app.directory.clone();
        app.sidecars.push(SidecarEntry {
            path: directory.join("movie.eng.srt"),
            companion: None,
            display_name: "movie.eng.srt".to_string(),
            format: SubtitleFormat::SubRip,
            language: "eng".to_string(),
            forced: false,
            hearing_impaired: false,
            number: None,
            fingerprint: crate::files::FileFingerprint {
                length: 0,
                modified: None,
            },
            companion_fingerprint: None,
        });
        app.selected_stream = 2;

        // Act
        app.open_stream_details();

        // Assert
        assert_that!(app.layer).is_equal_to(Layer::StreamDetails);
        assert_that!(app.details_scroll).is_equal_to(0);

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn cancelling_processing_should_wait_for_worker_cleanup_and_preserve_staged_edits() {
        // Arrange
        let mut app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "disposition": {"default": 1}},
            {"index": 1, "codec_type": "audio", "disposition": {"default": 1}},
            {"index": 2, "codec_type": "subtitle", "disposition": {"default": 0}}
        ])));
        let directory = app.directory.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        app.deleted_streams.insert(2);
        app.edit_cancel = Some(cancelled.clone());
        app.dialog = Some(Dialog::Processing);
        let (result_tx, result_rx) = std::sync::mpsc::channel();

        // Act
        app.cancel_edit();

        // Assert
        assert_that!(cancelled.load(Ordering::Relaxed)).is_true();
        assert_that!(app.dialog).contains(Dialog::Processing);
        assert_that!(app.edit_progress_label.as_deref()).contains("Stopping active tools");
        assert_that!(&app.deleted_streams).contains(2);

        // Act
        result_tx
            .send(EditEvent::Finished {
                path: PathBuf::from("movie.mkv"),
                outcome: EditOutcome::Cancelled,
            })
            .unwrap();
        app.receive_edit_results(&result_rx);

        // Assert
        assert_that!(app.dialog).is_none();
        assert_that!(app.layer).is_equal_to(Layer::Streams);
        assert_that!(&app.deleted_streams).contains(2);
        assert_that!(app.notice.as_deref()).contains("Media edit cancelled.");

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn request_cancel_edit_should_require_confirmation_before_signalling_the_worker() {
        // Arrange
        let mut app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "disposition": {"default": 1}}
        ])));
        let directory = app.directory.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        app.edit_cancel = Some(cancelled.clone());
        app.dialog = Some(Dialog::Processing);

        // Act
        app.request_cancel_edit();

        // Assert
        assert_that!(app.dialog).contains(Dialog::ConfirmCancel);
        assert_that!(app.cancel_edit_choice).is_equal_to(CancelEditChoice::KeepProcessing);
        assert_that!(cancelled.load(Ordering::Relaxed)).is_false();

        // Act
        app.choose_cancel_edit(1);
        app.activate_cancel_edit();

        // Assert
        assert_that!(cancelled.load(Ordering::Relaxed)).is_true();
        assert_that!(app.dialog).contains(Dialog::Processing);
        assert_that!(app.edit_progress_label.as_deref()).contains("Stopping active tools");

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn dismiss_cancel_edit_should_return_to_processing_without_signalling_the_worker() {
        // Arrange
        let mut app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "disposition": {"default": 1}}
        ])));
        let directory = app.directory.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        app.edit_cancel = Some(cancelled.clone());
        app.dialog = Some(Dialog::ConfirmCancel);
        app.cancel_edit_choice = CancelEditChoice::CancelProcessing;

        // Act
        app.dismiss_cancel_edit();

        // Assert
        assert_that!(app.dialog).contains(Dialog::Processing);
        assert_that!(app.cancel_edit_choice).is_equal_to(CancelEditChoice::KeepProcessing);
        assert_that!(cancelled.load(Ordering::Relaxed)).is_false();

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn fold_operations_should_toggle_and_manage_folded_files() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        app.layer = Layer::Files;
        let directory = app.directory.clone();
        let file_path = app.files[0].path.clone();

        // Assert initial state: folded by default
        assert_that!(app.is_file_folded(&file_path)).is_true();

        // Act & Assert 1: Toggle unfold
        app.toggle_fold_selected_file();
        assert_that!(app.is_file_folded(&file_path)).is_false();

        // Act & Assert 2: Toggle fold
        app.toggle_fold_selected_file();
        assert_that!(app.is_file_folded(&file_path)).is_true();

        // Act & Assert 3: Unfold all & fold all
        app.unfold_all_files();
        assert_that!(app.is_file_folded(&file_path)).is_false();

        app.fold_all_files();
        assert_that!(app.is_file_folded(&file_path)).is_true();

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn keybindings_search_state_transitions_should_manage_query_and_active_flag() {
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();

        assert!(!app.keybindings_search.is_active);
        assert_eq!(app.keybindings_search.value, "");

        app.start_keybindings_search();
        assert!(app.keybindings_search.is_active);

        app.keybindings_search_push('a');
        app.keybindings_search_push('b');
        assert_eq!(app.keybindings_search.value, "ab");
        assert!(app.keybindings_search.is_active);

        app.keybindings_search_pop();
        assert_eq!(app.keybindings_search.value, "a");
        assert!(app.keybindings_search.is_active);

        app.finish_keybindings_search();
        assert!(!app.keybindings_search.is_active);
        assert_eq!(app.keybindings_search.value, "a");

        app.clear_keybindings_search();
        assert!(!app.keybindings_search.is_active);
        assert_eq!(app.keybindings_search.value, "");

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn text_input_state_should_edit_unicode_only_while_active() {
        // Arrange
        let mut input = TextInputState::new("Café".to_string());

        // Act / Assert: navigation mode ignores text.
        input.insert('!', 8, |_| true);
        assert_that!(input.value.as_str()).is_equal_to("Café");

        // Act: edit in the middle of a Unicode value.
        input.activate();
        input.move_cursor(-1);
        input.insert('!', 8, |_| true);
        input.backspace();
        input.move_home(false);
        input.delete();

        // Assert
        assert_that!(input.value.as_str()).is_equal_to("afé");
        assert_that!(input.cursor).is_equal_to(0);

        // Act / Assert: field validators remain reusable.
        input.move_home(true);
        input.insert('x', 8, |value| value.is_ascii_digit());
        input.insert('2', 8, |value| value.is_ascii_digit());
        assert_that!(input.value.as_str()).is_equal_to("afé2");
    }

    #[test]
    fn file_search_should_match_media_and_only_matching_sidecars_case_insensitively() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv", "movie.eng.srt", "movie.nld.srt", "other.mkv"]);
        app.layer = Layer::Files;
        let directory = app.directory.clone();
        let movie_path = directory.join("movie.mkv");
        assert_that!(app.is_file_folded(&movie_path)).is_true();

        // Act: match through one sidecar.
        app.start_file_search();
        for ch in "ENG".chars() {
            app.file_search_push(ch);
        }
        let entries = app.file_panel_entries();

        // Assert
        assert_that!(entries.len()).is_equal_to(1);
        let entry = &entries[0];
        assert_that!(app.files[entry.file_index].display_name.as_str()).is_equal_to("movie.mkv");
        let sidecars = app.sidecars_for_media(&movie_path);
        let names = entry
            .sidecar_indices
            .iter()
            .map(|index| sidecars[*index].display_name.as_str())
            .collect::<Vec<_>>();
        assert_that!(names).contains_exactly_in_given_order(["movie.eng.srt"]);

        // Act: a media-only match must not bring nonmatching sidecars along.
        app.cancel_file_search();
        app.start_file_search();
        for ch in "movie.mkv".chars() {
            app.file_search_push(ch);
        }
        let entries = app.file_panel_entries();

        // Assert
        assert_that!(entries.len()).is_equal_to(1);
        assert_that!(&entries[0].sidecar_indices).is_empty();
        assert_that!(app.is_file_folded(&movie_path)).is_true();

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn file_search_should_restore_cancelled_selection_and_keep_confirmed_selection() {
        // Arrange
        let mut app = test_file_app(&["alpha.mkv", "beta.mkv", "gamma.mkv"]);
        app.layer = Layer::Files;
        app.select_next();
        let directory = app.directory.clone();
        assert_that!(app.selected_file().unwrap().display_name.as_str()).is_equal_to("beta.mkv");

        // Act: move to a result, then cancel the search.
        app.start_file_search();
        for ch in "gamma".chars() {
            app.file_search_push(ch);
        }
        assert_that!(app.selected_file().unwrap().display_name.as_str()).is_equal_to("gamma.mkv");
        app.cancel_file_search();

        // Assert: cancellation restores the pre-search file.
        assert_that!(app.selected_file().unwrap().display_name.as_str()).is_equal_to("beta.mkv");

        // Act: confirm the same result, then clear the confirmed filter.
        app.start_file_search();
        for ch in "gamma".chars() {
            app.file_search_push(ch);
        }
        app.finish_file_search();
        app.clear_file_search();

        // Assert: the confirmed result remains selected.
        assert_that!(app.selected_file().unwrap().display_name.as_str()).is_equal_to("gamma.mkv");

        // Act and assert: no matches leave no hidden selection.
        app.start_file_search();
        for ch in "missing".chars() {
            app.file_search_push(ch);
        }
        assert_that!(app.file_panel_entries()).is_empty();
        assert_that!(app.selected_file()).is_none();

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn file_search_should_reconcile_to_the_first_remaining_live_match() {
        // Arrange
        let mut app = test_file_app(&["alpha.mkv", "alpha.eng.srt", "beta.mkv", "beta.eng.srt"]);
        app.layer = Layer::Files;
        let directory = app.directory.clone();
        app.start_file_search();
        for ch in "eng".chars() {
            app.file_search_push(ch);
        }
        app.finish_file_search();
        app.select_next();
        assert_that!(app.selected_file().unwrap().display_name.as_str()).is_equal_to("beta.mkv");
        std::fs::remove_file(directory.join("beta.eng.srt")).unwrap();

        // Act
        app.apply_directory_snapshot(DirectorySnapshot::Files(
            scan_directory(&directory).unwrap(),
        ));

        // Assert
        assert_that!(app.file_panel_entries().len()).is_equal_to(1);
        assert_that!(app.selected_file().unwrap().display_name.as_str()).is_equal_to("alpha.mkv");

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn subtitle_settings_should_search_language_and_edit_unicode_title_with_a_caret() {
        let mut app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264"},
            {
                "index": 1,
                "codec_type": "subtitle",
                "codec_name": "subrip",
                "tags": {"language": "eng"},
                "disposition": {"default": 0}
            }
        ])));
        let directory = app.directory.clone();
        app.selected_stream = app
            .track_rows()
            .iter()
            .position(|track| *track == TrackRef::Embedded(1))
            .unwrap();
        app.open_track_settings();

        app.move_subtitle_settings_cursor(1);
        app.activate_subtitle_settings();
        app.start_subtitle_language_search();
        for character in "dutch".chars() {
            app.input_subtitle_language(character);
        }
        assert_that!(app.filtered_subtitle_languages().len()).is_equal_to(1);
        app.activate_subtitle_settings();
        assert_that!(app.subtitle_popup_metadata().unwrap().language.as_str()).is_equal_to("nld");

        app.move_subtitle_settings_cursor(1);
        app.start_subtitle_title_input();
        for character in "Café".chars() {
            app.input_subtitle_title(character);
        }
        app.move_subtitle_title_cursor(-1);
        app.input_subtitle_title('!');
        app.escape_subtitle_settings();

        let metadata = app.subtitle_popup_metadata().unwrap();
        assert_that!(metadata.title.as_deref()).contains("Caf!é");
        assert_that!(
            app.subtitle_changes
                .get(&SubtitleSource::Embedded(1))
                .unwrap()
                .metadata
                .as_ref()
        )
        .is_some();

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn subtitle_settings_should_toggle_default_exclusively_and_hide_unsupported_flags() {
        let mut app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264"},
            {
                "index": 1,
                "codec_type": "subtitle",
                "codec_name": "mov_text",
                "tags": {"language": "eng"},
                "disposition": {"default": 0, "forced": 0}
            },
            {
                "index": 2,
                "codec_type": "subtitle",
                "codec_name": "mov_text",
                "tags": {"language": "nld"},
                "disposition": {"default": 1, "forced": 0}
            }
        ])));
        let directory = app.directory.clone();
        app.container_target = Some(ContainerFormat::Mov);
        app.selected_stream = app
            .track_rows()
            .iter()
            .position(|track| *track == TrackRef::Embedded(1))
            .unwrap();
        app.open_track_settings();

        assert_eq!(
            app.visible_subtitle_fields(),
            vec![
                SubtitleSettingsField::Codec,
                SubtitleSettingsField::Language,
                SubtitleSettingsField::Title,
                SubtitleSettingsField::Default,
            ]
        );

        for _ in 0..3 {
            app.move_subtitle_settings_cursor(1);
        }
        app.activate_subtitle_settings();
        assert_that!(app.default_streams.contains(&1)).is_true();
        assert_that!(app.default_streams.contains(&2)).is_false();

        assert_that!(app.subtitle_popup_metadata().unwrap().forced).is_false();
        assert_that!(app.subtitle_field_visible(SubtitleSettingsField::Forced)).is_false();
        assert_that!(app.subtitle_field_reason(SubtitleSettingsField::Forced)).is_none();

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn subtitle_settings_should_edit_captions_and_hearing_impaired_independently() {
        let mut app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264"},
            {
                "index": 1,
                "codec_type": "subtitle",
                "codec_name": "mov_text",
                "tags": {"language": "eng"},
                "disposition": {"captions": 0, "hearing_impaired": 0}
            }
        ])));
        let directory = app.directory.clone();
        app.container_target = Some(ContainerFormat::Mp4);
        app.selected_stream = app
            .track_rows()
            .iter()
            .position(|track| *track == TrackRef::Embedded(1))
            .unwrap();
        app.open_track_settings();

        app.container_target = Some(ContainerFormat::Matroska);
        assert_that!(app.subtitle_field_visible(SubtitleSettingsField::Cc)).is_false();
        assert_that!(app.subtitle_field_visible(SubtitleSettingsField::HearingImpaired)).is_true();
        app.container_target = Some(ContainerFormat::WebM);
        assert_that!(app.subtitle_field_visible(SubtitleSettingsField::Cc)).is_true();
        assert_that!(app.subtitle_field_visible(SubtitleSettingsField::HearingImpaired)).is_false();
        app.container_target = Some(ContainerFormat::Mov);
        assert_that!(app.subtitle_field_visible(SubtitleSettingsField::Cc)).is_false();
        assert_that!(app.subtitle_field_visible(SubtitleSettingsField::HearingImpaired)).is_false();
        app.container_target = Some(ContainerFormat::Mp4);
        assert_that!(app.subtitle_field_visible(SubtitleSettingsField::Cc)).is_true();
        assert_that!(app.subtitle_field_visible(SubtitleSettingsField::HearingImpaired)).is_true();

        app.subtitle_settings_popup.as_mut().unwrap().field = SubtitleSettingsField::Cc;
        app.activate_subtitle_settings();
        let metadata = app.subtitle_popup_metadata().unwrap();
        assert_that!(metadata.cc).is_true();
        assert_that!(metadata.hearing_impaired).is_false();

        app.subtitle_settings_popup.as_mut().unwrap().field =
            SubtitleSettingsField::HearingImpaired;
        app.activate_subtitle_settings();
        let metadata = app.subtitle_popup_metadata().unwrap();
        assert_that!(metadata.cc).is_true();
        assert_that!(metadata.hearing_impaired).is_true();

        app.subtitle_settings_popup.as_mut().unwrap().field = SubtitleSettingsField::Cc;
        app.activate_subtitle_settings();
        let metadata = app.subtitle_popup_metadata().unwrap();
        assert_that!(metadata.cc).is_false();
        assert_that!(metadata.hearing_impaired).is_true();

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn legacy_subtitle_languages_should_remain_editable_and_pass_save_validation() {
        let mut app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264"},
            {
                "index": 1,
                "codec_type": "subtitle",
                "codec_name": "subrip",
                "tags": {"language": "cze"},
                "disposition": {"default": 0}
            },
            {
                "index": 2,
                "codec_type": "subtitle",
                "codec_name": "subrip",
                "tags": {"language": "ger"},
                "disposition": {"default": 0}
            },
            {
                "index": 3,
                "codec_type": "subtitle",
                "codec_name": "subrip",
                "tags": {"language": "chi"},
                "disposition": {"default": 0}
            }
        ])));
        let directory = app.directory.clone();
        app.selected_stream = app
            .track_rows()
            .iter()
            .position(|track| *track == TrackRef::Embedded(1))
            .unwrap();

        app.open_track_settings();
        assert_that!(app.subtitle_popup_metadata().unwrap().language.as_str()).is_equal_to("ces");
        app.move_subtitle_settings_cursor(1);
        app.activate_subtitle_settings();
        let popup = app.subtitle_settings_popup.as_ref().unwrap();
        assert_that!(
            app.filtered_subtitle_languages()[popup.language_cursor]
                .code
                .as_str()
        )
        .is_equal_to("ces");
        app.escape_subtitle_settings();
        app.subtitle_settings_popup.as_mut().unwrap().field = SubtitleSettingsField::Default;
        app.activate_subtitle_settings();
        app.close_subtitle_settings();
        app.request_save();
        assert_that!(app.dialog).contains(Dialog::ConfirmSave);

        std::fs::remove_dir_all(directory).unwrap();
    }
}
