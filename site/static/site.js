// sandlang.com: search, code block buttons, and the table of contents.

// ---------- Search ----------

const input = document.getElementById("search");
const list = document.getElementById("search-results");
let index = null;
let selected = -1;

async function loadIndex() {
  if (!index) {
    const rows = await (await fetch("/search-index.json")).json();
    index = rows.map(([name, kind, url, summary]) => ({ name, kind, url, summary, key: name.toLowerCase() }));
  }
  return index;
}

/** Lower is better; `null` if `entry` does not match every word of `query`. */
function rank(entry, words) {
  let score = 0;
  for (const word of words) {
    const at = entry.key.indexOf(word);
    if (at >= 0) {
      const last = entry.key.split(/[.› ]/).pop();
      score += last === word ? 0 : last.startsWith(word) ? 1 : entry.key.startsWith(word) ? 2 : 3 + at / 100;
    } else if (entry.summary.toLowerCase().includes(word)) {
      score += 8;
    } else {
      return null;
    }
  }
  // Prefer guide pages and short names among equals.
  return score + (entry.kind === "guide" ? -0.5 : 0) + entry.key.length / 1000;
}

function escapeHtml(text) {
  return text.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);
}

function summaryHtml(text) {
  return escapeHtml(text).replace(/`([^`]+)`/g, "<code>$1</code>");
}

function show(results) {
  selected = results.length ? 0 : -1;
  list.innerHTML = results.length
    ? results
        .map(
          (r, i) =>
            `<li role="option" id="result-${i}" aria-selected="${i === 0}"><a href="${r.url}"><span class="kind">${r.kind}</span>` +
            `<span class="name">${escapeHtml(r.name)}</span>` +
            (r.summary ? `<span class="summary">${summaryHtml(r.summary)}</span>` : "") +
            `</a></li>`,
        )
        .join("")
    : `<li class="empty">No results</li>`;
  list.hidden = false;
  input.setAttribute("aria-expanded", "true");
}

function hide() {
  list.hidden = true;
  input.setAttribute("aria-expanded", "false");
}

async function search() {
  const words = input.value.toLowerCase().split(/\s+/).filter(Boolean);
  if (!words.length) return hide();
  const entries = await loadIndex();
  const results = entries
    .map((e) => ({ e, score: rank(e, words) }))
    .filter((r) => r.score !== null)
    .sort((a, b) => a.score - b.score)
    .slice(0, 12)
    .map((r) => r.e);
  show(results);
}

function select(i) {
  const items = list.querySelectorAll("[role=option]");
  if (!items.length) return;
  selected = (i + items.length) % items.length;
  items.forEach((item, k) => item.setAttribute("aria-selected", String(k === selected)));
  items[selected].scrollIntoView({ block: "nearest" });
  input.setAttribute("aria-activedescendant", items[selected].id);
}

if (input) {
  input.addEventListener("focus", loadIndex, { once: true });
  input.addEventListener("input", search);
  input.addEventListener("keydown", (event) => {
    if (event.key === "ArrowDown") select(selected + 1);
    else if (event.key === "ArrowUp") select(selected - 1);
    else if (event.key === "Enter") {
      const link = list.querySelectorAll("[role=option] a")[selected];
      if (link) location.href = link.href;
    } else if (event.key === "Escape") {
      input.value = "";
      hide();
      input.blur();
    } else return;
    event.preventDefault();
  });
  document.addEventListener("click", (event) => {
    if (!event.target.closest(".search")) hide();
  });
  document.addEventListener("keydown", (event) => {
    const typing = event.target.closest("input, textarea, [contenteditable]");
    if (event.key === "/" && !typing && !event.metaKey && !event.ctrlKey) {
      event.preventDefault();
      input.focus();
    }
  });
}

// ---------- Code blocks ----------

document.addEventListener("click", async (event) => {
  const button = event.target.closest(".code-actions button");
  if (!button) return;
  const figure = button.closest("figure");
  if (button.classList.contains("copy")) {
    await navigator.clipboard.writeText(figure.querySelector("pre code").textContent);
    button.textContent = "Copied";
    setTimeout(() => (button.textContent = "Copy"), 1500);
  } else if (button.classList.contains("play")) {
    const { encodeShare, newSandbox } = await import("/play/sandbox.js");
    const files = { "main.sand": button.dataset.program, ...JSON.parse(button.dataset.files || "{}") };
    const sandbox = newSandbox({ title: document.title.split(" – ")[0], files });
    location.href = "/play/#share=" + (await encodeShare([sandbox]));
  }
});

// ---------- Navigation ----------

// On narrow screens the sidebar starts closed, above the content.
const menu = document.querySelector(".sidebar-menu");
if (menu && matchMedia("(max-width: 860px)").matches) menu.open = false;

// "On this page" marks the section at the top of the viewport.
const tocLinks = [...document.querySelectorAll(".toc a")];
if (tocLinks.length) {
  const targets = tocLinks.map((a) => document.getElementById(decodeURIComponent(a.hash.slice(1)))).filter(Boolean);
  const mark = () => {
    const top = innerHeight * 0.25;
    let current = targets[0];
    for (const t of targets) if (t.getBoundingClientRect().top < top) current = t;
    for (const a of tocLinks) a.classList.toggle("active", a.hash.slice(1) === current?.id);
  };
  addEventListener("scroll", mark, { passive: true });
  mark();
}
