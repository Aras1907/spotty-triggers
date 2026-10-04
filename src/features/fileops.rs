// File operation engine: copy / cut / paste for selected search results.
// Plus the find-mode shortcuts: trash a selected item and resolve which
// directory "open location in file manager" should land in.
//
// State lives in a thread_local so pending copy/cut selections survive Spotty
// being hidden and re-shown while the daemon keeps running.
//
// Operations:
//   • copy(path)            — remember a path to be copied on next paste
//   • cut(path)             — remember a path to be MOVED on next paste
//   • paste(dest_dir)       — perform the pending copy/cut into dest_dir
//   • trash_path(path)      — move a file/folder to the Trash (recoverable)
//   • location_target(...)  — dir to open in the file manager for a result

use std::cell::RefCell;
use std::path::{Path, PathBuf};

// `gio::FileExt` provides the `trash` method used by `trash_path`.
use gtk::gio::prelude::FileExt;

#[derive(Clone, Debug)]
enum PendingOp {
    Copy(PathBuf),
    Cut(PathBuf),
}

thread_local! {
    static PENDING: RefCell<Option<PendingOp>> = const { RefCell::new(None) };
}

/// Mark a path to be copied on the next paste.
pub fn copy(path: &Path) {
    PENDING.with(|p| *p.borrow_mut() = Some(PendingOp::Copy(path.to_path_buf())));
    log::info!("fileops: copy {}", path.display());
}

/// Mark a path to be moved on the next paste.
pub fn cut(path: &Path) {
    PENDING.with(|p| *p.borrow_mut() = Some(PendingOp::Cut(path.to_path_buf())));
    log::info!("fileops: cut {}", path.display());
}

/// True if there is a pending copy/cut waiting to be pasted.
pub fn has_pending() -> bool {
    PENDING.with(|p| p.borrow().is_some())
}

/// Paste the pending copy/cut into `dest_dir`. Returns the new path on success.
pub fn paste(dest_dir: &Path) -> Option<PathBuf> {
    let op = PENDING.with(|p| p.borrow().clone())?;
    let src = match &op { PendingOp::Copy(p) | PendingOp::Cut(p) => p };
    let meta = std::fs::symlink_metadata(src).ok()?;
    if meta.is_dir() && std::fs::canonicalize(dest_dir).ok()?
        .starts_with(std::fs::canonicalize(src).ok()?) {
        // Copying/moving a directory into itself would recurse indefinitely.
        return None;
    }
    match op {
        PendingOp::Copy(src) => {
            let dest = unique_dest(dest_dir, &src);
            if copy_recursive(&src, &dest).is_ok() {
                log::info!("fileops: pasted (copy) -> {}", dest.display());
                Some(dest)
            } else {
                None
            }
        }
        PendingOp::Cut(src) => {
            let dest = unique_dest(dest_dir, &src);
            if move_path(&src, &dest).is_ok() {
                // A cut is consumed once pasted.
                PENDING.with(|p| *p.borrow_mut() = None);
                log::info!("fileops: pasted (move) -> {}", dest.display());
                Some(dest)
            } else {
                None
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────

/// Choose a destination path inside `dir` named after `src`, avoiding collisions.
fn unique_dest(dir: &Path, src: &Path) -> PathBuf {
    let name = src
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_else(|| "item".into());
    unique_path(&dir.join(name))
}

/// If `candidate` exists, append " (copy)", " (copy 2)", ... before the extension.
fn unique_path(candidate: &Path) -> PathBuf {
    if std::fs::symlink_metadata(candidate).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) {
        return candidate.to_path_buf();
    }
    let parent = candidate.parent().unwrap_or_else(|| Path::new("."));
    let stem = candidate
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("item");
    let ext = candidate.extension().and_then(|s| s.to_str());
    for i in 1..1000 {
        let suffix = if i == 1 {
            " (copy)".to_string()
        } else {
            format!(" (copy {})", i)
        };
        let name = match ext {
            Some(e) => format!("{}{}.{}", stem, suffix, e),
            None => format!("{}{}", stem, suffix),
        };
        let p = parent.join(name);
        if std::fs::symlink_metadata(&p).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) {
            return p;
        }
    }
    candidate.to_path_buf()
}

fn copy_recursive(src: &Path, dest: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    let meta = std::fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        // Copy the link, not its target. This avoids leaking files outside
        // the selected tree and loops through self-referential symlinks.
        std::os::unix::fs::symlink(std::fs::read_link(src)?, dest)
    } else if meta.is_dir() {
        std::fs::DirBuilder::new().mode(0o700).create(dest)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &dest.join(entry.file_name()))?;
        }
        std::fs::set_permissions(dest, meta.permissions())
    } else if meta.is_file() {
        let mut input = std::fs::OpenOptions::new().read(true)
            .custom_flags(libc::O_NOFOLLOW).open(src)?;
        let mut output = std::fs::OpenOptions::new().write(true).create_new(true)
            .mode(meta.permissions().mode() & 0o777).open(dest)?;
        std::io::copy(&mut input, &mut output).map(|_| ())
    } else {
        Err(std::io::Error::other("Cannot copy special files"))
    }
}

fn move_path(src: &Path, dest: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let from = std::ffi::CString::new(src.as_os_str().as_bytes())?;
    let to = std::ffi::CString::new(dest.as_os_str().as_bytes())?;
    // Linux atomic no-replace rename: a newly created destination, including
    // a dangling symlink, must never be overwritten between check and move.
    let result = unsafe { libc::renameat2(libc::AT_FDCWD, from.as_ptr(),
        libc::AT_FDCWD, to.as_ptr(), libc::RENAME_NOREPLACE) };
    if result == 0 { return Ok(()); }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() != Some(libc::EXDEV) { return Err(error); }
    copy_recursive(src, dest)?;
    remove_path(src)
}

fn remove_path(p: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(p)?.is_dir() {
        std::fs::remove_dir_all(p)
    } else {
        std::fs::remove_file(p)
    }
}

// ──────────────────────────────────────────────────────────────────────
// Delete + location (find-mode shortcuts)
// ──────────────────────────────────────────────────────────────────────

/// Move a file/folder to the Trash — recoverable, never a permanent delete.
/// Uses GLib's trash API: the XDG trash natively, and the Trash portal inside
/// the Flatpak sandbox so the item lands in the host's Trash, not a sandbox
/// copy.
pub fn trash_path(path: &Path) -> Result<(), String> {
    let file = gtk::gio::File::for_path(path);
    file.trash(None::<&gtk::gio::Cancellable>)
        .map_err(|e| e.to_string())
}

/// Where "open location in file manager" lands for a selected result: a
/// folder opens itself, a file opens its containing folder. Generic on
/// purpose — asking a file manager to select the item would need
/// file-manager-specific flags, and this must work with any of them.
pub fn location_target(path: &Path, is_dir: bool) -> PathBuf {
    if is_dir {
        return path.to_path_buf();
    }
    match path.parent() {
        // `parent()` of a bare relative name is Some("") — treat like None.
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_target_opens_folder_itself_and_file_parent() {
        assert_eq!(
            location_target(Path::new("/home/u/Docs"), true),
            PathBuf::from("/home/u/Docs")
        );
        assert_eq!(
            location_target(Path::new("/home/u/Docs/report.pdf"), false),
            PathBuf::from("/home/u/Docs")
        );
        assert_eq!(
            location_target(Path::new("/file.txt"), false),
            PathBuf::from("/")
        );
        // No usable parent (bare relative name) → the path itself, never "".
        assert_eq!(
            location_target(Path::new("file.txt"), false),
            PathBuf::from("file.txt")
        );
    }
}
