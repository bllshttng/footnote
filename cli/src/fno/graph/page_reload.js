// Reloads an operator page on a timer, so a page opened from disk shows the
// newest render. Form values, pressed chips, expanded sections and the scroll
// position survive the reload through sessionStorage. The renderer stamps the
// interval in seconds on data-fno-reload; 0, or a value that is not a number,
// turns the reload off. build.rs copies this file to
// cli/src/fno/graph/page_reload.js for the Python board renderer.
(function () {
  var tag = document.currentScript;
  var secs = Number(tag && tag.dataset.fnoReload);
  if (!(secs > 0)) return;
  var KEY = 'fno-page-reload:' + location.pathname;
  var SELECTOR = 'input[id],select[id],[aria-pressed],[aria-expanded]';
  // A key names one element across two loads of the same page: its own or its
  // nearest ancestor's id, its data attributes, and its order among twins.
  function each(fn) {
    var seen = Object.create(null);
    document.querySelectorAll(SELECTOR).forEach(function (el) {
      var host = el.id ? el : el.parentElement && el.parentElement.closest('[id]');
      var base = (host ? host.id : '') + '|' + JSON.stringify(el.dataset);
      seen[base] = (seen[base] || 0) + 1;
      fn(base + '#' + seen[base], el);
    });
  }
  function valueOf(el) {
    if (el.matches('input,select')) return el.value;
    return el.getAttribute(el.hasAttribute('aria-pressed') ? 'aria-pressed' : 'aria-expanded');
  }
  // The page's own defaults, read before any restore, so a restored value
  // still counts as a change at the next save.
  var initial = Object.create(null);
  each(function (key, el) { initial[key] = valueOf(el); });
  var saved = null;
  try {
    saved = JSON.parse(sessionStorage.getItem(KEY) || 'null');
    sessionStorage.removeItem(KEY);
  } catch (e) {}
  if (saved && saved.values) {
    each(function (key, el) {
      if (!(key in saved.values) || valueOf(el) === saved.values[key]) return;
      if (el.matches('input,select')) {
        el.value = saved.values[key];
        el.dispatchEvent(new Event('input', { bubbles: true }));
        el.dispatchEvent(new Event('change', { bubbles: true }));
      } else {
        el.click();
      }
    });
    window.scrollTo(0, saved.y || 0);
  }
  var touched = 0;
  ['keydown', 'pointerdown', 'wheel', 'touchstart'].forEach(function (type) {
    window.addEventListener(type, function () { touched = Date.now(); }, { capture: true, passive: true });
  });
  setInterval(function () {
    if (document.hidden || Date.now() - touched < secs * 1000) return;
    if (String(window.getSelection ? window.getSelection() : '')) return;
    var values = {};
    each(function (key, el) { var now = valueOf(el); if (now !== initial[key]) values[key] = now; });
    try { sessionStorage.setItem(KEY, JSON.stringify({ y: window.scrollY, values: values })); } catch (e) {}
    location.reload();
  }, secs * 1000);
})();
