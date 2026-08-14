use std::path::Path;
use std::process::Command;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;

/// Starts a best-effort desktop-notification worker and returns its input channel.
/// Notification delivery stays off the UI thread; an unavailable notification
/// service must never turn a successful media edit into an application error.
fn spawn_completion_notification_worker() -> Sender<std::path::PathBuf> {
    spawn_completion_notification_worker_with("notify-send").0
}

pub fn completion_notification_sender(enabled: bool) -> Option<Sender<std::path::PathBuf>> {
    enabled.then(spawn_completion_notification_worker)
}

fn spawn_completion_notification_worker_with(
    program: impl Into<std::path::PathBuf>,
) -> (Sender<std::path::PathBuf>, JoinHandle<()>) {
    let (tx, rx) = channel();
    let program = program.into();
    let worker = std::thread::spawn(move || {
        notification_worker(rx, |path| {
            let _ = completion_command(&program, path).status();
        });
    });
    (tx, worker)
}

fn notification_worker(receiver: Receiver<std::path::PathBuf>, mut notify: impl FnMut(&Path)) {
    while let Ok(path) = receiver.recv() {
        notify(&path);
    }
}

fn completion_command(program: &Path, path: &Path) -> Command {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("Media file");
    let mut command = Command::new(program);
    command
        .arg("--app-name=reel")
        .arg("--icon=video-x-generic")
        .arg("Reel")
        .arg(format!("{name} finished processing."));
    command
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn completion_command_should_name_the_finished_file() {
        let command = completion_command(
            Path::new("notify-send"),
            Path::new("/media/films/Movie.mkv"),
        );
        let args = command.get_args().collect::<Vec<_>>();

        assert_eq!(command.get_program(), OsStr::new("notify-send"));
        assert_eq!(
            args,
            [
                OsStr::new("--app-name=reel"),
                OsStr::new("--icon=video-x-generic"),
                OsStr::new("Reel"),
                OsStr::new("Movie.mkv finished processing."),
            ]
        );
    }

    #[test]
    fn completion_command_should_fall_back_for_a_path_without_a_file_name() {
        let command = completion_command(Path::new("notify-send"), Path::new("/"));

        assert_eq!(
            command.get_args().last(),
            Some(OsStr::new("Media file finished processing."))
        );
    }

    #[test]
    fn notification_worker_should_deliver_every_path_until_the_channel_closes() {
        let (tx, rx) = channel();
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&delivered);
        let worker = std::thread::spawn(move || {
            notification_worker(rx, |path| {
                observed.lock().unwrap().push(path.to_path_buf());
            });
        });

        tx.send("first.mkv".into()).unwrap();
        tx.send("second.mp4".into()).unwrap();
        drop(tx);
        worker.join().unwrap();

        assert_eq!(
            *delivered.lock().unwrap(),
            vec![
                std::path::PathBuf::from("first.mkv"),
                std::path::PathBuf::from("second.mp4")
            ]
        );
    }

    #[test]
    fn completion_worker_should_run_commands_and_stop_when_its_channel_closes() {
        let (sender, worker) = spawn_completion_notification_worker_with("true");
        sender.send("Movie.mkv".into()).unwrap();
        drop(sender);

        worker.join().unwrap();
    }

    #[test]
    fn notification_setting_should_only_start_a_worker_when_enabled() {
        assert!(completion_notification_sender(false).is_none());
        drop(completion_notification_sender(true).unwrap());
    }
}
