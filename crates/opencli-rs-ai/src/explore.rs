//! API discovery: navigate a page, capture network traffic, and infer API endpoints.

use std::collections::HashMap;

use opencli_rs_core::{AutoScrollOptions, CliError, IPage, NetworkRequest, Strategy};
use serde_json::Value;
use tracing::debug;

use crate::types::{
    DiscoveredEndpoint, ExploreManifest, ExploreOptions, FieldInfo,
    FIELD_ROLES, KNOWN_SITE_ALIASES, LIMIT_PARAMS, PAGINATION_PARAMS,
    SEARCH_PARAMS, VOLATILE_PARAMS,
};

// ── JavaScript snippets ─────────────────────────────────────────────────────

const FRAMEWORK_DETECT_JS: &str = r#"
(() => {
    if (window.__NEXT_DATA__) return 'Next.js';
    if (window.__NUXT__) return 'Nuxt';
    if (document.querySelector('[data-reactroot]') || document.querySelector('#__next')) return 'React';
    if (document.querySelector('[data-v-]') || window.__vue_app__) return 'Vue';
    if (document.querySelector('[ng-version]') || window.ng) return 'Angular';
    if (window.__SVELTE__) return 'Svelte';
    return null;
})()
"#;

const STORE_DETECT_JS: &str = r#"
(() => {
    if (window.__pinia) return 'Pinia';
    if (window.__VUEX__) return 'Vuex';
    if (window.__REDUX_DEVTOOLS_EXTENSION__) return 'Redux';
    if (window.__MOBX_DEVTOOLS_GLOBAL_HOOK__) return 'MobX';
    return null;
})()
"#;

// ── Public API ──────────────────────────────────────────────────────────────

/// Explore a URL: navigate, auto-scroll, capture network traffic, and analyze.
pub async fn explore(
    page: &dyn IPage,
    url: &str,
    options: ExploreOptions,
) -> Result<ExploreManifest, CliError> {
    // 1. Navigate to URL
    page.goto(url, None).await?;

    // 2. Auto-scroll to trigger lazy loading
    let max_scrolls = options.max_scrolls.unwrap_or(5);
    page.auto_scroll(Some(AutoScrollOptions {
        max_scrolls: Some(max_scrolls),
        delay_ms: Some(500),
        ..Default::default()
    }))
    .await?;

    // 3. Capture network traffic
    let capture_network = options.capture_network.unwrap_or(true);
    let network = if capture_network {
        page.get_network_requests().await.unwrap_or_default()
    } else {
        vec![]
    };

    // 4. Analyze JSON responses — find API endpoints
    let endpoints = analyze_network_traffic(&network);
    debug!("Discovered {} API endpoints", endpoints.len());

    // 5. Detect framework
    let framework = detect_framework(page).await.ok().flatten();

    // 6. Detect store
    let store = detect_store(page).await.ok().flatten();

    // 7. Get page title
    let title = page
        .evaluate("document.title")
        .await
        .ok()
        .and_then(|v| v.as_str().map(String::from));

    // 8. Infer auth indicators
    let auth_indicators = infer_auth_indicators(&endpoints);

    Ok(ExploreManifest {
        url: url.to_string(),
        title,
        endpoints,
        framework,
        store,
        auth_indicators,
    })
}

// ── Network analysis ────────────────────────────────────────────────────────

/// Analyze captured network requests to discover API endpoints.
pub(crate) fn analyze_network_traffic(requests: &[NetworkRequest]) -> Vec<DiscoveredEndpoint> {
    let mut seen: HashMap<String, DiscoveredEndpoint> = HashMap::new();

    for req in requests {
        // Skip non-API content types
        let ct = req
            .headers
            .get("content-type")
            .cloned()
            .unwrap_or_default()
            .to_lowercase();
        if ct.contains("image/")
            || ct.contains("font/")
            || ct.contains("css")
            || ct.contains("javascript")
            || ct.contains("wasm")
        {
            continue;
        }
        // Skip error responses
        if let Some(status) = req.status {
            if status >= 400 {
                continue;
            }
        }

        let pattern = url_to_pattern(&req.url);
        let key = format!("{}:{}", req.method, pattern);
        if seen.contains_key(&key) {
            continue;
        }

        // Infer content type from URL if header is missing
        let effective_ct = if ct.is_empty() {
            if req.url.contains("/api/") || req.url.contains("/x/") || req.url.ends_with(".json") {
                "application/json".to_string()
            } else {
                String::new()
            }
        } else {
            ct.clone()
        };

        // Parse query parameters
        let query_params = extract_query_params(&req.url);
        let has_search = query_params.iter().any(|p| SEARCH_PARAMS.contains(&p.as_str()));
        let has_pagination = query_params.iter().any(|p| PAGINATION_PARAMS.contains(&p.as_str()));
        let has_limit = query_params.iter().any(|p| LIMIT_PARAMS.contains(&p.as_str()));

        // Analyze response body
        let (fields, sample_response) = if let Some(ref body) = req.response_body {
            parse_response_fields(body)
        } else {
            (vec![], None)
        };

        // Detect auth indicators
        let auth_indicators = detect_auth_indicators(&req.headers);

        // Score the endpoint
        let score = score_endpoint(
            &effective_ct,
            &pattern,
            req.status,
            has_search,
            has_pagination,
            has_limit,
            &fields,
            &sample_response,
        );

        if score < 5.0 {
            continue;
        }

        let auth_level = infer_strategy(&auth_indicators);

        seen.insert(
            key,
            DiscoveredEndpoint {
                url: req.url.clone(),
                method: req.method.clone(),
                content_type: Some(effective_ct),
                fields,
                confidence: (score / 20.0).min(1.0),
                auth_level,
                sample_response,
            },
        );
    }

    let mut endpoints: Vec<_> = seen.into_values().collect();
    endpoints.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap_or(std::cmp::Ordering::Equal));
    endpoints
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Normalize a URL into a pattern by replacing numeric/hex path segments.
pub(crate) fn url_to_pattern(url: &str) -> String {
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return url.to_string(),
    };
    let path = parsed.path();
    let mut normalized = String::new();
    for segment in path.split('/') {
        normalized.push('/');
        if segment.chars().all(|c| c.is_ascii_digit()) && !segment.is_empty() {
            normalized.push_str("{id}");
        } else if segment.len() >= 8
            && segment.chars().all(|c| c.is_ascii_hexdigit())
        {
            normalized.push_str("{hex}");
        } else {
            normalized.push_str(segment);
        }
    }

    // Collect non-volatile query params
    let mut params: Vec<String> = vec![];
    for (k, _) in parsed.query_pairs() {
        if !VOLATILE_PARAMS.contains(&k.as_ref()) {
            params.push(k.to_string());
        }
    }
    params.sort();

    let host = parsed.host_str().unwrap_or("");
    if params.is_empty() {
        format!("{}{}", host, normalized)
    } else {
        let qs = params.iter().map(|k| format!("{}={{}}", k)).collect::<Vec<_>>().join("&");
        format!("{}{}?{}", host, normalized, qs)
    }
}

/// Extract non-volatile query parameter names from a URL.
fn extract_query_params(url: &str) -> Vec<String> {
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return vec![],
    };
    parsed
        .query_pairs()
        .filter(|(k, _)| !VOLATILE_PARAMS.contains(&k.as_ref()))
        .map(|(k, _)| k.to_string())
        .collect()
}

/// Detect auth-related indicators from request headers.
fn detect_auth_indicators(headers: &HashMap<String, String>) -> Vec<String> {
    let mut indicators = vec![];
    let keys: Vec<String> = headers.keys().map(|k| k.to_lowercase()).collect();
    if keys.iter().any(|k| k == "authorization") {
        indicators.push("bearer".to_string());
    }
    if keys.iter().any(|k| k.starts_with("x-csrf") || k.starts_with("x-xsrf")) {
        indicators.push("csrf".to_string());
    }
    if keys.iter().any(|k| k.starts_with("x-s") || k == "x-t" || k == "x-s-common") {
        indicators.push("signature".to_string());
    }
    indicators
}

/// Score an endpoint by how likely it is to be a useful API.
fn score_endpoint(
    content_type: &str,
    pattern: &str,
    status: Option<u16>,
    has_search: bool,
    has_pagination: bool,
    has_limit: bool,
    fields: &[FieldInfo],
    sample_response: &Option<Value>,
) -> f64 {
    let mut s: f64 = 0.0;
    if content_type.contains("json") {
        s += 10.0;
    }
    if !fields.is_empty() {
        s += 5.0;
        s += (fields.len() as f64).min(10.0);
        // Bonus for recognized roles
        let role_count = fields.iter().filter(|f| f.role.is_some()).count();
        s += role_count as f64 * 2.0;
    }
    if pattern.contains("/api/") || pattern.contains("/x/") {
        s += 3.0;
    }
    if has_search {
        s += 3.0;
    }
    if has_pagination {
        s += 2.0;
    }
    if has_limit {
        s += 2.0;
    }
    if status == Some(200) {
        s += 2.0;
    }
    // Penalize empty JSON responses (anti-bot)
    if content_type.contains("json") && fields.is_empty() && sample_response.is_some() {
        s -= 3.0;
    }
    s
}

/// Infer the auth strategy from detected indicators.
fn infer_strategy(indicators: &[String]) -> Strategy {
    if indicators.iter().any(|i| i == "signature") {
        Strategy::Intercept
    } else if indicators.iter().any(|i| i == "bearer" || i == "csrf") {
        Strategy::Header
    } else {
        Strategy::Cookie
    }
}

/// Aggregate auth indicators from all discovered endpoints.
fn infer_auth_indicators(endpoints: &[DiscoveredEndpoint]) -> Vec<String> {
    let mut all: Vec<String> = vec![];
    for ep in endpoints {
        match ep.auth_level {
            Strategy::Intercept => {
                if !all.contains(&"signature".to_string()) {
                    all.push("signature".to_string());
                }
            }
            Strategy::Header => {
                if !all.contains(&"bearer".to_string()) {
                    all.push("bearer".to_string());
                }
            }
            _ => {}
        }
    }
    all
}

/// Parse JSON response body to extract field info and a sample value.
fn parse_response_fields(body: &str) -> (Vec<FieldInfo>, Option<Value>) {
    let value: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return (vec![], None),
    };

    // Find the best array of objects in the response
    let items = find_item_array(&value, 0);
    if items.is_empty() {
        return (vec![], Some(value));
    }

    // Analyze the first item
    let first = &items[0];
    let fields = extract_fields_from_value(first, "", 2);

    (fields, Some(value))
}

/// Recursively search for the largest array of objects (max depth 4).
fn find_item_array(value: &Value, depth: usize) -> Vec<&Value> {
    if depth > 4 {
        return vec![];
    }
    match value {
        Value::Array(arr) if arr.len() >= 2 => {
            let has_objects = arr.iter().any(|v| v.is_object());
            if has_objects {
                return arr.iter().collect();
            }
        }
        Value::Object(map) => {
            let mut best: Vec<&Value> = vec![];
            for val in map.values() {
                let candidate = find_item_array(val, depth + 1);
                if candidate.len() > best.len() {
                    best = candidate;
                }
            }
            return best;
        }
        _ => {}
    }
    vec![]
}

/// Extract field info from a JSON value, assigning roles based on well-known names.
fn extract_fields_from_value(value: &Value, prefix: &str, max_depth: usize) -> Vec<FieldInfo> {
    if max_depth == 0 {
        return vec![];
    }
    let obj = match value.as_object() {
        Some(o) => o,
        None => return vec![],
    };

    let mut fields = vec![];
    for (key, val) in obj {
        let full_name = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{}.{}", prefix, key)
        };

        let field_type = match val {
            Value::String(_) => "string",
            Value::Number(_) => "number",
            Value::Bool(_) => "boolean",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
            Value::Null => "string",
        };

        // Check role
        let lower = key.to_lowercase();
        let role = FIELD_ROLES.iter().find_map(|(role_name, aliases)| {
            if aliases.contains(&lower.as_str()) {
                Some(role_name.to_string())
            } else {
                None
            }
        });

        fields.push(FieldInfo {
            name: full_name.clone(),
            role,
            field_type: field_type.to_string(),
        });

        // Recurse into nested objects
        if val.is_object() {
            fields.extend(extract_fields_from_value(val, &full_name, max_depth - 1));
        }
    }
    fields
}

/// Detect the frontend framework used by the page.
async fn detect_framework(page: &dyn IPage) -> Result<Option<String>, CliError> {
    let value = page.evaluate(FRAMEWORK_DETECT_JS).await?;
    Ok(value.as_str().map(String::from))
}

/// Detect the state management store used by the page.
async fn detect_store(page: &dyn IPage) -> Result<Option<String>, CliError> {
    let value = page.evaluate(STORE_DETECT_JS).await?;
    Ok(value.as_str().map(String::from))
}

/// Detect a short site name from a URL.
pub fn detect_site_name(url: &str) -> String {
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return "site".to_string(),
    };
    let host = parsed.host_str().unwrap_or("").to_lowercase();

    // Check known aliases
    for &(alias_host, alias_name) in KNOWN_SITE_ALIASES {
        if host == alias_host {
            return alias_name.to_string();
        }
    }

    let parts: Vec<&str> = host.split('.').filter(|p| !p.is_empty() && *p != "www").collect();
    if parts.len() >= 2 {
        let last = parts[parts.len() - 1];
        if ["uk", "jp", "cn", "com"].contains(&last) && parts.len() >= 3 {
            return slugify(parts[parts.len() - 3]);
        }
        return slugify(parts[parts.len() - 2]);
    }
    parts.first().map(|p| slugify(p)).unwrap_or_else(|| "site".to_string())
}

/// Slugify a string for use as a site name.
pub fn slugify(value: &str) -> String {
    let s: String = value
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "site".to_string()
    } else {
        s
    }
}

/// Infer a capability name from an endpoint URL.
pub(crate) fn infer_capability_name(url: &str, goal: Option<&str>) -> String {
    if let Some(g) = goal {
        return g.to_string();
    }
    let u = url.to_lowercase();
    if u.contains("hot") || u.contains("popular") || u.contains("ranking") || u.contains("trending") {
        return "hot".to_string();
    }
    if u.contains("search") {
        return "search".to_string();
    }
    if u.contains("feed") || u.contains("timeline") || u.contains("dynamic") {
        return "feed".to_string();
    }
    if u.contains("comment") || u.contains("reply") {
        return "comments".to_string();
    }
    if u.contains("history") {
        return "history".to_string();
    }
    if u.contains("profile") || u.contains("userinfo") || u.contains("/me") {
        return "me".to_string();
    }
    if u.contains("favorite") || u.contains("collect") || u.contains("bookmark") {
        return "favorite".to_string();
    }
    // Try last meaningful path segment
    if let Ok(parsed) = url::Url::parse(url) {
        let segs: Vec<&str> = parsed
            .path_segments()
            .into_iter()
            .flatten()
            .filter(|s| {
                !s.is_empty()
                    && !s.chars().all(|c| c.is_ascii_digit())
                    && !(s.len() >= 8 && s.chars().all(|c| c.is_ascii_hexdigit()))
            })
            .collect();
        if let Some(last) = segs.last() {
            return last
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect::<String>()
                .to_lowercase();
        }
    }
    "data".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_url_to_pattern_replaces_ids() {
        let p = url_to_pattern("https://api.example.com/v1/posts/12345/comments");
        assert!(p.contains("{id}"));
        assert!(p.contains("comments"));
    }

    #[test]
    fn test_url_to_pattern_strips_volatile_params() {
        let p = url_to_pattern("https://api.example.com/data?q=rust&_=123456&t=999");
        assert!(p.contains("q={}"));
        assert!(!p.contains("_="));
        assert!(!p.contains("t="));
    }

    #[test]
    fn test_detect_site_name_known_alias() {
        assert_eq!(detect_site_name("https://news.ycombinator.com"), "hackernews");
        assert_eq!(detect_site_name("https://x.com/home"), "twitter");
        assert_eq!(detect_site_name("https://www.bilibili.com/hot"), "bilibili");
    }

    #[test]
    fn test_detect_site_name_generic() {
        assert_eq!(detect_site_name("https://www.example.com/foo"), "example");
        assert_eq!(detect_site_name("not-a-url"), "site");
    }

    #[test]
    fn test_slugify() {
        assert_eq!(slugify("Hello World"), "hello-world");
        assert_eq!(slugify("  "), "site");
    }

    #[test]
    fn test_infer_capability_name_with_goal() {
        assert_eq!(infer_capability_name("https://example.com/api", Some("trending")), "trending");
    }

    #[test]
    fn test_infer_capability_name_from_url() {
        assert_eq!(infer_capability_name("https://example.com/api/hot", None), "hot");
        assert_eq!(infer_capability_name("https://example.com/api/search", None), "search");
        assert_eq!(infer_capability_name("https://example.com/api/feed", None), "feed");
    }

    #[test]
    fn test_analyze_network_traffic_filters() {
        let requests = vec![
            NetworkRequest {
                url: "https://example.com/api/data".to_string(),
                method: "GET".to_string(),
                headers: {
                    let mut h = HashMap::new();
                    h.insert("content-type".to_string(), "application/json".to_string());
                    h
                },
                body: None,
                status: Some(200),
                response_body: Some(r#"{"data":{"list":[{"title":"a","url":"b"},{"title":"c","url":"d"}]}}"#.to_string()),
            },
            // Should be skipped: image
            NetworkRequest {
                url: "https://example.com/logo.png".to_string(),
                method: "GET".to_string(),
                headers: {
                    let mut h = HashMap::new();
                    h.insert("content-type".to_string(), "image/png".to_string());
                    h
                },
                body: None,
                status: Some(200),
                response_body: None,
            },
            // Should be skipped: 404
            NetworkRequest {
                url: "https://example.com/api/missing".to_string(),
                method: "GET".to_string(),
                headers: {
                    let mut h = HashMap::new();
                    h.insert("content-type".to_string(), "application/json".to_string());
                    h
                },
                body: None,
                status: Some(404),
                response_body: None,
            },
        ];

        let endpoints = analyze_network_traffic(&requests);
        assert_eq!(endpoints.len(), 1);
        assert!(endpoints[0].url.contains("api/data"));
        assert!(endpoints[0].confidence > 0.0);
    }

    #[test]
    fn test_parse_response_fields() {
        let body = r#"{"data":{"list":[{"title":"Hello","url":"https://x.com","author":"alice"},{"title":"World","url":"https://y.com","author":"bob"}]}}"#;
        let (fields, sample) = parse_response_fields(body);
        assert!(sample.is_some());
        assert!(!fields.is_empty());
        // Should detect title, url, author roles
        let roles: Vec<_> = fields.iter().filter_map(|f| f.role.as_ref()).collect();
        assert!(roles.contains(&&"title".to_string()));
        assert!(roles.contains(&&"url".to_string()));
        assert!(roles.contains(&&"author".to_string()));
    }

    #[test]
    fn test_framework_detect_js_is_valid() {
        // Ensure the JS string is non-empty and contains expected patterns
        assert!(FRAMEWORK_DETECT_JS.contains("__NEXT_DATA__"));
        assert!(FRAMEWORK_DETECT_JS.contains("React"));
        assert!(FRAMEWORK_DETECT_JS.contains("Vue"));
        assert!(FRAMEWORK_DETECT_JS.contains("Angular"));
    }

    #[test]
    fn test_store_detect_js_is_valid() {
        assert!(STORE_DETECT_JS.contains("__pinia"));
        assert!(STORE_DETECT_JS.contains("Pinia"));
        assert!(STORE_DETECT_JS.contains("Redux"));
    }
}
