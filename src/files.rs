use std::{
    fs,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    time::{Duration, Instant, SystemTime},
};

use anyhow::Result;
use notify::{RecursiveMode, Watcher};

const EVENT_DEBOUNCE: Duration = Duration::from_millis(150);
const RECONCILE_INTERVAL: Duration = Duration::from_secs(1);
const NETWORK_RECONCILE_INTERVAL: Duration = Duration::from_secs(10);

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

/// How many `metadata()` stats a network-mount scan keeps in flight at once. Each
/// stat on a network share is dominated by round-trip latency rather than the work
/// on either end, so issuing several concurrently lets the client pipeline them
/// instead of paying the round trip once per file — a directory of a few hundred
/// files can otherwise take the round-trip-count times the round-trip-time to list.
/// Local disks don't have that latency to hide, so they keep the plain sequential
/// path below.
const NETWORK_SCAN_CONCURRENCY: usize = 8;

pub fn scan_directory(directory: &Path) -> Result<Vec<FileEntry>> {
    let mut candidates = Vec::new();

    for entry in fs::read_dir(directory)? {
        let Ok(entry) = entry else {
            continue;
        };
        let display_name = entry.file_name().to_string_lossy().into_owned();
        if display_name.starts_with(".reel-tui-") {
            continue;
        }
        // `file_type()` is typically served from the same readdir buffer as the
        // name (e.g. `d_type` on Linux), not a fresh stat, so it's cheap even on a
        // network mount and safe to keep in this single-threaded first pass.
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        candidates.push((entry.path(), display_name));
    }

    let mut files = if crate::mount::is_network_mount(directory) {
        stat_candidates_concurrently(candidates)
    } else {
        candidates
            .into_iter()
            .filter_map(|(path, display_name)| file_entry_for(path, display_name))
            .collect()
    };

    // `sort_by_cached_key` computes each entry's key once (O(n) lowercase
    // allocations) instead of re-lowercasing both sides on every comparison inside a
    // `sort_by` closure (O(n log n) allocations for the same result).
    files.sort_by_cached_key(|file| (file.display_name.to_lowercase(), file.display_name.clone()));

    Ok(files)
}

fn file_entry_for(path: PathBuf, display_name: String) -> Option<FileEntry> {
    let metadata = fs::metadata(&path).ok()?;
    Some(FileEntry {
        path,
        display_name,
        fingerprint: FileFingerprint {
            length: metadata.len(),
            modified: metadata.modified().ok(),
        },
    })
}

/// Stats `candidates` across a small pool of scoped threads instead of one at a
/// time. Order doesn't matter here — `scan_directory` sorts the result afterward
/// regardless of which chunk finishes first.
fn stat_candidates_concurrently(candidates: Vec<(PathBuf, String)>) -> Vec<FileEntry> {
    if candidates.len() <= 1 {
        return candidates
            .into_iter()
            .filter_map(|(path, display_name)| file_entry_for(path, display_name))
            .collect();
    }

    let chunk_size = candidates.len().div_ceil(NETWORK_SCAN_CONCURRENCY).max(1);
    let mut files = Vec::with_capacity(candidates.len());
    std::thread::scope(|scope| {
        let handles: Vec<_> = candidates
            .chunks(chunk_size)
            .map(|chunk| {
                scope.spawn(move || {
                    chunk
                        .iter()
                        .cloned()
                        .filter_map(|(path, display_name)| file_entry_for(path, display_name))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for handle in handles {
            if let Ok(chunk_files) = handle.join() {
                files.extend(chunk_files);
            }
        }
    });
    files
}

pub fn spawn_directory_monitor(directory: PathBuf) -> Receiver<DirectorySnapshot> {
    let (snapshot_tx, snapshot_rx) = mpsc::channel();

    std::thread::spawn(move || {
        let is_network = crate::mount::is_network_mount(&directory);
        let reconcile_interval = if is_network {
            NETWORK_RECONCILE_INTERVAL
        } else {
            RECONCILE_INTERVAL
        };

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

            match event_rx.recv_timeout(reconcile_interval) {
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
                Err(RecvTimeoutError::Disconnected) => std::thread::sleep(reconcile_interval),
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

    /// `scan_directory` only takes this path when `is_network_mount` says so (not
    /// reproducible from a plain temp directory), so it's exercised directly here
    /// rather than through `scan_directory` — this is the path a real network-mount
    /// scan actually runs.
    #[test]
    fn stat_candidates_concurrently_should_return_metadata_for_every_candidate_across_chunks() {
        // Arrange: more candidates than `NETWORK_SCAN_CONCURRENCY` so the chunking
        // itself is exercised, not just a single thread handling everything.
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-network-scan-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let mut candidates = Vec::new();
        for index in 0..(NETWORK_SCAN_CONCURRENCY * 3) {
            let display_name = format!("movie-{index}.mkv");
            let path = directory.join(&display_name);
            fs::write(&path, b"not really media").unwrap();
            candidates.push((path, display_name));
        }

        // Act
        let mut files = stat_candidates_concurrently(candidates);
        files.sort_by(|a, b| a.display_name.cmp(&b.display_name));

        // Assert: every candidate came back, each with the length it was written with
        // (not zeroed out or dropped by whichever chunk happened to run it).
        assert_that!(files.len()).is_equal_to(NETWORK_SCAN_CONCURRENCY * 3);
        for file in &files {
            assert_that!(file.fingerprint.length).is_equal_to(b"not really media".len() as u64);
        }

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn stat_candidates_concurrently_should_silently_drop_a_candidate_that_no_longer_exists() {
        // Arrange: mirrors the sequential path's `let Ok(metadata) = ... else { continue }`
        // — a file that vanished between the readdir pass and the stat (common on a
        // network share where reconciliation and external changes race) must not
        // surface as an error or panic, just be left out.
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-network-scan-missing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let present = directory.join("present.mkv");
        fs::write(&present, b"media").unwrap();
        let missing = directory.join("missing.mkv");
        let candidates = vec![
            (present.clone(), "present.mkv".to_string()),
            (missing, "missing.mkv".to_string()),
        ];

        // Act
        let files = stat_candidates_concurrently(candidates);

        // Assert
        assert_that!(files.len()).is_equal_to(1);
        assert_that!(files[0].path.clone()).is_equal_to(present);

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
