// Voice / Meeting views.
//
// Voice:     tap to record, tap again to stop → the clip is transcribed by the
//            configured Whisper endpoint and shown as text to copy or send to
//            AICODING.
// Meeting:   same capture, with a real recording bar and an honest notice that
//            system (loopback) audio from other apps is a later phase — for now
//            this records the microphone with a big visible indicator.

import { h } from "./dom";
import { Bridge } from "../core/bridge";
import { Sound } from "../core/sound";
import { tl } from "../i18n/i18n";
import type { IslandViewName } from "../core/layout";
import type { ViewHost, ViewActions } from "./views";

function recDot(on: boolean): HTMLElement {
  const dot = h("i", { class: "rec-dot" });
  dot.classList.toggle("on", on);
  return dot;
}

function mmss(ms: number): string {
  const s = Math.floor(ms / 1000);
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}

function buildRecorder(sub: string, actions: ViewActions, mode: "voice" | "meeting"): ViewHost {
  const timer = h("span", { class: "timer", text: "0:00" });
  const stateLine = h("div", { class: "sub" });
  const transcript = h("div", { class: "voice-text" });
  const errLine = h("div", { class: "note err" });
  const btn = h("button", { class: "btn primary", text: "Start", onclick: toggle });
  const copyBtn = h("button", { class: "btn secondary", text: tl("Copy"), onclick: copy });
  const askBtn = h("button", { class: "btn secondary", text: "Send to chat" });
  const otherMode: IslandViewName = mode === "voice" ? "meeting" : "voice";
  const otherLabel = mode === "voice" ? "Meeting mode" : "Voice mode";
  const modeBtn = h("button", {
    class: "btn secondary",
    text: otherLabel,
    onclick: () => {
      actions.blip();
      actions.setView(otherMode);
    },
  });
  const dot = recDot(false);
  const statusRow = h(
    "div",
    { class: "row", style: "align-items:center;gap:8px" },
    dot,
    timer,
    stateLine,
  );

  let recording = false;
  let recordedMs = 0;
  let timerId = 0;
  let lastTranscript = "";

  const buttons = h("div", { class: "row", style: "gap:8px" }, btn, copyBtn, askBtn, modeBtn);
  const el = h(
    "div",
    { class: "view" },
    h(
      "div",
      { class: "card wash voice-card" },
      h(
        "div",
        { class: "stack", style: "padding:10px 18px 10px 108px" },
        h("div", { class: "title", text: tl("Voice") }),
        statusRow,
        transcript,
        errLine,
        buttons,
        h("div", { class: "hint", text: sub }),
      ),
    ),
  );

  async function toggle() {
    if (recording) {
      await stop(false);
      return;
    }
    errLine.textContent = "";
    transcript.textContent = "Listening…";
    try {
      await Bridge.voiceStart();
    } catch (err) {
      errLine.textContent = String(err).replace(/^Error:\s*/, "");
      return;
    }
    recording = true;
    recordedMs = 0;
    dot.classList.add("on");
    btn.textContent = "Stop";
    Sound.play("send");
    timerId = window.setInterval(() => {
      recordedMs += 1000;
      timer.textContent = mmss(recordedMs);
    }, 1000);
  }

  async function stop(cancel: boolean) {
    recording = false;
    dot.classList.remove("on");
    btn.textContent = "Start";
    window.clearInterval(timerId);
    if (cancel) {
      await Bridge.voiceCancel().catch(() => {});
      transcript.textContent = "Cancelled.";
      timer.textContent = "0:00";
      return;
    }
    stateLine.textContent = "Transcribing…";
    try {
      const text = await Bridge.voiceStop();
      lastTranscript = text;
      transcript.textContent = text;
      transcript.style.whiteSpace = "pre-wrap";
      stateLine.textContent = "";
      timer.textContent = mmss(recordedMs);
      Sound.play("finish");
    } catch (err) {
      errLine.textContent = String(err).replace(/^Error:\s*/, "");
      stateLine.textContent = "";
    }
  }

  function copy() {
    if (!lastTranscript) return;
    navigator.clipboard?.writeText(lastTranscript).catch(() => {});
    Sound.play("finish");
  }

  askBtn.addEventListener("click", () => {
    if (!lastTranscript) return;
    // No dedicated channel from voice to chat yet: copy + point at the chat.
    navigator.clipboard?.writeText(lastTranscript).catch(() => {});
    stateLine.textContent = "Copied — paste it into the chat.";
    Sound.play("finish");
  });

  return {
    el,
    sync() { /* stateless */ },
  };
}

export function buildVoice(actions: ViewActions): ViewHost {
  return buildRecorder(
    "Tap to record; Coucou transcribes it and shows the text. You stay in control of what gets sent.",
    actions,
    "voice",
  );
}

export function buildMeeting(actions: ViewActions): ViewHost {
  return buildRecorder(
    "Recording shows a clear indicator the whole time. Capturing other apps' system audio is a later phase — this records your microphone, nothing hidden.",
    actions,
    "meeting",
  );
}