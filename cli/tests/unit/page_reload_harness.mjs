// Executes the shared page-reload script against a minimal DOM, so a
// behavioral regression fails a test instead of surviving a substring
// assertion. The script rides both operator pages inline; what matters is
// that it reloads only when the reader has walked away, and that everything
// the reader set comes back after the reload.
//
// Takes the script path on argv[2], the stage in env STAGE, writes a JSON
// verdict to stdout.

/* ---------- the smallest DOM the script touches ---------- */

const all = [];

function makeEl(tag, attrs = {}, dataset = {}) {
  const el = {
    tagName: String(tag).toUpperCase(),
    id: "",
    dataset,
    attrs,
    value: "",
    parentElement: null,
    children: [],
    clicks: 0,
    events: [],
    matches(sel) {
      return sel.split(",").some((s) => this.tagName === s.trim().toUpperCase());
    },
    hasAttribute(k) { return k in this.attrs; },
    getAttribute(k) { return k in this.attrs ? this.attrs[k] : null; },
    setAttribute(k, v) { this.attrs[k] = String(v); },
    closest(sel) {
      let n = this;
      while (n) {
        if (sel === "[id]" && n.id) return n;
        n = n.parentElement;
      }
      return null;
    },
    dispatchEvent(ev) { this.events.push(ev.type); return true; },
    click() {
      this.clicks++;
      for (const k of ["aria-pressed", "aria-expanded"]) {
        if (k in this.attrs) {
          this.attrs[k] = this.attrs[k] === "true" ? "false" : "true";
          break;
        }
      }
    },
  };
  all.push(el);
  return el;
}

function withId(el, id) {
  el.id = id;
  return el;
}

const q = withId(makeEl("input"), "q");
const statusChips = withId(makeEl("div"), "statusChips");
const chipStatus = makeEl("button", { "aria-pressed": "true" }, { s: "ready" });
const projectChips = withId(makeEl("div"), "projectChips");
const chipProject = makeEl("button", { "aria-pressed": "false" }, { project: "fno" });
const board = withId(makeEl("main"), "board");
const section = makeEl("section");
const head = makeEl("button", { "aria-expanded": "true" });
const x1 = withId(makeEl("div"), "x-1");
const row = makeEl("button", { "aria-expanded": "false" });

chipStatus.parentElement = statusChips;
chipProject.parentElement = projectChips;
statusChips.parentElement = board;
projectChips.parentElement = board;
section.parentElement = board;
head.parentElement = section;
x1.parentElement = section;
row.parentElement = x1;
board.children = [statusChips, projectChips];

const store = new Map();
const sessionStorage = {
  getItem: (k) => (store.has(k) ? store.get(k) : null),
  setItem: (k, v) => store.set(k, String(v)),
  removeItem: (k) => store.delete(k),
};

const scrollCalls = [];
const listeners = {};
const window = {
  scrollY: 0,
  scrollTo: (...args) => scrollCalls.push(args),
  addEventListener: (type, fn) => { (listeners[type] ||= []).push(fn); },
  getSelection: () => "",
};

let reloaded = false;
const location = { pathname: "/tmp/board.html", reload: () => { reloaded = true; } };

const intervals = [];
function setInterval(fn, ms) { intervals.push({ fn, ms }); }

class Event {
  constructor(type) { this.type = type; }
}

// The script's own selector: inputs and selects with an id, or anything
// carrying an aria state. Walked in document order.
const document = {
  hidden: process.env.STAGE === "hidden",
  currentScript: { dataset: { fnoReload: process.env.STAGE === "off" ? "0" : "60" } },
  querySelectorAll() {
    return all.filter(
      (n) =>
        ((n.tagName === "INPUT" || n.tagName === "SELECT") && n.id) ||
        "aria-pressed" in n.attrs ||
        "aria-expanded" in n.attrs,
    );
  },
};

const KEY = "fno-page-reload:/tmp/board.html";
if (process.env.STAGE === "restore") store.set(KEY, process.env.STORED);

/* ---------- run the script ---------- */

const { readFileSync } = await import("node:fs");
const src = readFileSync(process.argv[2], "utf8");
new Function("document", "window", "location", "sessionStorage", "setInterval", "Event", src)(
  document, window, location, sessionStorage, setInterval, Event,
);

const stage = process.env.STAGE;

if (stage === "save") {
  q.value = "crown";
  chipProject.click();
  row.click();
  head.click();
  window.scrollY = 900;
  intervals[0].fn();
  process.stdout.write(JSON.stringify({
    ms: intervals[0].ms,
    reloaded,
    stored: JSON.parse(sessionStorage.getItem(KEY)),
  }));
} else if (stage === "restore") {
  process.stdout.write(JSON.stringify({
    value: q.value,
    events: q.events,
    statusClicks: chipStatus.clicks,
    projectClicks: chipProject.clicks,
    headClicks: head.clicks,
    rowClicks: row.clicks,
    scrollTo: scrollCalls,
    keyRemains: store.has(KEY),
  }));
} else if (stage === "hidden") {
  intervals[0].fn();
  process.stdout.write(JSON.stringify({ reloaded }));
} else if (stage === "touched") {
  listeners.keydown.forEach((fn) => fn());
  intervals[0].fn();
  process.stdout.write(JSON.stringify({ reloaded }));
} else if (stage === "off") {
  process.stdout.write(JSON.stringify({ registered: intervals.length > 0 }));
}
