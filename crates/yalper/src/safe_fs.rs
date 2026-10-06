//! File system access that stops Yalper from following links planted in a repository.
//!
//! A cloned repository could contain a `.yalper` symlink, or a `.yalper/errors.log` symlink, pointing to a
//! file outside the project. Yalper writes only inside a real directory owned by the current user, and only
//! to regular files.
//!
//! Files Yalper opens itself are checked on the open handle, never on a path that is opened afterwards. The
//! directory is held open while it is used: on Unix those files are opened relative to that handle
//! (`openat`), and on Windows the directory and file handles do not allow renaming or deleting, so their
//! paths keep naming what was checked.
//!
//! SQLite opens its own files by path. For those, see [`OwnedDir::check_regular_or_missing`],
//! [`OwnedDir::guard_path`], and `Store::open`.

use std::fs::File;
use std::io::{self, Read};
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
    ///
    /// On Unix, [`path`](Self::path) is canonical: the parent is resolved, so that the path SQLite reopens
    /// contains no symlink (SQLite is told to refuse any).
    pub fn open(path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use rustix::fs::{Mode, OFlags};
            // Resolving the parent, not `path` itself, keeps a `.yalper` symlink visible to O_NOFOLLOW.
            let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
                return Err(io::Error::other(format!(
                    "{} has no parent directory",
                    path.display()
                )));
            };
            let path = std::fs::canonicalize(parent)?.join(name);
            let handle = rustix::fs::open(
                &path,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?;
            if rustix::fs::fstat(&handle)?.st_uid != rustix::process::geteuid().as_raw() {
                return Err(io::Error::other(format!(
                    "{} is not owned by the current user",
                    path.display()
                )));
            }
            Ok(Self { path, handle })
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

    /// Another handle to the same directory, for a thread that outlives the borrow of this one.
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            path: self.path.clone(),
            #[cfg(unix)]
            handle: self.handle.try_clone()?,
            #[cfg(windows)]
            _handle: self._handle.try_clone()?,
        })
    }

    /// Checks that [`path`](Self::path) still names this directory, not a link or another folder put in its
    /// place. Unix: same device and inode as the held handle. Windows: the held handle stops the folder from
    /// being renamed or deleted, so the path is only checked to be a real folder.
    pub fn check_still_at_path(&self) -> io::Result<()> {
        let named = std::fs::symlink_metadata(&self.path)?;
        #[cfg(unix)]
        let same = {
            use std::os::unix::fs::MetadataExt;
            named.is_dir()
                && (named.dev(), named.ino()) == stat_id(&rustix::fs::fstat(&self.handle)?)
        };
        #[cfg(windows)]
        let same = named.is_dir();
        if same {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "{} was replaced",
                self.path.display()
            )))
        }
    }

    /// Whether users other than the owner can create, rename or delete entries in this directory: on Unix,
    /// its group or world write bit is set.
    ///
    /// Windows: always false, because Yalper does not inspect ACLs yet. A folder under the user's profile is
    /// private by default, but one created outside it (for example under `C:\`) inherits Modify for
    /// Authenticated Users, so other local users could change it there.
    pub fn is_writable_by_others(&self) -> io::Result<bool> {
        #[cfg(unix)]
        return Ok(rustix::fs::fstat(&self.handle)?.st_mode & 0o022 != 0);
        #[cfg(windows)]
        Ok(false)
    }

    /// Opens the file `name` inside this directory, creating it if it is missing. Anything other than a
    /// regular file (a symlink, a directory, a FIFO) is refused before a single byte is read or written, and
    /// so is, on Unix, a file with a second hard link (writing to it would change the other file too).
    ///
    /// Windows: the standard library does not expose the hard link count, so a hard-linked file is
    /// accepted. A clone cannot create hard links, so only another local process could plant one.
    pub fn open_file(&self, name: &str, access: Access) -> io::Result<File> {
        let file = self.open_entry(name, access)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || hard_link_count(&metadata) != 1 {
            return Err(self.not_a_regular_file(name));
        }
        Ok(file)
    }

    /// Checks that `name` inside this directory does not exist, or is a regular file (on Unix with no other
    /// hard link), for files that another library opens by path (SQLite's journal and WAL files).
    pub fn check_regular_or_missing(&self, name: &str) -> io::Result<()> {
        #[cfg(unix)]
        let is_file =
            match rustix::fs::statat(&self.handle, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
                // No link left: the file was deleted between the name lookup and the stat (SQLite deletes its
                // rollback journal while another process checks it), so it is missing.
                Ok(stat) if stat.st_nlink == 0 => return Ok(()),
                Ok(stat) => {
                    rustix::fs::FileType::from_raw_mode(stat.st_mode)
                        == rustix::fs::FileType::RegularFile
                        && stat.st_nlink == 1
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
        // macOS can fail with ENOENT when several processes create the same file at the same moment (seen
        // in CI; the Zig compiler works around the same race). Trying again succeeds.
        let mut retries = 0;
        loop {
            match rustix::fs::openat(&self.handle, name, flags, Mode::RUSR | Mode::WUSR) {
                Err(rustix::io::Errno::NOENT) if retries < 10 => retries += 1,
                result => return Ok(File::from(result?)),
            }
        }
    }

    #[cfg(windows)]
    fn open_entry(&self, name: &str, access: Access) -> io::Result<File> {
        use std::os::windows::fs::OpenOptionsExt;
        let mut options = File::options();
        options
            .create(true)
            // No FILE_SHARE_DELETE: while the file is open nobody can rename or delete it, so its path keeps
            // naming this file (SQLite reopens the database by path).
            .share_mode(windows::FILE_SHARE_READ | windows::FILE_SHARE_WRITE)
            .custom_flags(windows::FILE_FLAG_OPEN_REPARSE_POINT);
        match access {
            Access::Append => options.append(true),
            Access::ReadWrite => options.read(true).write(true),
        };
        options.open(self.path.join(name))
    }

    fn not_a_regular_file(&self, name: &str) -> io::Error {
        io::Error::other(format!(
            "{} is not a regular file with a single link",
            self.path.join(name).display()
        ))
    }
}

/// A file inside an [`OwnedDir`] that another library (SQLite) opens by path, checked before and after that
/// open. See [`OwnedDir::guard_path`].
#[derive(Debug)]
pub struct PathGuard {
    /// Device and inode of the file before the open, if it existed.
    #[cfg(unix)]
    before: Option<(u64, u64)>,
    /// The file, created and checked by [`OwnedDir::open_file`] and kept open: it stops the path from being
    /// renamed or deleted. Windows locks belong to one handle, so keeping it open is safe.
    #[cfg(windows)]
    _file: File,
}

impl OwnedDir {
    /// Prepares the file `name` for another library to open by path: refuses anything but a regular file
    /// with a single link. Call [`check_guarded`](Self::check_guarded) once the library has opened it.
    ///
    /// Unix: the check is made by name relative to the directory handle and the library creates a missing
    /// file. No descriptor is opened, because closing any descriptor of a file drops every POSIX lock this
    /// process holds on it, including the locks of SQLite connections already open on it.
    /// Windows: the file is created if missing, checked through a handle, and the handle is kept.
    pub fn guard_path(&self, name: &str) -> io::Result<PathGuard> {
        #[cfg(unix)]
        {
            self.check_regular_or_missing(name)?;
            let before =
                match rustix::fs::statat(&self.handle, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
                {
                    Ok(stat) => Some(stat_id(&stat)),
                    Err(rustix::io::Errno::NOENT) => None,
                    Err(error) => return Err(error.into()),
                };
            Ok(PathGuard { before })
        }
        #[cfg(windows)]
        Ok(PathGuard {
            _file: self.open_file(name, Access::ReadWrite)?,
        })
    }

    /// Checks, after another library opened `self.path().join(name)`, that this path names the regular
    /// single-link file `name` of this directory, and the same file as before the open if it existed then
    /// (Unix: device and inode). On Windows the kept handle already ensures this.
    pub fn check_guarded(&self, name: &str, guard: &PathGuard) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            self.check_regular_or_missing(name)?;
            let here = stat_id(&rustix::fs::statat(
                &self.handle,
                name,
                rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
            )?);
            let path = self.path.join(name);
            let named = std::fs::symlink_metadata(&path)?;
            if (named.dev(), named.ino()) != here
                || guard.before.is_some_and(|before| before != here)
            {
                return Err(io::Error::other(format!(
                    "{} was replaced while it was being opened",
                    path.display()
                )));
            }
        }
        #[cfg(windows)]
        let _ = (name, guard);
        Ok(())
    }
}

/// Opens the file at `path` for reading, refusing anything that is not a regular file once open.
///
/// For files of the project that a directory walk found to be regular files. On Unix a symlink at `path` is
/// refused without being followed, and a FIFO without waiting for a writer, in case the entry changed since
/// the walk. Windows: the walk already told links apart, and the file is opened normally, because opening the
/// reparse point itself would bypass OneDrive's handling of cloud files.
/// Remaining gap on Windows, outside the threat model: another process of the same user could swap the file
/// for a link between the walk and this open, and the link would be followed.
pub fn open_regular_file(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    let file = {
        use rustix::fs::{Mode, OFlags};
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        File::from(rustix::fs::open(path, flags, Mode::empty())?)
    };
    #[cfg(windows)]
    let file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    Ok(file)
}

/// The content of `path` if it is a regular file (not a link, FIFO or device) of at most `max_bytes`.
pub fn read_small_regular_file(path: &Path, max_bytes: u64) -> Option<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        return None;
    }
    let mut bytes = Vec::new();
    open_regular_file(path)
        .ok()?
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= max_bytes).then_some(bytes)
}

/// Device and inode, typed like `std::os::unix::fs::MetadataExt`.
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // The field types differ between Linux and macOS.
fn stat_id(stat: &rustix::fs::Stat) -> (u64, u64) {
    (stat.st_dev as u64, stat.st_ino as u64)
}

#[cfg(unix)]
fn hard_link_count(metadata: &std::fs::Metadata) -> u64 {
    std::os::unix::fs::MetadataExt::nlink(metadata)
}

#[cfg(windows)]
fn hard_link_count(_metadata: &std::fs::Metadata) -> u64 {
    1
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
        assert_eq!(
            fs::canonicalize(owned.path()).unwrap(),
            fs::canonicalize(dir.path()).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_parent_is_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let real = fs::canonicalize(dir.path()).unwrap().join("real");
        fs::create_dir_all(real.join(".yalper")).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let owned = OwnedDir::open(&link.join(".yalper")).unwrap();
        assert_eq!(owned.path(), real.join(".yalper"));
    }

    #[cfg(unix)]
    #[test]
    fn a_hard_linked_file_is_refused_and_the_other_file_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let other = dir.path().join("other.txt");
        fs::write(&other, "keep me").unwrap();
        fs::hard_link(&other, dir.path().join("log")).unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        for access in [Access::Append, Access::ReadWrite] {
            assert!(owned.open_file("log", access).is_err());
        }
        assert!(owned.check_regular_or_missing("log").is_err());
        assert_eq!(fs::read_to_string(&other).unwrap(), "keep me");
    }

    #[test]
    fn an_unchanged_file_passes_the_guard() {
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        fs::write(dir.path().join("db"), "").unwrap();
        let guard = owned.guard_path("db").unwrap();
        owned.check_guarded("db", &guard).unwrap();
    }

    #[test]
    fn a_file_created_by_the_other_library_passes_the_guard() {
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        let guard = owned.guard_path("db").unwrap();
        fs::write(owned.path().join("db"), "").unwrap();
        owned.check_guarded("db", &guard).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_replaced_file_fails_the_guard() {
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        fs::write(dir.path().join("db"), "").unwrap();
        let guard = owned.guard_path("db").unwrap();
        fs::write(dir.path().join("new"), "").unwrap();
        fs::rename(dir.path().join("new"), dir.path().join("db")).unwrap();
        assert!(owned.check_guarded("db", &guard).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_link_fails_the_guard() {
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        std::os::unix::fs::symlink(dir.path().join("elsewhere"), dir.path().join("db")).unwrap();
        assert!(owned.guard_path("db").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn a_guarded_file_cannot_be_swapped() {
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        let guard = owned.guard_path("db").unwrap();
        assert!(dir.path().join("db").is_file());
        fs::write(dir.path().join("new"), "").unwrap();
        assert!(fs::rename(dir.path().join("new"), dir.path().join("db")).is_err());
        assert!(fs::remove_file(dir.path().join("db")).is_err());
        drop(guard);
        fs::rename(dir.path().join("new"), dir.path().join("db")).unwrap();
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

    #[test]
    fn only_regular_files_are_opened_for_reading() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file.txt");
        fs::write(&file, "content").unwrap();
        let mut text = String::new();
        open_regular_file(&file)
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "content");
        assert!(open_regular_file(dir.path()).is_err());

        #[cfg(unix)]
        {
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(&file, &link).unwrap();
            assert!(open_regular_file(&link).is_err());
            let fifo = dir.path().join("fifo");
            let status = std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap();
            assert!(status.success());
            assert!(open_regular_file(&fifo).is_err());
        }
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
