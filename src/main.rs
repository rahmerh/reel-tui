use std::time::Duration;
use std::{path::PathBuf, process::ExitCode};

use anyhow::Result;
use crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyEventKind};
use crossterm::execute;
use reel_tui::app::App;
use reel_tui::cli;
use reel_tui::edit::spawn_edit_worker_pools;
use reel_tui::files::spawn_directory_monitor;
use reel_tui::input::{InputOutcome, InputState, handle_key};
use reel_tui::notification::completion_notification_sender;
use reel_tui::probe::{spawn_conflict_probe_worker, spawn_probe_worker};
use reel_tui::{config, mount, ui};

fn main() -> ExitCode {
    cli::dispatch(
        std::env::args_os().skip(1),
        std::env::current_dir,
        run,
        std::io::stdout(),
        std::io::stderr(),
    )
}

fn run(target_dir: PathBuf) -> Result<()> {
    let target_dir = std::fs::canonicalize(&target_dir).unwrap_or(target_dir);
    let directory_rx = spawn_directory_monitor(target_dir.clone());
    let (request_tx, result_rx) = spawn_probe_worker();
    let (conflict_tx, conflict_rx) = spawn_conflict_probe_worker();
    let app_config = config::Config::load();
    let is_network_mount = mount::is_network_mount(&target_dir);
    let (transcode_workers, remux_workers) = app_config.effective_workers(is_network_mount);
    let (transcode_tx, remux_tx, edit_rx) =
        spawn_edit_worker_pools(transcode_workers, remux_workers);
    let mut app = App::new(target_dir, request_tx, conflict_tx, transcode_tx, remux_tx)?;
    app.set_completion_notification_sender(completion_notification_sender(
        app_config.notifications,
    ));
    let mut input = InputState::default();

    ratatui::run(|terminal| -> Result<()> {
        let _paste = BracketedPaste::enable()?;
        // Redraw only when something render-relevant actually happened, instead of
        // unconditionally repainting the whole UI ~20 times a second forever. The
        // `Dialog::Processing` progress spinner is animated purely by elapsed wall
        // time (see `render_progress_dialog`), so it needs a redraw every tick while
        // showing regardless of whether app state changed.
        let mut dirty = true;
        let mut redraw = ui::RedrawState::default();
        loop {
            dirty |= app.receive_directory_snapshots(&directory_rx);
            dirty |= app.receive_probe_results(&result_rx);
            dirty |= app.receive_conflict_probe_results(&conflict_rx);
            dirty |= app.receive_edit_results(&edit_rx);
            app.start_pending_probe();
            dirty |= app.maybe_open_conflict_dialog();
            if redraw.tick(std::mem::take(&mut dirty), app.is_animating()) {
                terminal.draw(|frame| ui::render(frame, &mut app))?;
            }

            if event::poll(Duration::from_millis(50))? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        if handle_key(&mut app, &mut input, key) == InputOutcome::Quit {
                            break;
                        }
                        dirty = true;
                    }
                    // Only arrives while bracketed paste is on, which is why the
                    // clipboard lands as one edit rather than a burst of key events.
                    Event::Paste(text) => {
                        app.paste_text(&text);
                        dirty = true;
                    }
                    Event::Resize(_, _) => dirty = true,
                    _ => {}
                }
            }
        }
        Ok(())
    })
}

/// Turns bracketed paste on for the lifetime of the TUI. `ratatui::run` restores raw
/// mode and the alternate screen but knows nothing about this mode, so it is undone in
/// `Drop` — including while unwinding, or the shell inherits a terminal that wraps every
/// paste in escape sequences.
struct BracketedPaste;

impl BracketedPaste {
    fn enable() -> Result<Self> {
        execute!(std::io::stdout(), EnableBracketedPaste)?;
        Ok(Self)
    }
}

impl Drop for BracketedPaste {
    fn drop(&mut self) {
        let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    }
}
