//! Auth strategy probing: automatically discover the minimum-privilege strategy.
//!
//! Probes an API endpoint starting from PUBLIC and cascading through
//! COOKIE -> HEADER -> INTERCEPT until a working strategy is found.

use opencli_rs_core::{CliError, IPage, Strategy};
use tracing::debug;

use crate::types::{CascadeResult, StrategyTestResult};

/// Strategy cascade order (simplest to most complex).
const CASCADE_ORDER: &[Strategy] = &[
    Strategy::Public,
    Strategy::Cookie,
    Strategy::Header,
    Strategy::Intercept,
];

/// JavaScript to probe with a plain fetch (no credentials).
fn build_public_probe_js(url: &str) -> String {
    format!(
        r#"
        (async () => {{
            try {{
                const resp = await fetch({url}, {{ }});
                const status = resp.status;
                if (!resp.ok) return {{ status, ok: false, hasData: false }};
                const text = await resp.text();
                let hasData = false;
                try {{
                    const json = JSON.parse(text);
                    hasData = !!json && (Array.isArray(json) ? json.length > 0 :
                        typeof json === 'object' && Object.keys(json).length > 0);
                    if (json.code !== undefined && json.code !== 0) hasData = false;
                }} catch {{}}
                return {{ status, ok: true, hasData, preview: text.slice(0, 200) }};
            }} catch (e) {{ return {{ ok: false, error: e.message, hasData: false }}; }}
        }})()
        "#,
        url = serde_json::to_string(url).unwrap_or_else(|_| format!("\"{}\"", url)),
    )
}

/// JavaScript to probe with credentials included (cookies).
fn build_cookie_probe_js(url: &str) -> String {
    format!(
        r#"
        (async () => {{
            try {{
                const resp = await fetch({url}, {{ credentials: 'include' }});
                const status = resp.status;
                if (!resp.ok) return {{ status, ok: false, hasData: false }};
                const text = await resp.text();
                let hasData = false;
                try {{
                    const json = JSON.parse(text);
                    hasData = !!json && (Array.isArray(json) ? json.length > 0 :
                        typeof json === 'object' && Object.keys(json).length > 0);
                    if (json.code !== undefined && json.code !== 0) hasData = false;
                }} catch {{}}
                return {{ status, ok: true, hasData, preview: text.slice(0, 200) }};
            }} catch (e) {{ return {{ ok: false, error: e.message, hasData: false }}; }}
        }})()
        "#,
        url = serde_json::to_string(url).unwrap_or_else(|_| format!("\"{}\"", url)),
    )
}

/// JavaScript to probe with credentials + CSRF token extraction.
fn build_header_probe_js(url: &str) -> String {
    format!(
        r#"
        (async () => {{
            try {{
                const cookies = document.cookie.split(';').map(c => c.trim());
                const csrf = cookies.find(c =>
                    c.startsWith('ct0=') || c.startsWith('csrf_token=') || c.startsWith('_csrf=')
                )?.split('=').slice(1).join('=');
                const headers = {{}};
                if (csrf) {{ headers['X-Csrf-Token'] = csrf; headers['X-XSRF-Token'] = csrf; }}

                const resp = await fetch({url}, {{ credentials: 'include', headers }});
                const status = resp.status;
                if (!resp.ok) return {{ status, ok: false, hasData: false }};
                const text = await resp.text();
                let hasData = false;
                try {{
                    const json = JSON.parse(text);
                    hasData = !!json && (Array.isArray(json) ? json.length > 0 :
                        typeof json === 'object' && Object.keys(json).length > 0);
                    if (json.code !== undefined && json.code !== 0) hasData = false;
                }} catch {{}}
                return {{ status, ok: true, hasData, preview: text.slice(0, 200) }};
            }} catch (e) {{ return {{ ok: false, error: e.message, hasData: false }}; }}
        }})()
        "#,
        url = serde_json::to_string(url).unwrap_or_else(|_| format!("\"{}\"", url)),
    )
}

/// Test a single strategy against an endpoint.
async fn test_strategy(
    page: &dyn IPage,
    api_url: &str,
    strategy: Strategy,
) -> StrategyTestResult {
    let js = match strategy {
        Strategy::Public => build_public_probe_js(api_url),
        Strategy::Cookie => build_cookie_probe_js(api_url),
        Strategy::Header => build_header_probe_js(api_url),
        Strategy::Intercept | Strategy::Ui => {
            // Intercept/UI require site-specific implementation
            return StrategyTestResult {
                strategy,
                success: false,
                status_code: None,
                has_data: false,
            };
        }
    };

    match page.evaluate(&js).await {
        Ok(value) => {
            let ok = value.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
            let has_data = value.get("hasData").and_then(|v| v.as_bool()).unwrap_or(false);
            let status_code = value.get("status").and_then(|v| v.as_u64()).map(|s| s as u16);
            StrategyTestResult {
                strategy,
                success: ok && has_data,
                status_code,
                has_data,
            }
        }
        Err(e) => {
            debug!("Strategy {:?} probe failed: {}", strategy, e);
            StrategyTestResult {
                strategy,
                success: false,
                status_code: None,
                has_data: false,
            }
        }
    }
}

/// Run the cascade: try each strategy in order until one works.
///
/// Returns the simplest working strategy along with all test results.
pub async fn cascade(
    page: &dyn IPage,
    api_url: &str,
) -> Result<CascadeResult, CliError> {
    let mut tested = Vec::new();

    for &strategy in CASCADE_ORDER {
        let result = test_strategy(page, api_url, strategy).await;
        let success = result.success;
        tested.push(result);

        if success {
            debug!("Cascade found working strategy: {:?}", strategy);
            return Ok(CascadeResult {
                url: api_url.to_string(),
                strategy,
                tested,
            });
        }
    }

    // None worked — default to Cookie (most common for logged-in sites)
    debug!("Cascade: no strategy worked, defaulting to Cookie");
    Ok(CascadeResult {
        url: api_url.to_string(),
        strategy: Strategy::Cookie,
        tested,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cascade_order_starts_with_public() {
        assert_eq!(CASCADE_ORDER[0], Strategy::Public);
    }

    #[test]
    fn test_cascade_order_ends_with_intercept() {
        assert_eq!(CASCADE_ORDER[CASCADE_ORDER.len() - 1], Strategy::Intercept);
    }

    #[test]
    fn test_public_probe_js_has_no_credentials() {
        let js = build_public_probe_js("https://api.example.com/data");
        assert!(!js.contains("credentials: 'include'"));
    }

    #[test]
    fn test_cookie_probe_js_has_credentials() {
        let js = build_cookie_probe_js("https://api.example.com/data");
        assert!(js.contains("credentials: 'include'"));
    }

    #[test]
    fn test_header_probe_js_extracts_csrf() {
        let js = build_header_probe_js("https://api.example.com/data");
        assert!(js.contains("csrf"));
        assert!(js.contains("X-Csrf-Token"));
        assert!(js.contains("credentials: 'include'"));
    }

    #[test]
    fn test_probe_js_url_escaping() {
        let js = build_public_probe_js("https://api.example.com/data?q=hello&limit=10");
        // URL should be JSON-escaped in the script
        assert!(js.contains("api.example.com"));
    }
}
