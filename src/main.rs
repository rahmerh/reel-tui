mod app;
mod edit;
mod files;
mod probe;
mod subtitle;
mod ui;

use std::time::{Duration, Instant};

use anyhow::Result;
use app::{App, Dialog, Layer};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use edit::spawn_edit_worker;
use files::spawn_directory_monitor;
use probe::spawn_probe_worker;

fn main() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let directory_rx = spawn_directory_monitor(cwd.clone());
    let (request_tx, result_rx) = spawn_probe_worker();
    let (edit_tx, edit_rx) = spawn_edit_worker();
    let mut app = App::new(cwd, request_tx, edit_tx)?;
    let mut input = InputState::default();

    ratatui::run(|terminal| -> Result<()> {
        loop {
            app.receive_directory_snapshots(&directory_rx);
            app.receive_probe_results(&result_rx);
            app.receive_edit_results(&edit_rx);
            app.start_pending_probe();
            terminal.draw(|frame| ui::render(frame, &mut app))?;

            if event::poll(Duration::from_millis(50))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
                && handle_key(&mut app, &mut input, key) == InputOutcome::Quit
            {
                break;
            }
        }
        Ok(())
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InputOutcome {
    Continue,
    Quit,
}

#[derive(Default)]
struct InputState {
    pending_g: Option<Instant>,
}

impl InputState {
    fn reset_sequence(&mut self) {
        self.pending_g = None;
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
}

fn handle_key(app: &mut App, input: &mut InputState, key: KeyEvent) -> InputOutcome {
    match app.dialog {
        Some(Dialog::Processing) => {
            input.reset_sequence();
            if is_back_key(key) {
                app.request_cancel_edit();
            } else if matches!(
                (key.code, key.modifiers),
                (KeyCode::Char('c'), KeyModifiers::CONTROL)
            ) {
                app.cancel_edit();
            }
        }
        Some(Dialog::ConfirmCancel) => {
            input.reset_sequence();
            match (key.code, key.modifiers) {
                (KeyCode::Char('h') | KeyCode::Left, KeyModifiers::NONE) => {
                    app.choose_cancel_edit(-1)
                }
                (KeyCode::Char('l') | KeyCode::Right, KeyModifiers::NONE) => {
                    app.choose_cancel_edit(1)
                }
                (KeyCode::Enter, _) => app.activate_cancel_edit(),
                _ if is_back_key(key) => app.dismiss_cancel_edit(),
                _ => {}
            }
        }
        Some(Dialog::ContainerSettings) => {
            input.reset_sequence();
            match (key.code, key.modifiers) {
                (KeyCode::Char('j') | KeyCode::Down, KeyModifiers::NONE) => {
                    app.move_container_settings_cursor(1)
                }
                (KeyCode::Char('k') | KeyCode::Up, KeyModifiers::NONE) => {
                    app.move_container_settings_cursor(-1)
                }
                (KeyCode::Enter, _) => app.activate_container_settings(),
                _ if is_back_key(key) => app.close_container_settings(),
                _ => {}
            }
        }
        Some(Dialog::VideoSettings) => {
            input.reset_sequence();
            match (key.code, key.modifiers) {
                (KeyCode::Char('s'), KeyModifiers::CONTROL) => app.save_from_video_settings(),
                (KeyCode::Char('j') | KeyCode::Down, KeyModifiers::NONE) => {
                    app.move_video_settings_cursor(1)
                }
                (KeyCode::Char('k') | KeyCode::Up, KeyModifiers::NONE) => {
                    app.move_video_settings_cursor(-1)
                }
                (KeyCode::Char(digit), KeyModifiers::NONE) if digit.is_ascii_digit() => {
                    app.input_custom_resolution_digit(digit)
                }
                (KeyCode::Backspace, KeyModifiers::NONE) => app.backspace_custom_resolution(),
                (KeyCode::Enter, _) => app.activate_video_settings(),
                _ if is_back_key(key) => app.escape_video_settings(),
                _ => {}
            }
        }
        Some(Dialog::SubtitleSettings) => {
            input.reset_sequence();
            match (key.code, key.modifiers) {
                (KeyCode::Char('j') | KeyCode::Down, KeyModifiers::NONE) => {
                    app.move_subtitle_settings_cursor(1)
                }
                (KeyCode::Char('k') | KeyCode::Up, KeyModifiers::NONE) => {
                    app.move_subtitle_settings_cursor(-1)
                }
                (KeyCode::Enter | KeyCode::Char(' '), _) => app.activate_subtitle_settings(),
                (KeyCode::Char('s'), KeyModifiers::CONTROL) => {
                    app.close_subtitle_settings();
                    app.request_save();
                }
                _ if is_back_key(key) => app.escape_subtitle_settings(),
                _ => {}
            }
        }
        Some(Dialog::Keybindings) => match (key.code, key.modifiers) {
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
                app.dismiss_dialog();
            }
            _ if input.is_double_g(key) => app.scroll_keybindings_to_start(),
            _ => {}
        },
        Some(Dialog::ConfirmSave) => {
            input.reset_sequence();
            match (key.code, key.modifiers) {
                (KeyCode::Char('j') | KeyCode::Down, KeyModifiers::NONE) => {
                    app.move_save_dialog_cursor(1)
                }
                (KeyCode::Char('k') | KeyCode::Up, KeyModifiers::NONE) => {
                    app.move_save_dialog_cursor(-1)
                }
                (KeyCode::Char('h') | KeyCode::Left, KeyModifiers::NONE) => {
                    app.choose_save_destination(-1)
                }
                (KeyCode::Char('l') | KeyCode::Right, KeyModifiers::NONE) => {
                    app.choose_save_destination(1)
                }
                (KeyCode::Enter, _) => app.activate_save_dialog(),
                _ if is_back_key(key) => app.dismiss_dialog(),
                _ => {}
            }
        }
        Some(Dialog::Error) => {
            input.reset_sequence();
            if key.code == KeyCode::Enter || is_back_key(key) {
                app.dismiss_dialog();
            }
        }
        None => return handle_layer_key(app, input, key),
    }
    InputOutcome::Continue
}

fn handle_layer_key(app: &mut App, input: &mut InputState, key: KeyEvent) -> InputOutcome {
    if is_back_key(key) {
        input.reset_sequence();
        return if app.back() {
            InputOutcome::Continue
        } else {
            InputOutcome::Quit
        };
    }
    match (key.code, key.modifiers) {
        (KeyCode::Char('?'), _) => {
            input.reset_sequence();
            app.show_keybindings();
        }
        (KeyCode::Char('d'), KeyModifiers::CONTROL) if app.layer == Layer::StreamDetails => {
            input.reset_sequence();
            app.scroll_down();
        }
        (KeyCode::Char('u'), KeyModifiers::CONTROL) if app.layer == Layer::StreamDetails => {
            input.reset_sequence();
            app.scroll_up();
        }
        (KeyCode::Char('d'), KeyModifiers::NONE) if app.layer == Layer::Streams => {
            input.reset_sequence();
            app.toggle_delete_selected_stream();
        }
        (KeyCode::Char('a'), KeyModifiers::NONE) if app.layer == Layer::Streams => {
            input.reset_sequence();
            app.set_selected_stream_default();
        }
        (KeyCode::Char('s'), KeyModifiers::CONTROL) if app.layer == Layer::Streams => {
            input.reset_sequence();
            app.request_save();
        }
        (KeyCode::Char('k'), KeyModifiers::CONTROL) if app.layer == Layer::Streams => {
            input.reset_sequence();
            app.move_selected_stream(-1);
        }
        (KeyCode::Char('j'), KeyModifiers::CONTROL) if app.layer == Layer::Streams => {
            input.reset_sequence();
            app.move_selected_stream(1);
        }
        (KeyCode::Char('i'), KeyModifiers::NONE) if app.layer == Layer::Streams => {
            input.reset_sequence();
            app.open_stream_details();
        }
        (KeyCode::Char('h'), KeyModifiers::NONE) if app.layer == Layer::Streams => {
            input.reset_sequence();
            app.back();
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
    use std::{
        fs,
        path::PathBuf,
        sync::mpsc,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;
    use crate::{
        app::{
            CustomResolutionDraft, CustomResolutionField, VideoSettingsField, VideoSettingsMode,
            VideoSettingsPopup,
        },
        edit::{CustomResolution, CustomScaling, EditRequest, VideoResolution},
        probe::{MediaInfo, ProbeOutcome, ProbeRequest},
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
        let (edit_tx, _) = mpsc::channel::<EditRequest>();
        let app = App::new(directory.clone(), probe_tx, edit_tx).unwrap();
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
            (Layer::Streams, Some(Dialog::ConfirmSave)),
            (Layer::Streams, Some(Dialog::Processing)),
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
            app.dialog = Some(Dialog::Processing);
            let mut input = InputState::default();

            handle_key(&mut app, &mut input, key(code));
            assert_eq!(app.dialog, Some(Dialog::ConfirmCancel));

            handle_key(&mut app, &mut input, key(code));
            assert_eq!(app.dialog, Some(Dialog::Processing));

            drop(app);
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn error_dialog_should_consume_navigation_without_changing_underlying_selection() {
        let (mut app, directory) = test_app();
        app.layer = Layer::Streams;
        app.stream_order = vec![0, 1, 2];
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
                width: String::new(),
                height: String::new(),
                scaling: CustomScaling::FitPad,
                field: CustomResolutionField::Width,
                scaling_cursor: 0,
                scaling_dropdown_open: false,
            }),
        });
        let mut input = InputState::default();

        // Act
        handle_key(&mut app, &mut input, key(KeyCode::Char('1')));
        handle_key(&mut app, &mut input, key(KeyCode::Char('2')));
        handle_key(&mut app, &mut input, key(KeyCode::Backspace));
        handle_key(&mut app, &mut input, key(KeyCode::Down));
        handle_key(&mut app, &mut input, key(KeyCode::Char('8')));
        handle_key(&mut app, &mut input, key(KeyCode::Down));
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
        assert_eq!(draft.width, "1");
        assert_eq!(draft.height, "8");
        assert_eq!(draft.field, CustomResolutionField::Scaling);
        assert_eq!(draft.scaling, CustomScaling::Stretch);
        assert!(!draft.scaling_dropdown_open);

        // Cleanup
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
                    width: "1280".to_string(),
                    height: "720".to_string(),
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
}
