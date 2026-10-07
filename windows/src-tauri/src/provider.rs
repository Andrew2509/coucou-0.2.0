// AI provider abstraction.
//
// Coucou talks to any Anthropic-format `/v1/messages` endpoint today (AICODING,
// Anthropic). This module is the seam where the other providers plug in: an
// enum, a resolver, and connection/model probes that work per provider. Chat
// itself remains in `claude.rs` (AICODING/Anthropic) and is OpenAI-compatible
// through `send_openai_stream` for the OpenAI / Google / Ollama slots.

use std::time::Duration;

use serde_json::Value;

use crate::claude;

/// Provider ids stored in Settings. Stable contract values — never rename once
/// released.
#[allow(dead_code)]
pub const PROVIDERS: &[&str] = &[
    "aicoding",
    "anthropic",
    "openai",
    "google",
    "ollama",
];

pub const DEFAULT_PROVIDER: &str = "aicoding";

/// Whether the provider talks Anthropic's `/v1/messages` (AICODING does, in
/// the same format Anthropic does) or OpenAI-compatible `/v1/chat/completions`.
pub fn is_anthropic_format(provider: &str) -> bool {
    matches!(provider, "aicoding" | "anthropic")
}

/// Checks the key against the provider's model list. Every provider on this
/// list answers a `GET /v1/models` (Ollama: `/v1/models` also exists on its
/// OpenAI-compatible port), and AICODING/Anthropic already implement
/// `fetch_models`. A failed probe surfaces the endpoint's own message, which
/// is what tells a wrong key or URL apart.
pub async fn test_connection(provider: &str, base: &str, key: &str) -> Result<(), String> {
    list_models(provider, base, key).await.map(|_| ())
}

pub async fn list_models(provider: &str, base: &str, key: &str) -> Result<Vec<String>, String> {
    if is_anthropic_format(provider) {
        return claude::fetch_models(base, key).await;
    }

    // OpenAI-compatible (openai, google, ollama) model list.
    let url = format!("{}/v1/models?limit=100", claude::normalise_base(base));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .get(&url)
        .bearer_auth(key)
        .send()
        .await
        .map_err(|e| format!("Network error: {e}"))?;
    let status = response.status();
    let text = response.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("{provider} {status}: {}", claude::first_error_line(&text)));
    }
    let json: Value = serde_json::from_str(&text).map_err(|e| format!("Bad API response: {e}"))?;
    let items = json
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{provider} returned no model list."))?;
    let ids = items
        .iter()
        .filter_map(|item| item.get("id").and_then(Value::as_str).map(str::to_string))
        .collect::<Vec<_>>();
    if ids.is_empty() {
        return Err(format!("{provider} returned no models."));
    }
    Ok(ids)
}

/// Provider display name for settings.
pub fn label(provider: &str) -> &'static str {
    match provider {
        "aicoding" => "AICODING",
        "anthropic" => "Anthropic",
        "openai" => "OpenAI",
        "google" => "Google",
        "ollama" => "Ollama",
        _ => "AICODING",
    }
}