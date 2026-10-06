//! Owns the in-process Proton Bridge Go runtime for this Spotty process.
//!
//! The Go shared library is intentionally left mapped until process exit. Go
//! runtimes are not safe to unload with `dlclose` while their goroutines or
//! runtime threads may still exist.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

type StartFn = unsafe extern "C" fn(*const c_char) -> c_int;
type StopFn = unsafe extern "C" fn() -> c_int;
type StatusFn = unsafe extern "C" fn() -> c_int;
type VersionFn = unsafe extern "C" fn() -> *const c_char;
type LastErrorFn = unsafe extern "C" fn() -> *mut c_char;
type FreeFn = unsafe extern "C" fn(*mut c_char);

#[derive(Clone, Copy)]
struct Api {
    #[allow(dead_code)] // Retains the dlopen reference for the process lifetime.
    handle: usize,
    start: StartFn,
    stop: StopFn,
    status: StatusFn,
    version: VersionFn,
    last_error: LastErrorFn,
    free: FreeFn,
}

static API: OnceLock<Result<Api, String>> = OnceLock::new();
static INITIALIZING: AtomicBool = AtomicBool::new(false);
static INITIALIZED: AtomicBool = AtomicBool::new(false);
static RUNNING: AtomicBool = AtomicBool::new(false);
static SERVICE_REQUESTED: AtomicBool = AtomicBool::new(false);
static SERVICE_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static ENGINE_LOCK: Mutex<()> = Mutex::new(());

#[link(name = "dl")]
unsafe extern "C" {
    fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlerror() -> *const c_char;
}

const RTLD_NOW: c_int = 2;
const RTLD_LOCAL: c_int = 0;

fn api() -> Result<Api, String> {
    API.get_or_init(load_api).clone()
}

fn load_api() -> Result<Api, String> {
    let library = crate::bundle::inprocess_library()?;
    let path = CString::new(library.to_string_lossy().as_bytes())
        .map_err(|_| "The Bridge library path contains an invalid character.")?;
    // Keep the Go library mapped for the rest of the process lifetime.
    let handle = unsafe { dlopen(path.as_ptr(), RTLD_NOW | RTLD_LOCAL) };
    if handle.is_null() {
        return Err(format!(
            "Cannot load the in-process Bridge runtime: {}",
            loader_error()
        ));
    }
    unsafe fn symbol<T: Copy>(handle: *mut c_void, name: &'static [u8]) -> Result<T, String> {
        let pointer = unsafe { dlsym(handle, name.as_ptr().cast()) };
        if pointer.is_null() {
            let detail = loader_error();
            return Err(format!(
                "The in-process Bridge runtime is missing {}: {detail}",
                String::from_utf8_lossy(&name[..name.len() - 1])
            ));
        }
        Ok(unsafe { std::mem::transmute_copy(&pointer) })
    }
    let loaded = unsafe {
        Ok(Api {
            handle: handle as usize,
            start: symbol(handle, b"SpottyBridgeStart\0")?,
            stop: symbol(handle, b"SpottyBridgeStop\0")?,
            status: symbol(handle, b"SpottyBridgeStatus\0")?,
            version: symbol(handle, b"SpottyBridgeVersion\0")?,
            last_error: symbol(handle, b"SpottyBridgeLastError\0")?,
            free: symbol(handle, b"SpottyBridgeFree\0")?,
        })
    };
    loaded
}

fn loader_error() -> String {
    let detail = unsafe { dlerror() };
    if detail.is_null() {
        "unknown dynamic-loader error".into()
    } else {
        unsafe { CStr::from_ptr(detail) }
            .to_string_lossy()
            .into_owned()
    }
}

fn bridge_error(api: Api) -> String {
    let error = unsafe { (api.last_error)() };
    if error.is_null() {
        return "The in-process Bridge runtime reported an unspecified error.".into();
    }
    let message = unsafe { CStr::from_ptr(error) }
        .to_string_lossy()
        .into_owned();
    unsafe { (api.free)(error) };
    if message.trim().is_empty() {
        "The in-process Bridge runtime reported an unspecified error.".into()
    } else {
        message
    }
}

fn valid_status(status: i32) -> bool {
    (0..=2).contains(&status)
}

fn still_requested(generation: u64) -> bool {
    SERVICE_REQUESTED.load(Ordering::Acquire)
        && SERVICE_GENERATION.load(Ordering::Acquire) == generation
}

/// Start or ensure that the Bridge server is running in this process.
pub fn start() -> Result<(), String> {
    SERVICE_REQUESTED.store(true, Ordering::Release);
    start_inner(None)
}

fn start_for_generation(generation: u64) -> Result<(), String> {
    start_inner(Some(generation))
}

fn start_inner(generation: Option<u64>) -> Result<(), String> {
    let generation = generation.unwrap_or_else(|| SERVICE_GENERATION.load(Ordering::Acquire));
    let _guard = ENGINE_LOCK
        .lock()
        .map_err(|_| "Bridge engine lock was poisoned.")?;
    if !still_requested(generation) {
        return Ok(());
    }
    let api = api()?;
    if !still_requested(generation) {
        return Ok(());
    }
    let status = unsafe { (api.status)() };
    if status == 1 {
        RUNNING.store(true, Ordering::Release);
        if !still_requested(generation) {
            RUNNING.store(false, Ordering::Release);
        }
        return Ok(());
    }
    if status < 0 && status != -1 {
        return Err(bridge_error(api));
    }
    if status >= 0 && !valid_status(status) {
        return Err(format!(
            "The in-process Bridge runtime returned unknown state {status}."
        ));
    }
    let launcher = crate::bundle::launcher_argument()?;
    let launcher = CString::new(launcher.to_string_lossy().as_bytes())
        .map_err(|_| "The Bridge launcher path contains an invalid character.")?;
    if !still_requested(generation) {
        return Ok(());
    }
    let result = unsafe { (api.start)(launcher.as_ptr()) };
    if result < 0 {
        return Err(bridge_error(api));
    }
    RUNNING.store(true, Ordering::Release);
    if !still_requested(generation) {
        RUNNING.store(false, Ordering::Release);
        let _ = unsafe { (api.stop)() };
    }
    Ok(())
}

/// Stop the in-process service. It is safe to call during application shutdown.
pub fn stop() -> Result<(), String> {
    SERVICE_REQUESTED.store(false, Ordering::Release);
    SERVICE_GENERATION.fetch_add(1, Ordering::AcqRel);
    INITIALIZED.store(false, Ordering::Release);
    RUNNING.store(false, Ordering::Release);
    let Some(Ok(api)) = API.get() else {
        RUNNING.store(false, Ordering::Release);
        return Ok(());
    };
    let result = unsafe { (api.stop)() };
    if result < 0 {
        return Err(bridge_error(*api));
    }
    RUNNING.store(false, Ordering::Release);
    Ok(())
}

/// Return the adapter's state: 0 stopped, 1 starting/running, 2 stopping.
pub fn status() -> Result<i32, String> {
    let api = api()?;
    let status = unsafe { (api.status)() };
    if status < 0 {
        Err(bridge_error(api))
    } else if valid_status(status) {
        Ok(status)
    } else {
        Err(format!(
            "The in-process Bridge runtime returned unknown state {status}."
        ))
    }
}

/// Version string returned by the in-process adapter.
pub fn version() -> Result<String, String> {
    let api = api()?;
    let version = unsafe { (api.version)() };
    if version.is_null() {
        return Err(bridge_error(api));
    }
    let value = unsafe { CStr::from_ptr(version) }
        .to_string_lossy()
        .into_owned();
    unsafe { (api.free)(version.cast_mut()) };
    Ok(value)
}

/// True while process startup is initializing saved accounts for later login.
pub fn is_initializing() -> bool {
    INITIALIZING.load(Ordering::Acquire)
}

/// True only after this process started its own in-process Bridge server.
pub fn is_running() -> bool {
    RUNNING.load(Ordering::Acquire)
}

/// Start Bridge and initialize its saved-account state without retaining the
/// login stream. The join handle lets application startup observe failures.
pub fn start_service_async() -> JoinHandle<Result<(), String>> {
    if INITIALIZED.load(Ordering::Acquire) {
        return std::thread::spawn(|| {
            if still_requested(SERVICE_GENERATION.load(Ordering::Acquire)) && is_running() {
                Ok(())
            } else {
                Err("Bridge service is no longer running.".into())
            }
        });
    }
    let owns_initialization = INITIALIZING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok();
    if !owns_initialization {
        return std::thread::spawn(|| {
            while INITIALIZING.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(25));
            }
            if INITIALIZED.load(Ordering::Acquire) {
                Ok(())
            } else {
                Err("Bridge startup initialization did not complete.".into())
            }
        });
    }
    SERVICE_REQUESTED.store(true, Ordering::Release);
    let generation = SERVICE_GENERATION.load(Ordering::Acquire);
    std::thread::spawn(move || {
        struct InitializingGuard;
        impl Drop for InitializingGuard {
            fn drop(&mut self) {
                INITIALIZING.store(false, Ordering::Release);
            }
        }
        let _initializing = InitializingGuard;
        if !SERVICE_REQUESTED.load(Ordering::Acquire)
            || SERVICE_GENERATION.load(Ordering::Acquire) != generation
        {
            return Ok(());
        }
        start_for_generation(generation)?;
        let path = crate::rpc::config_path()?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| "Cannot start the Bridge initialization worker.")?;
        let result = runtime.block_on(async {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
            loop {
                if !SERVICE_REQUESTED.load(Ordering::Acquire)
                    || SERVICE_GENERATION.load(Ordering::Acquire) != generation
                {
                    return Ok(());
                }
                if crate::rpc::Connection::connect(&path).await.is_ok() {
                    return crate::session::initialize_and_detach(path).await;
                }
                match status() {
                    Ok(1) => {}
                    Ok(0) => {
                        return Err("Bridge stopped before its local connection was ready.".into());
                    }
                    Ok(2) => {
                        return Err(
                            "Bridge is stopping before its local connection became ready.".into(),
                        );
                    }
                    Ok(_) => unreachable!("status() validates adapter state"),
                    Err(message) => return Err(message),
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(
                        "Bridge did not become ready. Check the Linux keyring, then retry.".into(),
                    );
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        });
        if result.is_ok()
            && SERVICE_REQUESTED.load(Ordering::Acquire)
            && SERVICE_GENERATION.load(Ordering::Acquire) == generation
        {
            INITIALIZED.store(true, Ordering::Release);
        }
        result
    })
}

/// Stop Bridge on a worker thread so GTK and the app shutdown signal handler
/// remain responsive while the Go runtime drains its server goroutines.
pub fn stop_service_async() -> JoinHandle<Result<(), String>> {
    std::thread::spawn(stop)
}

/// Gracefully stop a process created by Spotty's previous detached Bridge
/// bundle. This explicit migration path is tightly scoped to that package and
/// never sends Unix signals, so unrelated Bridge installations remain alone.
pub fn stop_legacy_bundled(pid: u32) -> Result<(), String> {
    if pid == 0 || pid == std::process::id() {
        return Err("Invalid legacy Bridge process id.".into());
    }
    let root = crate::bundle::legacy_runtime_root()?
        .canonicalize()
        .map_err(|_| "The old Spotty Bridge runtime was not found.")?;
    if std::fs::read_to_string(root.join(".ready")).ok().as_deref()
        != Some("3.27.0-4ae1f9a38392379a-fido1.15-startup1")
    {
        return Err("The old Spotty Bridge runtime marker did not match.".into());
    }
    let executable = root
        .join("usr/lib/protonmail/bridge/bridge")
        .canonicalize()
        .map_err(|_| "The old Spotty Bridge executable was not found.")?;
    let launcher = root.join("spotty-bridge-launcher");
    let process = ProcIdentity::read(pid)?;
    if !process.matches(&executable, &launcher) {
        return Err("The process is not the old Spotty-managed Bridge backend.".into());
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "Cannot start the Bridge migration worker.")?;
    runtime.block_on(async {
        let path = crate::rpc::config_path()?;
        let mut connection = tokio::time::timeout(
            Duration::from_secs(5),
            crate::rpc::Connection::connect(&path),
        )
        .await
        .map_err(|_| "Timed out connecting to the old Spotty Bridge service.")??;
        if !ProcIdentity::read(pid)?.same_instance(&process, &executable, &launcher) {
            return Err("The old Bridge process changed during migration.".into());
        }
        let _ = tokio::time::timeout(Duration::from_secs(3), connection.stop_stream()).await;
        let request = connection.unary(crate::protocol::Empty {});
        let quit = connection.client.quit(request);
        tokio::time::timeout(Duration::from_secs(3), quit)
            .await
            .map_err(|_| "Timed out asking the old Spotty Bridge service to stop.")?
            .map_err(|_| "The old Spotty Bridge service rejected the stop request.")?;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(
                    "The old Spotty Bridge service did not exit after its stop request.".into(),
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
}

struct ProcIdentity {
    uid: u32,
    executable: PathBuf,
    arguments: Vec<std::ffi::OsString>,
    start_time: u64,
}

impl ProcIdentity {
    fn read(pid: u32) -> Result<Self, String> {
        let proc = PathBuf::from(format!("/proc/{pid}"));
        let status = std::fs::read_to_string(proc.join("status"))
            .map_err(|_| "Cannot inspect the old Bridge process.")?;
        let uid = status
            .lines()
            .find_map(|line| {
                line.strip_prefix("Uid:")?
                    .split_whitespace()
                    .next()?
                    .parse()
                    .ok()
            })
            .ok_or("Cannot verify the old Bridge process owner.")?;
        if uid != unsafe { libc::geteuid() } {
            return Err("The old Bridge process belongs to another user.".into());
        }
        let executable = std::fs::read_link(proc.join("exe"))
            .map_err(|_| "Cannot inspect the old Bridge executable.")?;
        let arguments = std::fs::read(proc.join("cmdline"))
            .map_err(|_| "Cannot inspect the old Bridge arguments.")?
            .split(|byte| *byte == 0)
            .filter(|argument| !argument.is_empty())
            .map(|argument| std::ffi::OsStr::from_bytes(argument).to_os_string())
            .collect();
        let stat = std::fs::read_to_string(proc.join("stat"))
            .map_err(|_| "Cannot verify the old Bridge process identity.")?;
        // The comm field may contain spaces and parentheses; fields after its
        // final ')' start at process state (field 3), so starttime is index 19.
        let fields = stat
            .rsplit_once(')')
            .ok_or("Invalid old Bridge process data.")?
            .1;
        let start_time = fields
            .split_whitespace()
            .nth(19)
            .ok_or("Invalid old Bridge process data.")?
            .parse()
            .map_err(|_| "Invalid old Bridge process data.")?;
        Ok(Self {
            uid,
            executable,
            arguments,
            start_time,
        })
    }

    fn matches(&self, executable: &Path, launcher: &Path) -> bool {
        self.uid == unsafe { libc::geteuid() }
            && self.executable == executable
            && self.arguments.iter().any(|arg| arg == "--grpc")
            && self
                .arguments
                .windows(2)
                .any(|pair| pair[0] == "--launcher" && PathBuf::from(&pair[1]) == launcher)
    }

    fn same_instance(&self, previous: &Self, executable: &Path, launcher: &Path) -> bool {
        self.start_time == previous.start_time && self.matches(executable, launcher)
    }
}

/// Resolve the bundled library path for diagnostics and lifecycle checks.
pub fn library_path() -> Result<PathBuf, String> {
    crate::bundle::inprocess_library()
}

#[cfg(all(test, feature = "bundled-bridge"))]
mod tests {
    use super::*;

    #[test]
    fn generated_c_header_matches_rust_ffi_symbol_and_state_contract() {
        let header = include_str!(concat!(env!("OUT_DIR"), "/libspotty_proton_bridge.h"));
        for symbol in [
            "SpottyBridgeStart",
            "SpottyBridgeStop",
            "SpottyBridgeStatus",
            "SpottyBridgeVersion",
            "SpottyBridgeLastError",
            "SpottyBridgeFree",
        ] {
            assert!(header.contains(symbol), "missing ABI symbol {symbol}");
        }
        assert!(header.contains("SpottyBridgeStart(char* launcher)"));
        assert!(header.contains("SpottyBridgeFree(char* value)"));
        assert!([0, 1, 2].into_iter().all(valid_status));
        assert!(![-2, -1, 3].into_iter().any(valid_status));
    }

    #[test]
    fn legacy_stop_requires_the_exact_old_owned_backend_identity() {
        let executable = PathBuf::from("/private/old-bundle/bridge");
        let launcher = PathBuf::from("/private/old-bundle/spotty-bridge-launcher");
        let process = ProcIdentity {
            uid: unsafe { libc::geteuid() },
            executable: executable.clone(),
            arguments: vec![
                executable.clone().into_os_string(),
                "--grpc".into(),
                "--launcher".into(),
                launcher.clone().into_os_string(),
            ],
            start_time: 4,
        };
        assert!(process.matches(&executable, &launcher));

        let mut unrelated = process;
        unrelated.arguments[3] = "/usr/bin/protonmail-bridge".into();
        assert!(!unrelated.matches(&executable, &launcher));
    }
}
