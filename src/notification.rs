use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;

use notify_rust::Notification;

use crate::cache::DiskCache;

/// The notification icon, baked into the binary so installing `reel` is still a single
/// file — no separate asset to place by hand. Desktop notification backends want a
/// real file path rather than embedded bytes, so it is written out once per machine.
const ICON_BYTES: &[u8] = include_bytes!("../assets/icon.png");

/// Starts a best-effort desktop-notification worker and returns its input channel.
/// Notification delivery stays off the UI thread; an unavailable notification
/// service must never turn a successful media edit into an application error.
fn spawn_completion_notification_worker() -> Sender<PathBuf> {
    spawn_completion_notification_worker_with(deliver_completion_notification).0
}

pub fn completion_notification_sender(enabled: bool) -> Option<Sender<PathBuf>> {
    enabled.then(spawn_completion_notification_worker)
}

/// The one call that actually reaches the notification service. Split out from the
/// closure that used it so a test can invoke this exact line directly — a failed
/// `.show()` (no session bus, no notification daemon) is discarded either way, so the
/// call is safe to make for real rather than needing a fake to stand in for it.
fn deliver_completion_notification(path: &Path, icon: Option<&Path>) {
    let _ = completion_notification(path, icon).show();
}

/// Split out from `spawn_completion_notification_worker` so a test can substitute a
/// closure that records calls instead of one that actually reaches the notification
/// service — the same seam the pre-`notify-rust` version got by injecting a program
/// name in place of `notify-send`.
fn spawn_completion_notification_worker_with(
    mut deliver: impl FnMut(&Path, Option<&Path>) + Send + 'static,
) -> (Sender<PathBuf>, JoinHandle<()>) {
    let icon = ensure_icon_file();
    let (tx, rx) = channel();
    let worker = std::thread::spawn(move || {
        notification_worker(rx, |path| deliver(path, icon.as_deref()));
    });
    (tx, worker)
}

fn notification_worker(receiver: Receiver<PathBuf>, mut notify: impl FnMut(&Path)) {
    while let Ok(path) = receiver.recv() {
        notify(&path);
    }
}

/// Writes the icon to the cache directory if it is not already there, and returns its
/// path. Best-effort like everything else here: a read-only cache or an unresolvable
/// cache directory just means notifications ship without an icon.
fn ensure_icon_file() -> Option<PathBuf> {
    let dir = DiskCache::cache_dir()?;
    ensure_icon_file_in(&dir).ok()
}

/// Split out from `ensure_icon_file` so a test can act on its own directory instead of
/// the one shared cache directory every worker and every other test in the process
/// contends for — same reason `DiskCache::save_to` takes its directory as a parameter.
///
/// Written via a uniquely-named temporary file and an atomic rename, the same idiom
/// `DiskCache::save_to` uses, so a torn read is impossible even when several callers
/// race here at once.
fn ensure_icon_file_in(dir: &Path) -> std::io::Result<PathBuf> {
    let path = dir.join("icon.png");
    if !path.exists() {
        std::fs::create_dir_all(dir)?;
        let temp_path = dir.join(format!(
            ".icon.png.tmp.{}.{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&temp_path, ICON_BYTES)?;
        std::fs::rename(&temp_path, &path)?;
    }
    Ok(path)
}

fn completion_notification(path: &Path, icon: Option<&Path>) -> Notification {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("Media file");
    let mut notification = Notification::new();
    notification
        .appname("reel")
        .summary("Reel")
        .body(&format!("{name} finished processing."));
    if let Some(icon) = icon.and_then(Path::to_str) {
        // `.icon()` (the spec's `app_icon` parameter) alone: it is the oldest and most
        // widely supported way to set this. Also setting `.image_path()` (the
        // `image-path` hint) made some daemons — swaync among them — render both as
        // separate, visibly nested icons instead of picking one.
        notification.icon(icon);
    }
    notification
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use notify_rust::Hint;

    use super::*;

    #[test]
    fn completion_notification_should_name_the_finished_file() {
        let notification = completion_notification(Path::new("/media/films/Movie.mkv"), None);

        assert_eq!(notification.appname, "reel");
        assert_eq!(notification.summary, "Reel");
        assert_eq!(notification.body, "Movie.mkv finished processing.");
    }

    #[test]
    fn completion_notification_should_fall_back_for_a_path_without_a_file_name() {
        let notification = completion_notification(Path::new("/"), None);

        assert_eq!(notification.body, "Media file finished processing.");
    }

    #[test]
    fn completion_notification_should_set_the_icon_when_a_path_is_given() {
        // Only `.icon()` (the `app_icon` parameter) is set — also setting the
        // `image-path` hint made swaync render both as separate, nested icons.
        let notification =
            completion_notification(Path::new("Movie.mkv"), Some(Path::new("/tmp/icon.png")));

        assert_eq!(notification.icon, "/tmp/icon.png");
        assert!(
            !notification
                .hints
                .iter()
                .any(|hint| matches!(hint, Hint::ImagePath(_))),
            "the image-path hint must stay unset, or daemons that honor both render two icons"
        );
    }

    #[test]
    fn completion_notification_should_set_no_icon_when_none_is_given() {
        let notification = completion_notification(Path::new("Movie.mkv"), None);

        assert_eq!(notification.icon, "");
        assert!(
            !notification
                .hints
                .iter()
                .any(|hint| matches!(hint, Hint::ImagePath(_)))
        );
    }

    #[test]
    fn deliver_completion_notification_should_not_panic_without_a_notification_service() {
        // Act / Assert: a discarded `.show()` error (no session bus in this test
        // environment) must never surface as a panic.
        deliver_completion_notification(Path::new("Movie.mkv"), None);
    }

    /// A directory this test alone owns, so it cannot observe another test's write to
    /// the shared cache directory and mistake that for its own "creates it" branch
    /// never running.
    fn scratch(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "reel-tui-notification-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn ensure_icon_file_in_should_create_the_icon_once_and_reuse_it_after() {
        let dir = scratch("write-once");

        let first = ensure_icon_file_in(&dir).unwrap();
        assert_eq!(std::fs::read(&first).unwrap(), ICON_BYTES);

        // Act: a second call must not fail or change the file's content just because it
        // already exists.
        let second = ensure_icon_file_in(&dir).unwrap();
        assert_eq!(first, second);
        assert_eq!(std::fs::read(&second).unwrap(), ICON_BYTES);
    }

    #[test]
    fn ensure_icon_file_should_resolve_the_cache_directory() {
        let path = ensure_icon_file().expect("cache dir should resolve under cfg(test)");
        assert_eq!(std::fs::read(&path).unwrap(), ICON_BYTES);
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
    fn completion_worker_should_deliver_the_path_and_stop_when_its_channel_closes() {
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&delivered);
        let (sender, worker) = spawn_completion_notification_worker_with(move |path, icon| {
            observed
                .lock()
                .unwrap()
                .push((path.to_path_buf(), icon.map(Path::to_path_buf)));
        });
        sender.send("Movie.mkv".into()).unwrap();
        drop(sender);

        worker.join().unwrap();

        assert_eq!(delivered.lock().unwrap().len(), 1);
        assert_eq!(delivered.lock().unwrap()[0].0, PathBuf::from("Movie.mkv"));
    }

    #[test]
    fn notification_setting_should_only_start_a_worker_when_enabled() {
        assert!(completion_notification_sender(false).is_none());
        drop(completion_notification_sender(true).unwrap());
    }
}
