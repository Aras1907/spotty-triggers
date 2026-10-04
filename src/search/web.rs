use crate::config::Config;
use crate::search::browser_engine::EngineIcon;
use crate::search::{Action, ResultKind, SearchResult};

pub fn result(q: &str, cfg: &Config) -> SearchResult {
    let browser_default = cfg.search_engine == crate::config::SearchEngine::BrowserDefault;
    let engine_name = if browser_default {
        crate::search::browser_engine::engine_name()
            .unwrap_or_else(|| "default browser".into())
    } else if cfg.search_engine == crate::config::SearchEngine::Custom {
        "custom web search".into()
    } else {
        cfg.search_engine.display_name().to_string()
    };
    let url = cfg.web_search_url_for(q);
    // The engine's own icon first — that is what "the icon itself" means, and
    // it works for engines whose site has no usable favicon (and for the ones
    // the browser already cached, without a request). The URL's host is the
    // fallback, and the generic glyph the last resort.
    let icon = if browser_default {
        match crate::search::browser_engine::engine_icon() {
            Some(EngineIcon::Data(uri)) => format!("engine-icon-data:{uri}"),
            Some(EngineIcon::Url(url)) => format!("engine-icon:{url}"),
            None => favicon_icon(&url),
        }
    } else {
        favicon_icon(&url)
    };
    SearchResult {
        kind: ResultKind::Web,
        title: format!("Search \"{q}\" on {engine_name}"),
        subtitle: Some("Open in browser".into()),
        icon: Some(icon),
        action: Action::OpenUrl(url),
        score: 100,
    }
}

/// The favicon of the site the search runs on.
fn favicon_icon(url: &str) -> String {
    domain_of(url)
        .map(|d| format!("favicon:{d}"))
        .unwrap_or_else(|| "web-browser-symbolic".into())
}

/// Extract the host (e.g. "duckduckgo.com") from a URL, including custom
/// search URLs, so the search-engine's own favicon can be shown — even for a
/// user-entered custom search engine.
fn domain_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = rest.split(['/', '?', '#']).next()?;
    let host = host.rsplit_once('@').map(|(_, h)| h).unwrap_or(host);
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_host_of_any_search_url() {
        assert_eq!(
            domain_of("https://www.google.com/search?q=x").as_deref(),
            Some("www.google.com")
        );
        assert_eq!(
            domain_of("https://duckduckgo.com/?q=x").as_deref(),
            Some("duckduckgo.com")
        );
        // A custom engine's URL, whatever shape the user pasted.
        assert_eq!(
            domain_of("https://user@search.example.org/find?term=x").as_deref(),
            Some("search.example.org")
        );
        // A scheme-less URL is still usable (custom engines are pasted that
        // way); only a URL with no host at all has none.
        assert_eq!(domain_of("kagi.com/search?q=x").as_deref(), Some("kagi.com"));
        assert_eq!(domain_of(""), None);
    }

    #[test]
    fn the_icon_falls_back_from_the_engines_own_to_the_sites() {
        // No engine icon known: the search URL's host decides, so the row still
        // shows *something* engine-specific rather than a generic globe.
        assert_eq!(
            favicon_icon("https://kagi.com/search?q=x"),
            "favicon:kagi.com"
        );
        assert_eq!(favicon_icon(""), "web-browser-symbolic");
    }
}