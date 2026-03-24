//! Adapter generation: produce YAML adapter candidates from an explore manifest.

use std::collections::HashSet;

use opencli_rs_core::CliError;
use tracing::debug;

use crate::explore::{detect_site_name, infer_capability_name};
use crate::types::{
    AdapterCandidate, DiscoveredEndpoint, ExploreManifest, FieldInfo, SynthesizeOptions,
    LIMIT_PARAMS, PAGINATION_PARAMS, SEARCH_PARAMS, VOLATILE_PARAMS,
};

/// Synthesize adapter candidates from an explore manifest.
///
/// For each discovered endpoint, generates a YAML adapter with a
/// fetch/evaluate pipeline, inferred args from URL parameters, and
/// columns from response fields. Returns top candidates sorted by confidence.
pub fn synthesize(
    manifest: &ExploreManifest,
    options: SynthesizeOptions,
) -> Result<Vec<AdapterCandidate>, CliError> {
    let site = options
        .site
        .as_deref()
        .unwrap_or_else(|| manifest.url.as_str());
    let site_name = detect_site_name(site);

    let mut candidates = Vec::new();
    let mut used_names = HashSet::new();

    // Process top 8 endpoints by confidence
    let mut endpoints = manifest.endpoints.clone();
    endpoints.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap_or(std::cmp::Ordering::Equal));

    for ep in endpoints.iter().take(8) {
        let mut cap_name = infer_capability_name(&ep.url, options.goal.as_deref());
        if used_names.contains(&cap_name) {
            // Disambiguate with a suffix
            let suffix = url_last_segment(&ep.url);
            cap_name = if let Some(s) = suffix {
                format!("{}_{}", cap_name, s)
            } else {
                format!("{}_{}", cap_name, used_names.len())
            };
        }
        used_names.insert(cap_name.clone());

        let yaml = build_adapter_yaml(&site_name, manifest, &cap_name, ep);
        let description = format!("{} {} (auto-generated)", site_name, cap_name);

        candidates.push(AdapterCandidate {
            site: site_name.clone(),
            name: cap_name,
            description,
            strategy: ep.auth_level,
            yaml,
            confidence: ep.confidence,
        });
    }

    // Sort by confidence descending, return top 3
    candidates.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap_or(std::cmp::Ordering::Equal));
    candidates.truncate(3);

    debug!("Synthesized {} adapter candidates for {}", candidates.len(), site_name);
    Ok(candidates)
}

// ── YAML generation ─────────────────────────────────────────────────────────

fn build_adapter_yaml(
    site: &str,
    manifest: &ExploreManifest,
    name: &str,
    endpoint: &DiscoveredEndpoint,
) -> String {
    let needs_browser = endpoint.auth_level.requires_browser();
    let templated_url = build_templated_url(&endpoint.url, &endpoint.fields);

    // Determine columns
    let columns = infer_columns(&endpoint.fields);

    // Build args
    let args_section = build_args_section(&endpoint.url, &endpoint.fields);

    // Build pipeline
    let pipeline = if needs_browser {
        build_browser_pipeline(&manifest.url, &templated_url, &endpoint.fields, &columns)
    } else {
        build_fetch_pipeline(&templated_url, &columns)
    };

    let mut lines = Vec::new();
    lines.push(format!("site: {}", site));
    lines.push(format!("name: {}", name));
    lines.push(format!(
        "description: \"{} {} (auto-generated)\"",
        site, name
    ));

    // Extract domain
    if let Ok(parsed) = url::Url::parse(&manifest.url) {
        if let Some(host) = parsed.host_str() {
            lines.push(format!("domain: {}", host));
        }
    }

    lines.push(format!("strategy: {}", endpoint.auth_level));
    lines.push(format!("browser: {}", needs_browser));
    lines.push(String::new());

    // Args
    if !args_section.is_empty() {
        lines.push("args:".to_string());
        lines.push(args_section);
    }

    // Pipeline
    lines.push("pipeline:".to_string());
    lines.push(pipeline);

    // Columns
    if !columns.is_empty() {
        lines.push(format!(
            "columns: [{}]",
            columns
                .iter()
                .map(|c| format!("\"{}\"", c))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    lines.join("\n")
}

/// Determine output columns from field roles.
fn infer_columns(fields: &[FieldInfo]) -> Vec<String> {
    let preferred_order = ["title", "url", "author", "score", "time"];
    let mut cols = Vec::new();
    for &role in &preferred_order {
        if fields.iter().any(|f| f.role.as_deref() == Some(role)) {
            cols.push(role.to_string());
        }
    }
    if cols.is_empty() {
        // Fallback
        cols.push("title".to_string());
        cols.push("url".to_string());
    }
    cols
}

/// Build a templated URL with Jinja-style arg placeholders.
fn build_templated_url(url: &str, fields: &[FieldInfo]) -> String {
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return url.to_string(),
    };
    let base = format!(
        "{}://{}{}",
        parsed.scheme(),
        parsed.host_str().unwrap_or(""),
        parsed.path()
    );

    let has_keyword = fields.iter().any(|f| f.role.as_deref() == Some("keyword"));
    let mut params = Vec::new();

    for (k, v) in parsed.query_pairs() {
        if VOLATILE_PARAMS.contains(&k.as_ref()) {
            continue;
        }
        if has_keyword && SEARCH_PARAMS.contains(&k.as_ref()) {
            params.push(format!("{}=${{{{ args.keyword }}}}", k));
        } else if LIMIT_PARAMS.contains(&k.as_ref()) {
            params.push(format!("{}=${{{{ args.limit | default(20) }}}}", k));
        } else if PAGINATION_PARAMS.contains(&k.as_ref()) {
            params.push(format!("{}=${{{{ args.page | default(1) }}}}", k));
        } else {
            params.push(format!("{}={}", k, v));
        }
    }

    if params.is_empty() {
        base
    } else {
        format!("{}?{}", base, params.join("&"))
    }
}

/// Build the args section of the YAML.
fn build_args_section(url: &str, _fields: &[FieldInfo]) -> String {
    let qp = extract_query_param_names(url);
    let has_search = qp.iter().any(|p| SEARCH_PARAMS.contains(&p.as_str()));
    let has_pagination = qp.iter().any(|p| PAGINATION_PARAMS.contains(&p.as_str()));

    let mut lines = Vec::new();
    if has_search {
        lines.push("  keyword:".to_string());
        lines.push("    type: str".to_string());
        lines.push("    required: true".to_string());
        lines.push("    description: Search keyword".to_string());
    }
    lines.push("  limit:".to_string());
    lines.push("    type: int".to_string());
    lines.push("    default: 20".to_string());
    lines.push("    description: Number of items to return".to_string());
    if has_pagination {
        lines.push("  page:".to_string());
        lines.push("    type: int".to_string());
        lines.push("    default: 1".to_string());
        lines.push("    description: Page number".to_string());
    }
    lines.join("\n")
}

/// Build a browser-based pipeline (navigate + evaluate).
fn build_browser_pipeline(
    navigate_url: &str,
    fetch_url: &str,
    fields: &[FieldInfo],
    columns: &[String],
) -> String {
    let mut lines = Vec::new();
    lines.push(format!("  - navigate: \"{}\"", navigate_url));
    lines.push("  - wait: 3".to_string());

    // Build evaluate script
    let map_code = build_map_code(fields, columns);
    let evaluate = format!(
        concat!(
            "  - evaluate: |\n",
            "      (async () => {{\n",
            "        const res = await fetch(\"{}\", {{ credentials: 'include' }});\n",
            "        const data = await res.json();\n",
            "        const items = findItems(data);\n",
            "        return items{};\n",
            "      }})()\n",
        ),
        fetch_url, map_code
    );
    lines.push(evaluate);

    // Map + limit
    lines.push(build_map_step(columns));
    lines.push("  - limit: \"${{ args.limit | default(20) }}\"".to_string());
    lines.join("\n")
}

/// Build a public fetch pipeline.
fn build_fetch_pipeline(url: &str, columns: &[String]) -> String {
    let mut lines = Vec::new();
    lines.push(format!("  - fetch:\n      url: \"{}\"", url));
    lines.push(build_map_step(columns));
    lines.push("  - limit: \"${{ args.limit | default(20) }}\"".to_string());
    lines.join("\n")
}

fn build_map_code(fields: &[FieldInfo], columns: &[String]) -> String {
    let mappings: Vec<String> = columns
        .iter()
        .filter_map(|col| {
            let field = fields.iter().find(|f| f.role.as_deref() == Some(col.as_str()));
            let field_path = field.map(|f| &f.name).cloned().unwrap_or_else(|| col.clone());
            let chain = field_path
                .split('.')
                .map(|p| format!("?.{}", p))
                .collect::<String>();
            Some(format!("        {}: item{}", col, chain))
        })
        .collect();

    if mappings.is_empty() {
        String::new()
    } else {
        format!(".map((item) => ({{\n{}\n      }}))", mappings.join(",\n"))
    }
}

fn build_map_step(columns: &[String]) -> String {
    let mut lines = Vec::new();
    lines.push("  - map:".to_string());
    lines.push("      rank: \"${{ index + 1 }}\"".to_string());
    for col in columns {
        lines.push(format!("      {}: \"${{{{ item.{} }}}}\"", col, col));
    }
    lines.join("\n")
}

fn extract_query_param_names(url: &str) -> Vec<String> {
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

fn url_last_segment(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    parsed
        .path_segments()?
        .filter(|s| {
            !s.is_empty()
                && !s.chars().all(|c| c.is_ascii_digit())
                && !(s.len() >= 8 && s.chars().all(|c| c.is_ascii_hexdigit()))
        })
        .last()
        .map(|s| {
            s.chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect::<String>()
                .to_lowercase()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FieldInfo;
    use opencli_rs_core::Strategy;

    fn sample_manifest() -> ExploreManifest {
        ExploreManifest {
            url: "https://www.example.com/hot".to_string(),
            title: Some("Example Hot".to_string()),
            endpoints: vec![
                DiscoveredEndpoint {
                    url: "https://api.example.com/v1/hot?limit=20".to_string(),
                    method: "GET".to_string(),
                    content_type: Some("application/json".to_string()),
                    fields: vec![
                        FieldInfo {
                            name: "title".to_string(),
                            role: Some("title".to_string()),
                            field_type: "string".to_string(),
                        },
                        FieldInfo {
                            name: "url".to_string(),
                            role: Some("url".to_string()),
                            field_type: "string".to_string(),
                        },
                        FieldInfo {
                            name: "author".to_string(),
                            role: Some("author".to_string()),
                            field_type: "string".to_string(),
                        },
                    ],
                    confidence: 0.85,
                    auth_level: Strategy::Public,
                    sample_response: None,
                },
                DiscoveredEndpoint {
                    url: "https://api.example.com/v1/search?q=test&limit=20".to_string(),
                    method: "GET".to_string(),
                    content_type: Some("application/json".to_string()),
                    fields: vec![
                        FieldInfo {
                            name: "title".to_string(),
                            role: Some("title".to_string()),
                            field_type: "string".to_string(),
                        },
                        FieldInfo {
                            name: "url".to_string(),
                            role: Some("url".to_string()),
                            field_type: "string".to_string(),
                        },
                    ],
                    confidence: 0.70,
                    auth_level: Strategy::Cookie,
                    sample_response: None,
                },
            ],
            framework: Some("React".to_string()),
            store: None,
            auth_indicators: vec![],
        }
    }

    #[test]
    fn test_synthesize_returns_candidates() {
        let manifest = sample_manifest();
        let options = SynthesizeOptions::default();
        let candidates = synthesize(&manifest, options).unwrap();
        assert!(!candidates.is_empty());
        assert!(candidates.len() <= 3);
    }

    #[test]
    fn test_synthesize_candidate_has_yaml() {
        let manifest = sample_manifest();
        let options = SynthesizeOptions::default();
        let candidates = synthesize(&manifest, options).unwrap();
        for c in &candidates {
            assert!(!c.yaml.is_empty());
            assert!(c.yaml.contains("site:"));
            assert!(c.yaml.contains("pipeline:"));
        }
    }

    #[test]
    fn test_synthesize_sorted_by_confidence() {
        let manifest = sample_manifest();
        let options = SynthesizeOptions::default();
        let candidates = synthesize(&manifest, options).unwrap();
        for window in candidates.windows(2) {
            assert!(window[0].confidence >= window[1].confidence);
        }
    }

    #[test]
    fn test_synthesize_with_goal() {
        let manifest = sample_manifest();
        let options = SynthesizeOptions {
            site: None,
            goal: Some("trending".to_string()),
        };
        let candidates = synthesize(&manifest, options).unwrap();
        // All should be named "trending" (or disambiguated)
        assert!(candidates[0].name.contains("trending"));
    }

    #[test]
    fn test_build_templated_url() {
        let fields = vec![];
        let url = build_templated_url("https://api.example.com/data?limit=20&_=12345", &fields);
        assert!(url.contains("limit="));
        assert!(!url.contains("_="));
    }

    #[test]
    fn test_infer_columns() {
        let fields = vec![
            FieldInfo { name: "title".into(), role: Some("title".into()), field_type: "string".into() },
            FieldInfo { name: "score".into(), role: Some("score".into()), field_type: "number".into() },
        ];
        let cols = infer_columns(&fields);
        assert_eq!(cols, vec!["title", "score"]);
    }
}
