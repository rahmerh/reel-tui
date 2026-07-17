mod app;
mod edit;
mod files;
mod probe;
mod ui;

use std::time::{Duration, Instant};

use anyhow::Result;
use app::{App, Dialog};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use edit::spawn_edit_worker;
use probe::spawn_probe_worker;

fn main() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let (request_tx, result_rx) = spawn_probe_worker();
    let (edit_tx, edit_rx) = spawn_edit_worker();
    let mut app = App::new(cwd, request_tx, edit_tx)?;
    let mut pending_g: Option<Instant> = None;

    ratatui::run(|terminal| -> Result<()> {
        loop {
            app.receive_probe_results(&result_rx);
            app.receive_edit_results(&edit_rx);
            app.start_pending_probe();
            terminal.draw(|frame| ui::render(frame, &mut app))?;

            if event::poll(Duration::from_millis(50))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                if app.dialog == Some(Dialog::Processing) {
                    if matches!(key.code, KeyCode::Esc | KeyCode::Char('q'))
                        || matches!(
                            (key.code, key.modifiers),
                            (KeyCode::Char('c'), KeyModifiers::CONTROL)
                        )
                    {
                        app.cancel_edit();
                    }
                    continue;
                }
                if app.dialog == Some(Dialog::Keybindings) {
                    match (key.code, key.modifiers) {
                        (KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q'), _) => {
                            app.dismiss_dialog()
                        }
                        (KeyCode::Char('j') | KeyCode::Down, _) => app.scroll_keybindings_down(1),
                        (KeyCode::Char('k') | KeyCode::Up, _) => app.scroll_keybindings_up(1),
                        (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                            app.scroll_keybindings_down(10)
                        }
                        (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                            app.scroll_keybindings_up(10)
                        }
                        _ => {}
                    }
                    continue;
                }
                if key.code != KeyCode::Char('g') {
                    pending_g = None;
                }
                match (key.code, key.modifiers) {
                    (KeyCode::Char('?'), _) if app.dialog.is_none() => app.show_keybindings(),
                    (KeyCode::Char('y'), _) if app.dialog == Some(Dialog::ConfirmSave) => {
                        app.confirm_save()
                    }
                    (KeyCode::Char('n'), _) if app.dialog == Some(Dialog::ConfirmSave) => {
                        app.dismiss_dialog()
                    }
                    (KeyCode::Enter, _) if app.dialog == Some(Dialog::ConfirmSave) => {
                        app.confirm_save()
                    }
                    (KeyCode::Enter, _) if app.dialog == Some(Dialog::Error) => {
                        app.dismiss_dialog()
                    }
                    (KeyCode::Esc, _) if app.dialog.is_some() => app.dismiss_dialog(),
                    (KeyCode::Char('q'), _) if app.dialog.is_none() => break,
                    (KeyCode::Esc, _) if !app.back() => break,
                    (KeyCode::Char('d'), KeyModifiers::NONE) => app.toggle_delete_selected_stream(),
                    (KeyCode::Char('a'), KeyModifiers::NONE) => app.set_selected_stream_default(),
                    (KeyCode::Char('s'), KeyModifiers::CONTROL) => app.request_save(),
                    (KeyCode::Char('k'), KeyModifiers::CONTROL) => app.move_selected_stream(-1),
                    (KeyCode::Char('j'), KeyModifiers::CONTROL) => app.move_selected_stream(1),
                    (KeyCode::Enter, _) => app.enter(),
                    (KeyCode::Char('j'), _) | (KeyCode::Down, _) => app.select_next(),
                    (KeyCode::Char('k'), _) | (KeyCode::Up, _) => app.select_previous(),
                    (KeyCode::Char('g'), _) => {
                        if pending_g
                            .is_some_and(|pressed| pressed.elapsed() <= Duration::from_millis(750))
                        {
                            app.select_first();
                            pending_g = None;
                        } else {
                            pending_g = Some(Instant::now());
                        }
                    }
                    (KeyCode::Char('G'), _) => app.select_last(),
                    (KeyCode::Char('d'), KeyModifiers::CONTROL) => app.scroll_down(),
                    (KeyCode::Char('u'), KeyModifiers::CONTROL) => app.scroll_up(),
                    (KeyCode::Char('r'), _) if app.dialog.is_none() => app.refresh()?,
                    _ => {}
                }
            }
        }
        Ok(())
    })
}
