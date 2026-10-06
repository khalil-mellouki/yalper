//! File system checks that stop Yalper from following links planted in a repository.
//!
//! A cloned repository could contain a `.yalper` symlink, or a `.yalper/errors.log` symlink, pointing to a
//! file outside the project. Yalper writes only inside a real directory owned by the current user, and only
//! to regular files.

use std::fs::{self, File, Metadata, OpenOptions};
use std::io;
use std::path::Path;

/// Returns true if `path` is a real directory owned by the current user. Symlinks (and Windows junctions,
/// which the standard library also reports as symlinks) are rejected without being followed.
pub fn is_owned_dir(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.is_dir() && is_owned_by_current_user(&metadata))
}

#[cfg(unix)]
fn is_owned_by_current_user(metadata: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.uid() == rustix::process::geteuid().as_raw()
}

#[cfg(not(unix))]
fn is_owned_by_current_user(_metadata: &Metadata) -> bool {
    true
}

/// Opens `path` with `options` only if it is a regular file or does not exist yet. A symlink or any other
/// kind of entry is refused. On Unix the open itself also refuses symlinks (`O_NOFOLLOW`).
pub fn open_regular_file(path: &Path, options: &OpenOptions) -> io::Result<File> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() => {
            return Err(io::Error::other(format!(
                "{} is not a regular file",
                path.display()
            )));
        }
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    #[cfg(unix)]
    let options = &{
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = options.clone();
        options.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
        options
    };
    options.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn a_real_directory_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        assert!(is_owned_dir(dir.path()));
    }

    #[test]
    fn a_file_or_missing_path_is_not_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        fs::write(&file, "x").unwrap();
        assert!(!is_owned_dir(&file));
        assert!(!is_owned_dir(&dir.path().join("missing")));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_to_a_directory_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(!is_owned_dir(&link));
    }

    #[cfg(windows)]
    #[test]
    fn a_junction_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = dir.path().join("link");
        let status = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(&link)
            .arg(&target)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        assert!(!is_owned_dir(&link));
    }

    #[test]
    fn regular_files_are_opened_and_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        open_regular_file(&path, &options)
            .unwrap()
            .write_all(b"one\n")
            .unwrap();
        open_regular_file(&path, &options)
            .unwrap()
            .write_all(b"two\n")
            .unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "one\ntwo\n");
    }

    #[test]
    fn a_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        assert!(open_regular_file(dir.path(), &options).is_err());
    }

    #[test]
    fn a_symlink_to_a_file_is_refused_and_its_target_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("outside.txt");
        fs::write(&target, "keep me").unwrap();
        let link = dir.path().join("log");
        if !symlink_file(&target, &link) {
            return;
        }
        for truncate in [false, true] {
            let mut options = OpenOptions::new();
            options.create(true);
            if truncate {
                options.write(true).truncate(true);
            } else {
                options.append(true);
            }
            assert!(open_regular_file(&link, &options).is_err());
        }
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep me");
    }

    /// Creates a file symlink. Returns false on Windows when the user may not create symlinks.
    fn symlink_file(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        let result = std::os::unix::fs::symlink(target, link);
        #[cfg(windows)]
        let result = std::os::windows::fs::symlink_file(target, link);
        match result {
            Ok(()) => true,
            Err(_) if cfg!(windows) => false,
            Err(error) => panic!("cannot create symlink: {error}"),
        }
    }
}
