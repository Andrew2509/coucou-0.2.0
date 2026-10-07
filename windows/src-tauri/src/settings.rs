// Preferences, stored as plain JSON in settings.json under platform::config_dir().
// No secret ever lands here — API keys live in the OS keychain (see secrets.rs).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub sound_enabled: bool,
    pub sound_volume: f64,
    pub auto_close_interval: f64,
    pub absence_interval: f64,
    pub active_integrations: Vec<String>,
    /// "primary" = the main display, "cursor" = whichever display the mouse is on.
    pub screen: String,
    pub autostart: bool,
    pub hooks_installed: bool,
    /// Claude model used by the chat. Changeable in the settings window.
    /// Defaulted explicitly so a settings.json written by an older build still loads.
    #[serde(default = "default_model")]
    pub model: String,
    /// Base URL of the Anthropic-format endpoint the chat talks to. `…/v1/messages`
    /// is appended to it. AICoding by default; `https://api.anthropic.com` still works.
    #[serde(default = "default_api_base")]
    pub api_base: String,
    /// Which provider slot is active: `aicoding` (default) | anthropic | openai |
    /// google | ollama. AICODING and Anthropic share the `/v1/messages` format;
    /// the rest are OpenAI-compatible.
    #[serde(default = "default_provider")]
    pub provider: String,
    /// Pill shown by default (the one the island opens on): which agent keeps its
    /// own slot. `integration_claude` is the built-in VS Code pill; other values
    /// name a `--agent <name>` hook pill such as `agent_cursor`.
    #[serde(default = "default_main_agent")]
    pub main_agent: String,
    /// Per-tool permission override. Keys are tool names, values are
    /// `"ask" | "allow" | "deny"`. Missing keys mean `"ask"` (the approval
    /// card). Never stores keys — only these labels.
    #[serde(default)]
    pub tool_permissions: std::collections::HashMap<String, String>,
    /// Privacy-adjacent features, all opt-in. Weekly recap is off until the
    /// user turns it on and stays local when off.
    #[serde(default)]
    pub weekly_recap_enabled: bool,
}

fn default_model() -> String {
    crate::claude::DEFAULT_MODEL.to_string()
}

fn default_api_base() -> String {
    crate::claude::DEFAULT_API_BASE.to_string()
}

fn default_provider() -> String {
    crate::provider::DEFAULT_PROVIDER.to_string()
}

fn default_main_agent() -> String {
    "integration_claude".into()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            sound_enabled: true,
            sound_volume: 0.12,
            auto_close_interval: 15.0,
            absence_interval: 180.0,
            active_integrations: vec![
                "integration_resend".into(),
                "integration_n8n".into(),
                "integration_vercel".into(),
                "integration_github".into(),
            ],
            screen: "primary".into(),
            autostart: false,
            hooks_installed: false,
            model: default_model(),
            api_base: default_api_base(),
            provider: default_provider(),
            main_agent: default_main_agent(),
            tool_permissions: std::collections::HashMap::new(),
            weekly_recap_enabled: false,
        }
    }
}

pub use crate::platform::{config_dir, local_dir};

pub fn hook_exe_path() -> PathBuf {
    local_dir().join("bin").join(crate::platform::HOOK_EXE)
}

fn settings_path() -> PathBuf {
    config_dir().join("settings.json")
}

pub fn load() -> Settings {
    match std::fs::read(settings_path()) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => Settings::default(),
    }
}

pub fn save(settings: &Settings) -> std::io::Result<()> {
    let dir = config_dir();
    crate::platform::ensure_private_dir(&dir)?;
    let json = serde_json::to_vec_pretty(settings)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(settings_path(), json)
}
