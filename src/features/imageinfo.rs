use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct ImageInfo {
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
    pub format: String,
}

// Process-wide cache: (path, size, mtime) -> Option<ImageInfo>
type CacheKey = (std::path::PathBuf, u64, u64);
thread_local! {
    static CACHE: RefCell<HashMap<CacheKey, Option<ImageInfo>>> = RefCell::new(HashMap::new());
}

fn cache_key(path: &Path) -> Option<CacheKey> {
    let meta = std::fs::metadata(path).ok()?;
    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some((path.to_path_buf(), size, mtime))
}

pub fn info(path: &Path) -> Option<ImageInfo> {
    let key = cache_key(path)?;
    if let Some(cached) = CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return cached;
    }
    let result = info_inner(path);
    let key = cache_key(path);
    if let Some(k) = key {
        CACHE.with(|c| c.borrow_mut().insert(k, result.clone()));
    }
    result
}

fn info_inner(path: &Path) -> Option<ImageInfo> {
    let (width, height) = image::image_dimensions(path).ok()?;
    let bytes = std::fs::metadata(path).ok()?.len();
    let format = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_uppercase())
        .unwrap_or_else(|| "PNG".into());
    Some(ImageInfo {
        width,
        height,
        bytes,
        format,
    })
}

pub fn human_size(bytes: u64) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

pub fn aspect_ratio(w: u32, h: u32) -> String {
    let g = gcd(w, h);
    let rw = w / g;
    let rh = h / g;
    format!("{}:{}", rw, rh)
}

fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a
}

pub fn orientation(w: u32, h: u32) -> &'static str {
    if w > h {
        "landscape"
    } else if h > w {
        "portrait"
    } else {
        "square"
    }
}

pub fn megapixels(w: u32, h: u32) -> String {
    let mp = (w as f64 * h as f64) / 1_000_000.0;
    format!("{:.1} MP", mp)
}

/// Build a caption string for the image preview pane.
/// e.g. "1920 × 1080 · PNG · 1.2 MB · 16:9 · Landscape"
pub fn caption(info: &ImageInfo) -> String {
    format!(
        "{} × {} · {} · {} · {} · {}",
        info.width,
        info.height,
        info.format,
        human_size(info.bytes),
        aspect_ratio(info.width, info.height),
        capitalize(orientation(info.width, info.height)),
    )
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Build a lowercased haystack for search token matching.
/// Contains tokens like: "1920x1080 1920×1080 1920 1080 png 1.2mb 1.2 mb
/// 1240000 bytes 2.1mp 2.1 mp 16:9 landscape"
pub fn search_tokens(info: &ImageInfo) -> String {
    let w = info.width;
    let h = info.height;
    let fmt = info.format.to_lowercase();
    let hs = human_size(info.bytes).to_lowercase();
    let hs_nospace = hs.replace(' ', "");
    let mp = megapixels(w, h).to_lowercase();
    let mp_nospace = mp.replace(' ', "");
    let ar = aspect_ratio(w, h);
    let or = orientation(w, h);

    format!(
        "{w}x{h} {w}×{h} {w} {h} {fmt} {hs} {hs_nospace} {} bytes {mp} {mp_nospace} {ar} {or}",
        info.bytes,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info_from(w: u32, h: u32, bytes: u64) -> ImageInfo {
        ImageInfo {
            width: w,
            height: h,
            bytes,
            format: "PNG".into(),
        }
    }

    #[test]
    fn human_size_bytes() {
        assert_eq!(human_size(512), "512 B");
    }

    #[test]
    fn human_size_kb() {
        assert_eq!(human_size(2048), "2.0 KB");
    }

    #[test]
    fn human_size_mb() {
        assert_eq!(human_size(1_500_000), "1.4 MB");
    }

    #[test]
    fn aspect_ratio_simple() {
        assert_eq!(aspect_ratio(1920, 1080), "16:9");
    }

    #[test]
    fn aspect_ratio_square() {
        assert_eq!(aspect_ratio(500, 500), "1:1");
    }

    #[test]
    fn aspect_ratio_prime() {
        assert_eq!(aspect_ratio(1024, 768), "4:3");
    }

    #[test]
    fn orientation_landscape() {
        assert_eq!(orientation(1920, 1080), "landscape");
    }

    #[test]
    fn orientation_portrait() {
        assert_eq!(orientation(1080, 1920), "portrait");
    }

    #[test]
    fn orientation_square_case() {
        assert_eq!(orientation(500, 500), "square");
    }

    #[test]
    fn megapixels_basic() {
        assert_eq!(megapixels(1920, 1080), "2.1 MP");
    }

    #[test]
    fn caption_string() {
        let info = info_from(1920, 1080, 1_500_000);
        let c = caption(&info);
        assert_eq!(c, "1920 × 1080 · PNG · 1.4 MB · 16:9 · Landscape");
    }

    #[test]
    fn search_tokens_contain_all_keys() {
        let info = info_from(1920, 1080, 1_500_000);
        let t = search_tokens(&info);
        assert!(t.contains("1920x1080"));
        assert!(t.contains("1920×1080"));
        assert!(t.contains("1920"));
        assert!(t.contains("1080"));
        assert!(t.contains("png"));
        assert!(t.contains("1.4mb"));
        assert!(t.contains("1.4 mb"));
        assert!(t.contains("1500000 bytes"));
        assert!(t.contains("2.1mp"));
        assert!(t.contains("16:9"));
        assert!(t.contains("landscape"));
    }

    #[test]
    fn search_tokens_small_image() {
        let info = info_from(640, 480, 120_000);
        let t = search_tokens(&info);
        assert!(t.contains("640x480"));
        assert!(t.contains("4:3"));
        assert!(t.contains("landscape"));
        assert!(t.contains("120000 bytes"));
    }
}
