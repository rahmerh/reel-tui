//! A headless driver for the real app.
//!
//! [`Harness`] replays the event loop from `src/main.rs` in-process: it spawns the
//! same directory monitor, probe worker and edit worker pools the binary does, feeds
//! synthetic `KeyEvent`s through the same `handle_key`, and renders through the same
//! `ui::render` into a `TestBackend`. Nothing in the crate is mocked — the probes and
//! edits are real `ffprobe`/`ffmpeg` subprocesses operating on real files.
//!
//! The only layers not covered are crossterm's terminal setup and its byte-to-
//! `KeyEvent` decoding, plus writing the finished frame to a tty.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Receiver;
use std::sync::{Mutex, MutexGuard, Once, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui_image::picker::Picker;
use reel_tui::app::{
    App, AudioSettingsField, AudioSettingsMode, ContainerSettingsField, ContainerSettingsMode,
    Dialog, Layer, ResolutionChoiceValue, SubtitleSettingsField, SubtitleSettingsMode, TrackRef,
    VideoSettingsField, VideoSettingsMode,
};
use reel_tui::edit::{EditEvent, VideoRotation, spawn_edit_worker_pools};
use reel_tui::files::{DirectorySnapshot, spawn_directory_monitor};
use reel_tui::input::{InputOutcome, InputState, handle_key};
use reel_tui::preview::{PreviewEvent, spawn_preview_workers};
use reel_tui::probe::{
    ProbeOutcome, ProbeResponse, spawn_conflict_probe_worker, spawn_probe_worker,
};
use reel_tui::ui;

/// Generous enough to absorb a cold ffmpeg start on a loaded machine, short enough
/// that a genuinely stuck test fails within a coffee break.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(90);

/// Points `XDG_CACHE_HOME` at throwaway storage before the first `App` is built.
///
/// `App::new` loads `DiskCache` and `receive_probe_results` saves it, and
/// `edit::log_edit_failure` appends to `edit_errors.log` — all under
/// `$XDG_CACHE_HOME/reel-tui`. Without this, running the suite would read and rewrite
/// the user's real probe cache and pollute their real failure log with test paths.
///
/// This is process-global and cannot be scoped to one test, which is why it runs once
/// under a `Once` before any `App` exists rather than per-harness. `XDG_CACHE_HOME` is
/// read in exactly one place (`src/cache.rs`), so the blast radius is small.
fn redirect_cache_dir() {
    static REDIRECT: Once = Once::new();
    REDIRECT.call_once(|| {
        // A fixed path rather than a per-process one, so repeated runs reuse a single
        // directory instead of littering `$TMPDIR`; cleared first so no run inherits a
        // previous run's cached probes.
        let dir = std::env::temp_dir().join("reel-tui-e2e-cache");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        // SAFETY: called once, before any harness spawns a worker thread, so no other
        // thread can be reading the environment concurrently.
        unsafe {
            std::env::set_var("XDG_CACHE_HOME", &dir);
        }
    });
}

/// Serialises the scenarios that share the preview frame cache.
///
/// `XDG_CACHE_HOME` is process-global (see [`redirect_cache_dir`]), so every scenario in
/// this binary renders into **one** frame cache — and the subtitle edit page prunes that cache to
/// `cache_tracks` whole media directories at the start of every background pass. Run in
/// parallel, the scenarios therefore evict each other's frames: one that walks a cue list
/// expecting the cache to answer waits forever for frames another scenario has just
/// deleted. `a_full_cache_should_evict_whole_tracks_and_never_the_open_one` is the sharpest
/// case, since it deliberately prunes to a single track.
///
/// A lock rather than `--test-threads=1`, because the invocation is not ours to control —
/// CI runs a bare `cargo test --test e2e` — and because the rest of the suite has no reason
/// to give up its parallelism. It costs nothing in wall clock: these scenarios are `ffmpeg`
/// waiting on `ffmpeg`, which is already using every core.
///
/// Poisoning is ignored on purpose. The lock guards nothing but ordering, so a scenario
/// that panicked while holding it has left no state for the next one to be confused by —
/// and a poisoned lock would turn one real failure into a cascade of unrelated ones.
pub fn frame_cache_lock() -> MutexGuard<'static, ()> {
    static FRAME_CACHE: Mutex<()> = Mutex::new(());
    FRAME_CACHE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A temp directory that cleans itself up even when a test panics — unlike the manual
/// `fs::remove_dir_all` at the end of each unit test, which leaks on failure.
pub struct Scratch(PathBuf);

impl Scratch {
    pub fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "reel-tui-e2e-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub struct Harness {
    pub app: App,
    input: InputState,
    terminal: Terminal<TestBackend>,
    directory_rx: Receiver<DirectorySnapshot>,
    probe_rx: Receiver<ProbeResponse>,
    conflict_rx: Receiver<ProbeResponse>,
    edit_rx: Receiver<EditEvent>,
    preview_rx: Receiver<PreviewEvent>,
    /// Declared last so the directory outlives the `App` and workers on drop.
    scratch: Scratch,
}

impl Harness {
    /// Wires up exactly what `main()` does, against an already-populated directory.
    ///
    /// The picker stands in for a terminal that offered a real image protocol. Halfblocks
    /// is the stand-in because it is the only protocol whose output `TestBackend` can be
    /// asserted on — it draws ordinary cells with colours where kitty, sixel and iTerm2
    /// write escape sequences the buffer never stores — and because
    /// `Picker::from_query_stdio` would write to the real terminal and wait for a reply no
    /// test runner sends. Everything the page does with a frame is protocol-agnostic; only
    /// the encoder differs. A terminal that offered *nothing* is
    /// [`Self::start_without_image_protocol`], which is what `preview::drawing_picker`
    /// produces for a real halfblocks terminal.
    pub fn start(scratch: Scratch) -> Self {
        Self::start_with_picker(scratch, Some(Picker::halfblocks()))
    }

    /// A terminal that offered no image protocol at all, so the subtitle edit page can never draw
    /// a frame and says so instead of rendering one nobody could read.
    pub fn start_without_image_protocol(scratch: Scratch) -> Self {
        Self::start_with_picker(scratch, None)
    }

    fn start_with_picker(scratch: Scratch, picker: Option<Picker>) -> Self {
        redirect_cache_dir();
        let directory = scratch.path().to_path_buf();
        let directory_rx = spawn_directory_monitor(directory.clone());
        let (request_tx, probe_rx) = spawn_probe_worker();
        let (conflict_tx, conflict_rx) = spawn_conflict_probe_worker();
        let (transcode_tx, remux_tx, edit_rx) = spawn_edit_worker_pools(1, 1);
        let (preview_handles, preview_rx) = spawn_preview_workers(picker);
        let mut app = App::new(directory, request_tx, conflict_tx, transcode_tx, remux_tx).unwrap();
        app.set_preview_handles(Some(preview_handles));
        Self {
            app,
            input: InputState::default(),
            terminal: Terminal::new(TestBackend::new(160, 45)).unwrap(),
            directory_rx,
            probe_rx,
            conflict_rx,
            edit_rx,
            preview_rx,
            scratch,
        }
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.scratch.join(name)
    }

    pub fn directory(&self) -> &Path {
        self.scratch.path()
    }

    /// One iteration of the loop in `src/main.rs`. Always draws, so every scenario
    /// also exercises rendering in each intermediate state it passes through.
    pub fn pump(&mut self) {
        self.app.receive_directory_snapshots(&self.directory_rx);
        self.app.receive_probe_results(&self.probe_rx);
        self.app.receive_conflict_probe_results(&self.conflict_rx);
        self.app.receive_edit_results(&self.edit_rx);
        self.app.receive_preview_events(&self.preview_rx);
        self.app.start_pending_probe();
        self.app.start_pending_preview();
        // In the same place and the same order `main`'s loop has it: after the drains, so a
        // span that arrived this iteration is stepped in the pass it landed in.
        self.app.advance_playback();
        self.app.maybe_open_conflict_dialog();
        let app = &mut self.app;
        self.terminal.draw(|frame| ui::render(frame, app)).unwrap();
    }

    pub fn press(&mut self, key: KeyEvent) -> InputOutcome {
        self.pump();
        handle_key(&mut self.app, &mut self.input, key)
    }

    /// Pumps until `predicate` holds, panicking with a screen dump on timeout.
    ///
    /// Iteration counts would be the wrong tool: `start_pending_probe` debounces for
    /// 120 ms and probes/encodes take real wall time, so progress here is a function
    /// of elapsed time rather than of how many times the loop spun.
    pub fn wait_until(&mut self, label: &str, predicate: impl Fn(&App) -> bool) {
        let started = Instant::now();
        loop {
            self.pump();
            if predicate(&self.app) {
                return;
            }
            if started.elapsed() > DEFAULT_TIMEOUT {
                panic!(
                    "timed out after {:?} waiting for {label}\n\
                     layer: {:?}, dialog: {:?}, notice: {:?}, edit_error: {:?}\n\
                     screen:\n{}",
                    DEFAULT_TIMEOUT,
                    self.app.layer,
                    self.app.dialog,
                    self.app.notice,
                    self.app.edit_error,
                    self.screen(),
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Selects `name` in the file panel and waits for its probe to land, leaving the
    /// app in the Streams layer with the file open — the state every scenario starts
    /// from. Driven entirely through keypresses so the real selection path runs.
    pub fn open(&mut self, name: &str) {
        self.wait_until(&format!("{name} to appear in the file panel"), |app| {
            app.files.iter().any(|file| file.display_name == name)
        });

        let target = self.path(name);
        for _ in 0..self.app.files.len().max(1) * 2 {
            if self.app.selected_file().is_some_and(|f| f.path == target) {
                break;
            }
            self.press(key(KeyCode::Char('j')));
        }
        assert_eq!(
            self.app.selected_file().map(|f| f.path.clone()),
            Some(target),
            "could not select {name} in the file panel"
        );

        self.wait_until(&format!("{name} to finish probing"), |app| {
            matches!(app.outcome, Some(ProbeOutcome::Video(_)))
        });
        self.press(key(KeyCode::Enter));
        assert_eq!(self.app.layer, Layer::Streams, "should have opened {name}");
    }

    /// Moves the stream cursor onto the row at `index` within `track_rows()`.
    pub fn select_track_row(&mut self, index: usize) {
        assert!(
            index < self.app.track_rows().len(),
            "track row {index} out of range ({} rows)",
            self.app.track_rows().len()
        );
        while self.app.selected_stream > index {
            self.press(key(KeyCode::Char('k')));
        }
        while self.app.selected_stream < index {
            self.press(key(KeyCode::Char('j')));
        }
    }

    /// Opens the container settings popup from the Container row in the track list.
    pub fn open_container_settings(&mut self) {
        let index = self
            .app
            .track_rows()
            .iter()
            .position(|track| *track == TrackRef::Container)
            .expect("the track list should have a container row");
        self.select_track_row(index);
        self.press(key(KeyCode::Enter));
        assert_eq!(
            self.app.dialog,
            Some(Dialog::ContainerSettings),
            "Enter on the container row should open container settings"
        );
    }

    /// Stages a container conversion by driving the format dropdown, exactly as a
    /// user would: Enter to open it, j to walk to the target, Enter to pick, Esc to
    /// close the popup.
    pub fn choose_container_format(&mut self, label: &str) {
        self.open_container_settings();
        self.press(key(KeyCode::Enter));

        // The dropdown cursor starts on the source's own format and does not wrap, so
        // walk toward the target in whichever direction it lies.
        let target = self
            .app
            .container_choices()
            .iter()
            .position(|choice| choice.label == label)
            .unwrap_or_else(|| {
                panic!(
                    "no container choice labelled {label}; choices: {:?}",
                    self.app
                        .container_choices()
                        .iter()
                        .map(|c| c.label.clone())
                        .collect::<Vec<_>>()
                )
            });
        for _ in 0..self.app.container_choices().len() {
            let cursor = self
                .app
                .container_settings_popup
                .as_ref()
                .map(|popup| popup.format_cursor)
                .unwrap_or_default();
            match cursor.cmp(&target) {
                std::cmp::Ordering::Equal => break,
                std::cmp::Ordering::Less => self.press(key(KeyCode::Char('j'))),
                std::cmp::Ordering::Greater => self.press(key(KeyCode::Char('k'))),
            };
        }

        let cursor = self
            .app
            .container_settings_popup
            .as_ref()
            .map(|popup| popup.format_cursor)
            .unwrap_or_default();
        assert_eq!(
            self.app.container_choices().get(cursor).map(|c| &c.label),
            Some(&label.to_string()),
            "could not move the dropdown onto {label}; choices: {:?}",
            self.app
                .container_choices()
                .iter()
                .map(|c| c.label.clone())
                .collect::<Vec<_>>()
        );

        self.press(key(KeyCode::Enter));
        self.press(key(KeyCode::Esc));
        assert_eq!(
            self.app.dialog, None,
            "Esc should close the container settings popup"
        );
    }

    /// Replaces one container metadata value through the popup's ordinary text
    /// editor. The popup must already be open so a scenario can edit several fields
    /// in one visit, just like a user would.
    pub fn type_container_metadata(&mut self, field: ContainerSettingsField, value: &str) {
        assert_ne!(
            field,
            ContainerSettingsField::Format,
            "Format is a dropdown rather than container metadata"
        );
        let target = ContainerSettingsField::ALL
            .iter()
            .position(|candidate| *candidate == field)
            .expect("container field should be listed");
        for _ in 0..ContainerSettingsField::ALL.len() * 2 {
            let current = self
                .app
                .container_settings_popup
                .as_ref()
                .map(|popup| popup.field)
                .expect("container settings should be open");
            let position = ContainerSettingsField::ALL
                .iter()
                .position(|candidate| *candidate == current)
                .expect("focused container field should be listed");
            match position.cmp(&target) {
                std::cmp::Ordering::Equal => break,
                std::cmp::Ordering::Less => self.press(key(KeyCode::Char('j'))),
                std::cmp::Ordering::Greater => self.press(key(KeyCode::Char('k'))),
            };
        }
        assert_eq!(
            self.app
                .container_settings_popup
                .as_ref()
                .map(|popup| popup.field),
            Some(field),
            "could not focus {}",
            field.label()
        );

        self.press(key(KeyCode::Char('i')));
        assert_eq!(
            self.app
                .container_settings_popup
                .as_ref()
                .map(|popup| popup.mode),
            Some(ContainerSettingsMode::TextEdit),
            "i on {} should enter ordinary text editing",
            field.label()
        );
        self.press(ctrl('u'));
        for character in value.chars() {
            self.press(key(KeyCode::Char(character)));
        }
        self.press(key(KeyCode::Enter));
    }

    pub fn close_container_settings(&mut self) {
        self.press(key(KeyCode::Esc));
        assert_eq!(self.app.dialog, None, "Esc should close container settings");
    }

    /// Opens the audio settings popup for the embedded track at `row`.
    pub fn open_audio_settings(&mut self, row: usize) {
        self.select_track_row(row);
        self.press(key(KeyCode::Enter));
        assert_eq!(
            self.app.dialog,
            Some(Dialog::AudioSettings),
            "Enter on row {row} should open audio settings"
        );
    }

    /// Selects a technical audio setting through the popup's real dropdown.
    pub fn choose_audio_setting(&mut self, field: AudioSettingsField, label: &str) {
        self.focus_audio_field(field);
        self.press(key(KeyCode::Enter));
        assert_eq!(
            self.app
                .audio_settings_popup
                .as_ref()
                .map(|popup| popup.mode),
            Some(AudioSettingsMode::Dropdown),
            "Enter on {} should open its dropdown",
            field.label()
        );

        let (_, choices) = self.audio_choice_state();
        let target = choices
            .iter()
            .position(|(choice, _, _)| choice == label)
            .unwrap_or_else(|| {
                panic!(
                    "no {} choice labelled {label}; choices: {:?}",
                    field.label(),
                    choices
                        .iter()
                        .map(|(choice, _, _)| choice)
                        .collect::<Vec<_>>()
                )
            });
        assert!(
            choices[target].1,
            "{label} is not selectable: {:?}",
            choices[target].2
        );

        for _ in 0..choices.len() * 2 {
            let (cursor, _) = self.audio_choice_state();
            match cursor.cmp(&target) {
                std::cmp::Ordering::Equal => break,
                std::cmp::Ordering::Less => self.press(key(KeyCode::Char('j'))),
                std::cmp::Ordering::Greater => self.press(key(KeyCode::Char('k'))),
            };
        }
        let (cursor, choices) = self.audio_choice_state();
        assert_eq!(
            choices.get(cursor).map(|(choice, _, _)| choice.as_str()),
            Some(label),
            "could not move the {} dropdown onto {label}",
            field.label()
        );
        self.press(key(KeyCode::Enter));
    }

    /// Filters the audio language dropdown and selects the expected ISO 639-2 code.
    pub fn choose_audio_language(&mut self, query: &str, code: &str) {
        self.focus_audio_field(AudioSettingsField::Language);
        self.press(key(KeyCode::Enter));
        self.press(key(KeyCode::Char('/')));
        for character in query.chars() {
            self.press(key(KeyCode::Char(character)));
        }

        let choices = self.app.filtered_audio_languages();
        let target = choices
            .iter()
            .position(|choice| choice.code == code)
            .unwrap_or_else(|| {
                panic!(
                    "language search {query:?} did not offer {code}; choices: {:?}",
                    choices
                        .iter()
                        .map(|choice| (&choice.code, &choice.name))
                        .collect::<Vec<_>>()
                )
            });
        for _ in 0..choices.len() * 2 {
            let cursor = self
                .app
                .audio_settings_popup
                .as_ref()
                .map(|popup| popup.language_cursor)
                .unwrap_or_default();
            match cursor.cmp(&target) {
                std::cmp::Ordering::Equal => break,
                std::cmp::Ordering::Less => self.press(key(KeyCode::Down)),
                std::cmp::Ordering::Greater => self.press(key(KeyCode::Up)),
            };
        }
        self.press(key(KeyCode::Enter));
    }

    /// Replaces the audio title through the popup's ordinary text editor.
    pub fn type_audio_title(&mut self, title: &str) {
        self.focus_audio_field(AudioSettingsField::Title);
        self.press(key(KeyCode::Char('i')));
        assert_eq!(
            self.app
                .audio_settings_popup
                .as_ref()
                .map(|popup| popup.mode),
            Some(AudioSettingsMode::TitleEdit),
            "i on Title should enter ordinary text editing"
        );
        self.press(ctrl('u'));
        for character in title.chars() {
            self.press(key(KeyCode::Char(character)));
        }
        self.press(key(KeyCode::Enter));
    }

    /// Toggles a checkbox field through the audio popup.
    pub fn toggle_audio_field(&mut self, field: AudioSettingsField) {
        self.focus_audio_field(field);
        self.press(key(KeyCode::Enter));
    }

    /// Closes the audio popup while retaining its staged changes.
    pub fn close_audio_settings(&mut self) {
        self.press(key(KeyCode::Esc));
        assert_eq!(self.app.dialog, None, "Esc should close audio settings");
    }

    fn focus_audio_field(&mut self, field: AudioSettingsField) {
        let fields = self.app.visible_audio_fields();
        let target = fields
            .iter()
            .position(|candidate| *candidate == field)
            .unwrap_or_else(|| panic!("{} is not visible", field.label()));
        for _ in 0..fields.len() * 2 {
            let current = self
                .app
                .audio_settings_popup
                .as_ref()
                .map(|popup| popup.field)
                .expect("audio settings should be open");
            let position = fields
                .iter()
                .position(|candidate| *candidate == current)
                .expect("the focused audio field should be visible");
            match position.cmp(&target) {
                std::cmp::Ordering::Equal => break,
                std::cmp::Ordering::Less => self.press(key(KeyCode::Char('j'))),
                std::cmp::Ordering::Greater => self.press(key(KeyCode::Char('k'))),
            };
        }
        assert_eq!(
            self.app
                .audio_settings_popup
                .as_ref()
                .map(|popup| popup.field),
            Some(field),
            "could not focus {}",
            field.label()
        );
    }

    fn audio_choice_state(&self) -> (usize, Vec<(String, bool, Option<String>)>) {
        let popup = self
            .app
            .audio_settings_popup
            .as_ref()
            .expect("audio settings should be open");
        match popup.field {
            AudioSettingsField::Codec => (
                popup.codec_cursor,
                self.app
                    .audio_codec_choices(popup.stream_index)
                    .into_iter()
                    .map(|choice| (choice.label, choice.enabled, choice.reason))
                    .collect(),
            ),
            AudioSettingsField::ChannelLayout => (
                popup.channel_cursor,
                self.app
                    .audio_channel_choices(popup.stream_index)
                    .into_iter()
                    .map(|choice| (choice.label, choice.enabled, choice.reason))
                    .collect(),
            ),
            field => panic!("{} does not have an audio dropdown", field.label()),
        }
    }

    /// Opens subtitle settings for the track at `row`.
    pub fn open_subtitle_settings(&mut self, row: usize) {
        self.select_track_row(row);
        self.press(key(KeyCode::Enter));
        assert_eq!(
            self.app.dialog,
            Some(Dialog::SubtitleSettings),
            "Enter on row {row} should open subtitle settings"
        );
    }

    /// Filters the subtitle language dropdown and selects the expected ISO 639-2
    /// code through the same search and navigation path as the live TUI.
    pub fn choose_subtitle_language(&mut self, query: &str, code: &str) {
        self.focus_subtitle_field(SubtitleSettingsField::Language);
        self.press(key(KeyCode::Enter));
        assert_eq!(
            self.app
                .subtitle_settings_popup
                .as_ref()
                .map(|popup| popup.mode),
            Some(SubtitleSettingsMode::LanguageDropdown)
        );
        self.press(key(KeyCode::Char('/')));
        for character in query.chars() {
            self.press(key(KeyCode::Char(character)));
        }

        let choices = self.app.filtered_subtitle_languages();
        let target = choices
            .iter()
            .position(|choice| choice.code == code)
            .unwrap_or_else(|| {
                panic!(
                    "language search {query:?} did not offer {code}; choices: {:?}",
                    choices
                        .iter()
                        .map(|choice| (&choice.code, &choice.name))
                        .collect::<Vec<_>>()
                )
            });
        for _ in 0..choices.len() * 2 {
            let cursor = self
                .app
                .subtitle_settings_popup
                .as_ref()
                .map(|popup| popup.language_cursor)
                .unwrap_or_default();
            match cursor.cmp(&target) {
                std::cmp::Ordering::Equal => break,
                std::cmp::Ordering::Less => self.press(key(KeyCode::Down)),
                std::cmp::Ordering::Greater => self.press(key(KeyCode::Up)),
            };
        }
        self.press(key(KeyCode::Enter));
    }

    pub fn type_subtitle_title(&mut self, title: &str) {
        self.focus_subtitle_field(SubtitleSettingsField::Title);
        self.press(key(KeyCode::Char('i')));
        assert_eq!(
            self.app
                .subtitle_settings_popup
                .as_ref()
                .map(|popup| popup.mode),
            Some(SubtitleSettingsMode::TitleEdit),
            "i on Title should enter ordinary text editing"
        );
        self.press(ctrl('u'));
        for character in title.chars() {
            self.press(key(KeyCode::Char(character)));
        }
        self.press(key(KeyCode::Enter));
    }

    pub fn toggle_subtitle_field(&mut self, field: SubtitleSettingsField) {
        self.focus_subtitle_field(field);
        self.press(key(KeyCode::Enter));
    }

    pub fn close_subtitle_settings(&mut self) {
        self.press(key(KeyCode::Esc));
        assert_eq!(self.app.dialog, None, "Esc should close subtitle settings");
    }

    fn focus_subtitle_field(&mut self, field: SubtitleSettingsField) {
        let fields = self.app.visible_subtitle_fields();
        let target = fields
            .iter()
            .position(|candidate| *candidate == field)
            .unwrap_or_else(|| panic!("{} is not visible", field.label()));
        for _ in 0..fields.len() * 2 {
            let current = self
                .app
                .subtitle_settings_popup
                .as_ref()
                .map(|popup| popup.field)
                .expect("subtitle settings should be open");
            let position = fields
                .iter()
                .position(|candidate| *candidate == current)
                .expect("focused subtitle field should be visible");
            match position.cmp(&target) {
                std::cmp::Ordering::Equal => break,
                std::cmp::Ordering::Less => self.press(key(KeyCode::Char('j'))),
                std::cmp::Ordering::Greater => self.press(key(KeyCode::Char('k'))),
            };
        }
        assert_eq!(
            self.app
                .subtitle_settings_popup
                .as_ref()
                .map(|popup| popup.field),
            Some(field),
            "could not focus {}",
            field.label()
        );
    }

    /// Stages a subtitle codec conversion on the subtitle track at `row`, the way the
    /// app's own conflict message tells the user to ("Convert it to MOV Text").
    pub fn convert_subtitle_to(&mut self, row: usize, label: &str) {
        self.select_track_row(row);
        self.press(key(KeyCode::Enter));
        assert_eq!(
            self.app.dialog,
            Some(Dialog::SubtitleSettings),
            "Enter on row {row} should open subtitle settings"
        );

        // The Codec field is focused by default; Enter opens its dropdown.
        self.press(key(KeyCode::Enter));

        let target = self
            .subtitle_choices()
            .iter()
            .position(|choice| choice.label == label)
            .unwrap_or_else(|| {
                panic!(
                    "no subtitle codec labelled {label}; choices: {:?}",
                    self.subtitle_choices()
                        .iter()
                        .map(|c| c.label.clone())
                        .collect::<Vec<_>>()
                )
            });

        assert!(
            self.subtitle_choices()[target].enabled,
            "{label} is not selectable for this source/container combination: {:?}",
            self.subtitle_choices()[target].reason
        );

        // The cursor skips disabled entries, so it can overshoot; bound the walk and
        // then assert on the label rather than trusting index arithmetic.
        for _ in 0..self.subtitle_choices().len() * 2 {
            let cursor = self
                .app
                .subtitle_settings_popup
                .as_ref()
                .map(|popup| popup.codec_cursor)
                .unwrap_or_default();
            match cursor.cmp(&target) {
                std::cmp::Ordering::Equal => break,
                std::cmp::Ordering::Less => self.press(key(KeyCode::Char('j'))),
                std::cmp::Ordering::Greater => self.press(key(KeyCode::Char('k'))),
            };
        }
        let cursor = self
            .app
            .subtitle_settings_popup
            .as_ref()
            .map(|popup| popup.codec_cursor)
            .unwrap_or_default();
        assert_eq!(
            self.subtitle_choices()
                .get(cursor)
                .map(|choice| choice.label.clone()),
            Some(label.to_string()),
            "could not move the codec dropdown onto {label}; choices: {:?}",
            self.subtitle_choices()
                .iter()
                .map(|c| c.label.clone())
                .collect::<Vec<_>>()
        );

        self.press(key(KeyCode::Enter));
        self.press(key(KeyCode::Esc));
        assert_eq!(
            self.app.dialog, None,
            "Esc should close the subtitle settings popup"
        );
        assert!(
            !self.app.subtitle_changes.is_empty(),
            "choosing {label} should have staged a subtitle change"
        );
    }

    /// Moves the open video-settings resolution dropdown onto its "Custom" entry.
    pub fn select_resolution_choice_custom(&mut self) {
        let index = self
            .app
            .video_settings_popup
            .as_ref()
            .map(|popup| popup.stream_index)
            .expect("video settings should be open");
        let target = self
            .app
            .resolution_choices(index)
            .iter()
            .position(|choice| choice.value == ResolutionChoiceValue::Custom)
            .expect("the resolution dropdown should offer a custom entry");

        for _ in 0..self.app.resolution_choices(index).len() * 2 {
            let cursor = self
                .app
                .video_settings_popup
                .as_ref()
                .map(|popup| popup.resolution_cursor)
                .unwrap_or_default();
            match cursor.cmp(&target) {
                std::cmp::Ordering::Equal => break,
                std::cmp::Ordering::Less => self.press(key(KeyCode::Char('j'))),
                std::cmp::Ordering::Greater => self.press(key(KeyCode::Char('k'))),
            };
        }
    }

    /// Types `value` into the focused custom-resolution field, replacing whatever it
    /// was prefilled with. Leaves the field with Esc so the draft stays open; pass
    /// `commit` to finish with Enter instead, which applies the whole draft.
    pub fn type_custom_dimension(&mut self, value: &str, commit: bool) {
        self.press(key(KeyCode::Char('i')));
        assert!(
            self.app.custom_resolution_input_active(),
            "`i` should have activated a custom resolution field"
        );
        for _ in 0..12 {
            self.press(key(KeyCode::Backspace));
            if !self.app.custom_resolution_input_active() {
                // Backspace on an empty field deactivates it; step back in.
                self.press(key(KeyCode::Char('i')));
                break;
            }
        }
        for character in value.chars() {
            self.press(key(KeyCode::Char(character)));
        }
        self.press(key(if commit { KeyCode::Enter } else { KeyCode::Esc }));
    }

    /// Drives the full custom-resolution flow from the video row: settings →
    /// Resolution → dropdown → Custom → width → height → commit.
    pub fn set_custom_resolution(&mut self, width: &str, height: &str) {
        self.select_track_row(1);
        self.press(key(KeyCode::Enter));
        assert_eq!(
            self.app.dialog,
            Some(Dialog::VideoSettings),
            "Enter on the video row should open video settings"
        );
        self.press(key(KeyCode::Char('j')));
        self.press(key(KeyCode::Enter));
        self.select_resolution_choice_custom();
        self.press(key(KeyCode::Enter));

        self.type_custom_dimension(width, false);
        self.press(key(KeyCode::Char('j')));
        self.type_custom_dimension(height, true);

        // The draft is applied on the way out of the custom-resolution editor, not by
        // the Enter that leaves the last field, so back all the way out of the popup.
        for _ in 0..4 {
            if self.app.dialog.is_none() {
                break;
            }
            self.press(key(KeyCode::Esc));
        }
        assert_eq!(
            self.app.dialog, None,
            "backing out should have closed the video settings popup"
        );
    }

    /// Selects a video codec through the video settings popup's real dropdown.
    pub fn choose_video_codec(&mut self, row: usize, label: &str) {
        self.select_track_row(row);
        self.press(key(KeyCode::Enter));
        assert_eq!(
            self.app.dialog,
            Some(Dialog::VideoSettings),
            "Enter on row {row} should open video settings"
        );
        self.press(key(KeyCode::Enter));
        assert_eq!(
            self.app
                .video_settings_popup
                .as_ref()
                .map(|popup| popup.mode),
            Some(VideoSettingsMode::Dropdown)
        );

        let index = self
            .app
            .video_settings_popup
            .as_ref()
            .map(|popup| popup.stream_index)
            .expect("video settings should be open");
        let choices = self.app.video_codec_choices(index);
        let target = choices
            .iter()
            .position(|choice| choice.label == label)
            .unwrap_or_else(|| {
                panic!(
                    "no video codec labelled {label}; choices: {:?}",
                    choices
                        .iter()
                        .map(|choice| &choice.label)
                        .collect::<Vec<_>>()
                )
            });
        assert!(
            choices[target].enabled,
            "{label} is not selectable: {:?}",
            choices[target].reason
        );
        for _ in 0..choices.len() * 2 {
            let cursor = self
                .app
                .video_settings_popup
                .as_ref()
                .map(|popup| popup.codec_cursor)
                .unwrap_or_default();
            match cursor.cmp(&target) {
                std::cmp::Ordering::Equal => break,
                std::cmp::Ordering::Less => self.press(key(KeyCode::Char('j'))),
                std::cmp::Ordering::Greater => self.press(key(KeyCode::Char('k'))),
            };
        }
        let cursor = self
            .app
            .video_settings_popup
            .as_ref()
            .map(|popup| popup.codec_cursor)
            .unwrap_or_default();
        assert_eq!(
            choices.get(cursor).map(|choice| choice.label.as_str()),
            Some(label)
        );
        self.press(key(KeyCode::Enter));
        self.press(key(KeyCode::Esc));
        assert_eq!(self.app.dialog, None, "Esc should close video settings");
    }

    /// Opens video settings for the track at `row`.
    pub fn open_video_settings(&mut self, row: usize) {
        self.select_track_row(row);
        self.press(key(KeyCode::Enter));
        assert_eq!(
            self.app.dialog,
            Some(Dialog::VideoSettings),
            "Enter on row {row} should open video settings"
        );
    }

    /// Opens the rotation dropdown on an already-open video popup and picks `rotation`.
    pub fn choose_video_rotation(&mut self, rotation: VideoRotation) {
        self.focus_video_field(VideoSettingsField::Rotation);
        self.press(key(KeyCode::Enter));
        assert_eq!(
            self.app
                .video_settings_popup
                .as_ref()
                .map(|popup| popup.mode),
            Some(VideoSettingsMode::Dropdown),
            "Enter on Rotation should open its dropdown"
        );
        let target = VideoRotation::ALL
            .iter()
            .position(|candidate| *candidate == rotation)
            .expect("every rotation is offered");
        for _ in 0..VideoRotation::ALL.len() * 2 {
            let cursor = self
                .app
                .video_settings_popup
                .as_ref()
                .map(|popup| popup.rotation_cursor)
                .unwrap_or_default();
            match cursor.cmp(&target) {
                std::cmp::Ordering::Equal => break,
                std::cmp::Ordering::Less => self.press(key(KeyCode::Char('j'))),
                std::cmp::Ordering::Greater => self.press(key(KeyCode::Char('k'))),
            };
        }
        self.press(key(KeyCode::Enter));
        assert_eq!(
            self.app
                .effective_video_settings(
                    self.app
                        .video_settings_popup
                        .as_ref()
                        .expect("video settings should be open")
                        .stream_index
                )
                .map(|settings| settings.rotation),
            Some(rotation),
        );
    }

    /// Filters the video language dropdown and selects the expected ISO 639-2 code.
    pub fn choose_video_language(&mut self, query: &str, code: &str) {
        self.focus_video_field(VideoSettingsField::Language);
        self.press(key(KeyCode::Enter));
        self.press(key(KeyCode::Char('/')));
        for character in query.chars() {
            self.press(key(KeyCode::Char(character)));
        }

        let choices = self.app.filtered_video_languages();
        let target = choices
            .iter()
            .position(|choice| choice.code == code)
            .unwrap_or_else(|| {
                panic!(
                    "language search {query:?} did not offer {code}; choices: {:?}",
                    choices
                        .iter()
                        .map(|choice| (&choice.code, &choice.name))
                        .collect::<Vec<_>>()
                )
            });
        for _ in 0..choices.len() * 2 {
            let cursor = self
                .app
                .video_settings_popup
                .as_ref()
                .map(|popup| popup.language_cursor)
                .unwrap_or_default();
            match cursor.cmp(&target) {
                std::cmp::Ordering::Equal => break,
                std::cmp::Ordering::Less => self.press(key(KeyCode::Down)),
                std::cmp::Ordering::Greater => self.press(key(KeyCode::Up)),
            };
        }
        self.press(key(KeyCode::Enter));
    }

    /// Replaces the video title through the popup's ordinary text editor.
    pub fn type_video_title(&mut self, title: &str) {
        self.focus_video_field(VideoSettingsField::Title);
        self.press(key(KeyCode::Char('i')));
        assert_eq!(
            self.app
                .video_settings_popup
                .as_ref()
                .map(|popup| popup.mode),
            Some(VideoSettingsMode::TitleEdit),
            "i on Title should enter ordinary text editing"
        );
        self.press(ctrl('u'));
        for character in title.chars() {
            self.press(key(KeyCode::Char(character)));
        }
        self.press(key(KeyCode::Enter));
    }

    /// Toggles the Default checkbox through the video popup.
    pub fn toggle_video_field(&mut self, field: VideoSettingsField) {
        self.focus_video_field(field);
        self.press(key(KeyCode::Enter));
    }

    /// Closes the video popup while retaining its staged changes.
    pub fn close_video_settings(&mut self) {
        self.press(key(KeyCode::Esc));
        assert_eq!(self.app.dialog, None, "Esc should close video settings");
    }

    fn focus_video_field(&mut self, field: VideoSettingsField) {
        let fields = self.app.visible_video_fields();
        let target = fields
            .iter()
            .position(|candidate| *candidate == field)
            .unwrap_or_else(|| panic!("{} is not visible", field.label()));
        for _ in 0..fields.len() * 2 {
            let current = self
                .app
                .video_settings_popup
                .as_ref()
                .map(|popup| popup.field)
                .expect("video settings should be open");
            let position = fields
                .iter()
                .position(|candidate| *candidate == current)
                .expect("the focused video field should be visible");
            match position.cmp(&target) {
                std::cmp::Ordering::Equal => break,
                std::cmp::Ordering::Less => self.press(key(KeyCode::Char('j'))),
                std::cmp::Ordering::Greater => self.press(key(KeyCode::Char('k'))),
            };
        }
        assert_eq!(
            self.app
                .video_settings_popup
                .as_ref()
                .map(|popup| popup.field),
            Some(field),
            "could not focus {}",
            field.label()
        );
    }

    /// The codec choices the open subtitle settings popup is currently offering.
    fn subtitle_choices(&self) -> Vec<reel_tui::subtitle::FormatChoice> {
        let popup = self
            .app
            .subtitle_settings_popup
            .as_ref()
            .expect("subtitle settings should be open");
        self.app
            .subtitle_choices(&popup.source, popup.source_format)
    }

    /// The first subtitle row in the track list, as a `track_rows()` index.
    pub fn first_subtitle_row(&self) -> usize {
        let info = match &self.app.outcome {
            Some(ProbeOutcome::Video(info)) => info,
            other => panic!("no probed media to inspect: {other:?}"),
        };
        let subtitle_indices: Vec<u64> = info
            .streams
            .iter()
            .enumerate()
            .filter(|(_, stream)| {
                stream.get("codec_type").and_then(serde_json::Value::as_str) == Some("subtitle")
            })
            .map(|(index, _)| index as u64)
            .collect();
        let first = *subtitle_indices
            .first()
            .expect("the fixture should carry a subtitle track");
        self.app
            .track_rows()
            .iter()
            .position(|track| *track == TrackRef::Embedded(first))
            .expect("the subtitle track should have a row")
    }

    /// Ctrl+S → confirm → wait for the batch to finish, the way a user saves.
    pub fn process_all(&mut self) {
        self.press(ctrl('s'));
        assert_eq!(
            self.app.dialog,
            Some(Dialog::ConfirmProcessAll),
            "Ctrl+S should open the confirm dialog; notice: {:?}, error: {:?}",
            self.app.notice,
            self.app.edit_error,
        );
        self.press(key(KeyCode::Enter));
        self.wait_until("the batch to finish", |app| app.active_batch.is_none());
    }

    /// Keeps replaying the loop for `duration` with nothing else happening, the way
    /// the app idles between keypresses. Background work — the directory monitor's
    /// reconcile, a conflict re-probe, the dialogs either can raise — only lands on a
    /// later tick, so a scenario that stops pumping the moment its foreground action
    /// returns never sees what that action set in motion.
    pub fn settle(&mut self, duration: Duration) {
        let started = Instant::now();
        while started.elapsed() < duration {
            self.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Everything drawn on the selection's fill, as one string.
    ///
    /// The subtitle edit page marks the selected cue by filling its block rather than by putting
    /// a character beside it, so "which cue is selected" is a question about colour that
    /// only the buffer can answer.
    pub fn filled_selection(&self) -> String {
        self.terminal
            .backend()
            .buffer()
            .content
            .iter()
            .filter(|cell| cell.style().bg == Some(ratatui::style::Color::Cyan))
            .map(|cell| cell.symbol())
            .collect()
    }

    /// The distinct image colours on screen, which is as much of a rendered frame as
    /// `TestBackend` can be asked about.
    ///
    /// Its `Buffer` stores a symbol and a style per cell and never exposes the backend's
    /// writer, so kitty, sixel and iTerm2 escape sequences are invisible to it entirely.
    /// The halfblocks protocol the harness picks is the one that draws through ordinary
    /// cells, and an `Rgb` background is something no part of this UI paints except an
    /// image — so "more than one" means a decoded picture rather than a blank pane or a
    /// solid fill.
    pub fn preview_shades(&self) -> BTreeSet<(u8, u8, u8)> {
        self.terminal
            .backend()
            .buffer()
            .content
            .iter()
            .filter_map(|cell| match cell.style().bg {
                Some(ratatui::style::Color::Rgb(red, green, blue)) => Some((red, green, blue)),
                _ => None,
            })
            .collect()
    }

    /// The terminal contents as one string, for assertions and failure messages.
    pub fn screen(&self) -> String {
        let buffer = self.terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Fails unless every edit in the last batch reported success. Batch status is
    /// consumed as it finishes, so the notice is what survives.
    ///
    /// Requires a *positive* success notice rather than merely the absence of failure
    /// words: a batch that never dispatched leaves no notice at all, and would
    /// otherwise sail through every assertion in this suite.
    pub fn assert_batch_succeeded(&self) {
        let notice = self.app.notice.clone().unwrap_or_default();
        assert!(
            notice.contains("saved") || notice.contains("Processed"),
            "expected a success notice, got {notice:?}\nscreen:\n{}",
            self.screen()
        );
        assert!(
            !notice.contains("could not")
                && !notice.contains("Could not")
                && !notice.contains("failed")
                && !notice.contains("changed. Reopen"),
            "edit reported a failure: {notice}\nscreen:\n{}",
            self.screen()
        );
        assert!(
            self.app.edit_error.is_none(),
            "edit surfaced an error dialog: {:?}",
            self.app.edit_error
        );
    }

    /// No `.reel-tui-*` scratch files or transaction backups may survive an edit —
    /// the standing cleanliness assertion in the `src/edit.rs` ffmpeg tests.
    pub fn assert_no_temp_leftovers(&self) {
        let leftovers: Vec<String> = fs::read_dir(self.directory())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".reel-tui-") || name.contains("transaction-backup"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "edit left temporary files behind: {leftovers:?}"
        );
    }
}

pub fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

pub fn ctrl(code: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(code), KeyModifiers::CONTROL)
}

/// Mirrors `require_tools` in `src/edit.rs`'s test module, which integration tests
/// cannot reach because it lives inside a `#[cfg(test)]` block. Accepts either a bare
/// program name or `program:encoder`.
pub fn require_tools(test: &str, tools: &[&str]) {
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
        // ffmpeg spells it `-version`, most other tools `--version`. The encoder
        // form has to consult `-encoders`: `ffmpeg -h encoder=<name>` exits 0 for
        // any name, including ones it has never heard of.
        let available = match encoder {
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
        };
        assert!(
            available,
            "{test} requires {tool}; install the missing test prerequisite"
        );
    }
}

/// Convenience for asserting on a produced file's streams.
pub fn probe(path: &Path) -> reel_tui::probe::MediaInfo {
    reel_tui::probe::probe_any_file(path)
        .unwrap_or_else(|error| panic!("ffprobe should read {}: {error}", path.display()))
}

pub fn codec_names(info: &reel_tui::probe::MediaInfo) -> Vec<String> {
    info.streams
        .iter()
        .map(|stream| {
            stream
                .get("codec_name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?")
                .to_string()
        })
        .collect()
}

pub fn languages(info: &reel_tui::probe::MediaInfo) -> Vec<String> {
    info.streams
        .iter()
        .map(|stream| {
            stream
                .get("tags")
                .and_then(|tags| tags.get("language"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("und")
                .to_string()
        })
        .collect()
}

pub fn default_flags(info: &reel_tui::probe::MediaInfo) -> Vec<bool> {
    info.streams
        .iter()
        .map(|stream| {
            stream
                .get("disposition")
                .and_then(|disposition| disposition.get("default"))
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0)
                == 1
        })
        .collect()
}

pub fn stream_indices_of_type(info: &reel_tui::probe::MediaInfo, kind: &str) -> BTreeSet<usize> {
    info.streams
        .iter()
        .enumerate()
        .filter(|(_, stream)| {
            stream.get("codec_type").and_then(serde_json::Value::as_str) == Some(kind)
        })
        .map(|(index, _)| index)
        .collect()
}
