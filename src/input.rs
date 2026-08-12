//! Terminal key handling: the mapping from a `KeyEvent` to an `App` mutation.
//!
//! Split out of `main.rs` so integration tests can drive the same input path the
//! binary does, rather than reaching into `App` directly.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::{App, ContainerSettingsMode, Dialog, Layer, SubtitleSettingsMode};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputOutcome {
    Continue,
    Quit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ZCommand {
    Toggle,
    Open,
    Close,
    CloseAll,
    OpenAll,
}

#[derive(Default)]
pub struct InputState {
    pending_g: Option<Instant>,
    pending_z: Option<Instant>,
}

impl InputState {
    fn reset_sequence(&mut self) {
        self.pending_g = None;
        self.pending_z = None;
    }

    fn is_double_g(&mut self, key: KeyEvent) -> bool {
        if key.code != KeyCode::Char('g') {
            self.pending_g = None;
            return false;
        }
        if self
            .pending_g
            .is_some_and(|pressed| pressed.elapsed() <= Duration::from_millis(750))
        {
            self.pending_g = None;
            true
        } else {
            self.pending_g = Some(Instant::now());
            false
        }
    }

    fn z_command(&mut self, key: KeyEvent) -> Option<ZCommand> {
        let is_recent_z = self
            .pending_z
            .is_some_and(|pressed| pressed.elapsed() <= Duration::from_millis(750));

        if is_recent_z {
            self.pending_z = None;
            match key.code {
                KeyCode::Char('a') | KeyCode::Char('z') => return Some(ZCommand::Toggle),
                KeyCode::Char('o') => return Some(ZCommand::Open),
                KeyCode::Char('c') => return Some(ZCommand::Close),
                KeyCode::Char('M') => return Some(ZCommand::CloseAll),
                KeyCode::Char('R') => return Some(ZCommand::OpenAll),
                _ => {}
            }
        }

        if key.code == KeyCode::Char('z') {
            self.pending_z = Some(Instant::now());
        } else {
            self.pending_z = None;
        }
        None
    }
}

pub fn handle_key(app: &mut App, input: &mut InputState, key: KeyEvent) -> InputOutcome {
    match app.dialog {
        Some(Dialog::ConfirmCancel) => match (key.code, key.modifiers) {
            (KeyCode::Char('h') | KeyCode::Left, KeyModifiers::NONE) => {
                input.reset_sequence();
                app.choose_cancel_edit(-1)
            }
            (KeyCode::Char('l') | KeyCode::Right, KeyModifiers::NONE) => {
                input.reset_sequence();
                app.choose_cancel_edit(1)
            }
            (KeyCode::Char('G'), KeyModifiers::NONE) => {
                input.reset_sequence();
                app.choose_cancel_edit_endpoint(true);
            }
            (KeyCode::Enter, _) => {
                input.reset_sequence();
                app.activate_cancel_edit();
            }
            _ if is_back_key(key) => {
                input.reset_sequence();
                app.dismiss_cancel_edit();
            }
            _ if input.is_double_g(key) => app.choose_cancel_edit_endpoint(false),
            _ => {}
        },
        Some(Dialog::ContainerSettings) => {
            let mode = app
                .container_settings_popup
                .as_ref()
                .map(|popup| popup.mode)
                .unwrap_or_default();
            match mode {
                ContainerSettingsMode::TextEdit => {
                    input.reset_sequence();
                    match (key.code, key.modifiers) {
                        (KeyCode::Char('s'), KeyModifiers::CONTROL) => {
                            app.save_from_container_settings()
                        }
                        (KeyCode::Esc | KeyCode::Enter, _) => app.escape_container_settings(),
                        _ => {
                            handle_text_input_key(app, key);
                        }
                    }
                }
                ContainerSettingsMode::Summary | ContainerSettingsMode::FormatDropdown => {
                    match (key.code, key.modifiers) {
                        (KeyCode::Char('s'), KeyModifiers::CONTROL) => {
                            input.reset_sequence();
                            app.save_from_container_settings();
                        }
                        (KeyCode::Char('K'), KeyModifiers::SHIFT) => {
                            input.reset_sequence();
                            app.toggle_container_help();
                        }
                        (KeyCode::Char('j') | KeyCode::Down, KeyModifiers::NONE) => {
                            input.reset_sequence();
                            app.move_container_settings_cursor(1)
                        }
                        (KeyCode::Char('k') | KeyCode::Up, KeyModifiers::NONE) => {
                            input.reset_sequence();
                            app.move_container_settings_cursor(-1)
                        }
                        (KeyCode::Char('G'), KeyModifiers::NONE) => {
                            input.reset_sequence();
                            app.move_container_settings_to_endpoint(true);
                        }
                        (KeyCode::Char('i'), KeyModifiers::NONE) => {
                            input.reset_sequence();
                            app.start_container_text_input();
                        }
                        (KeyCode::Char('r'), KeyModifiers::NONE) => {
                            input.reset_sequence();
                            app.reset_focused_field();
                        }
                        // Enter still drives the Format dropdown; it no longer opens a
                        // metadata field, so `i` is the one way into every text field.
                        (KeyCode::Enter, _) => {
                            input.reset_sequence();
                            app.activate_container_settings();
                        }
                        _ if is_back_key(key) => {
                            input.reset_sequence();
                            app.escape_container_settings();
                        }
                        _ if input.is_double_g(key) => {
                            app.move_container_settings_to_endpoint(false)
                        }
                        _ => {}
                    }
                }
            }
        }
        Some(Dialog::VideoSettings) => {
            if app.custom_resolution_input_active() {
                input.reset_sequence();
                match (key.code, key.modifiers) {
                    (KeyCode::Char('s'), KeyModifiers::CONTROL) => app.save_from_video_settings(),
                    (KeyCode::Esc, _) => app.finish_custom_resolution_input(),
                    (KeyCode::Enter, _) => {
                        app.finish_custom_resolution_input();
                        app.activate_video_settings();
                    }
                    _ => {
                        handle_text_input_key(app, key);
                    }
                }
            } else {
                match (key.code, key.modifiers) {
                    (KeyCode::Char('s'), KeyModifiers::CONTROL) => {
                        input.reset_sequence();
                        app.save_from_video_settings();
                    }
                    (KeyCode::Char('i'), KeyModifiers::NONE) => {
                        input.reset_sequence();
                        app.start_custom_resolution_input();
                    }
                    (KeyCode::Char('r'), KeyModifiers::NONE) => {
                        input.reset_sequence();
                        app.reset_focused_field();
                    }
                    (KeyCode::Char('j') | KeyCode::Down, KeyModifiers::NONE) => {
                        input.reset_sequence();
                        app.move_video_settings_cursor(1)
                    }
                    (KeyCode::Char('k') | KeyCode::Up, KeyModifiers::NONE) => {
                        input.reset_sequence();
                        app.move_video_settings_cursor(-1)
                    }
                    (KeyCode::Char('G'), KeyModifiers::NONE) => {
                        input.reset_sequence();
                        app.move_video_settings_to_endpoint(true);
                    }
                    (KeyCode::Enter, _) => {
                        input.reset_sequence();
                        app.activate_video_settings();
                    }
                    _ if is_back_key(key) => {
                        input.reset_sequence();
                        app.escape_video_settings();
                    }
                    _ if input.is_double_g(key) => app.move_video_settings_to_endpoint(false),
                    _ => {}
                }
            }
        }
        Some(Dialog::SubtitleSettings) => {
            let mode = app
                .subtitle_settings_popup
                .as_ref()
                .map(|popup| popup.mode)
                .unwrap_or_default();
            match mode {
                SubtitleSettingsMode::TitleEdit => {
                    input.reset_sequence();
                    match (key.code, key.modifiers) {
                        (KeyCode::Char('s'), KeyModifiers::CONTROL) => {
                            app.save_from_subtitle_settings()
                        }
                        (KeyCode::Esc | KeyCode::Enter, _) => app.escape_subtitle_settings(),
                        _ => {
                            handle_text_input_key(app, key);
                        }
                    }
                }
                SubtitleSettingsMode::LanguageDropdown => {
                    let searching = app
                        .subtitle_settings_popup
                        .as_ref()
                        .is_some_and(|popup| popup.language_search.is_active);
                    if searching {
                        input.reset_sequence();
                        match (key.code, key.modifiers) {
                            (KeyCode::Esc, _) => app.cancel_subtitle_language_search(),
                            (KeyCode::Down, KeyModifiers::NONE)
                            | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                                app.move_subtitle_settings_cursor(1)
                            }
                            (KeyCode::Up, KeyModifiers::NONE)
                            | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                                app.move_subtitle_settings_cursor(-1)
                            }
                            (KeyCode::Enter, _) => app.activate_subtitle_settings(),
                            _ => {
                                handle_text_input_key(app, key);
                            }
                        }
                    } else {
                        match (key.code, key.modifiers) {
                            (KeyCode::Char('K'), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                                input.reset_sequence();
                                app.toggle_subtitle_field_help();
                            }
                            (KeyCode::Char('/'), KeyModifiers::NONE) => {
                                input.reset_sequence();
                                app.start_subtitle_language_search();
                            }
                            (KeyCode::Char('j') | KeyCode::Down, KeyModifiers::NONE) => {
                                input.reset_sequence();
                                app.move_subtitle_settings_cursor(1)
                            }
                            (KeyCode::Char('k') | KeyCode::Up, KeyModifiers::NONE) => {
                                input.reset_sequence();
                                app.move_subtitle_settings_cursor(-1)
                            }
                            (KeyCode::Char('G'), KeyModifiers::NONE) => {
                                input.reset_sequence();
                                app.move_subtitle_settings_to_endpoint(true);
                            }
                            (KeyCode::Enter, _) => {
                                input.reset_sequence();
                                app.activate_subtitle_settings();
                            }
                            _ if is_back_key(key) => {
                                input.reset_sequence();
                                app.escape_subtitle_settings();
                            }
                            _ if input.is_double_g(key) => {
                                app.move_subtitle_settings_to_endpoint(false)
                            }
                            _ => {}
                        }
                    }
                }
                SubtitleSettingsMode::Summary | SubtitleSettingsMode::CodecDropdown => {
                    match (key.code, key.modifiers) {
                        (KeyCode::Char('K'), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                            input.reset_sequence();
                            app.toggle_subtitle_field_help();
                        }
                        (KeyCode::Char('s'), KeyModifiers::CONTROL) => {
                            input.reset_sequence();
                            app.save_from_subtitle_settings()
                        }
                        (KeyCode::Char('i'), KeyModifiers::NONE) => {
                            input.reset_sequence();
                            app.start_subtitle_title_input();
                        }
                        (KeyCode::Char('r'), KeyModifiers::NONE) => {
                            input.reset_sequence();
                            app.reset_focused_field();
                        }
                        (KeyCode::Char('j') | KeyCode::Down, KeyModifiers::NONE) => {
                            input.reset_sequence();
                            app.move_subtitle_settings_cursor(1)
                        }
                        (KeyCode::Char('k') | KeyCode::Up, KeyModifiers::NONE) => {
                            input.reset_sequence();
                            app.move_subtitle_settings_cursor(-1)
                        }
                        (KeyCode::Char('G'), KeyModifiers::NONE) => {
                            input.reset_sequence();
                            app.move_subtitle_settings_to_endpoint(true);
                        }
                        (KeyCode::Enter, _) => {
                            input.reset_sequence();
                            app.activate_subtitle_settings()
                        }
                        _ if is_back_key(key) => {
                            input.reset_sequence();
                            app.escape_subtitle_settings();
                        }
                        _ if input.is_double_g(key) => {
                            app.move_subtitle_settings_to_endpoint(false)
                        }
                        _ => {}
                    }
                }
            }
        }
        Some(Dialog::Keybindings) => {
            if app.keybindings_search.is_active {
                match (key.code, key.modifiers) {
                    (KeyCode::Esc, _) => {
                        input.reset_sequence();
                        app.clear_keybindings_search();
                    }
                    (KeyCode::Enter, _) => {
                        input.reset_sequence();
                        app.finish_keybindings_search();
                    }
                    (KeyCode::Down, KeyModifiers::NONE)
                    | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                        input.reset_sequence();
                        app.scroll_keybindings_down(1);
                    }
                    (KeyCode::Up, KeyModifiers::NONE)
                    | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                        input.reset_sequence();
                        app.scroll_keybindings_up(1);
                    }
                    _ => {
                        if handle_text_input_key(app, key) {
                            input.reset_sequence();
                        }
                    }
                }
            } else {
                match (key.code, key.modifiers) {
                    (KeyCode::Char('/'), KeyModifiers::NONE) => {
                        input.reset_sequence();
                        app.start_keybindings_search();
                    }
                    (KeyCode::Char('?'), _) => {
                        input.reset_sequence();
                        app.dismiss_dialog();
                    }
                    (KeyCode::Char('j') | KeyCode::Down, KeyModifiers::NONE) => {
                        input.reset_sequence();
                        app.scroll_keybindings_down(1);
                    }
                    (KeyCode::Char('k') | KeyCode::Up, KeyModifiers::NONE) => {
                        input.reset_sequence();
                        app.scroll_keybindings_up(1);
                    }
                    (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                        input.reset_sequence();
                        app.scroll_keybindings_down(10);
                    }
                    (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                        input.reset_sequence();
                        app.scroll_keybindings_up(10);
                    }
                    (KeyCode::Char('G'), _) => {
                        input.reset_sequence();
                        app.scroll_keybindings_to_end();
                    }
                    _ if is_back_key(key) => {
                        input.reset_sequence();
                        if !app.keybindings_search.value.is_empty() {
                            app.clear_keybindings_search();
                        } else {
                            app.dismiss_dialog();
                        }
                    }
                    _ if input.is_double_g(key) => app.scroll_keybindings_to_start(),
                    _ => {}
                }
            }
        }
        Some(Dialog::Error) => {
            input.reset_sequence();
            if key.code == KeyCode::Enter || is_back_key(key) {
                app.dismiss_dialog();
            }
        }
        Some(Dialog::ConfirmProcessAll) => match (key.code, key.modifiers) {
            (KeyCode::Enter, _) => {
                input.reset_sequence();
                app.activate_confirm_process_all();
            }
            (KeyCode::Char('h') | KeyCode::Left, KeyModifiers::NONE) => {
                input.reset_sequence();
                app.choose_confirm_process_all(-1);
            }
            (KeyCode::Char('l') | KeyCode::Right, KeyModifiers::NONE) => {
                input.reset_sequence();
                app.choose_confirm_process_all(1);
            }
            (KeyCode::Char('j') | KeyCode::Down, KeyModifiers::NONE) => {
                input.reset_sequence();
                app.scroll_confirm_process_all_down(1);
            }
            (KeyCode::Char('k') | KeyCode::Up, KeyModifiers::NONE) => {
                input.reset_sequence();
                app.scroll_confirm_process_all_up(1);
            }
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                input.reset_sequence();
                app.scroll_confirm_process_all_down(10);
            }
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                input.reset_sequence();
                app.scroll_confirm_process_all_up(10);
            }
            (KeyCode::Char('G'), _) => {
                input.reset_sequence();
                app.scroll_confirm_process_all_to_end();
            }
            _ if is_back_key(key) => {
                input.reset_sequence();
                app.dismiss_dialog();
            }
            _ if input.is_double_g(key) => app.scroll_confirm_process_all_to_start(),
            _ => {}
        },
        Some(Dialog::BatchProcessing) => match (key.code, key.modifiers) {
            (KeyCode::Char('j') | KeyCode::Down, KeyModifiers::NONE) => {
                input.reset_sequence();
                app.move_batch_cursor_down(1);
            }
            (KeyCode::Char('k') | KeyCode::Up, KeyModifiers::NONE) => {
                input.reset_sequence();
                app.move_batch_cursor_up(1);
            }
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                input.reset_sequence();
                app.move_batch_cursor_down(10);
            }
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                input.reset_sequence();
                app.move_batch_cursor_up(10);
            }
            // Stopping a running batch always goes through the confirm prompt below;
            // there is deliberately no immediate-stop key here.
            _ if is_back_key(key) => {
                input.reset_sequence();
                app.request_cancel_edit();
            }
            _ => {}
        },
        Some(Dialog::ConfirmReset) => match (key.code, key.modifiers) {
            (KeyCode::Char('h') | KeyCode::Left, KeyModifiers::NONE) => {
                input.reset_sequence();
                app.choose_reset(-1);
            }
            (KeyCode::Char('l') | KeyCode::Right, KeyModifiers::NONE) => {
                input.reset_sequence();
                app.choose_reset(1);
            }
            (KeyCode::Enter, _) => {
                input.reset_sequence();
                app.activate_reset();
            }
            _ if is_back_key(key) => {
                input.reset_sequence();
                app.dismiss_reset();
            }
            _ => {}
        },
        // Deliberately handles no back key: acknowledging is the only way out of the
        // source-changed notice, because deferring it would leave a staged edit
        // pointing at tracks the file no longer has, silently unprocessable. Escape
        // and `q` fall through to the catch-all below and do nothing.
        Some(Dialog::ResolveConflicts) => match (key.code, key.modifiers) {
            (KeyCode::Char('j') | KeyCode::Down, KeyModifiers::NONE) => {
                input.reset_sequence();
                app.scroll_conflicts(1);
            }
            (KeyCode::Char('k') | KeyCode::Up, KeyModifiers::NONE) => {
                input.reset_sequence();
                app.scroll_conflicts(-1);
            }
            (KeyCode::Enter | KeyCode::Char(' '), _) => {
                input.reset_sequence();
                app.acknowledge_conflicts();
            }
            _ => {}
        },
        None => return handle_layer_key(app, input, key),
    }
    InputOutcome::Continue
}

/// The editing keys every text field shares. Callers match their own keys first —
/// Escape, Enter, Ctrl-s, list navigation — and fall through to here, so cursor
/// movement, deletion and typing behave identically in all six fields. Returns whether
/// the key was consumed.
fn handle_text_input_key(app: &mut App, key: KeyEvent) -> bool {
    match (key.code, key.modifiers) {
        (KeyCode::Left, KeyModifiers::NONE) => app.move_text_cursor(-1),
        (KeyCode::Right, KeyModifiers::NONE) => app.move_text_cursor(1),
        (KeyCode::Home, KeyModifiers::NONE) => app.move_text_home(false),
        (KeyCode::End, KeyModifiers::NONE) => app.move_text_home(true),
        // Readline habits, since every caller here is a single-line field: Ctrl-a/e for
        // the ends, Ctrl-w for the previous word, Ctrl-u to clear what is behind the
        // cursor. Callers that bind these to something else match them first.
        (KeyCode::Char('a'), KeyModifiers::CONTROL) => app.move_text_home(false),
        (KeyCode::Char('e'), KeyModifiers::CONTROL) => app.move_text_home(true),
        (KeyCode::Backspace, KeyModifiers::NONE) => app.backspace_text(),
        (KeyCode::Delete, KeyModifiers::NONE) => app.delete_text(),
        (KeyCode::Char('w'), KeyModifiers::CONTROL) => app.delete_word_before_cursor(),
        (KeyCode::Char('u'), KeyModifiers::CONTROL) => app.clear_text_to_start(),
        // Which characters actually land is the field's own `CharClass`; the key layer
        // does not second-guess it.
        (KeyCode::Char(character), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
            app.input_text_char(character)
        }
        _ => return false,
    }
    true
}

fn handle_layer_key(app: &mut App, input: &mut InputState, key: KeyEvent) -> InputOutcome {
    if app.layer == Layer::Files && app.file_search.is_active {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => {
                input.reset_sequence();
                app.cancel_file_search();
            }
            (KeyCode::Enter, _) => {
                input.reset_sequence();
                app.finish_file_search();
            }
            (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                input.reset_sequence();
                app.select_next();
            }
            (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                input.reset_sequence();
                app.select_previous();
            }
            _ => {
                if handle_text_input_key(app, key) {
                    input.reset_sequence();
                }
            }
        }
        return InputOutcome::Continue;
    }
    if is_back_key(key) {
        input.reset_sequence();
        if app.layer == Layer::Files && app.file_search_has_query() {
            app.clear_file_search();
            return InputOutcome::Continue;
        }
        return if app.back() {
            InputOutcome::Continue
        } else {
            InputOutcome::Quit
        };
    }
    match (key.code, key.modifiers) {
        (KeyCode::Char('/'), KeyModifiers::NONE) if app.layer == Layer::Files => {
            input.reset_sequence();
            app.start_file_search();
        }
        (KeyCode::Char('?'), _) => {
            input.reset_sequence();
            app.show_keybindings();
        }
        (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
            input.reset_sequence();
            app.scroll_down();
        }
        (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
            input.reset_sequence();
            app.scroll_up();
        }
        (KeyCode::Char('d'), KeyModifiers::NONE) if app.layer == Layer::Streams => {
            input.reset_sequence();
            app.toggle_delete_selected_stream();
        }
        (KeyCode::Char('s'), KeyModifiers::CONTROL) if app.layer == Layer::Streams => {
            input.reset_sequence();
            app.request_process_all();
        }
        (KeyCode::Char('s'), KeyModifiers::CONTROL) if app.layer == Layer::Files => {
            input.reset_sequence();
            app.request_process_all();
        }
        (KeyCode::Char('r'), KeyModifiers::NONE) if app.layer == Layer::Streams => {
            input.reset_sequence();
            app.reset_focused_field();
        }
        (KeyCode::Char('R'), KeyModifiers::NONE | KeyModifiers::SHIFT)
            if app.layer != Layer::Files =>
        {
            input.reset_sequence();
            app.request_reset_current_file();
        }
        (KeyCode::Char('k'), KeyModifiers::CONTROL) if app.layer == Layer::Streams => {
            input.reset_sequence();
            app.move_selected_stream(-1);
        }
        (KeyCode::Char('j'), KeyModifiers::CONTROL) if app.layer == Layer::Streams => {
            input.reset_sequence();
            app.move_selected_stream(1);
        }
        (KeyCode::Char('h'), KeyModifiers::CONTROL) if app.layer == Layer::Streams => {
            input.reset_sequence();
            app.transfer_subtitle(-1);
        }
        (KeyCode::Char('l'), KeyModifiers::CONTROL) if app.layer == Layer::Streams => {
            input.reset_sequence();
            app.transfer_subtitle(1);
        }
        (KeyCode::Char('i'), KeyModifiers::NONE) if app.layer == Layer::Streams => {
            input.reset_sequence();
            app.open_stream_details();
        }
        (KeyCode::Char('i'), KeyModifiers::NONE) if app.layer == Layer::StreamDetails => {
            input.reset_sequence();
            app.back();
        }
        (KeyCode::Char('h'), KeyModifiers::NONE) if app.layer == Layer::Streams => {
            input.reset_sequence();
            if !app.move_subtitle_column(-1) {
                app.back();
            }
        }
        (KeyCode::Char('l'), KeyModifiers::NONE) if app.layer == Layer::Streams => {
            input.reset_sequence();
            app.move_subtitle_column(1);
        }
        (KeyCode::Char('l'), KeyModifiers::NONE) if app.layer == Layer::Files => {
            input.reset_sequence();
            app.enter();
        }
        (KeyCode::Enter, _) if app.layer == Layer::Streams => {
            input.reset_sequence();
            app.open_track_settings();
        }
        (KeyCode::Enter, _) if app.layer == Layer::Files => {
            input.reset_sequence();
            app.enter();
        }
        (KeyCode::Char('j') | KeyCode::Down, KeyModifiers::NONE) => {
            input.reset_sequence();
            app.select_next();
        }
        (KeyCode::Char('k') | KeyCode::Up, KeyModifiers::NONE) => {
            input.reset_sequence();
            app.select_previous();
        }
        (KeyCode::Char('G'), _) => {
            input.reset_sequence();
            app.select_last();
        }
        _ if app.layer == Layer::Files => {
            // `R` doubles as the back half of the `zR` (unfold-all) chord, so it must
            // go through `z_command` first — only once that says "not part of a z
            // chord" does a bare `R` mean "reset every staged file" here.
            if let Some(cmd) = input.z_command(key) {
                match cmd {
                    ZCommand::Toggle => app.toggle_fold_selected_file(),
                    ZCommand::Open => app.unfold_selected_file(),
                    ZCommand::Close => app.fold_selected_file(),
                    ZCommand::CloseAll => app.fold_all_files(),
                    ZCommand::OpenAll => app.unfold_all_files(),
                }
            } else if key.code == KeyCode::Char('R')
                && matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT)
            {
                app.request_reset_all_files();
            } else if key.code == KeyCode::Char('r') && key.modifiers == KeyModifiers::NONE {
                if let Some(path) = app.selected_file().map(|file| file.path.clone()) {
                    app.request_reset_file(path);
                }
            } else if input.is_double_g(key) {
                app.select_first();
            }
        }
        _ if input.is_double_g(key) => app.select_first(),
        _ => {}
    }
    InputOutcome::Continue
}

fn is_back_key(key: KeyEvent) -> bool {
    key.code == KeyCode::Esc
        || matches!(
            (key.code, key.modifiers),
            (KeyCode::Char('q'), KeyModifiers::NONE)
        )
}

#[cfg(test)]
mod tests {
    use crate::subtitle::SubtitleSource;
    use kernal::prelude::*;
    use std::{
        fs,
        path::PathBuf,
        sync::mpsc,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;
    use crate::{
        app::{
            CancelEditChoice, ContainerSettingsField, ContainerSettingsMode,
            ContainerSettingsPopup, CustomResolutionDraft, CustomResolutionField, ResetScope,
            SubtitleSettingsField, TextInputState, VideoSettingsField, VideoSettingsMode,
            VideoSettingsPopup,
        },
        edit::{CustomResolution, CustomScaling, EditRequest, VideoResolution},
        files::{FileEntry, FileFingerprint},
        probe::{MediaInfo, ProbeOutcome, ProbeRequest},
        subtitle::{SidecarEntry, SubtitleFormat},
    };

    fn test_app() -> (App, PathBuf) {
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-input-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let (probe_tx, _) = mpsc::channel::<ProbeRequest>();
        let (conflict_tx, _) = mpsc::channel::<ProbeRequest>();
        let (edit_tx, _) = mpsc::channel::<EditRequest>();
        let app = App::new(
            directory.clone(),
            probe_tx,
            conflict_tx,
            edit_tx.clone(),
            edit_tx,
        )
        .unwrap();
        (app, directory)
    }

    fn subtitle_settings_app() -> (App, PathBuf) {
        let (mut app, directory) = test_app();
        app.outcome = Some(ProbeOutcome::Video(
            MediaInfo::from_json(serde_json::json!({
                "streams": [
                    {"index": 0, "codec_type": "video", "codec_name": "h264"},
                    {
                        "index": 1,
                        "codec_type": "subtitle",
                        "codec_name": "subrip",
                        "tags": {"language": "eng"},
                        "disposition": {"default": 0}
                    }
                ]
            }))
            .unwrap(),
        ));
        app.stream_order = vec![0, 1];
        app.layer = Layer::Streams;
        app.selected_stream = app
            .track_rows()
            .iter()
            .position(|track| *track == crate::app::TrackRef::Embedded(1))
            .unwrap();
        app.open_track_settings();
        (app, directory)
    }

    fn container_settings_app() -> (App, PathBuf) {
        let (mut app, directory) = test_app();
        app.outcome = Some(ProbeOutcome::Video(
            MediaInfo::from_json(serde_json::json!({
                "streams": [{"index": 0, "codec_type": "video", "codec_name": "h264"}]
            }))
            .unwrap(),
        ));
        app.stream_order = vec![0];
        app.layer = Layer::Streams;
        app.selected_stream = app
            .track_rows()
            .iter()
            .position(|track| *track == crate::app::TrackRef::Container)
            .unwrap();
        app.open_container_settings();
        (app, directory)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(code), KeyModifiers::CONTROL)
    }

    fn exit_result(
        layer: Layer,
        dialog: Option<Dialog>,
        code: KeyCode,
    ) -> (InputOutcome, Layer, Option<Dialog>, Option<String>) {
        let (mut app, directory) = test_app();
        app.layer = layer;
        app.dialog = dialog;
        let outcome = handle_key(&mut app, &mut InputState::default(), key(code));
        let result = (outcome, app.layer, app.dialog, app.notice.clone());
        drop(app);
        fs::remove_dir_all(directory).unwrap();
        result
    }

    /// A probed file with editable tracks, left on the file list rather than in the track
    /// editor — the state the user is in while browsing, where track-editing keys must
    /// stay inert. Unlike `test_app`, the directory actually holds files, so a file is
    /// genuinely selected and file-list keys have something to act on.
    fn browsing_app() -> (App, PathBuf) {
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-input-browsing-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        for name in ["a.mkv", "b.mkv"] {
            fs::write(directory.join(name), b"media").unwrap();
        }
        let (probe_tx, _) = mpsc::channel::<ProbeRequest>();
        let (conflict_tx, _) = mpsc::channel::<ProbeRequest>();
        let (edit_tx, _) = mpsc::channel::<EditRequest>();
        let mut app = App::new(
            directory.clone(),
            probe_tx,
            conflict_tx,
            edit_tx.clone(),
            edit_tx,
        )
        .unwrap();
        app.select_first();
        app.outcome = Some(ProbeOutcome::Video(
            MediaInfo::from_json(serde_json::json!({
                "streams": [
                    {"index": 0, "codec_type": "video", "codec_name": "h264"},
                    {"index": 1, "codec_type": "audio", "codec_name": "aac"},
                    {
                        "index": 2,
                        "codec_type": "subtitle",
                        "codec_name": "subrip",
                        "tags": {"language": "eng"}
                    }
                ]
            }))
            .unwrap(),
        ));
        app.stream_order = vec![0, 1, 2];
        app.layer = Layer::Files;
        (app, directory)
    }

    #[test]
    fn track_editing_keys_should_do_nothing_while_the_user_is_browsing_files() {
        // Arrange: every one of these keys is guarded on `Layer::Streams` here, and the
        // `App` method behind it re-checks the layer for itself. This pins the outer half
        // of that pair and the end-to-end contract through `handle_key`: a keystroke aimed
        // at the file list must never reorder or delete a track in the file the cursor
        // happens to be sitting on. Because the guard is doubled, dropping either one
        // alone leaves the behaviour intact — so this test does not fail on that edit by
        // itself, and is a contract lock rather than a single-guard regression test.
        let keys = [
            ("d (delete track)", key(KeyCode::Char('d'))),
            ("i (stream details)", key(KeyCode::Char('i'))),
            ("h (subtitle column left)", key(KeyCode::Char('h'))),
            ("ctrl+k (move track up)", ctrl('k')),
            ("ctrl+j (move track down)", ctrl('j')),
            ("ctrl+h (transfer subtitle out)", ctrl('h')),
            ("ctrl+l (transfer subtitle in)", ctrl('l')),
        ];

        for (label, event) in keys {
            let (mut app, directory) = browsing_app();
            let order_before = app.stream_order.clone();

            // Act
            let outcome = handle_key(&mut app, &mut InputState::default(), event);

            // Assert: the app stays where it was and the track list is untouched.
            assert_that!(outcome).is_equal_to(InputOutcome::Continue);
            assert_eq!(app.layer, Layer::Files, "{label} must not change layer");
            assert_eq!(
                app.stream_order, order_before,
                "{label} must not reorder tracks from the file list",
            );
            assert!(
                app.deleted_streams.is_empty(),
                "{label} must not delete a track from the file list",
            );
            assert!(
                app.dialog.is_none(),
                "{label} must not open a dialog from the file list",
            );

            drop(app);
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn file_search_should_only_open_from_the_file_list() {
        // Arrange: `/` starts a filename search, which only means anything against the
        // file list. Firing it from the track editor would swallow the keystroke and
        // leave the user typing into a search they cannot see results for. As with the
        // track-editing keys above, `App::start_file_search` re-checks the layer, so this
        // covers both halves of the contract rather than one guard in isolation.
        let (mut app, directory) = browsing_app();
        app.layer = Layer::Streams;

        // Act
        handle_key(
            &mut app,
            &mut InputState::default(),
            key(KeyCode::Char('/')),
        );

        // Assert
        assert!(
            !app.file_search.is_active,
            "search must not open from Streams"
        );
        assert_eq!(app.layer, Layer::Streams);

        drop(app);
        fs::remove_dir_all(directory).unwrap();

        // And the guarded-for case still works.
        let (mut app, directory) = browsing_app();
        handle_key(
            &mut app,
            &mut InputState::default(),
            key(KeyCode::Char('/')),
        );
        assert!(app.file_search.is_active, "search must open from Files");

        drop(app);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn lowercase_r_should_reset_a_field_in_the_editor_but_a_whole_file_from_the_list() {
        // Arrange: `r` is bound twice with opposite scopes — in the editor it clears the
        // focused field, from the file list it discards a staged file's edits entirely.
        // The table's guard (`layer == Layer::Streams`) is the only thing separating a
        // one-field undo from throwing away every edit staged against that file, so the
        // two must be proven distinct rather than assumed.

        // Act / Assert: from the file list, it asks before discarding, and the scope it
        // stages is the one selected file — not the whole directory.
        let (mut app, directory) = browsing_app();
        handle_key(
            &mut app,
            &mut InputState::default(),
            key(KeyCode::Char('r')),
        );
        assert!(
            app.dialog.is_some(),
            "r from the file list must confirm before discarding a file's edits",
        );
        assert!(
            matches!(app.pending_reset(), Some(ResetScope::File(_))),
            "r from the file list must scope the reset to one file, got {:?}",
            app.pending_reset(),
        );
        drop(app);
        fs::remove_dir_all(directory).unwrap();

        // Act / Assert: from the editor it resets the focused field and raises no such
        // confirmation, because nothing beyond that field is being thrown away.
        let (mut app, directory) = browsing_app();
        app.layer = Layer::Streams;
        handle_key(
            &mut app,
            &mut InputState::default(),
            key(KeyCode::Char('r')),
        );
        assert!(
            app.dialog.is_none(),
            "r in the editor resets one field and must not raise a file-wide prompt",
        );
        assert_eq!(app.layer, Layer::Streams);
        drop(app);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn capital_r_should_reset_every_staged_file_from_the_list_and_only_the_open_one_elsewhere() {
        // Arrange: `R` is guarded by the table's one inverted condition
        // (`layer != Layer::Files`), with the file-list case handled further down. Both
        // sides raise the same `ConfirmReset` dialog, so the dialog alone proves nothing —
        // what separates them is the scope staged behind it. A regression that let the
        // file-list arm win everywhere would discard every staged file in the directory
        // when the user meant only the file they had open.

        // Act / Assert: from the file list, every staged file is in scope.
        let (mut app, directory) = browsing_app();
        handle_key(
            &mut app,
            &mut InputState::default(),
            KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT),
        );
        assert!(matches!(app.dialog, Some(Dialog::ConfirmReset)));
        assert!(
            matches!(app.pending_reset(), Some(ResetScope::AllFiles)),
            "R from the file list must scope the reset to every staged file, got {:?}",
            app.pending_reset(),
        );
        drop(app);
        fs::remove_dir_all(directory).unwrap();

        // Act / Assert: from the editor, only the open file is.
        let (mut app, directory) = browsing_app();
        app.layer = Layer::Streams;
        handle_key(
            &mut app,
            &mut InputState::default(),
            KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT),
        );
        assert!(matches!(app.dialog, Some(Dialog::ConfirmReset)));
        assert!(
            matches!(app.pending_reset(), Some(ResetScope::CurrentFile)),
            "R in the editor must scope the reset to the open file, got {:?}",
            app.pending_reset(),
        );
        drop(app);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn enter_should_do_nothing_in_a_layer_that_defines_no_action_for_it() {
        // Arrange: Enter is claimed by both `Streams` (open track settings) and `Files`
        // (descend). `StreamDetails` is neither — it is a read-only view, and Enter there
        // must not fall through to whichever arm happens to be listed first.
        let (mut app, directory) = browsing_app();
        app.layer = Layer::StreamDetails;

        // Act
        let outcome = handle_key(&mut app, &mut InputState::default(), key(KeyCode::Enter));

        // Assert
        assert_that!(outcome).is_equal_to(InputOutcome::Continue);
        assert_eq!(app.layer, Layer::StreamDetails);
        assert!(app.dialog.is_none());

        drop(app);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn process_all_should_be_reachable_from_both_the_file_list_and_the_track_editor() {
        // Arrange: ctrl+s is listed twice, once per layer, so a regression that dropped
        // either arm would still look correct from the other. Both are checked here.
        for layer in [Layer::Files, Layer::Streams] {
            let (mut app, directory) = browsing_app();
            app.layer = layer;

            // Act
            handle_key(&mut app, &mut InputState::default(), ctrl('s'));

            // Assert: something was raised — a dialog to confirm, or a notice explaining
            // why there is nothing to process. Silence would mean the key did nothing.
            assert!(
                app.dialog.is_some() || app.notice.is_some(),
                "ctrl+s must be answered in {layer:?}",
            );

            drop(app);
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn escape_and_q_should_have_identical_results_in_every_layer_and_dialog() {
        let contexts = [
            (Layer::Files, None),
            (Layer::Streams, None),
            (Layer::StreamDetails, None),
            (Layer::Streams, Some(Dialog::Keybindings)),
            (Layer::Streams, Some(Dialog::ContainerSettings)),
            (Layer::Streams, Some(Dialog::VideoSettings)),
            (Layer::Streams, Some(Dialog::SubtitleSettings)),
            (Layer::Streams, Some(Dialog::BatchProcessing)),
            (Layer::Streams, Some(Dialog::ConfirmCancel)),
            (Layer::Streams, Some(Dialog::Error)),
        ];

        for (layer, dialog) in contexts {
            let escape = exit_result(layer, dialog, KeyCode::Esc);
            let q = exit_result(layer, dialog, KeyCode::Char('q'));
            assert_eq!(escape, q, "Esc/q mismatch in {layer:?} with {dialog:?}");
        }
    }

    #[test]
    fn escape_and_q_should_open_then_dismiss_processing_cancellation_confirmation() {
        for code in [KeyCode::Esc, KeyCode::Char('q')] {
            let (mut app, directory) = test_app();
            app.layer = Layer::Streams;
            app.dialog = Some(Dialog::BatchProcessing);
            let mut input = InputState::default();

            handle_key(&mut app, &mut input, key(code));
            assert_eq!(app.dialog, Some(Dialog::ConfirmCancel));

            handle_key(&mut app, &mut input, key(code));
            assert_eq!(app.dialog, Some(Dialog::BatchProcessing));

            drop(app);
            fs::remove_dir_all(directory).unwrap();
        }
    }

    /// Ctrl-c used to stop a running batch outright. A batch is expensive and
    /// irreversible enough that a mistyped chord must not kill it: cancelling now only
    /// happens through the confirm dialog.
    #[test]
    fn ctrl_c_should_not_cancel_a_running_batch() {
        let (mut app, directory) = test_app();
        app.layer = Layer::Streams;
        app.dialog = Some(Dialog::BatchProcessing);
        app.active_batch = Some(crate::staging::BatchState {
            cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            items: vec![crate::staging::BatchItem {
                path: directory.join("movie.mkv"),
                label: None,
                fraction: None,
                status: crate::staging::BatchItemStatus::Running,
                output_path: None,
            }],
            started: std::time::Instant::now(),
        });
        let mut input = InputState::default();

        handle_key(&mut app, &mut input, ctrl('c'));

        assert_eq!(app.dialog, Some(Dialog::BatchProcessing));
        let batch = app
            .active_batch
            .as_ref()
            .expect("the batch is still running");
        assert!(!batch.cancelled.load(std::sync::atomic::Ordering::SeqCst));

        drop(app);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn error_dialog_should_consume_navigation_without_changing_underlying_selection() {
        let (mut app, directory) = test_app();
        app.layer = Layer::Streams;
        app.stream_order = vec![0, 1, 2, 3];
        app.selected_stream = 1;
        app.dialog = Some(Dialog::Error);
        let mut input = InputState::default();

        handle_key(&mut app, &mut input, key(KeyCode::Char('j')));
        handle_key(&mut app, &mut input, key(KeyCode::Char('G')));

        assert_eq!(app.selected_stream, 1);
        assert_eq!(app.dialog, Some(Dialog::Error));
        drop(app);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn l_should_enter_the_streams_panel_like_enter() {
        for code in [KeyCode::Char('l'), KeyCode::Enter] {
            let (mut app, directory) = test_app();
            app.outcome = Some(ProbeOutcome::Video(
                MediaInfo::from_json(serde_json::json!({
                    "streams": [{"index": 0, "codec_type": "video", "codec_name": "h264"}]
                }))
                .unwrap(),
            ));
            app.stream_order = vec![0];

            handle_key(&mut app, &mut InputState::default(), key(code));

            assert_eq!(app.layer, Layer::Streams);
            drop(app);
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn h_should_leave_the_streams_panel() {
        let (mut app, directory) = test_app();
        app.layer = Layer::Streams;

        handle_key(
            &mut app,
            &mut InputState::default(),
            key(KeyCode::Char('h')),
        );

        assert_eq!(app.layer, Layer::Files);
        drop(app);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn h_and_l_should_switch_between_wide_subtitle_columns_before_leaving() {
        // Arrange
        let (mut app, directory) = test_app();
        app.outcome = Some(ProbeOutcome::Video(
            MediaInfo::from_json(serde_json::json!({
                "streams": [
                    {"index": 0, "codec_type": "video", "codec_name": "h264"},
                    {"index": 1, "codec_type": "subtitle", "codec_name": "subrip"}
                ]
            }))
            .unwrap(),
        ));
        app.stream_order = vec![0, 1];
        app.sidecars.push(SidecarEntry {
            path: directory.join("movie.eng.srt"),
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
        });
        app.layer = Layer::Streams;
        app.selected_stream = 2;
        app.set_subtitle_columns_side_by_side(true);
        let mut input = InputState::default();

        // Act / Assert
        handle_key(&mut app, &mut input, key(KeyCode::Char('l')));
        assert_eq!(app.selected_stream, 3);
        assert_eq!(app.layer, Layer::Streams);
        handle_key(&mut app, &mut input, key(KeyCode::Char('h')));
        assert_eq!(app.selected_stream, 2);
        assert_eq!(app.layer, Layer::Streams);
        handle_key(&mut app, &mut input, key(KeyCode::Char('h')));
        assert_eq!(app.layer, Layer::Files);

        // Cleanup
        drop(app);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn ctrl_h_and_ctrl_l_should_trigger_transfer_subtitle() {
        // Arrange
        let (mut app, directory) = test_app();
        app.outcome = Some(ProbeOutcome::Video(
            MediaInfo::from_json(serde_json::json!({
                "streams": [
                    {"index": 0, "codec_type": "video", "codec_name": "h264"},
                    {"index": 1, "codec_type": "subtitle", "codec_name": "subrip"}
                ]
            }))
            .unwrap(),
        ));
        app.stream_order = vec![0, 1];
        app.layer = Layer::Streams;
        app.selected_stream = 2;
        let mut input = InputState::default();

        // Act - Ctrl+l: Mark for export
        handle_key(&mut app, &mut input, ctrl('l'));
        let source = SubtitleSource::Embedded(1);
        assert!(app.subtitle_changes.contains_key(&source));

        // Act - Ctrl+h: Cancel export
        handle_key(&mut app, &mut input, ctrl('h'));
        assert!(!app.subtitle_changes.contains_key(&source));

        // Cleanup
        drop(app);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn i_should_open_details_when_the_container_is_selected() {
        // Arrange
        let (mut app, directory) = test_app();
        app.outcome = Some(ProbeOutcome::Video(
            MediaInfo::from_json(serde_json::json!({
                "format": {
                    "format_name": "matroska,webm",
                    "duration": "60.0"
                },
                "streams": [
                    {"index": 0, "codec_type": "video", "codec_name": "h264"}
                ]
            }))
            .unwrap(),
        ));
        app.layer = Layer::Streams;
        app.selected_stream = 0;

        // Act
        handle_key(
            &mut app,
            &mut InputState::default(),
            key(KeyCode::Char('i')),
        );

        // Assert
        assert_eq!(app.layer, Layer::StreamDetails);
        assert_eq!(app.details_scroll, 0);

        // Cleanup
        drop(app);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn i_should_close_video_information_when_it_is_already_open() {
        // Arrange
        let (mut app, directory) = test_app();
        app.outcome = Some(ProbeOutcome::Video(
            MediaInfo::from_json(serde_json::json!({
                "streams": [
                    {"index": 0, "codec_type": "video", "codec_name": "h264"}
                ]
            }))
            .unwrap(),
        ));
        app.layer = Layer::Streams;
        app.stream_order = vec![0];
        app.selected_stream = 1;
        app.open_stream_details();

        // Act
        handle_key(
            &mut app,
            &mut InputState::default(),
            key(KeyCode::Char('i')),
        );

        // Assert
        assert_eq!(app.layer, Layer::Streams);

        // Cleanup
        drop(app);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn escape_and_q_should_both_close_a_video_settings_dropdown_one_level() {
        for code in [KeyCode::Esc, KeyCode::Char('q')] {
            let (mut app, directory) = test_app();
            app.dialog = Some(Dialog::VideoSettings);
            app.video_settings_popup = Some(VideoSettingsPopup {
                stream_index: 0,
                field: VideoSettingsField::Codec,
                mode: VideoSettingsMode::Dropdown,
                codec_cursor: 0,
                resolution_cursor: 0,
                custom_resolution: None,
            });

            handle_key(&mut app, &mut InputState::default(), key(code));

            assert_eq!(app.dialog, Some(Dialog::VideoSettings));
            assert_eq!(
                app.video_settings_popup.as_ref().unwrap().mode,
                VideoSettingsMode::Summary
            );
            drop(app);
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn custom_resolution_keys_should_edit_fields_and_select_scaling_dropdown() {
        // Arrange
        let (mut app, directory) = test_app();
        app.dialog = Some(Dialog::VideoSettings);
        app.video_settings_popup = Some(VideoSettingsPopup {
            stream_index: 0,
            field: VideoSettingsField::Resolution,
            mode: VideoSettingsMode::CustomResolution,
            codec_cursor: 0,
            resolution_cursor: 0,
            custom_resolution: Some(CustomResolutionDraft {
                width: TextInputState::default(),
                height: TextInputState::default(),
                scaling: CustomScaling::FitPad,
                field: CustomResolutionField::Width,
                scaling_cursor: 0,
                scaling_dropdown_open: false,
            }),
        });
        let mut input = InputState::default();

        // Act: digits are ignored until i activates the selected value field.
        handle_key(&mut app, &mut input, key(KeyCode::Char('9')));
        assert_eq!(
            app.video_settings_popup
                .as_ref()
                .unwrap()
                .custom_resolution
                .as_ref()
                .unwrap()
                .width
                .value,
            ""
        );
        handle_key(&mut app, &mut input, key(KeyCode::Char('i')));
        handle_key(&mut app, &mut input, key(KeyCode::Char('1')));
        handle_key(&mut app, &mut input, key(KeyCode::Char('2')));
        handle_key(&mut app, &mut input, key(KeyCode::Backspace));
        handle_key(&mut app, &mut input, key(KeyCode::Enter));
        handle_key(&mut app, &mut input, key(KeyCode::Char('i')));
        handle_key(&mut app, &mut input, key(KeyCode::Char('8')));
        handle_key(&mut app, &mut input, key(KeyCode::Enter));
        handle_key(&mut app, &mut input, key(KeyCode::Enter));
        handle_key(&mut app, &mut input, key(KeyCode::Down));
        handle_key(&mut app, &mut input, key(KeyCode::Enter));

        // Assert
        let draft = app
            .video_settings_popup
            .as_ref()
            .unwrap()
            .custom_resolution
            .as_ref()
            .unwrap();
        assert_eq!(draft.width.value, "1");
        assert_eq!(draft.height.value, "8");
        assert_eq!(draft.field, CustomResolutionField::Scaling);
        assert_eq!(draft.scaling, CustomScaling::Stretch);
        assert!(!draft.scaling_dropdown_open);

        // Cleanup
        drop(app);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn readline_keys_should_edit_text_without_colliding_with_dialog_bindings() {
        // Arrange: a subtitle title in edit mode.
        let (mut app, directory) = subtitle_settings_app();
        let mut input = InputState::default();
        app.move_subtitle_settings_cursor(1);
        app.move_subtitle_settings_cursor(1);
        handle_key(&mut app, &mut input, key(KeyCode::Char('i')));
        for character in "one two".chars() {
            handle_key(&mut app, &mut input, key(KeyCode::Char(character)));
        }

        // Act / Assert: Ctrl-a and Ctrl-e reach the ends, as Home and End do.
        handle_key(&mut app, &mut input, ctrl('a'));
        assert_eq!(
            app.subtitle_settings_popup
                .as_ref()
                .unwrap()
                .title_input
                .cursor,
            0
        );
        handle_key(&mut app, &mut input, ctrl('e'));
        assert_eq!(
            app.subtitle_settings_popup
                .as_ref()
                .unwrap()
                .title_input
                .cursor,
            7
        );

        // Act / Assert: Ctrl-w takes the last word, Ctrl-u the rest of the line.
        handle_key(&mut app, &mut input, ctrl('w'));
        assert_eq!(
            app.subtitle_settings_popup
                .as_ref()
                .unwrap()
                .title_input
                .value,
            "one "
        );
        handle_key(&mut app, &mut input, ctrl('u'));
        assert_eq!(
            app.subtitle_settings_popup
                .as_ref()
                .unwrap()
                .title_input
                .value,
            ""
        );

        drop(app);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn ctrl_u_should_scroll_the_keybindings_dialog_until_its_search_is_open() {
        // Arrange: Ctrl-u is a half-page scroll here and a line kill inside the search,
        // so the two must not shadow each other.
        let (mut app, directory) = test_app();
        let mut input = InputState::default();
        app.dialog = Some(Dialog::Keybindings);
        app.keybindings_scroll = 20;

        // Act / Assert: with the search closed it still scrolls.
        handle_key(&mut app, &mut input, ctrl('u'));
        assert_eq!(app.keybindings_scroll, 10);

        // Act: open the search and type a query.
        handle_key(&mut app, &mut input, key(KeyCode::Char('/')));
        for character in "quit".chars() {
            handle_key(&mut app, &mut input, key(KeyCode::Char(character)));
        }

        // Act / Assert: now it clears the query instead of scrolling.
        handle_key(&mut app, &mut input, ctrl('u'));
        assert_eq!(app.keybindings_search.value, "");

        drop(app);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn subtitle_title_should_require_i_and_keep_text_editing_conventional() {
        // Arrange
        let (mut app, directory) = subtitle_settings_app();
        let mut input = InputState::default();
        app.move_subtitle_settings_cursor(1);
        app.move_subtitle_settings_cursor(1);
        assert_eq!(
            app.subtitle_settings_popup.as_ref().unwrap().field,
            SubtitleSettingsField::Title
        );

        // Act / Assert: Enter, Space, and ordinary text do not start editing.
        for code in [KeyCode::Enter, KeyCode::Char(' '), KeyCode::Char('x')] {
            handle_key(&mut app, &mut input, key(code));
            let popup = app.subtitle_settings_popup.as_ref().unwrap();
            assert_eq!(popup.mode, SubtitleSettingsMode::Summary);
            assert!(!popup.title_input.is_active);
            assert_eq!(popup.title_input.value, "");
        }

        // Act: i starts editing; q is text; arrows and Esc retain conventional behavior.
        handle_key(&mut app, &mut input, key(KeyCode::Char('i')));
        handle_key(&mut app, &mut input, key(KeyCode::Char('q')));
        handle_key(&mut app, &mut input, key(KeyCode::Left));
        handle_key(&mut app, &mut input, key(KeyCode::Char('A')));
        handle_key(&mut app, &mut input, key(KeyCode::Esc));

        // Assert
        let popup = app.subtitle_settings_popup.as_ref().unwrap();
        assert_eq!(popup.mode, SubtitleSettingsMode::Summary);
        assert!(!popup.title_input.is_active);
        assert_eq!(
            app.subtitle_popup_metadata().unwrap().title.as_deref(),
            Some("Aq")
        );

        drop(app);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn container_metadata_should_require_i_and_leave_enter_to_the_format_dropdown() {
        // Arrange
        let (mut app, directory) = container_settings_app();
        let mut input = InputState::default();

        // Act / Assert: Enter still drives the Format dropdown.
        handle_key(&mut app, &mut input, key(KeyCode::Enter));
        assert_eq!(
            app.container_settings_popup.as_ref().unwrap().mode,
            ContainerSettingsMode::FormatDropdown
        );
        handle_key(&mut app, &mut input, key(KeyCode::Enter));
        assert_eq!(
            app.container_settings_popup.as_ref().unwrap().mode,
            ContainerSettingsMode::Summary
        );

        // Act / Assert: on a metadata field, Enter no longer starts editing.
        handle_key(&mut app, &mut input, key(KeyCode::Char('j')));
        assert_eq!(
            app.container_settings_popup.as_ref().unwrap().field,
            ContainerSettingsField::Title
        );
        for code in [KeyCode::Enter, KeyCode::Char(' '), KeyCode::Char('x')] {
            handle_key(&mut app, &mut input, key(code));
            let popup = app.container_settings_popup.as_ref().unwrap();
            assert_eq!(popup.mode, ContainerSettingsMode::Summary);
            assert!(!popup.text_input.is_active);
        }

        // Act: i is the one way in, matching every other text field.
        handle_key(&mut app, &mut input, key(KeyCode::Char('i')));
        assert_eq!(
            app.container_settings_popup.as_ref().unwrap().mode,
            ContainerSettingsMode::TextEdit
        );
        handle_key(&mut app, &mut input, key(KeyCode::Char('q')));
        handle_key(&mut app, &mut input, key(KeyCode::Esc));

        // Assert
        let popup = app.container_settings_popup.as_ref().unwrap();
        assert_eq!(popup.mode, ContainerSettingsMode::Summary);
        assert!(!popup.text_input.is_active);
        assert_eq!(
            app.effective_container_metadata().unwrap().title.as_deref(),
            Some("q")
        );

        drop(app);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn subtitle_language_should_use_normal_navigation_until_slash_starts_search() {
        // Arrange
        let (mut app, directory) = subtitle_settings_app();
        let mut input = InputState::default();
        app.move_subtitle_settings_cursor(1);
        handle_key(&mut app, &mut input, key(KeyCode::Enter));

        // Act / Assert: letters do not filter in navigation mode.
        handle_key(&mut app, &mut input, key(KeyCode::Char('d')));
        assert_eq!(
            app.subtitle_settings_popup
                .as_ref()
                .unwrap()
                .language_search
                .value,
            ""
        );

        // Act: slash starts search and Ctrl-n remains result navigation.
        handle_key(&mut app, &mut input, key(KeyCode::Char('/')));
        for character in "dutch".chars() {
            handle_key(&mut app, &mut input, key(KeyCode::Char(character)));
        }
        handle_key(&mut app, &mut input, ctrl('n'));

        // Assert
        let popup = app.subtitle_settings_popup.as_ref().unwrap();
        assert!(popup.language_search.is_active);
        assert_eq!(popup.language_search.value, "dutch");
        assert_eq!(app.filtered_subtitle_languages().len(), 1);

        drop(app);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn subtitle_field_help_should_toggle_follow_navigation_and_leave_text_input_alone() {
        // Arrange
        let (mut app, directory) = subtitle_settings_app();
        let mut input = InputState::default();
        let shifted_k = KeyEvent::new(KeyCode::Char('K'), KeyModifiers::SHIFT);

        // Act: open help, navigate, and enter the language dropdown.
        handle_key(&mut app, &mut input, shifted_k);
        handle_key(&mut app, &mut input, key(KeyCode::Char('j')));
        handle_key(&mut app, &mut input, key(KeyCode::Enter));

        // Assert: help remains passive and follows the parent field.
        let popup = app.subtitle_settings_popup.as_ref().unwrap();
        assert!(popup.help_visible);
        assert_eq!(popup.field, SubtitleSettingsField::Language);
        assert_eq!(popup.mode, SubtitleSettingsMode::LanguageDropdown);

        // Act: K toggles help while navigating, but types during search.
        handle_key(&mut app, &mut input, shifted_k);
        assert!(!app.subtitle_settings_popup.as_ref().unwrap().help_visible);
        handle_key(&mut app, &mut input, shifted_k);
        handle_key(&mut app, &mut input, key(KeyCode::Char('/')));
        handle_key(&mut app, &mut input, shifted_k);

        // Assert
        let popup = app.subtitle_settings_popup.as_ref().unwrap();
        assert!(popup.help_visible);
        assert_eq!(popup.language_search.value, "K");

        // Act: return to the summary, edit Title, and type K there too.
        handle_key(&mut app, &mut input, key(KeyCode::Esc));
        handle_key(&mut app, &mut input, key(KeyCode::Esc));
        handle_key(&mut app, &mut input, key(KeyCode::Char('j')));
        handle_key(&mut app, &mut input, key(KeyCode::Char('i')));
        handle_key(&mut app, &mut input, shifted_k);

        // Assert
        let popup = app.subtitle_settings_popup.as_ref().unwrap();
        assert!(popup.help_visible);
        assert_eq!(popup.mode, SubtitleSettingsMode::TitleEdit);
        assert_eq!(popup.title_input.value, "K");

        // Act: closing and reopening the editor resets help to closed.
        handle_key(&mut app, &mut input, key(KeyCode::Esc));
        handle_key(&mut app, &mut input, key(KeyCode::Char('q')));
        app.open_track_settings();

        // Assert
        assert!(!app.subtitle_settings_popup.as_ref().unwrap().help_visible);

        drop(app);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_should_not_mark_the_selected_stream_as_default() {
        // Arrange
        let (mut app, directory) = subtitle_settings_app();
        app.close_subtitle_settings();
        let mut input = InputState::default();

        // Act
        handle_key(&mut app, &mut input, key(KeyCode::Char('a')));

        // Assert
        assert!(app.default_streams.is_empty());
        assert!(app.default_sidecars.is_empty());

        drop(app);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn selectable_dialogs_should_support_gg_and_g_endpoints() {
        // Arrange: container choices.
        let (mut app, directory) = test_app();
        app.dialog = Some(Dialog::ContainerSettings);
        app.container_settings_popup = Some(ContainerSettingsPopup {
            field: ContainerSettingsField::Format,
            mode: ContainerSettingsMode::FormatDropdown,
            help_visible: false,
            format_cursor: 1,
            text_input: TextInputState::default(),
        });
        let last_container = app.container_choices().len() - 1;
        let mut input = InputState::default();

        // Act / Assert
        handle_key(&mut app, &mut input, key(KeyCode::Char('G')));
        assert_eq!(
            app.container_settings_popup.as_ref().unwrap().format_cursor,
            last_container
        );
        handle_key(&mut app, &mut input, key(KeyCode::Char('g')));
        handle_key(&mut app, &mut input, key(KeyCode::Char('g')));
        assert_eq!(
            app.container_settings_popup.as_ref().unwrap().format_cursor,
            0
        );

        // Arrange / Act / Assert: video summary fields.
        app.dialog = Some(Dialog::VideoSettings);
        app.video_settings_popup = Some(VideoSettingsPopup {
            stream_index: 0,
            field: VideoSettingsField::Codec,
            mode: VideoSettingsMode::Summary,
            codec_cursor: 0,
            resolution_cursor: 0,
            custom_resolution: None,
        });
        handle_key(&mut app, &mut input, key(KeyCode::Char('G')));
        assert_eq!(
            app.video_settings_popup.as_ref().unwrap().field,
            VideoSettingsField::Resolution
        );
        handle_key(&mut app, &mut input, key(KeyCode::Char('g')));
        handle_key(&mut app, &mut input, key(KeyCode::Char('g')));
        assert_eq!(
            app.video_settings_popup.as_ref().unwrap().field,
            VideoSettingsField::Codec
        );

        // Arrange / Act / Assert: cancel choice.
        app.container_target = Some(crate::edit::ContainerFormat::Matroska);
        app.dialog = Some(Dialog::ConfirmCancel);
        app.cancel_edit_choice = CancelEditChoice::CancelProcessing;
        handle_key(&mut app, &mut input, key(KeyCode::Char('g')));
        handle_key(&mut app, &mut input, key(KeyCode::Char('g')));
        assert_eq!(app.cancel_edit_choice, CancelEditChoice::KeepProcessing);
        handle_key(&mut app, &mut input, key(KeyCode::Char('G')));
        assert_eq!(app.cancel_edit_choice, CancelEditChoice::CancelProcessing);

        drop(app);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn escape_and_q_should_both_leave_custom_resolution_editor_one_level() {
        for code in [KeyCode::Esc, KeyCode::Char('q')] {
            // Arrange
            let (mut app, directory) = test_app();
            app.outcome = Some(ProbeOutcome::Video(
                MediaInfo::from_json(serde_json::json!({
                    "streams": [
                        {
                            "index": 0,
                            "codec_type": "video",
                            "codec_name": "h264",
                            "width": 1920,
                            "height": 1080
                        }
                    ]
                }))
                .unwrap(),
            ));
            app.dialog = Some(Dialog::VideoSettings);
            app.video_settings_popup = Some(VideoSettingsPopup {
                stream_index: 0,
                field: VideoSettingsField::Resolution,
                mode: VideoSettingsMode::CustomResolution,
                codec_cursor: 0,
                resolution_cursor: 0,
                custom_resolution: Some(CustomResolutionDraft {
                    width: TextInputState::new("1280".to_string()),
                    height: TextInputState::new("720".to_string()),
                    scaling: CustomScaling::FitPad,
                    field: CustomResolutionField::Scaling,
                    scaling_cursor: 0,
                    scaling_dropdown_open: false,
                }),
            });

            // Act
            handle_key(&mut app, &mut InputState::default(), key(code));

            // Assert
            let popup = app.video_settings_popup.as_ref().unwrap();
            assert_eq!(popup.mode, VideoSettingsMode::Dropdown);
            assert!(popup.custom_resolution.is_none());
            assert_eq!(
                app.video_settings
                    .get(&0)
                    .map(|settings| settings.resolution),
                Some(VideoResolution::Custom(CustomResolution {
                    width: 1280,
                    height: 720,
                    scaling: CustomScaling::FitPad,
                }))
            );

            // Cleanup
            drop(app);
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn language_dropdown_keys_should_search_move_and_commit() {
        // Arrange: the language list is long enough that typing is the only practical way
        // through it, so search, movement and the endpoint jumps all have to work inside
        // the open dropdown rather than leaking out to the pane behind it.
        let (mut app, directory) = subtitle_settings_app();
        let mut input = InputState::default();
        app.subtitle_settings_popup.as_mut().unwrap().field = SubtitleSettingsField::Language;
        handle_key(&mut app, &mut input, key(KeyCode::Enter));
        assert_eq!(
            app.subtitle_settings_popup.as_ref().unwrap().mode,
            crate::app::SubtitleSettingsMode::LanguageDropdown,
        );

        // Act / Assert: `j` and `k` move within the list.
        handle_key(&mut app, &mut input, key(KeyCode::Char('j')));
        let after_down = app
            .subtitle_settings_popup
            .as_ref()
            .unwrap()
            .language_cursor;
        handle_key(&mut app, &mut input, key(KeyCode::Char('k')));
        let after_up = app
            .subtitle_settings_popup
            .as_ref()
            .unwrap()
            .language_cursor;
        assert!(after_down > after_up, "`j` then `k` should return");

        // `G` and `gg` jump to the ends.
        handle_key(&mut app, &mut input, key(KeyCode::Char('G')));
        let last = app
            .subtitle_settings_popup
            .as_ref()
            .unwrap()
            .language_cursor;
        assert_eq!(last, app.filtered_subtitle_languages().len() - 1);
        handle_key(&mut app, &mut input, key(KeyCode::Char('g')));
        handle_key(&mut app, &mut input, key(KeyCode::Char('g')));
        assert_eq!(
            app.subtitle_settings_popup
                .as_ref()
                .unwrap()
                .language_cursor,
            0
        );

        // `/` starts a search that narrows the list, and Enter commits the match.
        handle_key(&mut app, &mut input, key(KeyCode::Char('/')));
        for character in "dut".chars() {
            handle_key(&mut app, &mut input, key(KeyCode::Char(character)));
        }
        let matches = app.filtered_subtitle_languages();
        assert!(
            !matches.is_empty() && matches.len() < 50,
            "the search should narrow the list, got {}",
            matches.len(),
        );
        handle_key(&mut app, &mut input, key(KeyCode::Enter));
        handle_key(&mut app, &mut input, key(KeyCode::Enter));
        assert_eq!(
            app.subtitle_metadata_for(&SubtitleSource::Embedded(1))
                .unwrap()
                .language,
            "nld",
            "the searched-for language should be committed",
        );

        drop(app);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn stream_pane_keys_should_reorder_delete_and_transfer() {
        // Arrange: the editing keys of the stream pane. Each is one keystroke away from
        // its neighbour, so a misrouted binding silently edits the wrong thing. Ctrl-S
        // ("process all staged files") needs a real selected file backed by a cached
        // probe result to stage anything — covered by
        // `app::tests::confirm_process_all_should_route_requests_by_transcode_vs_remux_and_track_batch_items`
        // instead of here, since this app is built with a bare `outcome` and no file
        // list at all.
        let (mut app, directory) = test_app();
        app.outcome = Some(ProbeOutcome::Video(
            MediaInfo::from_json(serde_json::json!({
                "format": {"format_name": "matroska,webm"},
                "streams": [
                    {"index": 0, "codec_type": "video", "codec_name": "h264"},
                    {"index": 1, "codec_type": "audio", "codec_name": "aac"},
                    {"index": 2, "codec_type": "audio", "codec_name": "ac3"},
                    {"index": 3, "codec_type": "subtitle", "codec_name": "subrip",
                     "tags": {"language": "eng"}}
                ]
            }))
            .unwrap(),
        ));
        app.loading = false;
        app.stream_order = vec![0, 1, 2, 3];
        app.layer = Layer::Streams;
        let mut input = InputState::default();
        let audio_row = app
            .track_rows()
            .iter()
            .position(|row| *row == crate::app::TrackRef::Embedded(1))
            .unwrap();

        // Act / Assert: Ctrl-j and Ctrl-k reorder the selected track.
        app.selected_stream = audio_row;
        handle_key(&mut app, &mut input, ctrl('j'));
        assert_eq!(app.stream_order, vec![0, 2, 1, 3], "Ctrl-j moves it down");
        handle_key(&mut app, &mut input, ctrl('k'));
        assert_eq!(app.stream_order, vec![0, 1, 2, 3], "Ctrl-k moves it back");

        // `d` marks the track for deletion and moves on, so a run of `d` deletes a run of
        // tracks; pressing it again on the same row is what undeletes.
        handle_key(&mut app, &mut input, key(KeyCode::Char('d')));
        assert!(app.deleted_streams.contains(&1), "`d` deletes");
        assert_ne!(app.selected_stream, audio_row, "`d` advances the selection");
        app.selected_stream = audio_row;
        handle_key(&mut app, &mut input, key(KeyCode::Char('d')));
        assert!(app.deleted_streams.is_empty(), "`d` undeletes");

        // Ctrl-l exports a subtitle to the external column, Ctrl-h brings it back.
        app.selected_stream = app
            .track_rows()
            .iter()
            .position(|row| *row == crate::app::TrackRef::Embedded(3))
            .unwrap();
        handle_key(&mut app, &mut input, ctrl('l'));
        assert!(app.is_stream_exported(3), "Ctrl-l should stage the export",);
        handle_key(&mut app, &mut input, ctrl('h'));
        assert!(!app.is_stream_exported(3), "Ctrl-h should undo it");

        handle_key(&mut app, &mut input, ctrl('l'));
        assert!(app.is_stream_exported(3), "Ctrl-l should stage the export");

        drop(app);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn z_fold_commands_should_need_a_recent_leading_z() {
        // Arrange: `zo`, `zc`, `zR`, `zM` and `za` are vim's fold commands, and folding a
        // file hides its sidecars from the list. A stale `z` must not turn a later `c`
        // into a fold command, or an unrelated keystroke collapses the tree the user was
        // reading.
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-fold-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        for name in ["a.mkv", "a.eng.srt", "b.mkv", "b.nld.srt"] {
            fs::write(directory.join(name), b"media").unwrap();
        }
        let (probe_tx, _) = mpsc::channel::<ProbeRequest>();
        let (conflict_tx, _) = mpsc::channel::<ProbeRequest>();
        let (edit_tx, _) = mpsc::channel::<EditRequest>();
        let mut app = App::new(
            directory.clone(),
            probe_tx,
            conflict_tx,
            edit_tx.clone(),
            edit_tx,
        )
        .unwrap();
        app.layer = Layer::Files;
        app.select_first();
        let mut input = InputState::default();
        let a = directory.join("a.mkv");
        let b = directory.join("b.mkv");
        let unfolded = |app: &App| {
            [&a, &b]
                .iter()
                .filter(|path| !app.is_file_folded(path))
                .count()
        };
        assert_eq!(unfolded(&app), 0, "everything starts folded");

        // Act / Assert: a bare `o` on its own does nothing.
        handle_key(&mut app, &mut input, key(KeyCode::Char('o')));
        assert_eq!(unfolded(&app), 0, "`o` alone is not a command");

        // `zo` reveals the selected file's sidecar, `zc` hides it again.
        handle_key(&mut app, &mut input, key(KeyCode::Char('z')));
        handle_key(&mut app, &mut input, key(KeyCode::Char('o')));
        assert_eq!(unfolded(&app), 1);
        handle_key(&mut app, &mut input, key(KeyCode::Char('z')));
        handle_key(&mut app, &mut input, key(KeyCode::Char('c')));
        assert_eq!(unfolded(&app), 0);

        // `za` toggles, and `zR` / `zM` act on every file at once.
        handle_key(&mut app, &mut input, key(KeyCode::Char('z')));
        handle_key(&mut app, &mut input, key(KeyCode::Char('a')));
        assert_eq!(unfolded(&app), 1);
        handle_key(&mut app, &mut input, key(KeyCode::Char('z')));
        handle_key(&mut app, &mut input, key(KeyCode::Char('R')));
        assert_eq!(unfolded(&app), 2, "`zR` unfolds every file");
        handle_key(&mut app, &mut input, key(KeyCode::Char('z')));
        handle_key(&mut app, &mut input, key(KeyCode::Char('M')));
        assert_eq!(unfolded(&app), 0, "`zM` folds every file");

        // A `z` that has timed out does not arm the next key.
        handle_key(&mut app, &mut input, key(KeyCode::Char('z')));
        input.pending_z = Some(Instant::now() - Duration::from_secs(2));
        handle_key(&mut app, &mut input, key(KeyCode::Char('o')));
        assert_eq!(unfolded(&app), 0, "an expired `z` must not still be armed");

        // And an unrelated key between them disarms it too.
        handle_key(&mut app, &mut input, key(KeyCode::Char('z')));
        handle_key(&mut app, &mut input, key(KeyCode::Char('x')));
        handle_key(&mut app, &mut input, key(KeyCode::Char('o')));
        assert_eq!(unfolded(&app), 0, "`z x o` is not a fold command");

        drop(app);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn files_panel_bare_r_capital_should_reset_all_without_colliding_with_the_fold_chord() {
        // Regression test: `R` is also the back half of the `zR` (unfold-all) chord,
        // so wiring a bare `R` to "reset every staged file" risks swallowing that
        // chord (or vice versa) depending on match-arm order. A bare `R` (no recent
        // `z`) must open the reset confirmation; `zR` must still unfold everything and
        // must NOT also trigger a reset.
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-files-reset-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        for name in ["a.mkv", "b.mkv"] {
            fs::write(directory.join(name), b"media").unwrap();
        }
        let (probe_tx, _) = mpsc::channel::<ProbeRequest>();
        let (conflict_tx, _) = mpsc::channel::<ProbeRequest>();
        let (edit_tx, _) = mpsc::channel::<EditRequest>();
        let mut app = App::new(
            directory.clone(),
            probe_tx,
            conflict_tx,
            edit_tx.clone(),
            edit_tx,
        )
        .unwrap();
        app.layer = Layer::Files;
        app.select_first();
        let mut input = InputState::default();
        let a = directory.join("a.mkv");
        app.staged_edits.insert(
            a.clone(),
            crate::staging::StagedEdit {
                fingerprint: crate::files::FileFingerprint {
                    length: 0,
                    modified: None,
                },
                stale: false,
                conflict_groups: Default::default(),
                stream_order: Vec::new(),
                moved_streams: Default::default(),
                deleted_streams: Default::default(),
                default_streams: Default::default(),
                default_sidecars: Default::default(),
                video_settings: Default::default(),
                subtitle_changes: Default::default(),
                left_subtitle_order: Vec::new(),
                container_target: Some(crate::edit::ContainerFormat::Mp4),
                container_metadata: None,
                original_stream_order: Vec::new(),
                original_default_streams: Default::default(),
                track_groups: Default::default(),
            },
        );

        // Act / Assert: a bare `R` opens the reset-all confirmation, not a fold chord.
        handle_key(&mut app, &mut input, key(KeyCode::Char('R')));
        assert_eq!(app.dialog, Some(Dialog::ConfirmReset));
        handle_key(&mut app, &mut input, key(KeyCode::Char('l'))); // choose "Reset edits"
        handle_key(&mut app, &mut input, key(KeyCode::Enter));
        assert!(
            app.staged_edits.is_empty(),
            "R should have reset every staged file"
        );
        assert_eq!(app.dialog, None);

        // Stage it again, then confirm `zR` (the actual fold chord) still works and
        // does NOT also trigger a reset.
        app.staged_edits.insert(
            a.clone(),
            crate::staging::StagedEdit {
                fingerprint: crate::files::FileFingerprint {
                    length: 0,
                    modified: None,
                },
                stale: false,
                conflict_groups: Default::default(),
                stream_order: Vec::new(),
                moved_streams: Default::default(),
                deleted_streams: Default::default(),
                default_streams: Default::default(),
                default_sidecars: Default::default(),
                video_settings: Default::default(),
                subtitle_changes: Default::default(),
                left_subtitle_order: Vec::new(),
                container_target: Some(crate::edit::ContainerFormat::Mp4),
                container_metadata: None,
                original_stream_order: Vec::new(),
                original_default_streams: Default::default(),
                track_groups: Default::default(),
            },
        );
        handle_key(&mut app, &mut input, key(KeyCode::Char('z')));
        handle_key(&mut app, &mut input, key(KeyCode::Char('R')));
        assert_eq!(
            app.dialog, None,
            "zR is the fold chord, not the reset confirmation"
        );
        assert!(
            app.staged_edits.contains_key(&a),
            "zR must not reset staged edits"
        );

        drop(app);
        fs::remove_dir_all(directory).unwrap();
    }

    /// Backdates the notice's open time so its button is already armed — the
    /// alternative is a test that sleeps for `CONFLICT_COUNTDOWN`.
    fn arm_conflict_notice(app: &mut App) {
        app.conflict_opened_at =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(60));
    }

    /// Stages a conflicted edit for each name and lists the files, so
    /// `conflicting_paths` (which reads live fingerprints out of `app.files`) sees
    /// them — `test_app`'s one synchronous scan ran before these existed on disk.
    fn conflicted_app(app: &mut App, directory: &std::path::Path, names: &[&str]) {
        app.files.clear();
        for name in names {
            let path = directory.join(name);
            fs::write(&path, b"media").unwrap();
            let fingerprint = FileFingerprint {
                length: 5,
                modified: None,
            };
            app.files.push(FileEntry {
                path: path.clone(),
                display_name: (*name).to_string(),
                fingerprint,
            });
            // `revert_group_edits` refuses to rebuild a group against content it
            // hasn't probed, so the acknowledgement needs a cached probe to land on.
            app.cache.insert(
                crate::app::CacheKey {
                    path: path.clone(),
                    length: fingerprint.length,
                    modified: fingerprint.modified,
                },
                crate::probe::ProbeOutcome::Video(
                    crate::probe::MediaInfo::from_json(serde_json::json!({
                        "format": {"format_name": "matroska,webm"},
                        "streams": [{"index": 0, "codec_type": "video", "codec_name": "h264"}]
                    }))
                    .unwrap(),
                ),
            );
            app.staged_edits.insert(
                path,
                crate::staging::StagedEdit {
                    fingerprint,
                    stale: true,
                    conflict_groups: std::collections::BTreeSet::from(["video"]),
                    stream_order: vec![0],
                    moved_streams: Default::default(),
                    deleted_streams: Default::default(),
                    default_streams: Default::default(),
                    default_sidecars: Default::default(),
                    video_settings: Default::default(),
                    subtitle_changes: Default::default(),
                    left_subtitle_order: Vec::new(),
                    container_target: None,
                    container_metadata: None,
                    original_stream_order: vec![0],
                    original_default_streams: Default::default(),
                    track_groups: Default::default(),
                },
            );
        }
    }

    #[test]
    fn resolve_conflicts_dialog_enter_should_acknowledge_every_listed_file_at_once() {
        // The notice has one button and no per-file choice, so Enter resolves the
        // whole list in one press rather than one file at a time.
        let (mut app, directory) = test_app();
        let mut input = InputState::default();
        conflicted_app(&mut app, &directory, &["a.mkv", "b.mkv"]);
        assert!(app.maybe_open_conflict_dialog());
        assert_eq!(app.conflicting_paths().len(), 2);

        // Enter is inert until the button arms, so a keystroke already in flight when
        // the notice appeared can't revert anything.
        handle_key(&mut app, &mut input, key(KeyCode::Enter));
        assert_eq!(
            app.dialog,
            Some(Dialog::ResolveConflicts),
            "Enter must do nothing while the button is still counting down"
        );
        assert_eq!(app.conflicting_paths().len(), 2);
        arm_conflict_notice(&mut app);

        // Act
        handle_key(&mut app, &mut input, key(KeyCode::Enter));

        // Assert
        assert_eq!(app.dialog, None);
        assert!(
            app.conflicting_paths().is_empty(),
            "acknowledging must clear every listed conflict, not just the first"
        );

        drop(app);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn resolve_conflicts_dialog_should_not_close_on_escape_or_q() {
        // Regression test for the deliberately missing back-key arm: deferring this
        // notice would leave a staged edit pointing at tracks the file no longer has,
        // so acknowledging is the only way out.
        let (mut app, directory) = test_app();
        let mut input = InputState::default();
        conflicted_app(&mut app, &directory, &["a.mkv"]);
        assert!(app.maybe_open_conflict_dialog());

        // Act
        handle_key(&mut app, &mut input, key(KeyCode::Esc));
        handle_key(&mut app, &mut input, key(KeyCode::Char('q')));

        // Assert
        assert_eq!(
            app.dialog,
            Some(Dialog::ResolveConflicts),
            "neither Escape nor q may dismiss the source-changed notice"
        );
        assert_eq!(app.conflicting_paths().len(), 1);

        // Space is the second accepted acknowledgement key.
        arm_conflict_notice(&mut app);
        handle_key(&mut app, &mut input, key(KeyCode::Char(' ')));
        assert_eq!(app.dialog, None);

        drop(app);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn stream_details_should_use_consistent_line_jump_and_endpoint_navigation() {
        let (mut app, directory) = test_app();
        app.layer = Layer::StreamDetails;
        app.details_max_scroll = 30;
        app.details_scroll = 5;
        let mut input = InputState::default();

        handle_key(&mut app, &mut input, key(KeyCode::Char('j')));
        assert_eq!(app.details_scroll, 6);
        handle_key(&mut app, &mut input, ctrl('d'));
        assert_eq!(app.details_scroll, 16);
        handle_key(&mut app, &mut input, key(KeyCode::Char('g')));
        handle_key(&mut app, &mut input, key(KeyCode::Char('g')));
        assert_eq!(app.details_scroll, 0);
        handle_key(&mut app, &mut input, key(KeyCode::Char('G')));
        assert_eq!(app.details_scroll, 30);

        drop(app);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn file_list_should_support_ctrl_d_and_ctrl_u_ten_item_jumps() {
        // Arrange
        let (mut app, directory) = test_app();
        app.files = (0..25)
            .map(|index| FileEntry {
                path: directory.join(format!("{index:02}.mkv")),
                display_name: format!("{index:02}.mkv"),
                fingerprint: FileFingerprint {
                    length: 0,
                    modified: None,
                },
            })
            .collect();
        app.list_state.select(Some(5));
        let mut input = InputState::default();

        // Act and assert
        handle_key(&mut app, &mut input, ctrl('d'));
        assert_eq!(app.list_state.selected(), Some(15));
        handle_key(&mut app, &mut input, ctrl('d'));
        assert_eq!(app.list_state.selected(), Some(24));
        handle_key(&mut app, &mut input, ctrl('u'));
        assert_eq!(app.list_state.selected(), Some(14));
        handle_key(&mut app, &mut input, ctrl('u'));
        assert_eq!(app.list_state.selected(), Some(4));
        handle_key(&mut app, &mut input, ctrl('u'));
        assert_eq!(app.list_state.selected(), Some(0));

        // Cleanup
        drop(app);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn keybindings_should_support_the_same_scroll_navigation() {
        let (mut app, directory) = test_app();
        app.dialog = Some(Dialog::Keybindings);
        app.keybindings_max_scroll = 30;
        app.keybindings_scroll = 5;
        let mut input = InputState::default();

        handle_key(&mut app, &mut input, key(KeyCode::Down));
        assert_eq!(app.keybindings_scroll, 6);
        handle_key(&mut app, &mut input, ctrl('d'));
        assert_eq!(app.keybindings_scroll, 16);
        handle_key(&mut app, &mut input, key(KeyCode::Char('g')));
        handle_key(&mut app, &mut input, key(KeyCode::Char('g')));
        assert_eq!(app.keybindings_scroll, 0);
        handle_key(&mut app, &mut input, key(KeyCode::Char('G')));
        assert_eq!(app.keybindings_scroll, 30);

        drop(app);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn keybindings_search_keyboard_events_should_support_lazygit_flow() {
        let (mut app, directory) = test_app();
        app.show_keybindings();
        let mut input = InputState::default();

        // 1. Press '/' to start search
        handle_key(&mut app, &mut input, key(KeyCode::Char('/')));
        assert!(app.keybindings_search.is_active);

        // 2. Type 't', 'r'
        handle_key(&mut app, &mut input, key(KeyCode::Char('t')));
        handle_key(&mut app, &mut input, key(KeyCode::Char('r')));
        assert_eq!(app.keybindings_search.value, "tr");
        assert!(app.keybindings_search.is_active);

        // 3. Esc while typing cancels search and restores full list immediately
        handle_key(&mut app, &mut input, key(KeyCode::Esc));
        assert!(!app.keybindings_search.is_active);
        assert_eq!(app.keybindings_search.value, "");
        assert_eq!(app.dialog, Some(Dialog::Keybindings));

        // 4. Press '/', type 't', 'r', press Enter to confirm
        handle_key(&mut app, &mut input, key(KeyCode::Char('/')));
        handle_key(&mut app, &mut input, key(KeyCode::Char('t')));
        handle_key(&mut app, &mut input, key(KeyCode::Char('r')));
        handle_key(&mut app, &mut input, key(KeyCode::Enter));
        assert!(!app.keybindings_search.is_active);
        assert_eq!(app.keybindings_search.value, "tr");
        assert_eq!(app.dialog, Some(Dialog::Keybindings));

        // 5. Esc while browsing filtered results clears filter
        handle_key(&mut app, &mut input, key(KeyCode::Esc));
        assert_eq!(app.keybindings_search.value, "");
        assert_eq!(app.dialog, Some(Dialog::Keybindings));

        // 6. Esc on empty filter closes popup
        handle_key(&mut app, &mut input, key(KeyCode::Esc));
        assert_eq!(app.dialog, None);

        drop(app);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn file_search_keyboard_events_should_filter_cancel_confirm_and_clear() {
        // Arrange
        let (mut app, directory) = test_app();
        app.files = ["alpha.mkv", "beta.mkv"]
            .into_iter()
            .map(|name| FileEntry {
                path: directory.join(name),
                display_name: name.to_string(),
                fingerprint: FileFingerprint {
                    length: 0,
                    modified: None,
                },
            })
            .collect();
        app.list_state.select(Some(0));
        let mut input = InputState::default();

        // Act: search for beta, then cancel while typing.
        handle_key(&mut app, &mut input, key(KeyCode::Char('/')));
        for ch in "beta".chars() {
            handle_key(&mut app, &mut input, key(KeyCode::Char(ch)));
        }
        assert_that!(app.selected_file().unwrap().display_name.as_str()).is_equal_to("beta.mkv");
        handle_key(&mut app, &mut input, key(KeyCode::Esc));

        // Assert: cancellation restores alpha.
        assert_that!(app.file_search.value.as_str()).is_empty();
        assert_that!(app.selected_file().unwrap().display_name.as_str()).is_equal_to("alpha.mkv");

        // Act: confirm beta and open/close keybindings over the retained filter.
        handle_key(&mut app, &mut input, key(KeyCode::Char('/')));
        for ch in "beta".chars() {
            handle_key(&mut app, &mut input, key(KeyCode::Char(ch)));
        }
        handle_key(&mut app, &mut input, key(KeyCode::Enter));
        handle_key(&mut app, &mut input, key(KeyCode::Char('?')));
        assert_that!(app.dialog).contains(Dialog::Keybindings);
        assert_that!(app.file_search.value.as_str()).is_equal_to("beta");
        handle_key(&mut app, &mut input, key(KeyCode::Char('?')));

        // Act and assert: first Esc clears the filter and keeps beta; second quits.
        let cleared = handle_key(&mut app, &mut input, key(KeyCode::Esc));
        assert_that!(cleared).is_equal_to(InputOutcome::Continue);
        assert_that!(app.file_search.value.as_str()).is_empty();
        assert_that!(app.selected_file().unwrap().display_name.as_str()).is_equal_to("beta.mkv");
        let quit = handle_key(&mut app, &mut input, key(KeyCode::Esc));
        assert_that!(quit).is_equal_to(InputOutcome::Quit);

        // Cleanup
        drop(app);
        fs::remove_dir_all(directory).unwrap();
    }

    /// `gg`/`G` and the back keys have to work in every scrollable dialog, not just the
    /// ones that happen to have a test. Each of these is a separate `match` arm, so a
    /// dialog added without them silently loses the bindings the keybindings popup
    /// promises.
    #[test]
    fn every_scrollable_dialog_should_answer_gg_g_and_the_back_keys() {
        // Arrange
        let (mut app, directory) = browsing_app();
        let mut input = InputState::default();

        // Act / Assert: the keybindings popup.
        app.dialog = Some(Dialog::Keybindings);
        app.set_keybindings_max_scroll(20);
        handle_key(&mut app, &mut input, key(KeyCode::Char('G')));
        assert_that!(app.keybindings_scroll).is_equal_to(20);
        handle_key(&mut app, &mut input, key(KeyCode::Char('g')));
        handle_key(&mut app, &mut input, key(KeyCode::Char('g')));
        assert_that!(app.keybindings_scroll).is_equal_to(0);

        // Act / Assert: the confirm-process-all list.
        app.dialog = Some(Dialog::ConfirmProcessAll);
        app.set_confirm_process_all_max_scroll(20);
        handle_key(&mut app, &mut input, key(KeyCode::Char('G')));
        assert_that!(app.confirm_process_all_scroll).is_equal_to(20);
        handle_key(&mut app, &mut input, ctrl('u'));
        let after_page_up = app.confirm_process_all_scroll;
        assert_that!(after_page_up < 20).is_true();
        handle_key(&mut app, &mut input, ctrl('d'));
        assert_that!(app.confirm_process_all_scroll > after_page_up).is_true();
        handle_key(&mut app, &mut input, key(KeyCode::Char('g')));
        handle_key(&mut app, &mut input, key(KeyCode::Char('g')));
        assert_that!(app.confirm_process_all_scroll).is_equal_to(0);
        handle_key(&mut app, &mut input, key(KeyCode::Esc));
        assert_that!(app.dialog).is_none();

        // Cleanup
        drop(app);
        fs::remove_dir_all(directory).unwrap();
    }

    /// The error notice has exactly one job: go away. Both the confirm key and the back
    /// keys have to do it, or a failure message strands the user in a modal.
    #[test]
    fn the_error_dialog_should_close_on_enter_and_on_either_back_key() {
        for code in [KeyCode::Enter, KeyCode::Esc, KeyCode::Char('q')] {
            // Arrange
            let (mut app, directory) = browsing_app();
            app.dialog = Some(Dialog::Error);

            // Act
            let outcome = handle_key(&mut app, &mut InputState::default(), key(code));

            // Assert: dismissed without quitting reel.
            assert_that!(app.dialog).is_none();
            assert_that!(outcome).is_equal_to(InputOutcome::Continue);

            // Cleanup
            drop(app);
            fs::remove_dir_all(directory).unwrap();
        }
    }

    /// `gg` is the only two-key chord outside the `z` family, and it shares its first
    /// key with nothing — but in the Files panel it has to survive being handled after
    /// the `z`-chord and reset keys, which see every keystroke first.
    #[test]
    fn double_g_should_jump_to_the_top_of_the_file_list() {
        // Arrange
        let (mut app, directory) = browsing_app();
        let mut input = InputState::default();
        app.select_last();
        assert_that!(app.selected_file().unwrap().display_name.as_str()).is_equal_to("b.mkv");

        // Act: a lone `g` is not enough.
        handle_key(&mut app, &mut input, key(KeyCode::Char('g')));
        assert_that!(app.selected_file().unwrap().display_name.as_str()).is_equal_to("b.mkv");
        handle_key(&mut app, &mut input, key(KeyCode::Char('g')));

        // Assert
        assert_that!(app.selected_file().unwrap().display_name.as_str()).is_equal_to("a.mkv");

        // Cleanup
        drop(app);
        fs::remove_dir_all(directory).unwrap();
    }

    /// The reset prompt discards staged work, so backing out of it has to be possible
    /// with either back key and must leave the staged edits alone.
    #[test]
    fn the_reset_prompt_should_be_dismissable_with_either_back_key() {
        for code in [KeyCode::Esc, KeyCode::Char('q')] {
            // Arrange
            let (mut app, directory) = browsing_app();
            let path = app.selected_file().unwrap().path.clone();
            app.request_reset_file(path);
            assert_that!(app.dialog).contains(Dialog::ConfirmReset);

            // Act
            let outcome = handle_key(&mut app, &mut InputState::default(), key(code));

            // Assert: the prompt is gone and nothing was reset.
            assert_that!(outcome).is_equal_to(InputOutcome::Continue);
            assert_that!(app.dialog).is_none();
            assert_that!(app.pending_reset().is_none()).is_true();

            // Cleanup
            drop(app);
            fs::remove_dir_all(directory).unwrap();
        }
    }

    /// `r` on a file row asks before discarding that file's staged edits; `R` asks
    /// about every staged file. Both share their key with a `z` chord, so both have to
    /// come out the other side of `z_command` still meaning what they said.
    #[test]
    fn the_reset_keys_should_survive_sharing_their_letters_with_the_fold_chords() {
        // Arrange
        let (mut app, directory) = browsing_app();
        let mut input = InputState::default();

        // Act / Assert: lowercase `r` scopes to the selected file.
        handle_key(&mut app, &mut input, key(KeyCode::Char('r')));
        assert_that!(app.dialog).contains(Dialog::ConfirmReset);
        let selected = app.selected_file().unwrap().path.clone();
        assert_that!(app.pending_reset()).contains(&ResetScope::File(selected));
        handle_key(&mut app, &mut input, key(KeyCode::Esc));

        // Act / Assert: uppercase `R` scopes to every staged file.
        handle_key(&mut app, &mut input, key(KeyCode::Char('R')));
        assert_that!(app.dialog).contains(Dialog::ConfirmReset);
        assert_that!(app.pending_reset()).contains(&ResetScope::AllFiles);
        handle_key(&mut app, &mut input, key(KeyCode::Esc));

        // Act / Assert: `zR` is still the unfold-all chord, not a reset.
        handle_key(&mut app, &mut input, key(KeyCode::Char('z')));
        handle_key(&mut app, &mut input, key(KeyCode::Char('R')));
        assert_that!(app.dialog).is_none();

        // Cleanup
        drop(app);
        fs::remove_dir_all(directory).unwrap();
    }
}
