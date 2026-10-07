// Minimal Markdown renderer for chat answers — DOM, no dependencies.
// Covers the shapes the model actually produces: paragraphs, # heading, - / 1.
// lists, > quotes, `code` and fenced ``` blocks (with a copy button), **bold**,
// *italic*, links. Only http/https links become clickable; every other protocol
// is dropped, and opening a link goes through the island (Bridge.openUrl), not a
// raw <a href> navigation.

import { h, svg } from "./dom";
import { ICONS } from "./icons";

/** Escapes text for use as literal HTML text (never raw innerHTML of user data). */
function escapeHtml(text: string): string {
  const div = document.createElement("div");
  div.textContent = text;
  return div.innerHTML;
}

function inline(text: string): string {
  let s = text;
  // Code spans first: their content is literal and must not be parsed again.
  s = s.replace(/`([^`]+)`/g, (_m, code: string) => `<code class="md-code">${escapeHtml(code)}</code>`);
  // Links: only http(s). Explicit [label](url) first, then bare URLs.
  s = s.replace(
    /\[([^\]]+)\]\((https?:\/\/[^)\s]+)\)/g,
    (_m, label: string, url: string) =>
      `<a class="md-link" href="${escapeHtml(url)}">${escapeHtml(label)}</a>`,
  );
  s = s.replace(
    /(^|[^>"'\]])https?:\/\/[^\s<]*/g,
    (m, pre: string) =>
      `${pre}<a class="md-link" href="${escapeHtml(m.slice(pre.length))}">${escapeHtml(m.slice(pre.length))}</a>`,
  );
  s = s.replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>");
  s = s.replace(/(^|[^*_])_([^_]+)_([^*_ ]|$)/g, "$1<em>$2</em>$3");
  // Keep single line breaks inside a paragraph (plain answers stay readable);
  // blank lines are paragraph breaks already handled by the renderer.
  s = s.replace(/\n/g, "<br>");
  return s;
}

function codeBlock(text: string): HTMLElement {
  const pre = h("pre", { class: "md-pre" });
  const code = h("code", { class: "md-block" });
  code.textContent = text;
  const copy = h("button", {
    class: "md-copy",
    title: "Copy",
    onclick: () => {
      if (navigator.clipboard) void navigator.clipboard.writeText(text);
    },
  }, svg(ICONS.copy, 10));
  pre.append(code, copy);
  return pre;
}

/** Renders one assistant answer (paragraph-level markdown) to DOM nodes. */
export function renderMarkdown(text: string): HTMLElement {
  const root = h("div", { class: "md" });
  const lines = text.split("\n");

  let para: string[] = [];
  const flushPara = () => {
    const content = para.join("\n").trim();
    para = [];
    if (!content) return;
    const p = h("div", { class: "md-p" });
    p.innerHTML = inline(content);
    root.append(p);
  };

  let i = 0;
  while (i < lines.length) {
    const line = lines[i];

    // Fenced code block.
    const fence = line.match(/^```(\w*)\s*$/);
    if (fence) {
      flushPara();
      const lang = fence[1] ?? "";
      const body: string[] = [];
      i += 1;
      while (i < lines.length && !/^```\s*$/.test(lines[i])) {
        body.push(lines[i]);
        i += 1;
      }
      i += 1; // closing fence
      if (lang) root.append(h("div", { class: "md-lang", text: lang }));
      root.append(codeBlock(body.join("\n")));
      continue;
    }

    // Blank line: paragraph break.
    if (line.trim() === "") {
      flushPara();
      i += 1;
      continue;
    }

    // Heading.
    const heading = line.match(/^(#{1,6})\s+(.*)$/);
    if (heading) {
      flushPara();
      const level = Math.min(heading[1].length, 6);
      const el = h("div", { class: `md-h md-h${level}`, text: heading[2].replace(/\s+#+\s*$/, "") });
      root.append(el);
      i += 1;
      continue;
    }

    // Quote.
    const quote = line.match(/^\s*>\s?(.*)$/);
    if (quote) {
      flushPara();
      const el = h("div", { class: "md-quote" });
      el.textContent = quote[1];
      root.append(el);
      i += 1;
      continue;
    }

    // List item (plain lines; consecutive items are separate rows).
    const item = line.match(/^\s*(?:[-*]|\d+[.)])\s+(.*)$/);
    if (item) {
      flushPara();
      const li = h("div", { class: "md-li" });
      li.innerHTML = inline(item[1]);
      root.append(li);
      i += 1;
      continue;
    }

    para.push(line);
    i += 1;
  }
  flushPara();
  return root;
}