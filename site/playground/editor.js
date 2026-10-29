// Syntax-highlighted code editor built over a <textarea> with line numbers and diagnostics.

const escapes = { "&": "&amp;", "<": "&lt;", ">": "&gt;" };
const escape = (s) => s.replace(/[&<>]/g, (c) => escapes[c]);

const MAX_HIGHLIGHT = 256 * 1024;

export class CodeEditor {
  constructor(parent, options = {}) {
    this.options = options;
    this.diagnostics = [];
    this.root = document.createElement("div");
    this.root.className = "editor";
    this.root.innerHTML = `
      <div class="editor-content">
        <div class="gutter" aria-hidden="true"></div>
        <div class="code">
          <pre class="layer highlight" aria-hidden="true"></pre>
          <pre class="layer marks" aria-hidden="true"></pre>
          <textarea spellcheck="false" autocapitalize="off" autocomplete="off" wrap="off"></textarea>
        </div>
      </div>`;
    parent.append(this.root);
    if (options.gutter === false) this.root.classList.add("no-gutter");
    this.gutter = this.root.querySelector(".gutter");
    this.highlightLayer = this.root.querySelector(".highlight");
    this.marksLayer = this.root.querySelector(".marks");
    this.textarea = this.root.querySelector("textarea");
    if (options.label) this.textarea.setAttribute("aria-label", options.label);
    if (options.placeholder) this.textarea.placeholder = options.placeholder;
    this.textarea.value = options.value ?? "";
    this.textarea.addEventListener("input", () => {
      this.render();
      this.options.onChange?.(this.value);
    });
    this.textarea.addEventListener("keydown", (e) => this.keydown(e));
    this.textarea.addEventListener("mousemove", (e) => this.hover(e));
    this.textarea.addEventListener("mouseleave", () => hideTooltip());
    this.gutter.addEventListener("mouseover", (e) => {
      const line = Number(e.target.dataset?.line);
      const found = this.diagnostics.filter((d) => d.line === line);
      if (found.length) showTooltip(e.target.getBoundingClientRect(), found);
    });
    this.gutter.addEventListener("mouseout", () => hideTooltip());
    for (const event of ["keyup", "click", "focus"]) this.textarea.addEventListener(event, () => this.caretMoved());
    this.render();
  }

  get value() {
    return this.textarea.value;
  }

  set value(text) {
    if (text === this.textarea.value) return;
    this.textarea.value = text;
    this.render();
  }

  replaceAll(text) {
    if (text === this.textarea.value) return;
    const caret = this.textarea.selectionStart;
    this.textarea.focus();
    this.textarea.select();
    if (!document.execCommand("insertText", false, text)) {
      this.textarea.value = text;
      this.textarea.dispatchEvent(new Event("input"));
    }
    this.textarea.setSelectionRange(Math.min(caret, text.length), Math.min(caret, text.length));
  }

  focus() {
    this.textarea.focus();
  }

  caretOnEdgeLine(last) {
    const { selectionStart: at, value } = this.textarea;
    return last ? !value.slice(at).includes("\n") : !value.slice(0, at).includes("\n");
  }

  setDiagnostics(diagnostics) {
    this.diagnostics = diagnostics ?? [];
    this.render();
  }

  setHighlighter(highlight) {
    this.options.highlight = highlight;
    this.render();
  }

  render() {
    const text = this.textarea.value;
    const shown = text.endsWith("\n") || text === "" ? text + " " : text;
    let ranges = [];
    if (this.options.highlight && text.length <= MAX_HIGHLIGHT) {
      try {
        ranges = this.options.highlight(text);
      } catch {
        ranges = [];
      }
    }
    this.highlightLayer.innerHTML = spans(shown, ranges);
    this.marks = this.markRanges(text);
    this.marksLayer.innerHTML = spans(shown, this.marks);
    const lines = text.split("\n").length;
    const marks = new Map();
    for (const d of this.diagnostics) {
      if (d.line < 1) continue;
      const prev = marks.get(d.line);
      if (!prev || (prev.severity !== "error" && d.severity === "error")) marks.set(d.line, d);
    }
    let gutter = "";
    for (let i = 1; i <= lines; i++) {
      const d = marks.get(i);
      gutter += d ? `<div class="mark-${d.severity}" data-line="${i}">${i}</div>` : `<div>${i}</div>`;
    }
    this.gutter.innerHTML = gutter;
    this.caretMoved();
  }

  caretMoved() {
    const line = this.textarea.value.slice(0, this.textarea.selectionStart).split("\n").length;
    const found = this.diagnostics.filter((d) => d.line === line);
    const key = found.map((d) => d.message).join("\n");
    if (key === this.caretKey) return;
    this.caretKey = key;
    this.options.onCaretDiagnostics?.(found);
  }

  hover(e) {
    if (!this.marks?.length) return hideTooltip();
    const style = getComputedStyle(this.textarea);
    const lineHeight = parseFloat(style.lineHeight);
    this.charWidth ??= measureChar(style.font);
    const row = Math.floor((e.offsetY - parseFloat(style.paddingTop)) / lineHeight);
    const col = Math.floor((e.offsetX - parseFloat(style.paddingLeft)) / this.charWidth);
    const lines = this.textarea.value.split("\n");
    if (row < 0 || row >= lines.length || col < 0) return hideTooltip();
    const chars = Array.from(lines[row]);
    let offset = lines.slice(0, row).reduce((n, l) => n + l.length + 1, 0) + chars.slice(0, col).join("").length;
    if (col >= chars.length) offset = Math.max(0, offset - 1);
    const mark = this.marks.find(([start, end]) => offset >= start && offset < end);
    if (!mark) return hideTooltip();
    showTooltip({ left: e.clientX, top: e.clientY - lineHeight / 2, bottom: e.clientY + lineHeight / 2 }, [mark[3]]);
  }

  markRanges(text) {
    if (!this.diagnostics.length) return [];
    const starts = [0];
    for (let i = 0; i < text.length; i++) if (text[i] === "\n") starts.push(i + 1);
    const out = [];
    for (const d of this.diagnostics) {
      if (d.line < 1 || d.line > starts.length) continue;
      const lineStart = starts[d.line - 1];
      const lineEnd = d.line < starts.length ? starts[d.line] - 1 : text.length;
      const line = Array.from(text.slice(lineStart, lineEnd));
      const col = Math.max(0, Math.min((d.column || 1) - 1, line.length));
      let start = lineStart + line.slice(0, col).join("").length;
      let end = start + line.slice(col, col + Math.max(1, d.width || 1)).join("").length;
      if (end <= start) {
        start = Math.max(lineStart, start - 1);
        end = Math.max(end, lineEnd, start + 1);
      }
      out.push([start, Math.min(end, Math.max(lineEnd, start + 1)), `mark-${d.severity}`, d]);
    }
    out.sort((a, b) => a[0] - b[0]);
    const merged = [];
    for (const r of out) if (!merged.length || r[0] >= merged[merged.length - 1][1]) merged.push(r);
    return merged;
  }

  keydown(e) {
    if (this.options.onKeyDown?.(e)) {
      e.preventDefault();
      return;
    }
    const ta = this.textarea;
    const mod = e.metaKey || e.ctrlKey;
    if (mod && e.key === "Enter") {
      e.preventDefault();
      this.options.onRun?.();
      return;
    }
    if (mod && (e.key === "s" || e.key === "S")) {
      e.preventDefault();
      this.options.onFormat?.();
      return;
    }
    if (e.key === "Tab" && !mod && !e.altKey) {
      if (this.escaped) return;
      e.preventDefault();
      this.indent(e.shiftKey);
      return;
    }
    this.escaped = e.key === "Escape";
    if (e.key === "Enter" && !mod && !e.altKey && !e.shiftKey) {
      e.preventDefault();
      const { selectionStart: start, value } = ta;
      const lineStart = value.lastIndexOf("\n", start - 1) + 1;
      const indent = value.slice(lineStart, start).match(/^[ \t]*/)[0];
      const before = value.slice(lineStart, start).trimEnd();
      const opens = /[{[(]$/.test(before);
      const after = value.slice(ta.selectionEnd).match(/^[ \t]*([}\])])?/);
      let insert = "\n" + indent + (opens ? "    " : "");
      if (opens && after?.[1]) {
        this.insert(insert + "\n" + indent);
        ta.setSelectionRange(start + insert.length, start + insert.length);
        return;
      }
      this.insert(insert);
      return;
    }
    if ((e.key === "}" || e.key === "]" || e.key === ")") && !mod) {
      const { selectionStart: start, selectionEnd: end, value } = ta;
      const lineStart = value.lastIndexOf("\n", start - 1) + 1;
      const typed = value.slice(lineStart, start);
      if (start === end && /^ {4,}$/.test(typed)) {
        e.preventDefault();
        ta.setSelectionRange(start - 4, start);
        this.insert(e.key);
      }
    }
  }

  insert(text) {
    if (!document.execCommand("insertText", false, text)) {
      this.textarea.setRangeText(text, this.textarea.selectionStart, this.textarea.selectionEnd, "end");
      this.textarea.dispatchEvent(new Event("input"));
    }
  }

  indent(dedent) {
    const ta = this.textarea;
    const { selectionStart: start, selectionEnd: end, value } = ta;
    const multiline = value.slice(start, end).includes("\n");
    if (!dedent && !multiline) {
      const col = start - (value.lastIndexOf("\n", start - 1) + 1);
      this.insert(" ".repeat(4 - (col % 4)));
      return;
    }
    const lineStart = value.lastIndexOf("\n", start - 1) + 1;
    const lineEnd = end > start && value[end - 1] === "\n" ? end - 1 : end;
    const block = value.slice(lineStart, lineEnd);
    const lines = block.split("\n");
    const changed = lines.map((l) => (dedent ? l.replace(/^ {1,4}/, "") : l === "" ? l : "    " + l)).join("\n");
    ta.setSelectionRange(lineStart, lineEnd);
    this.insert(changed);
    ta.setSelectionRange(lineStart, lineStart + changed.length);
  }
}

let tooltip = null;

function showTooltip(rect, diagnostics) {
  if (!tooltip) {
    tooltip = document.createElement("div");
    tooltip.className = "editor-tooltip";
    tooltip.setAttribute("role", "tooltip");
    document.body.append(tooltip);
  }
  tooltip.replaceChildren(
    ...diagnostics.map((d) => {
      const line = document.createElement("div");
      line.className = `tip-${d.severity}`;
      line.textContent = `${d.severity === "error" ? "error" : "warning"}: ${d.message}`;
      return line;
    }),
  );
  tooltip.hidden = false;
  const below = rect.bottom + 6;
  const top = below + tooltip.offsetHeight > innerHeight ? rect.top - tooltip.offsetHeight - 6 : below;
  tooltip.style.top = `${Math.max(4, top)}px`;
  tooltip.style.left = `${Math.max(4, Math.min(rect.left, innerWidth - tooltip.offsetWidth - 4))}px`;
}

function hideTooltip() {
  if (tooltip) tooltip.hidden = true;
}

function measureChar(font) {
  const context = document.createElement("canvas").getContext("2d");
  context.font = font;
  return context.measureText("M".repeat(100)).width / 100;
}

function spans(text, ranges) {
  let html = "";
  let at = 0;
  for (const [start, end, cls] of ranges) {
    if (start < at || end > text.length || start >= end) continue;
    html += escape(text.slice(at, start)) + `<span class="${cls}">${escape(text.slice(start, end))}</span>`;
    at = end;
  }
  return html + escape(text.slice(at));
}

export function highlightPolicy(text) {
  const out = [];
  let base = 0;
  for (const line of text.split("\n")) {
    const words = /"(?:[^"\\]|\\.)*"?|#.*|[^\s,]+/g;
    let w;
    let index = 0;
    let first = "";
    while ((w = words.exec(line))) {
      const [start, word] = [base + w.index, w[0]];
      const end = start + word.length;
      if (word.startsWith("#")) out.push([start, end, "comment"]);
      else if (index === 0) out.push([start, end, /^(allow|deny|prompt|limit|let)$/.test(word) ? "keyword" : "invalid"]);
      else if (index === 1 && first !== "let" && !/^[/.~]/.test(word)) out.push([start, end, "type"]);
      else if (word.startsWith('"')) out.push([start, end, "string"]);
      else if (/^\d[\d_.]*([kKmMgG]|ns|us|ms|s|m|h)?$/.test(word)) out.push([start, end, "number"]);
      else if (/^[A-Z][A-Z0-9_-]*$/.test(word)) out.push([start, end, "constant"]);
      else for (const r of word.matchAll(/\{[^}]*\}/g)) out.push([start + r.index, start + r.index + r[0].length, "interpolation"]);
      if (index === 0) first = word;
      index++;
    }
    base += line.length + 1;
  }
  return out;
}
