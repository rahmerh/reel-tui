use std::{
    cmp::Ordering,
    fs,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    time::{Duration, Instant, SystemTime},
};

use anyhow::Result;
use notify::{RecursiveMode, Watcher};

const EVENT_DEBOUNCE: Duration = Duration::from_millis(150);
const RECONCILE_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FileFingerprint {
    pub length: u64,
    pub modified: Option<SystemTime>,
}

impl FileFingerprint {
    pub fn for_path(path: &Path) -> std::io::Result<Self> {
        let metadata = fs::metadata(path)?;
        Ok(Self {
            length: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileEntry {
    pub path: PathBuf,
    pub display_name: String,
    pub fingerprint: FileFingerprint,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DirectorySnapshot {
    Files(Vec<FileEntry>),
    Error(String),
}

pub fn scan_directory(directory: &Path) -> Result<Vec<FileEntry>> {
    let mut files = Vec::new();

    for entry in fs::read_dir(directory)? {
        let Ok(entry) = entry else {
            continue;
        };
        let display_name = entry.file_name().to_string_lossy().into_owned();
        if display_name.starts_with(".reel-tui-") {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        files.push(FileEntry {
            path: entry.path(),
            display_name,
            fingerprint: FileFingerprint {
                length: metadata.len(),
                modified: metadata.modified().ok(),
            },
        });
    }

    files.sort_by(|left, right| {
        let lower = left
            .display_name
            .to_lowercase()
            .cmp(&right.display_name.to_lowercase());
        if lower == Ordering::Equal {
            left.display_name.cmp(&right.display_name)
        } else {
            lower
        }
    });

    Ok(files)
}

pub fn spawn_directory_monitor(directory: PathBuf) -> Receiver<DirectorySnapshot> {
    let (snapshot_tx, snapshot_rx) = mpsc::channel();

    std::thread::spawn(move || {
        let (event_tx, event_rx) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(event_tx).ok();
        if let Some(watcher) = watcher.as_mut() {
            let _ = watcher.watch(&directory, RecursiveMode::NonRecursive);
        }

        let mut previous = None;
        loop {
            let snapshot = match scan_directory(&directory) {
                Ok(files) => DirectorySnapshot::Files(files),
                Err(error) => DirectorySnapshot::Error(error.to_string()),
            };
            if previous.as_ref() != Some(&snapshot) {
                previous = Some(snapshot.clone());
                if snapshot_tx.send(snapshot).is_err() {
                    break;
                }
            }

            match event_rx.recv_timeout(RECONCILE_INTERVAL) {
                Ok(_) => {
                    let mut quiet_since = Instant::now();
                    loop {
                        let remaining = EVENT_DEBOUNCE.saturating_sub(quiet_since.elapsed());
                        if remaining.is_zero() || event_rx.recv_timeout(remaining).is_err() {
                            break;
                        }
                        quiet_since = Instant::now();
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => std::thread::sleep(RECONCILE_INTERVAL),
            }
        }
    });

    snapshot_rx
}

#[cfg(test)]
mod tests {
    use std::{fs::File, path::PathBuf, time::Duration};

    use kernal::prelude::*;

    use super::*;

    #[test]
    fn scan_directory_should_return_regular_files_in_case_insensitive_order_when_directory_contains_files_and_folder()
     {
        // Arrange
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-files-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(directory.join("folder")).unwrap();
        File::create(directory.join("zeta.mp4")).unwrap();
        File::create(directory.join("Alpha.txt")).unwrap();
        File::create(directory.join(".hidden")).unwrap();
        File::create(directory.join(".reel-tui-123-video.mkv")).unwrap();

        // Act
        let result = scan_directory(&directory).unwrap();
        let names: Vec<_> = result
            .iter()
            .map(|entry| entry.display_name.as_str())
            .collect();

        // Assert
        assert_that!(names).contains_exactly_in_given_order([".hidden", "Alpha.txt", "zeta.mp4"]);

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn scan_directory_should_return_empty_list_when_directory_is_empty() {
        // Arrange
        let directory: PathBuf =
            std::env::temp_dir().join(format!("reel-tui-empty-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();

        // Act
        let result = scan_directory(&directory).unwrap();

        // Assert
        assert_that!(result).is_empty();

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn directory_monitor_should_report_create_rename_and_delete_without_manual_refresh() {
        // Arrange
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-monitor-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let receiver = spawn_directory_monitor(directory.clone());
        let _ = receiver.recv_timeout(Duration::from_secs(2)).unwrap();

        // Act / Assert: create
        File::create(directory.join("before.mkv")).unwrap();
        assert_that!(receive_files_until(&receiver, |files| {
            files.iter().any(|file| file.display_name == "before.mkv")
        }))
        .is_true();

        // Act / Assert: rename
        fs::rename(directory.join("before.mkv"), directory.join("after.mkv")).unwrap();
        assert_that!(receive_files_until(&receiver, |files| {
            files.len() == 1 && files[0].display_name == "after.mkv"
        }))
        .is_true();

        // Act / Assert: delete
        fs::remove_file(directory.join("after.mkv")).unwrap();
        assert_that!(receive_files_until(&receiver, Vec::is_empty)).is_true();

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    fn receive_files_until(
        receiver: &Receiver<DirectorySnapshot>,
        predicate: impl Fn(&Vec<FileEntry>) -> bool,
    ) -> bool {
        let deadline = Instant::now() + Duration::from_secs(3);
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            match receiver.recv_timeout(remaining) {
                Ok(DirectorySnapshot::Files(files)) if predicate(&files) => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
        false
    }
}
