use crate::i18n::gettext;

use adw::prelude::*;
use gtk::pango;
use std::io::Read;
use std::path::Path;

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

// ── Async preview payload delivery slot ──
// Worker decodes/computes preview data and writes here; a persistent
// poll on the main thread picks it up and applies to the UI.
#[derive(Clone)]
enum PreviewPayload {
    Image {
        rgba: Vec<u8>,
        w: u32,
        h: u32,
        /// If this is a page from a multi-page document (PDF/PPTX),
        /// store the total pages/slides for the nav bar.
        total_pages: Option<usize>,
        /// Which page/slide this payload represents (1-indexed).
        page: Option<usize>,
    },
    Text {
        content: String,
    },
    Info,
}

static PREVIEW_PAYLOAD: OnceLock<Mutex<Option<(u64, std::path::PathBuf, PreviewPayload)>>> =
    OnceLock::new();

fn preview_payload_slot() -> &'static Mutex<Option<(u64, std::path::PathBuf, PreviewPayload)>> {
    PREVIEW_PAYLOAD.get_or_init(|| Mutex::new(None))
}

// ── OCR deduplication: prevent multiple tesseract jobs for the same file ──
static OCR_INFLIGHT: OnceLock<Mutex<std::collections::HashSet<std::path::PathBuf>>> =
    OnceLock::new();

fn ocr_inflight() -> &'static Mutex<std::collections::HashSet<std::path::PathBuf>> {
    OCR_INFLIGHT.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

// ── Bounded in-memory preview cache ──
// Key: (path, mtime_secs). Values are either decoded RGBA (for images) or
// text content (for text/audio/archive/doc-type previews).
const PREVIEW_CACHE_CAP: usize = 24;

struct CacheEntry {
    path: std::path::PathBuf,
    mtime: u64,
    payload: PreviewPayload,
}

static PREVIEW_CACHE: OnceLock<Mutex<VecDeque<CacheEntry>>> = OnceLock::new();

fn preview_cache() -> &'static Mutex<VecDeque<CacheEntry>> {
    PREVIEW_CACHE.get_or_init(|| Mutex::new(VecDeque::new()))
}

// ── Page/slide count metadata for nav bar ──
static DOC_META: OnceLock<Mutex<std::collections::HashMap<std::path::PathBuf, usize>>> =
    OnceLock::new();

fn store_doc_meta(path: &Path, total: usize) {
    if let Ok(mut map) = DOC_META.get_or_init(|| Mutex::new(std::collections::HashMap::new())).lock() {
        map.insert(path.to_path_buf(), total);
    }
}

fn cache_lookup(path: &Path) -> Option<PreviewPayload> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut cache = preview_cache().lock().ok()?;
    if let Some(pos) = cache.iter().position(|e| e.path == path && e.mtime == mtime) {
        let entry = cache.remove(pos).unwrap();
        // Move to back (most recently used).
        cache.push_back(entry);
        return Some(cache.back().unwrap().payload.clone());
    }
    // Stale entry — remove it.
    cache.retain(|e| e.path != path);
    None
}

fn cache_insert(path: std::path::PathBuf, payload: PreviewPayload) {
    let meta = std::fs::metadata(&path).ok();
    let mtime = meta
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut cache) = preview_cache().lock() {
        // Remove existing entry for this path.
        cache.retain(|e| e.path != path);
        // Evict oldest if full.
        while cache.len() >= PREVIEW_CACHE_CAP {
            cache.pop_front();
        }
        cache.push_back(CacheEntry {
            path,
            mtime,
            payload,
        });
    }
}

#[derive(Clone)]
pub struct PreviewPane {
    container: gtk::Box,
    stack: gtk::Stack,
    image: gtk::Picture,
    image_caption: gtk::Label,
    video: gtk::Video,
    text: gtk::TextView,
    iicon: gtk::Image,
    imetadata: gtk::Grid,
    ihint: gtk::Label,
    // Music overview page widgets.
    mcover: gtk::Picture,
    mtitle: gtk::Label,
    mmeta: gtk::Label,
    mstats: gtk::Label,
    // Trigger help page widgets.
    htext: gtk::Label,
    hpic: gtk::Picture,
    // Clip timestamp: shown above the preview while a clipboard entry is
    // selected ("Clipped 27.09.2026 14:32").
    clip_caption: gtk::Label,
    // Update page widgets: libadwaita boxed list of pending packages.
    u_title: gtk::Label,
    u_sub: gtk::Label,
    u_list: gtk::ListBox,
    // Identity of the track currently shown in the music page, so late async
    // cover/stats updates can verify they're still relevant.
    msel: std::rc::Rc<std::cell::RefCell<String>>,
    // Tracks the path currently being previewed, so an async thumbnail that
    // finishes late doesn't overwrite a newer selection.
    current: std::rc::Rc<std::cell::RefCell<std::path::PathBuf>>,
    // Debounce: cancel and reschedule on each selection change so rapid
    // list rebuilds during typing don't trigger expensive decodes.
    preview_debounce_id: std::rc::Rc<std::cell::Cell<Option<gtk::glib::SourceId>>>,
    // Async image preview: generation counter + persistent poll.
    preview_gen: std::rc::Rc<std::cell::Cell<u64>>,
    preview_poll_id: std::rc::Rc<std::cell::Cell<Option<gtk::glib::SourceId>>>,
    // Multi-page document navigation (PPTX slides, PDF pages, etc.)
    current_slide: std::rc::Rc<std::cell::Cell<usize>>,
    total_slides: std::rc::Rc<std::cell::Cell<usize>>,
    slide_paths: std::rc::Rc<std::cell::RefCell<Vec<std::path::PathBuf>>>,
    nav_box: gtk::Box,
    nav_label: gtk::Label,
    nav_prev: gtk::Button,
    nav_next: gtk::Button,
    // Track which file the nav bar belongs to (for on-demand page rendering).
    nav_file_path: std::rc::Rc<std::cell::RefCell<std::path::PathBuf>>,
    // Whether a preview payload has been applied (set true on apply, false on clear).
    // Prevents the stale-`current` early-return from blocking re-render after a clear.
    displayed: std::rc::Rc<std::cell::Cell<bool>>,
    // Whether a preview load is currently in flight (debounce fired, worker running).
    // Prevents list rebuilds from cancelling/restarting an in-flight load.
    pending: std::rc::Rc<std::cell::Cell<bool>>,
    // Whether the nav buttons have been connected (prevents duplicate handlers).
    nav_connected: std::rc::Rc<std::cell::Cell<bool>>,
    // (mtime secs, size) of `current` at load time — refresh_if_stale()
    // compares against it to re-render when the file changes on disk.
    // (0,0) = no path-preview active (text/help pages clear it).
    current_stamp: std::rc::Rc<std::cell::Cell<(u64, u64)>>,
    // Debounce for the list refresh scheduled after a live reload, so the
    // result rows + thumbnails catch up without being hammered per stat tick.
    list_refresh_id: std::rc::Rc<std::cell::Cell<Option<gtk::glib::SourceId>>>,
    // Persistent 1 s stat-timer driving refresh_if_stale().
    stale_poll_id: std::rc::Rc<std::cell::Cell<Option<gtk::glib::SourceId>>>,
    // Wheel stepping (preview nav): accumulated vertical scroll delta and
    // the instant of the last step (~200 ms debounce, one notch = one page).
    wheel_delta: std::rc::Rc<std::cell::Cell<f64>>,
    wheel_last: std::rc::Rc<std::cell::Cell<Option<std::time::Instant>>>,
}

impl PreviewPane {
    pub fn new() -> Self {
        // Fixed-width column for preview. Height grows with body.
        let container = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .width_request(340)
            .vexpand(true)
            .valign(gtk::Align::Fill)
            .css_classes(["preview-pane"])
            .build();

        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .transition_duration(100)
            .vexpand(true)
            .hexpand(true)
            .build();

        // Empty placeholder
        stack.add_named(
            &gtk::Label::builder()
                .label(gettext("Highlight a result\nto preview"))
                .css_classes(["dim-label"])
                .wrap(true)
                .justify(gtk::Justification::Center)
                .vexpand(true)
                .valign(gtk::Align::Center)
                .build(),
            Some("empty"),
        );

        // Image - put inside a ScrolledWindow with hard max content size.
        // This is the only way Picture can be capped in GTK4 since Picture
        // requests its natural pixel size by default.
        let image = gtk::Picture::builder()
            .can_shrink(true)
            .content_fit(gtk::ContentFit::Contain)
            .width_request(320)
            .height_request(240)
            .hexpand(true)
            .vexpand(true)
            .build();
        let image_clip = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .max_content_width(320)
            .max_content_height(240)
            .min_content_width(320)
            .min_content_height(240)
            .propagate_natural_width(false)
            .propagate_natural_height(false)
            .child(&image)
            .build();
        let image_caption = gtk::Label::builder()
            .css_classes(["caption", "dim-label"])
            .halign(gtk::Align::Center)
            .margin_top(6)
            .margin_bottom(2)
            .wrap(true)
            .build();
        let image_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        image_box.append(&image_clip);
        image_box.append(&image_caption);
        stack.add_named(&image_box, Some("image"));

        // Video - plays any format GStreamer (the GTK media backend) supports,
        // with the standard play/pause/seek/volume overlay controls.
        let video = gtk::Video::builder()
            .autoplay(false)
            .loop_(false)
            .width_request(320)
            .height_request(240)
            .hexpand(true)
            .vexpand(true)
            .build();
        let video_clip = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .max_content_width(320)
            .max_content_height(240)
            .min_content_width(320)
            .min_content_height(240)
            .propagate_natural_width(false)
            .propagate_natural_height(false)
            .child(&video)
            .build();
        stack.add_named(&video_clip, Some("video"));

        // Text
        let text = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            .wrap_mode(gtk::WrapMode::WordChar)
            .left_margin(8)
            .right_margin(8)
            .top_margin(8)
            .bottom_margin(8)
            .build();
        let text_scroll = gtk::ScrolledWindow::builder()
            .child(&text)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .max_content_height(200)
            .propagate_natural_height(true)
            .build();
        stack.add_named(&text_scroll, Some("text"));

        // Info card
        let ib = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .halign(gtk::Align::Fill)
            .valign(gtk::Align::Start)
            .vexpand(true)
            .margin_start(12)
            .margin_end(12)
            .margin_top(16)
            .build();
        let iicon = gtk::Image::builder().pixel_size(64).halign(gtk::Align::Center).build();
        let metadata_heading = gtk::Label::builder()
            .label(gettext("Metadata"))
            .halign(gtk::Align::Start)
            .css_classes(["heading"])
            .build();
        let imetadata = gtk::Grid::builder()
            .column_spacing(16)
            .row_spacing(8)
            .hexpand(true)
            .build();
        ib.append(&iicon);
        ib.append(&metadata_heading);
        ib.append(&imetadata);
        let ihint = gtk::Label::builder()
            .css_classes(["caption", "dim-label"])
            .halign(gtk::Align::Start)
            .wrap(true)
            .build();
        ib.append(&ihint);
        stack.add_named(&ib, Some("info"));

        // Music overview: large cover art, title, artist + source badge, and
        // live view/like statistics.
        let mb = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .vexpand(true)
            .build();
        let mcover = gtk::Picture::builder()
            .can_shrink(true)
            .content_fit(gtk::ContentFit::Contain)
            .width_request(240)
            .height_request(240)
            .css_classes(["music-cover-large"])
            .build();
        let mcover_clip = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .max_content_width(240)
            .max_content_height(240)
            .min_content_width(240)
            .min_content_height(240)
            .propagate_natural_width(false)
            .propagate_natural_height(false)
            .child(&mcover)
            .build();
        let mtitle = gtk::Label::builder()
            .wrap(true)
            .justify(gtk::Justification::Center)
            .max_width_chars(26)
            .css_classes(["title-4"])
            .build();
        let mmeta = gtk::Label::builder()
            .css_classes(["dim-label"])
            .wrap(true)
            .justify(gtk::Justification::Center)
            .max_width_chars(30)
            .build();
        let mstats = gtk::Label::builder()
            .css_classes(["caption"])
            .wrap(true)
            .justify(gtk::Justification::Center)
            .max_width_chars(32)
            .build();
        mb.append(&mcover_clip);
        mb.append(&mtitle);
        mb.append(&mmeta);
        mb.append(&mstats);
        stack.add_named(&mb, Some("music"));

        // Trigger help: instruction text (scrollable, selectable) plus an
        // optional screenshot below it.
        let hb = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .hexpand(true)
            .vexpand(true)
            .build();
        let htext = gtk::Label::builder()
            .wrap(true)
            .justify(gtk::Justification::Left)
            .xalign(0.0)
            .selectable(true)
            .css_classes(["dim-label"])
            .build();
        let htext_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .max_content_height(140)
            .propagate_natural_height(true)
            .child(&htext)
            .build();
        let hpic = gtk::Picture::builder()
            .can_shrink(true)
            .content_fit(gtk::ContentFit::Contain)
            .width_request(320)
            .height_request(240)
            .hexpand(true)
            .build();
        let hpic_clip = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .max_content_width(320)
            .max_content_height(240)
            .min_content_width(320)
            .min_content_height(240)
            .propagate_natural_width(false)
            .propagate_natural_height(false)
            .child(&hpic)
            .build();
        hb.append(&htext_scroll);
        hb.append(&hpic_clip);
        stack.add_named(&hb, Some("help"));

        // Update page: title + count, a libadwaita boxed list of what wants
        // updating.
        let u_title = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["title-4"])
            .build();
        let u_sub = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["caption", "dim-label"])
            .build();
        let u_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .hexpand(true)
            .build();
        let ub = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .hexpand(true)
            .build();
        ub.append(&u_title);
        ub.append(&u_sub);
        ub.append(&u_list);
        let u_scroll = gtk::ScrolledWindow::builder()
            .child(&ub)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .max_content_height(360)
            .propagate_natural_height(true)
            .build();
        stack.add_named(&u_scroll, Some("update"));

        let clip_caption = gtk::Label::builder()
            .css_classes(["caption", "dim-label"])
            .halign(gtk::Align::Start)
            .margin_start(8)
            .margin_end(8)
            .margin_top(6)
            .wrap(true)
            .visible(false)
            .build();
        container.append(&clip_caption);
        container.append(&stack);
        stack.set_visible_child_name("empty");

        // Navigation bar for multi-page documents (PPTX slides, PDF pages, etc.)
        let nav_prev = gtk::Button::builder()
            .label(gettext("◀"))
            .width_request(36)
            .sensitive(false)
            .build();
        let nav_label = gtk::Label::builder()
            .label(gettext("1 / 1"))
            .width_request(80)
            .halign(gtk::Align::Center)
            .build();
        let nav_next = gtk::Button::builder()
            .label(gettext("▶"))
            .width_request(36)
            .sensitive(false)
            .build();
        let nav_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .halign(gtk::Align::Center)
            .margin_top(4)
            .margin_bottom(4)
            .visible(false)
            .build();
        nav_box.append(&nav_prev);
        nav_box.append(&nav_label);
        nav_box.append(&nav_next);
        container.append(&nav_box);

        let pane = Self {
            container,
            stack,
            image,
            image_caption,
            video,
            text,
            iicon,
            imetadata,
            ihint,
            mcover,
            mtitle,
            mmeta,
            mstats,
            htext,
            hpic,
            clip_caption,
            u_title,
            u_sub,
            u_list,
            msel: std::rc::Rc::new(std::cell::RefCell::new(String::new())),
            current: std::rc::Rc::new(std::cell::RefCell::new(std::path::PathBuf::new())),
            preview_debounce_id: std::rc::Rc::new(std::cell::Cell::new(None)),
            preview_gen: std::rc::Rc::new(std::cell::Cell::new(0)),
            preview_poll_id: std::rc::Rc::new(std::cell::Cell::new(None)),
            current_slide: std::rc::Rc::new(std::cell::Cell::new(1)),
            total_slides: std::rc::Rc::new(std::cell::Cell::new(1)),
            slide_paths: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
            nav_box,
            nav_label,
            nav_prev,
            nav_next,
            nav_file_path: std::rc::Rc::new(std::cell::RefCell::new(std::path::PathBuf::new())),
            displayed: std::rc::Rc::new(std::cell::Cell::new(false)),
            pending: std::rc::Rc::new(std::cell::Cell::new(false)),
            nav_connected: std::rc::Rc::new(std::cell::Cell::new(false)),
            current_stamp: std::rc::Rc::new(std::cell::Cell::new((0, 0))),
            list_refresh_id: std::rc::Rc::new(std::cell::Cell::new(None)),
            stale_poll_id: std::rc::Rc::new(std::cell::Cell::new(None)),
            wheel_delta: std::rc::Rc::new(std::cell::Cell::new(0.0)),
            wheel_last: std::rc::Rc::new(std::cell::Cell::new(None)),
        };

        // Live refresh: re-render the preview when the file it's showing is
        // edited/saved externally. One stat per second — a no-op until the
        // (mtime, size) stamp actually changes.
        let pane_stale = pane.clone();
        let stale_id = gtk::glib::timeout_add_local(Duration::from_secs(1), move || {
            pane_stale.refresh_if_stale();
            gtk::glib::ControlFlow::Continue
        });
        pane.stale_poll_id.set(Some(stale_id));

        // Wheel over the preview steps multi-page documents (one notch =
        // one slide/page, ~200 ms debounce). Capture phase: the event is
        // ours before any inner ScrolledWindow can claim it; pages without
        // a multi-page image preview return Proceed untouched.
        // VERTICAL only: no KINETIC flag → no momentum continuation.
        let wheel = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        wheel.set_propagation_phase(gtk::PropagationPhase::Capture);
        let pane_wheel = pane.clone();
        wheel.connect_scroll(move |_ctl, _dx, dy| {
            if pane_wheel.wheel_step(dy) {
                gtk::glib::Propagation::Stop
            } else {
                gtk::glib::Propagation::Proceed
            }
        });
        pane.container.add_controller(wheel);

        pane
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.container
    }
    /// The path currently being previewed (a sentinel a late async update
    /// compares against before touching the UI).
    pub fn current_path(&self) -> std::path::PathBuf {
        self.current.borrow().clone()
    }
    pub fn clear(&self) {
        self.stop_video();
        *self.msel.borrow_mut() = String::new();
        self.displayed.set(false);
        self.pending.set(false);
        self.current_stamp.set((0, 0));
        self.clip_caption.set_visible(false);
        self.stack.set_visible_child_name("empty");
    }

    /// Show when a clipboard entry was captured ("Clipped <date> <time>"),
    /// above the preview content; `None` hides it (as does `clear`).
    pub fn show_clip_stamp(&self, at: Option<u64>) {
        let when = at
            .and_then(|s| gtk::glib::DateTime::from_unix_local(s as i64).ok())
            .and_then(|dt| dt.format("%x %X").ok())
            .map(|s| s.to_string());
        match when {
            Some(w) => {
                self.clip_caption
                    .set_label(&gettext("Clipped {time}").replace("{time}", &w));
                self.clip_caption.set_visible(true);
            }
            None => self.clip_caption.set_visible(false),
        }
    }

    /// Pause and detach the video player so it doesn't keep playing (or
    /// holding the file open) once the user navigates away.
    fn stop_video(&self) {
        if let Some(stream) = self.video.media_stream() {
            stream.pause();
        }
        self.video.set_file(gtk::gio::File::NONE);
    }

    /// Show raw text (e.g. a clipboard text entry) in full, scrollable.
    pub fn show_text(&self, s: &str) {
        *self.current.borrow_mut() = std::path::PathBuf::new();
        self.current_stamp.set((0, 0)); // not a path preview
        self.stop_video();
        self.displayed.set(true);
        self.text.buffer().set_text(s);
        self.stack.set_visible_child_name("text");
    }

    /// Show what an update run will do: the package list as a libadwaita
    /// boxed list.
    pub fn show_update(&self, title: &str, subtitle: &str, packages: &[String]) {
        self.stop_video();
        self.current_stamp.set((0, 0));
        *self.current.borrow_mut() = std::path::PathBuf::new();
        self.displayed.set(true);
        self.u_title.set_label(title);
        self.u_sub.set_label(subtitle);
        self.u_sub.set_visible(!subtitle.is_empty());

        // Rebuild the package rows — one adw row each, capped so a large
        // update set can't stretch the pane past its scroll limit.
        while let Some(child) = self.u_list.first_child() {
            self.u_list.remove(&child);
        }
        const CAP: usize = 40;
        for pkg in packages.iter().take(CAP) {
            let row = adw::ActionRow::builder().title(pkg).use_markup(false).build();
            row.add_prefix(&gtk::Image::from_icon_name("software-update-available-symbolic"));
            self.u_list.append(&row);
        }
        if packages.len() > CAP {
            let more = gettext("…and {n} more").replace(
                "{n}",
                &(packages.len() - CAP).to_string(),
            );
            let row = adw::ActionRow::builder().title(more).use_markup(false).build();
            row.add_prefix(&gtk::Image::from_icon_name("view-more-symbolic"));
            self.u_list.append(&row);
        }
        self.u_list.set_visible(!packages.is_empty());

        self.stack.set_visible_child_name("update");
    }

    /// Show trigger help: instruction text, plus the help_image screenshot
    /// when one is available locally (downloaded on demand to the cache).
    /// `image_path` may point at a not-yet-downloaded cache file; in that
    /// case the current marker is left as that path so the late download can
    /// verify it's still the row being previewed.
    pub fn show_help(&self, help: &str, image_path: Option<&Path>) {
        self.stop_video();
        self.current_stamp.set((0, 0)); // not a path preview
        self.displayed.set(true);
        self.htext.set_label(help);
        if let Some(p) = image_path {
            if p.exists() {
                *self.current.borrow_mut() = std::path::PathBuf::new();
                set_picture_from_file(&self.hpic, p);
                self.hpic.set_visible(true);
            } else {
                *self.current.borrow_mut() = p.to_path_buf();
                self.hpic.set_visible(false);
            }
        } else {
            *self.current.borrow_mut() = std::path::PathBuf::new();
            self.hpic.set_visible(false);
        }
        self.stack.set_visible_child_name("help");
    }

    pub fn show_path(&self, p: &Path) {
        self.show_path_inner(p, false);
    }

    /// `force` bypasses the "already displaying this path" early return so a
    /// file that changed on disk can be re-loaded (live refresh). It also
    /// tolerates the path being transiently absent during an atomic save.
    fn show_path_inner(&self, p: &Path, force: bool) {
        if !p.exists() {
            if force {
                return; // vanished mid-save — the next staleness tick retries
            }
            return self.clear();
        }
        // Skip re-render if this file is already being displayed
        // or a load for it is already in flight.
        if !force && *self.current.borrow() == p && (self.displayed.get() || self.pending.get()) {
            return;
        }
        *self.current.borrow_mut() = p.to_path_buf();
        // Change-detection key for refresh_if_stale() (mtime + size).
        self.current_stamp.set(file_stamp(p).unwrap_or((0, 0)));
        self.stop_video();

        let is_video_ext = |ext: &str| {
            matches!(
                ext,
                "mp4" | "mkv" | "webp" | "mov" | "avi" | "wmv" | "flv" | "m4v"
                    | "mpeg" | "mpg" | "m2ts" | "mts" | "ogv" | "3gp" | "3g2"
                    | "asf" | "rm" | "rmvb" | "vob" | "divx" | "f4v" | "mxf"
            )
        };
        let ext = p
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_lowercase())
            .unwrap_or_default();

        // Cancel any pending debounce from a previous selection.
        if let Some(id) = self.preview_debounce_id.take() {
            id.remove();
        }

        // For non-image files (text, audio, archive, office): try in-memory cache first.
        // Images and AVIF/HEIC always go through the async worker (they decode to RGBA).
        // Videos go through the debounce (loaded on main thread after settling).
        let is_raster_image = matches!(
            ext.as_str(),
            "png" | "jpg" | "jpeg" | "jpe" | "webp" | "gif" | "bmp" | "svg"
                | "svgz" | "ico" | "tiff" | "tif" | "jxl" | "ppm" | "pgm"
                | "pbm" | "pnm" | "xpm" | "tga" | "avif" | "heic" | "heif"
        );

        if !is_raster_image && !is_video_ext(&ext) {
            if let Some(cached) = cache_lookup(p) {
                self.apply_payload(p, &cached);
                return;
            }
        }

        // Debounce all preview loads: 120ms trailing.
        let path = p.to_path_buf();
        let gen_cell = self.preview_gen.clone();
        let db = self.preview_debounce_id.clone();
        let is_video = is_video_ext(&ext);
        let pending_flag = self.pending.clone();
        pending_flag.set(true);

        let pane = self.clone();
        let id = gtk::glib::timeout_add_local_once(Duration::from_millis(120), move || {
            db.set(None);
            pane.ensure_payload_poll();
            let new_gen = gen_cell.get().wrapping_add(1);
            gen_cell.set(new_gen);
            if is_video {
                // Video: load on the main thread via the delivery slot.
                // Signal delivery via a special marker that apply_payload recognizes.
                let _ = preview_payload_slot().lock().map(|mut s| {
                    let dominated = s.as_ref().map_or(false, |(g, _, _)| *g > new_gen);
                    if !dominated {
                        *s = Some((new_gen, path.clone(), PreviewPayload::Info));
                    }
                });
                return;
            }
            let path_clone = path.clone();
            std::thread::spawn(move || {
                let payload = compute_preview(&path_clone);
                // Cache text/archive/audio payloads for instant revisit.
                // Don't cache Image (too large) or Info (allow retry on next selection).
                if matches!(&payload, PreviewPayload::Text { .. }) {
                    cache_insert(path_clone.clone(), payload.clone());
                }
                let _ = preview_payload_slot().lock().map(|mut s| {
                    // Only store if our generation is still current or newer
                    // (prevent a stale slow worker from clobbering a fresh payload).
                    let dominated = s.as_ref().map_or(false, |(g, _, _)| *g > new_gen);
                    if !dominated {
                        *s = Some((new_gen, path_clone, payload));
                    }
                });
            });
        });
        self.preview_debounce_id.set(Some(id));
    }

    /// Re-render the currently shown preview when its file changed on disk
    /// (external edit/save). Called from a 1 s timer and whenever the
    /// indexer refreshes results; the common case is one stat() — a no-op.
    pub fn refresh_if_stale(&self) {
        if !self.displayed.get() || self.pending.get() {
            return;
        }
        let p = self.current.borrow().clone();
        if p.is_empty() {
            return; // text/help page — no file being previewed
        }
        let Some(st) = file_stamp(&p) else { return }; // unreadable or mid-save
        if st == self.current_stamp.get() {
            return; // unchanged
        }
        self.current_stamp.set(st);

        // Multi-page doc showing a page of ITSELF: re-render just that page
        // (disk cache is keyed by mtime → this renders fresh) so the user's
        // slide position survives the edit.
        let ext = p
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let nav_ok = *self.nav_file_path.borrow() == p && nav_native_ext(&ext);
        if nav_ok {
            self.show_slide(self.current_slide.get());
        } else {
            self.show_path_inner(&p, true);
        }
        log::info!(
            "preview: reloaded {} (file changed)",
            p.file_name().unwrap_or_default().to_string_lossy()
        );
        self.schedule_list_refresh();
    }

    /// Stop the two persistent timers (the 16 ms payload poll and the 1 s
    /// staleness check). They run forever by design — the owning window must
    /// call this when it is destroyed, or every replaced window would leave
    /// two live timers pinning its widget graph (and ticking) forever.
    pub fn stop_polling(&self) {
        if let Some(id) = self.preview_poll_id.take() {
            id.remove();
        }
        if let Some(id) = self.stale_poll_id.take() {
            id.remove();
        }
    }

    /// Debounce so the result list (rows + Tier-0 thumbnails) catches up
    /// too — the thumbnails memo keys include (path, mtime, size), so a
    /// rebuild re-renders the icon from the changed file.
    fn schedule_list_refresh(&self) {
        if let Some(id) = self.list_refresh_id.take() {
            id.remove();
        }
        let id = gtk::glib::timeout_add_local_once(Duration::from_millis(500), || {
            crate::app::refresh_search_window();
        });
        self.list_refresh_id.set(Some(id));
    }

    /// Kick off background OCR for the image so it becomes searchable.
    /// No UI update — OCR text is used by find-mode search via the cache.
    fn refresh_ocr_text(&self, p: &Path) {
        if crate::ocr::cached_text_for(p).is_some() {
            return;
        }
        if !crate::ocr::is_available() {
            return;
        }
        let ocr_path = p.to_path_buf();
        // Dedupe: skip if a tesseract job is already running for this file.
        {
            if let Ok(mut set) = ocr_inflight().lock() {
                if !set.insert(ocr_path.clone()) {
                    return;
                }
            }
        }
        std::thread::spawn(move || {
            crate::ocr::text_for(&ocr_path);
            if let Ok(mut set) = ocr_inflight().lock() {
                set.remove(&ocr_path);
            }
        });
    }

    /// Poll the async decode delivery slot and apply if the generation matches.
    /// Deliver async preview results with a 16 ms poll that runs only while
    /// a load is in flight: it stops itself once nothing is pending, so an
    /// idle (or hidden) pane causes no wakeups.
    fn ensure_payload_poll(&self) {
        if let Some(id) = self.preview_poll_id.take() {
            self.preview_poll_id.set(Some(id));
            return;
        }
        let pane = self.clone();
        let id = gtk::glib::timeout_add_local(Duration::from_millis(16), move || {
            pane.deliver_pending_preview_payload();
            if pane.pending.get() {
                return gtk::glib::ControlFlow::Continue;
            }
            pane.preview_poll_id.set(None);
            gtk::glib::ControlFlow::Break
        });
        self.preview_poll_id.set(Some(id));
    }

    fn deliver_pending_preview_payload(&self) {
        let (gen, path, payload) = {
            let slot = preview_payload_slot().lock().ok();
            match slot.and_then(|mut g| g.take()) {
                Some(v) => v,
                None => return,
            }
        };
        if gen != self.preview_gen.get() {
            return;
        }
        if *self.current.borrow() != path {
            return;
        }
        self.apply_payload(&path, &payload);
    }

    fn apply_payload(&self, path: &Path, payload: &PreviewPayload) {
        self.displayed.set(true);
        self.pending.set(false);
        match payload {
            PreviewPayload::Image { rgba, w, h, total_pages, page } => {
                log::info!("preview: applied Image {}x{} for {}{}", w, h, path.file_name().unwrap_or_default().to_string_lossy(),
                    total_pages.map_or_else(String::new, |t| format!(" ({} pages)", t)));
                let bytes = gtk::glib::Bytes::from(rgba);
                let tex = gtk::gdk::MemoryTexture::new(
                    *w as i32,
                    *h as i32,
                    gtk::gdk::MemoryFormat::R8g8b8a8,
                    &bytes,
                    (*w * 4) as usize,
                );
                self.image.set_paintable(Some(&tex));
                self.stack.set_visible_child_name("image");
                self.image_caption.set_label(
                    &crate::imageinfo::info(path)
                        .map(|i| crate::imageinfo::caption(&i))
                        .unwrap_or_default(),
                );
                // Spreadsheet pages show the sheet/part name as the caption
                // while paging; other types keep the file-info caption.
                if let Some(cap) = office_page_caption(path, page.unwrap_or(1)) {
                    self.image_caption.set_label(&cap);
                }
                // Only OCR actual image files — skip PDF/PPTX/office pages.
                if is_ocrable_image(path) {
                    self.refresh_ocr_text(path);
                }
                // Set up nav bar for multi-page documents (PDF/PPTX/legacy PPT).
                if let Some(&total) = total_pages.as_ref() {
                    let page = page.unwrap_or(1);
                    self.total_slides.set(total);
                    self.current_slide.set(page);
                    *self.nav_file_path.borrow_mut() = path.to_path_buf();
                    self.slide_paths.borrow_mut().clear();
                    self.nav_box.set_visible(total > 1);
                    self.nav_prev.set_sensitive(page > 1);
                    self.nav_next.set_sensitive(page < total);
                    self.update_nav_label();
                    self.connect_nav_buttons();
                } else {
                    self.nav_box.set_visible(false);
                }
            }
            PreviewPayload::Text { content } => {
                self.text.buffer().set_text(content);
                self.stack.set_visible_child_name("text");
            }
            PreviewPayload::Info => {
                log::info!("preview: applied Info for {}", path.file_name().unwrap_or_default().to_string_lossy());
                // Check if this is a video file — if so, load the video player.
                let is_video = path.extension()
                    .and_then(|s| s.to_str())
                    .map(|ext| matches!(ext.to_ascii_lowercase().as_str(),
                        "mp4" | "mkv" | "webp" | "mov" | "avi" | "wmv" | "flv" | "m4v"
                            | "mpeg" | "mpg" | "m2ts" | "mts" | "ogv" | "3gp" | "3g2"
                            | "asf" | "rm" | "rmvb" | "vob" | "divx" | "f4v" | "mxf"
                    ))
                    .unwrap_or(false);
                if is_video {
                    self.stop_video();
                    self.video.set_file(Some(&gtk::gio::File::for_path(path)));
                    self.stack.set_visible_child_name("video");
                } else {
                    self.show_info(path);
                }
            }
        }
    }

    /// Show a specific slide/page of the document the nav bar is bound to.
    /// Resolution (disk cache → on-demand render) is shared with the nav
    /// buttons and the wheel stepper via `resolve_page_png`.
    fn show_slide(&self, n: usize) {
        let file_path = self.nav_file_path.borrow().clone();
        if file_path.is_empty() {
            return;
        }
        if let Some(page_path) = resolve_page_png(&file_path, n) {
            set_picture_from_file(&self.image, &page_path);
            self.stack.set_visible_child_name("image");
            // Sheet pages name the sheet/part; prose/slide pages clear the
            // caption (the nav label already carries n / total).
            let cap = office_page_caption(&file_path, n).unwrap_or_default();
            self.image_caption.set_label(&cap);
            return;
        }
        // Fallback: pre-rendered page list (types without per-page rendering).
        let paths = self.slide_paths.borrow();
        if let Some(path) = paths.get(n.saturating_sub(1)) {
            set_picture_from_file(&self.image, path);
            self.stack.set_visible_child_name("image");
            self.image_caption.set_label("");
        } else {
            log::warn!(
                "preview: failed to render page {} of {}",
                n,
                file_path.display()
            );
        }
    }

    /// Step to slide/page `n`: clamp into range, update the live nav state
    /// (label + button sensitivity), then resolve and display the page.
    /// One shared path for the ◀/▶ buttons and the wheel stepper.
    fn nav_to(&self, n: usize) {
        // Only step while a multi-page image preview is actually on screen —
        // a nav bar lingering over a text/info page must not hijack it.
        if self.nav_file_path.borrow().is_empty()
            || self.stack.visible_child_name().as_deref() != Some("image")
        {
            return;
        }
        let n = clamp_page(n, self.total_slides.get());
        self.current_slide.set(n);
        self.update_nav_label();
        self.nav_prev.set_sensitive(n > 1);
        self.nav_next.set_sensitive(n < self.total_slides.get());
        self.show_slide(n);
    }

    /// Accumulate wheel deltas and step exactly one slide/page per full
    /// notch (|Δ| ≥ 1), debounced to ~200 ms so a trackpad flick or a held
    /// wheel key can't flip through the whole deck. GDK: positive delta_y =
    /// scroll down → next slide. Returns true when the event was consumed —
    /// only while a multi-page image preview shows; inert everywhere else
    /// (the event then Proceeds to inner widgets, e.g. text scrolling).
    fn wheel_step(&self, dy: f64) -> bool {
        if !self.nav_box.is_visible()
            || self.stack.visible_child_name().as_deref() != Some("image")
            || self.total_slides.get() <= 1
            || self.nav_file_path.borrow().is_empty()
        {
            return false;
        }
        let acc = self.wheel_delta.get() + dy;
        if acc.abs() < 1.0 {
            self.wheel_delta.set(acc);
            return true;
        }
        let now = std::time::Instant::now();
        if let Some(last) = self.wheel_last.get() {
            if now.duration_since(last) < Duration::from_millis(200) {
                // Inside the debounce window: swallow the notch so momentum
                // scrolls can't skip slides.
                self.wheel_delta.set(0.0);
                return true;
            }
        }
        self.wheel_delta.set(0.0);
        self.wheel_last.set(Some(now));
        let cur = self.current_slide.get();
        let target = if acc > 0.0 {
            cur + 1
        } else {
            cur.saturating_sub(1)
        };
        if clamp_page(target, self.total_slides.get()) != cur {
            self.nav_to(target);
        }
        true
    }

    /// Step one page/slide with the ←/→ keys. Same preconditions as the
    /// wheel stepper: consumed only while a multi-page image preview is
    /// actually on screen (decks, PDFs, paginated docs), inert otherwise —
    /// returns whether the key was handled so callers can fall through to
    /// the search entry's caret movement. `dir < 0` = previous page.
    pub fn key_step_page(&self, dir: i32) -> bool {
        if !self.nav_box.is_visible()
            || self.stack.visible_child_name().as_deref() != Some("image")
            || self.total_slides.get() <= 1
            || self.nav_file_path.borrow().is_empty()
        {
            return false;
        }
        let cur = self.current_slide.get();
        let target = if dir < 0 {
            cur.saturating_sub(1)
        } else {
            cur + 1
        };
        if clamp_page(target, self.total_slides.get()) != cur {
            self.nav_to(target);
        }
        true
    }

    fn update_nav_label(&self) {
        let cur = self.current_slide.get();
        let tot = self.total_slides.get();
        self.nav_label.set_label(&format!("{} / {}", cur, tot));
    }

    fn connect_nav_buttons(&self) {
        // Connect once: the handlers hold live Rc/widget handles — they
        // read the CURRENT cells on every click (never a snapshot taken
        // here), so one connection serves every document previewed after.
        // Duplicate handlers would make ▶ advance several pages per click.
        if self.nav_connected.get() {
            return;
        }
        self.nav_connected.set(true);

        {
            let pane = self.clone();
            self.nav_prev.connect_clicked(move |_| {
                pane.nav_to(pane.current_slide.get().saturating_sub(1));
            });
        }

        {
            let pane = self.clone();
            self.nav_next.connect_clicked(move |_| {
                pane.nav_to(pane.current_slide.get() + 1);
            });
        }
    }

    fn show_info(&self, p: &Path) {
        let is_dir = p.is_dir();
        if is_dir {
            self.iicon.set_icon_name(Some("folder-symbolic"));
        } else {
            self.iicon.set_icon_name(Some(info_icon_for(p)));
        }
        while let Some(child) = self.imetadata.first_child() {
            self.imetadata.remove(&child);
        }

        let name = p.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.display().to_string());
        let path = p.parent()
            .map(|parent| parent.display().to_string())
            .unwrap_or_else(|| p.display().to_string());
        let type_description = if is_dir {
            gettext("Folder")
        } else {
            let (content_type, _) = gio::content_type_guess(Some(p), &[]);
            gio::content_type_get_description(content_type.as_str()).to_string()
        };

        let mut rows = vec![
            (gettext("Name"), name),
            (gettext("Where"), path),
            (gettext("Type"), type_description),
        ];
        if let Ok(meta) = std::fs::metadata(p) {
            if is_dir {
                if let Ok(entries) = std::fs::read_dir(p) {
                    rows.push((gettext("Items"), entries.count().to_string()));
                }
            } else {
                rows.push((gettext("Size"), crate::imageinfo::human_size(meta.len())));
            }
            if let Ok(created) = meta.created() {
                rows.push((gettext("Created"), format_metadata_time(created)));
            }
            if let Ok(modified) = meta.modified() {
                rows.push((gettext("Modified"), format_metadata_time(modified)));
            }
        }

        for (row, (label, value)) in rows.into_iter().enumerate() {
            let key = gtk::Label::builder()
                .label(&label)
                .halign(gtk::Align::Start)
                .valign(gtk::Align::Start)
                .css_classes(["caption", "dim-label"])
                .build();
            let value = gtk::Label::builder()
                .label(&value)
                .halign(gtk::Align::Start)
                .valign(gtk::Align::Start)
                .hexpand(true)
                .wrap(true)
                .selectable(true)
                .max_width_chars(28)
                .ellipsize(pango::EllipsizeMode::Middle)
                .build();
            self.imetadata.attach(&key, 0, row as i32, 1, 1);
            self.imetadata.attach(&value, 1, row as i32, 1, 1);
        }
        self.ihint.set_text(if is_dir {
            "Ctrl+Enter — open in terminal"
        } else {
            "Enter — open with default app"
        });
        self.stack.set_visible_child_name("info");
    }
}

fn format_metadata_time(time: std::time::SystemTime) -> String {
    time.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| gtk::glib::DateTime::from_unix_local(duration.as_secs() as i64).ok())
        .and_then(|datetime| datetime.format("%x %X").ok())
        .map(|formatted| formatted.to_string())
        .unwrap_or_else(|| gettext("Unknown"))
}

// ──────────────────────────────────────────────────────────────────────
// Thumbnail generation (PDF first page, video frame)
//
// We shell out to standard desktop tools and cache the resulting PNG under
// ~/.cache/spotty/thumbnails/ keyed by a hash of (path + mtime), so re-previewing
// the same file is instant and a changed file regenerates. If the required tool
// isn't installed, the helper returns None and the caller falls back to the
// info card — no hard dependency.
// ──────────────────────────────────────────────────────────────────────

use std::path::PathBuf;

// We use the freedesktop SHARED thumbnail cache so thumbnails are interoperable
// with Nautilus and other desktop apps:
//
//   $XDG_CACHE_HOME/thumbnails/large/<md5(file-uri)>.png
//
/// Maximum bytes to read from a file for the text preview.
const TEXT_PREVIEW_READ_BYTES: usize = 1 << 20; // 1 MB
/// Maximum bytes to display (after which the preview is truncated).
const TEXT_PREVIEW_DISPLAY_BYTES: usize = 32 << 10; // 32 KB

/// Maximum image dimension (width or height) after downscaling.
const IMAGE_MAX_DIM: u32 = 700;

/// Check if the path is an actual image file suitable for OCR (not PDF/PPTX/office).
fn is_ocrable_image(path: &Path) -> bool {
    path.extension()
        .and_then(|s| s.to_str())
        .map(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "webp" | "tif" | "tiff" | "bmp" | "avif"
                    | "gif" | "ico" | "pnm" | "pgm" | "ppm" | "pbm" | "qoi" | "tga"
                    | "heic" | "heif" | "svg"
            )
        })
        .unwrap_or(false)
}

// ── Worker thread: compute preview payload for any file type ──

fn compute_preview(path: &Path) -> PreviewPayload {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();

    match ext.as_str() {
        // ── Raster images ──
        "png" | "jpg" | "jpeg" | "jpe" | "webp" | "gif" | "bmp" | "svg" | "svgz" | "ico"
        | "tiff" | "tif" | "jxl" | "ppm" | "pgm" | "pbm" | "pnm" | "xpm" | "tga" => {
            let Some(img) = image::open(path).ok() else {
                return PreviewPayload::Info;
            };
            let rgba = img.to_rgba8();
            let (w, h) = rgba.dimensions();
            let (dw, dh, raw) = if h > IMAGE_MAX_DIM || w > IMAGE_MAX_DIM {
                let scale = IMAGE_MAX_DIM as f32 / (w.max(h) as f32);
                let nw = ((w as f32 * scale) as u32).max(1);
                let nh = ((h as f32 * scale) as u32).max(1);
                let resized = image::imageops::resize(
                    &rgba,
                    nw,
                    nh,
                    image::imageops::FilterType::Lanczos3,
                );
                (nw, nh, resized.into_raw())
            } else {
                (w, h, rgba.into_raw())
            };
            PreviewPayload::Image {
                rgba: raw,
                w: dw,
                h: dh,
                total_pages: None, page: None,
            }
        }
        // ── AVIF / HEIC / HEIF ──
        "avif" | "heic" | "heif" => {
            if let Some(png) = avif_to_png_file(path) {
                if let Some(img) = image::open(&png).ok() {
                    let rgba = img.to_rgba8();
                    let (w, h) = rgba.dimensions();
                    let (dw, dh, raw) = if h > IMAGE_MAX_DIM || w > IMAGE_MAX_DIM {
                        let scale = IMAGE_MAX_DIM as f32 / (w.max(h) as f32);
                        let nw = ((w as f32 * scale) as u32).max(1);
                        let nh = ((h as f32 * scale) as u32).max(1);
                        let resized = image::imageops::resize(
                            &rgba,
                            nw,
                            nh,
                            image::imageops::FilterType::Lanczos3,
                        );
                        (nw, nh, resized.into_raw())
                    } else {
                        (w, h, rgba.into_raw())
                    };
                    return PreviewPayload::Image {
                        rgba: raw,
                        w: dw,
                        h: dh,
                        total_pages: None, page: None,
                    };
                }
            }
            PreviewPayload::Info
        }
        // ── PDF: render first page, cache total for nav ──
        "pdf" => {
            let total = pdf_page_count(path).unwrap_or(1);
            store_doc_meta(path, total);
            let cached = cached_pdf_page(path, 1)
                .or_else(|| render_pdf_page(path, 1));
            if let Some(page_path) = cached {
                if let Some(img) = image::open(&page_path).ok() {
                    let rgba = img.to_rgba8();
                    let (w, h) = rgba.dimensions();
                    let (dw, dh, raw) = if h > IMAGE_MAX_DIM || w > IMAGE_MAX_DIM {
                        let scale = IMAGE_MAX_DIM as f32 / (w.max(h) as f32);
                        let nw = ((w as f32 * scale) as u32).max(1);
                        let nh = ((h as f32 * scale) as u32).max(1);
                        let resized = image::imageops::resize(
                            &rgba, nw, nh, image::imageops::FilterType::Lanczos3,
                        );
                        (nw, nh, resized.into_raw())
                    } else {
                        (w, h, rgba.into_raw())
                    };
                    return PreviewPayload::Image {
                        rgba: raw, w: dw, h: dh,
                        total_pages: Some(total), page: Some(1),
                    };
                }
            }
            // Fallback to shared thumbnails or office-style rendering.
            if let Some(thumb_path) = pdf_thumbnail(path).or_else(|| office_thumbnail(path)) {
                if let Some(img) = image::open(&thumb_path).ok() {
                    let rgba = img.to_rgba8();
                    let (w, h) = rgba.dimensions();
                    let (dw, dh, raw) = if h > IMAGE_MAX_DIM || w > IMAGE_MAX_DIM {
                        let scale = IMAGE_MAX_DIM as f32 / (w.max(h) as f32);
                        let nw = ((w as f32 * scale) as u32).max(1);
                        let nh = ((h as f32 * scale) as u32).max(1);
                        let resized = image::imageops::resize(
                            &rgba, nw, nh, image::imageops::FilterType::Lanczos3,
                        );
                        (nw, nh, resized.into_raw())
                    } else {
                        (w, h, rgba.into_raw())
                    };
                    return PreviewPayload::Image {
                        rgba: raw, w: dw, h: dh,
                        total_pages: Some(total), page: Some(1),
                    };
                }
            }
            PreviewPayload::Info
        }
        // ── PPTX family (incl. macro-enabled + templates): render first
        // slide, cache total for nav. (ODP lives in the office arm below —
        // it's parsed as ODF text slides there, with per-slide nav like
        // every other deck format.)
        "pptx" | "ppsx" | "pps" | "pptm" | "ppsm" | "potx" | "potm" => {
            let total = pptx_slide_count(path).unwrap_or(1);
            store_doc_meta(path, total);
            // Native slide render first, then fall back to office_thumbnail
            // (shared thumbnails or the embedded PowerPoint preview image).
            let slide_path = pptx_first_slide_png(path).or_else(|| office_thumbnail(path));
            if let Some(slide_path) = slide_path {
                if let Some(img) = image::open(&slide_path).ok() {
                    let rgba = img.to_rgba8();
                    let (w, h) = rgba.dimensions();
                    let (dw, dh, raw) = if h > IMAGE_MAX_DIM || w > IMAGE_MAX_DIM {
                        let scale = IMAGE_MAX_DIM as f32 / (w.max(h) as f32);
                        let nw = ((w as f32 * scale) as u32).max(1);
                        let nh = ((h as f32 * scale) as u32).max(1);
                        let resized = image::imageops::resize(
                            &rgba, nw, nh, image::imageops::FilterType::Lanczos3,
                        );
                        (nw, nh, resized.into_raw())
                    } else {
                        (w, h, rgba.into_raw())
                    };
                    return PreviewPayload::Image {
                        rgba: raw, w: dw, h: dh,
                        total_pages: Some(total), page: Some(1),
                    };
                }
            }
            PreviewPayload::Info
        }
        // ── Office documents (Word / Excel / ODF / RTF / legacy PPT) ──
        "doc" | "docx" | "odt" | "rtf" | "ott" | "fodt" | "wps" | "xls" | "xlsx" | "ods"
        | "ots" | "fods" | "csv" | "ppt" | "otp" | "fodp" | "odp" => {
            // Native multi-page previews: legacy .ppt slides plus the
            // paginated prose / sheet-grid / ODF-text-slide renderer for
            // everything else. Page 1 renders natively first (so pages 2..N
            // behind the nav bar come from the same render pass), the nav
            // reports total_pages, and when no structured parse exists
            // (.doc/.wps, empty or broken files) we fall back to the
            // shared/embedded thumbnail with the nav hidden.
            let legacy_ppt = ext == "ppt";
            let page1 = if legacy_ppt {
                cached_legacy_ppt_slide(path, 1)
                    .or_else(|| render_legacy_ppt_slide_to_cache(path, 1))
            } else {
                cached_office_page(path, 1).or_else(|| render_office_page_to_cache(path, 1))
            };
            let native = page1.is_some();
            let native_total = if !native {
                1
            } else if legacy_ppt {
                legacy_ppt_slide_count(path).unwrap_or(1)
            } else {
                office_page_count(path).unwrap_or(1)
            };
            if native {
                store_doc_meta(path, native_total);
            }
            let thumb = page1.or_else(|| office_thumbnail(path));
            if let Some(thumb) = thumb {
                if let Some(img) = image::open(&thumb).ok() {
                    let rgba = img.to_rgba8();
                    let (w, h) = rgba.dimensions();
                    let (dw, dh, raw) = if h > IMAGE_MAX_DIM || w > IMAGE_MAX_DIM {
                        let scale = IMAGE_MAX_DIM as f32 / (w.max(h) as f32);
                        let nw = ((w as f32 * scale) as u32).max(1);
                        let nh = ((h as f32 * scale) as u32).max(1);
                        let resized = image::imageops::resize(
                            &rgba, nw, nh, image::imageops::FilterType::Lanczos3,
                        );
                        (nw, nh, resized.into_raw())
                    } else {
                        (w, h, rgba.into_raw())
                    };
                    return PreviewPayload::Image {
                        rgba: raw, w: dw, h: dh,
                        total_pages: native.then_some(native_total),
                        page: native.then_some(1),
                    };
                }
            }
            PreviewPayload::Info
        }
        // ── Archive listing ──
        _ => {
            if let Some((entries, total)) = archive_listing(path) {
                let mut listing = format!("{} entries\n\n", total);
                for e in &entries {
                    listing.push_str(e);
                    listing.push('\n');
                }
                if total > entries.len() {
                    listing.push_str(&format!(
                        "\u{2026} and {} more\n",
                        total - entries.len()
                    ));
                }
                return PreviewPayload::Text { content: listing };
            }
            // ── Audio metadata ──
            if let Some(audio) = audio_preview(path) {
                let mut meta = String::new();
                if let Some(t) = &audio.title {
                    meta.push_str(&format!("Title:  {}\n", t));
                }
                if let Some(a) = &audio.artist {
                    meta.push_str(&format!("Artist: {}\n", a));
                }
                if let Some(a) = &audio.album {
                    meta.push_str(&format!("Album:  {}\n", a));
                }
                if let Some(y) = &audio.year {
                    meta.push_str(&format!("Year:   {}\n", y));
                }
                if let Some(g) = &audio.genre {
                    meta.push_str(&format!("Genre:  {}\n", g));
                }
                if let Some(d) = &audio.duration {
                    meta.push_str(&format!("Length: {}\n", d));
                }
                meta.push_str(&format!(
                    "\n{} \u{00b7} {}",
                    audio.format.to_uppercase(),
                    crate::imageinfo::human_size(audio.size)
                ));
                return PreviewPayload::Text { content: meta };
            }
            // ── Text preview ──
            if let Some(tp) = text_preview_for(path) {
                let display = if tp.truncated {
                    format!(
                        "\u{2026} showing first {} of {} \u{2014}\n\n{}",
                        crate::imageinfo::human_size(TEXT_PREVIEW_DISPLAY_BYTES as u64),
                        crate::imageinfo::human_size(tp.total_size),
                        &tp.text[..tp.text.len().min(TEXT_PREVIEW_DISPLAY_BYTES)]
                    )
                } else if tp.text.is_empty() {
                    "(empty file)".to_string()
                } else {
                    tp.text
                };
                return PreviewPayload::Text { content: display };
            }
            PreviewPayload::Info
        }
    }
}

/// Get PDF page count from pdfinfo (fast, no rendering).
fn pdf_page_count(pdf: &Path) -> Option<usize> {
    let bin = resolve_tool("pdfinfo")?;
    let out = std::process::Command::new(&bin)
        .arg(pdf)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("Pages:") {
            if let Ok(n) = rest.trim().parse::<usize>() {
                return Some(n);
            }
        }
    }
    None
}

/// Count slides in a PPTX by counting ppt/slides/slideN.xml entries in the zip.
/// PowerPoint-Open-XML family: presentations, macro-enabled variants and
/// templates — identical zip containers, so they share the slide pipeline
/// (render, count, nav, cache) exactly.
const PPTX_EXTS: &[&str] = &["pptx", "ppsx", "pps", "pptm", "ppsm", "potx", "potm"];

fn pptx_slide_count(doc: &Path) -> Option<usize> {
    let file = std::fs::File::open(doc).ok()?;
    let mut zip = zip::ZipArchive::new(file).ok()?;
    let count = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().map(|e| e.name().to_string()))
        .filter(|n| n.starts_with("ppt/slides/slide") && n.ends_with(".xml"))
        .count();
    Some(count).filter(|&c| c > 0)
}

/// Check if a cached PDF page PNG exists.
fn cached_pdf_page(pdf: &Path, page: usize) -> Option<PathBuf> {
    let meta = std::fs::metadata(pdf).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let hash = crate::md5::hex(pdf.to_string_lossy().as_bytes());
    let cache_root = dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("spotty/pdf")
        .join(format!("{}-{}", hash, mtime));
    let page_path = cache_root.join(format!("page-{}.png", page));
    page_path.exists().then_some(page_path)
}

/// Render a single PDF page to a cached PNG (using pdftoppm).
fn render_pdf_page(pdf: &Path, page: usize) -> Option<PathBuf> {
    let bin = resolve_tool("pdftoppm")?;
    let meta = std::fs::metadata(pdf).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let hash = crate::md5::hex(pdf.to_string_lossy().as_bytes());
    let cache_root = dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("spotty/pdf")
        .join(format!("{}-{}", hash, mtime));
    let _ = crate::security::private_dir(&cache_root);
    let prefix = cache_root.join(format!("page-{}", page));
    let prefix_str = prefix.to_string_lossy().to_string();
    let status = std::process::Command::new(&bin)
        .args([
            "-png",
            "-r",
            "200",
            "-scale-to",
            "1600",
            "-f",
            &page.to_string(),
            "-l",
            &page.to_string(),
            "-singlefile",
        ])
        .arg(pdf)
        .arg(&prefix_str)
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    let out = cache_root.join(format!("page-{}.png", page));
    if out.exists() {
        Some(out)
    } else {
        let alt = PathBuf::from(format!("{}-{}.png", prefix_str, page));
        if alt.exists() {
            let _ = std::fs::rename(&alt, &out);
            Some(out)
        } else {
            None
        }
    }
}

/// Check if a cached PPTX slide PNG exists.
fn cached_pptx_slide(doc: &Path, slide: usize) -> Option<PathBuf> {
    let mtime = doc_mtime_secs(doc);
    let hash = crate::md5::hex(doc.to_string_lossy().as_bytes());
    let cache_root = dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(format!("spotty/pptx/v{}", PPTX_RENDER_VERSION))
        .join(format!("{}-{}", hash, mtime));
    let slide_path = cache_root.join(format!("slide-{}.png", slide));
    if sane_png_file(&slide_path) {
        Some(slide_path)
    } else {
        if slide_path.exists() {
            log::info!("pptx: dropping corrupt cached slide {}", slide_path.display());
            let _ = std::fs::remove_file(&slide_path);
        }
        None
    }
}

/// A cached slide PNG must start with the PNG signature and hold real
/// content. A truncated or bogus file must not be served forever — it gets
/// dropped here so the next caller re-renders instead (self-healing cache).
fn sane_png_file(path: &Path) -> bool {
    use std::io::Read;
    let Ok(meta) = std::fs::metadata(path) else { return false; };
    if meta.len() < 256 {
        return false;
    }
    let mut magic = [0u8; 8];
    let Ok(mut f) = std::fs::File::open(path) else { return false; };
    f.read_exact(&mut magic).is_ok() && &magic[..] == b"\x89PNG\r\n\x1a\n"
}

/// File modification time in whole seconds since the epoch (0 when unknown).
fn doc_mtime_secs(doc: &Path) -> u64 {
    std::fs::metadata(doc)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `(mtime secs, size)` — the live-refresh change key for one file
/// (`PreviewPane::refresh_if_stale`). None when unreadable.
fn file_stamp(p: &Path) -> Option<(u64, u64)> {
    let m = std::fs::metadata(p).ok()?;
    let mt = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Some((mt, m.len()))
}

/// Render a single PPTX slide to a cached PNG.
fn render_pptx_slide(doc: &Path, slide: usize) -> Option<PathBuf> {
    let Some(layout) = parse_pptx_slide(doc, slide) else {
        return None;
    };
    let png = render_slide_layout(&layout)?;

    let mtime = doc_mtime_secs(doc);
    let hash = crate::md5::hex(doc.to_string_lossy().as_bytes());
    let cache_root = dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(format!("spotty/pptx/v{}", PPTX_RENDER_VERSION))
        .join(format!("{}-{}", hash, mtime));
    let _ = crate::security::private_dir(&cache_root);
    let slide_path = cache_root.join(format!("slide-{}.png", slide));
    if crate::security::write_private(&slide_path, &png).is_ok() {
        log::info!(
            "pptx: rendered slide {} of {} (render v{}) -> {}",
            slide,
            doc.display(),
            PPTX_RENDER_VERSION,
            slide_path.display()
        );
        Some(slide_path)
    } else {
        None
    }
}

/// First-slide PNG for `doc`: cached if possible, rendered on demand
/// otherwise. Shared by the preview pane and the result-row thumbnails so
/// both surfaces always show the same faithful render.
pub(crate) fn pptx_first_slide_png(doc: &Path) -> Option<PathBuf> {
    let ext = doc
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase());
    if !ext.as_deref().is_some_and(|e| PPTX_EXTS.contains(&e)) {
        return None;
    }
    cached_pptx_slide(doc, 1).or_else(|| render_pptx_slide(doc, 1))
}

/// Clamp a requested page/slide index into the valid `1..=total` range
/// (a document always has at least a first page).
fn clamp_page(n: usize, total: usize) -> usize {
    n.clamp(1, total.max(1))
}

/// Versioned cache path for office page `n` (1-based). Page 1 shares
/// `render_cache_path` so result rows and the preview pane serve the exact
/// same image; further pages live next to it with a `-p{n}` suffix.
fn office_page_cache_path(doc: &Path, n: usize) -> PathBuf {
    let base = render_cache_path(doc);
    if n <= 1 {
        return base;
    }
    let stem = base
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    base.with_file_name(format!("{}-p{}.png", stem, n))
}

fn cached_office_page(doc: &Path, n: usize) -> Option<PathBuf> {
    let path = office_page_cache_path(doc, n);
    if sane_png_file(&path) {
        return Some(path);
    }
    if path.exists() {
        log::info!(
            "office: dropping corrupt cached page {}",
            path.display()
        );
        let _ = std::fs::remove_file(&path);
    }
    None
}

/// Prepare (memoized) and render office page `n`, write it to the versioned
/// cache, and return its path. Page indices clamp into the prepared range —
/// the nav clamps before calling, but this keeps direct callers safe too.
fn render_office_page_to_cache(doc: &Path, n: usize) -> Option<PathBuf> {
    let prep = prepare_office(doc)?;
    let n = clamp_page(n, prep.pages.len());
    let png = render_office_page(&prep, n)?;
    let out = office_page_cache_path(doc, n);
    if crate::security::write_private(&out, &png).is_ok() {
        log::debug!(
            "office: rendered page {} of {} for {}",
            n,
            prep.pages.len(),
            doc.display()
        );
        Some(out)
    } else {
        None
    }
}

/// Total native page count (prose pages / sheet chunks / ODF slides) for
/// documents Spotty renders itself; None when the type has no structured
/// parser (.doc/.wps) or the file couldn't be read.
fn office_page_count(doc: &Path) -> Option<usize> {
    prepare_office(doc).map(|p| p.pages.len().max(1))
}

/// Page caption override (sheet name / part) driving the preview caption
/// while paging. None for prose/slides (their caption stays the file info).
fn office_page_caption(doc: &Path, n: usize) -> Option<String> {
    let prep = prepare_office(doc)?;
    prep.pages.get(n.max(1) - 1)?.caption.clone()
}

/// True when previews of `ext` (lowercase) are native multi-page and can
/// keep their page position across a live file refresh.
fn nav_native_ext(ext: &str) -> bool {
    matches!(ext, "pdf" | "ppt") || PPTX_EXTS.contains(&ext) || OFFICE_PAGE_EXTS.contains(&ext)
}

/// Resolve the PNG for page/slide `n` (1-based) of a multi-page document:
/// disk cache first, render on demand otherwise. Shared by the nav bar
/// buttons, the wheel stepper, and the staleness refresh so all three step
/// through identical logic. None for types without per-page rendering —
/// the caller then falls back to pre-rendered `slide_paths`.
fn resolve_page_png(doc: &Path, n: usize) -> Option<PathBuf> {
    let ext = doc.extension().and_then(|s| s.to_str()).unwrap_or("");
    if ext.eq_ignore_ascii_case("pdf") {
        cached_pdf_page(doc, n).or_else(|| render_pdf_page(doc, n))
    } else if PPTX_EXTS.iter().any(|e| ext.eq_ignore_ascii_case(e)) {
        cached_pptx_slide(doc, n).or_else(|| render_pptx_slide(doc, n))
    } else if ext.eq_ignore_ascii_case("ppt") {
        cached_legacy_ppt_slide(doc, n).or_else(|| render_legacy_ppt_slide_to_cache(doc, n))
    } else if OFFICE_PAGE_EXTS.iter().any(|e| ext.eq_ignore_ascii_case(e)) {
        cached_office_page(doc, n).or_else(|| render_office_page_to_cache(doc, n))
    } else {
        None
    }
}

struct TextPreview {
    text: String,
    truncated: bool,
    total_size: u64,
}

/// Try to extract a text preview from any file.
/// Returns None for binary files (NUL-heavy) or unreadable files.
fn text_preview_for(path: &Path) -> Option<TextPreview> {
    let meta = std::fs::metadata(path).ok()?;
    let total_size = meta.len();
    let mut bytes = Vec::new();
    std::fs::File::open(path).ok()?.take(TEXT_PREVIEW_READ_BYTES as u64 + 1)
        .read_to_end(&mut bytes).ok()?;
    let read_len = bytes.len().min(TEXT_PREVIEW_READ_BYTES);

    // Binary detection: scan first 8 KB for NUL bytes.
    let scan_len = read_len.min(8192);
    let nul_count = bytes[..scan_len].iter().filter(|&&b| b == 0).count();
    if scan_len > 0 && (nul_count as f64) / (scan_len as f64) > 0.003 {
        return None;
    }

    let text = if let Ok(s) = std::str::from_utf8(&bytes[..read_len]) {
        s.to_string()
    } else {
        // Mostly-text file with some invalid UTF-8: lossy decode.
        String::from_utf8_lossy(&bytes[..read_len])
            .replace('\0', "")
    };

    let text = text.replace('\r', "");
    let truncated = bytes.len() > read_len;
    Some(TextPreview { text, truncated, total_size })
}

// ── Archive listing (zip/tar/tar.gz) ──

const ARCHIVE_MAX_ENTRIES: usize = 50;

fn archive_listing(path: &Path) -> Option<(Vec<String>, usize)> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "zip" | "cbz" | "epub" | "jar" | "whl" | "apk" => zip_listing(path),
        "tar" => tar_listing(&mut std::fs::File::open(path).ok()?),
        "gz" => {
            // Only handle .tar.gz — single-file gzip isn't an archive.
            let stem = path.file_stem()?.to_str()?;
            if stem.to_ascii_lowercase().ends_with(".tar") {
                let f = std::fs::File::open(path).ok()?;
                let mut gz = flate2::read::GzDecoder::new(f);
                tar_listing(&mut gz)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn zip_listing(path: &Path) -> Option<(Vec<String>, usize)> {
    let file = std::fs::File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;
    let total = archive.len();
    let mut entries = Vec::new();
    let limit = total.min(ARCHIVE_MAX_ENTRIES);
    for i in 0..limit {
        if let Ok(entry) = archive.by_index(i) {
            let size = entry.size();
            let name = entry.name().to_string();
            entries.push(format!("{}  {}", name, crate::imageinfo::human_size(size)));
        }
    }
    Some((entries, total))
}

fn tar_listing(reader: &mut dyn Read) -> Option<(Vec<String>, usize)> {
    let mut entries = Vec::new();
    let mut count = 0usize;
    let mut header = [0u8; 512];
    // Cap decompressed scanning as well as allocations. A tar.gz bomb must
    // not monopolize a worker while listing the first few files.
    let mut reader = reader.take(64 * 1024 * 1024);
    for _ in 0..4096 {
        if reader.read_exact(&mut header).is_err() || header.iter().all(|&b| b == 0) { break; }
        let size_str = std::str::from_utf8(&header[124..136]).ok()?
            .trim_matches(|c: char| c == '\0' || c.is_ascii_whitespace());
        let size = u64::from_str_radix(size_str, 8).ok()?;
        let typeflag = header[156];
        if typeflag == b'0' || typeflag == 0 {
            let name = std::str::from_utf8(&header[0..100]).unwrap_or("")
                .trim_end_matches('\0');
            count += 1;
            if entries.len() < ARCHIVE_MAX_ENTRIES {
                entries.push(format!("{}  {}", name, crate::imageinfo::human_size(size)));
            }
        }
        let skip = size.checked_add(511)? / 512 * 512;
        // Stream into a fixed-size internal buffer, never allocate based on
        // the untrusted size declared by a tar header.
        let copied = std::io::copy(&mut reader.by_ref().take(skip), &mut std::io::sink()).ok()?;
        if copied != skip { break; }
    }
    Some((entries, count))
}

// ── Audio metadata + cover art ──

struct AudioPreview {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    year: Option<String>,
    genre: Option<String>,
    duration: Option<String>,
    cover: Option<Vec<u8>>,
    cover_mime: Option<String>,
    format: String,
    size: u64,
}

fn audio_preview(path: &Path) -> Option<AudioPreview> {
    let file = std::fs::File::open(path).ok()?;
    let meta = std::fs::metadata(path).ok()?;
    let probe = symphonia::default::get_probe();
    let hint_opts = symphonia::core::meta::MetadataOptions {
        limit_metadata_bytes: symphonia::core::meta::Limit::Maximum(2 << 20),
        limit_visual_bytes: symphonia::core::meta::Limit::Maximum(4 << 20),
    };
    let mss = symphonia::core::io::MediaSourceStream::new(
        Box::new(file),
        Default::default(),
    );
    let mut result = probe
        .format(
            &symphonia::core::probe::Hint::new(),
            mss,
            &symphonia::core::formats::FormatOptions::default(),
            &hint_opts,
        )
        .ok()?;

    let mut title = None;
    let mut artist = None;
    let mut album = None;
    let mut year = None;
    let mut genre = None;
    let mut cover: Option<Vec<u8>> = None;
    let mut cover_mime: Option<String> = None;

    if let Some(meta) = result.metadata.get().and_then(|mut m| m.skip_to_latest().cloned()) {
        for tag in meta.tags() {
            match tag.std_key {
                Some(symphonia::core::meta::StandardTagKey::TrackTitle) => {
                    title = Some(tag.value.to_string());
                }
                Some(symphonia::core::meta::StandardTagKey::Artist) => {
                    artist = Some(tag.value.to_string());
                }
                Some(symphonia::core::meta::StandardTagKey::Album) => {
                    album = Some(tag.value.to_string());
                }
                Some(symphonia::core::meta::StandardTagKey::Date) => {
                    year = Some(tag.value.to_string());
                }
                Some(symphonia::core::meta::StandardTagKey::Genre) => {
                    genre = Some(tag.value.to_string());
                }
                _ => {}
            }
        }
        for visual in meta.visuals() {
            if cover.is_none() {
                cover = Some(visual.data.to_vec());
                cover_mime = Some(visual.media_type.clone());
            }
        }
    }

    if title.is_none() || cover.is_none() {
        let mut fmt_meta = result.format.metadata();
        if let Some(meta) = fmt_meta.skip_to_latest().cloned() {
            for tag in meta.tags() {
                if title.is_none() && matches!(
                    tag.std_key,
                    Some(symphonia::core::meta::StandardTagKey::TrackTitle)
                ) {
                    title = Some(tag.value.to_string());
                }
            }
            if cover.is_none() {
                for visual in meta.visuals() {
                    cover = Some(visual.data.to_vec());
                    cover_mime = Some(visual.media_type.clone());
                    break;
                }
            }
        }
    }

    let fmt_name = path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("audio")
        .to_uppercase();

    Some(AudioPreview {
        title,
        artist,
        album,
        year,
        genre,
        duration: None,
        cover,
        cover_mime,
        format: fmt_name,
        size: meta.len(),
    })
}

// The cache key is md5() of the file's "file://" URI (per the spec). Reading
// this location lets Spotty reuse thumbnails Nautilus already generated; writing
// here lets Nautilus reuse the ones Spotty generates. This works identically
// inside a Flatpak sandbox as long as ~/.cache is mapped in (it is by default
// for the app's own cache, and the shared thumbnail dir can be granted).

/// Root of the freedesktop thumbnail cache (~/.cache/thumbnails).
/// Load an image file into a Picture via an explicit gdk::Texture decode. Using
/// a Texture (rather than Picture::set_filename, which loads lazily) guarantees
/// the image is decoded immediately from the just-written file, avoiding any
/// race where the widget shows nothing because the file was loaded lazily.
fn set_picture_from_file(image: &gtk::Picture, path: &Path) {
    match gtk::gdk::Texture::from_filename(path) {
        Ok(texture) => image.set_paintable(Some(&texture)),
        Err(e) => {
            log::debug!("preview: native load failed for {}: {}", path.display(), e);
            // Fall back to decoding with the `image` crate (supports AVIF,
            // HEIF, JXL, QOI, TGA and other formats GTK doesn't handle
            // natively) and converting to a gdk::Texture via PNG bytes.
            if let Some(tex) = texture_from_image_crate(path) {
                image.set_paintable(Some(&tex));
            } else {
                // Last resort: lazy file loading (works for formats GTK
                // discovers at runtime via GdkPixbuf modules).
                image.set_filename(Some(path));
            }
        }
    }
}

/// Convert AVIF/HEIC to PNG using libheif-rs (pure Rust, no ffmpeg).
fn avif_to_png_file(path: &Path) -> Option<PathBuf> {
    let out = dirs::cache_dir()?
        .join("spotty")
        .join("avif-converted")
        .join(format!("{}.png", crate::md5::hex(path.to_string_lossy().as_bytes())));
    if out.exists() {
        return Some(out);
    }
    let parent = out.parent()?;
    let _ = crate::security::private_dir(parent);

    let bytes = std::fs::read(path).ok()?;
    let rgba = decode_heif_to_rgba(&bytes).ok()?;
    rgba.save_with_format(&out, image::ImageFormat::Png).ok()?;
    out.exists().then_some(out)
}

/// Decode AVIF/HEIC to a DynamicImage using libheif-rs (pure Rust).
fn decode_heif_to_rgba(bytes: &[u8]) -> Result<image::DynamicImage, Box<dyn std::error::Error>> {
    use libheif_rs::{ColorSpace, HeifContext, LibHeif, RgbChroma};

    let libheif = LibHeif::new();
    let ctx = HeifContext::read_from_bytes(bytes)?;
    let handle = ctx.primary_image_handle()?;
    let image = libheif.decode(&handle, ColorSpace::Rgb(RgbChroma::Rgba), None)?;
    let planes = image.planes();
    let interleaved = planes.interleaved.ok_or("no interleaved plane")?;

    let width = interleaved.width as usize;
    let height = interleaved.height as usize;
    let stride = interleaved.stride as usize;
    let data = interleaved.data;

    let mut rgba = image::RgbaImage::new(width as u32, height as u32);
    for y in 0..height {
        let row_start = y * stride;
        for x in 0..width {
            let idx = row_start + x * 4;
            let pixel = image::Rgba([data[idx], data[idx + 1], data[idx + 2], data[idx + 3]]);
            rgba.put_pixel(x as u32, y as u32, pixel);
        }
    }
    Ok(image::DynamicImage::ImageRgba8(rgba))
}

/// Use the `image` crate to decode an image file that GTK can't load natively
/// (e.g. AVIF, HEIF, JXL, QOI, TGA) and convert it to a `gdk::Texture` via
/// in-memory PNG bytes.  Returns `None` on any failure.
fn texture_from_image_crate(path: &Path) -> Option<gtk::gdk::Texture> {
    let img = image::open(path).ok()?;
    // Convert to RGBA8 for maximum compatibility.
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let bytes = rgba.into_raw();
    // Build a gdk::Texture from the raw pixel data via a GdkPixbuf.
    let pixbuf = gtk::gdk_pixbuf::Pixbuf::from_mut_slice(
        bytes,
        gtk::gdk_pixbuf::Colorspace::Rgb,
        true, // has alpha
        8,    // bits per sample
        w as i32,
        h as i32,
        w as i32 * 4, // rowstride
    );
    Some(gtk::gdk::Texture::for_pixbuf(&pixbuf))
}

fn fd_cache_root() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("thumbnails")
}

/// The "large" (256px) thumbnail directory, created if needed.
fn thumb_cache_dir() -> PathBuf {
    let dir = fd_cache_root().join("large");
    let _ = crate::security::private_dir(&dir);
    dir
}

/// Build the canonical file:// URI for a path (used as the cache key).
fn file_uri(path: &Path) -> String {
    // gio gives us a correctly percent-encoded URI matching what Nautilus uses.
    gtk::gio::File::for_path(path).uri().to_string()
}

/// The freedesktop shared-cache thumbnail path for `path`:
/// ~/.cache/thumbnails/large/<md5(uri)>.png
fn thumb_cache_path(path: &Path) -> PathBuf {
    let uri = file_uri(path);
    let hash = crate::md5::hex(uri.as_bytes());
    thumb_cache_dir().join(format!("{}.png", hash))
}

/// Bump this when the office/pptx RENDER code changes, so old cached renders are
/// invalidated and regenerated instead of being served stale forever.
const RENDER_VERSION: u32 = 26;
const PPTX_RENDER_VERSION: u32 = 6;

/// A Spotty-private cache path for thumbnails Spotty RENDERS itself (office docs,
/// pptx layout). Kept separate from the shared cache (which we only write real
/// extracted thumbnails to), and versioned so changing the renderer invalidates
/// old output. Lives under ~/.cache/spotty/render-cache/.
fn render_cache_path(path: &Path) -> PathBuf {
    let uri = file_uri(path);
    let mtime = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let key = format!("{}|{}|v{}", uri, mtime, RENDER_VERSION);
    let hash = crate::md5::hex(key.as_bytes());
    let dir = dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("spotty/render-cache");
    let _ = crate::security::private_dir(&dir);
    dir.join(format!("{}.png", hash))
}

/// Look for an EXISTING shared-cache thumbnail (any size), checking the standard
/// freedesktop subdirectories newest-spec-first. Returns it only if it is at
/// least as new as the source file (stale thumbnails are ignored).
fn existing_shared_thumb(path: &Path) -> Option<PathBuf> {
    let uri = file_uri(path);
    let hash = crate::md5::hex(uri.as_bytes());
    let src_mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    for size_dir in ["xx-large", "x-large", "large", "normal"] {
        let candidate = fd_cache_root().join(size_dir).join(format!("{}.png", hash));
        if candidate.exists() {
            // Reject stale thumbnails (older than the file they represent).
            if let (Ok(tm), Some(sm)) = (
                std::fs::metadata(&candidate).and_then(|m| m.modified()),
                src_mtime,
            ) {
                if tm < sm {
                    continue;
                }
            }
            return Some(candidate);
        }
    }
    None
}

/// Is `tool` on the PATH?
/// Resolve a helper tool to a runnable path.
///
/// Designed for Flatpak from the start: inside a Flatpak sandbox, bundled
/// binaries live in `/app/bin`, which is also on PATH inside the sandbox — but
/// we check it explicitly first so behaviour is identical whether bundled or
/// host-provided. Outside a sandbox we just find it on the host PATH. This means
/// the SAME code works today (host tools) and later (bundled in the Flatpak),
/// with no branching on "am I in a sandbox".
pub(crate) fn resolve_tool(tool: &str) -> Option<PathBuf> {
    // 1. Bundled location (Flatpak `/app/bin`, or a future portable layout).
    let bundled = PathBuf::from("/app/bin").join(tool);
    if bundled.exists() {
        return Some(bundled);
    }
    // 2. ~/.local/bin (user-local static/binary installs, e.g. downloaded ffmpeg).
    if let Some(home) = dirs::home_dir() {
        let local_bin = home.join(".local/bin").join(tool);
        if local_bin.exists() {
            return Some(local_bin);
        }
    }
    // 3. Anything on PATH (host tools now; also covers /app/bin inside Flatpak).
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let cand = dir.join(tool);
            if cand.exists() {
                return Some(cand);
            }
        }
    }
    None
}

/// Convenience boolean wrapper.
pub(crate) fn has_tool(tool: &str) -> bool {
    resolve_tool(tool).is_some()
}

/// Render the first page of a PDF to a cached PNG. Uses `pdftoppm`
/// (poppler-utils), which is installed on essentially every Linux desktop.
pub(crate) fn pdf_thumbnail(pdf: &Path) -> Option<PathBuf> {
    if let Some(existing) = existing_shared_thumb(pdf) {
        return Some(existing);
    }
    let out = thumb_cache_path(pdf);
    if out.exists() {
        return Some(out);
    }
    if !has_tool("pdftoppm") {
        return None;
    }
    // pdftoppm writes <prefix>.png (or <prefix>-1.png depending on version) for
    // a single page. We pass the prefix WITHOUT extension and then locate the
    // file it actually produced.
    let prefix = out.with_extension(""); // strip .png; we'll find the real output
    let prefix_str = prefix.to_string_lossy().to_string();
    let bin = resolve_tool("pdftoppm")?;
    let status = std::process::Command::new(&bin)
        .args([
            "-png",
            "-f",
            "1",
            "-l",
            "1", // first page only
            "-scale-to",
            "400",         // longest side 400px
            "-singlefile", // produce exactly <prefix>.png, no page suffix
        ])
        .arg(pdf)
        .arg(&prefix_str)
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    // With -singlefile, the output is exactly prefix + ".png" == `out`.
    if out.exists() {
        Some(out)
    } else {
        // Fallback for older pdftoppm without -singlefile: it may have written
        // "<prefix>-1.png". Try to find and rename it.
        let alt = PathBuf::from(format!("{}-1.png", prefix_str));
        if alt.exists() && std::fs::rename(&alt, &out).is_ok() {
            Some(out)
        } else {
            None
        }
    }
}

/// Render ALL pages of a PDF to cached PNGs. Returns vec of (path, page_number).
fn render_pdf_all_pages(pdf: &Path) -> Option<(Vec<PathBuf>, usize)> {
    let bin = resolve_tool("pdftoppm")?;
    // Use a hash of path + mtime for cache.
    let meta = std::fs::metadata(pdf).ok()?;
    let mtime = meta.modified().ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let hash = crate::md5::hex(pdf.to_string_lossy().as_bytes());
    let cache_root = dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("spotty/pdf")
        .join(format!("{}-{}", hash, mtime));
    let _ = crate::security::private_dir(&cache_root);

    // Check if pages are already cached.
    let mut cached = Vec::new();
    let mut i = 1;
    loop {
        let page = cache_root.join(format!("page-{}.png", i));
        if !page.exists() {
            break;
        }
        cached.push(page);
        i += 1;
    }
    let total_pages = if !cached.is_empty() {
        cached.len()
    } else {
        // Render all pages.
        let prefix = cache_root.join("page");
        let prefix_str = prefix.to_string_lossy().to_string();
        let status = std::process::Command::new(&bin)
            .args(["-png", "-r", "150", "-scale-to", "400"])
            .arg(pdf)
            .arg(&prefix_str)
            .status()
            .ok();
        if !status.map(|s| s.success()).unwrap_or(false) {
            return None;
        }
        let mut count = 0;
        loop {
            let page = cache_root.join(format!("page-{}.png", count + 1));
            if !page.exists() {
                break;
            }
            cached.push(page);
            count += 1;
        }
        if count == 0 {
            return None;
        }
        count
    };
    let paths: Vec<PathBuf> = cached.into_iter().collect();
    Some((paths, total_pages))
}

/// Produce a preview thumbnail for an office document from content embedded
/// inside the file itself (see extract_embedded_thumbnail) or rendered natively
/// — never by shelling out to an external converter, so behavior is identical
/// on every machine. Returns None when nothing usable exists, so the caller
/// falls back to the type-specific info card.
pub(crate) fn office_thumbnail(doc: &Path) -> Option<PathBuf> {
    let ext = doc
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_lowercase());
    // A real, externally-generated thumbnail (Nautilus, office suites, or one
    // we extracted before) always wins when it exists and is fresh.
    if let Some(existing) = existing_shared_thumb(doc) {
        return Some(existing);
    }

    if matches!(ext.as_deref(), Some("ppt")) {
        // Legacy binary .ppt: our own per-slide text-atom renderer (the
        // result-row thumbnail shows slide 1; the preview pane renders
        // slides 2..N on demand through the nav bar).
        return cached_legacy_ppt_slide(doc, 1)
            .or_else(|| render_legacy_ppt_slide_to_cache(doc, 1));
    }

    log::debug!("office preview: generating for {}", doc.display());

    // PowerPoint files need layout correctness first. Prefer the document's real
    // embedded preview image over Spotty's synthetic layout renderer, but keep it
    // in our private versioned cache so stale shared thumbnails are avoided.
    let out = render_cache_path(doc);
    if ext.as_deref().is_some_and(|e| PPTX_EXTS.contains(&e)) {
        if out.exists() {
            return Some(out);
        }
        if let Some(bytes) = extract_embedded_thumbnail(doc) {
            log::debug!(
                "office preview: using embedded PowerPoint thumbnail image ({} bytes)",
                bytes.len()
            );
            if write_embedded_thumbnail_png(&bytes, &out) {
                return Some(out);
            }
        }
        return None;
    }

    // STEP 1 — the best result: extract the PREVIEW IMAGE that Office embeds
    // inside the document (JPEG/PNG). Great for PowerPoint and ODF. This is a
    // real rendered image, so we store it in the SHARED cache for reuse.
    if let Some(bytes) = extract_embedded_thumbnail(doc) {
        log::debug!(
            "office preview: using embedded thumbnail ({} bytes)",
            bytes.len()
        );
        let out = thumb_cache_path(doc);
        if crate::security::write_private(&out, &bytes).is_ok() {
            return Some(out);
        }
    }

    // For thumbnails WE render (not real document previews), use a private,
    // VERSION-TAGGED cache path so changing the render code regenerates them
    // instead of serving a stale old render forever.
    if out.exists() {
        return Some(out);
    }

    // STEP 2 — native paginated render: page 1 shares `render_cache_path`
    // with the preview pane, so rows and pane show the same image. Falls
    // back to the legacy single-content card for types without a
    // structured parser (.doc/.wps strings card).
    if let Some(prep) = prepare_office(doc) {
        if let Some(png) = render_office_page(&prep, 1) {
            if crate::security::write_private(&out, &png).is_ok() {
                return Some(out);
            }
        }
    }

    // Legacy single-content card (Word/Excel text/data) drawn with Cairo.
    match extract_office_content(doc) {
        Some(content) => {
            log::debug!(
                "office preview: extracted content, {} lines",
                content.lines.len()
            );
            if let Some(png) = render_content_preview(&content) {
                if crate::security::write_private(&out, &png).is_ok() {
                    return Some(out);
                }
            }
            log::debug!("office preview: render failed");
        }
        None => log::debug!(
            "office preview: no content extracted from {}",
            doc.display()
        ),
    }

    None
}

fn write_embedded_thumbnail_png(bytes: &[u8], out: &Path) -> bool {
    const TARGET_LONG_EDGE: i32 = 3200;

    let loader = gdk_pixbuf::PixbufLoader::new();
    if loader.write(bytes).is_err() || loader.close().is_err() {
        return crate::security::write_private(out, bytes).is_ok();
    }

    let Some(pixbuf) = loader.pixbuf() else {
        return crate::security::write_private(out, bytes).is_ok();
    };

    let width = pixbuf.width().max(1);
    let height = pixbuf.height().max(1);
    let long_edge = width.max(height);
    let image = if long_edge < TARGET_LONG_EDGE {
        let scale = TARGET_LONG_EDGE as f64 / long_edge as f64;
        let scaled_w = ((width as f64 * scale).round() as i32).max(1);
        let scaled_h = ((height as f64 * scale).round() as i32).max(1);
        pixbuf
            .scale_simple(
                scaled_w,
                scaled_h,
                if long_edge <= 1600 {
                    gdk_pixbuf::InterpType::Nearest
                } else {
                    gdk_pixbuf::InterpType::Hyper
                },
            )
            .unwrap_or(pixbuf)
    } else {
        pixbuf
    };
    let image = image.copy().unwrap_or(image);
    sharpen_pixbuf(&image, 2.6);
    sharpen_pixbuf(&image, 1.7);
    boost_pixbuf_contrast(&image, 1.2);

    image.savev(out, "png", &[("compression", "6")]).is_ok() || crate::security::write_private(out, bytes).is_ok()
}

fn sharpen_pixbuf(pixbuf: &gdk_pixbuf::Pixbuf, amount: f32) {
    let width = pixbuf.width().max(0) as usize;
    let height = pixbuf.height().max(0) as usize;
    if width < 3 || height < 3 {
        return;
    }

    let rowstride = pixbuf.rowstride() as usize;
    let channels = pixbuf.n_channels().max(0) as usize;
    if channels < 3 {
        return;
    }

    let src = unsafe { pixbuf.pixels().to_vec() };
    let dst = unsafe { pixbuf.pixels() };

    for y in 1..(height - 1) {
        for x in 1..(width - 1) {
            let idx = y * rowstride + x * channels;
            for c in 0..3 {
                let center = src[idx + c] as i32;
                let left = src[idx - channels + c] as i32;
                let right = src[idx + channels + c] as i32;
                let up = src[idx - rowstride + c] as i32;
                let down = src[idx + rowstride + c] as i32;
                let avg = (left + right + up + down) / 4;
                let delta = center - avg;
                let value = center as f32 + (delta as f32 * amount);
                dst[idx + c] = value.clamp(0.0, 255.0) as u8;
            }
        }
    }
}

fn boost_pixbuf_contrast(pixbuf: &gdk_pixbuf::Pixbuf, amount: f32) {
    let channels = pixbuf.n_channels().max(0) as usize;
    if channels < 3 {
        return;
    }

    let rowstride = pixbuf.rowstride().max(0) as usize;
    let width = pixbuf.width().max(0) as usize;
    let height = pixbuf.height().max(0) as usize;
    let pixels = unsafe { pixbuf.pixels() };

    for y in 0..height {
        for x in 0..width {
            let idx = y * rowstride + x * channels;
            for c in 0..3 {
                let v = pixels[idx + c] as f32 / 255.0;
                let adjusted = ((v - 0.5) * amount + 0.5).clamp(0.0, 1.0);
                pixels[idx + c] = (adjusted * 255.0).round() as u8;
            }
        }
    }
}

/// Open an OOXML/ODF file as a ZIP and return the bytes of an embedded preview
/// image IF it is in a format GTK can display (JPEG or PNG). Returns None when
/// there is no thumbnail, or it is an EMF/WMF metafile we can't render.
fn extract_embedded_thumbnail(doc: &Path) -> Option<Vec<u8>> {
    let file = std::fs::File::open(doc).ok()?;
    let mut zip = zip::ZipArchive::new(file).ok()?;

    // Candidate entry names, in priority order. OOXML first, then ODF.
    const CANDIDATES: &[&str] = &[
        "docProps/thumbnail.jpeg",
        "docProps/thumbnail.jpg",
        "docProps/thumbnail.png",
        "Thumbnails/thumbnail.png",
    ];

    for name in CANDIDATES {
        if let Ok(mut entry) = zip.by_name(name) {
            use std::io::Read;
            let mut buf = Vec::new();
            if entry.by_ref().take(8 * 1024 * 1024 + 1).read_to_end(&mut buf).is_ok()
                && !buf.is_empty() && buf.len() <= 8 * 1024 * 1024 {
                return Some(buf);
            }
        }
    }
    // Note: we intentionally skip docProps/thumbnail.emf and .wmf — GdkPixbuf
    // can't decode Windows metafiles, so there's nothing we could display.
    None
}

/// Pick a type-appropriate symbolic icon for the info card, so files whose
/// visual thumbnail can't be generated still show a recognizable icon.
fn info_icon_for(p: &Path) -> &'static str {
    let ext = p
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_lowercase());
    match ext.as_deref() {
        Some("pdf") => "x-office-document-symbolic",
        Some("ppt" | "pptx" | "pptm" | "ppsm" | "potx" | "potm" | "odp" | "otp" | "fodp" | "pps" | "ppsx") => {
            "x-office-presentation-symbolic"
        }
        Some("doc" | "docx" | "odt" | "rtf" | "ott" | "fodt" | "wps") => "x-office-document-symbolic",
        Some("xls" | "xlsx" | "ods" | "ots" | "fods" | "csv") => "x-office-spreadsheet-symbolic",
        Some(
            "mp4" | "mkv" | "webm" | "mov" | "avi" | "wmv" | "flv" | "m4v" | "mpeg" | "mpg"
            | "m2ts" | "mts" | "ogv" | "3gp" | "3g2" | "asf" | "rm" | "rmvb" | "vob" | "divx"
            | "f4v" | "mxf",
        ) => "video-x-generic-symbolic",
        Some("mp3" | "flac" | "ogg" | "wav" | "m4a" | "opus" | "aac" | "wma") => {
            "audio-x-generic-symbolic"
        }
        Some(
            "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "svg" | "tiff" | "tif" | "avif"
            | "heic" | "heif" | "ico",
        ) => "image-x-generic-symbolic",
        Some("zip" | "tar" | "gz" | "xz" | "bz2" | "7z" | "rar" | "zst") => {
            "package-x-generic-symbolic"
        }
        Some(
            "rs" | "py" | "js" | "ts" | "c" | "h" | "cpp" | "go" | "java" | "rb" | "sh" | "lua"
            | "sql",
        ) => "text-x-generic-symbolic",
        _ => "text-x-generic-symbolic",
    }
}

// ──────────────────────────────────────────────────────────────────────
// Native (pure-Rust, no external converter) Office content preview
//
// We extract the document's text/data from its OOXML XML and draw a simple
// page-like image with Cairo's text API. This is a CONTENT preview — readable
// text/data, not a faithful visual render (no themes, images, charts, exact
// layout). It only runs as a fallback when no embedded thumbnail exists.
// ──────────────────────────────────────────────────────────────────────

/// What kind of document we're previewing, with the extracted content.
struct OfficeContent {
    kind: OfficeKind,
    title: String,
    /// Lines of text (slide bullets / paragraphs) or, for sheets, formatted rows.
    lines: Vec<String>,
}

#[derive(PartialEq)]
enum OfficeKind {
    Presentation,
    Document,
    Spreadsheet,
}

/// Read an OOXML file and pull out a representative chunk of its content.
fn extract_legacy_binary(doc: &Path, ext: &str) -> Option<OfficeContent> {
    // Only attempt strings extraction for legacy binary formats
    if !matches!(ext, "ppt" | "doc" | "xls") {
        return None;
    }

    // First try a pure-Rust extraction so legacy previews work even when
    // external helpers are unavailable in the sandbox.
    let bytes = std::fs::read(doc).ok()?;
    let mut candidates = Vec::<String>::new();
    candidates.extend(extract_ascii_strings(&bytes, 4));
    candidates.extend(extract_utf16le_strings(&bytes, 4));

    // If extraction was sparse, try host/system `strings` as a secondary source.
    if candidates.len() < 8 {
        let status = if let Some(bin) = resolve_tool("strings") {
            std::process::Command::new(&bin)
                .args(["-n", "4", "-e", "l"])
                .arg(doc)
                .output()
                .ok()
        } else if std::env::var("FLATPAK_ID").is_ok() {
            std::process::Command::new("flatpak-spawn")
                .args(["--host", "strings", "-n", "4", "-e", "l"])
                .arg(doc)
                .output()
                .ok()
        } else {
            None
        };
        if let Some(status) = status {
            if status.status.success() {
                let text = String::from_utf8_lossy(&status.stdout);
                candidates.extend(text.lines().map(|s| s.trim().to_string()));
            }
        }
    }

    let mut lines = Vec::new();
    for line in candidates {
        let trimmed = line.trim();
        if trimmed.len() > 4 && trimmed.chars().any(|c| c.is_ascii_alphabetic()) {
            if !lines
                .iter()
                .any(|x: &String| x.eq_ignore_ascii_case(trimmed))
            {
                lines.push(trimmed.to_string());
            }
        }
        if lines.len() >= 30 {
            break;
        }
    }

    if lines.is_empty() {
        return None;
    }

    let title = lines.first().cloned().unwrap_or_else(|| "Document".into());
    let body = lines.into_iter().skip(1).take(24).collect();

    let kind = match ext {
        "ppt" => OfficeKind::Presentation,
        "xls" => OfficeKind::Spreadsheet,
        _ => OfficeKind::Document,
    };

    Some(OfficeContent {
        kind,
        title,
        lines: body,
    })
}

fn extract_ascii_strings(bytes: &[u8], min_len: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = Vec::<u8>::new();
    for &b in bytes {
        if b.is_ascii_graphic() || b == b' ' {
            cur.push(b);
        } else {
            if cur.len() >= min_len {
                out.push(String::from_utf8_lossy(&cur).to_string());
            }
            cur.clear();
        }
    }
    if cur.len() >= min_len {
        out.push(String::from_utf8_lossy(&cur).to_string());
    }
    out
}

fn extract_utf16le_strings(bytes: &[u8], min_len: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut i = 0usize;
    while i + 1 < bytes.len() {
        let u = u16::from_le_bytes([bytes[i], bytes[i + 1]]);
        if (0x20..=0x7e).contains(&u) {
            if let Some(ch) = char::from_u32(u as u32) {
                cur.push(ch);
            }
        } else {
            if cur.len() >= min_len {
                out.push(cur.clone());
            }
            cur.clear();
        }
        i += 2;
    }
    if cur.len() >= min_len {
        out.push(cur);
    }
    out
}

fn extract_office_content(doc: &Path) -> Option<OfficeContent> {
    let ext = doc.extension()?.to_str()?.to_lowercase();

    if matches!(ext.as_str(), "ppt" | "doc" | "xls") {
        return extract_legacy_binary(doc, &ext);
    }
    if ext == "csv" {
        return extract_csv(doc);
    }

    let file = std::fs::File::open(doc).ok()?;
    let mut zip = zip::ZipArchive::new(file).ok()?;

    match ext.as_str() {
        e if PPTX_EXTS.contains(&e) => extract_pptx(&mut zip),
        "docx" => extract_docx(&mut zip),
        "xlsx" => extract_xlsx(&mut zip),
        _ => None, // .odp/.odt/.ods and legacy binaries not handled here
    }
}

fn extract_csv(doc: &Path) -> Option<OfficeContent> {
    let text = std::fs::read_to_string(doc).ok()?;
    let delimiter = detect_csv_delimiter(&text);
    let mut rows = parse_delimited_rows(&text, delimiter, OFFICE_MAX_ROWS);
    rows.retain(|row| row.iter().any(|cell| !cell.trim().is_empty()));
    if rows.is_empty() {
        return None;
    }
    rows.truncate(18);
    for row in &mut rows {
        row.truncate(8);
        for cell in row.iter_mut() {
            *cell = cell.trim().replace('\r', "");
        }
    }
    Some(OfficeContent {
        kind: OfficeKind::Spreadsheet,
        title: gettext("Spreadsheet").into(),
        lines: rows.into_iter().map(|row| row.join("\t")).collect(),
    })
}

fn detect_csv_delimiter(text: &str) -> char {
    let sample: Vec<&str> = text.lines().take(5).collect();
    let mut best = (',', 0usize);
    for delim in [',', ';', '\t'] {
        let score = sample
            .iter()
            .map(|line| line.matches(delim).count())
            .sum::<usize>();
        if score > best.1 {
            best = (delim, score);
        }
    }
    best.0
}

fn parse_delimited_rows(text: &str, delimiter: char, max_rows: usize) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut cell = String::new();
    let mut chars = text.chars().peekable();
    let mut in_quotes = false;

    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                if in_quotes && chars.peek() == Some(&'"') {
                    cell.push('"');
                    let _ = chars.next();
                } else {
                    in_quotes = !in_quotes;
                }
            }
            c if c == delimiter && !in_quotes => {
                row.push(cell.clone());
                cell.clear();
            }
            '\n' if !in_quotes => {
                row.push(cell.clone());
                cell.clear();
                rows.push(row);
                row = Vec::new();
                if rows.len() >= max_rows {
                    break;
                }
            }
            '\r' if !in_quotes => {}
            _ => cell.push(ch),
        }
    }

    if !cell.is_empty() || !row.is_empty() {
        row.push(cell);
        rows.push(row);
    }

    rows
}

// ──────────────────────────────────────────────────────────────────────
// Native office pages: paginated prose, sheet grids, ODF text slides
//
// Every supported office document is prepared into a list of renderable
// pages ONCE (memoized by path+mtime+size), then each page renders to its
// own versioned cache file. The preview pane's nav bar, the wheel stepper,
// the staleness refresh and the result-row thumbnails all share this path,
// so freshly created or downloaded files work with no pre-existing cache.
// ──────────────────────────────────────────────────────────────────────

/// Extensions rendered as native pages/sheets/slides (nav bar + wheel).
/// Types outside this list (.doc, .wps, unreadable or empty files) keep the
/// single-thumbnail behavior with the nav hidden.
const OFFICE_PAGE_EXTS: &[&str] = &[
    "docx", "odt", "rtf", "ott", "fodt",           // prose pages
    "xls", "xlsx", "ods", "ots", "fods", "csv",    // sheet pages
    "odp", "otp", "fodp",                          // ODF slides
];

/// Hard caps so a pathological file can't blow up memory or the nav bar.
const OFFICE_MAX_PARAS: usize = 2000;
const OFFICE_MAX_SHEETS: usize = 64;
const OFFICE_MAX_ROWS: usize = 5000;
const OFFICE_MAX_COLS: usize = 12;
/// Sheet pages are 4:3 at 2× the preview box (the pane caps the picture at
/// 320×240): an A4 page scaled into that box landed at ~2 px text, while
/// this aspect fills the box exactly so type drawn at 17+ px survives the
/// 0.5 fit-down as readable ~9 px glyphs.
const SHEET_PAGE_W: i32 = 640;
const SHEET_PAGE_H: i32 = 480;
/// Sheet-grid row heights: comfortable while rows fit, shrinking toward the
/// legible floor (with the font following) for dense sheets — never a
/// second page, never unreadably small.
const SHEET_ROW_H_MAX: f64 = 40.0;
const SHEET_ROW_H_MIN: f64 = 26.0;
/// Column width bounds for the content-measured layout.
const SHEET_COL_W_MIN: f64 = 88.0;
const SHEET_COL_W_MAX: f64 = 480.0;
/// Data-cell font bounds in page px (17 ≈ 8.5 px after the pane's fit-down).
const SHEET_DATA_FS_MIN: f64 = 17.0;
const SHEET_DATA_FS_MAX: f64 = 24.0;

// Document page canvas (A4 @ 96 dpi) + vertical rhythm — shared by the
// paginator and the renderer so measured page breaks are exact.
const DOC_PAGE_W: i32 = 794;
const DOC_PAGE_H: i32 = 1123;
const DOC_MARGIN: f64 = 56.0;
const DOC_BODY_SIZE: f64 = 14.0;
const DOC_BODY_LINE: f64 = 20.0;
const DOC_PARA_GAP: f64 = 9.0;
const DOC_FOOTER: f64 = 56.0;

/// One laid-out line of prose. `para_end` marks the last wrapped line of its
/// paragraph so measure and render share the exact vertical rhythm.
#[derive(Debug)]
struct ProseLine {
    text: String,
    para_end: bool,
}

/// One renderable page of a prepared office document.
struct OfficePage {
    /// Sheet name (plus part) shown as the preview caption while paging.
    caption: Option<String>,
    body: OfficePageBody,
}

#[derive(Debug)]
enum OfficePageBody {
    /// Paginated prose; `first` carries the big title block.
    Prose { first: bool, lines: Vec<ProseLine> },
    /// One whole sheet rendered with `sheet` as the tab label. Rows start
    /// at absolute sheet position (`row_base`, `col_base` — leading blank
    /// rows/columns of the range aren't drawn but are numbered for);
    /// `extra_cols` counts cells beyond the hard column cap. Row/col
    /// overflow past what fits the page is reported inside the render.
    /// `styles`/`style_pool` carry the workbook's real cell styling (fills,
    /// text colors, weights) — empty for formats with no style data, where
    /// the renderer falls back to neutral banding instead of inventing
    /// colors.
    Grid {
        sheet: String,
        rows: Vec<Vec<String>>,
        /// Per-cell pool index into `style_pool`, `u16::MAX` for plain cells.
        styles: Vec<Vec<u16>>,
        style_pool: Vec<SheetCellStyle>,
        row_base: usize,
        col_base: usize,
        extra_cols: usize,
        /// Full row count of the used range (may exceed `rows.len()` when
        /// the hard row cap kicked in — keeps the overflow note truthful).
        total_rows: usize,
    },
    /// ODF presentation slide: title + bullet paragraphs.
    Slide { title: String, bullets: Vec<String> },
}

/// Everything needed to render every page of one document.
struct PreparedOffice {
    kind: OfficeKind,
    doc_title: String,
    pages: Vec<OfficePage>,
}

/// Prepared pages keyed by (path, mtime, size) — stepping through pages
/// never re-parses the file, and an edited file re-prepares itself.
fn office_prepared_memo(
) -> &'static Mutex<std::collections::HashMap<(std::path::PathBuf, u64, u64), std::sync::Arc<PreparedOffice>>>
{
    static M: OnceLock<
        Mutex<std::collections::HashMap<(std::path::PathBuf, u64, u64), std::sync::Arc<PreparedOffice>>>,
    > = OnceLock::new();
    M.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn prepare_office(doc: &Path) -> Option<std::sync::Arc<PreparedOffice>> {
    let meta = std::fs::metadata(doc).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let key = (doc.to_path_buf(), mtime, meta.len());
    if let Ok(map) = office_prepared_memo().lock() {
        if let Some(p) = map.get(&key) {
            return Some(p.clone());
        }
    }
    let prepared = build_prepared_office(doc)?;
    let arc = std::sync::Arc::new(prepared);
    if let Ok(mut map) = office_prepared_memo().lock() {
        if map.len() > 64 {
            map.clear();
        }
        map.insert(key, arc.clone());
    }
    Some(arc)
}

/// Dispatch by extension into the structured parsers. Returns None for
/// types without one (.doc/.wps) and for files with no extractable content
/// — callers then fall back to the generic card / embedded thumbnail.
fn build_prepared_office(doc: &Path) -> Option<PreparedOffice> {
    let ext = doc.extension()?.to_str()?.to_lowercase();
    match ext.as_str() {
        "docx" => prose_from_paragraphs(docx_paragraphs(&zip_entry_string(doc, "word/document.xml")?)),
        "odt" | "ott" => prose_from_paragraphs(odf_paragraphs(&zip_entry_string(doc, "content.xml")?)),
        "fodt" => prose_from_paragraphs(odf_paragraphs(&read_bounded_text(doc, 4 * 1024 * 1024)?)),
        "rtf" => prose_from_paragraphs(rtf_paragraphs(&read_bounded_text(doc, 4 * 1024 * 1024)?)),
        "xls" | "xlsx" | "ods" | "ots" => sheets_via_calamine(doc),
        "fods" => {
            let rows = fods_rows(&read_bounded_text(doc, 8 * 1024 * 1024)?);
            if rows.is_empty() {
                return None;
            }
            let stem = file_stem_title(doc);
            grid_pages(vec![SheetGrid::plain(stem, rows)], Vec::new())
        }
        "csv" => {
            let rows = csv_rows_full(doc)?;
            let stem = file_stem_title(doc);
            grid_pages(vec![SheetGrid::plain(stem, rows)], Vec::new())
        }
        "odp" | "otp" => slides_from_odf(&zip_entry_string(doc, "content.xml")?),
        "fodp" => slides_from_odf(&read_bounded_text(doc, 8 * 1024 * 1024)?),
        _ => None,
    }
}

fn file_stem_title(doc: &Path) -> String {
    doc.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Spreadsheet".into())
}

/// Read a text file capped at `max_bytes` (lossy UTF-8 — a preview must not
/// fail on one bad byte near the cap).
fn read_bounded_text(doc: &Path, max_bytes: u64) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(doc).ok()?;
    let mut buf = Vec::new();
    f.by_ref().take(max_bytes).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

fn zip_entry_string(doc: &Path, name: &str) -> Option<String> {
    let file = std::fs::File::open(doc).ok()?;
    let mut zip = zip::ZipArchive::new(file).ok()?;
    read_zip_text(&mut zip, name)
}

// ── Prose documents ──

/// Paragraph texts from `word/document.xml` — the full document (capped only
/// by OFFICE_MAX_PARAS; pagination needs every paragraph, not the first 30).
fn docx_paragraphs(xml: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for para in xml.split("</w:p>") {
        let runs = extract_text_runs(para, "w:t");
        if !runs.is_empty() {
            let text = runs.join("");
            if !text.trim().is_empty() {
                lines.push(text);
            }
        }
        if lines.len() >= OFFICE_MAX_PARAS {
            break;
        }
    }
    lines
}

/// Build paginated prose pages: first non-empty paragraph is the title, the
/// rest is the body. All-empty input returns None (generic card instead).
fn prose_from_paragraphs(mut paras: Vec<String>) -> Option<PreparedOffice> {
    paras.truncate(OFFICE_MAX_PARAS);
    paras.retain(|p| !p.trim().is_empty());
    if paras.is_empty() {
        return None;
    }
    let mut it = paras.into_iter();
    let doc_title = it.next()?;
    let body: Vec<String> = it.collect();
    let pages = paginate_prose(&doc_title, &body)?;
    if pages.is_empty() {
        return None;
    }
    Some(PreparedOffice {
        kind: OfficeKind::Document,
        doc_title,
        pages,
    })
}

/// Measure-time wrap: split paragraphs into display lines and break them
/// into pages using the same fonts/margins the renderer draws with.
fn paginate_prose(title: &str, paras: &[String]) -> Option<Vec<OfficePage>> {
    let (_s, cr) = new_surface(8, 8)?;
    let content_w = DOC_PAGE_W as f64 - DOC_MARGIN * 2.0;
    let mut pages: Vec<Vec<ProseLine>> = Vec::new();
    let mut cur: Vec<ProseLine> = Vec::new();
    let mut y = prose_body_top(&cr, true, title);
    for para in paras {
        prose_set_body_font(&cr);
        let wrapped = wrap_lines_full(&cr, para, content_w);
        for (i, line) in wrapped.iter().enumerate() {
            if y + DOC_BODY_LINE > prose_body_bottom() && !cur.is_empty() {
                pages.push(std::mem::take(&mut cur));
                y = prose_body_top(&cr, false, title);
            }
            cur.push(ProseLine {
                text: line.clone(),
                para_end: i + 1 == wrapped.len(),
            });
            y += DOC_BODY_LINE;
        }
        y += DOC_PARA_GAP;
    }
    if !cur.is_empty() || pages.is_empty() {
        pages.push(cur);
    }
    Some(
        pages
            .into_iter()
            .enumerate()
            .map(|(i, lines)| OfficePage {
                caption: None,
                body: OfficePageBody::Prose {
                    first: i == 0,
                    lines,
                },
            })
            .collect(),
    )
}

// ── Prose layout metrics (shared by paginator and renderer) ──

fn prose_set_body_font(cr: &gtk::cairo::Context) {
    use gtk::cairo;
    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
    cr.set_font_size(DOC_BODY_SIZE);
}

/// Big title block height on the first page (≤3 wrapped lines + rule).
fn prose_title_block_h(cr: &gtk::cairo::Context, title: &str) -> f64 {
    use gtk::cairo;
    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
    cr.set_font_size(30.0);
    let n = wrap_lines_full(cr, title, DOC_PAGE_W as f64 - DOC_MARGIN * 2.0)
        .len()
        .min(3)
        .max(1);
    (n as f64) * 38.0 + 14.0
}

/// Small header height on continuation pages.
fn prose_header_h() -> f64 {
    40.0
}

/// Where the body starts vertically on a page.
fn prose_body_top(cr: &gtk::cairo::Context, first: bool, title: &str) -> f64 {
    if first {
        DOC_MARGIN + prose_title_block_h(cr, title)
    } else {
        DOC_MARGIN + prose_header_h()
    }
}

fn prose_body_bottom() -> f64 {
    DOC_PAGE_H as f64 - DOC_FOOTER
}

// ── ODF text extraction (ODT/FODT/ODP/OTP/FODP share this) ──

/// Find the next `<text:p` / `<text:h` opening tag at or after `from`,
/// returning its position and which element it is. The boundary check after
/// the letter rejects look-alikes such as `<text:paragraph>` or
/// `<text:header>`.
fn find_odf_para_tag(xml: &str, from: usize) -> Option<(usize, char)> {
    let mut search = from;
    while search < xml.len() {
        let rel = xml[search..].find("<text:")?;
        let at = search + rel;
        let after = at + "<text:".len();
        let Some(ch0) = xml[after..].chars().next() else {
            return None;
        };
        if matches!(ch0, 'p' | 'h') {
            let next = xml[after + 1..].chars().next();
            if matches!(
                next,
                Some('>') | Some(' ') | Some('\t') | Some('\r') | Some('\n') | Some('/')
            ) {
                return Some((at, ch0));
            }
        }
        search = at + "<text:".len() + 1;
    }
    None
}

/// Paragraph texts from ODF XML (`content.xml`): `<text:p>` and `<text:h>`
/// elements in document order, markup stripped, entities decoded.
fn odf_paragraphs(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some((start, which)) = find_odf_para_tag(xml, pos) {
        let Some(gt_rel) = xml[start..].find('>') else {
            break;
        };
        let open_end = start + gt_rel + 1;
        // self-closing <text:p/> carries no text
        if open_end >= 2 && xml.as_bytes()[open_end - 2] == b'/' {
            pos = open_end;
            continue;
        }
        let close = if which == 'p' { "</text:p>" } else { "</text:h>" };
        let Some(end_rel) = xml[open_end..].find(close) else {
            pos = open_end;
            continue;
        };
        let inner = &xml[open_end..open_end + end_rel];
        let text = strip_odf_markup(inner);
        if !text.trim().is_empty() {
            out.push(text);
            if out.len() >= OFFICE_MAX_PARAS {
                break;
            }
        }
        pos = open_end + end_rel + close.len();
    }
    out
}

/// Is `<tag…` an element with exactly this qualified name (no prefix match)?
fn tag_name_is(tag: &str, name: &str) -> bool {
    tag.strip_prefix('<')
        .and_then(|r| r.strip_prefix(name))
        .map_or(false, |r| {
            r.starts_with('>')
                || r.starts_with(' ')
                || r.starts_with('/')
                || r.starts_with('\t')
                || r.starts_with('\n')
        })
}

/// Strip ODF inline markup from one paragraph/cell: `<text:s>` runs become
/// spaces (honouring text:c), tabs and line breaks keep their whitespace,
/// all other tags are dropped, then XML entities are decoded.
fn strip_odf_markup(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        let Some(g) = rest[i..].find('>') else {
            out.push_str(&rest[i..]);
            return decode_xml_entities(&out);
        };
        let tag = &rest[i..i + g + 1];
        if tag_name_is(tag, "text:s") {
            let n = attr_value(tag, "text:c")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(1);
            for _ in 0..n.min(40) {
                out.push(' ');
            }
        } else if tag_name_is(tag, "text:tab") {
            out.push('\t');
        } else if tag_name_is(tag, "text:line-break") {
            out.push('\n');
        }
        rest = &rest[i + g + 1..];
    }
    out.push_str(rest);
    decode_xml_entities(&out)
}

// ── RTF ──

/// Paragraphs from RTF source: a control-word stripper that understands
/// `\par`/`\line` breaks, `\tab`, `\uN` unicode, `\'hh` hex, `\bin`
/// payloads, and skips skippable groups (font/color/style tables, pictures,
/// metadata, and anything marked with the `\*` destination marker).
fn rtf_paragraphs(src: &str) -> Vec<String> {
    let ch: Vec<char> = src.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut skip: Vec<bool> = vec![false];
    let mut i = 0usize;

    macro_rules! pushc {
        ($c:expr) => {
            if !*skip.last().unwrap_or(&false) {
                cur.push($c);
            }
        };
    }
    macro_rules! end_para {
        () => {
            if !*skip.last().unwrap_or(&false) {
                let t = cur.trim().to_string();
                if !t.is_empty() {
                    out.push(t);
                    if out.len() >= OFFICE_MAX_PARAS {
                        return out;
                    }
                }
                cur.clear();
            }
        };
    }

    while i < ch.len() {
        match ch[i] {
            '\\' => {
                i += 1;
                if i >= ch.len() {
                    break;
                }
                match ch[i] {
                    '\\' | '{' | '}' => {
                        pushc!(ch[i]);
                        i += 1;
                    }
                    '~' => {
                        pushc!(' ');
                        i += 1;
                    }
                    '\'' => {
                        if i + 2 < ch.len() {
                            let hex: String = ch[i + 1..i + 3].iter().collect();
                            if let Ok(b) = u8::from_str_radix(&hex, 16) {
                                pushc!(b as char); // latin-1 for the common case
                            }
                            i += 3;
                        } else {
                            i += 1;
                        }
                    }
                    '*' => {
                        // Destination marker: skip this group unless understood.
                        if let Some(last) = skip.last_mut() {
                            *last = true;
                        }
                        i += 1;
                    }
                    '\n' | '\r' => {
                        i += 1; // line continuation
                    }
                    c if c.is_ascii_alphabetic() => {
                        let start = i;
                        while i < ch.len() && ch[i].is_ascii_alphabetic() {
                            i += 1;
                        }
                        let word: String = ch[start..i].iter().collect();
                        // Optional numeric parameter (may be negative).
                        let mut param: i32 = 0;
                        let mut has_param = false;
                        if i < ch.len() && (ch[i] == '-' || ch[i].is_ascii_digit()) {
                            let pstart = i;
                            if ch[i] == '-' {
                                i += 1;
                            }
                            while i < ch.len() && ch[i].is_ascii_digit() {
                                i += 1;
                            }
                            if let Ok(v) = ch[pstart..i].iter().collect::<String>().parse::<i32>() {
                                param = v;
                                has_param = true;
                            }
                        }
                        // Optional single-space delimiter.
                        let mut consumed_delim = false;
                        if i < ch.len() && ch[i] == ' ' {
                            i += 1;
                            consumed_delim = true;
                        }

                        let skipped = *skip.last().unwrap_or(&false);
                        match word.as_str() {
                            "par" | "line" => end_para!(),
                            "tab" => {
                                if !skipped {
                                    cur.push('\t');
                                }
                            }
                            "u" if has_param => {
                                if !skipped {
                                    let code = if param < 0 { param + 65536 } else { param };
                                    if let Some(c) =
                                        u32::try_from(code).ok().and_then(char::from_u32)
                                    {
                                        cur.push(c);
                                    }
                                }
                                // The mandatory fallback character follows.
                                if !consumed_delim && i < ch.len() && ch[i] != '\\' {
                                    i += 1;
                                }
                            }
                            "bin" if has_param && param > 0 => {
                                i = (i + param as usize).min(ch.len());
                            }
                            "emdash" => pushc!('\u{2014}'),
                            "endash" => pushc!('\u{2013}'),
                            "bullet" => pushc!('\u{2022}'),
                            "lquote" => pushc!('\u{2018}'),
                            "rquote" => pushc!('\u{2019}'),
                            "ldblquote" => pushc!('\u{201c}'),
                            "rdblquote" => pushc!('\u{201d}'),
                            // Known skippable destinations open a group we ignore.
                            "fonttbl" | "colortbl" | "stylesheet" | "info" | "pict"
                            | "themedata" | "colorscheme" | "datastore" | "listtable"
                            | "listoverridetable" | "header" | "footer" | "headerl"
                            | "headerr" | "footerl" | "footerr" | "headerf" | "footerf"
                            | "generator" | "xmlnstbl" => {
                                if let Some(last) = skip.last_mut() {
                                    *last = true;
                                }
                            }
                            _ => {}
                        }
                    }
                    c if c.is_ascii_digit() => {
                        while i < ch.len() && ch[i].is_ascii_digit() {
                            i += 1;
                        }
                    }
                    c => {
                        pushc!(c);
                        i += 1;
                    }
                }
            }
            '{' => {
                let cur_skip = *skip.last().unwrap_or(&false);
                skip.push(cur_skip);
                i += 1;
            }
            '}' => {
                if skip.len() > 1 {
                    skip.pop();
                }
                i += 1;
            }
            '\n' | '\r' => {
                i += 1; // RTF source newlines are not content
            }
            c => {
                pushc!(c);
                i += 1;
            }
        }
    }
    end_para!();
    out
}

// ── Spreadsheets ──

/// One prepared sheet: name + rows at their ABSOLUTE sheet position — the
/// used range may start past blank rows/columns, and numbering/column
/// letters must follow the real sheet, not the range.
struct SheetGrid {
    name: String,
    rows: Vec<Vec<String>>,
    /// Per-cell style pool indices parallel to `rows` (`u16::MAX` = plain).
    /// Empty for formats with no style data at all.
    styles: Vec<Vec<u16>>,
    row_base: usize,
    col_base: usize,
    /// Rows in the used range — may exceed `rows.len()` when the hard row
    /// cap kicked in, so the "+N more rows" note stays truthful.
    total_rows: usize,
}

impl SheetGrid {
    /// A flat single-table source (CSV, FODS): starts at A1.
    fn plain(name: String, rows: Vec<Vec<String>>) -> Self {
        let total_rows = rows.len();
        SheetGrid {
            name,
            rows,
            styles: Vec::new(),
            row_base: 0,
            col_base: 0,
            total_rows,
        }
    }
}

/// All sheets of a workbook via calamine (xlsx/xls/ods/ots), prepared as
/// one page per sheet — the page IS the tab. Cells are stringified at
/// their absolute sheet coordinates, and for xlsx each cell's display
/// format from styles.xml is applied first, so what the grid shows is
/// what Excel shows (percent scales, grouped/fixed decimals, symbols) —
/// falling back to the exact value whenever a format isn't recognized.
fn sheets_via_calamine(doc: &Path) -> Option<PreparedOffice> {
    use calamine::Reader;
    let mut wb = calamine::open_workbook_auto(doc).ok()?;
    let names = wb.sheet_names();
    // An xlsx-only pass (None for xls/ods — those read exact values with no
    // workbook styling to honor).
    let info = xlsx_cell_info(doc, &names);
    let fmts = info.as_ref().and_then(|i| i.formats.as_ref());
    let stys = info.as_ref().and_then(|i| i.styles.as_ref());
    let style_pool = stys.map(|s| s.pool.clone()).unwrap_or_default();
    let mut sheets: Vec<SheetGrid> = Vec::new();
    for (si, name) in names.into_iter().enumerate().take(OFFICE_MAX_SHEETS) {
        let Ok(range) = wb.worksheet_range(&name) else {
            continue;
        };
        let (start_row, start_col) = range.start().unwrap_or((0, 0));
        let mut rows: Vec<Vec<String>> = Vec::new();
        let mut styles: Vec<Vec<u16>> = Vec::new();
        for (ri, row) in range.rows().enumerate().take(OFFICE_MAX_ROWS) {
            let abs_row = start_row as usize + ri;
            let mut cells: Vec<String> = Vec::new();
            let mut srow: Vec<u16> = Vec::new();
            for (ci, c) in row.iter().enumerate() {
                let abs_col = start_col as usize + ci;
                if let Some(s) = fmts.and_then(|f| f.apply(si, abs_col, abs_row, c)) {
                    cells.push(s);
                } else {
                    cells.push(crate::thumbnails::cell_value_str(c));
                }
                srow.push(
                    stys
                        .and_then(|s| s.pool_index(si, abs_col, abs_row))
                        .unwrap_or(u16::MAX),
                );
            }
            while cells.last().map(|c| c.trim().is_empty()).unwrap_or(false) {
                cells.pop();
            }
            // `srow` keeps one entry per cell the sheet actually holds —
            // a styled-but-empty cell still shows its fill in the render.
            rows.push(cells);
            styles.push(srow);
        }
        sheets.push(SheetGrid {
            name,
            rows,
            styles,
            row_base: start_row as usize,
            col_base: start_col as usize,
            total_rows: range.height(),
        });
    }
    if sheets.is_empty() {
        return None;
    }
    grid_pages(sheets, style_pool)
}

/// Exactly one page per sheet, caption = the sheet name (the preview's
/// nav bar then flips TABS — visible only for multi-sheet workbooks).
/// A sheet is never split into row-chunk pages: what doesn't fit the page
/// is counted inside it ("+N more rows/columns") instead of paginated
/// away, and cells beyond the hard column cap are counted too.
fn grid_pages(mut sheets: Vec<SheetGrid>, style_pool: Vec<SheetCellStyle>) -> Option<PreparedOffice> {
    let mut pages = Vec::new();
    for s in &mut sheets {
        let max_len = s.rows.iter().map(|r| r.len()).max().unwrap_or(0);
        let extra_cols = max_len.saturating_sub(OFFICE_MAX_COLS);
        if max_len > OFFICE_MAX_COLS {
            for row in &mut s.rows {
                row.truncate(OFFICE_MAX_COLS);
            }
        }
        for srow in &mut s.styles {
            srow.truncate(OFFICE_MAX_COLS);
        }
        pages.push(OfficePage {
            caption: Some(s.name.clone()),
            body: OfficePageBody::Grid {
                sheet: s.name.clone(),
                rows: std::mem::take(&mut s.rows),
                styles: std::mem::take(&mut s.styles),
                style_pool: style_pool.clone(),
                row_base: s.row_base,
                col_base: s.col_base,
                extra_cols,
                total_rows: s.total_rows,
            },
        });
    }
    if pages.is_empty() {
        return None;
    }
    let doc_title = sheets
        .first()
        .map(|s| s.name.clone())
        .unwrap_or_else(|| "Spreadsheet".into());
    Some(PreparedOffice {
        kind: OfficeKind::Spreadsheet,
        doc_title,
        pages,
    })
}

// ── xlsx display formats + visual styles ────────────────────────────

/// Per-cell display formats for one xlsx (built once per prepare, dropped
/// right after the rows are stringified): a pool of unique format codes
/// plus a map from (sheet, row, col) to its index. The sheet dimension
/// matters — sheets share coordinates, so A1 of two sheets must not
/// resolve to each other's format.
struct CellFormats {
    pool: Vec<String>,
    map: std::collections::HashMap<(usize, usize, usize), u16>,
}

impl CellFormats {
    /// Formatted display value for a numeric cell, or None to fall back to
    /// the exact value (unstyled/General cell, unsupported format shape,
    /// or a non-numeric cell the style doesn't apply to).
    fn apply(
        &self,
        sheet: usize,
        col: usize,
        row: usize,
        cell: &calamine::Data,
    ) -> Option<String> {
        let idx = *self.map.get(&(sheet, row, col))?;
        let v = match cell {
            calamine::Data::Int(i) => *i as f64,
            calamine::Data::Float(f) => *f,
            _ => return None,
        };
        apply_num_format(&self.pool[idx as usize], v)
    }
}

/// Display formats + resolved visual styles for an xlsx, both read from
/// `xl/styles.xml` in ONE scan over the workbook's cells (dropped right
/// after the rows are stringified). `s` style indices resolve to numFmt
/// codes (built-in ids mapped by hand) and to the cell's real paint —
/// fill, text color, weights — with workbook rels pairing sheet names
/// with their worksheet part so both land on the right (row, col). A file
/// without `xl/styles.xml`, a cell with a General format, and every other
/// container format (xls/ods/csv) fall through to the plain exact value.
struct XlsxCellInfo {
    /// None = every cell is General.
    formats: Option<CellFormats>,
    /// None = no cell deviates from a plain white cell.
    styles: Option<CellStyles>,
}

fn xlsx_cell_info(doc: &Path, sheet_names: &[String]) -> Option<XlsxCellInfo> {
    let styles_xml = zip_entry_string(doc, "xl/styles.xml")?;
    let styles = styles_xml.as_str();

    // Custom numFmt codes (ids ≥ 164).
    let mut custom: std::collections::HashMap<u16, String> = std::collections::HashMap::new();
    for frag in styles.split("<numFmt ").skip(1) {
        let head = &frag[..frag.find("/>").unwrap_or(frag.len())];
        let (Some(id), Some(code)) = (attr_u16(head, "numFmtId"), attr_value(head, "formatCode"))
        else {
            continue;
        };
        custom.insert(id, decode_xml_entities(&code));
    }

    // cellXfs order: style index → numFmtId. Positional — an <xf> without
    // a numFmtId must still occupy its slot (defaults to General = 0).
    let xfs: Vec<u16> = styles
        .split_once("<cellXfs")
        .and_then(|(_, rest)| rest.split("</cellXfs>").next())
        .map(|body| {
            body.split("<xf ")
                .skip(1)
                .map(|xf| attr_u16(&xf[..xf.find('>').unwrap_or(xf.len())], "numFmtId").unwrap_or(0))
                .collect()
        })?;
    if xfs.is_empty() {
        return None;
    }

    // Visual side of the same cellXfs table: theme slots, the file's
    // custom indexed palette, then fills/fonts → one resolved deviation
    // per style index (None = paints like a plain cell).
    let theme = xlsx_theme_colors(doc);
    let indexed = xlsx_indexed_palette(styles);
    let fills = xlsx_fills(styles, &theme, &indexed);
    let fonts = xlsx_fonts(styles, &theme, &indexed);
    let xf_styles = xlsx_xf_styles(styles, &fills, &fonts);

    // Sheet name → worksheet part (workbook order + rels).
    let wb_xml = zip_entry_string(doc, "xl/workbook.xml")?;
    let rels = zip_entry_string(doc, "xl/_rels/workbook.xml.rels")?;
    let mut part_of: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for frag in wb_xml.split("<sheet ").skip(1) {
        let end = frag.find("/>").unwrap_or_else(|| frag.find('>').unwrap_or(frag.len()));
        let head = &frag[..end];
        let (Some(name), Some(rid)) = (attr_value(head, "name"), attr_value(head, "r:id")) else {
            continue;
        };
        let target = rels.split("<Relationship").skip(1).find_map(|r| {
            (attr_value(r, "Id").as_deref() == Some(rid.as_str()))
                .then(|| attr_value(r, "Target"))
                .flatten()
        });
        if let Some(t) = target {
            let part = t.strip_prefix('/').map(str::to_string).unwrap_or(t);
            part_of.insert(name, if part.starts_with("xl/") { part } else { format!("xl/{}", part) });
        }
    }

    let mut pool: Vec<String> = Vec::new();
    let mut pool_idx: std::collections::HashMap<String, u16> = std::collections::HashMap::new();
    let mut map: std::collections::HashMap<(usize, usize, usize), u16> =
        std::collections::HashMap::new();
    let mut style_pool: Vec<SheetCellStyle> = Vec::new();
    let mut style_idx: std::collections::HashMap<SheetCellStyle, u16> =
        std::collections::HashMap::new();
    let mut style_map: std::collections::HashMap<(usize, usize, usize), u16> =
        std::collections::HashMap::new();
    for (si, name) in sheet_names.iter().enumerate().take(OFFICE_MAX_SHEETS) {
        let Some(part) = part_of.get(name) else {
            continue;
        };
        let Some(xml) = zip_entry_string(doc, part) else {
            continue;
        };
        let mut rest = xml.as_str();
        while let Some(i) = rest.find("<c ") {
            rest = &rest[i + 3..];
            let end = rest.find('>').unwrap_or(rest.len());
            let head = &rest[..end];
            rest = &rest[end.saturating_add(1)..];
            let (Some(s), Some(r)) = (attr_u16(head, "s"), attr_value(head, "r")) else {
                continue;
            };
            let Some((r0, c0)) = cell_ref_rc(&r) else {
                continue;
            };
            // Number format: only cells carrying a non-General code.
            if let Some(xf) = xfs.get(s as usize) {
                let code = match *xf {
                    id if id == 0 || id == 49 => None, // General / Text
                    id if id < 164 => builtin_numfmt(id).map(str::to_string),
                    id => custom.get(&id).cloned(),
                };
                if let Some(code) = code.filter(|c| !c.is_empty() && !c.contains("General")) {
                    let idx = *pool_idx.entry(code.clone()).or_insert_with(|| {
                        let i = pool.len() as u16;
                        pool.push(code);
                        i
                    });
                    map.insert((si, r0, c0), idx);
                }
            }
            // Visual style: only cells that deviate from a plain white cell.
            if let Some(st) = xf_styles.get(s as usize).copied().flatten() {
                let idx = *style_idx.entry(st).or_insert_with(|| {
                    let i = style_pool.len() as u16;
                    style_pool.push(st);
                    i
                });
                style_map.insert((si, r0, c0), idx);
            }
        }
    }
    Some(XlsxCellInfo {
        formats: (!map.is_empty()).then_some(CellFormats { pool, map }),
        styles: (!style_map.is_empty()).then_some(CellStyles {
            pool: style_pool,
            map: style_map,
        }),
    })
}

/// One cell's resolved look: only deviations from a plain cell (white
/// fill, default dark ink, no styling) are kept, so a pool entry always
/// means "the render must change something here". Eq + pooled so thousands
/// of cells share a handful of entries.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct SheetCellStyle {
    fill: Option<[u8; 3]>,
    text: Option<[u8; 3]>,
    bold: bool,
    italic: bool,
    underline: bool,
}

impl SheetCellStyle {
    /// A cell that paints like the renderer's default.
    const DEFAULT: SheetCellStyle = SheetCellStyle {
        fill: None,
        text: None,
        bold: false,
        italic: false,
        underline: false,
    };
}

/// Resolved visual styles: a pool of unique cell looks plus a map from
/// (sheet, row, col) to its index (the sheet keeps A1s of different
/// sheets from resolving to each other's paint). Cells at their default
/// are absent.
struct CellStyles {
    pool: Vec<SheetCellStyle>,
    map: std::collections::HashMap<(usize, usize, usize), u16>,
}

impl CellStyles {
    fn pool_index(&self, sheet: usize, col: usize, row: usize) -> Option<u16> {
        self.map.get(&(sheet, row, col)).copied()
    }
}

/// The workbook's theme slots as bytes — `xl/theme/theme1.xml`, with the
/// Office defaults filling any gap — which `<color theme="N"/>` refers to.
fn xlsx_theme_colors(doc: &Path) -> std::collections::HashMap<String, [u8; 3]> {
    let mut colors = default_theme_colors();
    if let Some(xml) = zip_entry_string(doc, "xl/theme/theme1.xml") {
        parse_clr_scheme(&xml, &mut colors);
    }
    colors
        .into_iter()
        .map(|(k, (r, g, b))| {
            (
                k,
                [
                    (r * 255.0 + 0.5) as u8,
                    (g * 255.0 + 0.5) as u8,
                    (b * 255.0 + 0.5) as u8,
                ],
            )
        })
        .collect()
}

/// `<indexedColors>`: an old file's custom palette — `<rgb>` values in
/// index order. Indices outside it fall back to the built-in auto slots.
fn xlsx_indexed_palette(styles: &str) -> std::collections::HashMap<usize, [u8; 3]> {
    let mut out = std::collections::HashMap::new();
    let Some(body) = styles
        .split_once("<indexedColors>")
        .and_then(|(_, rest)| rest.split("</indexedColors>").next())
    else {
        return out;
    };
    for (i, seg) in body.split("<rgb>").skip(1).enumerate() {
        let v = seg.split("</rgb>").next().unwrap_or("");
        if let Some(c) = hex_rgb8(v) {
            out.insert(i, c);
        }
    }
    out
}

/// `<fills>` → per fillId the solid color it paints (None for the `none`
/// and `gray125` placeholders and for pattern fills that show the plain
/// background).
fn xlsx_fills(
    styles: &str,
    theme: &std::collections::HashMap<String, [u8; 3]>,
    indexed: &std::collections::HashMap<usize, [u8; 3]>,
) -> Vec<Option<[u8; 3]>> {
    let mut out = Vec::new();
    let Some(body) = styles
        .split_once("<fills")
        .and_then(|(_, rest)| rest.split("</fills>").next())
    else {
        return out;
    };
    for seg in body.split("</fill>").filter(|s| s.contains("<fill")) {
        out.push(solid_fill_color(seg, theme, indexed));
    }
    out
}

/// The visible color of one `<fill>` fragment: a solid pattern paints its
/// `fgColor` (`bgColor` when that's all there is); every other pattern
/// (`none`, `gray125`, stripes…) shows the cell background.
fn solid_fill_color(
    seg: &str,
    theme: &std::collections::HashMap<String, [u8; 3]>,
    indexed: &std::collections::HashMap<usize, [u8; 3]>,
) -> Option<[u8; 3]> {
    let pi = find_open_tag(seg, 0, "patternFill")?;
    let pt_end = tag_end(seg, pi)?;
    if attr_value(&seg[pi..pt_end], "patternType")? != "solid" {
        return None;
    }
    let ci =
        find_open_tag(seg, 0, "fgColor").or_else(|| find_open_tag(seg, 0, "bgColor"))?;
    let ct = tag_end(seg, ci)?;
    xlsx_color8(&seg[ci..ct], theme, indexed)
}

/// One `<font>` in the font table: the color and weights that change how
/// a cell's text is drawn.
#[derive(Default, Clone, Copy)]
struct XlsxFont {
    color: Option<[u8; 3]>,
    bold: bool,
    italic: bool,
    underline: bool,
}

/// `<fonts>` → per fontId the styling its cells inherit.
fn xlsx_fonts(
    styles: &str,
    theme: &std::collections::HashMap<String, [u8; 3]>,
    indexed: &std::collections::HashMap<usize, [u8; 3]>,
) -> Vec<XlsxFont> {
    let mut out = Vec::new();
    let Some(body) = styles
        .split_once("<fonts")
        .and_then(|(_, rest)| rest.split("</fonts>").next())
    else {
        return out;
    };
    for seg in body.split("</font>").filter(|s| s.contains("<font")) {
        let mut f = XlsxFont::default();
        if let Some(ci) = find_open_tag(seg, 0, "color") {
            if let Some(ct) = tag_end(seg, ci) {
                f.color = xlsx_color8(&seg[ci..ct], theme, indexed);
            }
        }
        f.bold = find_open_tag(seg, 0, "b").is_some();
        f.italic = find_open_tag(seg, 0, "i").is_some();
        f.underline = find_open_tag(seg, 0, "u").is_some();
        out.push(f);
    }
    out
}

/// `<cellXfs>` → per style index the deviations from a plain cell it
/// paints, joining each `<xf>`'s fillId to its fontId. Entries resolving
/// to the default are `None` and never recorded per cell. Positional — an
/// `<xf>` without fontId/fillId occupies its slot (both default to 0).
fn xlsx_xf_styles(
    styles: &str,
    fills: &[Option<[u8; 3]>],
    fonts: &[XlsxFont],
) -> Vec<Option<SheetCellStyle>> {
    let Some(body) = styles
        .split_once("<cellXfs")
        .and_then(|(_, rest)| rest.split("</cellXfs>").next())
    else {
        return Vec::new();
    };
    body.split("<xf ")
        .skip(1)
        .map(|xf| {
            let head = &xf[..xf.find('>').unwrap_or(xf.len())];
            let fill = attr_u16(head, "fillId").unwrap_or(0) as usize;
            let font = attr_u16(head, "fontId").unwrap_or(0) as usize;
            let ft = fonts.get(font).copied().unwrap_or_default();
            let st = SheetCellStyle {
                fill: fills.get(fill).copied().flatten(),
                // Near-black reads as the default ink: without this every
                // plain cell would carry a color entry for no visible gain.
                text: ft
                    .color
                    .filter(|c| c[0] >= 48 || c[1] >= 48 || c[2] >= 48),
                bold: ft.bold,
                italic: ft.italic,
                underline: ft.underline,
            };
            (st != SheetCellStyle::DEFAULT).then_some(st)
        })
        .collect()
}

/// One xlsx `<color …/>` open tag → its bytes: `rgb`, `theme` (+tint), or
/// `indexed` (+ the file's custom palette). Excel's theme indices map
/// 0=background1, 1=text1, 2=background2, 3=text2, 4–9=accents, 10/11=
/// hyperlink / followed hyperlink.
fn xlsx_color8(
    tag: &str,
    theme: &std::collections::HashMap<String, [u8; 3]>,
    indexed: &std::collections::HashMap<usize, [u8; 3]>,
) -> Option<[u8; 3]> {
    let base = if let Some(rgb) = attr_value(tag, "rgb") {
        hex_rgb8(&rgb)
    } else if let Some(t) = attr_value(tag, "theme").and_then(|v| v.parse::<usize>().ok()) {
        match t {
            0 => theme.get("lt1").copied(),
            1 => theme.get("dk1").copied(),
            2 => theme.get("lt2").copied(),
            3 => theme.get("dk2").copied(),
            4..=9 => theme.get(&format!("accent{}", t - 3)).copied(),
            10 => theme.get("hlink").copied(),
            11 => theme.get("folHlink").copied(),
            _ => None,
        }
    } else if let Some(i) = attr_value(tag, "indexed").and_then(|v| v.parse::<usize>().ok()) {
        match i {
            64 => Some([0, 0, 0]),       // auto (foreground)
            65 => Some([255, 255, 255]), // system background
            _ => indexed.get(&i).copied(),
        }
    } else {
        None
    }?;
    let tint = attr_value(tag, "tint")
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(0.0);
    Some(if tint != 0.0 { apply_tint(base, tint) } else { base })
}

/// `RRGGBB`/`AARRGGBB` (alpha dropped) → bytes. Also tolerates a leading
/// `#` so hand-written fixtures behave like Excel's own output.
fn hex_rgb8(s: &str) -> Option<[u8; 3]> {
    let h = s.trim().trim_start_matches('#');
    if (h.len() != 6 && h.len() != 8) || !h.is_ascii() {
        return None;
    }
    let off = if h.len() == 8 { 2 } else { 0 }; // 8-digit is AARRGGBB
    let at = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some([at(off)?, at(off + 2)?, at(off + 4)?])
}

/// Excel's tint on a solid color: positive lightens toward white,
/// negative darkens toward black.
fn apply_tint(c: [u8; 3], tint: f64) -> [u8; 3] {
    let t = tint.clamp(-1.0, 1.0);
    let f = |v: u8| {
        let x = v as f64 / 255.0;
        let y = if t >= 0.0 {
            t * x + (1.0 - t)
        } else {
            (1.0 + t) * x
        };
        (y.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
    };
    [f(c[0]), f(c[1]), f(c[2])]
}

/// Numeric built-in numFmt ids worth applying (dates included: if such a
/// cell ever reaches us as a number, a serial would be worse than a date).
/// Accounting/scientific/fraction builtins fall back to the exact value.
fn builtin_numfmt(id: u16) -> Option<&'static str> {
    Some(match id {
        1 => "0",
        2 => "0.00",
        3 => "#,##0",
        4 => "#,##0.00",
        9 => "0%",
        10 => "0.00%",
        14..=17 | 22 => "yyyy-mm-dd",
        18..=21 | 45 => "hh:mm",
        46 => "[h]:mm",
        47 => "hh:mm:ss",
        _ => return None,
    })
}

/// "B12" → (row 11, col 1); ignores anything malformed.
fn cell_ref_rc(r: &str) -> Option<(usize, usize)> {
    let split = r.find(|c: char| c.is_ascii_digit()).unwrap_or(0);
    if split == 0 || split == r.len() {
        return None;
    }
    let mut col = 0usize;
    for c in r[..split].chars() {
        if !c.is_ascii_alphabetic() {
            return None;
        }
        col = col * 26 + (c.to_ascii_uppercase() as u8 - b'A' + 1) as usize;
    }
    Some((r[split..].parse::<usize>().ok()?.saturating_sub(1), col - 1))
}

/// attr value as u16, for numFmtId/s style indices.
fn attr_u16(tag: &str, name: &str) -> Option<u16> {
    attr_value(tag, name)?.parse().ok()
}

/// What a format section actually formats: dates/times render from the
/// serial (same output as a native DateTime cell), everything else is a
/// number format. Quoted and bracketed literals are ignored for detection,
/// so `"kg"` or `[$USD-407]` can't masquerade as a date pattern.
fn format_kind(sec: &str) -> u8 {
    let mut in_q = false;
    let mut in_b = false;
    let mut has_date = false;
    let mut has_time = false;
    let mut chars = sec.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => in_q = !in_q,
            '[' => in_b = true,
            ']' => in_b = false,
            _ if in_q || in_b => {}
            'y' | 'Y' | 'd' | 'D' => has_date = true,
            ':' | 'h' | 'H' => has_time = true,
            'A' | 'P' => {
                if chars.peek() == Some(&'M') {
                    has_time = true;
                }
            }
            _ => {}
        }
    }
    if has_date {
        1
    } else if has_time {
        2
    } else {
        0
    }
}

/// Split a format code on ';' into sections, ignoring quoted literals.
fn split_format_sections(code: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut in_q = false;
    for (i, c) in code.char_indices() {
        match c {
            '"' => in_q = !in_q,
            ';' if !in_q => {
                out.push(&code[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&code[start..]);
    out
}

/// Apply an xlsx number-format code to a value the way Excel displays it:
/// percent ×100 with the sign kept, decimal digits and thousands grouping
/// from the code (rounded half-up like Excel), quoted literals and
/// currency symbols in place, negative sections honored. Date/time codes
/// render from the serial. Anything unrecognized returns None so the
/// exact value shows instead of a wrong guess.
fn apply_num_format(code: &str, v: f64) -> Option<String> {
    if !v.is_finite() || code.is_empty() || code.contains('@') {
        return None;
    }
    let secs = split_format_sections(code);
    let neg = v < 0.0;
    let (sec, sign) = match (neg, secs.len()) {
        (true, n) if n >= 2 => (secs[1], ""),
        (true, _) => (secs[0], "-"),
        _ => (secs[0], ""),
    };

    // Dates/times: never a raw serial.
    match format_kind(sec) {
        1 => {
            return Some(crate::thumbnails::excel_datetime_str(v));
        }
        2 => {
            let t = if code.contains('[') {
                crate::thumbnails::elapsed_time_str(v) // [h]:mm-style duration
            } else {
                crate::thumbnails::elapsed_time_str(v.fract())
            };
            return Some(t);
        }
        _ => {}
    }

    // Tokenize: literals before the digit pattern become a prefix, after
    // it a suffix; unsupported shapes (scientific, fractions, conditions)
    // bail out to the exact value.
    let mut prefix = String::new();
    let mut suffix = String::new();
    let mut pat = String::new();
    let mut in_pattern = false;
    let mut percent = false;
    let push_lit = |lit: &str, in_pattern: &mut bool, prefix: &mut String, suffix: &mut String| {
        if *in_pattern {
            suffix.push_str(lit);
        } else {
            prefix.push_str(lit);
        }
    };
    let mut it = sec.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '"' => {
                let mut lit = String::new();
                for c in it.by_ref() {
                    if c == '"' {
                        break;
                    }
                    lit.push(c);
                }
                push_lit(&lit, &mut in_pattern, &mut prefix, &mut suffix);
            }
            '\\' => {
                if let Some(c) = it.next() {
                    push_lit(&c.to_string(), &mut in_pattern, &mut prefix, &mut suffix);
                }
            }
            '_' | '*' => {
                it.next(); // width fill / repeat fill: skip next char
            }
            '[' => {
                let mut tag = String::new();
                for c in it.by_ref() {
                    if c == ']' {
                        break;
                    }
                    tag.push(c);
                }
                if let Some(sym) = tag.strip_prefix('$') {
                    let sym = sym.split('-').next().unwrap_or(sym);
                    push_lit(sym, &mut in_pattern, &mut prefix, &mut suffix);
                } else if tag.starts_with('>') || tag.starts_with('<') || tag.starts_with('=') {
                    return None; // conditional formats: no safe display rule
                }
                // color tags: ignored
            }
            '0' | '#' | ',' | '.' => {
                in_pattern = true;
                pat.push(c);
            }
            '%' => {
                in_pattern = true;
                percent = true;
                suffix.push('%');
            }
            'E' | 'e' if matches!(it.peek(), Some(&'+') | Some(&'-')) => {
                return None; // scientific
            }
            'y' | 'Y' | 'd' | 'D' | '/' | ':' | 'h' | 'H' => return None, // date/fraction debris
            c => push_lit(&c.to_string(), &mut in_pattern, &mut prefix, &mut suffix),
        }
    }
    if pat.is_empty() || !pat.chars().any(|c| c == '0' || c == '#') {
        return None;
    }

    // Decimal digits: '#' slots are optional (trimmed), '0' slots forced.
    let (int_pat, frac_pat) = match pat.find('.') {
        Some(d) => (&pat[..d], &pat[d + 1..]),
        None => (pat.as_str(), ""),
    };
    let grouping = int_pat.contains(',');
    let forced = frac_pat.matches('0').count();
    let optional = frac_pat.matches('#').count();
    let total = forced + optional;

    let mut num = v.abs() * if percent { 100.0 } else { 1.0 };
    if num.abs() >= 1e15 {
        return None;
    }
    // Excel rounds half away from zero.
    let factor = 10f64.powi(total as i32);
    num = (num * factor + 0.5).floor() / factor;
    let mut s = format!("{:.*}", total, num);
    if total > forced {
        if let Some(d) = s.find('.') {
            s.truncate(d + 1 + forced);
            if forced == 0 {
                s.truncate(s.len().saturating_sub(1)); // drop the lone dot
            }
        }
    }
    // Thousands grouping in the integer part. (`num` is already |v|, so
    // the integer part is always plain digits.)
    if grouping {
        let (int_len, frac) = match s.find('.') {
            Some(d) => (d, s[d..].to_string()),
            None => (s.len(), String::new()),
        };
        let body: Vec<char> = s[..int_len].chars().collect();
        let mut grouped = String::new();
        for (i, c) in body.iter().enumerate() {
            if i > 0 && (body.len() - i) % 3 == 0 {
                grouped.push(',');
            }
            grouped.push(*c);
        }
        s = format!("{}{}", grouped, frac);
    }
    Some(format!("{}{}{}{}", sign, prefix, s, suffix))
}

/// Full CSV rows for pagination (the single-card extractor stays capped).
fn csv_rows_full(doc: &Path) -> Option<Vec<Vec<String>>> {
    let text = read_bounded_text(doc, 4 * 1024 * 1024)?;
    let delimiter = detect_csv_delimiter(&text);
    let mut rows = parse_delimited_rows(&text, delimiter, OFFICE_MAX_ROWS);
    rows.retain(|row| row.iter().any(|cell| !cell.trim().is_empty()));
    if rows.is_empty() {
        return None;
    }
    for row in &mut rows {
        for cell in row.iter_mut() {
            *cell = cell.trim().replace('\r', "");
        }
        while row.last().map(|c| c.trim().is_empty()).unwrap_or(false) {
            row.pop();
        }
        // Column cap + overflow count happen centrally in grid_pages, so a
        // wide CSV reports "+N more columns" instead of silently losing them.
    }
    Some(rows)
}

/// Rows of a flat ODS (fods): table rows/cells split from raw XML, markup
/// stripped, numeric `office:value` used when a cell has no text body.
fn fods_rows(xml: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    for row_chunk in xml.split("</table:table-row>") {
        let Some(i) = row_chunk.rfind("<table:table-row") else {
            continue; // prolog before the first row
        };
        let row_body = &row_chunk[i..];
        if !row_body.contains('>') {
            continue;
        }
        let mut cells = Vec::new();
        for cell_chunk in row_body.split("</table:table-cell>") {
            let Some(ci) = cell_chunk.rfind("<table:table-cell") else {
                continue;
            };
            let cell_tag_end = cell_chunk[ci..].find('>').map(|g| ci + g);
            let Some(cell_tag_end) = cell_tag_end else {
                continue;
            };
            let open_tag = &cell_chunk[ci..(cell_tag_end + 1).min(cell_chunk.len())];
            let inner = &cell_chunk[(cell_tag_end + 1).min(cell_chunk.len())..];
            let mut text = strip_odf_markup(inner);
            if text.trim().is_empty() {
                if let Some(v) = attr_value(open_tag, "office:value") {
                    text = v;
                }
            }
            cells.push(text);
        }
        if cells.iter().any(|c| !c.trim().is_empty()) {
            rows.push(cells);
        }
        if rows.len() >= OFFICE_MAX_ROWS {
            break;
        }
    }
    rows
}

// ── ODF presentations ──

/// Slide list from ODF content.xml (ODP/OTP zipped, FODP flat): one page per
/// `<draw:page>`; first paragraph is the title, the rest are bullets.
fn slides_from_odf(content_xml: &str) -> Option<PreparedOffice> {
    let mut starts: Vec<usize> = Vec::new();
    let mut scan = 0usize;
    while let Some(rel) = content_xml[scan..].find("<draw:page") {
        let at = scan + rel;
        starts.push(at);
        scan = at + "<draw:page".len();
    }

    let mut pages = Vec::new();
    for (i, &s) in starts.iter().enumerate() {
        let Some(gt_rel) = content_xml[s..].find('>') else {
            break;
        };
        let body_start = s + gt_rel + 1;
        let bound = starts.get(i + 1).copied().unwrap_or(content_xml.len());
        if body_start >= bound {
            continue; // self-closed page, no content
        }
        let end = content_xml[body_start..bound]
            .find("</draw:page>")
            .map(|r| body_start + r)
            .unwrap_or(bound);
        let chunk = &content_xml[body_start..end.min(bound)];
        let paras = odf_paragraphs(chunk);
        let mut it = paras.iter().filter(|p| !p.trim().is_empty());
        let Some(first) = it.next() else {
            continue; // picture-only or empty slide
        };
        let title = first.clone();
        let bullets: Vec<String> = it.cloned().collect();
        pages.push(OfficePage {
            caption: None,
            body: OfficePageBody::Slide { title, bullets },
        });
    }
    if pages.is_empty() {
        return None;
    }
    Some(PreparedOffice {
        kind: OfficeKind::Presentation,
        doc_title: "Presentation".into(),
        pages,
    })
}

/// Word-wrap `text` at `max_w` with no line cap — unlike the capped
/// `wrap_text` for small on-screen boxes; pagination needs every line.
/// Explicit newlines inside a paragraph become line breaks.
fn wrap_lines_full(cr: &gtk::cairo::Context, text: &str, max_w: f64) -> Vec<String> {
    let mut lines = Vec::new();
    for raw in text.split('\n') {
        let mut cur = String::new();
        for word in raw.split_whitespace() {
            let trial = if cur.is_empty() {
                word.to_string()
            } else {
                format!("{} {}", cur, word)
            };
            let w = cr
                .text_extents(&trial)
                .map(|e| e.width())
                .unwrap_or(0.0);
            if w > max_w && !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
                cur = word.to_string();
            } else {
                cur = trial;
            }
        }
        lines.push(cur);
    }
    lines
}

/// Read a named entry from the zip into a String (UTF-8, lossy).
fn read_zip_text<R: std::io::Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    name: &str,
) -> Option<String> {
    use std::io::Read;
    let mut entry = zip.by_name(name).ok()?;
    let mut buf = String::new();
    entry.read_to_string(&mut buf).ok()?;
    Some(buf)
}

/// Extract all text inside <a:t>...</a:t> runs (used by PPTX/DOCX share the w:t
/// / a:t convention). `tag` is the local element name ("a:t" or "w:t").
fn extract_text_runs(xml: &str, tag: &str) -> Vec<String> {
    let open = format!("<{}", tag); // tolerate attributes: <a:t> or <a:t xml:space=…>
    let close = format!("</{}>", tag);
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(start) = xml[pos..].find(&open) {
        let abs = pos + start;
        // The char right after the tag name must be '>' or whitespace, otherwise
        // "<t" would wrongly match "<title>", "<tableStyleId>", etc.
        let after = xml[abs + open.len()..].chars().next();
        let is_tag = matches!(
            after,
            Some('>') | Some(' ') | Some('\t') | Some('\r') | Some('\n') | Some('/')
        );
        if !is_tag {
            pos = abs + open.len();
            continue;
        }
        // Find the '>' that ends the opening tag.
        let Some(gt) = xml[abs..].find('>') else {
            break;
        };
        // Self-closing tag like <a:t/> has no text.
        if xml[abs..abs + gt].ends_with('/') {
            pos = abs + gt + 1;
            continue;
        }
        let text_start = abs + gt + 1;
        let Some(end_rel) = xml[text_start..].find(&close) else {
            break;
        };
        let text = &xml[text_start..text_start + end_rel];
        let decoded = decode_xml_entities(text);
        if !decoded.trim().is_empty() {
            out.push(decoded);
        }
        pos = text_start + end_rel + close.len();
    }
    out
}

/// Minimal XML entity decoding for the common five.
/// Decode XML entities: the five named ones plus numeric character
/// references (`&#10;`, `&#x2022;`) — decks routinely encode bullet chars
/// numerically. Single pass, so `&amp;lt;` decodes to the literal `&lt;`.
fn decode_xml_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let Some(semi) = after.find(';') else {
            out.push('&');
            return out + after;
        };
        let body = &after[..semi];
        let decoded: Option<char> = match body {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => body.strip_prefix('#').and_then(|num| {
                let code = match num.strip_prefix('x').or_else(|| num.strip_prefix('X')) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => num.parse::<u32>().ok(),
                };
                code.and_then(char::from_u32)
            }),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &after[semi + 1..];
            }
            None => {
                // not an entity after all — keep the '&' literally
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn extract_pptx<R: std::io::Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
) -> Option<OfficeContent> {
    // Slides live at ppt/slides/slide1.xml, slide2.xml, ... We preview slide 1.
    let xml = read_zip_text(zip, "ppt/slides/slide1.xml")?;
    let runs = extract_text_runs(&xml, "a:t");
    if runs.is_empty() {
        return None;
    }
    let title = runs
        .first()
        .cloned()
        .unwrap_or_else(|| "Presentation".into());
    let lines = runs.into_iter().skip(1).take(12).collect();
    Some(OfficeContent {
        kind: OfficeKind::Presentation,
        title,
        lines,
    })
}

fn extract_docx<R: std::io::Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
) -> Option<OfficeContent> {
    let xml = read_zip_text(zip, "word/document.xml")?;
    // Full paragraph list (not the first 30): pagination needs every line,
    // and the legacy single-card renderer clips at the page edge anyway.
    let lines = docx_paragraphs(&xml);
    if lines.is_empty() {
        return None;
    }
    let title = lines[0].clone();
    let body = lines[1..].to_vec();
    Some(OfficeContent {
        kind: OfficeKind::Document,
        title,
        lines: body,
    })
}

fn extract_xlsx<R: std::io::Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
) -> Option<OfficeContent> {
    // Shared strings: each <si> is ONE logical string, but may contain multiple
    // <t> runs (e.g. rich text). We must join the runs within each <si> so the
    // string-index lookup from cells stays aligned. extract_text_runs would
    // wrongly split those into separate entries, so we group by <si> here.
    let shared: Vec<String> = read_zip_text(zip, "xl/sharedStrings.xml")
        .map(|x| {
            x.split("</si>")
                .filter_map(|si| {
                    if !si.contains("<si") && !si.contains("<t") {
                        return None;
                    }
                    let runs = extract_text_runs(si, "t");
                    if runs.is_empty() {
                        Some(String::new())
                    } else {
                        Some(runs.join(""))
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    let sheet = read_zip_text(zip, "xl/worksheets/sheet1.xml")?;
    let mut rows: Vec<Vec<String>> = Vec::new();

    for row in sheet.split("</row>").take(24) {
        if !row.contains("<c") {
            continue;
        }
        let mut cells: Vec<String> = Vec::new();
        let mut pos = 0;
        // Each cell: <c r="A1" t="s"><v>idx</v></c> or <c r="A1"><v>3.14</v></c>
        // or inline string <c t="inlineStr"><is><t>text</t></is></c>.
        while let Some(rel) = row[pos..].find("<c") {
            let abs = pos + rel;
            let Some(tag) = tag_substr(&row[abs..], "<c") else {
                break;
            };
            let cell_type = attr_value(&tag, "t");
            // Advance past this cell's opening tag.
            let after_tag = abs + tag.len();
            // Determine the slice for this cell (up to the next <c or end).
            let cell_end = row[after_tag..]
                .find("<c")
                .map(|x| after_tag + x)
                .unwrap_or(row.len());
            let cell_body = &row[after_tag..cell_end];

            let text = match cell_type.as_deref() {
                Some("s") => {
                    // Shared-string index inside <v>.
                    inner_text(cell_body, "<v>", "</v>")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .and_then(|i| shared.get(i).cloned())
                        .unwrap_or_default()
                }
                Some("inlineStr") => extract_text_runs(cell_body, "t").join(""),
                _ => {
                    // Number / date / bool: the literal value.
                    inner_text(cell_body, "<v>", "</v>").unwrap_or_default()
                }
            };
            cells.push(decode_xml_entities(text.trim()));
            pos = cell_end;
            if cells.len() >= 8 {
                break;
            }
        }
        // Trim trailing empty cells.
        while cells.last().map(|s| s.is_empty()).unwrap_or(false) {
            cells.pop();
        }
        if cells.iter().any(|s| !s.is_empty()) {
            rows.push(cells);
        }
        if rows.len() >= 18 {
            break;
        }
    }

    if rows.is_empty() {
        return None;
    }
    // Store rows as tab-joined lines; the renderer splits them back into a grid.
    let lines = rows.into_iter().map(|r| r.join("\t")).collect();
    Some(OfficeContent {
        kind: OfficeKind::Spreadsheet,
        title: gettext("Spreadsheet").into(),
        lines,
    })
}

/// Extract text between the first `open` and the following `close` marker.
fn inner_text(s: &str, open: &str, close: &str) -> Option<String> {
    let start = s.find(open)? + open.len();
    let end = s[start..].find(close)? + start;
    Some(s[start..end].to_string())
}

/// Draw an OfficeContent to a PNG using Cairo's text API. Returns the PNG bytes.
fn render_content_preview(content: &OfficeContent) -> Option<Vec<u8>> {
    match content.kind {
        OfficeKind::Spreadsheet => render_spreadsheet(content),
        OfficeKind::Document => render_document(content),
        OfficeKind::Presentation => render_document(content), // PPTX prefers layout render
    }
}

/// Common helpers for the template renderers.
pub(crate) fn new_surface(w: i32, h: i32) -> Option<(gtk::cairo::ImageSurface, gtk::cairo::Context)> {
    use gtk::cairo;
    let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).ok()?;
    let cr = cairo::Context::new(&surface).ok()?;
    Some((surface, cr))
}

pub(crate) fn surface_to_png(surface: gtk::cairo::ImageSurface, cr: gtk::cairo::Context) -> Option<Vec<u8>> {
    drop(cr); // release the surface borrow before encoding
    let mut buf: Vec<u8> = Vec::new();
    surface.write_to_png(&mut buf).ok()?;
    Some(buf)
}

/// A clean "document page" template: white page, colored title, body paragraphs
/// with word-wrap. Used for Word docs (and as the PPTX fallback).
fn render_document(content: &OfficeContent) -> Option<Vec<u8>> {
    use gtk::cairo;
    const W: i32 = 320;
    const H: i32 = 414; // ~A4 portrait feel
    let (surface, cr) = new_surface(W, H)?;
    let wf = W as f64;
    let hf = H as f64;

    // Paper + subtle drop shadow edge.
    cr.set_source_rgb(1.0, 1.0, 1.0);
    cr.paint().ok()?;
    cr.set_source_rgb(0.88, 0.88, 0.90);
    cr.set_line_width(1.0);
    cr.rectangle(0.5, 0.5, wf - 1.0, hf - 1.0);
    cr.stroke().ok()?;

    let (ar, ag, ab) = accent_for(&content.kind);
    let margin = 22.0;

    // Title.
    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
    cr.set_font_size(17.0);
    cr.set_source_rgb(ar, ag, ab);
    let mut y = 44.0;
    for line in wrap_text(&cr, &content.title, wf - margin * 2.0)
        .into_iter()
        .take(2)
    {
        cr.move_to(margin, y);
        let _ = cr.show_text(&line);
        y += 22.0;
    }

    // Accent rule under the title.
    cr.set_source_rgba(ar, ag, ab, 0.5);
    cr.set_line_width(2.0);
    cr.move_to(margin, y - 6.0);
    cr.line_to(wf - margin, y - 6.0);
    cr.stroke().ok()?;
    y += 8.0;

    // Body paragraphs.
    cr.set_source_rgb(0.18, 0.18, 0.20);
    for para in &content.lines {
        cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
        cr.set_font_size(11.5);
        for line in wrap_text(&cr, para, wf - margin * 2.0) {
            if y > hf - 18.0 {
                return surface_to_png(surface, cr);
            }
            cr.move_to(margin, y);
            let _ = cr.show_text(&line);
            y += 16.0;
        }
        y += 6.0; // paragraph spacing
    }

    surface_to_png(surface, cr)
}

/// A compact spreadsheet preview: row/column headers, table borders, header
/// styling, and alternating row banding.
fn render_spreadsheet(content: &OfficeContent) -> Option<Vec<u8>> {
    use gtk::cairo;
    const W: i32 = 520;
    const H: i32 = 360;
    let (surface, cr) = new_surface(W, H)?;
    let wf = W as f64;
    let hf = H as f64;

    cr.set_source_rgb(1.0, 1.0, 1.0);
    cr.paint().ok()?;

    // Parse rows back into cells (renderer received them tab-joined).
    let rows: Vec<Vec<String>> = content
        .lines
        .iter()
        .map(|l| l.split('\t').map(|s| s.to_string()).collect())
        .collect();
    if rows.is_empty() {
        return None;
    }

    // Determine column count (cap to what fits).
    let ncols = rows.iter().map(|r| r.len()).max().unwrap_or(1).clamp(1, 6);
    let row_header_w = 34.0;
    let col_header_h = 22.0;
    let row_h = 22.0;
    let table_w = wf - row_header_w - 1.0;
    let col_w = table_w / ncols as f64;

    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);

    // Sheet headers.
    cr.set_source_rgb(0.91, 0.92, 0.93);
    cr.rectangle(0.0, 0.0, wf, col_header_h);
    cr.fill().ok()?;
    cr.rectangle(0.0, col_header_h, row_header_w, hf - col_header_h);
    cr.fill().ok()?;

    cr.set_source_rgb(0.74, 0.75, 0.77);
    cr.set_line_width(1.0);
    cr.move_to(0.0, col_header_h + 0.5);
    cr.line_to(wf, col_header_h + 0.5);
    cr.stroke().ok()?;
    cr.move_to(row_header_w + 0.5, 0.0);
    cr.line_to(row_header_w + 0.5, hf);
    cr.stroke().ok()?;

    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
    cr.set_font_size(10.0);
    cr.set_source_rgb(0.28, 0.29, 0.31);
    for ci in 0..ncols {
        let cx = row_header_w + ci as f64 * col_w;
        let label = ((b'A' + ci as u8) as char).to_string();
        let ext = cr.text_extents(&label).ok();
        let tw = ext.as_ref().map(|e| e.width()).unwrap_or(0.0);
        cr.move_to(cx + (col_w - tw) / 2.0, 15.0);
        let _ = cr.show_text(&label);
    }

    let mut y = col_header_h;
    for (ri, row) in rows.iter().enumerate() {
        if y > hf {
            break;
        }

        cr.set_source_rgb(0.91, 0.92, 0.93);
        cr.rectangle(0.0, y, row_header_w, row_h);
        cr.fill().ok()?;

        cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
        cr.set_font_size(9.5);
        cr.set_source_rgb(0.38, 0.39, 0.41);
        let row_num = (ri + 1).to_string();
        let ext = cr.text_extents(&row_num).ok();
        let tw = ext.as_ref().map(|e| e.width()).unwrap_or(0.0);
        cr.move_to(row_header_w - tw - 6.0, y + 15.5);
        let _ = cr.show_text(&row_num);

        // Row background: no workbook styling is reachable here (this is
        // the legacy text fallback), so a plain white header and quiet
        // zebra — never a hue the file didn't declare.
        if ri % 2 == 0 {
            cr.set_source_rgb(1.0, 1.0, 1.0);
        } else {
            cr.set_source_rgb(0.957, 0.965, 0.973);
        }
        cr.rectangle(row_header_w, y, table_w, row_h);
        cr.fill().ok()?;

        // Cell text.
        for ci in 0..ncols {
            let cx = row_header_w + ci as f64 * col_w;
            let cell = row.get(ci).cloned().unwrap_or_default();
            if ri == 0 {
                cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
                cr.set_font_size(10.8);
                cr.set_source_rgb(0.0, 0.0, 0.0);
            } else {
                cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
                cr.set_font_size(10.5);
                cr.set_source_rgb(0.15, 0.15, 0.17);
            }
            let shown = truncate_to_width(&cr, &cell, col_w - 10.0);
            cr.move_to(cx + 5.0, y + 15.5);
            let _ = cr.show_text(&shown);
        }
        y += row_h;
    }

    // Grid lines.
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.18);
    cr.set_line_width(1.0);
    for ci in 0..=ncols {
        let cx = row_header_w + ci as f64 * col_w;
        cr.move_to(cx + 0.5, 0.0);
        cr.line_to(cx + 0.5, y.min(hf));
        cr.stroke().ok()?;
    }
    let mut gy = col_header_h;
    while gy < y.min(hf) {
        cr.move_to(0.0, gy + 0.5);
        cr.line_to(wf, gy + 0.5);
        cr.stroke().ok()?;
        gy += row_h;
    }

    // Strong border around the styled table header.
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.70);
    cr.set_line_width(1.2);
    cr.rectangle(
        row_header_w + 0.5,
        col_header_h + 0.5,
        table_w - 1.0,
        row_h - 1.0,
    );
    cr.stroke().ok()?;

    // Subtle bottom/right sheet edge.
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.12);
    cr.set_line_width(1.0);
    cr.rectangle(0.5, 0.5, wf - 1.0, hf - 1.0);
    cr.stroke().ok()?;

    // A tiny sheet tab hint makes the image read as a spreadsheet even when
    // the selected area is mostly blank.
    let tab_y = hf - 22.0;
    cr.set_source_rgb(0.95, 0.96, 0.97);
    cr.rectangle(8.0, tab_y, 58.0, 18.0);
    cr.fill().ok()?;
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.16);
    cr.rectangle(8.5, tab_y + 0.5, 57.0, 17.0);
    cr.stroke().ok()?;
    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
    cr.set_font_size(9.0);
    cr.set_source_rgb(0.20, 0.21, 0.23);
    cr.move_to(20.0, tab_y + 12.5);
    let _ = cr.show_text("Sheet1");

    // Hide grid under the tab with a white strip if data reaches the bottom.
    if y > tab_y {
        cr.set_source_rgb(1.0, 1.0, 1.0);
        cr.rectangle(
            row_header_w,
            tab_y - 1.0,
            wf - row_header_w,
            hf - tab_y + 1.0,
        );
        cr.fill().ok()?;
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.12);
        cr.move_to(0.0, tab_y - 0.5);
        cr.line_to(wf, tab_y - 0.5);
        cr.stroke().ok()?;
    }

    surface_to_png(surface, cr)
}

// ── Native page renderers (prose / sheet grid / ODF slide) ──

/// Render prepared page `n` (1-based; clamped) to PNG bytes.
fn render_office_page(prep: &PreparedOffice, n: usize) -> Option<Vec<u8>> {
    let n = clamp_page(n, prep.pages.len());
    let page = prep.pages.get(n - 1)?;
    match &page.body {
        OfficePageBody::Prose { first, lines } => render_prose_page(
            &prep.doc_title,
            *first,
            lines,
            n,
            prep.pages.len(),
            &prep.kind,
        ),
        OfficePageBody::Grid {
            sheet,
            rows,
            styles,
            style_pool,
            row_base,
            col_base,
            extra_cols,
            total_rows,
        } => render_sheet_grid(
            rows,
            styles,
            style_pool,
            sheet,
            *row_base,
            *col_base,
            *extra_cols,
            *total_rows,
            n,
            prep.pages.len(),
        ),
        OfficePageBody::Slide { title, bullets } => {
            render_odf_slide(title, bullets, n, prep.pages.len())
        }
    }
}

/// One A4-ish page of prose: title block on page 1, small header after,
/// wrapped lines, folio bottom-right. Vertical positions must stay in sync
/// with `paginate_prose` (shared prose_body_top/metrics functions).
fn render_prose_page(
    title: &str,
    first: bool,
    lines: &[ProseLine],
    page_no: usize,
    total: usize,
    kind: &OfficeKind,
) -> Option<Vec<u8>> {
    use gtk::cairo;
    let (surface, cr) = new_surface(DOC_PAGE_W, DOC_PAGE_H)?;
    let wf = DOC_PAGE_W as f64;
    let hf = DOC_PAGE_H as f64;
    let content_w = wf - DOC_MARGIN * 2.0;

    cr.set_source_rgb(1.0, 1.0, 1.0);
    cr.paint().ok()?;
    let (ar, ag, ab) = accent_for(kind);

    if first {
        cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
        cr.set_font_size(30.0);
        cr.set_source_rgb(ar, ag, ab);
        let block_h = prose_title_block_h(&cr, title);
        let mut ty = DOC_MARGIN + 30.0;
        for line in wrap_lines_full(&cr, title, content_w).into_iter().take(3) {
            cr.move_to(DOC_MARGIN, ty);
            let _ = cr.show_text(&line);
            ty += 38.0;
        }
        cr.set_source_rgba(ar, ag, ab, 0.55);
        cr.set_line_width(2.0);
        cr.move_to(DOC_MARGIN, DOC_MARGIN + block_h - 7.0);
        cr.line_to(wf - DOC_MARGIN, DOC_MARGIN + block_h - 7.0);
        cr.stroke().ok()?;
    } else {
        cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
        cr.set_font_size(13.0);
        cr.set_source_rgb(0.45, 0.45, 0.48);
        let shown = truncate_to_width(&cr, title, content_w);
        cr.move_to(DOC_MARGIN, DOC_MARGIN + 16.0);
        let _ = cr.show_text(&shown);
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.18);
        cr.set_line_width(1.0);
        cr.move_to(DOC_MARGIN, DOC_MARGIN + 30.0);
        cr.line_to(wf - DOC_MARGIN, DOC_MARGIN + 30.0);
        cr.stroke().ok()?;
    }

    let mut y = prose_body_top(&cr, first, title);
    prose_set_body_font(&cr);
    cr.set_source_rgb(0.18, 0.18, 0.20);
    for line in lines {
        if y > prose_body_bottom() {
            break;
        }
        cr.move_to(DOC_MARGIN, y + 15.0);
        let _ = cr.show_text(&line.text);
        y += DOC_BODY_LINE;
        if line.para_end {
            y += DOC_PARA_GAP;
        }
    }

    draw_folio(&cr, page_no, total, wf - DOC_MARGIN, hf - 24.0, 11.0);

    cr.set_source_rgb(0.88, 0.88, 0.90);
    cr.set_line_width(1.0);
    cr.rectangle(0.5, 0.5, wf - 1.0, hf - 1.0);
    cr.stroke().ok()?;

    surface_to_png(surface, cr)
}

/// One page of spreadsheet rows at SHEET_PAGE_W×SHEET_PAGE_H (2× the
/// preview box — an A4 page in that box shrank text to ~2 px) — the WHOLE
/// sheet: content-measured column widths (squeezed proportionally to the
/// page, ellipsis-truncated at the cell edge), adaptive row height down to
/// a legible floor, absolute row numbers/column letters, numbers
/// right-aligned like a spreadsheet, and honest "+N more rows/columns"
/// notes for anything that doesn't fit. The workbook's own cell styling —
/// fills, text colors, bold/italic/underline — is painted as the file
/// declares it; only files with NO style data get quiet zebra banding, and
/// no header is ever colored in a hue the file didn't ask for. Sheet tab
/// (real name) + folio at the bottom.
fn render_sheet_grid(
    rows: &[Vec<String>],
    styles: &[Vec<u16>],
    style_pool: &[SheetCellStyle],
    sheet: &str,
    row_base: usize,
    col_base: usize,
    extra_cols: usize,
    total_rows: usize,
    page_no: usize,
    total: usize,
) -> Option<Vec<u8>> {
    use gtk::cairo;
    let (surface, cr) = new_surface(SHEET_PAGE_W, SHEET_PAGE_H)?;
    let wf = SHEET_PAGE_W as f64;
    let hf = SHEET_PAGE_H as f64;

    cr.set_source_rgb(1.0, 1.0, 1.0);
    cr.paint().ok()?;

    // From here on the file's own colors rule: when it styles at least one
    // cell of this page, every unstyled cell is plain white — no zebra, no
    // invented header band.
    let has_styles = styles.iter().any(|r| r.iter().any(|i| *i != u16::MAX));

    let row_header_w = 84.0;
    let col_header_h = 48.0;
    let table_w = wf - row_header_w - 1.0;
    let table_bottom = hf - 48.0; // tab/folio strip below the grid
    let avail_h = table_bottom - col_header_h;

    // Rows: comfortable while the sheet is small, then shrink toward the
    // legible floor — the font follows the row height, so dense sheets
    // stay readable instead of turning into page-fragments.
    let row_h = if rows.is_empty() {
        SHEET_ROW_H_MAX
    } else {
        (avail_h / rows.len() as f64).clamp(SHEET_ROW_H_MIN, SHEET_ROW_H_MAX)
    };
    let shown = if rows.is_empty() {
        0
    } else {
        (((avail_h / row_h).floor()) as usize)
            .min(rows.len())
            .max(1)
    };
    let hidden_rows = total_rows.max(rows.len()).saturating_sub(shown);
    let mut data_fs = (row_h - 8.0).clamp(SHEET_DATA_FS_MIN, SHEET_DATA_FS_MAX);

    // Columns: keep as many as fit at a sane minimum width; the rest is
    // reported under the grid (never silently dropped on the floor).
    let max_cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let ncols = ((table_w / SHEET_COL_W_MIN).floor() as usize)
        .min(max_cols)
        .max(1);
    let hidden_cols = max_cols.saturating_sub(ncols) + extra_cols;

    // Column widths from the shown content (min..max clamped), then
    // squeezed proportionally to the page — floors redrawn from the
    // widest columns so nothing drops below legibility.
    // Measure at the comfortable size first, then shrink the font toward
    // the legible floor while the content still doesn't fit — on a wide
    // sheet, full values beat big type. Only once the floor is reached do
    // columns get squeezed (and cells ellipsized).
    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
    let mut widths: Vec<f64> = loop {
        cr.set_font_size(data_fs);
        let w: Vec<f64> = (0..ncols)
            .map(|ci| {
                let mut w = SHEET_COL_W_MIN;
                for row in rows.iter().take(shown) {
                    if let Some(cell) = row.get(ci) {
                        if !cell.is_empty() {
                            let tw = cr.text_extents(cell).map(|e| e.width()).unwrap_or(0.0);
                            w = w.max(tw + 24.0);
                        }
                    }
                }
                w.min(SHEET_COL_W_MAX)
            })
            .collect();
        let s: f64 = w.iter().sum();
        if s <= table_w || data_fs <= SHEET_DATA_FS_MIN {
            break w;
        }
        data_fs = (data_fs - 0.5).max(SHEET_DATA_FS_MIN);
    };
    let sum: f64 = widths.iter().sum();
    if sum > table_w {
        let scale = table_w / sum;
        for w in &mut widths {
            *w *= scale;
        }
        for _ in 0..4 {
            let mut deficit = 0.0;
            for w in &mut widths {
                if *w < SHEET_COL_W_MIN {
                    deficit += SHEET_COL_W_MIN - *w;
                    *w = SHEET_COL_W_MIN;
                }
            }
            if deficit < 0.05 {
                break;
            }
            let over: f64 = widths.iter().map(|w| (w - SHEET_COL_W_MIN).max(0.0)).sum();
            if over < 0.05 {
                break;
            }
            for w in &mut widths {
                if *w > SHEET_COL_W_MIN {
                    let give = (*w - SHEET_COL_W_MIN) * deficit / over;
                    *w -= give;
                }
            }
        }
    } else {
        // Fill the page width so the grid reaches the right edge.
        let extra = (table_w - sum) / ncols as f64;
        for w in &mut widths {
            *w += extra;
        }
    }
    let mut xs: Vec<f64> = Vec::with_capacity(ncols);
    let mut left = row_header_w;
    for w in &widths {
        xs.push(left);
        left += w;
    }

    // Header bands + separator lines (neutral chrome — spreadsheet colors
    // live in the cells, not in our furniture).
    cr.set_source_rgb(0.91, 0.92, 0.93);
    cr.rectangle(0.0, 0.0, wf, col_header_h);
    cr.fill().ok()?;
    cr.rectangle(0.0, col_header_h, row_header_w, hf - col_header_h);
    cr.fill().ok()?;
    cr.set_source_rgb(0.74, 0.75, 0.77);
    cr.set_line_width(1.0);
    cr.move_to(0.0, col_header_h + 0.5);
    cr.line_to(wf, col_header_h + 0.5);
    cr.stroke().ok()?;
    cr.move_to(row_header_w + 0.5, 0.0);
    cr.line_to(row_header_w + 0.5, hf);
    cr.stroke().ok()?;

    // Column letters at their ABSOLUTE sheet columns (a range starting at
    // C4 still reads C, D, E…), centered per measured width.
    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
    cr.set_font_size(20.0);
    cr.set_source_rgb(0.28, 0.29, 0.31);
    for (ci, w) in widths.iter().enumerate() {
        let letter = col_letter(col_base + ci);
        let tw = cr.text_extents(&letter).map(|e| e.width()).unwrap_or(0.0);
        cr.move_to(xs[ci] + (w - tw) / 2.0, 32.0);
        let _ = cr.show_text(&letter);
    }

    // Rows: band (file fill / plain white / quiet zebra), then per-cell
    // fills, then text in the cell's own color and weights.
    let mut y = col_header_h;
    for (ri, row) in rows.iter().take(shown).enumerate() {
        cr.set_source_rgb(0.91, 0.92, 0.93);
        cr.rectangle(0.0, y, row_header_w, row_h);
        cr.fill().ok()?;
        cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
        cr.set_font_size(18.0);
        cr.set_source_rgb(0.38, 0.39, 0.41);
        let row_num = (row_base + ri + 1).to_string();
        let tw = cr.text_extents(&row_num).map(|e| e.width()).unwrap_or(0.0);
        cr.move_to(row_header_w - tw - 12.0, y + row_h / 2.0 + 6.0);
        let _ = cr.show_text(&row_num);
        if has_styles || ri % 2 == 0 {
            cr.set_source_rgb(1.0, 1.0, 1.0);
        } else {
            cr.set_source_rgb(0.957, 0.965, 0.973);
        }
        cr.rectangle(row_header_w, y, table_w, row_h);
        cr.fill().ok()?;
        let baseline = y + row_h / 2.0 + data_fs * 0.35;
        for ci in 0..ncols {
            let cw = widths[ci];
            let cell = row.get(ci).map(|s| s.as_str()).unwrap_or("");
            let st = grid_cell_style(styles, style_pool, ri, ci);
            if let Some(fill) = st.and_then(|s| s.fill) {
                cr.set_source_rgb(
                    fill[0] as f64 / 255.0,
                    fill[1] as f64 / 255.0,
                    fill[2] as f64 / 255.0,
                );
                cr.rectangle(xs[ci], y, cw, row_h);
                cr.fill().ok()?;
            }
            let italic = st.is_some_and(|s| s.italic);
            let bold = st.map_or(!has_styles && ri == 0, |s| s.bold);
            let underline = st.is_some_and(|s| s.underline);
            let ink = st.and_then(|s| s.text);
            cr.select_font_face(
                "Sans",
                if italic {
                    cairo::FontSlant::Italic
                } else {
                    cairo::FontSlant::Normal
                },
                if bold {
                    cairo::FontWeight::Bold
                } else {
                    cairo::FontWeight::Normal
                },
            );
            cr.set_font_size(data_fs);
            match ink {
                Some(c) => {
                    cr.set_source_rgb(c[0] as f64 / 255.0, c[1] as f64 / 255.0, c[2] as f64 / 255.0)
                }
                // No style data: the header band is simply dark ink.
                None if !has_styles && ri == 0 => cr.set_source_rgb(0.10, 0.12, 0.10),
                None => cr.set_source_rgb(0.15, 0.15, 0.17),
            }
            let text = truncate_to_width(&cr, cell, cw - 18.0);
            let tw = cr.text_extents(&text).map(|e| e.width()).unwrap_or(0.0);
            let tx = if ri > 0 && cell_is_numeric(cell) {
                (xs[ci] + cw - 9.0 - tw).max(xs[ci] + 9.0)
            } else {
                xs[ci] + 9.0
            };
            cr.move_to(tx, baseline);
            let _ = cr.show_text(&text);
            if underline && !text.is_empty() {
                cr.set_line_width(1.0);
                cr.move_to(tx, baseline + 3.0);
                cr.line_to(tx + tw, baseline + 3.0);
                cr.stroke().ok()?;
            }
        }
        y += row_h;
    }

    // Grid lines: one per measured column edge + row rhythm.
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.18);
    cr.set_line_width(1.0);
    for cx in &xs {
        cr.move_to(cx + 0.5, 0.0);
        cr.line_to(cx + 0.5, y);
        cr.stroke().ok()?;
    }
    cr.move_to(row_header_w + table_w + 0.5, 0.0);
    cr.line_to(row_header_w + table_w + 0.5, y);
    cr.stroke().ok()?;
    let mut gy = col_header_h;
    while gy < y {
        cr.move_to(0.0, gy + 0.5);
        cr.line_to(wf, gy + 0.5);
        cr.stroke().ok()?;
        gy += row_h;
    }
    if y > col_header_h {
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.70);
        cr.set_line_width(1.2);
        cr.rectangle(
            row_header_w + 0.5,
            col_header_h + 0.5,
            table_w - 1.0,
            row_h - 1.0,
        );
        cr.stroke().ok()?;
    }
    if rows.is_empty() {
        cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
        cr.set_font_size(26.0);
        cr.set_source_rgb(0.52, 0.54, 0.57);
        let msg = "empty sheet";
        let tw = cr.text_extents(msg).map(|e| e.width()).unwrap_or(0.0);
        cr.move_to(row_header_w + (table_w - tw) / 2.0, col_header_h + avail_h / 2.0);
        let _ = cr.show_text(msg);
    }

    // Sheet tab (real sheet name) bottom-left, overflow notes center,
    // folio bottom-right.
    let tab_y = hf - 44.0;
    cr.set_source_rgb(0.95, 0.96, 0.97);
    cr.rectangle(16.0, tab_y, 200.0, 36.0);
    cr.fill().ok()?;
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.16);
    cr.set_line_width(1.0);
    cr.rectangle(16.5, tab_y + 0.5, 199.0, 35.0);
    cr.stroke().ok()?;
    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
    cr.set_font_size(19.0);
    cr.set_source_rgb(0.20, 0.21, 0.23);
    let shown_sheet = truncate_to_width(&cr, sheet, 176.0);
    cr.move_to(30.0, tab_y + 24.0);
    let _ = cr.show_text(&shown_sheet);

    let mut notes: Vec<String> = Vec::new();
    if hidden_rows > 0 {
        notes.push(format!(
            "+{} more {}",
            hidden_rows,
            if hidden_rows == 1 { "row" } else { "rows" }
        ));
    }
    if hidden_cols > 0 {
        notes.push(format!(
            "+{} more {}",
            hidden_cols,
            if hidden_cols == 1 { "column" } else { "columns" }
        ));
    }
    if !notes.is_empty() {
        cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
        cr.set_font_size(16.0);
        cr.set_source_rgb(0.45, 0.47, 0.50);
        let joined = truncate_to_width(&cr, &notes.join("  \u{b7}  "), 330.0);
        cr.move_to(236.0, tab_y + 24.0);
        let _ = cr.show_text(&joined);
    }

    draw_folio(&cr, page_no, total, wf - 24.0, tab_y + 24.0, 16.0);

    cr.set_source_rgb(0.88, 0.88, 0.90);
    cr.set_line_width(1.0);
    cr.rectangle(0.5, 0.5, wf - 1.0, hf - 1.0);
    cr.stroke().ok()?;

    surface_to_png(surface, cr)
}

/// A cell's pooled style when it deviates from a plain default
/// (`u16::MAX` and short vectors both mean "no styling here").
fn grid_cell_style<'a>(
    styles: &'a [Vec<u16>],
    pool: &'a [SheetCellStyle],
    ri: usize,
    ci: usize,
) -> Option<&'a SheetCellStyle> {
    let idx = *styles.get(ri)?.get(ci)?;
    if idx == u16::MAX {
        return None;
    }
    pool.get(idx as usize)
}

/// Spreadsheet column letter for an absolute column index: 0 → A, 25 → Z,
/// 26 → AA — the same labeling Excel shows in its own header.
fn col_letter(mut i: usize) -> String {
    let mut out = Vec::new();
    loop {
        out.push((b'A' + (i % 26) as u8) as char);
        i /= 26;
        if i == 0 {
            break;
        }
        i -= 1;
    }
    out.reverse();
    out.into_iter().collect()
}

/// Does this cell hold a date/time? ("2023-07-15", "15/07/2023", "14:30")
fn looks_like_date_or_time(t: &str) -> bool {
    let sep_parts: &[&str] = if t.contains('-') {
        &["-"]
    } else if t.contains('/') {
        &["/"]
    } else {
        &[]
    };
    if !sep_parts.is_empty() {
        let parts: Vec<&str> = t.split(sep_parts[0]).collect();
        if parts.len() == 3
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        {
            return true;
        }
    }
    match t.split_once(':') {
        Some((h, rest)) => {
            !h.is_empty()
                && h.chars().all(|c| c.is_ascii_digit())
                && rest
                    .split(':')
                    .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        }
        None => false,
    }
}

/// Right-align like a spreadsheet does: numbers (plain, grouped, percent,
/// currency-prefixed) and dates/times are numeric content; everything else
/// stays text. The header row bypasses this at the call site.
fn cell_is_numeric(s: &str) -> bool {
    let mut t = s.trim();
    if let Some(rest) = t.strip_prefix(['+', '-']) {
        t = rest.trim_start();
    }
    if let Some(rest) = t.strip_prefix(['$', '\u{20ac}', '\u{a3}', '\u{a5}']) {
        t = rest.trim_start();
    }
    if t.is_empty() {
        return false;
    }
    let t = t.strip_suffix('%').unwrap_or(t).trim_end();
    if t.is_empty() {
        return false;
    }
    let (mut digits, mut dots) = (0usize, 0usize);
    for c in t.chars() {
        if c.is_ascii_digit() {
            digits += 1;
        } else if c == '.' {
            dots += 1;
        } else if c == ',' || c == ' ' || c == '\u{a0}' || c == 'e' || c == 'E' {
            // grouping / no-break space / scientific marker
        } else {
            return looks_like_date_or_time(t);
        }
    }
    digits > 0 && dots <= 1
}

/// One ODF presentation slide: title band, accent rule, bullet body —
/// same fidelity class as the legacy .ppt renderer (title + bullets).
fn render_odf_slide(title: &str, bullets: &[String], page_no: usize, total: usize) -> Option<Vec<u8>> {
    use gtk::cairo;
    const W: i32 = 1280;
    const H: i32 = 960;
    let (surface, cr) = new_surface(W, H)?;
    let wf = W as f64;
    let hf = H as f64;

    cr.set_source_rgb(1.0, 1.0, 1.0);
    cr.paint().ok()?;
    let (ar, ag, ab) = accent_for(&OfficeKind::Presentation);

    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
    cr.set_font_size(44.0);
    cr.set_source_rgb(ar, ag, ab);
    let mut ty = 108.0;
    for line in wrap_lines_full(&cr, title, wf - 140.0).into_iter().take(2) {
        cr.move_to(64.0, ty);
        let _ = cr.show_text(&line);
        ty += 56.0;
    }
    cr.set_source_rgba(ar, ag, ab, 0.55);
    cr.set_line_width(3.0);
    cr.move_to(64.0, ty + 4.0);
    cr.line_to(wf - 64.0, ty + 4.0);
    cr.stroke().ok()?;

    let mut y = ty + 76.0;
    for bullet in bullets.iter().take(12) {
        if y > hf - 60.0 {
            break;
        }
        y = draw_odf_bullet(&cr, bullet, 84.0, y, wf - 180.0);
    }

    draw_folio(&cr, page_no, total, wf - 64.0, hf - 32.0, 11.0);

    cr.set_source_rgb(0.85, 0.85, 0.87);
    cr.set_line_width(1.0);
    cr.rectangle(0.5, 0.5, wf - 1.0, hf - 1.0);
    cr.stroke().ok()?;

    surface_to_png(surface, cr)
}

/// Bullet with accent square marker; returns the y for the next bullet.
fn draw_odf_bullet(cr: &gtk::cairo::Context, text: &str, x: f64, y: f64, max_w: f64) -> f64 {
    use gtk::cairo;
    let (ar, ag, ab) = accent_for(&OfficeKind::Presentation);
    cr.set_source_rgb(ar, ag, ab);
    cr.rectangle(x, y - 15.0, 8.0, 8.0);
    let _ = cr.fill();
    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
    cr.set_font_size(23.0);
    cr.set_source_rgb(0.10, 0.10, 0.12);
    let mut out_y = y;
    for line in wrap_lines_full(cr, text, max_w - 24.0) {
        cr.move_to(x + 24.0, out_y);
        let _ = cr.show_text(&line);
        out_y += 34.0;
    }
    out_y + 12.0
}

/// Small gray "n / total" bottom-right folio on rendered office pages.
fn draw_folio(
    cr: &gtk::cairo::Context,
    page_no: usize,
    total: usize,
    right_x: f64,
    baseline: f64,
    size: f64,
) {
    use gtk::cairo;
    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
    cr.set_font_size(size);
    cr.set_source_rgb(0.55, 0.55, 0.58);
    let folio = format!("{}/{}", page_no, total);
    let tw = cr.text_extents(&folio).map(|e| e.width()).unwrap_or(0.0);
    cr.move_to(right_x - tw, baseline);
    let _ = cr.show_text(&folio);
}

/// Accent color per document kind (PowerPoint orange / Word blue / Excel green).
fn accent_for(kind: &OfficeKind) -> (f64, f64, f64) {
    match kind {
        OfficeKind::Presentation => (0.78, 0.32, 0.18),
        OfficeKind::Document => (0.16, 0.33, 0.60),
        OfficeKind::Spreadsheet => (0.13, 0.53, 0.30),
    }
}

/// Truncate a string with an ellipsis so it fits within `max_w` pixels for the
/// current Cairo font settings.
fn truncate_to_width(cr: &gtk::cairo::Context, text: &str, max_w: f64) -> String {
    if cr.text_extents(text).map(|e| e.width()).unwrap_or(0.0) <= max_w {
        return text.to_string();
    }
    let mut s = String::new();
    for ch in text.chars() {
        let mut trial = s.clone();
        trial.push(ch);
        trial.push('…');
        if cr
            .text_extents(&trial)
            .map(|e| e.width())
            .unwrap_or(f64::MAX)
            > max_w
        {
            s.push('…');
            return s;
        }
        s.push(ch);
    }
    s
}

// ──────────────────────────────────────────────────────────────────────
// Layout-aware PowerPoint preview
//
// PPTX slides describe each shape's position and size in EMUs (English Metric
// Units; 914,400 per inch). We walk the shape tree recursively in document
// order — shapes, pictures, groups (with their chOff/chExt child transforms),
// and connectors — compose the page from master → layout → slide parts, and
// draw everything scaled to the real slide aspect ratio (from
// presentation.xml). Backgrounds resolve slide → layout → master (solid,
// gradient, picture, theme bgRef), placeholder geometry and text styles
// inherit from the layout/master, and text renders through Pango with
// per-run size/weight/color/alignment. Charts, tables, and SmartArt diagrams
// are not rendered (yet). Everything is pure Rust — no external converter.
// ──────────────────────────────────────────────────────────────────────

/// One formatted text run inside a paragraph. Formatting is resolved at parse
/// time (run → paragraph defaults → placeholder/master styles → theme), so the
/// renderer only needs these effective values.
#[derive(Clone, Default, Debug)]
struct TextRun {
    text: String,
    sz_pt: Option<f64>,
    bold: bool,
    italic: bool,
    underline: bool,
    color: Option<(f64, f64, f64)>,
    font: Option<String>,
}

/// Bullet treatment for a paragraph.
#[derive(Clone, Debug)]
enum ParaBullet {
    Off,
    Char(String),
    /// Auto-numbered bullet (`arabicPeriod`, …) with its 1-based counter.
    Number(String, u32),
}

/// One paragraph of rich text (a `<a:p>`; line breaks become follow-up
/// paragraphs with `follow` set, sharing alignment but carrying no bullet).
#[derive(Clone, Default, Debug)]
struct TextPara {
    runs: Vec<TextRun>,
    /// Effective alignment: "l" | "ctr" | "r".
    algn: String,
    lvl: usize,
    bullet: Option<ParaBullet>,
    spc_bef_pt: f64,
    spc_aft_pt: f64,
    /// Continuation line after an `<a:br>`: no bullet, no spacing.
    follow: bool,
    /// `marL`: text-column offset from the box's left inset (EMU).
    mar_l_emu: f64,
    /// Hanging-indent zone (EMU): the first line starts this far left of the
    /// text column so the bullet sits in the margin (PPT `indent`, else marL).
    hang_emu: f64,
}

/// One positioned text box on a slide. Coordinates are in EMUs.
#[derive(Clone, Debug)]
struct SlideBox {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    text: String,
    is_title: bool,
    centered: bool,
    font_pt: Option<f64>,
    has_xfrm: bool,
    ph_type: String,
    color: Option<(f64, f64, f64)>,
    /// Structured PPTX text (None for the legacy binary .ppt path).
    paras: Option<Vec<TextPara>>,
    /// Vertical anchor: 0 = top, 1 = center, 2 = bottom.
    anchor: u8,
    /// Text-frame insets: left, top, right, bottom (EMU).
    insets: [f64; 4],
    /// `normAutofit` fontScale factor (1.0 = unchanged).
    autofit_scale: f64,
}

#[derive(Debug)]
enum SlideBackground {
    Solid(f64, f64, f64),
    Image(PathBuf),
    /// Linear gradient: (position 0..1, color, alpha 0..1) stops + angle (rad,
    /// cairo convention: 0 = left→right, positive = clockwise/down).
    Gradient(Vec<(f64, (f64, f64, f64), f64)>, f64),
}

/// Fill of a shape (`<a:noFill>`/`<a:solidFill>`/`<a:gradFill>`/`<a:pattFill>`).
#[derive(Clone, Debug)]
enum FillKind {
    None,
    Solid((f64, f64, f64), f64),
    Gradient(Vec<(f64, (f64, f64, f64), f64)>, f64),
}

/// Shape outline (`<a:ln>`): color, alpha, width in EMUs.
#[derive(Clone, Debug)]
struct LineSpec {
    color: (f64, f64, f64),
    alpha: f64,
    width_emu: f64,
}

/// One command of a `<a:custGeom>` freeform path. Coordinates are in the
/// path's own space (`<a:path w h>`), mapped onto the shape rect at draw time.
#[derive(Clone, Debug)]
enum PathCmd {
    MoveTo(f64, f64),
    LineTo(f64, f64),
    CubicBezTo([f64; 6]),
    QuadBezTo([f64; 4]),
    Close,
}

/// One `<a:path>` from a `<a:custGeom>` `<a:pathLst>`: `w`/`h` is its
/// coordinate space, `fill` false for `fill="none"` subpaths (stroked but
/// never filled — the stroke-only layers of compound shapes).
#[derive(Clone, Debug)]
struct FreeformPath {
    w: f64,
    h: f64,
    fill: bool,
    cmds: Vec<PathCmd>,
}

/// A filled/outlined shape (autoshape, text-box background, connector).
/// Coordinates are absolute EMUs on the slide; `rot` is clockwise radians.
/// `freeform` is set for `<a:custGeom>` geometry and wins over `prst`.
#[derive(Clone, Debug)]
struct DrawShape {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    prst: String,
    fill: Option<FillKind>,
    line: Option<LineSpec>,
    rot: f64,
    flip_h: bool,
    flip_v: bool,
    freeform: Option<Vec<FreeformPath>>,
}

/// A picture: absolute EMU rect, extracted media path, and `srcRect` crop
/// (fractions of the image cut from left/top/right/bottom).
#[derive(Clone, Debug)]
struct DrawPicture {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    path: PathBuf,
    crop: [f64; 4],
    line: Option<LineSpec>,
    rot: f64,
    flip_h: bool,
    flip_v: bool,
}

/// Parsed `c:doughnutChart`: data points, per-point colors (filled in from
/// `c:dPt`, theme accents, or the series fill — always `Some` after parse),
/// hole size (% of radius) and the start angle (degrees, 0 = 12 o'clock).
#[derive(Clone, Debug)]
struct DoughnutSpec {
    values: Vec<f64>,
    colors: Vec<Option<(f64, f64, f64)>>,
    hole_pct: f64,
    first_ang_deg: f64,
}

/// A chart `p:graphicFrame` positioned on the slide (doughnut charts only —
/// other chart types, tables, SmartArt stay Phase 3).
#[derive(Clone, Debug)]
struct DrawChart {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    doughnut: DoughnutSpec,
}

#[derive(Debug)]
enum SlideElement {
    Shape(DrawShape),
    Picture(DrawPicture),
    Chart(DrawChart),
    Text(SlideBox),
}

struct SlideLayout {
    slide_w: f64,
    slide_h: f64,
    background: Option<SlideBackground>,
    elements: Vec<SlideElement>,
}

/// Parse legacy binary PowerPoint (.ppt) text atoms from the OLE container and
/// map the first meaningful text runs onto a simple slide layout. This is not a
/// full MS-PPT renderer, but it produces a real slide-style overview instead of
/// the generic file card when no embedded thumbnail is available.
/// Raw "PowerPoint Document" OLE stream of a legacy binary .ppt.
fn legacy_ppt_stream(doc: &Path) -> Option<Vec<u8>> {
    use std::io::Read;

    let mut comp = cfb::open(doc).ok()?;
    let stream_name = if comp.exists("/PowerPoint Document") {
        "/PowerPoint Document"
    } else if comp.exists("PowerPoint Document") {
        "PowerPoint Document"
    } else {
        return None;
    };
    let mut stream = comp.open_stream(stream_name).ok()?;
    let mut bytes = Vec::new();
    stream.take(32 * 1024 * 1024 + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() <= 32 * 1024 * 1024).then_some(bytes)
}

/// The deck's `Pictures` storage stream — concatenated image records, each
/// `[header|uid(16)|tag|raw image]` — or None when the deck has none.
fn legacy_ppt_pictures_stream(doc: &Path) -> Option<Vec<u8>> {
    use std::io::Read;

    let mut comp = cfb::open(doc).ok()?;
    let stream_name = if comp.exists("/Pictures") {
        "/Pictures"
    } else if comp.exists("Pictures") {
        "Pictures"
    } else {
        return None;
    };
    let mut stream = comp.open_stream(stream_name).ok()?;
    let mut bytes = Vec::new();
    stream.take(32 * 1024 * 1024 + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() <= 32 * 1024 * 1024).then_some(bytes)
}

/// The deck's `Current User` stream — the entry point (offset word at 16)
/// into the persist chain that says which slide records are live, or None
/// when the deck has no such stream (older/synthetic files).
fn legacy_ppt_current_user(doc: &Path) -> Option<Vec<u8>> {
    use std::io::Read;

    let mut comp = cfb::open(doc).ok()?;
    let stream_name = if comp.exists("/Current User") {
        "/Current User"
    } else if comp.exists("Current User") {
        "Current User"
    } else {
        return None;
    };
    let mut stream = comp.open_stream(stream_name).ok()?;
    let mut bytes = Vec::new();
    stream.take(32 * 1024 * 1024 + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() <= 32 * 1024 * 1024).then_some(bytes)
}

/// Collect every record-1006 (slide) payload in `bytes`, in stream order.
/// A matched record's payload is taken whole (slides don't nest); other
/// container records are descended into so slides wrapped inside document
/// containers are found too.
fn collect_ppt_slide_payloads<'a>(bytes: &'a [u8], out: &mut Vec<&'a [u8]>, depth: usize) {
    if depth > 12 {
        return;
    }
    let mut pos = 0usize;
    while pos + 8 <= bytes.len() {
        let rec_info = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]);
        let rec_ver = rec_info & 0x000f;
        let rec_type = u16::from_le_bytes([bytes[pos + 2], bytes[pos + 3]]);
        let rec_len = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]) as usize;
        pos += 8;
        if rec_len > bytes.len().saturating_sub(pos) {
            break;
        }
        let payload = &bytes[pos..pos + rec_len];
        if rec_type == 1006 {
            out.push(payload);
        } else if rec_ver == 0x000f {
            collect_ppt_slide_payloads(payload, out, depth + 1);
        }
        pos += rec_len;
    }
}

/// Text lines of EVERY slide of a legacy .ppt stream — one entry per slide
/// record, in stream order. Falls back to the whole-stream text blob as a
/// single slide when no slide records exist (or none carry text), matching
/// the old first-slide-only extraction for unusual files.
fn legacy_ppt_slide_texts(bytes: &[u8]) -> Vec<Vec<String>> {
    let mut payloads = Vec::new();
    collect_ppt_slide_payloads(bytes, &mut payloads, 0);
    if !payloads.is_empty() {
        let slides = payloads
            .iter()
            .map(|p| {
                let mut lines = Vec::new();
                collect_ppt_text_atoms(p, &mut lines, 0);
                lines
            })
            .collect::<Vec<_>>();
        if slides.iter().any(|s| !s.is_empty()) {
            return slides;
        }
    }
    vec![extract_ppt_text_atoms(bytes)]
}

/// Slide count of a legacy binary .ppt — one per legible slide, at least 1.
fn legacy_ppt_slide_count(doc: &Path) -> Option<usize> {
    let bytes = legacy_ppt_stream(doc)?;
    // Persist order first: the CurrentUser chain lists exactly the live
    // slides (stream order can carry stale duplicates after edits).
    let current_user = legacy_ppt_current_user(doc);
    if let Some(slides) = legacy_ppt_persist_slides(&bytes, current_user.as_deref()) {
        return Some(slides.len().max(1));
    }
    // Raw slide-record count next: stays correct even for decks where no
    // slide carries legible text (the text walk then collapses to one blob).
    let mut payloads = Vec::new();
    collect_ppt_slide_payloads(&bytes, &mut payloads, 0);
    if !payloads.is_empty() {
        return Some(payloads.len().max(1));
    }
    Some(legacy_ppt_slide_texts(&bytes).len().max(1))
}

// ---------------------------------------------------------------------------
// Structured legacy .ppt parse (stage 1): decode a slide's escher Drawing
// (PPDrawing, record 1036) into the shared pptx render model so binary .ppt
// decks reuse `render_slide_layout`. Pictures, master backgrounds and
// per-run formatting arrive in later stages; until then those shapes are
// skipped and callers fall back to the text/blue-wave renderer when nothing
// parses at all.
// ---------------------------------------------------------------------------

/// Master unit → EMU: PowerPoint stores slide coordinates in 1/100" units
/// (5760 units = 10" = 9_144_000 EMU).
const PPT_MU_EMU: f64 = 1587.5;

/// Header of the MS-PPT record at `pos`: (version, instance, type, payload,
/// offset of the next record). None when truncated.
fn legacy_ppt_hdr(bytes: &[u8], pos: usize) -> Option<(u16, u16, u16, &[u8], usize)> {
    if pos + 8 > bytes.len() {
        return None;
    }
    let info = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]);
    let rec_type = u16::from_le_bytes([bytes[pos + 2], bytes[pos + 3]]);
    let len =
        u32::from_le_bytes([bytes[pos + 4], bytes[pos + 5], bytes[pos + 6], bytes[pos + 7]]) as usize;
    let end = pos.checked_add(8)?.checked_add(len)?;
    if end > bytes.len() {
        return None;
    }
    Some((info & 0x000f, info >> 4, rec_type, &bytes[pos + 8..end], end))
}

/// First payload of record type `rec_type`, descending into containers.
fn legacy_ppt_find(bytes: &[u8], rec_type: u16, depth: usize) -> Option<&[u8]> {
    if depth > 12 {
        return None;
    }
    let mut pos = 0usize;
    while pos + 8 <= bytes.len() {
        let Some((ver, _, ty, payload, next)) = legacy_ppt_hdr(bytes, pos) else {
            break;
        };
        if ty == rec_type {
            return Some(payload);
        }
        if ver == 0x0f {
            if let Some(found) = legacy_ppt_find(payload, rec_type, depth + 1) {
                return Some(found);
            }
        }
        pos = next;
    }
    None
}

/// Slide size in EMUs from the DocumentAtom (record 1001): its first two
/// u32s are width/height in master units. Falls back to PowerPoint's
/// default 4:3 size when the atom is missing or implausible.
fn legacy_ppt_slide_size(bytes: &[u8]) -> (f64, f64) {
    let default = (9_144_000.0, 6_858_000.0);
    let Some(pay) = legacy_ppt_find(bytes, 1001, 0) else {
        return default;
    };
    if pay.len() < 8 {
        return default;
    }
    let w = u32::from_le_bytes([pay[0], pay[1], pay[2], pay[3]]) as f64;
    let h = u32::from_le_bytes([pay[4], pay[5], pay[6], pay[7]]) as f64;
    if !(1_000.0..=60_000.0).contains(&w) || !(1_000.0..=60_000.0).contains(&h) {
        return default;
    }
    (w * PPT_MU_EMU, h * PPT_MU_EMU)
}

/// The slide's 8-entry color scheme (background, textAndLines, shadows,
/// titleText, fills, accent, accent/hyperlink, accent/following) from its
/// first ColorSchemeAtom (2032), or the classic Office defaults when the
/// atom is absent.
fn legacy_ppt_scheme(slide: &[u8]) -> [(f64, f64, f64); 8] {
    let default = [
        (1.0, 1.0, 1.0),        // background
        (0.0, 0.0, 0.0),        // textAndLines
        (0.933, 0.925, 0.882),  // shadows
        (0.122, 0.286, 0.490),  // titleText (#1F497D)
        (0.310, 0.506, 0.741),  // fills
        (0.753, 0.314, 0.302),  // accent
        (0.0, 0.0, 1.0),        // accent/hyperlink
        (0.502, 0.0, 0.502),    // accent/following
    ];
    let Some(pay) = legacy_ppt_find(slide, 2032, 0) else {
        return default;
    };
    if pay.len() < 32 {
        return default;
    }
    let mut out = default;
    for (i, slot) in out.iter_mut().enumerate() {
        let v = u32::from_le_bytes([pay[i * 4], pay[i * 4 + 1], pay[i * 4 + 2], pay[i * 4 + 3]]);
        // COLORREF: low byte = red.
        *slot = (
            (v & 0xff) as f64 / 255.0,
            ((v >> 8) & 0xff) as f64 / 255.0,
            ((v >> 16) & 0xff) as f64 / 255.0,
        );
    }
    out
}

/// Resolve one shape-property color word: scheme colors carry 0x08 in the
/// top byte (scheme index low), plain colors are COLORREFs (R, G, B from
/// the low bytes).
fn legacy_ppt_rgb(v: u32, scheme: &[(f64, f64, f64); 8]) -> (f64, f64, f64) {
    if v & 0xff00_0000 == 0x0800_0000 {
        return scheme[(v & 0xff).min(7) as usize];
    }
    (
        (v & 0xff) as f64 / 255.0,
        ((v >> 8) & 0xff) as f64 / 255.0,
        ((v >> 16) & 0xff) as f64 / 255.0,
    )
}

/// Simple values of one shape property list (OfficeArtFOPT): fixed 6-byte
/// headers (u16 opid [+0x8000 = complex] + u32 value/length) run first, the
/// variable-length blobs follow all headers, each padded to 4 bytes. Only
/// the simple values are needed here; the blobs' total length marks where
/// the header run ends.
fn legacy_ppt_props(pay: &[u8]) -> Vec<(u16, u32)> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    let mut blobs = 0usize;
    while pos + 6 <= pay.len() {
        let op = u16::from_le_bytes([pay[pos], pay[pos + 1]]);
        let v = u32::from_le_bytes([pay[pos + 2], pay[pos + 3], pay[pos + 4], pay[pos + 5]]);
        pos += 6;
        if op & 0x8000 != 0 {
            blobs += (v as usize + 3) & !3;
            if pos + blobs == pay.len() {
                break;
            }
        } else {
            out.push((op & 0x3fff, v));
        }
    }
    out
}

/// Plain text of one client-textbox record (F00D): concatenates its
/// TextChars (4000) / TextBytes (4008) / CString (4026) atoms in order,
/// preserving every line — unlike the cleaned heuristic extractor below.
fn legacy_ppt_box_text(bytes: &[u8], out: &mut String, depth: usize) {
    if depth > 8 {
        return;
    }
    let mut pos = 0usize;
    while pos + 8 <= bytes.len() {
        let Some((ver, _, rec_type, payload, next)) = legacy_ppt_hdr(bytes, pos) else {
            break;
        };
        match rec_type {
            4000 | 4026 => out.push_str(&decode_utf16le_lossy(payload)),
            4008 => out.push_str(&decode_ppt_8bit_text(payload)),
            _ => {}
        }
        if ver == 0x0f {
            legacy_ppt_box_text(payload, out, depth + 1);
        }
        pos = next;
    }
}

/// Map PowerPoint's paragraph/line separators (\r, VT, FF) onto '\n' and
/// drop stray control characters.
fn legacy_ppt_normalize_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\u{b}' | '\u{c}' => out.push('\n'),
            c if c != '\n' && c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out.trim().to_string()
}

/// Every shape container (F004) payload under `bytes`, in document order;
/// containers are descended so shapes nested in groups (F009) surface too.
fn legacy_ppt_collect_shapes<'a>(bytes: &'a [u8], out: &mut Vec<&'a [u8]>, depth: usize) {
    if depth > 12 {
        return;
    }
    let mut pos = 0usize;
    while pos + 8 <= bytes.len() {
        let Some((ver, _, rec_type, payload, next)) = legacy_ppt_hdr(bytes, pos) else {
            break;
        };
        if rec_type == 0xf004 {
            out.push(payload);
        }
        if ver == 0x0f {
            legacy_ppt_collect_shapes(payload, out, depth + 1);
        }
        pos = next;
    }
}

/// Shape rect in master units from a client anchor: F00F carries 4×i32
/// (x1, y1, x2, y2); F010 packs the same rect as four signed u16s
/// (y1, x1, x2, y2). Normalized to min/max per axis — both encodings of
/// the same rect decode identically (byte-verified against decks).
fn legacy_ppt_anchor_rect(payload: &[u8]) -> Option<(f64, f64, f64, f64)> {
    let s16 = |v: u16| if v > 32767 { v as f64 - 65_536.0 } else { v as f64 };
    let (x1, y1, x2, y2) = if payload.len() >= 16 {
        (
            i32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as f64,
            i32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]) as f64,
            i32::from_le_bytes([payload[8], payload[9], payload[10], payload[11]]) as f64,
            i32::from_le_bytes([payload[12], payload[13], payload[14], payload[15]]) as f64,
        )
    } else if payload.len() >= 8 {
        let h = [
            u16::from_le_bytes([payload[0], payload[1]]),
            u16::from_le_bytes([payload[2], payload[3]]),
            u16::from_le_bytes([payload[4], payload[5]]),
            u16::from_le_bytes([payload[6], payload[7]]),
        ];
        (s16(h[1]), s16(h[0]), s16(h[2]), s16(h[3]))
    } else {
        return None;
    };
    Some((x1.min(x2), y1.min(y2), x1.max(x2), y1.max(y2)))
}

/// Stage-1 font size for a plain-text box: fit the text (honouring hard
/// line breaks) into the box with PowerPoint's ~0.5em average glyph width.
/// Real run sizes arrive with StyleTextPropAtom in a later stage.
fn legacy_ppt_estimate_pt(w: f64, h: f64, text: &str) -> f64 {
    let w_pt = (w / 12_700.0).max(1.0);
    let h_pt = (h / 12_700.0).max(1.0);
    let mut pt = 18.0f64;
    for _ in 0..8 {
        let cpl = (w_pt / (0.5 * pt)).max(1.0);
        let lines = text
            .split('\n')
            .map(|seg| ((seg.chars().count() as f64 / cpl).ceil()).max(1.0))
            .sum::<f64>();
        let fit = (h_pt * 0.85 / lines).clamp(8.0, 60.0);
        if (fit - pt).abs() < 0.5 {
            return fit;
        }
        pt = fit;
    }
    pt
}

// ---------------------------------------------------------------------------
// Structured legacy .ppt parse (stage 3): outline text + per-run styles.
//
// PowerPoint 97 keeps placeholder text in the document's SlideListWithText
// (4080) blocks instead of on the slide, so a slide's escher boxes arrive
// empty — and even box text that exists is styled by a StyleTextPropAtom
// (4001) whose `cch` counts describe the raw text. The persist chain
// (Current User → UserEditAtom → persist tables) says which slides are live
// and maps each block's refID onto its slide record; this section decodes
// that, plus the atom itself (and the 4003 master styles below it) into the
// shared `TextPara`/`TextRun` model.
// ---------------------------------------------------------------------------

/// One outline-text entry of a SlideListWithText block: the textType its
/// TextHeaderAtom names, the entry's concatenated text, and its own
/// StyleTextPropAtom (the bytes that entry's `cch` counts describe).
struct LegacyPptTextEntry<'a> {
    tt: u32,
    text: String,
    style: Option<&'a [u8]>,
}

/// One slide in show order: its stream payload plus the outline-text
/// entries the document keeps for it (the text of placeholder shapes that
/// carry none of their own).
struct LegacyPptSlideRef<'a> {
    payload: &'a [u8],
    entries: Vec<LegacyPptTextEntry<'a>>,
}

/// Per-level base style of a TextMasterStyleAtom (4003): what a box's own
/// runs fall back to. Colors stay as raw ColorIndexStruct words, resolved
/// against the slide's scheme when a run is built.
#[derive(Clone, Copy, Default)]
struct LegacyPptLevelStyle {
    align: Option<&'static str>,
    has_bullet: Option<bool>,
    bullet_char: Option<u16>,
    bullet_font: Option<u16>,
    mar_l_emu: Option<f64>,
    indent_emu: Option<f64>,
    size_pt: Option<f64>,
    color_raw: Option<u32>,
    cf: Option<u16>,
}

/// One paragraph run of a StyleTextPropAtom: the characters it covers plus
/// the paragraph properties it overrides (absent fields inherit from the
/// master level).
#[derive(Clone, Copy, Default)]
struct LegacyPptParaRun {
    cch: usize,
    level: u16,
    align: Option<&'static str>,
    has_bullet: Option<bool>,
    bullet_char: Option<u16>,
    bullet_font: Option<u16>,
    mar_l_emu: Option<f64>,
    indent_emu: Option<f64>,
}

/// One char run of a StyleTextPropAtom: the characters it covers plus the
/// character properties it overrides (absent fields carry the previous
/// state).
#[derive(Clone, Copy, Default)]
struct LegacyPptCharRun {
    cch: usize,
    size_pt: Option<f64>,
    color_raw: Option<u32>,
    cf: Option<u16>,
}

/// Slide-wide state for one slide's shape pushes: the outline-text entries
/// still to place, and the deck's master text styles keyed by textType —
/// plus the font list bulletFontRef indexes resolve against.
struct LegacyPptTextCtx<'a> {
    pending: Vec<LegacyPptTextEntry<'a>>,
    master: std::collections::HashMap<u32, Vec<LegacyPptLevelStyle>>,
    fonts: std::collections::HashMap<u16, String>,
}

/// TextPFException property table [MS-PPT]: (property mask, byte size) in
/// the order the fields appear when their bit is set — `None` is the
/// variable tab-stop list (count, then count * 4 bytes), `Some(0)` a
/// zero-size field that only exists.
const PPT_PARA_PROPS: [(u32, Option<u32>); 19] = [
    (0x0000_000F, Some(2)),  // bulletFlags
    (0x0000_0080, Some(2)),  // bulletChar
    (0x0000_0010, Some(2)),  // bulletFontRef
    (0x0000_0040, Some(2)),  // bulletSize
    (0x0000_0020, Some(4)),  // bulletColor
    (0x0000_0800, Some(2)),  // textAlignment
    (0x0000_1000, Some(2)),  // lineSpacing
    (0x0000_2000, Some(2)),  // spaceBefore
    (0x0000_4000, Some(2)),  // spaceAfter
    (0x0000_0100, Some(2)),  // leftMargin
    (0x0000_0400, Some(2)),  // indent
    (0x0000_8000, Some(2)),  // defaultTabSize
    (0x0010_0000, None),     // tabStops
    (0x0001_0000, Some(2)),  // fontAlign
    (0x000E_0000, Some(2)),  // wrapFlags
    (0x0020_0000, Some(2)),  // textDirection
    (0x0080_0000, Some(0)),  // X
    (0x0100_0000, Some(0)),  // Y
    (0x0200_0000, Some(0)),  // Z
];

/// TextCFException property table [MS-PPT], same layout rules.
const PPT_CHAR_PROPS: [(u32, Option<u32>); 12] = [
    (0x0010_0000, Some(0)),
    (0x0100_0000, Some(0)),
    (0x0200_0000, Some(0)),
    (0x0400_0000, Some(0)),
    (0x0000_FFFF, Some(2)), // CFStyle (bit0 bold, bit1 italic, bit2 underline)
    (0x0001_0000, Some(2)), // fontRef
    (0x0020_0000, Some(2)), // oldEAFontRef
    (0x0040_0000, Some(2)), // ansiFontRef
    (0x0080_0000, Some(2)), // symbolFontRef
    (0x0002_0000, Some(2)), // fontSize (points)
    (0x0004_0000, Some(4)), // color (ColorIndexStruct)
    (0x0008_0000, Some(2)), // position
];

/// Little-endian readers for the fixed fields the decodes peek at (each
/// call site bounds-checks first).
fn legacy_ppt_u16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn legacy_ppt_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([
        bytes[at],
        bytes[at + 1],
        bytes[at + 2],
        bytes[at + 3],
    ])
}

fn legacy_ppt_i16(bytes: &[u8], at: usize) -> i16 {
    i16::from_le_bytes([bytes[at], bytes[at + 1]])
}

/// TextAlignmentEnum → the shared model's alignment token ("justify" has
/// no binding here and wraps like left).
fn legacy_ppt_align_str(v: u16) -> &'static str {
    match v {
        1 => "ctr",
        2 => "r",
        _ => "l",
    }
}

/// Text color from a ColorIndexStruct word: index 0..=7 picks the slide's
/// scheme color, 0xFE the explicit sRGB triple, and "automatic" (0xFF)
/// plus unknown indexes defer to the box color (None). Not the same
/// encoding as `legacy_ppt_rgb` (escher) — the index lives in the top byte.
fn legacy_ppt_text_color(v: u32, scheme: &[(f64, f64, f64); 8]) -> Option<(f64, f64, f64)> {
    match (v >> 24) as u8 {
        idx @ 0..=7 => Some(scheme[idx as usize]),
        0xfe => Some((
            (v & 0xff) as f64 / 255.0,
            ((v >> 8) & 0xff) as f64 / 255.0,
            ((v >> 16) & 0xff) as f64 / 255.0,
        )),
        _ => None,
    }
}

/// Bullet glyph for a `bulletChar`: NUL, control and whitespace codes can't
/// draw a marker and fall back to the default dot; anything else goes
/// through `map_bullet_char` with the bullet's font from the document font
/// list, so symbol-font bytes (Wingdings `l` = ●) become real glyphs while
/// text-font characters stay literal.
fn legacy_ppt_bullet_char(code: Option<u16>, font: Option<&str>) -> char {
    let code = code.unwrap_or(0);
    match char::from_u32(code as u32) {
        Some(c) if code != 0 && !c.is_control() && !c.is_whitespace() => {
            map_bullet_char(c, font).chars().next().unwrap_or('\u{2022}')
        }
        _ => '\u{2022}',
    }
}

/// Walk the property bytes a flags word announces, in table order, handing
/// each present field's bytes to `cap`. False when a field runs off the
/// record end (the decode then reports the style unreadable).
fn legacy_ppt_walk_props(
    pay: &[u8],
    pos: &mut usize,
    masks: u32,
    table: &[(u32, Option<u32>)],
    cap: &mut dyn FnMut(u32, &[u8]),
) -> bool {
    for &(mask, size) in table {
        if masks & mask == 0 {
            continue;
        }
        if *pos >= pay.len() {
            return false;
        }
        match size {
            None => {
                if *pos + 2 > pay.len() {
                    return false;
                }
                let n = legacy_ppt_u16(pay, *pos) as usize;
                *pos += 2 + n * 4;
            }
            Some(0) => {}
            Some(n) => {
                let n = n as usize;
                if *pos + n > pay.len() {
                    return false;
                }
                cap(mask, &pay[*pos..*pos + n]);
                *pos += n;
            }
        }
    }
    true
}

/// Byte-exact StyleTextPropAtom (4001) decode over the box's `text_len`
/// characters: paragraph runs, then char runs. None unless the walk lands
/// exactly on the record end — other layouts keep the plain-text path.
fn legacy_ppt_decode_style(
    pay: &[u8],
    text_len: usize,
) -> Option<(Vec<LegacyPptParaRun>, Vec<LegacyPptCharRun>)> {
    // Paragraph section: cch (clamped so the runs may describe one
    // character more than the text), then level + flags + announced props.
    let mut pos = 0usize;
    let mut paras = Vec::new();
    let mut handled = 0usize;
    let mut prsize = text_len;
    while pos + 4 <= pay.len() && handled < prsize {
        let cch = legacy_ppt_u32(pay, pos) as usize;
        pos += 4;
        let cch = cch.min((text_len + 1).saturating_sub(handled));
        handled += cch;
        if pos + 6 > pay.len() {
            return None;
        }
        let level = legacy_ppt_u16(pay, pos);
        pos += 2;
        let flags = legacy_ppt_u32(pay, pos);
        pos += 4;
        let mut run = LegacyPptParaRun {
            cch,
            level,
            ..Default::default()
        };
        let ok = legacy_ppt_walk_props(pay, &mut pos, flags, &PPT_PARA_PROPS, &mut |mask, b| {
            match mask {
                0x0000_0800 => run.align = Some(legacy_ppt_align_str(legacy_ppt_u16(b, 0))),
                0x0000_000F => run.has_bullet = Some(legacy_ppt_u16(b, 0) & 1 != 0),
                0x0000_0080 => run.bullet_char = Some(legacy_ppt_u16(b, 0)),
                // Valid only when bulletFlags.fBulletHasFont says so [MS-PPT].
                0x0000_0010 => {
                    if flags & 0x0002 != 0 {
                        run.bullet_font = Some(legacy_ppt_u16(b, 0));
                    }
                }
                0x0000_0100 => {
                    run.mar_l_emu = Some((legacy_ppt_i16(b, 0) as f64 * PPT_MU_EMU).max(0.0))
                }
                0x0000_0400 => {
                    run.indent_emu = Some((legacy_ppt_i16(b, 0) as f64 * PPT_MU_EMU).abs())
                }
                _ => {}
            }
        });
        if !ok {
            return None;
        }
        paras.push(run);
        if pos < pay.len() && handled == text_len {
            prsize = text_len + 1;
        }
    }

    // Char section: cch + flags + announced props, same clamping rule.
    let mut chars = Vec::new();
    let mut chsize = text_len;
    let mut handled = 0usize;
    while pos + 8 <= pay.len() && handled < chsize {
        let cch = legacy_ppt_u32(pay, pos) as usize;
        pos += 4;
        let cch = cch.min((text_len + 1).saturating_sub(handled));
        handled += cch;
        let flags = legacy_ppt_u32(pay, pos);
        pos += 4;
        let mut run = LegacyPptCharRun {
            cch,
            ..Default::default()
        };
        let ok = legacy_ppt_walk_props(pay, &mut pos, flags, &PPT_CHAR_PROPS, &mut |mask, b| {
            match mask {
                0x0002_0000 => run.size_pt = Some(legacy_ppt_u16(b, 0) as f64),
                0x0004_0000 => run.color_raw = Some(legacy_ppt_u32(b, 0)),
                0x0000_FFFF => run.cf = Some(legacy_ppt_u16(b, 0)),
                _ => {}
            }
        });
        if !ok {
            return None;
        }
        chars.push(run);
        if pos < pay.len() && handled == text_len {
            chsize = text_len + 1;
        }
    }

    (pos == pay.len()).then_some((paras, chars))
}

/// Character ranges of a box's paragraphs: '\r'/'\n' end one, a VT/FF soft
/// break starts a `follow` continuation (the pptx `<a:br>` shape — same
/// alignment, no bullet of its own). A trailing separator's empty tail is
/// not a paragraph; empty paragraphs between separators are.
fn legacy_ppt_paragraph_ranges(chars: &[char]) -> Vec<(usize, usize, bool)> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut follow = false;
    for (i, c) in chars.iter().enumerate() {
        match c {
            '\r' | '\n' => {
                out.push((start, i, follow));
                start = i + 1;
                follow = false;
            }
            '\u{b}' | '\u{c}' => {
                out.push((start, i, follow));
                start = i + 1;
                follow = true;
            }
            _ => {}
        }
    }
    if start < chars.len() {
        out.push((start, chars.len(), follow));
    }
    out
}

/// Append one style slice as a run, merging it into the previous run when
/// every attribute matches (a uniformly styled paragraph stays one markup
/// span). Stray control characters — the slices exclude the paragraph
/// separators — become spaces, as in `legacy_ppt_normalize_text`.
fn legacy_ppt_push_run(
    runs: &mut Vec<TextRun>,
    slice: &[char],
    size_pt: Option<f64>,
    color_raw: Option<u32>,
    cf: Option<u16>,
    fallback_pt: f64,
    scheme: &[(f64, f64, f64); 8],
) {
    if slice.is_empty() {
        return;
    }
    let cf = cf.unwrap_or(0);
    let run = TextRun {
        text: slice
            .iter()
            .map(|&c| if c.is_control() { ' ' } else { c })
            .collect(),
        sz_pt: Some(size_pt.unwrap_or(fallback_pt).clamp(1.0, 4000.0)),
        bold: cf & 1 != 0,
        italic: cf & 2 != 0,
        underline: cf & 4 != 0,
        color: color_raw.and_then(|v| legacy_ppt_text_color(v, scheme)),
        font: Some("Sans".into()),
    };
    if let Some(last) = runs.last_mut() {
        let same = last.sz_pt == run.sz_pt
            && last.bold == run.bold
            && last.italic == run.italic
            && last.underline == run.underline
            && last.color == run.color
            && last.font == run.font;
        if same {
            last.text.push_str(&run.text);
            return;
        }
    }
    runs.push(run);
}

/// Structured paragraphs for one box: its raw text (the text a `Style-
/// TextPropAtom`'s `cch` counts describe) plus that atom → the shared
/// `TextPara` model. A paragraph takes its properties from the run whose
/// range covers where it starts (runs include the separator a paragraph
/// excludes; one run may span several), with the 4003 master level for the
/// text type underneath; char runs slice it, carrying what they don't
/// restate. `sz_pt` is always filled — the renderer defaults to 18pt.
/// None when the style doesn't land byte-exact (callers keep the
/// plain-text path then).
fn legacy_ppt_box_paras(
    raw: &str,
    style: Option<&[u8]>,
    tt: Option<u32>,
    master: &std::collections::HashMap<u32, Vec<LegacyPptLevelStyle>>,
    fonts: &std::collections::HashMap<u16, String>,
    fallback_pt: f64,
    scheme: &[(f64, f64, f64); 8],
) -> Option<Vec<TextPara>> {
    let style = style?;
    let chars: Vec<char> = raw.chars().collect();
    if chars.is_empty() {
        return None;
    }
    let (para_runs, char_runs) = legacy_ppt_decode_style(style, chars.len())?;
    let ranges = legacy_ppt_paragraph_ranges(&chars);
    if ranges.is_empty() {
        return None;
    }

    let levels = tt.and_then(|t| master.get(&t));
    let base_for = |level: usize| -> LegacyPptLevelStyle {
        levels
            .and_then(|ls| ls.get(level.min(ls.len().saturating_sub(1))))
            .copied()
            .unwrap_or_default()
    };

    let mut paras: Vec<TextPara> = Vec::with_capacity(ranges.len());
    for (start, end, follow) in ranges {
        let mut acc = 0usize;
        let mut covering = None;
        for r in &para_runs {
            if start < acc + r.cch {
                covering = Some(r);
                break;
            }
            acc += r.cch;
        }
        let run = covering.or(para_runs.last());
        let lvl = run.map(|r| r.level as usize).unwrap_or(0).min(8);
        let base = base_for(lvl);

        let align = run
            .and_then(|r| r.align)
            .or(base.align)
            .unwrap_or("l")
            .to_string();
        let bullet = match run.and_then(|r| r.has_bullet).or(base.has_bullet) {
            Some(true) => {
                let code = run.and_then(|r| r.bullet_char).or(base.bullet_char);
                let font_ref = run.and_then(|r| r.bullet_font).or(base.bullet_font);
                let font = font_ref.and_then(|f| fonts.get(&f).map(String::as_str));
                Some(ParaBullet::Char(
                    legacy_ppt_bullet_char(code, font).to_string(),
                ))
            }
            Some(false) => Some(ParaBullet::Off),
            None => None,
        };
        let has_marker = matches!(bullet, Some(ParaBullet::Char(_)));
        let mar_l = run
            .and_then(|r| r.mar_l_emu)
            .or(base.mar_l_emu)
            .unwrap_or(0.0)
            .max(0.0);
        let mut hang = run
            .and_then(|r| r.indent_emu)
            .or(base.indent_emu)
            .unwrap_or(0.0);
        if hang == 0.0 && mar_l > 0.0 && has_marker {
            hang = mar_l;
        }

        // Char runs slice the paragraph in order; a run's present fields
        // update the carry (seeded from the master level), and whatever no
        // run covers keeps it.
        let mut size = base.size_pt;
        let mut color = base.color_raw;
        let mut cf = base.cf;
        let mut runs: Vec<TextRun> = Vec::new();
        let mut acc = 0usize;
        for cr in &char_runs {
            let from = acc;
            let to = acc + cr.cch;
            acc = to;
            if from >= end {
                break;
            }
            if to <= start {
                continue;
            }
            if let Some(v) = cr.size_pt {
                size = Some(v);
            }
            if let Some(v) = cr.color_raw {
                color = Some(v);
            }
            if let Some(v) = cr.cf {
                cf = Some(v);
            }
            legacy_ppt_push_run(
                &mut runs,
                &chars[from.max(start)..to.min(end)],
                size,
                color,
                cf,
                fallback_pt,
                scheme,
            );
        }
        if acc < end {
            legacy_ppt_push_run(
                &mut runs,
                &chars[acc.max(start)..end],
                size,
                color,
                cf,
                fallback_pt,
                scheme,
            );
        }
        if let Some(first) = runs.first_mut() {
            first.text = first.text.trim_start().to_string();
        }
        if let Some(last) = runs.last_mut() {
            last.text = last.text.trim_end().to_string();
        }
        runs.retain(|r| !r.text.is_empty());

        paras.push(TextPara {
            runs,
            algn: align,
            lvl,
            bullet,
            spc_bef_pt: 0.0,
            spc_aft_pt: 0.0,
            follow,
            mar_l_emu: mar_l,
            hang_emu: hang,
        });
    }

    // Trailing empty paragraphs only add dead vertical space — same rule as
    // the pptx parser.
    while paras
        .last()
        .map(|p| p.runs.iter().all(|r| r.text.trim().is_empty()))
        .unwrap_or(false)
    {
        paras.pop();
    }
    (!paras.is_empty()).then_some(paras)
}

/// Every record of type `rec_type` (optionally of instance `inst`) in
/// `bytes`: containers descended into, but a match's payload not descended
/// into (a record doesn't contain itself). Yields (instance, payload).
fn legacy_ppt_walk_rec<'a>(
    bytes: &'a [u8],
    rec_type: u16,
    inst: Option<u16>,
    out: &mut Vec<(u16, &'a [u8])>,
    depth: usize,
) {
    if depth > 12 {
        return;
    }
    let mut pos = 0usize;
    while pos + 8 <= bytes.len() {
        let Some((ver, instance, ty, payload, next)) = legacy_ppt_hdr(bytes, pos) else {
            break;
        };
        if ty == rec_type && inst.map_or(true, |want| want == instance) {
            out.push((instance, payload));
        } else if ver == 0x0f {
            legacy_ppt_walk_rec(payload, rec_type, inst, out, depth + 1);
        }
        pos = next;
    }
}

/// The document's font list: 4023 (FontEntityAtom) records whose
/// `recInstance` is the index every FontIndexRef — bullet fonts and char-run
/// fonts alike — points into. The payload is a NUL-terminated UTF-16LE name;
/// a name that doesn't end before heap leftovers is no name at all.
fn legacy_ppt_font_names(bytes: &[u8]) -> std::collections::HashMap<u16, String> {
    let mut recs = Vec::new();
    legacy_ppt_walk_rec(bytes, 4023, None, &mut recs, 0);
    let mut out = std::collections::HashMap::new();
    for (inst, pay) in recs {
        if out.contains_key(&inst) {
            continue; // doc and master both carry a copy — first wins
        }
        let units: Vec<u16> = pay
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .take(64) // a font name is short; past that it's the heap leftovers
            .take_while(|&u| u != 0)
            .collect();
        let name = String::from_utf16_lossy(&units);
        if !name.is_empty() && !name.chars().any(|c| c.is_control() || c == '\u{fffd}') {
            out.insert(inst, name);
        }
    }
    out
}

/// Every record of one container payload as (type, payload) pairs, in
/// document order with containers descended always — a block's
/// refID/header/text/style records surface flat. Yields (type, payload).
fn legacy_ppt_flat_records<'a>(bytes: &'a [u8], out: &mut Vec<(u16, &'a [u8])>, depth: usize) {
    if depth > 8 {
        return;
    }
    let mut pos = 0usize;
    while pos + 8 <= bytes.len() {
        let Some((ver, _, ty, payload, next)) = legacy_ppt_hdr(bytes, pos) else {
            break;
        };
        out.push((ty, payload));
        if ver == 0x0f {
            legacy_ppt_flat_records(payload, out, depth + 1);
        }
        pos = next;
    }
}

/// One SlideListWithText (4080) block split into its (refID, entries)
/// pairs: a 1011 starts a slide, a 3999 starts an entry, the text atoms
/// fill it in, and the first 4001 wins as its style.
fn legacy_ppt_block_entries<'a>(block: &'a [u8]) -> Vec<(u32, Vec<LegacyPptTextEntry<'a>>)> {
    let mut recs = Vec::new();
    legacy_ppt_flat_records(block, &mut recs, 0);
    let mut out: Vec<(u32, Vec<LegacyPptTextEntry<'a>>)> = Vec::new();
    for (ty, payload) in recs {
        match ty {
            1011 if payload.len() >= 4 => out.push((legacy_ppt_u32(payload, 0), Vec::new())),
            3999 if payload.len() >= 4 => {
                if let Some((_, entries)) = out.last_mut() {
                    entries.push(LegacyPptTextEntry {
                        tt: legacy_ppt_u32(payload, 0),
                        text: String::new(),
                        style: None,
                    });
                }
            }
            4000 | 4026 => {
                if let Some((_, entries)) = out.last_mut() {
                    if let Some(entry) = entries.last_mut() {
                        entry.text.push_str(&decode_utf16le_lossy(payload));
                    }
                }
            }
            4008 => {
                if let Some((_, entries)) = out.last_mut() {
                    if let Some(entry) = entries.last_mut() {
                        entry.text.push_str(&decode_ppt_8bit_text(payload));
                    }
                }
            }
            4001 => {
                if let Some((_, entries)) = out.last_mut() {
                    if let Some(entry) = entries.last_mut() {
                        if entry.style.is_none() {
                            entry.style = Some(payload);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// The deck's slides in show order, with their outline-text entries: the
/// CurrentUser chain (newest UserEditAtom first) yields the live persist
/// tables and document ref; the idmap they build resolves each block's
/// refIDs onto slide records. None when the chain can't be read or any ref
/// doesn't land on a 1006 — callers then keep stream order (stale slides
/// and no entries are still better than nothing).
fn legacy_ppt_persist_slides<'a>(
    bytes: &'a [u8],
    current_user: Option<&[u8]>,
) -> Option<Vec<LegacyPptSlideRef<'a>>> {
    let cu = current_user?;
    if cu.len() < 20 {
        return None;
    }

    // CurrentUser @16 → the newest UserEditAtom (4085): previous edit,
    // persist-table offset, document ref — walked back to the oldest.
    let mut chain: Vec<(u32, u32)> = Vec::new();
    let mut off = legacy_ppt_u32(cu, 16) as usize;
    let mut seen = std::collections::HashSet::new();
    while off > 0 && off < bytes.len() && seen.insert(off) && chain.len() < 64 {
        let Some((_, _, rec_type, pay, _)) = legacy_ppt_hdr(bytes, off) else {
            break;
        };
        if rec_type != 4085 || pay.len() < 20 {
            break;
        }
        let prev = legacy_ppt_u32(pay, 8);
        let tab = legacy_ppt_u32(pay, 12);
        let docref = legacy_ppt_u32(pay, 16);
        chain.push((tab, docref));
        off = prev as usize;
    }
    if chain.is_empty() {
        return None;
    }

    // 6002 tables, oldest first so a newer edit wins: key = count<<20 |
    // start, then `count` absolute stream offsets.
    let mut idmap = std::collections::HashMap::new();
    for (tab, _) in chain.iter().rev() {
        let Some((_, _, rec_type, pay, _)) = legacy_ppt_hdr(bytes, *tab as usize) else {
            continue;
        };
        if rec_type != 6002 {
            continue;
        }
        let mut p = 0usize;
        while p + 4 <= pay.len() {
            let key = legacy_ppt_u32(pay, p);
            p += 4;
            let count = (key >> 20) as usize;
            let start = key & 0x000f_ffff;
            for i in 0..count {
                if p + 4 > pay.len() {
                    break;
                }
                let slot = start + i as u32;
                let at = legacy_ppt_u32(pay, p);
                p += 4;
                idmap.insert(slot, at);
            }
        }
    }

    // The document's SlideListWithText blocks — whole-stream fallback when
    // the document ref went stale (the idmap can still resolve the refs).
    let mut blocks: Vec<(u16, &[u8])> = Vec::new();
    if let Some(&doc_off) = idmap.get(&chain[0].1) {
        if let Some((_, _, _, pay, _)) = legacy_ppt_hdr(bytes, doc_off as usize) {
            legacy_ppt_walk_rec(pay, 4080, Some(0), &mut blocks, 0);
        }
    }
    if blocks.is_empty() {
        legacy_ppt_walk_rec(bytes, 4080, Some(0), &mut blocks, 0);
    }
    if blocks.is_empty() {
        return None;
    }

    let mut slides: Vec<LegacyPptSlideRef<'a>> = Vec::new();
    let mut seen_refs = std::collections::HashSet::new();
    for (_, block) in blocks {
        for (ref_id, mut entries) in legacy_ppt_block_entries(block) {
            if !seen_refs.insert(ref_id) {
                continue;
            }
            // Entries without renderable text aren't placeholders to place.
            entries.retain(|e| !legacy_ppt_normalize_text(&e.text).is_empty());
            let Some(&at) = idmap.get(&ref_id) else {
                return None;
            };
            let Some((_, _, rec_type, payload, _)) = legacy_ppt_hdr(bytes, at as usize) else {
                return None;
            };
            if rec_type != 1006 {
                return None;
            }
            slides.push(LegacyPptSlideRef {
                payload,
                entries,
            });
        }
    }
    if slides.is_empty() {
        return None;
    }
    Some(slides)
}

/// Slide `slide` (1-based) for the parsers: persist order (plus its
/// outline-text entries) when the chain reads, stream order with no
/// entries otherwise. None when neither list reaches that far.
fn legacy_ppt_slide_at<'a>(
    bytes: &'a [u8],
    current_user: Option<&[u8]>,
    slide: usize,
) -> Option<LegacyPptSlideRef<'a>> {
    let idx = slide.saturating_sub(1);
    if let Some(slides) = legacy_ppt_persist_slides(bytes, current_user) {
        return slides.into_iter().nth(idx);
    }
    let mut payloads = Vec::new();
    collect_ppt_slide_payloads(bytes, &mut payloads, 0);
    payloads.get(idx).map(|payload| LegacyPptSlideRef {
        payload,
        entries: Vec::new(),
    })
}

/// The deck's 4003 TextMasterStyleAtom records keyed by textType (their
/// record instance): the per-level base under every box's own runs. First
/// successful decode per textType wins — decks restate some copies.
fn legacy_ppt_master_styles(
    bytes: &[u8],
) -> std::collections::HashMap<u32, Vec<LegacyPptLevelStyle>> {
    let mut found = Vec::new();
    legacy_ppt_walk_rec(bytes, 4003, None, &mut found, 0);
    let mut out = std::collections::HashMap::new();
    for (instance, payload) in found {
        if out.contains_key(&(instance as u32)) {
            continue;
        }
        if let Some(levels) = legacy_ppt_decode_master_style(instance, payload) {
            out.insert(instance as u32, levels);
        }
    }
    out
}

/// One TextMasterStyleAtom: its levels carry a leading `level` word iff
/// the record instance (the textType) is >= 5 [MS-PPT]; the other reading
/// is only tried when the spec reading fails to parse.
fn legacy_ppt_decode_master_style(instance: u16, payload: &[u8]) -> Option<Vec<LegacyPptLevelStyle>> {
    legacy_ppt_decode_master_levels(payload, instance >= 5)
        .or_else(|| legacy_ppt_decode_master_levels(payload, instance < 5))
}

/// `cLevels` TextMasterStyleLevels back to back — optional `level` word,
/// TextPFException, TextCFException each. None when a level runs off the
/// record (the caller then tries the other reading).
fn legacy_ppt_decode_master_levels(
    pay: &[u8],
    has_level: bool,
) -> Option<Vec<LegacyPptLevelStyle>> {
    if pay.len() < 2 {
        return None;
    }
    let count = legacy_ppt_u16(pay, 0) as usize;
    if count == 0 || count > 6 {
        return None;
    }
    let mut pos = 2usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        if has_level {
            pos += 2;
        }
        if pos + 4 > pay.len() {
            return None;
        }
        let pflags = legacy_ppt_u32(pay, pos);
        pos += 4;
        let mut level = LegacyPptLevelStyle::default();
        let ok = legacy_ppt_walk_props(pay, &mut pos, pflags, &PPT_PARA_PROPS, &mut |mask, b| {
            match mask {
                0x0000_0800 => level.align = Some(legacy_ppt_align_str(legacy_ppt_u16(b, 0))),
                0x0000_000F => level.has_bullet = Some(legacy_ppt_u16(b, 0) & 1 != 0),
                0x0000_0080 => level.bullet_char = Some(legacy_ppt_u16(b, 0)),
                // Valid only when bulletFlags.fBulletHasFont says so [MS-PPT].
                0x0000_0010 => {
                    if pflags & 0x0002 != 0 {
                        level.bullet_font = Some(legacy_ppt_u16(b, 0));
                    }
                }
                0x0000_0100 => {
                    level.mar_l_emu = Some((legacy_ppt_i16(b, 0) as f64 * PPT_MU_EMU).max(0.0))
                }
                0x0000_0400 => {
                    level.indent_emu = Some((legacy_ppt_i16(b, 0) as f64 * PPT_MU_EMU).abs())
                }
                _ => {}
            }
        });
        if !ok {
            return None;
        }
        if pos + 4 > pay.len() {
            return None;
        }
        let cflags = legacy_ppt_u32(pay, pos);
        pos += 4;
        let ok = legacy_ppt_walk_props(pay, &mut pos, cflags, &PPT_CHAR_PROPS, &mut |mask, b| {
            match mask {
                0x0002_0000 => level.size_pt = Some(legacy_ppt_u16(b, 0) as f64),
                0x0004_0000 => level.color_raw = Some(legacy_ppt_u32(b, 0)),
                0x0000_FFFF => level.cf = Some(legacy_ppt_u16(b, 0)),
                _ => {}
            }
        });
        if !ok {
            return None;
        }
        out.push(level);
    }
    // The levels must account for the whole record — a leftover means the
    // wrong `level` reading was used (real decks parse exact either way).
    (pos == pay.len()).then_some(out)
}

/// The entry an empty text box places: one with the box's own textType
/// first (its F00D names the placeholder), else the first still-unplaced
/// entry — entries follow placeholder order. None when nothing is left.
fn legacy_ppt_take_entry<'a>(
    ctx: &mut LegacyPptTextCtx<'a>,
    tt: Option<u32>,
) -> Option<LegacyPptTextEntry<'a>> {
    if ctx.pending.is_empty() {
        return None;
    }
    let idx = tt
        .and_then(|want| ctx.pending.iter().position(|e| e.tt == want))
        .unwrap_or(0);
    Some(ctx.pending.remove(idx))
}

/// Decode one shape container (F004) payload: its fill/outline become a
/// `DrawShape` drawn behind its client text (a `SlideBox`); a picture
/// property (pib / shape type 75) resolves through `media` into a
/// `DrawPicture`. Groups (type 0) render through their child containers,
/// which the collector surfaces on their own. `ctx` supplies the slide's
/// outline-text entries (a box with no text of its own places one) and the
/// master styles its runs fall back to.
fn legacy_ppt_push_shape(
    sp_container: &[u8],
    scheme: &[(f64, f64, f64); 8],
    media: Option<&LegacyPptMedia>,
    ctx: &mut LegacyPptTextCtx<'_>,
    out: &mut Vec<SlideElement>,
) {
    let mut shape_type = 0u16;
    let mut flags = 0u16;
    let mut have_sp = false;
    let mut props: Vec<(u16, u32)> = Vec::new();
    let mut rect = None;
    let mut textbox: Option<&[u8]> = None;

    let mut pos = 0usize;
    while pos + 8 <= sp_container.len() {
        let Some((_, inst, rec_type, payload, next)) = legacy_ppt_hdr(sp_container, pos) else {
            break;
        };
        match rec_type {
            0xf00a if payload.len() >= 6 => {
                shape_type = inst; // F00A's instance field is the shape type
                flags = u16::from_le_bytes([payload[4], payload[5]]);
                have_sp = true;
            }
            0xf00b => props = legacy_ppt_props(payload),
            0xf00f | 0xf010 => {
                if rect.is_none() {
                    rect = legacy_ppt_anchor_rect(payload);
                }
            }
            0xf00d => {
                if textbox.is_none() {
                    textbox = Some(payload);
                }
            }
            _ => {}
        }
        pos = next;
    }

    if !have_sp || shape_type == 0 {
        return;
    }
    let Some((mx1, my1, mx2, my2)) = rect else {
        return;
    };
    let mut x = mx1 * PPT_MU_EMU;
    let mut y = my1 * PPT_MU_EMU;
    let mut w = (mx2 - mx1) * PPT_MU_EMU;
    let mut h = (my2 - my1) * PPT_MU_EMU;

    // Rules (type 20) and connectors (CONNECTOR flag) stroke between the
    // rect corners; their zero-width/height slivers must stay drawable.
    let is_line = shape_type == 20 || flags & 0x0100 != 0;
    if is_line {
        if w <= 0.0 {
            w = 1.0;
        }
        if h <= 0.0 {
            h = 1.0;
        }
    } else if w <= 0.0 || h <= 0.0 {
        return;
    }

    let opt = |op: u16, default: f64| {
        props
            .iter()
            .find(|(o, _)| *o == op)
            .map(|(_, v)| *v as f64)
            .unwrap_or(default)
    };
    let line = props
        .iter()
        .find(|(op, _)| *op == 0x01c0)
        .map(|(_, v)| LineSpec {
            color: legacy_ppt_rgb(*v, scheme),
            alpha: 1.0,
            width_emu: opt(0x01cb, 9_525.0),
        });
    let rot = (opt(0x0004, 0.0) / 65_536.0).to_radians();
    let flip_h = flags & 0x0040 != 0;
    let flip_v = flags & 0x0080 != 0;

    // Picture: pib (0x104) names a BlipStore entry (1-based) → cached
    // image file. Drawn instead of a fill, outline kept as frame; no
    // resolvable image (WMF/EMF, missing stream) leaves the shape skipped.
    if let Some(pib) = props
        .iter()
        .find_map(|(op, v)| (*op == 0x0104).then_some(*v))
    {
        if !is_line {
            if let Some(path) = media.and_then(|m| m.path_for(pib)) {
                out.push(SlideElement::Picture(DrawPicture {
                    x,
                    y,
                    w,
                    h,
                    path,
                    crop: [0.0; 4],
                    line,
                    rot,
                    flip_h,
                    flip_v,
                }));
            }
        }
        return;
    }
    if shape_type == 75 {
        return; // picture record without a resolvable pib
    }

    let prst = match shape_type {
        3 => "ellipse",
        2 => "roundRect",
        _ if is_line => "line",
        _ => "rect",
    };
    let fill = if is_line {
        None
    } else {
        props
            .iter()
            .find(|(op, _)| *op == 0x0181)
            .map(|(_, v)| FillKind::Solid(legacy_ppt_rgb(*v, scheme), 1.0))
    };

    if fill.is_some() || line.is_some() {
        out.push(SlideElement::Shape(DrawShape {
            x,
            y,
            w,
            h,
            prst: prst.into(),
            fill,
            line,
            rot,
            flip_h,
            flip_v,
            freeform: None,
        }));
    }

    // Text: the box's own atoms; a box with none of its own places the next
    // outline-text entry for this slide (PPT 97 keeps placeholder text in
    // the document's SlideListWithText, not on the slide itself). The style
    // stays paired with the text it counts — own 4001 for own text, the
    // entry's for an entry's.
    let Some(tb) = textbox else {
        return;
    };
    let mut own_raw = String::new();
    legacy_ppt_box_text(tb, &mut own_raw, 0);
    let own_style = legacy_ppt_find(tb, 4001, 0);
    let own_tt = legacy_ppt_find(tb, 3999, 0)
        .filter(|p| p.len() >= 4)
        .map(|p| legacy_ppt_u32(p, 0));
    let (raw, style, tt) = if legacy_ppt_normalize_text(&own_raw).is_empty() {
        let Some(entry) = legacy_ppt_take_entry(ctx, own_tt) else {
            return;
        };
        (entry.text, entry.style.or(own_style), Some(entry.tt))
    } else {
        (own_raw, own_style, own_tt)
    };
    let text = legacy_ppt_normalize_text(&raw);
    if text.is_empty() {
        return;
    }
    let font_pt = legacy_ppt_estimate_pt(w, h, &text);
    let paras = legacy_ppt_box_paras(&raw, style, tt, &ctx.master, &ctx.fonts, font_pt, scheme);
    out.push(SlideElement::Text(SlideBox {
        x,
        y,
        w,
        h,
        text,
        is_title: false,
        centered: false,
        font_pt: Some(font_pt),
        has_xfrm: true,
        ph_type: "body".into(),
        color: None,
        paras,
        anchor: opt(0x0087, 0.0).clamp(0.0, 2.0) as u8,
        insets: [
            opt(0x0081, 91_440.0),
            opt(0x0082, 45_720.0),
            opt(0x0083, 91_440.0),
            opt(0x0084, 45_720.0),
        ],
        autofit_scale: 1.0,
    }));
}

/// Title pass + scheme colors: the shortest text box in the top half of the
/// slide wins (titles are short, bodies are not — textType codes are
/// overwhelmingly OTHER in binary decks, so position/length is what
/// survives), with the topmost box as fallback.
fn legacy_ppt_finish_boxes(
    elements: &mut [SlideElement],
    scheme: &[(f64, f64, f64); 8],
    slide_h: f64,
) {
    let half = slide_h * 0.5;
    let candidates: Vec<(usize, f64, usize)> = elements
        .iter()
        .enumerate()
        .filter_map(|(i, e)| match e {
            SlideElement::Text(b) if !b.text.trim().is_empty() => {
                Some((i, b.y, b.text.trim().chars().count()))
            }
            _ => None,
        })
        .collect();
    let title = candidates
        .iter()
        .filter(|(_, y, _)| *y < half)
        .min_by_key(|(_, y, len)| (*len, *y as i64))
        .or_else(|| candidates.iter().min_by_key(|(_, y, _)| *y as i64))
        .map(|(i, _, _)| *i);
    for (i, e) in elements.iter_mut().enumerate() {
        let SlideElement::Text(b) = e else { continue };
        if Some(i) == title {
            b.is_title = true;
            b.ph_type = "title".into();
            b.color = Some(scheme[3]);
        } else {
            b.color = Some(scheme[1]);
        }
    }
}

/// Deck-scoped picture material for a legacy .ppt: the BlipStore's image
/// ids (uids, in entry order) plus the raw `Pictures` stream, so a shape's
/// pib property (1-based) resolves to a file in the shared media cache.
/// Decks without either stream get an empty map — those pictures are then
/// skipped, exactly as before pictures were supported.
struct LegacyPptMedia {
    uids: Vec<[u8; 16]>,
    pictures: Vec<u8>,
    locs: std::collections::HashMap<[u8; 16], (usize, usize)>,
    dir: PathBuf,
}

impl LegacyPptMedia {
    fn new(doc: &Path, stream: &[u8]) -> Self {
        // BlipStoreContainer (F001) → BlipStoreEntry (F007) uid list.
        let mut uids = Vec::new();
        if let Some(bse) = legacy_ppt_find(stream, 0xf001, 0) {
            let mut pos = 0usize;
            while pos + 8 <= bse.len() {
                let Some((_, _, rec_type, payload, next)) = legacy_ppt_hdr(bse, pos) else {
                    break;
                };
                if rec_type == 0xf007 && payload.len() >= 18 {
                    let mut uid = [0u8; 16];
                    uid.copy_from_slice(&payload[2..18]);
                    uids.push(uid);
                }
                pos = next;
            }
        }
        // Pictures stream: concatenated [header | uid(16) | tag | image].
        let pictures = legacy_ppt_pictures_stream(doc).unwrap_or_default();
        let mut locs = std::collections::HashMap::new();
        let mut pos = 0usize;
        while pos + 8 <= pictures.len() {
            let Some((_, _, _, payload, next)) = legacy_ppt_hdr(&pictures, pos) else {
                break;
            };
            if payload.len() >= 16 {
                let mut uid = [0u8; 16];
                uid.copy_from_slice(&payload[..16]);
                locs.entry(uid).or_insert((pos + 8, payload.len()));
            }
            pos = next;
        }
        Self {
            uids,
            pictures,
            locs,
            dir: media_cache_dir(doc),
        }
    }

    /// File in the media cache holding pib's image (1-based BlipStore
    /// index); None when the entry or stream is missing, or the payload
    /// isn't a raster format we decode (WMF/EMF … are skipped).
    fn path_for(&self, pib: u32) -> Option<PathBuf> {
        if self.pictures.is_empty() {
            return None;
        }
        let uid = *self.uids.get(pib.checked_sub(1)? as usize)?;
        let (start, len) = *self.locs.get(&uid)?;
        let payload = self.pictures.get(start..start + len)?;
        // uid (16 bytes) + 1–2 tag bytes, then the raw image — scan a small
        // window for a known magic so the tag variant doesn't matter.
        let data = payload.get(16..)?;
        const MAGICS: [&[u8]; 6] = [
            b"\xff\xd8\xff", // JPEG
            b"\x89PNG",      // PNG
            b"GIF8",         // GIF
            b"II*\0",        // TIFF LE
            b"MM\0*",        // TIFF BE
            b"BM",           // BMP
        ];
        let off = (0..data.len().min(48))
            .find(|&i| MAGICS.iter().any(|m| data[i..].starts_with(m)))?;
        let bytes = &data[off..];
        if bytes.is_empty() {
            return None;
        }
        let out = self.dir.join(format!("{}.bin", crate::md5::hex(&uid)));
        if !out.exists() {
            crate::security::private_dir(&self.dir).ok()?;
            crate::security::write_private(&out, bytes).ok()?;
        }
        Some(out)
    }
}

/// The master record (top-level 1016, stream order) a slide follows: the
/// SlideAtom's masterID (u32@12) is matched against the join ids echoed by
/// the document's SlideListWithText (4080, instance 1) blocks, which list
/// the masters in stream order (byte-verified on multi-master decks).
/// Decks with a single master resolve through it directly; masterless or
/// ambiguous decks yield None.
fn legacy_ppt_master<'a>(bytes: &'a [u8], slide_payload: &[u8]) -> Option<&'a [u8]> {
    let mut masters: Vec<&'a [u8]> = Vec::new();
    let mut joins: Vec<u32> = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= bytes.len() {
        let Some((_, _, rec_type, payload, next)) = legacy_ppt_hdr(bytes, pos) else {
            break;
        };
        match rec_type {
            1016 => masters.push(payload),
            1000 if joins.is_empty() => {
                // First document container only: later copies restate the
                // same list (or a stale one) after edits.
                let mut c = 0usize;
                while c + 8 <= payload.len() {
                    let Some((_, cinst, cty, cpay, cnext)) = legacy_ppt_hdr(payload, c) else {
                        break;
                    };
                    if cty == 4080 && cinst == 1 {
                        let mut b = 0usize;
                        while b + 8 <= cpay.len() {
                            let Some((_, _, bty, bpay, bnext)) = legacy_ppt_hdr(cpay, b) else {
                                break;
                            };
                            if bty == 1011 && bpay.len() >= 16 {
                                joins.push(u32::from_le_bytes([
                                    bpay[12], bpay[13], bpay[14], bpay[15],
                                ]));
                            }
                            b = bnext;
                        }
                    }
                    c = cnext;
                }
            }
            _ => {}
        }
        pos = next;
    }
    if masters.is_empty() {
        return None;
    }
    let atom = legacy_ppt_find(slide_payload, 1007, 0)?;
    if atom.len() >= 16 {
        let id = u32::from_le_bytes([atom[12], atom[13], atom[14], atom[15]]);
        if let Some(idx) = joins.iter().position(|&j| j == id) {
            if let Some(m) = masters.get(idx) {
                return Some(m);
            }
        }
    }
    (masters.len() == 1).then_some(masters[0])
}

/// The master's full-bleed picture (≥90% of the slide on both axes) as the
/// background image; smaller pictures (logos) are decorative and out of
/// scope. None when the master has no Drawing or no such picture.
fn legacy_ppt_master_bg(
    master: &[u8],
    media: &LegacyPptMedia,
    slide_w: f64,
    slide_h: f64,
) -> Option<PathBuf> {
    let drawing = legacy_ppt_find(master, 1036, 0)?;
    let mut shapes = Vec::new();
    legacy_ppt_collect_shapes(drawing, &mut shapes, 0);
    for sp in shapes {
        let Some((pib, (_, _, w, h))) = legacy_ppt_shape_pic(sp) else {
            continue;
        };
        if w >= slide_w * 0.9 && h >= slide_h * 0.9 {
            return media.path_for(pib);
        }
    }
    None
}

/// (blip index, anchor rect in EMU) of one shape container — what the
/// master-background probe needs, without building render elements.
fn legacy_ppt_shape_pic(sp: &[u8]) -> Option<(u32, (f64, f64, f64, f64))> {
    let mut pib = None;
    let mut rect = None;
    let mut pos = 0usize;
    while pos + 8 <= sp.len() {
        let Some((_, _, rec_type, payload, next)) = legacy_ppt_hdr(sp, pos) else {
            break;
        };
        match rec_type {
            0xf00b => {
                if let Some(v) = legacy_ppt_props(payload)
                    .iter()
                    .find_map(|(op, v)| (*op == 0x0104).then_some(*v))
                {
                    pib = Some(v);
                }
            }
            0xf00f | 0xf010 => {
                if rect.is_none() {
                    rect = legacy_ppt_anchor_rect(payload);
                }
            }
            _ => {}
        }
        pos = next;
    }
    let (pib, (x1, y1, x2, y2)) = (pib?, rect?);
    Some((
        pib,
        (
            x1 * PPT_MU_EMU,
            y1 * PPT_MU_EMU,
            (x2 - x1) * PPT_MU_EMU,
            (y2 - y1) * PPT_MU_EMU,
        ),
    ))
}

/// Structured parse of slide `slide` (1-based) into the shared pptx render
/// model. None when the stream, the slide or its Drawing can't be read, or
/// the Drawing yields no positioned shapes/text — callers then fall back to
/// the legacy text/blue-wave renderer.
fn legacy_ppt_structured_layout(doc: &Path, slide: usize) -> Option<SlideLayout> {
    let bytes = legacy_ppt_stream(doc)?;
    let current_user = legacy_ppt_current_user(doc);
    let LegacyPptSlideRef { payload, entries } =
        legacy_ppt_slide_at(&bytes, current_user.as_deref(), slide)?;
    let (slide_w, slide_h) = legacy_ppt_slide_size(&bytes);

    let media = LegacyPptMedia::new(doc, &bytes);
    let master = legacy_ppt_master(&bytes, payload);
    // Own ColorSchemeAtom first; a slide without one follows its master's
    // scheme (first 2032), then the classic Office defaults.
    let scheme = if legacy_ppt_find(payload, 2032, 0).is_some() {
        legacy_ppt_scheme(payload)
    } else {
        legacy_ppt_scheme(master.unwrap_or(payload))
    };

    let mut ctx = LegacyPptTextCtx {
        pending: entries,
        master: legacy_ppt_master_styles(&bytes),
        fonts: legacy_ppt_font_names(&bytes),
    };
    let drawing = legacy_ppt_find(payload, 1036, 0)?;
    let mut shapes = Vec::new();
    legacy_ppt_collect_shapes(drawing, &mut shapes, 0);
    let mut elements = Vec::new();
    for sp in &shapes {
        legacy_ppt_push_shape(sp, &scheme, Some(&media), &mut ctx, &mut elements);
    }
    if elements.is_empty() {
        return None;
    }
    legacy_ppt_finish_boxes(&mut elements, &scheme, slide_h);

    // A full-bleed picture in the master is the background; otherwise the
    // scheme's background color (white by default).
    let background = master
        .and_then(|m| legacy_ppt_master_bg(m, &media, slide_w, slide_h))
        .map(SlideBackground::Image)
        .unwrap_or_else(|| SlideBackground::Solid(scheme[0].0, scheme[0].1, scheme[0].2));
    Some(SlideLayout {
        slide_w,
        slide_h,
        background: Some(background),
        elements,
    })
}

fn parse_legacy_ppt_layout(doc: &Path, slide: usize) -> Option<SlideLayout> {
    // Structured escher parse first; the text heuristic below stays as the
    // fallback for streams it can't read.
    if let Some(layout) = legacy_ppt_structured_layout(doc, slide) {
        return Some(layout);
    }
    let bytes = legacy_ppt_stream(doc)?;
    let current_user = legacy_ppt_current_user(doc);
    // The slide's own text atoms, then its outline-text entries (the same
    // pairing the structured parse does), and finally the whole-stream blob
    // for decks where neither surface is legible on its own.
    let mut lines: Vec<String> = Vec::new();
    if let Some(slide_ref) = legacy_ppt_slide_at(&bytes, current_user.as_deref(), slide) {
        collect_ppt_text_atoms(slide_ref.payload, &mut lines, 0);
        if lines.is_empty() {
            for entry in &slide_ref.entries {
                let norm = legacy_ppt_normalize_text(&entry.text);
                if !norm.is_empty() {
                    lines.extend(norm.split('\n').map(String::from));
                }
            }
        }
    }
    if lines.is_empty() {
        lines = legacy_ppt_slide_texts(&bytes)
            .get(slide.saturating_sub(1))
            .cloned()
            .unwrap_or_default();
    }
    if lines.is_empty() {
        return None;
    }

    // PowerPoint's default 4:3 slide size in EMUs.
    let slide_w = 9_144_000.0;
    let slide_h = 6_858_000.0;
    let mx = slide_w * 0.07;
    let title = lines[0].clone();
    let body = lines.iter().skip(1).take(8).cloned().collect::<Vec<_>>();

    let mut boxes = vec![SlideBox {
        x: mx,
        y: slide_h * 0.07,
        w: slide_w - mx * 2.0,
        h: slide_h * 0.22,
        text: title,
        is_title: true,
        centered: true,
        font_pt: Some(32.0),
        has_xfrm: true,
        ph_type: "title".into(),
        color: None,
        paras: None,
        anchor: 0,
        insets: [91440.0, 45720.0, 91440.0, 45720.0],
        autofit_scale: 1.0,
    }];

    if !body.is_empty() {
        boxes.push(SlideBox {
            x: mx * 1.25,
            y: slide_h * 0.34,
            w: slide_w - mx * 2.5,
            h: slide_h * 0.54,
            text: body.join("\n"),
            is_title: false,
            centered: false,
            font_pt: Some(18.0),
            has_xfrm: true,
            ph_type: "body".into(),
            color: None,
            paras: None,
            anchor: 0,
            insets: [91440.0, 45720.0, 91440.0, 45720.0],
            autofit_scale: 1.0,
        });
    }

    Some(SlideLayout {
        slide_w,
        slide_h,
        background: None,
        elements: boxes.into_iter().map(SlideElement::Text).collect(),
    })
}

/// Render slide `slide` (1-based) of a legacy binary .ppt — text/bullets
/// fidelity only (the binary layout is approximated from text atoms).
fn render_legacy_ppt_slide(doc: &Path, slide: usize) -> Option<Vec<u8>> {
    use gtk::cairo;

    // Structured escher render first (shared pptx pipeline); the blue-wave
    // text card below stays as the fallback for streams it can't read.
    if let Some(layout) = legacy_ppt_structured_layout(doc, slide) {
        if let Some(png) = render_slide_layout(&layout) {
            return Some(png);
        }
    }
    let layout =
        parse_legacy_ppt_layout(doc, slide).unwrap_or_else(|| fallback_ppt_layout(doc));
    const W: i32 = 1280;
    const H: i32 = 960;
    let (surface, cr) = new_surface(W, H)?;
    let wf = W as f64;
    let hf = H as f64;

    cr.set_source_rgb(1.0, 1.0, 1.0);
    cr.paint().ok()?;

    // Approximate the common PowerPoint blue-wave theme used by this legacy deck.
    cr.set_source_rgb(0.02, 0.66, 0.78);
    cr.rectangle(0.0, 0.0, wf, 70.0);
    cr.fill().ok()?;
    cr.set_source_rgb(0.48, 0.82, 0.92);
    cr.rectangle(0.0, 0.0, wf * 0.55, 70.0);
    cr.fill().ok()?;
    cr.set_source_rgb(1.0, 1.0, 1.0);
    cr.move_to(0.0, 56.0);
    cr.curve_to(115.0, 22.0, 210.0, 40.0, 318.0, 44.0);
    cr.curve_to(430.0, 48.0, 504.0, 30.0, wf, 14.0);
    cr.line_to(wf, 78.0);
    cr.curve_to(406.0, 95.0, 310.0, 82.0, 200.0, 76.0);
    cr.curve_to(94.0, 70.0, 44.0, 74.0, 0.0, 98.0);
    cr.close_path();
    cr.fill().ok()?;
    cr.set_source_rgba(0.0, 0.62, 0.76, 0.75);
    cr.set_line_width(1.5);
    cr.move_to(0.0, 94.0);
    cr.curve_to(112.0, 58.0, 214.0, 70.0, 320.0, 76.0);
    cr.curve_to(430.0, 82.0, 494.0, 70.0, wf, 58.0);
    cr.stroke().ok()?;

    cr.set_source_rgb(0.72, 0.72, 0.72);
    cr.rectangle(0.5, 0.5, wf - 1.0, hf - 1.0);
    cr.stroke().ok()?;

    let (title, bullets) = legacy_ppt_parts(&layout, doc);

    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
    cr.set_font_size(31.0);
    cr.set_source_rgb(0.0, 0.39, 0.48);
    cr.move_to(52.0, 142.0);
    let _ = cr.show_text(&truncate_to_width(&cr, &title, wf - 100.0));

    cr.select_font_face("Serif", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
    cr.set_font_size(19.0);
    let mut y = 182.0;
    for bullet in bullets.iter().take(7) {
        if y > hf - 36.0 {
            break;
        }
        y = draw_legacy_ppt_bullet(&cr, bullet, 63.0, y, wf - 102.0)?;
    }

    surface_to_png(surface, cr)
}

/// Versioned per-slide render-cache path for a legacy .ppt: the shared
/// `render_cache_path` key with a slide suffix, so each slide caches alone.
/// The `.png` extension is load-bearing — `image::open` guesses the image
/// format from the path only, so an extension-less cache file never decodes
/// and the preview falls back to the generic info card.
fn legacy_ppt_cache_path(path: &Path, slide: usize) -> PathBuf {
    let base = render_cache_path(path); // .../<md5(uri|mtime|v)>.png
    let stem = base
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    base.with_file_name(format!("{}-s{}.png", stem, slide)) // .../<md5>-s<n>.png
}

/// Cached slide PNG of a legacy .ppt, sane-checked — a truncated or bogus
/// file is dropped so the next caller re-renders (self-healing cache).
fn cached_legacy_ppt_slide(doc: &Path, slide: usize) -> Option<PathBuf> {
    let p = legacy_ppt_cache_path(doc, slide);
    if sane_png_file(&p) {
        return Some(p);
    }
    if p.exists() {
        log::info!("ppt: dropping corrupt cached slide {}", p.display());
        let _ = std::fs::remove_file(&p);
    }
    None
}

/// Render slide `slide` of a legacy .ppt into the versioned render cache
/// and return the PNG path (None when the render or write fails).
fn render_legacy_ppt_slide_to_cache(doc: &Path, slide: usize) -> Option<PathBuf> {
    let png = render_legacy_ppt_slide(doc, slide)?;
    let out = legacy_ppt_cache_path(doc, slide);
    if crate::security::write_private(&out, &png).is_ok() {
        log::info!(
            "ppt: rendered slide {} of {} -> {}",
            slide,
            doc.display(),
            out.display()
        );
        Some(out)
    } else {
        None
    }
}

fn legacy_ppt_parts(layout: &SlideLayout, doc: &Path) -> (String, Vec<String>) {
    let mut title = layout
        .elements
        .iter()
        .find_map(|e| match e { SlideElement::Text(b) if b.is_title => Some(b.text.trim().to_string()), _ => None })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            doc.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("Presentation")
                .replace(['_', '-'], " ")
        });
    if title.len() > 60 {
        title = title.chars().take(60).collect();
    }

    let mut bullets = Vec::new();
    for elem in &layout.elements {
        let SlideElement::Text(b) = elem else { continue };
        if b.is_title {
            continue;
        }
        for line in b.text.lines() {
            let line = clean_legacy_ppt_line(line);
            if line.is_empty() || line.eq_ignore_ascii_case(&title) {
                continue;
            }
            if !bullets
                .iter()
                .any(|x: &String| x.eq_ignore_ascii_case(&line))
            {
                bullets.push(line);
            }
        }
    }

    if bullets.is_empty() {
        bullets.push("No body text on this slide.".into());
    }

    (title, bullets)
}

fn clean_legacy_ppt_line(line: &str) -> String {
    let cleaned = line
        .trim()
        .trim_matches(|c: char| c == '-' || c == '*' || c == '\u{2022}' || c.is_whitespace())
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if cleaned.len() < 3 {
        return String::new();
    }
    let lower = cleaned.to_lowercase();
    if lower.starts_with("ppt") || lower == "slide" || lower == "click to edit" {
        return String::new();
    }
    cleaned
}

fn draw_legacy_ppt_bullet(
    cr: &gtk::cairo::Context,
    text: &str,
    x: f64,
    y: f64,
    max_w: f64,
) -> Option<f64> {
    use gtk::cairo;

    cr.set_source_rgb(0.0, 0.75, 0.86);
    cr.rectangle(x, y - 11.0, 6.0, 10.0);
    cr.stroke().ok()?;

    cr.select_font_face("Serif", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
    cr.set_font_size(19.0);
    cr.set_source_rgb(0.05, 0.05, 0.05);

    let text_x = x + 16.0;
    let lines = wrap_text(cr, text, max_w - 16.0);
    let mut out_y = y;
    for (idx, line) in lines.iter().enumerate() {
        if idx == 0 {
            draw_legacy_ppt_emphasis(cr, line, text_x, out_y)?;
        } else {
            cr.select_font_face("Serif", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
            cr.set_font_size(19.0);
            cr.set_source_rgb(0.05, 0.05, 0.05);
            cr.move_to(text_x + 18.0, out_y);
            let _ = cr.show_text(line);
        }
        out_y += 26.0;
    }
    Some(out_y + 4.0)
}

fn draw_legacy_ppt_emphasis(cr: &gtk::cairo::Context, line: &str, x: f64, y: f64) -> Option<()> {
    use gtk::cairo;

    let country = line.split_whitespace().next().unwrap_or("");
    let emphasized = matches!(
        country,
        "Britain"
            | "British"
            | "French"
            | "France"
            | "Portugal"
            | "Portuguese"
            | "German"
            | "Germany"
    );
    if emphasized {
        cr.select_font_face("Serif", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
        cr.set_font_size(19.0);
        cr.set_source_rgb(0.05, 0.05, 0.05);
        cr.move_to(x, y);
        let _ = cr.show_text(country);
        let dx = cr.text_extents(country).map(|e| e.width()).unwrap_or(0.0) + 5.0;
        let rest = line.get(country.len()..).unwrap_or("").trim_start();
        cr.select_font_face("Serif", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
        cr.set_font_size(19.0);
        cr.move_to(x + dx, y);
        let _ = cr.show_text(rest);
    } else {
        cr.select_font_face("Serif", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
        cr.set_font_size(19.0);
        cr.set_source_rgb(0.05, 0.05, 0.05);
        cr.move_to(x, y);
        let _ = cr.show_text(line);
    }
    Some(())
}

fn fallback_ppt_layout(doc: &Path) -> SlideLayout {
    let title = doc
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Presentation")
        .replace(['_', '-'], " ");
    let slide_w = 9_144_000.0;
    let slide_h = 6_858_000.0;
    let mx = slide_w * 0.08;
    SlideLayout {
        slide_w,
        slide_h,
        background: None,
        elements: vec![SlideElement::Text(SlideBox {
            x: mx,
            y: slide_h * 0.16,
            w: slide_w - mx * 2.0,
            h: slide_h * 0.28,
            text: title,
            is_title: true,
            centered: true,
            font_pt: Some(30.0),
            has_xfrm: true,
            ph_type: "title".into(),
            color: None,
            paras: None,
            anchor: 0,
            insets: [91440.0, 45720.0, 91440.0, 45720.0],
            autofit_scale: 1.0,
        })],
    }
}

fn extract_ppt_text_atoms(bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    collect_ppt_text_atoms(bytes, &mut out, 0);
    if out.is_empty() {
        for s in extract_utf16le_strings(bytes, 3) {
            push_clean_ppt_text(&mut out, &s);
            if out.len() >= 16 {
                break;
            }
        }
    }
    if out.is_empty() {
        for s in extract_ascii_strings(bytes, 4) {
            push_clean_ppt_text(&mut out, &s);
            if out.len() >= 16 {
                break;
            }
        }
    }
    out
}

fn collect_ppt_text_atoms(bytes: &[u8], out: &mut Vec<String>, depth: usize) {
    if depth > 12 {
        return;
    }
    let mut pos = 0usize;
    while pos + 8 <= bytes.len() {
        let rec_info = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]);
        let rec_ver = rec_info & 0x000f;
        let rec_type = u16::from_le_bytes([bytes[pos + 2], bytes[pos + 3]]);
        let rec_len = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]) as usize;
        pos += 8;
        if rec_len > bytes.len().saturating_sub(pos) {
            break;
        }
        let payload = &bytes[pos..pos + rec_len];
        if rec_ver == 0x000f {
            collect_ppt_text_atoms(payload, out, depth + 1);
        } else {
            match rec_type {
                4000 | 4026 => push_clean_ppt_text(out, &decode_utf16le_lossy(payload)),
                4008 => push_clean_ppt_text(out, &decode_ppt_8bit_text(payload)),
                _ => {}
            }
        }
        if out.len() >= 16 {
            break;
        }
        pos += rec_len;
    }
}

fn decode_utf16le_lossy(bytes: &[u8]) -> String {
    let units = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]));
    char::decode_utf16(units)
        .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

/// Decode one TextBytesAtom (record 4008): the document's ANSI code page —
/// Windows-1252 for Western decks. ASCII and Latin-1 pass through, the
/// CP1252 specials above Latin-1 get their real mappings, and control bytes
/// (plus the 5 undefined CP1252 slots) become spaces so word separation
/// survives. The old decoder blanked every byte ≥ 0x7F, dropping all
/// accented text ("S ntese" instead of "Síntese").
fn decode_ppt_8bit_text(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| match b {
            b'\r' | b'\n' | b'\t' => b as char,
            0x20..=0x7e => b as char,
            // Latin-1 range — identical to CP1252 (í ó ç ã ñ …).
            0xa0..=0xff => b as char,
            // CP1252 punctuation/specials in 0x80..=0x9F.
            0x80 => '\u{20ac}', // €
            0x82 => '\u{201a}', // ‚
            0x83 => '\u{0192}', // ƒ
            0x84 => '\u{201e}', // „
            0x85 => '\u{2026}', // …
            0x86 => '\u{2020}', // †
            0x87 => '\u{2021}', // ‡
            0x88 => '\u{02c6}', // ˆ
            0x89 => '\u{2030}', // ‰
            0x8a => '\u{0160}', // Š
            0x8b => '\u{2039}', // ‹
            0x8c => '\u{0152}', // Œ
            0x8e => '\u{017d}', // Ž
            0x91 => '\u{2018}', // '
            0x92 => '\u{2019}', // '
            0x93 => '\u{201c}', // "
            0x94 => '\u{201d}', // "
            0x95 => '\u{2022}', // •
            0x96 => '\u{2013}', // –
            0x97 => '\u{2014}', // —
            0x98 => '\u{02dc}', // ˜
            0x99 => '\u{2122}', // ™
            0x9a => '\u{0161}', // š
            0x9b => '\u{203a}', // ›
            0x9c => '\u{0153}', // œ
            0x9e => '\u{017e}', // ž
            0x9f => '\u{0178}', // Ÿ
            _ => ' ', // control bytes + undefined CP1252 slots
        })
        .collect()
}

fn push_clean_ppt_text(out: &mut Vec<String>, text: &str) {
    for raw in text.split(['\r', '\n', '\u{0b}', '\u{0c}']) {
        let line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.len() < 2 || !line.chars().any(|c| c.is_alphabetic()) {
            continue;
        }
        if line.chars().filter(|c| c.is_control()).count() > 0 {
            continue;
        }
        if out.iter().any(|s| s.eq_ignore_ascii_case(&line)) {
            continue;
        }
        out.push(line);
        if out.len() >= 16 {
            break;
        }
    }
}

/// A slide's inherited color map: logical slot (bg1/tx1/bg2/tx2/accent…)
/// → theme color key (lt1/dk1/…). Masters define it with `<p:clrMap>`;
/// layouts and slides normally just inherit it (`<a:masterClrMapping/>`) but
/// may override with `<a:overrideClrMapping>`.
#[derive(Clone)]
struct ClrMap {
    slots: std::collections::HashMap<String, String>,
}

impl ClrMap {
    /// ECMA-376 default mapping (also used when a master has no `<p:clrMap>`).
    fn default_map() -> ClrMap {
        let mut slots = std::collections::HashMap::new();
        for (slot, key) in [
            ("bg1", "lt1"),
            ("tx1", "dk1"),
            ("bg2", "lt2"),
            ("tx2", "dk2"),
            ("accent1", "accent1"),
            ("accent2", "accent2"),
            ("accent3", "accent3"),
            ("accent4", "accent4"),
            ("accent5", "accent5"),
            ("accent6", "accent6"),
            ("hlink", "hlink"),
            ("folHlink", "folHlink"),
        ] {
            slots.insert(slot.to_string(), key.to_string());
        }
        ClrMap { slots }
    }

    /// Read `<p:clrMap bg1="…" …/>` from a slideMaster part.
    fn from_master(master_xml: &str) -> ClrMap {
        let mut map = ClrMap::default_map();
        if let Some(tag) = tag_substr(master_xml, "<p:clrMap") {
            for slot in [
                "bg1", "tx1", "bg2", "tx2", "accent1", "accent2", "accent3", "accent4",
                "accent5", "accent6", "hlink", "folHlink",
            ] {
                if let Some(v) = attr_value(&tag, slot) {
                    map.slots.insert(slot.to_string(), v);
                }
            }
        }
        map
    }

    /// Read `<a:overrideClrMapping …/>` (all slots present on the tag).
    fn from_override(tag: &str) -> ClrMap {
        let mut map = ClrMap::default_map();
        for slot in [
            "bg1", "tx1", "bg2", "tx2", "accent1", "accent2", "accent3", "accent4",
            "accent5", "accent6", "hlink", "folHlink",
        ] {
            if let Some(v) = attr_value(tag, slot) {
                map.slots.insert(slot.to_string(), v);
            }
        }
        map
    }

    /// Logical slot → theme color key (identity for keys already theme-named).
    fn resolve(&self, key: &str) -> String {
        self.slots.get(key).cloned().unwrap_or_else(|| key.to_string())
    }
}

/// Bullet directive of one style layer: keep inheriting vs. an explicit choice.
#[derive(Clone, PartialEq)]
enum BulletLayer {
    Inherit,
    Off,
    Char(String),
    AutoNum(String),
}

impl Default for BulletLayer {
    fn default() -> Self {
        BulletLayer::Inherit
    }
}

/// Effective defaults for one indent level (`<a:lvlNpPr>` + its `<a:defRPr>`).
/// Fields are Options so layers can override selectively; `None` = inherit.
#[derive(Clone, Default)]
struct TextStyleDef {
    sz_pt: Option<f64>,
    bold: Option<bool>,
    italic: Option<bool>,
    underline: Option<bool>,
    color: Option<(f64, f64, f64)>,
    font: Option<String>,
    algn: Option<String>,
    bullet: BulletLayer,
    /// `marL` of the level (EMU) — kept for the hanging-indent approximation.
    mar_l_emu: Option<f64>,
}

/// Which `<p:txStyles>` table a piece of text draws from.
#[derive(Clone, Copy, PartialEq)]
enum PhKind {
    Title,
    Body,
    Other,
}

/// Map a placeholder type onto the txStyles table (ECMA-376 defaults:
/// `title`/`ctrTitle` → titleStyle, the classic text placeholders →
/// bodyStyle, everything else — including plain text boxes — → otherStyle).
fn ph_kind_of(ph_type: &str) -> PhKind {
    match ph_type {
        "title" | "ctrTitle" => PhKind::Title,
        "" | "obj" | "body" | "subTitle" => PhKind::Body,
        _ => PhKind::Other,
    }
}

/// `<p:txStyles>` from the slide master: per-level defaults (index 0 = lvl1).
struct TxStyles {
    title: Vec<TextStyleDef>,
    body: Vec<TextStyleDef>,
    other: Vec<TextStyleDef>,
}

impl TxStyles {
    fn empty() -> TxStyles {
        TxStyles {
            title: Vec::new(),
            body: Vec::new(),
            other: Vec::new(),
        }
    }

    /// Parse `<p:txStyles>` of a slideMaster part.
    fn from_master(master_xml: &str, theme: &ThemeColors, map: &ClrMap) -> TxStyles {
        let mut out = TxStyles::empty();
        let Some(i) = find_open_tag(master_xml, 0, "p:txStyles") else {
            return out;
        };
        let Some((s, e)) = element_span(master_xml, i, "p:txStyles") else {
            return out;
        };
        let styles = &master_xml[s..e];
        out.title = parse_style_table(styles, "p:titleStyle", theme, map);
        out.body = parse_style_table(styles, "p:bodyStyle", theme, map);
        out.other = parse_style_table(styles, "p:otherStyle", theme, map);
        out
    }
}

/// Parse one txStyles table (`<p:titleStyle>`…) into level defs.
fn parse_style_table(styles: &str, name: &str, theme: &ThemeColors, map: &ClrMap) -> Vec<TextStyleDef> {
    let Some(i) = find_open_tag(styles, 0, name) else {
        return Vec::new();
    };
    let Some((s, e)) = element_span(styles, i, name) else {
        return Vec::new();
    };
    parse_levels(&styles[s..e], theme, map)
}

/// Parse `<a:lvl1pPr>` … `<a:lvl9pPr>` children into per-level defs.
fn parse_levels(table: &str, theme: &ThemeColors, map: &ClrMap) -> Vec<TextStyleDef> {
    let mut out: Vec<TextStyleDef> = Vec::new();
    let mut pos = 0usize;
    while let Some(li) = find_open_tag(table, pos, "a:lvl") {
        let Some((ls, le)) = lvl_span(table, li) else { break };
        pos = le;
        let lvl_xml = &table[ls..le];
        // level number from the tag name: <a:lvl3pPr> → index 2
        let tag = tag_substr(lvl_xml, "<a:lvl").unwrap_or_default();
        let digits: String = tag
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .take_while(|c| c.is_ascii_digit())
            .collect();
        let idx = digits.parse::<usize>().unwrap_or(1).saturating_sub(1).min(8);
        let def = parse_lvl_style(lvl_xml, theme, map);
        if out.len() <= idx {
            out.resize(idx + 1, TextStyleDef::default());
        }
        out[idx] = def;
    }
    out
}

/// Span of one `<a:lvlNpPr>` … `</a:lvlNpPr>` (the close tag carries digits,
/// so a plain `</a:lvl` substring search is used instead of tag-boundary
/// matching).
fn lvl_span(table: &str, open_start: usize) -> Option<(usize, usize)> {
    let open_end = tag_end(table, open_start)?;
    if table[open_start..open_end].ends_with("/>") {
        return Some((open_start, open_end));
    }
    let rel = table[open_end..].find("</a:lvl")?;
    let c = open_end + rel;
    let gt = table[c..].find('>')?;
    Some((open_start, c + gt + 1))
}

/// Parse one `<a:lvlNpPr>`: level properties (algn, marL), bullet children,
/// and the `<a:defRPr>` run defaults.
fn parse_lvl_style(lvl_xml: &str, theme: &ThemeColors, map: &ClrMap) -> TextStyleDef {
    let mut d = TextStyleDef::default();
    if let Some(tag) = tag_substr(lvl_xml, "<a:lvl") {
        if let Some(a) = attr_value(&tag, "algn") {
            d.algn = Some(a);
        }
        if let Some(m) = attr_value(&tag, "marL").and_then(|v| v.parse::<f64>().ok()) {
            d.mar_l_emu = Some(m);
        }
    }
    d.bullet = parse_bullet_layer(lvl_xml);
    if let Some(i) = find_open_tag(lvl_xml, 0, "a:defRPr") {
        if let Some((s, e)) = element_span(lvl_xml, i, "a:defRPr") {
            apply_rpr(&mut d, &lvl_xml[s..e], theme, map);
        }
    }
    d
}

/// Bullet children of a `<a:pPr>`/`<a:lvlNpPr>`:
/// `<a:buNone>` / `<a:buChar char="…">` / `<a:buAutoNum type="…">`.
fn parse_bullet_layer(frag: &str) -> BulletLayer {
    if find_open_tag(frag, 0, "a:buNone").is_some() {
        return BulletLayer::Off;
    }
    if let Some(i) = find_open_tag(frag, 0, "a:buChar") {
        let font = find_open_tag(frag, 0, "a:buFont")
            .and_then(|j| tag_substr(&frag[j..], "<a:buFont"))
            .and_then(|f| attr_value(&f, "typeface"));
        let ch = tag_substr(&frag[i..], "<a:buChar")
            .and_then(|t| attr_value(&t, "char"))
            .map(|c| decode_xml_entities(&c));
        let mapped = match ch {
            Some(c) => c
                .chars()
                .map(|g| map_bullet_char(g, font.as_deref()))
                .collect(),
            None => "\u{2022}".into(),
        };
        return BulletLayer::Char(mapped);
    }
    if let Some(i) = find_open_tag(frag, 0, "a:buAutoNum") {
        if let Some(t) = tag_substr(&frag[i..], "<a:buAutoNum") {
            let kind = attr_value(&t, "type").unwrap_or_else(|| "arabicPeriod".into());
            return BulletLayer::AutoNum(kind);
        }
        return BulletLayer::AutoNum("arabicPeriod".into());
    }
    BulletLayer::Inherit
}

/// Map a `buChar` glyph encoded for a symbol font (private-use, e.g. `U+F097`
/// = byte 0x97 of Wingdings 2) onto an equivalent Unicode bullet, so renderers
/// without proprietary fonts show a real bullet instead of tofu. Characters
/// outside the private-use ranges pass through unchanged; unmapped PUA bytes
/// degrade to a plain `•`. Tables transcribed from the Wingdings / Wingdings 2
/// code charts (the cells decks actually use for bullets).
fn map_bullet_char(ch: char, font_typeface: Option<&str>) -> String {
    let fam = font_typeface.unwrap_or("").to_ascii_lowercase();
    let table: &[(u32, char)] = if fam.contains("wingdings 2") {
        &[
            (0x94, '⋅'),
            (0x96, '⦁'),
            (0x97, '●'),
            (0x98, '○'),
            (0x9C, '⊙'),
            (0x9D, '⦿'),
            (0x9E, '🞌'),
            (0xA1, '◾'),
            (0xA2, '■'),
            (0xA3, '□'),
            (0xA8, '▣'),
        ]
    } else if fam.contains("wingdings") {
        &[
            (0x6C, '●'), // the classic body bullet: byte 'l' of Wingdings
            (0x6E, '■'),
            (0x6F, '□'),
            (0x76, '❑'),
            (0x77, '❒'),
            (0x7A, '◆'),
            (0x7B, '❖'),
            (0x9E, '∙'),
            (0x9F, '•'),
            (0xA0, '▪'),
            (0xA7, '▪'),
            (0xFC, '✔'),
        ]
    } else if fam.contains("symbol") {
        &[(0xB7, '•'), (0xA7, '■')]
    } else {
        &[]
    };
    let byte = match ch {
        '\u{e000}'..='\u{ffff}' => Some(ch as u32 & 0xFF),
        // Legacy .ppt bulletChar stores the symbol font's byte directly
        // (0x6C 'l' in Wingdings = ●) rather than a private-use code — but
        // only decode it when a table can speak that font, so a text-font
        // letter stays the letter.
        _ if !table.is_empty() && (ch as u32) <= 0xFF => Some(ch as u32),
        _ => None,
    };
    let Some(byte) = byte else {
        return ch.to_string();
    };
    table
        .iter()
        .find(|(b, _)| *b == byte)
        .map(|(_, c)| c.to_string())
        .unwrap_or_else(|| match ch {
            '\u{e000}'..='\u{ffff}' => "\u{2022}".to_string(), // PUA with no cell: dot, never tofu
            _ => ch.to_string(),                               // unmapped symbol byte: keep it
        })
}

/// Overlay one `<a:rPr>`/`<a:defRPr>` element's attributes onto `d`.
fn apply_rpr(d: &mut TextStyleDef, rpr: &str, theme: &ThemeColors, map: &ClrMap) {
    let Some(open_end) = tag_end(rpr, 0) else { return };
    let tag = &rpr[..open_end];
    if let Some(v) = attr_value(tag, "sz").and_then(|v| v.parse::<f64>().ok()) {
        d.sz_pt = Some(v / 100.0);
    }
    if let Some(v) = attr_value(tag, "b") {
        d.bold = Some(v == "1" || v == "true");
    }
    if let Some(v) = attr_value(tag, "i") {
        d.italic = Some(v == "1" || v == "true");
    }
    if let Some(v) = attr_value(tag, "u") {
        d.underline = Some(v != "none" && v != "0" && v != "false");
    }
    if let Some((c, _a)) = parse_color_el(rpr, theme, map) {
        d.color = Some(c);
    }
    if let Some(i) = find_open_tag(rpr, 0, "a:latin") {
        if let Some(t) = tag_substr(&rpr[i..], "<a:latin") {
            if let Some(f) = attr_value(&t, "typeface") {
                d.font = Some(f);
            }
        }
    }
}

/// Geometry + fill/line + `<a:lstStyle>` a layout/master placeholder defines;
/// the slide's same placeholder (matched by `idx`, then type) inherits these.
struct PhDef {
    ph_type: String,
    idx: Option<u32>,
    /// Absolute EMU rect (x, y, w, h), when the part specifies one.
    geo: Option<(f64, f64, f64, f64)>,
    fill: Option<FillKind>,
    line: Option<LineSpec>,
    /// `lvl1pPr` … `lvl9pPr` text-style overrides (index 0 = lvl1).
    lst: Vec<TextStyleDef>,
}

/// Placeholder type equivalence: `ctrTitle`↔`title`, and the body-like types
/// (`obj`/`body`/`subTitle`/absent) all match each other.
fn placeholder_types_match(a: &str, b: &str) -> bool {
    fn norm(t: &str) -> &str {
        match t {
            "ctrTitle" => "title",
            "" | "obj" | "body" | "subTitle" => "body",
            other => other,
        }
    }
    norm(a) == norm(b)
}

/// The layout/master placeholder a slide placeholder inherits from:
/// `idx` match first (PowerPoint's primary key), then normalized type match.
fn find_ph<'a>(defs: &'a [PhDef], ph_type: &str, idx: Option<u32>) -> Option<&'a PhDef> {
    if let Some(i) = idx {
        if let Some(d) = defs.iter().find(|d| d.idx == Some(i)) {
            return Some(d);
        }
    }
    defs.iter().find(|d| placeholder_types_match(&d.ph_type, ph_type))
}

/// Effective text-style resolution for one shape's text frame. Layers,
/// lowest precedence first: master `<p:txStyles>` table → master placeholder
/// `<a:lstStyle>` → layout placeholder `<a:lstStyle>` → paragraph `defRPr` →
/// run `rPr` (the latter two are applied by `parse_tx_body`).
struct StyleSource<'a> {
    kind: PhKind,
    /// Centered-title heuristic (`<p:ph type="ctrTitle"/>`).
    centered_title: bool,
    layout_ph: Option<&'a PhDef>,
    master_ph: Option<&'a PhDef>,
    tx: Option<&'a TxStyles>,
    /// `<p:style><a:fontRef>` color — the shape's theme default text color.
    font_ref_color: Option<(f64, f64, f64)>,
    major_font: &'a str,
    minor_font: &'a str,
    theme: &'a ThemeColors,
    map: &'a ClrMap,
}

impl<'a> StyleSource<'a> {
    /// Defaults for one indent level (0-based), placeholder `lstStyle`
    /// layers applied over the master's txStyles table. Heuristic gaps are
    /// filled by `finish` at use time.
    fn level(&self, lvl: usize) -> TextStyleDef {
        let table: Option<&Vec<TextStyleDef>> = match self.kind {
            PhKind::Title => self.tx.as_ref().map(|t| &t.title),
            PhKind::Body => self.tx.as_ref().map(|t| &t.body),
            PhKind::Other => self.tx.as_ref().map(|t| &t.other),
        };
        let mut d = table.and_then(|t| t.get(lvl)).cloned().unwrap_or_default();
        for ph in [self.master_ph, self.layout_ph].into_iter().flatten() {
            if let Some(s) = ph.lst.get(lvl) {
                merge_style(&mut d, s);
            }
        }
        d
    }

    /// Map a typeface attribute to a concrete family: `+mj-lt`/`+mn-lt` are
    /// theme placeholders; empty means "keep whatever else applies".
    fn font_name(&self, typeface: &str) -> Option<String> {
        if typeface.starts_with("+mj") {
            Some(self.major_font.to_string())
        } else if typeface.starts_with("+mn") {
            Some(self.minor_font.to_string())
        } else if typeface.is_empty() {
            None
        } else {
            Some(typeface.to_string())
        }
    }

    /// Fill heuristic gaps: default size (title 32pt / body 18pt), fontRef
    /// color, theme font, alignment, and "no bullet unless specified".
    fn finish(&self, d: &mut TextStyleDef) {
        if d.sz_pt.is_none() {
            d.sz_pt = Some(match self.kind {
                PhKind::Title => 32.0,
                _ => 18.0,
            });
        }
        if d.color.is_none() {
            d.color = self.font_ref_color;
        }
        if d.color.is_none() {
            d.color = Some((0.10, 0.10, 0.14));
        }
        if d.font.is_none() || d.font.as_deref() == Some("") {
            d.font = Some(
                match self.kind {
                    PhKind::Title => self.major_font,
                    _ => self.minor_font,
                }
                .to_string(),
            );
        }
        if d.algn.is_none() {
            d.algn = Some(if self.centered_title { "ctr".into() } else { "l".into() });
        }
        if d.bullet == BulletLayer::Inherit {
            d.bullet = BulletLayer::Off;
        }
    }
}

/// Overlay `s` (a higher-precedence layer) onto `d`, field by field.
fn merge_style(d: &mut TextStyleDef, s: &TextStyleDef) {
    if s.sz_pt.is_some() {
        d.sz_pt = s.sz_pt;
    }
    if s.bold.is_some() {
        d.bold = s.bold;
    }
    if s.italic.is_some() {
        d.italic = s.italic;
    }
    if s.underline.is_some() {
        d.underline = s.underline;
    }
    if s.color.is_some() {
        d.color = s.color;
    }
    if s.font.is_some() {
        d.font = s.font.clone();
    }
    if s.algn.is_some() {
        d.algn = s.algn.clone();
    }
    if s.bullet != BulletLayer::Inherit {
        d.bullet = s.bullet.clone();
    }
    if s.mar_l_emu.is_some() {
        d.mar_l_emu = s.mar_l_emu;
    }
}

/// Resolve a relationship Target (possibly with ../) against a base dir into a
/// normalized zip entry path (zip uses forward slashes, no leading slash).
fn normalize_zip_rel(base_dir: &str, target: &str) -> String {
    let mut parts: Vec<&str> = base_dir.split('/').collect();
    for seg in target.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

// ── Shape-tree geometry: EMU transforms and element spans ──

/// Affine map from a child coordinate space into its parent's, restricted to
/// axis-aligned scale + offset + mirror (group `off/ext/chOff/chExt` + flips):
/// x' = a·x + e, y' = d·y + f. Ancestor *rotation* is deliberately ignored —
/// rotated groups are rare and text must stay readable.
#[derive(Clone, Copy)]
struct XformMap {
    a: f64,
    d: f64,
    e: f64,
    f: f64,
}

impl XformMap {
    const ID: XformMap = XformMap { a: 1.0, d: 1.0, e: 0.0, f: 0.0 };

    fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        (self.a * x + self.e, self.d * y + self.f)
    }

    /// Map a rect (mirrored maps normalize the corners).
    fn apply_rect(&self, x: f64, y: f64, w: f64, h: f64) -> (f64, f64, f64, f64) {
        let (x0, y0) = self.apply(x, y);
        let (x1, y1) = self.apply(x + w, y + h);
        (x0.min(x1), y0.min(y1), (x1 - x0).abs(), (y1 - y0).abs())
    }
}

/// Raw `<a:xfrm>` contents in EMUs (plus rotation / flip attributes).
#[derive(Default)]
struct RawXfrm {
    off: Option<(f64, f64)>,
    ext: Option<(f64, f64)>,
    ch_off: Option<(f64, f64)>,
    ch_ext: Option<(f64, f64)>,
    /// Clockwise radians (DrawingML `rot` is 60000ths of a degree).
    rot: f64,
    flip_h: bool,
    flip_v: bool,
}

impl RawXfrm {
    fn rect(&self) -> Option<(f64, f64, f64, f64)> {
        let (ox, oy) = self.off?;
        let (ex, ey) = self.ext?;
        Some((ox, oy, ex, ey))
    }
}

/// Parse an `<a:xfrm>` element's contents (the element may be self-closing).
fn parse_raw_xfrm(xfrm: &str) -> RawXfrm {
    let mut out = RawXfrm::default();
    let pt = |name: &str, ax: &str, ay: &str| -> Option<(f64, f64)> {
        let i = find_open_tag(xfrm, 0, name)?;
        let t = tag_substr(&xfrm[i..], &format!("<{}", name))?;
        Some((attr_value(&t, ax)?.parse().ok()?, attr_value(&t, ay)?.parse().ok()?))
    };
    out.off = pt("a:off", "x", "y");
    out.ext = pt("a:ext", "cx", "cy");
    out.ch_off = pt("a:chOff", "x", "y");
    out.ch_ext = pt("a:chExt", "cx", "cy");
    if let Some(i) = find_open_tag(xfrm, 0, "a:xfrm") {
        if let Some(t) = tag_substr(&xfrm[i..], "<a:xfrm") {
            if let Some(v) = attr_value(&t, "rot").and_then(|v| v.parse::<f64>().ok()) {
                // 60000ths of a degree → radians
                out.rot = (v / 60_000.0).to_radians();
            }
            out.flip_h = attr_value(&t, "flipH").map(|v| v == "1" || v == "true").unwrap_or(false);
            out.flip_v = attr_value(&t, "flipV").map(|v| v == "1" || v == "true").unwrap_or(false);
        }
    }
    out
}

/// Compose a group's child-space map: the group's `off/ext` frame re-maps
/// `chOff/chExt` child coordinates, flips mirror about the frame center, and
/// everything is pre-composed with the parent map.
fn compose_group(parent: XformMap, g: &RawXfrm) -> XformMap {
    let (ox, oy) = g.off.unwrap_or((0.0, 0.0));
    let (ex, ey) = g.ext.unwrap_or((0.0, 0.0));
    let (cx, cy) = g.ch_off.unwrap_or((0.0, 0.0));
    let (chx, chy) = g.ch_ext.unwrap_or((0.0, 0.0));
    let sx = if chx > 0.0 { ex / chx } else { 1.0 };
    let sy = if chy > 0.0 { ey / chy } else { 1.0 };
    // child → parent: p = off + (q − chOff) · s
    let mut a = sx;
    let mut d = sy;
    let mut e = ox - cx * sx;
    let mut f = oy - cy * sy;
    if g.flip_h {
        // mirror about the group's horizontal center (in parent coords)
        let cxp = ox + ex * 0.5;
        a = -a;
        e = 2.0 * cxp - e;
    }
    if g.flip_v {
        let cyp = oy + ey * 0.5;
        d = -d;
        f = 2.0 * cyp - f;
    }
    XformMap {
        a: parent.a * a,
        d: parent.d * d,
        e: parent.a * e + parent.e,
        f: parent.d * f + parent.f,
    }
}

/// Index of the first `<name` at/after `pos`, requiring a tag-name boundary
/// after the name (`<p:sp` never matches `<p:spPr>`; `<a:lvl` matches
/// `<a:lvl3pPr>` — digits also count as boundaries).
fn find_open_tag(hay: &str, pos: usize, name: &str) -> Option<usize> {
    let needle = format!("<{}", name);
    let mut from = pos;
    while let Some(rel) = hay[from..].find(&needle) {
        let at = from + rel;
        let after = hay[at + needle.len()..].chars().next();
        if matches!(
            after,
            Some('>') | Some(' ') | Some('\t') | Some('\n') | Some('\r') | Some('/')
                | Some('0'..='9')
        ) {
            return Some(at);
        }
        from = at + needle.len();
    }
    None
}

/// Index just past the `>` of the tag starting at `open_start` (works for
/// self-closing `<x/>` too).
fn tag_end(hay: &str, open_start: usize) -> Option<usize> {
    let gt = hay[open_start..].find('>')? + open_start;
    Some(gt + 1)
}

/// Full `[start, end)` span of the element whose open tag starts at
/// `open_start`, including its close tag (just the tag when self-closing).
/// `close_name` is a tag name without `<`/`>` (e.g. `"p:sp"`).
fn element_span(hay: &str, open_start: usize, close_name: &str) -> Option<(usize, usize)> {
    let open_end = tag_end(hay, open_start)?;
    if hay[open_start..open_end].ends_with("/>") {
        return Some((open_start, open_end));
    }
    let c = find_open_tag(hay, open_end, &format!("/{}", close_name))?;
    let end = tag_end(hay, c)?;
    Some((open_start, end))
}

/// End index of a (possibly nested) `<p:grpSp>` whose open tag starts at
/// `start`: depth-matched so nested groups don't end the walk early.
fn grp_end(hay: &str, start: usize) -> Option<usize> {
    let open_end = tag_end(hay, start)?;
    if hay[start..open_end].ends_with("/>") {
        return Some(open_end);
    }
    let mut depth = 1usize;
    let mut pos = open_end;
    loop {
        let next_open = find_open_tag(hay, pos, "p:grpSp");
        let next_close = find_open_tag(hay, pos, "/p:grpSp");
        match (next_open, next_close) {
            (Some(o), Some(c)) if o < c => {
                depth += 1;
                pos = tag_end(hay, o)?;
            }
            (_, Some(c)) => {
                depth -= 1;
                let ce = tag_end(hay, c)?;
                if depth == 0 {
                    return Some(ce);
                }
                pos = ce;
            }
            (Some(o), None) => {
                depth += 1;
                pos = tag_end(hay, o)?;
            }
            (None, None) => return None,
        }
    }
}

/// A shape's `<p:style>` theme references: fill (`fillRef`), outline
/// (`lnRef`), and default text color (`fontRef`), colors already
/// shade/tint-transformed.
struct ShapeStyle {
    fill: Option<FillKind>,
    line: Option<LineSpec>,
    font_color: Option<(f64, f64, f64)>,
}

fn parse_pstyle(shape_xml: &str, theme: &ThemeColors, map: &ClrMap) -> Option<ShapeStyle> {
    let i = find_open_tag(shape_xml, 0, "p:style")?;
    let (s, e) = element_span(shape_xml, i, "p:style")?;
    let style = &shape_xml[s..e];
    let mut out = ShapeStyle { fill: None, line: None, font_color: None };

    // fillRef: solid fill of its scheme color (idx 0 = "no fill from style")
    if let Some(i) = find_open_tag(style, 0, "a:fillRef") {
        if let Some((fs, fe)) = element_span(style, i, "a:fillRef") {
            let frag = &style[fs..fe];
            let idx = tag_end(frag, 0)
                .and_then(|t| attr_value(&frag[..t], "idx"))
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(1);
            if idx > 0 {
                if let Some((c, a)) = parse_color_el(frag, theme, map) {
                    out.fill = Some(FillKind::Solid(c, a));
                }
            }
        }
    }
    // lnRef: outline color; width approximated from the reference index
    if let Some(i) = find_open_tag(style, 0, "a:lnRef") {
        if let Some((ls, le)) = element_span(style, i, "a:lnRef") {
            let frag = &style[ls..le];
            let idx = tag_end(frag, 0)
                .and_then(|t| attr_value(&frag[..t], "idx"))
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(1);
            if idx > 0 {
                if let Some((c, a)) = parse_color_el(frag, theme, map) {
                    let width = match idx {
                        1 => 9_525.0,   // 0.75pt
                        2 => 19_050.0,  // 1.5pt
                        3 => 28_575.0,  // 2.25pt
                        _ => 38_100.0,  // 3pt
                    };
                    out.line = Some(LineSpec { color: c, alpha: a, width_emu: width });
                }
            }
        }
    }
    // fontRef: the shape's default text color
    if let Some(i) = find_open_tag(style, 0, "a:fontRef") {
        if let Some((fs, fe)) = element_span(style, i, "a:fontRef") {
            out.font_color = parse_color_el(&style[fs..fe], theme, map).map(|(c, _a)| c);
        }
    }
    Some(out)
}

// ── Full-fidelity PPTX rendering ──

/// Resolved Office theme: color scheme, latin typefaces, and the background
/// fill styles referenced by `<p:bgRef idx="1001…"/>`.
struct ThemeColors {
    colors: std::collections::HashMap<String, (f64, f64, f64)>,
    major_font: String,
    minor_font: String,
    bg_fills: Vec<FillKind>,
}

impl ThemeColors {
    fn color(&self, key: &str) -> Option<(f64, f64, f64)> {
        self.colors.get(key).copied()
    }
}

/// Office default color scheme (used when theme1.xml is missing/partial).
fn default_theme_colors() -> std::collections::HashMap<String, (f64, f64, f64)> {
    let mut m = std::collections::HashMap::new();
    for (k, v) in [
        ("dk1", "000000"),
        ("lt1", "FFFFFF"),
        ("dk2", "44546A"),
        ("lt2", "E7E6E6"),
        ("accent1", "4472C4"),
        ("accent2", "ED7D31"),
        ("accent3", "A5A5A5"),
        ("accent4", "FFC000"),
        ("accent5", "5B9BD5"),
        ("accent6", "70AD47"),
        ("hlink", "0563C1"),
        ("folHlink", "954F72"),
    ] {
        if let Some(c) = parse_hex_color(v) {
            m.insert(k.to_string(), c);
        }
    }
    m
}

/// `<a:clrScheme>` color slots of a theme XML into `colors` (the Office
/// defaults stand in for slots the file doesn't define). Shared by the
/// pptx theme reader and xlsx `<color theme="N"/>` resolution.
fn parse_clr_scheme(
    xml: &str,
    colors: &mut std::collections::HashMap<String, (f64, f64, f64)>,
) {
    let Some(i) = find_open_tag(xml, 0, "a:clrScheme") else {
        return;
    };
    let Some((s, e)) = element_span(xml, i, "a:clrScheme") else {
        return;
    };
    let scheme = &xml[s..e];
    for slot in [
        "dk1", "lt1", "dk2", "lt2", "accent1", "accent2", "accent3", "accent4", "accent5",
        "accent6", "hlink", "folHlink",
    ] {
        if let Some(si) = find_open_tag(scheme, 0, &format!("a:{}", slot)) {
            if let Some((ss, se)) = element_span(scheme, si, &format!("a:{}", slot)) {
                if let Some(c) = literal_hex(&scheme[ss..se]) {
                    colors.insert(slot.to_string(), c);
                }
            }
        }
    }
}

/// Load `ppt/theme/theme1.xml`: color scheme (srgbClr/sysClr slots), the
/// major/minor latin typefaces, and `<a:bgFillStyleLst>` fills.
fn parse_theme_colors<R: std::io::Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
) -> ThemeColors {
    let mut theme = ThemeColors {
        colors: default_theme_colors(),
        major_font: "Calibri Light".into(),
        minor_font: "Calibri".into(),
        bg_fills: Vec::new(),
    };
    let Some(xml) = read_zip_text(zip, "ppt/theme/theme1.xml") else {
        return theme;
    };

    // color scheme slots
    parse_clr_scheme(&xml, &mut theme.colors);

    // latin typefaces (skip theme placeholders like "+mj-lt")
    if let Some(i) = find_open_tag(&xml, 0, "a:majorFont") {
        if let Some((s, e)) = element_span(&xml, i, "a:majorFont") {
            if let Some(f) = latin_typeface(&xml[s..e]) {
                theme.major_font = f;
            }
        }
    }
    if let Some(i) = find_open_tag(&xml, 0, "a:minorFont") {
        if let Some((s, e)) = element_span(&xml, i, "a:minorFont") {
            if let Some(f) = latin_typeface(&xml[s..e]) {
                theme.minor_font = f;
            }
        }
    }

    // <a:bgFillStyleLst> — what <p:bgRef idx="1001…"> points at
    if let Some(i) = find_open_tag(&xml, 0, "a:bgFillStyleLst") {
        if let Some((s, e)) = element_span(&xml, i, "a:bgFillStyleLst") {
            let list = &xml[s..e];
            let tmap = ClrMap::default_map();
            let mut pos = 0usize;
            while pos < list.len() {
                let next = ["a:solidFill", "a:gradFill"]
                    .into_iter()
                    .filter_map(|n| find_open_tag(list, pos, n).map(|idx| (idx, n)))
                    .min_by_key(|(idx, _)| *idx);
                let Some((i, name)) = next else { break };
                let Some((fs, fe)) = element_span(list, i, name) else { break };
                pos = fe;
                if let Some(f) = fill_from_element(&list[fs..fe], name, &theme, &tmap) {
                    theme.bg_fills.push(f);
                }
            }
        }
    }
    theme
}

/// Read a literal hex color (`<a:srgbClr>` / `<a:sysClr lastClr>`) from a
/// theme color-slot fragment — used while parsing the theme itself, before a
/// `ThemeColors` exists.
fn literal_hex(frag: &str) -> Option<(f64, f64, f64)> {
    for name in ["a:srgbClr", "a:sysClr"] {
        if let Some(i) = find_open_tag(frag, 0, name) {
            if let Some(t) = tag_substr(&frag[i..], &format!("<{}", name)) {
                if let Some(v) = attr_value(&t, "lastClr").or_else(|| attr_value(&t, "val")) {
                    if let Some(c) = parse_hex_color(&v) {
                        return Some(c);
                    }
                }
            }
        }
    }
    None
}

/// `<a:latin typeface="…"/>` inside a font fragment; skips theme
/// placeholders (`+mj-lt`, `{langid…}` linked fonts).
fn latin_typeface(frag: &str) -> Option<String> {
    let i = find_open_tag(frag, 0, "a:latin")?;
    let t = tag_substr(&frag[i..], "<a:latin")?;
    let f = attr_value(&t, "typeface")?;
    if f.is_empty() || f.starts_with('+') || f.starts_with('{') {
        None
    } else {
        Some(f)
    }
}

/// `#RRGGBB` / `RRGGBB` (or 8-digit `RRGGBBAA`, alpha ignored) → 0..1.
fn parse_hex_color(hex: &str) -> Option<(f64, f64, f64)> {
    let h = hex.trim().trim_start_matches('#');
    if !h.is_ascii() || (h.len() != 6 && h.len() != 8) {
        return None;
    }
    let ch = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok().map(|v| v as f64 / 255.0);
    Some((ch(0)?, ch(2)?, ch(4)?))
}

/// Resolve the color that FILLS `frag`: the `<a:srgbClr>`/`<a:schemeClr>`/
/// `<a:sysClr>` inside its `<a:solidFill>`, else its first direct color
/// child (`fontRef`, `fillRef`, `gs`, `bgRef` …), including
/// `<a:alpha|tint|shade|lumMod|lumOff>` children. Colors nested deeper —
/// a drop-shadow's black in `<a:effectLst>`, a `<a:highlight>`, an outline
/// in `<a:ln>` — are decoration, never a fill, and are ignored.
/// Returns `(rgb, alpha 0..1)`; None when the fragment names no fill color.
fn parse_color_el(frag: &str, theme: &ThemeColors, map: &ClrMap) -> Option<((f64, f64, f64), f64)> {
    let solid = find_open_tag(frag, 0, "a:solidFill")
        .and_then(|i| element_span(frag, i, "a:solidFill"));
    let scope: &str = match solid {
        Some((s, e)) => &frag[s..e],
        None => frag,
    };
    // First DIRECT color child of `scope`, skipping other children wholesale
    // (effectLst/ln/highlight contain colors that are not fills).
    let mut pos = tag_end(scope, 0)?;
    if scope[..pos].ends_with("/>") {
        return None;
    }
    let (i, name): (usize, &str) = loop {
        let lt = pos + scope[pos..].find('<')?;
        if scope[lt + 1..].starts_with('/') {
            return None; // `scope` closed without a color child
        }
        let te = tag_end(scope, lt)?;
        let body = &scope[lt + 1..te];
        let nlen = body
            .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
            .unwrap_or(body.len());
        let tag_name = &body[..nlen];
        let name = match tag_name {
            "a:srgbClr" | "a:schemeClr" | "a:sysClr" => tag_name,
            _ => "",
        };
        if !name.is_empty() {
            break (lt, name);
        }
        pos = if scope[lt..te].ends_with("/>") {
            te
        } else {
            element_span(scope, lt, tag_name)
                .map(|(_, e)| e)
                .unwrap_or(te)
        };
    };
    let (s, e) = element_span(scope, i, name)?;
    let el = &scope[s..e];
    let tag = tag_end(el, 0).map(|t| &el[..t]).unwrap_or(el);
    let val = attr_value(tag, "val")?;

    let base: (f64, f64, f64) = if name == "a:srgbClr" {
        parse_hex_color(&val)?
    } else if name == "a:sysClr" {
        // sysClr: `lastClr` is the concrete color; `val` may be a system name.
        attr_value(tag, "lastClr").and_then(|v| parse_hex_color(&v)).or_else(|| parse_hex_color(&val))?
    } else {
        // schemeClr: map the logical slot through the part's clrMap to a
        // theme key (tx1 → dk1, bg1 → lt1 …) and resolve it.
        theme.color(&map.resolve(&val))?
    };

    // child transforms
    let mut alpha = 1.0f64;
    let mut lum_mod = 1.0f64;
    let mut lum_off = 0.0f64;
    let mut tint = 1.0f64;
    let mut shade = 1.0f64;
    for tname in ["a:alpha", "a:lumMod", "a:lumOff", "a:tint", "a:shade"] {
        if let Some(i) = find_open_tag(el, 0, tname) {
            if let Some(t) = tag_substr(&el[i..], &format!("<{}", tname)) {
                if let Some(v) = attr_value(&t, "val").and_then(|v| v.parse::<f64>().ok()) {
                    let v = (v / 100_000.0).clamp(0.0, 1.0);
                    match tname {
                        "a:alpha" => alpha = v,
                        "a:lumMod" => lum_mod = v,
                        "a:lumOff" => lum_off = v,
                        "a:tint" => tint = v,
                        "a:shade" => shade = v,
                        _ => {}
                    }
                }
            }
        }
    }
    // luminance scale + offset (channel-wise approximation), then tint
    // (mix toward white) / shade (scale toward black)
    let (mut r, mut g, mut b) = base;
    r = (r * lum_mod + lum_off).clamp(0.0, 1.0);
    g = (g * lum_mod + lum_off).clamp(0.0, 1.0);
    b = (b * lum_mod + lum_off).clamp(0.0, 1.0);
    let mix_white = |c: f64| (tint * c + (1.0 - tint)).clamp(0.0, 1.0);
    let color = (mix_white(r) * shade, mix_white(g) * shade, mix_white(b) * shade);
    Some((color, alpha))
}

/// Parse a fill element (the fragment STARTS at its open tag). `name` is the
/// fill kind: noFill/solidFill/gradFill/pattFill. `<a:blipFill>` (picture
/// fill) and unknowns return None → the caller falls through to lower layers.
fn fill_from_element(
    frag: &str,
    name: &str,
    theme: &ThemeColors,
    map: &ClrMap,
) -> Option<FillKind> {
    let (s, e) = element_span(frag, 0, name)?;
    let el = &frag[s..e];
    match name {
        "a:noFill" | "a:grpFill" => Some(FillKind::None),
        "a:solidFill" => {
            let (c, a) = parse_color_el(el, theme, map)?;
            Some(FillKind::Solid(c, a))
        }
        "a:gradFill" => parse_gradient(el, theme, map),
        "a:pattFill" => {
            // patterned fills approximate to their foreground color
            let i = find_open_tag(el, 0, "a:fgClr")?;
            let (cs, ce) = element_span(el, i, "a:fgClr")?;
            let (c, a) = parse_color_el(&el[cs..ce], theme, map)?;
            Some(FillKind::Solid(c, a))
        }
        _ => None,
    }
}

/// The first fill element inside `frag` (e.g. an `<p:spPr>`), resolved.
/// Returns None when no fill is named (inherit a lower layer); `<a:noFill>`
/// yields `Some(FillKind::None)` = an explicit "stop, no fill". Candidates
/// inside `<a:ln>` are excluded — an outline's `<a:noFill/>` must not be
/// mistaken for the shape's fill.
fn parse_fill_el(frag: &str, theme: &ThemeColors, map: &ClrMap) -> Option<FillKind> {
    let ln_start = find_open_tag(frag, 0, "a:ln").unwrap_or(usize::MAX);
    let (i, name) = ["a:noFill", "a:solidFill", "a:gradFill", "a:pattFill", "a:grpFill", "a:blipFill"]
        .into_iter()
        .filter_map(|n| find_open_tag(frag, 0, n).map(|idx| (idx, n)))
        .filter(|(idx, _)| *idx < ln_start)
        .min_by_key(|(idx, _)| *idx)?;
    fill_from_element(&frag[i..], name, theme, map)
}

/// `<a:gradFill>` → gradient stops + direction angle (cairo convention:
/// 0 = left→right, positive = clockwise — same as DrawingML `ang`).
fn parse_gradient(el: &str, theme: &ThemeColors, map: &ClrMap) -> Option<FillKind> {
    let mut stops: Vec<(f64, (f64, f64, f64), f64)> = Vec::new();
    let mut pos = 0usize;
    while let Some(i) = find_open_tag(el, pos, "a:gs") {
        let Some((s, e)) = element_span(el, i, "a:gs") else { break };
        pos = e;
        let gs = &el[s..e];
        let t = tag_end(gs, 0).map(|t| &gs[..t]).unwrap_or("");
        let p = attr_value(t, "pos").and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0) / 100_000.0;
        if let Some((c, a)) = parse_color_el(gs, theme, map) {
            stops.push((p.clamp(0.0, 1.0), c, a));
        }
    }
    if stops.is_empty() {
        return None;
    }
    stops.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    // direction: <a:lin ang="…" /> in 60000ths of a degree; path/circle
    // gradients approximate to top→bottom
    let mut angle = std::f64::consts::FRAC_PI_2;
    if let Some(i) = find_open_tag(el, 0, "a:lin") {
        if let Some(t) = tag_substr(&el[i..], "<a:lin") {
            if let Some(v) = attr_value(&t, "ang").and_then(|v| v.parse::<f64>().ok()) {
                angle = (v / 60_000.0).to_radians();
            }
        }
    }
    Some(FillKind::Gradient(stops, angle))
}

/// Outline of a shape: absent (`Inherit`), explicitly hidden, or a spec.
enum LineChoice {
    Inherit,
    Hide,
    Spec(LineSpec),
}

/// Parse `<a:ln>` from a shape's spPr fragment.
fn parse_ln_el(frag: &str, theme: &ThemeColors, map: &ClrMap) -> LineChoice {
    let Some(i) = find_open_tag(frag, 0, "a:ln") else { return LineChoice::Inherit };
    let Some((s, e)) = element_span(frag, i, "a:ln") else { return LineChoice::Inherit };
    let ln = &frag[s..e];
    if find_open_tag(ln, 0, "a:noFill").is_some() {
        return LineChoice::Hide;
    }
    let w = tag_end(ln, 0)
        .and_then(|t| attr_value(&ln[..t], "w"))
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(12_700.0)
        .max(1.0);
    if let Some((c, a)) = parse_color_el(ln, theme, map) {
        return LineChoice::Spec(LineSpec { color: c, alpha: a, width_emu: w });
    }
    // `<a:ln w="…">` with no color child: black hairline
    LineChoice::Spec(LineSpec { color: (0.0, 0.0, 0.0), alpha: 1.0, width_emu: w })
}

/// Resolve one part's `<p:bg>`: picture (blip), explicit solid/gradient, or
/// theme `<p:bgRef idx>` — None when the part names no usable background (the
/// caller then tries the next part down the slide → layout → master chain).
fn parse_bg_el(
    xml: &str,
    media: &std::collections::HashMap<String, PathBuf>,
    theme: &ThemeColors,
    map: &ClrMap,
) -> Option<SlideBackground> {
    let i = find_open_tag(xml, 0, "p:bg")?;
    let (s, e) = element_span(xml, i, "p:bg")?;
    let bg = &xml[s..e];

    // picture background: <p:bg><a:blipFill><a:blip r:embed="rIdN"/>
    if let Some(bi) = find_open_tag(bg, 0, "a:blip") {
        if let Some(t) = tag_substr(&bg[bi..], "<a:blip") {
            if let Some(rid) = attr_value(&t, "r:embed").or_else(|| attr_value(&t, "r:link")) {
                if let Some(p) = media.get(&rid) {
                    return Some(SlideBackground::Image(p.clone()));
                }
            }
        }
    }

    // explicit fill (solid/gradient, usually inside <p:bgPr>); an explicit
    // noFill means "keep searching" up the chain
    if let Some(f) = parse_fill_el(bg, theme, map) {
        match f {
            FillKind::Solid(c, _) => return Some(SlideBackground::Solid(c.0, c.1, c.2)),
            FillKind::Gradient(stops, ang) => return Some(SlideBackground::Gradient(stops, ang)),
            FillKind::None => {}
        }
    }

    // theme reference: <p:bgRef idx="1001"><a:schemeClr…/>
    if let Some(i) = find_open_tag(bg, 0, "p:bgRef") {
        let Some((s, e)) = element_span(bg, i, "p:bgRef") else { return None };
        let ref_frag = &bg[s..e];
        let idx = tag_end(ref_frag, 0)
            .and_then(|t| attr_value(&ref_frag[..t], "idx"))
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        if idx >= 1000 {
            // bgFillStyleLst entries are 0-based; PowerPoint's idx counts from
            // 1001 (try 1001-base first, then 1000-base for producers that
            // start at 1000).
            let n = theme.bg_fills.len();
            let k = [idx.checked_sub(1001), idx.checked_sub(1000)]
                .into_iter()
                .flatten()
                .find(|k| *k < n);
            if let Some(k) = k {
                match &theme.bg_fills[k] {
                    FillKind::Solid(c, _) => return Some(SlideBackground::Solid(c.0, c.1, c.2)),
                    FillKind::Gradient(stops, ang) => {
                        return Some(SlideBackground::Gradient(stops.clone(), *ang));
                    }
                    FillKind::None => {}
                }
            }
        }
        // child color (e.g. <p:bgRef idx="1000"><a:schemeClr val="bg1"/>)
        if let Some((c, _)) = parse_color_el(ref_frag, theme, map) {
            return Some(SlideBackground::Solid(c.0, c.1, c.2));
        }
    }
    None
}

// ── PPTX text frames: paragraphs, runs, bullets ──

/// Everything `parse_tx_body` needs: the shape's style-layer stack plus a
/// per-frame auto-numbering counter (`(kind, level) → current value`).
struct TxEnv<'a> {
    src: StyleSource<'a>,
    nums: std::collections::HashMap<(String, usize), u32>,
}

impl<'a> TxEnv<'a> {
    fn new(src: StyleSource<'a>) -> TxEnv<'a> {
        TxEnv { src, nums: std::collections::HashMap::new() }
    }
}

/// Parse a `<p:txBody>`: `<a:bodyPr>` (anchor / insets / normAutofit) and
/// `<a:p>` paragraphs with per-run formatting.
/// Returns `(paras, anchor, insets EMU, autofit scale)`.
fn parse_tx_body(tx: &str, env: &mut TxEnv) -> (Vec<TextPara>, u8, [f64; 4], f64) {
    // ── <a:bodyPr>: vertical anchor, text insets, autofit scale ──
    let mut anchor = 0u8;
    let mut insets = [91_440.0, 45_720.0, 91_440.0, 45_720.0]; // l, t, r, b (EMU)
    let mut autofit = 1.0f64;
    if let Some(i) = find_open_tag(tx, 0, "a:bodyPr") {
        if let Some((s, e)) = element_span(tx, i, "a:bodyPr") {
            let body = &tx[s..e];
            let open = tag_end(body, 0).map(|t| &body[..t]).unwrap_or("");
            anchor = match attr_value(open, "anchor").as_deref() {
                Some("ctr") => 1,
                Some("b") => 2,
                _ => 0,
            };
            for (attr, slot) in [("lIns", 0usize), ("tIns", 1), ("rIns", 2), ("bIns", 3)] {
                if let Some(v) = attr_value(open, attr).and_then(|v| v.parse::<f64>().ok()) {
                    insets[slot] = v;
                }
            }
            if let Some(fi) = find_open_tag(body, 0, "a:normAutofit") {
                if let Some(t) = tag_substr(&body[fi..], "<a:normAutofit") {
                    if let Some(v) = attr_value(&t, "fontScale").and_then(|v| v.parse::<f64>().ok()) {
                        autofit = (v / 100_000.0).clamp(0.1, 4.0);
                    }
                }
            }
        }
    }

    // ── paragraphs ──
    let mut paras = Vec::new();
    let mut pos = 0usize;
    while let Some(i) = find_open_tag(tx, pos, "a:p") {
        let Some((s, e)) = element_span(tx, i, "a:p") else { break };
        pos = e;
        parse_paragraph(&tx[s..e], env, &mut paras);
    }

    // Trailing empty paragraphs only add dead vertical space; drop them
    // (a body whose text lives entirely in the master's prompt text must
    // render as nothing at all, not as a stack of blanks).
    while paras
        .last()
        .map(|p| p.runs.iter().all(|r| r.text.trim().is_empty()))
        .unwrap_or(false)
    {
        paras.pop();
    }
    (paras, anchor, insets, autofit)
}

/// Parse one `<a:p>`: paragraph properties (level, alignment, bullet,
/// spacing, `defRPr` defaults) + content (`<a:r>`, `<a:br>` splits,
/// `<a:fld>` fields) in document order.
fn parse_paragraph(pfrag: &str, env: &mut TxEnv, out: &mut Vec<TextPara>) {
    // ── <a:pPr> ──
    let ppr: Option<&str> = find_open_tag(pfrag, 0, "a:pPr")
        .and_then(|i| element_span(pfrag, i, "a:pPr"))
        .map(|(s, e)| &pfrag[s..e]);
    let ppr_tag = ppr.and_then(|p| tag_substr(p, "<a:pPr"));

    let lvl = ppr_tag
        .as_deref()
        .and_then(|t| attr_value(t, "lvl"))
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0)
        .min(8);

    // Level defaults from the shape's style stack, heuristic gaps filled.
    let mut base = env.src.level(lvl);
    env.src.finish(&mut base);

    // Alignment: pPr attr wins over the level's, then the heuristic default.
    let mut algn = ppr_tag
        .as_deref()
        .and_then(|t| attr_value(t, "algn"))
        .or_else(|| base.algn.clone())
        .unwrap_or_else(|| "l".into());
    if algn == "just" {
        algn = "l".into(); // this Pango binding has no justify
    }

    // Bullet: pPr children win over the level's, then "no bullet".
    let bullet_layer = ppr.map(parse_bullet_layer).unwrap_or(BulletLayer::Inherit);
    let bullet_layer = match bullet_layer {
        BulletLayer::Inherit => base.bullet.clone(),
        other => other,
    };
    let bullet = match bullet_layer {
        BulletLayer::Off => Some(ParaBullet::Off),
        BulletLayer::Inherit => None,
        BulletLayer::Char(c) => Some(ParaBullet::Char(c)),
        BulletLayer::AutoNum(kind) => {
            let n = env.nums.entry((kind.clone(), lvl)).or_insert(0);
            *n += 1;
            Some(ParaBullet::Number(kind, *n))
        }
    };

    // marL (text column) + hanging indent: pPr attrs win over the level's,
    // then a lvl-staggered default. `indent` is the margin-zone width (the
    // standard pairing is indent = −marL); when absent it defaults to marL.
    let mar_l_emu = ppr_tag
        .as_deref()
        .and_then(|t| attr_value(t, "marL"))
        .and_then(|v| v.parse::<f64>().ok())
        .or(base.mar_l_emu)
        .unwrap_or_else(|| lvl as f64 * 342_900.0);
    let hang_emu = ppr_tag
        .as_deref()
        .and_then(|t| attr_value(t, "indent"))
        .and_then(|v| v.parse::<f64>().ok())
        .map(|v| v.abs())
        .unwrap_or(mar_l_emu);

    // ── <a:defRPr> inside pPr: paragraph-level run defaults ──
    if let Some(p) = ppr {
        if let Some(i) = find_open_tag(p, 0, "a:defRPr") {
            if let Some((s, e)) = element_span(p, i, "a:defRPr") {
                apply_rpr(&mut base, &p[s..e], env.src.theme, env.src.map);
            }
        }
    }

    // ── spacing (only spcPts is meaningful for stacking) ──
    let mut spc_bef = 0.0f64;
    let mut spc_aft = 0.0f64;
    if let Some(p) = ppr {
        for (name, slot) in [("a:spcBef", 0usize), ("a:spcAft", 1)] {
            if let Some(i) = find_open_tag(p, 0, name) {
                if let Some((s, e)) = element_span(p, i, name) {
                    let frag = &p[s..e];
                    if let Some(pi) = find_open_tag(frag, 0, "a:spcPts") {
                        if let Some(t) = tag_substr(&frag[pi..], "<a:spcPts") {
                            if let Some(v) = attr_value(&t, "val").and_then(|v| v.parse::<f64>().ok()) {
                                let pts = v / 100.0;
                                if slot == 0 {
                                    spc_bef = pts;
                                } else {
                                    spc_aft = pts;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // ── content in document order: <a:r>, <a:br>, <a:fld> ──
    let mut cur = TextPara {
        runs: Vec::new(),
        algn: algn.clone(),
        lvl,
        bullet,
        spc_bef_pt: spc_bef,
        spc_aft_pt: spc_aft,
        follow: false,
        mar_l_emu,
        hang_emu,
    };
    let mut pos = 0usize;
    loop {
        let next = ["a:r", "a:br", "a:fld"]
            .iter()
            .filter_map(|n| find_open_tag(pfrag, pos, n).map(|i| (i, *n)))
            .min_by_key(|(i, _)| *i);
        let Some((i, name)) = next else { break };
        let Some((s, e)) = element_span(pfrag, i, name) else { break };
        pos = e;
        match name {
            // line break: flush the current line as a paragraph, continue as
            // a bullet-less follow-up sharing alignment (spacing stays on the
            // last piece of the sequence)
            "a:br" => {
                let mut flushed = std::mem::replace(
                    &mut cur,
                    TextPara {
                        runs: Vec::new(),
                        algn: algn.clone(),
                        lvl,
                        bullet: None,
                        spc_bef_pt: 0.0,
                        spc_aft_pt: spc_aft,
                        follow: true,
                        mar_l_emu,
                        hang_emu,
                    },
                );
                flushed.spc_aft_pt = 0.0;
                out.push(flushed);
            }
            "a:r" | "a:fld" => {
                let run_frag = &pfrag[s..e];
                let rpr: Option<&str> = find_open_tag(run_frag, 0, "a:rPr")
                    .and_then(|ii| element_span(run_frag, ii, "a:rPr"))
                    .map(|(ss, ee)| &run_frag[ss..ee]);
                let mut run_base = base.clone();
                if let Some(rp) = rpr {
                    apply_rpr(&mut run_base, rp, env.src.theme, env.src.map);
                }
                let text = run_texts(run_frag);
                if text.is_empty() {
                    continue;
                }
                // typeface: run's concrete family, theme placeholders mapped,
                // empty/absent falls back to the level default
                let font = run_base
                    .font
                    .as_deref()
                    .filter(|f| !f.is_empty())
                    .and_then(|f| env.src.font_name(f))
                    .or_else(|| base.font.clone());
                cur.runs.push(TextRun {
                    text,
                    sz_pt: run_base.sz_pt,
                    bold: run_base.bold.unwrap_or(false),
                    italic: run_base.italic.unwrap_or(false),
                    underline: run_base.underline.unwrap_or(false),
                    color: run_base.color,
                    font,
                });
            }
            _ => unreachable!(),
        }
    }
    out.push(cur);
}

/// Concatenated `<a:t>` text of a run/field fragment (entities decoded).
fn run_texts(frag: &str) -> String {
    let mut out = String::new();
    let mut pos = 0usize;
    while let Some(i) = find_open_tag(frag, pos, "a:t") {
        let Some((s, e)) = element_span(frag, i, "a:t") else { break };
        pos = e;
        let el = &frag[s..e];
        if el.ends_with("/>") {
            continue; // <a:t/>
        }
        let body_start = match tag_end(el, 0) {
            Some(t) => t,
            None => continue,
        };
        let body_end = el.len().saturating_sub("</a:t>".len());
        if body_end >= body_start {
            out.push_str(&decode_xml_entities(&el[body_start..body_end]));
        }
    }
    out
}

// ── Shape-tree walk: document order, groups, placeholders ──

/// Which package part's shape tree is being walked.
#[derive(Clone, Copy, PartialEq)]
enum PartRole {
    Master,
    Layout,
    Slide,
}

/// Shared inputs for walking one part's shape tree.
struct WalkCtx<'a> {
    role: PartRole,
    theme: &'a ThemeColors,
    map: &'a ClrMap,
    /// rId → extracted media path for THIS part.
    media: &'a std::collections::HashMap<String, PathBuf>,
    /// Master `<p:txStyles>`.
    tx: Option<&'a TxStyles>,
    /// Placeholder defs already collected from the layout / master.
    layout_ph: &'a [PhDef],
    master_ph: &'a [PhDef],
    /// Theme typefaces (`+mj-lt` / `+mn-lt` resolution).
    major_font: &'a str,
    minor_font: &'a str,
    /// Chart rels already parsed from the zip (rId → doughnut spec). The
    /// shape-tree walk only sees the slide XML, so `parse_pptx_slide`
    /// pre-resolves every `<c:chart r:id>` target before walking.
    charts: &'a std::collections::HashMap<String, DoughnutSpec>,
}

/// Walk a part's `spTree` (or a group's children) in document order — the
/// resulting element order IS the z-order. Master/layout placeholder shapes
/// are collected into `ph_defs` instead of being drawn (the slide
/// instantiates them); everything else becomes drawable elements.
fn parse_shape_tree(
    xml: &str,
    role: PartRole,
    parent: XformMap,
    ctx: &WalkCtx,
    out: &mut Vec<SlideElement>,
    ph_defs: &mut Vec<PhDef>,
) {
    // Full part or group-children fragment; use the spTree span when present
    // so nvGrpSpPr/grpSpPr bookkeeping children are skipped.
    let tree: &str = match find_open_tag(xml, 0, "p:spTree").and_then(|i| element_span(xml, i, "p:spTree"))
    {
        Some((s, e)) => &xml[s..e],
        None => xml,
    };

    const OPENS: [(&str, &str); 5] = [
        ("p:sp", "p:sp"),
        ("p:pic", "p:pic"),
        ("p:grpSp", "p:grpSp"),
        ("p:cxnSp", "p:cxnSp"),
        ("p:graphicFrame", "p:graphicFrame"),
    ];
    let mut pos = 0usize;
    while let Some((i, name)) = OPENS
        .iter()
        .filter_map(|(n, c)| find_open_tag(tree, pos, n).map(|i| (i, *c)))
        .min_by_key(|(i, _)| *i)
    {
        // Groups need depth-matched close tags (nested groups).
        let span = if name == "p:grpSp" {
            grp_end(tree, i).map(|e| (i, e))
        } else {
            element_span(tree, i, name)
        };
        let Some((s, e)) = span else { break };
        pos = e;
        let frag = &tree[s..e];
        match name {
            "p:grpSp" => {
                // compose the group transform: off/ext frame + chOff/chExt
                // child space + flips, applied to everything inside
                let child_map = frag
                    .find("<p:grpSpPr")
                    .and_then(|_| {
                        let gi = find_open_tag(frag, 0, "p:grpSpPr")?;
                        let (gs, ge) = element_span(frag, gi, "p:grpSpPr")?;
                        let pr = &frag[gs..ge];
                        let xi = find_open_tag(pr, 0, "a:xfrm")?;
                        let (xs, xe) = element_span(pr, xi, "a:xfrm")?;
                        Some(parse_raw_xfrm(&pr[xs..xe]))
                    })
                    .map(|rx| compose_group(parent, &rx))
                    .unwrap_or(parent);
                // recurse with the group's CHILDREN (between its open and
                // close tags) — passing the whole element would re-find it
                let content_start = tag_end(tree, s).unwrap_or(e);
                let children = &tree[content_start..e.saturating_sub("</p:grpSp>".len())];
                parse_shape_tree(children, role, child_map, ctx, out, ph_defs);
            }
            "p:sp" => handle_sp(frag, parent, ctx, out, ph_defs),
            "p:pic" => handle_pic(frag, parent, ctx, out),
            "p:cxnSp" => handle_cxn(frag, parent, ctx, out),
            // p:graphicFrame: doughnut charts render; tables/SmartArt and
            // other chart types — Phase 3, skipped.
            "p:graphicFrame" => handle_graphic_frame(frag, parent, ctx, out),
            _ => {}
        }
    }
}

/// One `<p:sp>`: placeholder identity, geometry, fill/line/style, text.
/// Placeholders in layout/master parts become inheritance definitions
/// (`PhDef`); everything else renders as Shape + inline Text in that order.
#[allow(clippy::too_many_arguments)]
fn handle_sp(
    frag: &str,
    parent: XformMap,
    ctx: &WalkCtx,
    out: &mut Vec<SlideElement>,
    ph_defs: &mut Vec<PhDef>,
) {
    // ── identity: placeholder type / idx ──
    let ph_tag = find_open_tag(frag, 0, "p:ph").and_then(|i| tag_substr(&frag[i..], "<p:ph"));
    let ph_idx = ph_tag
        .as_deref()
        .and_then(|t| attr_value(t, "idx"))
        .and_then(|v| v.parse::<u32>().ok());
    // ECMA default placeholder type is "obj".
    let ph_type = ph_tag
        .as_deref()
        .and_then(|t| attr_value(t, "type"))
        .unwrap_or_else(|| "obj".into());
    let is_ph = ph_tag.is_some();

    // ── spPr: geometry, fill, line, preset ──
    let sp_pr: Option<&str> = find_open_tag(frag, 0, "p:spPr")
        .and_then(|i| element_span(frag, i, "p:spPr"))
        .map(|(s, e)| &frag[s..e]);
    let xfrm: Option<RawXfrm> = sp_pr.and_then(|spr| {
        let xi = find_open_tag(spr, 0, "a:xfrm")?;
        let (xs, xe) = element_span(spr, xi, "a:xfrm")?;
        Some(parse_raw_xfrm(&spr[xs..xe]))
    });
    let own_geo = xfrm
        .as_ref()
        .and_then(|x| x.rect())
        .map(|(x, y, w, h)| parent.apply_rect(x, y, w, h));
    let (rot, flip_h, flip_v) = xfrm
        .as_ref()
        .map(|x| (x.rot, x.flip_h, x.flip_v))
        .unwrap_or((0.0, false, false));
    let explicit_fill: Option<FillKind> = sp_pr.and_then(|s| parse_fill_el(s, ctx.theme, ctx.map));
    let explicit_line: LineChoice =
        sp_pr.map(|s| parse_ln_el(s, ctx.theme, ctx.map)).unwrap_or(LineChoice::Inherit);
    let prst = sp_pr
        .and_then(|spr| {
            let gi = find_open_tag(spr, 0, "a:prstGeom")?;
            let t = tag_substr(&spr[gi..], "<a:prstGeom")?;
            attr_value(&t, "prst")
        })
        .unwrap_or_else(|| "rect".into());
    // `<a:custGeom>` freeform geometry (world-map style artwork); None →
    // the preset path above applies instead.
    let freeform = sp_pr.and_then(parse_freeform_paths);
    let style = parse_pstyle(frag, ctx.theme, ctx.map);

    // ── master / layout: placeholders become definitions ──
    if ctx.role != PartRole::Slide && is_ph {
        let lst = find_open_tag(frag, 0, "a:lstStyle")
            .and_then(|i| element_span(frag, i, "a:lstStyle"))
            .map(|(s, e)| parse_lst_style(&frag[s..e], ctx.theme, ctx.map))
            .unwrap_or_default();
        // fill/line: explicit spPr wins, else this part's own p:style
        let fill = explicit_fill.or_else(|| style.as_ref().and_then(|s| s.fill.clone()));
        let line = match explicit_line {
            LineChoice::Spec(l) => Some(l),
            LineChoice::Hide => None,
            LineChoice::Inherit => style.as_ref().and_then(|s| s.line.clone()),
        };
        ph_defs.push(PhDef { ph_type, idx: ph_idx, geo: own_geo, fill, line, lst });
        return;
    }

    // ── resolve inheritance (slide placeholders only) ──
    let (layout_def, master_def) = if is_ph && ctx.role == PartRole::Slide {
        (find_ph(ctx.layout_ph, &ph_type, ph_idx), find_ph(ctx.master_ph, &ph_type, ph_idx))
    } else {
        (None, None)
    };
    let geo = own_geo
        .or_else(|| layout_def.and_then(|d| d.geo))
        .or_else(|| master_def.and_then(|d| d.geo));

    // Fill chain: own spPr explicit (incl. noFill stop) → layout ph →
    // master ph → own p:style fillRef.
    let fill: Option<FillKind> = explicit_fill
        .or_else(|| layout_def.and_then(|d| d.fill.clone()))
        .or_else(|| master_def.and_then(|d| d.fill.clone()))
        .or_else(|| style.as_ref().and_then(|s| s.fill.clone()));
    // Line chain: same precedence; explicit noFill hides the outline.
    let line: Option<LineSpec> = match explicit_line {
        LineChoice::Spec(l) => Some(l),
        LineChoice::Hide => None,
        LineChoice::Inherit => layout_def
            .and_then(|d| d.line.clone())
            .or_else(|| master_def.and_then(|d| d.line.clone()))
            .or_else(|| style.as_ref().and_then(|s| s.line.clone())),
    };

    // ── drawable box (only when something is actually painted) ──
    if let Some(g) = geo {
        let fill_visible = matches!(fill, Some(FillKind::Solid(..)) | Some(FillKind::Gradient(..)));
        if fill_visible || line.is_some() {
            out.push(SlideElement::Shape(DrawShape {
                x: g.0,
                y: g.1,
                w: g.2,
                h: g.3,
                prst: prst.clone(),
                fill,
                line,
                rot,
                flip_h,
                flip_v,
                freeform,
            }));
        }
    }

    // ── text: only the part's OWN txBody (master/layout prompts and "Click
    // to add…" instructions are placeholders, which returned above) ──
    if let Some(tx) = find_open_tag(frag, 0, "p:txBody")
        .and_then(|i| element_span(frag, i, "p:txBody"))
        .map(|(s, e)| &frag[s..e])
    {
        let kind = if is_ph { ph_kind_of(&ph_type) } else { PhKind::Other };
        let src = StyleSource {
            kind,
            centered_title: ph_type == "ctrTitle",
            layout_ph: layout_def,
            master_ph: master_def,
            tx: ctx.tx,
            font_ref_color: style.as_ref().and_then(|s| s.font_color),
            major_font: ctx.major_font,
            minor_font: ctx.minor_font,
            theme: ctx.theme,
            map: ctx.map,
        };
        let mut env = TxEnv::new(src);
        let (paras, anchor, insets, autofit) = parse_tx_body(tx, &mut env);
        let has_text = paras
            .iter()
            .any(|p| p.runs.iter().any(|r| !r.text.trim().is_empty()));
        if has_text {
            let g = geo.unwrap_or((0.0, 0.0, 0.0, 0.0)); // filled in later if absent
            let is_title = matches!(ph_type.as_str(), "title" | "ctrTitle");
            out.push(SlideElement::Text(SlideBox {
                x: g.0,
                y: g.1,
                w: g.2,
                h: g.3,
                text: plain_text(&paras),
                is_title,
                centered: paras.first().map(|p| p.algn == "ctr").unwrap_or(false),
                font_pt: paras.iter().flat_map(|p| p.runs.iter()).find_map(|r| r.sz_pt),
                has_xfrm: geo.is_some(),
                ph_type,
                color: paras.iter().flat_map(|p| p.runs.iter()).find_map(|r| r.color),
                paras: Some(paras),
                anchor,
                insets,
                autofit_scale: autofit,
            }));
        }
    }
}

/// Parse an `<a:lstStyle>` (`lvl1pPr`…`lvl9pPr`) into per-level defs.
fn parse_lst_style(frag: &str, theme: &ThemeColors, map: &ClrMap) -> Vec<TextStyleDef> {
    parse_levels(frag, theme, map)
}

/// Plain concatenated text of resolved paragraphs (for info cards/debug).
fn plain_text(paras: &[TextPara]) -> String {
    let mut s = String::new();
    for p in paras {
        for r in &p.runs {
            s.push_str(&r.text);
        }
        s.push('\n');
    }
    s.trim_end_matches('\n').to_string()
}

/// One `<p:pic>`: absolute rect, embedded image, `srcRect` crop, outline.
/// The blip + crop live in `<p:blipFill>`, a sibling of `<p:spPr>`, so the
/// whole element is searched.
fn handle_pic(frag: &str, parent: XformMap, ctx: &WalkCtx, out: &mut Vec<SlideElement>) {
    let sp_pr = find_open_tag(frag, 0, "p:spPr")
        .and_then(|i| element_span(frag, i, "p:spPr"))
        .map(|(s, e)| &frag[s..e]);
    let xfrm: Option<RawXfrm> = sp_pr.and_then(|spr| {
        let xi = find_open_tag(spr, 0, "a:xfrm")?;
        let (xs, xe) = element_span(spr, xi, "a:xfrm")?;
        Some(parse_raw_xfrm(&spr[xs..xe]))
    });
    let Some(x) = xfrm else { return };
    let Some((ox, oy, w, h)) = x.rect() else { return };
    let (px, py, pw, ph) = parent.apply_rect(ox, oy, w, h);
    if pw <= 0.0 || ph <= 0.0 {
        return;
    }

    let rid = find_open_tag(frag, 0, "a:blip")
        .and_then(|i| tag_substr(&frag[i..], "<a:blip"))
        .and_then(|t| attr_value(&t, "r:embed").or_else(|| attr_value(&t, "r:link")));
    let Some(path) = rid.and_then(|r| ctx.media.get(&r).cloned()) else { return };

    // <a:srcRect l="…" t="…" r="…" b="…" /> — fractions of 100000 cut off
    let mut crop = [0.0f64; 4];
    if let Some(i) = find_open_tag(frag, 0, "a:srcRect") {
        if let Some(t) = tag_substr(&frag[i..], "<a:srcRect") {
            for (attr, k) in [("l", 0usize), ("t", 1), ("r", 2), ("b", 3)] {
                if let Some(v) = attr_value(&t, attr).and_then(|v| v.parse::<f64>().ok()) {
                    crop[k] = (v / 100_000.0).clamp(0.0, 0.9);
                }
            }
        }
    }

    let line = match sp_pr.map(|s| parse_ln_el(s, ctx.theme, ctx.map)).unwrap_or(LineChoice::Inherit)
    {
        LineChoice::Spec(l) => Some(l),
        LineChoice::Hide => None,
        LineChoice::Inherit => parse_pstyle(frag, ctx.theme, ctx.map).and_then(|s| s.line),
    };

    out.push(SlideElement::Picture(DrawPicture {
        x: px,
        y: py,
        w: pw,
        h: ph,
        path,
        crop,
        line,
        rot: x.rot,
        flip_h: x.flip_h,
        flip_v: x.flip_v,
    }));
}

/// One `<p:cxnSp>` (connector): a stroked line between rect corners —
/// no fill, outline from `<a:ln>` or the style's `lnRef`.
fn handle_cxn(frag: &str, parent: XformMap, ctx: &WalkCtx, out: &mut Vec<SlideElement>) {
    let sp_pr = find_open_tag(frag, 0, "p:spPr")
        .and_then(|i| element_span(frag, i, "p:spPr"))
        .map(|(s, e)| &frag[s..e]);
    let xfrm: Option<RawXfrm> = sp_pr.and_then(|spr| {
        let xi = find_open_tag(spr, 0, "a:xfrm")?;
        let (xs, xe) = element_span(spr, xi, "a:xfrm")?;
        Some(parse_raw_xfrm(&spr[xs..xe]))
    });
    let Some(x) = xfrm else { return };
    let Some((ox, oy, w, h)) = x.rect() else { return };
    let (px, py, pw, ph) = parent.apply_rect(ox, oy, w, h);
    if pw <= 0.0 || ph <= 0.0 {
        return;
    }
    let prst = sp_pr
        .and_then(|spr| {
            let gi = find_open_tag(spr, 0, "a:prstGeom")?;
            let t = tag_substr(&spr[gi..], "<a:prstGeom")?;
            attr_value(&t, "prst")
        })
        .unwrap_or_else(|| "line".into());

    let style = parse_pstyle(frag, ctx.theme, ctx.map);
    let line = match sp_pr.map(|s| parse_ln_el(s, ctx.theme, ctx.map)).unwrap_or(LineChoice::Inherit)
    {
        LineChoice::Spec(l) => Some(l),
        LineChoice::Hide => return, // explicitly unfilled outline → invisible
        LineChoice::Inherit => style.and_then(|s| s.line),
    }
    .unwrap_or(LineSpec { color: (0.0, 0.0, 0.0), alpha: 1.0, width_emu: 12_700.0 });

    out.push(SlideElement::Shape(DrawShape {
        x: px,
        y: py,
        w: pw,
        h: ph,
        prst,
        fill: None,
        line: Some(line),
        rot: x.rot,
        flip_h: x.flip_h,
        flip_v: x.flip_v,
        freeform: None,
    }));
}

/// Parse a shape's `<a:custGeom><a:pathLst>` into freeform paths (the
/// world-map / artwork geometry PowerPoint uses for anything not preset).
/// Returns None when there's no custGeom — the caller keeps preset geom.
fn parse_freeform_paths(spr: &str) -> Option<Vec<FreeformPath>> {
    let ci = find_open_tag(spr, 0, "a:custGeom")?;
    let (cs, ce) = element_span(spr, ci, "a:custGeom")?;
    let geom = &spr[cs..ce];
    let pi = find_open_tag(geom, 0, "a:pathLst")?;
    let (ps, pe) = element_span(geom, pi, "a:pathLst")?;
    let lst = &geom[ps..pe];

    let mut paths = Vec::new();
    let mut ppos = 0usize;
    while let Some(i) = find_open_tag(lst, ppos, "a:path") {
        let Some((s, e)) = element_span(lst, i, "a:path") else { break };
        let open = tag_end(lst, i).map(|t| &lst[i..t]).unwrap_or("");
        ppos = e;
        let path_w = attr_value(open, "w").and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
        let path_h = attr_value(open, "h").and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
        let fill = attr_value(open, "fill").map(|v| v != "none").unwrap_or(true);

        let body = &lst[s..e];
        let mut cmds: Vec<PathCmd> = Vec::new();
        let mut cur = (0.0f64, 0.0f64); // path-space current point
        let mut start = (0.0f64, 0.0f64); // subpath start (for `close` + arcTo)
        let mut max_pt = 1.0f64;
        let mut pos = 0usize;
        loop {
            // Commands appear in document order; take whichever comes next.
            let next = ["a:moveTo", "a:lnTo", "a:cubicBezTo", "a:quadBezTo", "a:arcTo", "a:close"]
                .into_iter()
                .filter_map(|n| find_open_tag(body, pos, n).map(|idx| (idx, n)))
                .min_by_key(|(idx, _)| *idx);
            let Some((i, name)) = next else { break };
            let Some((fs, fe)) = element_span(body, i, name) else { break };
            pos = fe;
            let frag = &body[fs..fe];

            // collect this command's `<a:pt x y>` points in order
            let mut pts: Vec<(f64, f64)> = Vec::new();
            let mut qpos = 0usize;
            while let Some(qi) = find_open_tag(frag, qpos, "a:pt") {
                let Some((_, qe)) = element_span(frag, qi, "a:pt") else { break };
                qpos = qe;
                let t = tag_end(frag, qi).map(|t| &frag[qi..t]).unwrap_or("");
                if let (Some(x), Some(y)) = (
                    attr_value(t, "x").and_then(|v| v.parse::<f64>().ok()),
                    attr_value(t, "y").and_then(|v| v.parse::<f64>().ok()),
                ) {
                    max_pt = max_pt.max(x.abs()).max(y.abs());
                    pts.push((x, y));
                }
            }

            match name {
                "a:moveTo" => {
                    if let Some(&(x, y)) = pts.first() {
                        cmds.push(PathCmd::MoveTo(x, y));
                        cur = (x, y);
                        start = (x, y);
                    }
                }
                "a:lnTo" => {
                    if let Some(&(x, y)) = pts.first() {
                        cmds.push(PathCmd::LineTo(x, y));
                        cur = (x, y);
                    }
                }
                "a:cubicBezTo" if pts.len() >= 3 => {
                    cmds.push(PathCmd::CubicBezTo([
                        pts[0].0, pts[0].1, pts[1].0, pts[1].1, pts[2].0, pts[2].1,
                    ]));
                    cur = pts[2];
                }
                "a:quadBezTo" if pts.len() >= 2 => {
                    cmds.push(PathCmd::QuadBezTo([pts[0].0, pts[0].1, pts[1].0, pts[1].1]));
                    cur = pts[1];
                }
                "a:arcTo" => {
                    // Rare; approximate by the arc's true endpoint (angles are
                    // 1/60000°, radii path-space; ellipse center derives from
                    // the current point and stAng).
                    let num = |k: &str| {
                        attr_value(frag, k).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0)
                    };
                    let (wr, hr) = (num("wR"), num("hR"));
                    let st = (num("stAng") / 60_000.0).to_radians();
                    let sw = (num("swAng") / 60_000.0).to_radians();
                    let (ccx, ccy) = (cur.0 - wr * st.cos(), cur.1 - hr * st.sin());
                    let end = (ccx + wr * (st + sw).cos(), ccy + hr * (st + sw).sin());
                    cmds.push(PathCmd::LineTo(end.0, end.1));
                    max_pt = max_pt.max(end.0.abs()).max(end.1.abs());
                    cur = end;
                }
                "a:close" => {
                    cmds.push(PathCmd::Close);
                    cur = start;
                }
                _ => {}
            }
        }

        if !cmds.is_empty() {
            // Path coordinate space defaults to the extent of its own points
            // when `<a:path>` omits w/h.
            let (fw, fh) = if path_w > 0.0 && path_h > 0.0 {
                (path_w, path_h)
            } else {
                (max_pt, max_pt)
            };
            paths.push(FreeformPath { w: fw, h: fh, fill, cmds });
        }
    }
    (!paths.is_empty()).then_some(paths)
}

/// Parse a chart part's `c:doughnutChart`: data values, per-point colors,
/// hole size and start angle. Other chart types (bar/line/pie…) return None
/// — they stay Phase 3.
fn parse_doughnut_chart(xml: &str, theme: &ThemeColors, map: &ClrMap) -> Option<DoughnutSpec> {
    let di = find_open_tag(xml, 0, "c:doughnutChart")?;
    let (ds, de) = element_span(xml, di, "c:doughnutChart")?;
    let chart = &xml[ds..de];
    // Doughnut charts carry a single series.
    let si = find_open_tag(chart, 0, "c:ser")?;
    let (ss, se) = element_span(chart, si, "c:ser")?;
    let ser = &chart[ss..se];

    // ── values: `<c:val>…<c:pt idx="i"><c:v>65</c:v></c:pt>` ──
    let vi = find_open_tag(ser, 0, "c:val")?;
    let (vs, ve) = element_span(ser, vi, "c:val")?;
    let val = &ser[vs..ve];
    let mut points: Vec<(usize, f64)> = Vec::new();
    let mut vpos = 0usize;
    while let Some(qi) = find_open_tag(val, vpos, "c:pt") {
        let Some((qs, qe)) = element_span(val, qi, "c:pt") else { break };
        vpos = qe;
        let tg = tag_end(val, qi).map(|t| &val[qi..t]).unwrap_or("");
        let idx = attr_value(tg, "idx").and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(points.len());
        let vfrag = &val[qs..qe];
        if let Some(vi2) = find_open_tag(vfrag, 0, "c:v") {
            if let Some((vs2, ve2)) = element_span(vfrag, vi2, "c:v") {
                let start = tag_end(vfrag, vi2).unwrap_or(vs2);
                let end = ve2.saturating_sub("</c:v>".len()).max(start);
                if let Ok(f) = vfrag[start..end].trim().parse::<f64>() {
                    points.push((idx, f));
                }
            }
        }
    }
    let max_idx = points.iter().map(|(i, _)| *i).max()?;
    let mut values = vec![0.0f64; max_idx + 1];
    for (i, v) in points {
        values[i] = v;
    }
    if values.iter().all(|v| *v <= 0.0) {
        return None; // nothing drawable
    }

    // ── per-point colors: `<c:dPt><c:idx val="i"/><c:spPr>…` ──
    let mut colors: Vec<Option<(f64, f64, f64)>> = vec![None; values.len()];
    let mut dpos = 0usize;
    while let Some(di2) = find_open_tag(ser, dpos, "c:dPt") {
        let Some((ds2, de2)) = element_span(ser, di2, "c:dPt") else { break };
        dpos = de2;
        let dfrag = &ser[ds2..de2];
        let idx = find_open_tag(dfrag, 0, "c:idx")
            .and_then(|i| tag_substr(&dfrag[i..], "<c:idx"))
            .and_then(|t| attr_value(&t, "val"))
            .and_then(|v| v.parse::<usize>().ok());
        let Some(idx) = idx else { continue };
        if let Some(slot) = colors.get_mut(idx) {
            *slot = parse_color_el(dfrag, theme, map).map(|(c, _a)| c);
        }
    }

    // ── fallback palette ──
    // The series' own `<c:spPr>` (scoped so nested dPt fills don't leak in):
    // no explicit point colors → every point shares it; otherwise missing
    // points cycle the theme accents (varyColors) or fall back to the series
    // color, then accent1.
    let ser_fill = find_open_tag(ser, 0, "c:spPr")
        .and_then(|i| element_span(ser, i, "c:spPr"))
        .and_then(|(s, e)| parse_color_el(&ser[s..e], theme, map))
        .map(|(c, _a)| c);
    let vary = find_open_tag(chart, 0, "c:varyColors")
        .and_then(|i| tag_substr(&chart[i..], "<c:varyColors"))
        .and_then(|t| attr_value(&t, "val"))
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);
    let any_explicit = colors.iter().any(|c| c.is_some());
    for (i, slot) in colors.iter_mut().enumerate() {
        if slot.is_none() {
            let fallback = if !any_explicit || !vary {
                ser_fill
            } else {
                theme.color(&format!("accent{}", (i % 6) + 1)).or(ser_fill)
            };
            *slot = Some(
                fallback
                    .or_else(|| theme.color("accent1"))
                    .unwrap_or((0.6, 0.6, 0.6)),
            );
        }
    }

    let hole_pct = find_open_tag(chart, 0, "c:holeSize")
        .and_then(|i| tag_substr(&chart[i..], "<c:holeSize"))
        .and_then(|t| attr_value(&t, "val"))
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(50.0) // PowerPoint's default hole
        .clamp(1.0, 95.0);
    let first_ang_deg = find_open_tag(chart, 0, "c:firstSliceAng")
        .and_then(|i| tag_substr(&chart[i..], "<c:firstSliceAng"))
        .and_then(|t| attr_value(&t, "val"))
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(0.0)
        .rem_euclid(360.0);

    Some(DoughnutSpec { values, colors, hole_pct, first_ang_deg })
}

/// One `<p:graphicFrame>`: position via `<p:xfrm>` (graphic frames don't use
/// `<a:xfrm>`), content by `<c:chart r:id>` resolved through `ctx.charts`.
/// Tables/SmartArt/unknown chart types have no resolved spec → skipped.
fn handle_graphic_frame(
    frag: &str,
    parent: XformMap,
    ctx: &WalkCtx,
    out: &mut Vec<SlideElement>,
) {
    let rid = find_open_tag(frag, 0, "c:chart")
        .and_then(|i| tag_substr(&frag[i..], "<c:chart"))
        .and_then(|t| attr_value(&t, "r:id"));
    let Some(rid) = rid else { return };
    let Some(spec) = ctx.charts.get(&rid) else { return };

    // graphic frames position themselves with `<p:xfrm>` (not `<a:xfrm>`)
    let Some((xs, xe)) = find_open_tag(frag, 0, "p:xfrm")
        .and_then(|i| element_span(frag, i, "p:xfrm"))
    else {
        return;
    };
    let x = parse_raw_xfrm(&frag[xs..xe]);
    let Some((ox, oy, w, h)) = x.rect() else { return };
    let (px, py, pw, ph) = parent.apply_rect(ox, oy, w, h);
    if pw <= 0.0 || ph <= 0.0 {
        return;
    }
    out.push(SlideElement::Chart(DrawChart {
        x: px,
        y: py,
        w: pw,
        h: ph,
        doughnut: spec.clone(),
    }));
}

fn extract_pptx_media_from_dir<R: std::io::Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>, part_path: &str, media_cache: &Path,
    media_map: &mut std::collections::HashMap<String, PathBuf>,
) {
    // OOXML rels: {base_dir}/_rels/{file_name}.rels
    let base_dir = part_path.rsplit_once('/').map(|(b, _)| b).unwrap_or("ppt/slides");
    let file_name = part_path.rsplit_once('/').map(|(_, f)| f).unwrap_or(part_path);
    let rels_path = format!("{}/_rels/{}.rels", base_dir, file_name);
    if let Some(rels_xml) = read_zip_text(zip, &rels_path) {
        for rel in rels_xml.split("<Relationship") {
            if !rel.contains("image") { continue; }
            let rid = attr_value(rel, "Id").unwrap_or_default();
            let target = attr_value(rel, "Target").unwrap_or_default();
            let full = normalize_zip_rel(base_dir, &target);
            if let Ok(mut entry) = zip.by_name(&full) {
                let mut data = Vec::new();
                use std::io::Read;
                let read = entry.by_ref().take(16 * 1024 * 1024 + 1).read_to_end(&mut data);
                if read.is_ok() && !data.is_empty() && data.len() <= 16 * 1024 * 1024 {
                    let out = media_cache.join(format!("{}.bin", crate::md5::hex(full.as_bytes())));
                    if !out.exists() { let _ = crate::security::write_private(&out, &data); }
                    media_map.insert(rid, out);
                }
            }
        }
    }
}

/// On-disk media cache directory for a deck, keyed by (path, mtime) like
/// the slide PNG cache: editing the deck must invalidate extracted media
/// too, or a replaced picture keeps rendering from the old bytes forever.
/// Shared by pptx rel extraction and legacy .ppt BlipStore extraction.
fn media_cache_dir(doc: &Path) -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(format!(
            "spotty/pptx-media/{}-{}",
            crate::md5::hex(doc.to_string_lossy().as_bytes()),
            doc_mtime_secs(doc)
        ))
}

/// Extract one part's image relationships into the on-disk media cache and
/// return its rId → path map (per-part maps avoid rId collisions across
/// slide/layout/master).
fn extract_part_media<R: std::io::Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    doc: &Path,
    part_path: &str,
) -> std::collections::HashMap<String, PathBuf> {
    let media_cache = media_cache_dir(doc);
    let _ = crate::security::private_dir(&media_cache);
    let mut map = std::collections::HashMap::new();
    extract_pptx_media_from_dir(zip, part_path, &media_cache, &mut map);
    map
}

/// Resolve a relationship `Target` of the given type (`needle`, e.g.
/// `"slideLayout"`) from a part's `.rels` file, normalized into a zip path.
fn rel_target<R: std::io::Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    part: &str,
    needle: &str,
) -> Option<String> {
    let base = part.rsplit_once('/').map(|(b, _)| b).unwrap_or("");
    let file = part.rsplit_once('/').map(|(_, f)| f).unwrap_or(part);
    let xml = read_zip_text(zip, &format!("{}/_rels/{}.rels", base, file))?;
    let rel = xml.split("<Relationship").find(|r| r.contains(needle))?;
    let t = attr_value(rel, "Target")?;
    Some(normalize_zip_rel(base, &t))
}

/// Resolve a relationship by its exact `Id` from a part's `.rels` file,
/// normalized into a zip path (used for chart refs: `<c:chart r:id=…>`).
fn rel_target_by_id<R: std::io::Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    part: &str,
    rid: &str,
) -> Option<String> {
    let base = part.rsplit_once('/').map(|(b, _)| b).unwrap_or("");
    let file = part.rsplit_once('/').map(|(_, f)| f).unwrap_or(part);
    let xml = read_zip_text(zip, &format!("{}/_rels/{}.rels", base, file))?;
    let rel = xml
        .split("<Relationship")
        .find(|r| attr_value(r, "Id").as_deref() == Some(rid))?;
    let t = attr_value(rel, "Target")?;
    Some(normalize_zip_rel(base, &t))
}

/// Parse one slide end-to-end: theme + color map, master/layout composition
/// (background chain, decorative shapes, placeholder geometry/fill/text
/// styles), then the slide's own shape tree in document order.
fn parse_pptx_slide(doc: &Path, slide_no: usize) -> Option<SlideLayout> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(doc).ok()?).ok()?;

    // slide size from presentation.xml (fallback: 10″ × 7.5″ 4:3)
    let (slide_w, slide_h) = read_zip_text(&mut zip, "ppt/presentation.xml")
        .and_then(|p| parse_slide_size(&p))
        .unwrap_or((9_144_000.0, 6_858_000.0));
    if slide_w <= 0.0 || slide_h <= 0.0 {
        return None;
    }

    let theme = parse_theme_colors(&mut zip);

    // slide part: ppt/slides/slide{n}.xml in numeric order
    let mut slide_parts: Vec<(usize, String)> = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().map(|e| e.name().to_string()))
        .filter_map(|n| {
            let num = n.strip_prefix("ppt/slides/slide").and_then(|s| s.strip_suffix(".xml"))?;
            if !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()) {
                num.parse().ok().map(|v| (v, n))
            } else {
                None
            }
        })
        .collect();
    slide_parts.sort_by_key(|(n, _)| *n);
    let slide_part = slide_parts.get(slide_no.checked_sub(1)?)?.1.clone();
    let slide_xml = read_zip_text(&mut zip, &slide_part)?;

    // relationships: slide → layout → master
    let layout_path = rel_target(&mut zip, &slide_part, "slideLayout");
    let master_path = layout_path.as_ref().and_then(|p| rel_target(&mut zip, p, "slideMaster"));
    let layout_xml = layout_path.as_ref().and_then(|p| read_zip_text(&mut zip, p));
    let master_xml = master_path.as_ref().and_then(|p| read_zip_text(&mut zip, p));

    // ── color map: master's <p:clrMap>, overridable per part ──
    fn override_clr_map(xml: &str) -> Option<ClrMap> {
        // `<a:overrideClrMapping …/>` inside `<p:clrMapOvr>`; when the part
        // instead says `<a:masterClrMapping/>` this returns None → inherit.
        let i = find_open_tag(xml, 0, "a:overrideClrMapping")?;
        let t = tag_substr(&xml[i..], "<a:overrideClrMapping")?;
        Some(ClrMap::from_override(&t))
    }
    let mut clr_map = master_xml
        .as_deref()
        .map(ClrMap::from_master)
        .unwrap_or_else(ClrMap::default_map);
    if let Some(m) = layout_xml.as_deref().and_then(override_clr_map) {
        clr_map = m;
    }
    if let Some(m) = override_clr_map(&slide_xml) {
        clr_map = m;
    }

    let tx_styles = master_xml
        .as_deref()
        .map(|m| TxStyles::from_master(m, &theme, &clr_map))
        .unwrap_or_else(TxStyles::empty);

    // ── per-part media (rid → extracted path) ──
    let slide_media = extract_part_media(&mut zip, doc, &slide_part);
    let layout_media = layout_path
        .as_ref()
        .map(|p| extract_part_media(&mut zip, doc, p))
        .unwrap_or_default();
    let master_media = master_path
        .as_ref()
        .map(|p| extract_part_media(&mut zip, doc, p))
        .unwrap_or_default();

    // ── doughnut charts: pre-resolve every `<c:chart r:id>` on this slide.
    // The shape-tree walk only carries the slide XML (no zip access), so the
    // chart parts are parsed here and handed to the walk via `ctx.charts`.
    let mut charts: std::collections::HashMap<String, DoughnutSpec> =
        std::collections::HashMap::new();
    {
        let mut cpos = 0usize;
        while let Some(i) = find_open_tag(&slide_xml, cpos, "c:chart") {
            let Some(t) = tag_substr(&slide_xml[i..], "<c:chart") else { break };
            cpos = i + t.len();
            let Some(rid) = attr_value(&t, "r:id") else { continue };
            if charts.contains_key(&rid) {
                continue;
            }
            if let Some(target) = rel_target_by_id(&mut zip, &slide_part, &rid) {
                if let Some(cxml) = read_zip_text(&mut zip, &target) {
                    if let Some(spec) = parse_doughnut_chart(&cxml, &theme, &clr_map) {
                        log::info!(
                            "pptx: slide {} — doughnut chart {} ({} points, hole {}%)",
                            slide_no,
                            rid,
                            spec.values.len(),
                            spec.hole_pct as u32
                        );
                        charts.insert(rid, spec);
                    }
                }
            }
        }
    }

    // ── background: slide → layout → master (solid/gradient/picture/bgRef) ──
    let bg = parse_bg_el(&slide_xml, &slide_media, &theme, &clr_map)
        .or_else(|| {
            layout_xml.as_deref().and_then(|x| parse_bg_el(x, &layout_media, &theme, &clr_map))
        })
        .or_else(|| {
            master_xml.as_deref().and_then(|x| parse_bg_el(x, &master_media, &theme, &clr_map))
        });

    // ── composition: master (decor) → layout (decor + ph defs) → slide ──
    let mut elements = Vec::new();
    let mut master_ph: Vec<PhDef> = Vec::new();
    let mut layout_ph: Vec<PhDef> = Vec::new();

    let ctx_master = WalkCtx {
        role: PartRole::Master,
        theme: &theme,
        map: &clr_map,
        media: &master_media,
        tx: Some(&tx_styles),
        layout_ph: &[],
        master_ph: &[],
        major_font: &theme.major_font,
        minor_font: &theme.minor_font,
        charts: &charts,
    };
    let mut master_elems = Vec::new();
    if let Some(m) = master_xml.as_deref() {
        // master ph defs are collected even when the decor is hidden below —
        // the slide's placeholders still inherit from them
        parse_shape_tree(
            m,
            PartRole::Master,
            XformMap::ID,
            &ctx_master,
            &mut master_elems,
            &mut master_ph,
        );
    }

    // `showMasterSp="0"` on the layout (or slide) hides master decorations
    // (bio.pptx's layout1 sets it to hide the master behind its own logo).
    let show_master = layout_xml.as_deref().map(show_master_sp).unwrap_or(true)
        && show_master_sp(&slide_xml);
    if show_master {
        elements.append(&mut master_elems);
    }

    if let Some(l) = layout_xml.as_deref() {
        let ctx_layout = WalkCtx {
            role: PartRole::Layout,
            theme: &theme,
            map: &clr_map,
            media: &layout_media,
            tx: Some(&tx_styles),
            layout_ph: &[],
            master_ph: &master_ph,
            major_font: &theme.major_font,
            minor_font: &theme.minor_font,
            charts: &charts,
        };
        let mut layout_elems = Vec::new();
        parse_shape_tree(
            l,
            PartRole::Layout,
            XformMap::ID,
            &ctx_layout,
            &mut layout_elems,
            &mut layout_ph,
        );
        elements.append(&mut layout_elems);
    }

    {
        let ctx_slide = WalkCtx {
            role: PartRole::Slide,
            theme: &theme,
            map: &clr_map,
            media: &slide_media,
            tx: Some(&tx_styles),
            layout_ph: &layout_ph,
            master_ph: &master_ph,
            major_font: &theme.major_font,
            minor_font: &theme.minor_font,
            charts: &charts,
        };
        parse_shape_tree(
            &slide_xml,
            PartRole::Slide,
            XformMap::ID,
            &ctx_slide,
            &mut elements,
            &mut Vec::new(),
        );
    }

    // Fallback placement for text boxes that had no geometry anywhere.
    assign_default_geometry_from_elements(&mut elements, slide_w, slide_h);

    log::info!("pptx: slide {} — {} elements, bg={}", slide_no, elements.len(), bg.is_some());
    Some(SlideLayout { slide_w, slide_h, background: bg, elements })
}

/// `showMasterSp="0"` (searched wherever the part declares it — layout and
/// slide roots both carry it) hides master shapes and pictures.
fn show_master_sp(xml: &str) -> bool {
    !(xml.contains("showMasterSp=\"0\"") || xml.contains("showMasterSp=\"false\""))
}

fn assign_default_geometry_from_elements(elements: &mut [SlideElement], sw: f64, sh: f64) {
    let mx = sw * 0.06;
    let title_y = sh * 0.04;
    let title_h = sh * 0.18;
    let body_top = sh * 0.26;
    let body_h = sh * 0.66;
    let content_w = sw - mx * 2.0;
    let body_count = elements.iter().filter(|e| matches!(e, SlideElement::Text(b) if !b.has_xfrm && !(b.ph_type == "title" || b.ph_type == "ctrTitle"))).count().max(1);
    let mut body_idx = 0usize;
    for e in elements.iter_mut() {
        if let SlideElement::Text(b) = e {
            if b.has_xfrm { continue; }
            if b.is_title || b.ph_type == "title" || b.ph_type == "ctrTitle" {
                b.x = mx; b.y = title_y; b.w = content_w; b.h = title_h;
            } else if b.ph_type == "subTitle" {
                b.x = mx; b.y = sh * 0.50; b.w = content_w; b.h = sh * 0.20;
            } else {
                let col_w = content_w / body_count as f64;
                b.x = mx + col_w * body_idx as f64; b.y = body_top; b.w = col_w; b.h = body_h;
                body_idx += 1;
            }
        }
    }
}

/// Pull cx/cy from <p:sldSz .../> in presentation.xml.
fn parse_slide_size(xml: &str) -> Option<(f64, f64)> {
    let tag_start = xml.find("<p:sldSz")?;
    let rest = &xml[tag_start..];
    let gt = rest.find('>')?;
    let tag = &rest[..gt];
    let cx = attr_value(tag, "cx")?.parse::<f64>().ok()?;
    let cy = attr_value(tag, "cy")?.parse::<f64>().ok()?;
    if cx > 0.0 && cy > 0.0 {
        Some((cx, cy))
    } else {
        None
    }
}

/// Return the substring from `open` up to the next '>' (the full opening tag).
fn tag_substr(haystack: &str, open: &str) -> Option<String> {
    let start = haystack.find(open)?;
    let rest = &haystack[start..];
    let gt = rest.find('>')?;
    Some(rest[..gt].to_string())
}

/// Extract the value of an XML attribute like cx="123" from a tag substring.
fn attr_value(tag: &str, name: &str) -> Option<String> {
    let key = format!("{}=\"", name);
    let start = tag.find(&key)? + key.len();
    let end = tag[start..].find('"')? + start;
    Some(tag[start..end].to_string())
}

/// Render a parsed slide layout to PNG bytes: background, shapes (fills,
/// gradients, outlines, presets, rotation/flips), pictures (stretch +
/// srcRect crop), and Pango-laid-out text — all in the original z-order.
fn render_slide_layout(layout: &SlideLayout) -> Option<Vec<u8>> {
    use pangocairo::pango::prelude::*;
    // Context at 72 dpi: markup `size` (1/1024 units) then maps 1 unit = 1
    // canvas px, so sizes computed from pt·EMU·scale land exactly as written
    // regardless of the desktop's font DPI.
    let font_map = pangocairo::FontMap::new();
    let pango_ctx = font_map.create_context();
    pangocairo::functions::context_set_resolution(&pango_ctx, 72.0);

    // Uniform scale: long canvas side ≈1920 px, aspect from the slide size.
    let aspect = layout.slide_w / layout.slide_h;
    let (cw, ch) = if aspect >= 1.0 {
        (1920.0, 1920.0 / aspect)
    } else {
        (1920.0 * aspect, 1920.0)
    };
    let s = cw / layout.slide_w; // px per EMU (== ch / slide_h)
    let (surface, cr) = new_surface(cw.round() as i32, ch.round() as i32)?;

    paint_background(&cr, &layout.background, cw, ch);

    for elem in &layout.elements {
        match elem {
            SlideElement::Shape(sh) => draw_slide_shape(&cr, sh, s),
            SlideElement::Picture(p) => draw_slide_picture(&cr, p, s),
            SlideElement::Chart(c) => draw_slide_chart(&cr, c, s),
            SlideElement::Text(b) => draw_slide_text_box(&cr, b, s, &pango_ctx),
        }
    }

    let mut png = Vec::new();
    surface.write_to_png(&mut png).ok()?;
    if png.is_empty() {
        None
    } else {
        Some(png)
    }
}

/// Paint the slide background: white default, solid, linear gradient, or a
/// picture letterboxed (aspect-preserved, centered) on white.
fn paint_background(cr: &gtk::cairo::Context, bg: &Option<SlideBackground>, cw: f64, ch: f64) {
    // white base — also the letterbox color behind pictures
    cr.set_source_rgb(1.0, 1.0, 1.0);
    let _ = cr.paint();
    let Some(bg) = bg else { return };
    match bg {
        SlideBackground::Solid(r, g, b) => {
            cr.set_source_rgb(*r, *g, *b);
            let _ = cr.paint();
        }
        SlideBackground::Gradient(stops, angle) => {
            if let Some(grad) = linear_gradient(0.0, 0.0, cw, ch, stops, *angle) {
                let _ = cr.set_source(&grad);
                let _ = cr.paint();
            }
        }
        SlideBackground::Image(path) => {
            let Ok(bytes) = std::fs::read(path) else { return };
            let Ok(img) = image::load_from_memory(&bytes) else { return };
            let rgba = img.to_rgba8();
            let (iw, ih) = (rgba.width() as f64, rgba.height() as f64);
            if iw <= 0.0 || ih <= 0.0 {
                return;
            }
            let Some(surf) = cairo_image_from_rgba(&rgba, rgba.width(), rgba.height()) else {
                return;
            };
            let scale = (cw / iw).min(ch / ih);
            let (dw, dh) = (iw * scale, ih * scale);
            let (dx, dy) = ((cw - dw) / 2.0, (ch - dh) / 2.0);
            let _ = cr.save();
            cr.translate(dx, dy);
            cr.scale(scale, scale);
            let _ = cr.set_source_surface(&surf, 0.0, 0.0);
            let _ = cr.paint();
            let _ = cr.restore();
        }
    }
}

/// Linear gradient across a rect along `angle` (rad; 0 = left→right,
/// positive = clockwise — DrawingML and cairo share this convention).
/// The gradient line runs through the rect center, extended to cover all
/// corners at any angle.
fn linear_gradient(
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    stops: &[(f64, (f64, f64, f64), f64)],
    angle: f64,
) -> Option<gtk::cairo::LinearGradient> {
    if stops.is_empty() {
        return None;
    }
    let (sn, cs) = angle.sin_cos();
    let len = (w * cs.abs() + h * sn.abs()).max(1.0);
    let (cx, cyy) = (x + w / 2.0, y + h / 2.0);
    let grad = gtk::cairo::LinearGradient::new(
        cx - cs * len / 2.0,
        cyy - sn * len / 2.0,
        cx + cs * len / 2.0,
        cyy + sn * len / 2.0,
    );
    for (pos, c, a) in stops {
        grad.add_color_stop_rgba(*pos, c.0, c.1, c.2, *a);
    }
    Some(grad)
}

/// One shape: preset path, optional fill (solid/gradient), outline, with the
/// box's rotation/flips applied about its center. Everything — including
/// path construction and consumption — happens inside one save/restore.
fn draw_slide_shape(cr: &gtk::cairo::Context, sh: &DrawShape, s: f64) {
    let x = sh.x * s;
    let y = sh.y * s;
    let w = sh.w * s;
    let h = sh.h * s;
    if w <= 0.0 || h <= 0.0 {
        return;
    }

    let _ = cr.save();
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    if sh.rot != 0.0 || sh.flip_h || sh.flip_v {
        cr.translate(cx, cy);
        if sh.rot != 0.0 {
            cr.rotate(sh.rot);
        }
        if sh.flip_h || sh.flip_v {
            cr.scale(
                if sh.flip_h { -1.0 } else { 1.0 },
                if sh.flip_v { -1.0 } else { 1.0 },
            );
        }
        cr.translate(-cx, -cy);
    }

    // `<a:custGeom>` freeform: its own path tracing + fill/stroke rules
    // (the preset below would only ever produce a bounding rectangle).
    if let Some(paths) = &sh.freeform {
        draw_freeform_shape(cr, sh, paths, x, y, w, h, s);
        let _ = cr.restore();
        return;
    }

    build_shape_path(cr, &sh.prst, x, y, w, h, sh.flip_h, sh.flip_v);

    let mut filled = false;
    if let Some(fill) = &sh.fill {
        filled = match fill {
            FillKind::Solid(c, a) => {
                cr.set_source_rgba(c.0, c.1, c.2, *a);
                true
            }
            FillKind::Gradient(stops, ang) => match linear_gradient(x, y, w, h, stops, *ang) {
                Some(g) => {
                    let _ = cr.set_source(&g);
                    true
                }
                None => false,
            },
            FillKind::None => false,
        };
        if filled {
            // keep the path only when a stroke is about to consume it
            let _ = if sh.line.is_some() { cr.fill_preserve() } else { cr.fill() };
        }
    }
    if let Some(ln) = &sh.line {
        cr.set_source_rgba(ln.color.0, ln.color.1, ln.color.2, ln.alpha);
        cr.set_line_width((ln.width_emu * s).max(0.75));
        let _ = cr.stroke();
    } else if !filled {
        cr.new_path();
    }
    let _ = cr.restore();
}

/// A `<a:custGeom>` shape: fill only the subpaths marked fillable (cairo's
/// nonzero winding rule turns reversed inner contours into holes), then
/// stroke everything — same fill/outline precedence as `draw_slide_shape`.
fn draw_freeform_shape(
    cr: &gtk::cairo::Context,
    sh: &DrawShape,
    paths: &[FreeformPath],
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    s: f64,
) {
    let mut filled = false;
    if let Some(fill) = &sh.fill {
        cr.new_path();
        let mut traced_fillable = false;
        for p in paths.iter().filter(|p| p.fill) {
            trace_freeform_path(cr, p, x, y, w, h);
            traced_fillable = true;
        }
        if traced_fillable {
            filled = match fill {
                FillKind::Solid(c, a) => {
                    cr.set_source_rgba(c.0, c.1, c.2, *a);
                    true
                }
                FillKind::Gradient(stops, ang) => match linear_gradient(x, y, w, h, stops, *ang) {
                    Some(g) => {
                        let _ = cr.set_source(&g);
                        true
                    }
                    None => false,
                },
                FillKind::None => false,
            };
            if filled {
                // keep the path only when a stroke is about to consume it
                let _ = if sh.line.is_some() { cr.fill_preserve() } else { cr.fill() };
            }
        }
    }
    if let Some(ln) = &sh.line {
        if filled {
            // the fillable subpaths are already in the path — append the rest
            for p in paths.iter().filter(|p| !p.fill) {
                trace_freeform_path(cr, p, x, y, w, h);
            }
        } else {
            cr.new_path();
            for p in paths.iter() {
                trace_freeform_path(cr, p, x, y, w, h);
            }
        }
        cr.set_source_rgba(ln.color.0, ln.color.1, ln.color.2, ln.alpha);
        cr.set_line_width((ln.width_emu * s).max(0.75));
        let _ = cr.stroke();
    } else if !filled {
        cr.new_path();
    }
}

/// Trace one freeform subpath into the current path, mapping path-space
/// coordinates onto the shape rect (`pt` → `x + pt_x/path_w·w`).
fn trace_freeform_path(
    cr: &gtk::cairo::Context,
    p: &FreeformPath,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
) {
    let (pw, ph) = (p.w.max(1.0), p.h.max(1.0));
    let mx = |px: f64| x + px / pw * w;
    let my = |py: f64| y + py / ph * h;
    let mut cur = (0.0f64, 0.0f64); // path-space current point
    let mut start = (0.0f64, 0.0f64); // subpath start (for `close`)
    for cmd in &p.cmds {
        match *cmd {
            PathCmd::MoveTo(a, b) => {
                cr.move_to(mx(a), my(b));
                cur = (a, b);
                start = (a, b);
            }
            PathCmd::LineTo(a, b) => {
                cr.line_to(mx(a), my(b));
                cur = (a, b);
            }
            PathCmd::CubicBezTo(c) => {
                cr.curve_to(mx(c[0]), my(c[1]), mx(c[2]), my(c[3]), mx(c[4]), my(c[5]));
                cur = (c[4], c[5]);
            }
            PathCmd::QuadBezTo(q) => {
                // cairo has no quadratic — elevate to cubic (its control
                // points sit 2/3 of the way toward the quadratic control)
                let (sx, sy) = cur;
                let (c1x, c1y) = (sx + 2.0 * (q[0] - sx) / 3.0, sy + 2.0 * (q[1] - sy) / 3.0);
                let (c2x, c2y) =
                    (q[2] + 2.0 * (q[0] - q[2]) / 3.0, q[3] + 2.0 * (q[1] - q[3]) / 3.0);
                cr.curve_to(mx(c1x), my(c1y), mx(c2x), my(c2y), mx(q[2]), my(q[3]));
                cur = (q[2], q[3]);
            }
            PathCmd::Close => {
                cr.close_path();
                cur = start;
            }
        }
    }
}

/// A doughnut chart: ring segments stroked as butt-capped arcs.
/// `firstSliceAng` 0° = 12 o'clock, sweeping clockwise (y-down matches
/// cairo). A ~0.1° epsilon overdraw hides the antialiasing seams where
/// two segments meet.
fn draw_slide_chart(cr: &gtk::cairo::Context, ch: &DrawChart, s: f64) {
    let (x, y, w, h) = (ch.x * s, ch.y * s, ch.w * s, ch.h * s);
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let d = &ch.doughnut;
    let total: f64 = d.values.iter().filter(|v| **v > 0.0).sum();
    if total <= 0.0 {
        return;
    }
    let r_out = w.min(h) / 2.0; // inscribed in the chart's frame
    if r_out <= 0.5 {
        return;
    }
    let r_in = r_out * (d.hole_pct / 100.0).clamp(0.0, 0.95);
    let mid_r = (r_out + r_in) / 2.0;
    let thickness = (r_out - r_in).max(1.0);
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    const EPS: f64 = 0.002;
    let mut ang = (d.first_ang_deg - 90.0).to_radians();
    for (i, v) in d.values.iter().enumerate() {
        if *v <= 0.0 {
            continue;
        }
        let sweep = v / total * std::f64::consts::TAU;
        let color =
            d.colors.get(i).copied().flatten().unwrap_or((0.55, 0.55, 0.55));
        let _ = cr.save();
        cr.set_source_rgba(color.0, color.1, color.2, 1.0);
        cr.set_line_width(thickness);
        cr.new_path();
        cr.arc(cx, cy, mid_r, ang, ang + sweep + EPS);
        let _ = cr.stroke();
        let _ = cr.restore();
        ang += sweep;
    }
}

/// Trace the preset geometry into the current path (canvas px). Connectors
/// run corner-to-corner; flips choose the diagonal. Unknown presets fall
/// back to a rectangle.
fn build_shape_path(
    cr: &gtk::cairo::Context,
    prst: &str,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    flip_h: bool,
    flip_v: bool,
) {
    use std::f64::consts::{FRAC_PI_2, PI};
    cr.new_path();
    let poly = |pts: &[(f64, f64)]| {
        if let Some((first, rest)) = pts.split_first() {
            cr.move_to(first.0, first.1);
            for p in rest {
                cr.line_to(p.0, p.1);
            }
            cr.close_path();
        }
    };
    match prst {
        "ellipse" | "circle" => {
            // scale + arc, then restore the CTM before the fill: cairo bakes
            // path points to device space at add-time, so the ellipse stays put
            let (ccx, ccy) = (x + w / 2.0, y + h / 2.0);
            let _ = cr.save();
            cr.translate(ccx, ccy);
            cr.scale(w / 2.0, h / 2.0);
            cr.arc(0.0, 0.0, 1.0, 0.0, std::f64::consts::TAU);
            let _ = cr.restore();
        }
        "roundRect" => {
            let r = (w.min(h) / 6.0).min(w / 2.0).min(h / 2.0); // default adj 16667
            cr.move_to(x + r, y);
            cr.line_to(x + w - r, y);
            cr.arc(x + w - r, y + r, r, -FRAC_PI_2, 0.0);
            cr.line_to(x + w, y + h - r);
            cr.arc(x + w - r, y + h - r, r, 0.0, FRAC_PI_2);
            cr.line_to(x + r, y + h);
            cr.arc(x + r, y + h - r, r, FRAC_PI_2, PI);
            cr.line_to(x, y + r);
            cr.arc(x + r, y + r, r, PI, PI + FRAC_PI_2);
            cr.close_path();
        }
        "triangle" => poly(&[(x + w / 2.0, y), (x + w, y + h), (x, y + h)]),
        "rtTriangle" => poly(&[(x, y), (x, y + h), (x + w, y + h)]),
        "diamond" => poly(&[
            (x + w / 2.0, y),
            (x + w, y + h / 2.0),
            (x + w / 2.0, y + h),
            (x, y + h / 2.0),
        ]),
        "parallelogram" => poly(&[
            (x + 0.25 * w, y),
            (x + w, y),
            (x + 0.75 * w, y + h),
            (x, y + h),
        ]),
        "trapezoid" => poly(&[
            (x + 0.25 * w, y),
            (x + 0.75 * w, y),
            (x + w, y + h),
            (x, y + h),
        ]),
        "hexagon" => poly(&[
            (x + 0.25 * w, y),
            (x + 0.75 * w, y),
            (x + w, y + h / 2.0),
            (x + 0.75 * w, y + h),
            (x + 0.25 * w, y + h),
            (x, y + h / 2.0),
        ]),
        "pentagon" | "homePlate" => poly(&[
            (x + w / 2.0, y),
            (x + w, y + 0.382 * h),
            (x + 0.809 * w, y + h),
            (x + 0.191 * w, y + h),
            (x, y + 0.382 * h),
        ]),
        "chevron" => poly(&[
            (x, y),
            (x + 0.75 * w, y),
            (x + w, y + h / 2.0),
            (x + 0.75 * w, y + h),
            (x, y + h),
            (x + 0.25 * w, y + h / 2.0),
        ]),
        "star4" | "star5" | "star6" | "star8" => {
            let n = match prst {
                "star4" => 4.0,
                "star6" => 6.0,
                "star8" => 8.0,
                _ => 5.0,
            };
            let (mcx, mcy) = (x + w / 2.0, y + h / 2.0);
            let (rx, ry) = (w / 2.0, h / 2.0);
            let inner = 0.38;
            let total = (n as usize) * 2;
            for i in 0..total {
                let t = (i as f64) * PI / n - FRAC_PI_2;
                let f = if i % 2 == 0 { 1.0 } else { inner };
                let px = mcx + t.cos() * rx * f;
                let py = mcy + t.sin() * ry * f;
                if i == 0 {
                    cr.move_to(px, py);
                } else {
                    cr.line_to(px, py);
                }
            }
            cr.close_path();
        }
        // connectors + plain lines: corner-to-corner diagonal
        "line" | "straightConnector1" | "bentConnector2" | "bentConnector3"
        | "bentConnector4" | "bentConnector5" => {
            if flip_h != flip_v {
                cr.move_to(x + w, y);
                cr.line_to(x, y + h);
            } else {
                cr.move_to(x, y);
                cr.line_to(x + w, y + h);
            }
        }
        _ => cr.rectangle(x, y, w, h), // rect, custGeom fallback, …
    }
}

/// One picture: the `srcRect` sub-rectangle is mapped onto the destination
/// rect (OOXML stretches, it does not letterbox), then rotation/flips apply.
fn draw_slide_picture(cr: &gtk::cairo::Context, pic: &DrawPicture, s: f64) {
    use gtk::cairo;
    let x = pic.x * s;
    let y = pic.y * s;
    let w = pic.w * s;
    let h = pic.h * s;
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let Ok(bytes) = std::fs::read(&pic.path) else { return };
    let Ok(img) = image::load_from_memory(&bytes) else { return };
    let rgba = img.to_rgba8();
    let (iw, ih) = (rgba.width() as f64, rgba.height() as f64);
    if iw <= 0.0 || ih <= 0.0 {
        return;
    }
    let Some(surf) = cairo_image_from_rgba(&rgba, rgba.width(), rgba.height()) else { return };

    // source sub-rect in px (crop = fractions cut from each edge)
    let (cl, ct, cr_, cb) = (
        pic.crop[0].clamp(0.0, 0.9),
        pic.crop[1].clamp(0.0, 0.9),
        pic.crop[2].clamp(0.0, 0.9),
        pic.crop[3].clamp(0.0, 0.9),
    );
    if 1.0 - cl - cr_ <= 0.0 || 1.0 - ct - cb <= 0.0 {
        return;
    }
    let (sx0, sy0) = (cl * iw, ct * ih);
    let (sw, shp) = ((1.0 - cl - cr_) * iw, (1.0 - ct - cb) * ih);

    let _ = cr.save();
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    if pic.rot != 0.0 || pic.flip_h || pic.flip_v {
        cr.translate(cx, cy);
        if pic.rot != 0.0 {
            cr.rotate(pic.rot);
        }
        if pic.flip_h || pic.flip_v {
            cr.scale(
                if pic.flip_h { -1.0 } else { 1.0 },
                if pic.flip_v { -1.0 } else { 1.0 },
            );
        }
        cr.translate(-cx, -cy);
    }
    // map source sub-rect → dest rect (stretch); set_source_surface under
    // this CTM places source px (sx0, sy0) exactly at (x, y)
    let m = cairo::Matrix::new(
        w / sw,
        0.0,
        0.0,
        h / shp,
        x - sx0 * (w / sw),
        y - sy0 * (h / shp),
    );
    cr.transform(m);
    let _ = cr.set_source_surface(&surf, 0.0, 0.0);
    let _ = cr.paint();
    let _ = cr.restore();

    // optional outline (same rotation/flip box)
    if let Some(ln) = &pic.line {
        let _ = cr.save();
        if pic.rot != 0.0 || pic.flip_h || pic.flip_v {
            cr.translate(cx, cy);
            if pic.rot != 0.0 {
                cr.rotate(pic.rot);
            }
            if pic.flip_h || pic.flip_v {
                cr.scale(
                    if pic.flip_h { -1.0 } else { 1.0 },
                    if pic.flip_v { -1.0 } else { 1.0 },
                );
            }
            cr.translate(-cx, -cy);
        }
        cr.rectangle(x, y, w, h);
        cr.set_source_rgba(ln.color.0, ln.color.1, ln.color.2, ln.alpha);
        cr.set_line_width((ln.width_emu * s).max(0.75));
        let _ = cr.stroke();
        let _ = cr.restore();
    }
}

/// One text box, drawn with Pango: one `Layout` per paragraph stacked by
/// measured height, per-run markup (size/weight/slant/underline/color/face),
/// vertical anchor + insets honored, light text haloed for contrast. Boxes
/// without structured `paras` (legacy .ppt) are synthesized into one run.
fn draw_slide_text_box(
    cr: &gtk::cairo::Context,
    b: &SlideBox,
    s: f64,
    pctx: &pangocairo::pango::Context,
) {
    use pangocairo::pango;

    // legacy .ppt boxes carry plain text only — wrap them in one paragraph
    let legacy;
    let paras: &[TextPara] = match &b.paras {
        Some(p) => p,
        None => {
            legacy = vec![TextPara {
                runs: vec![TextRun {
                    text: b.text.clone(),
                    sz_pt: b.font_pt,
                    bold: b.is_title,
                    italic: false,
                    underline: false,
                    color: b.color,
                    font: Some("Sans".into()),
                }],
                algn: if b.centered { "ctr".into() } else { "l".into() },
                lvl: 0,
                bullet: None,
                spc_bef_pt: 0.0,
                spc_aft_pt: 0.0,
                follow: false,
                mar_l_emu: 0.0,
                hang_emu: 0.0,
            }];
            &legacy
        }
    };
    if paras.is_empty() {
        return;
    }

    let bx = b.x * s;
    let by = b.y * s;
    let bw = b.w * s;
    let bh = b.h * s;
    if bw <= 1.0 {
        return;
    }
    let insets = [
        b.insets[0] * s,
        b.insets[1] * s,
        b.insets[2] * s,
        b.insets[3] * s,
    ];
    let right_edge = bx + bw - insets[2];

    struct Piece {
        layout: pangocairo::pango::Layout,
        x: f64,
        h: f64,
        gap_before: f64,
        gap_after: f64,
        color: (f64, f64, f64),
        light: bool,
    }
    let mut pieces: Vec<Piece> = Vec::new();

    for para in paras {
        if pieces.len() > 48 {
            break; // pathological decks: hard cap
        }
        let marker = if para.follow { String::new() } else { bullet_marker(para) };

        let mut markup = String::new();
        let mut first = true;
        for run in &para.runs {
            let text = if first && !marker.is_empty() {
                format!("{}{}", marker, run.text)
            } else {
                run.text.clone()
            };
            first = false;
            markup.push_str(&run_markup(run, &text, b.autofit_scale, s));
        }
        if para.runs.is_empty() {
            // blank paragraph: keep one line of vertical space
            markup = format!(
                "<span size=\"{}\"> </span>",
                default_size_units(b, s)
            );
        }

        // Text column: box left inset + marL (level/paragraph). The bullet
        // marker hangs into the margin so wrapped lines land exactly under
        // the run text: the first line starts marker-width left of the
        // column, and Pango's negative indent brings subsequent lines back
        // to it (Pango applies negative indents to non-first lines).
        let mar_px = para.mar_l_emu * s;
        let x_col = bx + insets[0] + mar_px;
        let mut marker_w = 0.0;
        if !marker.is_empty() && !para.runs.is_empty() && para.hang_emu > 0.0 && para.algn != "ctr"
        {
            let probe = pangocairo::pango::Layout::new(pctx);
            let probe_markup = run_markup(&para.runs[0], &marker, b.autofit_scale, s);
            if let Ok((attrs, text, _)) = pango::parse_markup(&probe_markup, '\u{0}') {
                probe.set_attributes(Some(&attrs));
                probe.set_text(&text);
                marker_w = probe.pixel_size().0 as f64;
            }
        }
        let hang_px = marker_w.min(mar_px);
        let x_first = x_col - hang_px;
        let width_px = (right_edge - x_first).max(8.0);

        let layout = pangocairo::pango::Layout::new(pctx);
        layout.set_width((width_px * 1024.0) as i32);
        layout.set_wrap(pango::WrapMode::WordChar);
        layout.set_alignment(match para.algn.as_str() {
            "ctr" => pango::Alignment::Center,
            "r" => pango::Alignment::Right,
            _ => pango::Alignment::Left,
        });
        if hang_px > 0.0 {
            layout.set_indent((-(hang_px * 1024.0)) as i32);
        }

        match pango::parse_markup(&markup, '\u{0}') {
            Ok((attrs, text, _)) => {
                layout.set_attributes(Some(&attrs));
                layout.set_text(&text);
            }
            Err(_) => layout.set_text(&strip_markup_lossy(&markup)),
        }

        let (_, ph) = layout.pixel_size();
        let color = para
            .runs
            .iter()
            .find_map(|r| r.color)
            .or(b.color)
            .unwrap_or((0.10, 0.10, 0.14));
        let luma = 0.2126 * color.0 + 0.7152 * color.1 + 0.0722 * color.2;
        pieces.push(Piece {
            layout,
            x: x_first,
            h: ph as f64,
            gap_before: if pieces.is_empty() || para.follow {
                0.0
            } else {
                para.spc_bef_pt * 12_700.0 * s
            },
            gap_after: para.spc_aft_pt * 12_700.0 * s,
            color,
            light: luma > 0.55,
        });
    }
    if pieces.is_empty() {
        return;
    }

    // vertical anchor over the inset box
    let total: f64 = pieces.iter().map(|p| p.gap_before + p.h + p.gap_after).sum();
    let top_limit = by + insets[1];
    let bottom_limit = by + bh - insets[3];
    let mut y = match b.anchor {
        1 => ((top_limit + bottom_limit) / 2.0 - total / 2.0).max(top_limit),
        2 => (bottom_limit - total).max(top_limit),
        _ => top_limit,
    };

    for piece in &pieces {
        pangocairo::functions::update_layout(cr, &piece.layout);
        y += piece.gap_before;
        if piece.light {
            // soft dark halo so light text survives photos and gradients
            for (ox, oy) in [
                (-1.5, 0.0),
                (1.5, 0.0),
                (0.0, -1.5),
                (0.0, 1.5),
                (-1.0, -1.0),
                (1.0, 1.0),
                (-1.0, 1.0),
                (1.0, -1.0),
            ] {
                cr.set_source_rgba(0.0, 0.0, 0.0, 0.55);
                cr.move_to(piece.x + ox, y + oy);
                pangocairo::functions::show_layout(cr, &piece.layout);
            }
        }
        cr.set_source_rgba(piece.color.0, piece.color.1, piece.color.2, 1.0);
        cr.move_to(piece.x, y);
        pangocairo::functions::show_layout(cr, &piece.layout);
        y += piece.h + piece.gap_after;
    }
}

/// One run as Pango markup. At 72 dpi context resolution, markup `size`
/// (1/1024) behaves as canvas px: size = pt · 12700 · (px/EMU) · 1024.
fn run_markup(run: &TextRun, text: &str, autofit: f64, s: f64) -> String {
    let esc = |t: &str| t.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let pt = run.sz_pt.unwrap_or(18.0) * autofit;
    let px = (pt * 12_700.0 * s).max(1.0);
    let mut attrs = format!(" size=\"{}\"", (px * 1024.0).round() as i64);
    if run.bold {
        attrs.push_str(" weight=\"bold\"");
    }
    if run.italic {
        attrs.push_str(" style=\"italic\"");
    }
    if run.underline {
        attrs.push_str(" underline=\"single\"");
    }
    if let Some(c) = run.color {
        let ch = |v: f64| (v * 255.0).round().clamp(0.0, 255.0) as u8;
        attrs.push_str(&format!(" foreground=\"#{:02x}{:02x}{:02x}\"", ch(c.0), ch(c.1), ch(c.2)));
    }
    if let Some(f) = run.font.as_deref().filter(|f| !f.is_empty()) {
        attrs.push_str(&format!(" face=\"{}\"", esc(f)));
    }
    format!("<span{}>{}</span>", attrs, esc(text))
}

/// Markup size for an empty paragraph (so blank lines keep line height).
fn default_size_units(b: &SlideBox, s: f64) -> i64 {
    let pt = b.font_pt.unwrap_or(18.0) * b.autofit_scale;
    ((pt * 12_700.0 * s).max(1.0) * 1024.0).round() as i64
}

/// Bullet marker text for a paragraph's first line (char + nbsp).
fn bullet_marker(para: &TextPara) -> String {
    match &para.bullet {
        None | Some(ParaBullet::Off) => String::new(),
        Some(ParaBullet::Char(c)) => format!("{}\u{a0}", c),
        Some(ParaBullet::Number(kind, n)) => {
            let n = (*n).max(1);
            let label = match kind.as_str() {
                "alphaLcPeriod" => format!("{}.", (b'a' + ((n - 1) % 26) as u8) as char),
                "alphaUcPeriod" => format!("{}.", (b'A' + ((n - 1) % 26) as u8) as char),
                _ => format!("{}.", n),
            };
            format!("{}\u{a0}", label)
        }
    }
}

/// Strip Pango markup when parsing fails: drop tags, decode the entities.
fn strip_markup_lossy(markup: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for ch in markup.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    decode_xml_entities(&out)
}

/// Convert an `image::RgbaImage` to a Cairo ImageSurface.
/// Writes the image as PNG to a temp buffer, then loads it via Cairo.
pub(crate) fn cairo_image_from_rgba(rgba: &image::RgbaImage, _w: u32, _h: u32) -> Option<gtk::cairo::ImageSurface> {
    use gtk::cairo;
    let dyn_img = image::DynamicImage::ImageRgba8(rgba.clone());
    let mut png_buf = std::io::Cursor::new(Vec::new());
    dyn_img.write_to(&mut png_buf, image::ImageFormat::Png).ok()?;
    png_buf.set_position(0);
    cairo::ImageSurface::create_from_png(&mut png_buf).ok()
}

/// Greedy word-wrap for Cairo text within a pixel width.
pub(crate) fn wrap_text(cr: &gtk::cairo::Context, text: &str, max_w: f64) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let trial = if cur.is_empty() {
            word.to_string()
        } else {
            format!("{} {}", cur, word)
        };
        let w = cr.text_extents(&trial).map(|e| e.width()).unwrap_or(0.0);
        if w > max_w && !cur.is_empty() {
            lines.push(cur.clone());
            cur = word.to_string();
        } else {
            cur = trial;
        }
        if lines.len() >= 12 {
            break;
        } // cap per box
    }
    if !cur.is_empty() && lines.len() < 12 {
        lines.push(cur);
    }
    lines
}

/// Format a duration in milliseconds as "m:ss".
fn fmt_dur(ms: u64) -> String {
    let secs = ms / 1000;
    format!("{}:{:02}", secs / 60, secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn td(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("spotty_pv_{}_{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn text_preview_ascii() {
        let d = td("ascii");
        let p = d.join("hello.txt");
        fs::write(&p, b"Hello world!").unwrap();
        let tp = text_preview_for(&p).unwrap();
        assert_eq!(tp.text, "Hello world!");
        assert!(!tp.truncated);
        assert_eq!(tp.total_size, 12);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn text_preview_empty() {
        let d = td("empty");
        let p = d.join("e.txt");
        fs::write(&p, b"").unwrap();
        let tp = text_preview_for(&p).unwrap();
        assert!(tp.text.is_empty());
        assert!(!tp.truncated);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn text_preview_binary() {
        let d = td("binary");
        let p = d.join("bin.dat");
        fs::write(&p, vec![0u8; 1000]).unwrap();
        assert!(text_preview_for(&p).is_none());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn text_preview_truncation_flag() {
        let d = td("trunc");
        let p = d.join("big.txt");
        fs::write(&p, "x".repeat(1_100_000)).unwrap();
        let tp = text_preview_for(&p).unwrap();
        assert!(tp.truncated);
        assert!(tp.text.len() <= 1_048_576); // <= TEXT_PREVIEW_READ_BYTES
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn text_preview_crlf() {
        let d = td("crlf");
        let p = d.join("crlf.txt");
        fs::write(&p, "line1
line2
").unwrap();
        let tp = text_preview_for(&p).unwrap();
        assert_eq!(tp.text, "line1\nline2\n");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn text_preview_multibyte() {
        let d = td("utf8");
        let p = d.join("uni.txt");
        fs::write(&p, "Hello café 你好世界").unwrap();
        let tp = text_preview_for(&p).unwrap();
        assert_eq!(tp.text, "Hello café 你好世界");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn archive_listing_zip() {
        let d = td("zip");
        let p = d.join("test.zip");
        {
            let f = fs::File::create(&p).unwrap();
            let mut w = zip::ZipWriter::new(f);
            w.start_file("a.txt", zip::write::FileOptions::default()).unwrap();
            w.write_all(b"content a").unwrap();
            w.start_file("b.txt", zip::write::FileOptions::default()).unwrap();
            w.write_all(b"content bb").unwrap();
            w.finish().unwrap();
        }
        let (entries, total) = archive_listing(&p).unwrap();
        assert_eq!(total, 2);
        assert_eq!(entries.len(), 2);
        assert!(entries[0].contains("a.txt"));
        assert!(entries[1].contains("b.txt"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn cache_insert_and_lookup() {
        let d = td("cache");
        let p = d.join("data.txt");
        fs::write(&p, b"hello cache").unwrap();

        let payload = PreviewPayload::Text {
            content: "hello cache".to_string(),
        };
        cache_insert(p.clone(), payload.clone());

        let hit = cache_lookup(&p);
        assert!(hit.is_some(), "cache lookup should hit");
        if let Some(PreviewPayload::Text { content }) = hit {
            assert_eq!(content, "hello cache");
        } else {
            panic!("expected Text payload");
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn cache_stale_on_mtime_change() {
        let d = td("cache_stale");
        let p = d.join("data.txt");
        fs::write(&p, b"version1").unwrap();

        let payload = PreviewPayload::Text {
            content: "old".to_string(),
        };
        cache_insert(p.clone(), payload);

        // Rewrite the file to change mtime.
        std::thread::sleep(Duration::from_millis(1100));
        fs::write(&p, b"version2").unwrap();

        let hit = cache_lookup(&p);
        assert!(hit.is_none(), "stale cache entry should be evicted");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn cache_eviction_bound() {
        let d = td("cache_evict");
        // Insert more than PREVIEW_CACHE_CAP entries.
        for i in 0..PREVIEW_CACHE_CAP + 5 {
            let p = d.join(format!("file_{}.txt", i));
            fs::write(&p, format!("content {}", i)).unwrap();
            cache_insert(
                p,
                PreviewPayload::Text {
                    content: format!("content {}", i),
                },
            );
        }
        let cache = preview_cache().lock().unwrap();
        assert!(
            cache.len() <= PREVIEW_CACHE_CAP,
            "cache should not exceed cap, got {}",
            cache.len()
        );
        drop(cache);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn pdf_page_count_parsing() {
        // Test the parsing logic from pdfinfo output.
        let output = "Title:          test.pdf\nPages:          42\nPage size:      612 x 792 pts\n";
        let mut pages = None;
        for line in output.lines() {
            if let Some(rest) = line.strip_prefix("Pages:") {
                if let Ok(n) = rest.trim().parse::<usize>() {
                    pages = Some(n);
                }
            }
        }
        assert_eq!(pages, Some(42));
    }

    #[test]
    fn pptx_slide_count_from_zip() {
        let d = td("pptx_count");
        let p = d.join("test.pptx");
        {
            let f = fs::File::create(&p).unwrap();
            let mut w = zip::ZipWriter::new(f);
            w.start_file(
                "ppt/slides/slide1.xml",
                zip::write::FileOptions::default(),
            )
            .unwrap();
            w.write_all(b"<p:sld/>").unwrap();
            w.start_file(
                "ppt/slides/slide2.xml",
                zip::write::FileOptions::default(),
            )
            .unwrap();
            w.write_all(b"<p:sld/>").unwrap();
            w.start_file(
                "ppt/slides/slide3.xml",
                zip::write::FileOptions::default(),
            )
            .unwrap();
            w.write_all(b"<p:sld/>").unwrap();
            // Non-slide entry should not be counted.
            w.start_file(
                "ppt/presentation.xml",
                zip::write::FileOptions::default(),
            )
            .unwrap();
            w.write_all(b"<p:presentation/>").unwrap();
            w.finish().unwrap();
        }
        let count = pptx_slide_count(&p);
        assert_eq!(count, Some(3));
        let _ = fs::remove_dir_all(&d);
    }

    // ── Legacy binary .ppt: raw record builder for synthetic streams ──

    /// One raw PPT record: info (version in the low nibble), type, LE u32
    /// length, payload.
    fn ppt_rec(ver: u8, rec_type: u16, payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&(ver as u16).to_le_bytes());
        v.extend_from_slice(&rec_type.to_le_bytes());
        v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        v.extend_from_slice(payload);
        v
    }

    fn ppt_utf16(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
    }

    /// Record with a nonzero instance field (escher carries shape type /
    /// list kind there): info = (inst << 4) | ver, then type, len, payload.
    fn ppt_rec_inst(ver: u16, inst: u16, rec_type: u16, payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&((inst << 4) | ver).to_le_bytes());
        v.extend_from_slice(&rec_type.to_le_bytes());
        v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        v.extend_from_slice(payload);
        v
    }

    /// A 2×2 red PNG — the one blip payload of the stage-2 test decks.
    fn test_png() -> Vec<u8> {
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            2,
            2,
            image::Rgba([255u8, 0, 0, 255]),
        ))
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
        png
    }

    #[test]
    fn legacy_ppt_slide_texts_enumerates_every_slide() {
        // Document container wrapping three slide containers (record 1006),
        // each carrying one TextCharsAtom (record 4000), plus a junk record
        // that must be skipped.
        let slides_rec = [
            ppt_rec(0x0f, 1006, &ppt_rec(0, 4000, &ppt_utf16("Opening slide words"))),
            ppt_rec(0, 9, b"junk"),
            ppt_rec(0x0f, 1006, &ppt_rec(0, 4000, &ppt_utf16("Middle slide points"))),
            ppt_rec(0x0f, 1006, &ppt_rec(0, 4000, &ppt_utf16("Closing slide summary"))),
        ]
        .concat();
        let stream = ppt_rec(0x0f, 1000, &slides_rec);

        let slides = legacy_ppt_slide_texts(&stream);
        assert_eq!(slides.len(), 3, "one entry per slide record, in order");
        assert_eq!(slides[0], vec!["Opening slide words".to_string()]);
        assert_eq!(slides[1], vec!["Middle slide points".to_string()]);
        assert_eq!(slides[2], vec!["Closing slide summary".to_string()]);
    }

    #[test]
    fn legacy_ppt_slide_texts_falls_back_to_single_blob() {
        // No slide records at all: whole-stream atoms → a single slide
        // (the old first-slide-only behaviour for unusual files).
        let stream = ppt_rec(0, 4000, &ppt_utf16("Just some deck text"));
        let slides = legacy_ppt_slide_texts(&stream);
        assert_eq!(slides.len(), 1);
        assert!(!slides[0].is_empty());
    }

    #[test]
    fn legacy_ppt_count_and_per_slide_render() {
        let d = td("legacy_ppt");
        let p = d.join("deck.ppt");
        {
            let mut comp = cfb::create(&p).unwrap();
            let inner = [
                ppt_rec(0x0f, 1006, &ppt_rec(0, 4000, &ppt_utf16("First slide title"))),
                ppt_rec(0x0f, 1006, &ppt_rec(0, 4000, &ppt_utf16("Second slide body"))),
                ppt_rec(0x0f, 1006, &ppt_rec(0, 4000, &ppt_utf16("Third slide closing"))),
            ]
            .concat();
            let mut stream = comp.create_stream("/PowerPoint Document").unwrap();
            stream.write_all(&ppt_rec(0x0f, 1000, &inner)).unwrap();
            stream.flush().unwrap();
            drop(stream);
            comp.flush().unwrap();
        }

        assert_eq!(legacy_ppt_slide_count(&p), Some(3));
        let texts = legacy_ppt_slide_texts(&legacy_ppt_stream(&p).unwrap());
        assert_eq!(texts.len(), 3);
        assert_eq!(texts[1], vec!["Second slide body".to_string()]);

        // The shared page resolver renders slide 2 on demand, sane-checks
        // it, and caches it: a second resolve hits the same file.
        let png2 = resolve_page_png(&p, 2).expect("legacy slide 2 resolves");
        assert!(sane_png_file(&png2));
        assert_eq!(resolve_page_png(&p, 2).as_deref(), Some(png2.as_path()));

        // The cached file must keep its .png extension AND decode through
        // image::open — that's exactly how the preview pane loads slides.
        // An extension-less cache file (the pre-fix bug) made image::open
        // fail with Format(Unknown), so .ppt previews fell back to the
        // generic info card while the in-memory QA render still passed.
        assert_eq!(
            png2.extension().and_then(|e| e.to_str()),
            Some("png"),
            "legacy slide cache path must keep its .png extension"
        );
        assert!(
            image::open(&png2).is_ok(),
            "cached slide must decode via image::open"
        );

        // The pane path: compute_preview reports a real image with the nav
        // total instead of the generic info card.
        match compute_preview(&p) {
            PreviewPayload::Image { page, total_pages, .. } => {
                assert_eq!(page, Some(1));
                assert_eq!(total_pages, Some(3));
            }
            _ => panic!("legacy .ppt must preview as an image with page nav"),
        }
        let png1 = legacy_ppt_cache_path(&p, 1);
        assert!(sane_png_file(&png1));
        assert!(image::open(&png1).is_ok());

        for slide in 1..=3 {
            let _ = fs::remove_file(legacy_ppt_cache_path(&p, slide));
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn ppt_8bit_text_decodes_cp1252() {
        // TextBytesAtom (record 4008) carries the document's ANSI code page
        // — CP1252 for Western decks. Regression: the old decoder blanked
        // every byte ≥ 0x7F to a space ("S ntese" instead of "Síntese").
        assert_eq!(decode_ppt_8bit_text(b"S\xEDntese"), "Síntese");
        assert_eq!(decode_ppt_8bit_text(b"redu\xE7\xE3o"), "redução");
        // CP1252 specials above Latin-1: smart quotes + em dash.
        assert_eq!(
            decode_ppt_8bit_text(b"\x93quoted\x94 \x97 dash"),
            "“quoted” — dash"
        );
        // Whitespace survives; undefined slots / control bytes become spaces.
        assert_eq!(decode_ppt_8bit_text(b"a\tb\nc"), "a\tb\nc");
        assert_eq!(decode_ppt_8bit_text(b"a\x81b\x1fb"), "a b b");
    }

    #[test]
    fn legacy_ppt_bytes_atom_keeps_accents() {
        // End-to-end: a BytesAtom slide extracts its accented text intact.
        let stream = ppt_rec(0x0f, 1006, &ppt_rec(0, 4008, b"S\xEDntese do m\xF3dulo"));
        let slides = legacy_ppt_slide_texts(&stream);
        assert_eq!(slides.len(), 1);
        assert_eq!(slides[0], vec!["Síntese do módulo".to_string()]);
    }

    #[test]
    fn legacy_ppt_structured_layout_decodes_shapes() {
        // A minimal structured deck: DocumentAtom (slide size), one slide
        // with a ColorSchemeAtom and a PPDrawing holding a single shape —
        // F00A (instance = shape type), F00B (white fill), F010 anchor
        // (y1, x1, x2, y2 as u16s) and an F00D client textbox with "Hello".
        let d = td("legacy_ppt_escher");
        let p = d.join("deck.ppt");

        let escher = |ver: u16, inst: u16, rec_type: u16, payload: &[u8]| -> Vec<u8> {
            let mut v = Vec::new();
            v.extend_from_slice(&((inst << 4) | ver).to_le_bytes());
            v.extend_from_slice(&rec_type.to_le_bytes());
            v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            v.extend_from_slice(payload);
            v
        };

        // Anchor: F010 packs (y1, x1, x2, y2) → rect (200,100)-(300,400) mu.
        let anchor: Vec<u8> = [100u16, 200, 300, 400]
            .iter()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        // One simple property: fill color (0x181) = plain white COLORREF.
        let mut props = Vec::new();
        props.extend_from_slice(&0x0181u16.to_le_bytes());
        props.extend_from_slice(&0x00ff_ffffu32.to_le_bytes());
        // Client textbox: TextHeaderAtom (3999) + TextCharsAtom (4000).
        let textbox = [
            ppt_rec(0, 3999, &4u32.to_le_bytes()),
            ppt_rec(0, 4000, &ppt_utf16("Hello")),
        ]
        .concat();
        // F00A payload: [spid u32][flags u16][type u16].
        let mut sp = Vec::new();
        sp.extend_from_slice(&0x0000_0400u32.to_le_bytes());
        sp.extend_from_slice(&0x0a00u16.to_le_bytes()); // HAVEANCHOR|HASSHAPETYPE
        sp.extend_from_slice(&0u16.to_le_bytes());

        let shape = [
            escher(0, 1, 0xf00a, &sp),
            escher(0, 0, 0xf00b, &props),
            escher(0, 0, 0xf010, &anchor),
            escher(0x0f, 0, 0xf00d, &textbox),
        ]
        .concat();
        // PPDrawing → DgContainer → SpgrContainer → shape container.
        let drawing = escher(
            0x0f,
            0,
            1036,
            &escher(
                0x0f,
                0,
                0xf002,
                &escher(0x0f, 0, 0xf003, &escher(0x0f, 0, 0xf004, &shape)),
            ),
        );
        // ColorSchemeAtom: 8×COLORREF; title slot (3) = 0x7D491F (#1F497D).
        let mut scheme = Vec::new();
        for v in [
            0x00ff_ffffu32,
            0x0000_0000,
            0x00ec_ece1,
            0x007d_491f,
            0x0000_0000,
            0x0000_0000,
            0x0000_0000,
            0x0000_0000,
        ] {
            scheme.extend_from_slice(&v.to_le_bytes());
        }
        let slide = ppt_rec(0x0f, 1006, &[ppt_rec(0, 2032, &scheme), drawing].concat());
        // DocumentAtom: slide size 5760×4320 master units.
        let mut doc_atom = Vec::new();
        doc_atom.extend_from_slice(&5760u32.to_le_bytes());
        doc_atom.extend_from_slice(&4320u32.to_le_bytes());
        let stream = ppt_rec(0x0f, 1000, &[ppt_rec(0, 1001, &doc_atom), slide].concat());

        {
            let mut comp = cfb::create(&p).unwrap();
            let mut s = comp.create_stream("/PowerPoint Document").unwrap();
            s.write_all(&stream).unwrap();
            s.flush().unwrap();
            drop(s);
            comp.flush().unwrap();
        }

        let layout = legacy_ppt_structured_layout(&p, 1).expect("structured parse");
        assert_eq!(layout.slide_w, 9_144_000.0);
        assert_eq!(layout.slide_h, 6_858_000.0);
        assert_eq!(layout.elements.len(), 2, "fill shape behind its text");

        let emu = |mu: f64| mu * 1587.5;
        let SlideElement::Shape(sh) = &layout.elements[0] else {
            panic!("first element must be the fill shape");
        };
        assert_eq!(sh.x, emu(200.0));
        assert_eq!(sh.y, emu(100.0));
        assert_eq!(sh.w, emu(100.0));
        assert_eq!(sh.h, emu(300.0));
        assert_eq!(sh.prst, "rect");
        assert!(matches!(sh.fill, Some(FillKind::Solid((1.0, 1.0, 1.0), _))));

        let SlideElement::Text(b) = &layout.elements[1] else {
            panic!("second element must be the text box");
        };
        assert_eq!(b.text, "Hello");
        assert!(b.is_title, "the only text box becomes the title");
        // Title color resolves through the scheme: 0x7D491F → (0x1F,0x49,0x7D).
        let (r, g, bl) = b.color.expect("scheme title color");
        assert!((r - 0x1f as f64 / 255.0).abs() < 1e-9);
        assert!((g - 0x49 as f64 / 255.0).abs() < 1e-9);
        assert!((bl - 0x7d as f64 / 255.0).abs() < 1e-9);

        // Counts as one slide and renders through the shared pipeline.
        assert_eq!(legacy_ppt_slide_count(&p), Some(1));
        let png = render_legacy_ppt_slide(&p, 1).expect("render");
        assert!(png.starts_with(&[0x89, b'P', b'N', b'G']), "PNG output");

        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn legacy_ppt_persist_chain_orders_live_slides_with_entries() {
        let d = td("legacy_ppt_persist");
        let p = d.join("deck.ppt");

        // One SlideListWithText block: ref 1 carries two entries (body text
        // listed before title — matching is by textType, not position),
        // ref 2 only a blank one (never a placeholder to place).
        let block = [
            ppt_rec(0, 1011, &1u32.to_le_bytes()),
            ppt_rec(0, 3999, &1u32.to_le_bytes()),
            ppt_rec(0, 4000, &ppt_utf16("Outline body")),
            ppt_rec(0, 3999, &0u32.to_le_bytes()),
            ppt_rec(0, 4000, &ppt_utf16("Outline title")),
            ppt_rec(0, 1011, &2u32.to_le_bytes()),
            ppt_rec(0, 3999, &0u32.to_le_bytes()),
            ppt_rec(0, 4000, &ppt_utf16("   ")),
        ]
        .concat();
        let doc = ppt_rec_inst(0x0f, 0, 4080, &block);
        let doc_container = ppt_rec(0x0f, 1000, &doc);
        // A stale 1006 the chain doesn't list: stream order shows it first.
        let orphan = ppt_rec(0x0f, 1006, &ppt_rec(0, 4000, &ppt_utf16("Stale slide")));
        let slide1 = ppt_rec(0x0f, 1006, &ppt_rec(0, 4000, &ppt_utf16("Live one")));
        let slide2 = ppt_rec(0x0f, 1006, &ppt_rec(0, 4000, &ppt_utf16("Live two")));

        // Absolute offsets the chain walks: [pad][4085][6002][container]…
        // — the pad puts the UserEditAtom off offset 0 (0 stops the walk),
        // and the 6002 record's length is fixed by its slot count.
        let pad = ppt_rec(0, 9, b"pad");
        let mut edit_pay = vec![0u8; 20]; // prev@8, table@12, document ref@16
        edit_pay[12..16].copy_from_slice(&39u32.to_le_bytes());
        let edit = ppt_rec(0, 4085, &edit_pay);
        assert_eq!((pad.len(), edit.len()), (11, 28));
        let doc_off = (11 + 28 + 24) as u32;
        let slide1_off = doc_off + doc_container.len() as u32 + orphan.len() as u32;
        let slide2_off = slide1_off + slide1.len() as u32;
        let table_pay = [
            (3u32 << 20).to_le_bytes().to_vec(), // count 3, slots from 0
            doc_off.to_le_bytes().to_vec(),
            slide1_off.to_le_bytes().to_vec(),
            slide2_off.to_le_bytes().to_vec(),
        ]
        .concat();
        let table = ppt_rec(0, 6002, &table_pay);
        assert_eq!(table.len(), 24, "fixed-size persist record under this layout");

        let stream = vec![pad, edit, table, doc_container, orphan, slide1, slide2].concat();
        let mut current_user = vec![0u8; 20];
        current_user[16..20].copy_from_slice(&11u32.to_le_bytes()); // 4085 at 11

        // Stream order sees three slides with the stale one first; the
        // chain sees the two live ones, in show order.
        let mut stream_slides = Vec::new();
        collect_ppt_slide_payloads(&stream, &mut stream_slides, 0);
        assert_eq!(stream_slides.len(), 3);
        let slides = legacy_ppt_persist_slides(&stream, Some(&current_user)).expect("chain");
        assert_eq!(slides.len(), 2, "stale slides aren't in the chain");
        assert_eq!(
            slides[0].payload,
            ppt_rec(0, 4000, &ppt_utf16("Live one")),
            "chain order, not stream order"
        );
        assert_eq!(slides[1].payload, ppt_rec(0, 4000, &ppt_utf16("Live two")));
        assert_eq!(slides[0].entries.len(), 2);
        assert_eq!(slides[0].entries[0].tt, 1);
        assert_eq!(slides[0].entries[0].text, "Outline body");
        assert_eq!(slides[0].entries[1].tt, 0);
        assert_eq!(slides[0].entries[1].text, "Outline title");
        assert!(
            slides[1].entries.is_empty(),
            "blank entries aren't placeholders to place"
        );

        // No Current User stream → no chain → callers keep stream order.
        assert!(legacy_ppt_persist_slides(&stream, None).is_none());
        assert!(legacy_ppt_slide_at(&stream, Some(&current_user), 1).is_some());
        assert!(legacy_ppt_slide_at(&stream, Some(&current_user), 3).is_none());

        // End to end through the CFB file: the count follows the chain.
        {
            let mut comp = cfb::create(&p).unwrap();
            let mut s = comp.create_stream("/PowerPoint Document").unwrap();
            s.write_all(&stream).unwrap();
            s.flush().unwrap();
            drop(s);
            let mut s = comp.create_stream("/Current User").unwrap();
            s.write_all(&current_user).unwrap();
            s.flush().unwrap();
            drop(s);
            comp.flush().unwrap();
        }
        assert_eq!(legacy_ppt_current_user(&p).map(|c| c.len()), Some(20));
        assert_eq!(legacy_ppt_slide_count(&p), Some(2), "count follows the chain");

        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn legacy_ppt_style_decode_builds_paragraphs() {
        // A 4001 over "AB": paragraph run cch = text_len + 1 (the count
        // rule decks follow), bullet/align/margins present; the char run
        // carries bold, size and a ColorIndexStruct color.
        let mut pay = Vec::new();
        pay.extend_from_slice(&3u32.to_le_bytes()); // cch
        pay.extend_from_slice(&0u16.to_le_bytes()); // level
        pay.extend_from_slice(&0x0000_0d0fu32.to_le_bytes()); // align|marL|indent|bulletFlags
        pay.extend_from_slice(&1u16.to_le_bytes()); // bulletFlags: has bullet
        pay.extend_from_slice(&1u16.to_le_bytes()); // textAlignment: center
        pay.extend_from_slice(&100i16.to_le_bytes()); // leftMargin (master units)
        pay.extend_from_slice(&(-100i16).to_le_bytes()); // indent (hanging)
        pay.extend_from_slice(&3u32.to_le_bytes()); // char cch
        pay.extend_from_slice(&0x0002_ffffu32.to_le_bytes()); // CFStyle | fontSize
        pay.extend_from_slice(&1u16.to_le_bytes()); // CFStyle: bold
        pay.extend_from_slice(&36u16.to_le_bytes()); // fontSize: 36pt

        let (para_runs, char_runs) = legacy_ppt_decode_style(&pay, 2).expect("style decodes");
        assert_eq!(para_runs.len(), 1);
        let r = &para_runs[0];
        assert_eq!((r.cch, r.level, r.align, r.has_bullet), (3, 0, Some("ctr"), Some(true)));
        assert_eq!(r.mar_l_emu, Some(100.0 * 1587.5));
        assert_eq!(r.indent_emu, Some(100.0 * 1587.5));
        assert_eq!(char_runs.len(), 1);
        assert_eq!(char_runs[0].cch, 3);
        assert_eq!(char_runs[0].size_pt, Some(36.0));
        assert_eq!(char_runs[0].cf, Some(1));
        // A style that can't land byte-exact keeps the plain-text path.
        assert!(legacy_ppt_decode_style(&pay[..pay.len() - 1], 2).is_none());

        let scheme = [(0.0, 0.0, 0.0); 8];
        let master = std::collections::HashMap::new();
        let fonts = std::collections::HashMap::new();
        let paras = legacy_ppt_box_paras("AB", Some(&pay), None, &master, &fonts, 18.0, &scheme)
            .expect("paras");
        assert_eq!(paras.len(), 1);
        assert_eq!(paras[0].algn, "ctr");
        assert!(
            matches!(&paras[0].bullet, Some(ParaBullet::Char(c)) if c == "•"),
            "hasBullet without a glyph defaults to the dot"
        );
        assert_eq!(paras[0].mar_l_emu, 100.0 * 1587.5);
        assert_eq!(paras[0].hang_emu, 100.0 * 1587.5);
        assert_eq!(paras[0].runs.len(), 1);
        let run = &paras[0].runs[0];
        assert_eq!(run.text, "AB");
        assert_eq!(run.sz_pt, Some(36.0), "sz_pt always filled (renderer defaults 18)");
        assert!(run.bold);
        assert_eq!(run.font.as_deref(), Some("Sans"));

        // The 4003 master level is the base where a box's runs don't say.
        let mut master = std::collections::HashMap::new();
        master.insert(
            0u32,
            vec![LegacyPptLevelStyle {
                align: Some("r"),
                has_bullet: Some(true),
                bullet_char: Some(0xf0b7), // Symbol bullet (PUA) → default dot
                mar_l_emu: Some(300.0),
                size_pt: Some(14.0),
                ..Default::default()
            }],
        );
        let para_only = [
            3u32.to_le_bytes().to_vec(),
            0u16.to_le_bytes().to_vec(),
            0u32.to_le_bytes().to_vec(),
        ]
        .concat();
        let paras = legacy_ppt_box_paras("AB", Some(&para_only), Some(0), &master, &fonts, 18.0, &scheme)
            .expect("paras");
        assert_eq!(paras[0].algn, "r");
        assert!(
            matches!(&paras[0].bullet, Some(ParaBullet::Char(c)) if c == "•"),
            "private-use bulletChar falls back to the dot"
        );
        assert_eq!(paras[0].mar_l_emu, 300.0);
        assert_eq!(
            paras[0].hang_emu, 300.0,
            "bullet + margin hangs without an indent"
        );
        assert_eq!(paras[0].runs[0].sz_pt, Some(14.0));
    }

    /// Symbol-font bullets: the document font list resolves `bulletFontRef`,
    /// so a Wingdings byte ('l' = ●, 'ü' = ✔) becomes a glyph Sans can draw
    /// while a text-font character stays the letter it is; unmapped bytes and
    /// private-use codes keep the existing degradations.
    #[test]
    fn legacy_ppt_symbol_bullet_chars_resolve_font_list() {
        assert_eq!(legacy_ppt_bullet_char(Some(108), Some("Wingdings")), '●', "Wingdings 'l'");
        assert_eq!(legacy_ppt_bullet_char(Some(252), Some("Wingdings")), '✔', "Wingdings 'ü'");
        assert_eq!(legacy_ppt_bullet_char(Some(167), Some("Wingdings")), '▪', "Wingdings '§'");
        assert_eq!(legacy_ppt_bullet_char(Some(108), Some("Arial")), 'l', "text font keeps the letter");
        assert_eq!(legacy_ppt_bullet_char(Some(108), None), 'l', "no font resolves to no mapping");
        assert_eq!(legacy_ppt_bullet_char(Some(8226), Some("Wingdings")), '•', "Unicode char passes");
        assert_eq!(legacy_ppt_bullet_char(Some(0xf0b7), None), '•', "unmapped PUA → the dot");
        assert_eq!(legacy_ppt_bullet_char(Some(0x2d), Some("Wingdings")), '-', "unmapped byte keeps itself");
        assert_eq!(legacy_ppt_bullet_char(Some(0), None), '•', "NUL → the dot");

        // 4023 FontEntityAtom records: recInstance is the font index.
        let utf16 = |s: &str| -> Vec<u8> {
            let mut v: Vec<u8> = s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
            v.extend_from_slice(&[0, 0]);
            v
        };
        let stream = [
            ppt_rec_inst(0, 0, 4023, &utf16("Arial")),
            ppt_rec_inst(0, 3, 4023, &utf16("Wingdings")),
        ]
        .concat();
        let fonts = legacy_ppt_font_names(&stream);
        assert_eq!(fonts.get(&0).map(String::as_str), Some("Arial"));
        assert_eq!(fonts.get(&3).map(String::as_str), Some("Wingdings"));

        // The Chapter_6 case: the master level carries bullet + font, the
        // box's own paragraph run carries neither → ●.
        let scheme = [(0.0, 0.0, 0.0); 8];
        let mut master = std::collections::HashMap::new();
        master.insert(
            1u32,
            vec![LegacyPptLevelStyle {
                has_bullet: Some(true),
                bullet_char: Some(108),
                bullet_font: Some(3),
                ..Default::default()
            }],
        );
        let para_only = [
            3u32.to_le_bytes().to_vec(),
            0u16.to_le_bytes().to_vec(),
            0u32.to_le_bytes().to_vec(),
        ]
        .concat();
        let paras = legacy_ppt_box_paras("AB", Some(&para_only), Some(1), &master, &fonts, 18.0, &scheme)
            .expect("paras");
        assert!(
            matches!(&paras[0].bullet, Some(ParaBullet::Char(c)) if c == "●"),
            "font-list Wingdings 'l' → ●, got {:?}",
            paras[0].bullet
        );
    }

    #[test]
    fn legacy_ppt_master_styles_read_levels_by_instance() {
        // instance < 5: levels start straight at pflags; instance >= 5: a
        // `level` word leads each one [MS-PPT].
        let no_level = [
            1u16.to_le_bytes().to_vec(),          // cLevels
            0u32.to_le_bytes().to_vec(),          // pflags: no properties
            0x0002_0000u32.to_le_bytes().to_vec(), // cflags: fontSize
            32u16.to_le_bytes().to_vec(),
        ]
        .concat();
        let with_level = [
            1u16.to_le_bytes().to_vec(),          // cLevels
            0u16.to_le_bytes().to_vec(),          // level word (instance >= 5)
            0u32.to_le_bytes().to_vec(),          // pflags: no properties
            0x0002_0000u32.to_le_bytes().to_vec(), // cflags: fontSize
            28u16.to_le_bytes().to_vec(),
        ]
        .concat();
        let restated = [
            1u16.to_le_bytes().to_vec(),
            0u32.to_le_bytes().to_vec(),
            0x0002_0000u32.to_le_bytes().to_vec(),
            40u16.to_le_bytes().to_vec(),
        ]
        .concat();
        let stream = [
            ppt_rec_inst(0, 0, 4003, &no_level),
            ppt_rec_inst(0, 5, 4003, &with_level),
            ppt_rec_inst(0, 0, 4003, &restated),
        ]
        .concat();

        let styles = legacy_ppt_master_styles(&stream);
        assert_eq!(styles.len(), 2);
        assert_eq!(
            styles.get(&0).map(|l| l[0].size_pt),
            Some(Some(32.0)),
            "first successful decode per textType wins"
        );
        assert_eq!(styles.get(&5).map(|l| l[0].size_pt), Some(Some(28.0)));

        // The other reading is only tried when the spec one fails: inst 5
        // written without the level word still parses through the fallback.
        let decoded = legacy_ppt_decode_master_style(5, &no_level).expect("fallback parse");
        assert_eq!(decoded[0].size_pt, Some(32.0));
        // And inst 0 written with one parses through the spec reading.
        let decoded = legacy_ppt_decode_master_style(0, &with_level).expect("spec parse");
        assert_eq!(decoded[0].size_pt, Some(28.0));
    }

    #[test]
    fn legacy_ppt_outline_text_fills_empty_boxes() {
        // PPT 97 keeps placeholder text in the document's SlideListWithText:
        // the slide's own F00D box carries only its TextHeaderAtom, so the
        // text — and its 4001 styles — arrive through the chain, matched by
        // textType even though the entries aren't in box order.
        let d = td("legacy_ppt_outline");
        let p = d.join("deck.ppt");

        // Style over the tt=0 entry's 12 characters: bold, 44pt.
        let mut style = Vec::new();
        style.extend_from_slice(&13u32.to_le_bytes()); // cch (text_len + 1)
        style.extend_from_slice(&0u16.to_le_bytes()); // level
        style.extend_from_slice(&0u32.to_le_bytes()); // no paragraph props
        style.extend_from_slice(&13u32.to_le_bytes()); // char cch
        style.extend_from_slice(&0x0002_ffffu32.to_le_bytes()); // CFStyle | fontSize
        style.extend_from_slice(&1u16.to_le_bytes()); // CFStyle: bold
        style.extend_from_slice(&44u16.to_le_bytes()); // fontSize: 44pt
        let block = [
            ppt_rec(0, 1011, &1u32.to_le_bytes()),
            ppt_rec(0, 3999, &1u32.to_le_bytes()), // tt = body, listed first
            ppt_rec(0, 4000, &ppt_utf16("Body first")),
            ppt_rec(0, 3999, &0u32.to_le_bytes()), // tt = title — the box's own
            ppt_rec(0, 4000, &ppt_utf16("Title second")),
            ppt_rec(0, 4001, &style),
        ]
        .concat();
        let doc = ppt_rec_inst(0x0f, 0, 4080, &block);

        // Minimal escher slide: one text box whose F00D has no text atoms.
        let escher = |ver: u16, inst: u16, rec_type: u16, payload: &[u8]| -> Vec<u8> {
            let mut v = Vec::new();
            v.extend_from_slice(&((inst << 4) | ver).to_le_bytes());
            v.extend_from_slice(&rec_type.to_le_bytes());
            v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            v.extend_from_slice(payload);
            v
        };
        let anchor: Vec<u8> = [100u16, 200, 300, 400]
            .iter()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        let mut props = Vec::new();
        props.extend_from_slice(&0x0181u16.to_le_bytes());
        props.extend_from_slice(&0x00ff_ffffu32.to_le_bytes());
        let textbox = ppt_rec(0, 3999, &0u32.to_le_bytes()); // header only
        let mut sp = Vec::new();
        sp.extend_from_slice(&0x0000_0400u32.to_le_bytes());
        sp.extend_from_slice(&0x0a00u16.to_le_bytes()); // HAVEANCHOR|HASSHAPETYPE
        sp.extend_from_slice(&0u16.to_le_bytes());
        let shape = [
            escher(0, 1, 0xf00a, &sp),
            escher(0, 0, 0xf00b, &props),
            escher(0, 0, 0xf010, &anchor),
            escher(0x0f, 0, 0xf00d, &textbox),
        ]
        .concat();
        let drawing = escher(
            0x0f,
            0,
            1036,
            &escher(0x0f, 0, 0xf002, &escher(0x0f, 0, 0xf003, &escher(0x0f, 0, 0xf004, &shape))),
        );
        let slide = ppt_rec(0x0f, 1006, &drawing);
        let mut doc_atom = Vec::new();
        doc_atom.extend_from_slice(&5760u32.to_le_bytes());
        doc_atom.extend_from_slice(&4320u32.to_le_bytes());
        let container_payload = [ppt_rec(0, 1001, &doc_atom), doc].concat();
        let container = ppt_rec(0x0f, 1000, &container_payload);

        // Offsets: [pad 11][4085 28][6002 …][container][slide].
        let pad = ppt_rec(0, 9, b"pad");
        let mut edit_pay = vec![0u8; 20];
        edit_pay[12..16].copy_from_slice(&39u32.to_le_bytes());
        let edit = ppt_rec(0, 4085, &edit_pay);
        assert_eq!((pad.len(), edit.len()), (11, 28));
        let container_off = (11 + 28 + 8 + 4 + 8) as u32; // pad + edit + 2 slots
        let slide_off = container_off + container.len() as u32;
        let table_pay = [
            (2u32 << 20).to_le_bytes().to_vec(), // count 2, slots from 0
            container_off.to_le_bytes().to_vec(),
            slide_off.to_le_bytes().to_vec(),
        ]
        .concat();
        let table = ppt_rec(0, 6002, &table_pay);
        assert_eq!(table.len(), 8 + 4 + 8);
        let mut current_user = vec![0u8; 20];
        current_user[16..20].copy_from_slice(&11u32.to_le_bytes()); // 4085 at 11
        let stream = vec![pad, edit, table, container, slide].concat();

        {
            let mut comp = cfb::create(&p).unwrap();
            let mut s = comp.create_stream("/PowerPoint Document").unwrap();
            s.write_all(&stream).unwrap();
            s.flush().unwrap();
            drop(s);
            let mut s = comp.create_stream("/Current User").unwrap();
            s.write_all(&current_user).unwrap();
            s.flush().unwrap();
            drop(s);
            comp.flush().unwrap();
        }

        assert_eq!(legacy_ppt_slide_count(&p), Some(1));
        let layout = legacy_ppt_structured_layout(&p, 1).expect("structured parse");
        let SlideElement::Text(b) = layout.elements.last().expect("text box") else {
            panic!("last element must be the text box");
        };
        assert_eq!(
            b.text, "Title second",
            "entry matched by textType, not list order"
        );
        let paras = b.paras.as_ref().expect("4001-backed paragraphs");
        assert_eq!(paras.len(), 1);
        assert_eq!(paras[0].runs.len(), 1);
        assert_eq!(paras[0].runs[0].text, "Title second");
        assert_eq!(paras[0].runs[0].sz_pt, Some(44.0), "size from the entry's 4001");
        assert!(paras[0].runs[0].bold, "CFStyle bit0 = bold");

        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn legacy_ppt_picture_shape_resolves_media() {
        // pib (0x104) = 1-based BlipStore index → uid → Pictures record →
        // cached image file → DrawPicture element.
        let d = td("legacy_ppt_pic");
        let p = d.join("deck.ppt");

        let png = test_png();
        let uid = [7u8; 16];
        // BlipStoreEntry F007: btWin32, btMac, uid(16), tag, cbSave, …
        let mut bse = vec![5u8, 5];
        bse.extend_from_slice(&uid);
        bse.extend_from_slice(&[0xff, 0x00]); // tag
        bse.extend_from_slice(&0u32.to_le_bytes()); // cbSave
        bse.extend_from_slice(&0u32.to_le_bytes()); // cRef
        bse.extend_from_slice(&0u32.to_le_bytes()); // foDelay
        bse.extend_from_slice(&0u32.to_le_bytes()); // usage
        let blipstore = ppt_rec_inst(
            0x0f,
            0,
            1035,
            &ppt_rec_inst(0x0f, 0, 0xf001, &ppt_rec_inst(2, 2, 0xf007, &bse)),
        );

        // Pictures record: uid(16) + tag byte + raw image.
        let mut pic_payload = Vec::new();
        pic_payload.extend_from_slice(&uid);
        pic_payload.push(0xff);
        pic_payload.extend_from_slice(&png);
        let pictures = ppt_rec_inst(0, 0, 0xf01d, &pic_payload);

        // Picture shape: F00A (instance = 75, picture), pib = 1, anchor.
        let anchor: Vec<u8> = [100u16, 200, 300, 400]
            .iter()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        let mut props = Vec::new();
        props.extend_from_slice(&0x0104u16.to_le_bytes());
        props.extend_from_slice(&1u32.to_le_bytes()); // pib → entry 1
        let mut sp = Vec::new();
        sp.extend_from_slice(&0x0000_0400u32.to_le_bytes());
        sp.extend_from_slice(&0x0a00u16.to_le_bytes());
        sp.extend_from_slice(&75u16.to_le_bytes());
        let shape = [
            ppt_rec_inst(0, 75, 0xf00a, &sp),
            ppt_rec_inst(0, 0, 0xf00b, &props),
            ppt_rec_inst(0, 0, 0xf010, &anchor),
        ]
        .concat();
        let drawing = ppt_rec_inst(
            0x0f,
            0,
            1036,
            &ppt_rec_inst(
                0x0f,
                0,
                0xf002,
                &ppt_rec_inst(0x0f, 0, 0xf003, &ppt_rec_inst(0x0f, 0, 0xf004, &shape)),
            ),
        );
        let slide = ppt_rec(0x0f, 1006, &drawing);
        let mut doc_atom = Vec::new();
        doc_atom.extend_from_slice(&5760u32.to_le_bytes());
        doc_atom.extend_from_slice(&4320u32.to_le_bytes());
        let stream = ppt_rec(
            0x0f,
            1000,
            &[ppt_rec(0, 1001, &doc_atom), blipstore, slide].concat(),
        );

        {
            let mut comp = cfb::create(&p).unwrap();
            let mut s = comp.create_stream("/PowerPoint Document").unwrap();
            s.write_all(&stream).unwrap();
            s.flush().unwrap();
            drop(s);
            let mut s = comp.create_stream("/Pictures").unwrap();
            s.write_all(&pictures).unwrap();
            s.flush().unwrap();
            drop(s);
            comp.flush().unwrap();
        }

        let layout = legacy_ppt_structured_layout(&p, 1).expect("structured parse");
        assert_eq!(
            layout.elements.len(),
            1,
            "the picture is the slide's only element"
        );
        let SlideElement::Picture(pic) = &layout.elements[0] else {
            panic!("element must be the picture");
        };
        assert_eq!(pic.crop, [0.0; 4], "no srcRect crop");
        assert!(pic.path.exists(), "blip materialized into the media cache");
        // .bin has no image extension — decode by content, like the painter.
        assert!(
            image::load_from_memory(&fs::read(&pic.path).unwrap()).is_ok(),
            "cached blip must decode"
        );
        assert!(render_slide_layout(&layout).is_some(), "renders the picture");
        let rendered = render_legacy_ppt_slide(&p, 1).expect("render");
        assert!(rendered.starts_with(&[0x89, b'P', b'N', b'G']), "PNG output");

        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn legacy_ppt_master_scheme_and_background() {
        // The slide has no ColorSchemeAtom of its own: it follows its
        // master's (join id 0x80000001 via SlideListWithText inst 1), and
        // the master's full-bleed picture becomes the background image.
        let d = td("legacy_ppt_master_bg");
        let p = d.join("deck.ppt");

        let png = test_png();
        let uid = [9u8; 16];
        let mut bse = vec![5u8, 5];
        bse.extend_from_slice(&uid);
        bse.extend_from_slice(&[0xff, 0x00]);
        bse.extend_from_slice(&0u32.to_le_bytes());
        bse.extend_from_slice(&0u32.to_le_bytes());
        bse.extend_from_slice(&0u32.to_le_bytes());
        bse.extend_from_slice(&0u32.to_le_bytes());
        let blipstore = ppt_rec_inst(
            0x0f,
            0,
            1035,
            &ppt_rec_inst(0x0f, 0, 0xf001, &ppt_rec_inst(2, 2, 0xf007, &bse)),
        );
        let mut pic_payload = Vec::new();
        pic_payload.extend_from_slice(&uid);
        pic_payload.push(0xff);
        pic_payload.extend_from_slice(&png);
        let pictures = ppt_rec_inst(0, 0, 0xf01d, &pic_payload);

        // Master join: SlideListWithText (4080, inst 1) block echoing the
        // master id at u32@12.
        let mut join = vec![0u8; 16];
        join[12..16].copy_from_slice(&0x8000_0001u32.to_le_bytes());
        let master_list = ppt_rec_inst(0x0f, 1, 4080, &ppt_rec(0, 1011, &join));

        // Slide: SlideAtom with masterID 0x80000001, one filled shape
        // carrying a text box; no own scheme.
        let mut atom = vec![0u8; 24];
        atom[12..16].copy_from_slice(&0x8000_0001u32.to_le_bytes());
        let anchor: Vec<u8> = [100u16, 200, 300, 400]
            .iter()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        let mut props = Vec::new();
        props.extend_from_slice(&0x0181u16.to_le_bytes());
        props.extend_from_slice(&0x00ff_ffffu32.to_le_bytes());
        let textbox = [
            ppt_rec(0, 3999, &4u32.to_le_bytes()),
            ppt_rec(0, 4000, &ppt_utf16("Hello")),
        ]
        .concat();
        let mut sp = Vec::new();
        sp.extend_from_slice(&0x0000_0400u32.to_le_bytes());
        sp.extend_from_slice(&0x0a00u16.to_le_bytes());
        sp.extend_from_slice(&0u16.to_le_bytes());
        let shape = [
            ppt_rec_inst(0, 1, 0xf00a, &sp),
            ppt_rec_inst(0, 0, 0xf00b, &props),
            ppt_rec_inst(0, 0, 0xf010, &anchor),
            ppt_rec_inst(0x0f, 0, 0xf00d, &textbox),
        ]
        .concat();
        let drawing = ppt_rec_inst(
            0x0f,
            0,
            1036,
            &ppt_rec_inst(
                0x0f,
                0,
                0xf002,
                &ppt_rec_inst(0x0f, 0, 0xf003, &ppt_rec_inst(0x0f, 0, 0xf004, &shape)),
            ),
        );
        let slide = ppt_rec(0x0f, 1006, &[ppt_rec(0, 1007, &atom), drawing].concat());

        // Master: scheme title (slot 3) = 0x0000FF00 → green, plus a
        // full-bleed picture (anchor = the whole 5760×4320 slide).
        let mut scheme = Vec::new();
        for v in [
            0x00ff_ffffu32,
            0x0000_0000,
            0x00ec_ece1,
            0x0000_ff00, // title: green
            0x0000_0000,
            0x0000_0000,
            0x0000_0000,
            0x0000_0000,
        ] {
            scheme.extend_from_slice(&v.to_le_bytes());
        }
        // F010 packs (y1, x1, x2, y2) as 4×u16 — the whole 5760×4320 slide.
        let full_anchor: Vec<u8> = [0u16, 0, 5760, 4320]
            .iter()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        let mut bg_props = Vec::new();
        bg_props.extend_from_slice(&0x0104u16.to_le_bytes());
        bg_props.extend_from_slice(&1u32.to_le_bytes());
        let mut bg_sp = Vec::new();
        bg_sp.extend_from_slice(&0x0000_0401u32.to_le_bytes());
        bg_sp.extend_from_slice(&0x0a00u16.to_le_bytes());
        bg_sp.extend_from_slice(&75u16.to_le_bytes());
        let bg_shape = [
            ppt_rec_inst(0, 75, 0xf00a, &bg_sp),
            ppt_rec_inst(0, 0, 0xf00b, &bg_props),
            ppt_rec_inst(0, 0, 0xf010, &full_anchor),
        ]
        .concat();
        let master_drawing = ppt_rec_inst(
            0x0f,
            0,
            1036,
            &ppt_rec_inst(
                0x0f,
                0,
                0xf002,
                &ppt_rec_inst(0x0f, 0, 0xf003, &ppt_rec_inst(0x0f, 0, 0xf004, &bg_shape)),
            ),
        );
        let master = ppt_rec(
            0x0f,
            1016,
            &[ppt_rec(0, 2032, &scheme), master_drawing].concat(),
        );

        let mut doc_atom = Vec::new();
        doc_atom.extend_from_slice(&5760u32.to_le_bytes());
        doc_atom.extend_from_slice(&4320u32.to_le_bytes());
        let stream = ppt_rec(
            0x0f,
            1000,
            &[
                ppt_rec(0, 1001, &doc_atom),
                master_list,
                blipstore,
                slide,
            ]
            .concat(),
        );
        let stream = [stream, master].concat();

        {
            let mut comp = cfb::create(&p).unwrap();
            let mut s = comp.create_stream("/PowerPoint Document").unwrap();
            s.write_all(&stream).unwrap();
            s.flush().unwrap();
            drop(s);
            let mut s = comp.create_stream("/Pictures").unwrap();
            s.write_all(&pictures).unwrap();
            s.flush().unwrap();
            drop(s);
            comp.flush().unwrap();
        }

        let layout = legacy_ppt_structured_layout(&p, 1).expect("structured parse");
        // Scheme falls through to the master: the title box is green.
        let title = layout
            .elements
            .iter()
            .find_map(|e| match e {
                SlideElement::Text(b) if b.is_title => Some(b),
                _ => None,
            })
            .expect("title box");
        assert_eq!(
            title.color,
            Some((0.0, 1.0, 0.0)),
            "master scheme title color"
        );
        // Background: the master's full-bleed picture.
        let Some(SlideBackground::Image(bg)) = &layout.background else {
            panic!("master full-bleed picture must become the background");
        };
        assert!(bg.exists());
        assert!(
            image::load_from_memory(&fs::read(bg).unwrap()).is_ok(),
            "background blip must decode"
        );
        assert!(render_slide_layout(&layout).is_some(), "renders with bg");

        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn resolve_page_png_unknown_type_and_clamping() {
        // Types without per-page rendering fall back to slide_paths.
        assert!(resolve_page_png(Path::new("/does/not/matter.xyz"), 1).is_none());
        // Page indices clamp into 1..=total (total ≥ 1).
        assert_eq!(clamp_page(0, 5), 1);
        assert_eq!(clamp_page(3, 5), 3);
        assert_eq!(clamp_page(9, 5), 5);
        assert_eq!(clamp_page(7, 0), 1);
    }

    #[test]
    fn compute_preview_text_file() {
        let d = td("compute");
        let p = d.join("readme.md");
        fs::write(&p, "# Hello\nWorld").unwrap();
        let payload = compute_preview(&p);
        match payload {
            PreviewPayload::Text { content } => {
                assert!(content.contains("Hello"));
            }
            _ => panic!("expected Text payload for .md file"),
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn compute_preview_binary_returns_info() {
        let d = td("compute_bin");
        let p = d.join("data.bin");
        fs::write(&p, vec![0u8; 10000]).unwrap();
        let payload = compute_preview(&p);
        assert!(matches!(payload, PreviewPayload::Info));
        let _ = fs::remove_dir_all(&d);
    }

    // ── PPTX native renderer (Phases 1–2) ──────────────────────────────

    const THEME_XML: &str = r#"<a:theme xmlns:a="a"><a:themeElements>
<a:clrScheme name="T">
<a:dk1><a:srgbClr val="000000"/></a:dk1><a:lt1><a:srgbClr val="FFFFFF"/></a:lt1>
<a:dk2><a:srgbClr val="44546A"/></a:dk2><a:lt2><a:srgbClr val="E7E6E6"/></a:lt2>
<a:accent1><a:srgbClr val="4472C4"/></a:accent1><a:accent2><a:srgbClr val="ED7D31"/></a:accent2>
<a:accent3><a:srgbClr val="A5A5A5"/></a:accent3><a:accent4><a:srgbClr val="FFC000"/></a:accent4>
<a:accent5><a:srgbClr val="5B9BD5"/></a:accent5><a:accent6><a:srgbClr val="70AD47"/></a:accent6>
<a:hlink><a:srgbClr val="0563C1"/></a:hlink><a:folHlink><a:srgbClr val="954F72"/></a:folHlink>
</a:clrScheme>
<a:majorFont><a:latin typeface="Contoso Display"/></a:majorFont>
<a:minorFont><a:latin typeface="Contoso Sans"/></a:minorFont>
<a:fmtScheme name="F"><a:bgFillStyleLst>
<a:solidFill><a:schemeClr val="lt1"/></a:solidFill>
<a:gradFill><a:gsLst><a:gs pos="0"><a:srgbClr val="000000"/></a:gs><a:gs pos="100000"><a:srgbClr val="FFFFFF"/></a:gs></a:gsLst><a:lin ang="5400000"/></a:gradFill>
</a:bgFillStyleLst></a:fmtScheme>
</a:themeElements></a:theme>"#;

    const TX_STYLES: &str = r#"<p:txStyles>
<p:titleStyle><a:lvl1pPr algn="ctr"><a:buNone/><a:defRPr sz="3600" b="1">
<a:solidFill><a:schemeClr val="dk1"/></a:solidFill><a:latin typeface="+mj-lt"/></a:defRPr></a:lvl1pPr></p:titleStyle>
<p:bodyStyle><a:lvl1pPr algn="l" marL="342900" indent="-342900"><a:buChar char="&#x2022;"/>
<a:defRPr sz="1800"><a:latin typeface="+mn-lt"/></a:defRPr></a:lvl1pPr></p:bodyStyle>
<p:otherStyle><a:lvl1pPr><a:buNone/><a:defRPr sz="1400"/></a:lvl1pPr></p:otherStyle>
</p:txStyles>"#;

    /// ECMA-376 default color map (what a real master carries).
    const DEFAULT_CLR_MAP: &str = r#"<p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2"
accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4"
accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/>"#;

    /// A tiny real PNG for `ppt/media/image1.png`.
    fn tiny_png() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([10, 200, 30, 255]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        png
    }

    fn build_deck_zip(name: &str, parts: &[(String, Vec<u8>)]) -> std::path::PathBuf {
        let d = td(name);
        let path = d.join("deck.pptx");
        let f = fs::File::create(&path).unwrap();
        let mut w = zip::ZipWriter::new(f);
        for (n, data) in parts {
            w.start_file(n.as_str(), zip::write::FileOptions::default()).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap();
        path
    }

    /// Build a minimal single-slide PPTX. Shape-tree strings are the direct
    /// children of each part's `<p:spTree>`; `slide_bg` is the slide's
    /// `<p:bg>…</p:bg>` (or empty); `layout_root` carries extra root
    /// attributes (e.g. `showMasterSp="0"`); `clr_map` replaces the master's
    /// `<p:clrMap>` when non-empty. Returns the deck path.
    #[allow(clippy::too_many_arguments)]
    fn build_deck(
        name: &str,
        slide_bg: &str,
        slide_elements: &str,
        layout_root: &str,
        layout_elements: &str,
        master_elements: &str,
        clr_map: &str,
    ) -> std::path::PathBuf {
        let strs: Vec<(String, String)> = vec![
            (
                "ppt/presentation.xml".into(),
                r#"<p:presentation><p:sldSz cx="9144000" cy="6858000"/></p:presentation>"#.into(),
            ),
            ("ppt/theme/theme1.xml".into(), THEME_XML.into()),
            (
                "ppt/slideMasters/slideMaster1.xml".into(),
                format!(
                    "<p:sldMaster><p:cSld><p:spTree>{}</p:spTree></p:cSld>{}{}</p:sldMaster>",
                    master_elements,
                    if clr_map.is_empty() { DEFAULT_CLR_MAP } else { clr_map },
                    TX_STYLES
                ),
            ),
            (
                "ppt/slideLayouts/slideLayout1.xml".into(),
                format!(
                    "<p:sldLayout {}><p:cSld><p:spTree>{}</p:spTree></p:cSld></p:sldLayout>",
                    layout_root, layout_elements
                ),
            ),
            (
                "ppt/slideLayouts/_rels/slideLayout1.xml.rels".into(),
                r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="../slideMasters/slideMaster1.xml"/></Relationships>"#.into(),
            ),
            (
                "ppt/slides/slide1.xml".into(),
                format!(
                    "<p:sld><p:cSld>{}<p:spTree>{}</p:spTree></p:cSld></p:sld>",
                    slide_bg, slide_elements
                ),
            ),
            (
                "ppt/slides/_rels/slide1.xml.rels".into(),
                r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image1.png"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/></Relationships>"#.into(),
            ),
        ];
        let mut parts: Vec<(String, Vec<u8>)> = strs
            .into_iter()
            .map(|(n, d)| (n, d.into_bytes()))
            .collect();
        parts.push(("ppt/media/image1.png".into(), tiny_png()));
        build_deck_zip(name, &parts)
    }

    fn filled_rect(id: u32, x: i64, y: i64, cx: i64, cy: i64, color: &str) -> String {
        format!(
            r#"<p:sp><p:nvSpPr><p:cNvPr id="{id}" name="R{id}"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="{x}" y="{y}"/><a:ext cx="{cx}" cy="{cy}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom><a:solidFill><a:srgbClr val="{color}"/></a:solidFill></p:spPr></p:sp>"#
        )
    }

    fn text_sp(ph: &str, tx_body: &str, xfrm: &str) -> String {
        format!(
            r#"<p:sp><p:nvSpPr><p:cNvPr id="9" name="T"/><p:cNvSpPr/><p:nvPr>{ph}</p:nvPr></p:nvSpPr><p:spPr><a:prstGeom prst="rect"><a:avLst/></a:prstGeom>{xfrm}</p:spPr><p:txBody><a:bodyPr/>{tx_body}</p:txBody></p:sp>"#
        )
    }

    fn the_pic(xfrm: &str, src_rect: &str) -> String {
        format!(
            r#"<p:pic><p:nvPicPr><p:cNvPr id="5" name="P"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="rId1"/>{src_rect}<a:stretch><a:fillRect/></a:stretch></p:blipFill><p:spPr>{xfrm}<a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr></p:pic>"#
        )
    }

    /// Regression: a picture listed AFTER a shape must draw after it — the old
    /// string-scan renderer dropped every picture it passed. Element order is
    /// z-order, so Shape → Picture → Text here.
    #[test]
    fn pptx_picture_after_shape_keeps_z_order() {
        let rect = filled_rect(2, 0, 0, 9_144_000, 6_858_000, "FFEEDD");
        let pic = the_pic(
            r#"<a:xfrm><a:off x="100000" y="100000"/><a:ext cx="500000" cy="500000"/></a:xfrm>"#,
            "",
        );
        let title = text_sp(
            r#"<p:ph type="title"/>"#,
            r#"<a:p><a:r><a:t>Hello</a:t></a:r></a:p>"#,
            r#"<a:xfrm><a:off x="0" y="0"/><a:ext cx="9144000" cy="1000000"/></a:xfrm>"#,
        );
        let deck = build_deck(
            "pptx_zorder",
            "",
            &format!("{}{}{}", rect, pic, title),
            "",
            "",
            "",
            "",
        );
        let layout = parse_pptx_slide(&deck, 1).expect("parse slide 1");
        assert_eq!(layout.elements.len(), 3, "all three elements present");
        assert!(matches!(layout.elements[0], SlideElement::Shape(_)));
        assert!(matches!(layout.elements[1], SlideElement::Picture(_)));
        assert!(matches!(layout.elements[2], SlideElement::Text(_)));
        if let SlideElement::Picture(p) = &layout.elements[1] {
            assert!(p.path.exists(), "extracted image path exists: {:?}", p.path);
            assert_eq!(p.crop, [0.0; 4], "no srcRect → no crop");
            assert_eq!((p.x, p.y), (100_000.0, 100_000.0));
        }
        let _ = fs::remove_dir_all(deck.parent().unwrap());
    }

    /// `<a:srcRect>` fractions become the picture's crop.
    #[test]
    fn pptx_picture_srcrect_crop() {
        let pic = the_pic(
            r#"<a:xfrm><a:off x="0" y="0"/><a:ext cx="1" cy="1"/></a:xfrm>"#,
            r#"<a:srcRect l="25000" t="0" r="50000" b="12500"/>"#,
        );
        let deck = build_deck("pptx_srcrect", "", &pic, "", "", "", "");
        let layout = parse_pptx_slide(&deck, 1).unwrap();
        match &layout.elements[0] {
            SlideElement::Picture(p) => {
                assert_eq!(p.crop, [0.25, 0.0, 0.5, 0.125]);
            }
            other => panic!("expected picture, got {other:?}"),
        }
        let _ = fs::remove_dir_all(deck.parent().unwrap());
    }

    /// Group `off/ext/chOff/chExt` + flips compose onto every child.
    #[test]
    fn pptx_group_transform_math() {
        // child space 4000000×2000000 shown in a 2000000×1000000 box at
        // (1000000, 1000000) → children scale ×0.5 and shift
        let group = format!(
            r#"<p:grpSp><p:nvGrpSpPr><p:cNvPr id="6" name="G"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="1000000" y="1000000"/><a:ext cx="2000000" cy="1000000"/><a:chOff x="0" y="0"/><a:chExt cx="4000000" cy="2000000"/></a:xfrm></p:grpSpPr>{}</p:grpSp>"#,
            filled_rect(7, 0, 0, 4_000_000, 2_000_000, "ABCDEF")
        );
        let deck = build_deck("pptx_group", "", &group, "", "", "", "");
        let layout = parse_pptx_slide(&deck, 1).unwrap();
        match &layout.elements[0] {
            SlideElement::Shape(sh) => {
                assert_eq!(
                    (sh.x, sh.y, sh.w, sh.h),
                    (1_000_000.0, 1_000_000.0, 2_000_000.0, 1_000_000.0)
                );
            }
            other => panic!("expected shape, got {other:?}"),
        }

        // flips mirror about the group frame center: frame [100, 400] →
        // center 250, so x' = 500 − (100 + x) = 400 − x
        let parent = XformMap::ID;
        let rx = RawXfrm {
            off: Some((100.0, 0.0)),
            ext: Some((300.0, 100.0)),
            ch_off: Some((0.0, 0.0)),
            ch_ext: Some((300.0, 100.0)),
            rot: 0.0,
            flip_h: true,
            flip_v: false,
        };
        let m = compose_group(parent, &rx);
        assert_eq!(m.apply(0.0, 0.0), (400.0, 0.0));
        assert_eq!(m.apply(300.0, 100.0), (100.0, 100.0));
        let _ = fs::remove_dir_all(deck.parent().unwrap());
    }

    /// Layout placeholder geometry is inherited when the slide placeholder
    /// has no `xfrm`; layout decorations render before slide content; and
    /// `showMasterSp="0"` hides the master's shapes.
    #[test]
    fn pptx_layout_placeholder_inheritance_and_ordering() {
        let layout_decor = filled_rect(3, 0, 6_500_000, 9_144_000, 358_000, "112233");
        let layout_title_ph = format!(
            r#"<p:sp><p:nvSpPr><p:cNvPr id="4" name="L"/><p:cNvSpPr/><p:nvPr><p:ph type="title" idx="0"/></p:nvPr></p:nvSpPr><p:spPr><a:xfrm><a:off x="500000" y="400000"/><a:ext cx="8000000" cy="900000"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr></p:sp>"#
        );
        let master_decor = filled_rect(8, 0, 0, 100, 100, "999999");
        // slide title placeholder WITHOUT xfrm → inherits layout's geometry
        let slide_title = text_sp(
            r#"<p:ph type="title" idx="0"/>"#,
            r#"<a:p><a:r><a:t>Inherited</a:t></a:r></a:p>"#,
            "",
        );
        let layout_tree = format!("{}{}", layout_decor, layout_title_ph);

        // with showMasterSp="0": master decor hidden, layout decor first
        let deck = build_deck(
            "pptx_ph_hide",
            "",
            &slide_title,
            r#"showMasterSp="0""#,
            &layout_tree,
            &master_decor,
            "",
        );
        let layout = parse_pptx_slide(&deck, 1).unwrap();
        assert_eq!(layout.elements.len(), 2, "master decor hidden, no stray boxes");
        match &layout.elements[0] {
            SlideElement::Shape(sh) => {
                assert_eq!((sh.x, sh.y, sh.h), (0.0, 6_500_000.0, 358_000.0))
            }
            other => panic!("layout decor first, got {other:?}"),
        }
        match &layout.elements[1] {
            SlideElement::Text(b) => {
                assert!(b.has_xfrm, "geometry resolved from layout");
                assert_eq!(
                    (b.x, b.y, b.w, b.h),
                    (500_000.0, 400_000.0, 8_000_000.0, 900_000.0)
                );
                assert!(b.is_title);
                assert_eq!(b.text, "Inherited");
            }
            other => panic!("slide title last, got {other:?}"),
        }
        let _ = fs::remove_dir_all(deck.parent().unwrap());

        // without showMasterSp: master decor is visible first
        let deck2 = build_deck("pptx_ph_show", "", &slide_title, "", &layout_tree, &master_decor, "");
        let layout2 = parse_pptx_slide(&deck2, 1).unwrap();
        assert_eq!(layout2.elements.len(), 3, "master decor visible");
        assert!(matches!(layout2.elements[0], SlideElement::Shape(_)));
        let _ = fs::remove_dir_all(deck2.parent().unwrap());
    }

    /// Per-run formatting, paragraph alignment/level, bullets, and `<a:br>`
    /// continuation splitting survive parsing into structured paragraphs.
    #[test]
    fn pptx_paragraph_run_formatting() {
        let body = r#"<a:p>
<a:pPr algn="ctr" lvl="1"><a:buChar char="&#x2192;"/><a:defRPr sz="900"/></a:pPr>
<a:r><a:rPr sz="2400" b="1"><a:solidFill><a:srgbClr val="FF0000"/></a:solidFill><a:latin typeface="Comic Sans MS"/></a:rPr><a:t>Bold</a:t></a:r>
<a:br/>
<a:r><a:rPr sz="1200" i="1" u="sng"><a:solidFill><a:srgbClr val="00FF00"/></a:solidFill></a:rPr><a:t>rest</a:t></a:r>
</a:p>"#;
        let sp = text_sp(
            r#"<p:ph type="obj" idx="1"/>"#,
            body,
            r#"<a:xfrm><a:off x="5" y="6"/><a:ext cx="7" cy="8"/></a:xfrm>"#,
        );
        let deck = build_deck("pptx_runs", "", &sp, "", "", "", "");
        let layout = parse_pptx_slide(&deck, 1).unwrap();
        let SlideElement::Text(b) = &layout.elements[0] else {
            panic!("expected text")
        };
        let paras = b.paras.as_ref().unwrap();
        assert_eq!(paras.len(), 2, "<a:br> splits one <a:p> into two pieces");
        assert_eq!(paras[0].algn, "ctr");
        assert_eq!(paras[0].lvl, 1);
        assert!(matches!(&paras[0].bullet, Some(ParaBullet::Char(c)) if c == "→"));
        let r0 = &paras[0].runs[0];
        assert_eq!(r0.text, "Bold");
        assert_eq!(r0.sz_pt, Some(24.0));
        assert!(r0.bold);
        assert_eq!(r0.color.map(|c| (c.0 * 255.0) as u8), Some(255));
        assert_eq!(r0.font.as_deref(), Some("Comic Sans MS"));
        let r1 = &paras[1].runs[0];
        assert_eq!(r1.text, "rest");
        assert!(r1.italic && r1.underline);
        assert_eq!(r1.sz_pt, Some(12.0));
        assert!(paras[1].follow, "continuation piece after <a:br>");
        assert!(paras[1].bullet.is_none(), "continuation carries no bullet");
        assert_eq!(b.font_pt, Some(24.0), "first run size feeds the box");
        let _ = fs::remove_dir_all(deck.parent().unwrap());
    }

    /// schemeClr resolution goes slot → clrMap → theme key → hex, with
    /// shade transforms; `<p:bgRef idx="1001">` picks the theme's first
    /// background fill; an explicit `<a:ln><a:noFill/>` hides the outline.
    #[test]
    fn pptx_schemeclr_clrmap_theme_and_bgref() {
        let clr_override = r#"<p:clrMap bg1="lt2" tx1="accent1" bg2="bg2" tx2="tx2"
accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4"
accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/>"#;
        let sp = r#"<p:sp><p:nvSpPr><p:cNvPr id="2" name="C"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="10" cy="10"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:ln w="25400"><a:noFill/></a:ln></p:spPr></p:sp>"#;
        let bg = r#"<p:bg><p:bgPr><a:solidFill><a:schemeClr val="accent1"><a:shade val="50000"/></a:schemeClr></a:solidFill></p:bgPr></p:bg>"#;
        let deck = build_deck("pptx_scheme", bg, sp, "", "", "", clr_override);
        let layout = parse_pptx_slide(&deck, 1).unwrap();
        // bg: accent1 (4472C4) shaded 50%
        match &layout.background {
            Some(SlideBackground::Solid(r, g, b)) => {
                assert!((r - (0x44 as f64 / 255.0) * 0.5).abs() < 0.01, "r={r}");
                assert!((g - (0x72 as f64 / 255.0) * 0.5).abs() < 0.01, "g={g}");
                assert!((b - (0xC4 as f64 / 255.0) * 0.5).abs() < 0.01, "b={b}");
            }
            other => panic!("expected shaded solid bg, got {other:?}"),
        }
        // shape fill: schemeClr tx1 → clrMap tx1 → accent1 (4472C4)
        match &layout.elements[0] {
            SlideElement::Shape(sh) => match &sh.fill {
                Some(FillKind::Solid(c, _)) => {
                    assert!((c.0 - 0x44 as f64 / 255.0).abs() < 0.005, "r={:?}", c.0);
                    assert!((c.1 - 0x72 as f64 / 255.0).abs() < 0.005, "g={:?}", c.1);
                    assert!((c.2 - 0xC4 as f64 / 255.0).abs() < 0.005, "b={:?}", c.2);
                }
                other => panic!("expected solid accent1 fill, got {other:?}"),
            },
            other => panic!("expected shape, got {other:?}"),
        }
        // `<a:ln><a:noFill/></a:ln>` explicitly hides the outline
        assert!(matches!(&layout.elements[0], SlideElement::Shape(sh) if sh.line.is_none()));

        // bgRef path: theme bgFillStyleLst[0] (solid lt1 → white)
        let deck2 = build_deck(
            "pptx_bgref",
            r#"<p:bg><p:bgRef idx="1001"><a:schemeClr val="bg1"/></p:bgRef></p:bg>"#,
            "",
            "",
            "",
            "",
            "",
        );
        let layout2 = parse_pptx_slide(&deck2, 1).unwrap();
        assert!(
            matches!(&layout2.background, Some(SlideBackground::Solid(r, g, b)) if *r == 1.0 && *g == 1.0 && *b == 1.0),
            "bgRef 1001 → first bgFillStyleLst entry (lt1 white), got {:?}",
            layout2.background
        );
        let _ = fs::remove_dir_all(deck.parent().unwrap());
        let _ = fs::remove_dir_all(deck2.parent().unwrap());
    }

    /// Master txStyles set title style (36pt bold, centered, theme major
    /// font); body placeholders inherit their level's bullet + 18pt.
    #[test]
    fn pptx_master_txstyles_flow_into_text() {
        let title = text_sp(
            r#"<p:ph type="title"/>"#,
            r#"<a:p><a:r><a:t>T</a:t></a:r></a:p>"#,
            r#"<a:xfrm><a:off x="0" y="0"/><a:ext cx="9" cy="9"/></a:xfrm>"#,
        );
        let body = text_sp(
            r#"<p:ph type="body" idx="1"/>"#,
            r#"<a:p><a:r><a:t>point</a:t></a:r></a:p>"#,
            r#"<a:xfrm><a:off x="1" y="1"/><a:ext cx="2" cy="2"/></a:xfrm>"#,
        );
        let deck = build_deck("pptx_txstyles", "", &format!("{}{}", title, body), "", "", "", "");
        let layout = parse_pptx_slide(&deck, 1).unwrap();
        let SlideElement::Text(t) = &layout.elements[0] else {
            panic!("title")
        };
        let tp = t.paras.as_ref().unwrap();
        assert_eq!(tp[0].algn, "ctr", "titleStyle algn=ctr");
        let r = &tp[0].runs[0];
        assert_eq!(r.sz_pt, Some(36.0), "titleStyle sz=3600");
        assert!(r.bold, "titleStyle b=1");
        assert_eq!(r.font.as_deref(), Some("Contoso Display"), "+mj-lt → theme major");
        assert!(t.centered && t.is_title);

        let SlideElement::Text(b) = &layout.elements[1] else {
            panic!("body")
        };
        let bp = b.paras.as_ref().unwrap();
        assert!(
            matches!(&bp[0].bullet, Some(ParaBullet::Char(c)) if c == "•"),
            "bodyStyle buChar &#x2022; decoded, got {:?}",
            bp[0].bullet
        );
        assert_eq!(bp[0].runs[0].sz_pt, Some(18.0), "bodyStyle sz=1800");
        assert_eq!(
            bp[0].runs[0].font.as_deref(),
            Some("Contoso Sans"),
            "+mn-lt → theme minor"
        );
        let _ = fs::remove_dir_all(deck.parent().unwrap());
    }

    /// Symbol-font bullets (`buChar` PUA + `buFont`) map to real Unicode
    /// glyphs instead of tofu, and `marL`/`indent` reach the paragraph so the
    /// draw path can build the hanging indent.
    #[test]
    fn pptx_bullet_pua_mapping_and_indent_fields() {
        let sp = format!(
            r#"<p:sp><p:nvSpPr><p:cNvPr id="9" name="T"/><p:cNvSpPr/><p:nvPr><p:ph type="body" idx="1"/></p:nvPr></p:nvSpPr><p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="10" cy="10"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr><p:txBody><a:bodyPr/><a:p><a:pPr marL="722313" indent="-273050"><a:buFont typeface="Wingdings 2"/><a:buChar char="{}"/></a:pPr><a:r><a:t>x</a:t></a:r></a:p><a:p><a:r><a:t>y</a:t></a:r></a:p></p:txBody></p:sp>"#,
            '\u{f097}'
        );
        let deck = build_deck("pptx_pua", "", &sp, "", "", "", "");
        let layout = parse_pptx_slide(&deck, 1).unwrap();
        let SlideElement::Text(b) = &layout.elements[0] else {
            panic!("expected text")
        };
        let paras = b.paras.as_ref().unwrap();
        assert_eq!(paras.len(), 2);
        assert!(
            matches!(&paras[0].bullet, Some(ParaBullet::Char(c)) if c == "●"),
            "Wingdings 2 0xF097 → ●, got {:?}",
            paras[0].bullet
        );
        assert_eq!(paras[0].mar_l_emu, 722_313.0, "pPr marL");
        assert_eq!(paras[0].hang_emu, 273_050.0, "|pPr indent|");
        assert_eq!(paras[1].mar_l_emu, 342_900.0, "level marL");
        assert_eq!(paras[1].hang_emu, 342_900.0, "hang defaults to marL");
        let _ = fs::remove_dir_all(deck.parent().unwrap());
    }

    /// A drop-shadow's `<a:srgbClr>` is decoration, not a fill: an rPr with
    /// no `solidFill` must inherit the level color instead of picking up the
    /// shadow's black. `solidFill` beats any other child; a direct color
    /// child (fontRef-style) still resolves.
    #[test]
    fn pptx_shadow_color_is_not_fill_color() {
        let theme = ThemeColors {
            colors: default_theme_colors(),
            major_font: "M".into(),
            minor_font: "m".into(),
            bg_fills: Vec::new(),
        };
        let map = ClrMap::default_map();

        let shadow_only = r#"<a:rPr lang="en-CA" sz="3000" dirty="0"><a:effectLst><a:outerShdw blurRad="50800" dist="38100" dir="2700000"><a:srgbClr val="000000"><a:alpha val="43000"/></a:srgbClr></a:outerShdw></a:effectLst><a:latin typeface="Arial"/></a:rPr>"#;
        assert_eq!(
            parse_color_el(shadow_only, &theme, &map),
            None,
            "shadow black must not become the text color"
        );

        let solid_then_shadow = r#"<a:rPr sz="3000"><a:solidFill><a:schemeClr val="accent1"/></a:solidFill><a:effectLst><a:outerShdw><a:srgbClr val="000000"/></a:outerShdw></a:effectLst></a:rPr>"#;
        assert_eq!(
            parse_color_el(solid_then_shadow, &theme, &map).map(|(c, _a)| c),
            parse_hex_color("4472C4"),
            "solidFill wins over the shadow"
        );

        let font_ref = r#"<a:fontRef idx="minor"><a:schemeClr val="accent3"/></a:fontRef>"#;
        assert_eq!(
            parse_color_el(font_ref, &theme, &map).map(|(c, _a)| c),
            parse_hex_color("A5A5A5"),
            "direct color child still resolves"
        );
    }

    /// End-to-end smoke: parsing + Pango/Cairo rendering yields a real PNG.
    #[test]
    fn pptx_render_smoke_png() {
        let rect = filled_rect(2, 0, 0, 9_144_000, 6_858_000, "FFEEDD");
        let grad = r#"<p:sp><p:nvSpPr><p:cNvPr id="3" name="G"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="500000" y="500000"/><a:ext cx="4000000" cy="3000000"/></a:xfrm><a:prstGeom prst="roundRect"><a:avLst/></a:prstGeom><a:gradFill><a:gsLst><a:gs pos="0"><a:srgbClr val="FF0000"/></a:gs><a:gs pos="100000"><a:srgbClr val="0000FF"/></a:gs></a:gsLst><a:lin ang="0"/></a:gradFill><a:ln w="38100"><a:solidFill><a:srgbClr val="000000"/></a:solidFill></a:ln></p:spPr></p:sp>"#;
        let pic = the_pic(
            r#"<a:xfrm><a:off x="5000000" y="500000"/><a:ext cx="3000000" cy="2000000"/></a:xfrm>"#,
            "",
        );
        let title = text_sp(
            r#"<p:ph type="title"/>"#,
            r#"<a:p><a:r><a:t>Render &gt; smoke</a:t></a:r></a:p>"#,
            r#"<a:xfrm><a:off x="400000" y="400000"/><a:ext cx="8000000" cy="900000"/></a:xfrm>"#,
        );
        let deck = build_deck(
            "pptx_smoke",
            "",
            &format!("{}{}{}{}", rect, grad, pic, title),
            "",
            "",
            "",
            "",
        );
        let layout = parse_pptx_slide(&deck, 1).unwrap();
        let png = render_slide_layout(&layout).expect("render");
        assert!(png.len() > 1000, "png has content ({} bytes)", png.len());
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "png magic");
        let _ = fs::remove_dir_all(deck.parent().unwrap());
    }

    /// End-to-end: the app's real entry point (`compute_preview`, used by
    /// the preview pane) must yield a rendered slide image — this covers the
    /// slide-count, cache lookup, on-demand render, file write, and image
    /// decode glue that the other tests bypass.
    #[test]
    fn pptx_compute_preview_returns_image() {
        let rect = filled_rect(2, 0, 0, 9_144_000, 6_858_000, "FFEEDD");
        let title = text_sp(
            r#"<p:ph type="title"/>"#,
            r#"<a:p><a:r><a:t>Entry point</a:t></a:r></a:p>"#,
            r#"<a:xfrm><a:off x="400000" y="400000"/><a:ext cx="8000000" cy="900000"/></a:xfrm>"#,
        );
        let deck = build_deck(
            "pptx_e2e",
            "",
            &format!("{}{}", rect, title),
            "",
            "",
            "",
            "",
        );
        match compute_preview(&deck) {
            PreviewPayload::Image { w, h, total_pages, page, .. } => {
                assert_eq!(total_pages, Some(1), "slide count");
                assert_eq!(page, Some(1), "first slide");
                assert!(w > 100 && h > 100, "real slide size, got {w}x{h}");
            }
            _ => panic!("compute_preview must return a rendered slide image for a pptx"),
        }
        // The native renderer (not the office fallback) must have produced it.
        let cached = cached_pptx_slide(&deck, 1).expect("native slide cache written");
        // Clean up: temp deck + the render this test wrote into the shared cache.
        if let Some(dir) = cached.parent() {
            let _ = fs::remove_dir_all(dir);
        }
        let _ = fs::remove_dir_all(deck.parent().unwrap());
    }

    /// A chart part parsed as a doughnut: values, per-point colors (theme
    /// scheme + luminance transforms), hole size, start angle.
    const DOUGHNUT_XML: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><c:chart><c:plotArea><c:doughnutChart><c:varyColors val="1"/><c:ser><c:idx val="0"/><c:order val="0"/><c:spPr><a:ln><a:noFill/></a:ln></c:spPr><c:dPt><c:idx val="0"/><c:spPr><a:solidFill><a:schemeClr val="accent4"/></a:solidFill></c:spPr></c:dPt><c:dPt><c:idx val="1"/><c:spPr><a:solidFill><a:schemeClr val="bg1"><a:lumMod val="75000"/></a:schemeClr></a:solidFill></c:spPr></c:dPt><c:val><c:numRef><c:f>Sheet1!$B$2:$B$3</c:f><c:numCache><c:formatCode>General</c:formatCode><c:ptCount val="2"/><c:pt idx="0"><c:v>65</c:v></c:pt><c:pt idx="1"><c:v>35</c:v></c:pt></c:numCache></c:numRef></c:val></c:ser><c:firstSliceAng val="0"/><c:holeSize val="75"/></c:doughnutChart></c:plotArea></c:chart></c:chartSpace>"#;

    #[test]
    fn pptx_doughnut_chart_xml_parses() {
        let theme = ThemeColors {
            colors: default_theme_colors(),
            major_font: "M".into(),
            minor_font: "m".into(),
            bg_fills: Vec::new(),
        };
        let map = ClrMap::default_map();
        let d = parse_doughnut_chart(DOUGHNUT_XML, &theme, &map).expect("doughnut parses");
        assert_eq!(d.values, vec![65.0, 35.0]);
        assert_eq!(d.hole_pct, 75.0);
        assert_eq!(d.first_ang_deg, 0.0);
        assert_eq!(d.colors[0], parse_hex_color("FFC000"), "dPt0 = accent4");
        let c1 = d.colors[1].expect("dPt1 color");
        assert!(
            (c1.0 - 0.75).abs() < 0.01 && (c1.1 - 0.75).abs() < 0.01
                && (c1.2 - 0.75).abs() < 0.01,
            "dPt1 = bg1 lumMod 75% (gray), got {c1:?}"
        );

        // Non-doughnut chart types stay Phase 3 (skipped, not mis-drawn).
        let bar = r#"<c:chartSpace><c:chart><c:plotArea><c:barChart><c:ser><c:val><c:numRef><c:numCache><c:pt idx="0"><c:v>1</c:v></c:pt></c:numCache></c:numRef></c:val></c:ser></c:barChart></c:plotArea></c:chart></c:chartSpace>"#;
        assert!(
            parse_doughnut_chart(bar, &theme, &map).is_none(),
            "bar charts must not parse as doughnut"
        );
    }

    /// `<a:custGeom>` paths parse into freeform commands — the world-map
    /// style geometry that used to fall back to bounding rectangles.
    #[test]
    fn pptx_freeform_custgeom_parses() {
        let sp = r#"<p:sp><p:nvSpPr><p:cNvPr id="7" name="F"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="100000" y="200000"/><a:ext cx="4000000" cy="3000000"/></a:xfrm><a:custGeom><a:avLst/><a:pathLst><a:path w="1000" h="500" fill="norm"><a:moveTo><a:pt x="0" y="0"/></a:moveTo><a:lnTo><a:pt x="1000" y="0"/></a:lnTo><a:cubicBezTo><a:pt x="900" y="100"/><a:pt x="800" y="200"/><a:pt x="700" y="500"/></a:cubicBezTo><a:quadBezTo><a:pt x="500" y="400"/><a:pt x="300" y="500"/></a:quadBezTo><a:close/></a:path><a:path w="1000" h="500" fill="none"><a:moveTo><a:pt x="10" y="10"/></a:moveTo><a:lnTo><a:pt x="20" y="20"/></a:lnTo><a:close/></a:path></a:pathLst></a:custGeom><a:solidFill><a:srgbClr val="FF0000"/></a:solidFill></p:spPr><p:txBody><a:bodyPr/><a:p/></p:txBody></p:sp>"#;
        let deck = build_deck("pptx_freeform", "", sp, "", "", "", "");
        let layout = parse_pptx_slide(&deck, 1).unwrap();
        match &layout.elements[0] {
            SlideElement::Shape(sh) => {
                let ff = sh.freeform.as_ref().expect("custGeom → freeform paths");
                assert_eq!(ff.len(), 2, "both <a:path> subpaths");
                assert_eq!((ff[0].w, ff[0].h, ff[0].fill), (1000.0, 500.0, true));
                assert_eq!(ff[0].cmds.len(), 5, "move/ln/cubic/quad/close");
                match ff[0].cmds[0] {
                    PathCmd::MoveTo(a, b) => assert_eq!((a, b), (0.0, 0.0)),
                    ref other => panic!("expected MoveTo, got {other:?}"),
                }
                match ff[0].cmds[1] {
                    PathCmd::LineTo(a, b) => assert_eq!((a, b), (1000.0, 0.0)),
                    ref other => panic!("expected LineTo, got {other:?}"),
                }
                match ff[0].cmds[2] {
                    PathCmd::CubicBezTo(c) => {
                        assert_eq!(c, [900.0, 100.0, 800.0, 200.0, 700.0, 500.0])
                    }
                    ref other => panic!("expected CubicBezTo, got {other:?}"),
                }
                match ff[0].cmds[3] {
                    PathCmd::QuadBezTo(q) => assert_eq!(q, [500.0, 400.0, 300.0, 500.0]),
                    ref other => panic!("expected QuadBezTo, got {other:?}"),
                }
                assert!(matches!(ff[0].cmds[4], PathCmd::Close));
                assert!(!ff[1].fill, "fill=\"none\" subpath is stroke-only");
            }
            other => panic!("expected freeform shape, got {other:?}"),
        }
        let _ = fs::remove_dir_all(deck.parent().unwrap());
    }

    /// custGeom triangle: filled inside its real outline, background outside
    /// — the bounding box of a freeform must NOT be painted.
    #[test]
    fn pptx_freeform_renders_filled_region() {
        let layout = SlideLayout {
            slide_w: 9_144_000.0,
            slide_h: 6_858_000.0,
            background: None,
            elements: vec![SlideElement::Shape(DrawShape {
                x: 1_000_000.0,
                y: 1_000_000.0,
                w: 3_000_000.0,
                h: 3_000_000.0,
                prst: "rect".into(),
                fill: Some(FillKind::Solid((1.0, 0.0, 0.0), 1.0)),
                line: None,
                rot: 0.0,
                flip_h: false,
                flip_v: false,
                freeform: Some(vec![FreeformPath {
                    w: 1000.0,
                    h: 1000.0,
                    fill: true,
                    cmds: vec![
                        PathCmd::MoveTo(0.0, 0.0),
                        PathCmd::LineTo(1000.0, 0.0),
                        PathCmd::LineTo(500.0, 1000.0),
                        PathCmd::Close,
                    ],
                }]),
            })],
        };
        let png = render_slide_layout(&layout).expect("render");
        let img = image::load_from_memory(&png).unwrap().to_rgba8();
        let s = img.width() as f64 / 9_144_000.0;
        // inside: center of the triangle at 20% height (full-width top edge)
        let inside =
            img.get_pixel(((1_000_000.0 + 1_500_000.0) * s) as u32, ((1_000_000.0 + 600_000.0) * s) as u32).0;
        assert!(inside[0] > 200 && inside[1] < 80 && inside[2] < 80, "inside = red, got {inside:?}");
        // outside: bottom-left corner of the shape box (a rectangle fallback
        // would paint this red — the bug this test pins)
        let outside =
            img.get_pixel(((1_000_000.0 + 100_000.0) * s) as u32, ((1_000_000.0 + 2_950_000.0) * s) as u32).0;
        assert!(
            outside[0] > 240 && outside[1] > 240 && outside[2] > 240,
            "outside = white bg, got {outside:?}"
        );
    }

    /// Doughnut chart: white hole in the middle, red first segment (65% =
    /// 234° clockwise from top), blue remainder.
    #[test]
    fn pptx_doughnut_ring_renders() {
        let layout = SlideLayout {
            slide_w: 9_144_000.0,
            slide_h: 6_858_000.0,
            background: None,
            elements: vec![SlideElement::Chart(DrawChart {
                x: 1_000_000.0,
                y: 1_000_000.0,
                w: 4_000_000.0,
                h: 4_000_000.0,
                doughnut: DoughnutSpec {
                    values: vec![65.0, 35.0],
                    colors: vec![Some((1.0, 0.0, 0.0)), Some((0.0, 0.0, 1.0))],
                    hole_pct: 75.0,
                    first_ang_deg: 0.0,
                },
            })],
        };
        let png = render_slide_layout(&layout).expect("render");
        let img = image::load_from_memory(&png).unwrap().to_rgba8();
        let s = img.width() as f64 / 9_144_000.0;
        let cx = (1_000_000.0 + 2_000_000.0) * s;
        let cy = (1_000_000.0 + 2_000_000.0) * s;
        let r = 2_000_000.0 * s; // outer radius
        let at = |dx: f64, dy: f64| {
            img.get_pixel((cx + dx).round() as u32, (cy + dy).round() as u32).0
        };
        let hole = at(0.0, 0.0);
        assert!(hole[0] > 240 && hole[1] > 240 && hole[2] > 240, "hole = white, got {hole:?}");
        // 15° clockwise from top — well inside segment 0. (Exactly 12 o'clock
        // is the wrap seam where segment 1 overdraws into segment 0's start;
        // sampling there gets a deliberate blend, not either colour.)
        let top = at(0.259 * 0.875 * r, -0.966 * 0.875 * r);
        assert!(top[0] > 180 && top[1] < 90 && top[2] < 90, "top ring = red, got {top:?}");
        let bottom = at(0.0, 0.875 * r); // 180° < 234° — still segment 0
        assert!(bottom[0] > 180 && bottom[1] < 90, "bottom ring = red, got {bottom:?}");
        let left = at(-0.875 * r, 0.0); // 270° — segment 1
        assert!(left[2] > 180 && left[0] < 90, "left ring = blue, got {left:?}");
    }

    /// Append `ppt/charts/chart1.xml` + a slide rel (`rId50`) to a deck built
    /// by `build_deck`, returning the new deck path — the wiring test for
    /// `<p:graphicFrame>` → rId → chart part → parsed doughnut.
    fn add_chart(deck: &std::path::Path, chart_xml: &str) -> std::path::PathBuf {
        use std::io::Read;
        let mut src = zip::ZipArchive::new(fs::File::open(deck).unwrap()).unwrap();
        let mut parts: Vec<(String, Vec<u8>)> = Vec::new();
        for i in 0..src.len() {
            let mut e = src.by_index(i).unwrap();
            let name = e.name().to_string();
            let mut data = Vec::new();
            e.read_to_end(&mut data).unwrap();
            parts.push((name, data));
        }
        for (name, data) in parts.iter_mut() {
            if name == "ppt/slides/_rels/slide1.xml.rels" {
                let mut s = String::from_utf8_lossy(data).to_string();
                s = s.replace(
                    "</Relationships>",
                    r#"<Relationship Id="rId50" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart1.xml"/></Relationships>"#,
                );
                *data = s.into_bytes();
            }
        }
        parts.push(("ppt/charts/chart1.xml".into(), chart_xml.as_bytes().to_vec()));
        let stem = deck
            .parent()
            .and_then(|d| d.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "deck".into());
        build_deck_zip(&format!("{}_chart", stem), &parts)
    }

    /// End-to-end chart wiring: a chart graphicFrame resolves its rel and
    /// parses; without the chart part it is skipped like before.
    #[test]
    fn pptx_chart_graphicframe_wiring() {
        let frame = r#"<p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id="86" name="Chart 85"/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr><p:xfrm><a:off x="10267819" y="4584096"/><a:ext cx="1484845" cy="1610359"/></p:xfrm><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/chart"><c:chart r:id="rId50"/></a:graphicData></a:graphic></p:graphicFrame>"#;
        let deck = build_deck("pptx_chart_frame", "", frame, "", "", "", "");
        // no chart part → skipped (Phase 3 behavior preserved)
        let before = parse_pptx_slide(&deck, 1).unwrap();
        assert!(before.elements.is_empty(), "unresolved chart ref is skipped");

        let deck2 = add_chart(&deck, DOUGHNUT_XML);
        let layout = parse_pptx_slide(&deck2, 1).unwrap();
        match &layout.elements[0] {
            SlideElement::Chart(c) => {
                assert_eq!(
                    (c.x, c.y, c.w, c.h),
                    (10_267_819.0, 4_584_096.0, 1_484_845.0, 1_610_359.0),
                    "frame position from <p:xfrm>"
                );
                assert_eq!(c.doughnut.values, vec![65.0, 35.0]);
                assert_eq!(c.doughnut.hole_pct, 75.0);
                assert!(c.doughnut.colors.iter().all(|c| c.is_some()), "colors resolved");
            }
            other => panic!("expected chart element, got {other:?}"),
        }
        let _ = fs::remove_dir_all(deck.parent().unwrap());
        let _ = fs::remove_dir_all(deck2.parent().unwrap());
    }

    /// QA harness: render every slide of a real deck for visual inspection.
    /// Handles both OOXML decks (.pptx) and legacy binary .ppt (rendered
    /// through the native text-atom pipeline). Skips (passes) unless both
    /// env vars are set:
    /// `SPOTTY_QA_PPTX=/path/deck.pptx SPOTTY_QA_OUT=/tmp/qa cargo test pptx_qa`
    #[test]
    fn pptx_qa_render_all_slides() {
        let (Ok(src), Ok(out)) = (std::env::var("SPOTTY_QA_PPTX"), std::env::var("SPOTTY_QA_OUT"))
        else {
            return; // gated
        };
        let doc = std::path::Path::new(&src);
        if src.to_ascii_lowercase().ends_with(".ppt") {
            let n = legacy_ppt_slide_count(doc).expect("legacy slide count");
            fs::create_dir_all(&out).unwrap();
            let mut census: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
            for slide in 1..=n {
                let png = render_legacy_ppt_slide(doc, slide)
                    .unwrap_or_else(|| panic!("render legacy slide {slide}"));
                fs::write(format!("{}/slide-{}.png", out, slide), &png).unwrap();
                if let Some(layout) = legacy_ppt_structured_layout(doc, slide) {
                    for el in &layout.elements {
                        if let SlideElement::Text(t) = el {
                            if let Some(paras) = &t.paras {
                                for p in paras {
                                    if let Some(ParaBullet::Char(c)) = &p.bullet {
                                        *census.entry(c.clone()).or_insert(0) += 1;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            println!("BULLET-CENSUS {census:?}");
            println!("rendered {n} legacy slides to {out}");
            return;
        }
        let n = pptx_slide_count(doc).expect("slide count");
        fs::create_dir_all(&out).unwrap();
        for slide in 1..=n {
            let layout =
                parse_pptx_slide(doc, slide).unwrap_or_else(|| panic!("parse slide {slide}"));
            let png =
                render_slide_layout(&layout).unwrap_or_else(|| panic!("render slide {slide}"));
            fs::write(format!("{}/slide-{}.png", out, slide), &png).unwrap();
        }
        println!("rendered {n} slides to {out}");
    }

    // ── Office page preparation (prose / sheets / ODF slides) ──

    /// Zip container with an arbitrary name/extension (docx/odt/odp/…).
    fn write_zip(path: &std::path::Path, parts: &[(String, Vec<u8>)]) {
        let f = fs::File::create(path).unwrap();
        let mut w = zip::ZipWriter::new(f);
        for (n, data) in parts {
            w.start_file(n.as_str(), zip::write::FileOptions::default()).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap();
    }

    fn zip_fixture(name: &str, file_name: &str, parts: &[(String, Vec<u8>)]) -> std::path::PathBuf {
        let d = td(name);
        let path = d.join(file_name);
        write_zip(&path, parts);
        path
    }

    fn long_paras(n: usize) -> Vec<String> {
        (0..n)
            .map(|i| {
                format!(
                    "Paragraph {i}: the quick brown fox jumps over the lazy dog while the \
                     indexer walks the whole home directory looking for files to rank."
                )
            })
            .collect()
    }

    fn docx_parts(paras: &[String]) -> Vec<(String, Vec<u8>)> {
        let mut xml = String::from(
            "<?xml version=\"1.0\"?><w:document xmlns:w=\"x\"><w:body>",
        );
        for p in paras {
            xml.push_str(&format!("<w:p><w:r><w:t>{}</w:t></w:r></w:p>", p));
        }
        xml.push_str("</w:body></w:document>");
        vec![("word/document.xml".into(), xml.into_bytes())]
    }

    /// ODT content.xml from raw `<text:p>`/`<text:h>` segments.
    fn odt_parts(segments: &[String]) -> Vec<(String, Vec<u8>)> {
        let xml = format!(
            "<?xml version=\"1.0\"?><office:document-content><office:body><text:body>{}</text:body></office:body></office:document-content>",
            segments.join("")
        );
        vec![("content.xml".into(), xml.into_bytes())]
    }

    /// ODP content.xml: one `<draw:page>` per (title, bullets) slide.
    fn odp_parts(slides: &[(&str, &[&str])]) -> Vec<(String, Vec<u8>)> {
        let mut xml = String::from(
            "<?xml version=\"1.0\"?><office:document-content><office:body>",
        );
        for (title, bullets) in slides {
            xml.push_str(
                "<draw:page draw:name=\"p\"><draw:frame presentation:class=\"title\"><text:p>",
            );
            xml.push_str(title);
            xml.push_str("</text:p></draw:frame><draw:frame presentation:class=\"body\">");
            for b in bullets.iter() {
                xml.push_str(&format!("<text:p>{}</text:p>", b));
            }
            xml.push_str("</draw:frame></draw:page>");
        }
        xml.push_str("</office:body></office:document-content>");
        vec![("content.xml".into(), xml.into_bytes())]
    }

    /// Minimal OPC xlsx with inline-string cells: sheets in order.
    fn xlsx_parts(sheets: &[(&str, Vec<Vec<String>>)]) -> Vec<(String, Vec<u8>)> {
        let mut parts: Vec<(String, Vec<u8>)> = Vec::new();
        let mut ct = String::from(
            "<?xml version=\"1.0\"?><Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/><Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/>",
        );
        let mut wb = String::from(
            "<?xml version=\"1.0\"?><workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><sheets>",
        );
        let mut rels = String::from(
            "<?xml version=\"1.0\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">",
        );
        for (i, (name, rows)) in sheets.iter().enumerate() {
            let n = i + 1;
            ct.push_str(&format!(
                "<Override PartName=\"/xl/worksheets/sheet{n}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>"
            ));
            wb.push_str(&format!(
                "<sheet name=\"{}\" sheetId=\"{n}\" r:id=\"rId{n}\"/>",
                name
            ));
            rels.push_str(&format!(
                "<Relationship Id=\"rId{n}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet{n}.xml\"/>"
            ));
            let mut sheet = String::from(
                "<?xml version=\"1.0\"?><worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><sheetData>",
            );
            for (ri, row) in rows.iter().enumerate() {
                sheet.push_str(&format!("<row r=\"{}\">", ri + 1));
                for (ci, cell) in row.iter().enumerate() {
                    let col = (b'A' + ci as u8) as char;
                    sheet.push_str(&format!(
                        "<c r=\"{col}{}\" t=\"inlineStr\"><is><t>{}</t></is></c>",
                        ri + 1,
                        cell
                    ));
                }
                sheet.push_str("</row>");
            }
            sheet.push_str("</sheetData></worksheet>");
            parts.push((format!("xl/worksheets/sheet{n}.xml"), sheet.into_bytes()));
        }
        ct.push_str("</Types>");
        rels.push_str("</Relationships>");
        wb.push_str("</sheets></workbook>");
        parts.push(("[Content_Types].xml".into(), ct.into_bytes()));
        parts.push((
            "_rels/.rels".into(),
            br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#
                .to_vec(),
        ));
        parts.push(("xl/workbook.xml".into(), wb.into_bytes()));
        parts.push(("xl/_rels/workbook.xml.rels".into(), rels.into_bytes()));
        parts
    }

    fn rm_office_cache(doc: &std::path::Path, n_pages: usize) {
        for n in 1..=n_pages.max(1) {
            let _ = fs::remove_file(office_page_cache_path(doc, n));
        }
    }

    #[test]
    fn office_docx_paginates_and_compute_reports_pages() {
        let paras = long_paras(100);
        let path = zip_fixture("office_docx", "report.docx", &docx_parts(&paras));

        let prep = prepare_office(&path).expect("docx prepares");
        assert!(prep.pages.len() >= 4, "paginates: {} pages", prep.pages.len());
        assert!(matches!(
            prep.pages[0].body,
            OfficePageBody::Prose { first: true, .. }
        ));
        assert!(matches!(
            prep.pages[1].body,
            OfficePageBody::Prose { first: false, .. }
        ));
        // Re-preparation (fresh process / memo eviction) is deterministic.
        let again = build_prepared_office(&path).expect("re-prepares");
        assert_eq!(again.pages.len(), prep.pages.len());
        let total = prep.pages.len();
        for n in 1..=total {
            let png = render_office_page(&prep, n).expect("render page");
            assert!(png.starts_with(b"\x89PNG"), "page {n} is a PNG");
        }

        // The preview pane reports total pages + page 1 for the nav bar.
        match compute_preview(&path) {
            PreviewPayload::Image { total_pages, page, .. } => {
                assert_eq!(page, Some(1));
                assert_eq!(total_pages, Some(total));
            }
            _ => panic!("expected Image payload for paginated docx"),
        }
        // Contentless files stay on the generic card (no placeholder page).
        assert!(office_page_caption(&path, 1).is_none());

        rm_office_cache(&path, total);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn office_odt_and_flat_odt_text() {
        let segs = vec![
            "<text:h>Chapter One</text:h>".to_string(),
            "<text:p>Hello <text:span class=\"T1\">world</text:span> &amp; more</text:p>"
                .to_string(),
            "<text:p/>".to_string(),
            "<text:p>Second paragraph.</text:p>".to_string(),
        ];
        let odt = zip_fixture("office_odt", "notes.odt", &odt_parts(&segs));
        let prep = prepare_office(&odt).expect("odt prepares");
        assert_eq!(prep.pages.len(), 1);
        assert_eq!(prep.doc_title, "Chapter One");
        match &prep.pages[0].body {
            OfficePageBody::Prose { lines, .. } => {
                let joined: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
                assert!(joined.contains(&"Hello world & more"), "{joined:?}");
                assert!(joined.contains(&"Second paragraph."), "{joined:?}");
            }
            other => panic!("expected Prose, got {other:?}"),
        }

        // Flat ODT (.fodt) parses through the same scanner.
        let flat = td("office_fodt").join("flat.fodt");
        fs::write(&flat, &odt_parts(&segs)[0].1).unwrap();
        let prep2 = prepare_office(&flat).expect("fodt prepares");
        assert_eq!(prep2.doc_title, "Chapter One");

        rm_office_cache(&odt, 1);
        rm_office_cache(&flat, 1);
        let _ = fs::remove_dir_all(odt.parent().unwrap());
        let _ = fs::remove_dir_all(flat.parent().unwrap());
    }

    #[test]
    fn office_rtf_extracts_body_skipping_tables() {
        let d = td("office_rtf");
        let p = d.join("letter.rtf");
        fs::write(
            &p,
            r"{\rtf1\ansi\deff0{\fonttbl{\f0\fswiss Helvetica;}}{\colortbl;\red255\green0\blue0;}{\info{\title Quarterly Report}}\f0\fs24 First paragraph of the report.\par Second paragraph with a {\b bold} word and an emdash \emdash here.\par Third paragraph closing the document.}",
        )
        .unwrap();

        let prep = prepare_office(&p).expect("rtf prepares");
        assert_eq!(prep.doc_title, "First paragraph of the report.");
        match &prep.pages[0].body {
            OfficePageBody::Prose { lines, .. } => {
                let joined: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
                let all = joined.join("\n");
                assert!(all.contains("bold word"), "{all}");
                assert!(all.contains('\u{2014}'), "emdash decoded: {all}");
                assert!(
                    !all.contains("Helvetica") && !all.contains("Quarterly"),
                    "font table / metadata skipped: {all}"
                );
            }
            other => panic!("expected Prose, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn office_xlsx_sheet_pages_and_captions() {
        let rows1: Vec<Vec<String>> = (0..35)
            .map(|r| vec![format!("Item {r}"), (r * 3).to_string()])
            .collect();
        let rows2: Vec<Vec<String>> = (0..20)
            .map(|r| vec![format!("Stat {r}"), format!("{r}.5")])
            .collect();
        let path = zip_fixture(
            "office_xlsx",
            "book.xlsx",
            &xlsx_parts(&[("Data", rows1), ("Stats", rows2)]),
        );

        let prep = prepare_office(&path).expect("xlsx prepares");
        // One page per sheet — Data (35 rows) + Stats (20 rows) = 2 pages,
        // never row-chunked.
        assert_eq!(prep.pages.len(), 2);
        let caps: Vec<Option<String>> = prep
            .pages
            .iter()
            .map(|p| p.caption.clone())
            .collect();
        assert_eq!(
            caps,
            vec![Some("Data".to_string()), Some("Stats".to_string())]
        );
        match &prep.pages[0].body {
            OfficePageBody::Grid {
                sheet,
                rows,
                row_base,
                ..
            } => {
                assert_eq!(sheet, "Data");
                assert_eq!(rows.len(), 35);
                assert_eq!(rows[0][0], "Item 0");
                assert_eq!(*row_base, 0);
            }
            other => panic!("expected Grid, got {other:?}"),
        }

        // Shared resolver renders sheet pages on demand; caption helper too.
        let png = resolve_page_png(&path, 2).expect("sheet page 2 resolves");
        assert!(sane_png_file(&png));
        assert_eq!(resolve_page_png(&path, 2).as_deref(), Some(png.as_path()));
        assert_eq!(office_page_caption(&path, 2).as_deref(), Some("Stats"));

        rm_office_cache(&path, 2);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// One or more sheets with REAL style data: every cell carries a style
    /// index — 0 plain, 1 bold red on an amber fill, 2 a blue underlined
    /// theme hyperlink, 3 the `0.000` number format, 4 the gray125
    /// placeholder fill (which must never paint) — plus the styles.xml and
    /// theme1.xml that define them. Values parsing as numbers are written
    /// as numbers so numFmt applies.
    fn xlsx_styled_parts(
        sheets: &[(&str, Vec<Vec<(String, u16)>>)],
    ) -> Vec<(String, Vec<u8>)> {
        const STYLES_XML: &str = r#"<?xml version="1.0"?><styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><numFmts count="1"><numFmt numFmtId="164" formatCode="0.000"/></numFmts><fills count="3"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill><fill><patternFill patternType="solid"><fgColor rgb="FFFFCC00"/></patternFill></fill></fills><fonts count="3"><font><sz val="11"/><color theme="1"/><name val="Calibri"/></font><font><b/><sz val="11"/><color rgb="FFCC0000"/><name val="Calibri"/></font><font><u/><sz val="11"/><color theme="10"/><name val="Calibri"/></font></fonts><cellXfs count="5"><xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/><xf numFmtId="0" fontId="1" fillId="2" borderId="0" xfId="0"/><xf numFmtId="0" fontId="2" fillId="0" borderId="0" xfId="0"/><xf numFmtId="164" fontId="0" fillId="0" borderId="0" xfId="0"/><xf numFmtId="0" fontId="0" fillId="1" borderId="0" xfId="0"/></cellXfs></styleSheet>"#;
        // The hlink slot is deliberately NOT the Office default, so the
        // blue assertion only passes when THIS theme1.xml was read.
        const THEME_XML: &str = r#"<?xml version="1.0"?><a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:themeElements><a:clrScheme name="QA"><a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1><a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1><a:dk2><a:srgbClr val="44546A"/></a:dk2><a:lt2><a:srgbClr val="E7E6E6"/></a:lt2><a:accent1><a:srgbClr val="4472C4"/></a:accent1><a:accent2><a:srgbClr val="ED7D31"/></a:accent2><a:accent3><a:srgbClr val="A5A5A5"/></a:accent3><a:accent4><a:srgbClr val="FFC000"/></a:accent4><a:accent5><a:srgbClr val="5B9BD5"/></a:accent5><a:accent6><a:srgbClr val="70AD47"/></a:accent6><a:hlink><a:srgbClr val="123456"/></a:hlink><a:folHlink><a:srgbClr val="954F72"/></a:folHlink></a:clrScheme></a:themeElements></a:theme>"#;

        let plain: Vec<(&str, Vec<Vec<String>>)> = sheets
            .iter()
            .map(|(name, rows)| {
                let texts: Vec<Vec<String>> = rows
                    .iter()
                    .map(|r| r.iter().map(|(t, _)| t.clone()).collect())
                    .collect();
                (*name, texts)
            })
            .collect();
        let mut parts = xlsx_parts(&plain);
        // Rebuild each sheet part with the per-cell style indices.
        for (i, (_, rows)) in sheets.iter().enumerate() {
            let n = i + 1;
            let mut xml = String::from(
                "<?xml version=\"1.0\"?><worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><sheetData>",
            );
            for (ri, row) in rows.iter().enumerate() {
                xml.push_str(&format!("<row r=\"{}\">", ri + 1));
                for (ci, (cell, s)) in row.iter().enumerate() {
                    let col = (b'A' + ci as u8) as char;
                    if cell.parse::<f64>().is_ok() {
                        xml.push_str(&format!(
                            "<c r=\"{col}{}\" s=\"{s}\"><v>{cell}</v></c>",
                            ri + 1
                        ));
                    } else {
                        xml.push_str(&format!(
                            "<c r=\"{col}{}\" s=\"{s}\" t=\"inlineStr\"><is><t>{cell}</t></is></c>",
                            ri + 1
                        ));
                    }
                }
                xml.push_str("</row>");
            }
            xml.push_str("</sheetData></worksheet>");
            let part = format!("xl/worksheets/sheet{n}.xml");
            parts.retain(|(name, _)| name != &part);
            parts.push((part, xml.into_bytes()));
        }
        parts.push(("xl/styles.xml".into(), STYLES_XML.as_bytes().to_vec()));
        parts.push(("xl/theme/theme1.xml".into(), THEME_XML.as_bytes().to_vec()));
        parts
    }

    /// The style half of the xlsx pass resolves the file's own paint —
    /// fills, text colors (rgb + theme slots), weights — while plain cells
    /// and no-op fills stay unrecorded; numFmt still formats alongside.
    #[test]
    fn xlsx_styles_resolve_real_colors_and_formats() {
        let rows: Vec<Vec<(String, u16)>> = vec![
            vec![
                ("Region".into(), 1),
                ("Link".into(), 2),
                ("Plain".into(), 0),
                ("1.5".into(), 3),
            ],
            vec![
                ("Ghost".into(), 4),
                ("x".into(), 0),
                ("y".into(), 0),
                ("z".into(), 0),
            ],
        ];
        let path = zip_fixture(
            "office_xlsx_styled",
            "styled.xlsx",
            &xlsx_styled_parts(&[
                ("Data", rows),
                (
                    "Late",
                    vec![vec![
                        ("T1".into(), 0),
                        ("T2".into(), 0),
                        ("T3".into(), 0),
                        ("4".into(), 0),
                    ]],
                ),
            ]),
        );

        let info = xlsx_cell_info(&path, &["Data".to_string(), "Late".to_string()])
            .expect("styles.xml read");
        let st = info.styles.as_ref().expect("styled cells recorded");
        // A1: bold red text on the file's amber fill.
        let a1 = st.pool[st.pool_index(0, 0, 0).expect("A1 carries a style") as usize];
        assert_eq!(a1.fill, Some([255, 204, 0]), "fill from <fgColor>");
        assert_eq!(a1.text, Some([204, 0, 0]), "text from <color rgb>");
        assert!(a1.bold && !a1.italic);
        // B1: theme index 10 = the hyperlink slot of THIS theme1.xml.
        let b1 = st.pool[st.pool_index(0, 1, 0).expect("B1 carries a style") as usize];
        assert_eq!(b1.fill, None);
        assert_eq!(b1.text, Some([0x12, 0x34, 0x56]), "theme hlink slot");
        assert!(b1.underline && !b1.bold);
        // Plain cells, default-looking xfs, and the gray125 placeholder
        // fill are never recorded — nothing extra gets painted.
        assert!(st.pool_index(0, 2, 0).is_none(), "C1 is plain");
        assert!(st.pool_index(0, 0, 1).is_none(), "gray125 never paints");
        // Same coordinates on a LATER sheet keep their own plain look:
        // styles are keyed per sheet, never by bare (row, col) — otherwise
        // every sheet's A1 would inherit the last styled one's paint.
        assert!(
            st.pool_index(1, 0, 0).is_none(),
            "Late!A1 must not inherit Data's amber"
        );
        // The numFmt half of the same pass still works alongside styles.
        let fmts = info.formats.as_ref().expect("0.000 format recorded");
        assert_eq!(fmts.pool[0], "0.000", "custom code from <numFmts>");

        // End to end: styled cells ride into the grid page, and the number
        // shows as Excel would print it.
        let prep = prepare_office(&path).expect("prepares");
        match &prep.pages[0].body {
            OfficePageBody::Grid {
                rows,
                styles,
                style_pool,
                ..
            } => {
                assert_eq!(rows[0][3], "1.500");
                assert_eq!(styles[0][3], u16::MAX, "xf3 looks plain");
                assert!(style_pool.len() >= 2, "pool shared with the render");
            }
            other => panic!("expected Grid, got {other:?}"),
        }

        rm_office_cache(&path, 2);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// The sheet page is 640×480 — 2× the preview box, so the pane's fit
    /// lands at readable size — and paints the FILE's colors: amber header
    /// row (the old render's invented green fails this), white body.
    #[test]
    fn sheet_page_fits_preview_box_with_file_colors() {
        let mut rows: Vec<Vec<(String, u16)>> = vec![vec![
            ("Region".into(), 1),
            ("Code".into(), 1),
            ("Value".into(), 1),
            ("Note".into(), 1),
            ("Rank".into(), 1),
            ("Trend".into(), 1),
        ]];
        for i in 1..=12 {
            rows.push(vec![
                (format!("R{i}"), 0),
                (format!("C{i}"), 0),
                ("10".into(), 0),
                ("note".into(), 0),
            ]);
        }
        let path = zip_fixture(
            "office_xlsx_pixmap",
            "pix.xlsx",
            &xlsx_styled_parts(&[("Data", rows)]),
        );

        let prep = prepare_office(&path).expect("prepares");
        let png = render_office_page(&prep, 1).expect("sheet page renders");
        let img = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(
            (img.width(), img.height()),
            (640, 480),
            "sheet page is 2× the 320×240 preview box"
        );
        let head = img.get_pixel(600, 52).0;
        assert!(
            head[0] > 240 && (185..=220).contains(&head[1]) && head[2] < 40,
            "header = file amber (255,204,0), got {head:?}"
        );
        // A styled sheet's body is plain white: no zebra, no invented hue.
        let body = img.get_pixel(600, 150).0;
        assert!(
            body[0] > 250 && body[1] > 250 && body[2] > 250,
            "body = white, got {body:?}"
        );

        rm_office_cache(&path, 1);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// Formats with no style data keep a plain white header (never the old
    /// invented green) plus the quiet zebra on alternating rows.
    #[test]
    fn csv_sheet_page_never_invents_a_colored_header() {
        let d = td("office_csv_colors");
        let csv = d.join("plain.csv");
        let mut text = String::from("name,value,note\n");
        for i in 0..20 {
            text.push_str(&format!("item{i},{i},note{i}\n"));
        }
        fs::write(&csv, &text).unwrap();

        let prep = prepare_office(&csv).expect("csv prepares");
        let png = render_office_page(&prep, 1).expect("csv page renders");
        let img = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!((img.width(), img.height()), (640, 480));
        let head = img.get_pixel(600, 52).0;
        assert!(
            head[0] > 250 && head[1] > 250 && head[2] > 250,
            "header = white, got {head:?}"
        );
        let zebra = img.get_pixel(600, 77).0;
        assert!(
            (240..=251).contains(&zebra[0]) && (243..=252).contains(&zebra[2]),
            "row 2 = quiet zebra band, got {zebra:?}"
        );

        rm_office_cache(&csv, 1);
        let _ = fs::remove_dir_all(&d);
    }

    /// The result row's overview is the real page-1 render (downscaled to
    /// the 320×240 card), not the old synthesized gray grid card.
    #[test]
    fn xlsx_row_overview_is_the_real_sheet_render() {
        let mut rows: Vec<Vec<(String, u16)>> = vec![vec![
            ("Region".into(), 1),
            ("Code".into(), 1),
            ("Value".into(), 1),
            ("Note".into(), 1),
            ("Rank".into(), 1),
            ("Trend".into(), 1),
        ]];
        for i in 1..=6 {
            rows.push(vec![
                (format!("R{i}"), 0),
                (format!("C{i}"), 0),
                ("10".into(), 0),
            ]);
        }
        let path = zip_fixture(
            "office_xlsx_row",
            "row.xlsx",
            &xlsx_styled_parts(&[("Data", rows)]),
        );

        let thumb = crate::thumbnails::thumbnail_for(&path).expect("row thumbnail");
        let img = image::open(&thumb).unwrap().to_rgba8();
        assert_eq!((img.width(), img.height()), (320, 240), "row card size");
        let px = img.get_pixel(300, 30).0;
        assert!(
            px[0] > 240 && (185..=220).contains(&px[1]) && px[2] < 40,
            "overview = real amber sheet render, got {px:?}"
        );

        let _ = fs::remove_file(&thumb);
        rm_office_cache(&path, 1);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn office_odp_text_slides_resolve_pages() {
        let path = zip_fixture(
            "office_odp",
            "deck.odp",
            &odp_parts(&[
                ("Title One", &["Bullet A", "Bullet B"]),
                ("Title Two", &["Only bullet"]),
                ("Title Three", &[]),
            ]),
        );

        let prep = prepare_office(&path).expect("odp prepares");
        assert_eq!(office_page_count(&path), Some(3));
        match &prep.pages[0].body {
            OfficePageBody::Slide { title, bullets } => {
                assert_eq!(title, "Title One");
                assert_eq!(bullets, &["Bullet A".to_string(), "Bullet B".to_string()]);
            }
            other => panic!("expected Slide, got {other:?}"),
        }
        match &prep.pages[1].body {
            OfficePageBody::Slide { title, .. } => assert_eq!(title, "Title Two"),
            other => panic!("expected Slide, got {other:?}"),
        }
        for n in 1..=3 {
            let png = render_office_page(&prep, n).expect("render slide");
            assert!(png.starts_with(b"\x89PNG"));
        }

        // The shared page resolver walks ODP like pptx/PDF.
        let png2 = resolve_page_png(&path, 2).expect("odp page 2 resolves");
        assert!(sane_png_file(&png2));
        assert_eq!(resolve_page_png(&path, 2).as_deref(), Some(png2.as_path()));

        rm_office_cache(&path, 3);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn office_numfmt_displays_like_excel() {
        // Decimal + grouping from the code, rounded half-up like Excel.
        assert_eq!(
            apply_num_format("###,###,##0.000", 124.15602).as_deref(),
            Some("124.156")
        );
        assert_eq!(apply_num_format("##0.0", 283.25).as_deref(), Some("283.3"));
        assert_eq!(
            apply_num_format("#,##0.00", 1234.5).as_deref(),
            Some("1,234.50")
        );
        assert_eq!(apply_num_format("#,##0", 1947.0).as_deref(), Some("1,947"));
        // Percent scales ×100 and keeps the sign.
        assert_eq!(apply_num_format("0.00%", 0.4532).as_deref(), Some("45.32%"));
        // Quoted literals and locale currency tags in place.
        assert_eq!(
            apply_num_format("\"€\"#,##0.00", 1234.5).as_deref(),
            Some("€1,234.50")
        );
        assert_eq!(
            apply_num_format("[$€-407] #,##0.00", 9.0).as_deref(),
            Some("€ 9.00")
        );
        // A date code on a plain number cell renders a date, never a serial.
        assert_eq!(
            apply_num_format("yyyy-mm-dd", 44927.0).as_deref(),
            Some("2023-01-01")
        );
        // Unsupported shapes fall back to None → the exact value shows.
        assert_eq!(apply_num_format("General", 1.5), None);
        assert_eq!(apply_num_format("0.00E+00", 1.5), None);
    }

    #[test]
    fn office_csv_and_fods_grid_pages() {
        // 70 CSV rows → one page (rows are never split into swipe pages).
        let d = td("office_csv");
        let csv = d.join("data.csv");
        let mut text = String::from("name,value\n");
        for i in 0..69 {
            text.push_str(&format!("row{i},{}\n", i * 7));
        }
        fs::write(&csv, &text).unwrap();
        let prep = prepare_office(&csv).expect("csv prepares");
        assert_eq!(prep.pages.len(), 1, "one page per sheet");
        assert_eq!(office_page_caption(&csv, 1).as_deref(), Some("data"));
        match &prep.pages[0].body {
            OfficePageBody::Grid { rows, .. } => assert_eq!(rows.len(), 70),
            other => panic!("expected Grid, got {other:?}"),
        }

        // Flat ODS (.fods): rows/cells split from raw XML.
        let fods = td("office_fods").join("table.fods");
        fs::write(
            &fods,
            r#"<?xml version="1.0"?><office:document><office:body><table:table table:name="S1"><table:table-row><table:table-cell office:value="42"><text:p>42</text:p></table:table-cell><table:table-cell><text:p>Name</text:p></table:table-cell></table:table-row><table:table-row><table:table-cell><text:p>Beta</text:p></table:table-cell></table:table-row><table:table-row><table:table-cell><text:p>Gamma</text:p></table:table-cell></table:table-row></table:table></office:body></office:document>"#,
        )
        .unwrap();
        let prep2 = prepare_office(&fods).expect("fods prepares");
        assert_eq!(prep2.pages.len(), 1);
        match &prep2.pages[0].body {
            OfficePageBody::Grid { rows, .. } => {
                assert_eq!(rows.len(), 3);
                assert_eq!(rows[0][0], "42");
                assert_eq!(rows[1][0], "Beta");
            }
            other => panic!("expected Grid, got {other:?}"),
        }

        rm_office_cache(&csv, 1);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn office_empty_and_unparsed_files_stay_cards() {
        // 0-byte file: no prepared pages → generic info card path (None).
        let d = td("office_empty");
        let zero = d.join("zero.docx");
        fs::write(&zero, b"").unwrap();
        assert!(prepare_office(&zero).is_none());
        assert!(office_page_count(&zero).is_none());

        // Structurally empty ODT: parses, but nothing to lay out.
        let empty = zip_fixture("office_empty_odt", "empty.odt", &odt_parts(&[]));
        assert!(prepare_office(&empty).is_none());

        // .doc has no structured parser: stays on the single strings card.
        let docd = td("office_doc_legacy");
        let doc = docd.join("old.doc");
        fs::write(&doc, b"\xd0\xcf\x11\xe0 some legacy bytes").unwrap();
        assert!(prepare_office(&doc).is_none());

        let _ = fs::remove_dir_all(&d);
        let _ = fs::remove_dir_all(empty.parent().unwrap());
        let _ = fs::remove_dir_all(&docd);
    }

    // ── QA harness (env-gated, like pptx_qa) ──

    /// Build every office fixture into SPOTTY_QA_FIXTURES_OUT for visual QA:
    ///   SPOTTY_QA_FIXTURES_OUT=/tmp/opencode/office_fixtures cargo test office_qa_build -- --nocapture
    #[test]
    fn office_qa_build_fixtures() {
        let Ok(out) = std::env::var("SPOTTY_QA_FIXTURES_OUT") else {
            return;
        };
        let out = std::path::PathBuf::from(&out);
        fs::create_dir_all(&out).unwrap();

        write_zip(&out.join("long-report.docx"), &docx_parts(&long_paras(100)));
        write_zip(
            &out.join("notes.odt"),
            &odt_parts(&[
                "<text:h>Chapter One</text:h>".to_string(),
                "<text:p>These are the meeting notes for the quarterly review, kept as plain paragraphs that wrap across the A4 page width to exercise the paginator.</text:p>".to_string(),
                "<text:p>The indexer should pick this file up without any external converter.</text:p>".to_string(),
            ]),
        );
        fs::write(
            out.join("flat.fodt"),
            &odt_parts(&[
                "<text:h>Flat Document</text:h>".to_string(),
                "<text:p>Same text, flat ODF container, no zip.</text:p>".to_string(),
            ])[0]
                .1,
        )
        .unwrap();
        fs::write(
            out.join("letter.rtf"),
            r"{\rtf1\ansi\deff0{\fonttbl{\f0\fswiss Helvetica;}}{\info{\title Meta}}\f0\fs24 Dear colleague,\par This letter has several paragraphs so the RTF extractor proves it can paginate a real body of text.\par Regards,\par The Spotty Team}",
        )
        .unwrap();

        let rows1: Vec<Vec<String>> = (0..64)
            .map(|r| vec![format!("Item {r}"), (r * 3).to_string(), format!("v{r}")])
            .collect();
        let rows2: Vec<Vec<String>> = (0..20)
            .map(|r| vec![format!("Stat {r}"), format!("{r}.5")])
            .collect();
        write_zip(
            &out.join("book.xlsx"),
            &xlsx_parts(&[("Data", rows1), ("Stats", rows2)]),
        );

        write_zip(
            &out.join("deck.odp"),
            &odp_parts(&[
                ("Overview", &["First point", "Second point", "Third point"]),
                ("Architecture", &["Single GTK thread", "Background indexer", "futures, not tokio"]),
                ("Roadmap", &["Ship the overview", "Wire row thumbnails"]),
            ]),
        );

        let mut csv = String::from("name,value\n");
        for i in 0..69 {
            csv.push_str(&format!("row{i},{}\n", i * 7));
        }
        fs::write(out.join("data.csv"), csv).unwrap();
        fs::write(
            out.join("table.fods"),
            r#"<?xml version="1.0"?><office:document><office:body><table:table table:name="S1"><table:table-row><table:table-cell office:value="42"><text:p>42</text:p></table:table-cell><table:table-cell><text:p>Name</text:p></table:table-cell></table:table-row><table:table-row><table:table-cell><text:p>Beta</text:p></table:table-cell></table:table-row><table:table-row><table:table-cell><text:p>Gamma</text:p></table:table-cell></table:table-row></table:table></office:body></office:document>"#,
        )
        .unwrap();

        println!("office fixtures written to {out:?}");
    }

    /// Render EVERY native page of one office document as PNGs:
    ///   SPOTTY_QA_DOC=/path/file.xlsx SPOTTY_QA_OUT=/tmp/opencode/office_qa cargo test office_qa_render -- --nocapture
    #[test]
    fn office_qa_render_all_pages() {
        let Ok(src) = std::env::var("SPOTTY_QA_DOC") else {
            return;
        };
        let Ok(out) = std::env::var("SPOTTY_QA_OUT") else {
            panic!("set SPOTTY_QA_OUT together with SPOTTY_QA_DOC");
        };
        let doc = std::path::Path::new(&src);
        let prep = prepare_office(doc).unwrap_or_else(|| panic!("no prepared pages for {src}"));
        fs::create_dir_all(&out).unwrap();
        for n in 1..=prep.pages.len() {
            let png = render_office_page(&prep, n).unwrap_or_else(|| panic!("render page {n}"));
            fs::write(
                std::path::Path::new(&out).join(format!("page-{n:03}.png")),
                &png,
            )
            .unwrap();
        }
        println!(
            "{} pages rendered to {out} — caption(1) = {:?}",
            prep.pages.len(),
            office_page_caption(doc, 1)
        );
    }
}
