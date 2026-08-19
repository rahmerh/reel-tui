//! The on-disk cache of rendered preview frames.
//!
//! A frame is expensive: an accurate `ffmpeg` seek into the container plus a libass burn,
//! which is a fraction of a second locally and seconds over a network mount. Without this
//! the timing page paid that cost again for every cue the cursor came back to, and again
//! for every cue when the page was re-opened.
//!
//! Content-addressed: the filename *is* the hash of everything the frame depends on, so
//! there is no index to keep consistent, no lock to take, and nothing a crash mid-write or
//! two pages warming at once can corrupt. A cue whose text or timing changed hashes
//! differently and simply misses, which is what makes "the line changed, re-render it"
//! fall out for free rather than needing invalidation logic.
//!
//! Pure filesystem and hashing. No `ffmpeg`, no `App`, no `ratatui`.
//!
//! Every operation comes in two forms, the way `cache.rs` splits `save`/`save_to`: one
//! that resolves the cache directory and one that is handed it. The second is what the
//! tests drive, so each gets a directory of its own instead of racing the single one the
//! test binary shares — and the first is then a line long, with only the "this machine
//! has no cache directory at all" arm the test binary cannot reach.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::cache::DiskCache;
use crate::cue::Cue;

/// Directory the frames live in, under the same cache directory as `probe_cache.json`.
const FRAMES_DIR: &str = "preview_frames";

/// What a stored frame is, which is also what `preview::frame_command` asks `ffmpeg` for.
///
/// JPEG rather than PNG, and the difference is not a detail. PNG is lossless, and a frame
/// of live-action video has nothing lossless to preserve — it came out of a lossy codec to
/// begin with — so the encoder spends two megabytes storing grain. The same frame at twice
/// the resolution is under a hundred kilobytes as JPEG, which is the whole reason a
/// feature-length track's frames now fit in the cache at once rather than evicting each
/// other as they are written.
pub const FRAME_EXTENSION: &str = "jpg";

/// Everything a cached frame depends on, hashed into its filename.
///
/// The media's length and mtime are in here as well as its path: re-encoding a file in
/// place leaves the path identical while every frame in it moves, and serving the old
/// file's pictures for the new one is exactly the kind of silent wrongness a preview must
/// not have. `pixels` is in here because the stored PNG is rendered at that size, so
/// changing the cap must miss rather than hand back mis-sized images.
#[derive(Clone, Copy, Debug)]
pub struct FrameKeyParts<'a> {
    pub media: &'a Path,
    pub media_length: u64,
    pub media_modified: Option<SystemTime>,
    pub pixels: (u32, u32),
}

/// The cache key for one cue's frame, as hex.
///
/// FNV-1a, 128-bit, written out here rather than taken from a crate or from
/// `DefaultHasher`. `DefaultHasher`'s output is explicitly not stable across Rust
/// releases, and this key is written to disk to be read back by a later build — a
/// toolchain bump would silently orphan the whole cache. Sixteen bytes puts a collision
/// among the few thousand frames a film's subtitle track produces far below the odds of
/// the disk lying about the bytes.
pub fn key(parts: FrameKeyParts<'_>, cue: &Cue) -> String {
    // The two variable-length fields — which file, which line — go in first and next to
    // each other, which is what the separator between them is for: without it a path
    // ending where the next cue's text begins would hash the same as the pair the other
    // way round. Everything after them is fixed width and so delimits itself.
    let mut hash = Hasher::new();
    hash.bytes(parts.media.as_os_str().as_encoded_bytes());
    hash.separator();
    hash.bytes(cue.text.as_bytes());
    hash.number(u128::from(parts.media_length));
    hash.number(u128::from(
        parts
            .media_modified
            .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |since| since.as_nanos() as u64),
    ));
    hash.number(u128::from(parts.pixels.0));
    hash.number(u128::from(parts.pixels.1));
    // Millis rather than the whole `Duration`: it is what the SubRip format itself
    // stores, so two cues that are equal on disk cannot hash apart on a rounding.
    hash.number(u128::from(cue.start.as_millis() as u64));
    hash.number(u128::from(cue.end.as_millis() as u64));
    hash.finish()
}

/// Where the frame for `key` lives, or `None` when there is no cache directory at all.
///
/// Resolved through [`DiskCache::cache_dir`] rather than from the environment directly:
/// that one function is what redirects the unit-test binary to throwaway storage, and the
/// e2e harness redirects `XDG_CACHE_HOME` around it. Going straight to the environment
/// here would put a second door on the user's real cache directory that no test closes.
pub fn path(key: &str) -> Option<PathBuf> {
    Some(path_in(&directory()?, key))
}

pub fn directory() -> Option<PathBuf> {
    Some(DiskCache::cache_dir()?.join(FRAMES_DIR))
}

fn path_in(directory: &Path, key: &str) -> PathBuf {
    directory.join(format!("{key}.{FRAME_EXTENSION}"))
}

/// The cached frame for `key`, if one was ever stored and still is.
pub fn read(key: &str) -> Option<Vec<u8>> {
    read_in(&directory()?, key)
}

pub fn read_in(directory: &Path, key: &str) -> Option<Vec<u8>> {
    let bytes = fs::read(path_in(directory, key)).ok()?;
    // A zero-byte file is what a half-finished write outside `store` would leave, and
    // handing it to the decoder produces "Unreadable frame" for something that is really
    // just a miss. Treated as absent so the next grab replaces it.
    (!bytes.is_empty()).then_some(bytes)
}

pub fn is_cached(key: &str) -> bool {
    directory().is_some_and(|directory| is_cached_in(&directory, key))
}

pub fn is_cached_in(directory: &Path, key: &str) -> bool {
    fs::metadata(path_in(directory, key)).is_ok_and(|frame| frame.len() > 0)
}

/// Stores one frame, reporting only whether it landed.
///
/// Written to a temporary name and renamed, so a reader never sees a partial PNG — the
/// same shape `DiskCache::save_to` uses, and the reason two workers writing the same key
/// at once is harmless rather than a torn file.
///
/// Best-effort throughout: a cache that cannot be written is a slow page, not a broken
/// one, so every failure here is swallowed by the caller.
pub fn store(key: &str, bytes: &[u8]) -> bool {
    directory().is_some_and(|directory| store_in(&directory, key, bytes))
}

pub fn store_in(directory: &Path, key: &str, bytes: &[u8]) -> bool {
    if fs::create_dir_all(directory).is_err() {
        return false;
    }
    let path = path_in(directory, key);
    // The temporary is unique per *write*, not per key: the process id keeps two `reel`s
    // apart, and the counter keeps this process's own writers apart. Sharing a name would
    // let one writer rename another's half-written file into place — the very tearing the
    // temporary exists to prevent — and the two frame workers do render the same cue at
    // the same moment, since the interactive one answers the selection the background
    // pass is walking towards.
    static WRITES: AtomicU64 = AtomicU64::new(0);
    let temporary = directory.join(format!(
        ".{key}.{}.{}.tmp",
        std::process::id(),
        WRITES.fetch_add(1, Ordering::Relaxed)
    ));
    if fs::write(&temporary, bytes).is_err() {
        let _ = fs::remove_file(&temporary);
        return false;
    }
    if fs::rename(&temporary, &path).is_err() {
        let _ = fs::remove_file(&temporary);
        return false;
    }
    true
}

/// Deletes the least recently modified frames until the directory fits inside `limit`.
///
/// Called once per page opening, from the background worker rather than the event loop:
/// it stats every file in the directory, which on a full cache is thousands of them.
///
/// Oldest-first by mtime, which for a content-addressed cache is also "least recently
/// generated" — a frame that is still being looked at is re-read, not rewritten, so this
/// is an approximation of least-recently-used that costs no bookkeeping. The worst case
/// of getting it wrong is regenerating a frame.
pub fn prune(limit: u64) {
    if let Some(directory) = directory() {
        prune_in(&directory, limit);
    }
}

pub fn prune_in(directory: &Path, limit: u64) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut frames: Vec<(SystemTime, u64, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            metadata.is_file().then(|| {
                (
                    metadata.modified().unwrap_or(UNIX_EPOCH),
                    metadata.len(),
                    entry.path(),
                )
            })
        })
        .collect();

    let mut total: u64 = frames.iter().map(|(_, length, _)| *length).sum();
    frames.sort_by_key(|(modified, _, _)| *modified);
    for (_, length, path) in frames {
        if total <= limit {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(length);
        }
    }
}

/// FNV-1a over 128 bits, fed one field at a time.
///
/// The one variable-length pair it is fed is separated explicitly rather than
/// concatenated, so a path ending where the next field begins cannot hash the same as the
/// pair the other way round.
struct Hasher(u128);

impl Hasher {
    const OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;

    fn new() -> Self {
        Self(Self::OFFSET)
    }

    fn bytes(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u128::from(*byte);
            self.0 = self.0.wrapping_mul(Self::PRIME);
        }
    }

    fn number(&mut self, value: u128) {
        self.bytes(&value.to_le_bytes());
    }

    /// Marks where one variable-length field ends and the next begins.
    fn separator(&mut self) {
        self.bytes(&[0xff, 0x00]);
    }

    fn finish(self) -> String {
        format!("{:032x}", self.0)
    }
}

/// Keeps the unit-test binary's tests out of each other's way in the one cache directory
/// they share.
///
/// `DiskCache::cache_dir()` resolves to a single throwaway directory for the whole test
/// binary and the runner is threaded, so a test that clears or prunes the *directory* can
/// delete a frame another test — in this module or in `preview` — is in the middle of
/// asserting on. Directory-wide tests take the write guard; tests that only touch their
/// own key take the read guard.
#[cfg(test)]
pub(crate) mod testing {
    use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

    static CACHE: RwLock<()> = RwLock::new(());

    pub(crate) fn one_key() -> RwLockReadGuard<'static, ()> {
        CACHE.read().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn whole_directory() -> RwLockWriteGuard<'static, ()> {
        CACHE.write().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use kernal::prelude::*;

    use super::testing::{one_key, whole_directory};
    use super::*;

    fn cue(start: u64, end: u64, text: &str) -> Cue {
        Cue {
            index: 0,
            start: Duration::from_millis(start),
            end: Duration::from_millis(end),
            text: text.to_string(),
        }
    }

    fn parts(media: &Path) -> FrameKeyParts<'_> {
        FrameKeyParts {
            media,
            media_length: 1_000,
            media_modified: Some(UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
            pixels: (960, 540),
        }
    }

    /// A cache directory of this test's own, so nothing here races the single directory
    /// the whole test binary otherwise shares.
    fn scratch(tag: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-framecache-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn the_same_media_and_cue_should_always_hash_the_same() {
        // Arrange
        let media = PathBuf::from("/media/show.mkv");

        // Act / Assert
        assert_that!(key(parts(&media), &cue(1000, 2000, "hello")).as_str())
            .is_equal_to(key(parts(&media), &cue(1000, 2000, "hello")).as_str());
        // Sixteen bytes of hex, so a filename that is always the same length.
        assert_that!(key(parts(&media), &cue(1000, 2000, "hello")).len()).is_equal_to(32);
    }

    /// The whole point of the key: change what the frame would look like, and the cache
    /// misses instead of handing back the old picture.
    #[test]
    fn anything_that_changes_the_frame_should_change_the_key() {
        // Arrange
        let media = PathBuf::from("/media/show.mkv");
        let other = PathBuf::from("/media/other.mkv");
        let base = key(parts(&media), &cue(1000, 2000, "hello"));

        // Act / Assert: the cue's text, which is what the user edits.
        assert_that!(key(parts(&media), &cue(1000, 2000, "hello!")).as_str())
            .is_not_equal_to(base.as_str());
        // Its timing, which moves the frame the grab lands on.
        assert_that!(key(parts(&media), &cue(1500, 2000, "hello")).as_str())
            .is_not_equal_to(base.as_str());
        assert_that!(key(parts(&media), &cue(1000, 2500, "hello")).as_str())
            .is_not_equal_to(base.as_str());
        // The media it is burned onto.
        assert_that!(key(parts(&other), &cue(1000, 2000, "hello")).as_str())
            .is_not_equal_to(base.as_str());
        // The media's content, at the same path: a re-encode in place.
        let mut relength = parts(&media);
        relength.media_length = 1_001;
        assert_that!(key(relength, &cue(1000, 2000, "hello")).as_str())
            .is_not_equal_to(base.as_str());
        let mut retimed = parts(&media);
        retimed.media_modified = Some(UNIX_EPOCH + Duration::from_secs(1_700_000_001));
        assert_that!(key(retimed, &cue(1000, 2000, "hello")).as_str())
            .is_not_equal_to(base.as_str());
        // A file whose mtime cannot be read at all is its own case, not "epoch".
        let mut undated = parts(&media);
        undated.media_modified = None;
        assert_that!(key(undated, &cue(1000, 2000, "hello")).as_str())
            .is_not_equal_to(base.as_str());
        // And the size the frame was rendered at.
        let mut smaller = parts(&media);
        smaller.pixels = (640, 360);
        assert_that!(key(smaller, &cue(1000, 2000, "hello")).as_str())
            .is_not_equal_to(base.as_str());
        let mut squashed = parts(&media);
        squashed.pixels = (540, 960);
        assert_that!(key(squashed, &cue(1000, 2000, "hello")).as_str())
            .is_not_equal_to(base.as_str());
    }

    /// The two variable-length fields are separated rather than run together, so a path
    /// that ends where the next cue's text begins cannot collide with the pair the other
    /// way round.
    #[test]
    fn fields_should_not_run_into_one_another() {
        // Arrange
        let first = PathBuf::from("/media/ab");
        let second = PathBuf::from("/media/a");

        // Act / Assert
        assert_that!(key(parts(&first), &cue(1000, 2000, "c")).as_str())
            .is_not_equal_to(key(parts(&second), &cue(1000, 2000, "bc")).as_str());
    }

    #[test]
    fn a_stored_frame_should_read_back_byte_for_byte() {
        // Arrange
        let directory = scratch("round-trip");

        // Act
        let stored = store_in(&directory, "frame", b"\xff\xd8\xff frame bytes");

        // Assert
        assert_that!(stored).is_true();
        assert_that!(is_cached_in(&directory, "frame")).is_true();
        assert_that!(read_in(&directory, "frame"))
            .is_equal_to(Some(b"\xff\xd8\xff frame bytes".to_vec()));
        // No temporary left behind by a write that succeeded.
        let leftovers = fs::read_dir(&directory)
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_that!(leftovers).is_equal_to(0);

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_key_that_was_never_stored_should_read_as_absent() {
        // Arrange
        let directory = scratch("absent");

        // Act / Assert
        assert_that!(read_in(&directory, "never-stored")).is_none();
        assert_that!(is_cached_in(&directory, "never-stored")).is_false();

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    /// A zero-byte file is what an interrupted write outside `store` leaves. Handing it
    /// to the decoder reports "Unreadable frame" for what is really a miss, so it reads
    /// as absent and the next grab replaces it.
    #[test]
    fn an_empty_frame_file_should_read_as_absent() {
        // Arrange
        let directory = scratch("empty");
        fs::write(path_in(&directory, "empty"), b"").unwrap();

        // Act / Assert
        assert_that!(read_in(&directory, "empty")).is_none();
        assert_that!(is_cached_in(&directory, "empty")).is_false();

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    /// The frame directory is created on first use — a fresh machine has none, and the
    /// first frame of the first page still has to land.
    #[test]
    fn storing_should_create_the_frame_directory() {
        // Arrange
        let parent = scratch("creates-directory");
        let directory = parent.join("preview_frames");

        // Act
        let stored = store_in(&directory, "frame", b"frame");

        // Assert
        assert_that!(stored).is_true();
        assert_that!(is_cached_in(&directory, "frame")).is_true();

        // Cleanup
        fs::remove_dir_all(parent).unwrap();
    }

    /// Every way a cache directory can refuse a frame. None of them may panic: the page
    /// is only slower for it, and a `reel` that fell over because `~/.cache` was odd
    /// would be failing at something it does not even need.
    #[test]
    fn a_frame_that_cannot_be_written_should_report_failure_rather_than_panic() {
        // Arrange
        let parent = scratch("unwritable");

        // Act / Assert: a file sitting where the directory belongs.
        let blocked = parent.join("in-the-way");
        fs::write(&blocked, b"not a directory").unwrap();
        assert_that!(store_in(&blocked, "frame", b"frame")).is_false();

        // Act / Assert: a directory sitting where the frame belongs.
        let directory = parent.join("frames");
        fs::create_dir_all(path_in(&directory, "occupied")).unwrap();
        assert_that!(store_in(&directory, "occupied", b"frame")).is_false();
        assert_that!(read_in(&directory, "occupied")).is_none();

        // Act / Assert: and a temporary that cannot be written at all, which is the write
        // failing rather than the rename. A key naming a directory that does not exist
        // does it without depending on file permissions, which a test running as root
        // would not get to observe.
        assert_that!(store_in(&directory, "missing/taken", b"frame")).is_false();
        assert_that!(is_cached_in(&directory, "missing/taken")).is_false();

        // Assert: nothing was left lying beside them.
        let leftovers = fs::read_dir(&directory)
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .filter(|entry| entry.path().is_file())
            .count();
        assert_that!(leftovers).is_equal_to(0);

        // Cleanup
        fs::remove_dir_all(parent).unwrap();
    }

    /// Frames are hundreds of kilobytes each and a film's subtitle track has thousands of
    /// cues, so the directory has to have a ceiling — and the frames that go are the ones
    /// least recently written.
    #[test]
    fn pruning_should_delete_the_oldest_frames_until_the_cache_fits() {
        // Arrange: three frames of 100 bytes, aged oldest-first.
        let directory = scratch("prune");
        for index in 0..3u64 {
            assert_that!(store_in(&directory, &format!("frame-{index}"), &[0u8; 100])).is_true();
            let age = SystemTime::now() - Duration::from_secs(3600 * (3 - index));
            fs::File::open(path_in(&directory, &format!("frame-{index}")))
                .unwrap()
                .set_modified(age)
                .expect("the test filesystem should allow setting an mtime");
        }

        // Act: room for two of the three.
        prune_in(&directory, 250);

        // Assert: the oldest went, the two newer stayed.
        assert_that!(is_cached_in(&directory, "frame-0")).is_false();
        assert_that!(is_cached_in(&directory, "frame-1")).is_true();
        assert_that!(is_cached_in(&directory, "frame-2")).is_true();

        // Act / Assert: and a cache already inside its limit is left entirely alone.
        prune_in(&directory, 250);
        assert_that!(is_cached_in(&directory, "frame-1")).is_true();
        assert_that!(is_cached_in(&directory, "frame-2")).is_true();

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    /// Pruning walks whatever it finds, and a cache directory holds nothing but frames —
    /// a subdirectory is not one, and its size is not the cache's.
    #[test]
    fn pruning_should_ignore_anything_that_is_not_a_frame() {
        // Arrange
        let directory = scratch("prune-directories");
        fs::create_dir_all(directory.join("not-a-frame")).unwrap();
        assert_that!(store_in(&directory, "frame", &[0u8; 100])).is_true();

        // Act
        prune_in(&directory, 50);

        // Assert: the frame went, the directory stayed.
        assert_that!(is_cached_in(&directory, "frame")).is_false();
        assert_that!(directory.join("not-a-frame").is_dir()).is_true();

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    /// Pruning runs on every page opening, including the first one on a machine that has
    /// never cached a frame.
    #[test]
    fn pruning_a_cache_that_does_not_exist_should_do_nothing() {
        // Arrange
        let parent = scratch("prune-missing");
        let directory = parent.join("never-created");

        // Act / Assert: no panic, and nothing created.
        prune_in(&directory, 0);
        assert_that!(directory.exists()).is_false();

        // Cleanup
        fs::remove_dir_all(parent).unwrap();
    }

    /// The directory-resolving half, which is what production actually calls. It is a
    /// line each on top of the halves above, but it is also the line that decides *which*
    /// directory — and getting that wrong would write frames into the user's real cache
    /// from a test run.
    #[test]
    fn the_resolved_cache_directory_should_be_the_one_beside_the_probe_cache() {
        // Arrange
        let _guard = whole_directory();
        let directory = directory().expect("the test binary always has a cache directory");
        let _ = fs::remove_dir_all(&directory);

        // Act / Assert
        assert_that!(directory.parent().map(Path::to_path_buf))
            .is_equal_to(crate::cache::DiskCache::cache_dir());
        assert_that!(path("frame")).is_equal_to(Some(directory.join("frame.jpg")));
        assert_that!(store("frame", &[0u8; 100])).is_true();
        assert_that!(is_cached("frame")).is_true();
        assert_that!(read("frame")).is_equal_to(Some(vec![0u8; 100]));
        prune(0);
        assert_that!(is_cached("frame")).is_false();

        // Cleanup
        let _ = fs::remove_dir_all(&directory);
    }

    /// Two frames stored under one key at the same instant — two `reel`s previewing the
    /// same film — must leave one whole frame, not a torn one.
    #[test]
    fn a_frame_written_twice_at_once_should_still_be_whole() {
        // Arrange
        let _guard = one_key();
        let directory = scratch("concurrent");

        // Act
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    assert!(store_in(&directory, "frame", &[7u8; 4096]));
                });
            }
        });

        // Assert
        assert_that!(read_in(&directory, "frame")).is_equal_to(Some(vec![7u8; 4096]));

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }
}
