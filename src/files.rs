use std::{
    cmp::Ordering,
    fs,
    path::{Path, PathBuf},
};

use anyhow::Result;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileEntry {
    pub path: PathBuf,
    pub display_name: String,
}

pub fn scan_directory(directory: &Path) -> Result<Vec<FileEntry>> {
    let mut files = Vec::new();

    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_file()
            || entry
                .path()
                .metadata()
                .map(|metadata| metadata.is_file())
                .unwrap_or(false)
        {
            files.push(FileEntry {
                path: entry.path(),
                display_name: entry.file_name().to_string_lossy().into_owned(),
            });
        }
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

#[cfg(test)]
mod tests {
    use std::{fs::File, path::PathBuf};

    use super::*;

    #[test]
    fn scans_regular_files_and_sorts_case_insensitively() {
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

        let result = scan_directory(&directory).unwrap();
        let names: Vec<_> = result
            .iter()
            .map(|entry| entry.display_name.as_str())
            .collect();

        assert_eq!(names, vec![".hidden", "Alpha.txt", "zeta.mp4"]);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn empty_directory_returns_empty_list() {
        let directory: PathBuf =
            std::env::temp_dir().join(format!("reel-tui-empty-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        assert!(scan_directory(&directory).unwrap().is_empty());
        fs::remove_dir_all(directory).unwrap();
    }
}
