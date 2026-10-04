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
    if !candidate.exists() {
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
        if !p.exists() {
            return p;
        }
    }
    candidate.to_path_buf()
}

fn copy_recursive(src: &Path, dest: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dest)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let child_dest = dest.join(entry.file_name());
            copy_recursive(&entry.path(), &child_dest)?;
        }
        Ok(())
    } else {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dest).map(|_| ())
    }
}

fn move_path(src: &Path, dest: &Path) -> std::io::Result<()> {
    // Try a fast rename first; fall back to copy+delete across filesystems.
    if std::fs::rename(src, dest).is_ok() {
        return Ok(());
    }
    copy_recursive(src, dest)?;
    remove_path(src)
}

fn remove_path(p: &Path) -> std::io::Result<()> {
    if p.is_dir() {
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
