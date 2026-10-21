// sandlang.com: search, code block buttons, and the table of contents.

// ---------- Search ----------

const input = document.getElementById("search");
const list = document.getElementById("search-results");
let index = null;
let selected = -1;

function loadIndex() {
  index ??= fetch("/search-index.json")
    .then((response) => response.json())
    .then((rows) => rows.map(([name, kind, url, summary]) => ({ name, kind, url, summary, key: name.toLowerCase() })))
    .catch((error) => {
      index = null; // Try again at the next search.
      throw error;
    });
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
  list.innerHTML = results.length
    ? results
        .map(
          (r, i) =>
            `<li role="option" id="result-${i}"><a href="${r.url}" tabindex="-1"><span class="kind">${r.kind}</span>` +
            `<span class="name">${escapeHtml(r.name)}</span>` +
            (r.summary ? `<span class="summary">${summaryHtml(r.summary)}</span>` : "") +
            `</a></li>`,
        )
        .join("")
    : `<li class="empty">No results</li>`;
  list.hidden = false;
  input.setAttribute("aria-expanded", "true");
  select(0);
}

function hide() {
  list.hidden = true;
  selected = -1;
  input.setAttribute("aria-expanded", "false");
  input.removeAttribute("aria-activedescendant");
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
      const link = !list.hidden && list.querySelectorAll("[role=option] a")[selected];
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
    try {
      await navigator.clipboard.writeText(figure.querySelector("pre code").textContent);
      button.textContent = "Copied";
    } catch {
      button.textContent = "Copy failed";
    }
    setTimeout(() => (button.textContent = "Copy"), 1500);
  } else if (button.classList.contains("play")) {
    const { encodeShare, newSandbox } = await import("/play/sandbox.js");
    const { program, files, policy } = button.dataset;
    const sandbox = newSandbox({ title: document.title.split(" – ")[0], files: { "main.sand": program, ...JSON.parse(files) }, policy });
    location.href = "/play/#share=" + (await encodeShare([sandbox]));
  }
});

// ---------- Navigation ----------

// On narrow screens the sidebar is a menu above the content, closed at first.
const menu = document.querySelector(".sidebar-menu");
if (menu) {
  const narrow = matchMedia("(max-width: 860px)");
  const fit = () => (menu.open = !narrow.matches);
  fit();
  narrow.addEventListener("change", fit);
}

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
