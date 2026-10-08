// Claude API client — the same integration as ClaudeService.swift: multi-turn
// chat with web search, and files sent as document/image/text blocks. This
// fork adds the agentic loop on top: local tools (read, write, run) approved
// card by card on the island before they run.
//
// Everything happens here rather than in the island: the API key never leaves
// the credential store, and file bytes never cross the IPC boundary.
//
// COUCOU_ANTHROPIC_BASE_URL points the chat at an Anthropic-compatible gateway
// instead (issue #180). Only that variable is read: Claude Code's own
// ANTHROPIC_BASE_URL may name a proxy the user never meant to hand this key to.

use std::sync::OnceLock;
use std::time::Duration;

use reqwest::Url;
use serde_json::{json, Value};
use tauri::AppHandle;

use crate::chat::{self, Chat, ChatContext, ChatReply, ModelInfo};
use crate::i18n::{t, tf};
use crate::{chat_tools, net, secrets};

/// Credential store entry of the Anthropic API key.
pub const KEY: &str = "anthropic-api-key";
/// Credential store entry of the AICODING key — this fork's default brain.
pub const AICODING_KEY: &str = "aicoding-api-key";
/// AICODING's Anthropic-compatible gateway (short model ids: sonnet-5, …).
const AICODING_ENDPOINT: &str = "https://partner.api-github.com/v1/messages";
pub const AICODING_DEFAULT_MODEL: &str = "sonnet-5";
const DEFAULT_ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const BASE_URL_VAR: &str = "COUCOU_ANTHROPIC_BASE_URL";
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Server-side fallback: on a policy decline the API retries the same request on
/// a fallback model inside the same call, so the island never shows a dead end.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const MAX_TOKENS: u32 = 4096;
/// Text and code files are inlined; anything larger is skipped, as on macOS.
const MAX_INLINE_TEXT: u64 = 200_000;

pub const DEFAULT_MODEL: &str = "claude-opus-5";

/// The Messages endpoint: Anthropic's, or the gateway in COUCOU_ANTHROPIC_BASE_URL.
/// Read once; the gateway's host (never the key) goes to the log once.
fn endpoint() -> Result<Url, String> {
    static ENDPOINT: OnceLock<Result<Url, String>> = OnceLock::new();
    ENDPOINT
        .get_or_init(|| {
            let raw = std::env::var(BASE_URL_VAR).ok().filter(|v| !v.trim().is_empty());
            let resolved = match raw {
                None => Ok(Url::parse(DEFAULT_ENDPOINT).expect("valid default endpoint")),
                Some(raw) => net::anthropic_endpoint(&raw),
            };
            match &resolved {
                Ok(url) if url.as_str() != DEFAULT_ENDPOINT => crate::log::line(format!(
                    "chat: Claude requests go to {} ({BASE_URL_VAR})",
                    net::host_for_log(url)
                )),
                Err(err) => crate::log::line(format!("chat: {err}")),
                _ => {}
            }
            resolved
        })
        .clone()
}

/// Which Anthropic-wire gateway answers a turn: Anthropic itself (or the
/// gateway in COUCOU_ANTHROPIC_BASE_URL), or AICODING's compatible endpoint.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wire {
    Anthropic,
    Aicoding,
}

impl Wire {
    /// The Messages endpoint for this gateway.
    fn endpoint(self) -> Result<Url, String> {
        match self {
            Wire::Anthropic => endpoint(),
            Wire::Aicoding => Ok(Url::parse(AICODING_ENDPOINT).expect("valid AICODING endpoint")),
        }
    }

    /// Its credential store entry.
    fn key(self) -> &'static str {
        match self {
            Wire::Anthropic => KEY,
            Wire::Aicoding => AICODING_KEY,
        }
    }

    /// The conversation owner tag: the wire format is Anthropic's either way,
    /// but the provider (and the model behind it) may differ, so a switch
    /// rebuilds the history from the plain turns.
    pub fn owner(self) -> &'static str {
        match self {
            Wire::Anthropic => chat::ANTHROPIC,
            Wire::Aicoding => chat::AICODING,
        }
    }
}

/// The model list endpoint next to the Messages one.
fn models_endpoint(messages: &Url) -> Url {
    let mut url = messages.clone();
    let path = url.path().trim_end_matches("/messages").to_string();
    url.set_path(&format!("{path}/models"));
    url.set_query(Some("limit=100"));
    url
}

/// The user's content blocks for one turn. File / window context rides along
/// with the first message only, exactly like ClaudeService.chat().
fn user_content(first: bool, context: Option<&ChatContext>, query: &str) -> Vec<Value> {
    let mut content: Vec<Value> = Vec::new();
    if first {
        match context {
            Some(ChatContext::File { name, path }) => {
                if let Some(block) = file_block(path) {
                    content.push(block);
                }
                content.push(json!({ "type": "text", "text": format!("File: {name}") }));
            }
            Some(ChatContext::Window { app_name, title, url }) => {
                let line = chat::window_line(app_name, title, url.as_deref());
                content.push(json!({ "type": "text", "text": line }));
            }
            None => {}
        }
    }
    content.push(json!({ "type": "text", "text": query }));
    content
}

/// What this wire offers the model: this fork's local tools everywhere, and
/// Anthropic's server-side web search on top only where it runs — AICODING's
/// gateway rejects it, so its wire never asks for it.
fn tools_for(wire: Wire) -> Value {
    let mut tools = chat_tools::defs();
    if wire == Wire::Anthropic {
        if let Some(list) = tools.as_array_mut() {
            list.insert(
                0,
                json!({ "type": "web_search_20260209", "name": "web_search", "max_uses": 5 }),
            );
        }
    }
    tools
}

/// The request body: everything the model saw this round — the history, the
/// in-progress messages of this turn, and the tools of this wire. Only
/// Anthropic itself carries the server-side fallback beta field.
fn request_body_full(wire: Wire, model: &str, system: &str, messages: &[Value], tools: &Value) -> Value {
    let mut body = json!({
        "model": model,
        "max_tokens": MAX_TOKENS,
        "system": system,
        "tools": tools,
        "messages": messages,
    });
    if wire == Wire::Anthropic {
        body["fallbacks"] = json!("default");
    }
    body
}

/// The assistant's full text from a Messages API content array — the same as
/// claudeResponseText() on the Mac (#67). Web search answers interleave text
/// with tool blocks, and citations split a sentence across adjacent text
/// blocks: every text block is kept, joined as is, and only the whole trimmed.
pub fn response_text(content: &[Value]) -> Option<String> {
    let text: String = content
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect();
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// The content blocks to keep in the history and the text to show, or why there are none.
/// A response that only asks for a tool call carries no text yet: the loop
/// continues, and the answer comes in a later round.
fn interpret(response: &Value) -> Result<(Vec<Value>, String), String> {
    // A policy decline comes back as HTTP 200 with stop_reason "refusal".
    if response.get("stop_reason").and_then(Value::as_str) == Some("refusal") {
        let why = response
            .pointer("/stop_details/explanation")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| t("Claude declined this one."));
        return Err(why);
    }
    let blocks = response
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| t("Unexpected API response."))?;
    if let Some(text) = response_text(&blocks) {
        return Ok((blocks, text));
    }
    let wants_tool = blocks.iter().any(|b| b.get("type") == Some(&json!("tool_use")));
    if wants_tool {
        return Ok((blocks, String::new()));
    }
    Err(t("No response text."))
}

/// One chat turn. Returns the assistant's text, or a message the island shows
/// in the note view.
pub async fn send(
    app: &AppHandle,
    chat: &Chat,
    model: &str,
    query: String,
    context: Option<ChatContext>,
) -> Result<ChatReply, String> {
    send_with(app, Wire::Anthropic, chat, model, query, context).await
}

/// The line appended to the system prompt whenever local tools are offered.
const TOOLS_NOTE: &str = " Local tools are available: read_file, write_file, list_dir, run_powershell, run_python, and Spotify control (spotify_now, spotify_search, spotify_play, spotify_pause, spotify_next, spotify_previous, spotify_queue — the Spotify tools need the connector set up in Settings). Every call asks the user for permission first; when a call is denied, answer without it instead of asking again.";

/// One chat turn on a given Anthropic-wire gateway (Anthropic or AICODING),
/// with this fork's agentic loop: the model may call local tools, each call
/// crosses the island's approval card before anything runs, and the loop
/// ends on the first response that carries no tool call — or on
/// `chat_tools::MAX_ROUNDS`, so one question can never run forever.
pub async fn send_with(
    app: &AppHandle,
    wire: Wire,
    chat: &Chat,
    model: &str,
    query: String,
    context: Option<ChatContext>,
) -> Result<ChatReply, String> {
    let key = secrets::get(wire.key()).ok_or_else(|| t("API key missing. Open settings."))?;
    let endpoint = wire.endpoint()?;

    let turn = chat.begin(wire.owner());
    let user = json!({ "role": "user", "content": user_content(turn.first, context.as_ref(), &query) });
    let plain = chat::plain_question(turn.first, context.as_ref(), &query);
    let mut system = chat::system_prompt(wire == Wire::Anthropic);
    system.push_str(TOOLS_NOTE);
    let tools = tools_for(wire);

    let mut messages = vec![user];
    let mut used_tools = false;
    let mut last_text = String::new();
    let mut final_text: Option<String> = None;
    let mut failure: Option<String> = None;

    // One iteration per API round-trip; the +1 lets MAX_ROUNDS tool rounds
    // still be followed by the final answer.
    for _ in 0..=chat_tools::MAX_ROUNDS {
        let mut all = turn.history.clone();
        all.extend(messages.iter().cloned());
        let body = request_body_full(wire, model, &system, &all, &tools);
        let round = match call(wire, &endpoint, &key, &body).await {
            Ok(response) => interpret(&response),
            Err(e) => Err(e),
        };
        let (blocks, text) = match round {
            Ok(v) => v,
            Err(e) => {
                failure = Some(e);
                break;
            }
        };
        if !text.is_empty() {
            last_text = text.clone();
        }
        messages.push(json!({ "role": "assistant", "content": blocks.clone() }));

        let tool_uses: Vec<Value> = blocks
            .iter()
            .filter(|b| b.get("type") == Some(&json!("tool_use")))
            .cloned()
            .collect();
        if tool_uses.is_empty() {
            final_text = Some(text);
            break;
        }

        let mut results = Vec::new();
        for call in tool_uses {
            let id = call.get("id").cloned().unwrap_or(json!(""));
            let name = call.get("name").and_then(Value::as_str).unwrap_or("");
            let input = call.get("input").cloned().unwrap_or_else(|| json!({}));
            used_tools = true;
            chat_tools::step(app, "PreToolUse", name, &input);
            let display = chat_tools::display_input(name, &input);
            let allowed = chat_tools::approve(app, name, &display).await;
            let (content, is_error) = if allowed {
                match chat_tools::execute(name, &input).await {
                    Ok(out) => {
                        chat_tools::step(app, "PostToolUse", name, &input);
                        (out, false)
                    }
                    Err(e) => {
                        chat_tools::step(app, "PostToolUseFailure", name, &input);
                        (e, true)
                    }
                }
            } else {
                chat_tools::step(app, "PostToolUseFailure", name, &input);
                (format!("The user denied this call of {name}."), true)
            };
            results.push(json!({ "type": "tool_result", "tool_use_id": id, "content": content, "is_error": is_error }));
        }
        messages.push(json!({ "role": "user", "content": results }));
    }

    // The answer: the round that closed the loop, else the model's last words
    // before the round limit — worth keeping when there are any.
    let text = match failure.take() {
        Some(e) => {
            if used_tools {
                chat_tools::finish(app, "", false);
            }
            return Err(e);
        }
        None => final_text.unwrap_or(last_text),
    };
    if text.is_empty() {
        if used_tools {
            chat_tools::finish(app, "", false);
        }
        return Err(tf(
            "Stopped after {max} tool rounds without a final answer.", &[("max", &chat_tools::MAX_ROUNDS.to_string())],
        ));
    }

    chat.commit_messages(&turn, &messages, &plain, &text);
    if used_tools {
        chat_tools::finish(app, &text, true);
    }
    Ok(ChatReply { text })
}

async fn call(wire: Wire, endpoint: &Url, key: &str, body: &Value) -> Result<Value, String> {
    let request = net::client(endpoint, Duration::from_secs(90))?
        .post(endpoint.clone())
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header("content-type", "application/json");
    // Anthropic takes its key as x-api-key; AICODING's gateway takes the
    // Bearer form (its Anthropic endpoint is one of several behind the same
    // token), so each wire sends the auth it was verified with.
    let request = match wire {
        Wire::Anthropic => request
            .header("x-api-key", key)
            .header("anthropic-beta", FALLBACK_BETA),
        Wire::Aicoding => request.header("authorization", format!("Bearer {key}")),
    };
    let response = request.json(body).send().await
        .map_err(|e| tf("Network error: {error}", &[("error", &e.to_string())]))?;

    let status = response.status();
    if !status.is_success() {
        // Surface the API's own message, which is what makes a bad key obvious.
        let body = net::read_capped(response, net::MAX_ERROR_BODY).await.unwrap_or_default();
        return Err(format!("Claude API {status}: {}", net::error_detail(&body)));
    }
    let bytes = net::read_capped(response, net::MAX_BODY).await?;
    serde_json::from_slice(&bytes).map_err(|e| tf("Bad API response: {error}", &[("error", &e.to_string())]))
}

/// The models on the user's Anthropic account, newest first, as the API lists them.
pub async fn models(key: &str) -> Result<Vec<ModelInfo>, String> {
    models_with(Wire::Anthropic, key).await
}

/// The model list of a given Anthropic-wire gateway, next to its Messages endpoint.
pub async fn models_with(wire: Wire, key: &str) -> Result<Vec<ModelInfo>, String> {
    let url = models_endpoint(&wire.endpoint()?);
    let request = net::client(&url, Duration::from_secs(10))?
        .get(url.clone())
        .header("anthropic-version", ANTHROPIC_VERSION);
    let request = match wire {
        Wire::Anthropic => request.header("x-api-key", key),
        Wire::Aicoding => request.header("authorization", format!("Bearer {key}")),
    };
    let response = request.send().await
        .map_err(|e| tf("Network error: {error}", &[("error", &e.to_string())]))?;
    let status = response.status();
    if !status.is_success() {
        let body = net::read_capped(response, net::MAX_ERROR_BODY).await.unwrap_or_default();
        return Err(format!("Claude API {status}: {}", net::error_detail(&body)));
    }
    let bytes = net::read_capped(response, net::MAX_BODY).await?;
    let json: Value = serde_json::from_slice(&bytes).map_err(|_| t("Unexpected API response."))?;
    Ok(parse_models(&json))
}

fn parse_models(json: &Value) -> Vec<ModelInfo> {
    json.get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let id = m.get("id")?.as_str()?.to_string();
            let label = m.get("display_name").and_then(Value::as_str).unwrap_or(&id).to_string();
            Some(ModelInfo { id, label })
        })
        .collect()
}

/// PDF → document block, image → image block, text/code → inline text.
/// Mirrors readFileAsBlock() in ClaudeService.swift.
fn file_block(path: &str) -> Option<Value> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let media_type = match ext.as_str() {
        "pdf" => Some(("document", "application/pdf")),
        "jpg" | "jpeg" => Some(("image", "image/jpeg")),
        "png" => Some(("image", "image/png")),
        "gif" => Some(("image", "image/gif")),
        "webp" => Some(("image", "image/webp")),
        _ => None,
    };

    if let Some((block_type, media)) = media_type {
        let bytes = std::fs::read(path).ok()?;
        return Some(json!({
            "type": block_type,
            "source": { "type": "base64", "media_type": media, "data": base64(&bytes) },
        }));
    }

    let len = std::fs::metadata(path).ok()?.len();
    if len > MAX_INLINE_TEXT {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    Some(json!({ "type": "text", "text": format!("File contents:\n{text}") }))
}

/// Small standalone base64 encoder — not worth another dependency.
/// Also used for Stripe's basic auth and the images sent to other providers.
pub(crate) fn base64_for(bytes: &[u8]) -> String {
    base64(bytes)
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

/// URL percent-encode (application/x-www-form-urlencoded style: spaces → `+`).
/// Shared with the Spotify connector for search terms and queue URIs.
pub(crate) fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    fn texts(parts: &[&str]) -> Vec<Value> {
        parts.iter().map(|t| json!({ "type": "text", "text": t })).collect()
    }

    #[test]
    fn every_text_block_is_kept_in_order_without_added_separators() {
        assert_eq!(response_text(&texts(&["Simple answer."])).as_deref(), Some("Simple answer."));
        let content = vec![
            json!({"type":"text","text":"I'll look that up.\n"}),
            json!({"type":"server_tool_use","id":"srvtoolu_01","name":"web_search","input":{"query":"test"}}),
            json!({"type":"web_search_tool_result","tool_use_id":"srvtoolu_01","content":[]}),
            json!({"type":"text","text":"The answer is 42."}),
        ];
        assert_eq!(response_text(&content).as_deref(), Some("I'll look that up.\nThe answer is 42."));
        // An empty leading block does not hide the answer.
        let content = vec![
            json!({"type":"text","text":""}),
            json!({"type":"web_search_tool_result","tool_use_id":"b","content":[]}),
            json!({"type":"text","text":"Here is the actual answer."}),
        ];
        assert_eq!(response_text(&content).as_deref(), Some("Here is the actual answer."));
        // Citations split a sentence across blocks: no newline is inserted.
        assert_eq!(
            response_text(&texts(&["Paris is the ", "capital", " of France."])).as_deref(),
            Some("Paris is the capital of France.")
        );
        // Search result text never leaks into the answer.
        let content = vec![
            json!({"type":"text","text":"Answer."}),
            json!({"type":"web_search_tool_result","tool_use_id":"a","content":[{"type":"web_search_result","title":"Page","text":"Leak"}]}),
        ];
        assert_eq!(response_text(&content).as_deref(), Some("Answer."));
        assert_eq!(response_text(&[json!({"type":"server_tool_use","id":"x"})]), None);
        assert_eq!(response_text(&texts(&["", "  \n"])), None);
    }

    #[test]
    fn a_refusal_or_an_empty_answer_is_an_error() {
        let refusal = json!({"stop_reason":"refusal","stop_details":{"explanation":"Not this."},"content":[]});
        assert_eq!(interpret(&refusal).unwrap_err(), "Not this.");
        assert_eq!(interpret(&json!({"stop_reason":"refusal"})).unwrap_err(), "Claude declined this one.");
        assert_eq!(interpret(&json!({"id":"x"})).unwrap_err(), "Unexpected API response.");
        assert_eq!(interpret(&json!({"content":[]})).unwrap_err(), "No response text.");
        let (blocks, text) = interpret(&json!({"content":[{"type":"text","text":" Hi "}]})).unwrap();
        assert_eq!(text, "Hi");
        assert_eq!(blocks.len(), 1);
    }

    #[test]
    fn the_request_carries_history_web_search_and_the_new_turn_last() {
        let mut all = vec![json!({"role":"user","content":"a"}), json!({"role":"assistant","content":"b"})];
        let user = json!({"role":"user","content":[{"type":"text","text":"c"}]});
        all.push(user.clone());
        let tools = tools_for(Wire::Anthropic);
        let body = request_body_full(Wire::Anthropic, "claude-x", "sys", &all, &tools);
        assert_eq!(body["model"], "claude-x");
        assert_eq!(body["system"], "sys");
        assert_eq!(body["max_tokens"], MAX_TOKENS);
        assert_eq!(body["tools"][0]["name"], "web_search");
        assert_eq!(body["fallbacks"], "default");
        assert_eq!(body["messages"].as_array().unwrap().len(), 3);
        assert_eq!(body["messages"][2], user);
    }

    #[test]
    fn the_aicoding_wire_offers_the_local_tools_without_web_search_or_fallbacks() {
        let tools = tools_for(Wire::Aicoding);
        let names: Vec<&str> = tools
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"run_powershell"));
        assert!(!names.contains(&"web_search"));
        let body = request_body_full(
            Wire::Aicoding,
            "sonnet-5",
            "sys",
            &[json!({"role":"user","content":"q"})],
            &tools,
        );
        assert!(body.get("fallbacks").is_none());
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_tool_call_without_text_still_carries_on() {
        let tool_only = json!({"content":[{"type":"tool_use","id":"tu_1","name":"read_file","input":{"path":"x"}}]});
        let (blocks, text) = interpret(&tool_only).unwrap();
        assert_eq!(text, "");
        assert_eq!(blocks.len(), 1);
        // Empty content with no tool call is still the old error.
        assert_eq!(interpret(&json!({"content":[]})).unwrap_err(), "No response text.");
    }

    #[test]
    fn context_is_sent_with_the_first_turn_only() {
        let ctx = ChatContext::Window { app_name: "Edge".into(), title: "Docs".into(), url: Some("https://x.dev".into()) };
        let first = user_content(true, Some(&ctx), "q");
        assert_eq!(first.len(), 2);
        assert_eq!(first[0]["text"], "Context — App: Edge, Window: Docs, URL: https://x.dev");
        assert_eq!(user_content(false, Some(&ctx), "q"), vec![json!({"type":"text","text":"q"})]);
        // A file that cannot be read still names itself.
        let ctx = ChatContext::File { name: "gone.pdf".into(), path: "/no/such/gone.pdf".into() };
        assert_eq!(user_content(true, Some(&ctx), "q")[0]["text"], "File: gone.pdf");
    }

    #[test]
    fn the_model_list_sits_next_to_the_messages_endpoint() {
        let url = Url::parse(DEFAULT_ENDPOINT).unwrap();
        assert_eq!(models_endpoint(&url).as_str(), "https://api.anthropic.com/v1/models?limit=100");
        let url = net::anthropic_endpoint("https://gw.example.com/anthropic").unwrap();
        assert_eq!(models_endpoint(&url).as_str(), "https://gw.example.com/anthropic/v1/models?limit=100");
        let list = json!({"data":[{"id":"claude-opus-5","display_name":"Claude Opus 5"},{"id":"claude-x"},{"nope":1}]});
        assert_eq!(
            parse_models(&list),
            vec![
                ModelInfo { id: "claude-opus-5".into(), label: "Claude Opus 5".into() },
                ModelInfo { id: "claude-x".into(), label: "claude-x".into() },
            ]
        );
    }
}
