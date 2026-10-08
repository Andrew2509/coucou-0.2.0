// Spotify Connector — OAuth 2.0 with PKCE (plain method) plus the Web API calls
// the chat tools make. AICODING stays the brain; this module is pure tool layer.
//
// Security model:
//   * The user pastes their Spotify Client ID (from the Spotify Developer
//     Dashboard) in Settings; it is stored in the Credential Manager, not disk.
//   * The access & refresh tokens also live in the Credential Manager.
//   * A one-shot loopback server on 127.0.0.1:8000 receives the redirect; no
//     secret ever leaves the machine, and `state` guards the callback.
//   * Playback control (play/pause next/previous) needs Spotify Premium, which
//     the API itself reports; we just surface its error.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{platform, secrets};

const REDIRECT_PORT: u16 = 8000;
/// Loopback IP literal over plain HTTP. Spotify's dashboard refuses http for
/// a fresh app, but this port already works for this account; the port stays
/// fixed because the registered redirect URI is an exact match.
const REDIRECT_URI: &str = "http://127.0.0.1:8000/callback";
const AUTH_URL: &str = "https://accounts.spotify.com/authorize";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const API_URL: &str = "https://api.spotify.com/v1";

/// OAuth scopes, space-separated. `user-modify-playback-state` covers play /
/// pause / next / previous / queue and needs Premium, which Spotify enforces.
const SCOPES: &str = "user-read-playback-state user-modify-playback-state user-read-currently-playing";

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// 43 random bytes (base64url of 32 bytes keeps it inside the 43–128 char
/// verifier range) → a valid PKCE verifier, [A-Za-z0-9-._~]. Spotify requires
/// `code_challenge_method=S256`, so the challenge is base64url(sha256(verifier)).
fn generate_verifier() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("OS CSPRNG");
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// `challenge = base64url(sha256(verifier))`, with no padding.
fn s256_challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(verifier.as_bytes());
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// 16 random bytes as hex → the `state` nonce.
fn generate_state() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("OS CSPRNG");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A tiny one-shot HTTP server on 127.0.0.1:8000. The registered redirect URI
/// is the plain-HTTP loopback `http://127.0.0.1:8000/callback` (this account
/// accepts it), so no TLS is needed. The port stays fixed because the
/// dashboard stores the exact URI. Times out if the tab hangs.
async fn listen_for_code(expected_state: &str, cancel: Arc<AtomicBool>) -> Result<String, String> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", REDIRECT_PORT))
        .await
        .map_err(|e| format!(
            "could not start the Spotify callback on port {REDIRECT_PORT} ({e}). \
             Close whatever uses port {REDIRECT_PORT}, then Connect again."
        ))?;

    let accepted = tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err("Cancelled.".to_string());
            }
            let (mut socket, _) = match listener.accept().await {
                Ok(v) => v,
                Err(err) => return Err(format!("callback socket error: {err}")),
            };
            let mut buf = [0u8; 8192];
            let n = socket.read(&mut buf).await.map_err(|e| e.to_string())?;
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let response = if cancel.load(Ordering::Relaxed) {
                Err("Cancelled.".to_string())
            } else {
                let query = req
                    .split_whitespace()
                    .nth(1)
                    .and_then(|p| p.split('?').nth(1))
                    .unwrap_or("");
                handle_callback(query, expected_state)
            };
            match response {
                Ok(code) => {
                    let _ = socket.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: 74\r\nConnection: close\r\n\r\n<html><body>Connected. You can close this tab.</body></html>",
                    ).await;
                    return Ok(code);
                }
                Err(err) => {
                    let body = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n<html><body>{err}</body></html>");
                    let _ = socket.write_all(body.as_bytes()).await;
                    return Err(err);
                }
            }
        }
    })
    .await
    .map_err(|_| "The Spotify login took too long or was never completed.".to_string())??;
    Ok(accepted)
}

fn handle_callback(query: &str, expected_state: &str) -> Result<String, String> {
    let params = query_params(query);
    let state = params.get("state").cloned().unwrap_or_default();
    if state != expected_state {
        return Err("Sign-in was cancelled (state mismatch). Try again.".to_string());
    }
    let code = params
        .get("code")
        .cloned()
        .ok_or_else(|| "Spotify did not return a code.".to_string())?;
    Ok(code)
}

fn query_params(query: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        if let (Some(k), Some(v)) = (parts.next(), parts.next()) {
            out.insert(k.to_string(), percent_decode(v).unwrap_or_default());
        }
    }
    out
}

fn percent_decode(s: &str) -> Result<String, String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).map_err(|_| "bad hex")?;
            let val = u8::from_str_radix(hex, 16).map_err(|_| "bad hex")?;
            out.push(val);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| "invalid utf-8".to_string())
}

#[derive(serde::Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
}

async fn exchange_token(client_id: &str, code: &str, verifier: &str) -> Result<TokenResponse, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .post(TOKEN_URL)
        .header("content-type", "application/x-www-form-urlencoded")
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", REDIRECT_URI),
            ("client_id", client_id),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .map_err(|e| format!("Network error: {e}"))?;
    let status = response.status();
    let text = response.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("Spotify token error {status}: {text}"));
    }
    serde_json::from_str(&text).map_err(|e| format!("Bad response: {e}"))
}

async fn refresh_token(client_id: &str, refresh: &str) -> Result<TokenResponse, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .post(TOKEN_URL)
        .header("content-type", "application/x-www-form-urlencoded")
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", client_id),
        ])
        .send()
        .await
        .map_err(|e| format!("Network error: {e}"))?;
    let status = response.status();
    let text = response.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("Spotify refresh error {status}: {text}"));
    }
    serde_json::from_str(&text).map_err(|e| format!("Bad response: {e}"))
}

/// Returns a valid access token, refreshing when close to expiry.
pub async fn access_token() -> Result<String, String> {
    let client_id = secrets::get("spotify-client-id").ok_or("No Spotify Client ID stored.")?;
    let token = secrets::get("spotify-access-token").ok_or("Not connected to Spotify.")?;
    let expiry: i64 = secrets::get("spotify-token-expiry")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if expiry - now_unix() > 60 {
        return Ok(token);
    }
    let refresh = secrets::get("spotify-refresh-token").ok_or("Session cannot be refreshed.")?;
    let next = refresh_token(&client_id, &refresh).await?;
    let _ = secrets::set("spotify-access-token", &next.access_token);
    if let Some(r) = &next.refresh_token {
        let _ = secrets::set("spotify-refresh-token", r);
    }
    let _ = secrets::set("spotify-token-expiry", &(now_unix() + next.expires_in).to_string());
    Ok(next.access_token)
}

/// Generic Web API call. `method` is GET/PUT/POST; empty body means none.
async fn api(
    token: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?;
    let url = format!("{API_URL}{path}");
    let mut request = client
        .request(reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET), &url)
        .bearer_auth(token);
    if let Some(b) = body {
        request = request.json(&b);
    }
    let response = request.send().await.map_err(|e| format!("Network error: {e}"))?;
    let status = response.status();
    let text = response.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        // Surface Spotify's own error message, including the "Premium required"
        // ones, so the chat can explain instead of guessing.
        let why = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.pointer("/error/message").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| text.chars().take(200).collect());
        return Err(format!("Spotify {status}: {why}"));
    }
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&text).map_err(|e| format!("Bad response: {e}"))
}

// ── Public API for the chat tools / settings ──────────────────────────────────

pub fn connected() -> bool {
    secrets::present("spotify-access-token")
}

pub async fn start_auth(app: &tauri::AppHandle, cancel: std::sync::Arc<AtomicBool>) -> Result<(), String> {
    let client_id = secrets::get("spotify-client-id")
        .ok_or_else(|| "No Spotify Client ID stored — paste it in Settings → Connectors → Spotify.".to_string())?;
    let verifier = generate_verifier();
    let state = generate_state();

    let auth_url = format!(
        "{AUTH_URL}?client_id={client_id}&response_type=code&redirect_uri={}&code_challenge_method=S256&code_challenge={}&state={state}&scope={}",
        percent_encode_keep(REDIRECT_URI),
        s256_challenge(&verifier),
        SCOPES.replace(' ', "%20")
    );
    platform::open_url(&auth_url);

    let code = listen_for_code(&state, cancel).await?;
    let token = exchange_token(&client_id, &code, &verifier).await?;
    secrets::set("spotify-access-token", &token.access_token)?;
    if let Some(r) = &token.refresh_token {
        secrets::set("spotify-refresh-token", r)?;
    }
    secrets::set("spotify-token-expiry", &(now_unix() + token.expires_in).to_string())?;
    let _ = app; // reserved for future logging
    Ok(())
}

pub fn disconnect() -> Result<(), String> {
    for key in ["spotify-access-token", "spotify-refresh-token", "spotify-token-expiry"] {
        secrets::clear(key)?;
    }
    Ok(())
}

fn percent_encode_keep(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            ':' => ":".into(),
            '/' => "/".into(),
            ' ' => "%20".into(),
            c => c.to_string(),
        })
        .collect()
}

// ── Tool implementations ──────────────────────────────────────────────────────

/// `spotify_current`: what is playing right now, with position and progress.
pub async fn current_music() -> Result<String, String> {
    let token = access_token().await?;
    let data = api(&token, "GET", "/me/player?additional_types=track", None).await?;
    if data == json!({}) {
        return Ok("Nothing is playing and no player is active.".into());
    }
    let item = data.get("item").cloned().unwrap_or_default();
    let title = item.get("name").and_then(Value::as_str).unwrap_or("Unknown");
    let artists = item
        .get("artists")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.get("name").and_then(Value::as_str)).collect::<Vec<_>>().join(", "))
        .unwrap_or_default();
    let playing = data.get("is_playing").and_then(Value::as_bool).unwrap_or(false);
    let progress = data.get("progress_ms").and_then(Value::as_i64).unwrap_or(0);
    let duration = item.get("duration_ms").and_then(Value::as_i64).unwrap_or(0);
    let secs = |ms: i64| format!("{}:{:02}", ms / 60000, (ms % 60000) / 1000);
    let anything = data.get("device").and_then(|d| d.get("name")).and_then(Value::as_str).unwrap_or("a device");
    Ok(format!(
        "{}\n{title} — {artists}\n>>> ({}s / {}s) on {anything}",
        if playing { "◆ Playing" } else { "■ Paused" },
        secs(progress),
        secs(duration)
    ))
}

/// `spotify_search`: find tracks (or playlists) by a query string.
pub async fn search_tracks(query: &str) -> Result<String, String> {
    let token = access_token().await?;
    let q = crate::claude::percent_encode(query);
    let data = api(&token, "GET", &format!("/search?q={q}&type=track&limit=5"), None).await?;
    let items = data.pointer("/tracks/items").and_then(Value::as_array).cloned().unwrap_or_default();
    if items.is_empty() {
        return Ok(format!("No Spotify tracks found for \"{query}\"."));
    }
    let lines = items.iter().map(|t| {
        let name = t.get("name").and_then(Value::as_str).unwrap_or("?");
        let artists = t.pointer("/artists")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|x| x.get("name").and_then(Value::as_str)).collect::<Vec<_>>().join(", "))
            .unwrap_or_default();
        let uri = t.get("uri").and_then(Value::as_str).unwrap_or("");
        format!("{name} — {artists}\n{uri}")
    }).collect::<Vec<_>>();
    Ok(lines.join("\n"))
}

pub async fn search_playlists(query: &str) -> Result<String, String> {
    let token = access_token().await?;
    let q = crate::claude::percent_encode(query);
    let data = api(&token, "GET", &format!("/search?q={q}&type=playlist&limit=5"), None).await?;
    let items = data.pointer("/playlists/items").and_then(Value::as_array).cloned().unwrap_or_default();
    if items.is_empty() {
        return Ok(format!("No Spotify playlists found for \"{query}\"."));
    }
    let lines = items.iter().map(|p| {
        let name = p.get("name").and_then(Value::as_str).unwrap_or("?");
        let uri = p.get("uri").and_then(Value::as_str).unwrap_or("");
        format!("{name}\n{uri}")
    }).collect::<Vec<_>>();
    Ok(lines.join("\n"))
}

/// `spotify_play`: start playback. `uri` is a track/album/playlist URI
/// (`spotify:track:` or `spotify:playlist:`); an empty `uri` resumes whatever
/// is paused. Device is optional.
pub async fn play(uri: &str) -> Result<String, String> {
    let token = access_token().await?;
    let body = if uri.is_empty() {
        None // resume
    } else if uri.starts_with("spotify:playlist:") || uri.starts_with("spotify:album:") || uri.starts_with("spotify:artist:") {
        Some(json!({ "context_uri": uri }))
    } else {
        Some(json!({ "uris": [uri] }))
    };
    api(&token, "PUT", "/me/player/play", body).await?;
    Ok(if uri.is_empty() {
        "Resumed playback.".into()
    } else {
        format!("Playing {uri}.")
    })
}

pub async fn pause() -> Result<String, String> {
    let token = access_token().await?;
    api(&token, "PUT", "/me/player/pause", None).await?;
    Ok("Paused.".into())
}

pub async fn next_track() -> Result<String, String> {
    let token = access_token().await?;
    api(&token, "POST", "/me/player/next", None).await?;
    Ok("Next track.".into())
}

pub async fn previous_track() -> Result<String, String> {
    let token = access_token().await?;
    api(&token, "POST", "/me/player/previous", None).await?;
    Ok("Previous track.".into())
}

/// `spotify_queue`: read the upcoming queue, or enqueue a URI.
pub async fn queue(uri: Option<String>) -> Result<String, String> {
    let token = access_token().await?;
    if let Some(uri) = uri {
        let q = crate::claude::percent_encode(&uri);
        api(&token, "POST", &format!("/me/player/queue?uri={q}"), None).await?;
        return Ok(format!("Added {uri} to the queue."));
    }
    let data = api(&token, "GET", "/me/player/queue", None).await?;
    let items = data.get("queue").and_then(Value::as_array).cloned().unwrap_or_default();
    if items.is_empty() {
        return Ok("The queue is empty.".into());
    }
    let lines = items.iter().take(5).map(|t| {
        let name = t.get("name").and_then(Value::as_str).unwrap_or("?");
        let artists = t.pointer("/artists")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|x| x.get("name").and_then(Value::as_str)).collect::<Vec<_>>().join(", "))
            .unwrap_or_default();
        format!("{name} — {artists}")
    }).collect::<Vec<_>>();
    Ok(format!("Up next:\n{}", lines.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s256_matches_rfc7636_vector() {
        // https://datatracker.ietf.org/doc/html/rfc7636#appendix-B
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            s256_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn verifier_is_valid_for_spotify() {
        let v = generate_verifier();
        assert!((43..=128).contains(&v.len()));
        assert!(v.chars().all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c)));
    }

    #[test]
    fn query_params_decode_percent() {
        let p = query_params("state=abc123&code=12%2B34%20hi");
        assert_eq!(p.get("state").map(String::as_str), Some("abc123"));
        assert_eq!(p.get("code").map(String::as_str), Some("12+34 hi"));
    }

    #[test]
    fn state_mismatch_is_refused() {
        assert!(handle_callback("code=x&state=bad", "good").is_err());
        assert!(handle_callback("state=good&code=x", "good").is_ok());
        assert!(handle_callback("state=good", "good").is_err());
    }
}