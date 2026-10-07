// Chat view — DOM port of PromptView / ChatBubble / TypingDotsView from
// IslandViewContent.swift.

import { h, svg, clear } from "./dom";
import { ICONS } from "./icons";
import { Bridge, onChatUpdate, type ChatContext } from "../core/bridge";
import { Sound } from "../core/sound";
import { State, type ChatMessage } from "../core/state";
import { renderMarkdown } from "./markdown";
import type { ViewHost } from "./views";

let nextId = 1;

function bubble(message: ChatMessage): HTMLElement {
  if (message.role === "user") {
    return h(
      "div",
      { class: "chat-row user" },
      h("div", { class: "bubble", text: message.content }),
    );
  }
  // Assistant answers render as markdown: paragraphs, lists, code blocks with a
  // copy button. Only http/https links are clickable, and they open through the
  // island rather than navigating the webview away.
  const reply = h("div", { class: "chat-row md-row" }, renderMarkdown(message.content));
  reply.addEventListener("click", (e) => {
    const target = e.target as HTMLElement;
    const link = target.closest("a.md-link") as HTMLAnchorElement | null;
    if (link && /^https?:\/\//.test(link.href)) {
      e.preventDefault();
      void Bridge.openUrl(link.href);
    }
  });
  return reply;
}

function typingDots(): HTMLElement {
  return h(
    "div",
    { class: "chat-row" },
    h("div", { class: "typing" }, h("i"), h("i"), h("i")),
  );
}

/** The coloured chip showing what the question is about (a dropped file). */
function contextChip(label: string): HTMLElement {
  const chip = h("div", { class: "chip" }, h("i", { class: "chip-dot" }), h("span", { text: label }));
  requestAnimationFrame(() => chip.classList.add("settled"));
  return chip;
}

export function buildPrompt(onHeightChange: () => void): ViewHost {
  const chipRow = h("div", { class: "chip-row" });
  const log = h("div", { class: "chat-log" });
  const input = h("input", {
    type: "text",
    class: "chat-input",
    placeholder: "Ask me anything…",
    spellcheck: "false",
  }) as HTMLInputElement;
  const send = h("button", { class: "send-btn", title: "Send" }, svg(ICONS.arrowUp, 11));
  const bar = h("div", { class: "chat-bar" }, input, send);

  const el = h(
    "div",
    { class: "view" },
    h("div", { class: "card wash chat-card" }, h("div", { class: "chat-body" }, chipRow, log, bar)),
  );
  (el.querySelector(".card") as HTMLElement).style.setProperty("--wash", "rgba(99,102,241,0.5)");

  let sending = false;
  let renderedCount = -1;

  async function submit() {
    const query = input.value.trim();
    if (!query || sending) return;
    input.value = "";
    sending = true;
    Sound.play("send");

    State.chatHistory.push({ id: nextId++, role: "user", content: query });
    State.stateOverride = "thinking";
    State.notify();
    onHeightChange();

    // The assistant answer streams into this bubble as `chat-update` events.
    const pending: ChatMessage = { id: nextId++, role: "assistant", content: "" };
    const pendingIndex = State.chatHistory.length;
    State.chatHistory.push(pending);
    State.notify();

    let unlisten = () => {};
    try {
      unlisten = await onChatUpdate((u) => {
        if (u.kind === "text" && u.text) {
          pending.content += u.text;
          State.stateOverride = null;
          State.notify();
          onHeightChange();
        } else if (u.kind === "tool") {
          // A tool is running behind an approval card; Mochi shows "working".
          State.stateOverride = "working";
          State.notify();
        } else if (u.kind === "done" && u.text) {
          pending.content = u.text;
        }
      });

      const file = State.droppedFile;
      const context: ChatContext | null =
        State.chatHistory.length === 2 && file
          ? { kind: "file", name: file.name, path: file.path }
          : null;

      const reply = await Bridge.chatSend(query, context);
      // The stream already filled the bubble; trust the final text from Rust.
      if (reply.text) pending.content = reply.text;
      State.stateOverride = null;
      Sound.play("finish");
    } catch (err) {
      unlisten();
      // Remove the empty (or partial) assistant bubble the stream owned.
      State.chatHistory.splice(pendingIndex, 1);
      State.stateOverride = null;
      State.noteMessage = String(err).replace(/^Error:\s*/, "");
      State.view = "note";
      Sound.play("error");
    } finally {
      unlisten();
      sending = false;
      State.notify();
      onHeightChange();
      input.focus();
    }
  }

  send.addEventListener("click", () => {
    if (sending) {
      void Bridge.chatCancel();
      return;
    }
    void submit();
  });
  input.addEventListener("keydown", (e) => {
    if ((e as KeyboardEvent).key === "Enter") {
      e.preventDefault();
      void submit();
    }
    e.stopPropagation(); // Escape closes the island, not the chat
  });

  return {
    el,
    sync() {
      const file = State.droppedFile;
      const wantChip = file?.name ?? "";
      if (chipRow.dataset.label !== wantChip) {
        chipRow.dataset.label = wantChip;
        clear(chipRow);
        if (wantChip) chipRow.append(contextChip(wantChip));
      }

      const thinking = State.stateOverride === "thinking";
      const count = State.chatHistory.length + (thinking ? 0.5 : 0);
      if (count !== renderedCount) {
        renderedCount = count;
        clear(log);
        for (const m of State.chatHistory) log.append(bubble(m));
        if (thinking) log.append(typingDots());
        log.scrollTop = log.scrollHeight;
      }

      input.placeholder = State.chatHistory.length === 0 ? "Ask me anything…" : "Continue…";
      input.disabled = sending;
    },
    focus() {
      input.focus();
      input.select();
    },
  };
}
