//! A window that shows a Proton web page (for example the Proton Pass web app)
//! inside Spotty, rendered by the system's WebKitGTK (loaded at runtime;
//! nothing to install for Spotty's build). It keeps its own private web
//! profile (`~/.local/share/spotty/proton-web`).
//!
//! It is not used to sign in: Proton Calendar, Drive, Pass and VPN sign in
//! natively through Spotty's Proton account (`crate::proton_native`,
//! `crate::proton_session`). Without WebKitGTK 6, links open in the default
//! browser.

use crate::i18n::gettext;
use adw::prelude::*;
use glib::translate::{from_glib_full, ToGlibPtr};
use std::cell::RefCell;
use std::ffi::{c_char, c_void, CStr, CString};
use std::path::PathBuf;
use std::sync::OnceLock;

type GType = glib::ffi::GType;
type Ptr = *mut c_void;

struct WebKit {
    network_session_new: unsafe extern "C" fn(*const c_char, *const c_char) -> Ptr,
    session_data_manager: unsafe extern "C" fn(Ptr) -> Ptr,
    data_manager_clear: unsafe extern "C" fn(Ptr, u32, i64, Ptr, Ptr, Ptr),
    data_types_type: unsafe extern "C" fn() -> GType,
    web_view_type: unsafe extern "C" fn() -> GType,
    load_uri: unsafe extern "C" fn(Ptr, *const c_char),
    get_uri: unsafe extern "C" fn(Ptr) -> *const c_char,
    go_back: unsafe extern "C" fn(Ptr),
    can_go_back: unsafe extern "C" fn(Ptr) -> i32,
    reload: unsafe extern "C" fn(Ptr),
    decision_action: unsafe extern "C" fn(Ptr) -> Ptr,
    action_request: unsafe extern "C" fn(Ptr) -> Ptr,
    request_uri: unsafe extern "C" fn(Ptr) -> *const c_char,
    decision_ignore: unsafe extern "C" fn(Ptr),
    download_set_destination: unsafe extern "C" fn(Ptr, *const c_char),
}

unsafe impl Send for WebKit {}
unsafe impl Sync for WebKit {}

static WEBKIT: OnceLock<Option<WebKit>> = OnceLock::new();

fn webkit() -> Option<&'static WebKit> {
    WEBKIT
        .get_or_init(|| unsafe {
            let lib = libc::dlopen(c"libwebkitgtk-6.0.so.4".as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL);
            if lib.is_null() {
                log::info!("proton web: WebKitGTK 6 not found; Proton apps open in the browser");
                return None;
            }
            macro_rules! sym {
                ($name:literal) => {{
                    let raw = libc::dlsym(lib, concat!($name, "\0").as_ptr() as *const c_char);
                    if raw.is_null() {
                        log::warn!("proton web: WebKitGTK lacks {}", $name);
                        return None;
                    }
                    std::mem::transmute::<*mut c_void, _>(raw)
                }};
            }
            Some(WebKit {
                network_session_new: sym!("webkit_network_session_new"),
                session_data_manager: sym!("webkit_network_session_get_website_data_manager"),
                data_manager_clear: sym!("webkit_website_data_manager_clear"),
                data_types_type: sym!("webkit_website_data_types_get_type"),
                web_view_type: sym!("webkit_web_view_get_type"),
                load_uri: sym!("webkit_web_view_load_uri"),
                get_uri: sym!("webkit_web_view_get_uri"),
                go_back: sym!("webkit_web_view_go_back"),
                can_go_back: sym!("webkit_web_view_can_go_back"),
                reload: sym!("webkit_web_view_reload"),
                decision_action: sym!("webkit_navigation_policy_decision_get_navigation_action"),
                action_request: sym!("webkit_navigation_action_get_request"),
                request_uri: sym!("webkit_uri_request_get_uri"),
                decision_ignore: sym!("webkit_policy_decision_ignore"),
                download_set_destination: sym!("webkit_download_set_destination"),
            })
        })
        .as_ref()
}

/// True when Proton's apps can open inside Spotty.
pub fn available() -> bool {
    webkit().is_some()
}

fn data_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("spotty").join("proton-web")
}

fn marker() -> PathBuf {
    data_dir().join("signed-in")
}

/// True once a Proton app page was served to Spotty's Proton window, which
/// only happens to a signed-in session.
pub fn signed_in() -> bool {
    marker().exists()
}

/// Remember a signed-in session when `uri` is a Proton app page under a
/// `/u/N/` account path (signed-out visits are sent to account.proton.me).
fn note_session(uri: &str) {
    if is_session_page(uri) && !marker().exists() {
        let _ = crate::security::private_dir(&data_dir());
        let _ = std::fs::write(marker(), b"");
    }
}

fn is_session_page(uri: &str) -> bool {
    let Ok(parsed) = glib::Uri::parse(uri, glib::UriFlags::NONE) else { return false };
    let host = parsed.host().map(|h| h.to_lowercase()).unwrap_or_default();
    let app = matches!(host.as_str(), "calendar.proton.me" | "drive.proton.me" | "pass.proton.me" | "mail.proton.me");
    app && parsed.scheme() == "https" && parsed.path().starts_with("/u/")
}

fn cache_dir() -> PathBuf {
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("spotty").join("proton-web")
}

fn ptr(object: &glib::Object) -> Ptr {
    let raw: *mut glib::gobject_ffi::GObject = object.to_glib_none().0;
    raw.cast()
}

fn c_text(raw: *const c_char) -> Option<String> {
    (!raw.is_null()).then(|| unsafe { CStr::from_ptr(raw) }.to_string_lossy().into_owned())
}

/// Hosts that belong in the Proton window; everything else opens in the
/// default browser.
fn is_proton(uri: &str) -> bool {
    let Ok(parsed) = glib::Uri::parse(uri, glib::UriFlags::NONE) else { return false };
    let scheme = parsed.scheme();
    if scheme == "blob" || scheme == "about" || scheme == "data" {
        return true;
    }
    let host = parsed.host().map(|h| h.to_lowercase()).unwrap_or_default();
    scheme == "https" && (host == "proton.me" || host.ends_with(".proton.me"))
}

/// The one web profile Calendar and Drive share.
fn session(kit: &'static WebKit) -> glib::Object {
    thread_local! {
        static SESSION: RefCell<Option<glib::Object>> = const { RefCell::new(None) };
    }
    SESSION.with(|slot| {
        if let Some(session) = slot.borrow().as_ref() {
            return session.clone();
        }
        let data = data_dir();
        let cache = cache_dir();
        let _ = crate::security::private_dir(&data);
        let _ = std::fs::create_dir_all(&cache);
        let data_c = CString::new(data.to_string_lossy().as_bytes()).unwrap_or_default();
        let cache_c = CString::new(cache.to_string_lossy().as_bytes()).unwrap_or_default();
        let session: glib::Object = unsafe {
            from_glib_full((kit.network_session_new)(data_c.as_ptr(), cache_c.as_ptr()) as *mut glib::gobject_ffi::GObject)
        };
        connect_downloads(&session, kit);
        *slot.borrow_mut() = Some(session.clone());
        session
    })
}

/// Files downloaded from Drive go to the Downloads folder.
fn connect_downloads(session: &glib::Object, kit: &'static WebKit) {
    session.connect_local("download-started", false, move |values| {
        let download = values.get(1)?.get::<glib::Object>().ok()?;
        download.connect_local("decide-destination", false, move |values| {
            // A boolean signal: the handler must always answer (a missing
            // return value panics inside GLib's callback and aborts Spotty).
            let handled = (|| {
                let download = values.first()?.get::<glib::Object>().ok()?;
                let suggested = values.get(1).and_then(|v| v.get::<String>().ok()).unwrap_or_default();
                let path = download_path(&suggested);
                let c = CString::new(path.to_string_lossy().as_bytes()).ok()?;
                unsafe { (kit.download_set_destination)(ptr(&download), c.as_ptr()) };
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                download.connect_local("finished", false, move |_| {
                    notify(&gettext("Downloaded {name}").replace("{name}", &name), "");
                    None
                });
                Some(())
            })()
            .is_some();
            Some(handled.to_value())
        });
        None
    });
}

/// A free name in the Downloads folder for `suggested`.
fn download_path(suggested: &str) -> PathBuf {
    let dir = glib::user_special_dir(glib::UserDirectory::Downloads)
        .or_else(dirs::download_dir)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default());
    let clean: String = suggested
        .chars()
        .map(|c| if c == '/' || c == '\0' { '_' } else { c })
        .collect();
    let name = if clean.trim().is_empty() || clean == "." || clean == ".." { "download".to_owned() } else { clean };
    let path = std::path::Path::new(&name);
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| name.clone());
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let mut candidate = dir.join(&name);
    let mut n = 1;
    while candidate.exists() {
        candidate = dir.join(format!("{stem} ({n}){ext}"));
        n += 1;
    }
    candidate
}

fn notify(title: &str, body: &str) {
    if let Some(app) = gio::Application::default() {
        let notification = gio::Notification::new(title);
        if !body.is_empty() {
            notification.set_body(Some(body));
        }
        gio::prelude::ApplicationExt::send_notification(&app, Some("proton-web"), &notification);
    }
}

fn open_in_browser(uri: &str) {
    if crate::security::http_uri(uri).is_ok() {
        let _ = gio::AppInfo::launch_default_for_uri(uri, gio::AppLaunchContext::NONE);
    }
}

struct ProtonWindow {
    window: adw::ApplicationWindow,
    view: glib::Object,
}

thread_local! {
    /// One window per Proton app, keyed by host (calendar.proton.me, …).
    static WINDOWS: RefCell<Vec<(String, ProtonWindow)>> = const { RefCell::new(Vec::new()) };
}

fn app_title(host: &str) -> String {
    match host {
        "calendar.proton.me" => gettext("Proton Calendar"),
        "drive.proton.me" => gettext("Proton Drive"),
        "pass.proton.me" => gettext("Proton Pass"),
        "mail.proton.me" => gettext("Proton Mail"),
        _ => gettext("Proton"),
    }
}

fn host_of(uri: &str) -> String {
    glib::Uri::parse(uri, glib::UriFlags::NONE)
        .ok()
        .and_then(|u| u.host().map(|h| h.to_lowercase()))
        .unwrap_or_default()
}

/// Open a Proton web app URL in Spotty's Proton window (one per app), or in
/// the default browser when WebKitGTK isn't available or `uri` isn't Proton's.
pub fn open(uri: &str) {
    let Some(kit) = webkit() else {
        open_in_browser(uri);
        return;
    };
    if !is_proton(uri) || crate::security::http_uri(uri).is_err() {
        open_in_browser(uri);
        return;
    }
    let host = host_of(uri);
    let existing = WINDOWS.with(|windows| {
        windows
            .borrow()
            .iter()
            .find(|(h, _)| *h == host)
            .map(|(_, w)| (w.window.clone(), w.view.clone()))
    });
    if let Some((window, view)) = existing {
        load(kit, &view, uri);
        window.present();
        return;
    }
    let Some(app) = gio::Application::default().and_then(|app| app.downcast::<adw::Application>().ok()) else {
        open_in_browser(uri);
        return;
    };
    // `network-session` is construct-only, so build the view with it.
    let view = glib::Object::builder_with_type(unsafe { glib::translate::from_glib((kit.web_view_type)()) })
        .property("network-session", session(kit))
        .build();
    let Ok(widget) = view.clone().downcast::<gtk::Widget>() else {
        open_in_browser(uri);
        return;
    };
    widget.set_hexpand(true);
    widget.set_vexpand(true);
    connect_policy(kit, &view);

    let title = app_title(&host);
    let window = adw::ApplicationWindow::builder()
        .application(&app)
        .title(&title)
        .default_width(1180)
        .default_height(800)
        .build();
    let window_title = adw::WindowTitle::new(&title, &gettext("Built into Spotty"));
    let header = adw::HeaderBar::builder().title_widget(&window_title).build();
    let back = gtk::Button::from_icon_name("go-previous-symbolic");
    back.set_tooltip_text(Some(&gettext("Back")));
    back.set_sensitive(false);
    let reload = gtk::Button::from_icon_name("view-refresh-symbolic");
    reload.set_tooltip_text(Some(&gettext("Reload")));
    let browser = gtk::Button::from_icon_name("adw-external-link-symbolic");
    browser.set_tooltip_text(Some(&gettext("Open in browser")));
    header.pack_start(&back);
    header.pack_start(&reload);
    header.pack_end(&browser);
    let progress = gtk::ProgressBar::new();
    progress.add_css_class("osd");
    progress.set_visible(false);
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&widget));
    progress.set_valign(gtk::Align::Start);
    overlay.add_overlay(&progress);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&overlay));
    window.set_content(Some(&toolbar));

    {
        let view1 = view.clone();
        back.connect_clicked(move |_| unsafe { (kit.go_back)(ptr(&view1)) });
        let view2 = view.clone();
        reload.connect_clicked(move |_| unsafe { (kit.reload)(ptr(&view2)) });
        let view3 = view.clone();
        browser.connect_clicked(move |_| {
            if let Some(uri) = c_text(unsafe { (kit.get_uri)(ptr(&view3)) }) {
                open_in_browser(&uri);
            }
        });
    }
    {
        let back = back.clone();
        view.connect_notify_local(Some("uri"), move |view, _| {
            back.set_sensitive(unsafe { (kit.can_go_back)(ptr(view)) } != 0);
            if let Some(uri) = c_text(unsafe { (kit.get_uri)(ptr(view)) }) {
                note_session(&uri);
            }
        });
        let window_title = window_title.clone();
        view.connect_notify_local(Some("title"), move |view, _| {
            let page: Option<String> = view.property("title");
            window_title.set_subtitle(page.as_deref().filter(|t| !t.is_empty()).unwrap_or(""));
        });
        let progress = progress.clone();
        view.connect_notify_local(Some("estimated-load-progress"), move |view, _| {
            let value: f64 = view.property("estimated-load-progress");
            progress.set_fraction(value);
            progress.set_visible(value < 1.0);
        });
    }
    {
        let host = host.clone();
        window.connect_close_request(move |_| {
            WINDOWS.with(|windows| windows.borrow_mut().retain(|(h, _)| *h != host));
            glib::Propagation::Proceed
        });
    }
    load(kit, &view, uri);
    WINDOWS.with(|windows| {
        windows.borrow_mut().push((host, ProtonWindow { window: window.clone(), view }));
    });
    window.present();
}

fn load(kit: &WebKit, view: &glib::Object, uri: &str) {
    if let Ok(c) = CString::new(uri) {
        unsafe { (kit.load_uri)(ptr(view), c.as_ptr()) };
    }
}

/// Links that would open a new window: Proton pages stay in this window,
/// everything else goes to the default browser.
fn connect_policy(kit: &'static WebKit, view: &glib::Object) {
    view.connect_local("decide-policy", false, move |values| {
        // `decide-policy` returns a boolean: true when handled here, false to
        // let WebKit decide. It must ALWAYS answer: returning no value panics
        // inside GLib's callback and aborts Spotty on the first navigation.
        let handled = (|| {
            let view = values.first()?.get::<glib::Object>().ok()?;
            let decision = values.get(1)?.get::<glib::Object>().ok()?;
            let kind = unsafe { glib::gobject_ffi::g_value_get_enum(values.get(2)?.to_glib_none().0) };
            // WEBKIT_POLICY_DECISION_TYPE_NEW_WINDOW_ACTION
            if kind != 1 {
                return None;
            }
            let uri = unsafe {
                let action = (kit.decision_action)(ptr(&decision));
                let request = (kit.action_request)(action);
                c_text((kit.request_uri)(request))
            }?;
            unsafe { (kit.decision_ignore)(ptr(&decision)) };
            if is_proton(&uri) && host_of(&uri) == host_of(&c_text(unsafe { (kit.get_uri)(ptr(&view)) }).unwrap_or_default()) {
                load(kit, &view, &uri);
            } else if is_proton(&uri) && !uri.starts_with("blob:") {
                let uri = uri.clone();
                glib::idle_add_local_once(move || open(&uri));
            } else {
                open_in_browser(&uri);
            }
            Some(())
        })()
        .is_some();
        Some(handled.to_value())
    });
}

/// Sign out of Proton's web apps in Spotty: close their windows and erase the
/// web profile (cookies, storage and caches).
pub fn sign_out() {
    let _ = std::fs::remove_file(marker());
    WINDOWS.with(|windows| {
        for (_, w) in windows.borrow_mut().drain(..) {
            w.window.destroy();
        }
    });
    let Some(kit) = webkit() else {
        let _ = std::fs::remove_dir_all(data_dir());
        let _ = std::fs::remove_dir_all(cache_dir());
        return;
    };
    let session = session(kit);
    let types: glib::Type = unsafe { glib::translate::from_glib((kit.data_types_type)()) };
    let all = glib::FlagsClass::with_type(types)
        .and_then(|class| class.value_by_nick("all").map(|v| v.value()))
        .unwrap_or(u32::MAX >> 1);
    unsafe {
        let manager = (kit.session_data_manager)(ptr(&session));
        (kit.data_manager_clear)(manager, all, 0, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
    }
}

/// Sign in: Proton's own sign-in page, in Spotty's Proton window.
pub fn sign_in() {
    open("https://account.proton.me/login");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_proton_hosts_stay_in_the_window() {
        assert!(is_proton("https://calendar.proton.me/u/0/week"));
        assert!(is_proton("https://account.proton.me/login"));
        assert!(is_proton("https://proton.me/support"));
        assert!(!is_proton("https://proton.me.evil.example/"));
        assert!(!is_proton("http://drive.proton.me/"));
        assert!(!is_proton("https://example.com/"));
        assert_eq!(host_of("https://Drive.Proton.me/u/0/"), "drive.proton.me");
    }

    #[test]
    fn only_signed_in_app_pages_count_as_a_session() {
        assert!(is_session_page("https://calendar.proton.me/u/0/week"));
        assert!(is_session_page("https://pass.proton.me/u/1/"));
        assert!(!is_session_page("https://account.proton.me/login"));
        assert!(!is_session_page("https://calendar.proton.me/"));
        assert!(!is_session_page("https://evil.example/u/0/"));
        assert!(!is_session_page("http://drive.proton.me/u/0/"));
    }

    #[test]
    fn downloads_get_safe_unique_names() {
        let path = download_path("../x/report.pdf");
        assert_eq!(path.file_name().unwrap().to_string_lossy().contains('/'), false);
        assert!(download_path("").file_name().is_some());
    }
}
