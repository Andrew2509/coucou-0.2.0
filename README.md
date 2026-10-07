<div align="center">
  <img src="NotchBuddy/Assets.xcassets/AppIcon.appiconset/icon_256x256.png" width="120" alt="Coucou icon">
  
  # Coucou
  
  **Your AI Coding Companion, Living in Your Notch (or Taskbar)**
  
  A tiny, interactive friend that keeps an eye on your AI coding agent sessions. Manage permissions, monitor agent progress, drag-and-drop files, and chat with AI models—all without breaking your flow. Now available for macOS and Windows!

  [![Version](https://img.shields.io/github/v/release/Andrew2509/coucou-0.2.0?filter=v*&label=version&color=0A84FF)](https://github.com/Andrew2509/coucou-0.2.0/releases)

  **[📥 Releases (Hanya aplikasi desktop saja)](https://github.com/Andrew2509/coucou-0.2.0/releases)**
  ![macOS 15+](https://img.shields.io/badge/macOS-15%2B-black?logo=apple)
  ![Windows](https://img.shields.io/badge/Windows-10%2F11-0078D4?logo=windows&logoColor=white)
  ![License](https://img.shields.io/badge/license-MIT-green)

  <br />
  <img src="docs/media/demo.gif" width="760" style="border-radius:12px; box-shadow: 0 4px 12px rgba(0,0,0,0.1);" alt="Coucou in action">
</div>

---

## 🌟 Meet Mochi

Say hello to **Mochi**—a soft little squircle with big eyes that pops out of your screen's notch (or top edge). Mochi waves hello, follows your cursor, reacts to your clicks, and alerts you the moment your AI coding agents need your attention.

Coucou is fully open-source. Every line of code, animation, and sound is free to read, learn from, fork, and remix.

## 🚀 Key Features

### 🤖 Seamless AI Agent Integration
- **Live Agent Tracking:** Supports Claude Code, Cursor, Codex, Gemini CLI, Antigravity, Copilot CLI, Muse Code, OpenCode, and Amp. Watch what your agent reads, edits, and runs step by step.
- **In-Notch Approvals:** Easily **Allow**, **Deny**, or **Always Allow** permission requests. Respond to agent prompts directly from the notch!
- **Live Diffs:** See exact file modifications (file name, +N −M counts) directly in the ticker. Tap to view the full diff.

### 💬 Chat & Interactions
- **Multi-Model Chat:** Converse with Claude, Gemini, OpenAI, or local models (Ollama/LM Studio). Switch providers seamlessly.
- **Drag & Drop:** Drop files directly onto Mochi to ask questions about them or send them via email.
- **Window Attachment:** Drag Mochi onto any window to use it as context for Claude (macOS).

### 🎨 Personalization & Experience
- **Desktop Mode:** Drag Mochi out of the notch to roam your desktop.
- **Custom Outfits:** Right-click Mochi to open the wardrobe, or let him dress up automatically for the seasons.
- **Weekly Recap:** Get a Monday morning summary of your coding time, active sessions, lines changed, and top projects.

## 📥 Installation

### macOS
The easiest way is to download the pre-compiled app:
1. Download `Coucou.zip` from [Releases](https://github.com/Andrew2509/coucou-0.2.0/releases).
2. Unzip and drag **Coucou.app** to your `/Applications` folder.
3. Open it! (If macOS prompts you, confirm the opening).

*(Note: Mac App Store release coming soon!)*

### Windows
*Note: The Windows installer is temporarily unavailable due to a false-positive Microsoft Defender flag. You can build it from source in the meantime. See [`windows/README.md`](windows/README.md) for details.*

## ⚙️ Configuration & Setup

Access **Settings** by clicking the Coucou icon in your menu bar (macOS) or system tray (Windows).

| Configuration | Description |
|---|---|
| **Agent Hooks** | Install hooks for Claude Code, Gemini CLI, or Antigravity via Settings to enable live tracking. |
| **API Keys** | Enter keys for Anthropic, Google AI, or OpenAI to unlock chat. Safely stored in your OS Keychain. |
| **Local Models** | Connect to Ollama or LM Studio servers in Settings → Chat → Local models. |
| **Integrations** | Link Stripe, GitHub, Vercel, Notion, etc., to get dedicated monitoring pills. |

## 🛠️ Build from Source

### macOS (macOS 15+, Xcode 16+)
```bash
brew install xcodegen
git clone https://github.com/Andrew2509/coucou-0.2.0.git
cd coucou/NotchBuddy
xcodegen
open NotchBuddy.xcodeproj
```

### Windows (Rust, Node 20+)
```bash
# Requires Rust, Node 20+, and necessary OS build tools
git clone https://github.com/Andrew2509/coucou-0.2.0.git
cd coucou/windows
npm install
npm run pack
```

## 🤝 Contributing & License

We love contributions! Whether it's a new translation, bug fix, or integration, check out [CONTRIBUTING.md](CONTRIBUTING.md).

- **Code License:** [MIT License](LICENSE)
- **Brand Assets:** Name, Mochi character, sounds, and media are © Louis Raillé. (See [LICENSE-ASSETS.md](LICENSE-ASSETS.md)).

<div align="center">
  <br />
  <strong>Built by <a href="https://louisraille.fr">Louis Raillé</a> with Claude Code.</strong><br>
  If Mochi made you smile, please consider giving us a ⭐!
  <br /><br />
  <a href="https://louis-cfm.github.io/coucou/">Website</a> · <a href="https://louis-cfm.github.io/coucou/privacy.html">Privacy</a> · <a href="https://louis-cfm.github.io/coucou/terms.html">Terms</a> · <a href="https://louis-cfm.github.io/coucou/support.html">Support</a>
</div>
