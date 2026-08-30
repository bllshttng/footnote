// Executes the board's own JavaScript against a minimal DOM, so a behavioral
// regression fails a test instead of surviving a substring assertion.
//
// Review of PR 1208 found four JS defects the string-matching tests could not
// see: group headers counting the unfiltered set, an empty project selection
// blanking the board, positional bar colours, and a stale persisted selection.
// All four are behavior. This runs the real code and reads the real result.
//
// Reads the rendered document on argv[2], writes a JSON verdict to stdout.

import { readFileSync } from "node:fs";

const doc = readFileSync(process.argv[2], "utf8");

/* ---------- the smallest DOM that the board actually uses ---------- */

class ClassList {
  constructor(el) { this.el = el; }
  get _set() { return new Set(String(this.el.className || "").split(/\s+/).filter(Boolean)); }
  _write(s) { this.el.className = [...s].join(" "); }
  add(c) { const s = this._set; s.add(c); this._write(s); }
  remove(c) { const s = this._set; s.delete(c); this._write(s); }
  contains(c) { return this._set.has(c); }
}

class El {
  constructor(tag) {
    this.tagName = String(tag).toUpperCase();
    this.children = [];
    this.parentNode = null;
    this.dataset = {};
    this.attrs = {};
    this.style = {};
    this.className = "";
    this.textContent = "";
    this._html = "";
    this.hidden = false;
    this._id = "";
    this._listeners = {};
    this.classList = new ClassList(this);
  }
  // innerHTML is BOTH stored and parsed. Storing only the string left every
  // element the board builds this way invisible to querySelector, so a handler
  // bound through it could never be reached by a test: the guard would pass
  // while asserting nothing. The parser covers the subset this board emits
  // (open/close tags, attributes, text) and is deliberately not a real HTML
  // parser; it has no entity decoding and no void-element table beyond the
  // self-closing form.
  set innerHTML(v) {
    this._html = String(v);
    this.children = [];
    if (!v) return;
    const stack = [this];
    const token = /<\/?([a-zA-Z][\w-]*)([^>]*?)(\/?)>|([^<]+)/g;
    let m;
    while ((m = token.exec(this._html))) {
      const [raw, tag, attrs, selfClose, text] = m;
      const parent = stack[stack.length - 1];
      if (text !== undefined) {
        if (text.trim()) parent.textContent += text;
        continue;
      }
      if (raw[1] === "/") {
        if (stack.length > 1) stack.pop();
        continue;
      }
      const el = new El(tag);
      for (const a of attrs.matchAll(/([\w-]+)\s*=\s*"([^"]*)"/g)) {
        const [, k, val] = a;
        if (k === "class") el.className = val;
        else if (k.startsWith("data-")) {
          el.dataset[k.slice(5).replace(/-(\w)/g, (_, c) => c.toUpperCase())] = val;
          el.attrs[k] = val;
        } else if (k === "id") el.id = val;
        else el.attrs[k] = val;
      }
      el.parentNode = parent;
      parent.children.push(el);
      if (!selfClose) stack.push(el);
    }
  }
  get innerHTML() { return this._html; }
  appendChild(c) {
    if (c.parentNode) {
      const oldIndex = c.parentNode.children.indexOf(c);
      if (oldIndex >= 0) c.parentNode.children.splice(oldIndex, 1);
    }
    c.parentNode = this;
    this.children.push(c);
    return c;
  }
  setAttribute(k, v) { this.attrs[k] = String(v); }
  getAttribute(k) { return k in this.attrs ? this.attrs[k] : null; }
  addEventListener(k, fn) { (this._listeners[k] ||= []).push(fn); }
  // Events BUBBLE, and carry stopPropagation. Without both, a handler on a
  // child that must not trigger its ancestor could never be tested: the assert
  // would hold because nothing propagated, not because the code stopped it.
  fire(kind) {
    const ev = { target: this, _stopped: false,
      stopPropagation() { this._stopped = true; },
      preventDefault() {} };
    let node = this;
    while (node) {
      (node._listeners[kind] || []).forEach((f) => f(ev));
      if (ev._stopped) break;
      node = node.parentNode;
    }
  }
  click() { this.fire("click"); }
  closest(sel) {
    const want = sel.replace(".", "");
    let n = this;
    while (n) { if (n.classList.contains(want)) return n; n = n.parentNode; }
    return null;
  }
  scrollIntoView() {}
  get descendants() {
    const out = [];
    const walk = (n) => n.children.forEach((c) => { out.push(c); walk(c); });
    walk(this);
    return out;
  }
  get id() { return this._id; }
  // A row created by the board registers itself here. Seeding only the fixed
  // control ids left getElementById(rowId) null forever, so every test of the
  // anchor path returned at revealHash's `if (!row) return;` having asserted
  // nothing. A guard that cannot reach its subject is not a guard.
  set id(v) { this._id = v; if (v) byId.set(v, this); }
  querySelector(sel) { return this.querySelectorAll(sel)[0] || null; }
  querySelectorAll(sel) {
    const m = sel.match(/^\[([\w-]+)(?:="?([^"\]]*)"?)?\]$/);
    return this.descendants.filter((n) => {
      if (sel.startsWith(".")) return n.classList.contains(sel.slice(1));
      if (m) {
        const key = m[1].replace(/^data-/, "").replace(/-(\w)/g, (_, c) => c.toUpperCase());
        const has = m[1].startsWith("data-") ? key in n.dataset : m[1] in n.attrs;
        if (!has) return false;
        if (m[2] === undefined) return true;
        return (m[1].startsWith("data-") ? n.dataset[key] : n.attrs[m[1]]) === m[2];
      }
      return n.tagName === sel.toUpperCase();
    });
  }
}

const byId = new Map();
const body = new El("body");
body.dataset.local = /data-local="true"/.test(doc) ? "true" : "false";

const document = {
  body,
  createElement: (t) => new El(t),
  getElementById: (id) => byId.get(id) || null,
};
for (const id of [
  "stats", "totalCount", "planCount", "prCount", "statusChips", "projectChips",
  "groupSel", "typeSel", "prioSel", "sizeSel", "fromDate", "q", "planOnly",
  "prOnly", "shown", "board", "datef",
]) {
  const el = new El("div");
  el.id = id;
  byId.set(id, el);
}
const dataEl = new El("script");
dataEl.textContent = doc.split('<script id="data" type="application/json">')[1].split("</script>")[0];
byId.set("data", dataEl);

const store = new Map();
const localStorage = {
  getItem: (k) => (store.has(k) ? store.get(k) : null),
  setItem: (k, v) => store.set(k, String(v)),
};
const copied = [];
const navigator = { clipboard: { writeText: (t) => { copied.push(t); return Promise.resolve(); } } };
const location = { search: process.env.BOARD_QUERY || "", hash: process.env.BOARD_HASH || "" };
const window = { location, addEventListener() {} };

/* ---------- run the board's own code ---------- */

const js = doc.split("<script>")[1].split("</script>")[0];
const seed = process.env.BOARD_SEED_PROJECTS;
if (seed) store.set("fno-kanban-project-state", seed);

new Function("document", "window", "location", "localStorage", "URLSearchParams", "navigator", "setTimeout", js)(
  document, window, location, localStorage, URLSearchParams, navigator, () => {},
);

/* ---------- read what it produced ---------- */

const board = byId.get("board");
const groups = board.children;
const rows = board.descendants.filter((n) => n.classList.contains("row"));
const visible = rows.filter((r) => !r.classList.contains("is-hidden"));

const api = {
  shown: byId.get("shown").textContent,
  totalRows: rows.length,
  visibleRows: visible.length,
  visibleIds: visible.map((r) => r.id).filter(Boolean),
  groups: groups.map((g) => ({
    open: g.dataset.open,
    hidden: g.classList.contains("is-hidden"),
    count: (g.querySelector(".gc") || {}).textContent,
    bar: (g.querySelector(".tw") || {}).innerHTML || "",
  })),
  projectChips: byId.get("projectChips").children.map((b) => ({
    project: b.dataset.project,
    pressed: b.getAttribute("aria-pressed"),
  })),
  statusChips: byId.get("statusChips").children.map((b) => ({
    status: b.dataset.s,
    pressed: b.getAttribute("aria-pressed"),
  })),
  statClasses: byId.get("stats").children.map((d) => d.className),
  // Whether the row named by location.hash is on screen right now.
  revealedVisible: process.env.BOARD_HASH
    ? !(byId.get(process.env.BOARD_HASH.replace(/^#/, "")) || { classList: { contains: () => true } })
        .classList.contains("is-hidden")
    : null,
  rowHtml: rows.map((r) => ({
    id: r.id,
    type: r.dataset.type,
    html: (r.querySelector(".rmain") || {}).innerHTML || "",
  })),
};

/* ---------- optional scripted interactions ---------- */

const act = process.env.BOARD_ACTION;
if (act) {
  const [kind, arg] = act.split(":");
  if (kind === "toggleProject") {
    const times = Number(process.env.BOARD_ACTION_TIMES || 1);
    for (let i = 0; i < times; i++) {
      byId.get("projectChips").children
        .filter((b) => b.dataset.project === arg).forEach((b) => b.click());
    }
  }
  if (kind === "expand") {
    const row = rows.find((r) => r.id === arg);
    if (row) row.querySelector(".rmain").click();
    api.detail = row ? (row.querySelector(".detail") || {}).innerHTML || "" : null;
  }
  // Typing in the search box re-renders. The reveal bug only shows up on the
  // SECOND render: revealHash cleared is-hidden directly and render() then
  // reassigned className wholesale, so the anchored row vanished on a keystroke.
  // Copy affordances: the id lives on the row, the plan path in the detail.
  if (kind === "copyId") {
    const row = rows.find((r) => r.id === arg);
    if (row) row.querySelector(".rid").fire("click");
  }
  // The keyboard route to the same id copy: expand, then press the real button.
  // Records the tag, because a span with a click handler would satisfy a copy
  // assertion while still being unreachable by tab.
  if (kind === "copyIdDetail") {
    const row = rows.find((r) => r.id === arg);
    if (row) {
      row.querySelector(".rmain").click();
      const btn = row.descendants.filter((n) => n.attrs["data-copy"] === "id")[0];
      if (btn) { api.copyIdTag = btn.tagName; btn.fire("click"); }
    }
  }
  if (kind === "copyPath") {
    const row = rows.find((r) => r.id === arg);
    if (row) {
      row.querySelector(".rmain").click();
      const btn = row.descendants.filter((n) => n.attrs["data-copy"] === "path")[0];
      if (btn) btn.fire("click");
    }
  }
  if (kind === "search") {
    const q = byId.get("q");
    q.value = arg || "";
    q.fire("input");
  }
  const after = board.descendants.filter((n) => n.classList.contains("row"));
  api.after = {
    totalRows: after.length,
    visibleRows: after.filter((r) => !r.classList.contains("is-hidden")).length,
    shown: byId.get("shown").textContent,
    projectChips: byId.get("projectChips").children.map((b) => ({
      project: b.dataset.project,
      pressed: b.getAttribute("aria-pressed"),
    })),
    // What the board PERSISTED. The blank-board bug survived a reload
    // because an active-but-empty selection was written back.
    persisted: store.get("fno-kanban-project-state") || null,
    copied: copied.slice(),
    // Whether the acted-on row ended up expanded. The detail is appended to
    // the ROW, not inside .rmain, so reading .rmain's innerHTML can never
    // see it and an assertion built on that proves nothing.
    detailOpen: (() => {
      const acted = arg ? board.descendants.find((n) => n.id === arg) : null;
      return acted ? !!acted.querySelector(".detail") : null;
    })(),
    // Whether the row named by location.hash is still on screen.
    revealedVisible: process.env.BOARD_HASH
      ? !(byId.get(process.env.BOARD_HASH.replace(/^#/, "")) || { classList: { contains: () => true } })
          .classList.contains("is-hidden")
      : null,
  };
}

process.stdout.write(JSON.stringify(api));
