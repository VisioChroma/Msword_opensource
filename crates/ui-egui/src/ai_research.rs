//! "Word" AI, part 2: research and source verification.
//!
//! Sources come from real search results, never from the model's memory:
//!  1. Gemini with Google Search grounding researches the request and returns the pages it used
//!     and which passages each page supports.
//!  2. Crossref adds peer-reviewed articles (with DOIs).
//!  3. Sources are ranked: official documentation, primary research, credible publications, other web.
//!  4. The writer model sees the numbered sources and may cite only those numbers.
//!  5. Afterwards every citation is checked: unknown numbers are removed and the claim is flagged.
//!     The References list is built from the source records, not written by the model.

use std::collections::BTreeSet;

use crate::ai_plan::{Block, Plan};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    Official,
    Primary,
    Publication,
    Web,
}

impl Tier {
    pub fn label(self) -> &'static str {
        match self {
            Tier::Official => "Official documentation",
            Tier::Primary => "Primary research",
            Tier::Publication => "Credible publication",
            Tier::Web => "Web source",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Source {
    pub title: String,
    pub publisher: String,
    pub url: String,
    pub tier: Tier,
    /// Authors and year, when known.
    pub detail: String,
    /// Passages of the research notes this page was found to support.
    pub evidence: Vec<String>,
}

#[derive(Default)]
pub struct Research {
    pub notes: String,
    pub sources: Vec<Source>,
    pub warnings: Vec<String>,
}

#[derive(Default, Debug)]
pub struct Report {
    pub cited: usize,
    pub assumptions: usize,
    pub suggestions: usize,
    pub flagged: usize,
    pub removed: usize,
    pub used: usize,
}

pub struct Outcome {
    pub plan: Plan,
    pub label: String,
    pub summary: String,
}

const ADDENDUM: &str = r#"RESEARCH MODE
The user message holds RESEARCH NOTES and a numbered SOURCES list collected from real search results, ranked best first (official documentation, primary research, credible publications, other web).
- End every factual claim (figures, dates, findings, definitions, quotations) with the numbers of the sources that support it, written like [2] or [1][3], directly after the claim.
- Cite ONLY numbers that exist in SOURCES, and only where that source's "supports" text or title clearly backs the claim. Never invent a source, number, author, URL or statistic.
- A factual claim you cannot tie to a source must end with [source needed].
- Your own inferences must start with "Assumption:". Your own recommendations must start with "AI suggestion:". Do not cite sources on those.
- Prefer higher-ranked sources when several support a claim.
- No citation markers in the title or headings, and do not write a References section: the app adds it."#;

pub fn system_prompt() -> String {
    format!("{}\n\n{}", crate::ai_plan::SYSTEM_PROMPT, ADDENDUM)
}

fn host_of(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    host.split(':').next().unwrap_or("").trim_start_matches("www.").to_ascii_lowercase()
}

fn on(host: &str, list: &[&str]) -> bool {
    list.iter().any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

/// A rough, domain-based ranking (a heuristic, not a judgement of the page).
fn tier_of(host: &str) -> Tier {
    const PRIMARY: &[&str] = &[
        "doi.org", "arxiv.org", "nature.com", "science.org", "sciencedirect.com", "springer.com", "wiley.com", "ieee.org", "acm.org", "jstor.org",
        "plos.org", "cell.com", "nejm.org", "thelancet.com", "openreview.net", "aclanthology.org", "pubmed.ncbi.nlm.nih.gov", "biorxiv.org", "medrxiv.org",
    ];
    const OFFICIAL: &[&str] = &["w3.org", "ietf.org", "iso.org", "who.int", "europa.eu", "un.org", "oecd.org", "worldbank.org", "imf.org"];
    const PUBLICATION: &[&str] = &[
        "reuters.com", "apnews.com", "bbc.com", "bbc.co.uk", "nytimes.com", "ft.com", "economist.com", "wsj.com", "theguardian.com", "bloomberg.com",
        "hbr.org", "mckinsey.com", "technologyreview.com", "wired.com", "scientificamerican.com", "britannica.com",
    ];
    let official_suffix =
        host.ends_with(".gov") || host.contains(".gov.") || host.ends_with(".edu") || host.contains(".edu.") || host.ends_with(".ac.uk") || host.ends_with(".int");
    let docs_host = ["docs.", "developer.", "developers.", "learn.", "support."].iter().any(|p| host.starts_with(p));
    if on(host, PRIMARY) {
        Tier::Primary
    } else if official_suffix || docs_host || on(host, OFFICIAL) {
        Tier::Official
    } else if on(host, PUBLICATION) {
        Tier::Publication
    } else {
        Tier::Web
    }
}

/// What the writer model is shown: the request, the notes and the numbered sources.
pub fn writer_message(request: &str, r: &Research) -> String {
    let mut s = format!("REQUEST:\n{request}\n\nRESEARCH NOTES:\n{}\n\nSOURCES (cite by number only; ranked best first):\n", r.notes);
    for (i, src) in r.sources.iter().enumerate() {
        s.push_str(&format!("[{}] {} | {} | {} | {}\n", i + 1, src.tier.label(), src.title, src.publisher, src.detail));
        for e in &src.evidence {
            s.push_str(&format!("    supports: \"{e}\"\n"));
        }
    }
    s
}

/// Check the citations in one piece of text: known `[n]` stay, unknown ones are removed and the
/// claim is flagged `[source needed]`.
fn fix_text(text: &str, n: usize, used: &mut BTreeSet<usize>, rep: &mut Report) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let (mut i, mut valid, mut bad) = (0usize, 0usize, 0usize);
    while let Some(&c) = chars.get(i) {
        if c == '[' {
            let mut j = i + 1;
            let mut num = String::new();
            while let Some(&d) = chars.get(j) {
                if d.is_ascii_digit() && num.len() < 4 {
                    num.push(d);
                    j += 1;
                } else {
                    break;
                }
            }
            if !num.is_empty() && chars.get(j) == Some(&']') {
                match num.parse::<usize>() {
                    Ok(k) if (1..=n).contains(&k) => {
                        used.insert(k);
                        out.push_str(&format!("[{k}]"));
                        valid += 1;
                    }
                    _ => bad += 1,
                }
                i = j + 1;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    let mut out = out.trim_end().to_string();
    if bad > 0 && !out.contains("[source needed]") {
        out.push_str(" [source needed]");
    }
    rep.removed += bad;
    if out.starts_with("Assumption:") {
        rep.assumptions += 1;
    } else if out.starts_with("AI suggestion:") {
        rep.suggestions += 1;
    }
    if out.contains("[source needed]") {
        rep.flagged += 1;
    }
    if valid > 0 {
        rep.cited += 1;
    }
    out
}

fn cite_line(s: &Source) -> String {
    let mut parts = Vec::new();
    if !s.detail.is_empty() {
        parts.push(s.detail.clone());
    }
    if !s.title.is_empty() {
        parts.push(s.title.clone());
    }
    if !s.publisher.is_empty() && s.publisher != s.title {
        parts.push(s.publisher.clone());
    }
    parts.push(s.url.clone());
    format!("{} ({})", parts.join(". "), s.tier.label())
}

/// Verify every citation in the plan, then append a References section built from the real
/// source records (only the sources that are cited).
pub fn verify(plan: &mut Plan, sources: &[Source]) -> Report {
    let mut rep = Report::default();
    let mut used = BTreeSet::new();
    let n = sources.len();
    for block in plan.blocks.iter_mut() {
        match block {
            Block::Paragraph(t) => *t = fix_text(t, n, &mut used, &mut rep),
            Block::Bullets(items) | Block::Numbered(items) => {
                for it in items.iter_mut() {
                    *it = fix_text(it, n, &mut used, &mut rep);
                }
            }
            Block::Table { rows, .. } => {
                for row in rows.iter_mut() {
                    for cell in row.iter_mut() {
                        *cell = fix_text(cell, n, &mut used, &mut rep);
                    }
                }
            }
            _ => {}
        }
    }
    if !used.is_empty() {
        plan.blocks.push(Block::Heading(1, "References".to_string()));
        for id in &used {
            if let Some(s) = id.checked_sub(1).and_then(|i| sources.get(i)) {
                plan.blocks.push(Block::Paragraph(format!("[{id}] {}", cite_line(s))));
            }
        }
    }
    rep.used = used.len();
    rep
}

fn summary(r: &Research, rep: &Report) -> String {
    let count = |t: Tier| r.sources.iter().filter(|s| s.tier == t).count();
    let mut s = format!(
        "Research: {} sources found ({} official, {} primary research, {} credible publications, {} other web); {} cited in the document.\nClaims: {} sourced, {} assumptions, {} AI suggestions, {} flagged [source needed]",
        r.sources.len(),
        count(Tier::Official),
        count(Tier::Primary),
        count(Tier::Publication),
        count(Tier::Web),
        rep.used,
        rep.cited,
        rep.assumptions,
        rep.suggestions,
        rep.flagged,
    );
    if rep.removed > 0 {
        s.push_str(&format!(" ({} invalid citations removed)", rep.removed));
    }
    s.push('.');
    for w in &r.warnings {
        s.push_str(&format!("\nNote: {w}"));
    }
    s
}

#[cfg(target_arch = "wasm32")]
pub fn run(_request: &str, _only: Option<usize>) -> Result<Outcome, String> {
    Err("Research works in the desktop app".into())
}

/// Research, write, verify.
#[cfg(not(target_arch = "wasm32"))]
pub fn run(request: &str, only: Option<usize>) -> Result<Outcome, String> {
    let research = net::gather(request)?;
    let message = writer_message(request, &research);
    let reply = crate::ai_net::ask(&system_prompt(), &message, only, |t| crate::ai_plan::parse_plan(t).is_ok())?;
    let mut plan = crate::ai_plan::parse_plan(&reply.text)?;
    let report = verify(&mut plan, &research.sources);
    Ok(Outcome { plan, label: reply.label, summary: summary(&research, &report) })
}

#[cfg(not(target_arch = "wasm32"))]
mod net {
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::{Research, Source, Tier, host_of, tier_of};

    /// Most web pages kept from the grounded search.
    const MAX_WEB: usize = 8;
    const MAX_SOURCES: usize = 12;

    const RESEARCH_PROMPT: &str = "You are a research assistant. Research the topic in the user's request using web search. \
Prefer official documentation, primary research and credible publications over blogs and marketing pages. \
Write concise factual notes (short paragraphs) of the key facts; give figures only when a source states them. \
Do not write the final document. End with one line exactly like: SEARCH_TERMS: <academic query one> | <academic query two>";

    fn agent(secs: u64) -> ureq::Agent {
        let config = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(secs))).http_status_as_error(false).build();
        ureq::Agent::new_with_config(config)
    }

    fn get_json(url: &str) -> Result<Value, String> {
        let mut resp = agent(20).get(url).call().map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().map_err(|e| e.to_string())?;
        if !(200..300).contains(&status) {
            return Err(format!("HTTP {status}"));
        }
        serde_json::from_str(&text).map_err(|e| e.to_string())
    }

    fn post_json(url: &str, header: (&str, &str), body: &Value) -> Result<Value, String> {
        let mut resp = agent(90).post(url).header(header.0, header.1).send_json(body).map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().map_err(|e| e.to_string())?;
        if !(200..300).contains(&status) {
            return Err(format!("HTTP {status}: {}", text.chars().take(300).collect::<String>()));
        }
        serde_json::from_str(&text).map_err(|e| e.to_string())
    }

    fn enc(s: &str) -> String {
        s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
    }

    /// Google's search result links are redirects: follow one hop to get the real page address.
    fn resolve(url: &str) -> String {
        if !url.contains("vertexaisearch.cloud.google.com") {
            return url.to_string();
        }
        let config = ureq::Agent::config_builder()
            .max_redirects(0)
            .max_redirects_will_error(false)
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(8)))
            .build();
        let agent = ureq::Agent::new_with_config(config);
        agent
            .get(url)
            .call()
            .ok()
            .and_then(|r| r.headers().get("location").and_then(|v| v.to_str().ok()).map(str::to_string))
            .unwrap_or_else(|| url.to_string())
    }

    struct Grounded {
        text: String,
        /// (title, uri) in the search's own order; indices match `supports`.
        chunks: Vec<(String, String)>,
        /// (supported text, chunk indices)
        supports: Vec<(String, Vec<usize>)>,
    }

    fn grounded(request: &str) -> Result<Grounded, String> {
        let key = crate::ai_net::key(crate::ai_net::Provider::Gemini).ok_or("research needs GEMINI_API_KEY (it uses Google Search grounding)")?;
        let body = json!({
            "systemInstruction": {"parts": [{"text": RESEARCH_PROMPT}]},
            "contents": [{"role": "user", "parts": [{"text": request}]}],
            "tools": [{"google_search": {}}],
            "generationConfig": {"temperature": 0.2}
        });
        let v = post_json("https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent", ("x-goog-api-key", &key), &body)
            .map_err(|e| format!("web research failed: {e}"))?;
        let cand = v.pointer("/candidates/0");
        let text = cand
            .and_then(|c| c.pointer("/content/parts"))
            .and_then(Value::as_array)
            .map(|p| p.iter().filter_map(|x| x.get("text").and_then(Value::as_str)).collect::<String>())
            .unwrap_or_default();
        let meta = cand.and_then(|c| c.get("groundingMetadata"));
        let chunks: Vec<(String, String)> = meta
            .and_then(|m| m.get("groundingChunks"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|c| {
                        (
                            c.pointer("/web/title").and_then(Value::as_str).unwrap_or("").to_string(),
                            c.pointer("/web/uri").and_then(Value::as_str).unwrap_or("").to_string(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let supports: Vec<(String, Vec<usize>)> = meta
            .and_then(|m| m.get("groundingSupports"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|s| {
                        (
                            s.pointer("/segment/text").and_then(Value::as_str).unwrap_or("").to_string(),
                            s.get("groundingChunkIndices")
                                .and_then(Value::as_array)
                                .map(|i| i.iter().filter_map(Value::as_u64).map(|n| n as usize).collect())
                                .unwrap_or_default(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        if text.trim().is_empty() {
            return Err("web research returned nothing".into());
        }
        Ok(Grounded { text, chunks, supports })
    }

    /// The notes without the SEARCH_TERMS line, and the terms.
    fn split_terms(text: &str) -> (String, Vec<String>) {
        let mut terms = Vec::new();
        let mut notes = Vec::new();
        for line in text.lines() {
            if let Some(rest) = line.trim().strip_prefix("SEARCH_TERMS:") {
                terms = rest.split('|').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect();
            } else {
                notes.push(line);
            }
        }
        (notes.join("\n").trim().to_string(), terms)
    }

    /// Peer-reviewed articles for a query (Crossref: free, no key).
    fn crossref(term: &str) -> Result<Vec<Source>, String> {
        let url = format!(
            "https://api.crossref.org/works?query={}&rows=3&filter=type:journal-article&select=DOI,title,author,issued,container-title",
            enc(term)
        );
        let v = get_json(&url)?;
        let items = v.pointer("/message/items").and_then(Value::as_array).cloned().unwrap_or_default();
        Ok(items
            .iter()
            .filter_map(|it| {
                let title = it.pointer("/title/0").and_then(Value::as_str)?.trim().to_string();
                let doi = it.get("DOI").and_then(Value::as_str)?;
                if title.is_empty() {
                    return None;
                }
                let journal = it.pointer("/container-title/0").and_then(Value::as_str).unwrap_or("").to_string();
                let year = it.pointer("/issued/date-parts/0/0").and_then(Value::as_i64).map(|y| y.to_string()).unwrap_or_default();
                let authors = it
                    .get("author")
                    .and_then(Value::as_array)
                    .map(|a| {
                        let names: Vec<String> = a.iter().take(3).filter_map(|x| x.get("family").and_then(Value::as_str).map(str::to_string)).collect();
                        if a.len() > 3 { format!("{} et al.", names.join(", ")) } else { names.join(", ") }
                    })
                    .unwrap_or_default();
                let detail = match (authors.is_empty(), year.is_empty()) {
                    (false, false) => format!("{authors} ({year})"),
                    (false, true) => authors,
                    (true, false) => format!("({year})"),
                    (true, true) => String::new(),
                };
                Some(Source { title, publisher: journal, url: format!("https://doi.org/{doi}"), tier: Tier::Primary, detail, evidence: Vec::new() })
            })
            .collect())
    }

    pub fn gather(request: &str) -> Result<Research, String> {
        let g = grounded(request)?;
        let (notes, terms) = split_terms(&g.text);
        let mut warnings = Vec::new();

        // Real page addresses, looked up in parallel.
        let urls: Vec<String> = std::thread::scope(|s| {
            let handles: Vec<_> = g.chunks.iter().take(MAX_WEB).map(|(_, u)| s.spawn(move || resolve(u))).collect();
            handles.into_iter().map(|h| h.join().unwrap_or_default()).collect()
        });

        let mut sources: Vec<Source> = Vec::new();
        for (i, ((title, orig), resolved)) in g.chunks.iter().zip(urls.iter()).enumerate() {
            if orig.is_empty() {
                continue;
            }
            let url = if resolved.is_empty() { orig.clone() } else { resolved.clone() };
            if sources.iter().any(|s| s.url == url) {
                continue;
            }
            let host = host_of(&url);
            let mut evidence: Vec<String> = Vec::new();
            for (seg, idx) in &g.supports {
                if idx.contains(&i) && !seg.is_empty() && evidence.len() < 4 {
                    let e: String = seg.chars().take(220).collect();
                    if !evidence.contains(&e) {
                        evidence.push(e);
                    }
                }
            }
            let tier = tier_of(&host);
            let name = if title.is_empty() { host.clone() } else { title.clone() };
            sources.push(Source { title: name, publisher: host, url, tier, detail: String::new(), evidence });
        }

        let mut scholarly_failed = false;
        for term in terms.iter().take(2) {
            match crossref(term) {
                Ok(found) => {
                    for f in found {
                        if !sources.iter().any(|s| s.url == f.url) {
                            sources.push(f);
                        }
                    }
                }
                Err(_) => scholarly_failed = true,
            }
        }
        if scholarly_failed {
            warnings.push("the scholarly lookup (Crossref) was unavailable, so journal articles may be missing".into());
        }

        // Best tier first; the sort is stable, so the search's own order holds inside a tier.
        sources.sort_by_key(|s| s.tier);
        sources.truncate(MAX_SOURCES);
        if sources.is_empty() {
            warnings.push("no sources were found, so factual claims are marked [source needed]".into());
        }
        Ok(Research { notes, sources, warnings })
    }
}