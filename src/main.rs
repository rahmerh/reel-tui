mod app;
mod files;
mod probe;
mod ui;

use std::time::Duration;

use anyhow::Result;
use app::App;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use probe::spawn_probe_worker;

fn main() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let (request_tx, result_rx) = spawn_probe_worker();
    let mut app = App::new(cwd, request_tx)?;

    ratatui::run(|terminal| -> Result<()> {
        loop {
            app.receive_probe_results(&result_rx);
            app.start_pending_probe();
            terminal.draw(|frame| ui::render(frame, &mut app))?;

            if event::poll(Duration::from_millis(50))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                match (key.code, key.modifiers) {
                    (KeyCode::Char('q'), _) => break,
                    (KeyCode::Esc, _) if !app.back() => break,
                    (KeyCode::Enter, _) => app.enter(),
                    (KeyCode::Char('j'), _) | (KeyCode::Down, _) => app.select_next(),
                    (KeyCode::Char('k'), _) | (KeyCode::Up, _) => app.select_previous(),
                    (KeyCode::Char('g'), _) => app.select_first(),
                    (KeyCode::Char('G'), _) => app.select_last(),
                    (KeyCode::Char('d'), KeyModifiers::CONTROL) => app.scroll_down(),
                    (KeyCode::Char('u'), KeyModifiers::CONTROL) => app.scroll_up(),
                    (KeyCode::Char('r'), _) => app.refresh()?,
                    _ => {}
                }
            }
        }
        Ok(())
    })
}
