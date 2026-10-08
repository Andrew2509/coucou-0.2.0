// Local tools the chat's model can call — this fork's agentic layer.
//
// Every call crosses the island's approval card before anything runs: Rust
// emits the same PermissionRequest hook the island already shows for Claude
// Code, waits for an explicit Allow / Deny click, and only then executes.
// No tool runs without a human answering, and a card nobody answers times
// out as a denial.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

use crate::i18n::{t, tf};
use crate::island::WINDOW_LABEL;

/// How long a card may wait for a human before the call counts as denied.
/// The island drops its own card at 110 s and declines it; this is the
/// backstop behind it, so a dead webview cannot stall a turn forever.
const APPROVAL_WAIT: Duration = Duration::from_secs(120);
/// Tool output back to the model, capped so one command cannot flood a turn.
const MAX_RESULT: usize = 40_000;
/// A file bigger than this is refused by read_file.
const MAX_FILE: u64 = 8 * 1024 * 1024;
/// A write bigger than this is refused.
const MAX_WRITE: usize = 4 * 1024 * 1024;
/// How many tool rounds one question may use before the chat stops.
pub const MAX_ROUNDS: usize = 8;
/// Longest a command may run before it is killed.
const COMMAND_WAIT: Duration = Duration::from_secs(60);

/// Waiting approvals, by request id. Answers arrive from the island through
/// the approval commands in lib.rs.
type Pending = Mutex<HashMap<String, tokio::sync::oneshot::Sender<bool>>>;
static PENDING: OnceLock<Pending> = OnceLock::new();
static NEXT: AtomicU64 = AtomicU64::new(1);

fn pending() -> &'static Pending {
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Where the chat's commands run when the model names no directory: the
/// user's home, a place that always exists and is never surprising.
fn default_cwd() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The tool definitions the model is offered. Names and fields are the ones
/// the island's approval card knows how to show (`command`, `file_path`,
/// `path`, `query`, `uri` — see APPROVAL_FIELDS in the island's hooks).
pub fn defs() -> Value {
    json!([
        {
            "name": "read_file",
            "description": "Read a UTF-8 text file from this computer. Returns numbered lines (1-based). Use offset/limit for a window of a large file.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Absolute path of the file to read." },
                    "offset": { "type": "integer", "description": "First line to read (1-based). Default 1." },
                    "limit": { "type": "integer", "description": "How many lines to read. Default 2000, max 8000." }
                },
                "required": ["path"]
            }
        },
        {
            "name": "write_file",
            "description": "Create or fully overwrite a UTF-8 text file on this computer. Parent directories are created when missing.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "Absolute path of the file to write." },
                    "content": { "type": "string", "description": "The whole new content of the file." }
                },
                "required": ["file_path", "content"]
            }
        },
        {
            "name": "list_dir",
            "description": "List a directory on this computer: subdirectories first, then files, with sizes.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Absolute path of the directory." }
                },
                "required": ["path"]
            }
        },
        {
            "name": "run_powershell",
            "description": "Run a PowerShell command on this computer and return its output. No interactive prompts: -NoProfile -NonInteractive. Times out after 60 seconds.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The PowerShell command to run." },
                    "cwd": { "type": "string", "description": "Working directory. Defaults to the user's home." }
                },
                "required": ["command"]
            }
        },
        {
            "name": "run_python",
            "description": "Run a Python script on this computer and return stdout and stderr. The script must not wait for input. Times out after 60 seconds.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "code": { "type": "string", "description": "The whole script to run." },
                    "cwd": { "type": "string", "description": "Working directory. Defaults to the user's home." }
                },
                "required": ["code"]
            }
        },
        {
            "name": "spotify_now",
            "description": "What is playing on Spotify right now: track, artists, progress and device.",
            "input_schema": { "type": "object", "properties": {} }
        },
        {
            "name": "spotify_search",
            "description": "Search Spotify for tracks or playlists. Returns names and Spotify URIs to hand to spotify_play.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "What to look for." },
                    "type": { "type": "string", "description": "Either \"track\" (the default) or \"playlist\"." }
                },
                "required": ["query"]
            }
        },
        {
            "name": "spotify_play",
            "description": "Play on Spotify: the URI returned by spotify_search (track, album, playlist or artist), or resume what is paused when no URI is given. Needs an active Spotify device.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "uri": { "type": "string", "description": "Spotify URI such as spotify:track:… . Omit it to resume." }
                }
            }
        },
        {
            "name": "spotify_pause",
            "description": "Pause Spotify playback.",
            "input_schema": { "type": "object", "properties": {} }
        },
        {
            "name": "spotify_next",
            "description": "Skip to the next track on Spotify.",
            "input_schema": { "type": "object", "properties": {} }
        },
        {
            "name": "spotify_previous",
            "description": "Go back to the previous track on Spotify.",
            "input_schema": { "type": "object", "properties": {} }
        },
        {
            "name": "spotify_queue",
            "description": "Without a URI: the next tracks of the Spotify queue. With a URI: add that track to the queue.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "uri": { "type": "string", "description": "Spotify URI to enqueue; omit it to read the queue." }
                }
            }
        }
    ])
}

/// What the approval card shows: the command itself when it is short enough
/// to read whole, else its first line and its size — the island's line never
/// wraps, so the user is never shown a wall of text they cannot see.
pub fn summarize_command(text: &str) -> String {
    let first = text.lines().next().unwrap_or("").trim();
    let chars = text.chars().count();
    let lines = text.lines().count();
    if !text.contains('\n') && chars <= 120 {
        return text.to_string();
    }
    let head: String = first.chars().take(80).collect();
    let extra_lines = lines.saturating_sub(1);
    format!("{head}… (+{extra_lines} more lines, {chars} chars)")
}

/// One request id for one card. `display` is what the card shows; the
/// executed input stays with the caller.
fn register() -> (String, tokio::sync::oneshot::Receiver<bool>) {
    let id = format!(
        "chat-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let (tx, rx) = tokio::sync::oneshot::channel();
    pending().lock().unwrap().insert(id.clone(), tx);
    (id, rx)
}

fn hook_payload(request_id: &str, event: &str, tool: &str, input: &Value) -> Value {
    json!({
        "hook_event_name": event,
        "request_id": request_id,
        "session_id": "chat",
        "cwd": default_cwd(),
        "coucou_agent": "aicoding",
        "tool_name": tool,
        "tool_input": input,
    })
}

/// Asks the island for one tool call. True only on an explicit Allow: a
/// Deny, a paused island, another card already up, or no answer at all
/// within APPROVAL_WAIT all count as no.
pub async fn approve(app: &AppHandle, tool: &str, display: &Value) -> bool {
    let (id, rx) = register();
    let payload = hook_payload(&id, "PermissionRequest", tool, display);
    let _ = app.emit_to(WINDOW_LABEL, "hook", payload);
    let answer = tokio::time::timeout(APPROVAL_WAIT, rx).await;
    let allowed = matches!(answer, Ok(Ok(true)));
    pending().lock().unwrap().remove(&id);
    allowed
}

/// The island answered a card. True when the request was one of ours, so the
/// caller can leave the hook relay alone.
pub fn resolve(request_id: &str, allow: bool) -> bool {
    let Some(tx) = pending().lock().unwrap().remove(request_id) else {
        return false;
    };
    let _ = tx.send(allow);
    true
}

/// True while a card of ours waits for an answer (the ack is a no-op for it).
pub fn is_ours(request_id: &str) -> bool {
    pending().lock().unwrap().contains_key(request_id)
}

/// Every waiting card is answered "no": the chat was reset or a new question
/// began, so the old question must not run anything anymore.
pub fn deny_all() {
    for (_, tx) in pending().lock().unwrap().drain() {
        let _ = tx.send(false);
    }
}

/// The pill's ticker: PreToolUse before the card, PostToolUse(Failure)
/// after the result — the same events an agent session produces.
pub fn step(app: &AppHandle, event: &str, tool: &str, input: &Value) {
    let payload = hook_payload("", event, tool, input);
    let _ = app.emit_to(WINDOW_LABEL, "hook", payload);
}

/// The turn is over: the pill shows the answer and leaves as any agent's
/// does, or shows the failure and is cleaned up straight away.
pub fn finish(app: &AppHandle, answer: &str, ok: bool) {
    if ok {
        let payload = json!({
            "hook_event_name": "Stop",
            "session_id": "chat",
            "cwd": default_cwd(),
            "coucou_agent": "aicoding",
            "last_assistant_message": answer,
        });
        let _ = app.emit_to(WINDOW_LABEL, "hook", payload);
    } else {
        let base = json!({
            "hook_event_name": "StopFailure",
            "session_id": "chat",
            "cwd": default_cwd(),
            "coucou_agent": "aicoding",
        });
        let _ = app.emit_to(WINDOW_LABEL, "hook", &base);
        let end = json!({
            "hook_event_name": "SessionEnd",
            "session_id": "chat",
            "cwd": default_cwd(),
            "coucou_agent": "aicoding",
        });
        let _ = app.emit_to(WINDOW_LABEL, "hook", end);
    }
}

// ── Execution ─────────────────────────────────────────────────────────────────

fn cut(text: String) -> String {
    if text.chars().count() <= MAX_RESULT {
        return text;
    }
    let head: String = text.chars().take(MAX_RESULT).collect();
    format!("{head}\n… output cut at {MAX_RESULT} characters")
}

fn str_field(input: &Value, key: &str) -> Result<String, String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| tf("The tool needs a value for {field}.", &[("field", key)]))
}

fn opt_field(input: &Value, key: &str) -> Option<String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The directory a command runs in: the model's, or the default one — which
/// must exist, or the command would run somewhere unnameable.
fn workdir(cwd: Option<&str>) -> Result<PathBuf, String> {
    match cwd {
        Some(dir) => {
            let path = PathBuf::from(dir);
            if !path.is_dir() {
                return Err(tf("No such directory: {path}", &[("path", dir)]));
            }
            Ok(path)
        }
        None => Ok(default_cwd()),
    }
}

/// Reads `path` as text lines (1-based `offset`, up to `limit` lines).
fn read_file(input: &Value) -> Result<String, String> {
    let path = str_field(input, "path")?;
    let file = PathBuf::from(&path);
    let meta = std::fs::metadata(&file)
        .map_err(|e| tf("Cannot read {path}: {error}", &[("path", &path), ("error", &e.to_string())]))?;
    if meta.is_dir() {
        return Err(tf("{path} is a directory, not a file.", &[("path", &path)]));
    }
    if meta.len() > MAX_FILE {
        return Err(tf(
            "{path} is larger than {max} MB — use offset and limit on a smaller part.", &[("path", &path), ("max", &(MAX_FILE / 1024 / 1024).to_string())],
        ));
    }
    let bytes = std::fs::read(&file)
        .map_err(|e| tf("Cannot read {path}: {error}", &[("path", &path), ("error", &e.to_string())]))?;
    if bytes.iter().take(8192).any(|b| *b == 0) {
        return Err(tf("{path} looks like a binary file, not text.", &[("path", &path)]));
    }
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    let offset = input.get("offset").and_then(Value::as_i64).unwrap_or(1).max(1) as usize;
    let limit = input
        .get("limit")
        .and_then(Value::as_i64)
        .unwrap_or(2000)
        .clamp(1, 8000) as usize;
    let start = offset.saturating_sub(1).min(lines.len());
    let end = (start + limit).min(lines.len());
    if start >= lines.len() {
        return Ok(tf("The file has {count} lines.", &[("count", &lines.len().to_string())]));
    }
    let mut out = String::new();
    for (i, line) in lines[start..end].iter().enumerate() {
        out.push_str(&format!("{}\t{line}\n", start + i + 1));
    }
    if end < lines.len() {
        out.push_str(&format!(
            "… ({lines_end} more lines)",
            lines_end = lines.len() - end
        ));
    }
    Ok(cut(out))
}

/// Creates or overwrites `file_path` with `content`.
fn write_file(input: &Value) -> Result<String, String> {
    let path = str_field(input, "file_path")?;
    let content = input
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| tf("The tool needs a value for {field}.", &[("field", "content")]))?;
    if content.len() > MAX_WRITE {
        return Err(tf(
            "That content is larger than {max} MB — write it in parts.", &[("max", &(MAX_WRITE / 1024 / 1024).to_string())],
        ));
    }
    let file = PathBuf::from(&path);
    if let Some(parent) = file.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| {
                tf("Cannot create {path}: {error}", &[("path", &parent.display().to_string()), ("error", &e.to_string())])
            })?;
        }
    }
    std::fs::write(&file, content.as_bytes())
        .map_err(|e| tf("Cannot write {path}: {error}", &[("path", &path), ("error", &e.to_string())]))?;
    Ok(tf(
        "Wrote {bytes} bytes to {path}.", &[("bytes", &content.len().to_string()), ("path", &path)],
    ))
}

/// Lists `path`: directories first, then files with sizes.
fn list_dir(input: &Value) -> Result<String, String> {
    let path = str_field(input, "path")?;
    let dir = PathBuf::from(&path);
    if !dir.is_dir() {
        return Err(tf("No such directory: {path}", &[("path", &path)]));
    }
    let mut dirs: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    let entries = std::fs::read_dir(&dir)
        .map_err(|e| tf("Cannot list {path}: {error}", &[("path", &path), ("error", &e.to_string())]))?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let meta = entry.metadata().ok();
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        if is_dir {
            dirs.push(format!("{name}/"));
        } else {
            files.push(format!("{name}  ({size} B)"));
        }
        if dirs.len() + files.len() >= 500 {
            files.push("… more entries cut".into());
            break;
        }
    }
    dirs.sort();
    files.sort();
    let mut out = dirs;
    out.extend(files);
    if out.is_empty() {
        return Ok(t("(empty directory)"));
    }
    Ok(cut(out.join("\n")))
}

/// What a finished process produced, shaped for the model.
struct Outcome {
    status: i32,
    stdout: String,
    stderr: String,
}

impl Outcome {
    fn render(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if !self.stdout.is_empty() {
            parts.push(&self.stdout);
        }
        if !self.stderr.is_empty() {
            parts.push(&self.stderr);
        }
        let mut text = parts.join("\n");
        if text.trim().is_empty() {
            text = t("(no output)").into();
        }
        if self.status != 0 {
            text.push_str(&format!("\n(exit code {})", self.status));
        }
        cut(text)
    }
}

/// Runs a program with a deadline. The future owns the child: dropping it at
/// the deadline kills the process, so a runaway command cannot outlive the turn.
async fn run(program: &str, args: &[&str], cwd: &Path) -> Result<Outcome, RunError> {
    let future = tokio::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .output();
    match tokio::time::timeout(COMMAND_WAIT, future).await {
        Err(_) => Err(RunError::TimedOut),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => Err(RunError::NotFound(program.to_string())),
        Ok(Err(e)) => Err(RunError::Io(format!("{program}: {e}"))),
        Ok(Ok(out)) => Ok(Outcome {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        }),
    }
}

enum RunError {
    NotFound(String),
    Io(String),
    TimedOut,
}

impl RunError {
    fn into_string(self) -> String {
        match self {
            RunError::NotFound(program) => tf("{program} is not installed on this computer.", &[("program", &program)]),
            RunError::Io(what) => what,
            RunError::TimedOut => t("The command timed out after 60 seconds.").into(),
        }
    }
}

/// PowerShell with no profile and no interactivity, in `cwd`.
async fn powershell(input: &Value) -> Result<String, String> {
    let command = str_field(input, "command")?;
    let cwd = workdir(opt_field(input, "cwd").as_deref())?;
    let output = run(
        "powershell.exe",
        &["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &command],
        &cwd,
    )
    .await
    .map_err(RunError::into_string)?;
    Ok(output.render())
}

/// A Python script in a temp file, in `cwd`. `py -3` is the Windows fallback
/// when `python` is not on the PATH.
async fn python(input: &Value) -> Result<String, String> {
    let code = str_field(input, "code")?;
    let cwd = workdir(opt_field(input, "cwd").as_deref())?;
    let script = std::env::temp_dir().join(format!(
        "coucou-chat-{}-{}.py",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&script, code.as_bytes())
        .map_err(|e| tf("Cannot write the script: {error}", &[("error", &e.to_string())]))?;
    let path = script.to_string_lossy().to_string();
    let result = match run("python", &["-u", &path], &cwd).await {
        Err(RunError::NotFound(_)) => run("py", &["-3", "-u", &path], &cwd).await,
        other => other,
    };
    let _ = std::fs::remove_file(&script);
    let output = result.map_err(RunError::into_string)?;
    Ok(output.render())
}

/// The Spotify tools fail early, in the interface's language, when the
/// connector has never been set up: the model must be able to tell the user
/// what to do about it instead of relaying a bare API error.
fn spotify_ready() -> Result<(), String> {
    if crate::spotify::connected() {
        Ok(())
    } else {
        Err(t("Not connected to Spotify — connect it in Settings."))
    }
}

/// One tool call, executed. Errors go back to the model as a failed result —
/// the turn continues, the island only shows what the model then says.
pub async fn execute(name: &str, input: &Value) -> Result<String, String> {
    match name {
        "read_file" => read_file(input),
        "write_file" => write_file(input),
        "list_dir" => list_dir(input),
        "run_powershell" => powershell(input).await,
        "run_python" => python(input).await,
        "spotify_now" => {
            spotify_ready()?;
            crate::spotify::current_music().await
        }
        "spotify_search" => {
            spotify_ready()?;
            let query = str_field(input, "query")?;
            match opt_field(input, "type").as_deref() {
                Some("playlist") => crate::spotify::search_playlists(&query).await,
                _ => crate::spotify::search_tracks(&query).await,
            }
        }
        "spotify_play" => {
            spotify_ready()?;
            let uri = opt_field(input, "uri").unwrap_or_default();
            crate::spotify::play(&uri).await
        }
        "spotify_pause" => {
            spotify_ready()?;
            crate::spotify::pause().await
        }
        "spotify_next" => {
            spotify_ready()?;
            crate::spotify::next_track().await
        }
        "spotify_previous" => {
            spotify_ready()?;
            crate::spotify::previous_track().await
        }
        "spotify_queue" => {
            spotify_ready()?;
            crate::spotify::queue(opt_field(input, "uri")).await
        }
        other => Err(tf("Unknown tool: {tool}.", &[("tool", other)])),
    }
}

/// The path or command the card must show for this call, in the field the
/// island's approval card reads (see APPROVAL_FIELDS). The model's input is
/// never shown raw when it is too long to read: the summary carries size.
pub fn display_input(name: &str, input: &Value) -> Value {
    let mut shown = input.clone();
    match name {
        "run_powershell" => {
            if let Ok(cmd) = str_field(input, "command") {
                let cwd = opt_field(input, "cwd");
                let mut summary = summarize_command(&cmd);
                if let Some(dir) = cwd {
                    summary = format!("{summary}  (in {dir})");
                }
                shown["command"] = json!(summary);
            }
        }
        "run_python" => {
            if let Ok(code) = str_field(input, "code") {
                shown["command"] = json!(summarize_command(&code));
            }
        }
        _ => {}
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "coucou-chat-tools-{tag}-{}",
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn short_commands_show_whole_long_ones_show_size() {
        assert_eq!(summarize_command("Get-Date"), "Get-Date");
        let long = "Write-Output 'line'\n".repeat(40);
        let shown = summarize_command(&long);
        assert!(shown.starts_with("Write-Output 'line'…"));
        assert!(shown.contains("chars"));
    }

    #[test]
    fn a_registered_request_can_only_be_answered_once() {
        let (id, rx) = register();
        assert!(is_ours(&id));
        assert!(resolve(&id, true));
        assert!(!is_ours(&id));
        assert!(!resolve(&id, false));
        assert!(rx.blocking_recv().unwrap());
    }

    #[test]
    fn deny_all_releases_every_waiter_with_no() {
        let (a, rx_a) = register();
        let (b, rx_b) = register();
        deny_all();
        assert!(!rx_a.blocking_recv().unwrap());
        assert!(!rx_b.blocking_recv().unwrap());
        assert!(!is_ours(&a));
        assert!(!is_ours(&b));
    }

    #[test]
    fn read_and_write_round_trip_through_the_tools() {
        let dir = temp_dir("rw");
        let file = dir.join("note.txt");
        let path = file.to_string_lossy().to_string();
        let wrote = write_file(&json!({ "file_path": path, "content": "one\ntwo\nthree\n" })).unwrap();
        assert!(wrote.contains(&path));
        let read = read_file(&json!({ "path": path })).unwrap();
        assert!(read.contains("1\tone"));
        assert!(read.contains("3\tthree"));
        let window = read_file(&json!({ "path": path, "offset": 2, "limit": 1 })).unwrap();
        assert!(window.contains("2\ttwo"));
        assert!(!window.contains("one"));
        let listed = list_dir(&json!({ "path": dir.to_string_lossy() })).unwrap();
        assert!(listed.contains("note.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_binary_file_is_refused_instead_of_returning_garbage() {
        let dir = temp_dir("bin");
        let file = dir.join("blob.bin");
        std::fs::write(&file, [0u8, 1, 2, 3, 0]).unwrap();
        let err = read_file(&json!({ "path": file.to_string_lossy() })).unwrap_err();
        assert!(err.contains("binary"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_card_shows_the_python_script_summarized_not_raw() {
        let code = "print('hi')\n" .repeat(30);
        let shown = display_input("run_python", &json!({ "code": code }));
        let command = shown["command"].as_str().unwrap();
        assert!(command.starts_with("print('hi')…"));
        assert!(!command.contains('\n'));
    }

    #[test]
    fn powershell_reports_output_and_exit_codes() {
        // Same manual runtime as local_chat.rs and net.rs: no macro feature.
        // The cwd is explicit: another test rewrites USERPROFILE in parallel,
        // and a child must never spawn in a directory that is being swapped.
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let cwd = std::env::current_dir().unwrap().to_string_lossy().to_string();
        rt.block_on(async {
            let out = powershell(&json!({ "command": "Write-Output 'hi'", "cwd": cwd })).await.unwrap();
            assert!(out.contains("hi"));
            let failed = powershell(&json!({ "command": "exit 3", "cwd": cwd })).await.unwrap();
            assert!(failed.contains("exit code 3"));
        });
    }

    #[test]
    fn the_spotify_tools_are_offered_with_their_fields() {
        let list = defs().as_array().unwrap().clone();
        let names: Vec<&str> = list.iter().map(|d| d["name"].as_str().unwrap()).collect();
        for tool in [
            "spotify_now", "spotify_search", "spotify_play", "spotify_pause",
            "spotify_next", "spotify_previous", "spotify_queue",
        ] {
            assert!(names.contains(&tool), "{tool} is not offered");
        }
        let schema = |name: &str| list.iter().find(|d| d["name"].as_str() == Some(name)).unwrap()["input_schema"].clone();
        assert_eq!(schema("spotify_search")["required"], json!(["query"]));
        assert_eq!(schema("spotify_search")["properties"]["type"]["type"], "string");
        assert_eq!(schema("spotify_play")["properties"]["uri"]["type"], "string");
        assert_eq!(schema("spotify_queue")["properties"]["uri"]["type"], "string");
        assert!(schema("spotify_now").get("required").is_none());
        assert!(schema("spotify_pause").get("required").is_none());
    }

    #[test]
    fn spotify_calls_carry_what_the_card_shows() {
        let search = display_input("spotify_search", &json!({ "query": "daft punk" }));
        assert_eq!(search["query"], "daft punk");
        let play = display_input("spotify_play", &json!({ "uri": "spotify:track:4uLU" }));
        assert_eq!(play["uri"], "spotify:track:4uLU");
        let now = display_input("spotify_now", &json!({}));
        assert_eq!(now, json!({}));
    }
}
