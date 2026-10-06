//! File system access that stops Yalper from following links planted in a repository.
//!
//! A cloned repository could contain a `.yalper` symlink, or a `.yalper/errors.log` symlink, pointing to a
//! file outside the project. Yalper writes only inside a real directory owned by the current user, and only
//! to regular files.
//!
//! Every check is made on an open handle, never on a path that is opened afterwards, so a link swapped in
//! between the check and the open is refused too. The directory is held open while it is used: on Unix
//! files are opened relative to that handle (`openat`), and on Windows the handle does not allow the
//! directory to be renamed or deleted, so its path keeps naming the directory that was checked.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

/// How [`OwnedDir::open_file`] opens a file. Both create the file if it is missing and never truncate it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Write only, every write goes to the end of the file.
    Append,
    /// Read and write from the start of the file.
    ReadWrite,
}

/// A real directory owned by the current user, held open.
#[derive(Debug)]
pub struct OwnedDir {
    path: PathBuf,
    #[cfg(unix)]
    handle: std::os::fd::OwnedFd,
    #[cfg(windows)]
    _handle: File,
}

impl OwnedDir {
    /// Opens `path` if it is a real directory owned by the current user. A symlink (or a Windows junction,
    /// which the standard library also reports as a symlink) is refused without being followed.
    pub fn open(path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use rustix::fs::{Mode, OFlags};
            let handle = rustix::fs::open(
                path,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?;
            if rustix::fs::fstat(&handle)?.st_uid != rustix::process::geteuid().as_raw() {
                return Err(io::Error::other(format!(
                    "{} is not owned by the current user",
                    path.display()
                )));
            }
            Ok(Self {
                path: path.to_owned(),
                handle,
            })
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            let handle = File::options()
                .read(true)
                .share_mode(windows::FILE_SHARE_READ | windows::FILE_SHARE_WRITE)
                .custom_flags(
                    windows::FILE_FLAG_BACKUP_SEMANTICS | windows::FILE_FLAG_OPEN_REPARSE_POINT,
                )
                .open(path)?;
            if !handle.metadata()?.is_dir() {
                return Err(io::Error::other(format!(
                    "{} is not a directory",
                    path.display()
                )));
            }
            Ok(Self {
                path: path.to_owned(),
                _handle: handle,
            })
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Opens the file `name` inside this directory, creating it if it is missing. Anything other than a
    /// regular file (a symlink, a directory, a FIFO) is refused before a single byte is read or written.
    pub fn open_file(&self, name: &str, access: Access) -> io::Result<File> {
        let file = self.open_entry(name, access)?;
        if !file.metadata()?.is_file() {
            return Err(self.not_a_regular_file(name));
        }
        Ok(file)
    }

    /// Checks that `name` inside this directory is a regular file or does not exist, for files that another
    /// library opens by path (SQLite's database, journal and WAL files).
    pub fn check_regular_or_missing(&self, name: &str) -> io::Result<()> {
        #[cfg(unix)]
        let is_file =
            match rustix::fs::statat(&self.handle, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) => {
                    rustix::fs::FileType::from_raw_mode(stat.st_mode)
                        == rustix::fs::FileType::RegularFile
                }
                Err(rustix::io::Errno::NOENT) => return Ok(()),
                Err(error) => return Err(error.into()),
            };
        #[cfg(windows)]
        let is_file = match std::fs::symlink_metadata(self.path.join(name)) {
            Ok(metadata) => metadata.is_file(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if is_file {
            Ok(())
        } else {
            Err(self.not_a_regular_file(name))
        }
    }

    #[cfg(unix)]
    fn open_entry(&self, name: &str, access: Access) -> io::Result<File> {
        use rustix::fs::{Mode, OFlags};
        // NONBLOCK: opening a FIFO for writing would otherwise wait forever for a reader.
        let mut flags = OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
        flags |= match access {
            Access::Append => OFlags::WRONLY | OFlags::APPEND,
            Access::ReadWrite => OFlags::RDWR,
        };
        let fd = rustix::fs::openat(&self.handle, name, flags, Mode::RUSR | Mode::WUSR)?;
        Ok(File::from(fd))
    }

    #[cfg(windows)]
    fn open_entry(&self, name: &str, access: Access) -> io::Result<File> {
        use std::os::windows::fs::OpenOptionsExt;
        let mut options = File::options();
        options
            .create(true)
            .custom_flags(windows::FILE_FLAG_OPEN_REPARSE_POINT);
        match access {
            Access::Append => options.append(true),
            Access::ReadWrite => options.read(true).write(true),
        };
        options.open(self.path.join(name))
    }

    fn not_a_regular_file(&self, name: &str) -> io::Error {
        io::Error::other(format!(
            "{} is not a regular file",
            self.path.join(name).display()
        ))
    }
}

/// Win32 constants, defined here instead of adding a crate for four numbers.
#[cfg(windows)]
mod windows {
    pub const FILE_SHARE_READ: u32 = 0x0000_0001;
    pub const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    /// Needed to open a directory.
    pub const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    /// Opens a symlink or junction itself instead of its target.
    pub const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{Read, Seek, Write};

    #[test]
    fn a_real_directory_is_opened() {
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        assert_eq!(owned.path(), dir.path());
    }

    #[test]
    fn a_file_or_missing_path_is_not_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        fs::write(&file, "x").unwrap();
        assert!(OwnedDir::open(&file).is_err());
        assert!(OwnedDir::open(&dir.path().join("missing")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_to_a_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(OwnedDir::open(&link).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn a_junction_is_refused() {
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
        assert!(OwnedDir::open(&link).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn an_open_directory_cannot_be_swapped() {
        let parent = tempfile::tempdir().unwrap();
        let path = parent.path().join("dir");
        let moved = parent.path().join("moved");
        fs::create_dir(&path).unwrap();
        let owned = OwnedDir::open(&path).unwrap();
        assert!(fs::rename(&path, &moved).is_err());
        owned.open_file("still-works", Access::Append).unwrap();
        drop(owned);
        fs::rename(&path, &moved).unwrap();
    }

    #[test]
    fn regular_files_are_created_and_appended_to() {
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        owned
            .open_file("log", Access::Append)
            .unwrap()
            .write_all(b"one\n")
            .unwrap();
        owned
            .open_file("log", Access::Append)
            .unwrap()
            .write_all(b"two\n")
            .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("log")).unwrap(),
            "one\ntwo\n"
        );
    }

    #[test]
    fn read_write_does_not_truncate() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("data"), "keep").unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        let mut file = owned.open_file("data", Access::ReadWrite).unwrap();
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        assert_eq!(text, "keep");
        file.rewind().unwrap();
        file.write_all(b"K").unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("data")).unwrap(), "Keep");
    }

    #[cfg(unix)]
    #[test]
    fn new_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        let file = owned.open_file("new", Access::ReadWrite).unwrap();
        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn a_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        for access in [Access::Append, Access::ReadWrite] {
            assert!(owned.open_file("sub", access).is_err());
        }
        assert!(owned.check_regular_or_missing("sub").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_is_refused_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("mkfifo")
            .arg(dir.path().join("fifo"))
            .status()
            .unwrap();
        assert!(status.success());
        let owned = OwnedDir::open(dir.path()).unwrap();
        for access in [Access::Append, Access::ReadWrite] {
            assert!(owned.open_file("fifo", access).is_err());
        }
        assert!(owned.check_regular_or_missing("fifo").is_err());
    }

    #[test]
    fn a_symlink_to_a_file_is_refused_and_its_target_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("outside.txt");
        fs::write(&target, "keep me").unwrap();
        if !symlink_file(&target, &dir.path().join("log")) {
            return;
        }
        let owned = OwnedDir::open(dir.path()).unwrap();
        for access in [Access::Append, Access::ReadWrite] {
            assert!(owned.open_file("log", access).is_err());
        }
        assert!(owned.check_regular_or_missing("log").is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep me");
    }

    #[test]
    fn regular_and_missing_files_pass_the_check() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("file"), "x").unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        owned.check_regular_or_missing("file").unwrap();
        owned.check_regular_or_missing("missing").unwrap();
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
