//! Network I/O: Google's unofficial translate endpoint.

use crate::logic::parse_google_translation;

/// Translate `text` to language `to` (Google code) via the gtx endpoint (`sl=auto`).
pub fn translate(text: &str, to: &str) -> Result<String, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(10))
        .build();

    let resp = agent
        .get("https://translate.googleapis.com/translate_a/single")
        .query("client", "gtx")
        .query("sl", "auto")
        .query("tl", to)
        .query("dt", "t")
        .query("q", text)
        .call()
        .map_err(|e| format!("translation request failed: {e}"))?;

    let body = resp
        .into_string()
        .map_err(|e| format!("failed to read translation response: {e}"))?;

    parse_google_translation(&body)
        .ok_or_else(|| "failed to parse translation response".to_string())
}
