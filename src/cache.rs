use std::{
    collections::HashMap,
    fs::{self, File},
    io::{BufReader, BufWriter},
    path::{Path, PathBuf},
    time::SystemTime,
};

use serde::{Deserialize, Serialize};

use crate::probe::ProbeOutcome;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiskCacheEntry {
    pub length: u64,
    pub modified: Option<SystemTime>,
    pub outcome: ProbeOutcome,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DiskCache {
    pub entries: HashMap<PathBuf, DiskCacheEntry>,
}

impl DiskCache {
    /// Where `probe_cache.json` and `edit_errors.log` live.
    ///
    /// The unit-test binary is redirected to throwaway storage wholesale. `App::new`
    /// calls `load()`, `receive_probe_results` calls `save()`, and `log_edit_failure`
    /// appends to `edit_errors.log` — so without this, every `cargo test` run rewrote
    /// the user's real probe cache with `/tmp` fixture paths and appended each test's
    /// deliberately-failing edit to the real failure log. That log is the first place
    /// AGENTS.md says to look when hunting a regression, and it had reached 1132 lines
    /// of which 925 were test noise.
    ///
    /// The e2e binary links this crate compiled *without* `cfg(test)`, so it is not
    /// covered here; it redirects `XDG_CACHE_HOME` itself (`tests/e2e/harness.rs`).
    /// Between the two, no test run can reach the user's real cache directory.
    pub fn cache_dir() -> Option<PathBuf> {
        #[cfg(test)]
        {
            Some(Self::test_cache_dir())
        }
        #[cfg(not(test))]
        {
            Self::cache_dir_from(
                std::env::var("XDG_CACHE_HOME").ok().as_deref(),
                std::env::var("HOME").ok().as_deref(),
            )
        }
    }

    /// A single throwaway directory shared by the whole unit-test binary, cleared once
    /// per run so no run inherits the previous one's cached probes. A `OnceLock` rather
    /// than an environment variable: `set_var` is `unsafe` in this edition and racy
    /// under the threaded test runner, and this needs no such trade-off.
    #[cfg(test)]
    fn test_cache_dir() -> PathBuf {
        static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        DIR.get_or_init(|| {
            let dir = std::env::temp_dir().join("reel-tui-unit-test-cache");
            let _ = fs::remove_dir_all(&dir);
            let _ = fs::create_dir_all(&dir);
            dir
        })
        .clone()
    }

    /// Resolves the cache directory from the two environment variables that can name it.
    /// Split out from `cache_dir` so the precedence and the empty-value handling can be
    /// tested directly — mutating the process environment from a test is racy under the
    /// threaded test runner and `unsafe` in this edition.
    ///
    /// An empty value is treated as unset rather than as the relative path it literally
    /// is: `XDG_CACHE_HOME=` would otherwise resolve to `reel-tui/` in whatever directory
    /// `reel` happens to be launched from, scattering cache files across the user's disk.
    fn cache_dir_from(xdg_cache_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
        if let Some(path) = xdg_cache_home.filter(|path| !path.is_empty()) {
            return Some(PathBuf::from(path).join("reel-tui"));
        }
        if let Some(home) = home.filter(|home| !home.is_empty()) {
            return Some(PathBuf::from(home).join(".cache").join("reel-tui"));
        }
        None
    }

    pub fn cache_file_path() -> Option<PathBuf> {
        Self::cache_dir().map(|dir| dir.join("probe_cache.json"))
    }

    pub fn load() -> Self {
        let Some(path) = Self::cache_file_path() else {
            return Self::default();
        };
        Self::load_from(&path)
    }

    /// Reads a cache file, discarding anything unreadable or malformed — a stale cache is
    /// never worth failing a launch over. Entries that no longer describe a real video
    /// are dropped so a file that once probed as video is re-probed instead of trusted.
    pub fn load_from(path: &Path) -> Self {
        let Ok(file) = File::open(path) else {
            return Self::default();
        };

        let reader = BufReader::new(file);
        let mut cache: DiskCache = serde_json::from_reader(reader).unwrap_or_default();
        cache.entries.retain(|_, entry| match &entry.outcome {
            ProbeOutcome::Video(info) => info.streams.iter().any(|stream| {
                stream.get("codec_type").and_then(serde_json::Value::as_str) == Some("video")
                    && !crate::probe::is_attached_picture(stream)
                    && !crate::probe::is_still_image(stream, &info.format)
            }),
            _ => true,
        });
        cache
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(dir) = Self::cache_dir() else {
            return Ok(());
        };
        self.save_to(&dir)
    }

    /// Writes the cache into `dir`, via a temporary file so a crash mid-write leaves the
    /// previous cache intact rather than a truncated one.
    pub fn save_to(&self, dir: &Path) -> std::io::Result<()> {
        fs::create_dir_all(dir)?;
        let file_path = dir.join("probe_cache.json");
        let temp_path = dir.join(".probe_cache.json.tmp");

        let file = File::create(&temp_path)?;
        let writer = BufWriter::new(file);
        serde_json::to_writer(writer, self)?;
        fs::rename(temp_path, file_path)?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn get(
        &self,
        path: &Path,
        length: u64,
        modified: Option<SystemTime>,
    ) -> Option<ProbeOutcome> {
        let entry = self.entries.get(path)?;
        if entry.length == length && entry.modified == modified {
            match &entry.outcome {
                ProbeOutcome::Video(info) => {
                    let valid_video = info.streams.iter().any(|stream| {
                        stream.get("codec_type").and_then(serde_json::Value::as_str)
                            == Some("video")
                            && !crate::probe::is_attached_picture(stream)
                            && !crate::probe::is_still_image(stream, &info.format)
                    });
                    if valid_video {
                        Some(entry.outcome.clone())
                    } else {
                        None
                    }
                }
                other => Some(other.clone()),
            }
        } else {
            None
        }
    }

    pub fn insert(
        &mut self,
        path: PathBuf,
        length: u64,
        modified: Option<SystemTime>,
        outcome: ProbeOutcome,
    ) {
        self.entries.insert(
            path,
            DiskCacheEntry {
                length,
                modified,
                outcome,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, time::Duration};

    use serde_json::json;

    use super::*;
    use crate::probe::MediaInfo;

    fn scratch(tag: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-cache-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    fn video(codec_type: &str) -> ProbeOutcome {
        ProbeOutcome::Video(MediaInfo {
            format: BTreeMap::from([("format_name".to_string(), json!("matroska"))]),
            streams: vec![BTreeMap::from([
                ("codec_type".to_string(), json!(codec_type)),
                ("codec_name".to_string(), json!("h264")),
            ])],
            chapters: vec![],
        })
    }

    #[test]
    fn disk_cache_should_survive_a_round_trip_through_a_file() {
        // Arrange
        let directory = scratch("round-trip");
        let path = PathBuf::from("/media/video.mkv");
        let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let mut cache = DiskCache::default();
        cache.insert(path.clone(), 1024, Some(modified), video("video"));

        // Act
        cache.save_to(&directory).unwrap();
        let loaded = DiskCache::load_from(&directory.join("probe_cache.json"));

        // Assert: the fingerprint survives the trip, so a reload still serves the entry
        // and still rejects a file whose length has moved on.
        assert!(loaded.get(&path, 1024, Some(modified)).is_some());
        assert!(loaded.get(&path, 2048, Some(modified)).is_none());

        // And the temporary file is not left behind next to the real one.
        assert!(!directory.join(".probe_cache.json.tmp").exists());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn disk_cache_should_start_empty_when_the_file_is_missing_or_unreadable() {
        // Arrange
        let directory = scratch("unreadable");
        let path = directory.join("probe_cache.json");

        // Act / Assert: a missing cache and a corrupt one are both recoverable states —
        // `reel` must launch either way rather than propagate the error.
        assert!(DiskCache::load_from(&path).entries.is_empty());

        fs::write(&path, b"{ this is not json").unwrap();
        assert!(DiskCache::load_from(&path).entries.is_empty());

        fs::write(&path, b"{\"entries\":{}}").unwrap();
        assert!(DiskCache::load_from(&path).entries.is_empty());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn disk_cache_should_drop_a_stored_entry_that_no_longer_describes_a_video() {
        // Arrange: an entry that was cached as a video but holds no video stream — the
        // shape a still image or an audio file leaves behind after a probe-rule change.
        let directory = scratch("not-a-video");
        let path = PathBuf::from("/media/song.mkv");
        let mut cache = DiskCache::default();
        cache.insert(path.clone(), 10, None, video("audio"));

        // Act
        cache.save_to(&directory).unwrap();
        let loaded = DiskCache::load_from(&directory.join("probe_cache.json"));

        // Assert: dropped on load, so the file is re-probed instead of misreported.
        assert!(loaded.entries.is_empty());
        // `get` refuses it too, in case it is still in a cache held in memory.
        assert!(cache.get(&path, 10, None).is_none());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn disk_cache_should_keep_a_non_video_outcome_verbatim() {
        // Arrange: `NotVideo` and `Error` are answers in their own right, not stale
        // video entries, and re-probing them every launch is what the cache exists to
        // avoid on network mounts.
        let directory = scratch("not-video-outcome");
        let mut cache = DiskCache::default();
        cache.insert(
            PathBuf::from("/media/notes.txt"),
            4,
            None,
            ProbeOutcome::NotVideo("no video stream".to_string()),
        );
        cache.insert(
            PathBuf::from("/media/broken.mkv"),
            4,
            None,
            ProbeOutcome::Error("ffprobe failed".to_string()),
        );

        // Act
        cache.save_to(&directory).unwrap();
        let loaded = DiskCache::load_from(&directory.join("probe_cache.json"));

        // Assert
        assert_eq!(loaded.entries.len(), 2);
        assert!(matches!(
            loaded.get(&PathBuf::from("/media/notes.txt"), 4, None),
            Some(ProbeOutcome::NotVideo(_)),
        ));

        fs::remove_dir_all(directory).unwrap();
    }

    /// Regression test for a real leak: `cargo test` was rewriting the user's real
    /// `probe_cache.json` with `/tmp` fixture paths and appending every deliberately
    /// failing test edit to their real `edit_errors.log` — the log AGENTS.md points at
    /// as the first place to look when hunting a regression, found holding 925 lines of
    /// test noise against 207 real ones. Nothing in the unit-test binary may resolve to
    /// a directory the user actually keeps data in.
    #[test]
    fn the_unit_test_binary_should_never_resolve_to_a_real_cache_directory() {
        // Act
        let resolved = DiskCache::cache_dir().unwrap();

        // Assert: throwaway storage, and specifically not either directory the real
        // resolver would have picked.
        assert!(
            resolved.starts_with(std::env::temp_dir()),
            "the test cache must live in throwaway storage, got {resolved:?}",
        );
        let real = DiskCache::cache_dir_from(
            std::env::var("XDG_CACHE_HOME").ok().as_deref(),
            std::env::var("HOME").ok().as_deref(),
        );
        if let Some(real) = real {
            assert_ne!(
                resolved, real,
                "the test cache must not be the directory the real resolver picks",
            );
        }
        // And the file the cache actually writes lands inside it.
        assert!(DiskCache::cache_file_path().unwrap().starts_with(&resolved));
    }

    #[test]
    fn cache_dir_should_prefer_xdg_cache_home_and_fall_back_to_home() {
        // Arrange / Act / Assert: XDG wins when both are set, `$HOME/.cache` is the
        // fallback, and with neither set there is nowhere to write — which must be `None`
        // rather than a relative path.
        assert_eq!(
            DiskCache::cache_dir_from(Some("/xdg"), Some("/home/bas")),
            Some(PathBuf::from("/xdg/reel-tui")),
        );
        assert_eq!(
            DiskCache::cache_dir_from(None, Some("/home/bas")),
            Some(PathBuf::from("/home/bas/.cache/reel-tui")),
        );
        assert_eq!(DiskCache::cache_dir_from(None, None), None);
    }

    #[test]
    fn cache_dir_should_treat_an_empty_variable_as_unset() {
        // Arrange / Act / Assert: `XDG_CACHE_HOME=` is a common way to accidentally unset
        // a variable in a shell profile or systemd unit. Taking it literally would resolve
        // the cache to a relative `reel-tui/` inside whatever directory `reel` was
        // launched from, writing cache files into the user's media folders.
        assert_eq!(
            DiskCache::cache_dir_from(Some(""), Some("/home/bas")),
            Some(PathBuf::from("/home/bas/.cache/reel-tui")),
        );
        assert_eq!(DiskCache::cache_dir_from(Some(""), Some("")), None);
        assert_eq!(DiskCache::cache_dir_from(None, Some("")), None);
    }

    #[test]
    fn cache_file_path_should_sit_inside_the_cache_directory() {
        // Arrange / Act / Assert: the pairing the e2e harness relies on when it redirects
        // `XDG_CACHE_HOME` to keep tests off the user's real cache.
        assert_eq!(
            DiskCache::cache_dir_from(Some("/xdg"), None)
                .map(|dir| dir.join("probe_cache.json"))
                .as_deref(),
            Some(Path::new("/xdg/reel-tui/probe_cache.json")),
        );
    }

    #[test]
    fn disk_cache_should_drop_a_stored_entry_whose_video_stream_is_only_cover_art() {
        // Arrange: an MP3 with embedded artwork probes as having a video stream. The cache
        // must apply the same attached-picture rule the live probe does — otherwise the
        // first launch correctly hides the file and every launch after it, served from
        // cache, offers an audio file for track editing.
        let directory = scratch("cover-art");
        let path = PathBuf::from("/media/song.mp3");
        let mut cache = DiskCache::default();
        cache.insert(
            path.clone(),
            10,
            None,
            ProbeOutcome::Video(MediaInfo {
                format: BTreeMap::from([("format_name".to_string(), json!("mp3"))]),
                streams: vec![BTreeMap::from([
                    ("codec_type".to_string(), json!("video")),
                    ("codec_name".to_string(), json!("mjpeg")),
                    ("disposition".to_string(), json!({"attached_pic": 1})),
                ])],
                chapters: vec![],
            }),
        );

        // Act
        cache.save_to(&directory).unwrap();
        let loaded = DiskCache::load_from(&directory.join("probe_cache.json"));

        // Assert: rejected both on load and by an in-memory `get`.
        assert!(loaded.entries.is_empty());
        assert!(cache.get(&path, 10, None).is_none());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn disk_cache_should_drop_a_stored_entry_that_is_really_a_still_image() {
        // Arrange: a PNG cached back when it looked like a one-frame video. Same failure
        // as cover art — a cached answer must not outlive the rule that produced it.
        let directory = scratch("still-image");
        let path = PathBuf::from("/media/poster.png");
        let mut cache = DiskCache::default();
        cache.insert(
            path.clone(),
            10,
            None,
            ProbeOutcome::Video(MediaInfo {
                format: BTreeMap::from([("format_name".to_string(), json!("image2"))]),
                streams: vec![BTreeMap::from([
                    ("codec_type".to_string(), json!("video")),
                    ("codec_name".to_string(), json!("png")),
                ])],
                chapters: vec![],
            }),
        );

        // Act
        cache.save_to(&directory).unwrap();
        let loaded = DiskCache::load_from(&directory.join("probe_cache.json"));

        // Assert
        assert!(loaded.entries.is_empty());
        assert!(cache.get(&path, 10, None).is_none());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn disk_cache_should_keep_a_real_video_that_also_carries_cover_art() {
        // Arrange: a film with an embedded poster has both an attached picture and a real
        // video stream. The rule is "has at least one genuine video stream", not "has no
        // cover art" — getting that backwards would hide ordinary films from the list.
        let directory = scratch("video-with-cover");
        let path = PathBuf::from("/media/film.mkv");
        let mut cache = DiskCache::default();
        cache.insert(
            path.clone(),
            10,
            None,
            ProbeOutcome::Video(MediaInfo {
                format: BTreeMap::from([("format_name".to_string(), json!("matroska"))]),
                streams: vec![
                    BTreeMap::from([
                        ("codec_type".to_string(), json!("video")),
                        ("codec_name".to_string(), json!("mjpeg")),
                        ("disposition".to_string(), json!({"attached_pic": 1})),
                    ]),
                    BTreeMap::from([
                        ("codec_type".to_string(), json!("video")),
                        ("codec_name".to_string(), json!("h264")),
                    ]),
                ],
                chapters: vec![],
            }),
        );

        // Act
        cache.save_to(&directory).unwrap();
        let loaded = DiskCache::load_from(&directory.join("probe_cache.json"));

        // Assert: kept, and still served.
        assert_eq!(loaded.entries.len(), 1);
        assert!(loaded.get(&path, 10, None).is_some());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn disk_cache_should_reject_an_entry_whose_modification_time_is_now_unknown() {
        // Arrange: a fingerprint recorded with a timestamp, queried with none — what
        // happens when a filesystem stops reporting mtime (some network mounts do). The
        // safe answer is a miss and a re-probe, not a stale hit.
        let mut cache = DiskCache::default();
        let path = PathBuf::from("/media/video.mkv");
        let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        cache.insert(path.clone(), 1024, Some(modified), video("video"));

        // Act / Assert
        assert!(cache.get(&path, 1024, None).is_none());
        assert!(cache.get(&path, 1024, Some(modified)).is_some());
        // A path that was never cached at all is a miss, not a panic.
        assert!(
            cache
                .get(Path::new("/media/other.mkv"), 1024, None)
                .is_none()
        );
    }

    #[test]
    fn disk_cache_should_store_retrieve_and_invalidate_on_length_mismatch() {
        let mut cache = DiskCache::default();
        let path = PathBuf::from("/media/video.mkv");
        let now = SystemTime::now();

        let info = MediaInfo {
            format: BTreeMap::from([
                ("format_name".to_string(), json!("matroska")),
                ("duration".to_string(), json!("120.0")),
            ]),
            streams: vec![BTreeMap::from([
                ("codec_type".to_string(), json!("video")),
                ("codec_name".to_string(), json!("h264")),
            ])],
            chapters: vec![],
        };
        let outcome = ProbeOutcome::Video(info);

        cache.insert(path.clone(), 1024, Some(now), outcome);

        // Retrieve valid entry
        let hit = cache.get(&path, 1024, Some(now));
        assert!(hit.is_some());

        // Invalidate on size mismatch
        let miss_size = cache.get(&path, 2048, Some(now));
        assert!(miss_size.is_none());

        // Invalidate on time mismatch
        let miss_time = cache.get(&path, 1024, Some(now + Duration::from_secs(10)));
        assert!(miss_time.is_none());
    }
}
