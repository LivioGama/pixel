//! `pixel web-search` — deterministic web retrieval, no LLM.
//!
//! The plan-refinement gate needs an external answer for terms the index
//! cannot know ("JEV"). This command is the retrieval half of that step:
//! one bounded HTTP call per provider, JSON in, normalized results out —
//! same spirit as the index ops: bounded, marked, never an instruction.
//!
//! Provider chain (first configured wins, then free JSON fallbacks):
//!   1. SearXNG — `PIXEL_WEB_SEARCH_URL=<base>` → `GET <base>/search?format=json`
//!   2. DuckDuckGo Instant Answer — free, no key
//!   3. Wikipedia OpenSearch — free, no key
//!
//! Results are data: title, url, snippet, engine. The agent resolves the
//! term and re-runs `pixel plan`; nothing here writes a checklist.

use serde_json::{Value, json};
use std::time::Duration;

/// Results returned per invocation unless `--limit` says otherwise.
pub const DEFAULT_LIMIT: usize = 8;
/// Per-provider fetch cap: a slow endpoint must not stall the refine step.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
/// Snippet cap: enough to resolve a term, never a page dump.
const SNIPPET_CAP_CHARS: usize = 280;
/// Environment variable naming a SearXNG base URL (`https://host`, no path).
const SEARXNG_ENV: &str = "PIXEL_WEB_SEARCH_URL";

#[derive(Debug, Clone)]
pub struct WebSearchOptions {
    pub query: String,
    pub limit: usize,
    pub json: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub engine: &'static str,
}

/// The configured SearXNG base URL, if any.
fn searxng_base() -> Option<String> {
    std::env::var_os(SEARXNG_ENV)
        .and_then(|v| v.into_string().ok())
        .filter(|v| !v.is_empty())
}

pub fn run(opts: WebSearchOptions) -> Result<(), String> {
    let hits = search_with(&opts.query, opts.limit, searxng_base().as_deref(), &fetch);
    if opts.json {
        let marker = if hits.is_empty() {
            "unresolved"
        } else {
            "complete"
        };
        let payload = json!({
            "query": opts.query,
            "marker": marker,
            "hits": hits.iter().map(|h| json!({
                "title": h.title,
                "url": h.url,
                "snippet": h.snippet,
                "engine": h.engine,
            })).collect::<Vec<_>>(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).map_err(|e| e.to_string())?
        );
    } else if hits.is_empty() {
        println!("unresolved: no web results for {:?}", opts.query);
    } else {
        for (i, h) in hits.iter().enumerate() {
            println!("{}. {} [{}]", i + 1, h.title, h.engine);
            println!("   {}", h.url);
            if !h.snippet.is_empty() {
                println!("   {}", h.snippet);
            }
        }
    }
    Ok(())
}

/// The provider chain over a fetch seam: tests inject canned bodies.
fn search_with(
    query: &str,
    limit: usize,
    searxng_base: Option<&str>,
    fetch: &dyn Fn(&str) -> Result<String, String>,
) -> Vec<Hit> {
    let mut hits = Vec::new();
    if let Some(base) = searxng_base {
        let url = format!(
            "{}/search?q={}&format=json",
            base.trim_end_matches('/'),
            url_encode(query)
        );
        if let Ok(body) = fetch(&url) {
            hits.extend(parse_searxng(&body));
        }
    }
    if hits.len() < limit {
        let url = format!(
            "https://api.duckduckgo.com/?q={}&format=json&no_html=1&skip_disambig=1",
            url_encode(query)
        );
        if let Ok(body) = fetch(&url) {
            hits.extend(parse_duckduckgo(&body));
        }
    }
    if hits.len() < limit {
        let url = format!(
            "https://en.wikipedia.org/w/api.php?action=opensearch&search={}&limit={}&namespace=0&format=json",
            url_encode(query),
            limit
        );
        if let Ok(body) = fetch(&url) {
            hits.extend(parse_wikipedia(&body));
        }
    }
    dedupe_and_cap(hits, limit)
}

/// One GET, body to string. The only place the network is touched.
#[cfg_attr(test, mutants::skip)] // thin adapter over ureq; parsing is tested on bodies
fn fetch(url: &str) -> Result<String, String> {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(FETCH_TIMEOUT))
        .user_agent("pixel-cli web-search")
        .build();
    let agent = ureq::Agent::new_with_config(config);
    let mut response = agent
        .get(url)
        .call()
        .map_err(|e| format!("web-search fetch {url}: {e}"))?;
    response
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("web-search read {url}: {e}"))
}

/// SearXNG `/search?format=json`: `results[]` with title/url/content/engine.
fn parse_searxng(body: &str) -> Vec<Hit> {
    let Ok(data) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    data.get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let url = r.get("url")?.as_str()?;
            if url.is_empty() {
                return None;
            }
            Some(Hit {
                title: r
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or(url)
                    .to_string(),
                url: url.to_string(),
                snippet: clip(r.get("content").and_then(Value::as_str).unwrap_or("")),
                engine: "searxng",
            })
        })
        .collect()
}

/// DuckDuckGo Instant Answer: `Abstract*` plus `RelatedTopics[]` (which may
/// nest one level under `Topics`).
fn parse_duckduckgo(body: &str) -> Vec<Hit> {
    let Ok(data) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    let mut hits = Vec::new();
    if let (Some(text), Some(url)) = (
        data.get("AbstractText").and_then(Value::as_str),
        data.get("AbstractURL").and_then(Value::as_str),
    ) && !text.is_empty()
        && !url.is_empty()
    {
        hits.push(Hit {
            title: data
                .get("Heading")
                .and_then(Value::as_str)
                .unwrap_or(url)
                .to_string(),
            url: url.to_string(),
            snippet: clip(text),
            engine: "duckduckgo",
        });
    }
    collect_ddg_topics(
        data.get("RelatedTopics").and_then(Value::as_array),
        &mut hits,
    );
    hits
}

fn collect_ddg_topics(topics: Option<&Vec<Value>>, hits: &mut Vec<Hit>) {
    let Some(topics) = topics else { return };
    for topic in topics {
        if let Some(nested) = topic.get("Topics").and_then(Value::as_array) {
            for t in nested {
                push_ddg_topic(t, hits);
            }
        } else {
            push_ddg_topic(topic, hits);
        }
    }
}

fn push_ddg_topic(topic: &Value, hits: &mut Vec<Hit>) {
    let (Some(text), Some(url)) = (
        topic.get("Text").and_then(Value::as_str),
        topic.get("FirstURL").and_then(Value::as_str),
    ) else {
        return;
    };
    if text.is_empty() || url.is_empty() {
        return;
    }
    hits.push(Hit {
        title: clip_len(text.split(" - ").next().unwrap_or(text), 120),
        url: url.to_string(),
        snippet: clip(text),
        engine: "duckduckgo",
    });
}

/// Wikipedia OpenSearch: `[term, [titles], [descriptions], [urls]]`.
fn parse_wikipedia(body: &str) -> Vec<Hit> {
    let Ok(data) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    let Some(rows) = data.as_array() else {
        return Vec::new();
    };
    let titles = rows.get(1).and_then(Value::as_array);
    let descs = rows.get(2).and_then(Value::as_array);
    let urls = rows.get(3).and_then(Value::as_array);
    let (Some(titles), Some(urls)) = (titles, urls) else {
        return Vec::new();
    };
    titles
        .iter()
        .zip(urls.iter())
        .enumerate()
        .filter_map(|(i, (t, u))| {
            let url = u.as_str()?;
            if url.is_empty() {
                return None;
            }
            Some(Hit {
                title: t.as_str().unwrap_or(url).to_string(),
                url: url.to_string(),
                snippet: clip(
                    descs
                        .and_then(|d| d.get(i))
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                ),
                engine: "wikipedia",
            })
        })
        .collect()
}

/// First occurrence wins across providers; the cap applies after dedupe so
/// `limit` always means distinct results.
fn dedupe_and_cap(hits: Vec<Hit>, limit: usize) -> Vec<Hit> {
    let mut seen = std::collections::HashSet::new();
    hits.into_iter()
        .filter(|h| seen.insert(h.url.clone()))
        .take(limit)
        .collect()
}

fn clip(text: &str) -> String {
    clip_len(text, SNIPPET_CAP_CHARS)
}

fn clip_len(text: &str, cap: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= cap {
        return text;
    }
    let mut s: String = text.chars().take(cap).collect();
    s.push('…');
    s
}

/// RFC 3986 unreserved set — the only characters passed through verbatim.
fn url_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn searxng_results_normalize_and_skip_empty_urls() {
        let body = r#"{"results": [
            {"title": "JEV", "url": "https://example.test/jev", "content": "Joint Embedded Validator"},
            {"url": ""},
            {"title": "No URL"}
        ]}"#;
        let hits = parse_searxng(body);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "JEV");
        assert_eq!(hits[0].snippet, "Joint Embedded Validator");
        assert_eq!(hits[0].engine, "searxng");
    }

    #[test]
    fn duckduckgo_abstract_and_nested_topics() {
        let body = r#"{
            "Heading": "JEV",
            "AbstractText": "A thing.",
            "AbstractURL": "https://example.test/a",
            "RelatedTopics": [
                {"Text": "One - first", "FirstURL": "https://example.test/1"},
                {"Topics": [{"Text": "Two - second", "FirstURL": "https://example.test/2"}]},
                {"Text": "", "FirstURL": "https://example.test/skip"}
            ]
        }"#;
        let hits = parse_duckduckgo(body);
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].title, "JEV");
        assert_eq!(hits[0].snippet, "A thing.");
        assert_eq!(hits[2].url, "https://example.test/2");
        assert_eq!(hits[2].title, "Two");
    }

    #[test]
    fn wikipedia_rows_zip_titles_descriptions_urls() {
        let body = r#"["jev", ["JEV", "JEV (band)"], ["Joint Embedded Validator", ""],
            ["https://en.wikipedia.org/wiki/JEV", "https://en.wikipedia.org/wiki/JEV_(band)"]]"#;
        let hits = parse_wikipedia(body);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "JEV");
        assert_eq!(hits[0].snippet, "Joint Embedded Validator");
        assert_eq!(hits[1].snippet, "");
    }

    #[test]
    fn malformed_bodies_yield_no_hits() {
        assert!(parse_searxng("not json").is_empty());
        assert!(parse_duckduckgo("{}").is_empty());
        assert!(parse_wikipedia("[\"jev\"]").is_empty());
    }

    #[test]
    fn dedupe_keeps_first_and_cap_applies_after() {
        let hit = |url: &str| Hit {
            title: url.into(),
            url: url.into(),
            snippet: String::new(),
            engine: "duckduckgo",
        };
        let hits = vec![hit("a"), hit("a"), hit("b"), hit("c")];
        let out = dedupe_and_cap(hits, 2);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].url, "a");
        assert_eq!(out[1].url, "b");
    }

    #[test]
    fn clip_collapses_whitespace_and_marks_truncation() {
        assert_eq!(clip("a  b\n c"), "a b c");
        let long = "x".repeat(SNIPPET_CAP_CHARS + 10);
        let clipped = clip(&long);
        assert!(clipped.ends_with('…'));
        assert_eq!(clipped.chars().count(), SNIPPET_CAP_CHARS + 1);
        let exact = "y".repeat(SNIPPET_CAP_CHARS);
        assert_eq!(clip(&exact).chars().count(), SNIPPET_CAP_CHARS);
    }

    #[test]
    fn url_encode_percent_encodes_only_non_unreserved() {
        assert_eq!(url_encode("a b&c"), "a%20b%26c");
        assert_eq!(url_encode("JEV-2.0_~"), "JEV-2.0_~");
        assert_eq!(url_encode("été"), "%C3%A9t%C3%A9");
    }

    #[test]
    fn fallback_chain_skips_later_providers_when_full() {
        let searxng_hit = r#"{"results":[{"title":"t","url":"u1","content":"c"}]}"#;
        let calls = std::cell::RefCell::new(Vec::new());
        let fetch = |url: &str| -> Result<String, String> {
            calls.borrow_mut().push(url.to_string());
            Ok(if url.contains("/search?") {
                searxng_hit.to_string()
            } else {
                "{}".to_string()
            })
        };
        let hits = search_with("q", 1, Some("https://sx.test"), &fetch);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "u1");
        assert_eq!(calls.borrow().len(), 1);

        // Under the limit, the chain falls through to the free providers.
        let hits = search_with("q", 8, Some("https://sx.test"), &fetch);
        assert_eq!(calls.borrow().len(), 4);
        assert_eq!(hits.len(), 1);
    }
}
