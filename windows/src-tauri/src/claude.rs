// Claude API client — the same integration as ClaudeService.swift: multi-turn
// chat and files sent as document/image/text blocks. On top of that it carries
// the local tools that let the chat open a document, change data or run Python,
// which is what makes it behave like Claude rather than answer like a chatbox.
//
// Everything happens here rather than in the island: the API key never leaves
// the Credential Manager, and file bytes never cross the IPC boundary.
//
// The endpoint speaks Anthropic's /v1/messages on any base URL that does —
// AICoding (the default) and api.anthropic.com both answer it, so the request,
// the model and the conversation are identical on either. Only the auth header
// differs: `Authorization: Bearer` everywhere except api.anthropic.com.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::AppHandle;

use crate::{pipe, secrets};

/// Anthropic-format base URL. AICoding's partner endpoint: it serves
/// `/v1/messages`, `/v1/models` and `/v1/kredit` behind one Bearer key.
pub const DEFAULT_API_BASE: &str = "https://partner.api-github.com";

pub const DEFAULT_MODEL: &str = "sonnet-5";
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Server-side fallback: on a policy decline the API retries the same request on
/// a fallback model inside the same call, so the island never shows a dead end.
/// Anthropic understands the header; AICoding does not, hence the base-URL check.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const MAX_TOKENS: u32 = 4096;
/// Text and code files are inlined; anything larger is truncated, as on macOS.
const MAX_INLINE_TEXT: u64 = 200_000;
/// One answer may need several local tool calls before it can be given.
const MAX_TOOL_ROUNDS: usize = 10;
/// Cap on how much a single tool may push into the conversation.
const MAX_TOOL_RESULT: usize = 40_000;
/// Local tools run on a worker thread; this is their wall clock.
const TOOL_TIMEOUT: Duration = Duration::from_secs(30);
/// Files the bundled reader script opens — everything else is read as text.
const DOCUMENT_EXTS: &[&str] = &[
    "docx", "docm", "dotx", "dotm", "pptx", "pptm", "ppsx", "ppsm", "potx",
    "xlsx", "xlsm", "xltx",
    "odt", "ods", "odp", "ott", "ots",
    "pdf", "rtf",
];

/// The bundled document reader, written in Python so that Word, Excel,
/// PowerPoint, OpenDocument, RTF and PDF all open with the standard library:
/// zipfile + xml for the OOXML/ODF family, zlib for PDF, one small scanner for
/// RTF. No pip install, nothing to keep updated on the user's machine.
const READER_PY: &str = r#"# coucou_read.py — text extraction from office documents, standard library only.
# Kept next to coucou-hook.exe and run as `python coucou_read.py <file>`.
import html
import os
import re
import sys
import zipfile
import zlib

try:
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
except Exception:
    pass


def unent(s):
    return html.unescape(s)


def para_text(xml):
    """WordprocessingML / DrawingML paragraph text: runs concatenated, cells and
    paragraphs separated by tabs and newlines."""
    s = re.sub(r"<!--.*?-->", "", xml, flags=re.S)
    s = re.sub(r">\n+<", "><", s)
    s = re.sub(r"<w:instrText.*?</w:instrText>", "", s, flags=re.S)
    s = re.sub(r"<w:del\b.*?</w:del>", "", s, flags=re.S)
    s = re.sub(r"<w:tab\b[^>]*/>", "\t", s)
    s = re.sub(r"<(?:w|a):br\b[^>]*/>", "\n", s)
    s = s.replace("</w:p>", "\n").replace("</a:p>", "\n")
    # A paragraph closing inside a cell must not break the row.
    s = re.sub(r"\n+</w:tc>", "\t", s)
    s = re.sub(r"\n+</w:tr>", "\n", s)
    s = s.replace("</w:tc>", "\t").replace("</w:tr>", "\n")
    s = re.sub(r"<[^>]+>", "", s)
    return unent(s)


def docx(path):
    z = zipfile.ZipFile(path)
    names = [n for n in z.namelist()
             if re.match(r"word/(document|footnotes|endnotes)\.xml$", n)
             or re.match(r"word/(header|footer)\d+\.xml$", n)]
    order = {"word/document.xml": 0}
    names.sort(key=lambda n: order.get(n, 1))
    out = []
    for n in names:
        out.append(para_text(z.read(n).decode("utf-8", "replace")))
    return "\n".join(out)


def pptx(path):
    z = zipfile.ZipFile(path)
    slides = [n for n in z.namelist() if re.match(r"ppt/slides/slide\d+\.xml$", n)]
    slides.sort(key=lambda n: int(re.search(r"(\d+)\.xml$", n).group(1)))
    out = []
    for i, n in enumerate(slides, 1):
        text = para_text(z.read(n).decode("utf-8", "replace")).strip()
        if text:
            out.append("Slide %d\n%s" % (i, text))
    return "\n\n".join(out)


def col_num(ref):
    n = 0
    for ch in ref:
        if ch.isalpha():
            n = n * 26 + (ord(ch.upper()) - 64)
    return n


def sheet_rows(data, shared):
    rows = []
    for row in re.findall(r"<row\b[^>]*>(.*?)</row>", data, flags=re.S):
        row = re.sub(r">\n+<", "><", row)
        cells = {}
        for m in re.finditer(r"<c\b([^>]*?)(?:/>|>(.*?)</c>)", row, flags=re.S):
            attrs, body = m.group(1), m.group(2) or ""
            ref = re.search(r'\br="([A-Z]+)\d+"', attrs)
            col = col_num(ref.group(1)) if ref else len(cells) + 1
            t = re.search(r'\bt="([^"]+)"', attrs)
            typ = t.group(1) if t else ""
            value = ""
            if "<is>" in body or typ == "inlineStr":
                value = unent("".join(re.findall(r"<t[^>]*>(.*?)</t>", body, flags=re.S)))
            else:
                v = re.search(r"<v>(.*?)</v>", body, flags=re.S)
                if v:
                    raw = unent(v.group(1))
                    if typ == "s":
                        try:
                            value = shared[int(raw)]
                        except Exception:
                            value = raw
                    else:
                        value = raw
            if value != "":
                cells[col] = value
        if not cells:
            continue
        width = max(cells)
        rows.append("\t".join(cells.get(i, "") for i in range(1, width + 1)).rstrip())
    return "\n".join(rows)


def xlsx(path):
    z = zipfile.ZipFile(path)
    shared = []
    if "xl/sharedStrings.xml" in z.namelist():
        data = z.read("xl/sharedStrings.xml").decode("utf-8", "replace")
        for si in re.findall(r"<si>(.*?)</si>", data, flags=re.S):
            si = re.sub(r">\n+<", "><", si)
            shared.append(unent("".join(re.findall(r"<t[^>]*>(.*?)</t>", si, flags=re.S))))
    sheets = [n for n in z.namelist() if re.match(r"xl/worksheets/sheet\d+\.xml$", n)]
    sheets.sort(key=lambda n: int(re.search(r"(\d+)\.xml$", n).group(1)))
    titles = []
    if "xl/workbook.xml" in z.namelist():
        wb = z.read("xl/workbook.xml").decode("utf-8", "replace")
        titles = [unent(t) for t in re.findall(r'<sheet[^>]*\bname="([^"]*)"', wb)]
    out = []
    for i, n in enumerate(sheets):
        title = titles[i] if i < len(titles) else "Sheet %d" % (i + 1)
        body = sheet_rows(z.read(n).decode("utf-8", "replace"), shared)
        out.append("--- %s ---\n%s" % (title, body))
    return "\n".join(out)


def odf(path):
    z = zipfile.ZipFile(path)
    s = z.read("content.xml").decode("utf-8", "replace")
    s = re.sub(r"<!--.*?-->", "", s, flags=re.S)
    s = re.sub(r">\n+<", "><", s)

    def spaces(m):
        c = re.search(r'text:c="(\d+)"', m.group(0))
        return " " * (int(c.group(1)) if c else 1)

    s = re.sub(r"<text:s\b[^>]*/>", spaces, s)
    s = re.sub(r"<text:line-break\b[^>]*/>", "\n", s)
    s = re.sub(r"<text:tab\b[^>]*/>", "\t", s)
    s = s.replace("</text:p>", "\n").replace("</text:h>", "\n")
    # A paragraph closing inside a cell must not break the row.
    s = re.sub(r"\n+</table:table-cell>", "\t", s)
    s = re.sub(r"\n+</table:table-row>", "\n", s)
    s = s.replace("</table:table-cell>", "\t").replace("</table:table-row>", "\n")
    s = re.sub(r"<[^>]+>", "", s)
    return unent(s)


def decode_bytes(raw):
    if not raw:
        return ""
    if raw[:2] == b"\xfe\xff":
        return raw[2:].decode("utf-16-be", "replace")
    if raw[:2] == b"\xff\xfe":
        return raw[2:].decode("utf-16-le", "replace")
    try:
        return raw.decode("utf-8")
    except UnicodeDecodeError:
        return raw.decode("latin-1", "replace")


def pdf_literal(b, start):
    """Reads a PDF literal string `(...)` starting at `start`; returns (text, next)."""
    depth = 1
    i = start + 1
    buf = bytearray()
    n = len(b)
    while i < n:
        c = b[i]
        if c == 0x5C:  # backslash
            i += 1
            if i >= n:
                break
            e = b[i]
            if e in b"nrtbf":
                buf += {0x6E: b"\n", 0x72: b"\r", 0x74: b"\t",
                        0x62: b"\b", 0x66: b"\x0C"}[e]
                i += 1
            elif e in b"()\\":
                buf.append(e)
                i += 1
            elif 0x30 <= e <= 0x37:
                digits = ""
                while i < n and len(digits) < 3 and 0x30 <= b[i] <= 0x37:
                    digits += chr(b[i])
                    i += 1
                buf.append(int(digits, 8) & 0xFF)
            elif e == 0x0D:
                i += 1
                if i < n and b[i] == 0x0A:
                    i += 1
            elif e == 0x0A:
                i += 1
            else:
                buf.append(e)
                i += 1
        elif c == 0x28:
            depth += 1
            buf.append(c)
            i += 1
        elif c == 0x29:
            depth -= 1
            i += 1
            if depth == 0:
                break
            buf.append(c)
        else:
            buf.append(c)
            i += 1
    return decode_bytes(bytes(buf)), i


def pdf_stream_text(b):
    out = []
    i = 0
    n = len(b)
    while i < n:
        c = b[i]
        if c == 0x28:
            text, i = pdf_literal(b, i)
            if text:
                out.append(text)
        elif c == 0x3C and i + 1 < n and b[i + 1] != 0x3C:
            j = b.find(b">", i + 1)
            if j < 0:
                break
            hx = re.sub(rb"[^0-9A-Fa-f]", b"", b[i + 1:j])
            if len(hx) % 2:
                hx += b"0"
            if hx:
                out.append(decode_bytes(bytes.fromhex(hx.decode("ascii"))))
            i = j + 1
        elif c == 0x5C:  # line continuation outside a string
            i += 2
        elif c in b" \t\r\n\x00":
            i += 1
        else:
            j = i
            while j < n and b[j] not in b" \t\r\n\x00()<>[]{}":
                j += 1
            token = b[i:j]
            if token in (b"Td", b"TD", b"T*", b"BT", b"ET", b"'", b"Em"):
                if out and not out[-1].endswith("\n"):
                    out.append("\n")
            elif token and token[0:1] in b"-+." or (token and token[0:1].isdigit()):
                try:
                    gap = float(token)
                except ValueError:
                    gap = 0.0
                if gap <= -200 and out:
                    out.append(" ")
            i = j if j > i else i + 1
    return "".join(out).replace("  ", " ")


def pdf(path):
    data = open(path, "rb").read()
    chunks = []
    for m in re.finditer(rb"stream\r?\n(.*?)\r?\nendstream", data, flags=re.S):
        raw = m.group(1)
        try:
            raw = zlib.decompress(raw)
        except Exception:
            pass
        if b"Tj" in raw or b"TJ" in raw:
            chunks.append(pdf_stream_text(raw))
    return "\n".join(chunks)


def rtf(path):
    data = open(path, "rb").read().decode("latin-1", "replace")
    out = []
    depth = 0
    skip = None
    uc = 1
    i = 0
    n = len(data)
    destinations = (
        "fonttbl", "colortbl", "stylesheet", "listtable", "listoverridetable",
        "info", "pict", "object", "header", "footer", "themedata", "generator",
        "datastore", "latentstyles", "stylesheed",
    )
    while i < n:
        c = data[i]
        if c == "{":
            depth += 1
            i += 1
        elif c == "}":
            if skip is not None and depth == skip:
                skip = None
            depth -= 1
            i += 1
        elif c == "\\":
            i += 1
            if i >= n:
                break
            if data[i].isalpha():
                j = i
                while j < n and data[j].isalpha():
                    j += 1
                word = data[i:j]
                k = j
                sign = ""
                if k < n and data[k] in "-+":
                    sign = data[k]
                    k += 1
                digits = ""
                while k < n and data[k].isdigit():
                    digits += data[k]
                    k += 1
                if k < n and data[k] == " ":
                    k += 1
                if word == "*":
                    skip = depth
                elif word in destinations and skip is None:
                    skip = depth
                elif word == "uc":
                    uc = int(digits or 1)
                elif skip is None:
                    if word in ("par", "line", "row"):
                        out.append("\n")
                    elif word in ("tab", "cell"):
                        out.append("\t")
                    elif word == "emdash":
                        out.append("\u2014")
                    elif word == "endash":
                        out.append("\u2013")
                    elif word == "u" and digits:
                        val = int(digits)
                        out.append(chr(val if val >= 0 else val + 65536))
                        for _ in range(uc):
                            if i < n and data[i] not in "\r\n":
                                i += 1
                    elif word == "bin" and digits:
                        i = k + int(digits)
                        continue
                i = k
            elif data[i] == "'":
                hx = data[i + 1:i + 3]
                i += 3
                try:
                    byte = bytes([int(hx, 16)])
                except ValueError:
                    byte = b"?"
                if skip is None:
                    out.append(decode_bytes(byte))
            else:
                if skip is None and data[i] in "{}\\":
                    out.append(data[i])
                i += 1
        else:
            if skip is None and c not in "\r\n":
                out.append(c)
            i += 1
    return "".join(out)


READERS = {}
for ext in ("docx", "docm", "dotx", "dotm"):
    READERS[ext] = docx
for ext in ("pptx", "pptm", "ppsx", "ppsm", "potx"):
    READERS[ext] = pptx
for ext in ("xlsx", "xlsm", "xltx"):
    READERS[ext] = xlsx
for ext in ("odt", "ods", "odp", "ott", "ots"):
    READERS[ext] = odf
READERS["pdf"] = pdf
READERS["rtf"] = rtf


def main(argv):
    if len(argv) < 2:
        sys.stderr.write("usage: coucou_read.py <file>\n")
        return 2
    path = argv[1]
    ext = os.path.splitext(path)[1].lower().lstrip(".")
    if not os.path.isfile(path):
        sys.stderr.write("not found: %s\n" % path)
        return 1
    reader = READERS.get(ext)
    if reader is None:
        sys.stderr.write("unsupported file type: .%s\n" % ext)
        return 1
    try:
        text = reader(path)
    except zipfile.BadZipFile:
        sys.stderr.write("not a valid Office or OpenDocument file\n")
        return 1
    except Exception as exc:
        sys.stderr.write("reader failed: %s\n" % exc)
        return 1
    text = (text or "").strip()
    if not text:
        sys.stderr.write("no text found in this file\n")
        return 1
    sys.stdout.write("\n".join(line.rstrip() for line in text.split("\n")))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
"#;

const SYSTEM_PROMPT: &str = "You are Mochi, a personal AI assistant living at the top of the user's screen. \
You have web search access and can help with absolutely anything — research, coding, finding places, recommendations, tasks, questions. \
You also have local tools on the user's machine: read_file opens any document and returns its text (Word, Excel, PowerPoint, OpenDocument, RTF, PDF, code and plain text), \
list_dir lists a folder, write_file creates or rewrites a file, run_python runs a Python 3 script, and web_search searches the web. \
Use them instead of guessing: whenever a task involves a file, some data or a real computation, call the tool, read the result, then answer. \
Every local tool call asks the user for permission first, so make each call count and never call a tool you do not need. \
Respond in the user's language. Be thorough and complete — use as much detail as the task requires. \
No markdown formatting (no **, no ##, no bullet dashes). Use plain text with line breaks.";

#[derive(Default)]
pub struct Chat {
    /// Full multi-turn history, including tool_use / tool_result blocks.
    messages: Mutex<Vec<Value>>,
}

impl Chat {
    pub fn reset(&self) {
        self.messages.lock().unwrap().clear();
    }

    fn is_empty(&self) -> bool {
        self.messages.lock().unwrap().is_empty()
    }

    fn push(&self, message: Value) {
        self.messages.lock().unwrap().push(message);
    }

    fn snapshot(&self) -> Vec<Value> {
        self.messages.lock().unwrap().clone()
    }

    fn len(&self) -> usize {
        self.messages.lock().unwrap().len()
    }

    /// Rolls the history back to `len`. A turn that failed halfway must leave no
    /// assistant `tool_use` without its `tool_result` behind — the next request
    /// would be rejected for exactly that.
    fn truncate(&self, len: usize) {
        let mut messages = self.messages.lock().unwrap();
        if messages.len() > len {
            messages.truncate(len);
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ChatContext {
    File { name: String, path: String },
    Window { app_name: String, title: String, url: Option<String> },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatReply {
    pub text: String,
}

// ── Endpoint ──────────────────────────────────────────────────────────────────

fn normalise_base(base: &str) -> String {
    let trimmed = base.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        DEFAULT_API_BASE.to_string()
    } else {
        trimmed.to_string()
    }
}

fn is_native_anthropic(base: &str) -> bool {
    base.starts_with("https://api.anthropic.com")
}

/// `x-api-key` for Anthropic, `Authorization: Bearer` for every gateway that
/// fronts the same format — AICoding included.
fn auth_header(base: &str, key: &str) -> (&'static str, String) {
    if is_native_anthropic(base) {
        ("x-api-key", key.to_string())
    } else {
        ("Authorization", format!("Bearer {key}"))
    }
}

fn messages_url(base: &str) -> String {
    format!("{}/v1/messages", normalise_base(base))
}

fn models_url(base: &str) -> String {
    format!("{}/v1/models?limit=100", normalise_base(base))
}

/// Model ids the endpoint actually accepts. AICoding shortens them (`sonnet-5`,
/// `opus-5`), Anthropic does not — so the list is always read from the API and
/// only falls back to a static one when the key or the network says no.
pub async fn fetch_models(base: &str, key: &str) -> Result<Vec<String>, String> {
    let url = models_url(base);
    let (header, value) = auth_header(base, key);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .get(&url)
        .header(header, value)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .send()
        .await
        .map_err(|e| format!("Network error: {e}"))?;
    let status = response.status();
    let text = response.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("Model list {status}: {}", first_error_line(&text)));
    }
    let json: Value = serde_json::from_str(&text).map_err(|e| format!("Bad API response: {e}"))?;
    let items = json.get("data").and_then(Value::as_array).ok_or("No models returned.")?;
    let ids = items
        .iter()
        .filter_map(|item| item.get("id").and_then(Value::as_str).map(str::to_string))
        .collect::<Vec<_>>();
    if ids.is_empty() {
        return Err("No models returned.".into());
    }
    Ok(ids)
}

fn first_error_line(text: &str) -> String {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .or_else(|| v.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| text.chars().take(200).collect())
}

// ── Tools ─────────────────────────────────────────────────────────────────────

/// What the model may call. All five are executed here, behind a click in the
/// island — nothing touches the disk or the network without one.
fn tools() -> Value {
    json!([
        {
            "name": "read_file",
            "description": "Read a file and return its contents as text. Word, Excel, PowerPoint, OpenDocument, RTF and PDF documents are extracted by a local reader; text and code files are returned as-is. Use this before answering anything about a file's contents.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Full path of the file." }
                },
                "required": ["path"]
            }
        },
        {
            "name": "list_dir",
            "description": "List the entries of a folder: name, and for files their size in bytes.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Full path of the folder." }
                },
                "required": ["path"]
            }
        },
        {
            "name": "write_file",
            "description": "Create a file, or overwrite it when it already exists, with exactly this text content. Parent folders are created when needed.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "Full path of the file to write." },
                    "content": { "type": "string", "description": "The whole new content of the file." }
                },
                "required": ["file_path", "content"]
            }
        },
        {
            "name": "run_python",
            "description": "Run a Python 3 script on the user's machine and return stdout and stderr. Use it for anything that needs a real computation or data handling: parsing or converting files, calculations, reading a folder in bulk, rewriting structured data.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Complete Python source code to execute." }
                },
                "required": ["command"]
            }
        },
        {
            "name": "web_search",
            "description": "Search the web and return up to five results, each with its title, URL and snippet.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "The search query." }
                },
                "required": ["query"]
            }
        }
    ])
}

/// One local tool call: ask the human, then run it. The answer is whatever goes
/// back into the conversation as the `tool_result`.
async fn run_tool(app: &AppHandle, name: &str, input: &Value) -> (Value, bool) {
    if !matches!(name, "read_file" | "list_dir" | "write_file" | "run_python" | "web_search") {
        return (json!(format!("Unknown tool: {name}")), true);
    }
    if !approve(app, name, input).await {
        return (json!("Permission denied by the user."), true);
    }

    let outcome = if name == "web_search" {
        let query = input
            .get("query")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        match query {
            Some(query) => web_search_tool(&query).await,
            None => Err("No query given.".to_string()),
        }
    } else {
        let name = name.to_string();
        let input = input.clone();
        match tauri::async_runtime::spawn_blocking(move || dispatch_local(&name, &input)).await {
            Ok(result) => result,
            Err(err) => Err(format!("tool thread failed: {err}")),
        }
    };

    match outcome {
        Ok(text) => (json!(cap(&text)), false),
        Err(err) => (json!(cap(&err)), true),
    }
}

/// The file and Python tools are disk-and-CPU work, so they run on a worker
/// thread; the search tool is the only one that wants the async HTTP client.
fn dispatch_local(name: &str, input: &Value) -> Result<String, String> {
    let str_arg = |field: &str| {
        input
            .get(field)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    match name {
        "read_file" => str_arg("path")
            .ok_or_else(|| "No path given.".to_string())
            .and_then(|path| read_file_tool(&path)),
        "list_dir" => str_arg("path")
            .ok_or_else(|| "No path given.".to_string())
            .and_then(|path| list_dir_tool(&path)),
        "write_file" => {
            let path = str_arg("file_path").ok_or_else(|| "No path given.".to_string());
            let content = input.get("content").and_then(Value::as_str).map(str::to_string);
            match (path, content) {
                (Ok(path), Some(content)) => write_file_tool(&path, &content),
                (Err(err), _) => Err(err),
                (_, None) => Err("No content given.".to_string()),
            }
        }
        "run_python" => str_arg("command")
            .ok_or_else(|| "No code given.".to_string())
            .and_then(|code| run_python_tool(&code)),
        _ => Err("Unknown tool.".into()),
    }
}

/// The island's approval card, reused verbatim: same view, same Allow/Deny, same
/// pinned island. The payload only says where to go back to afterwards.
async fn approve(app: &AppHandle, name: &str, input: &Value) -> bool {
    // The card shows `tool_input` only, so a 40-line script would otherwise be
    // laid out in full. The real input is what gets executed, not this copy.
    let mut display = input.clone();
    if let Some(code) = display.get("command").and_then(Value::as_str) {
        let preview: String = code.chars().take(160).collect();
        if preview.len() < code.len() {
            display["command"] = json!(format!("{preview}…"));
        }
    }
    let payload = json!({
        "hook_event_name": "PermissionRequest",
        "coucou_chat": true,
        "session_id": "",
        "cwd": "",
        "tool_name": name,
        "tool_input": display,
        "subject": "Mochi",
        "return_view": "prompt",
    });
    pipe::ask_chat_tool(app, payload).await
}

fn cap(text: &str) -> String {
    let count = text.chars().count();
    if count <= MAX_TOOL_RESULT {
        return text.to_string();
    }
    let head: String = text.chars().take(MAX_TOOL_RESULT).collect();
    format!("{head}\n[truncated — {count} characters in total]")
}

// ── Local tool bodies ─────────────────────────────────────────────────────────

fn read_file_tool(path: &str) -> Result<String, String> {
    let file = Path::new(path);
    let meta = std::fs::metadata(file).map_err(|e| format!("cannot read {path}: {e}"))?;
    if meta.is_dir() {
        return Err(format!("{path} is a folder — use list_dir instead."));
    }
    let ext = file
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    if DOCUMENT_EXTS.contains(&ext.as_str()) {
        return document_text(file, &ext);
    }

    let len = meta.len();
    if len > MAX_INLINE_TEXT * 2 {
        return Err(format!(
            "{path} is {} bytes — too large to read whole. Use run_python to work through it.",
            len
        ));
    }
    let bytes = std::fs::read(file).map_err(|e| format!("cannot read {path}: {e}"))?;
    if bytes.contains(&0) {
        return Err(format!(
            "{path} is a binary file. Drop it on the island to have me look at it, or convert it first."
        ));
    }
    let mut text = String::from_utf8_lossy(&bytes).to_string();
    if text.len() > MAX_INLINE_TEXT as usize {
        let cut = text
            .char_indices()
            .take_while(|(i, _)| *i < MAX_INLINE_TEXT as usize)
            .last()
            .map(|(i, _)| i)
            .unwrap_or(0);
        text.truncate(cut);
        text.push_str("\n[truncated]");
    }
    Ok(text)
}

fn list_dir_tool(path: &str) -> Result<String, String> {
    let dir = Path::new(path);
    let entries = std::fs::read_dir(dir).map_err(|e| format!("cannot list {path}: {e}"))?;
    let mut folders: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            folders.push(format!("{name}/"));
        } else {
            files.push(format!("{name}  ({} bytes)", meta.len()));
        }
        if folders.len() + files.len() >= 500 {
            files.push("… more entries not shown".into());
            break;
        }
    }
    folders.sort();
    files.sort();
    let mut lines = folders;
    lines.extend(files);
    if lines.is_empty() {
        return Ok(format!("{path} is empty."));
    }
    Ok(lines.join("\n"))
}

fn write_file_tool(path: &str, content: &str) -> Result<String, String> {
    let file = Path::new(path);
    if let Some(parent) = file.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {parent:?}: {e}"))?;
        }
    }
    let bytes = content.as_bytes();
    if bytes.len() > 8_000_000 {
        return Err("That content is over 8 MB — use run_python to write it instead.".into());
    }
    std::fs::write(file, bytes).map_err(|e| format!("cannot write {path}: {e}"))?;
    Ok(format!("Wrote {} bytes to {path}.", bytes.len()))
}

// ── Python ────────────────────────────────────────────────────────────────────

/// `python` / `python3` on PATH, then the Windows launcher. The Microsoft Store
/// alias lives in WindowsApps and opens a store page instead of running code,
/// so it is skipped rather than trusted.
fn find_python() -> Result<(PathBuf, Vec<String>), String> {
    for stem in ["python3", "python"] {
        if let Some(found) = crate::platform::find_on_path(stem) {
            if !is_store_stub(&found) {
                return Ok((found, Vec::new()));
            }
        }
    }
    if let Some(found) = crate::platform::find_on_path("py") {
        return Ok((found, vec!["-3".to_string()]));
    }
    Err("Python 3 was not found on PATH. Install it from python.org (tick \"Add to PATH\"), then ask again.".into())
}

#[cfg(windows)]
fn is_store_stub(path: &Path) -> bool {
    path.to_string_lossy().to_lowercase().contains("windowsapps")
}

#[cfg(not(windows))]
fn is_store_stub(_path: &Path) -> bool {
    false
}

/// Spawns a process without ever flashing a console, reads it to the end and
/// gives up at the deadline. Output comes back as one string, stderr included,
/// because that is where Python puts its tracebacks.
fn run_process(program: &Path, args: &[String], timeout: Duration) -> Result<(String, i32), String> {
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = crate::platform::no_console(&mut command)
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", program.display()))?;

    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let out_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(pipe) = out_pipe.as_mut() {
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    });
    let err_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(pipe) = err_pipe.as_mut() {
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    });

    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(40))
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("timed out after {} seconds", timeout.as_secs()));
            }
            Err(err) => {
                let _ = child.kill();
                return Err(format!("waiting for the process: {err}"));
            }
        }
    };

    let stdout = out_thread.join().unwrap_or_default();
    let stderr = err_thread.join().unwrap_or_default();
    let mut text = String::from_utf8_lossy(&stdout).to_string();
    if !stderr.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&String::from_utf8_lossy(&stderr));
    }
    Ok((text, status.code().unwrap_or(-1)))
}

fn run_python_tool(code: &str) -> Result<String, String> {
    let (program, mut args) = find_python()?;
    args.push("-c".into());
    args.push(code.to_string());

    // `-c` is the real herd of running arbitrary code; stderr is merged into
    // the output so a traceback ends up in the conversation.
    let (text, code) = run_process(&program, &args, TOOL_TIMEOUT)?;
    let text = text.trim().to_string();
    if text.is_empty() {
        if code == 0 {
            return Ok("(the script produced no output)".into());
        }
        return Err(format!("the script exited with code {code} and no output"));
    }
    Ok(text)
}

/// Documents are opened by a small standard-library-only script we keep next to
/// coucou-hook.exe: zipfile and xml handle Word/PowerPoint/Excel/OpenDocument,
/// zlib handles PDF, and nothing has to be pip-installed for it to work.
fn document_text(file: &Path, ext: &str) -> Result<String, String> {
    let script = ensure_reader_script()?;
    let (program, mut args) = find_python()?;
    args.push(script.to_string_lossy().to_string());
    args.push(file.to_string_lossy().to_string());

    // The script decides what to do from the extension; if it does not know the
    // type it exits non-zero with a reason we can pass straight through.
    let (text, code) = run_process(&program, &args, TOOL_TIMEOUT)?;
    if code != 0 {
        let err = text.trim();
        if !err.is_empty() {
            return Err(err.to_string());
        }
        return Err(format!("could not read .{ext} (exit code {code})"));
    }
    let text = text.trim().to_string();
    if text.is_empty() {
        return Err(format!("no text could be extracted from this .{ext} file"));
    }
    Ok(text)
}

fn ensure_reader_script() -> Result<PathBuf, String> {
    let path = crate::settings::local_dir().join("bin").join("coucou_read.py");
    let current = std::fs::read_to_string(&path).ok();
    if current.as_deref() == Some(READER_PY) {
        return Ok(path);
    }
    let dir = path.parent().ok_or("no folder for the reader script")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    std::fs::write(&path, READER_PY).map_err(|e| format!("cannot write the reader script: {e}"))?;
    Ok(path)
}

// ── Web search ────────────────────────────────────────────────────────────────

/// DuckDuckGo has no key and answers any client with plain HTML, so it fits the
/// "no extra account" rule. The results page is scraped for the five top
/// anchors; the app strips redirects and decodes entities.
const SEARCH_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Coucou/1.0";

async fn web_search_tool(query: &str) -> Result<String, String> {
    let url = format!("https://html.duckduckgo.com/html/?q={}", percent_encode(query));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(SEARCH_UA)
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("search failed: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("search failed: {status}"));
    }
    let body = response.text().await.map_err(|e| format!("search failed: {e}"))?;
    Ok(format_web_results(&body))
}

/// `result__a` anchors carry the title and the real link; each block ends at
/// the next anchor, which is where its snippet lives.
fn format_web_results(html: &str) -> String {
    let anchors = html
        .match_indices("result__a")
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    let mut lines = Vec::new();
    for (n, &start) in anchors.iter().take(5).enumerate() {
        let title = element_text(html, start, "a");
        if title.trim().is_empty() {
            continue;
        }
        let finish = anchors.get(n + 1).copied().unwrap_or(html.len());
        let url = attr_after(html, start, "href").unwrap_or_default();
        let snippet = html[start..finish]
            .find("result__snippet")
            .map(|p| element_text(&html[start + p..], 0, "a"))
            .unwrap_or_default();
        lines.push(format!(
            "{}. {}\n{}\n{}",
            n + 1,
            clean_inline(&title),
            resolve_search_url(&url),
            clean_inline(&snippet),
        ));
    }
    if lines.is_empty() {
        return "No results for that query.".to_string();
    }
    lines.join("\n\n")
}

/// The text between `>` and `</tag>`, starting from `from`.
fn element_text(html: &str, from: usize, tag: &str) -> String {
    let Some(gt) = html[from..].find('>') else {
        return String::new();
    };
    let inner = &html[from + gt + 1..];
    let close = format!("</{tag}>");
    match inner.find(&close) {
        Some(end) => inner[..end].to_string(),
        None => inner.to_string(),
    }
}

fn attr_after(html: &str, from: usize, name: &str) -> Option<String> {
    let needle = format!(r#"{name}=""#);
    let tail = &html[from..];
    let at = tail.find(&needle)?;
    let after = &tail[at + needle.len()..];
    after.split('"').next().map(str::to_string)
}

fn clean_inline(text: &str) -> String {
    decode_entities(&remove_tags(text))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn remove_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for ch in text.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'&' {
            let rest = &text[i..];
            let c = rest.chars().next().unwrap();
            out.push(c);
            i += c.len_utf8();
            continue;
        }
        match text[i..].find(';') {
            Some(semi) => {
                let entity = &text[i + 1..i + semi];
                let decoded = match entity {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    "nbsp" => Some(' '),
                    "hellip" => Some('…'),
                    _ => entity
                        .strip_prefix("#x")
                        .and_then(|h| u32::from_str_radix(h, 16).ok())
                        .and_then(char::from_u32)
                        .or_else(|| {
                            entity
                                .strip_prefix('#')
                                .and_then(|d| d.parse::<u32>().ok())
                                .and_then(char::from_u32)
                        }),
                };
                match decoded {
                    Some(c) => {
                        out.push(c);
                        i += semi + 1;
                    }
                    None => {
                        out.push('&');
                        i += 1;
                    }
                }
            }
            None => {
                out.push('&');
                i += 1;
            }
        }
    }
    out
}

/// DDG wraps real links in `/l/?uddg=<encoded>&…`; unwrap or fall back to https.
fn resolve_search_url(href: &str) -> String {
    if href.starts_with("//") {
        let decoded = href
            .split('?')
            .nth(1)
            .and_then(|q| q.split('&').find(|p| p.starts_with("uddg=")))
            .and_then(|p| percent_decode(&p[5..]).ok());
        return decoded.unwrap_or_else(|| format!("https:{href}"));
    }
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }
    href.to_string()
}

fn percent_encode(text: &str) -> String {
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

fn percent_decode(text: &str) -> Result<String, String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            match u8::from_str_radix(&text[i + 1..i + 3], 16).ok() {
                Some(byte) => {
                    out.push(byte);
                    i += 3;
                }
                None => {
                    out.push(b'%');
                    i += 1;
                }
            }
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| "invalid encoding".to_string())
}

// ── Chat ──────────────────────────────────────────────────────────────────────

/// One chat turn. Returns the assistant's text, or a message the island shows
/// in the note view.
pub async fn send(
    app: &AppHandle,
    chat: &Chat,
    model: &str,
    api_base: &str,
    query: String,
    context: Option<ChatContext>,
) -> Result<ChatReply, String> {
    let key = secrets::get("anthropic-api-key")
        .ok_or_else(|| "API key missing. Open settings.".to_string())?;
    let base = normalise_base(api_base);
    let native = is_native_anthropic(&base);

    let mut content: Vec<Value> = Vec::new();

    // File / window context rides along with the first message only, exactly
    // like ClaudeService.chat().
    if chat.is_empty() {
        match &context {
            Some(ChatContext::File { name, path }) => {
                if let Some(block) = file_block(path) {
                    content.push(block);
                }
                content.push(json!({ "type": "text", "text": format!("File: {name}") }));
            }
            Some(ChatContext::Window { app_name, title, url }) => {
                let mut text = format!("Context — App: {app_name}, Window: {title}");
                if let Some(url) = url {
                    text.push_str(&format!(", URL: {url}"));
                }
                content.push(json!({ "type": "text", "text": text }));
            }
            None => {}
        }
    }
    content.push(json!({ "type": "text", "text": query }));

    let mark = chat.len();
    chat.push(json!({ "role": "user", "content": content }));

    let mut rounds = 0usize;
    loop {
        let with_tools = rounds < MAX_TOOL_ROUNDS;
        let mut body = json!({
            "model": model,
            "max_tokens": MAX_TOKENS,
            "system": SYSTEM_PROMPT,
            "messages": chat.snapshot(),
        });
        if with_tools {
            body["tools"] = tools();
        }
        if native {
            body["fallbacks"] = json!("default");
        }

        let response = match call(&base, &key, &body, native).await {
            Ok(value) => value,
            Err(err) => {
                chat.truncate(mark);
                return Err(err);
            }
        };

        // A policy decline comes back as HTTP 200 with stop_reason "refusal".
        if response.get("stop_reason").and_then(Value::as_str) == Some("refusal") {
            chat.truncate(mark);
            let why = response
                .pointer("/stop_details/explanation")
                .and_then(Value::as_str)
                .unwrap_or("Claude declined this one.");
            return Err(why.to_string());
        }

        let Some(blocks) = response.get("content").and_then(Value::as_array).cloned() else {
            chat.truncate(mark);
            return Err("Unexpected API response.".into());
        };

        // Store the whole content — tool_use / tool_result blocks included — so
        // the next turn has the right context.
        chat.push(json!({ "role": "assistant", "content": blocks.clone() }));

        let tool_uses = blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
            .cloned()
            .collect::<Vec<_>>();
        if tool_uses.is_empty() {
            break;
        }
        if !with_tools {
            // The budget ran out; the answer must come from what is already known.
            chat.truncate(mark);
            return Err("Stopped after 10 local tool calls without a final answer.".into());
        }

        rounds += 1;
        let mut results = Vec::with_capacity(tool_uses.len());
        for block in &tool_uses {
            let id = block
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let name = block.get("name").and_then(Value::as_str).unwrap_or_default();
            let input = block.get("input").cloned().unwrap_or_default();
            let (answer, is_error) = run_tool(app, name, &input).await;
            let mut result = json!({ "type": "tool_result", "tool_use_id": id, "content": answer });
            if is_error {
                result["is_error"] = json!(true);
            }
            results.push(result);
        }
        chat.push(json!({ "role": "user", "content": results }));
    }

    let text = chat
        .snapshot()
        .into_iter()
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
        .filter_map(|m| m.get("content").and_then(Value::as_array).cloned())
        .flat_map(|blocks| blocks.into_iter().collect::<Vec<_>>())
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str).map(str::to_string))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();

    if text.is_empty() {
        return Err("No response text.".into());
    }
    Ok(ChatReply { text })
}

async fn call(base: &str, key: &str, body: &Value, native: bool) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(90))
        .build()
        .map_err(|e| e.to_string())?;

    let (header, value) = auth_header(base, key);
    let mut request = client
        .post(messages_url(base))
        .header(header, value)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header("content-type", "application/json")
        .json(body);
    if native {
        request = request.header("anthropic-beta", FALLBACK_BETA);
    }

    let response = request.send().await.map_err(|e| format!("Network error: {e}"))?;
    let status = response.status();
    let text = response.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        // Surface the API's own message, which is what makes a bad key obvious.
        return Err(format!("Claude API {status}: {}", first_error_line(&text)));
    }
    serde_json::from_str(&text).map_err(|e| format!("Bad API response: {e}"))
}

/// PDF → document block, image → image block, text/code → inline text.
fn file_block(path: &str) -> Option<Value> {
    let ext = Path::new(path)
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
/// Also used for Stripe's basic auth.
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

    #[test]
    fn percent_roundtrip() {
        assert_eq!(percent_encode("a b&c/d"), "a+b%26c%2Fd");
        assert_eq!(percent_decode("a+b%26c%2Fd").unwrap(), "a b&c/d");
        assert_eq!(percent_decode("%C3%A9").unwrap(), "é");
    }

    #[test]
    fn web_results_format() {
        let html = r##"
<a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fx%3Fa%3D1&amp;rut=abc">Example &amp; Co</a>
<div class="result__snippet" data-result="snippet"><a class="result__snippet" href="...">A <b>snippet</b> here.</a></div>
<div class="result">
<a class="result__a" href="https://plain.org/">No redirect</a>
<div class="result__snippet"><a class="result__snippet" href="...">Second result</a></div>
</div>
"##;
        let out = format_web_results(html);
        assert!(out.contains("1. Example & Co"), "got: {out}");
        assert!(out.contains("https://example.com/x?a=1"), "got: {out}");
        assert!(out.contains("A snippet here."), "got: {out}");
        assert!(out.contains("2. No redirect"), "got: {out}");
        assert!(out.contains("https://plain.org/"), "got: {out}");
        assert!(out.contains("Second result"), "got: {out}");
    }

    #[test]
    fn decode_entities_named_and_numeric() {
        assert_eq!(decode_entities("a&amp;b&lt;c&gt;d&quot;e&apos;f"), "a&b<c>d\"e'f");
        assert_eq!(decode_entities("&#39;&quot;&#x41;"), "'\"A");
        assert_eq!(decode_entities("no entities here"), "no entities here");
        assert_eq!(decode_entities("bad &amp;"), "bad &");
    }

    #[test]
    fn percent_of_weird() {
        assert_eq!(percent_encode("héllo"), "h%C3%A9llo");
    }
}
