//! Built-in thumbnail generation for .pptx and .xlsx files.
//!
//! Tier 0 (presentations): the **real rendered slide** — the same PNG the
//! preview pane shows (native renderer), downscaled to card size.
//! Tier 1: embedded `docProps/thumbnail` extraction (zero rendering).
//! Tier 2: synthesized content card (pptx) or grid (xlsx) via Cairo — the
//! fallback when nothing above produces an image (sheets, or a deck the
//! renderer can't parse).
//!
//! Tier 0 output bypasses this module's card cache (it lives in the
//! preview's versioned render cache), so list icons can't go stale when the
//! renderer changes. Cards/grids are cached under
//! `$XDG_CACHE_HOME/spotty/thumbnails/` keyed by `(path, mtime, size)`,
//! never regenerated for unchanged files.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

// ── card constants ──────────────────────────────────────────────────
const CARD_W: u32 = 320;
const CARD_H: u32 = 240;
const THUMB_VERSION: u32 = 2;
const MAX_ENTRY_BYTES: u64 = 8 * 1024 * 1024;

// ── memo (in-memory; avoids repeated disk stat + cache reads) ───────
#[derive(Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    path: PathBuf,
    mtime: u64,
    size: u64,
}

#[derive(Clone)]
enum ThumbSlot {
    Pending,
    Done(Option<PathBuf>),
}

fn memo() -> &'static Mutex<HashMap<CacheKey, ThumbSlot>> {
    static M: OnceLock<Mutex<HashMap<CacheKey, ThumbSlot>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

// ── public API ──────────────────────────────────────────────────────

/// Document extensions whose result rows get a generated overview
/// thumbnail (rendered page/slide/grid or embedded preview). Images and
/// everything else keep their normal icons.
const ROW_THUMB_EXTS: &[&str] = &[
    "pdf",
    "pptx", "ppsx", "pps", "ppt", "pptm", "ppsm", "potx", "potm", "odp", "otp", "fodp",
    "docx", "doc", "odt", "ott", "fodt", "rtf", "wps",
    "xlsx", "xls", "ods", "ots", "fods", "csv",
];

/// Image files whose row overview is the image itself, downscaled — for a
/// picture the overview IS the picture (they used to show nothing but the
/// generic `image-x-generic-symbolic` icon).
const IMAGE_ROW_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "webp", "gif", "bmp", "ico", "tiff", "tif", "avif", "heic", "heif",
];

/// Request a thumbnail for `path` into `icon`. Fast-path: if cached, swap
/// the icon synchronously (stat + in-memory memo); otherwise spawn a worker.
pub fn request(icon: &gtk::Image, path: &Path) {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    if !ROW_THUMB_EXTS.contains(&ext.as_str()) && !IMAGE_ROW_EXTS.contains(&ext.as_str()) {
        return;
    }
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return,
    };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let key = CacheKey {
        path: path.to_path_buf(),
        mtime,
        size: meta.len(),
    };

    // Fast-path: already computed or in flight.
    let cached = memo().lock().unwrap().get(&key).cloned();
    if let Some(ref slot) = cached {
        if let ThumbSlot::Done(Some(p)) = slot {
            icon.set_from_file(Some(p));
            return;
        }
        return; // Pending or Done(None)
    }

    // Mark pending and spawn a worker (per-request thread, deduped by memo).
    memo().lock().unwrap().insert(key.clone(), ThumbSlot::Pending);
    let icon = icon.clone();
    let (tx, rx) = futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let thumb = thumbnail_for(&key.path);
        memo().lock().unwrap().insert(key, ThumbSlot::Done(thumb.clone()));
        let _ = tx.send(thumb);
    });
    glib::MainContext::default().spawn_local(async move {
        if let Some(path) = rx.await.ok().flatten() {
            icon.set_from_file(Some(path));
        }
    });
}

// ── entry point ─────────────────────────────────────────────────────

/// Generate a thumbnail for `path`, returning the PNG path on success.
pub fn thumbnail_for(path: &Path) -> Option<PathBuf> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let key = CacheKey {
        path: path.to_path_buf(),
        mtime,
        size: meta.len(),
    };

    let cache_path = cache_path_for(&key);

    // Tier 0 — presentations: the actual rendered slide, the same image the
    // preview pane shows. Checked before the card cache so result icons are
    // the faithful render and never a stale synthesized text card.
    if let Some(slide_png) = crate::preview::pptx_first_slide_png(path) {
        return match downscale_slide_icon(&slide_png, &cache_path) {
            Some(small) => Some(small),
            None => Some(slide_png),
        };
    }

    if cache_path.exists() && cache_path.metadata().map(|m| m.len() > 0).unwrap_or(false) {
        return Some(cache_path);
    }

    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    // Images: the picture itself, downscaled — no render pass involved.
    if IMAGE_ROW_EXTS.contains(&ext.as_str()) {
        return downscale_image_icon(path, &cache_path);
    }
    // PDF pages and office page-1/card previews come back as full-size
    // files — compute, then downscale into the row cache.
    match ext.as_str() {
        "pdf" => {
            return reuse_full_preview(crate::preview::pdf_thumbnail(path), &cache_path);
        }
        // Real page-1 renders: the office pager covers ODF presentations
        // too, so .odp/.fodp show their actual first slide here instead of
        // a stale embedded image or a text card.
        "doc" | "docx" | "odt" | "ott" | "fodt" | "rtf" | "wps" | "ppt" | "otp"
        | "ots" | "fods" | "xls" | "odp" | "fodp" => {
            return reuse_full_preview(crate::preview::office_thumbnail(path), &cache_path);
        }
        // Spreadsheets: the real page-1 render carries the file's actual
        // cell colors — a synthesized gray card is only the fallback when
        // the workbook can't be prepared at all (falls through below).
        "xlsx" | "ods" | "csv" => {
            if let Some(icon) =
                reuse_full_preview(crate::preview::office_thumbnail(path), &cache_path)
            {
                return Some(icon);
            }
        }
        _ => {}
    }

    let png = match ext.as_str() {
        // PowerPoint family fallback (Tier 0 already tried the real slide):
        // embedded image, else the synthesized layout card.
        "pptx" | "ppsx" | "pps" | "pptm" | "ppsm" | "potx" | "potm" => {
            tier1_embedded_thumbnail(path).or_else(|| render_pptx_card(path))
        }
        "xlsx" | "ods" | "csv" => render_xlsx_grid(path),
        _ => return None,
    };

    if let Some(ref buf) = png {
        let _ = std::fs::create_dir_all(cache_path.parent().unwrap());
        let _ = std::fs::write(&cache_path, buf);
        Some(cache_path)
    } else {
        None
    }
}

// ── cache path ──────────────────────────────────────────────────────

fn cache_path_for(key: &CacheKey) -> PathBuf {
    let raw = format!(
        "{}|{}|{}|v{}",
        key.path.display(),
        key.mtime,
        key.size,
        THUMB_VERSION,
    );
    let hash = crate::md5::hex(raw.as_bytes());
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("spotty")
        .join("thumbnails")
        .join(format!("{}.png", hash))
}

// ── Tier 0: faithful native slide render, downscaled ────────────────

/// Shrink the native slide PNG (e.g. 1200×900) to card size so the result
/// row keeps its layout — the preview pane still shows full resolution.
fn downscale_slide_icon(full_png: &Path, out: &Path) -> Option<PathBuf> {
    let img = image::open(full_png).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    let scale = (CARD_W as f32 / w as f32)
        .min(CARD_H as f32 / h as f32)
        .min(1.0);
    let (nw, nh) = if scale < 1.0 {
        (
            ((w as f32 * scale).round() as u32).max(1),
            ((h as f32 * scale).round() as u32).max(1),
        )
    } else {
        (w, h)
    };
    let scaled = image::imageops::resize(&rgba, nw, nh, image::imageops::FilterType::Lanczos3);
    let mut buf = Vec::new();
    image::DynamicImage::ImageRgba8(scaled)
        .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .ok()?;
    let _ = std::fs::create_dir_all(out.parent()?);
    std::fs::write(out, &buf).ok()?;
    Some(out.to_path_buf())
}

// ── Tier 0b: image files ────────────────────────────────────────────

/// The picture itself, downscaled into the row cache — for an image file
/// the overview IS the image. Formats `image` can't decode (and absurdly
/// large files) return None, so the row simply keeps its generic icon
/// instead of showing something wrong.
fn downscale_image_icon(src: &Path, out: &Path) -> Option<PathBuf> {
    // A row icon isn't worth a multi-hundred-MB decode spike.
    if std::fs::metadata(src).ok()?.len() > 64 * 1024 * 1024 {
        return None;
    }
    let img = image::open(src).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    // Same fit box as slide icons, so every row keeps identical layout.
    let scale = (CARD_W as f32 / w as f32)
        .min(CARD_H as f32 / h as f32)
        .min(1.0);
    let (nw, nh) = if scale < 1.0 {
        (
            ((w as f32 * scale).round() as u32).max(1),
            ((h as f32 * scale).round() as u32).max(1),
        )
    } else {
        (w, h)
    };
    let scaled = image::imageops::resize(&rgba, nw, nh, image::imageops::FilterType::Lanczos3);
    let mut buf = Vec::new();
    image::DynamicImage::ImageRgba8(scaled)
        .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .ok()?;
    let _ = std::fs::create_dir_all(out.parent()?);
    std::fs::write(out, &buf).ok()?;
    Some(out.to_path_buf())
}

// ── Tier 1: embedded thumbnail ─────────────────────────────────────

pub(crate) fn tier1_embedded_thumbnail(path: &Path) -> Option<Vec<u8>> {
    let file = std::fs::File::open(path).ok()?;
    let mut zip = zip::ZipArchive::new(file).ok()?;
    let candidates = [
        "docProps/thumbnail.jpeg",
        "docProps/thumbnail.jpg",
        "docProps/thumbnail.png",
        "Thumbnails/thumbnail.png",
    ];
    for name in candidates {
        if let Ok(mut entry) = zip.by_name(name) {
            if entry.size() > MAX_ENTRY_BYTES {
                continue;
            }
            let mut buf = Vec::new();
            if entry.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
                return Some(buf);
            }
        }
    }
    None
}

// ── Tier 2: pptx content card ──────────────────────────────────────

fn render_pptx_card(path: &Path) -> Option<Vec<u8>> {
    let file = std::fs::File::open(path).ok()?;
    let mut zip = zip::ZipArchive::new(file).ok()?;
    let mut title = String::new();
    let mut body_lines: Vec<String> = Vec::new();
    let mut first_image_bytes: Option<Vec<u8>> = None;

    let slide_xml = read_zip_string(&mut zip, "ppt/slides/slide1.xml")?;
    for shape in slide_xml.split("<p:sp>") {
        let end = shape.find("</p:sp>").unwrap_or(shape.len());
        let frag = &shape[..end];
        let ph = frag
            .find("<p:ph")
            .and_then(|_| tag_attr(frag, "<p:ph", "type"))
            .unwrap_or_default();
        let runs = extract_a_t_runs(frag);
        if runs.is_empty() {
            continue;
        }
        let joined = runs.join(" ");
        if is_title_placeholder(&ph) && title.is_empty() {
            title = joined;
        } else {
            body_lines.push(joined);
        }
        if body_lines.len() >= 3 {
            break;
        }
    }
    if let Some(rels_xml) = read_zip_string(&mut zip, "ppt/slides/_rels/slide1.xml.rels") {
        if let Some(rid) = first_image_rid(&rels_xml) {
            if let Some(target) = rels_target(&rels_xml, &rid) {
                let full = normalize_pptx_rel("ppt/slides", &target);
                if let Ok(mut entry) = zip.by_name(&full) {
                    if entry.size() <= MAX_ENTRY_BYTES {
                        let mut buf = Vec::new();
                        let _ = entry.read_to_end(&mut buf);
                        if !buf.is_empty() {
                            first_image_bytes = Some(buf);
                        }
                    }
                }
            }
        }
    }
    let img_rgba = first_image_bytes.and_then(|b| {
        image::load_from_memory(&b).ok().map(|img| img.to_rgba8())
    });
    render_card(&title, &body_lines, img_rgba.as_ref())
}

fn render_card(title: &str, body: &[String], img: Option<&image::RgbaImage>) -> Option<Vec<u8>> {
    use gtk::cairo;
    let (surface, cr) = crate::preview::new_surface(CARD_W as i32, CARD_H as i32)?;
    let wf = CARD_W as f64;
    let hf = CARD_H as f64;
    cr.set_source_rgb(0.97, 0.97, 0.98);
    cr.rectangle(0.0, 0.0, wf, hf);
    cr.fill().ok()?;
    let margin = 14.0;
    let mut y = margin;

    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
    cr.set_font_size(17.0);
    cr.set_source_rgb(0.12, 0.12, 0.14);
    for line in crate::preview::wrap_text(&cr, title, wf - margin * 2.0).into_iter().take(2) {
        if y > hf - 20.0 {
            break;
        }
        cr.move_to(margin, y + 14.0);
        let _ = cr.show_text(&line);
        y += 20.0;
    }
    y += 8.0;

    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
    cr.set_font_size(12.0);
    cr.set_source_rgb(0.35, 0.35, 0.37);
    let body_max_w = if img.is_some() { wf - margin - 80.0 } else { wf - margin * 2.0 };
    for line in body.iter().take(3) {
        if y > hf - 20.0 {
            break;
        }
        for wl in crate::preview::wrap_text(&cr, line, body_max_w).into_iter().take(2) {
            if y > hf - 20.0 {
                break;
            }
            cr.move_to(margin, y + 12.0);
            let _ = cr.show_text(&wl);
            y += 15.0;
        }
    }
    if let Some(rgba) = img {
        let (iw, ih) = rgba.dimensions();
        if iw > 0 && ih > 0 {
            let sw = (iw as f64 * 0.3).min(70.0);
            let sh = (ih as f64 * 0.3).min(50.0);
            let sx = wf - margin - sw;
            let sy = hf - margin - sh;
            if let Some(is) = crate::preview::cairo_image_from_rgba(rgba, iw, ih) {
                cr.save().ok()?;
                cr.translate(sx, sy);
                cr.scale(sw / iw as f64, sh / ih as f64);
                cr.set_source_surface(&is, 0.0, 0.0).ok()?;
                cr.paint().ok()?;
                cr.restore().ok()?;
            }
        }
    }
    crate::preview::surface_to_png(surface, cr)
}

// ── Tier 2: xlsx mini grid ─────────────────────────────────────────

fn render_xlsx_grid(path: &Path) -> Option<Vec<u8>> {
    use calamine::Reader;
    let mut workbook = calamine::open_workbook_auto(path).ok()?;
    let range = workbook.worksheet_range_at(0)?.ok()?;
    let rows = 8.min(range.rows().count());
    let cols = 5.min(range.width());
    if rows == 0 || cols == 0 {
        return None;
    }
    use gtk::cairo;
    let (surface, cr) = crate::preview::new_surface(CARD_W as i32, CARD_H as i32)?;
    let wf = CARD_W as f64;
    let hf = CARD_H as f64;
    cr.set_source_rgb(1.0, 1.0, 1.0);
    cr.rectangle(0.0, 0.0, wf, hf);
    cr.fill().ok()?;
    let margin = 8.0;
    let cell_h = 20.0;
    let col_w = (wf - margin * 2.0) / cols as f64;
    cr.select_font_face("Mono", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
    cr.set_font_size(10.0);
    for ri in 0..rows {
        let y = margin + ri as f64 * cell_h;
        if ri == 0 {
            cr.set_source_rgb(0.90, 0.91, 0.92);
            cr.rectangle(margin, y, wf - margin * 2.0, cell_h);
            cr.fill().ok()?;
        }
        for ci in 0..cols {
            let x = margin + ci as f64 * col_w;
            cr.set_source_rgb(0.82, 0.82, 0.84);
            cr.set_line_width(0.5);
            cr.rectangle(x, y, col_w, cell_h);
            cr.stroke().ok()?;
            let cell_val = range.get_value((ri as u32, ci as u32)).unwrap_or(&calamine::Data::Empty);
            let txt = cell_value_str(cell_val);
            if !txt.is_empty() {
                cr.set_source_rgb(0.12, 0.12, 0.14);
                cr.move_to(x + 3.0, y + 14.0);
                let ellipsized = truncate_to_width(&cr, &txt, col_w - 6.0);
                let _ = cr.show_text(&ellipsized);
            }
        }
    }
    crate::preview::surface_to_png(surface, cr)
}

/// Downscale a full-size preview PNG into the row cache (or hand back the
/// original path when downscaling isn't possible).
fn reuse_full_preview(src: Option<PathBuf>, cache: &Path) -> Option<PathBuf> {
    let src = src?;
    if !src.exists() {
        return None;
    }
    Some(downscale_slide_icon(&src, cache).unwrap_or(src))
}

/// Format one spreadsheet cell for display (shared with preview.rs).
///
/// Values must be FAITHFUL: integers exact, floats at shortest round-trip
/// precision (never `{:.4}`'s invented trailing digits), and date/time cells
/// as dates — never the raw Excel serial number behind them.
pub(crate) fn cell_value_str(v: &calamine::Data) -> String {
    match v {
        calamine::Data::Empty => String::new(),
        calamine::Data::Bool(b) => b.to_string(),
        calamine::Data::Int(i) => i.to_string(),
        calamine::Data::Float(f) => float_value_str(*f),
        calamine::Data::Error(e) => format!("#{:?}", e),
        calamine::Data::String(s) => s.clone(),
        calamine::Data::DateTime(d) => {
            let v = d.as_f64();
            if d.is_duration() {
                elapsed_time_str(v)
            } else {
                excel_datetime_str(v)
            }
        }
        calamine::Data::DateTimeIso(d) => d.clone(),
        calamine::Data::DurationIso(d) => d.clone(),
    }
}

/// Exact display for a float: clean integers, exponent form only for
/// extremes (so 1e20 doesn't become 21 characters of digits), otherwise
/// Rust's shortest round-trip — which is precisely what the cell holds.
fn float_value_str(f: f64) -> String {
    if !f.is_finite() {
        return format!("{}", f);
    }
    if f == f.trunc() && f.abs() < 1e15 {
        return format!("{}", f as i64);
    }
    if f.abs() >= 1e15 || (f != 0.0 && f.abs() < 1e-6) {
        return format!("{:e}", f);
    }
    format!("{}", f)
}

/// Excel serial day count → days since 1970-01-01 (1900 date system).
/// Serials ≥ 61 map through Excel's own epoch arithmetic; below that the
/// fictitious 1900-02-29 shifts everything by one day. Serial 60 (that
/// fictitious day) is treated like 59.
fn excel_days_1970(serial: i64) -> i64 {
    if serial >= 61 {
        serial - 25569
    } else if serial <= 59 {
        serial - 25567
    } else {
        59 - 25567
    }
}

/// Days since 1970-01-01 → (year, month, day). Howard Hinnant's civil
/// algorithm — no chrono dependency needed for one date stamp.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as i64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// An Excel date serial → "2023-07-15", "2023-07-15 14:30" (or just the
/// time when the serial holds no whole day). ISO date order keeps it
/// unambiguous regardless of locale. (Shared with preview.rs — a date
/// numFmt on a plain number cell still deserves a date, not a serial.)
pub(crate) fn excel_datetime_str(serial: f64) -> String {
    if !serial.is_finite() {
        return format!("{}", serial);
    }
    let mut days = serial.floor() as i64;
    let mut secs = ((serial - days as f64) * 86400.0).round() as i64;
    if secs >= 86400 {
        secs -= 86400;
        days += 1;
    }
    if days == 0 {
        return elapsed_time_str(secs.max(0) as f64);
    }
    let (y, m, d) = civil_from_days(excel_days_1970(days));
    let date = format!("{:04}-{:02}-{:02}", y, m, d);
    if secs <= 0 {
        date
    } else {
        format!("{} {}", date, hms_str(secs))
    }
}

/// Elapsed-time serial (a duration) → "14:30" / "40:15:00".
pub(crate) fn elapsed_time_str(days: f64) -> String {
    let secs = ((days * 86400.0).round() as i64).max(0);
    hms_str(secs)
}

/// Seconds → "14:30" when whole minutes suffice, "14:30:09" otherwise.
fn hms_str(secs: i64) -> String {
    let (h, rem) = (secs / 3600, secs % 3600);
    let (m, s) = (rem / 60, rem % 60);
    if s == 0 {
        format!("{:02}:{:02}", h, m)
    } else {
        format!("{:02}:{:02}:{:02}", h, m, s)
    }
}

// ── helpers ─────────────────────────────────────────────────────────

fn read_zip_string(zip: &mut zip::ZipArchive<std::fs::File>, name: &str) -> Option<String> {
    let mut entry = zip.by_name(name).ok()?;
    let mut buf = String::new();
    entry.read_to_string(&mut buf).ok()?;
    Some(buf)
}

fn tag_attr<'a>(haystack: &'a str, tag: &str, attr: &str) -> Option<&'a str> {
    let i = haystack.find(tag)?;
    let frag = &haystack[i..];
    let key = format!("{}=\"", attr);
    let kpos = frag.find(&key)?;
    let val_start = kpos + key.len();
    let val_end = frag[val_start..].find('"')?;
    Some(&frag[val_start..val_start + val_end])
}

fn is_title_placeholder(ph: &str) -> bool {
    matches!(ph, "title" | "ctrTitle")
}

fn extract_a_t_runs(shape: &str) -> Vec<String> {
    let mut runs = Vec::new();
    let mut pos = 0;
    while let Some(start) = shape[pos..].find("<a:t>") {
        let abs = pos + start;
        if let Some(end) = shape[abs..].find("</a:t>") {
            let text = shape[abs + 5..abs + end].to_string();
            if !text.is_empty() {
                runs.push(text);
            }
            pos = abs + end + 6;
        } else {
            break;
        }
    }
    runs
}

fn first_image_rid(rels_xml: &str) -> Option<String> {
    for rel in rels_xml.split("<Relationship") {
        if !rel.contains("image") {
            continue;
        }
        let id = rel.find("Id=\"").and_then(|v| {
            let s = v + 4;
            let e = rel[s..].find('"')?;
            Some(&rel[s..s + e])
        })?;
        return Some(id.to_string());
    }
    None
}

fn rels_target(rels_xml: &str, rid: &str) -> Option<String> {
    let needle = format!("Id=\"{}\"", rid);
    let s = rels_xml.find(&needle)?;
    let frag = &rels_xml[s..];
    let t = frag.find("Target=\"")? + 8;
    let e = frag[t..].find('"')?;
    Some(frag[t..t + e].to_string())
}

fn normalize_pptx_rel(base_dir: &str, target: &str) -> String {
    let mut parts: Vec<&str> = base_dir.split('/').collect();
    for seg in target.split('/') {
        match seg {
            "" | "." => {}
            ".." => { parts.pop(); }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

fn truncate_to_width(cr: &gtk::cairo::Context, text: &str, max_w: f64) -> String {
    if cr.text_extents(text).map(|e| e.width()).unwrap_or(0.0) <= max_w {
        return text.to_string();
    }
    let mut s = String::new();
    for ch in text.chars() {
        let mut trial = s.clone();
        trial.push(ch);
        trial.push('…');
        if cr.text_extents(&trial).map(|e| e.width()).unwrap_or(f64::MAX) > max_w {
            s.push('…');
            return s;
        }
        s.push(ch);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn td(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("thumb_test_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn cache_path_deterministic() {
        let key = CacheKey {
            path: PathBuf::from("/test/file.pptx"),
            mtime: 1000,
            size: 2000,
        };
        let p1 = cache_path_for(&key);
        let p2 = cache_path_for(&key);
        assert_eq!(p1, p2);
        assert!(p1.to_string_lossy().contains("spotty/thumbnails/"));
    }

    #[test]
    fn cell_display_is_faithful() {
        // Exact floats — no invented trailing digits from the old {:.4},
        // integers clean.
        assert_eq!(
            cell_value_str(&calamine::Data::Float(124.15602)),
            "124.15602"
        );
        assert_eq!(cell_value_str(&calamine::Data::Float(45123.0)), "45123");
        // Dates and times render as dates — never the raw serial number.
        let d = calamine::ExcelDateTime::new(
            44927.0,
            calamine::ExcelDateTimeType::DateTime,
            false,
        );
        assert_eq!(cell_value_str(&calamine::Data::DateTime(d)), "2023-01-01");
        let t = calamine::ExcelDateTime::new(0.5, calamine::ExcelDateTimeType::TimeDelta, false);
        assert_eq!(cell_value_str(&calamine::Data::DateTime(t)), "12:00");
    }

    #[test]
    fn image_row_thumbnail_downscales() {
        // The PNG fix: image files get their own picture in the result row.
        let d = td("image_row");
        let src = d.join("pic.png");
        let img = image::RgbaImage::from_pixel(400, 300, image::Rgba([200, 30, 30, 255]));
        image::DynamicImage::ImageRgba8(img).save(&src).unwrap();
        let out = d.join("pic_row.png");
        let got = downscale_image_icon(&src, &out).expect("image row thumbnail");
        assert!(got.exists());
        let thumb = image::open(&got).unwrap();
        assert!(thumb.width() > 0 && thumb.height() > 0);
        assert!(thumb.width() <= CARD_W as u32 && thumb.height() <= CARD_H as u32);
        // Aspect kept: 400×300 fits the card box without distortion.
        let ratio = thumb.width() as f64 / thumb.height() as f64;
        assert!((ratio - 400.0 / 300.0).abs() < 0.05);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn embedded_thumbnail_from_zip() {
        let d = td("embedded");
        let p = d.join("test.pptx");
        {
            let f = std::fs::File::create(&p).unwrap();
            let mut w = zip::ZipWriter::new(f);
            w.start_file("docProps/thumbnail.png", zip::write::FileOptions::default()).unwrap();
            w.write_all(b"fake-png-data").unwrap();
            w.finish().unwrap();
        }
        let data = tier1_embedded_thumbnail(&p).unwrap();
        assert_eq!(data, b"fake-png-data");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn card_render_produces_png() {
        let card = render_card("Test Title", &["Line 1".into(), "Line 2".into()], None).unwrap();
        assert!(card.len() > 8);
        assert_eq!(&card[..4], b"\x89PNG"); // PNG magic
    }

    #[test]
    fn thumbnail_for_pptx_with_embedded_thumb() {
        let d = td("thumb");
        let p = d.join("file.pptx");
        {
            let f = std::fs::File::create(&p).unwrap();
            let mut w = zip::ZipWriter::new(f);
            w.start_file("docProps/thumbnail.png", zip::write::FileOptions::default()).unwrap();
            // Write a minimal valid 1x1 red PNG.
            let png: Vec<u8> = vec![
                0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a,
                0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
                0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
                0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
                0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41,
                0x54, 0x08, 0xd7, 0x63, 0xd8, 0xa8, 0xc0, 0x00,
                0x00, 0x00, 0x04, 0x00, 0x01, 0x27, 0x34, 0x21,
                0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44,
                0xae, 0x42, 0x60, 0x82,
            ];
            w.write_all(&png).unwrap();
            w.finish().unwrap();
        }
        let result = thumbnail_for(&p).unwrap();
        assert!(result.exists());
        let meta = std::fs::metadata(&result).unwrap();
        assert!(meta.len() > 0);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Row gate covers document types end-to-end: a .docx row gets the
    /// native page-1 render, downscaled to row-card size.
    #[test]
    fn thumbnail_for_docx_office_page() {
        assert!(ROW_THUMB_EXTS.contains(&"docx"));
        assert!(ROW_THUMB_EXTS.contains(&"pdf"));

        let d = td("docx_row");
        let p = d.join("doc.docx");
        {
            let mut xml = String::from(
                "<?xml version=\"1.0\"?><w:document xmlns:w=\"x\"><w:body>",
            );
            for i in 0..40 {
                xml.push_str(&format!(
                    "<w:p><w:r><w:t>Paragraph {i} of the row thumbnail test document, long enough to wrap.</w:t></w:r></w:p>"
                ));
            }
            xml.push_str("</w:body></w:document>");
            let f = std::fs::File::create(&p).unwrap();
            let mut w = zip::ZipWriter::new(f);
            w.start_file("word/document.xml", zip::write::FileOptions::default())
                .unwrap();
            w.write_all(xml.as_bytes()).unwrap();
            w.finish().unwrap();
        }

        let thumb = thumbnail_for(&p).expect("docx row thumbnail");
        assert!(thumb.exists());
        assert_eq!(&std::fs::read(&thumb).unwrap()[..4], b"\x89PNG");
        let img = image::open(&thumb).unwrap().to_rgba8();
        assert!(
            (img.width() as f32) <= CARD_W as f32 && (img.height() as f32) <= CARD_H as f32,
            "downscaled to row size: {}x{}",
            img.width(),
            img.height()
        );
        let _ = std::fs::remove_file(&thumb);
        let _ = std::fs::remove_dir_all(&d);
    }
}
