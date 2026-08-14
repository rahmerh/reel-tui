use std::{
    env, fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MountType {
    Local,
    Network,
}

pub fn is_network_mount(path: &Path) -> bool {
    detect_mount_type(path) == MountType::Network
}

/// Reads `REEL_NETWORK_MODE` as an override of the detected mount type. Split out from
/// `detect_mount_type` so the parsing can be tested directly: the alternative is mutating
/// the process environment from a test, which is both racy under the threaded test runner
/// and `unsafe` in this edition.
///
/// An unrecognised value is deliberately `None` (fall through to real detection) rather
/// than an error — the variable is a performance escape hatch, and a typo in it must not
/// stop `reel` from opening a directory.
fn network_mode_override(value: &str) -> Option<MountType> {
    match value.trim().to_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(MountType::Network),
        "0" | "false" | "no" | "off" => Some(MountType::Local),
        _ => None,
    }
}

pub fn detect_mount_type(path: &Path) -> MountType {
    // The override answers without touching the filesystem. This runs per probe and per
    // temporary-path computation, so someone who set `REEL_NETWORK_MODE` precisely to stop
    // Reel from guessing should not pay a `/proc/mounts` read on every one of them.
    let override_value = env::var("REEL_NETWORK_MODE").ok();
    if let Some(override_type) = override_value.as_deref().and_then(network_mode_override) {
        return override_type;
    }
    #[cfg(target_os = "linux")]
    let mounts_content = fs::read_to_string("/proc/mounts").ok();
    #[cfg(not(target_os = "linux"))]
    let mounts_content: Option<String> = None;
    detect_mount_type_from(path, override_value.as_deref(), mounts_content.as_deref())
}

fn detect_mount_type_from(
    path: &Path,
    override_value: Option<&str>,
    mounts_content: Option<&str>,
) -> MountType {
    if let Some(override_type) = override_value.and_then(network_mode_override) {
        return override_type;
    }
    #[cfg(target_os = "linux")]
    if let Some(is_network) = mounts_content.and_then(|mounts| check_proc_mounts(mounts, path)) {
        return if is_network {
            MountType::Network
        } else {
            MountType::Local
        };
    }
    MountType::Local
}

#[cfg(target_os = "linux")]
fn check_proc_mounts(mounts_content: &str, target_path: &Path) -> Option<bool> {
    let canonical = target_path
        .canonicalize()
        .unwrap_or_else(|_| target_path.to_path_buf());

    let mut longest_match_len = 0;
    let mut is_network = None;

    for line in mounts_content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }

        let mount_point_str = unescape_octal(parts[1]);
        let mount_point = PathBuf::from(&mount_point_str);
        let fstype = parts[2];

        if canonical == mount_point || canonical.starts_with(&mount_point) {
            let len = mount_point.as_os_str().len();
            if len >= longest_match_len {
                longest_match_len = len;
                is_network = Some(is_known_network_fstype(fstype));
            }
        }
    }

    is_network
}

pub fn is_known_network_fstype(fstype: &str) -> bool {
    let lower = fstype.to_lowercase();
    matches!(
        lower.as_str(),
        "nfs" | "nfs4" | "cifs" | "smb3" | "smbfs" | "ceph" | "glusterfs" | "9p" | "afs" | "nfsd"
    ) || lower.starts_with("fuse.sshfs")
        || lower.starts_with("fuse.rclone")
        || lower.starts_with("sshfs")
        || lower.starts_with("rclone")
}

fn unescape_octal(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let octal_digits = &s[i + 1..i + 4];
            if let Ok(val) = u8::from_str_radix(octal_digits, 8) {
                result.push(val as char);
                i += 4;
                continue;
            }
        }
        result.push(bytes[i] as char);
        i += 1;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_known_network_fstype_should_identify_nfs_cifs_and_sshfs() {
        assert!(is_known_network_fstype("nfs"));
        assert!(is_known_network_fstype("nfs4"));
        assert!(is_known_network_fstype("cifs"));
        assert!(is_known_network_fstype("smb3"));
        assert!(is_known_network_fstype("fuse.sshfs"));
        assert!(is_known_network_fstype("fuse.rclone"));
        assert!(!is_known_network_fstype("ext4"));
        assert!(!is_known_network_fstype("btrfs"));
        assert!(!is_known_network_fstype("xfs"));
        assert!(!is_known_network_fstype("tmpfs"));
    }

    #[test]
    fn is_known_network_fstype_should_identify_the_remaining_cluster_and_fuse_filesystems() {
        // Arrange / Act / Assert: each of these is its own branch in the match and prefix
        // chain. A share whose fstype falls through reads as local, which means `reel`
        // probes it with local timings and stats it once a second — the exact thrashing
        // the network mode exists to avoid.
        assert!(is_known_network_fstype("ceph"));
        assert!(is_known_network_fstype("glusterfs"));
        assert!(is_known_network_fstype("9p"));
        assert!(is_known_network_fstype("afs"));
        assert!(is_known_network_fstype("nfsd"));
        assert!(is_known_network_fstype("smbfs"));
        // Bare (non-FUSE) spellings, and the versioned forms FUSE appends.
        assert!(is_known_network_fstype("sshfs"));
        assert!(is_known_network_fstype("rclone"));
        assert!(is_known_network_fstype("fuse.rclone-mount"));
        // The lowercasing is what makes an uppercase fstype match at all.
        assert!(is_known_network_fstype("NFS4"));
        assert!(is_known_network_fstype("CIFS"));
        // Neighbours that must not match: local filesystems and unrelated FUSE mounts.
        assert!(!is_known_network_fstype("fuse.portal"));
        assert!(!is_known_network_fstype("overlay"));
        assert!(!is_known_network_fstype("zfs"));
        assert!(!is_known_network_fstype(""));
    }

    #[test]
    fn network_mode_override_should_accept_every_documented_spelling() {
        // Arrange / Act / Assert: `REEL_NETWORK_MODE` is documented as taking 1/0, and in
        // practice people write `true`/`yes`/`on`. Casing and stray whitespace come free
        // with shell quoting, so both are normalised away.
        for value in ["1", "true", "yes", "on", "TRUE", "  On  "] {
            assert_eq!(
                network_mode_override(value),
                Some(MountType::Network),
                "{value:?} must force network mode",
            );
        }
        for value in ["0", "false", "no", "off", "FALSE", " off "] {
            assert_eq!(
                network_mode_override(value),
                Some(MountType::Local),
                "{value:?} must force local mode",
            );
        }
    }

    #[test]
    fn network_mode_override_should_defer_to_real_detection_for_an_unrecognised_value() {
        // Arrange / Act / Assert: a typo or an empty assignment must fall through to
        // `/proc/mounts` rather than silently pinning one mode — the variable is a tuning
        // escape hatch, not a switch that fails closed.
        for value in ["", "   ", "maybe", "2", "network"] {
            assert_eq!(
                network_mode_override(value),
                None,
                "{value:?} must fall through to real detection",
            );
        }
    }

    #[test]
    fn a_recognised_override_should_answer_without_consulting_proc_mounts() {
        // The override exists so someone on an exotic mount can stop Reel guessing. It
        // runs per probe and per temporary-path computation, so it has to answer from the
        // variable alone — passing no mount table at all must not change the verdict.
        for (value, expected) in [
            ("1", MountType::Network),
            ("true", MountType::Network),
            ("off", MountType::Local),
            ("no", MountType::Local),
        ] {
            assert_eq!(
                detect_mount_type_from(Path::new("/mnt/media/movie.mkv"), Some(value), None),
                expected,
            );
        }
        // An unrecognised value still falls through to real detection rather than
        // becoming an error, so a typo cannot stop a directory opening.
        assert_eq!(
            detect_mount_type_from(Path::new("/mnt/media/movie.mkv"), Some("maybe"), None),
            MountType::Local,
        );
    }

    #[test]
    fn mount_detection_should_apply_overrides_then_proc_mounts_then_local_fallback() {
        let mounts = "/dev/sda1 / ext4 rw 0 0\n//nas/media /mnt/media cifs rw 0 0\n";

        assert_eq!(
            detect_mount_type_from(Path::new("/local/movie.mkv"), Some("yes"), Some(mounts)),
            MountType::Network,
        );
        assert_eq!(
            detect_mount_type_from(Path::new("/mnt/media/movie.mkv"), Some("off"), Some(mounts),),
            MountType::Local,
        );
        assert_eq!(
            detect_mount_type_from(
                Path::new("/mnt/media/movie.mkv"),
                Some("maybe"),
                Some(mounts)
            ),
            MountType::Network,
        );
        assert_eq!(
            detect_mount_type_from(Path::new("/local/movie.mkv"), None, Some(mounts)),
            MountType::Local,
        );
        assert_eq!(
            detect_mount_type_from(Path::new("/any/movie.mkv"), None, None),
            MountType::Local,
        );
    }

    #[test]
    fn unescape_octal_should_convert_space_escapes() {
        assert_eq!(unescape_octal("/mnt/my\\040media"), "/mnt/my media");
        assert_eq!(unescape_octal("/plain/path"), "/plain/path");
    }

    #[test]
    fn unescape_octal_should_pass_through_escapes_it_cannot_decode() {
        // Arrange / Act / Assert: a backslash too close to the end to carry three digits,
        // and one followed by non-octal digits. Both must survive verbatim — dropping the
        // backslash would silently rewrite the mount point and break the prefix match
        // that decides whether the path is on a network share.
        assert_eq!(unescape_octal("/mnt/trailing\\"), "/mnt/trailing\\");
        assert_eq!(unescape_octal("/mnt/short\\04"), "/mnt/short\\04");
        assert_eq!(unescape_octal("/mnt/notoctal\\999"), "/mnt/notoctal\\999");
        assert_eq!(unescape_octal("/mnt/hex\\0x4"), "/mnt/hex\\0x4");
        // A tab escape, alongside the space case already covered above.
        assert_eq!(unescape_octal("/mnt/a\\011b"), "/mnt/a\tb");
    }

    #[test]
    fn check_proc_mounts_should_report_nothing_when_no_mount_point_contains_the_path() {
        // Arrange: `/proc/mounts` without a root entry — the shape a container or a
        // truncated read can produce. Returning `Some(false)` here would assert the path
        // is local on no evidence; `None` lets the caller fall back to its own default.
        let mounts = "192.168.1.50:/volume1/media /mnt/media nfs4 rw,relatime 0 0\n";

        // Act / Assert
        assert_eq!(check_proc_mounts(mounts, Path::new("/srv/video.mkv")), None);
    }

    #[test]
    fn check_proc_mounts_should_skip_lines_that_are_too_short_to_carry_an_fstype() {
        // Arrange: a blank line and a truncated one alongside the real entries. Indexing
        // straight into `parts[2]` without the length guard would panic here, taking down
        // the whole app on a malformed `/proc/mounts` read.
        let mounts = "\n\
            garbage\n\
            /dev/sda1 /mnt\n\
            192.168.1.50:/volume1/media /mnt/media nfs4 rw,relatime 0 0\n";

        // Act / Assert: the well-formed line is still found.
        assert_eq!(
            check_proc_mounts(mounts, Path::new("/mnt/media/Movies/film.mkv")),
            Some(true),
        );
    }

    #[test]
    fn check_proc_mounts_should_keep_the_longest_match_when_a_shorter_one_comes_later() {
        // Arrange: `/proc/mounts` is not sorted, so the more specific mount point can
        // appear before the general one it sits inside. Only the longest match describes
        // the path — taking the later, shorter entry would report a local sub-mount as
        // network (or the reverse) purely because of line order.
        let mounts = "\
/dev/nvme0n1p2 / btrfs rw 0 0
/dev/sdb1 /mnt/media/fast ext4 rw 0 0
192.168.1.50:/volume1 /mnt/media nfs4 rw 0 0
";

        // Act / Assert: the file inside the local sub-mount stays local, even though the
        // network mount matches it too and is listed afterwards.
        assert_eq!(
            check_proc_mounts(mounts, Path::new("/mnt/media/fast/film.mkv")),
            Some(false),
        );
        // A file outside the sub-mount still resolves to the network mount.
        assert_eq!(
            check_proc_mounts(mounts, Path::new("/mnt/media/other.mkv")),
            Some(true),
        );
    }

    #[test]
    fn check_proc_mounts_should_prefer_the_last_entry_when_two_mount_points_tie() {
        // Arrange: the same mount point listed twice is what a remount leaves behind —
        // the later line is the one in effect, so the scan must not stop at the first.
        let mounts = "\
/dev/nvme0n1p2 / btrfs rw 0 0
192.168.1.50:/volume1 /mnt/media ext4 rw 0 0
192.168.1.50:/volume1 /mnt/media nfs4 rw 0 0
";

        // Act / Assert
        assert_eq!(
            check_proc_mounts(mounts, Path::new("/mnt/media/film.mkv")),
            Some(true),
        );
    }

    #[test]
    fn check_proc_mounts_should_match_a_mount_point_containing_an_escaped_space() {
        // Arrange: a share mounted under a path with a space arrives octal-escaped in
        // `/proc/mounts`. Comparing against the raw escaped text would never match, and
        // the share would be treated as local.
        let mounts = "//nas/media /mnt/my\\040media cifs rw 0 0\n";

        // Act / Assert
        assert_eq!(
            check_proc_mounts(mounts, Path::new("/mnt/my media/film.mkv")),
            Some(true),
        );
    }

    #[test]
    fn detect_mount_type_should_fall_back_to_local_for_a_path_that_does_not_exist() {
        // Arrange / Act: canonicalize fails on a missing path, so detection works from the
        // uncanonicalised path and finds no better answer than the root mount.
        let mount_type = detect_mount_type(Path::new("/nonexistent/reel-tui/movie.mkv"));

        // Assert: a definite answer either way — never a panic, and never a hang — since
        // this runs on the UI thread before every probe.
        assert!(matches!(mount_type, MountType::Local | MountType::Network));
        assert_eq!(
            is_network_mount(Path::new("/nonexistent/reel-tui/movie.mkv")),
            mount_type == MountType::Network,
        );
    }

    #[test]
    fn check_proc_mounts_should_pick_longest_matching_mountpoint() {
        let sample_mounts = r#"
sysfs /sys sysfs rw,nosuid,nodev,noexec,relatime 0 0
/dev/nvme0n1p2 / btrfs rw,noatime,compress=zstd:1,ssd 0 0
192.168.1.50:/volume1/media /mnt/media nfs4 rw,relatime 0 0
192.168.1.50:/volume1/media/fast /mnt/media/fast ext4 rw,relatime 0 0
"#;

        assert_eq!(
            check_proc_mounts(sample_mounts, Path::new("/mnt/media/Movies")),
            Some(true)
        );
        assert_eq!(
            check_proc_mounts(sample_mounts, Path::new("/mnt/media/fast/video.mkv")),
            Some(false)
        );
        assert_eq!(
            check_proc_mounts(sample_mounts, Path::new("/home/user/video.mp4")),
            Some(false)
        );
    }
}
