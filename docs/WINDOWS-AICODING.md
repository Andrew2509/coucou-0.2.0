# Coucou Windows × AICODING — Architecture Audit

Terakhir diperbarui: fase roadmap *Coucou Windows — AICODING Desktop Agent*.

## 1. Tujuan

Coucou Windows diarahkan menjadi *AI desktop agent* dengan **AICODING sebagai
AI provider utama**, Coucou sebagai interface + agent runtime, dan Windows
sebagai execution environment. PRD lengkap mengacu ke dokumen konsep
(AICODING Desktop Agent). Dokumen ini adalah peta arsitektur dari repo yang
sudah ada — apa yang bisa dipakai ulang, apa yang belum ada.

## 2. Layout repo

```
coucou-0.2.0/
  NotchBuddy/            macOS app (Swift) — di luar cakupan Windows
  windows/               Tauri app (Windows + Linux)
    src/                 front end TypeScript (tanpa framework)
      core/              state (state.ts), bridge (bridge.ts), layout, sound
      island/            fsm, hooks handling (hooks.ts), integrations
      mochi/             Mochi + greeting (Canvas 2D)
      views/             semua island view (chat, approval, upload, …)
      settings/          jendela settings
      upload/            animasi file drop
    src-tauri/src/       backend Rust
      claude.rs          AI chat + tool calling + dokumen reader
      hooks.rs           instalasi Claude Code hooks
      pipe.rs            named pipe relay (coucou-hook)
      integrations.rs    poller n8n/GitHub/… 
      files.rs           file drop ingest
      secrets.rs         Credential Manager (Windows) / Keyring (Linux)
      settings.rs        preferensi JSON
      platform/          windows.rs & linux.rs
    hook/                coucou-hook (relay executable)
  docs/                  dokumen (macOS-centric)
```

## 3. AI provider: status saat ini

| Kebutuhan PRD | Status | Lokasi |
|---|---|---|
| Base URL configurable | ✅ | `Settings.api_base` (settings.rs), dikirim ke `claude::send` |
| API key secure | ✅ | `secrets.rs` (Credential Manager), tidak pernah ke frontend |
| Model configurable | ✅ | `Settings.model`, default `sonnet-5` |
| Model list dari endpoint | ✅ | `fetch_models` (claude.rs) | 
| Chat (non-stream) | ✅ | `chat_send` → `claude::send` |
| Tool calling (read/write/list/python/web) | ✅ | `claude.rs` `run_tool` loop + approval card |
| Dokumen (PDF/DOCX/PPTX/XLSX/ODF/RTF) | ✅ | Reader Python `coucou_read.py` |
| **Streaming (SSE incremental)** | ✅ | `call_stream` (Anthropic + OpenAI wire) + `chat-update` events + cancel |
| **Provider abstraction** | ✅ | `provider.rs`: enum + test_connection/list_models per provider |
| **Test Connection** | ✅ | Settings → Claude → Test Connection |
| **Permissions (READ/WRITE/EXECUTE/…)** | ✅ | `tool_permissions` per tool (`ask/allow/deny`) + Settings UI |
| PowerShell / filesystem / Git / automation | ✅ | `execute_powershell`, file tools, `git_*`, screenshot, clipboard, open_application |
| Weekly recap | ✅ | opt-in log → provider summary |

### Catatan penting (keputusan sebelumnya)
- Chat saat ini memakai **format Anthropic `/v1/messages`** (bukan
  OpenAI `/v1/chat/completions`). AICODING menjawab kedua-duanya; kita sudah
  memverifikasi `/v1/messages` + `tool_use`/`tool_result` + `fetch_models`.
- Base URL default `https://partner.api-github.com`, model default `sonnet-5`.
- Dokumen: `%LOCALAPPDATA%\Coucou\bin\coucou_read.py` (stdlib-only).

## 4. Alur request → tool (sekarang)

```
island chat.ts  ──invoke──▶  Rust chat_send
                                 │
                                 ▼
                        claude::send()
                        loop:
                          POST {base}/v1/messages
                             │
                   ◀── no tool_use ──▶ hasil final
                             │ tool_use
                             ▼
                        approve() → pipe::ask_chat_tool (kartu approval di island)
                             │ allow
                             ▼
                        run_tool()  (read/write/list/python/web)
                             │ result
                             ▼  (balik ke loop)
```

Permission: **semua tool menampilkan card Allow/Deny** melalui `pipe::ask_chat_tool`
(payload `coucou_chat: true`, `subject: "Mochi"`), kartu reuse dari Claude Code
approval. Tidak ada pengecualian per-kategori.

## 5. Alur file drop (sekarang)

```
drag ke island → Rust ingest (files.rs) → State.droppedFile
   → prompt view chip → chat_send dengan ChatContext::File
   → claude.rs file_block(base64) / text inline → ke provider
```

## 6. Rekomendasi arsitektur target (fase bertahap)

Fase 1: **Streaming** di `claude.rs` (SSE `/v1/messages?stream=true` atau
`/v1/chat/completions?stream=true`), channel ke frontend, `cancel`.

Fase 2: **Provider abstraction** — bungkus `claude.rs` jadi trait
`AIProvider { chat, stream, list_models, test_connection }`; implementasi
`AICodingProvider` (dan slot Anthropic/OpenAI/Google/Ollama nol). Provider
pilihan masuk Settings.

Fase 3: **Settings** — section AI Providers + Test Connection + Permissions +
Privacy (mengikuti desain settings existing).

Fase 4: **Tool registry + permission levels** — tiap tool dideklarasi dengan
level (READ/WRITE/EXECUTE/DESTRUCTIVE/NETWORK/SYSTEM); permission manager
menentukan card/auto.

Fase 5–7: Filesystem lengkap, `execute_powershell`, Git, coding agent.

Fase 8+: Windows automation (screenshot, clipboard, app control, browser),
weekly recap (opt-in, kolektor lokal + AICODING).

## 7. Keamanan (aturan repo yang dipertahankan)

- API key **hanya** di Credential Manager/Keyring; frontend hanya tahu `present`.
- Tidak ada telemetri; request keluar hanya ke provider yang dikonfigurasi.
- Semua operasi destructive butuh konfirmasi eksplisit.
- Tidak pernah `DELETE`/`commit`/`push` tanpa klik.