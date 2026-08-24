//! The on-disk cache of rendered preview frames.
//!
//! A frame is expensive: an accurate `ffmpeg` seek into the container plus a libass burn,
//! which is a fraction of a second locally and seconds over a network mount. Without this
//! the timing page paid that cost again for every cue the cursor came back to, and again
//! for every cue when the page was re-opened.
//!
//! Content-addressed: the path *is* the hash of everything the frame depends on, so there
//! is no index to keep consistent, no lock to take, and nothing a crash mid-write or two
//! pages warming at once can corrupt. A cue whose text or timing changed hashes differently
//! and simply misses, which is what makes "the line changed, re-render it" fall out for
//! free rather than needing invalidation logic.
//!
//! # Why frames are grouped by media
//!
//! The path has two halves — `{media_key}/{cue_key}.jpg` — and the split is the whole
//! shape of the eviction policy. What the user opens is a *track*, and half a track is
//! nearly worthless: the missing half is re-rendered on every visit. A cache that evicts
//! individual frames against a byte budget can always bisect one, and every cache bug this
//! module has had came from that — a whole track re-rendering on every open, a pass
//! evicting the frames it had just been opened to show, a guard against that guarding the
//! wrong files.
//!
//! So the unit of eviction is the unit of use: a whole media directory, or nothing. The
//! directory *is* the index, which is what keeps the grouping without giving up the
//! properties above — no side file to keep consistent, no lock, and a crash leaves at worst
//! one stray temporary.
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

/// Bumped whenever a stored frame stops meaning what it used to.
///
/// Hashed into [`media_key`], so a change orphans whole directories that then age out as
/// units. Without it a change of format or encoder leaves files whose names still collide
/// with live keys — the PNG-to-JPEG switch did exactly that, because a key does not encode
/// the format, and the leftovers had to be swept by hand. Bump this instead.
///
/// Version 4 is [`crate::preview::seek_for`] moving from the cue's midpoint to its start:
/// the key covers the cue's timing, which did not change, so every frame already on disk
/// would keep being served for a moment the page no longer grabs.
const CACHE_FORMAT_VERSION: u32 = 4;

/// Records when a media directory was last *used*, by its own mtime.
///
/// Written on every page opening, whether or not a frame is rendered. Ranking directories
/// by their own mtime would rank by last *write*, so a track that is fully cached — the
/// ideal case, where the pass renders nothing — would never refresh its position and would
/// age out under tracks rendered once and never opened again.
///
/// Leading dot and no [`FRAME_EXTENSION`], so it can never be mistaken for a frame.
const USED_MARKER: &str = ".used";

/// Everything about the media a cached frame depends on, hashed into its directory name.
///
/// The media's length and mtime are in here as well as its path: re-encoding a file in
/// place leaves the path identical while every frame in it moves, and serving the old
/// file's pictures for the new one is exactly the kind of silent wrongness a preview must
/// not have. `pixels` is in here because the stored frame is rendered at that size, so
/// changing the cap must miss rather than hand back mis-sized images.
///
/// All of it is per-media rather than per-cue, which is what lets it name a directory.
#[derive(Clone, Copy, Debug)]
pub struct FrameKeyParts<'a> {
    pub media: &'a Path,
    pub media_length: u64,
    pub media_modified: Option<SystemTime>,
    pub pixels: (u32, u32),
}

/// The directory every frame of one media file is stored under, as hex.
///
/// FNV-1a, 128-bit, written out here rather than taken from a crate or from
/// `DefaultHasher`. `DefaultHasher`'s output is explicitly not stable across Rust
/// releases, and this key is written to disk to be read back by a later build — a
/// toolchain bump would silently orphan the whole cache. Sixteen bytes puts a collision
/// among the few thousand frames a film's subtitle track produces far below the odds of
/// the disk lying about the bytes.
pub fn media_key(parts: FrameKeyParts<'_>) -> String {
    // The path is variable-length and goes in first, followed by the separator: without it
    // a path ending where the next field begins would hash the same as a different pair.
    // Everything after is fixed width and so delimits itself.
    let mut hash = Hasher::new();
    hash.bytes(parts.media.as_os_str().as_encoded_bytes());
    hash.separator();
    hash.number(u128::from(parts.media_length));
    hash.number(u128::from(
        parts
            .media_modified
            .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |since| since.as_nanos() as u64),
    ));
    hash.number(u128::from(parts.pixels.0));
    hash.number(u128::from(parts.pixels.1));
    hash.number(u128::from(CACHE_FORMAT_VERSION));
    hash.finish()
}

/// The filename one cue's frame is stored under inside its media's directory, as hex.
///
/// **Hashes the staged subtitle file rather than the cue's text**, and that is what makes
/// the key provably complete: the picture is a pure function of the media, the moment
/// seeked to, and the file handed to libass. [`media_key`] covers the first, the cue's
/// timing here covers the second, and `staged` is the third exactly as written to disk.
///
/// Reading the cue's text instead was right while SubRip was the only format, where text
/// *is* the staged file. It stops being right the moment a format carries how it draws:
/// two ASS cues can read the same words in different styles, at different positions, and
/// under different `PlayRes` — the same key for two different pictures.
///
/// Still only the cue and its staging, so two subtitle tracks of one film that carry an
/// identical line at an identical time in an identical style share the one frame. They
/// would render the same picture.
pub fn cue_key(cue: &Cue, staged: &str) -> String {
    let mut hash = Hasher::new();
    hash.bytes(staged.as_bytes());
    hash.separator();
    // Millis rather than the whole `Duration`: it is what the subtitle formats themselves
    // store, so two cues that are equal on disk cannot hash apart on a rounding. Needed
    // separately from `staged` because the staged file is *retimed* — the cue's own
    // timing is what decides where the grab seeks to, and two cues with identical text at
    // different moments stage byte-for-byte identically.
    hash.number(u128::from(cue.start.as_millis() as u64));
    hash.number(u128::from(cue.end.as_millis() as u64));
    hash.finish()
}

/// Where one cue's frame lives, or `None` when there is no cache directory at all.
///
/// Resolved through [`DiskCache::cache_dir`] rather than from the environment directly:
/// that one function is what redirects the unit-test binary to throwaway storage, and the
/// e2e harness redirects `XDG_CACHE_HOME` around it. Going straight to the environment
/// here would put a second door on the user's real cache directory that no test closes.
pub fn path(media: &str, cue: &str) -> Option<PathBuf> {
    Some(path_in(&directory()?, media, cue))
}

pub fn directory() -> Option<PathBuf> {
    Some(DiskCache::cache_dir()?.join(FRAMES_DIR))
}

fn path_in(directory: &Path, media: &str, cue: &str) -> PathBuf {
    directory
        .join(media)
        .join(format!("{cue}.{FRAME_EXTENSION}"))
}

/// The cached frame for one cue, if one was ever stored and still is.
pub fn read(media: &str, cue: &str) -> Option<Vec<u8>> {
    read_in(&directory()?, media, cue)
}

pub fn read_in(directory: &Path, media: &str, cue: &str) -> Option<Vec<u8>> {
    let bytes = fs::read(path_in(directory, media, cue)).ok()?;
    // A zero-byte file is what a half-finished write outside `store` would leave, and
    // handing it to the decoder produces "Unreadable frame" for something that is really
    // just a miss. Treated as absent so the next grab replaces it.
    (!bytes.is_empty()).then_some(bytes)
}

pub fn is_cached(media: &str, cue: &str) -> bool {
    directory().is_some_and(|directory| is_cached_in(&directory, media, cue))
}

pub fn is_cached_in(directory: &Path, media: &str, cue: &str) -> bool {
    fs::metadata(path_in(directory, media, cue)).is_ok_and(|frame| frame.len() > 0)
}

/// Records that a media's frames are being used now, for [`prune`]'s ranking.
///
/// Best-effort and cheap: one small write per page opening. Creates the directory, so a
/// track whose frames all turn out to be cached still has somewhere for its marker.
pub fn touch(media: &str) -> bool {
    directory().is_some_and(|directory| touch_in(&directory, media))
}

pub fn touch_in(directory: &Path, media: &str) -> bool {
    let media_directory = directory.join(media);
    if fs::create_dir_all(&media_directory).is_err() {
        return false;
    }
    // Rewritten rather than opened for append: the content is irrelevant, the mtime is the
    // whole point, and a write is the one portable way to move it.
    fs::write(media_directory.join(USED_MARKER), b"").is_ok()
}

/// Stores one frame, reporting only whether it landed.
///
/// Written to a temporary name and renamed, so a reader never sees a partial PNG — the
/// same shape `DiskCache::save_to` uses, and the reason two workers writing the same key
/// at once is harmless rather than a torn file.
///
/// Best-effort throughout: a cache that cannot be written is a slow page, not a broken
/// one, so every failure here is swallowed by the caller.
pub fn store(media: &str, cue: &str, bytes: &[u8]) -> bool {
    directory().is_some_and(|directory| store_in(&directory, media, cue, bytes))
}

pub fn store_in(directory: &Path, media: &str, cue: &str, bytes: &[u8]) -> bool {
    let media_directory = directory.join(media);
    if fs::create_dir_all(&media_directory).is_err() {
        return false;
    }
    let path = path_in(directory, media, cue);
    // Beside the frame, inside the media's own directory, so the rename is within one
    // directory and one filesystem — and so `prune_in`'s sweep of the cache root, which
    // deletes anything that is not a media directory, can never catch a write in flight.
    //
    // The temporary is unique per *write*, not per key: the process id keeps two `reel`s
    // apart, and the counter keeps this process's own writers apart. Sharing a name would
    // let one writer rename another's half-written file into place — the very tearing the
    // temporary exists to prevent — and the frame workers do render the same cue at the
    // same moment, since the interactive one answers the selection the background pass is
    // walking towards.
    static WRITES: AtomicU64 = AtomicU64::new(0);
    let temporary = media_directory.join(format!(
        ".{cue}.{}.{}.tmp",
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

/// Moves a media's whole directory of frames from one key to another.
///
/// The one place anything is *claimed* about two keys naming the same pictures, and the
/// claim is the caller's: [`media_key`] covers the file's length and mtime precisely so that
/// a file rewritten in place cannot serve its old frames, and a remux rewrites the file. But
/// this application's remux copies the video stream through untouched, so the frames it
/// grabbed are still the frames the new file would give — the container moved, the pictures
/// did not. Without this, saving a one-word cue edit throws away every rendered frame of a
/// feature-length track and the page spends the next several minutes rendering them again.
///
/// Deliberately not an equivalence baked into the key. Keying on something that survives a
/// remux would mean guessing which rewrites preserve the picture; moving the directory means
/// only the caller that *performed* the rewrite gets to say so, for the one file it just
/// wrote. `App::frames_survive_edit` is where that is decided.
///
/// Best-effort, like everything else here: a failure is a page that renders again, which is
/// exactly where it was without this.
pub fn migrate(from: &str, to: &str) -> bool {
    directory().is_some_and(|directory| migrate_in(&directory, from, to))
}

pub fn migrate_in(directory: &Path, from: &str, to: &str) -> bool {
    if from == to {
        return true;
    }
    let source = directory.join(from);
    if !source.is_dir() {
        return false;
    }
    let destination = directory.join(to);
    if destination.exists() {
        // The new file already has frames of its own — rendered before this landed, or by
        // another `reel`. They are the same pictures, so the older set is simply stale disk;
        // dropping it here is what keeps a run of saves from leaving a directory apiece.
        return fs::remove_dir_all(&source).is_ok();
    }
    fs::rename(&source, &destination).is_ok()
}

/// Deletes least-recently-used media directories until at most `tracks` remain, never the
/// one named by `open`.
///
/// Called once per page opening, from the background worker rather than the event loop: it
/// stats every entry in the cache root.
///
/// **Whole directories, never part of one.** That is the policy, not an implementation
/// detail: what the user opens is a track, and a track missing some of its frames is
/// re-rendered on every visit, so evicting a fraction of one buys disk at the cost of the
/// work the cache exists to avoid. Counting tracks rather than bytes is what makes that
/// expressible — a byte budget has no way to stop at a track boundary.
///
/// Ranked by the [`USED_MARKER`]'s mtime, falling back to the directory's own for one
/// written before the marker existed, so this is genuine least-recently-*used* rather than
/// least-recently-written.
///
/// `open` is excluded outright rather than merely ranked last. It cannot be the right
/// answer: the pass is about to render into it, so evicting it guarantees the re-render
/// this whole module exists to prevent. With `tracks = 0` that means the open track's
/// frames survive until the page closes and something else prunes.
///
/// Also sweeps anything in the root that is *not* a media directory. That is where the old
/// flat `{key}.jpg` layout's files sit, so the first prune after this change clears them;
/// it is also the only rule needed for any other stray. In-flight temporaries live inside a
/// media directory, so the sweep cannot race a write.
pub fn prune(tracks: usize, open: Option<&str>) {
    if let Some(directory) = directory() {
        prune_in(&directory, tracks, open);
    }
}

pub fn prune_in(directory: &Path, tracks: usize, open: Option<&str>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut cached: Vec<(SystemTime, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_dir() {
            // Not a media directory, so nothing here can ever be read again.
            let _ = fs::remove_file(&path);
            continue;
        }
        if path.file_name().and_then(|name| name.to_str()) == open {
            continue;
        }
        cached.push((used_at(&path, &metadata), path));
    }

    // The open track was skipped above, but it still occupies one of the `tracks` slots.
    let keep = tracks.saturating_sub(usize::from(open.is_some()));
    cached.sort_by_key(|(used, _)| *used);
    for (_, path) in cached.iter().take(cached.len().saturating_sub(keep)) {
        let _ = fs::remove_dir_all(path);
    }
}

/// When a media directory was last used: its marker's mtime, or its own if it has none.
fn used_at(path: &Path, metadata: &fs::Metadata) -> SystemTime {
    fs::metadata(path.join(USED_MARKER))
        .and_then(|marker| marker.modified())
        .or_else(|_| metadata.modified())
        .unwrap_or(UNIX_EPOCH)
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
            dialogue: Vec::new(),
            events: 1,
        }
    }

    /// A stand-in media directory, for the storage tests that are not about hashing.
    const MEDIA: &str = "media-key";

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

    /// Roughly what `preview` stages a SubRip cue as, which is what the key covers.
    ///
    /// Derived from the text rather than being the text, so a test cannot pass by hashing
    /// the cue where it should be hashing the file.
    fn staged(cue: &Cue) -> String {
        format!("1\n00:00:00,000 --> 00:10:00,000\n{}\n\n", cue.text)
    }

    /// Both halves of the path, together, for the tests that care about the whole thing.
    fn key(parts: FrameKeyParts<'_>, cue: &Cue) -> String {
        format!("{}/{}", media_key(parts), cue_key(cue, &staged(cue)))
    }

    #[test]
    fn the_same_media_and_cue_should_always_hash_the_same() {
        // Arrange
        let media = PathBuf::from("/media/show.mkv");

        // Act / Assert
        assert_that!(key(parts(&media), &cue(1000, 2000, "hello")).as_str())
            .is_equal_to(key(parts(&media), &cue(1000, 2000, "hello")).as_str());
        // Sixteen bytes of hex either side, so a path whose parts are always one length.
        assert_that!(media_key(parts(&media)).len()).is_equal_to(32);
        let one = cue(1000, 2000, "hello");
        assert_that!(cue_key(&one, &staged(&one)).len()).is_equal_to(32);
    }

    /// The split is the eviction policy: everything about the *media* has to land in the
    /// directory name, and everything about the *cue* in the filename. A media-level field
    /// that leaked into the cue key would scatter one track across directories, and a
    /// cue-level one that leaked into the media key would give every cue its own.
    #[test]
    fn the_media_half_and_the_cue_half_should_each_hold_only_their_own_fields() {
        // Arrange
        let media = PathBuf::from("/media/show.mkv");
        let other = PathBuf::from("/media/other.mkv");
        let base = media_key(parts(&media));

        // Act / Assert: every cue of one media shares one directory.
        let hello = cue(1000, 2000, "hello");
        let goodbye = cue(3000, 4000, "goodbye");
        assert_that!(cue_key(&hello, &staged(&hello)).as_str())
            .is_not_equal_to(cue_key(&goodbye, &staged(&goodbye)).as_str());
        assert_that!(media_key(parts(&media)).as_str()).is_equal_to(base.as_str());

        // Act / Assert: and every media-level difference moves the directory.
        assert_that!(media_key(parts(&other)).as_str()).is_not_equal_to(base.as_str());
        let mut relength = parts(&media);
        relength.media_length = 1_001;
        assert_that!(media_key(relength).as_str()).is_not_equal_to(base.as_str());
        let mut smaller = parts(&media);
        smaller.pixels = (640, 360);
        assert_that!(media_key(smaller).as_str()).is_not_equal_to(base.as_str());
    }

    /// Two cues can read the same words and still draw differently — an ASS cue names a
    /// style, a position and a layer, and none of that is in its text. Keying on the text
    /// would file both pictures under one name and hand back whichever was rendered first.
    ///
    /// Keying on the *staged file* is what makes this impossible: it is byte-for-byte what
    /// libass is given, so two frames that would differ cannot key alike.
    #[test]
    fn cues_that_read_alike_but_draw_differently_should_not_share_a_key() {
        // Arrange: identical text and timing, different staging.
        let cue = cue(1000, 2000, "look out");
        let plain = "Dialogue: 0,0:00:00.00,0:10:00.00,Default,,0,0,0,,look out";
        let sign = "Dialogue: 0,0:00:00.00,0:10:00.00,Sign,,0,0,0,,{\\pos(320,10)}look out";

        // Act / Assert
        assert_that!(cue_key(&cue, plain).as_str()).is_not_equal_to(cue_key(&cue, sign).as_str());

        // Act / Assert: and the same staging keys the same, or nothing would ever hit.
        assert_that!(cue_key(&cue, plain).as_str()).is_equal_to(cue_key(&cue, plain).as_str());
    }

    /// The staged file is retimed to cover the whole grab, so two cues carrying the same
    /// line at different moments stage byte-for-byte identically — and would collide on a
    /// key made of the staging alone, despite the grab seeking somewhere else entirely.
    #[test]
    fn cues_staged_alike_at_different_times_should_not_share_a_key() {
        // Arrange
        let early = cue(1000, 2000, "same words");
        let late = cue(60_000, 61_000, "same words");
        let file = "1\n00:00:00,000 --> 00:10:00,000\nsame words\n\n";

        // Act / Assert
        assert_that!(cue_key(&early, file).as_str()).is_not_equal_to(cue_key(&late, file).as_str());
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

    /// The version is what makes a change to what a stored frame *means* — its format, its
    /// encoder — orphan whole directories that then age out as units. Without it such a
    /// change leaves files whose names still collide with live keys, which is exactly the
    /// bug the PNG-to-JPEG switch left behind and had to be swept by hand.
    ///
    /// Asserted by rebuilding the hash by hand, both with the version and with the next
    /// one. Only the *matching* half catches the failure that matters: a version simply
    /// left out of `media_key` still differs from `version + 1`, so a test that checked
    /// nothing but the difference would pass against exactly the bug it is here for.
    #[test]
    fn the_format_version_should_be_part_of_every_media_directory() {
        // Arrange
        let media = PathBuf::from("/media/show.mkv");
        let composed = |version: u32| {
            let mut hash = Hasher::new();
            hash.bytes(media.as_os_str().as_encoded_bytes());
            hash.separator();
            hash.number(u128::from(1_000u64));
            hash.number(u128::from(1_700_000_000_000_000_000u64));
            hash.number(u128::from(960u32));
            hash.number(u128::from(540u32));
            hash.number(u128::from(version));
            hash.finish()
        };

        // Act / Assert: the version is in there…
        assert_that!(media_key(parts(&media)).as_str())
            .is_equal_to(composed(CACHE_FORMAT_VERSION).as_str());
        // …and moves the whole directory when it changes.
        assert_that!(composed(CACHE_FORMAT_VERSION).as_str())
            .is_not_equal_to(composed(CACHE_FORMAT_VERSION + 1).as_str());
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
        let stored = store_in(&directory, MEDIA, "frame", b"\xff\xd8\xff frame bytes");

        // Assert
        assert_that!(stored).is_true();
        assert_that!(is_cached_in(&directory, MEDIA, "frame")).is_true();
        assert_that!(read_in(&directory, MEDIA, "frame"))
            .is_equal_to(Some(b"\xff\xd8\xff frame bytes".to_vec()));
        // No temporary left behind by a write that succeeded.
        let leftovers = fs::read_dir(directory.join(MEDIA))
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
        assert_that!(read_in(&directory, MEDIA, "never-stored")).is_none();
        assert_that!(is_cached_in(&directory, MEDIA, "never-stored")).is_false();

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
        fs::create_dir_all(directory.join(MEDIA)).unwrap();
        fs::write(path_in(&directory, MEDIA, "empty"), b"").unwrap();

        // Act / Assert
        assert_that!(read_in(&directory, MEDIA, "empty")).is_none();
        assert_that!(is_cached_in(&directory, MEDIA, "empty")).is_false();

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
        let stored = store_in(&directory, MEDIA, "frame", b"frame");

        // Assert
        assert_that!(stored).is_true();
        assert_that!(is_cached_in(&directory, MEDIA, "frame")).is_true();

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

        // Act / Assert: a file sitting where the cache root belongs.
        let blocked = parent.join("in-the-way");
        fs::write(&blocked, b"not a directory").unwrap();
        assert_that!(store_in(&blocked, MEDIA, "frame", b"frame")).is_false();

        // Act / Assert: a directory sitting where the frame belongs.
        let directory = parent.join("frames");
        fs::create_dir_all(path_in(&directory, MEDIA, "occupied")).unwrap();
        assert_that!(store_in(&directory, MEDIA, "occupied", b"frame")).is_false();
        assert_that!(read_in(&directory, MEDIA, "occupied")).is_none();

        // Act / Assert: and a temporary that cannot be written at all, which is the write
        // failing rather than the rename. A cue key naming a directory that does not exist
        // does it without depending on file permissions, which a test running as root
        // would not get to observe.
        assert_that!(store_in(&directory, MEDIA, "missing/taken", b"frame")).is_false();
        assert_that!(is_cached_in(&directory, MEDIA, "missing/taken")).is_false();

        // Act / Assert: and a marker that cannot be written is a track that simply loses
        // its place in the ranking, not a failure that reaches the page.
        assert_that!(touch_in(&blocked, MEDIA)).is_false();

        // Assert: nothing was left lying beside them.
        let leftovers = fs::read_dir(directory.join(MEDIA))
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .filter(|entry| entry.path().is_file())
            .count();
        assert_that!(leftovers).is_equal_to(0);

        // Cleanup
        fs::remove_dir_all(parent).unwrap();
    }

    /// Puts `frames` frames in a media directory and dates its use marker `hours` ago.
    fn track(directory: &Path, media: &str, frames: usize, hours: u64) {
        for index in 0..frames {
            assert_that!(store_in(
                directory,
                media,
                &format!("cue-{index}"),
                &[0u8; 100]
            ))
            .is_true();
        }
        assert_that!(touch_in(directory, media)).is_true();
        fs::File::open(directory.join(media).join(USED_MARKER))
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(3600 * hours))
            .expect("the test filesystem should allow setting an mtime");
    }

    fn holds(directory: &Path, media: &str, frames: usize) -> bool {
        (0..frames).all(|index| is_cached_in(directory, media, &format!("cue-{index}")))
    }

    /// The cache has to have a ceiling, and the track that goes is the least recently used
    /// one — whole, never in part.
    #[test]
    fn pruning_should_delete_whole_tracks_least_recently_used_first() {
        // Arrange: three tracks of differing size, used oldest-first.
        let directory = scratch("prune");
        track(&directory, "oldest", 4, 3);
        track(&directory, "middle", 2, 2);
        track(&directory, "newest", 3, 1);

        // Act: room for two of the three.
        prune_in(&directory, 2, None);

        // Assert: the least recently used one went entirely, and the others are intact —
        // *every* frame of them, which is the property a byte budget could not offer.
        assert_that!(directory.join("oldest").exists()).is_false();
        assert_that!(holds(&directory, "middle", 2)).is_true();
        assert_that!(holds(&directory, "newest", 3)).is_true();

        // Act / Assert: and a cache already inside its limit is left entirely alone.
        prune_in(&directory, 2, None);
        assert_that!(holds(&directory, "middle", 2)).is_true();
        assert_that!(holds(&directory, "newest", 3)).is_true();

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    /// Ranked by last *use*, not last write. A track that is fully cached is the ideal
    /// case — the pass renders nothing — so ranking by write would let the tracks actually
    /// being watched age out under ones rendered once and never opened again.
    #[test]
    fn pruning_should_rank_by_last_use_rather_than_last_write() {
        // Arrange: `stale` was written most recently, `watched` used most recently.
        let directory = scratch("prune-lru");
        track(&directory, "watched", 2, 10);
        track(&directory, "stale", 2, 1);
        // The watched track is opened again, rendering nothing — only the marker moves.
        assert_that!(touch_in(&directory, "watched")).is_true();

        // Act: room for one.
        prune_in(&directory, 1, None);

        // Assert
        assert_that!(holds(&directory, "watched", 2)).is_true();
        assert_that!(directory.join("stale").exists()).is_false();

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    /// The open track is excluded outright rather than merely ranked last. Evicting it
    /// guarantees the re-render the cache exists to prevent, because the pass is about to
    /// render into it — so it survives even at a limit that leaves room for nothing else,
    /// and even when it is the least recently used thing there.
    #[test]
    fn pruning_should_never_evict_the_track_that_is_open() {
        // Arrange: the open track is the oldest of the three.
        let directory = scratch("prune-open");
        track(&directory, "open", 2, 100);
        track(&directory, "other", 2, 2);
        track(&directory, "another", 2, 1);

        // Act: room for two, one of which the open track takes.
        prune_in(&directory, 2, Some("open"));

        // Assert: the open track stayed despite being oldest, and only one other survived.
        assert_that!(holds(&directory, "open", 2)).is_true();
        assert_that!(directory.join("other").exists()).is_false();
        assert_that!(holds(&directory, "another", 2)).is_true();

        // Act / Assert: and at zero it is still the one thing kept, since the pass is
        // rendering into it — everything else goes.
        prune_in(&directory, 0, Some("open"));
        assert_that!(holds(&directory, "open", 2)).is_true();
        assert_that!(directory.join("another").exists()).is_false();

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    /// Anything in the cache root that is not a media directory can never be read again:
    /// the flat `{key}.jpg` files the previous layout left behind, and any other stray.
    /// Sweeping them is one rule rather than a migration.
    #[test]
    fn pruning_should_sweep_files_left_in_the_cache_root() {
        // Arrange
        let directory = scratch("prune-root");
        track(&directory, "kept", 2, 1);
        let flat = directory.join(format!("0123456789abcdef.{FRAME_EXTENSION}"));
        fs::write(&flat, [0u8; 100]).unwrap();
        let stray = directory.join("something-else");
        fs::write(&stray, b"?").unwrap();

        // Act: a limit generous enough that no track is at risk.
        prune_in(&directory, 10, None);

        // Assert: the leftovers went and the track did not.
        assert_that!(flat.exists()).is_false();
        assert_that!(stray.exists()).is_false();
        assert_that!(holds(&directory, "kept", 2)).is_true();

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    /// The sweep runs over the cache *root*, and a write in flight is
    /// `{media}/.{cue}.{pid}.{n}.tmp` — inside a media directory, never beside one. That
    /// is what keeps a prune from deleting a frame another `reel` is halfway through
    /// writing.
    #[test]
    fn the_root_sweep_should_not_reach_a_write_in_flight() {
        // Arrange
        let directory = scratch("prune-inflight");
        track(&directory, "busy", 1, 1);
        let temporary = directory.join("busy").join(".cue-9.999.0.tmp");
        fs::write(&temporary, [0u8; 100]).unwrap();

        // Act
        prune_in(&directory, 10, Some("busy"));

        // Assert
        assert_that!(temporary.exists()).is_true();

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
        prune_in(&directory, 0, None);
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
        assert_that!(path(MEDIA, "frame"))
            .is_equal_to(Some(directory.join(MEDIA).join("frame.jpg")));
        assert_that!(store(MEDIA, "frame", &[0u8; 100])).is_true();
        assert_that!(is_cached(MEDIA, "frame")).is_true();
        assert_that!(read(MEDIA, "frame")).is_equal_to(Some(vec![0u8; 100]));
        assert_that!(touch(MEDIA)).is_true();
        // Nothing open, so the one track there is goes.
        prune(0, None);
        assert_that!(is_cached(MEDIA, "frame")).is_false();

        // Cleanup
        let _ = fs::remove_dir_all(&directory);
    }

    /// A save rewrites the container around a video stream it copied through untouched, so
    /// the file's key moves while its pictures do not. The frames go with the key, whole
    /// track at a time, or the page re-renders every one of them after every save.
    #[test]
    fn migrating_a_track_should_carry_its_whole_directory_to_the_new_key() {
        // Arrange: a track's frames, and its used marker.
        let _guard = one_key();
        let directory = scratch("migrate");
        assert!(store_in(&directory, "before", "one", &[1u8; 64]));
        assert!(store_in(&directory, "before", "two", &[2u8; 64]));
        assert!(touch_in(&directory, "before"));

        // Act
        assert_that!(migrate_in(&directory, "before", "after")).is_true();

        // Assert: every frame is there under the new key, and nothing under the old one.
        assert_that!(read_in(&directory, "after", "one")).is_equal_to(Some(vec![1u8; 64]));
        assert_that!(read_in(&directory, "after", "two")).is_equal_to(Some(vec![2u8; 64]));
        assert_that!(directory.join("after").join(USED_MARKER).exists()).is_true();
        assert_that!(directory.join("before").exists()).is_false();

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    /// The cases that are not a move: nothing to move, a key that did not change, and a
    /// destination that already holds the same pictures — which is stale disk to drop
    /// rather than a reason to keep two directories of one track.
    #[test]
    fn migrating_should_answer_for_a_key_that_did_not_move_or_has_nothing_behind_it() {
        // Arrange
        let _guard = one_key();
        let directory = scratch("migrate-edges");
        assert!(store_in(&directory, "here", "one", &[3u8; 64]));

        // Act / Assert
        assert_that!(migrate_in(&directory, "here", "here")).is_true();
        assert_that!(read_in(&directory, "here", "one")).is_equal_to(Some(vec![3u8; 64]));
        assert_that!(migrate_in(&directory, "missing", "elsewhere")).is_false();
        assert_that!(directory.join("elsewhere").exists()).is_false();

        // A destination that already exists keeps its own frames and loses the old set.
        assert!(store_in(&directory, "newer", "one", &[4u8; 64]));
        assert_that!(migrate_in(&directory, "here", "newer")).is_true();
        assert_that!(read_in(&directory, "newer", "one")).is_equal_to(Some(vec![4u8; 64]));
        assert_that!(directory.join("here").exists()).is_false();

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    /// The directory-resolving half of the move, which is the one production calls.
    #[test]
    fn the_resolved_migration_should_move_frames_inside_the_real_cache_directory() {
        // Arrange
        let _guard = whole_directory();
        let directory = directory().expect("the test binary always has a cache directory");
        let _ = fs::remove_dir_all(&directory);
        assert_that!(store(MEDIA, "frame", &[9u8; 32])).is_true();

        // Act
        assert_that!(migrate(MEDIA, "moved")).is_true();

        // Assert
        assert_that!(read("moved", "frame")).is_equal_to(Some(vec![9u8; 32]));
        assert_that!(is_cached(MEDIA, "frame")).is_false();

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
                    assert!(store_in(&directory, MEDIA, "frame", &[7u8; 4096]));
                });
            }
        });

        // Assert
        assert_that!(read_in(&directory, MEDIA, "frame")).is_equal_to(Some(vec![7u8; 4096]));

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }
}
