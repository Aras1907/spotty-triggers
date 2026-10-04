use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "lowercase")]
pub enum ClipboardEntry {
    Text(String),
    /// A copied image, saved to a cache file. Stores the path to the PNG.
    Image(PathBuf),
    /// A copied file or folder (the actual filesystem item). Stores its path so
    /// the clipboard manager can show it with a type/thumbnail preview.
    File(PathBuf),
}

pub struct ClipboardHistory {
    entries: Vec<ClipboardEntry>,
    times: Vec<u64>,
    cap: usize,
    suppressed_text: Vec<String>,
    suppressed_images: Vec<PathBuf>,
    suppressed_files: Vec<PathBuf>,
    /// The most recently removed entry (and its original position), kept
    /// around briefly so a deletion can be undone (Ctrl+Z / Undo).
    last_removed: Option<(usize, ClipboardEntry)>,
    /// The most recently unpinned clipboard item (and its kind), for undo support.
    last_unpinned: Option<(ClipboardEntryKind, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ClipboardEntryKind {
    Text,
    Image,
    File,
}

impl ClipboardHistory {
    pub fn new(cap: usize) -> Self {
        Self {
            entries: Vec::with_capacity(cap),
            times: Vec::with_capacity(cap),
            cap,
            suppressed_text: Vec::new(),
            suppressed_images: Vec::new(),
            suppressed_files: Vec::new(),
            last_removed: None,
            last_unpinned: None,
        }
    }

    /// Build a minimal history for search purposes (worker threads).
    pub fn from_entries(entries: Vec<ClipboardEntry>) -> Self {
        let cap = entries.len().max(100);
        let now = unix_now();
        let times = vec![now; entries.len()];
        Self {
            entries,
            times,
            cap,
            suppressed_text: Vec::new(),
            suppressed_images: Vec::new(),
            suppressed_files: Vec::new(),
            last_removed: None,
            last_unpinned: None,
        }
    }

    /// Build a history and restore the persisted entries from disk.
    pub fn load(cap: usize, retention_days: Option<u64>) -> Self {
        let mut h = Self::new(cap);
        h.load_from(&history_path(), retention_days);
        h
    }

    /// Restore entries previously written by [`save`].  Image entries whose
    /// cache file no longer exists are dropped silently; a missing or corrupt
    /// file simply yields an empty history.
    fn load_from(&mut self, path: &std::path::Path, retention_days: Option<u64>) {
        let Ok(data) = std::fs::read_to_string(path) else {
            log::info!("clipboard: no history file found");
            return;
        };

        // Try v2 envelope format first: { version, entries, times }
        if let Ok(v2) = serde_json::from_str::<serde_json::Value>(&data) {
            if let (Some(ver), Some(entries_val)) = (v2.get("version"), v2.get("entries")) {
                if ver.as_u64() == Some(2) {
                    if let Ok(entries) = serde_json::from_value::<Vec<ClipboardEntry>>(entries_val.clone()) {
                        let times = v2.get("times")
                            .and_then(|t| serde_json::from_value::<Vec<u64>>(t.clone()).ok())
                            .map(|mut v| {
                                v.resize(entries.len(), unix_now());
                                v
                            })
                            .unwrap_or_else(|| vec![unix_now(); entries.len()]);
                        let cap = self.cap;
                        self.entries = entries;
                        self.times = times;
                        // Align times to entries length
                        self.times.resize(self.entries.len(), unix_now());
                        if self.entries.len() > cap {
                            self.entries.truncate(cap);
                            self.times.truncate(cap);
                        }
                        self.apply_retention(retention_days);
                        log::info!("clipboard: restored {} history entries (v2)", self.entries.len());
                        return;
                    }
                }
            }
        }

        // Fallback: legacy format (plain Vec<ClipboardEntry>)
        match serde_json::from_str::<Vec<ClipboardEntry>>(&data) {
            Ok(entries) => {
                let now = unix_now();
                self.entries = entries;
                self.times = vec![now; self.entries.len()];
                if self.entries.len() > self.cap {
                    self.entries.truncate(self.cap);
                    self.times.truncate(self.cap);
                }
                self.apply_retention(retention_days);
                log::info!("clipboard: restored {} history entries (migrated to v2)", self.entries.len());
            }
            Err(e) => log::warn!("clipboard: could not parse history file: {e}"),
        }
    }

    /// Persist the current entries to disk.  Called after every mutation so
    /// the on-disk copy is always current (survives restarts, updates and
    /// shutdowns).
    fn save(&self) {
        self.save_to(&history_path());
    }

    fn save_to(&self, path: &std::path::Path) {
        if let Some(dir) = path.parent() {
            let _ = crate::security::private_dir(dir);
        }
        // v2 envelope: { version, entries, times }
        let envelope = serde_json::json!({
            "version": 2,
            "entries": &self.entries,
            "times": &self.times,
        });
        let Ok(data) = serde_json::to_vec(&envelope) else {
            log::warn!("clipboard: could not serialize history");
            return;
        };
        if let Err(e) = crate::security::write_private(path, &data) {
            log::warn!("clipboard: could not persist history: {e}");
        }
    }

    pub fn entries(&self) -> &[ClipboardEntry] {
        &self.entries
    }

    /// When the given entry was captured (unix seconds) — as long as it is
    /// still in the history. Used for the "Clipped …" preview caption.
    pub fn time_of(&self, entry: &ClipboardEntry) -> Option<u64> {
        self.entries
            .iter()
            .position(|e| e == entry)
            .and_then(|i| self.times.get(i).copied())
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn remove_text(&mut self, text: &str) {
        if let Some(pos) = self
            .entries
            .iter()
            .position(|e| matches!(e, ClipboardEntry::Text(t) if t == text))
        {
            let e = self.entries.remove(pos);
            let _t = self.times.remove(pos);
            self.last_removed = Some((pos, e));
        }
        remember_suppressed(&mut self.suppressed_text, text.to_string());
        self.save();
    }

    /// Remove an image entry by its cache path (and delete the file).
    pub fn remove_image(&mut self, path: &std::path::Path) {
        if let Some(pos) = self
            .entries
            .iter()
            .position(|e| matches!(e, ClipboardEntry::Image(p) if p == path))
        {
            let e = self.entries.remove(pos);
            let _t = self.times.remove(pos);
            self.last_removed = Some((pos, e));
        }
        remember_suppressed(&mut self.suppressed_images, path.to_path_buf());
        self.save();
    }

    pub fn push_text(&mut self, t: String) {
        if !crate::security::clipboard_capture_enabled() { return; }
        if self.suppressed_text.iter().any(|s| s == &t) {
            return;
        }
        if let Some(p) = self
            .entries
            .iter()
            .position(|e| matches!(e, ClipboardEntry::Text(s) if s == &t))
        {
            let e = self.entries.remove(p);
            let _t = self.times.remove(p);
            self.entries.insert(0, e);
            self.times.insert(0, unix_now());
            self.save();
            return;
        }
        if t.trim().is_empty() || t.len() > 1_000_000 {
            return;
        }
        self.entries.insert(0, ClipboardEntry::Text(t));
        self.times.insert(0, unix_now());
        self.trim();
        self.save();
    }

    pub fn push_image(&mut self, path: PathBuf) {
        if !crate::security::clipboard_capture_enabled() { return; }
        if self.suppressed_images.iter().any(|p| p == &path) {
            return;
        }
        // Avoid duplicates by exact path (each capture is a new file, so this
        // mostly guards re-insertion of the same file)
        if self
            .entries
            .iter()
            .any(|e| matches!(e, ClipboardEntry::Image(p) if p == &path))
        {
            return;
        }
        self.entries.insert(0, ClipboardEntry::Image(path));
        self.times.insert(0, unix_now());
        self.trim();
        self.save();
    }

    /// Record a copied file/folder so it shows in the clipboard manager.
    pub fn push_file(&mut self, path: PathBuf) {
        if !crate::security::clipboard_capture_enabled() { return; }
        if self.suppressed_files.iter().any(|p| p == &path) {
            return;
        }
        // Move an existing entry for the same path to the front.
        if let Some(p) = self
            .entries
            .iter()
            .position(|e| matches!(e, ClipboardEntry::File(x) if x == &path))
        {
            let e = self.entries.remove(p);
            let _t = self.times.remove(p);
            self.entries.insert(0, e);
            self.times.insert(0, unix_now());
            self.save();
            return;
        }
        self.entries.insert(0, ClipboardEntry::File(path));
        self.times.insert(0, unix_now());
        self.trim();
        self.save();
    }

    /// Remove a file entry by its path (does NOT delete the actual file).
    pub fn remove_file(&mut self, path: &std::path::Path) {
        if let Some(pos) = self
            .entries
            .iter()
            .position(|e| matches!(e, ClipboardEntry::File(p) if p == path))
        {
            let e = self.entries.remove(pos);
            let _t = self.times.remove(pos);
            self.last_removed = Some((pos, e));
        }
        remember_suppressed(&mut self.suppressed_files, path.to_path_buf());
        self.save();
    }

    /// Restore the most recently removed entry (Ctrl+Z / Undo). Returns
    /// `true` if something was restored.
    pub fn undo_remove(&mut self) -> bool {
        let Some((pos, entry)) = self.last_removed.take() else {
            return false;
        };
        match &entry {
            ClipboardEntry::Text(t) => self.suppressed_text.retain(|s| s != t),
            ClipboardEntry::Image(p) => self.suppressed_images.retain(|s| s != p),
            ClipboardEntry::File(p) => self.suppressed_files.retain(|s| s != p),
        }
        let pos = pos.min(self.entries.len());
        self.entries.insert(pos, entry);
        self.times.insert(pos, unix_now());
        self.save();
        true
    }

    /// Record that an item was unpinned (for undo support).
    pub fn record_unpinned(&mut self, kind: ClipboardEntryKind, key: String) {
        self.last_unpinned = Some((kind, key));
    }

    /// Get the last unpinned item and clear it. Used to undo an accidental unpin.
    pub fn get_and_clear_last_unpinned(&mut self) -> Option<(ClipboardEntryKind, String)> {
        self.last_unpinned.take()
    }

    fn apply_retention(&mut self, retention_days: Option<u64>) {
        let Some(days) = retention_days else { return; };
        if days == 0 { return; }
        let cutoff = unix_now().saturating_sub(days * 86400);
        let mut i = self.entries.len();
        while i > 0 {
            i -= 1;
            if self.times.get(i).copied().unwrap_or(u64::MAX) < cutoff {
                let e = self.entries.remove(i);
                self.times.remove(i);
                if let ClipboardEntry::Image(p) = e {
                    let _ = std::fs::remove_file(p);
                }
            }
        }
    }

    fn trim(&mut self) {
        if self.entries.len() > self.cap {
            // Delete cache files for any image entries we drop
            for e in self.entries.drain(self.cap..) {
                if let ClipboardEntry::Image(p) = e {
                    let _ = std::fs::remove_file(p);
                }
            }
            self.times.truncate(self.cap);
        }
    }

    pub fn start_watching(&mut self) {
        let Some(d) = gdk::Display::default() else {
            return;
        };
        let cb = d.clipboard();
        // Track last seen content so we don't re-add the same clipboard entry
        // on every change. For text we keep the literal string; for images we keep
        // a SHA-style content hash of the PNG bytes. For files we track the full
        // URI list content as a hash.
        let last_text: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let last_img_hash: Rc<RefCell<Option<u64>>> = Rc::new(RefCell::new(None));
        let last_file_hash: Rc<RefCell<Option<u64>>> = Rc::new(RefCell::new(None));

        log::info!("clipboard: starting to watch for changes");
        read_current_clipboard(&cb, &last_text, &last_img_hash, &last_file_hash);

        // Connect to the changed signal for real-time updates when window is active
        let cb_signal = cb.clone();
        let last_text_signal = last_text.clone();
        let last_img_signal = last_img_hash.clone();
        let last_file_signal = last_file_hash.clone();
        cb.connect_changed(move |_| {
            log::debug!("clipboard: change signal fired");
            read_current_clipboard(
                &cb_signal,
                &last_text_signal,
                &last_img_signal,
                &last_file_signal,
            );
        });

        // ALSO: poll the clipboard periodically (every 250ms) to catch changes even
        // when the window is hidden and the changed signal doesn't fire reliably.
        // This ensures we never miss a copy, even after long idle periods.
        let cb_poll = cb.clone();
        let last_text_poll = last_text.clone();
        let last_img_poll = last_img_hash.clone();
        let last_file_poll = last_file_hash.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
            log::debug!("clipboard: polling timer tick");
            read_current_clipboard(&cb_poll, &last_text_poll, &last_img_poll, &last_file_poll);
            glib::ControlFlow::Continue
        });
    }

    pub fn refresh_now(&mut self) {
        let Some(d) = gdk::Display::default() else {
            return;
        };
        let cb = d.clipboard();
        let last_text: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let last_img_hash: Rc<RefCell<Option<u64>>> = Rc::new(RefCell::new(None));
        let last_file_hash: Rc<RefCell<Option<u64>>> = Rc::new(RefCell::new(None));
        read_current_clipboard(&cb, &last_text, &last_img_hash, &last_file_hash);
    }
}

fn remember_suppressed<T: PartialEq>(items: &mut Vec<T>, item: T) {
    if !items.iter().any(|existing| existing == &item) {
        items.insert(0, item);
    }
    items.truncate(100);
}

fn read_current_clipboard(
    cb: &gdk::Clipboard,
    last_text: &Rc<RefCell<Option<String>>>,
    last_img_hash: &Rc<RefCell<Option<u64>>>,
    last_file_hash: &Rc<RefCell<Option<u64>>>,
) {
    if !crate::security::clipboard_capture_enabled() { return; }
    log::debug!("clipboard: read_current_clipboard called");
    let formats = cb.formats();

    // Image first so a copied image is not shadowed by text/uri-list.
    let has_image = formats.contains_type(gdk::Texture::static_type())
        || formats.contain_mime_type("image/png")
        || formats.contain_mime_type("image/jpeg")
        || formats.contain_mime_type("image/bmp")
        || formats.contain_mime_type("image/webp");
    if has_image {
        let last_h = last_img_hash.clone();
        cb.read_texture_async(gio::Cancellable::NONE, move |res| {
            match res {
                Ok(Some(texture)) => {
                    // Compare raw pixels: on almost every poll the image is
                    // the one already captured, and encoding it to PNG just
                    // to find that out was the costly part.
                    let hash = texture_hash(&texture);
                    let mut prev = last_h.borrow_mut();
                    if *prev == Some(hash) {
                        return;
                    }
                    log::info!("clipboard: new image captured, adding to history");
                    *prev = Some(hash);
                    let png_bytes = texture.save_to_png_bytes();
                    if let Some(path) = save_png_bytes(&png_bytes) {
                        crate::app::with_state(|st| st.clipboard.borrow_mut().push_image(path.clone()));
                        crate::app::refresh_search_window();
                        // OCR the image in background so its text becomes searchable
                        if crate::ocr::is_available() {
                            std::thread::spawn(move || {
                                crate::ocr::text_for(&path);
                                glib::idle_add_once(crate::app::refresh_search_window);
                            });
                        }
                    }
                }
                Ok(None) => {
                    log::debug!("clipboard: texture is empty");
                    // Reset hash so we capture the next image
                    let mut prev = last_h.borrow_mut();
                    if prev.is_some() {
                        log::debug!("clipboard: clearing image tracker for next capture");
                        *prev = None;
                    }
                }
                Err(_e) => {
                    log::debug!("clipboard: image read error (will retry)");
                }
            }
        });
    }

    // Check for files/folders (URIs) in the clipboard. text/uri-list is the
    // standard MIME type for copied files (from file managers, etc.).
    // Skipped while we own the clipboard (is_local): re-reading our own URI
    // list would add bogus File entries for images/files we set ourselves,
    // and a stream read on a local provider needs the main context, which
    // deadlocked the GTK main loop here (Enter on a clipboard image froze
    // the app: the poll timer and the SIGUSR1 toggle timer never ran again).
    let has_files = !cb.is_local()
        && (formats.contain_mime_type("text/uri-list")
            || formats.contain_mime_type("x-special/gnome-copied-files"));
    if has_files {
        let last_fh = last_file_hash.clone();
        cb.read_async(
            &["text/uri-list"],
            glib::Priority::DEFAULT,
            gio::Cancellable::NONE,
            move |res| {
                match res {
                    Ok((stream, _)) => {
                        // Read asynchronously: a blocking read on this thread
                        // can stall the main loop when the provider needs the
                        // main context to deliver data (local providers always
                        // do, and slow foreign ones may).
                        read_uri_stream(
                            stream,
                            Rc::new(RefCell::new(Vec::new())),
                            last_fh,
                        );
                    }
                    Err(_e) => {
                        log::debug!("clipboard: file URI read error (will retry)");
                    }
                }
            },
        );
    }

    let last = last_text.clone();
    cb.read_text_async(gio::Cancellable::NONE, move |res| {
        match res {
            Ok(Some(txt)) => {
                let txt = txt.to_string();
                log::debug!("clipboard: read text ({} bytes)", txt.len());
                let mut prev = last.borrow_mut();
                if prev.as_deref() != Some(&txt) {
                    log::info!("clipboard: new text captured, adding to history");
                    *prev = Some(txt.clone());
                    crate::app::with_state(|st| st.clipboard.borrow_mut().push_text(txt));
                    crate::app::refresh_search_window();
                }
            }
            Ok(None) => {
                log::debug!("clipboard: text is empty");
                // Reset the last seen text so we capture the next non-empty text
                let mut prev = last.borrow_mut();
                if prev.is_some() {
                    log::debug!("clipboard: clearing text tracker for next capture");
                    *prev = None;
                }
            }
            Err(e) => {
                log::debug!("clipboard: text read error (will retry): {:?}", e);
                // Don't log errors aggressively; just silently retry on next poll
            }
        }
    });
}

/// Upper bound for a collected text/uri-list payload. A URI list of a few
/// hundred files fits comfortably; anything larger is truncated.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

const MAX_URI_LIST_BYTES: usize = 65_536;

/// Recursively read a clipboard URI-list stream without ever blocking the
/// main loop, then hand the collected payload to [`finish_file_uri_read`].
fn read_uri_stream(
    stream: gio::InputStream,
    collected: Rc<RefCell<Vec<u8>>>,
    last_fh: Rc<RefCell<Option<u64>>>,
) {
    let stream_next = stream.clone();
    let collected_next = collected.clone();
    stream.read_bytes_async(
        4096,
        glib::Priority::DEFAULT,
        gio::Cancellable::NONE,
        move |res| match res {
            Ok(bytes) => {
                if bytes.is_empty() {
                    // EOF: process what we have.
                    let data: Vec<u8> = std::mem::take(&mut *collected.borrow_mut());
                    if data.is_empty() {
                        log::debug!("clipboard: no file URI data");
                        // Reset so we capture next file copy
                        let mut prev = last_fh.borrow_mut();
                        if prev.is_some() {
                            *prev = None;
                        }
                        return;
                    }
                    match String::from_utf8(data) {
                        Ok(text) => {
                            log::debug!("clipboard: read file URIs ({} bytes)", text.len());
                            finish_file_uri_read(text, &last_fh);
                        }
                        Err(_) => {
                            log::debug!("clipboard: invalid UTF-8 in file URI data");
                        }
                    }
                    return;
                }
                {
                    let mut buf = collected.borrow_mut();
                    let room = MAX_URI_LIST_BYTES.saturating_sub(buf.len());
                    let take = room.min(bytes.len());
                    buf.extend_from_slice(&bytes[..take]);
                    if take < bytes.len() {
                        // Over the cap: stop reading, process what we have.
                        drop(buf);
                        let data: Vec<u8> = std::mem::take(&mut *collected.borrow_mut());
                        if let Ok(text) = String::from_utf8(data) {
                            finish_file_uri_read(text, &last_fh);
                        }
                        return;
                    }
                }
                read_uri_stream(stream_next, collected_next, last_fh);
            }
            Err(_e) => {
                log::debug!("clipboard: file URI read error (will retry)");
            }
        },
    );
}

/// Handle the fully-collected file-URI payload: skip unchanged content, and
/// record any copied files/folders in the history.
fn finish_file_uri_read(text: String, last_fh: &Rc<RefCell<Option<u64>>>) {
    // Hash the full URI list content to detect actual changes
    let content_hash = quick_hash(text.as_bytes());
    let mut prev = last_fh.borrow_mut();
    if *prev == Some(content_hash) {
        return;
    }
    log::info!("clipboard: new files/folders captured, parsing");
    *prev = Some(content_hash);

    for path in parse_uri_list(&text) {
        log::debug!("clipboard: adding file to history");
        crate::app::with_state(|st| st.clipboard.borrow_mut().push_file(path));
        crate::app::refresh_search_window();
    }
}

/// Parse a text/uri-list payload into local paths, ignoring blanks and `#`
/// comments and dropping entries that no longer exist.
fn parse_uri_list(text: &str) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Convert "file://..." URIs to paths
        if let Some(path) = uri_to_path(line) {
            if path.exists() {
                out.push(path);
            }
        }
    }
    out
}

/// Convert a file:// URI to a path, handling URL encoding.
fn uri_to_path(uri: &str) -> Option<std::path::PathBuf> {
    let f = gio::File::for_uri(uri);
    f.path().map(|p| p.to_path_buf())
}

/// FNV-1a 64-bit hash. Good enough to distinguish two PNG payloads cheaply.
/// Content hash of a texture's pixels (and size), for change detection only.
fn texture_hash(texture: &gdk::Texture) -> u64 {
    use std::hash::{Hash, Hasher};
    let (w, h) = (texture.width(), texture.height());
    let stride = w as usize * 4;
    let mut pixels = vec![0u8; stride * h as usize];
    texture.download(&mut pixels, stride);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (w, h).hash(&mut hasher);
    pixels.hash(&mut hasher);
    hasher.finish()
}

fn quick_hash(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Save raw PNG bytes to a uniquely-named cache file.
fn save_png_bytes(bytes: &[u8]) -> Option<PathBuf> {
    let dir = cache_dir();
    crate::security::private_dir(&dir).ok()?;
    let path = dir.join(format!("clip-{:016x}.png", quick_hash(bytes)));
    if path.is_file() || crate::security::write_private(&path, bytes).is_ok() {
        Some(path)
    } else {
        None
    }
}

pub fn cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| "/tmp".into())
        .join("spotty/clipboard-images")
}

/// Persistent clipboard history file (kept next to the other Spotty state).
fn history_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("spotty/clipboard_history.json")
}

pub fn set_text(t: &str) {
    if let Some(d) = gdk::Display::default() {
        d.clipboard().set_text(t);
    }
}

/// Copy a FILE or FOLDER onto the clipboard so file managers can paste it.
/// Provides multiple MIME types so different paste targets work:
///   - text/uri-list: standard URI list (most file managers)
///   - x-special/gnome-copied-files: Nautilus-specific format with "copy" action
///   - text/plain: the path as fallback when pasting into text fields
pub fn set_file(path: &std::path::Path) {
    let Some(d) = gdk::Display::default() else {
        return;
    };
    let cb = d.clipboard();

    // Build a file:// URI from the absolute path.
    let abs = match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(_) => path.to_path_buf(),
    };
    let uri = gio::File::for_path(&abs).uri().to_string();
    let path_str = abs.display().to_string();

    // text/uri-list payload
    let uri_list = format!("{}\r\n", uri);
    // Nautilus-style "copy" payload
    let gnome_copied = format!("copy\n{}", uri);

    let uri_bytes = glib::Bytes::from(uri_list.as_bytes());
    let gnome_bytes = glib::Bytes::from(gnome_copied.as_bytes());
    let text_bytes = glib::Bytes::from(path_str.as_bytes());

    let uri_provider = gdk::ContentProvider::for_bytes("text/uri-list", &uri_bytes);
    let gnome_provider =
        gdk::ContentProvider::for_bytes("x-special/gnome-copied-files", &gnome_bytes);
    let text_provider = gdk::ContentProvider::for_bytes("text/plain;charset=utf-8", &text_bytes);

    // Combine all three so any paste target finds a usable format.
    let union = gdk::ContentProvider::new_union(&[uri_provider, gnome_provider, text_provider]);
    let _ = cb.set_content(Some(&union));
}

/// Re-copy an image file back onto the clipboard. We provide BOTH a GdkTexture
/// (so image editors can paste the pixels) AND a uri-list (so file managers
/// can paste the file). This way a copied image works wherever you paste it.
pub fn set_image(path: &std::path::Path) {
    let Some(d) = gdk::Display::default() else {
        return;
    };
    let cb = d.clipboard();

    let mut providers: Vec<gdk::ContentProvider> = Vec::new();

    // Texture provider for image-pixel paste targets
    if let Ok(texture) = gdk::Texture::from_filename(path) {
        let p = gdk::ContentProvider::for_value(&texture.to_value());
        providers.push(p);
    }

    // URI list so file managers can paste the file
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let uri = gio::File::for_path(&abs).uri().to_string();
    let uri_list = format!("{}\r\n", uri);
    let gnome_copied = format!("copy\n{}", uri);

    providers.push(gdk::ContentProvider::for_bytes(
        "text/uri-list",
        &glib::Bytes::from(uri_list.as_bytes()),
    ));
    providers.push(gdk::ContentProvider::for_bytes(
        "x-special/gnome-copied-files",
        &glib::Bytes::from(gnome_copied.as_bytes()),
    ));

    if providers.is_empty() {
        return;
    }
    let union = gdk::ContentProvider::new_union(&providers);
    let _ = cb.set_content(Some(&union));
}

#[cfg(test)]
mod tests {

    fn solid_texture(w: i32, h: i32, px: [u8; 4]) -> gdk::Texture {
        let data: Vec<u8> = px.iter().copied().cycle().take((w * h * 4) as usize).collect();
        gdk::MemoryTexture::new(
            w,
            h,
            gdk::MemoryFormat::R8g8b8a8,
            &glib::Bytes::from_owned(data),
            (w * 4) as usize,
        )
        .upcast()
    }

    #[test]
    fn texture_hash_tells_images_apart() {
        let a = solid_texture(4, 3, [1, 2, 3, 255]);
        assert_eq!(super::texture_hash(&a), super::texture_hash(&solid_texture(4, 3, [1, 2, 3, 255])));
        assert_ne!(super::texture_hash(&a), super::texture_hash(&solid_texture(4, 3, [1, 2, 4, 255])));
        // Same pixel bytes, different shape.
        assert_ne!(super::texture_hash(&a), super::texture_hash(&solid_texture(3, 4, [1, 2, 3, 255])));
    }

    use super::*;

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!(
            "spotty-clip-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    #[test]
    fn time_of_reports_when_an_entry_was_clipped() {
        let mut h = ClipboardHistory::new(10);
        h.entries.push(ClipboardEntry::Text("hello".into()));
        h.entries.push(ClipboardEntry::Text("second".into()));
        h.times = vec![1_000, 2_000];

        assert_eq!(
            h.time_of(&ClipboardEntry::Text("hello".into())),
            Some(1_000)
        );
        assert_eq!(
            h.time_of(&ClipboardEntry::Text("second".into())),
            Some(2_000)
        );
        assert_eq!(h.time_of(&ClipboardEntry::Text("gone".into())), None);
        assert_eq!(
            h.time_of(&ClipboardEntry::File(PathBuf::from("/tmp/x"))),
            None
        );
    }

    #[test]
    fn history_roundtrip_preserves_entries_and_order() {
        let dir = temp_dir();
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("clipboard_history.json");

        let mut h = ClipboardHistory::new(100);
        h.entries.push(ClipboardEntry::Text("hello".into()));
        h.entries
            .push(ClipboardEntry::File(PathBuf::from("/tmp/some-file")));
        h.times = vec![unix_now(); 2];
        h.save_to(&path);

        let mut restored = ClipboardHistory::new(100);
        restored.load_from(&path, None);
        assert_eq!(restored.entries().len(), 2);
        assert!(matches!(&restored.entries()[0], ClipboardEntry::Text(t) if t == "hello"));
        assert!(
            matches!(&restored.entries()[1], ClipboardEntry::File(p) if p == &PathBuf::from("/tmp/some-file"))
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_image_files_are_kept_on_load() {
        let dir = temp_dir();
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("clipboard_history.json");

        let mut h = ClipboardHistory::new(100);
        h.entries
            .push(ClipboardEntry::Image(PathBuf::from("/nonexistent/xyz.png")));
        h.times = vec![unix_now()];
        h.save_to(&path);

        let mut restored = ClipboardHistory::new(100);
        restored.load_from(&path, None);
        assert_eq!(restored.entries().len(), 1, "missing image entries should be kept");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_file_yields_empty_history() {
        let mut h = ClipboardHistory::new(100);
        h.load_from(std::path::Path::new("/nonexistent/spotty-history.json"), None);
        assert!(h.entries().is_empty());
    }

    #[test]
    fn parse_uri_list_keeps_existing_files() {
        let dir = temp_dir();
        let _ = std::fs::create_dir_all(&dir);
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"y").unwrap();
        let payload = format!(
            "{}\r\n{}\r\n",
            gio::File::for_path(&a).uri(),
            gio::File::for_path(&b).uri()
        );
        let mut got = parse_uri_list(&payload);
        got.sort();
        assert_eq!(got, vec![a, b]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_uri_list_ignores_comments_blanks_and_missing() {
        let dir = temp_dir();
        let _ = std::fs::create_dir_all(&dir);
        let a = dir.join("keep.txt");
        std::fs::write(&a, b"x").unwrap();
        let payload = format!(
            "# a comment\r\n\r\n   \r\n{}\r\nfile:///nonexistent/spotty-gone.txt\r\n",
            gio::File::for_path(&a).uri()
        );
        assert_eq!(parse_uri_list(&payload), vec![a]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_uri_list_decodes_percent_encoding() {
        let dir = temp_dir();
        let _ = std::fs::create_dir_all(&dir);
        let spaced = dir.join("my file.txt");
        std::fs::write(&spaced, b"x").unwrap();
        let payload = format!("{}\r\n", gio::File::for_path(&spaced).uri());
        assert!(payload.contains("%20"));
        assert_eq!(parse_uri_list(&payload), vec![spaced]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
