//! "Word" AI: free online models over HTTPS, with fallback between them. Desktop only.
//! Keys come from the environment (GEMINI_API_KEY, GROQ_API_KEY); nothing is stored.

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Gemini,
    Groq,
}

pub struct Candidate {
    pub provider: Provider,
    pub model: &'static str,
    pub label: &'static str,
}

/// Tried in this order unless the user pins one.
pub const CANDIDATES: &[Candidate] = &[
    Candidate { provider: Provider::Gemini, model: "gemini-2.5-flash", label: "Gemini 2.5 Flash" },
    Candidate { provider: Provider::Groq, model: "openai/gpt-oss-120b", label: "Groq GPT-OSS 120B" },
    Candidate { provider: Provider::Groq, model: "llama-3.3-70b-versatile", label: "Groq Llama 3.3 70B" },
    Candidate { provider: Provider::Gemini, model: "gemini-2.5-flash-lite", label: "Gemini 2.5 Flash-Lite" },
];

pub struct Reply {
    pub text: String,
    pub label: String,
}

pub(crate) fn key(p: Provider) -> Option<String> {
    let name = match p {
        Provider::Gemini => "GEMINI_API_KEY",
        Provider::Groq => "GROQ_API_KEY",
    };
    std::env::var(name).ok().map(|k| k.trim().to_string()).filter(|k| !k.is_empty())
}

/// Whether at least one provider has a key.
pub fn any_key() -> bool {
    key(Provider::Gemini).is_some() || key(Provider::Groq).is_some()
}

#[cfg(target_arch = "wasm32")]
pub fn ask(_system: &str, _user: &str, _only: Option<usize>, _accept: impl Fn(&str) -> bool) -> Result<Reply, String> {
    Err("Word AI works in the desktop app".into())
}

/// Ask the models in turn until one gives a reply `accept` likes.
#[cfg(not(target_arch = "wasm32"))]
pub fn ask(system: &str, user: &str, only: Option<usize>, accept: impl Fn(&str) -> bool) -> Result<Reply, String> {
    let list: Vec<&Candidate> = match only {
        Some(i) => CANDIDATES.get(i).into_iter().collect(),
        None => CANDIDATES.iter().collect(),
    };
    let mut errors = Vec::new();
    for c in list {
        let Some(k) = key(c.provider) else {
            errors.push(format!("{}: no API key set", c.label));
            continue;
        };
        match call(c, &k, system, user) {
            Ok(text) if accept(&text) => return Ok(Reply { text, label: c.label.to_string() }),
            Ok(_) => errors.push(format!("{}: the reply was not a usable document", c.label)),
            Err(e) => errors.push(format!("{}: {e}", c.label)),
        }
    }
    Err(errors.join("\n"))
}

#[cfg(not(target_arch = "wasm32"))]
fn call(c: &Candidate, key: &str, system: &str, user: &str) -> Result<String, String> {
    use serde_json::{Value, json};

    let (url, header, body) = match c.provider {
        Provider::Gemini => (
            format!("https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent", c.model),
            ("x-goog-api-key", key.to_string()),
            json!({
                "systemInstruction": {"parts": [{"text": system}]},
                "contents": [{"role": "user", "parts": [{"text": user}]}],
                "generationConfig": {"responseMimeType": "application/json", "temperature": 0.4}
            }),
        ),
        Provider::Groq => {
            let mut body = json!({
                "model": c.model,
                "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}],
                "response_format": {"type": "json_object"},
                "temperature": 0.4,
                "max_completion_tokens": 6000
            });
            if c.model.starts_with("openai/gpt-oss")
                && let Some(m) = body.as_object_mut()
            {
                m.insert("reasoning_effort".into(), json!("low"));
            }
            ("https://api.groq.com/openai/v1/chat/completions".to_string(), ("authorization", format!("Bearer {key}")), body)
        }
    };

    let config = ureq::Agent::config_builder().timeout_global(Some(std::time::Duration::from_secs(90))).http_status_as_error(false).build();
    let agent = ureq::Agent::new_with_config(config);
    let mut resp = agent.post(&url).header(header.0, header.1.as_str()).send_json(&body).map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().map_err(|e| e.to_string())?;
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}: {}", text.chars().take(300).collect::<String>()));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let out = match c.provider {
        Provider::Gemini => v
            .pointer("/candidates/0/content/parts")
            .and_then(Value::as_array)
            .map(|parts| parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<String>())
            .unwrap_or_default(),
        Provider::Groq => v.pointer("/choices/0/message/content").and_then(Value::as_str).unwrap_or("").to_string(),
    };
    if out.trim().is_empty() {
        return Err("empty reply".into());
    }
    Ok(out)
}
