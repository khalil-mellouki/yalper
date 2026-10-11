//! Links from a test copy to the project's installed dependencies (`node_modules` packages, Python virtual
//! environments), and the path rules they share: project folders are listed and checked without following
//! links, and a path is never followed out of the project.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use super::copy::{native, remove_entry};
use crate::snapshot::CASE_INSENSITIVE;

/// The kind of error returned when the project's dependencies cannot be linked on this system: on Windows, a
/// project on a network share or a mapped network drive, because junctions can only point to local volumes;
/// anywhere, a file system where the first link of a copy cannot be made (exFAT, FAT32). The caller then uses
/// a copy with its own install instead.
pub const CANNOT_LINK: io::ErrorKind = io::ErrorKind::Unsupported;

/// The entries of the folder `dir`: name, type (a link not followed) and path. Empty if the folder cannot be
/// read (a test may have taken the permission away) or is gone.
pub(super) fn list(dir: &Path) -> io::Result<Vec<(OsString, fs::FileType, PathBuf)>> {
    let skipped = |error: &io::Error| {
        matches!(
            error.kind(),
            io::ErrorKind::PermissionDenied | io::ErrorKind::NotFound
        )
    };
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if skipped(&error) => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut listed = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if skipped(&error) => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        match entry.file_type() {
            Ok(file_type) => listed.push((entry.file_name(), file_type, entry.path())),
            Err(error) if skipped(&error) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(listed)
}

/// `path` (relative, `/`-separated, `""` for `root` itself) under `root`.
pub(super) fn at(root: &Path, path: &str) -> PathBuf {
    if path.is_empty() {
        root.to_owned()
    } else {
        native(root, path)
    }
}

/// Whether `path` is a real folder, not a link (which is not followed).
pub(super) fn is_real_dir_no_follow(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

/// Whether `path` is `root` or inside it, comparing names without regard to case where the file system
/// ignores it.
pub(super) fn is_under(path: &Path, root: &Path) -> bool {
    let mut path = path.components();
    root.components().all(|root| {
        path.next().is_some_and(|component| {
            let (a, b) = (component.as_os_str(), root.as_os_str());
            if CASE_INSENSITIVE {
                a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
            } else {
                a == b
            }
        })
    })
}

/// `path` without the `\\?\` prefix of Windows' canonical paths: junction targets are stored and read back
/// without it, so the targets Yalper writes compare equal to what it reads.
pub(super) fn plain(path: &Path) -> PathBuf {
    #[cfg(windows)]
    if let Some(rest) = path.to_str().and_then(|text| text.strip_prefix(r"\\?\")) {
        if let Some(share) = rest.strip_prefix(r"UNC\") {
            return PathBuf::from(format!(r"\\{share}"));
        }
        if rest.as_bytes().get(1) == Some(&b':') {
            return PathBuf::from(rest);
        }
    }
    path.to_owned()
}

/// Whether `path` is on a network share (on Windows, a UNC path; a mapped network drive is one once
/// canonical).
pub(super) fn is_network(path: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::path::Prefix;
        matches!(path.components().next(), Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), Prefix::UNC(..) | Prefix::VerbatimUNC(..)))
    }
    #[cfg(unix)]
    {
        let _ = path;
        false
    }
}

/// Whether `path` starts with a Windows prefix other than a drive letter: a network share or a device.
pub(super) fn has_special_prefix(path: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::path::Prefix;
        matches!(path.components().next(), Some(Component::Prefix(prefix))
            if !matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)))
    }
    #[cfg(unix)]
    {
        let _ = path;
        false
    }
}

/// `path` with `.` and `..` resolved from its own text, as Python's `os.path.abspath` does: no link is looked
/// at, and `..` never goes above the root.
pub(super) fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(
                    normalized.components().next_back(),
                    Some(Component::Normal(_))
                ) {
                    normalized.pop();
                }
            }
            other => normalized.push(other),
        }
    }
    normalized
}

/// Makes `path` a link to `target`, unless it already is one (`is_link`: the listing of its folder found a
/// link there). Anything else at `path` is removed first (a link itself, never what it points to). Returns
/// whether a link was made.
///
/// `made_link`: whether a link was already made in the copy. Until one was, a failure has the kind
/// [`CANNOT_LINK`]: the file system may not support links at all (exFAT, FAT32), and the caller then uses a
/// copy with its own install. Afterwards a failure keeps its own kind (a full disk, a permission).
/// [`make_link`] leaves nothing behind when it fails.
pub(super) fn link_to(
    path: &Path,
    is_link: bool,
    target: &Path,
    made_link: &mut bool,
) -> io::Result<bool> {
    if is_link && fs::read_link(path).is_ok_and(|current| current == target) {
        return Ok(false);
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) => remove_entry(path, &metadata)?,
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        Err(_) => {}
    }
    make_link(target, path).map_err(|error| {
        if *made_link {
            error
        } else {
            io::Error::new(
                CANNOT_LINK,
                format!("links cannot be made in the test copy: {error}"),
            )
        }
    })?;
    *made_link = true;
    Ok(true)
}

/// Creates a link at `link` to the folder `target`: a symlink on Unix, a junction on Windows (absolute target).
pub(super) fn make_link(target: &Path, link: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    junction::create(target, link).inspect_err(|_| {
        // `junction::create` makes the folder first; nothing was at `link` before.
        let _ = fs::remove_dir(link);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn network_and_verbatim_paths_are_told_apart() {
        let path = |text: &str| PathBuf::from(text);
        assert!(is_network(&path(r"\\?\UNC\server\share\project")));
        assert!(is_network(&path(r"\\server\share\project")));
        assert!(!is_network(&path(r"\\?\C:\project")));
        assert!(!is_network(&path(r"C:\project")));
        assert_eq!(plain(&path(r"\\?\C:\project")), path(r"C:\project"));
        assert_eq!(
            plain(&path(r"\\?\UNC\server\share\project")),
            path(r"\\server\share\project")
        );
        assert_eq!(plain(&path(r"C:\project")), path(r"C:\project"));
        assert!(has_special_prefix(&path(r"\\server\share\x")));
        assert!(has_special_prefix(&path(r"\\.\pipe\x")));
        assert!(has_special_prefix(&path(r"\\?\UNC\server\share\x")));
        assert!(!has_special_prefix(&path(r"C:\x")));
        assert!(!has_special_prefix(&path(r"..\packages\lib")));
    }

    #[test]
    fn a_path_is_under_a_folder_by_whole_names() {
        let path = |text: &str| PathBuf::from(text);
        assert!(is_under(&path("/p/packages/lib"), &path("/p")));
        assert!(is_under(&path("/p"), &path("/p")));
        assert!(!is_under(&path("/other/lib"), &path("/p")));
        assert!(!is_under(&path("/p2/lib"), &path("/p")));
        assert_eq!(is_under(&path("/P/lib"), &path("/p")), CASE_INSENSITIVE);
    }

    #[test]
    fn dots_are_resolved_from_the_text_and_never_go_above_the_root() {
        let normalized = |text: &str| {
            normalize(Path::new(text))
                .to_string_lossy()
                .replace('\\', "/")
        };
        assert_eq!(normalized("/p/a/./b/../c"), "/p/a/c");
        assert_eq!(normalized("/p/../../x"), "/x");
        assert_eq!(normalized("/p/a/"), "/p/a");
        #[cfg(windows)]
        assert_eq!(normalized(r"C:\p\..\..\x"), "C:/x");
    }

    #[test]
    fn a_link_failure_before_any_link_was_made_tells_the_caller_to_fall_back() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        // No folder to make the link in: the system's error, whatever the cause.
        let missing = dir.path().join("missing").join("link");
        let mut made_link = false;
        let error = link_to(&missing, false, &target, &mut made_link).unwrap_err();
        assert_eq!(error.kind(), CANNOT_LINK);
        assert!(!made_link);
        assert!(!dir.path().join("missing").exists());

        // Once a link was made, a later failure keeps its own kind.
        let made = dir.path().join("link");
        assert!(link_to(&made, false, &target, &mut made_link).unwrap());
        assert!(made_link);
        assert_eq!(
            fs::read_link(&made).unwrap(),
            std::path::absolute(&target).unwrap()
        );
        // Already a link to the target: nothing to do.
        assert!(!link_to(&made, true, &target, &mut made_link).unwrap());
        let error = link_to(&missing, false, &target, &mut made_link).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}
