//! Read Git changes for directory listings.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub fn changed_entries(directory: &Path) -> HashSet<PathBuf> {
    let Ok(directory) = directory.canonicalize() else {
        return HashSet::new();
    };
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(&directory)
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stderr(Stdio::null())
            .output()
    };
    let Ok(root) = git(&["rev-parse", "--show-toplevel"]) else {
        return HashSet::new();
    };
    if !root.status.success() {
        return HashSet::new();
    }
    let root = PathBuf::from(
        String::from_utf8_lossy(&root.stdout)
            .trim_end_matches(['\r', '\n'])
            .to_string(),
    );
    let Ok(output) = git(&[
        "status",
        "--porcelain=v1",
        "-z",
        "--untracked-files=all",
        "--no-renames",
        "--",
        ".",
    ]) else {
        return HashSet::new();
    };
    if !output.status.success() {
        return HashSet::new();
    }
    let mut changed = HashSet::new();
    for record in output.stdout.split(|byte| *byte == 0) {
        if record.len() < 4 || record[2] != b' ' {
            continue;
        }
        #[cfg(unix)]
        let relative = {
            use std::os::unix::ffi::OsStrExt;
            Path::new(std::ffi::OsStr::from_bytes(&record[3..]))
        };
        #[cfg(not(unix))]
        let relative_string = String::from_utf8_lossy(&record[3..]);
        #[cfg(not(unix))]
        let relative = Path::new(relative_string.as_ref());
        let absolute = root.join(relative);
        if let Ok(within) = absolute.strip_prefix(&directory) {
            if let Some(first) = within.components().next() {
                changed.insert(directory.join(first.as_os_str()));
            }
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_changed_and_untracked_entries_and_their_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q"]);
        std::fs::create_dir(root.join("nested")).unwrap();
        std::fs::write(root.join("modified.txt"), "before\n").unwrap();
        std::fs::write(root.join("clean.txt"), "clean\n").unwrap();
        std::fs::write(root.join("nested/tracked.txt"), "before\n").unwrap();
        std::fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
        git(&["add", "."]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "initial",
        ]);
        std::fs::write(root.join("modified.txt"), "after\n").unwrap();
        std::fs::write(root.join("nested/tracked.txt"), "after\n").unwrap();
        std::fs::write(root.join("new file.txt"), "new\n").unwrap();
        std::fs::write(root.join("ignored.txt"), "ignored\n").unwrap();
        let changes = changed_entries(&root);
        assert!(changes.contains(&root.join("modified.txt")));
        assert!(changes.contains(&root.join("new file.txt")));
        assert!(changes.contains(&root.join("nested")));
        assert!(!changes.contains(&root.join("clean.txt")));
        assert!(!changes.contains(&root.join("ignored.txt")));
        assert!(changed_entries(&root.join("nested")).contains(&root.join("nested/tracked.txt")));
    }
}
