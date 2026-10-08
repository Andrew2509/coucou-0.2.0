# Coucou Windows × AICODING — Architecture

Last updated: Phase 2 (Spotify chat tools) completed.

## 1. Purpose

Coucou Windows is an AI desktop agent interface. **AICODING** (`https://partner.api-github.com`) is the default chat provider over Anthropic's `/v1/messages` wire. The Tauri backend (`windows/src-tauri/src/`) runs a tool loop with **explicit approval before every local tool call** (reads included). The frontend (`windows/src/`) shows an approval card on the island and lets the user Allow/Deny per request.

## 2. Layout

```text
windows/
  src/                    TypeScript (no framework)
    core/providers.ts     provider registry (includes "aicoding")
    island/agents.ts      APPROVAL_AGENTS + CHAT_AGENT = "aicoding"
    island/hooks.ts       approval labels/fields + step labels
    i18n/                 strings.json (MAC) + extra.json (Windows-only)
    settings/main.ts      Settings UI (Chat providers, Spotify, etc.)
  src-tauri/src/
    chat.rs               provider routing + model selection
    claude.rs             Anthropic/AICODING wires, tools_for, TOOLS_NOTE, loop
    chat_tools.rs          local tool registry (10 tools), approval display
    spotify.rs            Spotify OAuth PKCE + Web API (current/search/play/pause/next/prev/queue)
    lib.rs                Tauri commands, approval intercepts, chat_send
    hooks.rs              Claude Code hook installer/logic
    pipe.rs               named pipe relay (coucou-hook)
    secrets.rs            Credential Manager (Windows) / Keyring (Linux)
    settings.rs           persisted settings JSON
    i18n.rs               translations (Rust)
    platform/             windows.rs / linux.rs
docs/WINDOWS-AICODING.md  (this document)
```

## 3. Providers

| Slot | Wire | Tools | Web search |
|---|---|---|---|
| `aicoding` (default) | Anthropic `/v1/messages` to `https://partner.api-github.com` | **Yes** (local tools via approval card) | **No** (omitted on AICODING wire) |
| `anthropic` (Claude) | Anthropic `/v1/messages` | **Yes** (same local tools) | **Yes** (Anthropic wire only) |
| `openai`/`google`/`openrouter` | OpenAI-compatible | No | No |
| `ollama`/`lmstudio` + custom OpenAI-compatible | OpenAI-compatible (streamed) | No | No |

Defaults: `chatProvider = "aicoding"`, `model = "sonnet-5"` (AICODING), Claude uses `claude-opus-5`/`sonnet-5` as configured. Models fetched per provider when selected (with key present).

## 4. Local chat tools (`chat_tools.rs`)

**10 tools** are exposed to AICODING and Claude. Every call requires approval via the island card (reads included). `spotify_ready()` returns the translated error `"Not connected to Spotify — connect it in Settings."` when Spotify is not connected.

| Tool | Required | Optional | Purpose |
|---|---|---|---|
| `read_file` | `file_path` | — | Read a file |
| `write_file` | `file_path`, `content` | — | Write/overwrite a file |
| `list_dir` | `path` | — | List directory contents |
| `run_powershell` | `command` | — | Run PowerShell 5.1 command |
| `run_python` | `code` | — | Run Python code |
| `spotify_now` | — | — | Get current playback (title/artist/track/playlist context) |
| `spotify_search` | `query` | `type` = `"track"`\|`"playlist"` | Search Spotify |
| `spotify_play` | — | `uri` | Play by `uri`, or **resume** if `uri` is empty |
| `spotify_pause` | — | — | Pause playback |
| `spotify_next` | — | — | Next track |
| `spotify_previous` | — | — | Previous track |
| `spotify_queue` | — | `uri` | Add `uri` to queue |

`execute()` handles all 10 arms before the fallback; `display_input()` builds a concise one-line summary for the approval card (uses `uri`/`query`/`file_path`/`path`/`command`/`code`). The approval registry carries `pillId = "agent_aicoding"` for chat tool requests.

## 5. Tool loop & wires (`claude.rs`)

- `Wire::Aicoding`: base `https://partner.api-github.com`, auth `Bearer <aic-…>`, omits `"web_search"` tool and omits `"fallbacks"` field (server-side web_search unusable).
- `Wire::Anthropic`: base `https://api.anthropic.com`, auth `x-api-key`, includes `"web_search"` tool (at index 0) and `"fallbacks"` when applicable.
- `tools_for(wire)`: returns `chat_tools::defs()` unioned with Anthropic-only `web_search` (inserted at index 0). No filtering — new tools flow to both wires automatically.
- `TOOLS_NOTE` (appended to system prompt): lists all 10 tools + note about Spotify connector. No test asserts its exact content.
- `send_with(app, wire, chat, model, query, context)`: posts `/v1/messages`, loops while response contains `tool_use`:
  - approve via `pipe::ask_chat_tool` (creates island approval card) with `session_id = "chat"`, `coucou_agent = "aicoding"`, `request_id = "chat-<pid>-<n>"`
  - on Allow → `chat_tools::execute(tool, input, tx)` returns `tool_result`
  - on Deny/timeout → result omitted (request not sent back as executed) and loop continues/ends per policy
- Max tool rounds: **8** (`MAX_ROUNDS = 8`).
- File context: dropped files become `ChatContext::File { name, path }` → sent as text (`File: <name>\n\n<query>`) or as `file_block` for PDFs/images when supported by the wire/path handling.

## 6. Approval flow (island/frontend)

- `CHAT_AGENT = "aicoding"` (`windows/src/island/agents.ts`), `APPROVAL_AGENTS` includes `"codex","copilot","muse"` (relay/pure values; chat tools use `agent_aicoding` pill id).
- `TOOL_LABELS` maps tool names (including `spotify_*`) to translated labels (`N_("*")`).
- `stepLabel()` builds card title: `search` → `"Searches · <query>"`, `play` with `uri` → `"Plays · <uri>"` (truncated), `queue` with `uri` → `"Queues · <uri>"`, bare → bare label.
- `APPROVAL_FIELDS` includes `"uri"` (after `"url"`).
- Auto-decline: different `pending.requestId`, paused island, or unknown agent.
- Timeouts: frontend drops card at **110 s**; Rust backstop `APPROVAL_WAIT` **120 s**.
- Approval requests use ids `chat-<pid>-<n>`, `session_id: "chat"`, `coucou_agent: "aicoding"`. Denying a chat tool request (`pending.pillId === "agent_aicoding"`) triggers `dropPendingCard` decline path.

## 7. Spotify connector

- OAuth **PKCE-S256**, loopback `http://127.0.0.1:8000`, state-guarded, CSRF protection.
- Tokens stored in Credential Manager (`secrets.rs`). `connected()` checks token validity/expiry.
- API fns: `current_music`, `search_tracks`, `search_playlists`, `play(uri)` — empty `uri` = **resume** (uses `None` body), `pause`, `next_track`, `previous_track`, `queue(uri)`.
- Chat tools call these directly after `spotify_ready()` gate.
- Settings: **Settings → Spotify** (separate section). Error strings reference "connect it in Settings." (not "Settings → Connectors").

## 8. Translations (i18n)

- `strings.json`: MAC catalog (`_generated`, `languages`, `strings: {key:{lang:str}}`) — generated by `scripts/gen-strings.mjs`, never hand-edit.
- `extra.json`: Windows/Linux-only keys (`strings` with 9 non-English languages, no `en`), 1-space indent. Must not shadow MAC keys; every key used in code; placeholders equal per language.
- Rust: `t(key)` (single arg), `tf(key, &[("name", val)])` for placeholders. Frontend: `t/tl/N_/tn`.
- `tests/i18n.test.mjs`: "every string translated" checks `usedKeys` (Rust `t(`/`tf(` in `src-tauri/`, excludes `#[cfg(test)] mod tests` via comment stripping); flags raw translated keys as literals in CHECKED dirs (`src/views`, `src/settings`, `src/island`, `src/upload`, `src/recap`, `src/mochi/wardrobe.ts`, `src/main.ts`), exempting `t(`/`N_(` calls. `NOT_TEXT = {file, finished, unknown, Resend}`.
- New Spotify labels: `Now playing`, `Plays`, `Next track`, `Previous track`, `Queues`, `Not connected to Spotify — connect it in Settings.` (6 keys × 9). `Searches` and `Pause` reused.

## 9. Streaming & non-tool chat

- Local models (`ollama`/`lmstudio`/OpenAI-compatible): answers streamed via SSE (`stream: true`), `chat-delta` events emitted to island (`WINDOW_LABEL`), `<think>` blocks hidden.
- OpenAI/Google/OpenRouter: handled by `openai_compat.rs` (non-streaming paths where applicable; streaming exists for local/OpenAI-compatible). AICODING/Claude use Anthropic `/v1/messages` (non-streaming tool loop; streaming for plain answers is provider-dependent as implemented).
- Switching providers mid-conversation carries history as plain text only.

## 10. Security & rules

- Secrets only in Credential Manager/Keyring (`secrets.rs`, `secrets::KNOWN_KEYS`). Never written to disk/git.
- No telemetry. Network calls only to configured services.
- Every destructive/local action requires explicit Allow via island card.
- Never commit without asking (repo has many uncommitted files). No fake features; no restyling of shipped views.
- Pill IDs stable. `APPROVAL_AGENTS`/`CHAT_AGENT` are contract values.
- tokio has no `macros` feature (tests use manual runtime builder). Tests set `HOME_VAR`/`USERPROFILE` explicitly when cwd-sensitive.
- PowerShell 5.1 quirks noted (use `cmd /c "… 2>&1"`; avoid complex inline `node -e` quoting — write temp scripts; `findstr` not `find /i`; `rg` may be absent — use grep).

## 11. Test status (as of Phase 2)

- `cargo test --lib` → **210 passed, 0 failed** (warnings cleared)
- `npx tsc --noEmit` → clean
- `npm test` → **318 passed, 0 failed**

Key additions: 2 new `chat_tools.rs` tests (`the_spotify_tools_are_offered_with_their_fields`, `spotify_calls_carry_what_the_card_shows`), 2 new `hooks.test.mjs` tests (Spotify labels + card naming).

## 12. Quick verification

- Chat → ask AICODING to `read_file` a small text file: approval card appears with label+fields, Allow runs, result returned, Stop pill appears. Deny omits result.
- Spotify not connected: `spotify_now` returns translated "Not connected to Spotify — connect it in Settings."
- Spotify connected: `spotify_search {"query":"daft punk","type":"track"}` shows card `Searches · daft punk`, Allow queues/plays via backend. `spotify_play {}` resumes.
- Approval timeout (110 s frontend / 120 s Rust) auto-declines.
