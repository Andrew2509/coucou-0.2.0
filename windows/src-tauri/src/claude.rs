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
use tauri::{AppHandle, Manager};

use futures_util::StreamExt;

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

/// One streaming chat update, normalised so the island needs no knowledge of
/// SSE framing. `Text` deltas carry the answer; `Tool` announces a tool that is
/// about to ask permission; `Done` closes the turn with the assembled text.
#[derive(Clone, Debug)]
pub enum ChatEvent {
    Text(String),
    Tool { name: String, preview: String },
    Done(String),
}

/// Receives chat updates as a stream. A closure or a channel work either way,
/// so the same `send` drives both the replay tests and the live IPC.
pub type ChatSink = Box<dyn FnMut(ChatEvent) + Send>;

// ── Endpoint ──────────────────────────────────────────────────────────────────

pub(crate) fn normalise_base(base: &str) -> String {
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

pub(crate) fn first_error_line(text: &str) -> String {
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
        },
        {
            "name": "create_file",
            "description": "Create a NEW file with this exact text content. Refuses to overwrite an existing file — use write_file for that.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "Full path of the file to create." },
                    "content": { "type": "string", "description": "The whole content of the new file." }
                },
                "required": ["file_path", "content"]
            }
        },
        {
            "name": "rename_file",
            "description": "Rename (a file or folder) from one full path to another.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Current full path." },
                    "to": { "type": "string", "description": "New full path." }
                },
                "required": ["path", "to"]
            }
        },
        {
            "name": "move_file",
            "description": "Move a file or folder to another path. Creates the destination folder if needed and refuses to overwrite.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Current full path." },
                    "to": { "type": "string", "description": "Destination full path." }
                },
                "required": ["path", "to"]
            }
        },
        {
            "name": "copy_file",
            "description": "Copy a file to another path. Refuses to overwrite an existing file.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Source full path." },
                    "to": { "type": "string", "description": "Destination full path." }
                },
                "required": ["path", "to"]
            }
        },
        {
            "name": "delete_file",
            "description": "Delete a single file. Cannot be undone — the user always confirms this one.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Full path of the file to delete." }
                },
                "required": ["path"]
            }
        },
        {
            "name": "search_files",
            "description": "Find files whose name contains a pattern, recursively under a folder. Returns up to 100 matches.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "root": { "type": "string", "description": "Folder to search under." },
                    "pattern": { "type": "string", "description": "Substring to match, case-insensitive." }
                },
                "required": ["root", "pattern"]
            }
        },
        {
            "name": "execute_powershell",
            "description": "Run a Windows PowerShell command and return its output. Use it to run the user's project commands, inspect the system, or script anything a terminal can.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The PowerShell command to run." }
                },
                "required": ["command"]
            }
        },
        {
            "name": "git_status",
            "description": "Git status, short with the branch: read-only.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "cwd": { "type": "string", "description": "Repository folder." }
                },
                "required": ["cwd"]
            }
        },
        {
            "name": "git_diff",
            "description": "Uncommitted changes (diff), read-only.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "cwd": { "type": "string", "description": "Repository folder." }
                },
                "required": ["cwd"]
            }
        },
        {
            "name": "git_log",
            "description": "The last 20 commits with their messages, read-only.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "cwd": { "type": "string", "description": "Repository folder." }
                },
                "required": ["cwd"]
            }
        },
        {
            "name": "git_branch",
            "description": "List all branches (local and remote), read-only.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "cwd": { "type": "string", "description": "Repository folder." }
                },
                "required": ["cwd"]
            }
        },
        {
            "name": "git_checkout",
            "description": "Switch to a branch. Changing the working tree — ask first.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "cwd": { "type": "string", "description": "Repository folder." },
                    "branch": { "type": "string", "description": "Branch name." }
                },
                "required": ["cwd", "branch"]
            }
        },
        {
            "name": "git_add",
            "description": "Stage files for the next commit.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "cwd": { "type": "string", "description": "Repository folder." },
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Paths to stage." }
                },
                "required": ["cwd", "paths"]
            }
        },
        {
            "name": "git_commit",
            "description": "Create a commit with the staged changes and this message.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "cwd": { "type": "string", "description": "Repository folder." },
                    "message": { "type": "string", "description": "Commit message." }
                },
                "required": ["cwd", "message"]
            }
        },
        {
            "name": "screenshot",
            "description": "Capture the primary screen to a PNG under the temp folder and return the file path. Use it to inspect what is on the user's display.",
            "input_schema": { "type": "object", "properties": {} }
        },
        {
            "name": "open_application",
            "description": "Open an application, document or web URL as if double-clicked in Explorer.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Command line, file path or URL to open." }
                },
                "required": ["name"]
            }
        },
        {
            "name": "clipboard_read",
            "description": "Read the current clipboard text.",
            "input_schema": { "type": "object", "properties": {} }
        },
        {
            "name": "clipboard_write",
            "description": "Put text on the clipboard.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "content": { "type": "string", "description": "Text to copy." }
                },
                "required": ["content"]
            }
        },
        {
            "name": "spotify_current",
            "description": "What is playing on the user's Spotify right now: track, artists, position, device.",
            "input_schema": { "type": "object", "properties": {} }
        },
        {
            "name": "spotify_search",
            "description": "Search Spotify for tracks by name and return the top 5 with their URIs.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Track or artist to search for." }
                },
                "required": ["query"]
            }
        },
        {
            "name": "spotify_search_playlist",
            "description": "Search the user's Spotify for playlists and return the top 5 with their URIs.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Playlist name to search for." }
                },
                "required": ["query"]
            }
        },
        {
            "name": "spotify_play",
            "description": "Start playback of a Spotify URI (spotify:track:…, spotify:playlist:…, spotify:album:…). Needs an active device.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "uri": { "type": "string", "description": "A Spotify URI from spotify_search." }
                },
                "required": ["uri"]
            }
        },
        {
            "name": "spotify_pause",
            "description": "Pause Spotify.",
            "input_schema": { "type": "object", "properties": {} }
        },
        {
            "name": "spotify_next",
            "description": "Skip to the next track.",
            "input_schema": { "type": "object", "properties": {} }
        },
        {
            "name": "spotify_previous",
            "description": "Go back to the previous track.",
            "input_schema": { "type": "object", "properties": {} }
        },
        {
            "name": "spotify_queue",
            "description": "Read the upcoming queue, or add a URI to it when 'uri' is given.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "uri": { "type": "string", "description": "Optional track URI to enqueue." }
                }
            }
        }
    ])
}

/// One local tool call: ask the human, then run it. The answer is whatever goes
/// back into the conversation as the `tool_result`.
async fn run_tool(app: &AppHandle, name: &str, input: &Value) -> (Value, bool) {
    if !matches!(
        name,
        "read_file" | "list_dir" | "write_file" | "create_file" | "rename_file" | "move_file"
            | "copy_file" | "delete_file" | "search_files" | "run_python" | "execute_powershell"
            | "web_search" | "git_status" | "git_diff" | "git_log" | "git_branch"
            | "git_checkout" | "git_add" | "git_commit" | "screenshot" | "open_application"
            | "clipboard_read" | "clipboard_write" | "spotify_current" | "spotify_search"
            | "spotify_play" | "spotify_pause" | "spotify_next" | "spotify_previous"
            | "spotify_queue" | "spotify_search_playlist"
    ) {
        return (json!(format!("Unknown tool: {name}")), true);
    }
    if !approve(app, name, input).await {
        return (json!("Permission denied by the user."), true);
    }

    let preview = input
        .get("path")
        .or_else(|| input.get("file_path"))
        .or_else(|| input.get("query"))
        .and_then(Value::as_str)
        .map(|s| s.chars().take(60).collect::<String>())
        .unwrap_or_default();
    log_activity(app, name, &preview);

    let outcome = if name.starts_with("spotify_") {
        let name = name.to_string();
        let input = input.clone();
        match dispatch_spotify(&name, &input).await {
            Ok(text) => Ok(text),
            Err(err) => Err(err),
        }
    } else if name == "web_search" {
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

/// Spotify connector tools — every call needs the user's OAuth consent, which
/// the approval card already showed; there is nothing local to run.
async fn dispatch_spotify(name: &str, input: &Value) -> Result<String, String> {
    let str_arg = |field: &str| {
        input
            .get(field)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    crate::spotify::access_token().await?;
    match name {
        "spotify_current" => crate::spotify::current_music().await,
        "spotify_search" => {
            let q = str_arg("query").ok_or_else(|| "No query given.".to_string())?;
            crate::spotify::search_tracks(&q).await
        }
        "spotify_search_playlist" => {
            let q = str_arg("query").ok_or_else(|| "No query given.".to_string())?;
            crate::spotify::search_playlists(&q).await
        }
        "spotify_play" => {
            let uri = str_arg("uri").ok_or_else(|| "No track URI given.".to_string())?;
            crate::spotify::play(&uri).await
        }
        "spotify_pause" => crate::spotify::pause().await,
        "spotify_next" => crate::spotify::next_track().await,
        "spotify_previous" => crate::spotify::previous_track().await,
        "spotify_queue" => crate::spotify::queue(str_arg("uri")).await,
        _ => Err("Unknown Spotify tool.".into()),
    }
}

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
        "create_file" => {
            let path = str_arg("file_path").ok_or_else(|| "No path given.".to_string());
            let content = input.get("content").and_then(Value::as_str).map(str::to_string);
            match (path, content) {
                (Ok(path), Some(content)) => create_file_tool(&path, &content),
                (Err(err), _) => Err(err),
                (_, None) => Err("No content given.".to_string()),
            }
        }
        "rename_file" => paired_paths(input).and_then(|(from, to)| rename_file_tool(&from, &to)),
        "move_file" => paired_paths(input).and_then(|(from, to)| move_file_tool(&from, &to)),
        "copy_file" => paired_paths(input).and_then(|(from, to)| copy_file_tool(&from, &to)),
        "delete_file" => str_arg("path")
            .ok_or_else(|| "No path given.".to_string())
            .and_then(|path| delete_file_tool(&path)),
        "search_files" => {
            let root = str_arg("root").ok_or_else(|| "No root given.".to_string());
            let pattern = str_arg("pattern").ok_or_else(|| "No pattern given.".to_string());
            match (root, pattern) {
                (Ok(root), Ok(pattern)) => search_files_tool(&root, &pattern),
                (Err(e), _) | (_, Err(e)) => Err(e),
            }
        }
        "run_python" => str_arg("command")
            .ok_or_else(|| "No code given.".to_string())
            .and_then(|code| run_python_tool(&code)),
        "execute_powershell" => str_arg("command")
            .ok_or_else(|| "No command given.".to_string())
            .and_then(|cmd| execute_powershell_tool(&cmd)),
        "git_status" => str_arg("cwd").ok_or_else(|| "No repository path given.".to_string())
            .and_then(|cwd| git_status_tool(&cwd)),
        "git_diff" => str_arg("cwd").ok_or_else(|| "No repository path given.".to_string())
            .and_then(|cwd| git_diff_tool(&cwd)),
        "git_log" => str_arg("cwd").ok_or_else(|| "No repository path given.".to_string())
            .and_then(|cwd| git_log_tool(&cwd)),
        "git_branch" => str_arg("cwd").ok_or_else(|| "No repository path given.".to_string())
            .and_then(|cwd| git_branch_tool(&cwd)),
        "git_checkout" => {
            let cwd = str_arg("cwd").ok_or_else(|| "No repository path given.".to_string());
            let branch = str_arg("branch").ok_or_else(|| "No branch given.".to_string());
            match (cwd, branch) {
                (Ok(cwd), Ok(branch)) => git_checkout_tool(&cwd, &branch),
                (Err(e), _) | (_, Err(e)) => Err(e),
            }
        }
        "git_add" => {
            let cwd = str_arg("cwd").ok_or_else(|| "No repository path given.".to_string());
            let paths: Option<Vec<String>> = input
                .get("paths")
                .and_then(Value::as_array)
                .map(|arr| arr.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .filter(|p: &Vec<String>| !p.is_empty())
                .or_else(|| str_arg("path").map(|p| vec![p]));
            match (cwd, paths) {
                (Ok(cwd), Some(paths)) => git_add_tool(&cwd, &paths),
                (Err(e), _) => Err(e),
                (_, None) => Err("No paths given.".to_string()),
            }
        }
        "git_commit" => {
            let cwd = str_arg("cwd").ok_or_else(|| "No repository path given.".to_string());
            let message = str_arg("message").ok_or_else(|| "No commit message given.".to_string());
            match (cwd, message) {
                (Ok(cwd), Ok(message)) => git_commit_tool(&cwd, &message),
                (Err(e), _) | (_, Err(e)) => Err(e),
            }
        }
        "screenshot" => screenshot_tool(),
        "open_application" => str_arg("name")
            .ok_or_else(|| "No application given.".to_string())
            .and_then(|name| open_application_tool(&name)),
        "clipboard_read" => clipboard_read_tool(),
        "clipboard_write" => str_arg("content")
            .ok_or_else(|| "No content given.".to_string())
            .and_then(|content| clipboard_write_tool(&content)),
        _ => Err("Unknown tool.".into()),
    }
}

/// Extracts `path` + `to` from a tool input for two-path operations.
fn paired_paths(input: &Value) -> Result<(String, String), String> {
    let path = input
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "No path given.".to_string())?;
    let to = input
        .get("to")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "No destination given.".to_string())?;
    Ok((path, to))
}

/// The island's approval card, reused verbatim: same view, same Allow/Deny, same
/// pinned island. The payload only says where to go back to afterwards.
///
/// Permissions are checked first: a tool the user set to `allow`/`deny` in
/// Settings never opens a card; everything else (the default) asks.
async fn approve(app: &AppHandle, name: &str, input: &Value) -> bool {
    let override_permission = app
        .state::<crate::Shared>()
        .settings
        .lock()
        .unwrap()
        .tool_permissions
        .get(name)
        .cloned();
    match override_permission.as_deref() {
        Some("allow") => return true,
        Some("deny") => return false,
        _ => {}
    }

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

/// `write_file`, but only creates: refuses to overwrite an existing file, so
/// "create a new file" can never clobber one.
fn create_file_tool(path: &str, content: &str) -> Result<String, String> {
    let file = Path::new(path);
    if file.exists() {
        return Err(format!("{path} already exists — use write_file to overwrite it, or pick another name."));
    }
    if let Some(parent) = file.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {parent:?}: {e}"))?;
        }
    }
    std::fs::write(file, content.as_bytes()).map_err(|e| format!("cannot create {path}: {e}"))?;
    Ok(format!("Created {path}."))
}

fn rename_file_tool(from: &str, to: &str) -> Result<String, String> {
    std::fs::rename(from, to).map_err(|e| format!("cannot rename {from} → {to}: {e}"))?;
    Ok(format!("Renamed {from} → {to}."))
}

fn move_file_tool(from: &str, to: &str) -> Result<String, String> {
    if Path::new(to).exists() {
        return Err(format!("{to} already exists — refusing to overwrite it."));
    }
    if let Some(parent) = Path::new(to).parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {parent:?}: {e}"))?;
    }
    std::fs::rename(from, to).map_err(|e| format!("cannot move {from} → {to}: {e}"))?;
    Ok(format!("Moved {from} → {to}."))
}

fn copy_file_tool(from: &str, to: &str) -> Result<String, String> {
    if Path::new(to).exists() {
        return Err(format!("{to} already exists — refusing to overwrite it."));
    }
    std::fs::copy(from, to).map_err(|e| format!("cannot copy {from} → {to}: {e}"))?;
    Ok(format!("Copied {from} → {to}."))
}

fn delete_file_tool(path: &str) -> Result<String, String> {
    let file = Path::new(path);
    let meta = std::fs::metadata(file).map_err(|e| format!("cannot access {path}: {e}"))?;
    if meta.is_dir() {
        return Err(format!("{path} is a folder — Coucou only deletes files from chat. Use a file manager for folders."));
    }
    std::fs::remove_file(file).map_err(|e| format!("cannot delete {path}: {e}"))?;
    Ok(format!("Deleted {path}."))
}

/// Case-insensitive recursive name search, capped to keep the reply small.
fn search_files_tool(root: &str, pattern: &str) -> Result<String, String> {
    let needle = pattern.to_lowercase();
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::from(root)];
    let mut visited = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            visited += 1;
            if visited > 20_000 {
                out.push("… searched too many entries, stopped early".into());
                break;
            }
            let path = entry.path();
            let name = path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            if name.to_lowercase().contains(&needle) {
                out.push(path.to_string_lossy().to_string());
            }
            if path.is_dir() {
                stack.push(path);
            }
        }
        if visited > 20_000 {
            break;
        }
    }
    if out.is_empty() {
        return Ok(format!("No files matching \"{pattern}\" under {root}."));
    }
    out.truncate(100);
    Ok(out.join("\n"))
}

// ── Weekly recap (opt-in) ─────────────────────────────────────────────────────

/// One line per tool run, appended only when the user turned the recap on. The
/// line holds a timestamp, the tool name and a short preview (a path or query) —
/// no file contents, no command output. The recap summary is written by the AI
/// and shown in the island; the raw log stays on this machine.
fn log_activity(app: &AppHandle, tool: &str, preview: &str) {
    let enabled = app
        .state::<crate::Shared>()
        .settings
        .lock()
        .unwrap()
        .weekly_recap_enabled;
    if !enabled {
        return;
    }
    let path = crate::settings::local_dir().join("activity.jsonl");
    use std::io::Write;
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let entry = serde_json::json!({
            "ts": ts,
            "tool": tool,
            "preview": preview,
        });
        let _ = writeln!(file, "{entry}");
        // Keep the file from growing forever: a little over 12 weeks of daily
        // activity, in practice tens of KB.
        if file.metadata().map(|m| m.len() > 1_000_000).unwrap_or(false) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Rounds the summary of the last 7 days up to the model. Returns the recap
/// text exactly like a chat reply, so the island can show it as a note.
pub async fn weekly_recap(
    app: &AppHandle,
    chat: &Chat,
    model: &str,
    api_base: &str,
) -> Result<String, String> {
    let _ = app;
    let path = crate::settings::local_dir().join("activity.jsonl");
    let body = std::fs::read_to_string(&path).unwrap_or_default();
    let lines: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return Err("No activity collected yet — use the chat a little first, then ask again.".into());
    }
    // Take the last 7 days by scanning the timestamps (they sort by append).
    let week_ago = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
        - 7 * 86400;
    let events: Vec<&str> = lines
        .into_iter()
        .filter(|l| {
            serde_json::from_str::<serde_json::Value>(l)
                .ok()
                .and_then(|v| v.get("ts").and_then(|t| t.as_i64()))
                .map(|t| t >= week_ago)
                .unwrap_or(false)
        })
        .collect();
    if events.is_empty() {
        return Err("No activity in the last 7 days.".into());
    }
    let query = format!(
        "Here is my local activity log for the past week (tool runs only, paths and queries). \
         Write a short, warm weekly recap in plain text with a title, a headline about what I \
         focused on, what got done, and one suggestion for next week. Do not invent anything not \
         in the log.\n\n{}",
        events.join("\n")
    );
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sink: ChatSink = Box::new(|_| {});
    let reply = send(app, chat, model, api_base, query, None, cancel, sink).await?;
    Ok(reply.text)
}

/// Runs a PowerShell command without a console window, merging stderr into the
/// output. The command is exactly what the approval card previewed.
fn execute_powershell_tool(command: &str) -> Result<String, String> {
    // On Linux there is no PowerShell by default; pwsh may exist.
    #[cfg(windows)]
    let program = PathBuf::from("powershell.exe");
    #[cfg(not(windows))]
    let program = match crate::platform::find_on_path("pwsh") {
        Some(p) => p,
        None => return Err("PowerShell (pwsh) is not installed on this system.".into()),
    };
    let args: Vec<String> = vec![
        "-NoProfile".into(),
        "-NonInteractive".into(),
        "-Command".into(),
        command.to_string(),
    ];
    // run_process_cmd applies CREATE_NO_WINDOW on Windows via platform::no_console.
    let (text, code) = run_process(&program, &args, TOOL_TIMEOUT)?;
    let text = text.trim().to_string();
    if text.is_empty() {
        if code == 0 {
            return Ok("(the command produced no output)".into());
        }
        return Err(format!("the command exited with code {code} and no output"));
    }
Ok(text)
    }

// ── Windows automation ────────────────────────────────────────────────────────

/// Captures the whole primary screen to a PNG under %TEMP%\coucou_screenshots\
/// and returns the file path and dimensions. The image itself is a file the
/// model can open elsewhere; feeding pixels straight into the conversation
/// needs a richer request builder than tool_result supports today.
fn screenshot_tool() -> Result<String, String> {
    let dir = std::env::temp_dir().join("coucou_screenshots");
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {dir:?}: {e}"))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let dest = dir.join(format!("shot-{stamp}.png"));
    let dest_str = dest.display().to_string();
    let script = format!(
        "
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$bounds = [System.Windows.Forms.SystemInformation]::VirtualScreen
$bmp = New-Object System.Drawing.Bitmap $bounds.Width, $bounds.Height
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
$bmp.Save('{dest_str}', [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose(); $bmp.Dispose()
Write-Output $bounds.Width x $bounds.Height
"
    );
    let (text, code) = run_process(
        &PathBuf::from("powershell.exe"),
        &["-NoProfile".into(), "-NonInteractive".into(), "-Command".into(), script],
        Duration::from_secs(15),
    )?;
    if code != 0 || !dest.exists() {
        return Err(format!("screenshot failed: {}", text.trim()));
    }
    Ok(format!("Saved screenshot to {dest_str} ({})", text.trim()))
}

/// Launches an application by its command or URL. The command is what the card
/// previewed; it is spawned as-is through `cmd /c start`, so shell shortcuts
/// and `explorer` URLs work, but arbitrary executables run the same way.
fn open_application_tool(app_spec: &str) -> Result<String, String> {
    if app_spec.trim().is_empty() {
        return Err("No application given.".into());
    }
    let mut cmd = std::process::Command::new("cmd");
    cmd.args(["/c", "start", "", app_spec]);
    let (text, code) = run_process_cmd_no_pipe(&mut cmd, Duration::from_secs(10))?;
    if code != 0 {
        return Err(format!("cannot start \"{app_spec}\": {}", text.trim()));
    }
    Ok(format!("Started {app_spec}."))
}

/// Reads the clipboard text via PowerShell.
fn clipboard_read_tool() -> Result<String, String> {
    let (text, code) = run_process(
        &PathBuf::from("powershell.exe"),
        &["-NoProfile".into(), "-NonInteractive".into(), "-Command".into(), "Get-Clipboard -Raw".into()],
        TOOL_TIMEOUT,
    )?;
    let _ = code;
    Ok(text)
}

/// Writes the clipboard text via PowerShell.
fn clipboard_write_tool(content: &str) -> Result<String, String> {
    // Base64-encode to keep the script a single safe line regardless of content.
    let b64 = base64(content.as_bytes());
    let script = format!(
        "[System.Text.Encoding]::UTF8.GetString([System.Convert]::FromBase64String('{b64}')) | Set-Clipboard"
    );
    let (_, code) = run_process(
        &PathBuf::from("powershell.exe"),
        &["-NoProfile".into(), "-NonInteractive".into(), "-Command".into(), script],
        TOOL_TIMEOUT,
    )?;
    if code != 0 {
        return Err("Set-Clipboard failed.".into());
    }
    Ok("Copied to the clipboard.".into())
}

/// `cmd /c start` does not wait; use a detached spawn that reports success.
fn run_process_cmd_no_pipe(cmd: &mut std::process::Command, _timeout: Duration) -> Result<(String, i32), String> {
    let mut child = crate::platform::no_console(cmd)
        .spawn()
        .map_err(|e| format!("cannot start: {e}"))?;
    let status = child.wait().map_err(|e| format!("cannot wait: {e}"))?;
    Ok((String::new(), status.code().unwrap_or(-1)))
}

// ── Git ───────────────────────────────────────────────────────────────────────

/// Runs `git` in `cwd`, merging stderr into the output. Read tools reuse this;
/// `git_add`/`git_checkout`/`git_commit` ask permission like any other tool.
fn run_git(cwd: &str, args: &[String], timeout: Duration) -> Result<String, String> {
    let mut command = std::process::Command::new("git");
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (text, code) = run_process_cmd(&mut command, timeout)?;
    let text = text.trim().to_string();
    if text.is_empty() {
        if code == 0 {
            return Ok("(no output)".into());
        }
        return Err(format!("git exited with code {code} and no output"));
    }
    Ok(text)
}

fn git_status_tool(cwd: &str) -> Result<String, String> {
    run_git(cwd, &["status".into(), "--short".into(), "--branch".into()], TOOL_TIMEOUT)
}

fn git_diff_tool(cwd: &str) -> Result<String, String> {
    run_git(cwd, &["diff".into()], TOOL_TIMEOUT)
}

fn git_log_tool(cwd: &str) -> Result<String, String> {
    run_git(
        cwd,
        &[
            "log".into(),
            "--oneline".into(),
            "-20".into(),
            "--decorate".into(),
        ],
        TOOL_TIMEOUT,
    )
}

fn git_branch_tool(cwd: &str) -> Result<String, String> {
    run_git(cwd, &["branch".into(), "-a".into()], TOOL_TIMEOUT)
}

fn git_checkout_tool(cwd: &str, branch: &str) -> Result<String, String> {
    run_git(cwd, &["checkout".into(), branch.into()], TOOL_TIMEOUT)
}

fn git_add_tool(cwd: &str, paths: &[String]) -> Result<String, String> {
    let mut args = vec!["add".into()];
    args.extend_from_slice(paths);
    run_git(cwd, &args, TOOL_TIMEOUT)
}

fn git_commit_tool(cwd: &str, message: &str) -> Result<String, String> {
    run_git(
        cwd,
        &["commit".into(), "-m".into(), message.into()],
        TOOL_TIMEOUT,
    )
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
    run_process_cmd(&mut command, timeout)
}

/// Spawns `command` (already configured) and joins stdout+stderr. `no_console`
/// keeps the window off on Windows; a caller may add extra flags (e.g. CREATE_NO_WINDOW)
/// before calling.
fn run_process_cmd(command: &mut std::process::Command, timeout: Duration) -> Result<(String, i32), String> {
    let mut child = crate::platform::no_console(command)
        .spawn()
        .map_err(|e| format!("cannot start the process: {e}"))?;

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
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    mut sink: ChatSink,
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
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            chat.truncate(mark);
            return Err("Stopped.".into());
        }
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

        let response = match call_stream(
            &base,
            &key,
            &body,
            native,
            cancel.clone(),
            &mut sink,
        )
        .await
        {
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
            // Tell the island which tool is about to ask for permission.
            let preview = input
                .get("path")
                .or_else(|| input.get("file_path"))
                .or_else(|| input.get("query"))
                .and_then(Value::as_str)
                .map(|s| s.chars().take(60).collect::<String>())
                .unwrap_or_default();
            sink(ChatEvent::Tool { name: name.to_string(), preview: preview.clone() });
            log_activity(app, name, &preview);
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
    // The front end closes on this event; the invoke still resolves with the
    // final string for the (stream-less) paths that call chat_send directly.
    sink(ChatEvent::Done(text.clone()));
    Ok(ChatReply { text })
}

/// Sends the same payload with `stream: true` and feeds the answer to `sink`
/// as it arrives, token by token. Returns the full response object (content
/// blocks included) so the caller can pick out tool_use blocks exactly like it
/// would from a non-streaming call.

/// Sends the same payload with `stream: true` and feeds the answer to `sink`
/// as it arrives, token by token. Returns the full response object (content
/// blocks included) so the caller can pick out tool_use blocks exactly like it
/// would from a non-streaming call.
///
/// Two wire formats are understood, because the endpoint decides:
///   * Anthropic-style SSE: `event:` / `data:` lines with
///     `content_block_(start|delta|stop)`;
///   * OpenAI-style SSE: `data: {"choices":[{"delta":{"content":…}}]}`.
/// Tool-call blocks (Anthropic `tool_use`, OpenAI `tool_calls`) are accumulated
/// into the returned `content` so the loop above can run them.
async fn call_stream(
    base: &str,
    key: &str,
    body: &Value,
    native: bool,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    sink: &mut ChatSink,
) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| e.to_string())?;

    let (header_name, header_value) = auth_header(base, key);
    let mut full = body.clone();
    full["stream"] = json!(true);
    full["stream_options"] = json!({ "include_usage": false });
    let mut request = client
        .post(messages_url(base))
        .header(header_name, header_value)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header("content-type", "application/json")
        .json(&full);
    if native {
        request = request.header("anthropic-beta", FALLBACK_BETA);
    }

    let response = request.send().await.map_err(|e| format!("Network error: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.map_err(|e| e.to_string())?;
        return Err(format!("Claude API {status}: {}", first_error_line(&text)));
    }

    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    // Final assistant content blocks (text + tool_use), rebuilt from the stream.
    let mut blocks: Vec<Value> = Vec::new();
    // Streaming text, joined for a pure-text answer.
    let mut streamed_text = String::new();
    let mut stop_reason = "end_turn".to_string();

    // Anthropic: index → in-progress tool block (id/name/input).
    // OpenAI: accumulated arguments string per tool call index.
    let mut tool_blocks: std::collections::HashMap<usize, Value> = Default::default();
    let mut tool_inputs: std::collections::HashMap<usize, String> = Default::default();

    while let Some(chunk) = stream.next().await {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("Stopped.".into());
        }
        let bytes = chunk.map_err(|e| format!("stream read error: {e}"))?;
        buffer.push_str(&String::from_utf8_lossy(&bytes));
        while let Some(nl) = buffer.find('\n') {
            let line = buffer[..nl].to_string();
            buffer.drain(..=nl);

            let data = line.strip_prefix("data: ").map(str::trim);
            let Some(data) = data else { continue };
            if data == "[DONE]" {
                break;
            }
            let Ok(ev) = serde_json::from_str::<Value>(data) else { continue };

            // ── OpenAI-style choice delta ───────────────────────────────────
            if let Some(content) = ev.pointer("/choices/0/delta/content").and_then(Value::as_str) {
                if !content.is_empty() {
                    sink(ChatEvent::Text(content.to_string()));
                    streamed_text.push_str(content);
                }
            }
            if let Some(calls) = ev.pointer("/choices/0/delta/tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    if let Some(id) = call.pointer("/id").and_then(Value::as_str) {
                        tool_blocks.insert(
                            index,
                            json!({ "type": "tool_use", "id": id, "name": "", "input": {} }),
                        );
                    }
                    if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                        if let Some(b) = tool_blocks.get_mut(&index) {
                            b["name"] = json!(name);
                        }
                    }
                    if let Some(arg) = call.pointer("/function/arguments").and_then(Value::as_str) {
                        tool_inputs.entry(index).or_default().push_str(arg);
                    }
                }
            }

            // ── Anthropic-style events ───────────────────────────────────────
            let etype = ev.get("type").and_then(Value::as_str).unwrap_or("");
            match etype {
                "content_block_start" => {
                    let index = ev.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    let block = ev.get("content_block").cloned().unwrap_or_default();
                    if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                        tool_blocks.insert(index, block);
                    } else if block.get("type").and_then(Value::as_str) == Some("text") {
                        if let Some(t) = block.get("text").and_then(Value::as_str) {
                            if !t.is_empty() {
                                sink(ChatEvent::Text(t.to_string()));
                                streamed_text.push_str(t);
                            }
                        }
                    }
                }
                "content_block_delta" => {
                    let index = ev.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    let delta = ev.get("delta").cloned().unwrap_or_default();
                    if delta.get("type").and_then(Value::as_str) == Some("text_delta") {
                        if let Some(t) = delta.get("text").and_then(Value::as_str) {
                            if !t.is_empty() {
                                sink(ChatEvent::Text(t.to_string()));
                                streamed_text.push_str(t);
                            }
                        }
                    } else if delta.get("type").and_then(Value::as_str) == Some("input_json_delta") {
                        if let Some(p) = delta.get("partial_json").and_then(Value::as_str) {
                            tool_inputs.entry(index).or_default().push_str(p);
                        }
                    }
                }
                "content_block_stop" => {
                    // Tool blocks are moved into `blocks` here, complete.
                    let index = ev.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    if let Some(mut block) = tool_blocks.remove(&index) {
                        if let Some(raw) = tool_inputs.remove(&index) {
                            // Merge the accumulated partial JSON into input, if parseable.
                            if let Ok(parsed) = serde_json::from_str::<Value>(&raw) {
                                block["input"] = parsed;
                            } else {
                                block["input"] = json!(raw);
                            }
                        }
                        blocks.push(block);
                    }
                }
                "message_delta" => {
                    if let Some(sr) = ev.pointer("/delta/stop_reason").and_then(Value::as_str) {
                        stop_reason = sr.to_string();
                    }
                }
                _ => {}
            }
        }
    }

    // If text was streamed but never flushed as a block (pure-text stream),
    // make a text block so the caller sees the final content.
    if !streamed_text.is_empty() && blocks.is_empty() {
        blocks.push(json!({ "type": "text", "text": streamed_text }));
    }

    Ok(json!({ "stop_reason": stop_reason, "content": blocks }))
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

    #[test]
    fn filesystem_tools_roundtrip() {
        let dir = std::env::temp_dir().join(format!("coucou-fs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        let fpath = file.to_str().unwrap();

        write_file_tool(fpath, "hello").unwrap();
        assert_eq!(read_file_tool(fpath).unwrap().replace("\n", "").trim(), "hello");
        // create_file refuses to overwrite.
        assert!(create_file_tool(fpath, "again").is_err());
        let fresh = dir.join("b.txt");
        create_file_tool(fresh.to_str().unwrap(), "new").unwrap();
        // rename + move + copy.
        let renamed = dir.join("c.txt");
        rename_file_tool(fresh.to_str().unwrap(), renamed.to_str().unwrap()).unwrap();
        let moved = dir.join("sub").join("d.txt");
        move_file_tool(fpath, moved.to_str().unwrap()).unwrap();
        assert!(moved.exists());
        let copy = dir.join("e.txt");
        copy_file_tool(moved.to_str().unwrap(), copy.to_str().unwrap()).unwrap();
        assert!(copy.exists());
        // search finds them.
        let hits = search_files_tool(dir.to_str().unwrap(), "e.txt").unwrap();
        assert!(hits.contains("e.txt"), "got: {hits}");
        // delete only files.
        assert!(delete_file_tool(dir.to_str().unwrap()).is_err());
        delete_file_tool(copy.to_str().unwrap()).unwrap();
        assert!(!copy.exists());

        // write_file is the only one that refuses nothing (overwrites).
        write_file_tool(fpath, "again").unwrap();
        delete_file_tool(fpath).unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }
}
