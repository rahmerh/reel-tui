use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    ops::Deref,
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
        AudioChannelLayout, AudioCodec, AudioMetadata, AudioRole, AudioSettings,
        CHANNEL_UPMIX_NOT_IMPLEMENTED, ContainerFormat, ContainerMetadata, CustomResolution,
        CustomScaling, EditEvent, EditOutcome, EditRequest, MINIMUM_CUSTOM_DIMENSION,
        SaveDestination, VideoCodec, VideoMetadata, VideoResolution, VideoRotation, VideoSettings,
        audio_bitrate_kbps, audio_requires_transcode, audio_stream_title,
        container_conflict_streams, container_conflicts, effective_audio_codec,
        imported_subtitle_conflicts, plan_requires_transcode, stream_channels, stream_disposition,
        stream_index, stream_rotation, subtitle_metadata_conflicts, validate_edit,
        video_stream_title,
    },
    files::{DirectorySnapshot, FileEntry, scan_directory},
    probe::{MediaInfo, ProbeOutcome, ProbeRequest, ProbeResponse},
    staging::{BatchItem, BatchItemStatus, BatchState, StagedEdit},
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

/// Which characters a text field accepts. An enum rather than a `fn(char) -> bool`
/// so [`TextInputConfig`] stays `Copy + Eq + Debug` and tests can assert the config a
/// site resolves to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CharClass {
    /// Any non-control character.
    Text,
    /// Non-control and non-whitespace, for filter queries typed against a word list.
    Word,
    /// ASCII digits only.
    Digits,
}

impl CharClass {
    pub fn accepts(self, character: char) -> bool {
        match self {
            Self::Text => !character.is_control(),
            Self::Word => !character.is_control() && !character.is_whitespace(),
            Self::Digits => character.is_ascii_digit(),
        }
    }
}

/// Everything that distinguishes one text field from another. Each site declares this
/// once so the rendered width, the scroll width, the length cap and the accepted
/// characters cannot drift apart the way they did when each was hardcoded at its call
/// site.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TextInputConfig {
    /// Value cells the field renders, one of which is reserved for the caret glyph.
    pub width: usize,
    pub max_len: usize,
    pub accepts: CharClass,
    /// Backspace on an empty value leaves the field instead of doing nothing.
    pub exit_on_empty_backspace: bool,
}

impl TextInputConfig {
    /// Value cells every fixed-width field renders. Sharing one number is what lets the
    /// container and subtitle popups line up their right edges, not just their labels.
    /// The resolution fields are the deliberate exception: they hold four digits.
    pub const DEFAULT_WIDTH: usize = 28;

    pub const CONTAINER_METADATA: Self = Self {
        width: Self::DEFAULT_WIDTH,
        max_len: 512,
        accepts: CharClass::Text,
        exit_on_empty_backspace: false,
    };
    pub const SUBTITLE_TITLE: Self = Self {
        width: Self::DEFAULT_WIDTH,
        max_len: 512,
        accepts: CharClass::Text,
        exit_on_empty_backspace: false,
    };
    pub const LANGUAGE_SEARCH: Self = Self {
        width: Self::DEFAULT_WIDTH,
        max_len: 64,
        accepts: CharClass::Word,
        exit_on_empty_backspace: true,
    };
    pub const RESOLUTION: Self = Self {
        width: 16,
        max_len: 20,
        accepts: CharClass::Digits,
        exit_on_empty_backspace: false,
    };

    /// A pane-bottom search bar, whose width is whatever the last frame measured.
    pub const fn search(width: usize) -> Self {
        Self {
            width,
            max_len: 256,
            accepts: CharClass::Text,
            exit_on_empty_backspace: true,
        }
    }

    /// Characters of the value on screen at once; one cell holds the caret glyph.
    pub const fn visible_width(self) -> usize {
        self.width.saturating_sub(1)
    }
}

/// Why a keystroke or paste did not land in full. Carried back to the renderer so a
/// refused character says why instead of looking like a dropped keypress.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputReject {
    /// The field does not accept characters of this class.
    Character(CharClass),
    /// The value is at its length cap.
    Full(usize),
}

/// What an edit did, so a caller can run its site-specific follow-up.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputEdit {
    Changed,
    Unchanged,
    /// Backspace on an empty value; the field has deactivated itself.
    Exited,
    /// Some or all of the input was refused. A paste can be partly accepted and still
    /// report this, so callers must run their follow-up for it as they would for
    /// [`InputEdit::Changed`].
    Rejected(InputReject),
}

/// Every text field in the application. Editing keys resolve to exactly one of these.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextInputSite {
    ContainerMetadata,
    AudioTitle,
    AudioLanguageSearch,
    VideoTitle,
    VideoLanguageSearch,
    SubtitleTitle,
    LanguageSearch,
    CustomResolution,
    FileSearch,
    KeybindingsSearch,
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

    pub fn insert(&mut self, character: char, config: TextInputConfig) -> InputEdit {
        if !self.is_active {
            return InputEdit::Unchanged;
        }
        if !config.accepts.accepts(character) {
            return InputEdit::Rejected(InputReject::Character(config.accepts));
        }
        if self.value.chars().count() >= config.max_len {
            return InputEdit::Rejected(InputReject::Full(config.max_len));
        }
        let byte = char_byte_index(&self.value, self.cursor);
        self.value.insert(byte, character);
        self.cursor += 1;
        self.keep_cursor_visible(config.visible_width());
        InputEdit::Changed
    }

    /// Inserts a whole run of text at the cursor, as a bracketed paste delivers it.
    /// Filtering and the length cap are applied once for the run rather than per
    /// character, so a paste is one edit and one redraw instead of one per keystroke.
    pub fn insert_str(&mut self, text: &str, config: TextInputConfig) -> InputEdit {
        if !self.is_active {
            return InputEdit::Unchanged;
        }
        let mut accepted = String::new();
        let mut reject = None;
        let mut remaining = config.max_len.saturating_sub(self.value.chars().count());
        for character in text.chars() {
            // A pasted line break would otherwise glue two lines into one word.
            let character = match character {
                '\n' | '\r' | '\t' if config.accepts == CharClass::Text => ' ',
                character => character,
            };
            if !config.accepts.accepts(character) {
                reject.get_or_insert(InputReject::Character(config.accepts));
                continue;
            }
            if remaining == 0 {
                reject = Some(InputReject::Full(config.max_len));
                break;
            }
            accepted.push(character);
            remaining -= 1;
        }
        if !accepted.is_empty() {
            let byte = char_byte_index(&self.value, self.cursor);
            self.value.insert_str(byte, &accepted);
            self.cursor += accepted.chars().count();
            self.keep_cursor_visible(config.visible_width());
        }
        match reject {
            Some(reject) => InputEdit::Rejected(reject),
            None if accepted.is_empty() => InputEdit::Unchanged,
            None => InputEdit::Changed,
        }
    }

    /// Deletes the word before the cursor, as `Ctrl-w` does in a shell: the run of
    /// whitespace immediately behind the cursor, then the run of non-whitespace behind
    /// that.
    pub fn delete_word_before(&mut self, config: TextInputConfig) -> InputEdit {
        if !self.is_active || self.cursor == 0 {
            return InputEdit::Unchanged;
        }
        let characters = self.value.chars().collect::<Vec<_>>();
        let mut start = self.cursor;
        while start > 0 && characters[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !characters[start - 1].is_whitespace() {
            start -= 1;
        }
        self.replace_range_by_chars(start, self.cursor, config)
    }

    /// Deletes everything before the cursor, as `Ctrl-u` does in a shell.
    pub fn clear_to_start(&mut self, config: TextInputConfig) -> InputEdit {
        if !self.is_active || self.cursor == 0 {
            return InputEdit::Unchanged;
        }
        self.replace_range_by_chars(0, self.cursor, config)
    }

    /// Removes `start..end` counted in characters and leaves the cursor where the text
    /// was, rescrolling to keep it visible.
    fn replace_range_by_chars(
        &mut self,
        start: usize,
        end: usize,
        config: TextInputConfig,
    ) -> InputEdit {
        if start >= end {
            return InputEdit::Unchanged;
        }
        let bytes = char_byte_index(&self.value, start)..char_byte_index(&self.value, end);
        self.value.replace_range(bytes, "");
        self.cursor = start;
        self.keep_cursor_visible(config.visible_width());
        InputEdit::Changed
    }

    pub fn backspace(&mut self, config: TextInputConfig) -> InputEdit {
        if !self.is_active {
            return InputEdit::Unchanged;
        }
        if self.value.is_empty() && config.exit_on_empty_backspace {
            self.deactivate();
            return InputEdit::Exited;
        }
        if self.cursor == 0 {
            return InputEdit::Unchanged;
        }
        let start = char_byte_index(&self.value, self.cursor - 1);
        let end = char_byte_index(&self.value, self.cursor);
        self.value.replace_range(start..end, "");
        self.cursor -= 1;
        self.keep_cursor_visible(config.visible_width());
        InputEdit::Changed
    }

    pub fn delete(&mut self, config: TextInputConfig) -> InputEdit {
        if !self.is_active || self.cursor >= self.value.chars().count() {
            return InputEdit::Unchanged;
        }
        let start = char_byte_index(&self.value, self.cursor);
        let end = char_byte_index(&self.value, self.cursor + 1);
        self.value.replace_range(start..end, "");
        self.keep_cursor_visible(config.visible_width());
        InputEdit::Changed
    }

    pub fn move_cursor(&mut self, direction: isize, config: TextInputConfig) -> InputEdit {
        if !self.is_active {
            return InputEdit::Unchanged;
        }
        let count = self.value.chars().count();
        self.cursor = if direction.is_negative() {
            self.cursor.saturating_sub(1)
        } else {
            self.cursor.saturating_add(1).min(count)
        };
        self.keep_cursor_visible(config.visible_width());
        InputEdit::Changed
    }

    pub fn move_home(&mut self, end: bool, config: TextInputConfig) -> InputEdit {
        if !self.is_active {
            return InputEdit::Unchanged;
        }
        self.cursor = if end { self.value.chars().count() } else { 0 };
        self.keep_cursor_visible(config.visible_width());
        InputEdit::Changed
    }

    pub fn keep_cursor_visible(&mut self, visible_width: usize) {
        // A search bar's width is only known once a frame has measured it. Scrolling
        // against a zero-width viewport would push the whole value off screen.
        if visible_width == 0 {
            return;
        }
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
    /// Value cells the last rendered frame gave this bar, matching
    /// [`TextInputConfig::width`]. Zero until the first draw, which
    /// [`TextInputState::keep_cursor_visible`] treats as "not measured yet".
    pub field_width: usize,
}

impl Deref for SearchState {
    type Target = TextInputState;

    fn deref(&self) -> &Self::Target {
        &self.input
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilePanelEntry {
    pub file_index: usize,
    pub sidecar_indices: Vec<usize>,
}

/// Memoized `App::file_panel_entries()` result along with the inputs it was built
/// from, so a cache hit can be verified by direct comparison rather than a dirty
/// flag (see the `file_panel_cache` field doc comment for why).
struct FilePanelCache {
    files: Vec<FileEntry>,
    sidecars_by_media: HashMap<PathBuf, Vec<SidecarEntry>>,
    query: String,
    entries: Vec<FilePanelEntry>,
}

impl SearchState {
    pub fn activate(&mut self) {
        self.input.activate();
    }

    pub fn deactivate(&mut self) {
        self.input.deactivate();
    }

    /// Empties the query *and* leaves the bar; the two always happen together, since
    /// every caller reaches this from an Escape that dismisses the search entirely.
    pub fn clear(&mut self) {
        self.input.clear();
        self.match_count = 0;
    }

    /// The config for a search bar sized by the last frame. A search bar reserves room
    /// for the widest match-count suffix so the value window does not shift a column
    /// every time the count crosses a digit boundary.
    pub fn config(&self) -> TextInputConfig {
        TextInputConfig::search(self.field_width)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Dialog {
    Keybindings,
    ContainerSettings,
    AudioSettings,
    VideoSettings,
    SubtitleSettings,
    /// A confirm-cancel prompt over whichever processing view is showing
    /// (`Dialog::BatchProcessing`, one item or many) — see `App::request_cancel_edit`.
    ConfirmCancel,
    Error,
    /// Confirms "process all staged files" before dispatching a batch — see
    /// `App::request_process_all`/`confirm_process_all`.
    ConfirmProcessAll,
    /// The processing popup for an in-flight batch — a single centered box (the
    /// original single-file design) when there's exactly one item, or a table with
    /// one row per file otherwise. See `App::active_batch`.
    BatchProcessing,
    /// "Are you sure?" before an `r`/`R` reset that discards more than a single
    /// field's value — see `App::pending_reset`/`ResetScope`.
    ConfirmReset,
    /// Lists every staged file whose background structural re-probe confirmed a
    /// genuine change on disk (not just an mtime bump) since it was staged, each with
    /// its own Keep/Discard choice — opens itself automatically, no manual action
    /// needed. See `App::conflicting_paths`/`maybe_open_conflict_dialog`.
    ResolveConflicts,
}

/// What an in-progress `Dialog::ConfirmReset` would discard if confirmed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResetScope {
    /// The `r` keybind on a file row in the Files panel — unstages one file.
    File(PathBuf),
    /// The `R` keybind anywhere outside the Files panel — resets the whole
    /// currently-open file back to its probed baseline.
    CurrentFile,
    /// The `R` keybind in the Files panel — unstages every staged file.
    AllFiles,
}

impl ResetScope {
    /// The confirm dialog's title and the file-list bullet text it should show.
    pub fn label(&self) -> String {
        match self {
            ResetScope::File(path) => {
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("this file");
                format!("Reset {name}?")
            }
            ResetScope::CurrentFile => "Reset this file's edits?".to_string(),
            ResetScope::AllFiles => "Reset every staged file?".to_string(),
        }
    }
}

/// The two-option cursor for `Dialog::ConfirmReset`, mirroring `CancelEditChoice`'s
/// "default to the safe choice" pattern.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ResetChoice {
    #[default]
    KeepEdits,
    ResetEdits,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CancelEditChoice {
    #[default]
    KeepProcessing,
    CancelProcessing,
}

/// The two-option cursor for `Dialog::ConfirmProcessAll`'s Start/Cancel button bar.
/// Defaults to `Start` (rather than the "safe choice" every other two-option dialog
/// defaults to) so a bare Enter keeps starting the batch immediately, matching the
/// dialog's behavior before it grew a visible button bar.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ConfirmProcessAllChoice {
    #[default]
    Start,
    Cancel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrackRef {
    Container,
    Embedded(u64),
    Sidecar(usize),
}

/// Whether a file (open or staged elsewhere) has pending edits and, if so, whether
/// they'd currently pass validation. Drives the file-list yellow/warning-triangle
/// markers and gates Ctrl+S's "process all staged files" action. See
/// `App::staged_file_status`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StagedFileStatus {
    Unstaged,
    Valid,
    Invalid(String),
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
    Rotation,
    Language,
    Title,
    Default,
    Commentary,
}

impl VideoSettingsField {
    pub const ALL: [Self; 7] = [
        Self::Codec,
        Self::Resolution,
        Self::Rotation,
        Self::Language,
        Self::Title,
        Self::Default,
        Self::Commentary,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Codec => "Codec",
            Self::Resolution => "Resolution",
            Self::Rotation => "Rotation",
            Self::Language => "Language",
            Self::Title => "Title",
            Self::Default => "Default",
            Self::Commentary => "Commentary",
        }
    }
}

#[derive(Clone, Debug)]
pub struct VideoSettingsPopup {
    pub stream_index: u64,
    pub field: VideoSettingsField,
    pub mode: VideoSettingsMode,
    pub help_visible: bool,
    pub codec_cursor: usize,
    pub resolution_cursor: usize,
    pub rotation_cursor: usize,
    pub language_cursor: usize,
    pub language_search: SearchState,
    pub title_input: TextInputState,
    pub custom_resolution: Option<CustomResolutionDraft>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum VideoSettingsMode {
    #[default]
    Summary,
    Dropdown,
    LanguageDropdown,
    TitleEdit,
    CustomResolution,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AudioSettingsField {
    #[default]
    Codec,
    ChannelLayout,
    Language,
    Title,
    Default,
    Commentary,
    HearingImpaired,
    AudioDescription,
    Original,
    Dubbed,
}

impl AudioSettingsField {
    pub const ALL: [Self; 10] = [
        Self::Codec,
        Self::ChannelLayout,
        Self::Language,
        Self::Title,
        Self::Default,
        Self::Commentary,
        Self::HearingImpaired,
        Self::AudioDescription,
        Self::Original,
        Self::Dubbed,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Codec => "Codec",
            Self::ChannelLayout => "Channel layout",
            Self::Language => "Language",
            Self::Title => "Title",
            Self::Default => "Default",
            Self::Commentary => "Commentary",
            Self::HearingImpaired => "Hearing impaired",
            Self::AudioDescription => "Audio description",
            Self::Original => "Original",
            Self::Dubbed => "Dubbed",
        }
    }

    pub fn role(self) -> Option<AudioRole> {
        match self {
            Self::Commentary => Some(AudioRole::Commentary),
            Self::HearingImpaired => Some(AudioRole::HearingImpaired),
            Self::AudioDescription => Some(AudioRole::AudioDescription),
            Self::Original => Some(AudioRole::Original),
            Self::Dubbed => Some(AudioRole::Dubbed),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AudioSettingsMode {
    #[default]
    Summary,
    Dropdown,
    LanguageDropdown,
    TitleEdit,
}

#[derive(Clone, Debug)]
pub struct AudioSettingsPopup {
    pub stream_index: u64,
    pub field: AudioSettingsField,
    pub mode: AudioSettingsMode,
    pub help_visible: bool,
    pub codec_cursor: usize,
    pub channel_cursor: usize,
    pub language_cursor: usize,
    pub language_search: SearchState,
    pub title_input: TextInputState,
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ContainerSettingsField {
    #[default]
    Format,
    Title,
    Comment,
    Date,
    Genre,
    Artist,
}

impl ContainerSettingsField {
    pub const ALL: [Self; 6] = [
        Self::Format,
        Self::Title,
        Self::Comment,
        Self::Date,
        Self::Genre,
        Self::Artist,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Format => "Format",
            Self::Title => "Title",
            Self::Comment => "Comment",
            Self::Date => "Date / Year",
            Self::Genre => "Genre",
            Self::Artist => "Artist / Director",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ContainerSettingsMode {
    #[default]
    Summary,
    FormatDropdown,
    TextEdit,
}

#[derive(Clone, Debug)]
pub struct ContainerSettingsPopup {
    pub field: ContainerSettingsField,
    pub mode: ContainerSettingsMode,
    pub help_visible: bool,
    pub format_cursor: usize,
    pub text_input: TextInputState,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioChoice<T> {
    pub value: T,
    pub label: String,
    pub current: bool,
    pub enabled: bool,
    pub reason: Option<String>,
}

/// How long `Dialog::ResolveConflicts`' button stays inert after the notice appears,
/// counting down inside the label. Long enough that a keystroke already in flight when
/// the notice popped up can't acknowledge it, and that the reverted-changes list gets
/// read rather than dismissed by reflex. See `App::conflict_countdown`.
pub(crate) const CONFLICT_COUNTDOWN: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct CacheKey {
    pub(crate) path: PathBuf,
    pub(crate) length: u64,
    pub(crate) modified: Option<std::time::SystemTime>,
}

impl CacheKey {
    fn for_file(file: &FileEntry) -> Self {
        Self {
            path: file.path.clone(),
            length: file.fingerprint.length,
            modified: file.fingerprint.modified,
        }
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
    /// Scroll offset (in rows, one `BatchItem` per row) for `Dialog::BatchProcessing`
    /// when more staged files are being processed than fit in the popup at once.
    /// Derived from `batch_cursor` at render time by `sync_batch_scroll`.
    pub batch_scroll: u16,
    /// Highlighted row in `Dialog::BatchProcessing`. Purely navigational: it never
    /// follows the file currently being processed, only the user's own movement.
    pub batch_cursor: usize,
    pub confirm_process_all_scroll: u16,
    pub confirm_process_all_max_scroll: u16,
    pub layer: Layer,
    pub selected_stream: usize,
    pub stream_order: Vec<u64>,
    moved_streams: BTreeSet<u64>,
    pub deleted_streams: BTreeSet<u64>,
    pub default_streams: BTreeSet<u64>,
    pub default_sidecars: BTreeSet<usize>,
    pub audio_settings: BTreeMap<u64, AudioSettings>,
    pub audio_settings_popup: Option<AudioSettingsPopup>,
    pub video_settings: BTreeMap<u64, VideoSettings>,
    pub video_settings_popup: Option<VideoSettingsPopup>,
    pub sidecars: Vec<SidecarEntry>,
    pub subtitle_columns_side_by_side: bool,
    pub subtitle_changes: BTreeMap<SubtitleSource, SubtitleChange>,
    pub left_subtitle_order: Vec<TrackRef>,
    pub subtitle_settings_popup: Option<SubtitleSettingsPopup>,
    pub subtitle_capabilities: ToolCapabilities,
    pub container_target: Option<ContainerFormat>,
    pub container_metadata: Option<ContainerMetadata>,
    pub container_settings_popup: Option<ContainerSettingsPopup>,
    pub dialog: Option<Dialog>,
    pub notice: Option<String>,
    pub edit_error: Option<String>,
    pub cancel_edit_choice: CancelEditChoice,
    pub confirm_process_all_choice: ConfirmProcessAllChoice,
    pub scan_error: Option<String>,
    request_tx: Sender<ProbeRequest>,
    /// Every staged file's `EditRequest` is classified by `plan_requires_transcode`
    /// (in `confirm_process_all`) and sent to whichever of these matches.
    transcode_tx: Sender<EditRequest>,
    remux_tx: Sender<EditRequest>,
    /// Optional best-effort desktop notification channel. `main` only supplies it
    /// when notifications are enabled in the user's config; tests and other library
    /// consumers remain notification-free unless they explicitly opt in.
    completion_notification_tx: Option<Sender<PathBuf>>,
    /// State for an in-flight "process all staged files" batch
    /// (`Dialog::BatchProcessing`/`ConfirmCancel`) — the sole edit-processing path;
    /// even a single staged file goes through a one-item batch, so `App` no longer
    /// tracks separate single-file progress/cancellation fields.
    pub active_batch: Option<crate::staging::BatchState>,
    /// What an in-progress `Dialog::ConfirmReset` would discard if confirmed.
    pending_reset: Option<ResetScope>,
    pub reset_choice: ResetChoice,
    generation: u64,
    pending_since: Option<Instant>,
    pub(crate) cache: HashMap<CacheKey, ProbeOutcome>,
    original_stream_order: Vec<u64>,
    original_default_streams: BTreeSet<u64>,
    /// See `StagedEdit::track_groups` — the live-editing-field counterpart, kept in
    /// sync with `original_stream_order` (populated on `reset_track_edits`, snapshotted
    /// into a `StagedEdit` on staging, restored from it on reopening).
    track_groups: BTreeMap<u64, &'static str>,
    sidecars_by_media: HashMap<PathBuf, Vec<SidecarEntry>>,
    unfolded_files: BTreeSet<PathBuf>,
    /// Memoized `file_panel_entries()` result. Rebuilding that Vec re-lowercases every
    /// file/sidecar display name, which is wasteful when called repeatedly (every
    /// render frame plus several call sites per keystroke) with nothing having
    /// changed. Validity is checked by comparing full snapshots (not a dirty flag)
    /// because `files` is a `pub` field callers — including tests — assign to
    /// directly, bypassing `reconcile_files`; a counter bumped only there would go
    /// stale against that kind of external mutation.
    file_panel_cache: RefCell<Option<FilePanelCache>>,
    /// Edits staged on files other than (or including, transiently, before a
    /// selection change is finalized) the one currently open. Lets a user edit file
    /// A, switch to file B, edit it too, and process both instead of losing A's
    /// edits the moment the selection moves. See `snapshot_current_edits` and
    /// `load_staged_or_reset`.
    pub staged_edits: HashMap<PathBuf, StagedEdit>,
    pub is_network_mount: bool,
    pub disk_cache: crate::cache::DiskCache,
    /// Sender for the second, non-coalescing probe worker (`probe::spawn_conflict_probe_worker`)
    /// dedicated to background staleness re-checks — kept separate from `request_tx`
    /// so a burst of several changed staged files can never starve, or be starved by,
    /// the interactive per-selection probe. See `reconcile_files`'s dispatch and
    /// `receive_conflict_probe_results`'s answering half.
    conflict_tx: Sender<ProbeRequest>,
    /// For each path with a background structural check dispatched (queued or
    /// running) against the conflict worker, the exact fingerprint that check is for.
    /// Prevents re-enqueuing the same fingerprint on every reconcile pass while a
    /// file stays stale, and lets `receive_conflict_probe_results` recognize a
    /// response as superseded (the file changed again since this check was
    /// dispatched) rather than authoritative.
    pending_conflict_checks: HashMap<PathBuf, crate::files::FileFingerprint>,
    /// When `Dialog::ResolveConflicts` opened, driving `conflict_countdown`. The
    /// notice is the one dialog that appears unprompted — a background probe can
    /// answer at any moment, including mid-keystroke — so its button stays inert for
    /// `CONFLICT_COUNTDOWN` first, or an Enter already in flight for something else
    /// would silently revert staged work.
    pub(crate) conflict_opened_at: Option<Instant>,
    pub conflict_scroll: u16,
    pub conflict_max_scroll: u16,
    pub keybindings_search: SearchState,
    pub file_search: SearchState,
    file_search_origin: Option<PathBuf>,
    /// The last refused input and the field it was aimed at. Kept until that field
    /// accepts something or is left, so the explanation stays on screen long enough to
    /// read instead of flashing for a single frame.
    text_input_reject: Option<(TextInputSite, InputReject)>,
}

impl App {
    pub fn new(
        directory: PathBuf,
        request_tx: Sender<ProbeRequest>,
        conflict_tx: Sender<ProbeRequest>,
        transcode_tx: Sender<EditRequest>,
        remux_tx: Sender<EditRequest>,
    ) -> Result<Self> {
        let is_network_mount = crate::mount::is_network_mount(&directory);
        // `disk_cache` can span every directory the user has ever browsed, so eagerly
        // cloning all of it into the in-memory lookup here would do O(total cached
        // entries) work — and `reconcile_files` would immediately prune almost all of
        // it right back out again once the first directory scan narrows `self.files`
        // down to what's actually in `directory`. Instead `cache` starts empty and
        // `reconcile_files` populates it on demand, per file, from `disk_cache`.
        let disk_cache = crate::cache::DiskCache::load();

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
            batch_scroll: 0,
            batch_cursor: 0,
            confirm_process_all_scroll: 0,
            confirm_process_all_max_scroll: 0,
            layer: Layer::Files,
            selected_stream: 0,
            stream_order: Vec::new(),
            moved_streams: BTreeSet::new(),
            deleted_streams: BTreeSet::new(),
            default_streams: BTreeSet::new(),
            default_sidecars: BTreeSet::new(),
            audio_settings: BTreeMap::new(),
            audio_settings_popup: None,
            video_settings: BTreeMap::new(),
            video_settings_popup: None,
            sidecars: Vec::new(),
            subtitle_columns_side_by_side: false,
            subtitle_changes: BTreeMap::new(),
            left_subtitle_order: Vec::new(),
            subtitle_settings_popup: None,
            subtitle_capabilities: ToolCapabilities::detect_cached(),
            container_target: None,
            container_metadata: None,
            container_settings_popup: None,
            dialog: None,
            notice: None,
            edit_error: None,
            cancel_edit_choice: CancelEditChoice::KeepProcessing,
            confirm_process_all_choice: ConfirmProcessAllChoice::Start,
            scan_error: None,
            request_tx,
            transcode_tx,
            remux_tx,
            completion_notification_tx: None,
            active_batch: None,
            pending_reset: None,
            reset_choice: ResetChoice::default(),
            generation: 0,
            pending_since: None,
            cache: HashMap::new(),
            original_stream_order: Vec::new(),
            original_default_streams: BTreeSet::new(),
            track_groups: BTreeMap::new(),
            sidecars_by_media: HashMap::new(),
            unfolded_files: BTreeSet::new(),
            file_panel_cache: RefCell::new(None),
            staged_edits: HashMap::new(),
            is_network_mount,
            disk_cache,
            conflict_tx,
            pending_conflict_checks: HashMap::new(),
            conflict_opened_at: None,
            conflict_scroll: 0,
            conflict_max_scroll: 0,
            keybindings_search: SearchState::default(),
            file_search: SearchState::default(),
            file_search_origin: None,
            text_input_reject: None,
        };
        let snapshot = match scan_directory(&app.directory) {
            Ok(files) => DirectorySnapshot::Files(files),
            Err(error) => DirectorySnapshot::Error(error.to_string()),
        };
        app.apply_directory_snapshot(snapshot);
        Ok(app)
    }

    pub fn set_completion_notification_sender(&mut self, sender: Option<Sender<PathBuf>>) {
        self.completion_notification_tx = sender;
    }

    /// Returns whether any snapshot was applied, so the main loop can skip redrawing
    /// when nothing arrived. `spawn_directory_monitor` only sends a snapshot when its
    /// content actually differs from the previous one, so this is rarely a false
    /// positive.
    pub fn receive_directory_snapshots(&mut self, receiver: &Receiver<DirectorySnapshot>) -> bool {
        let mut changed = false;
        while let Ok(snapshot) = receiver.try_recv() {
            self.apply_directory_snapshot(snapshot);
            changed = true;
        }
        changed
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
            Some(Dialog::BatchProcessing | Dialog::ConfirmCancel)
        );
        if selected_removed && was_processing {
            return;
        }

        // Snapshot the open file's live edits *before* `self.files` is reassigned to
        // the rescanned list, while `self.selected_file()` still resolves to
        // `old_file`'s pre-change fingerprint. This must happen before the per-file
        // staged-edit reconciliation loop below, so that loop treats this file
        // exactly like any other staged file whose fingerprint moved — routing it
        // through the same stale/conflict-probe/auto-resolve pipeline instead of the
        // unconditional wipe the `selected_changed` branch used to do further down.
        if selected_changed && !was_processing && self.has_track_edits() {
            self.snapshot_current_edits();
        }

        self.files = files;
        self.sidecars_by_media = sidecars_by_media;

        let current_paths: std::collections::HashSet<&Path> =
            self.files.iter().map(|file| file.path.as_path()).collect();
        let mut current_keys: std::collections::HashSet<CacheKey> =
            self.files.iter().map(CacheKey::for_file).collect();
        // Staged edits describe the content they were staged against, not the content
        // on disk now, so the probe at each staged fingerprint has to outlive the
        // change that made it stale — otherwise `staged_file_summary` and the conflict
        // notice's `conflicting_change_summary` have nothing to name the staged tracks
        // with, precisely when the user most needs to read them.
        current_keys.extend(self.staged_edits.iter().map(|(path, staged)| CacheKey {
            path: path.clone(),
            length: staged.fingerprint.length,
            modified: staged.fingerprint.modified,
        }));
        self.unfolded_files
            .retain(|path| current_paths.contains(path.as_path()));
        self.cache.retain(|key, _| current_keys.contains(key));
        // Only truly-gone files lose their staged entry; a file that's still present
        // just gets flagged stale below if its fingerprint no longer matches what was
        // staged against, so the user can see and resolve it rather than losing the
        // staged work silently.
        self.staged_edits
            .retain(|path, _| current_paths.contains(path.as_path()));
        self.pending_conflict_checks
            .retain(|path, _| current_paths.contains(path.as_path()));
        for file in &self.files {
            let Some(staged) = self.staged_edits.get_mut(&file.path) else {
                continue;
            };
            staged.stale = staged.fingerprint != file.fingerprint;
            if !staged.stale {
                staged.conflict_groups.clear();
                self.pending_conflict_checks.remove(&file.path);
                continue;
            }
            // Dispatch a background structural re-check, but only once per fingerprint
            // — otherwise every reconcile pass while a file stays stale (up to once a
            // second locally) would re-enqueue it, and the second, non-coalescing
            // conflict worker answers every request it's sent rather than dropping
            // superseded ones itself (see `probe::spawn_conflict_probe_worker`).
            if self.pending_conflict_checks.get(&file.path) != Some(&file.fingerprint) {
                let _ = self.conflict_tx.send(ProbeRequest {
                    generation: 0,
                    path: file.path.clone(),
                    fingerprint: file.fingerprint,
                    is_network_mount: self.is_network_mount,
                });
                self.pending_conflict_checks
                    .insert(file.path.clone(), file.fingerprint);
            }
        }
        // Pull in any disk-cached outcome for files that just appeared and aren't in
        // the in-memory lookup yet (typically every file, on startup) instead of
        // eagerly cloning the whole (potentially much larger, multi-directory) disk
        // cache up front.
        for file in &self.files {
            let key = CacheKey::for_file(file);
            if self.cache.contains_key(&key) {
                continue;
            }
            if let Some(entry) = self.disk_cache.entries.get(&file.path)
                && entry.length == file.fingerprint.length
                && entry.modified == file.fingerprint.modified
            {
                self.cache.insert(key, entry.outcome.clone());
            }
        }
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
                if !self.has_track_edits() {
                    self.clear_edit_state();
                    self.notice =
                        Some("Selected file changed; reloaded latest metadata.".to_string());
                    self.queue_probe();
                }
                // Otherwise: already snapshotted into `staged_edits` above and now
                // flowing through the same stale/conflict-probe pipeline every other
                // staged file uses. Deliberately not calling `queue_probe()` here —
                // it force-resets `self.layer` to `Layer::Files`, which would eject
                // the user from the Streams view they're actively in. `self.outcome`
                // is refreshed lazily once resolution actually completes (see
                // `refresh_cached_outcome`).
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
            if let Some(batch) = &self.active_batch {
                batch.cancelled.store(true, Ordering::Relaxed);
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

    /// Builds the filtered/searchable file-panel listing. This rebuilds from scratch
    /// (re-lowercasing every file and sidecar display name) only when `files`,
    /// `sidecars_by_media`, or the search query differ from the last call; otherwise
    /// it returns a cheap clone of the memoized result. Validity is checked by
    /// comparing full snapshots rather than a dirty flag/counter, since `files` is a
    /// `pub` field some callers (including tests) assign to directly. This method is
    /// called on every render frame plus several times per keystroke/reconcile, so the
    /// cache matters even though the returned `Vec` itself is still cloned per call.
    pub fn file_panel_entries(&self) -> Vec<FilePanelEntry> {
        let query = self.file_search.value.trim().to_lowercase();
        {
            let cache = self.file_panel_cache.borrow();
            if let Some(cached) = cache.as_ref()
                && cached.query == query
                && cached.files == self.files
                && cached.sidecars_by_media == self.sidecars_by_media
            {
                return cached.entries.clone();
            }
        }

        let entries = self.compute_file_panel_entries(&query);
        *self.file_panel_cache.borrow_mut() = Some(FilePanelCache {
            files: self.files.clone(),
            sidecars_by_media: self.sidecars_by_media.clone(),
            query,
            entries: entries.clone(),
        });
        entries
    }

    fn compute_file_panel_entries(&self, query: &str) -> Vec<FilePanelEntry> {
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
                                .contains(query)
                                .then_some(index)
                        })
                        .collect::<Vec<_>>()
                };
                let file_matches =
                    query.is_empty() || file.display_name.to_lowercase().contains(query);
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
            self.snapshot_current_edits();
            self.dialog = None;
            self.notice = None;
            self.edit_error = None;
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
            self.load_staged_or_reset();
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
            is_network_mount: self.is_network_mount,
        });
        self.pending_since = None;
    }

    /// Returns whether any probe response was drained, so the main loop can skip
    /// redrawing when none arrived.
    pub fn receive_probe_results(&mut self, receiver: &Receiver<ProbeResponse>) -> bool {
        // Built once per drain rather than re-scanning `self.files` per response, and
        // the disk cache is flushed at most once per batch instead of once per
        // response — `DiskCache::save` rewrites the entire cache file, so saving once
        // per drained probe result turned opening an uncached directory into an
        // O(files x cache size) blocking-write storm on the UI thread.
        let current_keys: std::collections::HashSet<CacheKey> =
            self.files.iter().map(CacheKey::for_file).collect();
        let mut disk_cache_dirty = false;
        let mut received = false;
        while let Ok(response) = receiver.try_recv() {
            received = true;
            let key = CacheKey {
                path: response.path.clone(),
                length: response.fingerprint.length,
                modified: response.fingerprint.modified,
            };
            if current_keys.contains(&key) {
                self.cache.insert(key, response.outcome.clone());
                self.disk_cache.insert(
                    response.path.clone(),
                    response.fingerprint.length,
                    response.fingerprint.modified,
                    response.outcome.clone(),
                );
                disk_cache_dirty = true;
            }
            if response.generation == self.generation
                && self.selected_file().is_some_and(|file| {
                    file.path == response.path && file.fingerprint == response.fingerprint
                })
            {
                self.outcome = Some(response.outcome);
                self.loading = false;
                self.selected_stream = 0;
                self.load_staged_or_reset();
            }
        }
        if disk_cache_dirty {
            let _ = self.disk_cache.save();
        }
        received
    }

    /// Returns whether any conflict-check response was drained, so the main loop can
    /// skip redrawing when none arrived. Answers arrive from the second, FIFO probe
    /// worker (`conflict_tx`/`probe::spawn_conflict_probe_worker`) that `reconcile_files`
    /// dispatches into — every request it sends gets an answer here, not just the
    /// newest, unlike `receive_probe_results`'s interactive channel.
    pub fn receive_conflict_probe_results(&mut self, receiver: &Receiver<ProbeResponse>) -> bool {
        let mut disk_cache_dirty = false;
        let mut received = false;
        while let Ok(response) = receiver.try_recv() {
            received = true;
            // Superseded: the file changed again since this check was dispatched (a
            // newer check now owns this path in `pending_conflict_checks`, or the file
            // is gone entirely) — this answer describes on-disk state that's no longer
            // current, so it must not resolve or confirm anything.
            if self.pending_conflict_checks.get(&response.path) != Some(&response.fingerprint) {
                continue;
            }
            self.pending_conflict_checks.remove(&response.path);

            let key = CacheKey {
                path: response.path.clone(),
                length: response.fingerprint.length,
                modified: response.fingerprint.modified,
            };
            self.cache.insert(key, response.outcome.clone());
            self.disk_cache.insert(
                response.path.clone(),
                response.fingerprint.length,
                response.fingerprint.modified,
                response.outcome.clone(),
            );
            disk_cache_dirty = true;

            let Some(staged) = self.staged_edits.get(&response.path) else {
                continue;
            };
            if !staged.stale {
                // Resolved another way (manual reopen, reset) while this check was in
                // flight — nothing left to do.
                continue;
            }
            // The only thing that forces the notice is a *structural* conflict: a
            // track type this edit stages changes for no longer holding the tracks it
            // was staged against. Everything else auto-resolves here. A change
            // unrelated to what's staged (an untouched audio track's tags changing
            // while only video settings are staged, or an untouched group gaining or
            // losing a track entirely) is absorbed, and so is a staged edit that
            // reconciles cleanly but has become *invalid* against the new content —
            // say a staged MP4 conversion after a SubRip track appeared. That last one
            // is not the ground moving under the user's edit, it's an edit that can't
            // be processed as-is, which `staged_file_status` already reports with the
            // compatibility markers and Ctrl+S block built for exactly that.
            let reconciled = match &response.outcome {
                ProbeOutcome::Video(fresh_info) => Some(reconcile_untouched_groups(
                    fresh_info,
                    &staged.stream_order,
                    &staged.deleted_streams,
                    &staged.default_streams,
                    &staged.original_stream_order,
                    &staged.original_default_streams,
                    &staged.track_groups,
                    &staged.moved_streams,
                    &staged.audio_settings,
                    &staged.video_settings,
                    &staged.subtitle_changes,
                    &BTreeSet::new(),
                )),
                _ => None,
            };

            // Captured before the mutable `staged_edits` borrow below — needed to
            // resync the live edit fields if this is the currently-open file (see
            // `apply_staged_snapshot`), since a quiet auto-resolve here would
            // otherwise leave the Streams view showing pre-reconciliation data.
            let is_open = self
                .selected_file()
                .is_some_and(|file| file.path == response.path);

            let Some(staged) = self.staged_edits.get_mut(&response.path) else {
                continue;
            };
            // Not probeable as video (mid-write, or replaced by something else
            // entirely): there are no tracks to reconcile against and so no track type
            // to name in a notice. Left `stale`, which keeps it out of Ctrl+S;
            // `reconcile_files` re-dispatches a check on the next fingerprint change,
            // so this resolves itself once the file settles.
            let Some(reconciled) = reconciled else {
                continue;
            };
            if !reconciled.conflicting_groups.is_empty() {
                staged.conflict_groups = reconciled.conflicting_groups;
                continue;
            }
            staged.stream_order = reconciled.stream_order;
            staged.deleted_streams = reconciled.deleted_streams;
            staged.default_streams = reconciled.default_streams;
            staged.original_stream_order = reconciled.original_stream_order;
            staged.original_default_streams = reconciled.original_default_streams;
            staged.track_groups = reconciled.track_groups;
            staged.fingerprint = response.fingerprint;
            staged.stale = false;
            staged.conflict_groups = BTreeSet::new();
            let snapshot = is_open.then(|| staged.clone());
            let name = response
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("file");
            // Deliberately says the edits were *kept*, not that they're still valid:
            // reconciling cleanly no longer implies passing every save gate, since a
            // newly-invalid edit now takes this path too and reports itself through
            // `staged_file_status`.
            self.notice = Some(format!("{name} changed on disk; staged edits kept."));
            if let Some(snapshot) = snapshot {
                self.refresh_cached_outcome();
                self.apply_staged_snapshot(&snapshot);
            }
        }
        if disk_cache_dirty {
            let _ = self.disk_cache.save();
        }
        received
    }

    /// Returns whether any edit event was drained, so the main loop can skip
    /// redrawing when none arrived. Every `EditRequest` is now dispatched as part of
    /// a batch (even a single staged file becomes a one-item batch), so an event with
    /// no `active_batch` to attribute it to is a stale leftover — discarded, not
    /// applied to anything.
    pub fn receive_edit_results(&mut self, receiver: &Receiver<EditEvent>) -> bool {
        let mut received = false;
        while let Ok(event) = receiver.try_recv() {
            received = true;
            if self.active_batch.is_some() {
                self.apply_batch_event(event);
            }
        }
        received
    }

    /// Routes one `EditEvent` to the matching row in `active_batch` by path.
    fn apply_batch_event(&mut self, event: EditEvent) {
        match event {
            EditEvent::Progress { path, progress } => {
                if let Some(batch) = &mut self.active_batch
                    && let Some(item) = batch.item_mut(&path)
                {
                    item.status = BatchItemStatus::Running;
                    item.fraction = progress.fraction;
                    item.label = Some(progress.label());
                }
            }
            EditEvent::Finished { path, outcome } => {
                let mut output_path = None;
                let status = match outcome {
                    EditOutcome::Completed {
                        output_path: output,
                        ..
                    } => {
                        self.unstage_consumed_file(&path);
                        if let Some(sender) = &self.completion_notification_tx {
                            let _ = sender.send(output.clone());
                        }
                        output_path = Some(output);
                        BatchItemStatus::Completed
                    }
                    EditOutcome::Cancelled => BatchItemStatus::Cancelled,
                    EditOutcome::SourceChanged(error) => {
                        // The attempted edit was against stale data; the staged
                        // edits are no longer meaningful and must be redone.
                        self.unstage_consumed_file(&path);
                        BatchItemStatus::Failed(error)
                    }
                    EditOutcome::Failed(error) => BatchItemStatus::Failed(error),
                };
                if let Some(batch) = &mut self.active_batch
                    && let Some(item) = batch.item_mut(&path)
                {
                    item.status = status;
                    item.fraction = None;
                    item.output_path = output_path;
                }
                self.finish_batch_if_done();
            }
        }
    }

    /// Drops `path`'s staged edits once a worker has consumed them — either applied
    /// them (`Completed`) or found them unusable against the file on disk
    /// (`SourceChanged`). Either way they must not be staged any more.
    ///
    /// Clearing the *live* edit fields too when `path` is the open file is the half
    /// that isn't optional: `staged_edits` is only half of where an edit lives, and
    /// `finish_batch_if_done` rescans the directory immediately after this. That
    /// rescan sees the file's fingerprint move — because this app just moved it — so
    /// `reconcile_files` snapshots whatever the open file still holds live back into
    /// `staged_edits` against the *pre-save* fingerprint, marks it stale, and the
    /// structural re-check then correctly reports that the tracks the edit names are
    /// gone. The user gets an unskippable conflict notice demanding they discard the
    /// edit that just succeeded. The baseline is rebuilt from the new content by the
    /// probe `finish_batch_if_done` queues (`load_staged_or_reset`), so clearing here
    /// loses nothing.
    fn unstage_consumed_file(&mut self, path: &Path) {
        self.staged_edits.remove(path);
        if self.selected_file().is_some_and(|file| file.path == path) {
            self.clear_track_edits();
        }
    }

    /// Once every item in the active batch has reached a terminal state, closes the
    /// dialog, summarizes the outcome as a notice, and re-scans the directory so
    /// renamed/converted output files and refreshed staged-status markers show up.
    fn finish_batch_if_done(&mut self) {
        let Some(batch) = &self.active_batch else {
            return;
        };
        if !batch.is_finished() {
            return;
        }
        // With exactly one file, the specific reason is more useful than an aggregate
        // count — matches the wording the single-file flow always used.
        let notice = if let [item] = batch.items.as_slice() {
            match &item.status {
                BatchItemStatus::Completed => match &item.output_path {
                    Some(output) if *output != item.path => format!(
                        "Media edits saved to {}.",
                        output
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("edited copy")
                    ),
                    _ => "Media edits saved.".to_string(),
                },
                BatchItemStatus::Cancelled => "Media edit cancelled.".to_string(),
                BatchItemStatus::Failed(reason) => reason.clone(),
                BatchItemStatus::Pending | BatchItemStatus::Running => unreachable!(),
            }
        } else {
            let total = batch.items.len();
            let completed = batch
                .items
                .iter()
                .filter(|item| item.status == BatchItemStatus::Completed)
                .count();
            let failed = batch
                .items
                .iter()
                .filter(|item| matches!(item.status, BatchItemStatus::Failed(_)))
                .count();
            let cancelled = batch
                .items
                .iter()
                .filter(|item| item.status == BatchItemStatus::Cancelled)
                .count();
            summarize_batch_outcome(total, completed, failed, cancelled)
        };
        // With exactly one file, explicitly reselect what it became (its
        // `output_path` can differ from the source, e.g. a container conversion
        // renaming `movie.mkv` to `movie.mp4`) — `reconcile_files`'s generic
        // "old selection vanished, pick a fallback" path has no way to know which of
        // possibly many files is "the" output for a multi-file batch, so this only
        // applies to the single-item case.
        let reselect = match batch.items.as_slice() {
            [item] => item.output_path.clone(),
            _ => None,
        };
        // For a single-item batch, whichever path the file should now be found under
        // (its `output_path`, or its original path if nothing renamed it) — used below
        // to tell "the file is still around, drop back into its Streams view" apart
        // from "the file genuinely vanished, `reconcile_files` already picked a
        // sensible fallback in the Files panel and that should stand."
        let expected_selection = match batch.items.as_slice() {
            [item] => Some(
                item.output_path
                    .clone()
                    .unwrap_or_else(|| item.path.clone()),
            ),
            _ => None,
        };
        // A rescan is only needed to pick up filesystem changes an item's outcome may
        // have caused (a new/renamed output file, or a source that got removed out
        // from under a failed edit) — a purely cancelled batch touches nothing on
        // disk, so skip it and go straight back to the Streams view, matching the
        // single-file flow this replaces.
        let needs_rescan = batch.items.iter().any(|item| {
            matches!(
                item.status,
                BatchItemStatus::Completed | BatchItemStatus::Failed(_)
            )
        });
        self.active_batch = None;
        self.dialog = None;
        if needs_rescan && let Ok(files) = scan_directory(&self.directory) {
            self.reconcile_files(files);
        }
        if let Some(output_path) = &reselect {
            if self.files.iter().any(|file| file.path == *output_path)
                && self.file_panel_position(output_path).is_none()
            {
                self.file_search.clear();
                self.file_search_origin = None;
            }
            if let Some(position) = self.file_panel_position(output_path) {
                self.list_state.select(Some(position));
                self.sidecars = self
                    .selected_file()
                    .and_then(|file| self.sidecars_by_media.get(&file.path))
                    .cloned()
                    .unwrap_or_default();
                self.queue_probe();
            }
        }
        // Set last, after `reconcile_files` — which may have set its own notice for
        // an unrelated reason (e.g. the source path momentarily "disappearing" mid
        // rename) — so the batch's own outcome always wins as the final word.
        self.notice = Some(notice);
        if !needs_rescan
            || expected_selection
                .is_some_and(|path| self.selected_file().is_some_and(|file| file.path == path))
        {
            self.layer = Layer::Streams;
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
        let active_left = self.active_left_subtitle_tracks();
        rows.extend(active_left.iter().cloned());
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
        // Attachments, data and any other stream kind are deliberately absent: the
        // overview lists the container, video, audio and subtitles, and a row that is
        // selectable but never drawn would put the cursor somewhere invisible.
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
        let file = self.selected_file()?;
        let format_name = self
            .media_info()
            .and_then(|info| info.format.get("format_name"))
            .and_then(serde_json::Value::as_str);
        ContainerFormat::detect(&file.path, format_name)
    }

    pub fn effective_container(&self) -> Option<ContainerFormat> {
        self.container_target.or_else(|| self.source_container())
    }

    pub fn original_container_label(&self) -> String {
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
        container_conflicts_for_plan(
            &self.subtitle_capabilities,
            self.media_info(),
            &self.stream_order,
            &self.deleted_streams,
            &self.audio_settings,
            &self.video_settings,
            &self.subtitle_changes,
            &self.sidecars,
            target,
        )
    }

    pub fn selected_container_conflicts(&self) -> Vec<String> {
        staged_container_conflicts(
            &self.subtitle_capabilities,
            self.media_info(),
            self.source_container(),
            self.container_target,
            &self.stream_order,
            &self.deleted_streams,
            &self.audio_settings,
            &self.video_settings,
            &self.subtitle_changes,
            &self.sidecars,
        )
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
            &self.audio_settings,
            &self.video_settings,
            &subtitle_changes,
            target,
        )
    }

    /// Whether a file has staged edits and, if so, whether they'd currently pass the
    /// same gates `request_save` runs before allowing a save — used to drive the
    /// file-list yellow/warning-triangle markers and to block Ctrl+S's "process all
    /// staged files" until every staged file is `Valid`.
    pub fn staged_file_status(&self, path: &Path) -> StagedFileStatus {
        let is_open = self.selected_file().is_some_and(|file| file.path == path);
        // A `staged_edits` entry can exist (and be `stale`/conflicted) for the
        // currently-open file too, now that `reconcile_files` snapshots live edits
        // into it the moment the open file's on-disk content changes — see
        // `App::reconcile_files`. That has to be checked *before* branching on
        // `is_open` below, or an unresolved conflict on the open file would fall
        // straight through to live-field validation and wrongly report `Valid`,
        // letting `request_process_all` process content that no longer matches what
        // was staged. A confirmed conflict means a background re-probe already
        // established, using fresh content, that the tracks these fields edit are no
        // longer the tracks in the file — re-running validation here against the cache
        // entry at `staged.fingerprint` would be wrong, since that entry is
        // necessarily the *old* (pre-change) content, not current disk state. `stale`
        // with no `conflict_groups` means that check is still queued or in flight.
        if let Some(staged) = self.staged_edits.get(path) {
            if !staged.conflict_groups.is_empty() {
                return StagedFileStatus::Invalid(format!(
                    "The file's {} changed on disk — the staged changes for them have to be \
                     reverted before this can be processed.",
                    describe_track_groups(&staged.conflict_groups)
                ));
            }
            if staged.stale {
                return StagedFileStatus::Invalid(
                    "File changed on disk; still checking whether the staged edit is affected…"
                        .to_string(),
                );
            }
        }

        // The currently-open file's live edit fields count as "staged" here even
        // before `snapshot_current_edits` has captured them into `staged_edits` on a
        // selection change, so the file list reflects in-progress edits immediately.
        if is_open {
            if !self.has_track_edits() {
                return StagedFileStatus::Unstaged;
            }
            return self.validate_live_edits(path);
        }

        let Some(staged) = self.staged_edits.get(path) else {
            return StagedFileStatus::Unstaged;
        };
        let key = CacheKey {
            path: path.to_path_buf(),
            length: staged.fingerprint.length,
            modified: staged.fingerprint.modified,
        };
        let Some(ProbeOutcome::Video(info)) = self.cache.get(&key) else {
            return StagedFileStatus::Invalid(
                "This file hasn't been probed yet; open it once before processing.".to_string(),
            );
        };
        let sidecars = self
            .sidecars_by_media
            .get(path)
            .cloned()
            .unwrap_or_default();
        match validate_staged_edit(
            &self.subtitle_capabilities,
            path,
            info,
            &sidecars,
            &staged.stream_order,
            &staged.deleted_streams,
            &staged.default_streams,
            &staged.original_stream_order,
            &staged.original_default_streams,
            &staged.track_groups,
            &staged.moved_streams,
            &staged.audio_settings,
            &staged.video_settings,
            &staged.subtitle_changes,
            staged.container_target,
        ) {
            Ok(_) => StagedFileStatus::Valid,
            Err(error) => StagedFileStatus::Invalid(error),
        }
    }

    /// The human-readable list of changes staged for `path`, shown per-file in the
    /// `ConfirmProcessAll` dialog. Only meaningful once `request_process_all` has run
    /// `snapshot_current_edits` (which it always does before opening that dialog), so
    /// even the currently-open file's live edits are already in `staged_edits` by the
    /// time this is called.
    pub fn staged_file_summary(&self, path: &Path) -> Vec<String> {
        let Some(staged) = self.staged_edits.get(path) else {
            return Vec::new();
        };
        let key = CacheKey {
            path: path.to_path_buf(),
            length: staged.fingerprint.length,
            modified: staged.fingerprint.modified,
        };
        let Some(ProbeOutcome::Video(info)) = self.cache.get(&key) else {
            return vec!["(changes staged)".to_string()];
        };
        staged_edit_summary(path, info, staged)
    }

    /// The `staged_file_status` gates run against the currently-open file's live edit
    /// fields (as opposed to a `StagedEdit` snapshot for a non-open staged file).
    fn validate_live_edits(&self, path: &Path) -> StagedFileStatus {
        let Some(info) = self.media_info() else {
            return StagedFileStatus::Invalid("Media metadata isn't loaded yet.".to_string());
        };
        match validate_staged_edit(
            &self.subtitle_capabilities,
            path,
            info,
            &self.sidecars,
            &self.stream_order,
            &self.deleted_streams,
            &self.default_streams,
            &self.original_stream_order,
            &self.original_default_streams,
            &self.track_groups,
            &self.moved_streams,
            &self.audio_settings,
            &self.video_settings,
            &self.subtitle_changes,
            self.container_target,
        ) {
            Ok(_) => StagedFileStatus::Valid,
            Err(error) => StagedFileStatus::Invalid(error),
        }
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

    pub fn original_container_metadata(&self) -> Option<ContainerMetadata> {
        let info = self.media_info()?;
        Some(ContainerMetadata {
            title: info.container_title(),
            comment: info.container_comment(),
            date: info.container_date(),
            genre: info.container_genre(),
            artist: info.container_artist(),
        })
    }

    pub fn effective_container_metadata(&self) -> Option<ContainerMetadata> {
        self.container_metadata
            .clone()
            .or_else(|| self.original_container_metadata())
    }

    pub fn container_field_changed(&self, field: ContainerSettingsField) -> bool {
        let orig = self.original_container_metadata();
        let eff = self.effective_container_metadata();
        match (orig, eff) {
            (Some(orig), Some(eff)) => match field {
                ContainerSettingsField::Format => self.container_target.is_some(),
                ContainerSettingsField::Title => orig.title != eff.title,
                ContainerSettingsField::Comment => orig.comment != eff.comment,
                ContainerSettingsField::Date => orig.date != eff.date,
                ContainerSettingsField::Genre => orig.genre != eff.genre,
                ContainerSettingsField::Artist => orig.artist != eff.artist,
            },
            (None, Some(eff)) => match field {
                ContainerSettingsField::Format => self.container_target.is_some(),
                ContainerSettingsField::Title => eff.title.is_some(),
                ContainerSettingsField::Comment => eff.comment.is_some(),
                ContainerSettingsField::Date => eff.date.is_some(),
                ContainerSettingsField::Genre => eff.genre.is_some(),
                ContainerSettingsField::Artist => eff.artist.is_some(),
            },
            _ => field == ContainerSettingsField::Format && self.container_target.is_some(),
        }
    }

    pub fn container_metadata_changed(&self) -> bool {
        let orig = self.original_container_metadata();
        let eff = self.effective_container_metadata();
        match (orig, eff) {
            (Some(orig), Some(eff)) => orig != eff,
            (None, Some(eff)) => !eff.is_empty(),
            (Some(_), None) => true,
            (None, None) => false,
        }
    }

    pub fn open_container_settings(&mut self) {
        if self.layer != Layer::Streams
            || self.dialog.is_some()
            || self.selected_track() != Some(TrackRef::Container)
        {
            return;
        }
        let choices = self.container_choices();
        let format_cursor = choices.iter().position(|choice| choice.staged).unwrap_or(0);
        self.container_settings_popup = Some(ContainerSettingsPopup {
            field: ContainerSettingsField::Format,
            mode: ContainerSettingsMode::Summary,
            help_visible: false,
            format_cursor,
            text_input: TextInputState::default(),
        });
        self.notice = None;
        self.dialog = Some(Dialog::ContainerSettings);
    }

    pub fn toggle_container_help(&mut self) {
        if let Some(popup) = self.container_settings_popup.as_mut() {
            popup.help_visible = !popup.help_visible;
        }
    }

    pub fn move_container_settings_cursor(&mut self, direction: isize) {
        let choices_len = self.container_choices().len();
        let Some(popup) = self.container_settings_popup.as_mut() else {
            return;
        };
        match popup.mode {
            ContainerSettingsMode::FormatDropdown => {
                popup.format_cursor =
                    move_cursor(popup.format_cursor, choices_len, direction, |_| true);
            }
            ContainerSettingsMode::Summary => {
                let fields = ContainerSettingsField::ALL;
                let current_index = fields.iter().position(|f| *f == popup.field).unwrap_or(0);
                let new_index = move_cursor(current_index, fields.len(), direction, |_| true);
                popup.field = fields[new_index];
            }
            ContainerSettingsMode::TextEdit => {}
        }
    }

    pub fn move_container_settings_to_endpoint(&mut self, end: bool) {
        let choices_len = self.container_choices().len();
        let Some(popup) = self.container_settings_popup.as_mut() else {
            return;
        };
        match popup.mode {
            ContainerSettingsMode::FormatDropdown => {
                if let Some(position) = cursor_endpoint(choices_len, end, |_| true) {
                    popup.format_cursor = position;
                }
            }
            ContainerSettingsMode::Summary => {
                let fields = ContainerSettingsField::ALL;
                if let Some(position) = cursor_endpoint(fields.len(), end, |_| true) {
                    popup.field = fields[position];
                }
            }
            ContainerSettingsMode::TextEdit => {}
        }
    }

    pub fn activate_container_settings(&mut self) {
        let mode = self
            .container_settings_popup
            .as_ref()
            .map(|popup| popup.mode);
        let Some(mode) = mode else {
            return;
        };
        match mode {
            ContainerSettingsMode::Summary => {
                // Only the Format field opens on Enter. Metadata fields are entered
                // with `i`, like every other text field in the application.
                let field = self.container_settings_popup.as_ref().unwrap().field;
                if field == ContainerSettingsField::Format
                    && let Some(popup) = self.container_settings_popup.as_mut()
                {
                    popup.mode = ContainerSettingsMode::FormatDropdown;
                }
            }
            ContainerSettingsMode::FormatDropdown => {
                let cursor = self
                    .container_settings_popup
                    .as_ref()
                    .unwrap()
                    .format_cursor;
                if let Some(choice) = self.container_choices().get(cursor).cloned() {
                    self.container_target = choice.value;
                    self.notice = choice.warning();
                }
                if let Some(popup) = self.container_settings_popup.as_mut() {
                    popup.mode = ContainerSettingsMode::Summary;
                }
            }
            ContainerSettingsMode::TextEdit => {
                self.save_container_text_input();
            }
        }
    }

    /// Opens the selected metadata field for editing, seeded with its current value.
    /// Mirrors [`Self::start_subtitle_title_input`].
    pub fn start_container_text_input(&mut self) {
        self.clear_text_input_reject();
        let Some(field) = self
            .container_settings_popup
            .as_ref()
            .filter(|popup| popup.mode == ContainerSettingsMode::Summary)
            .map(|popup| popup.field)
            .filter(|field| *field != ContainerSettingsField::Format)
        else {
            return;
        };
        let metadata = self.effective_container_metadata().unwrap_or_default();
        let current_text = match field {
            ContainerSettingsField::Title => metadata.title.as_deref(),
            ContainerSettingsField::Comment => metadata.comment.as_deref(),
            ContainerSettingsField::Date => metadata.date.as_deref(),
            ContainerSettingsField::Genre => metadata.genre.as_deref(),
            ContainerSettingsField::Artist => metadata.artist.as_deref(),
            ContainerSettingsField::Format => None,
        }
        .unwrap_or("")
        .to_string();
        if let Some(popup) = self.container_settings_popup.as_mut() {
            popup.text_input = TextInputState::new(current_text);
            popup.text_input.activate();
            popup
                .text_input
                .move_home(true, TextInputConfig::CONTAINER_METADATA);
            popup.mode = ContainerSettingsMode::TextEdit;
        }
    }

    pub fn save_container_text_input(&mut self) {
        let Some(popup) = self.container_settings_popup.as_mut() else {
            return;
        };
        if popup.mode != ContainerSettingsMode::TextEdit {
            return;
        }
        let field = popup.field;
        let value = popup.text_input.value.trim().to_string();
        let val_opt = if value.is_empty() { None } else { Some(value) };
        let mut metadata = self.effective_container_metadata().unwrap_or_default();
        match field {
            ContainerSettingsField::Title => metadata.title = val_opt,
            ContainerSettingsField::Comment => metadata.comment = val_opt,
            ContainerSettingsField::Date => metadata.date = val_opt,
            ContainerSettingsField::Genre => metadata.genre = val_opt,
            ContainerSettingsField::Artist => metadata.artist = val_opt,
            ContainerSettingsField::Format => {}
        }
        let orig = self.original_container_metadata().unwrap_or_default();
        if metadata == orig {
            self.container_metadata = None;
        } else {
            self.container_metadata = Some(metadata);
        }
        if let Some(popup) = self.container_settings_popup.as_mut() {
            // The renderer takes "is being edited" from `is_active`, so leaving it set
            // here would keep drawing a caret on a field that is no longer in edit mode.
            popup.text_input.deactivate();
            popup.mode = ContainerSettingsMode::Summary;
        }
    }

    pub fn escape_container_settings(&mut self) {
        let mode = self
            .container_settings_popup
            .as_ref()
            .map(|popup| popup.mode);
        let Some(mode) = mode else {
            return;
        };
        match mode {
            ContainerSettingsMode::TextEdit => {
                self.save_container_text_input();
            }
            ContainerSettingsMode::FormatDropdown => {
                if let Some(popup) = self.container_settings_popup.as_mut() {
                    popup.mode = ContainerSettingsMode::Summary;
                }
            }
            ContainerSettingsMode::Summary => {
                self.close_container_settings();
            }
        }
    }

    pub fn close_container_settings(&mut self) {
        self.container_settings_popup = None;
        self.dialog = None;
    }

    /// Ctrl+S from inside the Container Settings dialog — commits a field being
    /// text-edited, closes the popup, then hands off to the same unified
    /// `request_process_all` entry point Ctrl+S uses everywhere else. Mirrors
    /// `save_from_video_settings`/`save_from_subtitle_settings`.
    pub fn save_from_container_settings(&mut self) {
        if self
            .container_settings_popup
            .as_ref()
            .is_some_and(|popup| popup.mode == ContainerSettingsMode::TextEdit)
        {
            self.save_container_text_input();
        }
        self.close_container_settings();
        self.request_process_all();
    }

    /// Which text field the generic editing keys currently drive. The guards mirror
    /// `handle_key`'s routing exactly, so a field belonging to a dismissed layer can
    /// never be reached by a keystroke meant for whatever is on top.
    pub fn active_text_input(&self) -> Option<TextInputSite> {
        if self
            .container_settings_popup
            .as_ref()
            .is_some_and(|popup| popup.mode == ContainerSettingsMode::TextEdit)
        {
            return Some(TextInputSite::ContainerMetadata);
        }
        if self
            .audio_settings_popup
            .as_ref()
            .is_some_and(|popup| popup.mode == AudioSettingsMode::TitleEdit)
        {
            return Some(TextInputSite::AudioTitle);
        }
        if self.audio_settings_popup.as_ref().is_some_and(|popup| {
            popup.mode == AudioSettingsMode::LanguageDropdown && popup.language_search.is_active
        }) {
            return Some(TextInputSite::AudioLanguageSearch);
        }
        if self
            .video_settings_popup
            .as_ref()
            .is_some_and(|popup| popup.mode == VideoSettingsMode::TitleEdit)
        {
            return Some(TextInputSite::VideoTitle);
        }
        if self.video_settings_popup.as_ref().is_some_and(|popup| {
            popup.mode == VideoSettingsMode::LanguageDropdown && popup.language_search.is_active
        }) {
            return Some(TextInputSite::VideoLanguageSearch);
        }
        if self
            .subtitle_settings_popup
            .as_ref()
            .is_some_and(|popup| popup.mode == SubtitleSettingsMode::TitleEdit)
        {
            return Some(TextInputSite::SubtitleTitle);
        }
        if self.subtitle_settings_popup.as_ref().is_some_and(|popup| {
            popup.mode == SubtitleSettingsMode::LanguageDropdown && popup.language_search.is_active
        }) {
            return Some(TextInputSite::LanguageSearch);
        }
        if self.custom_resolution_input_active() {
            return Some(TextInputSite::CustomResolution);
        }
        if self.dialog == Some(Dialog::Keybindings) && self.keybindings_search.is_active {
            return Some(TextInputSite::KeybindingsSearch);
        }
        if self.dialog.is_none() && self.layer == Layer::Files && self.file_search.is_active {
            return Some(TextInputSite::FileSearch);
        }
        None
    }

    /// The field's buffer plus the config that governs it. The config comes back by
    /// value (it is `Copy`) so reading it does not conflict with the `&mut` borrow.
    fn text_input_mut(
        &mut self,
        site: TextInputSite,
    ) -> Option<(&mut TextInputState, TextInputConfig)> {
        match site {
            TextInputSite::ContainerMetadata => Some((
                &mut self.container_settings_popup.as_mut()?.text_input,
                TextInputConfig::CONTAINER_METADATA,
            )),
            TextInputSite::AudioTitle => Some((
                &mut self.audio_settings_popup.as_mut()?.title_input,
                TextInputConfig::SUBTITLE_TITLE,
            )),
            TextInputSite::AudioLanguageSearch => Some((
                &mut self.audio_settings_popup.as_mut()?.language_search.input,
                TextInputConfig::LANGUAGE_SEARCH,
            )),
            TextInputSite::VideoTitle => Some((
                &mut self.video_settings_popup.as_mut()?.title_input,
                TextInputConfig::SUBTITLE_TITLE,
            )),
            TextInputSite::VideoLanguageSearch => Some((
                &mut self.video_settings_popup.as_mut()?.language_search.input,
                TextInputConfig::LANGUAGE_SEARCH,
            )),
            TextInputSite::SubtitleTitle => Some((
                &mut self.subtitle_settings_popup.as_mut()?.title_input,
                TextInputConfig::SUBTITLE_TITLE,
            )),
            TextInputSite::LanguageSearch => Some((
                &mut self.subtitle_settings_popup.as_mut()?.language_search.input,
                TextInputConfig::LANGUAGE_SEARCH,
            )),
            TextInputSite::CustomResolution => {
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
                Some((input, TextInputConfig::RESOLUTION))
            }
            TextInputSite::FileSearch => {
                let config = self.file_search.config();
                Some((&mut self.file_search.input, config))
            }
            TextInputSite::KeybindingsSearch => {
                let config = self.keybindings_search.config();
                Some((&mut self.keybindings_search.input, config))
            }
        }
    }

    /// Runs one edit against whichever field is active, then the follow-up that field
    /// needs. Every text-editing key in the application goes through here.
    fn edit_active_text(
        &mut self,
        edit: impl FnOnce(&mut TextInputState, TextInputConfig) -> InputEdit,
    ) {
        let Some(site) = self.active_text_input() else {
            return;
        };
        // The file panel's selection has to be sampled before the query changes, since
        // filtering the list is what moves it.
        let selected_path = (site == TextInputSite::FileSearch)
            .then(|| self.selected_file().map(|file| file.path.clone()))
            .flatten();
        let Some((input, config)) = self.text_input_mut(site) else {
            return;
        };
        let outcome = edit(input, config);
        self.after_text_edit(site, outcome, selected_path);
    }

    /// Site-specific bookkeeping that has to follow a successful edit: resetting the
    /// cursor into a filtered result list, rescrolling, or leaving the field entirely.
    fn after_text_edit(
        &mut self,
        site: TextInputSite,
        outcome: InputEdit,
        selected_path: Option<PathBuf>,
    ) {
        match outcome {
            // A rejection still runs the follow-up below: a paste can be truncated and
            // have changed the value all the same.
            InputEdit::Rejected(reject) => self.text_input_reject = Some((site, reject)),
            InputEdit::Changed | InputEdit::Exited => self.text_input_reject = None,
            InputEdit::Unchanged => return,
        }
        match site {
            TextInputSite::AudioLanguageSearch => {
                if let Some(popup) = self.audio_settings_popup.as_mut() {
                    popup.language_cursor = 0;
                }
            }
            TextInputSite::VideoLanguageSearch => {
                if let Some(popup) = self.video_settings_popup.as_mut() {
                    popup.language_cursor = 0;
                }
            }
            TextInputSite::LanguageSearch => {
                if let Some(popup) = self.subtitle_settings_popup.as_mut() {
                    popup.language_cursor = 0;
                }
            }
            TextInputSite::KeybindingsSearch => self.keybindings_scroll = 0,
            TextInputSite::FileSearch => {
                if outcome == InputEdit::Exited {
                    self.file_search_origin = None;
                }
                self.reselect_file_after_view_change(selected_path.clone(), selected_path);
            }
            TextInputSite::ContainerMetadata
            | TextInputSite::AudioTitle
            | TextInputSite::VideoTitle
            | TextInputSite::SubtitleTitle
            | TextInputSite::CustomResolution => {}
        }
    }

    /// Why the last keystroke aimed at this field did not land, if it did not. Reported
    /// only while the field is still the active one, so a stale explanation cannot
    /// surface on a field the user has since moved away from.
    pub fn text_input_reject(&self, site: TextInputSite) -> Option<InputReject> {
        let (rejected_site, reject) = self.text_input_reject?;
        (rejected_site == site && self.active_text_input() == Some(site)).then_some(reject)
    }

    /// Drops any pending rejection explanation. Called wherever a text field is opened
    /// or closed, so entering a field never shows the reason a previous visit stopped.
    pub fn clear_text_input_reject(&mut self) {
        self.text_input_reject = None;
    }

    pub fn input_text_char(&mut self, character: char) {
        self.edit_active_text(|input, config| input.insert(character, config));
    }

    /// Inserts a bracketed paste into whichever field is active.
    pub fn paste_text(&mut self, text: &str) {
        self.edit_active_text(|input, config| input.insert_str(text, config));
    }

    pub fn delete_word_before_cursor(&mut self) {
        self.edit_active_text(|input, config| input.delete_word_before(config));
    }

    pub fn clear_text_to_start(&mut self) {
        self.edit_active_text(|input, config| input.clear_to_start(config));
    }

    pub fn backspace_text(&mut self) {
        self.edit_active_text(|input, config| input.backspace(config));
    }

    pub fn delete_text(&mut self) {
        self.edit_active_text(|input, config| input.delete(config));
    }

    pub fn move_text_cursor(&mut self, direction: isize) {
        self.edit_active_text(|input, config| input.move_cursor(direction, config));
    }

    pub fn move_text_home(&mut self, end: bool) {
        self.edit_active_text(|input, config| input.move_home(end, config));
    }

    fn original_audio_settings(&self, index: u64) -> Option<AudioSettings> {
        let stream = self
            .media_info()
            .and_then(|info| stream_by_index(info, index))?;
        (stream_kind(stream) == Some("audio")).then(|| AudioSettings {
            codec: AudioCodec::Original,
            channel_layout: AudioChannelLayout::Original,
            metadata: AudioMetadata {
                language: stream_language(stream),
                title: audio_stream_title(stream),
                commentary: stream_commentary(stream),
                hearing_impaired: stream_hearing_impaired(stream),
                audio_description: stream_disposition(stream, "visual_impaired"),
                original: stream_original(stream),
                dubbed: stream_disposition(stream, "dub"),
            },
        })
    }

    /// The staged audio settings for `index`, or the track's own settings when nothing is
    /// staged. Deliberately *not* filtered against the target container: this is the
    /// user's intent, and it has to survive them changing the container target again.
    ///
    /// Filtering here instead used to leak the filtered value straight back into the
    /// staged edit — `store_audio_settings` compared it against the unfiltered source, so
    /// they could never match, and merely opening and leaving a field staged an edit
    /// nobody made with (say) MP4's unsupported Original role cleared. Switching back to
    /// MKV then wrote that cleared flag out for real. The container filter belongs where
    /// the file is actually written (`retain_supported_audio_metadata` in edit.rs) and
    /// where fields are offered (`audio_field_visible` hides what the container can't
    /// store), not in the staged model.
    pub fn effective_audio_settings(&self, index: u64) -> Option<AudioSettings> {
        self.audio_settings
            .get(&index)
            .cloned()
            .or_else(|| self.original_audio_settings(index))
    }

    fn store_audio_settings(&mut self, index: u64, mut settings: AudioSettings) {
        settings.metadata.language = canonical_language_code(&settings.metadata.language)
            .unwrap_or_else(|| "und".to_string());
        if self.original_audio_settings(index).as_ref() == Some(&settings) {
            self.audio_settings.remove(&index);
        } else {
            self.audio_settings.insert(index, settings);
        }
    }

    fn original_video_settings(&self, index: u64) -> Option<VideoSettings> {
        let stream = self
            .media_info()
            .and_then(|info| stream_by_index(info, index))?;
        (stream_kind(stream) == Some("video")).then(|| VideoSettings {
            codec: VideoCodec::Original,
            resolution: VideoResolution::Original,
            rotation: stream_rotation(stream),
            metadata: VideoMetadata {
                language: stream_language(stream),
                title: video_stream_title(stream),
                commentary: stream_commentary(stream),
            },
        })
    }

    /// The staged video settings for `index`, or the track's own settings when nothing is
    /// staged. See `effective_audio_settings` for why this is deliberately not filtered
    /// against the target container.
    pub fn effective_video_settings(&self, index: u64) -> Option<VideoSettings> {
        self.video_settings
            .get(&index)
            .cloned()
            .or_else(|| self.original_video_settings(index))
    }

    fn store_video_settings(&mut self, index: u64, mut settings: VideoSettings) {
        settings.metadata.language = canonical_language_code(&settings.metadata.language)
            .unwrap_or_else(|| "und".to_string());
        if self.original_video_settings(index).as_ref() == Some(&settings) {
            self.video_settings.remove(&index);
        } else {
            self.video_settings.insert(index, settings);
        }
    }

    pub fn open_audio_settings(&mut self) {
        if self.layer != Layer::Streams || self.dialog.is_some() {
            return;
        }
        let Some(index) = self.selected_stream_index() else {
            return;
        };
        if self.deleted_streams.contains(&index) {
            self.notice =
                Some("Unmark this track for deletion before changing its audio settings.".into());
            return;
        }
        if !self
            .selected_stream_info()
            .is_some_and(|stream| stream_kind(stream) == Some("audio"))
        {
            self.notice = Some("Audio settings are only available for audio tracks.".into());
            return;
        }
        let settings = self
            .effective_audio_settings(index)
            .expect("the selected stream was just verified as audio");
        let codecs = self.audio_codec_choices(index);
        let channels = self.audio_channel_choices(index);
        let languages = self.audio_language_choices_for(index, "");
        let language_cursor = languages
            .iter()
            .position(|choice| choice.code == settings.metadata.language)
            .unwrap_or(0);
        self.audio_settings_popup = Some(AudioSettingsPopup {
            stream_index: index,
            field: AudioSettingsField::Codec,
            mode: AudioSettingsMode::Summary,
            help_visible: false,
            codec_cursor: codecs
                .iter()
                .position(|choice| choice.value == settings.codec)
                .unwrap_or(0),
            channel_cursor: channels
                .iter()
                .position(|choice| choice.value == settings.channel_layout)
                .unwrap_or(0),
            language_cursor,
            language_search: SearchState::default(),
            title_input: TextInputState::new(settings.metadata.title.unwrap_or_default()),
        });
        self.notice = None;
        self.dialog = Some(Dialog::AudioSettings);
    }

    pub fn toggle_audio_field_help(&mut self) {
        if let Some(popup) = self.audio_settings_popup.as_mut() {
            popup.help_visible = !popup.help_visible;
        }
    }

    pub fn audio_codec_choices(&self, index: u64) -> Vec<AudioChoice<AudioCodec>> {
        let source_name = self
            .media_info()
            .and_then(|info| stream_by_index(info, index))
            .and_then(|stream| stream.get("codec_name"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let source_codec = AudioCodec::from_codec_name(source_name);
        let container = self.effective_container();
        let mut choices = Vec::with_capacity(AudioCodec::TARGETS.len() + 1);
        if source_codec.is_none() {
            let rejected = container
                .filter(|container| !container.supports_codec("audio", source_name, false));
            choices.push(AudioChoice {
                value: AudioCodec::Original,
                label: source_name.to_ascii_uppercase(),
                current: true,
                enabled: rejected.is_none(),
                reason: rejected.map(|container| {
                    format!(
                        "{} cannot contain {} audio",
                        container.label(),
                        source_name.to_ascii_uppercase()
                    )
                }),
            });
        }
        choices.extend(AudioCodec::TARGETS.into_iter().map(|codec| {
            let current = source_codec == Some(codec);
            let encoder_missing = codec.encoder().is_some_and(|encoder| {
                !self.subtitle_capabilities.ffmpeg_encoders.contains(encoder)
            });
            let rejected = container.filter(|container| {
                !container.supports_codec("audio", codec.codec_name().unwrap(), false)
            });
            let reason = if encoder_missing && !current {
                Some("FFmpeg encoder is unavailable".to_string())
            } else {
                rejected.map(|container| {
                    format!(
                        "{} cannot contain {} audio",
                        container.label(),
                        codec.label()
                    )
                })
            };
            AudioChoice {
                value: if current { AudioCodec::Original } else { codec },
                label: codec.label().to_string(),
                current,
                enabled: reason.is_none(),
                reason,
            }
        }));
        choices
    }

    pub fn effective_audio_codec_for(&self, index: u64) -> Option<AudioCodec> {
        let stream = self
            .media_info()
            .and_then(|info| stream_by_index(info, index))?;
        let settings = self.effective_audio_settings(index)?;
        effective_audio_codec(stream, &settings)
    }

    pub fn audio_channel_choices(&self, index: u64) -> Vec<AudioChoice<AudioChannelLayout>> {
        let source_channels = self
            .media_info()
            .and_then(|info| stream_by_index(info, index))
            .and_then(stream_channels);
        let codec = self.effective_audio_codec_for(index);
        let mut choices = Vec::new();
        if !matches!(source_channels, Some(1 | 2 | 6 | 8)) {
            choices.push(AudioChoice {
                value: AudioChannelLayout::Original,
                label: source_channels
                    .map(|channels| format!("{channels} channels"))
                    .unwrap_or_else(|| "Original".to_string()),
                current: true,
                enabled: source_channels.is_some(),
                reason: source_channels
                    .is_none()
                    .then(|| "Source layout is unavailable".into()),
            });
        }
        choices.extend(AudioChannelLayout::TARGETS.into_iter().map(|layout| {
            let channels = layout.channels().unwrap();
            let current = source_channels == Some(channels);
            let reason = if source_channels.is_none() {
                Some("Source layout is unavailable".to_string())
            } else if source_channels.is_some_and(|source| channels > source) {
                Some(CHANNEL_UPMIX_NOT_IMPLEMENTED.to_string())
            } else {
                codec
                    .filter(|codec| !codec.supports_channels(channels))
                    .map(|codec| format!("{} does not support this layout", codec.label()))
            };
            AudioChoice {
                value: if current {
                    AudioChannelLayout::Original
                } else {
                    layout
                },
                label: layout.label().to_string(),
                current,
                enabled: reason.is_none(),
                reason,
            }
        }));
        choices
    }

    fn audio_language_choices_for(&self, index: u64, query: &str) -> Vec<LanguageChoice> {
        let mut choices = common_language_choices();
        choices.insert(
            0,
            LanguageChoice {
                code: "und".to_string(),
                two_letter: String::new(),
                name: "Undetermined".to_string(),
            },
        );
        if let Some(current) = self
            .effective_audio_settings(index)
            .and_then(|settings| language_choice(&settings.metadata.language))
            && !choices.iter().any(|choice| choice.code == current.code)
        {
            choices.push(current);
        }
        choices.retain(|choice| choice.matches(query));
        choices
    }

    pub fn filtered_audio_languages(&self) -> Vec<LanguageChoice> {
        let Some(popup) = self.audio_settings_popup.as_ref() else {
            return Vec::new();
        };
        self.audio_language_choices_for(popup.stream_index, &popup.language_search.value)
    }

    pub fn audio_field_visible(&self, field: AudioSettingsField) -> bool {
        let Some(container) = self.effective_container() else {
            return true;
        };
        match field {
            AudioSettingsField::Language => container.supports_stream_language(),
            field if field.role().is_some() => field
                .role()
                .is_some_and(|role| container.supports_audio_role(role)),
            _ => true,
        }
    }

    pub fn visible_audio_fields(&self) -> Vec<AudioSettingsField> {
        AudioSettingsField::ALL
            .into_iter()
            .filter(|field| self.audio_field_visible(*field))
            .collect()
    }

    pub fn filtered_video_languages(&self) -> Vec<LanguageChoice> {
        let Some(popup) = self.video_settings_popup.as_ref() else {
            return Vec::new();
        };
        self.video_language_choices_for(popup.stream_index, &popup.language_search.value)
    }

    fn video_language_choices_for(&self, index: u64, query: &str) -> Vec<LanguageChoice> {
        let mut choices = common_language_choices();
        choices.insert(
            0,
            LanguageChoice {
                code: "und".to_string(),
                two_letter: String::new(),
                name: "Undetermined".to_string(),
            },
        );
        if let Some(current) = self
            .effective_video_settings(index)
            .and_then(|settings| language_choice(&settings.metadata.language))
            && !choices.iter().any(|choice| choice.code == current.code)
        {
            choices.push(current);
        }
        choices.retain(|choice| choice.matches(query));
        choices
    }

    /// The same stream-language container restriction audio has, plus the one role video
    /// offers — MOV and WebM store no commentary flag, so the field is hidden rather than
    /// promising a flag the remux would drop.
    pub fn video_field_visible(&self, field: VideoSettingsField) -> bool {
        let Some(container) = self.effective_container() else {
            return true;
        };
        match field {
            VideoSettingsField::Language => container.supports_stream_language(),
            VideoSettingsField::Commentary => container.supports_commentary_flag(),
            VideoSettingsField::Rotation => container.supports_display_rotation(),
            _ => true,
        }
    }

    pub fn visible_video_fields(&self) -> Vec<VideoSettingsField> {
        VideoSettingsField::ALL
            .into_iter()
            .filter(|field| self.video_field_visible(*field))
            .collect()
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

        let settings = self
            .effective_video_settings(index)
            .expect("the selected stream was just verified as video");
        let codecs = self.video_codec_choices(index);
        let resolutions = self.resolution_choices(index);
        let languages = self.video_language_choices_for(index, "");
        let language_cursor = languages
            .iter()
            .position(|choice| choice.code == settings.metadata.language)
            .unwrap_or(0);
        self.video_settings_popup = Some(VideoSettingsPopup {
            stream_index: index,
            field: VideoSettingsField::Codec,
            mode: VideoSettingsMode::Summary,
            help_visible: false,
            codec_cursor: codecs
                .iter()
                .position(|choice| choice.value == settings.codec)
                .unwrap_or(0),
            resolution_cursor: resolutions
                .iter()
                .position(|choice| choice.selected(settings.resolution))
                .unwrap_or(0),
            rotation_cursor: VideoRotation::ALL
                .iter()
                .position(|rotation| *rotation == settings.rotation)
                .unwrap_or(0),
            language_cursor,
            language_search: SearchState::default(),
            title_input: TextInputState::new(settings.metadata.title.unwrap_or_default()),
            custom_resolution: None,
        });
        self.notice = None;
        self.dialog = Some(Dialog::VideoSettings);
    }

    pub fn toggle_video_field_help(&mut self) {
        if let Some(popup) = self.video_settings_popup.as_mut() {
            popup.help_visible = !popup.help_visible;
        }
    }

    pub fn open_track_settings(&mut self) {
        match self.selected_track() {
            Some(TrackRef::Container) => self.open_container_settings(),
            Some(TrackRef::Embedded(_))
                if self
                    .selected_stream_info()
                    .is_some_and(|stream| stream_kind(stream) == Some("audio")) =>
            {
                self.open_audio_settings();
            }
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
        original_subtitle_metadata_for(self.media_info(), &self.sidecars, source)
    }

    pub fn subtitle_metadata_for(&self, source: &SubtitleSource) -> Option<SubtitleMetadata> {
        subtitle_metadata_for_plan(
            &self.subtitle_changes,
            self.media_info(),
            &self.sidecars,
            source,
        )
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
        self.clear_text_input_reject();
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
        self.clear_text_input_reject();
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
        self.request_process_all();
    }

    pub fn move_audio_settings_cursor(&mut self, direction: isize) {
        let Some(popup) = self.audio_settings_popup.as_ref() else {
            return;
        };
        match popup.mode {
            AudioSettingsMode::Summary => {
                let fields = self.visible_audio_fields();
                let position = fields
                    .iter()
                    .position(|field| *field == popup.field)
                    .unwrap_or(0);
                let next = move_cursor(position, fields.len(), direction, |_| true);
                self.audio_settings_popup.as_mut().unwrap().field = fields[next];
            }
            AudioSettingsMode::Dropdown => {
                let index = popup.stream_index;
                let field = popup.field;
                match field {
                    AudioSettingsField::Codec => {
                        let choices = self.audio_codec_choices(index);
                        let popup = self.audio_settings_popup.as_mut().unwrap();
                        popup.codec_cursor =
                            move_cursor(popup.codec_cursor, choices.len(), direction, |position| {
                                choices[position].enabled
                            });
                    }
                    AudioSettingsField::ChannelLayout => {
                        let choices = self.audio_channel_choices(index);
                        let popup = self.audio_settings_popup.as_mut().unwrap();
                        popup.channel_cursor = move_cursor(
                            popup.channel_cursor,
                            choices.len(),
                            direction,
                            |position| choices[position].enabled,
                        );
                    }
                    _ => {}
                }
            }
            AudioSettingsMode::LanguageDropdown => {
                let choices = self.filtered_audio_languages();
                let popup = self.audio_settings_popup.as_mut().unwrap();
                popup.language_cursor =
                    move_cursor(popup.language_cursor, choices.len(), direction, |_| true);
            }
            AudioSettingsMode::TitleEdit => {}
        }
    }

    pub fn move_audio_settings_to_endpoint(&mut self, end: bool) {
        let Some(popup) = self.audio_settings_popup.as_ref() else {
            return;
        };
        match popup.mode {
            AudioSettingsMode::Summary => {
                let fields = self.visible_audio_fields();
                let field = if end {
                    *fields.last().expect("codec is always an audio field")
                } else {
                    *fields.first().expect("codec is always an audio field")
                };
                self.audio_settings_popup.as_mut().unwrap().field = field;
            }
            AudioSettingsMode::Dropdown => {
                let index = popup.stream_index;
                let field = popup.field;
                let endpoint = match field {
                    AudioSettingsField::Codec => {
                        let choices = self.audio_codec_choices(index);
                        cursor_endpoint(choices.len(), end, |position| choices[position].enabled)
                    }
                    AudioSettingsField::ChannelLayout => {
                        let choices = self.audio_channel_choices(index);
                        cursor_endpoint(choices.len(), end, |position| choices[position].enabled)
                    }
                    _ => None,
                };
                if let Some(endpoint) = endpoint {
                    let popup = self.audio_settings_popup.as_mut().unwrap();
                    if field == AudioSettingsField::Codec {
                        popup.codec_cursor = endpoint;
                    } else {
                        popup.channel_cursor = endpoint;
                    }
                }
            }
            AudioSettingsMode::LanguageDropdown => {
                let len = self.filtered_audio_languages().len();
                if len > 0 {
                    self.audio_settings_popup.as_mut().unwrap().language_cursor =
                        if end { len - 1 } else { 0 };
                }
            }
            AudioSettingsMode::TitleEdit => {}
        }
    }

    pub fn activate_audio_settings(&mut self) {
        let Some(popup) = self.audio_settings_popup.as_ref() else {
            return;
        };
        let index = popup.stream_index;
        let field = popup.field;
        let mode = popup.mode;
        if mode == AudioSettingsMode::Summary {
            match field {
                AudioSettingsField::Codec | AudioSettingsField::ChannelLayout => {
                    self.audio_settings_popup.as_mut().unwrap().mode = AudioSettingsMode::Dropdown;
                }
                AudioSettingsField::Language => {
                    let current = self
                        .effective_audio_settings(index)
                        .map(|settings| settings.metadata.language)
                        .unwrap_or_else(|| "und".to_string());
                    let choices = self.audio_language_choices_for(index, "");
                    let popup = self.audio_settings_popup.as_mut().unwrap();
                    popup.language_search.clear();
                    popup.language_cursor = choices
                        .iter()
                        .position(|choice| choice.code == current)
                        .unwrap_or(0);
                    popup.mode = AudioSettingsMode::LanguageDropdown;
                }
                AudioSettingsField::Title => self.start_audio_title_input(),
                AudioSettingsField::Default => self.toggle_audio_default(index),
                field => {
                    let role = field.role().expect("remaining audio fields are roles");
                    if let Some(mut settings) = self.effective_audio_settings(index) {
                        let enabled = settings.metadata.get_role(role);
                        settings.metadata.set_role(role, !enabled);
                        self.store_audio_settings(index, settings);
                    }
                }
            }
            return;
        }
        if mode == AudioSettingsMode::LanguageDropdown {
            let choices = self.filtered_audio_languages();
            let cursor = popup.language_cursor;
            if let Some(choice) = choices.get(cursor)
                && let Some(mut settings) = self.effective_audio_settings(index)
            {
                settings.metadata.language.clone_from(&choice.code);
                self.store_audio_settings(index, settings);
                let popup = self.audio_settings_popup.as_mut().unwrap();
                popup.language_search.clear();
                popup.mode = AudioSettingsMode::Summary;
            }
            return;
        }
        if mode != AudioSettingsMode::Dropdown {
            return;
        }
        let Some(mut settings) = self.effective_audio_settings(index) else {
            return;
        };
        match field {
            AudioSettingsField::Codec => {
                let cursor = popup.codec_cursor;
                let choices = self.audio_codec_choices(index);
                let Some(choice) = choices.get(cursor).filter(|choice| choice.enabled) else {
                    return;
                };
                settings.codec = choice.value;
            }
            AudioSettingsField::ChannelLayout => {
                let cursor = popup.channel_cursor;
                let choices = self.audio_channel_choices(index);
                let Some(choice) = choices.get(cursor).filter(|choice| choice.enabled) else {
                    return;
                };
                settings.channel_layout = choice.value;
            }
            _ => return,
        }
        self.store_audio_settings(index, settings);
        self.audio_settings_popup.as_mut().unwrap().mode = AudioSettingsMode::Summary;
    }

    fn toggle_audio_default(&mut self, index: u64) {
        if !self.default_streams.remove(&index) {
            let audio_indices = self
                .media_info()
                .into_iter()
                .flat_map(|info| info.streams.iter())
                .filter(|stream| stream_kind(stream) == Some("audio"))
                .filter_map(stream_index)
                .collect::<Vec<_>>();
            for audio_index in audio_indices {
                self.default_streams.remove(&audio_index);
            }
            self.default_streams.insert(index);
        }
    }

    pub fn start_audio_title_input(&mut self) {
        self.clear_text_input_reject();
        let Some(popup) = self.audio_settings_popup.as_mut() else {
            return;
        };
        if popup.mode != AudioSettingsMode::Summary || popup.field != AudioSettingsField::Title {
            return;
        }
        popup.title_input.activate();
        popup.mode = AudioSettingsMode::TitleEdit;
    }

    fn commit_audio_title(&mut self) {
        let Some(popup) = self.audio_settings_popup.as_ref() else {
            return;
        };
        let index = popup.stream_index;
        let title = popup.title_input.value.trim().to_string();
        if let Some(mut settings) = self.effective_audio_settings(index) {
            settings.metadata.title = (!title.is_empty()).then_some(title);
            self.store_audio_settings(index, settings);
        }
    }

    pub fn start_audio_language_search(&mut self) {
        self.clear_text_input_reject();
        let Some(popup) = self
            .audio_settings_popup
            .as_mut()
            .filter(|popup| popup.mode == AudioSettingsMode::LanguageDropdown)
        else {
            return;
        };
        popup.language_search.activate();
    }

    pub fn cancel_audio_language_search(&mut self) {
        let Some(popup) = self.audio_settings_popup.as_mut() else {
            return;
        };
        popup.language_search.clear();
        popup.language_cursor = 0;
    }

    pub fn escape_audio_settings(&mut self) {
        let Some(mode) = self.audio_settings_popup.as_ref().map(|popup| popup.mode) else {
            self.dialog = None;
            return;
        };
        match mode {
            AudioSettingsMode::TitleEdit => {
                self.commit_audio_title();
                let popup = self.audio_settings_popup.as_mut().unwrap();
                popup.title_input.deactivate();
                popup.mode = AudioSettingsMode::Summary;
            }
            AudioSettingsMode::Dropdown | AudioSettingsMode::LanguageDropdown => {
                let popup = self.audio_settings_popup.as_mut().unwrap();
                popup.language_search.clear();
                popup.mode = AudioSettingsMode::Summary;
            }
            AudioSettingsMode::Summary => {
                self.audio_settings_popup = None;
                self.dialog = None;
            }
        }
    }

    pub fn save_from_audio_settings(&mut self) {
        if self
            .audio_settings_popup
            .as_ref()
            .is_some_and(|popup| popup.mode == AudioSettingsMode::TitleEdit)
        {
            self.commit_audio_title();
        }
        self.audio_settings_popup = None;
        self.dialog = None;
        self.request_process_all();
    }

    pub fn audio_field_changed(&self, field: AudioSettingsField) -> bool {
        let Some(popup) = self.audio_settings_popup.as_ref() else {
            return false;
        };
        let Some(current) = self.effective_audio_settings(popup.stream_index) else {
            return false;
        };
        let Some(original) = self.original_audio_settings(popup.stream_index) else {
            return false;
        };
        match field {
            AudioSettingsField::Codec => current.codec != original.codec,
            AudioSettingsField::ChannelLayout => current.channel_layout != original.channel_layout,
            AudioSettingsField::Language => current.metadata.language != original.metadata.language,
            AudioSettingsField::Title => current.metadata.title != original.metadata.title,
            AudioSettingsField::Default => {
                self.default_streams.contains(&popup.stream_index)
                    != self.original_default_streams.contains(&popup.stream_index)
            }
            field => field.role().is_some_and(|role| {
                current.metadata.get_role(role) != original.metadata.get_role(role)
            }),
        }
    }

    pub fn move_video_settings_cursor(&mut self, direction: isize) {
        let Some(popup) = self.video_settings_popup.as_ref() else {
            return;
        };
        match popup.mode {
            VideoSettingsMode::Summary => {
                let fields = self.visible_video_fields();
                let position = fields
                    .iter()
                    .position(|field| *field == popup.field)
                    .unwrap_or(0);
                let next = move_cursor(position, fields.len(), direction, |_| true);
                self.video_settings_popup.as_mut().unwrap().field = fields[next];
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
                VideoSettingsField::Rotation => {
                    let popup = self.video_settings_popup.as_mut().unwrap();
                    popup.rotation_cursor = move_cursor(
                        popup.rotation_cursor,
                        VideoRotation::ALL.len(),
                        direction,
                        |_| true,
                    );
                }
                _ => {}
            },
            VideoSettingsMode::LanguageDropdown => {
                let choices = self.filtered_video_languages();
                let popup = self.video_settings_popup.as_mut().unwrap();
                popup.language_cursor =
                    move_cursor(popup.language_cursor, choices.len(), direction, |_| true);
            }
            VideoSettingsMode::TitleEdit => {}
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
                let fields = self.visible_video_fields();
                let field = if end {
                    *fields.last().expect("codec is always a video field")
                } else {
                    *fields.first().expect("codec is always a video field")
                };
                self.video_settings_popup.as_mut().unwrap().field = field;
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
                VideoSettingsField::Rotation => {
                    self.video_settings_popup.as_mut().unwrap().rotation_cursor =
                        if end { VideoRotation::ALL.len() - 1 } else { 0 };
                }
                _ => {}
            },
            VideoSettingsMode::LanguageDropdown => {
                let len = self.filtered_video_languages().len();
                if len > 0 {
                    self.video_settings_popup.as_mut().unwrap().language_cursor =
                        if end { len - 1 } else { 0 };
                }
            }
            VideoSettingsMode::TitleEdit => {}
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
        let index = popup.stream_index;
        let field = popup.field;
        let mode = popup.mode;
        if mode == VideoSettingsMode::Summary {
            match field {
                VideoSettingsField::Codec
                | VideoSettingsField::Resolution
                | VideoSettingsField::Rotation => {
                    self.video_settings_popup.as_mut().unwrap().mode = VideoSettingsMode::Dropdown;
                }
                VideoSettingsField::Language => {
                    let current = self
                        .effective_video_settings(index)
                        .map(|settings| settings.metadata.language)
                        .unwrap_or_else(|| "und".to_string());
                    let choices = self.video_language_choices_for(index, "");
                    let popup = self.video_settings_popup.as_mut().unwrap();
                    popup.language_search.clear();
                    popup.language_cursor = choices
                        .iter()
                        .position(|choice| choice.code == current)
                        .unwrap_or(0);
                    popup.mode = VideoSettingsMode::LanguageDropdown;
                }
                VideoSettingsField::Title => self.start_video_title_input(),
                VideoSettingsField::Default => self.toggle_video_default(index),
                VideoSettingsField::Commentary => {
                    if let Some(mut settings) = self.effective_video_settings(index) {
                        settings.metadata.commentary = !settings.metadata.commentary;
                        self.store_video_settings(index, settings);
                    }
                }
            }
            return;
        }
        if mode == VideoSettingsMode::LanguageDropdown {
            let choices = self.filtered_video_languages();
            let cursor = popup.language_cursor;
            if let Some(choice) = choices.get(cursor)
                && let Some(mut settings) = self.effective_video_settings(index)
            {
                settings.metadata.language.clone_from(&choice.code);
                self.store_video_settings(index, settings);
                let popup = self.video_settings_popup.as_mut().unwrap();
                popup.language_search.clear();
                popup.mode = VideoSettingsMode::Summary;
            }
            return;
        }
        if mode == VideoSettingsMode::CustomResolution {
            self.activate_custom_resolution();
            return;
        }
        if mode != VideoSettingsMode::Dropdown {
            return;
        }

        let codec_cursor = popup.codec_cursor;
        let resolution_cursor = popup.resolution_cursor;
        let rotation_cursor = popup.rotation_cursor;
        let Some(mut settings) = self.effective_video_settings(index) else {
            return;
        };
        match field {
            VideoSettingsField::Rotation => {
                let Some(rotation) = VideoRotation::ALL.get(rotation_cursor) else {
                    return;
                };
                settings.rotation = *rotation;
            }
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
            _ => return,
        }
        self.store_video_settings(index, settings);
        self.video_settings_popup.as_mut().unwrap().mode = VideoSettingsMode::Summary;
    }

    fn toggle_video_default(&mut self, index: u64) {
        if !self.default_streams.remove(&index) {
            let video_indices = self
                .media_info()
                .into_iter()
                .flat_map(|info| info.streams.iter())
                .filter(|stream| stream_kind(stream) == Some("video"))
                .filter_map(stream_index)
                .collect::<Vec<_>>();
            for video_index in video_indices {
                self.default_streams.remove(&video_index);
            }
            self.default_streams.insert(index);
        }
    }

    pub fn start_video_title_input(&mut self) {
        self.clear_text_input_reject();
        let Some(popup) = self.video_settings_popup.as_mut() else {
            return;
        };
        if popup.mode != VideoSettingsMode::Summary || popup.field != VideoSettingsField::Title {
            return;
        }
        popup.title_input.activate();
        popup.mode = VideoSettingsMode::TitleEdit;
    }

    fn commit_video_title(&mut self) {
        let Some(popup) = self.video_settings_popup.as_ref() else {
            return;
        };
        let index = popup.stream_index;
        let title = popup.title_input.value.trim().to_string();
        if let Some(mut settings) = self.effective_video_settings(index) {
            settings.metadata.title = (!title.is_empty()).then_some(title);
            self.store_video_settings(index, settings);
        }
    }

    pub fn start_video_language_search(&mut self) {
        self.clear_text_input_reject();
        let Some(popup) = self
            .video_settings_popup
            .as_mut()
            .filter(|popup| popup.mode == VideoSettingsMode::LanguageDropdown)
        else {
            return;
        };
        popup.language_search.activate();
    }

    pub fn cancel_video_language_search(&mut self) {
        let Some(popup) = self.video_settings_popup.as_mut() else {
            return;
        };
        popup.language_search.clear();
        popup.language_cursor = 0;
    }

    pub fn video_field_changed(&self, field: VideoSettingsField) -> bool {
        let Some(popup) = self.video_settings_popup.as_ref() else {
            return false;
        };
        let Some(current) = self.effective_video_settings(popup.stream_index) else {
            return false;
        };
        let Some(original) = self.original_video_settings(popup.stream_index) else {
            return false;
        };
        match field {
            VideoSettingsField::Codec => current.codec != original.codec,
            VideoSettingsField::Resolution => current.resolution != original.resolution,
            VideoSettingsField::Rotation => current.rotation != original.rotation,
            VideoSettingsField::Language => current.metadata.language != original.metadata.language,
            VideoSettingsField::Title => current.metadata.title != original.metadata.title,
            VideoSettingsField::Default => {
                self.default_streams.contains(&popup.stream_index)
                    != self.original_default_streams.contains(&popup.stream_index)
            }
            VideoSettingsField::Commentary => {
                current.metadata.commentary != original.metadata.commentary
            }
        }
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
                let settings = self.effective_video_settings(index).unwrap_or_default();
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
                    _ => {}
                }
                return;
            }
            VideoSettingsMode::LanguageDropdown => {
                let popup = self.video_settings_popup.as_mut().unwrap();
                popup.language_search.clear();
                popup.mode = VideoSettingsMode::Summary;
                return;
            }
            VideoSettingsMode::TitleEdit => {
                self.commit_video_title();
                let popup = self.video_settings_popup.as_mut().unwrap();
                popup.title_input.deactivate();
                popup.mode = VideoSettingsMode::Summary;
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
        if self
            .video_settings_popup
            .as_ref()
            .is_some_and(|popup| popup.mode == VideoSettingsMode::TitleEdit)
        {
            self.commit_video_title();
        }
        self.close_video_settings();
        self.request_process_all();
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
        self.clear_text_input_reject();
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
        let Some(mut settings) = self.effective_video_settings(index) else {
            return false;
        };
        settings.resolution = resolution;
        self.store_video_settings(index, settings);
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
        if width < MINIMUM_CUSTOM_DIMENSION || height < MINIMUM_CUSTOM_DIMENSION {
            return Err(format!(
                "Width and height must be at least {MINIMUM_CUSTOM_DIMENSION} pixels."
            ));
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
                .or_else(|| match source_dimensions {
                    // Same condition as `custom_is_current`, expressed so the dimensions
                    // are read from the binding that proved they exist.
                    Some((width, height)) if source_preset.is_none() => {
                        Some(format!("Custom ({width}×{height})"))
                    }
                    _ => None,
                })
                .unwrap_or_else(|| "Custom…".to_string()),
            enabled: width.is_some() && height.is_some(),
            current: custom_is_current,
        };

        let mut choices = Vec::with_capacity(VideoResolution::PRESETS.len() + 1);
        // `PRESETS` holds only fixed resolutions today, but the invariant lives in
        // `edit.rs`; skipping a dimensionless entry keeps adding one from panicking here.
        for (value, (preset_width, preset_height)) in VideoResolution::PRESETS
            .into_iter()
            .filter_map(|value| Some((value, value.dimensions()?)))
        {
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
        // Bound once: without a target container nothing can be rejected, so both the
        // check and the explanation below read from the same value.
        let container = self.effective_container();
        let rejects = |codec_name: &str| {
            container.filter(|container| !container.supports_codec("video", codec_name, false))
        };

        let mut choices = Vec::with_capacity(VideoCodec::TARGETS.len() + 1);
        if source_codec.is_none() {
            let rejected_by = rejects(source_name);
            choices.push(VideoCodecChoice {
                value: VideoCodec::Original,
                label: source_name.to_uppercase(),
                current: true,
                enabled: rejected_by.is_none(),
                reason: rejected_by.map(|container| {
                    format!(
                        "{} cannot contain {} video",
                        container.label(),
                        source_name.to_uppercase()
                    )
                }),
            });
        }
        // `TARGETS` excludes `Original`, the only codec without an ffmpeg name; skipping
        // rather than unwrapping keeps that invariant local to this loop.
        choices.extend(VideoCodec::TARGETS.into_iter().filter_map(|codec| {
            let current = source_codec == Some(codec);
            let rejected_by = rejects(codec.codec_name()?);
            Some(VideoCodecChoice {
                value: if current { VideoCodec::Original } else { codec },
                label: codec.label().to_string(),
                current,
                enabled: rejected_by.is_none(),
                reason: rejected_by.map(|container| {
                    format!(
                        "{} cannot contain {} video",
                        container.label(),
                        codec.label()
                    )
                }),
            })
        }));
        choices
    }

    /// The Ctrl+S entry point everywhere (Files panel, Streams layer, and the
    /// Container/Video/Subtitle Settings dialogs' own Ctrl+S shortcuts) — snapshots
    /// the currently-open file's live edits (if any) so it's included, then requires
    /// every staged file to be `StagedFileStatus::Valid` before opening the confirm
    /// dialog; otherwise it names what's wrong rather than silently refusing. Even a
    /// single staged file goes through this batch flow now.
    pub fn request_process_all(&mut self) {
        if self.dialog.is_some() {
            return;
        }
        self.snapshot_current_edits();
        let paths: Vec<PathBuf> = self.staged_edits.keys().cloned().collect();
        if paths.is_empty() {
            self.notice = Some("No staged changes to process.".to_string());
            return;
        }
        let mut problems = Vec::new();
        for path in &paths {
            if let StagedFileStatus::Invalid(reason) = self.staged_file_status(path) {
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("file");
                problems.push(format!("{name}: {reason}"));
            }
        }
        if !problems.is_empty() {
            self.show_error(format!(
                "Resolve the following before processing:\n{}",
                problems.join("\n")
            ));
            return;
        }
        self.notice = None;
        self.confirm_process_all_scroll = 0;
        self.confirm_process_all_choice = ConfirmProcessAllChoice::Start;
        self.dialog = Some(Dialog::ConfirmProcessAll);
    }

    pub fn choose_confirm_process_all(&mut self, direction: isize) {
        if self.dialog != Some(Dialog::ConfirmProcessAll) || direction == 0 {
            return;
        }
        self.confirm_process_all_choice = if direction.is_positive() {
            ConfirmProcessAllChoice::Cancel
        } else {
            ConfirmProcessAllChoice::Start
        };
    }

    /// Activates whichever button is currently focused — reached by Enter, so a bare
    /// Enter still starts the batch immediately (matching the dialog's behavior
    /// before it grew a visible button bar), while Right/Left-ing over to Cancel and
    /// pressing Enter dismisses instead.
    pub fn activate_confirm_process_all(&mut self) {
        if self.dialog != Some(Dialog::ConfirmProcessAll) {
            return;
        }
        match self.confirm_process_all_choice {
            ConfirmProcessAllChoice::Start => self.confirm_process_all(),
            ConfirmProcessAllChoice::Cancel => self.dismiss_dialog(),
        }
    }

    /// Builds one `EditRequest` per staged file — classified into the transcode or
    /// remux pool via `plan_requires_transcode` — and dispatches them all sharing one
    /// batch-wide cancellation flag (`BatchState::cancelled`), so cancelling stops
    /// every request not yet finished, queued or running, with a single flag flip.
    pub fn confirm_process_all(&mut self) {
        if self.dialog != Some(Dialog::ConfirmProcessAll) {
            return;
        }
        let paths: Vec<PathBuf> = self.staged_edits.keys().cloned().collect();
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut items = Vec::new();
        for path in paths {
            // Re-checked defensively: `request_process_all` already required every
            // staged file to be `Valid`, but time (and a background reconcile) can
            // pass between opening the confirm dialog and this actually running.
            if !matches!(self.staged_file_status(&path), StagedFileStatus::Valid) {
                continue;
            }
            let Some(staged) = self.staged_edits.get(&path) else {
                continue;
            };
            let key = CacheKey {
                path: path.clone(),
                length: staged.fingerprint.length,
                modified: staged.fingerprint.modified,
            };
            let Some(ProbeOutcome::Video(info)) = self.cache.get(&key) else {
                continue;
            };
            let sidecars = self
                .sidecars_by_media
                .get(&path)
                .cloned()
                .unwrap_or_default();
            // `staged.stream_order` is the raw display order and still contains
            // deleted tracks (`final_stream_order` is what actually drops them and
            // reconciles group positions) — sending it as-is made `validate_edit`
            // see a track as both kept and deleted the moment anything got deleted.
            let stream_order =
                final_stream_order(info, &staged.stream_order, &staged.deleted_streams);
            let default_streams = staged
                .default_streams
                .difference(&staged.deleted_streams)
                .copied()
                .collect::<BTreeSet<_>>();
            let transcode = plan_requires_transcode(
                info,
                &stream_order,
                &staged.audio_settings,
                &staged.video_settings,
            );
            let request = EditRequest {
                path: path.clone(),
                destination: SaveDestination::ReplaceOriginal,
                container: staged.container_target,
                container_metadata: staged.container_metadata.clone(),
                stream_order,
                deleted_streams: staged.deleted_streams.clone(),
                default_streams,
                default_sidecars: staged.default_sidecars.clone(),
                audio_settings: staged.audio_settings.clone(),
                video_settings: staged.video_settings.clone(),
                subtitle_changes: staged.subtitle_changes.values().cloned().collect(),
                left_subtitle_order: staged.left_subtitle_order.clone(),
                sidecars,
                cancelled: cancelled.clone(),
            };
            let sender = if transcode {
                &self.transcode_tx
            } else {
                &self.remux_tx
            };
            if sender.send(request).is_ok() {
                items.push(BatchItem {
                    path,
                    label: None,
                    fraction: None,
                    status: BatchItemStatus::Pending,
                    output_path: None,
                });
            }
        }
        if items.is_empty() {
            self.dialog = None;
            self.notice =
                Some("Nothing could be started; the staged files may have changed.".to_string());
            return;
        }
        self.active_batch = Some(BatchState {
            cancelled,
            items,
            started: Instant::now(),
        });
        self.batch_scroll = 0;
        self.batch_cursor = 0;
        self.dialog = Some(Dialog::BatchProcessing);
    }

    /// Moves the `Dialog::BatchProcessing` highlight by `amount` rows; the viewport
    /// catches up later, in `sync_batch_scroll`.
    pub fn move_batch_cursor_down(&mut self, amount: usize) {
        let Some(last) = self.batch_cursor_last_row() else {
            return;
        };
        self.batch_cursor = self.batch_cursor.saturating_add(amount).min(last);
    }

    pub fn move_batch_cursor_up(&mut self, amount: usize) {
        if self.batch_cursor_last_row().is_none() {
            return;
        }
        self.batch_cursor = self.batch_cursor.saturating_sub(amount);
    }

    /// The highest selectable row index, or `None` when the batch popup is not the
    /// active dialog or holds no items — both cases where the cursor must not move.
    fn batch_cursor_last_row(&self) -> Option<usize> {
        if self.dialog != Some(Dialog::BatchProcessing) {
            return None;
        }
        self.active_batch
            .as_ref()
            .map(|batch| batch.items.len())
            .filter(|len| *len > 0)
            .map(|len| len - 1)
    }

    /// Slides the viewport the minimum distance needed to keep `batch_cursor` visible.
    /// Called from the renderer, the only place that knows how many rows fit.
    pub fn sync_batch_scroll(&mut self, visible_rows: usize) {
        let total = self
            .active_batch
            .as_ref()
            .map_or(0, |batch| batch.items.len());
        let visible_rows = visible_rows.max(1);
        let mut scroll = self.batch_scroll as usize;
        if self.batch_cursor < scroll {
            scroll = self.batch_cursor;
        } else if self.batch_cursor >= scroll + visible_rows {
            scroll = self.batch_cursor + 1 - visible_rows;
        }
        self.batch_scroll = scroll.min(total.saturating_sub(visible_rows)) as u16;
    }

    /// `r` on a file row in the Files panel — the one `r` case that needs a confirm,
    /// since it discards a whole file's staged edits rather than one field.
    pub fn request_reset_file(&mut self, path: PathBuf) {
        if self.dialog.is_some() {
            return;
        }
        self.pending_reset = Some(ResetScope::File(path));
        self.reset_choice = ResetChoice::KeepEdits;
        self.dialog = Some(Dialog::ConfirmReset);
    }

    /// `R` anywhere outside the Files panel — resets the whole currently-open file.
    pub fn request_reset_current_file(&mut self) {
        if self.dialog.is_some() {
            return;
        }
        self.pending_reset = Some(ResetScope::CurrentFile);
        self.reset_choice = ResetChoice::KeepEdits;
        self.dialog = Some(Dialog::ConfirmReset);
    }

    /// `R` in the Files panel — resets every staged file in the directory.
    pub fn request_reset_all_files(&mut self) {
        if self.dialog.is_some() {
            return;
        }
        self.pending_reset = Some(ResetScope::AllFiles);
        self.reset_choice = ResetChoice::KeepEdits;
        self.dialog = Some(Dialog::ConfirmReset);
    }

    pub fn pending_reset(&self) -> Option<&ResetScope> {
        self.pending_reset.as_ref()
    }

    pub fn choose_reset(&mut self, direction: isize) {
        if self.dialog != Some(Dialog::ConfirmReset) || direction == 0 {
            return;
        }
        self.reset_choice = if direction.is_positive() {
            ResetChoice::ResetEdits
        } else {
            ResetChoice::KeepEdits
        };
    }

    pub fn dismiss_reset(&mut self) {
        if self.dialog != Some(Dialog::ConfirmReset) {
            return;
        }
        self.pending_reset = None;
        self.reset_choice = ResetChoice::default();
        self.dialog = None;
    }

    /// Carries out `pending_reset` if the user chose `ResetEdits`, otherwise just
    /// dismisses like `dismiss_reset`.
    pub fn activate_reset(&mut self) {
        if self.dialog != Some(Dialog::ConfirmReset) {
            return;
        }
        let Some(scope) = self.pending_reset.take() else {
            self.dialog = None;
            return;
        };
        if self.reset_choice != ResetChoice::ResetEdits {
            self.reset_choice = ResetChoice::default();
            self.dialog = None;
            return;
        }
        match scope {
            ResetScope::File(path) => self.discard_staged_file(&path),
            ResetScope::CurrentFile => {
                if let Some(path) = self.selected_file().map(|file| file.path.clone()) {
                    self.staged_edits.remove(&path);
                }
                self.reset_track_edits();
            }
            ResetScope::AllFiles => {
                self.staged_edits.clear();
                if self.selected_file().is_some() {
                    self.reset_track_edits();
                }
            }
        }
        self.reset_choice = ResetChoice::default();
        self.dialog = None;
        self.notice = Some("Staged edits reset.".to_string());
    }

    /// Unstages `path`'s edits and, if it's the currently-open file, resets the live
    /// edit fields too. Refreshes `self.outcome` from the cache first, so a discard
    /// triggered by a genuine on-disk change rebuilds the reset baseline from the
    /// file's *current* content rather than whatever was probed before — see
    /// `refresh_cached_outcome`.
    fn discard_staged_file(&mut self, path: &Path) {
        self.staged_edits.remove(path);
        if self.selected_file().is_some_and(|file| file.path == path) {
            self.refresh_cached_outcome();
            self.reset_track_edits();
        }
    }

    /// The staged files whose background structural re-probe found a track type they
    /// edit no longer holding the tracks they were staged against. Computed on demand
    /// from `staged_edits` rather than stored, so it can never drift from live state.
    /// Sorted for stable rendering order.
    pub fn conflicting_paths(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = self
            .staged_edits
            .iter()
            .filter(|(_, staged)| staged.stale && !staged.conflict_groups.is_empty())
            .map(|(path, _)| path.clone())
            .collect();
        paths.sort();
        paths
    }

    /// The conflicting track types for `path`, for the notice's per-file heading.
    pub fn conflict_groups_for(&self, path: &Path) -> BTreeSet<&'static str> {
        self.staged_edits
            .get(path)
            .map(|staged| staged.conflict_groups.clone())
            .unwrap_or_default()
    }

    /// The staged changes `acknowledge_conflicts` would revert for `path` — only the
    /// ones belonging to its conflicting track types, so the notice lists exactly what
    /// is about to be lost and nothing else.
    pub fn conflicting_change_summary(&self, path: &Path) -> Vec<String> {
        self.split_change_summary(path).0
    }

    /// The mirror image: everything staged for `path` that survives acknowledgement.
    /// The notice shows this next to the reverted list, because "only the conflicting
    /// type is reverted" is invisible otherwise — a file with five staged changes and
    /// one of them listed reads as though the other four went too.
    pub fn kept_change_summary(&self, path: &Path) -> Vec<String> {
        self.split_change_summary(path).1
    }

    /// `(reverted, kept)` for `path`, partitioned by whether each staged change
    /// belongs to a conflicting track type. Container-level changes carry their own
    /// label and so always land in `kept`.
    fn split_change_summary(&self, path: &Path) -> (Vec<String>, Vec<String>) {
        let Some(staged) = self.staged_edits.get(path) else {
            return (Vec::new(), Vec::new());
        };
        let key = CacheKey {
            path: path.to_path_buf(),
            length: staged.fingerprint.length,
            modified: staged.fingerprint.modified,
        };
        let Some(ProbeOutcome::Video(info)) = self.cache.get(&key) else {
            return (vec!["(changes staged)".to_string()], Vec::new());
        };
        let mut reverted = Vec::new();
        let mut kept = Vec::new();
        for (group, line) in staged_edit_summary_entries(path, info, staged) {
            if staged.conflict_groups.contains(group) {
                reverted.push(line);
            } else {
                kept.push(line);
            }
        }
        (reverted, kept)
    }

    /// Opens `Dialog::ResolveConflicts` the instant there's something to resolve and
    /// nothing else is already showing — called every main-loop tick, so it fires
    /// both the moment a background check confirms a conflict and the moment any
    /// other dialog closes, with no manual action required.
    /// Returns whether it opened the dialog, so the main loop knows to redraw —
    /// otherwise a conflict confirmed between key presses would sit unseen until
    /// some unrelated event happened to trigger a redraw.
    pub fn maybe_open_conflict_dialog(&mut self) -> bool {
        if self.dialog.is_some() || self.conflicting_paths().is_empty() {
            return false;
        }
        self.conflict_scroll = 0;
        self.conflict_opened_at = Some(Instant::now());
        self.dialog = Some(Dialog::ResolveConflicts);
        true
    }

    /// Whole seconds left before the notice's button arms, or `None` once it has (and
    /// whenever the notice isn't showing). Counts down 5, 4, 3, 2, 1 — rendered into
    /// the button label itself, so an inert button reads as deliberate rather than
    /// broken. A file that starts conflicting while the notice is already open joins
    /// the list without restarting the countdown; the user is already reading it, and
    /// a file changing repeatedly could otherwise keep the button from ever arming.
    pub fn conflict_countdown(&self) -> Option<u64> {
        if self.dialog != Some(Dialog::ResolveConflicts) {
            return None;
        }
        let remaining = CONFLICT_COUNTDOWN.checked_sub(self.conflict_opened_at?.elapsed())?;
        // Ceiling, so the first frame reads "(5)" and the last whole second "(1)"
        // rather than starting a second low and ending on a stale "(0)".
        (!remaining.is_zero()).then(|| remaining.as_millis().div_ceil(1000) as u64)
    }

    /// Whether the UI changes on its own, with no input and no state change — the
    /// batch progress spinner, and the conflict notice's counting-down button.
    /// `main`'s loop repaints every tick while this holds *and once more after it
    /// stops*, since the frame that shows the finished state (a settled gauge, an
    /// armed button) is by definition the first frame where this is false.
    pub fn is_animating(&self) -> bool {
        matches!(self.dialog, Some(Dialog::BatchProcessing)) || self.conflict_countdown().is_some()
    }

    pub fn scroll_conflicts(&mut self, direction: isize) {
        if self.dialog != Some(Dialog::ResolveConflicts) {
            return;
        }
        self.conflict_scroll = if direction.is_positive() {
            self.conflict_scroll
                .saturating_add(1)
                .min(self.conflict_max_scroll)
        } else {
            self.conflict_scroll.saturating_sub(1)
        };
    }

    pub fn set_conflict_max_scroll(&mut self, maximum: u16) {
        self.conflict_max_scroll = maximum;
        self.conflict_scroll = self.conflict_scroll.min(maximum);
    }

    /// The notice's only exit — there is no defer and no keep, because neither can
    /// produce a processable edit: the tracks these staged changes name are not the
    /// tracks in the file any more, and no amount of re-validating them changes that.
    /// Reverts each listed file's staged changes for its conflicting types only,
    /// leaving every other type, the container target, and the container metadata
    /// staged.
    pub fn acknowledge_conflicts(&mut self) {
        if self.dialog != Some(Dialog::ResolveConflicts) || self.conflict_countdown().is_some() {
            return;
        }
        for path in self.conflicting_paths() {
            self.revert_group_edits(&path);
        }
        self.conflict_scroll = 0;
        self.conflict_opened_at = None;
        self.dialog = None;
    }

    /// Drops everything `path`'s staged edit stages for its conflicting track types
    /// and rebuilds those types from the file's current content, exactly as reopening
    /// the file would for just those tracks. Everything else the edit stages survives.
    ///
    /// Reuses `reconcile_untouched_groups` via `force_untouched` for the track fields;
    /// `moved_streams`, `video_settings` and the subtitle state it doesn't carry are
    /// pruned here, keyed off the *pre-revert* `track_groups` snapshot, since that is
    /// the only record of which type a staged index used to belong to.
    fn revert_group_edits(&mut self, path: &Path) {
        let Some(file) = self.files.iter().find(|file| file.path == path) else {
            return;
        };
        let fingerprint = file.fingerprint;
        let key = CacheKey {
            path: path.to_path_buf(),
            length: fingerprint.length,
            modified: fingerprint.modified,
        };
        // The file changed again between the check that raised this notice and the
        // acknowledgement, and the newer content isn't probed yet. Reverting against
        // stale content would rebuild the group from tracks that are already gone, so
        // leave it conflicted — `reconcile_files` has already re-dispatched a check,
        // and the notice reopens on its own once that answers.
        let Some(ProbeOutcome::Video(info)) = self.cache.get(&key).cloned() else {
            return;
        };
        let Some(staged) = self.staged_edits.get_mut(path) else {
            return;
        };
        let reverted = std::mem::take(&mut staged.conflict_groups);
        if reverted.is_empty() {
            return;
        }
        let was_in_group = |index: &u64| {
            staged
                .track_groups
                .get(index)
                .is_some_and(|group| reverted.contains(group))
        };
        staged.moved_streams.retain(|index| !was_in_group(index));
        staged
            .audio_settings
            .retain(|index, _| !was_in_group(index));
        staged
            .video_settings
            .retain(|index, _| !was_in_group(index));
        // Subtitle changes are pinned to a group rather than an index — a sidecar
        // change has no embedded index at all — so reverting "subtitle" clears the
        // whole set, mirroring how `reconcile_untouched_groups` treats any subtitle
        // change as touching the entire group.
        if reverted.contains("subtitle") {
            staged.subtitle_changes.clear();
            staged.default_sidecars.clear();
            staged.left_subtitle_order.clear();
        }

        let reconciled = reconcile_untouched_groups(
            &info,
            &staged.stream_order,
            &staged.deleted_streams,
            &staged.default_streams,
            &staged.original_stream_order,
            &staged.original_default_streams,
            &staged.track_groups,
            &staged.moved_streams,
            &staged.audio_settings,
            &staged.video_settings,
            &staged.subtitle_changes,
            &reverted,
        );
        staged.stream_order = reconciled.stream_order;
        staged.deleted_streams = reconciled.deleted_streams;
        staged.default_streams = reconciled.default_streams;
        staged.original_stream_order = reconciled.original_stream_order;
        staged.original_default_streams = reconciled.original_default_streams;
        staged.track_groups = reconciled.track_groups;
        staged.fingerprint = fingerprint;
        staged.stale = false;

        // Reverting the only thing that was staged leaves nothing behind — drop the
        // entry rather than leaving an edit that stages nothing marked as staged.
        let snapshot = self.staged_edits.get(path).cloned();
        let is_open = self.selected_file().is_some_and(|file| file.path == path);
        match snapshot.filter(staged_edit_stages_anything) {
            Some(snapshot) => {
                if is_open {
                    // The Streams view still shows what was on screen before the
                    // revert; resync it from the rebuilt snapshot.
                    self.refresh_cached_outcome();
                    self.apply_staged_snapshot(&snapshot);
                }
            }
            None => self.discard_staged_file(path),
        }
    }

    /// Actually stops the batch: flips the shared cancellation flag, so every request
    /// not yet finished (queued or running, across both pools) aborts at its next
    /// checkpoint. Reached only via `activate_cancel_edit`'s confirm step.
    pub fn cancel_edit(&mut self) {
        if !matches!(
            self.dialog,
            Some(Dialog::BatchProcessing | Dialog::ConfirmCancel)
        ) {
            return;
        }
        if let Some(batch) = &self.active_batch {
            batch.cancelled.store(true, Ordering::Relaxed);
        }
        self.dialog = Some(Dialog::BatchProcessing);
        self.edit_error = None;
        self.cancel_edit_choice = CancelEditChoice::KeepProcessing;
    }

    /// Opens the "are you sure you want to cancel?" prompt over the batch progress
    /// view, rather than cancelling immediately — the batch keeps running
    /// (unpaused) while the prompt is up; nothing is actually stopped until
    /// `activate_cancel_edit` confirms it.
    pub fn request_cancel_edit(&mut self) {
        if self.dialog != Some(Dialog::BatchProcessing) {
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
            self.dialog = Some(Dialog::BatchProcessing);
        }
    }

    pub fn dismiss_dialog(&mut self) {
        if matches!(
            self.dialog,
            Some(Dialog::BatchProcessing | Dialog::ConfirmCancel)
        ) {
            return;
        }
        self.dialog = None;
        self.edit_error = None;
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
        self.clear_text_input_reject();
        self.keybindings_search.activate();
    }

    pub fn finish_keybindings_search(&mut self) {
        self.keybindings_search.deactivate();
    }

    pub fn clear_keybindings_search(&mut self) {
        self.keybindings_search.clear();
        self.keybindings_scroll = 0;
    }

    pub fn start_file_search(&mut self) {
        self.clear_text_input_reject();
        if self.layer != Layer::Files {
            return;
        }
        if self.file_search.value.is_empty() {
            self.file_search_origin = self.selected_file().map(|file| file.path.clone());
        }
        self.file_search.activate();
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
        let entries = self.file_panel_entries();
        let selection = preferred_position.or_else(|| (!entries.is_empty()).then_some(0));
        let next_path = selection
            .and_then(|position| entries.get(position).cloned())
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

    pub fn scroll_confirm_process_all_down(&mut self, amount: u16) {
        self.confirm_process_all_scroll = scroll_forward(
            self.confirm_process_all_scroll,
            self.confirm_process_all_max_scroll,
            amount,
        );
    }

    pub fn scroll_confirm_process_all_up(&mut self, amount: u16) {
        self.confirm_process_all_scroll = scroll_backward(self.confirm_process_all_scroll, amount);
    }

    pub fn scroll_confirm_process_all_to_start(&mut self) {
        self.confirm_process_all_scroll = 0;
    }

    pub fn scroll_confirm_process_all_to_end(&mut self) {
        self.confirm_process_all_scroll = self.confirm_process_all_max_scroll;
    }

    pub fn set_confirm_process_all_max_scroll(&mut self, maximum: u16) {
        self.confirm_process_all_max_scroll = maximum;
        self.confirm_process_all_scroll = self.confirm_process_all_scroll.min(maximum);
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
    }

    /// Saves the currently-open file's live edit fields into `staged_edits` if it has
    /// any (`has_track_edits()`), or removes any prior staged entry for it if it no
    /// longer does (e.g. the user reset every edit before switching away). Must be
    /// called while the file being left is still the selected file, so `media_info()`
    /// still resolves to its probe data.
    ///
    /// Preserves an existing entry's `fingerprint`/`stale`/`conflict_groups`
    /// rather than blindly re-baselining them against the live file whenever it's
    /// already `stale` — this is also called unconditionally from
    /// `request_process_all` and `select_file_position_with_force`, and clobbering a
    /// pending or confirmed conflict here would silently launder it past
    /// `staged_file_status`'s staleness check before the background conflict pipeline
    /// (`App::reconcile_files`/`receive_conflict_probe_results`) ever gets to resolve
    /// it properly.
    fn snapshot_current_edits(&mut self) {
        let Some(file) = self.selected_file() else {
            return;
        };
        let path = file.path.clone();
        if self.has_track_edits() {
            let (fingerprint, stale, conflict_groups) = match self.staged_edits.get(&path) {
                Some(existing) if existing.stale => (
                    existing.fingerprint,
                    existing.stale,
                    existing.conflict_groups.clone(),
                ),
                _ => (file.fingerprint, false, BTreeSet::new()),
            };
            self.staged_edits.insert(
                path,
                StagedEdit {
                    fingerprint,
                    stale,
                    conflict_groups,
                    stream_order: self.stream_order.clone(),
                    moved_streams: self.moved_streams.clone(),
                    deleted_streams: self.deleted_streams.clone(),
                    default_streams: self.default_streams.clone(),
                    default_sidecars: self.default_sidecars.clone(),
                    audio_settings: self.audio_settings.clone(),
                    video_settings: self.video_settings.clone(),
                    subtitle_changes: self.subtitle_changes.clone(),
                    left_subtitle_order: self.left_subtitle_order.clone(),
                    container_target: self.container_target,
                    container_metadata: self.container_metadata.clone(),
                    original_stream_order: self.original_stream_order.clone(),
                    original_default_streams: self.original_default_streams.clone(),
                    track_groups: self.track_groups.clone(),
                },
            );
        } else {
            self.staged_edits.remove(&path);
        }
    }

    /// Loads the newly-selected file's previously staged edits (if any) into the live
    /// edit fields, or falls back to deriving a fresh baseline from its probe data
    /// (`reset_track_edits`) if nothing was staged for it. Replaces the two call sites
    /// that used to call `reset_track_edits()` unconditionally whenever a file became
    /// selected. Staged edits are loaded even if flagged `stale`, so the user can see
    /// and manually resolve them rather than having them silently vanish.
    fn load_staged_or_reset(&mut self) {
        let Some(path) = self.selected_file().map(|file| file.path.clone()) else {
            self.clear_track_edits();
            return;
        };
        let Some(staged) = self.staged_edits.get(&path).cloned() else {
            self.reset_track_edits();
            return;
        };
        self.apply_staged_snapshot(&staged);
    }

    /// Copies one `StagedEdit`'s track/container/subtitle fields onto the live edit
    /// fields — shared by `load_staged_or_reset` (selecting a file), `keep_staged_edit`
    /// (re-syncing the Streams view after "Keep" resolves a conflict on the currently
    /// open file), and `receive_conflict_probe_results` (syncing after a quiet
    /// background auto-resolve on the currently open file).
    fn apply_staged_snapshot(&mut self, staged: &StagedEdit) {
        self.stream_order = staged.stream_order.clone();
        self.moved_streams = staged.moved_streams.clone();
        self.deleted_streams = staged.deleted_streams.clone();
        self.default_streams = staged.default_streams.clone();
        self.default_sidecars = staged.default_sidecars.clone();
        self.audio_settings = staged.audio_settings.clone();
        self.video_settings = staged.video_settings.clone();
        self.subtitle_changes = staged.subtitle_changes.clone();
        self.left_subtitle_order = staged.left_subtitle_order.clone();
        self.container_target = staged.container_target;
        self.container_metadata = staged.container_metadata.clone();
        self.original_stream_order = staged.original_stream_order.clone();
        self.original_default_streams = staged.original_default_streams.clone();
        self.track_groups = staged.track_groups.clone();
        self.audio_settings_popup = None;
        self.video_settings_popup = None;
        self.subtitle_settings_popup = None;
        self.container_settings_popup = None;
    }

    /// Refreshes `self.outcome` from `self.cache` for the currently-selected file,
    /// without `queue_probe`'s side effects (bumping `generation`, resetting
    /// `layer`/`details_scroll`/`selected_stream`, dispatching a new async probe).
    /// Used once a staged conflict resolves for the open file — the fresh outcome is
    /// already cached by whatever triggered the resolution (`receive_conflict_probe_results`
    /// inserts it before any staged-edit logic runs; `keep_staged_edit` only proceeds
    /// once it's confirmed cached).
    fn refresh_cached_outcome(&mut self) {
        if let Some(key) = self.selected_file().map(CacheKey::for_file)
            && let Some(cached) = self.cache.get(&key)
        {
            self.outcome = Some(cached.clone());
        }
    }

    /// Restores one embedded track's position in `stream_order` to where it sat
    /// relative to the *other* tracks' current (possibly also individually staged)
    /// arrangement, without disturbing their positions. Concretely: take the current
    /// order minus this track, then reinsert it immediately after the last track that
    /// preceded it in `original_stream_order` (or at the front if none did). This is
    /// well-defined for any permutation of survivors and only perturbs the one track
    /// being reset — used by `reset_track` (the `r`-on-a-track-row keybind).
    fn reset_track_position(&mut self, track: u64) {
        if !self.stream_order.contains(&track) {
            return;
        }
        let others: Vec<u64> = self
            .stream_order
            .iter()
            .copied()
            .filter(|candidate| *candidate != track)
            .collect();
        let before: BTreeSet<u64> = self
            .original_stream_order
            .iter()
            .copied()
            .take_while(|candidate| *candidate != track)
            .filter(|candidate| others.contains(candidate))
            .collect();
        let insert_at = others
            .iter()
            .rposition(|candidate| before.contains(candidate))
            .map_or(0, |position| position + 1);
        let mut new_order = others;
        new_order.insert(insert_at, track);
        self.stream_order = new_order;
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

    /// Resets every staged edit type for a single track (the `r` keybind on a track
    /// row in the Streams list): delete flag, default flag, video settings, subtitle
    /// change, and — for embedded tracks — position (via `reset_track_position`),
    /// while leaving every other track's staged edits untouched. For the container
    /// row, resets the staged container format/metadata.
    pub fn reset_track(&mut self, track: TrackRef) {
        match track {
            TrackRef::Container => {
                self.container_target = None;
                self.container_metadata = None;
            }
            TrackRef::Embedded(index) => {
                self.deleted_streams.remove(&index);
                self.audio_settings.remove(&index);
                self.video_settings.remove(&index);
                let originally_default = self.original_default_streams.contains(&index);
                if originally_default {
                    self.default_streams.insert(index);
                } else {
                    self.default_streams.remove(&index);
                }
                self.subtitle_changes
                    .remove(&SubtitleSource::Embedded(index));
                self.reset_track_position(index);
            }
            TrackRef::Sidecar(sidecar_index) => {
                self.default_sidecars.remove(&sidecar_index);
                if let Some(sidecar) = self.sidecars.get(sidecar_index) {
                    let source = SubtitleSource::Sidecar(sidecar.path.clone());
                    self.subtitle_changes.remove(&source);
                }
            }
        }
    }

    /// The `r` keybind's dispatch when nothing needs a confirm: inside a settings
    /// popup (in its field-navigation mode, not while text-editing — those modes have
    /// their own key handling in `main.rs` and never reach this), resets just the
    /// currently focused field; in the Streams track list, resets the whole
    /// highlighted track (`reset_track`). A no-op everywhere else.
    pub fn reset_focused_field(&mut self) {
        match self.dialog {
            Some(Dialog::ContainerSettings) => {
                if let Some(field) = self
                    .container_settings_popup
                    .as_ref()
                    .map(|popup| popup.field)
                {
                    self.reset_container_field(field);
                }
            }
            Some(Dialog::VideoSettings) => {
                if let Some(field) = self.video_settings_popup.as_ref().map(|popup| popup.field) {
                    self.reset_video_field(field);
                }
            }
            Some(Dialog::AudioSettings) => {
                if let Some(field) = self.audio_settings_popup.as_ref().map(|popup| popup.field) {
                    self.reset_audio_field(field);
                }
            }
            Some(Dialog::SubtitleSettings) => {
                if let Some(field) = self
                    .subtitle_settings_popup
                    .as_ref()
                    .map(|popup| popup.field)
                {
                    self.reset_subtitle_field(field);
                }
            }
            None if self.layer == Layer::Streams => {
                if let Some(track) = self.selected_track() {
                    self.reset_track(track);
                }
            }
            _ => {}
        }
    }

    /// Resets one `ContainerSettings` field to its original value: `Format` clears
    /// the staged container target entirely; every metadata field reverts just that
    /// one property, leaving any other staged metadata field untouched — mirrors
    /// `save_container_text_input`'s "compare to original, store `Some`/`None`" logic.
    fn reset_container_field(&mut self, field: ContainerSettingsField) {
        if field == ContainerSettingsField::Format {
            self.container_target = None;
            return;
        }
        let mut metadata = self.effective_container_metadata().unwrap_or_default();
        let original = self.original_container_metadata().unwrap_or_default();
        match field {
            ContainerSettingsField::Title => metadata.title = original.title.clone(),
            ContainerSettingsField::Comment => metadata.comment = original.comment.clone(),
            ContainerSettingsField::Date => metadata.date = original.date.clone(),
            ContainerSettingsField::Genre => metadata.genre = original.genre.clone(),
            ContainerSettingsField::Artist => metadata.artist = original.artist.clone(),
            ContainerSettingsField::Format => unreachable!(),
        }
        self.container_metadata = (metadata != original).then_some(metadata);
    }

    fn reset_audio_field(&mut self, field: AudioSettingsField) {
        let Some(index) = self
            .audio_settings_popup
            .as_ref()
            .map(|popup| popup.stream_index)
        else {
            return;
        };
        if field == AudioSettingsField::Default {
            if self.original_default_streams.contains(&index) {
                self.default_streams.insert(index);
            } else {
                self.default_streams.remove(&index);
            }
            return;
        }
        let Some(original) = self.original_audio_settings(index) else {
            return;
        };
        let mut settings = self
            .effective_audio_settings(index)
            .expect("original audio settings imply effective audio settings");
        match field {
            AudioSettingsField::Codec => settings.codec = original.codec,
            AudioSettingsField::ChannelLayout => settings.channel_layout = original.channel_layout,
            AudioSettingsField::Language => settings.metadata.language = original.metadata.language,
            AudioSettingsField::Title => settings.metadata.title = original.metadata.title,
            field => {
                let role = field.role().expect("audio role field");
                settings
                    .metadata
                    .set_role(role, original.metadata.get_role(role));
            }
        }
        self.store_audio_settings(index, settings);
    }

    /// Resets one `VideoSettings` field to its original value, leaving any other
    /// staged field alone — mirrors `reset_audio_field`.
    fn reset_video_field(&mut self, field: VideoSettingsField) {
        let Some(index) = self
            .video_settings_popup
            .as_ref()
            .map(|popup| popup.stream_index)
        else {
            return;
        };
        if field == VideoSettingsField::Default {
            if self.original_default_streams.contains(&index) {
                self.default_streams.insert(index);
            } else {
                self.default_streams.remove(&index);
            }
            return;
        }
        let Some(original) = self.original_video_settings(index) else {
            return;
        };
        let mut settings = self
            .effective_video_settings(index)
            .expect("original video settings imply effective video settings");
        match field {
            VideoSettingsField::Codec => settings.codec = original.codec,
            VideoSettingsField::Resolution => settings.resolution = original.resolution,
            VideoSettingsField::Rotation => settings.rotation = original.rotation,
            VideoSettingsField::Language => settings.metadata.language = original.metadata.language,
            VideoSettingsField::Title => settings.metadata.title = original.metadata.title,
            VideoSettingsField::Commentary => {
                settings.metadata.commentary = original.metadata.commentary
            }
            VideoSettingsField::Default => unreachable!(),
        }
        self.store_video_settings(index, settings);
    }

    /// Resets one `SubtitleSettings` field to its original value. `Codec` clears
    /// whichever of `embedded_target`/`export_target` is currently active (mirroring
    /// the same "exporting vs. embedding" branch `activate_subtitle_settings`'s
    /// `CodecDropdown` case uses when *staging* a choice) — it does not touch
    /// `import_into_media`, which is a separate action (`transfer_subtitle`, Ctrl-h/l)
    /// not driven by this field at all. `Default` reverts the mutually-exclusive
    /// default-subtitle marking for this source specifically. The seven
    /// metadata-backed fields (language/title/forced/cc/hearing-impaired/original/
    /// commentary) each revert just that one property via `store_subtitle_metadata`'s
    /// "compare to original" logic. In every case, any other staged field on the same
    /// subtitle is left untouched.
    fn reset_subtitle_field(&mut self, field: SubtitleSettingsField) {
        let Some(popup) = self.subtitle_settings_popup.as_ref() else {
            return;
        };
        let source = popup.source.clone();
        let source_format = popup.source_format;
        match field {
            SubtitleSettingsField::Codec => {
                let mut change = self.subtitle_change(&source, source_format);
                if change.export_target.is_some() {
                    change.export_target = None;
                } else {
                    change.embedded_target = None;
                }
                self.store_subtitle_change(source, change);
            }
            SubtitleSettingsField::Default => match &source {
                SubtitleSource::Embedded(index) => {
                    self.default_streams.remove(index);
                    if self.original_default_streams.contains(index) {
                        self.default_streams.insert(*index);
                    }
                }
                SubtitleSource::Sidecar(_) => {
                    if let Some(index) = self.subtitle_sidecar_index(&source) {
                        self.default_sidecars.remove(&index);
                    }
                }
            },
            _ => {
                let (Some(mut metadata), Some(original)) = (
                    self.subtitle_metadata_for(&source),
                    self.original_subtitle_metadata(&source),
                ) else {
                    return;
                };
                match field {
                    SubtitleSettingsField::Language => {
                        metadata.language = original.language.clone()
                    }
                    SubtitleSettingsField::Title => metadata.title = original.title.clone(),
                    SubtitleSettingsField::Forced => metadata.forced = original.forced,
                    SubtitleSettingsField::Cc => metadata.cc = original.cc,
                    SubtitleSettingsField::HearingImpaired => {
                        metadata.hearing_impaired = original.hearing_impaired;
                    }
                    SubtitleSettingsField::Original => metadata.original = original.original,
                    SubtitleSettingsField::Commentary => metadata.commentary = original.commentary,
                    SubtitleSettingsField::Codec | SubtitleSettingsField::Default => unreachable!(),
                }
                self.store_subtitle_metadata(source, source_format, metadata);
            }
        }
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
        let track_groups = track_groups_for(info);
        self.stream_order = order.clone();
        self.original_stream_order = order;
        self.track_groups = track_groups;
        self.moved_streams.clear();
        self.default_streams = defaults.clone();
        self.original_default_streams = defaults;
        self.default_sidecars.clear();
        self.deleted_streams.clear();
        self.audio_settings.clear();
        self.audio_settings_popup = None;
        self.video_settings.clear();
        self.video_settings_popup = None;
        self.subtitle_changes.clear();
        self.subtitle_settings_popup = None;
        self.container_target = None;
        self.container_settings_popup = None;
        self.container_metadata = None;
    }

    fn clear_track_edits(&mut self) {
        self.stream_order.clear();
        self.original_stream_order.clear();
        self.track_groups.clear();
        self.left_subtitle_order.clear();
        self.moved_streams.clear();
        self.deleted_streams.clear();
        self.default_streams.clear();
        self.original_default_streams.clear();
        self.default_sidecars.clear();
        self.audio_settings.clear();
        self.audio_settings_popup = None;
        self.video_settings.clear();
        self.video_settings_popup = None;
        self.subtitle_changes.clear();
        self.subtitle_settings_popup = None;
        self.container_target = None;
        self.container_settings_popup = None;
        self.container_metadata = None;
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
        changed.extend(self.audio_settings.keys().copied());
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
            || self.container_metadata.is_some()
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
            || !self.audio_settings.is_empty()
            || !self.video_settings.is_empty()
            || self
                .subtitle_changes
                .values()
                .any(SubtitleChange::has_effect)
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

/// Snapshot of every stream's `stream_group` in `info`, for `StagedEdit::track_groups`
/// — see `reconcile_untouched_groups`.
fn track_groups_for(info: &MediaInfo) -> BTreeMap<u64, &'static str> {
    info.streams
        .iter()
        .filter_map(|stream| Some((stream_index(stream)?, stream_group(stream))))
        .collect()
}

fn is_default(stream: &std::collections::BTreeMap<String, serde_json::Value>) -> bool {
    stream
        .get("disposition")
        .and_then(serde_json::Value::as_object)
        .and_then(|disposition| disposition.get("default"))
        .and_then(serde_json::Value::as_i64)
        == Some(1)
}

/// Free-function core of `App::original_subtitle_metadata` — see
/// `container_conflicts_for_plan` for why this is parameterized rather than reading
/// `self.*` directly.
fn original_subtitle_metadata_for(
    info: Option<&MediaInfo>,
    sidecars: &[SidecarEntry],
    source: &SubtitleSource,
) -> Option<SubtitleMetadata> {
    match source {
        SubtitleSource::Embedded(index) => {
            let stream = info.and_then(|info| stream_by_index(info, *index))?;
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
            let sidecar = sidecars.iter().find(|sidecar| &sidecar.path == path)?;
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

/// Free-function core of `App::subtitle_metadata_for` — see
/// `container_conflicts_for_plan`.
fn subtitle_metadata_for_plan(
    subtitle_changes: &BTreeMap<SubtitleSource, SubtitleChange>,
    info: Option<&MediaInfo>,
    sidecars: &[SidecarEntry],
    source: &SubtitleSource,
) -> Option<SubtitleMetadata> {
    subtitle_changes
        .get(source)
        .and_then(|change| change.metadata.clone())
        .or_else(|| original_subtitle_metadata_for(info, sidecars, source))
}

/// A one-line summary notice shown once every file in a batch reaches a terminal
/// state — e.g. "Processed 3 of 4 files (1 failed)." Singular/plural and the omission
/// of a breakdown when everything succeeded are handled so the common case reads
/// cleanly rather than "Processed 1 of 1 files (0 failed, 0 cancelled)."
fn summarize_batch_outcome(
    total: usize,
    completed: usize,
    failed: usize,
    cancelled: usize,
) -> String {
    let file_word = if total == 1 { "file" } else { "files" };
    if completed == total {
        return format!("Processed {total} {file_word}.");
    }
    let mut detail = Vec::new();
    if failed > 0 {
        detail.push(format!("{failed} failed"));
    }
    if cancelled > 0 {
        detail.push(format!("{cancelled} cancelled"));
    }
    if detail.is_empty() {
        format!("Processed {completed} of {total} {file_word}.")
    } else {
        format!(
            "Processed {completed} of {total} {file_word} ({}).",
            detail.join(", ")
        )
    }
}

/// Free-function core of `App::subtitle_language_error` — see
/// `container_conflicts_for_plan`.
fn subtitle_language_error_for(
    info: Option<&MediaInfo>,
    deleted_streams: &BTreeSet<u64>,
    subtitle_changes: &BTreeMap<SubtitleSource, SubtitleChange>,
    sidecars: &[SidecarEntry],
) -> Option<String> {
    if let Some(info) = info {
        for stream in info
            .streams
            .iter()
            .filter(|stream| stream_kind(stream) == Some("subtitle"))
        {
            let index = stream_index(stream)?;
            if deleted_streams.contains(&index) {
                continue;
            }
            let source = SubtitleSource::Embedded(index);
            let language =
                subtitle_metadata_for_plan(subtitle_changes, Some(info), sidecars, &source)
                    .map(|metadata| metadata.language)
                    .unwrap_or_else(|| stream_language(stream));
            if language_choice(&language).is_none() {
                return Some(format!(
                    "Choose a language for subtitle track #{index}; Undetermined is not allowed."
                ));
            }
        }
    }
    for sidecar in sidecars {
        let source = SubtitleSource::Sidecar(sidecar.path.clone());
        let language = subtitle_metadata_for_plan(subtitle_changes, info, sidecars, &source)
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

/// Free-function core of `App::container_conflicts_for`, taking explicit edit-state
/// parameters instead of implicit `self.*` fields so it can be run against a staged
/// (not currently open) file's `StagedEdit` data — see `App::staged_file_status`.
#[allow(clippy::too_many_arguments)]
fn container_conflicts_for_plan(
    subtitle_capabilities: &ToolCapabilities,
    info: Option<&MediaInfo>,
    stream_order: &[u64],
    deleted_streams: &BTreeSet<u64>,
    audio_settings: &BTreeMap<u64, AudioSettings>,
    video_settings: &BTreeMap<u64, VideoSettings>,
    subtitle_changes: &BTreeMap<SubtitleSource, SubtitleChange>,
    sidecars: &[SidecarEntry],
    target: ContainerFormat,
) -> Vec<String> {
    let mut conflicts = Vec::new();
    if !subtitle_capabilities.ffmpeg {
        conflicts.push("FFmpeg is not available in PATH.".to_string());
    } else if !subtitle_capabilities.ffmpeg_muxers.contains(target.muxer()) {
        conflicts.push(format!(
            "The installed FFmpeg build does not provide the {} muxer.",
            target.label()
        ));
    }
    let Some(info) = info else {
        return conflicts;
    };
    let order = final_stream_order(info, stream_order, deleted_streams);
    let changes = subtitle_changes.values().cloned().collect::<Vec<_>>();
    conflicts.extend(container_conflicts(
        info,
        &order,
        audio_settings,
        video_settings,
        &changes,
        target,
    ));
    conflicts.extend(imported_subtitle_conflicts(&changes, sidecars, target));
    conflicts.extend(subtitle_metadata_conflicts(
        info, &changes, sidecars, target, true,
    ));
    conflicts
}

/// Free-function core of `App::selected_container_conflicts` — see
/// `container_conflicts_for_plan`.
#[allow(clippy::too_many_arguments)]
fn staged_container_conflicts(
    subtitle_capabilities: &ToolCapabilities,
    info: Option<&MediaInfo>,
    source_container: Option<ContainerFormat>,
    container_target: Option<ContainerFormat>,
    stream_order: &[u64],
    deleted_streams: &BTreeSet<u64>,
    audio_settings: &BTreeMap<u64, AudioSettings>,
    video_settings: &BTreeMap<u64, VideoSettings>,
    subtitle_changes: &BTreeMap<SubtitleSource, SubtitleChange>,
    sidecars: &[SidecarEntry],
) -> Vec<String> {
    if let Some(target) = container_target {
        return container_conflicts_for_plan(
            subtitle_capabilities,
            info,
            stream_order,
            deleted_streams,
            audio_settings,
            video_settings,
            subtitle_changes,
            sidecars,
            target,
        );
    }
    let Some(target) = source_container else {
        return if subtitle_changes
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
    let changes = subtitle_changes.values().cloned().collect::<Vec<_>>();
    // No container change is staged, so only the tracks this edit re-encodes can newly
    // fail to fit the container the file is already in — picking Opus for an audio track
    // No container change is staged, so most streams are about to be copied through
    // byte-for-byte and need no opinion from us. Two kinds still do:
    //
    //   * tracks this edit re-encodes, whose new codec is the user's choice and may not
    //     fit the container the file is already in — Vorbis for an audio track in an MP4;
    //   * streams Reel offers no control over at all (`data`, `attachment`), which it
    //     cannot drop or convert on the user's behalf, so an unwritable one has to be
    //     reported here rather than surfacing as a raw ffmpeg "codec not currently
    //     supported in container" once the batch is already running.
    //
    // Everything else is deliberately left alone. `supports_codec` is a conservative
    // shortlist, not a full account of what these containers accept, so blocking an
    // untouched audio or subtitle track on it refuses edits that would have worked: an
    // MKV holding `dvb_subtitle` could not be reordered or have a default flag flipped.
    let checked = info
        .map(|info| final_stream_order(info, stream_order, deleted_streams))
        .unwrap_or_default()
        .into_iter()
        .filter(|index| {
            audio_settings.contains_key(index)
                || video_settings.contains_key(index)
                || info
                    .and_then(|info| stream_by_index(info, *index))
                    .is_some_and(|stream| {
                        !matches!(stream_kind(stream), Some("video" | "audio" | "subtitle"))
                    })
        })
        .collect::<Vec<_>>();
    let mut conflicts = info
        .map(|info| {
            container_conflicts(info, &checked, audio_settings, video_settings, &[], target)
        })
        .unwrap_or_default();
    conflicts.extend(imported_subtitle_conflicts(&changes, sidecars, target));
    if let Some(info) = info {
        conflicts.extend(subtitle_metadata_conflicts(
            info, &changes, sidecars, target, false,
        ));
    }
    conflicts
}

/// The single gate a set of staged track/container/subtitle edits must pass against
/// `info` (a probe of the file's *current* content) — shared by `staged_file_status`'s
/// two branches (staged-but-not-open, and the currently-open file's live fields), so
/// they can't drift into disagreeing about what "still valid" means. Takes
/// `subtitle_capabilities` explicitly rather than being a method so it can run while a
/// `&mut StagedEdit` borrow is held nearby.
///
/// Flattens both failure modes into one message, because a caller asking "can this be
/// processed?" treats them alike. `App::receive_conflict_probe_results` is the one
/// place that must tell them apart — a structural group conflict raises the
/// unskippable notice, anything else is an ordinary `Invalid` — so it calls
/// `reconcile_untouched_groups` itself rather than going through here.
#[allow(clippy::too_many_arguments)]
fn validate_staged_edit(
    subtitle_capabilities: &ToolCapabilities,
    path: &Path,
    info: &MediaInfo,
    sidecars: &[SidecarEntry],
    stream_order: &[u64],
    deleted_streams: &BTreeSet<u64>,
    default_streams: &BTreeSet<u64>,
    original_stream_order: &[u64],
    original_default_streams: &BTreeSet<u64>,
    track_groups: &BTreeMap<u64, &'static str>,
    moved_streams: &BTreeSet<u64>,
    audio_settings: &BTreeMap<u64, AudioSettings>,
    video_settings: &BTreeMap<u64, VideoSettings>,
    subtitle_changes: &BTreeMap<SubtitleSource, SubtitleChange>,
    container_target: Option<ContainerFormat>,
) -> Result<ReconciledTracks, String> {
    let reconciled = reconcile_untouched_groups(
        info,
        stream_order,
        deleted_streams,
        default_streams,
        original_stream_order,
        original_default_streams,
        track_groups,
        moved_streams,
        audio_settings,
        video_settings,
        subtitle_changes,
        &BTreeSet::new(),
    );
    if !reconciled.conflicting_groups.is_empty() {
        return Err(format!(
            "The file's {} changed on disk since these edits were staged.",
            describe_track_groups(&reconciled.conflicting_groups)
        ));
    }
    let order = final_stream_order(info, &reconciled.stream_order, &reconciled.deleted_streams);
    let defaults = reconciled
        .default_streams
        .difference(&reconciled.deleted_streams)
        .copied()
        .collect();
    validate_edit(
        info,
        &order,
        &reconciled.deleted_streams,
        &defaults,
        audio_settings,
        video_settings,
    )?;
    for (index, settings) in audio_settings {
        let stream = stream_by_index(info, *index)
            .expect("validated audio settings refer to an existing stream");
        if !audio_requires_transcode(stream, settings) {
            continue;
        }
        let codec = effective_audio_codec(stream, settings)
            .expect("validate_edit accepts only encodable audio codecs");
        let encoder = codec
            .encoder()
            .expect("audio target codecs always provide an encoder");
        if !subtitle_capabilities.ffmpeg_encoders.contains(encoder) {
            return Err(format!(
                "The installed FFmpeg build does not provide the {} encoder.",
                codec.label()
            ));
        }
    }
    if let Some(error) = subtitle_language_error_for(
        Some(info),
        &reconciled.deleted_streams,
        subtitle_changes,
        sidecars,
    ) {
        return Err(error);
    }
    // `ContainerFormat::detect` cross-checks the extension against ffprobe's own
    // `format_name` rather than trusting a possibly misleading extension, matching
    // `App::source_container`'s logic for the currently-open file.
    let format_name = info
        .format
        .get("format_name")
        .and_then(serde_json::Value::as_str);
    let source_container = ContainerFormat::detect(path, format_name);
    let conflicts = staged_container_conflicts(
        subtitle_capabilities,
        Some(info),
        source_container,
        container_target,
        &reconciled.stream_order,
        &reconciled.deleted_streams,
        audio_settings,
        video_settings,
        subtitle_changes,
        sidecars,
    );
    if !conflicts.is_empty() {
        return Err(conflicts.join("\n"));
    }
    Ok(reconciled)
}

/// The `stream_order`/`deleted_streams`/`default_streams`/`original_stream_order`/
/// `original_default_streams`/`track_groups` fields `reconcile_untouched_groups`
/// produces — either carried forward unchanged (a touched group) or rebuilt fresh
/// from `info` (an untouched one). See `App::validate_staged_edit`.
struct ReconciledTracks {
    stream_order: Vec<u64>,
    deleted_streams: BTreeSet<u64>,
    default_streams: BTreeSet<u64>,
    original_stream_order: Vec<u64>,
    original_default_streams: BTreeSet<u64>,
    track_groups: BTreeMap<u64, &'static str>,
    /// Groups the staged edit touches whose tracks are no longer the ones it was
    /// staged against. Their staged state is carried forward here unchanged — the
    /// only resolution is `App::revert_group_edits`, which re-runs this function with
    /// them in `force_untouched`. Empty for every other outcome, including a staged
    /// edit that reconciles cleanly but then fails an ordinary save gate.
    conflicting_groups: BTreeSet<&'static str>,
}

/// Distinguishes "a change to a track type the staged edit never touched" (silently
/// absorbed) from "a change to a track type the staged edit actually edits" (reported
/// in `ReconciledTracks::conflicting_groups`), rather than treating every added or
/// removed track as an equally hard conflict. A group counts as touched when the
/// staged edit moved, deleted, applied video settings to, or toggled the default flag
/// on one of its tracks, or (for "subtitle") staged any subtitle change at all —
/// sidecar subtitle changes don't correspond to an embedded index, so they can't be
/// pinned to one via `track_groups`.
///
/// Requires `track_groups` — the type-per-index snapshot taken when editing began
/// (see `StagedEdit::track_groups`) — to know which group an index that has since
/// vanished from the file used to belong to. When that snapshot is empty (edits
/// staged before this field existed, or bare test fixtures), there's no way to tell
/// touched from untouched, so every group is conservatively treated as touched — the
/// original, all-or-nothing completeness check.
///
/// `force_untouched` names groups to push down the untouched path regardless of what
/// the edit stages for them, discarding their staged state in favour of the file's
/// current tracks. That's what makes `App::revert_group_edits` a reuse of this
/// function rather than a second, parallel implementation of "rebuild this group from
/// disk"; every other caller passes an empty set.
#[allow(clippy::too_many_arguments)]
fn reconcile_untouched_groups(
    info: &MediaInfo,
    stream_order: &[u64],
    deleted_streams: &BTreeSet<u64>,
    default_streams: &BTreeSet<u64>,
    original_stream_order: &[u64],
    original_default_streams: &BTreeSet<u64>,
    track_groups: &BTreeMap<u64, &'static str>,
    moved_streams: &BTreeSet<u64>,
    audio_settings: &BTreeMap<u64, AudioSettings>,
    video_settings: &BTreeMap<u64, VideoSettings>,
    subtitle_changes: &BTreeMap<SubtitleSource, SubtitleChange>,
    force_untouched: &BTreeSet<&'static str>,
) -> ReconciledTracks {
    if track_groups.is_empty() {
        return ReconciledTracks {
            stream_order: stream_order.to_vec(),
            deleted_streams: deleted_streams.clone(),
            default_streams: default_streams.clone(),
            original_stream_order: original_stream_order.to_vec(),
            original_default_streams: original_default_streams.clone(),
            track_groups: track_groups.clone(),
            conflicting_groups: BTreeSet::new(),
        };
    }

    let touched_indices: BTreeSet<u64> = moved_streams
        .iter()
        .chain(deleted_streams.iter())
        .chain(audio_settings.keys())
        .chain(video_settings.keys())
        .chain(default_streams.symmetric_difference(original_default_streams))
        .copied()
        .collect();
    let mut touched_groups: BTreeSet<&'static str> = touched_indices
        .iter()
        .filter_map(|index| track_groups.get(index).copied())
        .collect();
    if !subtitle_changes.is_empty() {
        touched_groups.insert("subtitle");
    }

    let mut reconciled = ReconciledTracks {
        stream_order: Vec::new(),
        deleted_streams: BTreeSet::new(),
        default_streams: BTreeSet::new(),
        original_stream_order: Vec::new(),
        original_default_streams: BTreeSet::new(),
        track_groups: BTreeMap::new(),
        conflicting_groups: BTreeSet::new(),
    };
    for group in ["video", "audio", "subtitle", "other"] {
        let old_set: BTreeSet<u64> = track_groups
            .iter()
            .filter(|(_, candidate)| **candidate == group)
            .map(|(index, _)| *index)
            .collect();
        let new_set: BTreeSet<u64> = info
            .streams
            .iter()
            .filter(|stream| stream_group(stream) == group)
            .filter_map(stream_index)
            .collect();

        if touched_groups.contains(group) && !force_untouched.contains(group) {
            if old_set != new_set {
                // Carried forward as staged rather than rebuilt: until the user
                // acknowledges the notice, the staged work is still exactly what they
                // left, and `validate_staged_edit` refuses it on the strength of this
                // set alone.
                reconciled.conflicting_groups.insert(group);
            }
            reconciled
                .stream_order
                .extend(stream_order.iter().filter(|index| old_set.contains(index)));
            reconciled.deleted_streams.extend(
                deleted_streams
                    .iter()
                    .filter(|index| old_set.contains(index)),
            );
            reconciled.default_streams.extend(
                default_streams
                    .iter()
                    .filter(|index| old_set.contains(index)),
            );
            reconciled.original_stream_order.extend(
                original_stream_order
                    .iter()
                    .filter(|index| old_set.contains(index)),
            );
            reconciled.original_default_streams.extend(
                original_default_streams
                    .iter()
                    .filter(|index| old_set.contains(index)),
            );
            for index in old_set {
                reconciled.track_groups.insert(index, group);
            }
        } else {
            // Untouched (or reverted via `force_untouched`): forget whatever was
            // staged for this group and rebuild it fresh from the file's current
            // content, exactly as opening the file anew would for just this group's
            // tracks — vanished ones simply drop out, new ones join at their natural
            // position, kept, with whatever default flag the file itself currently
            // gives them.
            for index in new_set {
                reconciled.stream_order.push(index);
                reconciled.original_stream_order.push(index);
                reconciled.track_groups.insert(index, group);
                if stream_by_index(info, index).is_some_and(is_default) {
                    reconciled.default_streams.insert(index);
                    reconciled.original_default_streams.insert(index);
                }
            }
        }
    }
    reconciled
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

/// Summarizes track moves/deletes/default-changes between a staged edit's baseline
/// and its current state, grouped by track type — used by `staged_edit_summary` as
/// the fallback once container/video/subtitle changes have been described.
fn edit_summary(
    info: &MediaInfo,
    original_order: &[u64],
    staged_order: &[u64],
    moved_streams: &BTreeSet<u64>,
    deleted: &BTreeSet<u64>,
    original_defaults: &BTreeSet<u64>,
    staged_defaults: &BTreeSet<u64>,
) -> Vec<(&'static str, String)> {
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
            lines.push((
                group,
                format!("Moving {moved} {}", track_count_label(group, moved)),
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
            lines.push((
                group,
                format!("Deleting {count} {}", track_count_label(group, count)),
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
            lines.push((kind, format!("Changing the default {kind} track")));
        }
    }
    lines
}

fn track_count_label(group: &str, count: usize) -> String {
    format!("{group} track{}", if count == 1 { "" } else { "s" })
}

/// "audio tracks", "video and audio tracks", "video, audio and subtitle tracks" — one
/// trailing noun for the whole list, since every element is a track type. Used both in
/// `validate_staged_edit`'s message and in the conflict notice's per-file heading.
pub(crate) fn describe_track_groups(groups: &BTreeSet<&'static str>) -> String {
    let names: Vec<&str> = groups.iter().copied().collect();
    let joined = match names.split_last() {
        None => return "tracks".to_string(),
        Some((last, [])) => (*last).to_string(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
    };
    format!("{joined} tracks")
}

/// Builds the human-readable list of changes staged for one file — container
/// conversion/metadata, video re-encodes, subtitle import/export/metadata edits, and
/// track moves/deletes/default changes — shown in the `ConfirmProcessAll` dialog
/// before Ctrl+S actually dispatches the batch. Mirrors the old single-file
/// `save_summary`, but reads from a given `StagedEdit` and that file's cached
/// `MediaInfo` instead of the live `App` fields, since any staged file (not just the
/// one currently open) needs to be summarizable.
fn staged_edit_summary(path: &Path, info: &MediaInfo, staged: &StagedEdit) -> Vec<String> {
    staged_edit_summary_entries(path, info, staged)
        .into_iter()
        .map(|(_, line)| line)
        .collect()
}

/// Every staged change tagged with the `stream_group` label it belongs to (or
/// `"container"` for container-level ones), so callers can either drop the tag
/// (`staged_edit_summary`) or partition on it (`App::split_change_summary`).
fn staged_edit_summary_entries(
    path: &Path,
    info: &MediaInfo,
    staged: &StagedEdit,
) -> Vec<(&'static str, String)> {
    let mut lines = edit_summary(
        info,
        &staged.original_stream_order,
        &staged.stream_order,
        &staged.moved_streams,
        &staged.deleted_streams,
        &staged.original_default_streams,
        &staged.default_streams,
    );
    if let Some(target) = staged.container_target {
        let format_name = info
            .format
            .get("format_name")
            .and_then(serde_json::Value::as_str);
        let source = ContainerFormat::detect(path, format_name)
            .map(ContainerFormat::label)
            .unwrap_or("original");
        lines.insert(
            0,
            (
                "container",
                format!("Changing container from {source} to {}", target.label()),
            ),
        );
    }
    let original_metadata = ContainerMetadata {
        title: info.container_title(),
        comment: info.container_comment(),
        date: info.container_date(),
        genre: info.container_genre(),
        artist: info.container_artist(),
    };
    let effective_metadata = staged
        .container_metadata
        .clone()
        .unwrap_or_else(|| original_metadata.clone());
    if original_metadata != effective_metadata {
        let fields = [
            ("title", &original_metadata.title, &effective_metadata.title),
            (
                "comment",
                &original_metadata.comment,
                &effective_metadata.comment,
            ),
            ("date", &original_metadata.date, &effective_metadata.date),
            ("genre", &original_metadata.genre, &effective_metadata.genre),
            (
                "artist",
                &original_metadata.artist,
                &effective_metadata.artist,
            ),
        ];
        let changed: Vec<&str> = fields
            .iter()
            .filter(|(_, o, e)| o != e)
            .map(|(name, _, _)| *name)
            .collect();
        if !changed.is_empty() {
            lines.push((
                "container",
                format!("Updating container metadata: {}", changed.join(", ")),
            ));
        }
    }
    for (index, settings) in &staged.audio_settings {
        let source = stream_by_index(info, *index);
        let target_codec = source.and_then(|stream| effective_audio_codec(stream, settings));
        let channels = settings
            .channel_layout
            .channels()
            .or_else(|| source.and_then(stream_channels));
        let mut details = vec![
            target_codec
                .map(AudioCodec::label)
                .unwrap_or("original codec")
                .to_string(),
        ];
        if settings.channel_layout != AudioChannelLayout::Original {
            details.push(settings.channel_layout.label().to_string());
        }
        if let Some(rate) = target_codec
            .filter(|codec| !codec.is_lossless())
            .zip(channels)
            .and_then(|(codec, channels)| audio_bitrate_kbps(codec, channels))
        {
            details.push(format!("{rate} kbps"));
        }
        let technical_change = settings.codec != AudioCodec::Original
            || settings.channel_layout != AudioChannelLayout::Original;
        if technical_change {
            lines.push((
                "audio",
                format!("Encoding audio track #{index} as {}", details.join(" · ")),
            ));
        }
        let original = source.map(|stream| AudioMetadata {
            language: stream_language(stream),
            title: audio_stream_title(stream),
            commentary: stream_commentary(stream),
            hearing_impaired: stream_hearing_impaired(stream),
            audio_description: stream_disposition(stream, "visual_impaired"),
            original: stream_original(stream),
            dubbed: stream_disposition(stream, "dub"),
        });
        if original.as_ref() != Some(&settings.metadata) {
            lines.push(("audio", format!("Updating audio track #{index} metadata")));
        }
    }
    for (index, settings) in &staged.video_settings {
        let codec = match settings.codec {
            VideoCodec::Original => stream_by_index(info, *index)
                .and_then(|stream| stream.get("codec_name"))
                .and_then(serde_json::Value::as_str)
                .map(|codec| codec.to_uppercase())
                .unwrap_or_else(|| "original codec".to_string()),
            codec => codec.label().to_string(),
        };
        let resolution = settings.resolution.label();
        // Grouped by where the *staged* edit put this index rather than by what the
        // stream is now: for the conflict notice these entries have to line up with
        // `StagedEdit::track_groups`, which is what `App::revert_group_edits` prunes
        // by. `validate_edit` guarantees video settings only ever land on video
        // tracks, so the fallback is only reached for an index the snapshot predates.
        let group = staged.track_groups.get(index).copied().unwrap_or("video");
        lines.push((
            group,
            match settings.resolution {
                VideoResolution::Original => {
                    format!("Encoding video track #{index} as {codec}")
                }
                _ => format!("Encoding video track #{index} as {codec} at {resolution}"),
            },
        ));
    }
    for change in staged.subtitle_changes.values() {
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
            lines.push((
                "subtitle",
                format!("Importing {source} as {}", target.label()),
            ));
            continue;
        }
        if let Some(target) = change
            .embedded_target
            .filter(|_| change.export_target.is_none())
        {
            lines.push((
                "subtitle",
                match change.source {
                    SubtitleSource::Embedded(_) => {
                        format!("Converting {source} in the media to {}", target.label())
                    }
                    SubtitleSource::Sidecar(_) => {
                        format!("Converting {source} to {}", target.label())
                    }
                },
            ));
        }
        if let Some(target) = change.export_target {
            lines.push((
                "subtitle",
                format!("Exporting {source} as {}", target.label()),
            ));
        }
        if let Some(metadata) = &change.metadata {
            let mut values = vec![format!("language {}", metadata.language.to_uppercase())];
            values.push(match &metadata.title {
                Some(title) => format!("title \u{201c}{title}\u{201d}"),
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
            lines.push((
                "subtitle",
                format!("Updating {source} metadata: {}", values.join(", ")),
            ));
        }
    }
    lines
}

/// Whether a `StagedEdit` still stages anything at all — the non-live mirror of
/// `App::has_track_edits`, used by `App::revert_group_edits` to drop an entry whose
/// only content was just reverted rather than leaving a file marked as staged with
/// nothing staged in it.
fn staged_edit_stages_anything(staged: &StagedEdit) -> bool {
    staged.container_target.is_some()
        || staged.container_metadata.is_some()
        || !staged.deleted_streams.is_empty()
        || !staged.default_sidecars.is_empty()
        || !staged.audio_settings.is_empty()
        || !staged.video_settings.is_empty()
        || staged.stream_order != staged.original_stream_order
        || staged.default_streams != staged.original_default_streams
        || staged
            .subtitle_changes
            .values()
            .any(SubtitleChange::has_effect)
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
        let (conflict_tx, _) = std::sync::mpsc::channel::<ProbeRequest>();
        let (edit_tx, _) = std::sync::mpsc::channel::<EditRequest>();
        let mut app = App::new(directory, probe_tx, conflict_tx, edit_tx.clone(), edit_tx).unwrap();
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
        let (conflict_tx, _) = std::sync::mpsc::channel::<ProbeRequest>();
        let (edit_tx, _) = std::sync::mpsc::channel::<EditRequest>();
        let mut app = App::new(directory, probe_tx, conflict_tx, edit_tx.clone(), edit_tx).unwrap();
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

    /// Every entry point here is reachable from a key `main` still delivers to `App`
    /// while a dialog is up, so each has to refuse for itself. A missing guard means a
    /// keystroke rewrites the edit behind a dialog the user is reading, with nothing on
    /// screen to show it happened.
    #[test]
    fn an_open_dialog_should_make_the_track_editing_entry_points_inert() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        let path = app.selected_file().unwrap().path.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "audio", "codec_name": "aac"},
                {"index": 2, "codec_type": "subtitle", "codec_name": "subrip",
                 "tags": {"language": "eng"}},
            ]),
        );
        app.sidecars = vec![test_sidecar(&app, "movie.dan.srt", "dan")];
        let order = app.stream_order.clone();
        app.dialog = Some(Dialog::Keybindings);

        // Act
        app.toggle_delete_selected_stream();
        app.open_container_settings();
        app.open_video_settings();
        app.open_stream_details();
        app.move_selected_stream(1);
        app.move_selected_stream(-1);
        let transferred = app.transfer_subtitle(1);
        let moved_column = app.move_subtitle_column(1);
        app.request_process_all();
        app.request_reset_file(path);
        app.request_reset_current_file();
        app.request_reset_all_files();

        // Assert: the dialog is still the only thing on screen and nothing behind it
        // moved.
        assert_that!(app.dialog).contains(Dialog::Keybindings);
        assert_that!(app.stream_order.clone()).is_equal_to(order);
        assert_that!(app.deleted_streams.clone()).is_equal_to(BTreeSet::new());
        assert_that!(app.container_settings_popup.is_none()).is_true();
        assert_that!(app.video_settings_popup.is_none()).is_true();
        assert_that!(app.subtitle_settings_popup.is_none()).is_true();
        assert_that!(app.pending_reset().is_none()).is_true();
        assert_that!(transferred).is_false();
        assert_that!(moved_column).is_false();
        assert_that!(app.notice.clone()).is_none();

        std::fs::remove_dir_all(&app.directory).unwrap();
    }

    /// The conflict and cancel prompts are driven by keys that stay live for other
    /// dialogs too, so each checks that *its own* dialog is the one on screen.
    #[test]
    fn a_dialog_specific_action_should_do_nothing_while_a_different_dialog_is_up() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        app.set_conflict_max_scroll(5);
        app.dialog = Some(Dialog::Keybindings);

        // Act
        app.scroll_conflicts(1);
        app.acknowledge_conflicts();
        app.request_cancel_edit();
        app.choose_cancel_edit(1);
        // Zero is "no direction" and must not move the choice even on the right dialog.
        app.dialog = Some(Dialog::ConfirmCancel);
        app.cancel_edit_choice = CancelEditChoice::KeepProcessing;
        app.choose_cancel_edit(0);

        // Assert
        assert_that!(app.conflict_scroll).is_equal_to(0);
        assert_that!(app.cancel_edit_choice).is_equal_to(CancelEditChoice::KeepProcessing);

        std::fs::remove_dir_all(&app.directory).unwrap();
    }

    /// Container metadata is staged only when it actually differs from the file's own
    /// tags. Typing a value back to what it already was — or clearing it to nothing —
    /// has to leave the file unstaged, or the save dialog offers work with no effect.
    #[test]
    fn container_metadata_should_stage_only_a_value_that_differs_from_the_file() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        app.outcome = Some(ProbeOutcome::Video(
            MediaInfo::from_json(serde_json::json!({
                "format": {"tags": {"title": "Original title"}},
                "streams": [{"index": 0, "codec_type": "video", "codec_name": "h264"}],
            }))
            .unwrap(),
        ));
        app.loading = false;
        app.reset_track_edits();
        app.layer = Layer::Streams;
        focus_track(&mut app, TrackRef::Container);
        app.open_container_settings();
        app.move_container_settings_cursor(1);

        // Act / Assert: a new title stages.
        app.start_container_text_input();
        app.clear_text_to_start();
        app.paste_text(" Different title ");
        app.save_container_text_input();
        assert_that!(
            app.container_metadata
                .as_ref()
                .and_then(|metadata| metadata.title.clone())
        )
        .contains("Different title".to_string());

        // Act / Assert: typing the file's own title back unstages it entirely.
        app.start_container_text_input();
        app.clear_text_to_start();
        app.paste_text("Original title");
        app.save_container_text_input();
        assert_that!(app.container_metadata.is_none()).is_true();

        // Act / Assert: clearing the field stages an explicit removal.
        app.start_container_text_input();
        app.clear_text_to_start();
        app.save_container_text_input();
        assert_that!(app.container_metadata.is_some()).is_true();
        assert_that!(
            app.container_metadata
                .as_ref()
                .and_then(|metadata| metadata.title.clone())
        )
        .is_none();

        // Act / Assert: saving again outside edit mode changes nothing.
        let staged = app.container_metadata.clone();
        app.save_container_text_input();
        assert_that!(app.container_metadata.clone()).is_equal_to(staged);

        std::fs::remove_dir_all(&app.directory).unwrap();
    }

    /// `G` means "the end of what I'm looking at". With the columns side by side that
    /// is the end of the focused column, not the last row on screen.
    #[test]
    fn going_to_the_end_should_stop_at_the_end_of_the_focused_subtitle_column() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "subtitle", "codec_name": "subrip",
                 "tags": {"language": "eng"}},
                {"index": 2, "codec_type": "subtitle", "codec_name": "subrip",
                 "tags": {"language": "dan"}},
            ]),
        );
        app.sidecars = vec![test_sidecar(&app, "movie.nld.srt", "nld")];
        app.left_subtitle_order = app.active_left_subtitle_tracks();
        app.set_subtitle_columns_side_by_side(true);

        // Act / Assert: from an embedded subtitle, the last embedded one.
        focus_track(&mut app, TrackRef::Embedded(1));
        app.select_last();
        assert_that!(app.selected_track()).contains(TrackRef::Embedded(2));

        // Act / Assert: from the sidecar column, its own last row.
        focus_track(&mut app, TrackRef::Sidecar(0));
        app.select_last();
        assert_that!(app.selected_track()).contains(TrackRef::Sidecar(0));

        // Act / Assert: from a non-subtitle row the columns do not apply, so it is the
        // last row of the whole list.
        focus_track(&mut app, TrackRef::Container);
        app.select_first();
        assert_that!(app.selected_stream).is_equal_to(0);

        std::fs::remove_dir_all(&app.directory).unwrap();
    }

    /// Every list navigation key is live on an empty directory too — a filter that
    /// matches nothing is the ordinary way to get there — so each has to survive a list
    /// with no rows rather than indexing into it.
    #[test]
    fn navigating_an_empty_file_list_should_do_nothing_rather_than_panic() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        app.files.clear();
        app.list_state.select(None);
        app.layer = Layer::Files;

        // Act
        app.select_next();
        app.select_previous();
        app.select_first();
        app.select_last();

        // Assert
        assert_that!(app.selected_file().is_none()).is_true();

        std::fs::remove_dir_all(&app.directory).unwrap();
    }

    /// h/l jump the cursor between the two subtitle columns, and which column a track
    /// is in follows its staged import/export — not where it came from. Pushing toward
    /// the column a track is already in has nowhere to go and must refuse.
    #[test]
    fn jumping_between_subtitle_columns_should_follow_where_each_track_currently_sits() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "subtitle", "codec_name": "subrip",
                 "tags": {"language": "eng"}},
            ]),
        );
        app.subtitle_capabilities = full_subtitle_capabilities();
        app.sidecars = vec![
            test_sidecar(&app, "movie.nld.srt", "nld"),
            test_sidecar(&app, "movie.fra.srt", "fra"),
        ];
        app.left_subtitle_order = app.active_left_subtitle_tracks();
        app.set_subtitle_columns_side_by_side(true);

        // Arrange: stage an import so one sidecar sits in the left column.
        focus_track(&mut app, TrackRef::Sidecar(0));
        assert_that!(app.transfer_subtitle(-1)).is_true();
        app.left_subtitle_order = app.active_left_subtitle_tracks();

        // Act / Assert: the imported sidecar is a left-column track now, so only a
        // rightward jump has anywhere to land.
        focus_track(&mut app, TrackRef::Sidecar(0));
        assert_that!(app.move_subtitle_column(-1)).is_false();
        assert_that!(app.move_subtitle_column(1)).is_true();
        assert_that!(app.selected_track()).contains(TrackRef::Sidecar(1));

        // Act / Assert: and the right-column sidecar only jumps left.
        assert_that!(app.move_subtitle_column(1)).is_false();
        assert_that!(app.move_subtitle_column(-1)).is_true();

        // Arrange: export the embedded track, putting it in the right column.
        focus_track(&mut app, TrackRef::Embedded(1));
        assert_that!(app.transfer_subtitle(1)).is_true();
        app.left_subtitle_order = app.active_left_subtitle_tracks();

        // Act / Assert
        focus_track(&mut app, TrackRef::Embedded(1));
        assert_that!(app.move_subtitle_column(1)).is_false();
        assert_that!(app.move_subtitle_column(-1)).is_true();
        assert_that!(app.selected_track()).contains(TrackRef::Sidecar(0));

        std::fs::remove_dir_all(&app.directory).unwrap();
    }

    /// Ctrl-j/Ctrl-k reorder a track *within its own kind* — video with video, audio
    /// with audio. A move that would cross a boundary, run off the end, or shuffle a
    /// track already marked for deletion has to leave the order exactly as it was.
    #[test]
    fn reordering_a_track_should_stop_at_its_own_group_and_at_the_ends_of_the_list() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "audio", "codec_name": "aac"},
                {"index": 2, "codec_type": "audio", "codec_name": "ac3"},
            ]),
        );
        let original = app.stream_order.clone();

        // Act / Assert: the lone video cannot move down into the audio group.
        focus_track(&mut app, TrackRef::Embedded(0));
        app.move_selected_stream(1);
        assert_that!(app.stream_order.clone()).is_equal_to(original.clone());
        assert_that!(app.moved_streams.clone()).is_equal_to(BTreeSet::new());

        // Act / Assert: nor can the last audio track move past the end of the list.
        focus_track(&mut app, TrackRef::Embedded(2));
        app.move_selected_stream(1);
        assert_that!(app.stream_order.clone()).is_equal_to(original.clone());

        // Act / Assert: nor can the first track move above position zero.
        focus_track(&mut app, TrackRef::Embedded(0));
        app.move_selected_stream(-1);
        assert_that!(app.stream_order.clone()).is_equal_to(original.clone());

        // Act / Assert: a track on its way out is not reordered either.
        focus_track(&mut app, TrackRef::Embedded(1));
        app.toggle_delete_selected_stream();
        focus_track(&mut app, TrackRef::Embedded(1));
        app.move_selected_stream(1);
        assert_that!(app.stream_order.clone()).is_equal_to(original.clone());
        assert_that!(app.notice.clone())
            .contains("Unmark this track for deletion before moving it.".to_string());

        // Act / Assert: within the audio group it does move, and says so.
        focus_track(&mut app, TrackRef::Embedded(1));
        app.toggle_delete_selected_stream();
        focus_track(&mut app, TrackRef::Embedded(1));
        app.move_selected_stream(1);
        assert_that!(app.stream_order.clone()).is_equal_to(vec![0, 2, 1]);
        // Only the track the user actually moved is marked, not the one it swapped with.
        assert_that!(app.moved_streams.clone()).is_equal_to(BTreeSet::from([1_u64]));
        assert_that!(app.notice.clone()).is_none();

        // Act / Assert: moving it back clears the "moved" marks entirely.
        focus_track(&mut app, TrackRef::Embedded(1));
        app.move_selected_stream(-1);
        assert_that!(app.stream_order.clone()).is_equal_to(original);
        assert_that!(app.moved_streams.clone()).is_equal_to(BTreeSet::new());

        std::fs::remove_dir_all(&app.directory).unwrap();
    }

    /// With the columns side by side, j/k walk one column at a time; running off either
    /// end has to land back on the single-column rows above and below, not stick.
    #[test]
    fn walking_off_a_subtitle_column_should_leave_it_for_the_rows_around_it() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "audio", "codec_name": "aac"},
                {"index": 2, "codec_type": "subtitle", "codec_name": "subrip",
                 "tags": {"language": "eng"}},
                {"index": 3, "codec_type": "subtitle", "codec_name": "subrip",
                 "tags": {"language": "dan"}},
            ]),
        );
        app.subtitle_capabilities = full_subtitle_capabilities();
        app.sidecars = vec![test_sidecar(&app, "movie.nld.srt", "nld")];
        app.left_subtitle_order = app.active_left_subtitle_tracks();
        app.set_subtitle_columns_side_by_side(true);

        // Act / Assert: down within the embedded column.
        focus_track(&mut app, TrackRef::Embedded(2));
        assert_that!(app.move_within_subtitle_column(1, 1, false)).is_true();
        assert_that!(app.selected_track()).contains(TrackRef::Embedded(3));

        // Act / Assert: up off the top of the column lands on the row above it.
        focus_track(&mut app, TrackRef::Embedded(2));
        assert_that!(app.move_within_subtitle_column(-1, 1, false)).is_true();
        assert_that!(app.selected_track()).contains(TrackRef::Embedded(1));

        // Act / Assert: clamped, the same move stays put inside the column.
        focus_track(&mut app, TrackRef::Embedded(2));
        assert_that!(app.move_within_subtitle_column(-1, 1, true)).is_true();
        assert_that!(app.selected_track()).contains(TrackRef::Embedded(2));

        // Act / Assert: an exported track joins the external column, so its neighbour
        // is now the sidecar rather than the other embedded subtitle.
        focus_track(&mut app, TrackRef::Embedded(2));
        app.transfer_subtitle(1);
        let rows = app.track_rows();
        let exported_row = rows
            .iter()
            .position(|row| *row == TrackRef::Embedded(2))
            .unwrap();
        let sidecar_row = rows
            .iter()
            .position(|row| *row == TrackRef::Sidecar(0))
            .unwrap();
        let toward_sidecar = if sidecar_row > exported_row { 1 } else { -1 };
        focus_track(&mut app, TrackRef::Embedded(2));
        assert_that!(app.move_within_subtitle_column(toward_sidecar, 1, true)).is_true();
        assert_that!(app.selected_track()).contains(TrackRef::Sidecar(0));

        std::fs::remove_dir_all(&app.directory).unwrap();
    }

    /// Nothing follows the subtitles, so walking down off the last one has nowhere to
    /// go and must leave the selection where it is instead of running past the list.
    #[test]
    fn walking_down_off_the_last_subtitle_should_stay_when_nothing_follows_it() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "subtitle", "codec_name": "subrip",
                 "tags": {"language": "eng"}},
            ]),
        );
        app.set_subtitle_columns_side_by_side(true);
        focus_track(&mut app, TrackRef::Embedded(1));
        let before = app.selected_stream;

        // Act
        let moved = app.move_within_subtitle_column(1, 1, false);

        // Assert
        assert_that!(moved).is_true();
        assert_that!(app.selected_stream).is_equal_to(before);

        std::fs::remove_dir_all(&app.directory).unwrap();
    }

    fn full_subtitle_capabilities() -> ToolCapabilities {
        ToolCapabilities {
            ffmpeg: true,
            ffmpeg_encoders: BTreeSet::from([
                "aac".to_string(),
                "ac3".to_string(),
                "eac3".to_string(),
                "libopus".to_string(),
                "flac".to_string(),
                "alac".to_string(),
                "libmp3lame".to_string(),
                "libvorbis".to_string(),
                "subrip".to_string(),
                "ass".to_string(),
                "webvtt".to_string(),
                "ttml".to_string(),
                "mov_text".to_string(),
            ]),
            ffmpeg_muxers: BTreeSet::from(["matroska".to_string()]),
            seconv: true,
            tesseract_languages: vec!["eng".to_string()],
        }
    }

    /// Ctrl-h/Ctrl-l repeat while held, so the second press has to be a no-op rather
    /// than a second edit — and it still reports success, or the caller treats a
    /// track that is already where it was asked to go as a failed move.
    #[test]
    fn pushing_a_subtitle_to_the_side_it_is_already_on_should_change_nothing() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "subtitle", "codec_name": "subrip",
                 "tags": {"language": "eng"}},
            ]),
        );
        app.subtitle_capabilities = full_subtitle_capabilities();
        app.sidecars = vec![test_sidecar(&app, "movie.dan.srt", "dan")];
        app.left_subtitle_order = app.active_left_subtitle_tracks();

        // Act / Assert: exporting an embedded track, twice.
        focus_track(&mut app, TrackRef::Embedded(1));
        assert_that!(app.transfer_subtitle(1)).is_true();
        let exported = app.subtitle_changes.clone();
        assert_that!(app.transfer_subtitle(1)).is_true();
        assert_that!(app.subtitle_changes.clone()).is_equal_to(exported);

        // Act / Assert: pulling it back, twice.
        assert_that!(app.transfer_subtitle(-1)).is_true();
        let kept = app.subtitle_changes.clone();
        assert_that!(app.transfer_subtitle(-1)).is_true();
        assert_that!(app.subtitle_changes.clone()).is_equal_to(kept);

        // Act / Assert: importing a sidecar, twice.
        focus_track(&mut app, TrackRef::Sidecar(0));
        assert_that!(app.transfer_subtitle(-1)).is_true();
        let imported = app.subtitle_changes.clone();
        assert_that!(app.transfer_subtitle(-1)).is_true();
        assert_that!(app.subtitle_changes.clone()).is_equal_to(imported);

        // Act / Assert: sending it back out, twice.
        assert_that!(app.transfer_subtitle(1)).is_true();
        let external = app.subtitle_changes.clone();
        assert_that!(app.transfer_subtitle(1)).is_true();
        assert_that!(app.subtitle_changes.clone()).is_equal_to(external);

        std::fs::remove_dir_all(&app.directory).unwrap();
    }

    /// The container-only half of a sidecar's metadata (title, Original, Commentary)
    /// exists because the track is going *into* the file. Sending it back out has to
    /// drop it, or the sidecar keeps flags no `.srt` on disk can carry.
    #[test]
    fn sending_an_imported_sidecar_back_out_should_drop_what_only_a_container_can_hold() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        set_media(
            &mut app,
            serde_json::json!([{"index": 0, "codec_type": "video", "codec_name": "h264"}]),
        );
        app.subtitle_capabilities = full_subtitle_capabilities();
        app.sidecars = vec![test_sidecar(&app, "movie.dan.srt", "dan")];
        app.left_subtitle_order = app.active_left_subtitle_tracks();
        let source = SubtitleSource::Sidecar(app.sidecars[0].path.clone());
        focus_track(&mut app, TrackRef::Sidecar(0));
        app.transfer_subtitle(-1);
        app.default_sidecars.insert(0);
        let change = app.subtitle_changes.get_mut(&source).unwrap();
        change.metadata = Some(SubtitleMetadata {
            language: "dan".to_string(),
            title: Some("Commentary track".to_string()),
            forced: true,
            cc: false,
            hearing_impaired: true,
            original: true,
            commentary: true,
        });

        // Act
        assert_that!(app.transfer_subtitle(1)).is_true();

        // Assert: the sidecar-capable flags survive; the container-only ones do not.
        let metadata = app.subtitle_changes[&source].metadata.clone().unwrap();
        assert_that!(metadata.title).is_none();
        assert_that!(metadata.original).is_false();
        assert_that!(metadata.commentary).is_false();
        assert_that!(metadata.forced).is_true();
        assert_that!(metadata.hearing_impaired).is_true();
        assert_that!(app.default_sidecars.contains(&0)).is_false();

        std::fs::remove_dir_all(&app.directory).unwrap();
    }

    fn focus_track(app: &mut App, track: TrackRef) {
        app.selected_stream = app
            .track_rows()
            .iter()
            .position(|row| *row == track)
            .unwrap_or_else(|| panic!("{track:?} has no row"));
    }

    /// Encoding settings only make sense for a track ffmpeg will actually re-encode,
    /// and the dialog has no way to say "never mind" — so the refusals happen before it
    /// opens, each with the notice that says what to do instead.
    #[test]
    fn video_settings_should_refuse_every_track_that_cannot_be_re_encoded() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080},
                {"index": 1, "codec_type": "audio", "codec_name": "aac"},
                {"index": 2, "codec_type": "video", "codec_name": "mjpeg",
                 "disposition": {"attached_pic": 1}},
            ]),
        );

        // Act / Assert: audio.
        focus_track(&mut app, TrackRef::Embedded(1));
        app.open_video_settings();
        assert_that!(app.video_settings_popup.is_none()).is_true();
        assert_that!(app.notice.clone())
            .contains("Encoding settings are only available for video tracks.".to_string());

        // Act / Assert: cover art is a video stream but not a playable one.
        app.notice = None;
        focus_track(&mut app, TrackRef::Embedded(2));
        app.open_video_settings();
        assert_that!(app.video_settings_popup.is_none()).is_true();
        assert_that!(app.notice.clone())
            .contains("Encoding settings are only available for video tracks.".to_string());

        // Act / Assert: a video track already marked for deletion.
        app.notice = None;
        focus_track(&mut app, TrackRef::Embedded(0));
        app.toggle_delete_selected_stream();
        app.notice = None;
        focus_track(&mut app, TrackRef::Embedded(0));
        app.open_video_settings();
        assert_that!(app.video_settings_popup.is_none()).is_true();
        assert_that!(app.notice.clone()).contains(
            "Unmark this track for deletion before changing its video settings.".to_string(),
        );

        // Act / Assert: unmarked, it opens.
        focus_track(&mut app, TrackRef::Embedded(0));
        app.toggle_delete_selected_stream();
        app.notice = None;
        focus_track(&mut app, TrackRef::Embedded(0));
        app.open_video_settings();
        assert_that!(app.video_settings_popup.is_some()).is_true();

        std::fs::remove_dir_all(&app.directory).unwrap();
    }

    #[test]
    fn subtitle_settings_should_refuse_a_track_that_is_on_its_way_out() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "subtitle", "codec_name": "subrip",
                 "tags": {"language": "eng"}},
                {"index": 2, "codec_type": "subtitle", "codec_name": "dvb_teletext",
                 "tags": {"language": "eng"}},
            ]),
        );

        // Act / Assert: marked for deletion.
        focus_track(&mut app, TrackRef::Embedded(1));
        app.toggle_delete_selected_stream();
        app.notice = None;
        app.open_subtitle_settings(SubtitleSource::Embedded(1));
        assert_that!(app.subtitle_settings_popup.is_none()).is_true();
        assert_that!(app.notice.clone())
            .contains("Unmark this subtitle track for deletion before converting it.".to_string());

        // Act / Assert: a codec reel has no conversion route for.
        focus_track(&mut app, TrackRef::Embedded(1));
        app.toggle_delete_selected_stream();
        app.notice = None;
        focus_track(&mut app, TrackRef::Embedded(2));
        app.open_subtitle_settings(SubtitleSource::Embedded(2));
        assert_that!(app.subtitle_settings_popup.is_none()).is_true();
        assert_that!(app.notice.clone())
            .contains("This subtitle format is not supported for conversion yet.".to_string());

        std::fs::remove_dir_all(&app.directory).unwrap();
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
    fn navigation_keys_should_act_on_whichever_pane_holds_the_focus() {
        // Arrange: the same four movements, meaning three different things depending on
        // the focused layer. Getting this wrong scrolls the details pane when the user
        // meant to change file.
        let mut app = test_file_app(&["a.mkv", "b.mkv", "c.mkv"]);
        let directory = app.directory.clone();

        // Act / Assert: in the file pane, they move the file selection and clamp at both
        // ends rather than wrapping.
        app.layer = Layer::Files;
        app.select(0);
        app.select_previous();
        assert_that!(app.list_state.selected()).is_equal_to(Some(0));
        app.select_next();
        assert_that!(app.list_state.selected()).is_equal_to(Some(1));
        app.select_previous();
        assert_that!(app.list_state.selected()).is_equal_to(Some(0));

        // In the stream pane, the same keys move the track selection.
        app.layer = Layer::Streams;
        app.selected_stream = 2;
        app.select_previous();
        assert_that!(app.selected_stream).is_equal_to(1);
        app.select_previous();
        app.select_previous();
        assert_that!(app.selected_stream).is_equal_to(0);

        // In the details popup they scroll it, and never past the top.
        app.layer = Layer::StreamDetails;
        app.set_details_max_scroll(10);
        app.select_next();
        app.select_next();
        assert_that!(app.details_scroll).is_equal_to(2);
        app.select_previous();
        assert_that!(app.details_scroll).is_equal_to(1);
        app.select_previous();
        app.select_previous();
        assert_that!(app.details_scroll).is_equal_to(0);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn folding_should_apply_only_in_the_file_pane() {
        // Arrange: folding hides a file's sidecars. The commands are file-pane commands,
        // so pressing them with the stream pane focused must not silently reshape the
        // list the user is not looking at.
        let mut app = test_file_app(&["a.mkv", "b.mkv"]);
        let directory = app.directory.clone();
        app.layer = Layer::Files;
        app.select(0);

        // Act / Assert
        app.unfold_selected_file();
        assert_that!(app.unfolded_files.len()).is_equal_to(1);
        app.fold_selected_file();
        assert_that!(app.unfolded_files.is_empty()).is_true();

        app.unfold_all_files();
        assert_that!(app.unfolded_files.len()).is_equal_to(2);
        app.fold_all_files();
        assert_that!(app.unfolded_files.is_empty()).is_true();

        // With the stream pane focused, every one of them is a no-op.
        app.layer = Layer::Streams;
        app.unfold_selected_file();
        app.unfold_all_files();
        assert_that!(app.unfolded_files.is_empty()).is_true();
        app.unfolded_files.insert(app.files[0].path.clone());
        app.fold_selected_file();
        app.fold_all_files();
        assert_that!(app.unfolded_files.len()).is_equal_to(1);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn reconcile_files_should_populate_the_in_memory_cache_from_disk_cache_on_demand() {
        // Regression test: the in-memory probe cache used to be built once, eagerly,
        // by cloning the *entire* on-disk cache at startup — including entries for
        // files outside the current directory — which `reconcile_files` then
        // immediately pruned back down again. It's now populated lazily, per file,
        // from `disk_cache` as files show up in `self.files`; this exercises that path
        // directly rather than relying on the full clone that used to do it.
        let mut app = test_file_app(&["a.mkv", "unrelated.mkv"]);
        let directory = app.directory.clone();
        let first = app.files[0].clone();
        let outcome = ProbeOutcome::Video(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264"}
        ])));
        app.disk_cache.insert(
            first.path.clone(),
            first.fingerprint.length,
            first.fingerprint.modified,
            outcome.clone(),
        );
        // An entry for a file that was never in this directory must never be pulled
        // into the in-memory cache, matching the old eager-clone-then-prune result.
        app.disk_cache.insert(
            directory.join("elsewhere.mkv"),
            0,
            None,
            ProbeOutcome::NotVideo("audio".to_string()),
        );
        assert_that!(&app.cache).is_empty();

        // Act: re-run reconciliation so it re-derives `self.cache` from `disk_cache`.
        app.apply_directory_snapshot(DirectorySnapshot::Files(
            scan_directory(&directory).unwrap(),
        ));

        // Assert
        match app.cache.get(&CacheKey::for_file(&first)) {
            Some(ProbeOutcome::Video(info)) => {
                assert_that!(info.streams[0]["codec_name"].as_str()).contains("h264");
            }
            other => panic!("expected a cached video outcome, got {other:?}"),
        }
        assert_that!(app.cache.len()).is_equal_to(1);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn receive_directory_snapshots_should_apply_a_scan_error_and_then_recover_from_it() {
        // Arrange: the watcher thread reports both outcomes on the same channel, and an
        // unreadable directory must not leave the last good listing on screen.
        let mut app = test_file_app(&["a.mkv", "b.mkv"]);
        let directory = app.directory.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        assert_that!(app.files.len()).is_equal_to(2);

        // Act
        tx.send(DirectorySnapshot::Error("permission denied".to_string()))
            .unwrap();
        app.receive_directory_snapshots(&rx);

        // Assert
        assert_that!(app.scan_error.as_deref()).is_equal_to(Some("permission denied"));
        assert_that!(app.files.is_empty()).is_true();

        // Act: the directory becomes readable again.
        tx.send(DirectorySnapshot::Files(
            crate::files::scan_directory(&directory).unwrap(),
        ))
        .unwrap();
        app.receive_directory_snapshots(&rx);

        // Assert: the error clears rather than sticking around behind a good listing.
        assert_that!(app.scan_error.is_none()).is_true();
        assert_that!(app.files.len()).is_equal_to(2);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn start_pending_probe_should_wait_out_the_debounce_before_asking_the_worker() {
        // Arrange: holding a cursor key past ten files must not queue ten ffprobe runs,
        // so a probe is only sent once the selection has settled.
        let (probe_tx, probe_rx) = std::sync::mpsc::channel::<ProbeRequest>();
        let (conflict_tx, _) = std::sync::mpsc::channel::<ProbeRequest>();
        let (edit_tx, _) = std::sync::mpsc::channel::<EditRequest>();
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-pending-probe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("a.mkv"), b"media").unwrap();
        let mut app = App::new(
            directory.clone(),
            probe_tx,
            conflict_tx,
            edit_tx.clone(),
            edit_tx,
        )
        .unwrap();
        app.select_file_position_with_force(Some(0), true);
        assert_that!(app.pending_since.is_some()).is_true();

        // Act: immediately, while the selection is still fresh.
        app.start_pending_probe();

        // Assert: nothing sent, and the request is still pending.
        assert!(probe_rx.try_recv().is_err(), "too early to probe");
        assert_that!(app.pending_since.is_some()).is_true();

        // Act: with the debounce backdated, as it would be after the user stopped moving.
        app.pending_since = Some(Instant::now() - Duration::from_millis(200));
        app.start_pending_probe();

        // Assert: exactly one request, tagged with the current generation so a later
        // response can be matched against the selection that asked for it.
        let request = probe_rx.try_recv().expect("the probe should be queued");
        assert_that!(request.generation).is_equal_to(app.generation);
        assert_that!(request.path).is_equal_to(directory.join("a.mkv"));
        assert_that!(request.is_network_mount).is_equal_to(app.is_network_mount);
        assert_that!(app.pending_since.is_none()).is_true();

        // And a second call sends nothing more.
        app.start_pending_probe();
        assert!(probe_rx.try_recv().is_err(), "one probe per selection");

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn start_pending_probe_should_forward_the_app_s_cached_network_mount_flag() {
        // `App::new` decides `is_network_mount` once for the whole session; the probe
        // worker must reuse that answer instead of re-deriving it per file (which, on
        // an actual network mount, can mean a fresh `/proc/mounts` read plus a
        // `canonicalize()` round trip on every single selection).
        let (probe_tx, probe_rx) = std::sync::mpsc::channel::<ProbeRequest>();
        let (conflict_tx, _) = std::sync::mpsc::channel::<ProbeRequest>();
        let (edit_tx, _) = std::sync::mpsc::channel::<EditRequest>();
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-pending-probe-network-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("a.mkv"), b"media").unwrap();
        let mut app = App::new(
            directory.clone(),
            probe_tx,
            conflict_tx,
            edit_tx.clone(),
            edit_tx,
        )
        .unwrap();
        app.is_network_mount = true;
        app.select_file_position_with_force(Some(0), true);
        app.pending_since = Some(Instant::now() - Duration::from_millis(200));

        app.start_pending_probe();

        let request = probe_rx.try_recv().expect("the probe should be queued");
        assert_that!(request.is_network_mount).is_true();

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn receive_probe_results_should_ignore_a_response_for_a_superseded_generation() {
        // Arrange: a probe answer that arrives after the user has already moved on. The
        // worker is asynchronous, so this is the ordinary case for a fast keyboard, and
        // showing the stale answer would label one file with another's streams.
        let mut app = test_file_app(&["a.mkv", "b.mkv"]);
        let (tx, rx) = std::sync::mpsc::channel();
        let directory = app.directory.clone();
        let first = app.files[0].clone();
        app.list_state.select(Some(0));
        let stale_generation = app.generation;
        app.select_file_position(Some(1));
        assert_ne!(app.generation, stale_generation, "moving files re-probes");

        // Act
        tx.send(ProbeResponse {
            generation: stale_generation,
            path: first.path.clone(),
            fingerprint: first.fingerprint,
            outcome: ProbeOutcome::NotVideo("stale".to_string()),
        })
        .unwrap();
        app.receive_probe_results(&rx);

        // Assert: not shown — but still cached, because the answer is true about that
        // file and re-probing it later would be wasted work.
        assert!(
            !matches!(&app.outcome, Some(ProbeOutcome::NotVideo(reason)) if reason == "stale"),
            "a superseded response must not become the displayed outcome",
        );
        assert!(
            app.cache
                .get(&CacheKey::for_file(&first))
                .is_some_and(|outcome| matches!(outcome, ProbeOutcome::NotVideo(_))),
            "the answer is still worth caching",
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn receive_probe_results_should_show_a_response_that_still_matches_the_selection() {
        // Arrange: the same plumbing, with the response the user is actually waiting for.
        let mut app = test_file_app(&["a.mkv"]);
        let directory = app.directory.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        app.select_file_position_with_force(Some(0), true);
        app.loading = true;
        app.selected_stream = 3;
        let file = app.files[0].clone();

        // Act
        tx.send(ProbeResponse {
            generation: app.generation,
            path: file.path.clone(),
            fingerprint: file.fingerprint,
            outcome: ProbeOutcome::Video(media(serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"}
            ]))),
        })
        .unwrap();
        let drained = app.receive_probe_results(&rx);

        // Assert: displayed, and the stream selection resets so it cannot point past the
        // end of a file with fewer tracks than the last one.
        assert!(matches!(app.outcome, Some(ProbeOutcome::Video(_))));
        assert_that!(app.loading).is_false();
        assert_that!(app.selected_stream).is_equal_to(0);
        // The main loop skips redrawing when nothing was drained; assert this doesn't
        // regress into always reporting "nothing changed".
        assert_that!(drained).is_true();

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn receive_functions_should_report_no_change_when_their_channel_is_empty() {
        // Regression test: the main loop only redraws when `receive_directory_snapshots`
        // / `receive_probe_results` / `receive_edit_results` report they drained
        // something, to avoid repainting the whole UI on every idle tick. If these ever
        // stopped reporting "unchanged" for an empty channel, that optimization would
        // silently turn back into an unconditional redraw.
        let mut app = test_file_app(&["a.mkv"]);
        let directory = app.directory.clone();
        let (_snapshot_tx, snapshot_rx) = std::sync::mpsc::channel();
        let (_probe_tx, probe_rx) = std::sync::mpsc::channel();
        let (_edit_tx, edit_rx) = std::sync::mpsc::channel();

        assert_that!(app.receive_directory_snapshots(&snapshot_rx)).is_false();
        assert_that!(app.receive_probe_results(&probe_rx)).is_false();
        assert_that!(app.receive_edit_results(&edit_rx)).is_false();

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn selecting_a_cached_file_should_skip_loading_entirely() {
        // Arrange: the reason the disk cache exists — on a network mount, re-probing a
        // file the user already opened costs seconds.
        let mut app = test_file_app(&["a.mkv", "b.mkv"]);
        let directory = app.directory.clone();
        let first = app.files[0].clone();
        app.cache.insert(
            CacheKey::for_file(&first),
            ProbeOutcome::Video(media(serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"}
            ]))),
        );

        // Act
        app.select_file_position(Some(1));
        app.select_file_position(Some(0));

        // Assert: answered from cache, with no probe queued at all.
        assert!(matches!(app.outcome, Some(ProbeOutcome::Video(_))));
        assert_that!(app.loading).is_false();
        assert_that!(app.pending_since.is_none()).is_true();

        // And an uncached neighbour still goes through the loading state.
        app.select_file_position(Some(1));
        assert_that!(app.loading).is_true();
        assert_that!(app.outcome.is_none()).is_true();

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn switching_files_should_preserve_each_files_staged_edits() {
        // Regression test for the exact bug reported: edit file A, switch to file B,
        // edit it too, switch back to A — A's edits used to be silently discarded the
        // moment the selection moved (`select_file_position_with_force` unconditionally
        // called `clear_edit_state()`). Both files' edits must now survive round-trip
        // navigation, and switching to an unedited file must show a clean baseline
        // rather than leaking the other file's staged state.
        let mut app = test_file_app(&["a.mkv", "b.mkv", "c.mkv"]);
        let directory = app.directory.clone();
        let files = app.files.clone();
        let streams = serde_json::json!([
            {"index": 0, "codec_type": "video", "disposition": {"default": 1}},
            {"index": 1, "codec_type": "audio", "disposition": {"default": 1}}
        ]);
        for file in &files {
            app.cache.insert(
                CacheKey::for_file(file),
                ProbeOutcome::Video(media(streams.clone())),
            );
        }

        // Edit file A: mark its audio track for deletion.
        app.select_file_position(Some(0));
        app.deleted_streams.insert(1);
        assert_that!(app.has_track_edits()).is_true();

        // Edit file B: give it a container target.
        app.select_file_position(Some(1));
        assert_that!(app.deleted_streams.is_empty()).is_true(); // clean baseline for B
        app.container_target = Some(crate::edit::ContainerFormat::Mp4);

        // C is never touched — selecting it should show a clean baseline too.
        app.select_file_position(Some(2));
        assert_that!(app.deleted_streams.is_empty()).is_true();
        assert_that!(app.container_target.is_none()).is_true();

        // Switch back to A: its deletion must still be staged.
        app.select_file_position(Some(0));
        assert_that!(app.deleted_streams.contains(&1)).is_true();
        assert_that!(app.container_target.is_none()).is_true();

        // And back to B: its container target must still be staged.
        app.select_file_position(Some(1));
        assert_that!(app.deleted_streams.is_empty()).is_true();
        assert_that!(app.container_target).is_equal_to(Some(crate::edit::ContainerFormat::Mp4));

        // Both ended up recorded in the staged-edits map, keyed by path.
        assert_that!(app.staged_edits.contains_key(&files[0].path)).is_true();
        assert_that!(app.staged_edits.contains_key(&files[1].path)).is_true();
        assert_that!(app.staged_edits.contains_key(&files[2].path)).is_false();

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn resetting_every_edit_on_a_file_should_unstage_it() {
        // Regression test: staging is keyed on `has_track_edits()` at the moment of
        // leaving a file — if the user undoes every edit before switching away, the
        // stale staged entry must be dropped, not kept around as an empty no-op edit.
        let mut app = test_file_app(&["a.mkv", "b.mkv"]);
        let directory = app.directory.clone();
        let files = app.files.clone();
        for file in &files {
            app.cache.insert(
                CacheKey::for_file(file),
                ProbeOutcome::Video(media(serde_json::json!([
                    {"index": 0, "codec_type": "video", "disposition": {"default": 1}}
                ]))),
            );
        }

        app.select_file_position(Some(0));
        app.container_target = Some(crate::edit::ContainerFormat::Mp4);
        app.select_file_position(Some(1));
        assert_that!(app.staged_edits.contains_key(&files[0].path)).is_true();

        app.select_file_position(Some(0));
        app.container_target = None;
        app.select_file_position(Some(1));
        assert_that!(app.staged_edits.contains_key(&files[0].path)).is_false();

        std::fs::remove_dir_all(directory).unwrap();
    }

    /// The batch cursor is driven by keys that stay live while other dialogs are up, so
    /// it has to refuse to move whenever the batch list is not what is on screen.
    #[test]
    fn the_batch_cursor_should_not_move_while_the_batch_list_is_not_on_screen() {
        // Arrange
        let mut app = batch_app(4);
        app.batch_cursor = 2;

        // Act / Assert: the cursor moves while the batch dialog owns the screen.
        app.move_batch_cursor_down(1);
        assert_that!(app.batch_cursor).is_equal_to(3);
        app.move_batch_cursor_up(1);
        assert_that!(app.batch_cursor).is_equal_to(2);

        // Act / Assert: it does not once the confirm prompt takes over.
        app.dialog = Some(Dialog::ConfirmCancel);
        app.move_batch_cursor_down(1);
        app.move_batch_cursor_up(1);
        assert_that!(app.batch_cursor).is_equal_to(2);

        // Act / Assert: nor with the batch dialog up but no batch behind it.
        app.dialog = Some(Dialog::BatchProcessing);
        app.active_batch = None;
        app.move_batch_cursor_down(1);
        app.move_batch_cursor_up(1);
        assert_that!(app.batch_cursor).is_equal_to(2);

        std::fs::remove_dir_all(&app.directory).unwrap();
    }

    /// The confirm dialog is the last thing between the user and a batch that rewrites
    /// their files, so container metadata has to be listed field by field — and a
    /// "change" that matches what the file already says must not be listed at all.
    #[test]
    fn the_save_summary_should_name_which_container_metadata_fields_actually_changed() {
        // Arrange
        let info = MediaInfo::from_json(serde_json::json!({
            "format": {
                "format_name": "matroska,webm",
                "tags": {"title": "Old title", "genre": "Documentary"},
            },
            "streams": [{"index": 0, "codec_type": "video", "codec_name": "h264"}],
        }))
        .unwrap();
        let fingerprint = crate::files::FileFingerprint {
            length: 10,
            modified: None,
        };
        let base = || {
            let mut staged = staged_edit(fingerprint, vec![0]);
            staged.original_stream_order = vec![0];
            staged
        };
        let path = Path::new("/videos/movie.mkv");

        // Act: two fields differ from the file, one is retyped to the same value.
        let mut changed = base();
        changed.container_metadata = Some(ContainerMetadata {
            title: Some("New title".to_string()),
            comment: None,
            date: None,
            genre: Some("Documentary".to_string()),
            artist: Some("Someone".to_string()),
        });
        let changed_lines = staged_edit_summary(path, &info, &changed);

        // Act: metadata staged, but identical to what the file already has.
        let mut identical = base();
        identical.container_metadata = Some(ContainerMetadata {
            title: Some("Old title".to_string()),
            comment: None,
            date: None,
            genre: Some("Documentary".to_string()),
            artist: None,
        });
        let identical_lines = staged_edit_summary(path, &info, &identical);

        // Act: a container conversion is named with both ends of the change.
        let mut converted = base();
        converted.container_target = Some(ContainerFormat::Mp4);
        let converted_lines = staged_edit_summary(path, &info, &converted);

        // Assert
        assert!(
            changed_lines
                .iter()
                .any(|line| line == "Updating container metadata: title, artist"),
            "only the fields that differ may be listed: {changed_lines:?}",
        );
        assert!(
            !identical_lines
                .iter()
                .any(|line| line.contains("container metadata")),
            "metadata equal to the file's own is not a change: {identical_lines:?}",
        );
        assert_that!(converted_lines.first().map(String::as_str))
            .contains("Changing container from MKV to MP4");
    }

    /// A dropdown cursor can outlive the list it points into — a language filter that
    /// now matches fewer entries, a codec list that shrank when the container changed.
    /// Confirming on a cursor with nothing under it has to do nothing and leave the
    /// dropdown open, not stage a change or index past the end.
    #[test]
    fn confirming_a_dropdown_with_nothing_under_the_cursor_should_do_nothing() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "subtitle", "codec_name": "subrip",
                 "tags": {"language": "eng"}},
            ]),
        );
        app.subtitle_capabilities = full_subtitle_capabilities();
        app.container_target = Some(ContainerFormat::Matroska);
        focus_track(&mut app, TrackRef::Embedded(1));
        app.open_subtitle_settings(SubtitleSource::Embedded(1));

        // Act / Assert: the codec dropdown, with the cursor past its last entry.
        app.activate_subtitle_settings();
        assert_that!(app.subtitle_settings_popup.as_ref().unwrap().mode)
            .is_equal_to(SubtitleSettingsMode::CodecDropdown);
        app.subtitle_settings_popup.as_mut().unwrap().codec_cursor = 99;
        app.activate_subtitle_settings();
        assert_that!(app.subtitle_settings_popup.as_ref().unwrap().mode)
            .is_equal_to(SubtitleSettingsMode::CodecDropdown);
        assert_that!(app.subtitle_changes.is_empty()).is_true();

        // Act / Assert: the language dropdown, same story.
        app.escape_subtitle_settings();
        app.move_subtitle_settings_cursor(1);
        app.activate_subtitle_settings();
        assert_that!(app.subtitle_settings_popup.as_ref().unwrap().mode)
            .is_equal_to(SubtitleSettingsMode::LanguageDropdown);
        app.subtitle_settings_popup
            .as_mut()
            .unwrap()
            .language_cursor = 999;
        app.activate_subtitle_settings();
        assert_that!(app.subtitle_settings_popup.as_ref().unwrap().mode)
            .is_equal_to(SubtitleSettingsMode::LanguageDropdown);
        assert_that!(app.subtitle_changes.is_empty()).is_true();

        std::fs::remove_dir_all(&app.directory).unwrap();
    }

    /// The confirm dialog lists one summary per staged file, and it is drawn from data
    /// that can legitimately be missing — a file staged before its probe landed. Every
    /// such case has to produce a line the user can read rather than an empty row.
    #[test]
    fn the_staged_summary_should_stay_readable_when_the_probe_is_missing() {
        // Arrange
        // The staged file must not be the open one, whose status is validated against
        // the live edit fields instead.
        let mut app = test_file_app(&["a.mkv", "b.mkv"]);
        app.select_file_position(Some(0));
        let file = app.files[1].clone();
        let mut edit = staged_edit(file.fingerprint, vec![0]);
        edit.stale = false;
        edit.container_target = Some(ContainerFormat::Mp4);
        app.staged_edits.insert(file.path.clone(), edit);

        // Act
        let unprobed = app.staged_file_summary(&file.path);
        let unstaged = app.staged_file_summary(&app.directory.join("nothing.mkv"));
        let status = app.staged_file_status(&file.path);
        app.cache.insert(
            CacheKey::for_file(&file),
            ProbeOutcome::Video(media(serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"}
            ]))),
        );
        let probed = app.staged_file_summary(&file.path);

        // Assert
        assert_that!(unprobed).is_equal_to(vec!["(changes staged)".to_string()]);
        assert_that!(unstaged).is_empty();
        assert!(
            matches!(status, StagedFileStatus::Invalid(ref message) if message.contains("hasn't been probed yet")),
            "an unprobed staged file must not be processable: {status:?}",
        );
        assert!(
            probed.iter().any(|line| line.contains("MP4")),
            "once probed, the real change is described: {probed:?}",
        );

        std::fs::remove_dir_all(app.directory.clone()).unwrap();
    }

    /// The conflict notice tells the user exactly what it is about to throw away and
    /// what it will keep. Container-level changes are never in a track group, so they
    /// always survive; anything tagged with a conflicting group does not.
    #[test]
    fn the_conflict_notice_should_split_what_it_reverts_from_what_it_keeps() {
        // Arrange
        let mut app = test_file_app(&["a.mkv"]);
        let file = app.files[0].clone();
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264",
             "disposition": {"default": 1}},
            {"index": 1, "codec_type": "audio", "codec_name": "aac"},
        ]));
        let mut edit = staged_edit_with_groups(file.fingerprint, vec![0, 1], &info);
        edit.conflict_groups = BTreeSet::from(["video"]);
        edit.container_target = Some(ContainerFormat::Mp4);
        edit.video_settings.insert(
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
        );
        app.staged_edits.insert(file.path.clone(), edit);

        // Act: with the file's probe not yet cached, there is nothing to describe.
        let unprobed = app.conflicting_change_summary(&file.path);
        app.cache
            .insert(CacheKey::for_file(&file), ProbeOutcome::Video(info));
        let reverted = app.conflicting_change_summary(&file.path);
        let kept = app.kept_change_summary(&file.path);
        let unstaged = app.conflicting_change_summary(&app.directory.join("nothing.mkv"));

        // Assert
        assert_that!(unprobed).is_equal_to(vec!["(changes staged)".to_string()]);
        assert!(
            reverted.iter().any(|line| line.contains("HEVC")),
            "the video re-encode is in the conflicting group: {reverted:?}",
        );
        assert!(
            kept.iter().any(|line| line.contains("MP4")),
            "the container change is not in any track group: {kept:?}",
        );
        assert_that!(unstaged).is_empty();

        std::fs::remove_dir_all(app.directory.clone()).unwrap();
    }

    /// A sidecar appearing or vanishing changes what the subtitle columns contain, so
    /// any staged subtitle work refers to a list that no longer exists. The rest of the
    /// edit — the file itself has not changed — has to survive.
    #[test]
    fn reconcile_files_should_reload_the_sidecars_when_only_they_changed() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        std::fs::write(directory.join("movie.eng.srt"), b"1\n").unwrap();
        app.reconcile_files(scan_directory(&directory).unwrap());
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "subtitle", "codec_name": "subrip",
                 "tags": {"language": "eng"}},
            ]),
        );
        app.subtitle_capabilities = full_subtitle_capabilities();
        assert_that!(app.sidecars.len()).is_equal_to(1);
        focus_track(&mut app, TrackRef::Embedded(1));
        app.transfer_subtitle(1);
        app.container_target = Some(ContainerFormat::Mp4);
        assert_that!(app.subtitle_changes.is_empty()).is_false();

        // Act: a second sidecar shows up beside the same, unchanged, media file.
        std::fs::write(directory.join("movie.nld.srt"), b"1\n").unwrap();
        app.reconcile_files(scan_directory(&directory).unwrap());

        // Assert: the sidecar list is rebuilt and the subtitle work that referred to
        // the old one is dropped, while the container change stays staged.
        assert_that!(app.sidecars.len()).is_equal_to(2);
        assert_that!(app.subtitle_changes.is_empty()).is_true();
        assert_that!(app.subtitle_settings_popup.is_none()).is_true();
        assert_that!(app.container_target).contains(ContainerFormat::Mp4);
        assert_that!(app.notice.clone())
            .contains("Matching subtitle sidecars changed; reloaded them.".to_string());

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn reconcile_files_should_flag_a_staged_files_edits_stale_when_it_changes_on_disk() {
        // Regression test for the confirmed staleness rule: if a staged (non-open)
        // file's on-disk fingerprint changes before it's processed, its staged edits
        // must be kept (not silently dropped) but flagged `stale`.
        let mut app = test_file_app(&["a.mkv", "b.mkv"]);
        let directory = app.directory.clone();
        let files = app.files.clone();
        app.cache.insert(
            CacheKey::for_file(&files[1]),
            ProbeOutcome::Video(media(serde_json::json!([
                {"index": 0, "codec_type": "video", "disposition": {"default": 1}}
            ]))),
        );

        // Stage an edit on b.mkv, then move away from it so it's no longer "open".
        app.select_file_position(Some(1));
        app.container_target = Some(crate::edit::ContainerFormat::Mp4);
        app.select_file_position(Some(0));
        assert_that!(app.staged_edits.get(&files[1].path).unwrap().stale).is_false();

        // Something else touches b.mkv on disk.
        std::fs::write(&files[1].path, b"changed contents").unwrap();

        // Act: reconcile against the new on-disk state.
        let rescanned = scan_directory(&directory).unwrap();
        app.reconcile_files(rescanned);

        // Assert: still staged, but now flagged stale.
        let staged = app
            .staged_edits
            .get(&files[1].path)
            .expect("staged edits must be kept, not dropped");
        assert_that!(staged.stale).is_true();
        assert_that!(staged.container_target).is_equal_to(Some(crate::edit::ContainerFormat::Mp4));

        std::fs::remove_dir_all(directory).unwrap();
    }

    /// Builds a bare `StagedEdit` with every collection defaulted, for tests that only
    /// care about `fingerprint`/`stale`/`conflict_groups`/`stream_order`. Leaves
    /// `track_groups` empty, which makes `reconcile_untouched_groups` fall back to its
    /// no-snapshot, everything-is-touched behavior — i.e. the original all-or-nothing
    /// completeness check. Tests that need the group-aware leniency itself should use
    /// `staged_edit_with_groups` instead.
    fn staged_edit(
        fingerprint: crate::files::FileFingerprint,
        stream_order: Vec<u64>,
    ) -> StagedEdit {
        StagedEdit {
            fingerprint,
            stale: true,
            conflict_groups: Default::default(),
            stream_order,
            moved_streams: Default::default(),
            deleted_streams: Default::default(),
            default_streams: Default::default(),
            default_sidecars: Default::default(),
            audio_settings: Default::default(),
            video_settings: Default::default(),
            subtitle_changes: Default::default(),
            left_subtitle_order: Vec::new(),
            container_target: None,
            container_metadata: None,
            original_stream_order: Vec::new(),
            original_default_streams: Default::default(),
            track_groups: BTreeMap::new(),
        }
    }

    /// Like `staged_edit`, but also stamps `original_stream_order`/`track_groups` from
    /// `info` — the snapshot `reconcile_untouched_groups` needs to tell a group the
    /// edit touches apart from one it doesn't. `stream_order` is taken as both the
    /// live and the original order (tests that need them to differ can still mutate
    /// the result afterward).
    fn staged_edit_with_groups(
        fingerprint: crate::files::FileFingerprint,
        stream_order: Vec<u64>,
        info: &MediaInfo,
    ) -> StagedEdit {
        let mut edit = staged_edit(fingerprint, stream_order.clone());
        edit.original_stream_order = stream_order;
        edit.track_groups = track_groups_for(info);
        edit
    }

    #[test]
    fn every_kind_of_staged_change_on_its_own_should_count_as_staging_something() {
        // Arrange: this predicate decides whether a staged entry survives a partial
        // revert. Each arm of its `||` chain is a different kind of edit, and an arm that
        // stopped counting would drop the whole entry — silently discarding the user's
        // remaining staged work for that file rather than keeping it listed.
        let fingerprint = crate::files::FileFingerprint {
            length: 10,
            modified: None,
        };
        let base = || {
            let mut edit = staged_edit(fingerprint, vec![0, 1, 2]);
            edit.original_stream_order = vec![0, 1, 2];
            edit
        };

        // Act / Assert: a bare entry with the order untouched stages nothing.
        assert!(
            !staged_edit_stages_anything(&base()),
            "an edit with nothing changed must not count as staged",
        );

        // Act / Assert: each kind of change alone is enough.
        let mut container = base();
        container.container_target = Some(ContainerFormat::Mp4);
        assert!(staged_edit_stages_anything(&container), "container target");

        let mut metadata = base();
        metadata.container_metadata = Some(ContainerMetadata::default());
        assert!(staged_edit_stages_anything(&metadata), "container metadata");

        let mut deleted = base();
        deleted.deleted_streams = BTreeSet::from([1]);
        assert!(staged_edit_stages_anything(&deleted), "deleted track");

        let mut sidecars = base();
        sidecars.default_sidecars = BTreeSet::from([0]);
        assert!(staged_edit_stages_anything(&sidecars), "default sidecar");

        let mut video = base();
        video.video_settings = BTreeMap::from([(
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
        assert!(staged_edit_stages_anything(&video), "video settings");

        let mut reordered = base();
        reordered.stream_order = vec![1, 0, 2];
        assert!(staged_edit_stages_anything(&reordered), "reordered tracks");

        let mut defaults = base();
        defaults.default_streams = BTreeSet::from([1]);
        assert!(staged_edit_stages_anything(&defaults), "default track");

        let mut subtitles = base();
        subtitles.subtitle_changes = BTreeMap::from([(
            SubtitleSource::Embedded(2),
            SubtitleChange {
                source: SubtitleSource::Embedded(2),
                source_format: SubtitleFormat::SubRip,
                embedded_target: Some(SubtitleFormat::Ass),
                export_target: None,
                import_into_media: false,
                ocr_language: None,
                metadata: None,
            },
        )]);
        assert!(staged_edit_stages_anything(&subtitles), "subtitle change");
    }

    #[test]
    fn the_summary_should_describe_each_kind_of_subtitle_change_in_its_own_words() {
        // Arrange: this is the list the user reads before committing a save, and each
        // subtitle operation reads differently — importing a sidecar, converting a track
        // in place, and exporting one out are three very different outcomes for their
        // files. Collapsing them to one phrasing would have the user approve a save that
        // does something other than what they read.
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video"},
            {"index": 1, "codec_type": "subtitle", "codec_name": "subrip"},
            {"index": 2, "codec_type": "subtitle", "codec_name": "subrip"}
        ]));
        let fingerprint = crate::files::FileFingerprint {
            length: 10,
            modified: None,
        };
        let mut staged = staged_edit(fingerprint, vec![0, 1, 2]);
        staged.original_stream_order = vec![0, 1, 2];
        let change = |source: SubtitleSource, embedded_target, export_target, import_into_media| {
            SubtitleChange {
                source,
                source_format: SubtitleFormat::SubRip,
                embedded_target,
                export_target,
                import_into_media,
                ocr_language: None,
                metadata: None,
            }
        };
        let sidecar = PathBuf::from("/videos/movie.eng.srt");
        staged.subtitle_changes = BTreeMap::from([
            // A sidecar pulled into the container.
            (
                SubtitleSource::Sidecar(sidecar.clone()),
                change(
                    SubtitleSource::Sidecar(sidecar),
                    Some(SubtitleFormat::Ass),
                    None,
                    true,
                ),
            ),
            // An embedded track converted in place.
            (
                SubtitleSource::Embedded(1),
                change(
                    SubtitleSource::Embedded(1),
                    Some(SubtitleFormat::Ass),
                    None,
                    false,
                ),
            ),
            // An embedded track written out to a sidecar.
            (
                SubtitleSource::Embedded(2),
                change(
                    SubtitleSource::Embedded(2),
                    None,
                    Some(SubtitleFormat::WebVtt),
                    false,
                ),
            ),
        ]);

        // Act
        let lines = staged_edit_summary(Path::new("/videos/movie.mkv"), &info, &staged);

        // Assert: three distinct descriptions, each naming its source the way the user
        // sees it — a sidecar by filename, an embedded track by index.
        assert!(
            lines
                .iter()
                .any(|line| line == "Importing movie.eng.srt as ASS"),
            "sidecar import must be described by filename: {lines:?}",
        );
        assert!(
            lines
                .iter()
                .any(|line| line == "Converting subtitle track #1 in the media to ASS"),
            "an in-place conversion must say it happens in the media: {lines:?}",
        );
        assert!(
            lines
                .iter()
                .any(|line| line == "Exporting subtitle track #2 as WebVTT"),
            "an export must be described as leaving the media: {lines:?}",
        );
    }

    #[test]
    fn the_summary_should_spell_out_a_subtitles_metadata_including_what_was_cleared() {
        // Arrange: metadata edits are the easiest to stage by accident and the hardest to
        // see in the track list, so the summary spells out every value — including a
        // cleared title, which must read as "no title" rather than silently vanishing and
        // leaving the user thinking the title was left alone.
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video"},
            {"index": 1, "codec_type": "subtitle", "codec_name": "subrip"}
        ]));
        let fingerprint = crate::files::FileFingerprint {
            length: 10,
            modified: None,
        };
        let mut staged = staged_edit(fingerprint, vec![0, 1]);
        staged.original_stream_order = vec![0, 1];
        staged.subtitle_changes = BTreeMap::from([(
            SubtitleSource::Embedded(1),
            SubtitleChange {
                source: SubtitleSource::Embedded(1),
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
                    hearing_impaired: true,
                    original: false,
                    commentary: true,
                }),
            },
        )]);

        // Act
        let lines = staged_edit_summary(Path::new("/videos/movie.mkv"), &info, &staged);
        let metadata_line = lines
            .iter()
            .find(|line| line.starts_with("Updating subtitle track #1 metadata"))
            .unwrap_or_else(|| panic!("a metadata line must be present: {lines:?}"));

        // Assert: every set flag is named, the cleared title is spelled out, and flags
        // that are off are absent rather than listed as disabled.
        assert!(metadata_line.contains("language NLD"), "{metadata_line}");
        assert!(metadata_line.contains("no title"), "{metadata_line}");
        assert!(metadata_line.contains("Forced"), "{metadata_line}");
        assert!(
            metadata_line.contains("Hearing impaired"),
            "{metadata_line}",
        );
        assert!(metadata_line.contains("Commentary"), "{metadata_line}");
        assert!(!metadata_line.contains("CC"), "{metadata_line}");
        assert!(!metadata_line.contains("Original"), "{metadata_line}");
    }

    #[test]
    fn a_subtitle_change_that_changes_nothing_should_not_keep_a_file_staged() {
        // Arrange: a subtitle entry left behind after the user undid what it described —
        // same format, no export, no import, no metadata. `has_effect` is what tells this
        // apart from a real change, and treating the entry's mere presence as an edit
        // would leave the file marked staged forever with nothing to save.
        let fingerprint = crate::files::FileFingerprint {
            length: 10,
            modified: None,
        };
        let mut edit = staged_edit(fingerprint, vec![0, 1]);
        edit.original_stream_order = vec![0, 1];
        edit.subtitle_changes = BTreeMap::from([(
            SubtitleSource::Embedded(1),
            SubtitleChange {
                source: SubtitleSource::Embedded(1),
                source_format: SubtitleFormat::SubRip,
                embedded_target: Some(SubtitleFormat::SubRip),
                export_target: None,
                import_into_media: false,
                ocr_language: None,
                metadata: None,
            },
        )]);

        // Act / Assert
        assert!(!staged_edit_stages_anything(&edit));
    }

    #[test]
    fn reconcile_files_should_dispatch_exactly_one_conflict_probe_request_per_fingerprint() {
        // Regression test for the dedup rule in `reconcile_files`: while a staged file
        // stays stale at the same fingerprint, repeated reconcile passes (up to once a
        // second locally) must not re-enqueue a background check every time.
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-conflict-dispatch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("a.mkv"), b"media").unwrap();
        let (probe_tx, _) = std::sync::mpsc::channel::<ProbeRequest>();
        let (conflict_tx, conflict_rx) = std::sync::mpsc::channel::<ProbeRequest>();
        let (edit_tx, _) = std::sync::mpsc::channel::<EditRequest>();
        let mut app = App::new(
            directory.clone(),
            probe_tx,
            conflict_tx,
            edit_tx.clone(),
            edit_tx,
        )
        .unwrap();
        let path = app.files[0].path.clone();
        let old_fingerprint = app.files[0].fingerprint;
        app.staged_edits
            .insert(path.clone(), staged_edit(old_fingerprint, vec![0]));
        app.staged_edits.get_mut(&path).unwrap().stale = false;

        std::fs::write(&path, b"changed contents").unwrap();
        let rescanned = scan_directory(&directory).unwrap();

        // Act: reconcile twice against the same (now-stale) on-disk state.
        app.reconcile_files(rescanned.clone());
        app.reconcile_files(rescanned);

        // Assert: exactly one request went out, for the new fingerprint.
        let request = conflict_rx.try_recv().expect("a check must be dispatched");
        assert_that!(request.path).is_equal_to(path);
        assert_that!(request.fingerprint).is_not_equal_to(old_fingerprint);
        assert!(
            conflict_rx.try_recv().is_err(),
            "a second reconcile at the same fingerprint must not dispatch again"
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn receive_conflict_probe_results_should_auto_resolve_when_the_staged_edit_still_validates() {
        // Regression test for the reported false positive: a staged edit that only
        // touches the video track must not be treated as conflicting just because an
        // unrelated audio track changed — even though the file's MediaInfo as a whole
        // is no longer identical to what was staged against. Detection is based on
        // "does the staged edit still validate", not raw equality.
        let mut app = test_file_app(&["a.mkv"]);
        let path = app.files[0].path.clone();
        let old_fingerprint = app.files[0].fingerprint;
        let new_fingerprint = crate::files::FileFingerprint {
            length: old_fingerprint.length + 1,
            modified: old_fingerprint.modified,
        };
        let mut edit = staged_edit(old_fingerprint, vec![0, 1]);
        edit.video_settings.insert(
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
        );
        app.staged_edits.insert(path.clone(), edit);
        app.pending_conflict_checks
            .insert(path.clone(), new_fingerprint);

        let (tx, rx) = std::sync::mpsc::channel::<ProbeResponse>();
        tx.send(ProbeResponse {
            generation: 0,
            path: path.clone(),
            fingerprint: new_fingerprint,
            // Same two tracks staged against (0 = video, 1 = audio), but the audio
            // track's codec is now different — irrelevant to the video-only edit.
            outcome: ProbeOutcome::Video(media(serde_json::json!([
                {"index": 0, "codec_type": "video", "disposition": {"default": 1}},
                {"index": 1, "codec_type": "audio", "codec_name": "opus", "disposition": {"default": 1}}
            ]))),
        })
        .unwrap();

        // Act
        app.receive_conflict_probe_results(&rx);

        // Assert
        let staged = app.staged_edits.get(&path).unwrap();
        assert_that!(staged.stale).is_false();
        assert_that!(staged.conflict_groups.is_empty()).is_true();
        assert_that!(staged.fingerprint).is_equal_to(new_fingerprint);
        assert_that!(app.pending_conflict_checks.contains_key(&path)).is_false();
        assert_that!(app.notice.as_deref())
            .is_equal_to(Some("a.mkv changed on disk; staged edits kept."));

        std::fs::remove_dir_all(app.directory.clone()).unwrap();
    }

    #[test]
    fn a_staged_edit_with_no_track_group_snapshot_should_auto_resolve_into_a_plain_invalid() {
        // The fallback for an edit staged without a `track_groups` snapshot: there is
        // no way to attribute the change to a track type, so there is nothing the
        // notice could name or revert. It re-baselines instead, and the staged order
        // no longer accounting for every track on disk surfaces through the ordinary
        // `staged_file_status` gate — which still refuses to process it.
        let mut app = test_file_app(&["a.mkv", "z.mkv"]);
        app.select_file_position(Some(1));
        let path = app.files[0].path.clone();
        let old_fingerprint = app.files[0].fingerprint;
        let new_fingerprint = crate::files::FileFingerprint {
            length: old_fingerprint.length + 1,
            modified: old_fingerprint.modified,
        };
        app.files[0].fingerprint = new_fingerprint;
        let edit = staged_edit(old_fingerprint, vec![0]);
        app.staged_edits.insert(path.clone(), edit);
        app.pending_conflict_checks
            .insert(path.clone(), new_fingerprint);

        let (tx, rx) = std::sync::mpsc::channel::<ProbeResponse>();
        tx.send(ProbeResponse {
            generation: 0,
            path: path.clone(),
            fingerprint: new_fingerprint,
            outcome: ProbeOutcome::Video(media(serde_json::json!([
                {"index": 0, "codec_type": "video", "disposition": {"default": 1}},
                {"index": 1, "codec_type": "audio", "disposition": {"default": 1}}
            ]))),
        })
        .unwrap();

        // Act
        app.receive_conflict_probe_results(&rx);

        // Assert
        let staged = app.staged_edits.get(&path).unwrap();
        assert_that!(staged.stale).is_false();
        assert_that!(staged.conflict_groups.is_empty()).is_true();
        assert_that!(app.conflicting_paths()).is_equal_to(Vec::new());
        match app.staged_file_status(&path) {
            StagedFileStatus::Invalid(message) => {
                assert!(
                    message.contains("tracks changed"),
                    "unexpected message: {message}"
                );
            }
            other => panic!("expected Invalid for an incomplete staged order, got {other:?}"),
        }

        std::fs::remove_dir_all(app.directory.clone()).unwrap();
    }

    #[test]
    fn receive_conflict_probe_results_should_flag_a_confirmed_conflict_when_the_edited_groups_track_count_changes()
     {
        // The user-facing rule this whole reconciliation exists for: a group the
        // staged edit actually touches (here, video, via `video_settings`) always
        // requires manual resolution if its own track count changes on disk — unlike
        // a *different*, untouched group gaining or losing a track, which
        // auto-resolves (see the two tests below).
        let mut app = test_file_app(&["a.mkv"]);
        let path = app.files[0].path.clone();
        let old_fingerprint = app.files[0].fingerprint;
        let new_fingerprint = crate::files::FileFingerprint {
            length: old_fingerprint.length + 1,
            modified: old_fingerprint.modified,
        };
        let original_info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "disposition": {"default": 1}}
        ]));
        let mut edit = staged_edit_with_groups(old_fingerprint, vec![0], &original_info);
        edit.video_settings.insert(
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
        );
        app.staged_edits.insert(path.clone(), edit);
        app.pending_conflict_checks
            .insert(path.clone(), new_fingerprint);

        let (tx, rx) = std::sync::mpsc::channel::<ProbeResponse>();
        tx.send(ProbeResponse {
            generation: 0,
            path: path.clone(),
            fingerprint: new_fingerprint,
            // A second video track appears — the touched group itself changed.
            outcome: ProbeOutcome::Video(media(serde_json::json!([
                {"index": 0, "codec_type": "video", "disposition": {"default": 1}},
                {"index": 1, "codec_type": "video", "disposition": {"default": 0}}
            ]))),
        })
        .unwrap();

        // Act
        app.receive_conflict_probe_results(&rx);

        // Assert: still stale, at the *old* fingerprint, and confirmed as a conflict.
        let staged = app.staged_edits.get(&path).unwrap();
        assert_that!(staged.stale).is_true();
        assert_that!(staged.conflict_groups.iter().copied().collect::<Vec<_>>())
            .is_equal_to(vec!["video"]);
        assert_that!(staged.fingerprint).is_equal_to(old_fingerprint);

        std::fs::remove_dir_all(app.directory.clone()).unwrap();
    }

    #[test]
    fn receive_conflict_probe_results_should_auto_resolve_when_an_untouched_groups_track_disappears()
     {
        // Regression test for the reported case: only the video track was edited; an
        // unrelated, untouched audio track vanishing entirely on disk must not force
        // the whole staged edit into conflict (as it did before this was scoped to
        // "did a *touched* group change") — it should just drop out of the order,
        // leaving the video edit intact.
        let mut app = test_file_app(&["a.mkv"]);
        let path = app.files[0].path.clone();
        let old_fingerprint = app.files[0].fingerprint;
        let new_fingerprint = crate::files::FileFingerprint {
            length: old_fingerprint.length + 1,
            modified: old_fingerprint.modified,
        };
        let original_info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "disposition": {"default": 1}},
            {"index": 1, "codec_type": "audio", "disposition": {"default": 0}}
        ]));
        let mut edit = staged_edit_with_groups(old_fingerprint, vec![0, 1], &original_info);
        edit.video_settings.insert(
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
        );
        app.staged_edits.insert(path.clone(), edit);
        app.pending_conflict_checks
            .insert(path.clone(), new_fingerprint);

        let (tx, rx) = std::sync::mpsc::channel::<ProbeResponse>();
        tx.send(ProbeResponse {
            generation: 0,
            path: path.clone(),
            fingerprint: new_fingerprint,
            // The audio track is gone entirely — video (the only edited group) is
            // untouched by the change.
            outcome: ProbeOutcome::Video(media(serde_json::json!([
                {"index": 0, "codec_type": "video", "disposition": {"default": 1}}
            ]))),
        })
        .unwrap();

        // Act
        app.receive_conflict_probe_results(&rx);

        // Assert: auto-resolved, the vanished audio track quietly dropped from the
        // order, and the video edit itself untouched.
        let staged = app.staged_edits.get(&path).unwrap();
        assert_that!(staged.stale).is_false();
        assert_that!(staged.conflict_groups.is_empty()).is_true();
        assert_that!(staged.fingerprint).is_equal_to(new_fingerprint);
        assert_that!(staged.stream_order.clone()).is_equal_to(vec![0]);
        assert_that!(staged.video_settings.contains_key(&0)).is_true();

        std::fs::remove_dir_all(app.directory.clone()).unwrap();
    }

    #[test]
    fn receive_conflict_probe_results_should_auto_resolve_when_an_untouched_groups_track_appears() {
        // Mirror image of the disappearing-track case: only the video track was
        // edited; a brand new, untouched audio track appearing on disk must not force
        // a conflict either — it should just be picked up as a normal kept track.
        let mut app = test_file_app(&["a.mkv"]);
        let path = app.files[0].path.clone();
        let old_fingerprint = app.files[0].fingerprint;
        let new_fingerprint = crate::files::FileFingerprint {
            length: old_fingerprint.length + 1,
            modified: old_fingerprint.modified,
        };
        let original_info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "disposition": {"default": 1}}
        ]));
        let mut edit = staged_edit_with_groups(old_fingerprint, vec![0], &original_info);
        edit.video_settings.insert(
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
        );
        app.staged_edits.insert(path.clone(), edit);
        app.pending_conflict_checks
            .insert(path.clone(), new_fingerprint);

        let (tx, rx) = std::sync::mpsc::channel::<ProbeResponse>();
        tx.send(ProbeResponse {
            generation: 0,
            path: path.clone(),
            fingerprint: new_fingerprint,
            // A new default audio track appears — video (the only edited group) is
            // untouched by the change.
            outcome: ProbeOutcome::Video(media(serde_json::json!([
                {"index": 0, "codec_type": "video", "disposition": {"default": 1}},
                {"index": 1, "codec_type": "audio", "disposition": {"default": 1}}
            ]))),
        })
        .unwrap();

        // Act
        app.receive_conflict_probe_results(&rx);

        // Assert: auto-resolved, the new audio track picked up (kept, and marked
        // default per its own disposition), and the video edit itself untouched.
        let staged = app.staged_edits.get(&path).unwrap();
        assert_that!(staged.stale).is_false();
        assert_that!(staged.conflict_groups.is_empty()).is_true();
        assert_that!(staged.fingerprint).is_equal_to(new_fingerprint);
        assert_that!(staged.stream_order.clone()).is_equal_to(vec![0, 1]);
        assert_that!(staged.default_streams.contains(&1)).is_true();
        assert_that!(staged.video_settings.contains_key(&0)).is_true();

        std::fs::remove_dir_all(app.directory.clone()).unwrap();
    }

    #[test]
    fn receive_conflict_probe_results_should_resync_the_open_files_live_fields_after_a_quiet_auto_resolve()
     {
        // Regression test: a quiet background auto-resolve on the *currently open*
        // file used to update `staged_edits[path]`'s reconciled fields but never sync
        // them back onto the live edit fields the Streams view actually reads from —
        // leaving it stuck showing pre-reconciliation data. See
        // `App::apply_staged_snapshot`/`App::refresh_cached_outcome`.
        let mut app = test_file_app(&["a.mkv"]);
        let path = app.files[0].path.clone();
        app.video_settings.insert(
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
        );
        assert_that!(app.has_track_edits()).is_true();

        std::fs::write(&path, b"changed contents").unwrap();
        let rescanned = scan_directory(&app.directory).unwrap();
        app.reconcile_files(rescanned);
        let new_fingerprint = app.files[0].fingerprint;
        assert_that!(app.staged_edits.get(&path).unwrap().stale).is_true();

        let (tx, rx) = std::sync::mpsc::channel::<ProbeResponse>();
        tx.send(ProbeResponse {
            generation: 0,
            path: path.clone(),
            fingerprint: new_fingerprint,
            // Same three tracks staged against (0 = video, 1 = audio, 2 = subtitle),
            // but the audio track's codec is now different — irrelevant to the
            // video-only edit; video (the only touched group) is unaffected.
            outcome: ProbeOutcome::Video(media(serde_json::json!([
                {"index": 0, "codec_type": "video", "disposition": {"default": 1}},
                {"index": 1, "codec_type": "audio", "codec_name": "opus", "disposition": {"default": 1}},
                {"index": 2, "codec_type": "subtitle", "disposition": {"default": 0},
                 "tags": {"language": "eng"}}
            ]))),
        })
        .unwrap();

        // Act
        app.receive_conflict_probe_results(&rx);

        // Assert: quietly resolved, and the live fields (not just the `StagedEdit`
        // snapshot) reflect the fresh content.
        let staged = app.staged_edits.get(&path).unwrap();
        assert_that!(staged.stale).is_false();
        assert_that!(staged.conflict_groups.is_empty()).is_true();
        assert_that!(app.video_settings.contains_key(&0)).is_true();
        assert_that!(&app.stream_order).contains_exactly_in_given_order([0, 1, 2]);
        let audio_codec = app
            .media_info()
            .and_then(|info| info.streams.get(1))
            .and_then(|stream| stream.get("codec_name"))
            .and_then(|value| value.as_str());
        assert_that!(audio_codec).is_equal_to(Some("opus"));

        std::fs::remove_dir_all(app.directory.clone()).unwrap();
    }

    #[test]
    fn staged_file_status_should_report_invalid_for_a_confirmed_conflict_on_the_open_file() {
        // Regression test: `staged_file_status` used to check "is this the open
        // file?" first and, if so, only ever validate *live* fields — never looking
        // at `staged_edits[path].conflict_groups`. Once the open file can become a
        // confirmed conflict (via `App::reconcile_files` staging its live edits),
        // that early branch would incorrectly keep reporting `Valid`, letting
        // `Ctrl+S` sail past an unresolved conflict.
        let mut app = test_file_app(&["a.mkv"]);
        let path = app.files[0].path.clone();
        app.video_settings.insert(
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
        );
        std::fs::write(&path, b"changed contents").unwrap();
        let rescanned = scan_directory(&app.directory).unwrap();
        app.reconcile_files(rescanned);
        let new_fingerprint = app.files[0].fingerprint;

        let (tx, rx) = std::sync::mpsc::channel::<ProbeResponse>();
        tx.send(ProbeResponse {
            generation: 0,
            path: path.clone(),
            fingerprint: new_fingerprint,
            // A new video track appears — the touched group (video) itself changed,
            // a genuine structural conflict.
            outcome: ProbeOutcome::Video(media(serde_json::json!([
                {"index": 0, "codec_type": "video", "disposition": {"default": 1}},
                {"index": 1, "codec_type": "audio", "disposition": {"default": 1}},
                {"index": 2, "codec_type": "subtitle", "disposition": {"default": 0}},
                {"index": 3, "codec_type": "video", "disposition": {"default": 0}}
            ]))),
        })
        .unwrap();

        // Act
        app.receive_conflict_probe_results(&rx);

        // Assert
        let staged = app.staged_edits.get(&path).unwrap();
        assert_that!(staged.conflict_groups.iter().copied().collect::<Vec<_>>())
            .is_equal_to(vec!["video"]);
        match app.staged_file_status(&path) {
            StagedFileStatus::Invalid(message) => {
                assert!(
                    message.contains("have to be reverted"),
                    "unexpected message: {message}"
                );
            }
            other => panic!("expected Invalid for a confirmed conflict, got {other:?}"),
        }

        std::fs::remove_dir_all(app.directory.clone()).unwrap();
    }

    #[test]
    fn request_process_all_should_not_launder_an_unresolved_conflict_on_the_open_file() {
        // Regression test for `App::snapshot_current_edits` preserving staleness:
        // `request_process_all` calls `snapshot_current_edits()` itself before
        // validating every staged file — if that call blindly re-baselined the open
        // file's `StagedEdit` (clearing `stale`/`conflict_groups`), an unresolved
        // conflict would silently sail past validation and the batch would start.
        let mut app = test_file_app(&["a.mkv"]);
        let path = app.files[0].path.clone();
        app.video_settings.insert(
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
        );
        std::fs::write(&path, b"changed contents").unwrap();
        let rescanned = scan_directory(&app.directory).unwrap();
        app.reconcile_files(rescanned);
        let new_fingerprint = app.files[0].fingerprint;

        let (tx, rx) = std::sync::mpsc::channel::<ProbeResponse>();
        tx.send(ProbeResponse {
            generation: 0,
            path: path.clone(),
            fingerprint: new_fingerprint,
            outcome: ProbeOutcome::Video(media(serde_json::json!([
                {"index": 0, "codec_type": "video", "disposition": {"default": 1}},
                {"index": 1, "codec_type": "audio", "disposition": {"default": 1}},
                {"index": 2, "codec_type": "subtitle", "disposition": {"default": 0}},
                {"index": 3, "codec_type": "video", "disposition": {"default": 0}}
            ]))),
        })
        .unwrap();
        app.receive_conflict_probe_results(&rx);
        assert_that!(
            app.staged_edits
                .get(&path)
                .unwrap()
                .conflict_groups
                .is_empty()
        )
        .is_false();

        // Act
        app.request_process_all();

        // Assert: refused, not silently started.
        assert_that!(app.dialog).is_equal_to(Some(Dialog::Error));
        assert_that!(app.edit_error.as_deref().unwrap_or_default()).contains("have to be reverted");

        std::fs::remove_dir_all(app.directory.clone()).unwrap();
    }

    #[test]
    fn receive_conflict_probe_results_should_ignore_a_response_superseded_by_a_newer_dispatched_check()
     {
        // Mirrors `receive_probe_results_should_ignore_a_response_for_a_superseded_generation`:
        // the file changed again after this check was dispatched, so its answer
        // describes on-disk state that's no longer current and must not be acted on.
        let mut app = test_file_app(&["a.mkv"]);
        let path = app.files[0].path.clone();
        let old_fingerprint = app.files[0].fingerprint;
        let stale_response_fingerprint = crate::files::FileFingerprint {
            length: old_fingerprint.length + 1,
            modified: old_fingerprint.modified,
        };
        let newer_dispatched_fingerprint = crate::files::FileFingerprint {
            length: old_fingerprint.length + 2,
            modified: old_fingerprint.modified,
        };
        let edit = staged_edit(old_fingerprint, vec![0]);
        app.staged_edits.insert(path.clone(), edit);
        app.pending_conflict_checks
            .insert(path.clone(), newer_dispatched_fingerprint);

        let (tx, rx) = std::sync::mpsc::channel::<ProbeResponse>();
        tx.send(ProbeResponse {
            generation: 0,
            path: path.clone(),
            fingerprint: stale_response_fingerprint,
            outcome: ProbeOutcome::Video(media(serde_json::json!([
                {"index": 0, "codec_type": "video", "disposition": {"default": 1}}
            ]))),
        })
        .unwrap();

        // Act
        app.receive_conflict_probe_results(&rx);

        // Assert: nothing about the staged edit changed, and the newer dispatched
        // check is still tracked as pending (not clobbered by the stale answer).
        let staged = app.staged_edits.get(&path).unwrap();
        assert_that!(staged.stale).is_true();
        assert_that!(staged.conflict_groups.is_empty()).is_true();
        assert_that!(staged.fingerprint).is_equal_to(old_fingerprint);
        assert_that!(app.pending_conflict_checks.get(&path).copied())
            .is_equal_to(Some(newer_dispatched_fingerprint));

        std::fs::remove_dir_all(app.directory.clone()).unwrap();
    }

    #[test]
    fn only_a_touched_group_whose_tracks_changed_should_raise_a_conflict() {
        // The single condition the notice exists for. The staged edit touches audio
        // (it moves a track), and audio is exactly what changed on disk — so the
        // tracks it names are no longer the tracks in the file.
        let mut app = test_file_app(&["a.mkv", "z.mkv"]);
        app.select_file_position(Some(1));
        let path = app.files[0].path.clone();
        let old_fingerprint = app.files[0].fingerprint;
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video"},
            {"index": 1, "codec_type": "audio"},
            {"index": 2, "codec_type": "audio"}
        ]));
        let mut edit = staged_edit_with_groups(old_fingerprint, vec![0, 1, 2], &info);
        edit.stream_order = vec![0, 2, 1];
        edit.moved_streams = BTreeSet::from([2]);
        app.staged_edits.insert(path.clone(), edit);
        let new_fingerprint = crate::files::FileFingerprint {
            length: old_fingerprint.length + 1,
            modified: old_fingerprint.modified,
        };
        app.files[0].fingerprint = new_fingerprint;
        app.pending_conflict_checks
            .insert(path.clone(), new_fingerprint);

        let (tx, rx) = std::sync::mpsc::channel::<ProbeResponse>();
        tx.send(ProbeResponse {
            generation: 0,
            path: path.clone(),
            fingerprint: new_fingerprint,
            // A third audio track appears.
            outcome: ProbeOutcome::Video(media(serde_json::json!([
                {"index": 0, "codec_type": "video"},
                {"index": 1, "codec_type": "audio"},
                {"index": 2, "codec_type": "audio"},
                {"index": 3, "codec_type": "audio"}
            ]))),
        })
        .unwrap();

        // Act
        app.receive_conflict_probe_results(&rx);

        // Assert
        let staged = app.staged_edits.get(&path).unwrap();
        assert_that!(staged.conflict_groups.iter().copied().collect::<Vec<_>>())
            .is_equal_to(vec!["audio"]);
        assert_that!(staged.stale).is_true();
        assert_that!(staged.fingerprint).is_equal_to(old_fingerprint);
        assert_that!(app.conflicting_paths()).is_equal_to(vec![path]);

        std::fs::remove_dir_all(app.directory.clone()).unwrap();
    }

    #[test]
    fn a_staged_edit_that_merely_became_invalid_should_auto_resolve_rather_than_raise_a_conflict() {
        // The distinction this whole design turns on. A SubRip track appears while a
        // conversion to MP4 is staged; nothing staged touches subtitles, so the new
        // track is absorbed. The conversion is now impossible — but that's an ordinary
        // "this edit can't be processed", reported through `staged_file_status` with
        // the compatibility markers that already exist for it, not the ground moving
        // under an edit. Raising the unskippable notice here would be noise.
        let mut app = test_file_app(&["a.mkv", "z.mkv"]);
        app.select_file_position(Some(1));
        let path = app.files[0].path.clone();
        let old_fingerprint = app.files[0].fingerprint;
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264"}
        ]));
        let mut edit = staged_edit_with_groups(old_fingerprint, vec![0], &info);
        edit.container_target = Some(ContainerFormat::Mp4);
        app.staged_edits.insert(path.clone(), edit);
        let new_fingerprint = crate::files::FileFingerprint {
            length: old_fingerprint.length + 1,
            modified: old_fingerprint.modified,
        };
        app.files[0].fingerprint = new_fingerprint;
        app.pending_conflict_checks
            .insert(path.clone(), new_fingerprint);

        let (tx, rx) = std::sync::mpsc::channel::<ProbeResponse>();
        tx.send(ProbeResponse {
            generation: 0,
            path: path.clone(),
            fingerprint: new_fingerprint,
            outcome: ProbeOutcome::Video(media(serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "subtitle", "codec_name": "subrip",
                 "tags": {"language": "eng"}}
            ]))),
        })
        .unwrap();

        // Act
        app.receive_conflict_probe_results(&rx);

        // Assert: absorbed — no notice, re-baselined onto the new content...
        let staged = app.staged_edits.get(&path).unwrap();
        assert_that!(staged.conflict_groups.is_empty()).is_true();
        assert_that!(staged.stale).is_false();
        assert_that!(staged.fingerprint).is_equal_to(new_fingerprint);
        assert_that!(app.conflicting_paths()).is_equal_to(Vec::new());
        assert_that!(staged.stream_order.contains(&1)).is_true();
        // ...and the now-impossible conversion surfaces as a plain `Invalid` instead.
        match app.staged_file_status(&path) {
            StagedFileStatus::Invalid(message) => {
                assert!(
                    message.contains("MP4") || message.contains("SubRip"),
                    "expected a container compatibility message, got {message:?}"
                );
            }
            other => {
                panic!("expected Invalid for an unprocessable container change, got {other:?}")
            }
        }

        std::fs::remove_dir_all(app.directory.clone()).unwrap();
    }

    #[test]
    fn acknowledge_conflicts_should_revert_only_the_conflicting_group() {
        // The point of reverting per type rather than per file: the audio edit is the
        // one that lost its tracks, so the video re-encode and the container
        // conversion staged alongside it must survive untouched.
        let mut app = test_file_app(&["a.mkv", "z.mkv"]);
        app.select_file_position(Some(1));
        let path = app.files[0].path.clone();
        let old_fingerprint = app.files[0].fingerprint;
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video"},
            {"index": 1, "codec_type": "audio"},
            {"index": 2, "codec_type": "audio"}
        ]));
        app.cache.insert(
            CacheKey {
                path: path.clone(),
                length: old_fingerprint.length,
                modified: old_fingerprint.modified,
            },
            ProbeOutcome::Video(info.clone()),
        );
        let mut edit = staged_edit_with_groups(old_fingerprint, vec![0, 1, 2], &info);
        edit.stream_order = vec![0, 2, 1];
        edit.moved_streams = BTreeSet::from([2]);
        edit.default_streams = BTreeSet::from([2]);
        edit.container_target = Some(ContainerFormat::Mp4);
        edit.video_settings.insert(
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
        );
        app.staged_edits.insert(path.clone(), edit);

        let new_fingerprint = crate::files::FileFingerprint {
            length: old_fingerprint.length + 1,
            modified: old_fingerprint.modified,
        };
        app.files[0].fingerprint = new_fingerprint;
        app.pending_conflict_checks
            .insert(path.clone(), new_fingerprint);
        let (tx, rx) = std::sync::mpsc::channel::<ProbeResponse>();
        tx.send(ProbeResponse {
            generation: 0,
            path: path.clone(),
            fingerprint: new_fingerprint,
            // Audio track 2 is gone and a different one took its place.
            outcome: ProbeOutcome::Video(media(serde_json::json!([
                {"index": 0, "codec_type": "video"},
                {"index": 1, "codec_type": "audio"},
                {"index": 3, "codec_type": "audio", "disposition": {"default": 1}}
            ]))),
        })
        .unwrap();
        app.receive_conflict_probe_results(&rx);
        assert_that!(
            app.staged_edits
                .get(&path)
                .unwrap()
                .conflict_groups
                .iter()
                .copied()
                .collect::<Vec<_>>()
        )
        .is_equal_to(vec!["audio"]);

        // The notice lists only the audio changes it is about to revert — not the
        // video re-encode or the container conversion, which survive.
        let summary = app.conflicting_change_summary(&path);
        assert_that!(summary).contains_exactly_in_any_order([
            "Moving 1 audio track".to_string(),
            "Changing the default audio track".to_string(),
        ]);

        // Act
        app.dialog = Some(Dialog::ResolveConflicts);
        app.conflict_opened_at = Some(Instant::now() - CONFLICT_COUNTDOWN);
        app.acknowledge_conflicts();

        // Assert
        assert_that!(app.dialog).is_equal_to(None);
        let staged = app.staged_edits.get(&path).expect("still staged");
        assert_that!(staged.conflict_groups.is_empty()).is_true();
        assert_that!(staged.stale).is_false();
        assert_that!(staged.fingerprint).is_equal_to(new_fingerprint);
        // Audio: rebuilt from what's on disk now, with the staged move and default
        // change dropped and the file's own default flag adopted.
        assert_that!(staged.stream_order.clone()).is_equal_to(vec![0, 1, 3]);
        assert_that!(staged.moved_streams.is_empty()).is_true();
        assert_that!(staged.default_streams.clone()).is_equal_to(BTreeSet::from([3]));
        assert_that!(staged.original_default_streams.clone()).is_equal_to(BTreeSet::from([3]));
        // Everything else: untouched.
        assert_that!(staged.container_target).is_equal_to(Some(ContainerFormat::Mp4));
        assert_that!(staged.video_settings.contains_key(&0)).is_true();

        std::fs::remove_dir_all(app.directory.clone()).unwrap();
    }

    /// A subtitle change is pinned to the group, not to a stream index — a sidecar
    /// import has no index at all — so acknowledging a subtitle conflict has to clear
    /// the whole subtitle side of the edit and leave every other kind of change alone.
    #[test]
    fn acknowledging_a_subtitle_conflict_should_clear_the_whole_subtitle_side_of_the_edit() {
        // Arrange
        let mut app = test_file_app(&["a.mkv"]);
        let file = app.files[0].clone();
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264",
             "disposition": {"default": 1}},
            {"index": 1, "codec_type": "subtitle", "codec_name": "subrip",
             "tags": {"language": "eng"}},
        ]));
        app.cache
            .insert(CacheKey::for_file(&file), ProbeOutcome::Video(info.clone()));
        let sidecar_path = app.directory.join("a.dan.srt");
        let mut edit = staged_edit_with_groups(file.fingerprint, vec![0, 1], &info);
        edit.conflict_groups = BTreeSet::from(["subtitle"]);
        edit.container_target = Some(ContainerFormat::Mp4);
        edit.video_settings.insert(
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
        );
        edit.subtitle_changes.insert(
            SubtitleSource::Sidecar(sidecar_path.clone()),
            SubtitleChange {
                source: SubtitleSource::Sidecar(sidecar_path),
                source_format: SubtitleFormat::SubRip,
                embedded_target: None,
                export_target: None,
                import_into_media: true,
                ocr_language: None,
                metadata: None,
            },
        );
        edit.default_sidecars.insert(0);
        edit.left_subtitle_order = vec![TrackRef::Embedded(1)];
        app.staged_edits.insert(file.path.clone(), edit);
        assert_that!(app.maybe_open_conflict_dialog()).is_true();
        app.conflict_opened_at = Some(Instant::now() - CONFLICT_COUNTDOWN);

        // Act
        app.acknowledge_conflicts();

        // Assert
        let staged = &app.staged_edits[&file.path];
        assert_that!(staged.subtitle_changes.is_empty()).is_true();
        assert_that!(staged.default_sidecars.is_empty()).is_true();
        assert_that!(staged.left_subtitle_order.is_empty()).is_true();
        assert_that!(staged.conflict_groups.is_empty()).is_true();
        // The video side of the same edit is untouched.
        assert_that!(staged.container_target).is_equal_to(Some(ContainerFormat::Mp4));
        assert_that!(staged.video_settings.contains_key(&0)).is_true();
        assert_that!(app.dialog).is_none();

        std::fs::remove_dir_all(app.directory.clone()).unwrap();
    }

    #[test]
    fn the_notices_button_should_stay_inert_until_its_countdown_finishes() {
        // The notice is the only dialog that appears unprompted — a background probe
        // can answer mid-keystroke — so an Enter already in flight must not revert
        // staged work. The button counts down in its label first.
        let mut app = test_file_app(&["a.mkv"]);
        let path = app.files[0].path.clone();
        let mut edit = staged_edit(app.files[0].fingerprint, vec![0]);
        edit.conflict_groups = BTreeSet::from(["video"]);
        app.staged_edits.insert(path.clone(), edit);
        // `revert_group_edits` rebuilds the group from the file's current content, so
        // acknowledging needs that content probed.
        app.cache.insert(
            CacheKey::for_file(&app.files[0].clone()),
            ProbeOutcome::Video(media(serde_json::json!([
                {"index": 0, "codec_type": "video", "disposition": {"default": 1}}
            ]))),
        );

        assert_that!(app.conflict_countdown()).is_equal_to(None);
        assert_that!(app.maybe_open_conflict_dialog()).is_true();

        // Opens counting from the full five seconds...
        assert_that!(app.conflict_countdown()).is_equal_to(Some(5));
        app.acknowledge_conflicts();
        assert_that!(app.dialog).is_equal_to(Some(Dialog::ResolveConflicts));
        assert_that!(app.conflicting_paths().len()).is_equal_to(1);

        // ...ticking down rather than jumping, and never reading a stale zero.
        app.conflict_opened_at = Some(Instant::now() - Duration::from_millis(1500));
        assert_that!(app.conflict_countdown()).is_equal_to(Some(4));
        app.conflict_opened_at = Some(Instant::now() - Duration::from_millis(4990));
        assert_that!(app.conflict_countdown()).is_equal_to(Some(1));
        app.acknowledge_conflicts();
        assert_that!(app.dialog).is_equal_to(Some(Dialog::ResolveConflicts));

        // Armed: now it acknowledges.
        app.conflict_opened_at = Some(Instant::now() - CONFLICT_COUNTDOWN);
        assert_that!(app.conflict_countdown()).is_equal_to(None);
        app.acknowledge_conflicts();
        assert_that!(app.dialog).is_equal_to(None);
        assert_that!(app.conflicting_paths()).is_equal_to(Vec::new());

        std::fs::remove_dir_all(app.directory.clone()).unwrap();
    }

    #[test]
    fn is_animating_should_cover_the_countdown_so_the_armed_button_gets_painted() {
        // Regression test for a notice that reached "Understood (1)" and stayed
        // there, dim, until an unrelated keypress: `main` only repaints when
        // something changed or something is animating, so the countdown has to report
        // itself as animating — and `main` pairs that with one trailing frame, since
        // the frame showing the *armed* button is the first non-animating one.
        let mut app = test_file_app(&["a.mkv"]);
        assert_that!(app.is_animating()).is_false();

        let mut edit = staged_edit(app.files[0].fingerprint, vec![0]);
        edit.conflict_groups = BTreeSet::from(["video"]);
        app.staged_edits.insert(app.files[0].path.clone(), edit);
        assert_that!(app.maybe_open_conflict_dialog()).is_true();
        assert_that!(app.is_animating()).is_true();

        // The tick the button arms on is the one that stops the animation — which is
        // exactly why the loop needs to draw once more after this goes false.
        app.conflict_opened_at = Some(Instant::now() - CONFLICT_COUNTDOWN);
        assert_that!(app.conflict_countdown()).is_equal_to(None);
        assert_that!(app.is_animating()).is_false();

        std::fs::remove_dir_all(app.directory.clone()).unwrap();
    }

    #[test]
    fn the_countdown_should_only_run_while_the_notice_is_showing() {
        // `main`'s redraw loop asks this every tick to decide whether to repaint, so
        // it must not claim to be animating when no notice is up.
        let mut app = test_file_app(&["a.mkv"]);
        app.conflict_opened_at = Some(Instant::now());
        assert_that!(app.conflict_countdown()).is_equal_to(None);

        app.dialog = Some(Dialog::ResolveConflicts);
        assert_that!(app.conflict_countdown()).is_equal_to(Some(5));

        std::fs::remove_dir_all(app.directory.clone()).unwrap();
    }

    #[test]
    fn acknowledge_conflicts_should_unstage_a_file_whose_only_edit_was_reverted() {
        // Reverting the last thing a file staged must leave it genuinely unstaged
        // rather than an entry that stages nothing, and — for the open file — must
        // rebuild the live fields from the *fresh* content (see
        // `refresh_cached_outcome`), not the pre-change probe.
        let mut app = test_file_app(&["a.mkv"]);
        let path = app.files[0].path.clone();
        app.video_settings.insert(
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
        );
        assert_that!(app.media_info().unwrap().streams.len()).is_equal_to(3);

        std::fs::write(&path, b"changed contents").unwrap();
        let rescanned = scan_directory(&app.directory).unwrap();
        app.reconcile_files(rescanned);
        let new_fingerprint = app.files[0].fingerprint;

        let (tx, rx) = std::sync::mpsc::channel::<ProbeResponse>();
        tx.send(ProbeResponse {
            generation: 0,
            path: path.clone(),
            fingerprint: new_fingerprint,
            // A second video track appears — a conflict against the touched group.
            outcome: ProbeOutcome::Video(media(serde_json::json!([
                {"index": 0, "codec_type": "video", "disposition": {"default": 1}},
                {"index": 1, "codec_type": "audio", "disposition": {"default": 1}},
                {"index": 2, "codec_type": "subtitle", "disposition": {"default": 0}},
                {"index": 3, "codec_type": "video", "disposition": {"default": 0}}
            ]))),
        })
        .unwrap();
        app.receive_conflict_probe_results(&rx);
        assert_that!(
            app.staged_edits
                .get(&path)
                .unwrap()
                .conflict_groups
                .iter()
                .copied()
                .collect::<Vec<_>>()
        )
        .is_equal_to(vec!["video"]);
        // Not yet resynced — still showing the pre-conflict, 3-track content.
        assert_that!(app.media_info().unwrap().streams.len()).is_equal_to(3);

        // Act
        app.dialog = Some(Dialog::ResolveConflicts);
        app.conflict_opened_at = Some(Instant::now() - CONFLICT_COUNTDOWN);
        app.acknowledge_conflicts();

        // Assert
        assert_that!(app.staged_edits.contains_key(&path)).is_false();
        assert_that!(app.media_info().unwrap().streams.len()).is_equal_to(4);
        assert_that!(app.stream_order.contains(&3)).is_true();
        assert_that!(app.video_settings.contains_key(&0)).is_false();
        assert_that!(app.staged_file_status(&path)).is_equal_to(StagedFileStatus::Unstaged);

        std::fs::remove_dir_all(app.directory.clone()).unwrap();
    }

    #[test]
    fn maybe_open_conflict_dialog_should_list_multiple_simultaneous_conflicts_together() {
        let mut app = test_file_app(&["a.mkv", "b.mkv"]);
        for file in app.files.clone() {
            let mut edit = staged_edit(file.fingerprint, vec![0]);
            edit.conflict_groups = BTreeSet::from(["video"]);
            app.staged_edits.insert(file.path, edit);
        }

        assert_that!(app.maybe_open_conflict_dialog()).is_true();
        let paths = app.conflicting_paths();
        assert_that!(paths.len()).is_equal_to(2);

        std::fs::remove_dir_all(app.directory.clone()).unwrap();
    }

    #[test]
    fn staged_file_status_should_reflect_the_open_files_live_edit_validity() {
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "subtitle", "codec_name": "subrip",
                 "tags": {"language": "eng"}}
            ]),
        );
        let path = app.files[0].path.clone();
        assert_that!(app.staged_file_status(&path)).is_equal_to(StagedFileStatus::Unstaged);

        // A no-op-ish but genuine edit (container target matching the source format)
        // still counts as staged and passes validation.
        app.container_target = Some(ContainerFormat::Matroska);
        assert_that!(app.staged_file_status(&path)).is_equal_to(StagedFileStatus::Valid);

        // SubRip can't go into MP4 — this is now an invalid staged edit.
        app.container_target = Some(ContainerFormat::Mp4);
        match app.staged_file_status(&path) {
            StagedFileStatus::Invalid(_) => {}
            other => panic!("expected Invalid, got {other:?}"),
        }

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn staged_file_status_should_validate_a_non_open_staged_file_via_the_cache() {
        // `staged_file_status` must be able to validate a file that isn't currently
        // open at all, using its cached probe data — this is what lets the file list
        // mark other staged files valid/invalid, and what gates a future "process all
        // staged files" action.
        let mut app = test_file_app(&["a.mkv", "b.mkv"]);
        let directory = app.directory.clone();
        let files = app.files.clone();
        let streams = serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264"},
            {"index": 1, "codec_type": "subtitle", "codec_name": "subrip",
             "tags": {"language": "eng"}}
        ]);
        for file in &files {
            app.cache.insert(
                CacheKey::for_file(file),
                ProbeOutcome::Video(media(streams.clone())),
            );
        }

        app.select_file_position(Some(0));
        app.container_target = Some(ContainerFormat::Matroska);
        app.select_file_position(Some(1));
        assert_that!(app.staged_file_status(&files[0].path)).is_equal_to(StagedFileStatus::Valid);

        app.select_file_position(Some(0));
        app.container_target = Some(ContainerFormat::Mp4);
        app.select_file_position(Some(1));
        match app.staged_file_status(&files[0].path) {
            StagedFileStatus::Invalid(_) => {}
            other => panic!("expected Invalid, got {other:?}"),
        }

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn staged_file_status_should_report_invalid_for_a_stale_staged_file() {
        let mut app = test_file_app(&["a.mkv", "b.mkv"]);
        let directory = app.directory.clone();
        let files = app.files.clone();
        app.cache.insert(
            CacheKey::for_file(&files[1]),
            ProbeOutcome::Video(media(serde_json::json!([
                {"index": 0, "codec_type": "video", "disposition": {"default": 1}}
            ]))),
        );

        app.select_file_position(Some(1));
        app.container_target = Some(ContainerFormat::Matroska);
        app.select_file_position(Some(0));
        assert_that!(app.staged_file_status(&files[1].path)).is_equal_to(StagedFileStatus::Valid);

        std::fs::write(&files[1].path, b"changed contents").unwrap();
        let rescanned = scan_directory(&directory).unwrap();
        app.reconcile_files(rescanned);

        match app.staged_file_status(&files[1].path) {
            StagedFileStatus::Invalid(message) => {
                assert_that!(message.as_str()).contains("changed on disk");
            }
            other => panic!("expected Invalid, got {other:?}"),
        }

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn staged_file_summary_should_describe_every_kind_of_staged_change_for_a_non_open_file() {
        // Arrange: the confirm-all popup is the last thing a user reads before an
        // irreversible batch remux, so every staged change on every file — not just
        // the one currently open — has to be named in it, sourced from
        // `staged_edits`/the cache rather than the (by then different) live fields.
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-staged-summary-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("movie.mkv"), b"media").unwrap();
        std::fs::write(directory.join("other.mkv"), b"media").unwrap();

        let (probe_tx, _) = std::sync::mpsc::channel::<ProbeRequest>();
        let (conflict_tx, _) = std::sync::mpsc::channel::<ProbeRequest>();
        let (transcode_tx, _) = std::sync::mpsc::channel::<EditRequest>();
        let (remux_tx, _) = std::sync::mpsc::channel::<EditRequest>();
        let mut app = App::new(
            directory.clone(),
            probe_tx,
            conflict_tx,
            transcode_tx,
            remux_tx,
        )
        .unwrap();

        let movie = app
            .files
            .iter()
            .find(|file| file.display_name == "movie.mkv")
            .unwrap()
            .clone();
        let other = app
            .files
            .iter()
            .find(|file| file.display_name == "other.mkv")
            .unwrap()
            .clone();
        app.cache.insert(
            CacheKey::for_file(&movie),
            ProbeOutcome::Video(media(serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264",
                 "width": 1920, "height": 1080},
                {"index": 1, "codec_type": "subtitle", "codec_name": "subrip",
                 "tags": {"language": "eng"}}
            ]))),
        );
        app.cache.insert(
            CacheKey::for_file(&other),
            ProbeOutcome::Video(media(serde_json::json!([
                {"index": 0, "codec_type": "video", "disposition": {"default": 1}}
            ]))),
        );

        let movie_index = app
            .files
            .iter()
            .position(|file| file.display_name == "movie.mkv")
            .unwrap();
        let other_index = app
            .files
            .iter()
            .position(|file| file.display_name == "other.mkv")
            .unwrap();

        // Force a real transition away from whatever `App::new` auto-selected before
        // the cache above existed, so the next selection genuinely picks up the
        // now-cached probe data via `queue_probe`'s synchronous cache-hit path.
        // `queue_probe` resets `layer` to `Files` on every selection, so it's set
        // back to `Streams` only after the last select below, not before.
        app.select_file_position(Some(other_index));
        app.select_file_position(Some(movie_index));
        app.layer = Layer::Streams;

        app.video_settings.insert(
            0,
            VideoSettings {
                codec: VideoCodec::Av1,
                resolution: VideoResolution::P720,
                metadata: VideoMetadata {
                    language: "und".to_string(),
                    title: None,
                    commentary: false,
                },
                rotation: VideoRotation::None,
            },
        );
        app.selected_stream = app
            .track_rows()
            .iter()
            .position(|row| *row == TrackRef::Embedded(1))
            .unwrap();
        assert!(app.transfer_subtitle(1), "the embedded track should export");
        app.container_target = Some(ContainerFormat::Mp4);

        // Act: switch away, staging movie.mkv's edits.
        app.select_file_position(Some(other_index));
        let summary = app.staged_file_summary(&movie.path).join("\n");

        // Assert
        assert_that!(summary.as_str()).contains("Changing container from MKV to MP4");
        assert_that!(summary.as_str()).contains("Encoding video track #0 as AV1 at 1280×720");
        assert_that!(summary.as_str()).contains("Exporting subtitle track #1");

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn staged_file_summary_should_group_track_moves_deletes_and_default_changes() {
        // Arrange: with nothing more specific staged (no container/video/subtitle
        // change), the summary falls back to describing raw track moves, deletes, and
        // default changes, grouped by track type.
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-staged-summary-fallback-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("movie.mkv"), b"media").unwrap();
        std::fs::write(directory.join("other.mkv"), b"media").unwrap();

        let (probe_tx, _) = std::sync::mpsc::channel::<ProbeRequest>();
        let (conflict_tx, _) = std::sync::mpsc::channel::<ProbeRequest>();
        let (transcode_tx, _) = std::sync::mpsc::channel::<EditRequest>();
        let (remux_tx, _) = std::sync::mpsc::channel::<EditRequest>();
        let mut app = App::new(
            directory.clone(),
            probe_tx,
            conflict_tx,
            transcode_tx,
            remux_tx,
        )
        .unwrap();
        app.layer = Layer::Streams;

        let movie = app
            .files
            .iter()
            .find(|file| file.display_name == "movie.mkv")
            .unwrap()
            .clone();
        let other = app
            .files
            .iter()
            .find(|file| file.display_name == "other.mkv")
            .unwrap()
            .clone();
        app.cache.insert(
            CacheKey::for_file(&movie),
            ProbeOutcome::Video(media(serde_json::json!([
                {"index": 0, "codec_type": "video"},
                {"index": 1, "codec_type": "audio"},
                {"index": 2, "codec_type": "subtitle"},
                {"index": 3, "codec_type": "audio"}
            ]))),
        );
        app.cache.insert(
            CacheKey::for_file(&other),
            ProbeOutcome::Video(media(serde_json::json!([
                {"index": 0, "codec_type": "video"}
            ]))),
        );

        let movie_index = app
            .files
            .iter()
            .position(|file| file.display_name == "movie.mkv")
            .unwrap();
        let other_index = app
            .files
            .iter()
            .position(|file| file.display_name == "other.mkv")
            .unwrap();

        app.select_file_position(Some(other_index));
        app.select_file_position(Some(movie_index));

        app.original_stream_order = vec![0, 1, 3, 2];
        app.stream_order = vec![0, 3, 1, 2];
        app.moved_streams = BTreeSet::from([3]);
        app.deleted_streams = BTreeSet::from([2]);
        app.original_default_streams = BTreeSet::from([1]);
        app.default_streams = BTreeSet::from([3]);

        // Act
        app.select_file_position(Some(other_index));
        let summary = app.staged_file_summary(&movie.path);

        // Assert
        assert_that!(summary).contains_exactly_in_given_order([
            "Moving 1 audio track".to_string(),
            "Deleting 1 subtitle track".to_string(),
            "Changing the default audio track".to_string(),
        ]);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn confirm_process_all_scroll_should_move_within_the_clamped_range() {
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        app.set_confirm_process_all_max_scroll(5);

        app.scroll_confirm_process_all_down(2);
        assert_that!(app.confirm_process_all_scroll).is_equal_to(2);

        app.scroll_confirm_process_all_down(10);
        assert_that!(app.confirm_process_all_scroll).is_equal_to(5);

        app.scroll_confirm_process_all_up(2);
        assert_that!(app.confirm_process_all_scroll).is_equal_to(3);

        app.scroll_confirm_process_all_to_start();
        assert_that!(app.confirm_process_all_scroll).is_equal_to(0);

        app.scroll_confirm_process_all_to_end();
        assert_that!(app.confirm_process_all_scroll).is_equal_to(5);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn confirm_process_all_should_route_requests_by_transcode_vs_remux_and_track_batch_items() {
        // Regression test for the pool-classification wiring: a plain staged edit
        // must land in the remux pool, a genuine codec change must land in the
        // transcode pool, and both must show up as Pending batch items.
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-confirm-process-all-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("remux.mkv"), b"media").unwrap();
        std::fs::write(directory.join("transcode.mkv"), b"media").unwrap();

        let (probe_tx, _) = std::sync::mpsc::channel::<ProbeRequest>();
        let (conflict_tx, _) = std::sync::mpsc::channel::<ProbeRequest>();
        let (transcode_tx, transcode_rx) = std::sync::mpsc::channel::<EditRequest>();
        let (remux_tx, remux_rx) = std::sync::mpsc::channel::<EditRequest>();
        let mut app = App::new(
            directory.clone(),
            probe_tx,
            conflict_tx,
            transcode_tx,
            remux_tx,
        )
        .unwrap();
        app.layer = Layer::Streams;

        let streams = serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264", "disposition": {"default": 1}},
            {"index": 1, "codec_type": "audio", "disposition": {"default": 1}}
        ]);
        for name in ["remux.mkv", "transcode.mkv"] {
            let file = app
                .files
                .iter()
                .find(|file| file.display_name == name)
                .unwrap()
                .clone();
            app.cache.insert(
                CacheKey::for_file(&file),
                ProbeOutcome::Video(media(streams.clone())),
            );
        }
        let position_of = |app: &App, name: &str| {
            app.files
                .iter()
                .position(|file| file.display_name == name)
                .unwrap()
        };
        let remux_index = position_of(&app, "remux.mkv");
        let transcode_index = position_of(&app, "transcode.mkv");

        // Whichever file `App::new` auto-selected got probed before the cache above
        // was populated, so its `stream_order` is still the construction-time empty
        // default. Force a real transition away from it first so the next selection
        // below is a genuine change and picks up the now-cached probe data via
        // `queue_probe`'s synchronous cache-hit path.
        app.select_file_position(Some(transcode_index));

        // Stage a plain container change on remux.mkv — no codec change, stays a copy.
        app.select_file_position(Some(remux_index));
        app.container_target = Some(ContainerFormat::Matroska);

        // Stage a genuine codec change on transcode.mkv.
        app.select_file_position(Some(transcode_index));
        app.video_settings.insert(
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
        );

        // Act
        app.request_process_all();
        assert_that!(app.dialog).is_equal_to(Some(Dialog::ConfirmProcessAll));
        app.confirm_process_all();

        // Assert: each request landed in the pool matching its actual work.
        let remux_request = remux_rx
            .try_recv()
            .expect("remux.mkv should route to the remux pool");
        assert_that!(remux_request.path.file_name().unwrap().to_str().unwrap())
            .is_equal_to("remux.mkv");
        assert!(
            remux_rx.try_recv().is_err(),
            "transcode.mkv must not also land in the remux pool"
        );

        let transcode_request = transcode_rx
            .try_recv()
            .expect("transcode.mkv should route to the transcode pool");
        assert_that!(
            transcode_request
                .path
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
        )
        .is_equal_to("transcode.mkv");

        // And the batch dialog now tracks both as pending.
        assert_that!(app.dialog).is_equal_to(Some(Dialog::BatchProcessing));
        let batch = app.active_batch.as_ref().unwrap();
        assert_that!(batch.items.len()).is_equal_to(2);
        assert!(
            batch
                .items
                .iter()
                .all(|item| item.status == crate::staging::BatchItemStatus::Pending)
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn confirm_process_all_should_send_staged_audio_settings_to_the_transcode_worker() {
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-confirm-audio-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("movie.mkv"), b"media").unwrap();
        let (probe_tx, _) = std::sync::mpsc::channel::<ProbeRequest>();
        let (conflict_tx, _) = std::sync::mpsc::channel::<ProbeRequest>();
        let (transcode_tx, transcode_rx) = std::sync::mpsc::channel::<EditRequest>();
        let (remux_tx, remux_rx) = std::sync::mpsc::channel::<EditRequest>();
        let mut app = App::new(
            directory.clone(),
            probe_tx,
            conflict_tx,
            transcode_tx,
            remux_tx,
        )
        .unwrap();
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264",
             "disposition": {"default": 1}},
            {"index": 1, "codec_type": "audio", "codec_name": "aac",
             "channels": 2, "sample_rate": "48000", "tags": {"language": "eng"},
             "disposition": {"default": 1}}
        ]));
        app.outcome = Some(ProbeOutcome::Video(info.clone()));
        app.loading = false;
        app.reset_track_edits();
        app.layer = Layer::Streams;
        app.subtitle_capabilities = full_subtitle_capabilities();
        let file = app.files[0].clone();
        app.cache
            .insert(CacheKey::for_file(&file), ProbeOutcome::Video(info));
        let settings = AudioSettings {
            codec: AudioCodec::Ac3,
            channel_layout: AudioChannelLayout::Mono,
            metadata: AudioMetadata {
                language: "nld".to_string(),
                title: Some("Director commentary".to_string()),
                commentary: true,
                hearing_impaired: false,
                audio_description: false,
                original: false,
                dubbed: false,
            },
        };
        app.audio_settings.insert(1, settings.clone());

        app.request_process_all();
        assert_that!(app.dialog).is_equal_to(Some(Dialog::ConfirmProcessAll));
        app.confirm_process_all();

        let request = transcode_rx
            .try_recv()
            .expect("technical audio settings should route to the transcode worker");
        assert_that!(request.audio_settings.get(&1)).contains(&settings);
        assert!(remux_rx.try_recv().is_err());
        assert_that!(app.dialog).is_equal_to(Some(Dialog::BatchProcessing));

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn confirm_process_all_should_filter_deleted_tracks_out_of_the_dispatched_stream_order() {
        // Regression test: `StagedEdit::stream_order` is the raw display order and
        // still contains deleted tracks (`final_stream_order` is what actually drops
        // them and reconciles group positions) — `staged_file_status` already runs
        // edits through it before validating, which is why a deletion never shows the
        // ⚠ invalid marker. But `confirm_process_all` was sending
        // `staged.stream_order` straight through unfiltered, so the worker's own
        // `validate_edit` saw the deleted track as both kept and deleted and rejected
        // a perfectly valid edit — reported to the user as a spurious "source
        // changed" failure with no way to tell it apart from genuine staleness.
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-confirm-process-all-deletion-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("movie.mkv"), b"media").unwrap();
        std::fs::write(directory.join("other.mkv"), b"media").unwrap();

        let (probe_tx, _) = std::sync::mpsc::channel::<ProbeRequest>();
        let (conflict_tx, _) = std::sync::mpsc::channel::<ProbeRequest>();
        let (transcode_tx, _) = std::sync::mpsc::channel::<EditRequest>();
        let (remux_tx, remux_rx) = std::sync::mpsc::channel::<EditRequest>();
        let mut app = App::new(
            directory.clone(),
            probe_tx,
            conflict_tx,
            transcode_tx,
            remux_tx,
        )
        .unwrap();

        let movie = app
            .files
            .iter()
            .find(|file| file.display_name == "movie.mkv")
            .unwrap()
            .clone();
        app.cache.insert(
            CacheKey::for_file(&movie),
            ProbeOutcome::Video(media(serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264", "disposition": {"default": 1}},
                {"index": 1, "codec_type": "subtitle", "codec_name": "subrip",
                 "tags": {"language": "eng"}, "disposition": {"default": 1}},
                {"index": 2, "codec_type": "subtitle", "codec_name": "ass",
                 "tags": {"language": "eng"}}
            ]))),
        );
        let movie_index = app
            .files
            .iter()
            .position(|file| file.display_name == "movie.mkv")
            .unwrap();
        let other_index = app
            .files
            .iter()
            .position(|file| file.display_name == "other.mkv")
            .unwrap();
        // Force a real transition away from whatever `App::new` auto-selected before
        // the cache above existed, so the next selection genuinely picks up the
        // now-cached probe data via `queue_probe`'s synchronous cache-hit path.
        app.select_file_position(Some(other_index));
        app.select_file_position(Some(movie_index));
        app.layer = Layer::Streams;

        // Delete track 2 (the ASS subtitle) — `stream_order` itself is untouched by
        // this, per `App`'s own model; only `deleted_streams` records it.
        app.deleted_streams.insert(2);
        assert_that!(&app.stream_order).contains(2);

        // Act
        app.request_process_all();
        assert_that!(app.dialog).contains(Dialog::ConfirmProcessAll);
        app.confirm_process_all();

        // Assert: the dispatched request's stream_order must not contain the deleted
        // track — this is exactly what `validate_edit` (in `edit.rs`) requires.
        let request = remux_rx
            .try_recv()
            .expect("movie.mkv should have been dispatched");
        assert_that!(&request.stream_order).does_not_contain(2);
        assert_that!(&request.default_streams).does_not_contain(2);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn confirm_process_all_choice_should_control_whether_activating_starts_or_cancels() {
        // Arrange
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-confirm-process-all-choice-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("movie.mkv"), b"media").unwrap();
        std::fs::write(directory.join("other.mkv"), b"media").unwrap();

        let (probe_tx, _) = std::sync::mpsc::channel::<ProbeRequest>();
        let (conflict_tx, _) = std::sync::mpsc::channel::<ProbeRequest>();
        let (transcode_tx, _) = std::sync::mpsc::channel::<EditRequest>();
        let (remux_tx, remux_rx) = std::sync::mpsc::channel::<EditRequest>();
        let mut app = App::new(
            directory.clone(),
            probe_tx,
            conflict_tx,
            transcode_tx,
            remux_tx,
        )
        .unwrap();

        let movie = app
            .files
            .iter()
            .find(|file| file.display_name == "movie.mkv")
            .unwrap()
            .clone();
        app.cache.insert(
            CacheKey::for_file(&movie),
            ProbeOutcome::Video(media(serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264", "disposition": {"default": 1}}
            ]))),
        );
        let movie_index = app
            .files
            .iter()
            .position(|file| file.display_name == "movie.mkv")
            .unwrap();
        let other_index = app
            .files
            .iter()
            .position(|file| file.display_name == "other.mkv")
            .unwrap();
        // Force a real transition away from whatever `App::new` auto-selected before
        // the cache above existed, so the next selection genuinely picks up the
        // now-cached probe data via `queue_probe`'s synchronous cache-hit path.
        app.select_file_position(Some(other_index));
        app.select_file_position(Some(movie_index));
        app.layer = Layer::Streams;
        app.container_target = Some(ContainerFormat::Mp4);

        app.request_process_all();
        assert_that!(app.dialog).contains(Dialog::ConfirmProcessAll);
        assert_that!(app.confirm_process_all_choice).is_equal_to(ConfirmProcessAllChoice::Start);

        // Act: focus Cancel and activate — dismisses without starting anything.
        app.choose_confirm_process_all(1);
        assert_that!(app.confirm_process_all_choice).is_equal_to(ConfirmProcessAllChoice::Cancel);
        app.activate_confirm_process_all();

        // Assert
        assert_that!(app.dialog).is_none();
        assert!(
            remux_rx.try_recv().is_err(),
            "cancelling the confirm dialog must not dispatch anything"
        );
        assert_that!(app.staged_edits.len()).is_equal_to(1);

        // Act: reopen and activate with Start (the default) focused. Leftover cursor
        // state from an earlier run must not survive into the new batch.
        app.batch_cursor = 3;
        app.batch_scroll = 2;
        app.request_process_all();
        assert_that!(app.confirm_process_all_choice).is_equal_to(ConfirmProcessAllChoice::Start);
        app.activate_confirm_process_all();

        // Assert: this time it actually dispatched.
        assert_that!(app.dialog).is_equal_to(Some(Dialog::BatchProcessing));
        assert_that!(app.batch_cursor).is_equal_to(0);
        assert_that!(app.batch_scroll).is_equal_to(0);
        assert!(remux_rx.try_recv().is_ok());

        std::fs::remove_dir_all(directory).unwrap();
    }

    fn batch_app(count: usize) -> App {
        let mut app = test_file_app(&["a.mkv"]);
        app.dialog = Some(Dialog::BatchProcessing);
        app.active_batch = Some(crate::staging::BatchState {
            cancelled: Arc::new(AtomicBool::new(false)),
            items: (0..count)
                .map(|index| crate::staging::BatchItem {
                    path: app.directory.join(format!("movie{index}.mkv")),
                    label: None,
                    fraction: None,
                    status: crate::staging::BatchItemStatus::Pending,
                    output_path: None,
                })
                .collect(),
            started: std::time::Instant::now(),
        });
        app
    }

    #[test]
    fn batch_cursor_should_clamp_at_both_ends_of_the_file_list() {
        // Arrange
        let mut app = batch_app(4);
        let directory = app.directory.clone();

        // Act / Assert: the cursor stops on the last row rather than running past it.
        app.move_batch_cursor_down(10);
        assert_that!(app.batch_cursor).is_equal_to(3);
        app.move_batch_cursor_up(10);
        assert_that!(app.batch_cursor).is_equal_to(0);
        app.move_batch_cursor_up(1);
        assert_that!(app.batch_cursor).is_equal_to(0);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn batch_cursor_should_not_move_without_an_open_batch_dialog() {
        // Arrange: the right dialog but no batch, then a batch but the wrong dialog.
        let mut app = batch_app(4);
        let directory = app.directory.clone();
        app.active_batch = None;

        // Act / Assert
        app.move_batch_cursor_down(1);
        assert_that!(app.batch_cursor).is_equal_to(0);

        app = batch_app(4);
        app.dialog = None;
        app.move_batch_cursor_down(1);
        assert_that!(app.batch_cursor).is_equal_to(0);

        std::fs::remove_dir_all(app.directory.clone()).unwrap();
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn sync_batch_scroll_should_slide_the_viewport_only_when_the_cursor_leaves_it() {
        // Arrange: eight files, three rows visible.
        let mut app = batch_app(8);
        let directory = app.directory.clone();

        // Act / Assert: movement inside the window leaves the viewport alone.
        app.move_batch_cursor_down(2);
        app.sync_batch_scroll(3);
        assert_that!(app.batch_scroll).is_equal_to(0);

        // One row further pulls the viewport down by exactly one row.
        app.move_batch_cursor_down(1);
        app.sync_batch_scroll(3);
        assert_that!(app.batch_scroll).is_equal_to(1);

        // Jumping to the end stops at the last full window, not past it.
        app.move_batch_cursor_down(10);
        app.sync_batch_scroll(3);
        assert_that!(app.batch_cursor).is_equal_to(7);
        assert_that!(app.batch_scroll).is_equal_to(5);

        // Coming back up drags the viewport with the cursor.
        app.move_batch_cursor_up(7);
        app.sync_batch_scroll(3);
        assert_that!(app.batch_scroll).is_equal_to(0);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn receive_edit_results_should_route_batch_events_by_path_and_summarize_on_completion() {
        let mut app = test_file_app(&["a.mkv", "b.mkv"]);
        let directory = app.directory.clone();
        let files = app.files.clone();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let (notification_tx, notification_rx) = std::sync::mpsc::channel();
        app.set_completion_notification_sender(Some(notification_tx));
        app.dialog = Some(Dialog::BatchProcessing);
        app.active_batch = Some(crate::staging::BatchState {
            cancelled: Arc::new(AtomicBool::new(false)),
            items: vec![
                crate::staging::BatchItem {
                    path: files[0].path.clone(),
                    label: None,
                    fraction: None,
                    status: crate::staging::BatchItemStatus::Pending,
                    output_path: None,
                },
                crate::staging::BatchItem {
                    path: files[1].path.clone(),
                    label: None,
                    fraction: None,
                    status: crate::staging::BatchItemStatus::Pending,
                    output_path: None,
                },
            ],
            started: std::time::Instant::now(),
        });
        // A staged entry for a.mkv, so completion can be checked to remove it.
        app.staged_edits.insert(
            files[0].path.clone(),
            StagedEdit {
                fingerprint: files[0].fingerprint,
                stale: false,
                conflict_groups: Default::default(),
                stream_order: vec![0],
                moved_streams: BTreeSet::new(),
                deleted_streams: BTreeSet::new(),
                default_streams: BTreeSet::new(),
                default_sidecars: BTreeSet::new(),
                audio_settings: BTreeMap::new(),
                video_settings: BTreeMap::new(),
                subtitle_changes: BTreeMap::new(),
                left_subtitle_order: Vec::new(),
                container_target: Some(ContainerFormat::Mp4),
                container_metadata: None,
                original_stream_order: vec![0],
                original_default_streams: BTreeSet::new(),
                track_groups: BTreeMap::new(),
            },
        );

        // Progress for a.mkv only must not affect b.mkv's row.
        result_tx
            .send(EditEvent::Progress {
                path: files[0].path.clone(),
                progress: crate::edit::EditProgress::measured(
                    crate::edit::EditPhase::WriteMedia("Remuxing".to_string()),
                    0.5,
                ),
            })
            .unwrap();
        app.receive_edit_results(&result_rx);
        {
            let batch = app.active_batch.as_ref().unwrap();
            let a = batch
                .items
                .iter()
                .find(|item| item.path == files[0].path)
                .unwrap();
            let b = batch
                .items
                .iter()
                .find(|item| item.path == files[1].path)
                .unwrap();
            assert_that!(a.status.clone()).is_equal_to(crate::staging::BatchItemStatus::Running);
            assert_that!(a.fraction).is_equal_to(Some(0.5));
            assert_that!(b.status.clone()).is_equal_to(crate::staging::BatchItemStatus::Pending);
        }

        // a.mkv finishes successfully; the batch is still open (b.mkv pending).
        result_tx
            .send(EditEvent::Finished {
                path: files[0].path.clone(),
                outcome: EditOutcome::Completed {
                    output_path: directory.join("a.mp4"),
                    media_changed: true,
                },
            })
            .unwrap();
        app.receive_edit_results(&result_rx);
        assert_that!(app.dialog).is_equal_to(Some(Dialog::BatchProcessing));
        assert!(!app.staged_edits.contains_key(&files[0].path));
        assert_eq!(notification_rx.try_recv(), Ok(directory.join("a.mp4")));

        // b.mkv fails; the batch is now fully terminal and must auto-close with a
        // summary notice, since every item has reached a terminal state.
        result_tx
            .send(EditEvent::Finished {
                path: files[1].path.clone(),
                outcome: EditOutcome::Failed("ffmpeg exited with status 1".to_string()),
            })
            .unwrap();
        app.receive_edit_results(&result_rx);
        assert_that!(app.dialog).is_none();
        assert!(app.active_batch.is_none());
        assert_that!(app.notice.as_deref().unwrap()).contains("1 of 2");
        assert_that!(app.notice.as_deref().unwrap()).contains("1 failed");
        assert_eq!(
            notification_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty),
            "failed files must not produce completion notifications"
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_completed_edit_should_not_restage_the_open_file_against_the_output_it_just_wrote() {
        // Regression test for the app accusing itself of an on-disk change: delete a
        // track, save, and the conflict notice opens over the freshly written file
        // demanding the deletion that just succeeded be discarded.
        //
        // An edit lives in two places — `staged_edits` and the open file's live edit
        // fields — and completion only cleared the first. `finish_batch_if_done`
        // rescans the directory right afterwards, `reconcile_files` sees the
        // fingerprint move (this app moved it), and re-stages the live fields against
        // the pre-save fingerprint, which is stale by construction.
        let mut app = test_file_app(&["clip.mkv"]);
        let directory = app.directory.clone();
        let path = app.files[0].path.clone();
        let (result_tx, result_rx) = std::sync::mpsc::channel();

        // Stage a deletion of the audio track through the same seam Ctrl+S uses.
        app.deleted_streams.insert(1);
        app.snapshot_current_edits();
        assert!(app.staged_edits.contains_key(&path));

        app.dialog = Some(Dialog::BatchProcessing);
        app.active_batch = Some(crate::staging::BatchState {
            cancelled: Arc::new(AtomicBool::new(false)),
            items: vec![crate::staging::BatchItem {
                path: path.clone(),
                label: None,
                fraction: None,
                status: crate::staging::BatchItemStatus::Running,
                output_path: None,
            }],
            started: std::time::Instant::now(),
        });

        // What the worker does before reporting success: the file on disk is now the
        // edited output, so its fingerprint no longer matches what was staged against.
        std::fs::write(&path, b"media with one track fewer").unwrap();

        result_tx
            .send(EditEvent::Finished {
                path: path.clone(),
                outcome: EditOutcome::Completed {
                    output_path: path.clone(),
                    media_changed: true,
                },
            })
            .unwrap();
        app.receive_edit_results(&result_rx);

        assert_that!(app.notice.as_deref().unwrap()).contains("saved");
        assert!(
            app.staged_edits.is_empty(),
            "an applied edit must not be re-staged against its own output: {:?}",
            app.staged_edits.keys().collect::<Vec<_>>()
        );
        assert!(
            app.deleted_streams.is_empty(),
            "the applied deletion must not survive in the live edit fields: {:?}",
            app.deleted_streams
        );
        assert!(
            app.pending_conflict_checks.is_empty(),
            "nothing is staged, so no structural re-check should have been dispatched"
        );
        assert!(
            app.conflicting_paths().is_empty(),
            "a clean save must leave nothing to resolve: {:?}",
            app.conflicting_paths()
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn request_cancel_edit_should_prompt_before_flipping_the_shared_cancellation_flag() {
        // Regression test for the "pause and prompt" behavior: Esc from the batch
        // progress dialog must NOT cancel immediately — it opens a confirm prompt,
        // and the batch keeps running (the flag stays unflipped) until the user
        // actually confirms.
        let mut app = test_file_app(&["a.mkv"]);
        let directory = app.directory.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        app.dialog = Some(Dialog::BatchProcessing);
        app.active_batch = Some(crate::staging::BatchState {
            cancelled: cancelled.clone(),
            items: vec![crate::staging::BatchItem {
                path: app.files[0].path.clone(),
                label: None,
                fraction: None,
                status: crate::staging::BatchItemStatus::Running,
                output_path: None,
            }],
            started: std::time::Instant::now(),
        });

        // Act: request cancellation — this must only open the prompt.
        app.request_cancel_edit();
        assert_that!(app.dialog).is_equal_to(Some(Dialog::ConfirmCancel));
        assert!(!cancelled.load(Ordering::Relaxed), "must not cancel yet");

        // Choosing "keep processing" and confirming leaves the batch untouched.
        app.dismiss_cancel_edit();
        assert_that!(app.dialog).is_equal_to(Some(Dialog::BatchProcessing));
        assert!(!cancelled.load(Ordering::Relaxed));

        // Only explicitly confirming "cancel processing" flips the flag.
        app.request_cancel_edit();
        app.choose_cancel_edit(1);
        app.activate_cancel_edit();
        assert!(cancelled.load(Ordering::Relaxed));

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn summarize_batch_outcome_should_read_naturally_for_common_cases() {
        assert_that!(summarize_batch_outcome(1, 1, 0, 0).as_str()).is_equal_to("Processed 1 file.");
        assert_that!(summarize_batch_outcome(3, 3, 0, 0).as_str())
            .is_equal_to("Processed 3 files.");
        assert_that!(summarize_batch_outcome(3, 1, 1, 1).as_str())
            .is_equal_to("Processed 1 of 3 files (1 failed, 1 cancelled).");
        assert_that!(summarize_batch_outcome(2, 1, 0, 1).as_str())
            .is_equal_to("Processed 1 of 2 files (1 cancelled).");
        // A short count with nothing to blame it on still has to read as a sentence
        // rather than trailing an empty "()".
        assert_that!(summarize_batch_outcome(3, 2, 0, 0).as_str())
            .is_equal_to("Processed 2 of 3 files.");
        assert_that!(summarize_batch_outcome(3, 0, 3, 0).as_str())
            .is_equal_to("Processed 0 of 3 files (3 failed).");
    }

    #[test]
    fn an_undetermined_language_should_be_reported_for_whichever_subtitle_still_has_one() {
        // Arrange
        let app = test_file_app(&["movie.mkv"]);
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264"},
            {"index": 1, "codec_type": "subtitle", "codec_name": "subrip",
             "tags": {"language": "und"}},
        ]));
        let undetermined_sidecar = test_sidecar(&app, "movie.und.srt", "und");
        let source = SubtitleSource::Embedded(1);
        let mut retagged = SubtitleMetadata {
            language: "dan".to_string(),
            title: None,
            forced: false,
            cc: false,
            hearing_impaired: false,
            original: false,
            commentary: false,
        };
        let change = |source: SubtitleSource, metadata: SubtitleMetadata| {
            BTreeMap::from([(
                source.clone(),
                SubtitleChange {
                    source,
                    source_format: SubtitleFormat::SubRip,
                    embedded_target: None,
                    export_target: None,
                    import_into_media: false,
                    ocr_language: None,
                    metadata: Some(metadata),
                },
            )])
        };

        // Act
        let embedded =
            subtitle_language_error_for(Some(&info), &BTreeSet::new(), &BTreeMap::new(), &[]);
        let deleted = subtitle_language_error_for(
            Some(&info),
            &BTreeSet::from([1_u64]),
            &BTreeMap::new(),
            &[],
        );
        let relabelled = subtitle_language_error_for(
            Some(&info),
            &BTreeSet::new(),
            &change(source, retagged.clone()),
            &[],
        );
        let sidecar = subtitle_language_error_for(
            None,
            &BTreeSet::new(),
            &BTreeMap::new(),
            std::slice::from_ref(&undetermined_sidecar),
        );
        retagged.language = "nld".to_string();
        let relabelled_sidecar = subtitle_language_error_for(
            None,
            &BTreeSet::new(),
            &change(
                SubtitleSource::Sidecar(undetermined_sidecar.path.clone()),
                retagged,
            ),
            std::slice::from_ref(&undetermined_sidecar),
        );
        let nothing_open =
            subtitle_language_error_for(None, &BTreeSet::new(), &BTreeMap::new(), &[]);

        // Assert
        assert_that!(embedded.as_deref())
            .contains("Choose a language for subtitle track #1; Undetermined is not allowed.");
        assert_that!(deleted).is_none();
        assert_that!(relabelled).is_none();
        assert_that!(sidecar.as_deref())
            .contains("Choose a language for movie.und.srt; Undetermined is not allowed.");
        assert_that!(relabelled_sidecar).is_none();
        assert_that!(nothing_open).is_none();
    }

    #[test]
    fn a_container_conversion_should_report_a_missing_ffmpeg_before_anything_else() {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "vp9"},
        ]));
        let with_muxer = ToolCapabilities {
            ffmpeg: true,
            ffmpeg_muxers: BTreeSet::from(["mp4".to_string()]),
            ..ToolCapabilities::default()
        };
        let conflicts = |capabilities: &ToolCapabilities, info: Option<&MediaInfo>| {
            container_conflicts_for_plan(
                capabilities,
                info,
                &[0],
                &BTreeSet::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                &[],
                ContainerFormat::Mp4,
            )
        };

        // Act
        let no_ffmpeg = conflicts(&ToolCapabilities::default(), Some(&info));
        let no_muxer = conflicts(
            &ToolCapabilities {
                ffmpeg: true,
                ..ToolCapabilities::default()
            },
            Some(&info),
        );
        let unprobed = conflicts(&with_muxer, None);
        let codec = conflicts(&with_muxer, Some(&info));

        // Assert: the tool-level reasons come first, and a file that has not been
        // probed yet contributes no per-track conflicts at all.
        assert_that!(no_ffmpeg[0].as_str()).is_equal_to("FFmpeg is not available in PATH.");
        assert_that!(no_muxer[0].as_str())
            .is_equal_to("The installed FFmpeg build does not provide the MP4 muxer.");
        assert_that!(unprobed).is_empty();
        assert_that!(codec.len()).is_equal_to(1);
        assert_that!(codec[0].as_str()).contains("MP4 can't contain VP9 video track #0.");
    }

    #[test]
    fn reset_track_should_restore_one_tracks_position_without_disturbing_others() {
        // Regression test for the per-track reset algorithm: resetting one track's
        // position must not undo other tracks' independently staged reordering.
        let mut app = test_file_app(&["a.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "disposition": {"default": 1}},
                {"index": 1, "codec_type": "audio", "disposition": {"default": 1}},
                {"index": 2, "codec_type": "audio", "disposition": {"default": 0}},
                {"index": 3, "codec_type": "audio", "disposition": {"default": 0}}
            ]),
        );
        let original = app.stream_order.clone();
        assert_that!(original).is_equal_to(vec![0, 1, 2, 3]);

        // Move track 1 down twice (past 2 and 3), then move track 3 up once. Both are
        // now individually "moved" relative to the original order.
        app.selected_stream = app
            .track_rows()
            .iter()
            .position(|row| *row == TrackRef::Embedded(1))
            .unwrap();
        app.move_selected_stream(1);
        app.selected_stream = app
            .track_rows()
            .iter()
            .position(|row| *row == TrackRef::Embedded(1))
            .unwrap();
        app.move_selected_stream(1);
        app.selected_stream = app
            .track_rows()
            .iter()
            .position(|row| *row == TrackRef::Embedded(3))
            .unwrap();
        app.move_selected_stream(-1);
        let before_reset = app.stream_order.clone();

        // Act: reset only track 1's position.
        app.reset_track(TrackRef::Embedded(1));

        // Assert: only track 1 moved (back to sitting right after track 0, since
        // track 0 is the only original predecessor still present); the OTHER tracks'
        // relative order among themselves is exactly what it was before this reset —
        // that reordering was staged independently and must be untouched by it.
        let after_reset = app.stream_order.clone();
        let others_before: Vec<u64> = before_reset
            .iter()
            .copied()
            .filter(|track| *track != 1)
            .collect();
        let others_after: Vec<u64> = after_reset
            .iter()
            .copied()
            .filter(|track| *track != 1)
            .collect();
        assert_that!(others_after).is_equal_to(others_before);
        assert_that!(after_reset.iter().position(|track| *track == 1)).is_equal_to(Some(1)); // right after track 0, per the algorithm

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn reset_focused_field_should_reset_only_the_highlighted_container_field() {
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([{"index": 0, "codec_type": "video", "codec_name": "h264"}]),
        );
        app.container_target = Some(ContainerFormat::Mp4);
        app.container_metadata = Some(ContainerMetadata {
            title: Some("Custom title".to_string()),
            comment: None,
            date: None,
            genre: None,
            artist: None,
        });
        app.open_container_settings();
        app.container_settings_popup.as_mut().unwrap().field = ContainerSettingsField::Title;

        // Act: reset only the focused Title field.
        app.reset_focused_field();

        // Assert: Title reverted, but the Format field (a different staged edit) is
        // untouched — `r` only resets the one field under the cursor.
        assert_that!(app.container_metadata.is_none()).is_true();
        assert_that!(app.container_target).is_equal_to(Some(ContainerFormat::Mp4));

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn reset_focused_field_should_reset_only_the_highlighted_video_field() {
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264",
                 "width": 1920, "height": 1080}
            ]),
        );
        app.video_settings.insert(
            0,
            VideoSettings {
                codec: VideoCodec::Hevc,
                resolution: VideoResolution::P720,
                metadata: VideoMetadata {
                    language: "und".to_string(),
                    title: None,
                    commentary: false,
                },
                rotation: VideoRotation::None,
            },
        );
        app.selected_stream = app
            .track_rows()
            .iter()
            .position(|row| *row == TrackRef::Embedded(0))
            .unwrap();
        app.open_video_settings();
        app.video_settings_popup.as_mut().unwrap().field = VideoSettingsField::Resolution;

        // Act: reset only the focused Resolution field.
        app.reset_focused_field();

        // Assert: resolution reverted to Original, codec change (a different field)
        // survives.
        let settings = app.video_settings.get(&0).cloned().unwrap();
        assert_that!(settings.resolution).is_equal_to(VideoResolution::Original);
        assert_that!(settings.codec).is_equal_to(VideoCodec::Hevc);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn video_metadata_and_default_should_stage_without_changing_neighboring_video() {
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264",
                 "tags": {"language": "eng"}, "disposition": {"default": 1}},
                {"index": 1, "codec_type": "video", "codec_name": "h264",
                 "tags": {"language": "fra"}, "disposition": {"default": 0}}
            ]),
        );
        focus_track(&mut app, TrackRef::Embedded(1));
        app.open_video_settings();

        app.video_settings_popup.as_mut().unwrap().field = VideoSettingsField::Title;
        app.start_video_title_input();
        for character in "Director's cut".chars() {
            app.input_text_char(character);
        }
        app.escape_video_settings();
        app.video_settings_popup.as_mut().unwrap().field = VideoSettingsField::Language;
        app.activate_video_settings();
        let cursor = app
            .filtered_video_languages()
            .iter()
            .position(|choice| choice.code == "nld")
            .unwrap();
        app.video_settings_popup.as_mut().unwrap().language_cursor = cursor;
        app.activate_video_settings();
        app.video_settings_popup.as_mut().unwrap().field = VideoSettingsField::Default;
        app.activate_video_settings();

        assert_that!(app.video_settings[&1].metadata.title.as_deref()).contains("Director's cut");
        assert_that!(app.video_settings[&1].metadata.language.as_str()).is_equal_to("nld");
        assert_that!(app.video_field_changed(VideoSettingsField::Title)).is_true();
        assert_that!(app.video_field_changed(VideoSettingsField::Language)).is_true();
        assert_that!(app.video_field_changed(VideoSettingsField::Default)).is_true();
        // Toggling default on track 1 clears it from every other video track, so track
        // 0's originally-default flag is gone even though nothing about it was staged.
        assert_that!(app.default_streams.clone()).is_equal_to(BTreeSet::from([1]));
        assert_that!(app.video_settings.contains_key(&0)).is_false();

        // Resetting Title and Language independently leaves the other staged and
        // Default untouched.
        app.video_settings_popup.as_mut().unwrap().field = VideoSettingsField::Title;
        app.reset_focused_field();
        assert_that!(app.video_settings[&1].metadata.title.as_deref()).is_none();
        assert_that!(app.video_settings[&1].metadata.language.as_str()).is_equal_to("nld");
        // Resetting Default reverts only the focused track (1) to its own original
        // disposition (not default); it does not restore track 0's original default,
        // since a reset only ever touches the field under focus.
        app.video_settings_popup.as_mut().unwrap().field = VideoSettingsField::Default;
        app.reset_focused_field();
        assert_that!(app.default_streams.clone()).is_equal_to(BTreeSet::new());

        std::fs::remove_dir_all(directory).unwrap();
    }

    /// The one role a picture track can hold, staged the way audio's roles are: toggled
    /// from the dialog, reverted by `r`, and stored only while it differs from the source.
    #[test]
    fn video_commentary_should_toggle_stage_and_reset_against_the_source_flag() {
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264",
                 "tags": {"language": "eng"}},
                {"index": 1, "codec_type": "video", "codec_name": "h264",
                 "tags": {"language": "eng"}, "disposition": {"comment": 1}}
            ]),
        );

        // A track that starts without the flag: the dialog opens unchecked, one toggle
        // stages it, a second toggle matches the source again and drops the staged edit.
        focus_track(&mut app, TrackRef::Embedded(0));
        app.open_video_settings();
        app.video_settings_popup.as_mut().unwrap().field = VideoSettingsField::Commentary;
        assert_that!(app.effective_video_settings(0).unwrap().metadata.commentary).is_false();
        app.activate_video_settings();
        assert_that!(app.video_settings[&0].metadata.commentary).is_true();
        assert_that!(app.video_field_changed(VideoSettingsField::Commentary)).is_true();
        app.activate_video_settings();
        assert_that!(app.video_settings.contains_key(&0)).is_false();
        assert_that!(app.video_field_changed(VideoSettingsField::Commentary)).is_false();

        // And a track that starts with it: the dialog opens checked, unticking stages the
        // removal, and `r` puts the source's flag back without touching a staged title.
        app.escape_video_settings();
        focus_track(&mut app, TrackRef::Embedded(1));
        app.open_video_settings();
        assert_that!(app.effective_video_settings(1).unwrap().metadata.commentary).is_true();
        app.video_settings_popup.as_mut().unwrap().field = VideoSettingsField::Title;
        app.start_video_title_input();
        for character in "Angle 2".chars() {
            app.input_text_char(character);
        }
        app.escape_video_settings();
        app.video_settings_popup.as_mut().unwrap().field = VideoSettingsField::Commentary;
        app.activate_video_settings();
        assert_that!(app.video_settings[&1].metadata.commentary).is_false();
        app.reset_focused_field();
        assert_that!(app.video_settings[&1].metadata.commentary).is_true();
        assert_that!(app.video_settings[&1].metadata.title.as_deref()).contains("Angle 2");

        // The neighboring video track was never staged by any of it.
        assert_that!(app.video_settings.contains_key(&0)).is_false();

        std::fs::remove_dir_all(directory).unwrap();
    }

    /// Rotation is staged as an absolute angle read from the source, so the dialog opens
    /// on whatever the file already says and `r` puts that value back.
    #[test]
    fn video_rotation_should_stage_from_the_sources_own_angle_and_reset_to_it() {
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264",
                 "width": 1920, "height": 1080,
                 "side_data_list": [{"side_data_type": "Display Matrix", "rotation": -90}]},
                {"index": 1, "codec_type": "video", "codec_name": "h264",
                 "width": 1920, "height": 1080}
            ]),
        );
        focus_track(&mut app, TrackRef::Embedded(0));
        app.open_video_settings();

        // The source's own angle, normalized: ffmpeg writes 270° as -90.
        assert_that!(app.effective_video_settings(0).unwrap().rotation)
            .is_equal_to(VideoRotation::Cw270);
        assert_that!(app.video_settings_popup.as_ref().unwrap().rotation_cursor)
            .is_equal_to(VideoRotation::ALL.len() - 1);

        // Staging a different angle, chosen from the dropdown.
        app.video_settings_popup.as_mut().unwrap().field = VideoSettingsField::Rotation;
        app.activate_video_settings();
        assert_that!(app.video_settings_popup.as_ref().unwrap().mode)
            .is_equal_to(VideoSettingsMode::Dropdown);
        app.video_settings_popup.as_mut().unwrap().rotation_cursor = 1;
        app.activate_video_settings();
        assert_that!(app.video_settings[&0].rotation).is_equal_to(VideoRotation::Cw90);
        assert_that!(app.video_field_changed(VideoSettingsField::Rotation)).is_true();

        // Choosing the source's own angle again is not an edit at all.
        app.activate_video_settings();
        app.video_settings_popup.as_mut().unwrap().rotation_cursor = VideoRotation::ALL.len() - 1;
        app.activate_video_settings();
        assert_that!(app.video_settings.contains_key(&0)).is_false();
        assert_that!(app.video_field_changed(VideoSettingsField::Rotation)).is_false();

        // Clearing it to upright stages an edit, and `r` restores the source's angle
        // without disturbing a staged codec change.
        app.activate_video_settings();
        app.video_settings_popup.as_mut().unwrap().rotation_cursor = 0;
        app.activate_video_settings();
        assert_that!(app.video_settings[&0].rotation).is_equal_to(VideoRotation::None);
        app.video_settings_popup.as_mut().unwrap().field = VideoSettingsField::Codec;
        app.activate_video_settings();
        let hevc = app
            .video_codec_choices(0)
            .iter()
            .position(|choice| choice.value == VideoCodec::Hevc)
            .unwrap();
        app.video_settings_popup.as_mut().unwrap().codec_cursor = hevc;
        app.activate_video_settings();
        app.video_settings_popup.as_mut().unwrap().field = VideoSettingsField::Rotation;
        app.reset_focused_field();
        assert_that!(app.video_settings[&0].rotation).is_equal_to(VideoRotation::Cw270);
        assert_that!(app.video_settings[&0].codec).is_equal_to(VideoCodec::Hevc);

        // The neighboring video track was never staged by any of it.
        assert_that!(app.video_settings.contains_key(&1)).is_false();
        assert_that!(app.effective_video_settings(1).unwrap().rotation)
            .is_equal_to(VideoRotation::None);

        std::fs::remove_dir_all(directory).unwrap();
    }

    /// WebM always re-encodes to VP9, and an encode bakes the rotation into the picture
    /// instead of tagging it, so there is no matrix left for the container to store.
    #[test]
    fn video_rotation_field_should_only_be_offered_by_containers_that_store_a_matrix() {
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"}
            ]),
        );
        focus_track(&mut app, TrackRef::Embedded(0));
        app.open_video_settings();

        for (container, offered) in [
            (ContainerFormat::Matroska, true),
            (ContainerFormat::Mp4, true),
            (ContainerFormat::Mov, true),
            (ContainerFormat::WebM, false),
        ] {
            app.container_target = Some(container);
            assert_eq!(
                app.visible_video_fields()
                    .contains(&VideoSettingsField::Rotation),
                offered,
                "{container:?} should {} the Rotation field",
                if offered { "offer" } else { "hide" }
            );
        }

        std::fs::remove_dir_all(directory).unwrap();
    }

    /// MOV and WebM store no commentary flag, so the field is hidden rather than offering
    /// a tick the remux would silently drop.
    #[test]
    fn video_commentary_field_should_only_be_offered_by_containers_that_store_it() {
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264",
                 "disposition": {"comment": 1}}
            ]),
        );
        focus_track(&mut app, TrackRef::Embedded(0));
        app.open_video_settings();

        for (container, offered) in [
            (ContainerFormat::Matroska, true),
            (ContainerFormat::Mp4, true),
            (ContainerFormat::Mov, false),
            (ContainerFormat::WebM, false),
        ] {
            app.container_target = Some(container);
            assert_eq!(
                app.visible_video_fields()
                    .contains(&VideoSettingsField::Commentary),
                offered,
                "{container:?} should {} the Commentary field",
                if offered { "offer" } else { "hide" }
            );
            // Hiding it never edits the staged model — the container target is not a
            // decision the user made about this flag, and they may change it back.
            assert_that!(app.effective_video_settings(0).unwrap().metadata.commentary).is_true();
            assert_that!(app.video_settings.contains_key(&0)).is_false();
        }

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn reset_focused_field_should_reset_only_the_highlighted_subtitle_field() {
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "subtitle", "codec_name": "subrip",
                 "tags": {"language": "eng"}}
            ]),
        );
        let source = SubtitleSource::Embedded(1);
        let mut metadata = app.subtitle_metadata_for(&source).unwrap();
        metadata.language = "fre".to_string();
        metadata.forced = true;
        app.store_subtitle_metadata(source.clone(), SubtitleFormat::SubRip, metadata);
        assert_that!(app.subtitle_changes.contains_key(&source)).is_true();

        app.selected_stream = app
            .track_rows()
            .iter()
            .position(|row| *row == TrackRef::Embedded(1))
            .unwrap();
        app.open_subtitle_settings(source.clone());
        app.subtitle_settings_popup.as_mut().unwrap().field = SubtitleSettingsField::Language;

        // Act: reset only the focused Language field.
        app.reset_focused_field();

        // Assert: language reverted, but Forced (a different staged field on the same
        // subtitle) survives.
        let current = app.subtitle_metadata_for(&source).unwrap();
        assert_that!(current.language.as_str()).is_equal_to("eng");
        assert_that!(current.forced).is_true();

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn reset_focused_field_should_clear_only_the_subtitle_codec_choice() {
        // Regression test: resetting the Codec field must clear just the staged
        // embedded/export format choice, leaving any other staged field (metadata) on
        // the same subtitle untouched, and must not touch `import_into_media` — a
        // separate concern driven by `transfer_subtitle` (Ctrl-h/l), not this field.
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "subtitle", "codec_name": "subrip",
                 "tags": {"language": "eng"}}
            ]),
        );
        app.subtitle_capabilities = ToolCapabilities {
            ffmpeg: true,
            ffmpeg_encoders: BTreeSet::from(["subrip".to_string(), "ass".to_string()]),
            ..ToolCapabilities::default()
        };
        let source = SubtitleSource::Embedded(1);

        // Stage both a metadata change and a codec change on the same subtitle.
        let mut metadata = app.subtitle_metadata_for(&source).unwrap();
        metadata.language = "fre".to_string();
        app.store_subtitle_metadata(source.clone(), SubtitleFormat::SubRip, metadata);
        let mut change = app.subtitle_change(&source, SubtitleFormat::SubRip);
        change.embedded_target = Some(SubtitleFormat::Ass);
        app.store_subtitle_change(source.clone(), change);
        assert_that!(
            app.subtitle_changes
                .get(&source)
                .and_then(|change| change.embedded_target)
        )
        .is_equal_to(Some(SubtitleFormat::Ass));

        app.selected_stream = app
            .track_rows()
            .iter()
            .position(|row| *row == TrackRef::Embedded(1))
            .unwrap();
        app.open_subtitle_settings(source.clone());
        app.subtitle_settings_popup.as_mut().unwrap().field = SubtitleSettingsField::Codec;

        // Act: reset only the focused Codec field.
        app.reset_focused_field();

        // Assert: codec choice cleared, language change (a different field) survives.
        let remaining_codec = app
            .subtitle_changes
            .get(&source)
            .and_then(|change| change.embedded_target);
        assert_that!(remaining_codec.is_none()).is_true();
        let current = app.subtitle_metadata_for(&source).unwrap();
        assert_that!(current.language.as_str()).is_equal_to("fra");

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn request_reset_file_should_unstage_a_non_open_file_after_confirmation() {
        let mut app = test_file_app(&["a.mkv", "b.mkv"]);
        let directory = app.directory.clone();
        let files = app.files.clone();
        for file in &files {
            app.cache.insert(
                CacheKey::for_file(file),
                ProbeOutcome::Video(media(serde_json::json!([
                    {"index": 0, "codec_type": "video", "disposition": {"default": 1}}
                ]))),
            );
        }
        app.select_file_position(Some(1));
        app.select_file_position(Some(0));
        app.container_target = Some(ContainerFormat::Matroska);
        app.select_file_position(Some(1));
        assert_that!(app.staged_edits.contains_key(&files[0].path)).is_true();

        // Act: request + confirm resetting a.mkv specifically, while b.mkv is open.
        app.request_reset_file(files[0].path.clone());
        assert_that!(app.dialog).is_equal_to(Some(Dialog::ConfirmReset));
        app.choose_reset(1); // "Reset edits"
        app.activate_reset();

        // Assert
        assert_that!(app.staged_edits.contains_key(&files[0].path)).is_false();
        assert_that!(app.dialog.is_none()).is_true();

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn activate_reset_should_keep_edits_unless_reset_is_explicitly_chosen() {
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([{"index": 0, "codec_type": "video", "codec_name": "h264"}]),
        );
        app.container_target = Some(ContainerFormat::Mp4);

        // Act: open the confirm-current-file dialog but confirm with the default
        // ("keep edits") choice.
        app.request_reset_current_file();
        assert_that!(app.reset_choice).is_equal_to(ResetChoice::KeepEdits);
        app.activate_reset();

        // Assert: nothing was discarded.
        assert_that!(app.container_target).is_equal_to(Some(ContainerFormat::Mp4));
        assert_that!(app.dialog.is_none()).is_true();

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn request_save_should_refuse_while_container_conflicts_remain() {
        // Arrange: SubRip cannot go into MP4, so converting the container leaves a
        // conflict the user has to resolve. Saving anyway would silently drop the track.
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "subtitle", "codec_name": "subrip",
                 "tags": {"language": "eng"}}
            ]),
        );
        app.container_target = Some(ContainerFormat::Mp4);
        assert!(!app.selected_container_conflicts().is_empty());

        // Act
        app.request_process_all();

        // Assert: the error dialog, not the confirm dialog.
        assert_that!(app.dialog).is_equal_to(Some(Dialog::Error));
        assert_that!(app.edit_error.as_deref().unwrap()).contains("Resolve the following");

        // And with no edits staged at all, processing is a no-op with an explanation
        // rather than an error dialog.
        let mut app = test_file_app(&["movie.mkv"]);
        app.request_process_all();
        assert_that!(app.dialog).is_none();
        assert_that!(app.notice.as_deref()).is_equal_to(Some("No staged changes to process."));

        std::fs::remove_dir_all(directory).unwrap();
        std::fs::remove_dir_all(app.directory.clone()).unwrap();
    }

    #[test]
    fn receive_edit_results_should_ignore_stale_events_with_no_active_batch() {
        // Arrange: an edit worker that is still reporting after its batch already
        // finished (and `active_batch` was cleared) — exactly what a straggling event
        // from a just-completed or just-cancelled job looks like.
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        app.dialog = None;
        app.active_batch = None;

        // Act: with no active batch, the event has nothing to attribute to and is
        // discarded.
        tx.send(EditEvent::Progress {
            path: app.directory.join("movie.mkv"),
            progress: crate::edit::EditProgress {
                phase: crate::edit::EditPhase::WriteMedia("movie.mkv".to_string()),
                fraction: Some(0.5),
            },
        })
        .unwrap();
        app.receive_edit_results(&rx);
        assert!(app.active_batch.is_none());

        // With an active batch, the same shape of event updates the matching item.
        app.dialog = Some(Dialog::BatchProcessing);
        app.active_batch = Some(crate::staging::BatchState {
            cancelled: Arc::new(AtomicBool::new(false)),
            items: vec![crate::staging::BatchItem {
                path: app.directory.join("movie.mkv"),
                label: None,
                fraction: None,
                status: crate::staging::BatchItemStatus::Pending,
                output_path: None,
            }],
            started: std::time::Instant::now(),
        });
        tx.send(EditEvent::Progress {
            path: app.directory.join("movie.mkv"),
            progress: crate::edit::EditProgress {
                phase: crate::edit::EditPhase::WriteMedia("movie.mkv".to_string()),
                fraction: Some(0.5),
            },
        })
        .unwrap();
        app.receive_edit_results(&rx);
        let item = &app.active_batch.as_ref().unwrap().items[0];
        assert_that!(item.fraction).is_equal_to(Some(0.5));

        std::fs::remove_dir_all(directory).unwrap();
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
        // The attachment at index 4 is not a row, so this is already the last one.
        app.select_next();
        assert_that!(app.selected_track()).contains(TrackRef::Embedded(3));

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn attachments_and_data_streams_should_not_be_selectable_rows() {
        // Arrange: the overview lists the container, video, audio and subtitles only, so
        // anything else must be absent from navigation too — a row the cursor can reach
        // but the renderer never draws would leave the selection invisible.
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video"},
                {"index": 1, "codec_type": "audio"},
                {"index": 2, "codec_type": "subtitle"},
                {"index": 3, "codec_type": "attachment"},
                {"index": 4, "codec_type": "data"}
            ]),
        );

        // Act
        let rows = app.track_rows();

        // Assert
        assert_that!(rows).is_equal_to(vec![
            TrackRef::Container,
            TrackRef::Embedded(0),
            TrackRef::Embedded(1),
            TrackRef::Embedded(2),
        ]);

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn opening_settings_on_an_audio_track_should_open_the_audio_editor() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video"},
                {"index": 1, "codec_type": "audio", "codec_name": "aac",
                 "channels": 2, "sample_rate": "48000", "tags": {"language": "eng"}}
            ]),
        );
        app.layer = Layer::Streams;
        app.selected_stream = app
            .track_rows()
            .iter()
            .position(|row| *row == TrackRef::Embedded(1))
            .unwrap();

        // Act
        app.open_track_settings();

        // Assert
        assert_that!(app.notice).is_none();
        assert_that!(app.dialog).is_equal_to(Some(Dialog::AudioSettings));
        assert_that!(
            app.audio_settings_popup
                .as_ref()
                .map(|popup| popup.stream_index)
        )
        .is_equal_to(Some(1));

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn audio_test_app(streams: serde_json::Value, index: u64) -> App {
        let mut app = test_file_app(&["movie.mkv"]);
        set_media(&mut app, streams);
        app.subtitle_capabilities = full_subtitle_capabilities();
        focus_track(&mut app, TrackRef::Embedded(index));
        app
    }

    fn empty_audio_metadata() -> AudioMetadata {
        AudioMetadata {
            language: "und".to_string(),
            title: None,
            commentary: false,
            hearing_impaired: false,
            audio_description: false,
            original: false,
            dubbed: false,
        }
    }

    fn automatic_audio_settings() -> AudioSettings {
        AudioSettings {
            codec: AudioCodec::Original,
            channel_layout: AudioChannelLayout::Original,
            metadata: empty_audio_metadata(),
        }
    }

    #[test]
    fn audio_choices_should_keep_upmixing_visible_but_disabled() {
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "audio", "codec_name": "aac",
                 "channels": 2, "sample_rate": "48000", "tags": {"language": "eng"}}
            ]),
        );
        app.subtitle_capabilities = full_subtitle_capabilities();
        focus_track(&mut app, TrackRef::Embedded(1));
        app.open_audio_settings();

        let layouts = app.audio_channel_choices(1);
        let surround = layouts
            .iter()
            .find(|choice| choice.value == AudioChannelLayout::Surround71)
            .unwrap();
        assert_that!(surround.enabled).is_false();
        assert_that!(surround.reason.as_deref()).contains(CHANNEL_UPMIX_NOT_IMPLEMENTED);
        assert_that!(layouts.iter().filter(|choice| choice.current).count()).is_equal_to(1);
        assert_eq!(app.visible_audio_fields(), AudioSettingsField::ALL);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn audio_metadata_and_default_should_stage_without_changing_neighboring_audio() {
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "audio", "codec_name": "aac",
                 "channels": 2, "sample_rate": "48000", "tags": {"language": "eng"},
                 "disposition": {"default": 1}},
                {"index": 2, "codec_type": "audio", "codec_name": "aac",
                 "channels": 2, "sample_rate": "48000", "tags": {"language": "fra"},
                 "disposition": {"default": 0}}
            ]),
        );
        app.subtitle_capabilities = full_subtitle_capabilities();
        focus_track(&mut app, TrackRef::Embedded(2));
        app.open_audio_settings();

        app.audio_settings_popup.as_mut().unwrap().field = AudioSettingsField::Title;
        app.start_audio_title_input();
        for character in "Director commentary".chars() {
            app.input_text_char(character);
        }
        app.escape_audio_settings();
        app.audio_settings_popup.as_mut().unwrap().field = AudioSettingsField::Commentary;
        app.activate_audio_settings();
        app.audio_settings_popup.as_mut().unwrap().field = AudioSettingsField::Default;
        app.activate_audio_settings();

        assert_that!(app.audio_settings[&2].metadata.title.as_deref())
            .contains("Director commentary");
        assert_that!(app.audio_settings[&2].metadata.commentary).is_true();
        assert_that!(app.default_streams.clone()).is_equal_to(BTreeSet::from([2]));
        assert_that!(app.audio_settings.contains_key(&1)).is_false();

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn adding_supported_roles_to_original_matroska_audio_should_not_create_a_conflict() {
        // Arrange: this is the real failure shape. The source track is already marked
        // Original; editing any other role causes the complete effective metadata set
        // to be validated, so Matroska must accept that untouched source role too.
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "audio", "codec_name": "aac",
                 "channels": 2, "sample_rate": "48000", "tags": {"language": "eng"},
                 "disposition": {"default": 1, "original": 1}},
                {"index": 2, "codec_type": "audio", "codec_name": "aac",
                 "channels": 2, "sample_rate": "48000", "tags": {"language": "nld"},
                 "disposition": {"default": 0}}
            ]),
        );
        app.subtitle_capabilities = full_subtitle_capabilities();
        focus_track(&mut app, TrackRef::Embedded(1));
        app.open_audio_settings();

        // Act
        for field in [
            AudioSettingsField::Commentary,
            AudioSettingsField::HearingImpaired,
            AudioSettingsField::AudioDescription,
        ] {
            app.audio_settings_popup.as_mut().unwrap().field = field;
            app.activate_audio_settings();
        }

        // Assert: all selected roles and the pre-existing Original role coexist, the
        // neighboring track stays unstaged, and the save-time conflict gate is clear.
        let metadata = &app.audio_settings[&1].metadata;
        assert!(metadata.commentary && metadata.hearing_impaired);
        assert!(metadata.audio_description && metadata.original);
        assert!(!metadata.dubbed);
        assert_that!(app.audio_settings.contains_key(&2)).is_false();
        assert_that!(app.selected_container_conflicts()).is_empty();

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn an_untouched_track_should_not_be_judged_against_a_container_it_already_lives_in() {
        // Regression: `supports_codec` is a conservative shortlist, not a full account of
        // what a container accepts, and plenty of real files carry a codec it omits — an
        // MKV with `dvb_subtitle`, an MP4 with DTS. Judging every surviving stream against
        // the container the file is *already in* turned those into a hard refusal from
        // `validate_staged_edit`, so the file could not be reordered, renamed, or have a
        // default flag flipped at all, over content the edit never touches and ffmpeg
        // would have copied through untouched.
        let mut app = test_file_app(&["movie.mp4"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "audio", "codec_name": "dts",
                 "channels": 6, "sample_rate": "48000"},
                {"index": 2, "codec_type": "audio", "codec_name": "aac",
                 "channels": 2, "sample_rate": "48000"}
            ]),
        );
        app.subtitle_capabilities = full_subtitle_capabilities();
        app.container_target = None;

        // Act: an ordinary edit that says nothing about the DTS track.
        app.default_streams.insert(2);

        // Assert: nothing to report, and specifically not about the untouched track.
        assert_that!(app.selected_container_conflicts()).is_empty();

        // A codec the edit itself chooses is still checked, which is what this gate is
        // for — MP4 cannot carry Vorbis.
        let mut vorbis = app.effective_audio_settings(1).unwrap();
        vorbis.codec = AudioCodec::Vorbis;
        app.audio_settings.insert(1, vorbis);
        assert_that!(app.selected_container_conflicts().join("\n").as_str())
            .contains("audio track #1");

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_stream_reel_cannot_edit_should_still_be_reported_before_the_batch_starts() {
        // The mirror of the case above. A `tmcd` timecode track is not something Reel can
        // drop, convert, or even show a row for, and ffmpeg genuinely refuses to write one
        // into the output — so this is the one shape that has to be refused up front
        // rather than failing mid-batch with a raw muxer error.
        let mut app = test_file_app(&["movie.mp4"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "audio", "codec_name": "aac",
                 "channels": 2, "sample_rate": "48000"},
                {"index": 2, "codec_type": "data", "codec_name": "unknown"}
            ]),
        );
        app.subtitle_capabilities = full_subtitle_capabilities();
        app.container_target = None;
        app.default_streams.insert(1);

        assert_that!(app.selected_container_conflicts().join("\n").as_str())
            .contains("data track #2");

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn mp4_conversion_should_hide_and_clear_unsupported_source_audio_roles() {
        // Arrange: reproduce the reported workflow. The FLAC track starts with both a
        // supported Commentary role and MP4's unsupported Original role, while a
        // neighboring AAC track must remain unstaged and retain its metadata.
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "audio", "codec_name": "flac",
                 "channels": 2, "sample_rate": "48000",
                 "tags": {"language": "eng", "title": "Main mix"},
                 "disposition": {"comment": 1, "original": 1}},
                {"index": 2, "codec_type": "audio", "codec_name": "aac",
                 "channels": 2, "sample_rate": "48000",
                 "tags": {"language": "nld", "title": "Commentary"},
                 "disposition": {"comment": 1}}
            ]),
        );
        app.subtitle_capabilities = full_subtitle_capabilities();
        app.subtitle_capabilities
            .ffmpeg_muxers
            .insert("mp4".to_string());
        app.container_target = Some(ContainerFormat::Mp4);
        focus_track(&mut app, TrackRef::Embedded(1));
        app.open_audio_settings();

        // Assert: MP4 cannot store Original, so the dialog does not offer it — there is
        // nothing for the user to untick. The staged model still carries the source's
        // value, because the container target is not a decision the user made about this
        // flag and they may yet change it back.
        assert_that!(app.visible_audio_fields()).contains(AudioSettingsField::Commentary);
        assert_that!(app.visible_audio_fields()).does_not_contain(AudioSettingsField::Original);
        let effective = app.effective_audio_settings(1).unwrap();
        assert_that!(effective.metadata.original).is_true();
        assert_that!(effective.metadata.commentary).is_true();

        // Merely opening a field and leaving it must not stage anything: the value the
        // dialog showed has to compare equal to the source it came from.
        app.audio_settings_popup.as_mut().unwrap().field = AudioSettingsField::Title;
        app.start_audio_title_input();
        app.escape_audio_settings();
        assert_that!(app.audio_settings.contains_key(&1)).is_false();
        app.audio_settings_popup.as_mut().unwrap().field = AudioSettingsField::Codec;

        // Act: choose the expected lossless MP4 replacement through the popup path.
        app.activate_audio_settings();
        let alac = app
            .audio_codec_choices(1)
            .iter()
            .position(|choice| choice.value == AudioCodec::Alac)
            .unwrap();
        app.audio_settings_popup.as_mut().unwrap().codec_cursor = alac;
        app.activate_audio_settings();

        // Assert: the resulting plan is valid without touching Original manually, and
        // supported metadata on both the edited track and its neighbor is preserved.
        let staged = &app.audio_settings[&1];
        assert_that!(staged.codec).is_equal_to(AudioCodec::Alac);
        assert_that!(staged.metadata.commentary).is_true();
        assert_that!(staged.metadata.title.as_deref()).contains("Main mix");
        // The role MP4 cannot store is dropped when the file is written, not baked into
        // the staged edit: dropping the MP4 target has to bring it back rather than leave
        // it silently cleared in an MKV that could have carried it.
        assert_that!(staged.metadata.original).is_true();
        let mut mp4_metadata = staged.metadata.clone();
        ContainerFormat::Mp4.retain_supported_audio_metadata(&mut mp4_metadata);
        assert_that!(mp4_metadata.original).is_false();
        assert_that!(app.audio_settings.contains_key(&2)).is_false();
        let neighbor = app.effective_audio_settings(2).unwrap();
        assert_that!(neighbor.metadata.language.as_str()).is_equal_to("nld");
        assert_that!(neighbor.metadata.title.as_deref()).contains("Commentary");
        assert_that!(neighbor.metadata.commentary).is_true();
        assert_that!(app.selected_container_conflicts()).is_empty();

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn original_and_dubbed_audio_roles_should_replace_each_other() {
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "audio", "codec_name": "aac",
                 "channels": 2, "sample_rate": "48000", "tags": {"language": "eng"},
                 "disposition": {"original": 1}}
            ]),
        );
        app.subtitle_capabilities = full_subtitle_capabilities();
        focus_track(&mut app, TrackRef::Embedded(1));
        app.open_audio_settings();

        app.audio_settings_popup.as_mut().unwrap().field = AudioSettingsField::Dubbed;
        app.activate_audio_settings();
        let dubbed = &app.audio_settings[&1].metadata;
        assert!(dubbed.dubbed && !dubbed.original);

        app.audio_settings_popup.as_mut().unwrap().field = AudioSettingsField::Original;
        app.activate_audio_settings();
        let original = &app
            .effective_audio_settings(1)
            .expect("the original role should be restored")
            .metadata;
        assert!(original.original && !original.dubbed);
        assert_that!(app.selected_container_conflicts()).is_empty();

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn audio_settings_entry_points_should_cover_every_guard_and_editor_mode() {
        let mut app = audio_test_app(
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "audio", "codec_name": "aac",
                 "channels": 2, "sample_rate": "48000", "tags": {"language": "eng"}}
            ]),
            1,
        );
        let directory = app.directory.clone();

        app.layer = Layer::Files;
        app.open_audio_settings();
        assert_that!(app.audio_settings_popup.is_none()).is_true();
        app.layer = Layer::Streams;
        app.dialog = Some(Dialog::Keybindings);
        app.open_audio_settings();
        assert_that!(app.audio_settings_popup.is_none()).is_true();
        app.dialog = None;
        focus_track(&mut app, TrackRef::Container);
        app.open_audio_settings();
        assert_that!(app.audio_settings_popup.is_none()).is_true();
        focus_track(&mut app, TrackRef::Embedded(1));
        app.deleted_streams.insert(1);
        app.open_audio_settings();
        assert!(
            app.notice
                .as_deref()
                .is_some_and(|notice| notice.contains("Unmark this track"))
        );
        app.deleted_streams.clear();
        focus_track(&mut app, TrackRef::Embedded(0));
        app.open_audio_settings();
        assert!(
            app.notice
                .as_deref()
                .is_some_and(|notice| notice.contains("only available for audio"))
        );

        focus_track(&mut app, TrackRef::Embedded(1));
        app.open_audio_settings();
        assert_that!(app.dialog).contains(Dialog::AudioSettings);
        app.toggle_audio_field_help();
        assert_that!(app.audio_settings_popup.as_ref().unwrap().help_visible).is_true();
        app.toggle_audio_field_help();
        assert_that!(app.audio_settings_popup.as_ref().unwrap().help_visible).is_false();

        app.audio_settings_popup.as_mut().unwrap().field = AudioSettingsField::Codec;
        app.start_audio_title_input();
        assert_that!(app.audio_settings_popup.as_ref().unwrap().mode)
            .is_equal_to(AudioSettingsMode::Summary);
        app.audio_settings_popup.as_mut().unwrap().field = AudioSettingsField::Title;
        app.start_audio_title_input();
        assert_that!(app.audio_settings_popup.as_ref().unwrap().mode)
            .is_equal_to(AudioSettingsMode::TitleEdit);
        app.start_audio_title_input();
        app.escape_audio_settings();
        assert_that!(app.audio_settings_popup.as_ref().unwrap().mode)
            .is_equal_to(AudioSettingsMode::Summary);

        app.start_audio_language_search();
        assert_that!(
            app.audio_settings_popup
                .as_ref()
                .unwrap()
                .language_search
                .is_active
        )
        .is_false();
        app.audio_settings_popup.as_mut().unwrap().mode = AudioSettingsMode::LanguageDropdown;
        app.start_audio_language_search();
        assert_that!(
            app.audio_settings_popup
                .as_ref()
                .unwrap()
                .language_search
                .is_active
        )
        .is_true();
        app.cancel_audio_language_search();
        assert_that!(app.audio_settings_popup.as_ref().unwrap().language_cursor).is_equal_to(0);

        app.audio_settings_popup = None;
        app.toggle_audio_field_help();
        app.start_audio_title_input();
        app.start_audio_language_search();
        app.cancel_audio_language_search();
        app.escape_audio_settings();
        assert_that!(app.dialog).is_none();
        assert_that!(app.filtered_audio_languages()).is_empty();
        assert_that!(app.effective_audio_settings(99)).is_none();

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn audio_choice_builders_should_cover_missing_unknown_and_container_specific_inputs() {
        let mut app = audio_test_app(
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "audio", "codec_name": "mystery",
                 "channels": 3, "bit_rate": true, "tags": {"language": "qaa"}}
            ]),
            1,
        );
        let directory = app.directory.clone();
        app.subtitle_capabilities.ffmpeg_encoders.clear();
        app.container_target = Some(ContainerFormat::WebM);

        let codecs = app.audio_codec_choices(1);
        assert_that!(codecs[0].label.as_str()).is_equal_to("MYSTERY");
        assert_that!(codecs[0].enabled).is_false();
        assert_that!(
            codecs.iter().any(|choice| {
                choice.reason.as_deref() == Some("FFmpeg encoder is unavailable")
            })
        )
        .is_true();
        let layouts = app.audio_channel_choices(1);
        assert_that!(layouts[0].label.as_str()).is_equal_to("3 channels");
        assert_that!(layouts[0].enabled).is_true();
        app.subtitle_capabilities = full_subtitle_capabilities();
        let rejected = app.audio_codec_choices(1);
        assert_that!(rejected.iter().any(|choice| {
            choice.value == AudioCodec::Aac
                && choice.reason.as_deref() == Some("WebM cannot contain AAC audio")
        }))
        .is_true();
        assert_that!(app.audio_codec_choices(99)[0].label.as_str()).is_equal_to("UNKNOWN");
        assert_that!(app.effective_audio_codec_for(99)).is_none();
        assert!(
            app.audio_channel_choices(99)[0]
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("unavailable"))
        );

        app.container_target = Some(ContainerFormat::Matroska);
        app.audio_settings.insert(
            1,
            AudioSettings {
                codec: AudioCodec::Aac,
                channel_layout: AudioChannelLayout::Original,
                metadata: AudioMetadata {
                    language: "zza".to_string(),
                    ..empty_audio_metadata()
                },
            },
        );
        assert_that!(app.audio_language_choices_for(1, "zza").len()).is_equal_to(1);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn audio_navigation_and_activation_should_cover_every_popup_branch() {
        let mut app = audio_test_app(
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "audio", "codec_name": "aac",
                 "channels": 2, "sample_rate": "48000", "tags": {"language": "eng"},
                 "disposition": {"default": 1}},
                {"index": 2, "codec_type": "audio", "codec_name": "aac",
                 "channels": 2, "sample_rate": "48000", "tags": {"language": "nld"}}
            ]),
            1,
        );
        let directory = app.directory.clone();

        app.move_audio_settings_cursor(1);
        app.move_audio_settings_to_endpoint(true);
        app.activate_audio_settings();
        app.open_audio_settings();
        assert_that!(app.active_text_input()).is_none();
        app.move_audio_settings_cursor(1);
        app.move_audio_settings_cursor(-1);
        app.move_audio_settings_to_endpoint(true);
        app.move_audio_settings_to_endpoint(false);

        for field in [AudioSettingsField::Codec, AudioSettingsField::ChannelLayout] {
            let popup = app.audio_settings_popup.as_mut().unwrap();
            popup.field = field;
            popup.mode = AudioSettingsMode::Dropdown;
            app.move_audio_settings_cursor(1);
            app.move_audio_settings_cursor(-1);
            app.move_audio_settings_to_endpoint(true);
            app.move_audio_settings_to_endpoint(false);
        }
        app.audio_settings_popup.as_mut().unwrap().field = AudioSettingsField::Title;
        app.move_audio_settings_cursor(1);
        app.move_audio_settings_to_endpoint(true);

        {
            let popup = app.audio_settings_popup.as_mut().unwrap();
            popup.field = AudioSettingsField::Language;
            popup.mode = AudioSettingsMode::LanguageDropdown;
        }
        app.move_audio_settings_cursor(1);
        app.move_audio_settings_to_endpoint(true);
        app.move_audio_settings_to_endpoint(false);
        app.audio_settings_popup
            .as_mut()
            .unwrap()
            .language_search
            .input
            .value = "no language matches this".to_string();
        app.move_audio_settings_cursor(1);
        app.move_audio_settings_to_endpoint(true);
        app.audio_settings_popup.as_mut().unwrap().mode = AudioSettingsMode::TitleEdit;
        app.move_audio_settings_cursor(1);
        app.move_audio_settings_to_endpoint(true);

        for field in [AudioSettingsField::Codec, AudioSettingsField::ChannelLayout] {
            let popup = app.audio_settings_popup.as_mut().unwrap();
            popup.mode = AudioSettingsMode::Summary;
            popup.field = field;
            app.activate_audio_settings();
            assert_that!(app.audio_settings_popup.as_ref().unwrap().mode)
                .is_equal_to(AudioSettingsMode::Dropdown);
            app.escape_audio_settings();
        }

        app.audio_settings_popup.as_mut().unwrap().field = AudioSettingsField::Language;
        app.activate_audio_settings();
        assert_that!(app.audio_settings_popup.as_ref().unwrap().mode)
            .is_equal_to(AudioSettingsMode::LanguageDropdown);
        app.audio_settings_popup.as_mut().unwrap().language_cursor = 1;
        app.activate_audio_settings();
        assert_that!(app.audio_settings_popup.as_ref().unwrap().mode)
            .is_equal_to(AudioSettingsMode::Summary);
        app.audio_settings_popup.as_mut().unwrap().field = AudioSettingsField::Language;
        app.activate_audio_settings();
        app.audio_settings_popup.as_mut().unwrap().language_cursor = usize::MAX;
        app.activate_audio_settings();

        app.audio_settings_popup.as_mut().unwrap().mode = AudioSettingsMode::Summary;
        app.audio_settings_popup.as_mut().unwrap().field = AudioSettingsField::Title;
        app.activate_audio_settings();
        app.audio_settings_popup.as_mut().unwrap().title_input.value = "  New title  ".to_string();
        app.activate_audio_settings();
        app.escape_audio_settings();
        assert_that!(app.audio_settings[&1].metadata.title.as_deref()).contains("New title");

        app.audio_settings_popup.as_mut().unwrap().field = AudioSettingsField::Default;
        app.activate_audio_settings();
        assert_that!(app.default_streams.contains(&1)).is_false();
        app.activate_audio_settings();
        assert!(app.default_streams.contains(&1));
        for field in [
            AudioSettingsField::Commentary,
            AudioSettingsField::HearingImpaired,
            AudioSettingsField::AudioDescription,
            AudioSettingsField::Original,
            AudioSettingsField::Dubbed,
        ] {
            app.audio_settings_popup.as_mut().unwrap().field = field;
            app.activate_audio_settings();
        }

        app.audio_settings_popup.as_mut().unwrap().mode = AudioSettingsMode::Dropdown;
        app.audio_settings_popup.as_mut().unwrap().field = AudioSettingsField::ChannelLayout;
        app.audio_settings_popup.as_mut().unwrap().channel_cursor = usize::MAX;
        app.activate_audio_settings();
        app.audio_settings_popup.as_mut().unwrap().field = AudioSettingsField::Codec;
        app.audio_settings_popup.as_mut().unwrap().codec_cursor = usize::MAX;
        app.activate_audio_settings();
        app.audio_settings_popup.as_mut().unwrap().field = AudioSettingsField::ChannelLayout;
        let disabled = app
            .audio_channel_choices(1)
            .iter()
            .position(|choice| !choice.enabled)
            .unwrap();
        app.audio_settings_popup.as_mut().unwrap().channel_cursor = disabled;
        app.activate_audio_settings();
        let mono = app
            .audio_channel_choices(1)
            .iter()
            .position(|choice| choice.value == AudioChannelLayout::Mono)
            .unwrap();
        app.audio_settings_popup.as_mut().unwrap().channel_cursor = mono;
        app.activate_audio_settings();
        assert_that!(app.audio_settings[&1].channel_layout).is_equal_to(AudioChannelLayout::Mono);
        app.audio_settings_popup.as_mut().unwrap().mode = AudioSettingsMode::Dropdown;
        app.audio_settings_popup.as_mut().unwrap().field = AudioSettingsField::Language;
        app.activate_audio_settings();
        app.audio_settings_popup.as_mut().unwrap().mode = AudioSettingsMode::TitleEdit;
        app.activate_audio_settings();
        app.audio_settings_popup.as_mut().unwrap().mode = AudioSettingsMode::Dropdown;
        app.audio_settings_popup.as_mut().unwrap().stream_index = 99;
        app.audio_settings_popup.as_mut().unwrap().field = AudioSettingsField::Codec;
        app.activate_audio_settings();
        app.audio_settings_popup.as_mut().unwrap().mode = AudioSettingsMode::Summary;
        app.audio_settings_popup.as_mut().unwrap().field = AudioSettingsField::Commentary;
        app.activate_audio_settings();
        app.audio_settings_popup.as_mut().unwrap().field = AudioSettingsField::Language;
        app.activate_audio_settings();
        assert_that!(app.active_text_input()).is_none();
        app.audio_settings_popup.as_mut().unwrap().language_cursor = 0;
        app.activate_audio_settings();

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn audio_reset_change_detection_escape_and_save_should_cover_all_fields() {
        let mut app = audio_test_app(
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "audio", "codec_name": "aac",
                 "channels": 2, "sample_rate": "48000",
                 "tags": {"language": "eng", "title": "Original"},
                 "disposition": {"default": 1, "original": 1}},
                {"index": 2, "codec_type": "audio", "codec_name": "aac",
                 "channels": 2, "sample_rate": "48000", "tags": {"language": "nld"}}
            ]),
            1,
        );
        let directory = app.directory.clone();
        app.open_audio_settings();
        let changed = AudioSettings {
            codec: AudioCodec::Ac3,
            channel_layout: AudioChannelLayout::Mono,
            metadata: AudioMetadata {
                language: "nld".to_string(),
                title: Some("Changed".to_string()),
                commentary: true,
                hearing_impaired: true,
                audio_description: true,
                original: false,
                dubbed: true,
            },
        };
        app.audio_settings.insert(1, changed.clone());
        app.default_streams.clear();
        for field in AudioSettingsField::ALL {
            assert_that!(app.audio_field_changed(field)).is_true();
        }
        for field in AudioSettingsField::ALL {
            app.audio_settings.insert(1, changed.clone());
            app.default_streams.clear();
            app.audio_settings_popup.as_mut().unwrap().field = field;
            app.reset_focused_field();
        }
        app.audio_settings_popup.as_mut().unwrap().field = AudioSettingsField::Default;
        app.reset_focused_field();
        assert!(app.default_streams.contains(&1));

        app.audio_settings_popup.as_mut().unwrap().stream_index = 2;
        app.default_streams.insert(2);
        app.reset_audio_field(AudioSettingsField::Default);
        assert_that!(app.default_streams.contains(&2)).is_false();
        app.audio_settings_popup.as_mut().unwrap().stream_index = 99;
        app.reset_audio_field(AudioSettingsField::Codec);
        app.audio_settings_popup = None;
        app.reset_audio_field(AudioSettingsField::Codec);
        app.commit_audio_title();
        app.after_text_edit(TextInputSite::AudioLanguageSearch, InputEdit::Changed, None);
        assert_that!(app.audio_field_changed(AudioSettingsField::Codec)).is_false();

        app.audio_settings_popup = Some(AudioSettingsPopup {
            stream_index: 99,
            field: AudioSettingsField::Title,
            mode: AudioSettingsMode::TitleEdit,
            help_visible: false,
            codec_cursor: 0,
            channel_cursor: 0,
            language_cursor: 0,
            language_search: SearchState::default(),
            title_input: TextInputState::new("Missing".to_string()),
        });
        app.commit_audio_title();
        assert_that!(app.audio_field_changed(AudioSettingsField::Title)).is_false();

        focus_track(&mut app, TrackRef::Embedded(0));
        app.open_audio_settings();
        app.audio_settings_popup = Some(AudioSettingsPopup {
            stream_index: 0,
            field: AudioSettingsField::Codec,
            mode: AudioSettingsMode::Summary,
            help_visible: false,
            codec_cursor: 0,
            channel_cursor: 0,
            language_cursor: 0,
            language_search: SearchState::default(),
            title_input: TextInputState::new(String::new()),
        });
        app.audio_settings.insert(0, changed.clone());
        assert_that!(app.audio_field_changed(AudioSettingsField::Codec)).is_false();
        app.audio_settings.remove(&0);

        app.dialog = Some(Dialog::AudioSettings);
        app.audio_settings_popup = None;
        app.reset_focused_field();
        app.dialog = None;

        app.video_settings_popup = None;
        app.move_video_settings_cursor(1);
        app.video_settings_popup = Some(VideoSettingsPopup {
            stream_index: 0,
            field: VideoSettingsField::Codec,
            mode: VideoSettingsMode::Summary,
            help_visible: false,
            codec_cursor: 0,
            resolution_cursor: 0,
            rotation_cursor: 0,
            language_cursor: 0,
            language_search: SearchState::default(),
            title_input: TextInputState::new(String::new()),
            custom_resolution: None,
        });
        app.move_video_settings_cursor(1);
        app.move_video_settings_cursor(1);
        app.move_video_settings_cursor(-1);
        app.video_settings_popup.as_mut().unwrap().mode = VideoSettingsMode::CustomResolution;
        app.video_settings_popup.as_mut().unwrap().custom_resolution = None;
        app.move_video_settings_cursor(1);

        focus_track(&mut app, TrackRef::Embedded(1));
        app.open_audio_settings();
        app.audio_settings_popup.as_mut().unwrap().mode = AudioSettingsMode::Dropdown;
        app.audio_settings_popup
            .as_mut()
            .unwrap()
            .language_search
            .input
            .value = "eng".to_string();
        app.escape_audio_settings();
        assert_that!(app.audio_settings_popup.as_ref().unwrap().mode)
            .is_equal_to(AudioSettingsMode::Summary);
        app.audio_settings_popup.as_mut().unwrap().mode = AudioSettingsMode::LanguageDropdown;
        app.escape_audio_settings();
        app.audio_settings_popup.as_mut().unwrap().mode = AudioSettingsMode::Summary;
        app.escape_audio_settings();
        assert_that!(app.audio_settings_popup.is_none()).is_true();

        focus_track(&mut app, TrackRef::Embedded(1));
        app.open_audio_settings();
        app.audio_settings_popup.as_mut().unwrap().field = AudioSettingsField::Title;
        app.start_audio_title_input();
        app.audio_settings_popup.as_mut().unwrap().title_input.value = "Saved".to_string();
        app.save_from_audio_settings();
        assert_that!(app.audio_settings_popup.is_none()).is_true();
        assert_that!(app.audio_settings[&1].metadata.title.as_deref()).contains("Saved");

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn audio_staged_summary_should_describe_every_technical_and_metadata_shape() {
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264"},
            {"index": 1, "codec_type": "audio", "codec_name": "aac",
             "channels": 2, "sample_rate": "48000", "tags": {"language": "eng"}},
            {"index": 2, "codec_type": "audio", "codec_name": "dts",
             "channels": 2, "sample_rate": "48000", "tags": {"language": "eng"}},
            {"index": 3, "codec_type": "audio", "codec_name": "aac",
             "channels": 2, "sample_rate": "48000", "tags": {"language": "eng"}},
            {"index": 4, "codec_type": "audio", "codec_name": "aac",
             "channels": 2, "sample_rate": "48000", "tags": {"language": "eng"}},
            {"index": 5, "codec_type": "audio", "codec_name": "aac",
             "channels": 2, "sample_rate": "48000", "tags": {"language": "eng"}}
        ]));
        let fingerprint = crate::files::FileFingerprint {
            length: 1,
            modified: None,
        };
        let order = vec![0, 1, 2, 3, 4, 5];
        let mut staged = staged_edit(fingerprint, order.clone());
        staged.original_stream_order = order;
        staged.audio_settings.insert(
            1,
            AudioSettings {
                codec: AudioCodec::Ac3,
                channel_layout: AudioChannelLayout::Mono,
                metadata: AudioMetadata {
                    language: "nld".to_string(),
                    title: Some("Commentary".to_string()),
                    commentary: true,
                    ..empty_audio_metadata()
                },
            },
        );
        staged.audio_settings.insert(
            99,
            AudioSettings {
                codec: AudioCodec::Original,
                channel_layout: AudioChannelLayout::Original,
                metadata: empty_audio_metadata(),
            },
        );
        let source_metadata = AudioMetadata {
            language: "eng".to_string(),
            ..empty_audio_metadata()
        };
        staged.audio_settings.insert(
            98,
            AudioSettings {
                metadata: empty_audio_metadata(),
                ..automatic_audio_settings()
            },
        );
        staged.audio_settings.insert(
            2,
            AudioSettings {
                codec: AudioCodec::Aac,
                metadata: source_metadata.clone(),
                ..automatic_audio_settings()
            },
        );
        staged.audio_settings.insert(
            3,
            AudioSettings {
                channel_layout: AudioChannelLayout::Mono,
                metadata: source_metadata.clone(),
                ..automatic_audio_settings()
            },
        );
        staged.audio_settings.insert(
            5,
            AudioSettings {
                metadata: source_metadata,
                ..automatic_audio_settings()
            },
        );

        let lines = staged_edit_summary_entries(Path::new("movie.mkv"), &info, &staged);
        let rendered = lines
            .iter()
            .map(|(_, line)| line.as_str())
            .collect::<Vec<_>>();
        assert!(
            rendered.contains(&"Encoding audio track #1 as Dolby Digital (AC-3) · Mono · 128 kbps")
        );
        assert!(rendered.contains(&"Updating audio track #1 metadata"));
        assert!(rendered.contains(&"Updating audio track #99 metadata"));
        assert!(rendered.iter().any(|line| line.contains("audio track #2")));
        assert!(rendered.iter().any(|line| line.contains("audio track #3")));
        // #5's staged settings match the source exactly, so it contributes no line.
        assert!(!rendered.iter().any(|line| line.contains("audio track #5")));
    }

    #[test]
    fn staged_audio_validation_should_skip_metadata_only_work_and_require_target_encoders() {
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264"},
            {"index": 1, "codec_type": "audio", "codec_name": "aac",
             "channels": 2, "sample_rate": "48000", "tags": {"language": "eng"}}
        ]));
        let groups = track_groups_for(&info);
        let validate = |settings: AudioSettings| {
            validate_staged_edit(
                &ToolCapabilities::default(),
                Path::new("movie.mkv"),
                &info,
                &[],
                &[0, 1],
                &BTreeSet::new(),
                &BTreeSet::new(),
                &[0, 1],
                &BTreeSet::new(),
                &groups,
                &BTreeSet::new(),
                &BTreeMap::from([(1, settings)]),
                &BTreeMap::new(),
                &BTreeMap::new(),
                None,
            )
        };

        let mut metadata_only = automatic_audio_settings();
        metadata_only.metadata.language = "eng".to_string();
        metadata_only.metadata.title = Some("Commentary".to_string());
        assert!(validate(metadata_only).is_ok());

        let mut technical = automatic_audio_settings();
        technical.codec = AudioCodec::Ac3;
        technical.metadata.language = "eng".to_string();
        let error = match validate(technical) {
            Ok(_) => panic!("missing AC-3 encoder should reject the staged edit"),
            Err(error) => error,
        };
        assert_that!(error.as_str()).contains("Dolby Digital (AC-3) encoder");

        let mut staged = staged_edit(
            crate::files::FileFingerprint {
                length: 1,
                modified: None,
            },
            vec![0, 1],
        );
        staged.original_stream_order = vec![0, 1];
        assert_that!(staged_edit_stages_anything(&staged)).is_false();
        staged.audio_settings.insert(1, automatic_audio_settings());
        assert_that!(staged_edit_stages_anything(&staged)).is_true();
    }

    #[test]
    fn a_sidecars_hearing_impaired_and_cc_checkboxes_should_behave_as_one_choice() {
        // Arrange: a sidecar's filename can only say one accessibility thing — `.cc.`,
        // `.sdh.` and `.hi.` all parse to the same idea. The two checkboxes are therefore
        // folded into one for external subtitles: turning on Hearing impaired must clear
        // CC rather than leave both set, which would round-trip to a filename claiming
        // something the user never chose.
        let mut app = test_file_app(&["movie.mkv", "movie.eng.srt"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video"},
                {"index": 1, "codec_type": "subtitle", "codec_name": "subrip"}
            ]),
        );
        app.sidecars = vec![test_sidecar(&app, "movie.eng.srt", "eng")];
        app.layer = Layer::Streams;
        app.selected_stream = app
            .track_rows()
            .iter()
            .position(|row| *row == TrackRef::Sidecar(0))
            .unwrap();
        app.open_track_settings();
        assert!(
            app.subtitle_settings_popup.is_some(),
            "the sidecar's settings popup should be open",
        );
        let metadata = |app: &App| {
            app.subtitle_metadata_for(&SubtitleSource::Sidecar(app.sidecars[0].path.clone()))
                .unwrap()
        };

        // Act / Assert: from clear, the folded checkbox turns on as hearing-impaired —
        // the spelling this codebase writes into filenames — and leaves CC alone.
        app.toggle_subtitle_checkbox(SubtitleSettingsField::HearingImpaired);
        let turned_on = metadata(&app);
        assert!(turned_on.hearing_impaired, "the flag should be set");
        assert!(!turned_on.cc, "the two must never both be set on a sidecar");

        // Act / Assert: toggling it again clears it.
        app.toggle_subtitle_checkbox(SubtitleSettingsField::HearingImpaired);
        let turned_off = metadata(&app);
        assert!(!turned_off.hearing_impaired && !turned_off.cc);

        // Act / Assert: with CC set instead, the folded checkbox already reads as "on",
        // so pressing it turns the whole accessibility flag off rather than adding a
        // second one — leaving both set would round-trip to a filename claiming a
        // combination the parser cannot express.
        app.toggle_subtitle_checkbox(SubtitleSettingsField::Cc);
        assert!(metadata(&app).cc, "CC should have been set");
        app.toggle_subtitle_checkbox(SubtitleSettingsField::HearingImpaired);
        let collapsed = metadata(&app);
        assert!(
            !collapsed.cc && !collapsed.hearing_impaired,
            "the folded checkbox must clear both, got {collapsed:?}",
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn column_movement_should_refuse_to_push_a_track_further_than_its_own_side() {
        // Arrange: the two columns mean "stays in the media" (left) and "becomes a
        // sidecar" (right). The cursor crosses between them only in the direction that
        // matches where the selected track is actually going — an embedded track already
        // marked for export lives on the right, so pushing it right again is meaningless
        // and must not silently jump the cursor somewhere unrelated.
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        // Two embedded subtitles, so exporting one still leaves the left column
        // populated — an empty column is refused for its own reason, covered separately.
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video"},
                {"index": 1, "codec_type": "subtitle", "codec_name": "subrip"},
                {"index": 2, "codec_type": "subtitle", "codec_name": "subrip"}
            ]),
        );
        app.sidecars = vec![test_sidecar(&app, "movie.eng.srt", "eng")];
        app.set_subtitle_columns_side_by_side(true);
        let select = |app: &mut App, track: TrackRef| {
            app.selected_stream = app
                .track_rows()
                .iter()
                .position(|row| *row == track)
                .unwrap();
        };

        // Act / Assert: an ordinary embedded subtitle sits left — it cannot go further
        // left, and the cursor stays exactly where it was.
        select(&mut app, TrackRef::Embedded(1));
        let before = app.selected_stream;
        assert_that!(app.move_subtitle_column(-1)).is_false();
        assert_eq!(app.selected_stream, before, "a refused move must not move");

        // Act / Assert: an unimported sidecar sits right — it cannot go further right.
        select(&mut app, TrackRef::Sidecar(0));
        let before = app.selected_stream;
        assert_that!(app.move_subtitle_column(1)).is_false();
        assert_eq!(app.selected_stream, before);

        // Act / Assert: once the embedded track is staged for export it belongs to the
        // right column, so the refused direction flips.
        select(&mut app, TrackRef::Embedded(1));
        app.transfer_subtitle(1);
        assert!(
            app.is_stream_exported(1),
            "the track should now be exported"
        );
        select(&mut app, TrackRef::Embedded(1));
        let before = app.selected_stream;
        assert_that!(app.move_subtitle_column(1)).is_false();
        assert_eq!(app.selected_stream, before);
        // And it can now be walked back to the left column.
        assert_that!(app.move_subtitle_column(-1)).is_true();

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn column_movement_should_do_nothing_when_one_column_is_empty() {
        // Arrange: with no sidecars there is no right-hand column to move into. Indexing
        // into the empty target column is what the emptiness guard prevents — without it
        // this panics rather than declining.
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video"},
                {"index": 1, "codec_type": "subtitle"}
            ]),
        );
        app.sidecars = Vec::new();
        app.set_subtitle_columns_side_by_side(true);
        app.selected_stream = app
            .track_rows()
            .iter()
            .position(|row| *row == TrackRef::Embedded(1))
            .unwrap();

        // Act / Assert
        assert_that!(app.move_subtitle_column(1)).is_false();
        assert_that!(app.move_subtitle_column(-1)).is_false();
        assert_that!(app.selected_track()).contains(TrackRef::Embedded(1));

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn column_movement_should_ignore_a_track_that_is_not_a_subtitle() {
        // Arrange: the video and audio rows share the left column visually but have no
        // counterpart on the right. Moving from one must decline rather than drag the
        // cursor onto an unrelated sidecar.
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video"},
                {"index": 1, "codec_type": "audio"},
                {"index": 2, "codec_type": "subtitle"}
            ]),
        );
        app.sidecars = vec![test_sidecar(&app, "movie.eng.srt", "eng")];
        app.set_subtitle_columns_side_by_side(true);

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
            let before = app.selected_stream;
            assert_that!(app.move_subtitle_column(1)).is_false();
            assert_that!(app.move_subtitle_column(-1)).is_false();
            assert_eq!(
                app.selected_stream, before,
                "{track:?} must not move between subtitle columns",
            );
        }

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
        let change = app
            .subtitle_changes
            .get(&SubtitleSource::Embedded(1))
            .unwrap();
        assert_that!(change.export_target).contains(SubtitleFormat::VobSub);
        assert_that!(change.embedded_target).is_none();
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
        assert_that!(app.has_track_edits()).is_true();

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
            app.input_text_char(digit);
        }
        app.finish_custom_resolution_input();
        app.move_video_settings_cursor(1);
        app.start_custom_resolution_input();
        for digit in "720".chars() {
            app.input_text_char(digit);
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
    fn escape_video_settings_should_stage_a_valid_custom_resolution() {
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

        // Assert
        assert_that!(staged).contains(VideoResolution::Custom(CustomResolution {
            width: 1280,
            height: 720,
            scaling: CustomScaling::FitPad,
        }));

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
            app.input_text_char(digit);
        }
        app.finish_custom_resolution_input();
        app.move_video_settings_cursor(1);
        app.start_custom_resolution_input();
        for digit in "720".chars() {
            app.input_text_char(digit);
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
            app.input_text_char(digit);
        }
        app.finish_custom_resolution_input();
        app.move_video_settings_cursor(1);
        app.start_custom_resolution_input();
        for digit in "720".chars() {
            app.input_text_char(digit);
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
            // A real report: 1920x10 is positive, even and not upscaling, so it used to
            // reach ffmpeg and fail there once libx265 was already opening.
            ("1920", "10", "Width and height must be at least 16 pixels."),
            ("10", "1080", "Width and height must be at least 16 pixels."),
            // Odd dimensions in either axis: the yuv420p chroma subsampling every
            // common encoder uses needs both to be even, and each axis is its own
            // condition, so an unguarded one only surfaces once ffmpeg is already running.
            ("1281", "720", "Width and height must be even."),
            ("1280", "721", "Width and height must be even."),
            ("1281", "721", "Width and height must be even."),
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

    /// `gg`/`G` in the custom editor mean whichever list is actually in front of the
    /// user: the scaling dropdown when it is open, the field list when it is not — and
    /// neither while a dimension is being typed, where they are ordinary text.
    #[test]
    fn gg_and_g_in_the_custom_editor_should_address_whichever_list_is_in_front() {
        // Arrange
        let mut app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080}
        ])));
        let directory = app.directory.clone();
        open_custom_resolution_editor(&mut app);
        let draft = |app: &App| {
            app.video_settings_popup
                .as_ref()
                .unwrap()
                .custom_resolution
                .clone()
                .unwrap()
        };

        // Act / Assert: with a dimension being typed, the field list must not move.
        app.start_custom_resolution_input();
        let field_before = draft(&app).field;
        app.move_video_settings_to_endpoint(true);
        assert_that!(draft(&app).field).is_equal_to(field_before);

        // Act / Assert: once typing stops, they walk the field list.
        app.finish_custom_resolution_input();
        app.move_video_settings_to_endpoint(true);
        assert_that!(draft(&app).field).is_equal_to(CustomResolutionField::Scaling);
        app.move_video_settings_to_endpoint(false);
        assert_that!(draft(&app).field).is_equal_to(CustomResolutionField::Width);

        // Act / Assert: with the scaling dropdown open they address its options.
        let popup = app.video_settings_popup.as_mut().unwrap();
        let editor = popup.custom_resolution.as_mut().unwrap();
        editor.field = CustomResolutionField::Scaling;
        editor.scaling_dropdown_open = true;
        app.move_video_settings_to_endpoint(true);
        assert_that!(draft(&app).scaling_cursor)
            .is_equal_to(CustomScaling::OPTIONS.len().saturating_sub(1));
        app.move_video_settings_to_endpoint(false);
        assert_that!(draft(&app).scaling_cursor).is_equal_to(0);
        // The field list is untouched while the dropdown owns the keys.
        assert_that!(draft(&app).field).is_equal_to(CustomResolutionField::Scaling);

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// The custom editor is prefilled with the source dimensions, so pressing Enter on
    /// it unchanged is the easiest thing in the dialog to do by accident. It must stage
    /// "Original" rather than a scale filter that resizes 1920×1080 to 1920×1080 —
    /// which would force a full re-encode for no change at all.
    #[test]
    fn a_custom_size_equal_to_the_source_should_stage_no_scaling_at_all() {
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
        draft.width = TextInputState::new("1920".to_string());
        draft.height = TextInputState::new("1080".to_string());

        // Act: leaving the editor is what commits the draft.
        assert_that!(app.custom_resolution_error()).is_none();
        app.finish_custom_resolution_input();
        app.escape_video_settings();

        // Assert: nothing is staged at all, because staging "resize to the size it
        // already is" would force a full re-encode for no change.
        assert_that!(app.video_settings_popup.as_ref().unwrap().mode)
            .is_equal_to(VideoSettingsMode::Dropdown);
        assert_that!(app.video_settings.is_empty()).is_true();

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn resolution_choices_should_disable_scaling_when_the_source_dimensions_are_unknown() {
        // Arrange: a video stream ffprobe reported without width/height (a partially
        // written or damaged file does this). Every resolution choice is a comparison
        // against the source, so with no source there is nothing to scale relative to.
        // Offering them anyway would let the user stage a downscale that is really an
        // upscale, which only fails once the encoder is running.
        let app = test_app(media(serde_json::json!([
            {"index": 0, "codec_type": "video", "codec_name": "h264"}
        ])));
        let directory = app.directory.clone();

        // Act
        let choices = app.resolution_choices(0);

        // Assert: the choices are still listed (so the user sees why), but none of the
        // scaling ones can be selected, and there are no dimensions to report.
        assert_that!(app.video_source_dimensions(0)).is_none();
        assert!(!choices.is_empty(), "the list must still be shown");
        assert!(
            choices
                .iter()
                .filter(|choice| choice.value
                    != ResolutionChoiceValue::Resolution(VideoResolution::Original))
                .all(|choice| !choice.enabled),
            "no scaling choice may be selectable without source dimensions: {choices:?}",
        );

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

    /// The warning line is all the container list has room for, so each reason has to
    /// collapse to its own one-liner rather than showing the raw per-track conflicts.
    #[test]
    fn container_warning_should_lead_with_the_reason_conversion_is_impossible() {
        // Arrange
        let choice = |conflicts: Vec<String>| ContainerChoice {
            value: Some(ContainerFormat::WebM),
            label: "WebM".to_string(),
            current: false,
            staged: false,
            conflicts,
        };

        // Act
        let none = choice(Vec::new()).warning();
        let no_ffmpeg = choice(vec![
            "FFmpeg is not available; container conversion is disabled.".to_string(),
            "WebM can't contain AAC audio track #1.".to_string(),
        ])
        .warning();
        let no_muxer = choice(vec![
            "This FFmpeg build has no webm muxer.".to_string(),
            "WebM can't contain AAC audio track #1.".to_string(),
        ])
        .warning();
        // A conflict phrased some other way still has to warn about something.
        let unparseable = choice(vec!["Something else went wrong.".to_string()]).warning();
        let several_kinds = choice(vec![
            "WebM can't contain H264 video track #0.".to_string(),
            "WebM can't contain AAC audio track #1.".to_string(),
            "WebM can't contain SUBRIP subtitle track #2.".to_string(),
            "WebM can't contain TTF attachment track #3.".to_string(),
        ])
        .warning();

        // Assert
        assert_that!(none).is_none();
        assert_that!(no_ffmpeg.as_deref()).contains("Can't convert: FFmpeg is not available.");
        assert_that!(no_muxer.as_deref())
            .contains("Can't convert: FFmpeg does not support this container.");
        assert_that!(unparseable.as_deref()).contains("WebM can't contain one or more tracks.");
        assert_that!(several_kinds.as_deref()).contains(
            "WebM can't contain H.264 video or AAC audio or SubRip/SRT subtitles or TTF attachments.",
        );
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
        app.activate_container_settings();
        app.move_container_settings_cursor(1);
        app.activate_container_settings();
        app.close_container_settings();
        app.request_process_all();

        // Assert: batch processing always replaces the original in place — there's no
        // per-batch destination choice.
        assert_that!(app.container_target).contains(ContainerFormat::Mp4);
        assert_that!(app.dialog).contains(Dialog::ConfirmProcessAll);

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
        app.request_process_all();
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
        app.request_process_all();

        // Assert
        assert_that!(app.dialog).contains(Dialog::ConfirmProcessAll);
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
    fn rejected_video_codec_choices_should_name_the_container_that_rejects_them() {
        // Arrange: a codec ffmpeg reports but `reel` has no target for, so it takes the
        // "source codec" arm, plus a target container that cannot hold either it or MPEG-4.
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "mpeg2video"}
            ]),
        );
        app.container_target = Some(ContainerFormat::WebM);

        // Act
        let choices = app.video_codec_choices(0);

        // Assert: every disabled choice explains itself by naming the container, and no
        // choice is disabled without a reason.
        let source = choices
            .iter()
            .find(|choice| choice.label == "MPEG2VIDEO")
            .expect("the unrecognised source codec should still be offered");
        assert_that!(source.enabled).is_false();
        assert_that!(source.reason.as_deref().unwrap())
            .contains("WebM")
            .contains("MPEG2VIDEO");
        for choice in &choices {
            assert_eq!(
                choice.enabled,
                choice.reason.is_none(),
                "{:?} should carry a reason exactly when it is disabled",
                choice.label,
            );
        }

        // And with no target container nothing can be rejected.
        app.container_target = None;
        assert!(
            app.video_codec_choices(0)
                .iter()
                .all(|choice| choice.enabled && choice.reason.is_none()),
        );

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
        app.active_batch = Some(crate::staging::BatchState {
            cancelled: cancelled.clone(),
            items: vec![crate::staging::BatchItem {
                path: directory.join("alpha.mkv"),
                label: None,
                fraction: None,
                status: crate::staging::BatchItemStatus::Running,
                output_path: None,
            }],
            started: std::time::Instant::now(),
        });
        app.dialog = Some(Dialog::BatchProcessing);
        std::fs::remove_file(directory.join("alpha.mkv")).unwrap();
        let (result_tx, result_rx) = std::sync::mpsc::channel();

        // Act: the watcher reports the source removal before the worker result
        app.apply_directory_snapshot(DirectorySnapshot::Files(
            scan_directory(&directory).unwrap(),
        ));

        // Assert: keep ownership with the worker to allow cross-extension replacement
        assert_that!(cancelled.load(Ordering::Relaxed)).is_false();
        assert_that!(app.dialog).contains(Dialog::BatchProcessing);
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
    fn directory_update_should_reload_when_selected_file_changes_with_no_live_edits() {
        // Arrange: nothing staged, so there's nothing to lose — the plain reload path
        // (`clear_edit_state`/`queue_probe`) is still correct here.
        let mut app = test_file_app(&["alpha.mkv"]);
        let directory = app.directory.clone();
        std::fs::write(directory.join("alpha.mkv"), b"changed media contents").unwrap();

        // Act
        app.apply_directory_snapshot(DirectorySnapshot::Files(
            scan_directory(&directory).unwrap(),
        ));

        // Assert
        assert_that!(app.layer).is_equal_to(Layer::Files);
        assert_that!(app.loading).is_true();
        assert_that!(app.notice.as_deref().unwrap()).contains("reloaded");

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn directory_update_should_stage_rather_than_wipe_the_open_files_live_edits_when_it_changes_on_disk()
     {
        // Regression test: previously, the moment the *currently open* file's
        // on-disk fingerprint changed, `reconcile_files` unconditionally called
        // `clear_edit_state()` — silently destroying any live track edits with no
        // conflict prompt at all, and force-resetting `self.layer` back to
        // `Layer::Files` via `queue_probe()` even though the user never navigated
        // away. Now it must route through the same staged-edit conflict pipeline
        // every other staged file already uses: snapshot into `staged_edits`,
        // `stale`, and leave the user right where they were.
        let mut app = test_file_app(&["alpha.mkv"]);
        let directory = app.directory.clone();
        let path = app.files[0].path.clone();
        let old_fingerprint = app.files[0].fingerprint;
        assert_that!(app.staged_edits.contains_key(&path)).is_false();

        app.deleted_streams.insert(2);
        assert_that!(app.has_track_edits()).is_true();
        std::fs::write(directory.join("alpha.mkv"), b"changed media contents").unwrap();

        // Act
        app.apply_directory_snapshot(DirectorySnapshot::Files(
            scan_directory(&directory).unwrap(),
        ));

        // Assert: edits survive, layer is untouched (no forced bounce to Files), and
        // a background conflict probe is now tracking the new fingerprint.
        assert_that!(app.layer).is_equal_to(Layer::Streams);
        assert_that!(app.has_track_edits()).is_true();
        assert_that!(&app.deleted_streams).contains(&2);
        assert_that!(app.notice.as_deref()).is_equal_to(None);
        let staged = app
            .staged_edits
            .get(&path)
            .expect("live edits must be snapshotted, not wiped");
        assert_that!(staged.stale).is_true();
        assert_that!(staged.fingerprint).is_equal_to(old_fingerprint);
        assert_that!(app.pending_conflict_checks.contains_key(&path)).is_true();
        // The file-panel marker must keep reflecting the staged edit rather than
        // reverting to Unstaged.
        match app.staged_file_status(&path) {
            StagedFileStatus::Invalid(_) => {}
            other => panic!("expected Invalid while the check is pending, got {other:?}"),
        }

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
    fn completed_copy_should_refresh_and_select_the_output_file() {
        // Arrange
        let mut app = test_file_app(&["alpha.mkv"]);
        let directory = app.directory.clone();
        let source = directory.join("alpha.mkv");
        let output = directory.join("alpha-edited.mkv");
        std::fs::write(&output, b"edited media").unwrap();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        app.dialog = Some(Dialog::BatchProcessing);
        app.active_batch = Some(crate::staging::BatchState {
            cancelled: Arc::new(AtomicBool::new(false)),
            items: vec![crate::staging::BatchItem {
                path: source.clone(),
                label: None,
                fraction: None,
                status: crate::staging::BatchItemStatus::Running,
                output_path: None,
            }],
            started: std::time::Instant::now(),
        });
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
        app.dialog = Some(Dialog::BatchProcessing);
        app.active_batch = Some(crate::staging::BatchState {
            cancelled: Arc::new(AtomicBool::new(false)),
            items: vec![crate::staging::BatchItem {
                path: source.clone(),
                label: None,
                fraction: None,
                status: crate::staging::BatchItemStatus::Running,
                output_path: None,
            }],
            started: std::time::Instant::now(),
        });
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
        app.active_batch = Some(crate::staging::BatchState {
            cancelled: Arc::new(AtomicBool::new(false)),
            items: vec![crate::staging::BatchItem {
                path: app.directory.join("alpha.mkv"),
                label: None,
                fraction: None,
                status: crate::staging::BatchItemStatus::Running,
                output_path: None,
            }],
            started: std::time::Instant::now(),
        });
        result_tx
            .send(EditEvent::Progress {
                path: app.directory.join("alpha.mkv"),
                progress: crate::edit::EditProgress::measured(
                    crate::edit::EditPhase::WriteMedia("Encoding video".to_string()),
                    0.5,
                ),
            })
            .unwrap();

        // Act
        app.receive_edit_results(&result_rx);

        // Assert
        assert_that!(app.dialog).contains(Dialog::ConfirmCancel);
        let item = &app.active_batch.as_ref().unwrap().items[0];
        assert_that!(item.fraction).contains(0.5);
        assert_that!(item.label.as_deref()).contains("Encoding video");

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
        let item_path = directory.join("movie.mkv");
        app.dialog = Some(Dialog::BatchProcessing);
        app.active_batch = Some(crate::staging::BatchState {
            cancelled: cancelled.clone(),
            items: vec![crate::staging::BatchItem {
                path: item_path.clone(),
                label: None,
                fraction: None,
                status: crate::staging::BatchItemStatus::Running,
                output_path: None,
            }],
            started: std::time::Instant::now(),
        });
        let (result_tx, result_rx) = std::sync::mpsc::channel();

        // Act
        app.cancel_edit();

        // Assert
        assert_that!(cancelled.load(Ordering::Relaxed)).is_true();
        assert_that!(app.dialog).contains(Dialog::BatchProcessing);
        assert_that!(&app.deleted_streams).contains(2);

        // Act
        result_tx
            .send(EditEvent::Finished {
                path: item_path,
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
        app.dialog = Some(Dialog::BatchProcessing);
        app.active_batch = Some(crate::staging::BatchState {
            cancelled: cancelled.clone(),
            items: vec![crate::staging::BatchItem {
                path: directory.join("movie.mkv"),
                label: None,
                fraction: None,
                status: crate::staging::BatchItemStatus::Running,
                output_path: None,
            }],
            started: std::time::Instant::now(),
        });

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
        assert_that!(app.dialog).contains(Dialog::BatchProcessing);

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
        app.active_batch = Some(crate::staging::BatchState {
            cancelled: cancelled.clone(),
            items: vec![crate::staging::BatchItem {
                path: directory.join("movie.mkv"),
                label: None,
                fraction: None,
                status: crate::staging::BatchItemStatus::Running,
                output_path: None,
            }],
            started: std::time::Instant::now(),
        });
        app.dialog = Some(Dialog::ConfirmCancel);
        app.cancel_edit_choice = CancelEditChoice::CancelProcessing;

        // Act
        app.dismiss_cancel_edit();

        // Assert
        assert_that!(app.dialog).contains(Dialog::BatchProcessing);
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

        app.dialog = Some(Dialog::Keybindings);
        app.input_text_char('a');
        app.input_text_char('b');
        assert_eq!(app.keybindings_search.value, "ab");
        assert!(app.keybindings_search.is_active);

        app.backspace_text();
        assert_eq!(app.keybindings_search.value, "a");
        assert!(app.keybindings_search.is_active);

        // Backspacing past the start leaves the bar rather than doing nothing.
        app.backspace_text();
        assert_eq!(app.keybindings_search.value, "");
        assert!(app.keybindings_search.is_active);
        app.backspace_text();
        assert!(!app.keybindings_search.is_active);

        app.start_keybindings_search();
        app.input_text_char('a');
        app.finish_keybindings_search();
        assert!(!app.keybindings_search.is_active);
        assert_eq!(app.keybindings_search.value, "a");

        app.clear_keybindings_search();
        assert!(!app.keybindings_search.is_active);
        assert_eq!(app.keybindings_search.value, "");

        std::fs::remove_dir_all(directory).unwrap();
    }

    /// Drives `app` into edit mode at `site` so the generic text keys resolve there.
    fn enter_text_input(app: &mut App, site: TextInputSite) {
        let mut active = TextInputState::new(String::new());
        active.activate();
        match site {
            TextInputSite::ContainerMetadata => {
                app.container_settings_popup = Some(ContainerSettingsPopup {
                    field: ContainerSettingsField::Title,
                    mode: ContainerSettingsMode::TextEdit,
                    help_visible: false,
                    format_cursor: 0,
                    text_input: active,
                });
            }
            TextInputSite::AudioTitle | TextInputSite::AudioLanguageSearch => {
                let language_search = SearchState {
                    input: active.clone(),
                    match_count: 0,
                    field_width: 0,
                };
                app.audio_settings_popup = Some(AudioSettingsPopup {
                    stream_index: 1,
                    field: AudioSettingsField::Title,
                    mode: if site == TextInputSite::AudioTitle {
                        AudioSettingsMode::TitleEdit
                    } else {
                        AudioSettingsMode::LanguageDropdown
                    },
                    help_visible: false,
                    codec_cursor: 0,
                    channel_cursor: 0,
                    language_cursor: 0,
                    language_search,
                    title_input: active,
                });
            }
            TextInputSite::VideoTitle | TextInputSite::VideoLanguageSearch => {
                let language_search = SearchState {
                    input: active.clone(),
                    match_count: 0,
                    field_width: 0,
                };
                app.video_settings_popup = Some(VideoSettingsPopup {
                    stream_index: 0,
                    field: VideoSettingsField::Title,
                    mode: if site == TextInputSite::VideoTitle {
                        VideoSettingsMode::TitleEdit
                    } else {
                        VideoSettingsMode::LanguageDropdown
                    },
                    help_visible: false,
                    codec_cursor: 0,
                    resolution_cursor: 0,
                    rotation_cursor: 0,
                    language_cursor: 0,
                    language_search,
                    title_input: active,
                    custom_resolution: None,
                });
            }
            TextInputSite::SubtitleTitle | TextInputSite::LanguageSearch => {
                let language_search = SearchState {
                    input: active.clone(),
                    match_count: 0,
                    field_width: 0,
                };
                app.subtitle_settings_popup = Some(SubtitleSettingsPopup {
                    source: SubtitleSource::Embedded(2),
                    source_format: SubtitleFormat::SubRip,
                    field: SubtitleSettingsField::Title,
                    mode: if site == TextInputSite::SubtitleTitle {
                        SubtitleSettingsMode::TitleEdit
                    } else {
                        SubtitleSettingsMode::LanguageDropdown
                    },
                    help_visible: false,
                    codec_cursor: 0,
                    language_cursor: 0,
                    language_search,
                    title_input: active,
                });
            }
            TextInputSite::CustomResolution => {
                app.video_settings_popup = Some(VideoSettingsPopup {
                    stream_index: 0,
                    field: VideoSettingsField::Resolution,
                    mode: VideoSettingsMode::CustomResolution,
                    help_visible: false,
                    codec_cursor: 0,
                    resolution_cursor: 0,
                    rotation_cursor: 0,
                    language_cursor: 0,
                    language_search: SearchState::default(),
                    title_input: TextInputState::new(String::new()),
                    custom_resolution: Some(CustomResolutionDraft {
                        width: active,
                        height: TextInputState::new(String::new()),
                        scaling: crate::edit::CustomScaling::FitPad,
                        field: CustomResolutionField::Width,
                        scaling_cursor: 0,
                        scaling_dropdown_open: false,
                    }),
                });
            }
            TextInputSite::FileSearch => {
                app.layer = Layer::Files;
                app.dialog = None;
                app.start_file_search();
            }
            TextInputSite::KeybindingsSearch => {
                app.dialog = Some(Dialog::Keybindings);
                app.start_keybindings_search();
            }
        }
    }

    const ALL_TEXT_INPUT_SITES: [TextInputSite; 10] = [
        TextInputSite::ContainerMetadata,
        TextInputSite::AudioTitle,
        TextInputSite::AudioLanguageSearch,
        TextInputSite::VideoTitle,
        TextInputSite::VideoLanguageSearch,
        TextInputSite::SubtitleTitle,
        TextInputSite::LanguageSearch,
        TextInputSite::CustomResolution,
        TextInputSite::FileSearch,
        TextInputSite::KeybindingsSearch,
    ];

    #[test]
    fn every_text_input_site_should_resolve_to_its_own_config() {
        for site in ALL_TEXT_INPUT_SITES {
            // Arrange
            let mut app = test_file_app(&["movie.mkv"]);
            let directory = app.directory.clone();

            // Act
            enter_text_input(&mut app, site);

            // Assert
            assert_that!(app.active_text_input()).is_equal_to(Some(site));
            let expected = match site {
                TextInputSite::ContainerMetadata => TextInputConfig::CONTAINER_METADATA,
                TextInputSite::AudioTitle => TextInputConfig::SUBTITLE_TITLE,
                TextInputSite::AudioLanguageSearch => TextInputConfig::LANGUAGE_SEARCH,
                TextInputSite::VideoTitle => TextInputConfig::SUBTITLE_TITLE,
                TextInputSite::VideoLanguageSearch => TextInputConfig::LANGUAGE_SEARCH,
                TextInputSite::SubtitleTitle => TextInputConfig::SUBTITLE_TITLE,
                TextInputSite::LanguageSearch => TextInputConfig::LANGUAGE_SEARCH,
                TextInputSite::CustomResolution => TextInputConfig::RESOLUTION,
                TextInputSite::FileSearch => app.file_search.config(),
                TextInputSite::KeybindingsSearch => app.keybindings_search.config(),
            };
            let (_, config) = app.text_input_mut(site).expect("site should resolve");
            assert_that!(config).is_equal_to(expected);

            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn text_input_keys_should_behave_identically_across_all_six_fields() {
        // Every field goes through one editing path, so the same keys must produce the
        // same value and caret everywhere.
        for site in ALL_TEXT_INPUT_SITES {
            // Arrange
            let mut app = test_file_app(&["movie.mkv"]);
            let directory = app.directory.clone();
            enter_text_input(&mut app, site);

            // Act: type, move left, insert, then delete on both sides of the caret.
            for character in "1234".chars() {
                app.input_text_char(character);
            }
            app.move_text_cursor(-1);
            app.input_text_char('9');
            app.backspace_text();
            app.move_text_home(false);
            app.delete_text();
            app.move_text_home(true);

            // Assert
            let (input, _) = app.text_input_mut(site).expect("site should resolve");
            assert_that!(input.value.as_str()).is_equal_to("234");
            assert_that!(input.cursor).is_equal_to(3);

            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn text_input_sites_should_not_resolve_while_another_dialog_is_open() {
        // Arrange: a file search left active underneath a dialog.
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        enter_text_input(&mut app, TextInputSite::FileSearch);
        assert_that!(app.active_text_input()).is_equal_to(Some(TextInputSite::FileSearch));

        // Act
        app.dialog = Some(Dialog::Keybindings);

        // Assert: keys typed at the dialog must not reach the search bar behind it.
        assert_that!(app.active_text_input()).is_equal_to(None);
        app.input_text_char('z');
        assert_that!(app.file_search.value.as_str()).is_equal_to("");

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn pasting_should_land_as_one_edit_in_every_field() {
        // A bracketed paste bypasses the per-character key path entirely, so it needs
        // its own proof that it reaches all six fields and respects the caret.
        for site in ALL_TEXT_INPUT_SITES {
            // Arrange
            let mut app = test_file_app(&["movie.mkv"]);
            let directory = app.directory.clone();
            enter_text_input(&mut app, site);
            for character in "14".chars() {
                app.input_text_char(character);
            }
            app.move_text_cursor(-1);

            // Act
            app.paste_text("23");

            // Assert: the run went in at the caret, and the caret followed it.
            let (input, _) = app.text_input_mut(site).expect("site should resolve");
            assert_that!(input.value.as_str()).is_equal_to("1234");
            assert_that!(input.cursor).is_equal_to(3);

            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn pasting_should_fold_line_breaks_and_drop_characters_the_field_refuses() {
        // Arrange: a free-text field and a digits-only one.
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        enter_text_input(&mut app, TextInputSite::SubtitleTitle);

        // Act: a two-line clipboard.
        app.paste_text("first\nsecond");

        // Assert: the break became a space rather than gluing the lines together.
        let (input, _) = app
            .text_input_mut(TextInputSite::SubtitleTitle)
            .expect("site should resolve");
        assert_that!(input.value.as_str()).is_equal_to("first second");
        assert_that!(app.text_input_reject(TextInputSite::SubtitleTitle)).is_equal_to(None);

        // Act: the same clipboard into a digits-only field.
        app.subtitle_settings_popup = None;
        enter_text_input(&mut app, TextInputSite::CustomResolution);
        app.paste_text("1a9b2");

        // Assert: the letters are dropped, the digits kept, and the field says why.
        let (input, _) = app
            .text_input_mut(TextInputSite::CustomResolution)
            .expect("site should resolve");
        assert_that!(input.value.as_str()).is_equal_to("192");
        assert_that!(app.text_input_reject(TextInputSite::CustomResolution))
            .is_equal_to(Some(InputReject::Character(CharClass::Digits)));

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn pasting_past_the_length_cap_should_keep_what_fits_and_report_it() {
        // Arrange: the language filter caps at 64 characters.
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        enter_text_input(&mut app, TextInputSite::LanguageSearch);
        let cap = TextInputConfig::LANGUAGE_SEARCH.max_len;

        // Act
        app.paste_text(&"e".repeat(cap + 10));

        // Assert
        let (input, _) = app
            .text_input_mut(TextInputSite::LanguageSearch)
            .expect("site should resolve");
        assert_that!(input.value.chars().count()).is_equal_to(cap);
        assert_that!(app.text_input_reject(TextInputSite::LanguageSearch))
            .is_equal_to(Some(InputReject::Full(cap)));

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_truncated_paste_should_still_run_the_field_s_follow_up() {
        // Arrange: the file panel refilters as a side effect of the query changing, so
        // a paste that was only partly accepted must not skip that step.
        let mut app = test_file_app(&["movie.mkv", "other.mkv"]);
        let directory = app.directory.clone();
        enter_text_input(&mut app, TextInputSite::FileSearch);
        let cap = app.file_search.config().max_len;

        // Act: a run that overflows the cap, whose first characters still match.
        app.paste_text(&format!("movie{}", "x".repeat(cap)));

        // Assert: the query was applied and the panel filtered by it.
        assert_that!(app.text_input_reject(TextInputSite::FileSearch))
            .is_equal_to(Some(InputReject::Full(cap)));
        assert_that!(app.file_search.value.chars().count()).is_equal_to(cap);
        assert_that!(app.file_panel_entries()).is_empty();

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn deleting_a_word_or_a_line_should_work_in_every_field() {
        for site in ALL_TEXT_INPUT_SITES {
            // Arrange
            let mut app = test_file_app(&["movie.mkv"]);
            let directory = app.directory.clone();
            enter_text_input(&mut app, site);
            // Search and numeric fields refuse the space that would otherwise separate
            // the words, so for them a single run is the whole "word".
            let (typed, after_word) = match site {
                TextInputSite::LanguageSearch
                | TextInputSite::AudioLanguageSearch
                | TextInputSite::VideoLanguageSearch => ("onetwo", ""),
                TextInputSite::CustomResolution => ("1234", ""),
                _ => ("one two", "one "),
            };
            for character in typed.chars() {
                app.input_text_char(character);
            }

            // Act / Assert: Ctrl-w takes the last word.
            app.delete_word_before_cursor();
            let (input, _) = app.text_input_mut(site).expect("site should resolve");
            assert_that!(input.value.as_str()).is_equal_to(after_word);

            // Act / Assert: Ctrl-u takes whatever is left behind the caret.
            app.clear_text_to_start();
            let (input, _) = app.text_input_mut(site).expect("site should resolve");
            assert_that!(input.value.as_str()).is_equal_to("");
            assert_that!(input.cursor).is_equal_to(0);
            assert_that!(input.view_offset).is_equal_to(0);

            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn word_deletion_should_leave_the_caret_and_the_window_together() {
        // Arrange: a value long enough that the field has scrolled.
        let config = TextInputConfig::SUBTITLE_TITLE;
        let mut input = TextInputState::new(String::new());
        input.activate();
        for character in "alpha beta gamma delta epsilon zeta".chars() {
            input.insert(character, config);
        }
        assert_that!(input.view_offset).is_greater_than(0);

        // Act: remove the last word, which pulls the caret back inside the window.
        input.delete_word_before(config);

        // Assert: the caret is visible in the window the field will draw.
        assert_that!(input.value.as_str()).is_equal_to("alpha beta gamma delta epsilon ");
        assert_that!(input.cursor).is_greater_than_or_equal_to(input.view_offset);
        assert_that!(input.cursor - input.view_offset)
            .is_less_than_or_equal_to(config.visible_width());
    }

    #[test]
    fn a_refused_keystroke_should_be_reported_until_the_field_accepts_something() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        enter_text_input(&mut app, TextInputSite::CustomResolution);

        // Act / Assert: a letter in a digits-only field explains itself.
        app.input_text_char('x');
        assert_that!(app.text_input_reject(TextInputSite::CustomResolution))
            .is_equal_to(Some(InputReject::Character(CharClass::Digits)));

        // Act / Assert: a benign no-op does not clear the explanation.
        app.backspace_text();
        assert_that!(app.text_input_reject(TextInputSite::CustomResolution))
            .is_equal_to(Some(InputReject::Character(CharClass::Digits)));

        // Act / Assert: an accepted character does.
        app.input_text_char('7');
        assert_that!(app.text_input_reject(TextInputSite::CustomResolution)).is_equal_to(None);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_refusal_should_not_survive_leaving_the_field() {
        // Arrange: refuse a keystroke in the file search.
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        enter_text_input(&mut app, TextInputSite::FileSearch);
        for _ in 0..app.file_search.config().max_len {
            app.input_text_char('a');
        }
        app.input_text_char('a');
        assert_that!(app.text_input_reject(TextInputSite::FileSearch)).is_some();

        // Act / Assert: it belongs to that field alone.
        assert_that!(app.text_input_reject(TextInputSite::KeybindingsSearch)).is_equal_to(None);

        // Act: leave the bar and come back to it.
        app.cancel_file_search();
        app.start_file_search();

        // Assert: a fresh visit starts clean.
        assert_that!(app.text_input_reject(TextInputSite::FileSearch)).is_equal_to(None);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn backspace_on_an_empty_search_should_leave_the_field() {
        for site in [
            TextInputSite::FileSearch,
            TextInputSite::KeybindingsSearch,
            TextInputSite::LanguageSearch,
        ] {
            // Arrange
            let mut app = test_file_app(&["movie.mkv"]);
            let directory = app.directory.clone();
            enter_text_input(&mut app, site);
            app.input_text_char('a');

            // Act: one backspace empties the query, the next leaves the bar.
            app.backspace_text();
            assert_that!(app.active_text_input()).is_equal_to(Some(site));
            app.backspace_text();

            // Assert
            assert_that!(app.active_text_input()).is_equal_to(None);
            if site == TextInputSite::FileSearch {
                assert_that!(app.file_search_origin.as_ref()).is_none();
            }

            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn language_filter_should_reject_whitespace_and_stop_at_sixty_four_characters() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        enter_text_input(&mut app, TextInputSite::LanguageSearch);

        // Act
        app.input_text_char('e');
        app.input_text_char(' ');
        app.input_text_char('n');
        for _ in 0..TextInputConfig::LANGUAGE_SEARCH.max_len {
            app.input_text_char('g');
        }

        // Assert
        let value = &app
            .subtitle_settings_popup
            .as_ref()
            .unwrap()
            .language_search
            .value;
        assert_that!(value.as_str()).starts_with("eng");
        assert_that!(value.chars().count()).is_equal_to(TextInputConfig::LANGUAGE_SEARCH.max_len);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn custom_resolution_should_reject_non_digits_through_the_generic_input_path() {
        // The key layer no longer filters digits; the field's CharClass is the only
        // thing standing between a keystroke and the buffer.
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        enter_text_input(&mut app, TextInputSite::CustomResolution);

        // Act
        for character in "1a9 2".chars() {
            app.input_text_char(character);
        }

        // Assert
        let (input, _) = app
            .text_input_mut(TextInputSite::CustomResolution)
            .expect("width field should resolve");
        assert_that!(input.value.as_str()).is_equal_to("192");

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn saving_container_metadata_should_deactivate_the_text_input() {
        // Regression: the popup left `is_active` set after saving, so the renderer —
        // which takes "being edited" from that flag — kept drawing a caret in Summary.
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        enter_text_input(&mut app, TextInputSite::ContainerMetadata);
        app.input_text_char('X');

        // Act
        app.save_container_text_input();

        // Assert
        let popup = app.container_settings_popup.as_ref().unwrap();
        assert_that!(popup.mode).is_equal_to(ContainerSettingsMode::Summary);
        assert!(!popup.text_input.is_active);
        assert_that!(app.active_text_input()).is_equal_to(None);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn keep_cursor_visible_should_use_the_rendered_field_width() {
        // Regression: every field once scrolled against a hardcoded 42 (or 14) columns
        // while rendering at 32, 48 or 16, so the caret could sit off screen.
        for config in [
            TextInputConfig::CONTAINER_METADATA,
            TextInputConfig::SUBTITLE_TITLE,
            TextInputConfig::LANGUAGE_SEARCH,
            TextInputConfig::RESOLUTION,
        ] {
            // Arrange
            let mut input = TextInputState::new(String::new());
            input.activate();

            // Act: fill the field one character past what it can show.
            for _ in 0..config.visible_width() + 1 {
                input.insert('7', config);
            }

            // Assert: the window scrolled the minimum that keeps the trailing caret on
            // screen — two, since the caret sits one past the last character. Scrolling
            // against 42 columns left the caret outside a 32-wide field and scrolled a
            // 48-wide one further than it needed to.
            assert_that!(input.view_offset).is_equal_to(2);
            assert!(
                input.cursor - input.view_offset < config.visible_width(),
                "caret escaped the {}-wide field",
                config.width,
            );
        }
    }

    #[test]
    fn keep_cursor_visible_should_do_nothing_before_the_viewport_is_measured() {
        // Arrange: a search bar that has not been drawn yet reports a zero width.
        let mut input = TextInputState::new(String::new());
        input.activate();
        let unmeasured = TextInputConfig::search(0);

        // Act
        for character in "query".chars() {
            input.insert(character, unmeasured);
        }

        // Assert: the value stays on screen instead of scrolling itself away.
        assert_that!(input.value.as_str()).is_equal_to("query");
        assert_that!(input.view_offset).is_equal_to(0);
    }

    #[test]
    fn deleting_forward_should_keep_the_cursor_visible() {
        // Regression: delete used to skip the scroll fix that insert and backspace ran.
        // Arrange: scroll the window away from the start.
        let config = TextInputConfig::RESOLUTION;
        let mut input = TextInputState::new(String::new());
        input.activate();
        for _ in 0..config.width * 2 {
            input.insert('7', config);
        }
        assert!(input.view_offset > 0);

        // Act: jump back to the start and delete forward.
        input.move_home(false, config);
        input.delete(config);

        // Assert: the window followed the caret home.
        assert_that!(input.view_offset).is_equal_to(0);
    }

    #[test]
    fn backspace_on_an_empty_metadata_field_should_not_leave_edit_mode() {
        // Arrange
        let config = TextInputConfig::CONTAINER_METADATA;
        let mut input = TextInputState::new(String::new());
        input.activate();

        // Act
        let outcome = input.backspace(config);

        // Assert: only search bars are dismissed by backspacing past the start.
        assert_that!(outcome).is_equal_to(InputEdit::Unchanged);
        assert!(input.is_active);
    }

    #[test]
    fn text_input_state_should_edit_unicode_only_while_active() {
        // Arrange
        let text = TextInputConfig {
            width: 9,
            max_len: 8,
            accepts: CharClass::Text,
            exit_on_empty_backspace: false,
        };
        let digits = TextInputConfig {
            accepts: CharClass::Digits,
            ..text
        };
        let mut input = TextInputState::new("Café".to_string());

        // Act / Assert: navigation mode ignores text.
        assert_that!(input.insert('!', text)).is_equal_to(InputEdit::Unchanged);
        assert_that!(input.value.as_str()).is_equal_to("Café");

        // Act: edit in the middle of a Unicode value.
        input.activate();
        input.move_cursor(-1, text);
        input.insert('!', text);
        input.backspace(text);
        input.move_home(false, text);
        input.delete(text);

        // Assert
        assert_that!(input.value.as_str()).is_equal_to("afé");
        assert_that!(input.cursor).is_equal_to(0);

        // Act / Assert: the field's own character class still filters input, and says so.
        input.move_home(true, text);
        assert_that!(input.insert('x', digits)).is_equal_to(InputEdit::Rejected(
            InputReject::Character(CharClass::Digits),
        ));
        assert_that!(input.insert('2', digits)).is_equal_to(InputEdit::Changed);
        assert_that!(input.value.as_str()).is_equal_to("afé2");
    }

    /// The shared shape of the popup fields (title, container metadata): plain text, a
    /// generous cap, and backspace on empty does nothing.
    fn text_config() -> TextInputConfig {
        TextInputConfig {
            width: 12,
            max_len: 16,
            accepts: CharClass::Text,
            exit_on_empty_backspace: false,
        }
    }

    #[test]
    fn track_group_lists_should_read_as_prose_with_one_trailing_noun() {
        // Arrange / Act / Assert: this string is spliced into the conflict notice and the
        // staged-edit validation message, so each arm of the join is user-visible text.
        // Getting the split wrong produces "video, audio, tracks" or a bare "and audio".
        assert_eq!(describe_track_groups(&BTreeSet::new()), "tracks");
        assert_eq!(
            describe_track_groups(&BTreeSet::from(["audio"])),
            "audio tracks"
        );
        assert_eq!(
            describe_track_groups(&BTreeSet::from(["video", "audio"])),
            "audio and video tracks",
        );
        assert_eq!(
            describe_track_groups(&BTreeSet::from(["video", "audio", "subtitle"])),
            "audio, subtitle and video tracks",
        );
    }

    #[test]
    fn track_counts_should_be_singular_for_one_and_plural_for_the_rest() {
        // Arrange / Act / Assert: "Deleting 1 audio tracks" is the kind of wording slip
        // that reads as a bug in the summary the user confirms a destructive save against.
        assert_eq!(track_count_label("audio", 1), "audio track");
        assert_eq!(track_count_label("audio", 2), "audio tracks");
        assert_eq!(track_count_label("subtitle", 0), "subtitle tracks");
    }

    #[test]
    fn the_edit_summary_should_describe_moves_deletions_and_default_changes_per_group() {
        // Arrange: one file with every kind of staged change at once — an audio track
        // moved, a subtitle deleted, and the default audio switched. The summary is what
        // the user reads before committing a remux, so each change must be listed under
        // its own group rather than collapsed or double-counted.
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video"},
            {"index": 1, "codec_type": "audio"},
            {"index": 2, "codec_type": "audio"},
            {"index": 3, "codec_type": "subtitle"},
            {"index": 4, "codec_type": "subtitle"}
        ]));

        // Act: audio 1 and 2 swapped, subtitle 4 deleted, default audio moved 1 -> 2.
        let lines = edit_summary(
            &info,
            &[0, 1, 2, 3, 4],
            &[0, 2, 1, 3],
            &BTreeSet::from([1, 2]),
            &BTreeSet::from([4]),
            &BTreeSet::from([1]),
            &BTreeSet::from([2]),
        );
        let rendered: Vec<&str> = lines.iter().map(|(_, text)| text.as_str()).collect();

        // Assert: each change is described once, counted correctly, and attributed to the
        // right group — the untouched video track contributes nothing.
        assert_that!(rendered).contains_exactly_in_given_order([
            "Moving 2 audio tracks",
            "Deleting 1 subtitle track",
            "Changing the default audio track",
        ]);
        assert!(
            lines.iter().all(|(group, _)| *group != "video"),
            "an untouched group must not appear: {lines:?}",
        );
    }

    #[test]
    fn the_edit_summary_should_stay_empty_when_nothing_was_actually_staged() {
        // Arrange: a track marked as "moved" that ended up back in its original position —
        // the state left behind by moving a track down and then up again. Reporting it as
        // a change would make an untouched file look edited and offer a pointless remux.
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video"},
            {"index": 1, "codec_type": "audio"},
            {"index": 2, "codec_type": "audio"}
        ]));

        // Act
        let lines = edit_summary(
            &info,
            &[0, 1, 2],
            &[0, 1, 2],
            &BTreeSet::from([1, 2]),
            &BTreeSet::new(),
            &BTreeSet::from([1]),
            &BTreeSet::from([1]),
        );

        // Assert
        assert_that!(lines).is_empty();
    }

    #[test]
    fn a_deleted_track_should_not_also_count_as_a_default_change() {
        // Arrange: deleting the default audio track. The default genuinely goes away, but
        // reporting "Changing the default audio track" alongside "Deleting 1 audio track"
        // would double-report one action — the default filters exclude deleted indices on
        // both sides precisely so this reads as the single deletion it is.
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video"},
            {"index": 1, "codec_type": "audio"},
            {"index": 2, "codec_type": "audio"}
        ]));

        // Act
        let lines = edit_summary(
            &info,
            &[0, 1, 2],
            &[0, 2],
            &BTreeSet::new(),
            &BTreeSet::from([1]),
            &BTreeSet::from([1]),
            &BTreeSet::from([1]),
        );
        let rendered: Vec<&str> = lines.iter().map(|(_, text)| text.as_str()).collect();

        // Assert
        assert_that!(rendered).contains_exactly_in_given_order(["Deleting 1 audio track"]);
    }

    #[test]
    fn a_dialogs_actions_should_be_inert_unless_that_dialog_is_the_one_showing() {
        // Arrange: every confirm dialog's choose/activate/dismiss methods are guarded on
        // its own `Dialog` variant. Those guards are what stop a keystroke arriving for
        // one dialog from driving another — the dangerous shape being an Enter meant for
        // a harmless prompt reaching `activate_reset` (which discards staged edits) or
        // `confirm_process_all` (which launches a batch remux over the user's files).
        // Each is exercised here from a *different* dialog and from no dialog at all.
        for context in [None, Some(Dialog::Keybindings)] {
            let mut app = test_file_app(&["movie.mkv"]);
            let directory = app.directory.clone();
            app.dialog = context;
            // Something staged, so a reset that wrongly fired would have work to undo.
            app.toggle_delete_selected_stream();
            app.layer = Layer::Streams;
            app.selected_stream = 2;
            app.toggle_delete_selected_stream();
            let staged_before = app.deleted_streams.clone();

            // Act: drive every dialog's controls while its dialog is not showing.
            app.choose_confirm_process_all(1);
            app.activate_confirm_process_all();
            app.confirm_process_all();
            app.choose_reset(1);
            app.activate_reset();
            app.dismiss_reset();
            app.choose_cancel_edit(1);
            app.choose_cancel_edit_endpoint(true);
            app.activate_cancel_edit();
            app.dismiss_cancel_edit();

            // Assert: the dialog state is exactly where it started, no batch was launched,
            // and nothing staged was discarded.
            assert_eq!(
                app.dialog, context,
                "no guarded action may open or close a dialog from {context:?}",
            );
            assert!(
                app.active_batch.is_none(),
                "no batch may start from {context:?}",
            );
            assert_eq!(
                app.deleted_streams, staged_before,
                "staged edits must survive stray dialog keys from {context:?}",
            );
            assert_eq!(
                app.confirm_process_all_choice,
                ConfirmProcessAllChoice::Start
            );
            assert_eq!(app.reset_choice, ResetChoice::default());
            assert_eq!(app.cancel_edit_choice, CancelEditChoice::KeepProcessing);

            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn a_zero_direction_should_not_move_any_dialogs_button_focus() {
        // Arrange: the choose_* methods take a signed direction and treat zero as "no
        // movement". Without that guard a zero would fall into the `is_positive()` else
        // branch and silently focus the left-hand button — moving focus onto a
        // destructive choice the user never arrowed to.
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();

        // Act / Assert: process-all, parked on the right-hand button.
        app.dialog = Some(Dialog::ConfirmProcessAll);
        app.choose_confirm_process_all(1);
        assert_eq!(
            app.confirm_process_all_choice,
            ConfirmProcessAllChoice::Cancel
        );
        app.choose_confirm_process_all(0);
        assert_eq!(
            app.confirm_process_all_choice,
            ConfirmProcessAllChoice::Cancel,
            "a zero step must not move process-all focus",
        );

        // Act / Assert: reset.
        app.dialog = Some(Dialog::ConfirmReset);
        app.choose_reset(1);
        assert_eq!(app.reset_choice, ResetChoice::ResetEdits);
        app.choose_reset(0);
        assert_eq!(
            app.reset_choice,
            ResetChoice::ResetEdits,
            "a zero step must not move reset focus",
        );

        // Act / Assert: cancel-processing.
        app.dialog = Some(Dialog::ConfirmCancel);
        app.choose_cancel_edit(1);
        assert_eq!(app.cancel_edit_choice, CancelEditChoice::CancelProcessing);
        app.choose_cancel_edit(0);
        assert_eq!(
            app.cancel_edit_choice,
            CancelEditChoice::CancelProcessing,
            "a zero step must not move cancel focus",
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn dropdown_cursor_should_skip_disabled_entries_without_wrapping_past_either_end() {
        // Arrange: dropdown lists (codecs, containers, scaling modes) grey out choices the
        // current file cannot use, and the cursor has to step over them. Landing on a
        // disabled entry would let Enter stage a codec ffmpeg will refuse; wrapping would
        // send a held arrow key back around the list instead of stopping.
        let enabled = |position: usize| !matches!(position, 1 | 2 | 4);

        // Act / Assert: forward from 0 skips the disabled pair and lands on 3.
        assert_eq!(move_cursor(0, 6, 1, enabled), 3);
        // Backward from 3 skips them again on the way to 0.
        assert_eq!(move_cursor(3, 6, -1, enabled), 0);
        // From the last enabled entry, forward has only a disabled entry left, so it
        // stays put rather than wrapping to the top or stopping on the disabled one.
        assert_eq!(move_cursor(5, 6, 1, enabled), 5);
        assert_eq!(move_cursor(3, 6, 1, enabled), 5);
        // Backward from the first entry cannot go below zero.
        assert_eq!(move_cursor(0, 6, -1, enabled), 0);
    }

    #[test]
    fn dropdown_cursor_should_stand_still_for_an_empty_list_or_a_zero_step() {
        // Arrange / Act / Assert: an empty list has nowhere to move, and a zero direction
        // is not a movement. Both must return the cursor untouched — the loop below them
        // would otherwise never terminate on a zero step.
        assert_eq!(move_cursor(0, 0, 1, |_| true), 0);
        assert_eq!(move_cursor(0, 0, -1, |_| true), 0);
        assert_eq!(move_cursor(2, 5, 0, |_| true), 2);
    }

    #[test]
    fn dropdown_cursor_should_stay_put_when_every_remaining_entry_is_disabled() {
        // Arrange: a list where only the current position is selectable — a container
        // whose every alternative is incompatible with the staged tracks. The cursor has
        // nowhere legal to go and must not creep onto a disabled row.
        let only_middle = |position: usize| position == 2;

        // Act / Assert
        assert_eq!(move_cursor(2, 5, 1, only_middle), 2);
        assert_eq!(move_cursor(2, 5, -1, only_middle), 2);
    }

    #[test]
    fn dropdown_endpoints_should_land_on_the_first_and_last_selectable_entry() {
        // Arrange: `gg`/`G` jump to the ends of a dropdown, which must mean the first and
        // last *selectable* entry — jumping onto a greyed-out row would leave Enter
        // staging something the file cannot accept.
        let enabled = |position: usize| !matches!(position, 0 | 1 | 5);

        // Act / Assert
        assert_eq!(cursor_endpoint(6, false, enabled), Some(2));
        assert_eq!(cursor_endpoint(6, true, enabled), Some(4));

        // With everything selectable the ends are the literal ends.
        assert_eq!(cursor_endpoint(6, false, |_| true), Some(0));
        assert_eq!(cursor_endpoint(6, true, |_| true), Some(5));
    }

    #[test]
    fn dropdown_endpoints_should_report_nothing_when_no_entry_can_be_selected() {
        // Arrange / Act / Assert: an empty list, and a list where every entry is disabled,
        // both have no endpoint to jump to. `None` is what keeps the caller from moving
        // the cursor at all rather than defaulting it to row zero.
        assert_eq!(cursor_endpoint(0, false, |_| true), None);
        assert_eq!(cursor_endpoint(0, true, |_| true), None);
        assert_eq!(cursor_endpoint(4, false, |_| false), None);
        assert_eq!(cursor_endpoint(4, true, |_| false), None);
    }

    #[test]
    fn every_editing_command_should_be_inert_while_the_field_is_in_navigation_mode() {
        // Arrange: a field that has not been activated is showing a value, not editing
        // one — the same keys are navigating the list behind it. Any command that slipped
        // through would edit a title the user is only looking at, silently staging a
        // change they never typed.
        let config = text_config();
        let mut input = TextInputState::new("Chapter One".to_string());
        input.cursor = 5;

        // Act / Assert: every mutating entry point reports Unchanged and leaves the value
        // and cursor exactly as they were.
        for (label, edit) in [
            ("insert_str", input.insert_str("xyz", config)),
            ("delete_word_before", input.delete_word_before(config)),
            ("clear_to_start", input.clear_to_start(config)),
            ("backspace", input.backspace(config)),
            ("delete", input.delete(config)),
            ("move_cursor", input.move_cursor(-1, config)),
            ("move_home", input.move_home(true, config)),
        ] {
            assert_that!(edit).is_equal_to(InputEdit::Unchanged);
            assert_eq!(
                input.value, "Chapter One",
                "{label} must not edit the value"
            );
            assert_eq!(input.cursor, 5, "{label} must not move the cursor");
        }
    }

    #[test]
    fn pasting_should_flatten_carriage_returns_and_tabs_alongside_newlines() {
        // Arrange: `pasting_should_fold_line_breaks_and_drop_characters_the_field_refuses`
        // covers `\n`; the same arm also folds `\r` and `\t`, which is what a clipboard
        // copied from a Windows-authored file or a tab-separated table actually carries.
        // Left raw, they reach ffmpeg as container metadata and break the metadata line.
        let config = text_config();
        let mut input = TextInputState::new(String::new());
        input.activate();

        // Act
        let edit = input.insert_str("a\r\nb\tc", config);

        // Assert: each break became its own space, none were dropped or merged.
        assert_that!(edit).is_equal_to(InputEdit::Changed);
        assert_eq!(input.value, "a  b c");
    }

    #[test]
    fn pasting_a_run_with_nothing_acceptable_should_report_no_change_rather_than_an_edit() {
        // Arrange: a digits-only field receiving a paste of pure letters. Reporting
        // `Changed` would mark the file as edited and redraw for an edit that never
        // happened; `Unchanged` is only correct when there was also nothing to reject.
        let config = TextInputConfig {
            accepts: CharClass::Digits,
            ..text_config()
        };
        let mut input = TextInputState::new("720".to_string());
        input.activate();
        input.move_home(true, config);

        // Act: an empty paste has nothing to accept and nothing to reject.
        let empty = input.insert_str("", config);
        // A paste of only rejected characters has something to report.
        let letters = input.insert_str("abc", config);

        // Assert
        assert_that!(empty).is_equal_to(InputEdit::Unchanged);
        assert_that!(letters).is_equal_to(InputEdit::Rejected(InputReject::Character(
            CharClass::Digits,
        )));
        assert_eq!(input.value, "720");
    }

    #[test]
    fn word_fields_should_refuse_whitespace_that_text_fields_accept() {
        // Arrange: the language search filters a word list, so a space would only ever
        // produce zero matches while looking like a typed character. Plain text fields
        // must still take it.
        assert!(CharClass::Text.accepts(' '));
        assert!(!CharClass::Word.accepts(' '));
        assert!(!CharClass::Word.accepts('\t'));
        assert!(CharClass::Word.accepts('a'));
        // Control characters are refused by both.
        assert!(!CharClass::Text.accepts('\u{7}'));
        assert!(!CharClass::Word.accepts('\u{7}'));
    }

    #[test]
    fn ctrl_w_should_delete_the_trailing_whitespace_and_the_word_behind_it() {
        // Arrange: `Ctrl-w` after a trailing space must consume the space *and* the word
        // before it, as a shell does. Stopping at the space would need two presses to
        // remove one word, which is the bug the whitespace-skipping loop prevents.
        let config = text_config();
        let mut input = TextInputState::new("one two ".to_string());
        input.activate();
        input.move_home(true, config);

        // Act
        let edit = input.delete_word_before(config);

        // Assert
        assert_that!(edit).is_equal_to(InputEdit::Changed);
        assert_eq!(input.value, "one ");
        assert_eq!(input.cursor, 4);

        // Act / Assert: again from the end removes the remaining word and its space.
        input.move_home(true, config);
        input.delete_word_before(config);
        assert_eq!(input.value, "");
        assert_eq!(input.cursor, 0);

        // Act / Assert: with the cursor already at the start there is no word behind it.
        assert_that!(input.delete_word_before(config)).is_equal_to(InputEdit::Unchanged);
    }

    #[test]
    fn ctrl_u_should_clear_only_what_is_behind_the_cursor() {
        // Arrange: `Ctrl-u` clears to the start, leaving anything after the cursor — a
        // regression that cleared the whole field would silently discard the tail the
        // user meant to keep.
        let config = text_config();
        let mut input = TextInputState::new("keep this".to_string());
        input.activate();
        input.move_home(false, config);
        for _ in 0..5 {
            input.move_cursor(1, config);
        }

        // Act
        let edit = input.clear_to_start(config);

        // Assert
        assert_that!(edit).is_equal_to(InputEdit::Changed);
        assert_eq!(input.value, "this");
        assert_eq!(input.cursor, 0);

        // Act / Assert: with the cursor already at the start there is nothing before it.
        assert_that!(input.clear_to_start(config)).is_equal_to(InputEdit::Unchanged);
        assert_eq!(input.value, "this");
    }

    #[test]
    fn delete_should_do_nothing_once_the_cursor_is_past_the_last_character() {
        // Arrange: forward-delete at the end of the value has nothing ahead of it.
        // Indexing one past the end here would panic mid-edit.
        let config = text_config();
        let mut input = TextInputState::new("abc".to_string());
        input.activate();
        input.move_home(true, config);

        // Act / Assert
        assert_that!(input.delete(config)).is_equal_to(InputEdit::Unchanged);
        assert_eq!(input.value, "abc");

        // And from one character back it removes exactly that character.
        input.move_cursor(-1, config);
        assert_that!(input.delete(config)).is_equal_to(InputEdit::Changed);
        assert_eq!(input.value, "ab");
    }

    #[test]
    fn cursor_movement_should_stop_at_both_ends_rather_than_wrapping_or_overflowing() {
        // Arrange: held arrow keys drive the cursor past both ends. Saturating is what
        // keeps `char_byte_index` in range — an overflow or a wrap here is a panic during
        // ordinary typing.
        let config = text_config();
        let mut input = TextInputState::new("ab".to_string());
        input.activate();
        input.move_home(false, config);

        // Act / Assert: left at the start stays put, and still reports Changed because
        // the field is active and consumed the key.
        assert_that!(input.move_cursor(-1, config)).is_equal_to(InputEdit::Changed);
        assert_eq!(input.cursor, 0);

        // Act / Assert: right past the end clamps to the character count.
        for _ in 0..10 {
            input.move_cursor(1, config);
        }
        assert_eq!(input.cursor, 2);

        // Act / Assert: Home and End land on the two ends exactly.
        input.move_home(false, config);
        assert_eq!(input.cursor, 0);
        input.move_home(true, config);
        assert_eq!(input.cursor, 2);
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
            app.input_text_char(ch);
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
            app.input_text_char(ch);
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
            app.input_text_char(ch);
        }
        assert_that!(app.selected_file().unwrap().display_name.as_str()).is_equal_to("gamma.mkv");
        app.cancel_file_search();

        // Assert: cancellation restores the pre-search file.
        assert_that!(app.selected_file().unwrap().display_name.as_str()).is_equal_to("beta.mkv");

        // Act: confirm the same result, then clear the confirmed filter.
        app.start_file_search();
        for ch in "gamma".chars() {
            app.input_text_char(ch);
        }
        app.finish_file_search();
        app.clear_file_search();

        // Assert: the confirmed result remains selected.
        assert_that!(app.selected_file().unwrap().display_name.as_str()).is_equal_to("gamma.mkv");

        // Act and assert: no matches leave no hidden selection.
        app.start_file_search();
        for ch in "missing".chars() {
            app.input_text_char(ch);
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
            app.input_text_char(ch);
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
    fn file_panel_entries_should_reflect_files_reassigned_outside_reconcile() {
        // Regression test: `file_panel_entries()` memoizes its result, but `files` is a
        // `pub` field some callers assign to directly instead of going through
        // `reconcile_files` (tests do this routinely for setup). A cache keyed by a
        // dirty flag/counter bumped only inside `reconcile_files` would silently return
        // a stale, shorter list after such a direct assignment — this must not happen.
        let mut app = test_file_app(&["a.mkv"]);
        let directory = app.directory.clone();
        assert_that!(app.file_panel_entries().len()).is_equal_to(1);

        app.files = (0..5)
            .map(|index| FileEntry {
                path: directory.join(format!("{index}.mkv")),
                display_name: format!("{index}.mkv"),
                fingerprint: crate::files::FileFingerprint {
                    length: 0,
                    modified: None,
                },
            })
            .collect();

        assert_that!(app.file_panel_entries().len()).is_equal_to(5);

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
            app.input_text_char(character);
        }
        assert_that!(app.filtered_subtitle_languages().len()).is_equal_to(1);
        app.activate_subtitle_settings();
        assert_that!(app.subtitle_popup_metadata().unwrap().language.as_str()).is_equal_to("nld");

        app.move_subtitle_settings_cursor(1);
        app.start_subtitle_title_input();
        for character in "Café".chars() {
            app.input_text_char(character);
        }
        app.move_text_cursor(-1);
        app.input_text_char('!');
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
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
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
            ]),
        );
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
        app.request_process_all();
        assert_that!(app.dialog).contains(Dialog::ConfirmProcessAll);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn source_container_should_trust_the_probed_format_over_a_renamed_extension() {
        // Arrange: a file literally named `movie.mp4` whose actual bytes ffprobe still
        // reports as Matroska — as happens when a `.mkv` gets renamed rather than
        // genuinely converted.
        let mut app = test_file_app(&["movie.mp4"]);
        let directory = app.directory.clone();
        app.outcome = Some(ProbeOutcome::Video(
            MediaInfo::from_json(serde_json::json!({
                "format": { "format_name": "matroska,webm" },
                "streams": [
                    {"index": 0, "codec_type": "video", "codec_name": "h264"}
                ]
            }))
            .unwrap(),
        ));

        // Act / Assert: the label follows what the file actually is, not its name.
        assert_that!(app.source_container()).contains(ContainerFormat::Matroska);

        // Before a probe result is in yet, the extension is still the best guess
        // available.
        app.outcome = None;
        assert_that!(app.source_container()).contains(ContainerFormat::Mp4);

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn container_settings_should_support_editing_title_comment_date_genre_artist() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"}
            ]),
        );

        // Act 1: Open container settings and navigate to Title
        app.open_container_settings();
        app.move_container_settings_cursor(1); // Title field
        assert_that!(app.container_settings_popup.as_ref().unwrap().field)
            .is_equal_to(ContainerSettingsField::Title);

        // Act 2: Enter text edit mode and type title
        app.start_container_text_input();
        assert_that!(app.container_settings_popup.as_ref().unwrap().mode)
            .is_equal_to(ContainerSettingsMode::TextEdit);
        for ch in "My Great Movie".chars() {
            app.input_text_char(ch);
        }
        app.activate_container_settings(); // saves text input and returns to summary mode

        // Assert 1: Staged container metadata has Title set
        let metadata = app.effective_container_metadata().unwrap();
        assert_that!(metadata.title.as_deref()).contains("My Great Movie");
        assert_that!(app.has_track_edits()).is_true();
        assert_that!(app.container_metadata_changed()).is_true();

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn container_metadata_changed_should_return_false_when_metadata_is_unedited() {
        // Arrange
        let info = MediaInfo::from_json(serde_json::json!({
            "format": {
                "format_name": "matroska,webm",
                "tags": { "title": "Original Title" }
            },
            "streams": [
                {"index": 0, "codec_type": "video", "codec_name": "h264"}
            ]
        }))
        .unwrap();
        let app = test_app(info);
        let directory = app.directory.clone();

        // Assert
        assert_that!(app.container_metadata_changed()).is_false();

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn container_metadata_should_be_cleared_on_file_switch() {
        // Arrange
        let mut app = test_file_app(&["movie.mkv", "other.mkv"]);
        let directory = app.directory.clone();
        app.container_metadata = Some(ContainerMetadata {
            title: Some("Edited Title".to_string()),
            ..Default::default()
        });

        // Act — reset_track_edits should clear container_metadata
        app.reset_track_edits();

        // Assert
        assert!(app.container_metadata.is_none());
        assert_that!(app.container_metadata_changed()).is_false();
        assert_that!(app.has_track_edits()).is_false();

        // Act — also verify clear_track_edits
        app.container_metadata = Some(ContainerMetadata {
            title: Some("Edited Title".to_string()),
            ..Default::default()
        });
        app.clear_track_edits();

        // Assert
        assert!(app.container_metadata.is_none());

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn subtitle_settings_cursor_should_walk_the_visible_fields_and_stop_at_both_ends() {
        // Arrange: `j` and `k` in the settings popup walk the rows the popup is actually
        // showing. Stopping on a hidden row would leave the cursor nowhere.
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "subtitle", "codec_name": "subrip",
                 "tags": {"language": "eng"}}
            ]),
        );
        app.selected_stream = app
            .track_rows()
            .iter()
            .position(|row| *row == TrackRef::Embedded(1))
            .unwrap();
        app.open_track_settings();
        let fields = app.visible_subtitle_fields();
        assert!(fields.len() > 2, "there should be rows to walk: {fields:?}");

        // Act / Assert: walking down visits every visible row in order and stops at the
        // last one rather than wrapping around to the top.
        app.move_subtitle_settings_to_endpoint(false);
        let mut visited = vec![app.subtitle_settings_popup.as_ref().unwrap().field];
        for _ in 0..fields.len() + 2 {
            app.move_subtitle_settings_cursor(1);
            let field = app.subtitle_settings_popup.as_ref().unwrap().field;
            assert!(fields.contains(&field), "{field:?} is not a visible row");
            if *visited.last().unwrap() != field {
                visited.push(field);
            }
        }
        assert_that!(visited).is_equal_to(fields.clone());

        // And walking back up stops at the first row.
        for _ in 0..fields.len() + 2 {
            app.move_subtitle_settings_cursor(-1);
        }
        assert_that!(app.subtitle_settings_popup.as_ref().unwrap().field).is_equal_to(fields[0]);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn subtitle_flag_checkboxes_should_toggle_only_the_flags_the_container_stores() {
        // Arrange: every flag checkbox on an embedded track, toggled on and back off. The
        // flags are what a player uses to pick a subtitle automatically, so a checkbox
        // that does not stick is a track that behaves differently after saving.
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "subtitle", "codec_name": "subrip",
                 "tags": {"language": "eng"}}
            ]),
        );
        app.selected_stream = app
            .track_rows()
            .iter()
            .position(|row| *row == TrackRef::Embedded(1))
            .unwrap();
        app.open_track_settings();
        let source = SubtitleSource::Embedded(1);
        let flag_of = |app: &App, field: SubtitleSettingsField| {
            let metadata = app.subtitle_metadata_for(&source).unwrap();
            match field {
                SubtitleSettingsField::Forced => metadata.forced,
                SubtitleSettingsField::Cc => metadata.cc,
                SubtitleSettingsField::HearingImpaired => metadata.hearing_impaired,
                SubtitleSettingsField::Original => metadata.original,
                SubtitleSettingsField::Commentary => metadata.commentary,
                other => panic!("{other:?} is not a checkbox"),
            }
        };

        // Act / Assert: every checkbox the popup actually offers toggles on and back off.
        // A checkbox that does not stick is a track that behaves differently after saving.
        let checkboxes: Vec<_> = app
            .visible_subtitle_fields()
            .into_iter()
            .filter(|field| field.subtitle_flag().is_some())
            .collect();
        assert!(
            checkboxes.contains(&SubtitleSettingsField::Forced),
            "a SubRip track in Matroska should at least offer Forced: {checkboxes:?}",
        );
        assert!(
            !checkboxes.contains(&SubtitleSettingsField::Cc),
            "Matroska cannot store closed captions, so the box is not offered",
        );
        for field in checkboxes {
            app.subtitle_settings_popup.as_mut().unwrap().field = field;
            assert!(!flag_of(&app, field), "{field:?} starts clear");
            app.activate_subtitle_settings();
            assert!(flag_of(&app, field), "{field:?} should set");
            app.activate_subtitle_settings();
            assert!(!flag_of(&app, field), "{field:?} should clear again");
        }

        // Converting to MP4 takes Original away — the popup stops offering it, and
        // pressing it anyway stages nothing rather than a flag that the save would drop.
        app.container_target = Some(ContainerFormat::Mp4);
        assert!(
            !app.visible_subtitle_fields()
                .contains(&SubtitleSettingsField::Original),
            "MP4 cannot store Original, so the box is withdrawn",
        );
        app.subtitle_settings_popup.as_mut().unwrap().field = SubtitleSettingsField::Original;
        app.activate_subtitle_settings();
        assert!(
            !flag_of(&app, SubtitleSettingsField::Original),
            "a withdrawn checkbox must not stage its flag",
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn only_one_subtitle_can_be_the_default_across_embedded_and_external_tracks() {
        // Arrange: exactly one subtitle may be flagged default, and sidecars compete for
        // the same slot as embedded tracks. Two defaults is a file players disagree about.
        let mut app = test_file_app(&["movie.mkv", "movie.nld.srt"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "subtitle", "codec_name": "subrip",
                 "tags": {"language": "eng"}},
                {"index": 2, "codec_type": "subtitle", "codec_name": "subrip",
                 "tags": {"language": "fra"}}
            ]),
        );
        app.sidecars = vec![test_sidecar(&app, "movie.nld.srt", "nld")];
        // Only a sidecar that is being imported can be the container's default subtitle —
        // one staying on disk is not in the container to be defaulted to.
        app.selected_stream = app
            .track_rows()
            .iter()
            .position(|row| *row == TrackRef::Sidecar(0))
            .unwrap();
        assert!(app.transfer_subtitle(-1), "the sidecar should import");
        let make_default = |app: &mut App, row: TrackRef| {
            app.selected_stream = app.track_rows().iter().position(|r| *r == row).unwrap();
            app.open_track_settings();
            app.subtitle_settings_popup.as_mut().unwrap().field = SubtitleSettingsField::Default;
            app.activate_subtitle_settings();
            app.close_subtitle_settings();
        };

        // Act / Assert: each new default displaces the previous one, whichever side it
        // was on.
        make_default(&mut app, TrackRef::Embedded(1));
        assert_that!(app.default_streams.contains(&1)).is_true();

        make_default(&mut app, TrackRef::Embedded(2));
        assert_that!(app.default_streams.contains(&1)).is_false();
        assert_that!(app.default_streams.contains(&2)).is_true();

        make_default(&mut app, TrackRef::Sidecar(0));
        eprintln!(
            "popup={:?} notice={:?} sidecars={:?} defaults={:?}",
            app.subtitle_settings_popup.is_some(),
            app.notice,
            app.default_sidecars,
            app.default_streams
        );
        assert_that!(app.default_streams.contains(&2)).is_false();
        assert_that!(app.default_sidecars.contains(&0)).is_true();

        make_default(&mut app, TrackRef::Embedded(1));
        assert_that!(app.default_sidecars.is_empty()).is_true();
        assert_that!(app.default_streams.contains(&1)).is_true();

        // And pressing it again on the current default clears it — no subtitle default
        // is a legitimate state.
        make_default(&mut app, TrackRef::Embedded(1));
        assert_that!(app.default_streams.contains(&1)).is_false();

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn video_settings_endpoints_should_land_on_selectable_options_in_every_mode() {
        // Arrange: Home and End through the video settings popup's three modes. Landing
        // on a disabled codec would leave Enter silently doing nothing.
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264",
                 "width": 1920, "height": 1080}
            ]),
        );
        app.selected_stream = app
            .track_rows()
            .iter()
            .position(|row| *row == TrackRef::Embedded(0))
            .unwrap();
        app.open_video_settings();

        // Act / Assert: in summary mode they move between the visible fields.
        app.move_video_settings_to_endpoint(true);
        assert_that!(app.video_settings_popup.as_ref().unwrap().field)
            .is_equal_to(VideoSettingsField::Commentary);
        app.move_video_settings_to_endpoint(false);
        assert_that!(app.video_settings_popup.as_ref().unwrap().field)
            .is_equal_to(VideoSettingsField::Codec);

        // In the codec dropdown they land on enabled entries at each end.
        app.activate_video_settings();
        let codecs = app.video_codec_choices(0);
        app.move_video_settings_to_endpoint(true);
        let last = app.video_settings_popup.as_ref().unwrap().codec_cursor;
        app.move_video_settings_to_endpoint(false);
        let first = app.video_settings_popup.as_ref().unwrap().codec_cursor;
        assert!(codecs[last].enabled && codecs[first].enabled);
        assert!(last > first, "End should land past Home");

        // Same in the resolution dropdown.
        app.escape_video_settings();
        app.move_video_settings_cursor(1);
        app.activate_video_settings();
        let resolutions = app.resolution_choices(0);
        app.move_video_settings_to_endpoint(true);
        let last = app.video_settings_popup.as_ref().unwrap().resolution_cursor;
        app.move_video_settings_to_endpoint(false);
        let first = app.video_settings_popup.as_ref().unwrap().resolution_cursor;
        assert!(resolutions[last].enabled && resolutions[first].enabled);
        assert!(last > first, "End should land past Home");

        // And inside the custom-resolution editor they move between its fields.
        app.video_settings_popup.as_mut().unwrap().resolution_cursor = resolutions.len() - 1;
        app.activate_video_settings();
        app.move_video_settings_to_endpoint(true);
        fn draft_field(app: &App) -> CustomResolutionField {
            app.video_settings_popup
                .as_ref()
                .unwrap()
                .custom_resolution
                .as_ref()
                .unwrap()
                .field
        }
        assert_that!(draft_field(&app)).is_equal_to(CustomResolutionField::Scaling);
        app.move_video_settings_to_endpoint(false);
        assert_that!(draft_field(&app)).is_equal_to(CustomResolutionField::Width);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn subtitle_settings_should_jump_to_the_first_and_last_selectable_option() {
        // Arrange: Home and End inside an open dropdown. They must land on a *selectable*
        // option — stopping on a disabled codec would leave Enter doing nothing.
        let mut app = test_file_app(&["movie.mkv"]);
        let directory = app.directory.clone();
        set_media(
            &mut app,
            serde_json::json!([
                {"index": 0, "codec_type": "video", "codec_name": "h264"},
                {"index": 1, "codec_type": "subtitle", "codec_name": "subrip",
                 "tags": {"language": "eng"}}
            ]),
        );
        app.selected_stream = app
            .track_rows()
            .iter()
            .position(|row| *row == TrackRef::Embedded(1))
            .unwrap();
        app.open_track_settings();
        let popup = app
            .subtitle_settings_popup
            .as_mut()
            .expect("the subtitle settings popup should open");
        popup.field = SubtitleSettingsField::Language;
        popup.mode = SubtitleSettingsMode::LanguageDropdown;

        // Act / Assert
        app.move_subtitle_settings_to_endpoint(true);
        let last = app
            .subtitle_settings_popup
            .as_ref()
            .unwrap()
            .language_cursor;
        app.move_subtitle_settings_to_endpoint(false);
        let first = app
            .subtitle_settings_popup
            .as_ref()
            .unwrap()
            .language_cursor;
        assert!(
            last > first,
            "End should land past Home ({first} then {last})",
        );
        assert_that!(first).is_equal_to(0);
        assert_that!(last).is_equal_to(app.filtered_subtitle_languages().len() - 1);

        // And the help panel toggles independently of the cursor.
        app.close_subtitle_settings();
        app.selected_stream = 0;
        app.open_container_settings();
        let before = app.container_settings_popup.as_ref().unwrap().help_visible;
        app.toggle_container_help();
        assert_that!(app.container_settings_popup.as_ref().unwrap().help_visible)
            .is_equal_to(!before);

        std::fs::remove_dir_all(directory).unwrap();
    }
}
