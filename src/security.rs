//! Shared boundaries for private state, network requests and untrusted input.
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static NETWORK_ICONS: AtomicBool = AtomicBool::new(false);
static SEARCH_HISTORY: AtomicBool = AtomicBool::new(false);
static CLIPBOARD_CAPTURE: AtomicBool = AtomicBool::new(false);

pub fn set_preferences(history: bool, icons: bool, clipboard: bool) {
    SEARCH_HISTORY.store(history, Ordering::Relaxed);
    NETWORK_ICONS.store(icons, Ordering::Relaxed);
    CLIPBOARD_CAPTURE.store(clipboard, Ordering::Relaxed);
}
pub fn network_icons_enabled() -> bool { NETWORK_ICONS.load(Ordering::Relaxed) }
pub fn search_history_enabled() -> bool { SEARCH_HISTORY.load(Ordering::Relaxed) }
pub fn clipboard_capture_enabled() -> bool { CLIPBOARD_CAPTURE.load(Ordering::Relaxed) }


/// Only Spotty's directory and its descendants are tightened; never chmod
/// the user's home, XDG config/cache roots or the shared temporary directory.
pub fn private_dir(path: &Path) -> io::Result<()> {
    let root = path.ancestors().filter(|p| p.file_name().is_some_and(|n| n == "spotty"))
        .last().unwrap_or(path);
    let mut dir = root.to_path_buf();
    secure_directory(&dir)?;
    for component in path.strip_prefix(root).map_err(io::Error::other)?.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return Err(io::Error::other("invalid private directory path"));
        }
        dir.push(component);
        secure_directory(&dir)?;
    }
    Ok(())
}

fn secure_directory(path: &Path) -> io::Result<()> {
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {},
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {},
        Err(e) => return Err(e),
    }
    let dir = OpenOptions::new().read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW).open(path)?;
    if dir.metadata()?.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::other("private directory belongs to another user"));
    }
    dir.set_permissions(fs::Permissions::from_mode(0o700))
}

pub fn protect_state_dirs() -> io::Result<()> {
    for root in [dirs::config_dir(), dirs::cache_dir()].into_iter().flatten() {
        // XDG roots may not exist on a fresh account; these retain normal
        // user permissions. Only the Spotty subdirectory is made private.
        fs::create_dir_all(&root)?;
        private_dir(&root.join("spotty"))?;
    }
    Ok(())
}

/// Create with mode 0600 before writing, then atomically replace the name.
/// create_new rejects pre-existing files/symlinks; rename never follows the
/// destination symlink. A unique temporary name permits concurrent writers.
pub fn write_private(path: impl AsRef<Path>, data: impl AsRef<[u8]>) -> io::Result<()> {
    let path = path.as_ref();
    let parent = path.parent().ok_or_else(|| io::Error::other("missing parent"))?;
    private_dir(parent)?;
    let tmp = parent.join(format!(".spotty-write-{}-{}", std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)));
    let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
    let result = file.write_all(data.as_ref()).and_then(|_| file.sync_all())
        .and_then(|_| fs::rename(&tmp, path));
    if result.is_err() { let _ = fs::remove_file(&tmp); }
    result
}

pub struct PrivateTempDir(pub PathBuf);
impl PrivateTempDir {
    pub fn new(label: &str) -> io::Result<Self> {
        let parent = dirs::cache_dir().ok_or_else(|| io::Error::other("no cache directory"))?
            .join("spotty/tmp");
        private_dir(&parent)?;
        let path = parent.join(format!("{label}-{}-{}", std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)));
        DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }
}
impl Drop for PrivateTempDir {
    fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id != "." && id != ".."
        && id.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
}

/// Browser actions allow HTTP(S), never file:, javascript: or custom handlers.
pub fn http_uri(url: &str) -> Result<glib::Uri, String> {
    if url.len() > 8192 || url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("Invalid URL".into());
    }
    let uri = glib::Uri::parse(url, glib::UriFlags::NONE).map_err(|e| e.to_string())?;
    let scheme = uri.scheme();
    if !matches!(scheme.as_str(), "http" | "https")
        || uri.host().is_none_or(|h| h.is_empty()) || uri.userinfo().is_some() {
        return Err("Only HTTP(S) URLs without embedded credentials are allowed".into());
    }
    Ok(uri)
}

/// Network fetches need TLS, except explicitly local services such as
/// LibreTranslate. HTTPS redirects cannot downgrade to plaintext HTTP.
pub fn curl_request(url: &str, body: Option<&[u8]>, limit: usize, seconds: u64,
                    fail_http: bool) -> io::Result<Output> {
    let uri = http_uri(url).map_err(io::Error::other)?;
    let secure = uri.scheme().as_str() == "https";
    let host = uri.host().unwrap();
    if !secure && !matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1" | "[::1]") {
        return Err(io::Error::other("Remote services require HTTPS"));
    }
    if body.is_some_and(|b| b.len() > 1_048_576) {
        return Err(io::Error::other("Request exceeds 1 MiB"));
    }
    let mut cmd = crate::app::host_process("curl");
    // -q must be first: ignore curlrc overrides of TLS, proxy or output options.
    cmd.args(["-q", "-sS", "--proto", "=http,https", "--proto-redir", "=https",
              "--max-redirs", "5", "--connect-timeout", "4", "--max-time", &seconds.to_string(),
              "--max-filesize", &limit.to_string(), "--url", url]);
    if secure && body.is_none() { cmd.arg("-L"); }
    if fail_http { cmd.arg("-f"); }
    if body.is_some() {
        cmd.args(["-H", "Content-Type: application/json", "--data-binary", "@-"]);
    }
    let mut child = cmd.stdin(if body.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
    // Send sensitive data over stdin, never in the shell or process argv.
    let writer = body.map(|bytes| {
        let bytes = bytes.to_vec();
        let mut stdin = child.stdin.take().unwrap();
        std::thread::spawn(move || stdin.write_all(&bytes))
    });
    let mut stdout = Vec::new();
    let read = child.stdout.take().unwrap().take(limit as u64 + 1).read_to_end(&mut stdout);
    if read.is_err() || stdout.len() > limit {
        let _ = child.kill();
        let _ = child.wait();
        if let Some(writer) = writer { let _ = writer.join(); }
        return Err(io::Error::other("Response exceeds size limit or could not be read"));
    }
    let status = child.wait()?;
    if let Some(writer) = writer { let _ = writer.join(); }
    Ok(Output { status, stdout, stderr: Vec::new() })
}
