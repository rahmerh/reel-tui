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

pub fn detect_mount_type(path: &Path) -> MountType {
    if let Ok(value) = env::var("REEL_NETWORK_MODE") {
        match value.trim().to_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => return MountType::Network,
            "0" | "false" | "no" | "off" => return MountType::Local,
            _ => {}
        }
    }

    #[cfg(target_os = "linux")]
    {
        if let Ok(mounts_content) = fs::read_to_string("/proc/mounts")
            && let Some(is_net) = check_proc_mounts(&mounts_content, path)
        {
            return if is_net {
                MountType::Network
            } else {
                MountType::Local
            };
        }
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
    fn unescape_octal_should_convert_space_escapes() {
        assert_eq!(unescape_octal("/mnt/my\\040media"), "/mnt/my media");
        assert_eq!(unescape_octal("/plain/path"), "/plain/path");
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
