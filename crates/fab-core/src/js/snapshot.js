// Injected into every document. Installs settle instrumentation and exposes a
// non-enumerable `window.__ub` with snapshot + action helpers. Idempotent.
(() => {
  if (window.__ub) return;
  const now = () => performance.now();
  // Captured before any instrumentation so settle's own polling isn't counted.
  const oST = window.setTimeout.bind(window);
  const U = {
    docId: Math.random().toString(36).slice(2),
    version: 0,
    lastMut: now(),
    inflight: 0,
    navigating: false,
    timerMax: 600,
    els: [],
    texts: [],
  };
  Object.defineProperty(window, "__ub", { value: U, enumerable: false, configurable: true });

  // ---------- settle instrumentation ----------
  const bump = () => { U.version++; U.lastMut = now(); };
  new MutationObserver(bump).observe(document, {
    subtree: true, childList: true, attributes: true, characterData: true,
  });
  // Property changes (a checkbox's `checked`, a typed `value`) aren't DOM
  // mutations; without this the snapshot cache served a stale "unchecked" and
  // a second "check" toggled the box back off.
  for (const ev of ["input", "change", "click", "keyup", "reset"]) addEventListener(ev, bump, true);
  // Same-origin iframes are part of the snapshot, so their changes must
  // invalidate it too; neither the observer above nor window listeners see
  // inside a frame. Attached once per frame document, when a snapshot reads it.
  const watchedDocs = new WeakSet();
  const watchFrame = (doc) => {
    if (!doc || watchedDocs.has(doc)) return;
    watchedDocs.add(doc);
    try {
      new MutationObserver(bump).observe(doc, { subtree: true, childList: true, attributes: true, characterData: true });
      const w = doc.defaultView;
      if (w) for (const ev of ["input", "change", "click", "keyup", "reset"]) w.addEventListener(ev, bump, true);
    } catch (_) {}
  };
  addEventListener("beforeunload", () => { U.navigating = true; }, true);
  addEventListener("pagehide", () => { U.navigating = true; }, true);
  addEventListener("pageshow", () => { U.navigating = false; }, true);

  // Network/timer instrumentation must run in the page's main world. Some
  // backends (Camoufox) evaluate us in an isolated world, so it is installed
  // via a <script> element when possible and reports back through DOM events,
  // which both worlds see. At document start (CDP init script) we are already
  // in the main world and there is no documentElement yet, so install directly.
  let pendingTimers = 0;
  const INSTRUMENT = function (report) {
    if (window.__ubNet) return;
    Object.defineProperty(window, "__ubNet", { value: 1, enumerable: false });
    addEventListener("ub-ping", () => dispatchEvent(new CustomEvent("ub-pong")));
    const oFetch = window.fetch;
    if (oFetch) {
      window.fetch = function (...a) {
        report("f+");
        let p;
        try { p = oFetch.apply(this, a); } catch (e) { report("f-"); throw e; }
        return p.finally(() => report("f-"));
      };
    }
    const X = window.XMLHttpRequest && XMLHttpRequest.prototype;
    if (X) {
      const oSend = X.send;
      X.send = function (...a) {
        report("f+");
        this.addEventListener("loadend", () => report("f-"), { once: true });
        return oSend.apply(this, a);
      };
    }
    const oST = window.setTimeout, oCT = window.clearTimeout, live = new Set();
    window.setTimeout = function (fn, d, ...r) {
      d = +d || 0;
      if (typeof fn !== "function" || d > 600) return oST.call(window, fn, d, ...r);
      const id = oST.call(window, function () {
        if (live.delete(id)) report("t-");
        return fn.apply(this, arguments);
      }, d, ...r);
      live.add(id);
      report("t+");
      return id;
    };
    window.clearTimeout = function (id) { if (live.delete(id)) report("t-"); return oCT.call(window, id); };
  };
  const onNet = (k) => {
    if (k === "f+") U.inflight++;
    else if (k === "f-") { U.inflight = Math.max(0, U.inflight - 1); bump(); }
    else if (k === "t+") pendingTimers++;
    else if (k === "t-") { pendingTimers = Math.max(0, pendingTimers - 1); U.lastMut = Math.max(U.lastMut, now()); }
  };
  addEventListener("ub-net", (e) => onNet(e.detail));
  const pong = () => { let ok = false; const h = () => { ok = true; };
    addEventListener("ub-pong", h, { once: true }); dispatchEvent(new CustomEvent("ub-ping"));
    removeEventListener("ub-pong", h); return ok; };
  if (!pong()) {
    const root = document.documentElement;
    if (root) {
      const sc = document.createElement("script");
      sc.textContent = `(${INSTRUMENT})((k) => dispatchEvent(new CustomEvent("ub-net", { detail: k })));`;
      root.appendChild(sc);
      sc.remove();
    }
    if (!pong()) INSTRUMENT(onNet); // document start (main world) or CSP-blocked
  }
  const pending = { get size() { return pendingTimers; } };

  // Resolves once the DOM has been quiet for `quiet` ms with no network or
  // short timers pending, or when a navigation starts, or at `cap` ms.
  // ---------- text helpers (plain string code, no regular expressions) ----------
  // White space as JS's \s and String.prototype.trim define it.
  const isWs = (n) => n === 32 || (n >= 9 && n <= 13) || n === 0xa0 || n === 0x1680 || (n >= 0x2000 && n <= 0x200a) ||
    n === 0x2028 || n === 0x2029 || n === 0x202f || n === 0x205f || n === 0x3000 || n === 0xfeff;
  const isBreak = (n) => n === 10 || n === 13 || n === 0x2028 || n === 0x2029;
  // A word character (for whole-word checks): an ASCII letter, digit or "_".
  const isWord = (n) => (n >= 48 && n <= 57) || (n >= 65 && n <= 90) || (n >= 97 && n <= 122) || n === 95;
  const isAlpha = (n) => (n >= 65 && n <= 90) || (n >= 97 && n <= 122);
  // Only ASCII letters change case: case-insensitive matching of ASCII words.
  const lowerAscii = (s) => {
    let o = "";
    for (let i = 0; i < s.length; i++) {
      const n = s.charCodeAt(i);
      o += n >= 65 && n <= 90 ? String.fromCharCode(n + 32) : s[i];
    }
    return o;
  };
  // Runs of white space become one space; ends trimmed.
  const collapse = (s) => {
    const parts = [];
    for (let i = 0; i < s.length;) {
      while (i < s.length && isWs(s.charCodeAt(i))) i++;
      const j = i;
      while (i < s.length && !isWs(s.charCodeAt(i))) i++;
      if (i > j) parts.push(s.slice(j, i));
    }
    return parts.join(" ");
  };
  // Split at runs of white space, keeping empty ends (as split(/\s+/) does).
  const splitWs = (s) => {
    const out = [];
    let p = 0;
    for (let i = 0; i < s.length;) {
      if (!isWs(s.charCodeAt(i))) { i++; continue; }
      out.push(s.slice(p, i));
      while (i < s.length && isWs(s.charCodeAt(i))) i++;
      p = i;
    }
    out.push(s.slice(p));
    return out;
  };
  // `w` at `i` as a whole word.
  const wordAt = (s, i, w) => s.startsWith(w, i) && !isWord(s.charCodeAt(i - 1)) && !isWord(s.charCodeAt(i + w.length));
  const hasWord = (s, w) => {
    for (let i = s.indexOf(w); i >= 0; i = s.indexOf(w, i + 1)) if (wordAt(s, i, w)) return true;
    return false;
  };
  const startsWord = (s, words) => words.some((w) => wordAt(s, 0, w));
  const hasAny = (s, words) => words.some((w) => s.includes(w));
  // "one time", "one-time", "onetime": a, at most one character (not a line
  // break), b.
  const near = (s, a, b) => {
    for (let i = s.indexOf(a); i >= 0; i = s.indexOf(a, i + 1)) {
      const j = i + a.length;
      if (s.startsWith(b, j) || (j < s.length && !isBreak(s.charCodeAt(j)) && s.startsWith(b, j + 1))) return true;
    }
    return false;
  };

  // In-progress indicators: a button or status reading "Saving…" / "Please
  // wait…", or anything aria-busy. Apps show these while a save is in flight;
  // until they clear, nothing has been confirmed.
  // Busy text is "please wait" / "one moment", or a first word ending in
  // "ing" and at most 40 more characters on its line; then "…" or 2-3 dots.
  const BUSY_SAYS = ["please wait", "one moment"];
  const busyBody = (b) => {
    if (BUSY_SAYS.includes(lowerAscii(b.trimEnd()))) return true;
    let i = 0;
    while (i < b.length && isAlpha(b.charCodeAt(i))) i++;
    if (i < 4 || lowerAscii(b.slice(i - 3, i)) !== "ing" || isWord(b.charCodeAt(i))) return false;
    const rest = b.slice(i).trimEnd();
    return rest.length <= 40 && !rest.includes("\n") && !rest.includes("\u2026");
  };
  const busyText = (t) => {
    const s = t.trim();
    if (s.endsWith("\u2026")) return busyBody(s.slice(0, -1));
    let d = 0;
    while (d < s.length && s[s.length - 1 - d] === ".") d++;
    if (d < 2) return false;
    // The ending is two or three of the dots (an "…ing" text may hold more).
    if (d <= 3 && BUSY_SAYS.includes(lowerAscii(s.slice(0, s.length - d).trimEnd()))) return true;
    return busyBody(s.slice(0, -2)) || (d >= 3 && busyBody(s.slice(0, -3)));
  };
  U.busy = () => {
    try {
      for (const e of document.querySelectorAll('[aria-busy="true"]')) if (visible(e)) return true;
      for (const e of document.querySelectorAll('button,[role=button],[role=status],[aria-live],input[type=submit]')) {
        const t = (e.tagName === "INPUT" ? e.value : e.innerText || "").trim();
        if (t.length > 0 && t.length < 40 && busyText(t) && visible(e)) return true;
      }
    } catch (_) {}
    return false;
  };

  // Resolves once the DOM has been quiet for `quiet` ms with no network or
  // short timers pending and nothing showing as in progress, or when a
  // navigation starts, or at `cap` ms (up to 8 s while something is busy).
  U.settle = (quiet = 40, cap = 3000, timerMax = 600) => new Promise((res) => {
    U.timerMax = timerMax;
    const t0 = now();
    const busyCap = Math.max(cap, 8000);
    const tick = () => {
      const t = now();
      if (U.navigating) return res({ nav: true, ms: t - t0 });
      const idle = t - Math.max(U.lastMut, t0);
      const quietNow = U.inflight <= 0 && pending.size === 0 && idle >= quiet && document.readyState !== "loading";
      const busy = (quietNow || t - t0 > cap) && U.busy();
      if (t - t0 > (busy ? busyCap : cap)) return res({ timeout: true, busy, ms: t - t0, inflight: U.inflight, timers: pending.size });
      if (quietNow && !busy) return res({ ok: true, ms: t - t0 });
      oST(tick, busy ? 25 : 8);
    };
    tick();
  });

  // ---------- snapshot ----------
  const SKIP = new Set(["SCRIPT", "STYLE", "NOSCRIPT", "TEMPLATE", "HEAD", "META", "LINK", "svg", "SVG", "PATH"]);
  const BLOCK = new Set(["P", "LI", "TD", "TH", "H1", "H2", "H3", "H4", "H5", "H6", "DT", "DD", "BLOCKQUOTE",
    "PRE", "FIGCAPTION", "CAPTION", "LEGEND", "DIV", "SECTION", "ARTICLE", "MAIN", "ASIDE", "HEADER", "FOOTER",
    "NAV", "FORM", "BODY", "DIALOG", "LABEL", "FIELDSET", "TABLE", "UL", "OL", "DL", "SUMMARY", "DETAILS"]);
  const ITEM = new Set(["TR", "LI", "ARTICLE", "DIALOG", "FIELDSET"]);
  const ITEM_ROLE = new Set(["row", "listitem", "article", "dialog", "alertdialog", "group", "option", "gridcell"]);
  // A class with a word (between spaces, "_" or "-") naming a card, item,
  // product… (or several).
  const ITEM_WORDS = new Set(["card", "item", "product", "result", "row", "entry", "tile"]);
  const itemClass = (c) => {
    let w = "";
    for (let i = 0; i <= c.length; i++) {
      const n = c.charCodeAt(i);
      if (i < c.length && !isWs(n) && n !== 95 && n !== 45) { w += c[i]; continue; }
      const l = lowerAscii(w);
      if (ITEM_WORDS.has(l) || (l.endsWith("s") && ITEM_WORDS.has(l.slice(0, -1)))) return true;
      w = "";
    }
    return false;
  };
  const INTERACTIVE_ROLES = new Set(["button", "link", "checkbox", "radio", "tab", "menuitem", "menuitemcheckbox",
    "menuitemradio", "option", "combobox", "switch", "textbox", "searchbox", "listbox", "treeitem", "slider",
    "spinbutton", "gridcell"]);

  // Cuts at most n UTF-16 units without splitting a surrogate pair: half an
  // emoji is a lone surrogate, which strict JSON parsers reject.
  const cut = (s, n) => {
    let t = s.slice(0, n);
    const c = t.charCodeAt(t.length - 1);
    if (c >= 0xd800 && c <= 0xdbff) t = t.slice(0, -1);
    return t;
  };
  const squash = (s, n) => {
    s = collapse(s || "");
    return n && s.length > n ? cut(s, n - 1) + "…" : s;
  };

  // Text of a subtree, skipping form controls (so a label wrapping a select
  // doesn't swallow every option).
  const plainText = (root, skip = "select,textarea,option,script,style") => {
    let out = "";
    const w = (root.ownerDocument || document).createTreeWalker(root, NodeFilter.SHOW_TEXT);
    for (let n = w.nextNode(); n; n = w.nextNode()) {
      const p = n.parentElement;
      if (p && p.closest(skip)) continue;
      out += n.nodeValue + " ";
    }
    return out;
  };
  // Parent across shadow-root and same-origin iframe boundaries.
  const up = (n) => {
    if (n.parentElement) return n.parentElement;
    const r = n.parentNode;
    if (r && r.host) return r.host;
    if (n.nodeType === 1 && n.tagName === "HTML") {
      try { return n.ownerDocument.defaultView.frameElement; } catch (_) { return null; }
    }
    return null;
  };

  const roleOf = (el, tag) => {
    const r = el.getAttribute("role");
    if (r) return r.split(" ")[0];
    switch (tag) {
      case "A": return el.hasAttribute("href") ? "link" : "";
      case "BUTTON": case "SUMMARY": return "button";
      case "SELECT": return "select";
      case "TEXTAREA": return "textbox";
      case "OPTION": return "";
      case "INPUT": {
        const t = (el.getAttribute("type") || "").toLowerCase();
        if (t === "hidden") return "";
        if (t === "checkbox" || t === "radio") return t;
        if (t === "submit" || t === "button" || t === "reset" || t === "image") return "button";
        if (t === "range") return "slider";
        if (t === "file") return "file";
        if (t === "color") return "button";
        return "textbox";
      }
    }
    if (el.isContentEditable && el.getAttribute("contenteditable") !== null) return "textbox";
    return "";
  };

  const byId = (el, id) => {
    const root = el.getRootNode();
    return (root.getElementById && root.getElementById(id)) || el.ownerDocument.getElementById(id);
  };

  const nameOf = (el, tag, role) => {
    let s = el.getAttribute("aria-label");
    if (s && s.trim()) return squash(s, 80);
    const lb = el.getAttribute("aria-labelledby");
    if (lb) {
      s = splitWs(lb).map((id) => { const n = byId(el, id); return n ? plainText(n) : ""; }).join(" ");
      if (s.trim()) return squash(s, 80);
    }
    if (tag === "INPUT" || tag === "SELECT" || tag === "TEXTAREA") {
      const t = (el.type || "").toLowerCase();
      if (t === "submit" || t === "button" || t === "reset") return squash(el.value || t, 80);
      if (el.labels && el.labels.length) {
        s = [...el.labels].map((l) => plainText(l)).join(" ");
        if (s.trim()) return squash(s, 80);
      }
      s = el.getAttribute("placeholder") || el.getAttribute("title");
      if (s && s.trim()) return squash(s, 80);
      // Unlabelled control: borrow nearby text.
      const prev = el.previousElementSibling || (el.parentElement && el.parentElement.previousElementSibling);
      if (prev && !prev.matches("input,select,textarea,button")) {
        s = squash(plainText(prev), 40);
        if (s) return s;
      }
      return squash(el.getAttribute("name") || el.id || "", 40);
    }
    if (tag === "IMG") return squash(el.alt || el.title, 80);
    s = el.innerText;
    // Unrendered content (closed <details>, hidden panels) has no innerText.
    if (!(s && s.trim())) s = el.textContent;
    if (s && s.trim()) return squash(s, 80);
    const img = el.querySelector && el.querySelector("img[alt],svg title,[aria-label]");
    if (img) return squash(img.getAttribute("alt") || img.getAttribute("aria-label") || img.textContent, 80);
    return squash(el.getAttribute("title") || el.getAttribute("name") || el.id || "", 40);
  };

  const visible = (el) => {
    if (el.checkVisibility && !el.checkVisibility({ checkOpacity: true, checkVisibilityCSS: true })) return false;
    const r = el.getBoundingClientRect();
    return r.width > 0 && r.height > 0;
  };

  // ---------- stable node keys ----------
  // A node keeps its key for as long as it lives in this document, so ids the
  // caller saw stay valid across re-renders and later snapshots.
  const keys = new WeakMap();
  const byKey = new Map();
  let nextKey = 1;
  const keyOf = (n) => {
    let k = keys.get(n);
    if (k === undefined) {
      k = nextKey++;
      keys.set(n, k);
      byKey.set(k, new WeakRef(n));
    }
    return k;
  };
  const pruneKeys = () => {
    if (byKey.size < 20000) return;
    for (const [k, r] of byKey) if (!r.deref()) byKey.delete(k);
  };

  const ctxCache = new Map();
  // A class-matched container only counts when it is one of several siblings
  // sharing that class (a card in a list), not a one-off layout wrapper.
  const repeated = (p) => {
    const par = p.parentElement;
    if (!par || !p.className) return false;
    let n = 0;
    for (const s of par.children) if (s.className === p.className && ++n >= 2) return true;
    return false;
  };
  const itemOf = (el) => {
    for (let p = up(el), d = 0; p && d < 8; p = up(p), d++) {
      if (p.tagName === "BODY" || p.tagName === "FORM" || p.tagName === "MAIN") return null;
      if (ITEM.has(p.tagName) || ITEM_ROLE.has(p.getAttribute("role") || "")) return p;
      if (typeof p.className === "string" && itemClass(p.className) && repeated(p)) return p;
    }
    return null;
  };
  // A record is one of several repeated containers: a table row, a list item,
  // a card or feed entry. Returns the container or null.
  const RECORD_ROLE = new Set(["row", "listitem", "article"]);
  const recordOf = (n) => {
    for (let p = n.nodeType === 1 ? n : up(n), d = 0; p && d < 10; p = up(p), d++) {
      const tag = p.tagName;
      if (tag === "BODY" || tag === "MAIN" || tag === "FORM" || tag === "DIALOG") return null;
      if (tag === "TR") return p.closest("thead") ? null : p;
      if (tag === "LI" || tag === "ARTICLE" || RECORD_ROLE.has(p.getAttribute("role") || "")) {
        const par = p.parentElement;
        if (par && par.children.length >= 2) return p;
      }
      if (typeof p.className === "string" && p.className && itemClass(p.className) && repeated(p)) return p;
    }
    return null;
  };
  const collOf = (r) => (r.tagName === "TR" ? r.closest("table") || r.parentElement : r.parentElement);
  const colsOf = (coll) => {
    if (coll.tagName !== "TABLE") return undefined;
    const hr = coll.querySelector("thead tr") || coll.querySelector("tr");
    if (!hr || !hr.querySelector("th")) return undefined;
    return [...hr.children].map((c) => squash(plainText(c), 40));
  };
  // "Next", "Next ›", "Next page", "›", "Load more", "Show older"… (a wizard's
  // "Next" is only attached if a collection precedes it).
  const ARROWS = "›»>→";
  const PAGER_SAYS = new Set(["older", "older entries", "older posts", "show older", "more results", "view more"]);
  const pagerName = (name) => {
    const s = lowerAscii(name);
    if (PAGER_SAYS.has(s) || (s.length === 1 && ARROWS.includes(s))) return true;
    if (s.startsWith("load more") || s.startsWith("show more")) {
      for (let i = 9; i < s.length; i++) if (isBreak(s.charCodeAt(i))) return false;
      return true;
    }
    if (!s.startsWith("next")) return false;
    // "next" or "next page", then optional space and one arrow.
    const r = (s.startsWith(" page", 4) ? s.slice(9) : s.slice(4)).trimStart();
    return r === "" || (r.length === 1 && ARROWS.includes(r));
  };
  const isPager = (e, name) =>
    e.getAttribute("rel") === "next" || pagerName(name) || hasWord(lowerAscii(e.getAttribute("aria-label") || ""), "next");

  // Label of the enclosing form/section/dialog (or iframe document), e.g. "Newsletter".
  const SECTION = new Set(["FORM", "FIELDSET", "SECTION", "DIALOG", "ASIDE", "NAV", "DETAILS"]);
  const SECTION_ROLE = new Set(["dialog", "alertdialog", "region", "form", "navigation", "tabpanel", "group", "radiogroup"]);
  const secCache = new Map();
  const sectionLabel = (p) => {
    let t = secCache.get(p);
    if (t !== undefined) return t;
    t = p.getAttribute("aria-label") || "";
    if (!t && p.getAttribute("aria-labelledby")) {
      const n = byId(p, splitWs(p.getAttribute("aria-labelledby"))[0]);
      if (n) t = plainText(n);
    }
    if (!t) {
      const h = p.querySelector(":scope > legend, :scope > summary, h1, h2, h3, h4, legend, summary");
      if (h) t = plainText(h);
    }
    if (!t && p.tagName === "HTML") t = p.ownerDocument.title || "";
    t = squash(t, 50);
    secCache.set(p, t);
    return t;
  };
  const sectionOf = (el) => {
    for (let p = up(el), d = 0; p && d < 14; p = up(p), d++) {
      if (SECTION.has(p.tagName) || SECTION_ROLE.has(p.getAttribute("role") || "") ||
          (p.tagName === "HTML" && p.ownerDocument !== document)) {
        const t = sectionLabel(p);
        if (t) return t;
      }
    }
    return "";
  };
  const ctxOf = (el, name) => {
    const it = itemOf(el);
    let t = "";
    if (it) {
      t = ctxCache.get(it);
      if (t === undefined) {
        const row = it.tagName === "TR" || it.getAttribute("role") === "row";
        const h = row ? null : it.querySelector("h1,h2,h3,h4,h5,h6,legend,[class*=title],[class*=name],strong,b");
        t = squash(row ? plainText(it, "select,textarea,option,script,style,button") : h ? plainText(h) : plainText(it), 120);
        if (h && t.length < 3) t = squash(plainText(it), 120);
        ctxCache.set(it, t);
      }
      // In a table row, drop the element's own label from the row text.
      if (name && it.tagName === "TR" && t.includes(name)) t = squash(t.replace(name, " "), 120);
    }
    if (!t) t = sectionOf(el);
    if (name && t === name) return "";
    return t.length > 80 ? cut(t, 79) + "…" : t;
  };

  const isInteractive = (el, tag, role) => {
    if (INTERACTIVE_ROLES.has(role) || role === "select" || role === "file") return true;
    if (tag === "A" && el.hasAttribute("href")) return true;
    if (el.hasAttribute("onclick")) return true;
    const ti = el.getAttribute("tabindex");
    if (ti !== null && +ti >= 0 && tag !== "DIV" && tag !== "SECTION") return true;
    if (ti !== null && +ti >= 0 && el.childElementCount < 4) return true;
    return false;
  };

  const isToggleType = (t) => { const l = lowerAscii(String(t)); return l === "checkbox" || l === "radio"; };
  const flagsOf = (el, tag, role) => {
    const f = [];
    if (el.disabled || el.getAttribute("aria-disabled") === "true") f.push("disabled");
    // Native checkbox/radio inputs, whatever role they carry (role="switch"
    // on an <input type=checkbox> is common), report their real state.
    if (tag === "INPUT" && (role === "checkbox" || role === "radio" || isToggleType(el.type))) { if (el.checked) f.push("checked"); }
    else {
      const c = el.getAttribute("aria-checked");
      if (c === "true") f.push("checked");
      else if (c === "false" && (role === "checkbox" || role === "switch" || role === "radio" || role.startsWith("menuitem"))) f.push("unchecked");
    }
    const ex = el.getAttribute("aria-expanded");
    if (ex) f.push(ex === "true" ? "expanded" : "collapsed");
    if (el.getAttribute("aria-selected") === "true") f.push("selected");
    if (el.getAttribute("aria-pressed") === "true") f.push("pressed");
    if (el.getAttribute("aria-current") && el.getAttribute("aria-current") !== "false") f.push("current");
    if (el.required) f.push("required");
    if (el.readOnly) f.push("readonly");
    return f;
  };

  // Offsets of same-origin iframes so coordinates are top-level viewport coords.
  const frameOffset = (el) => {
    let x = 0, y = 0;
    for (let w = el.ownerDocument.defaultView; w && w !== window.top; w = w.parent) {
      const fe = w.frameElement;
      if (!fe) break;
      const r = fe.getBoundingClientRect();
      x += r.left + fe.clientLeft; y += r.top + fe.clientTop;
    }
    return { x, y };
  };

  const covered = (el, modal) => {
    if (modal && !modal.contains(el) && !(el.getRootNode().host && modal.contains(el.getRootNode().host))) return true;
    const r = el.getBoundingClientRect();
    const view = el.ownerDocument.defaultView;
    const cx = r.left + r.width / 2, cy = r.top + r.height / 2;
    if (cx < 0 || cy < 0 || cx > view.innerWidth || cy > view.innerHeight) return false;
    const hit = el.getRootNode().elementFromPoint ? el.getRootNode().elementFromPoint(cx, cy) : null;
    if (!hit || hit === el || el.contains(hit) || hit.contains(el)) return false;
    if (hit.tagName === "LABEL" && hit.control === el) return false;
    if (el.labels && [...el.labels].some((l) => l.contains(hit))) return false;
    return true;
  };

  U.snapshot = (opts = {}) => {
    const t0 = now();
    U.truncated = null;
    if (opts.since === U.version && opts.doc === U.docId) return { same: true, version: U.version, docId: U.docId };
    ctxCache.clear();
    secCache.clear();
    pruneKeys();
    const els = [], texts = [];
    const blockText = new Map();
    // The topmost visible modal: several may be in the DOM (an editor whose
    // overlay was hidden when its confirmation opened), and the first match in
    // document order is often not the one on screen.
    const modal = [...document.querySelectorAll("dialog:modal,[aria-modal=true],[role=alertdialog]")].reverse().find((m) => visible(m)) || null;
    const modalEl = modal && visible(modal) ? modal : null;
    const maxEls = opts.maxEls || 3000;
    const recs = new Map(); // record element -> {k, coll, label}
    const colls = new Map(); // collection element -> {k, label, cols, n}
    const recKey = (n) => {
      const r = recordOf(n);
      if (!r) return undefined;
      let o = recs.get(r);
      if (!o) {
        const c = collOf(r);
        let co = colls.get(c);
        if (!co) {
          co = { k: keyOf(c), n: 0 };
          const lab = sectionOf(r) || c.getAttribute("aria-label") || "";
          if (lab) co.label = squash(lab, 60);
          const cols = colsOf(c);
          if (cols) co.cols = cols;
          colls.set(c, co);
        }
        co.n++;
        o = { k: keyOf(r), coll: co.k };
        recs.set(r, o);
      }
      return o.k;
    };
    // Hidden containers that a visible control reveals: aria-controls targets of
    // collapsed triggers and unselected tabs, plus closed <details>.
    const revealers = new Map();
    for (const t of document.querySelectorAll("[aria-controls]")) {
      const collapsed = t.getAttribute("aria-expanded") === "false" ||
        (t.getAttribute("role") === "tab" && t.getAttribute("aria-selected") !== "true");
      if (!collapsed) continue;
      for (const id of splitWs(t.getAttribute("aria-controls"))) if (id) revealers.set(id, t);
    }
    let latentN = 0;
    const revealerOf = (el) => {
      const d = el.closest("details:not([open])");
      if (d) { const s = d.querySelector(":scope > summary"); if (s && visible(s)) return s; }
      for (let p = el, i = 0; p && i < 12; p = p.parentElement, i++) {
        const t = p.id && revealers.get(p.id);
        if (t && visible(t)) return t;
      }
      return null;
    };

    const record = (el, tag, role, via) => {
      const n = nameOf(el, tag, role);
      const o = { i: keyOf(el), r: role, n };
      if (tag === "INPUT" || tag === "TEXTAREA") {
        const t = (el.type || "").toLowerCase();
        if (role === "combobox" || role === "searchbox") o.t = "input";
        if (t && t !== "text" && t !== "textarea" && role === "textbox") o.t = t;
        if (role === "textbox" || o.t === "input") o.v = t === "password" || secretEls.has(el) ? (el.value ? "••••" : "") : squash(el.value, 80);
        if (role === "radio" && el.name) o.g = el.name;
        if (role === "slider") o.v = el.value;
        const ph = el.getAttribute("placeholder");
        if (ph && ph.trim() && squash(ph, 60) !== n) o.p = squash(ph, 60);
      } else if (tag === "SELECT") {
        const opts = [...el.options];
        o.v = squash(el.selectedOptions[0] ? el.selectedOptions[0].text : "", 60);
        o.o = opts.slice(0, 250).map((x) => squash(x.text, 60));
        if (el.multiple) o.t = "multiple";
      } else if (role === "textbox" || role === "searchbox" || role === "combobox") {
        const v = el.isContentEditable ? el.innerText
          : el.getAttribute("aria-valuetext") || (role === "combobox" ? el.innerText : "") || "";
        if (el.isContentEditable) { o.v = secretEls.has(el) ? "••••" : squash(v, 80); o.t = "editable"; }
        else if (v) o.v = squash(v, 80);
      } else if (role === "radio") {
        const grp = el.closest("[role=radiogroup]");
        if (grp) o.g = nameOf(grp, grp.tagName, "radiogroup") || "group";
      }
      const f = flagsOf(el, tag, role);
      if (via) { f.push("latent"); o.rv = keyOf(via); }
      else if (covered(el, modalEl)) f.push("covered");
      if (f.length) o.f = f.join(" ");
      const c = ctxOf(el, n);
      if (c) o.c = c;
      const rk = recKey(el);
      if (rk !== undefined) o.rc = rk;
      if (!via && isPager(el, n)) o.pg = 1;
      els.push(o);
    };

    // Nearest block ancestor, crossing shadow-root boundaries.
    const blockOf = (n) => {
      let b = n.parentElement || (n.parentNode && n.parentNode.host);
      while (b && !BLOCK.has(b.tagName)) {
        b = b.parentElement || (b.parentNode && b.parentNode.host) || null;
      }
      return b;
    };

    const addText = (el, s) => {
      let arr = blockText.get(el);
      if (!arr) { arr = []; blockText.set(el, arr); }
      arr.push(s);
    };

    // A snapshot must never hang the page: the walk stops at a node budget
    // or a time budget, and says so. A node reached twice (a shadow root or
    // frame reached two ways) is skipped.
    let visited = 0;
    const seenNodes = new WeakSet();
    const walk = (root) => {
      const stack = [root];
      // DFS in document order: children are pushed in reverse. Shadow roots and
      // iframe bodies are pushed last so they are visited before light children.
      while (stack.length) {
        const node = stack.pop();
        if (seenNodes.has(node)) continue;
        seenNodes.add(node);
        if (++visited > 200000 || ((visited & 1023) === 0 && now() - t0 > 4000)) {
          U.truncated = `walk stopped after ${visited} nodes, ${Math.round(now() - t0)} ms, stack ${stack.length}`;
          break;
        }
        // Text nodes go through the stack too, so a block's text keeps document
        // order ("<b>Ann</b> revoked key" reads "Ann revoked key").
        if (node.nodeType === 3) {
          const v = node.nodeValue;
          if (v && v.trim()) {
            const b = blockOf(node);
            if (b) addText(b, v);
          }
          continue;
        }
        let el = null;
        if (node.nodeType === 1) {
          el = node;
          const tag = el.tagName;
          if (SKIP.has(tag)) continue;
          const role = roleOf(el, tag);
          if (els.length < maxEls && isInteractive(el, tag, role)) {
            let vis = visible(el);
            // Styled checkbox/radio/switch: the native input is hidden (opacity 0,
            // 1px) but its label is visible and clickable.
            const toggle = role === "checkbox" || role === "radio" || role === "switch" || (tag === "INPUT" && isToggleType(el.type));
            if (!vis && toggle && el.labels && [...el.labels].some(visible)) vis = true;
            if (vis) record(el, tag, role, null);
            else if (latentN < 300) {
              const via = revealerOf(el);
              if (via) { latentN++; record(el, tag, role, via); }
            }
          }
        }
        const kids = node.childNodes;
        for (let k = kids.length - 1; k >= 0; k--) {
          const c = kids[k];
          if (c.nodeType === 1 || c.nodeType === 3) stack.push(c);
        }
        if (!el) continue;
        if (el.shadowRoot) stack.push(el.shadowRoot);
        if (el.tagName === "IFRAME") {
          try {
            if (el.contentDocument && el.contentDocument.body) {
              watchFrame(el.contentDocument);
              stack.push(el.contentDocument.body);
            }
          } catch (_) {}
        }
      }
    };
    // Pop order is reversed; walk children in document order by pushing in reverse.
    walk(document.documentElement);

    // Text blocks, in document order.
    const blocks = [...blockText.keys()];
    blocks.sort((a, b) => (a === b ? 0 : a.compareDocumentPosition(b) & Node.DOCUMENT_POSITION_FOLLOWING ? -1 : 1));
    const maxTexts = opts.maxTexts || 1500;
    const rowIds = new Map();
    for (const b of blocks) {
      if (texts.length >= maxTexts) break;
      const s = squash(blockText.get(b).join(" "), 300);
      if (!s || s.length < 1) continue;
      if (!visible(b)) continue;
      const o = { i: keyOf(b), x: s };
      const rk = recKey(b);
      if (rk !== undefined) o.rc = rk;
      const tn = b.tagName;
      if (tn.length === 2 && tn[0] === "H" && tn[1] >= "1" && tn[1] <= "6") o.h = +tn[1];
      const role = b.getAttribute("role");
      if (role === "alert" || role === "status" || b.getAttribute("aria-live")) o.a = 1;
      if (b.tagName === "TD" || b.tagName === "TH") {
        const tr = b.parentElement, table = b.closest("table");
        let w = rowIds.get(tr);
        if (w === undefined) { w = rowIds.size; rowIds.set(tr, w); }
        o.w = w;
        const idx = [...tr.children].indexOf(b);
        const rowHead = idx > 0 ? squash(plainText(tr.children[0]), 50) : "";
        const hr = table && table.querySelector("thead tr, tr:first-child");
        const colHead = hr && hr !== tr && hr.children[idx] ? squash(plainText(hr.children[idx]), 40) : "";
        const c = [rowHead && "row: " + rowHead, colHead && "col: " + colHead].filter(Boolean).join(", ");
        if (c) o.c = c;
      } else if (b.closest("[role=alert],[role=status],[aria-live]")) o.a = 1;
      texts.push(o);
    }

    // Records that have visible content, with their label (heading/first cells).
    const records = [];
    const seenRec = new Set([...els.filter((e) => e.rc !== undefined && !(e.f || "").includes("latent")).map((e) => e.rc),
      ...texts.filter((t) => t.rc !== undefined).map((t) => t.rc)]);
    for (const [r, o] of recs) {
      if (!seenRec.has(o.k)) continue;
      const lab = ctxOf(r.firstElementChild || r, "");
      if (lab) o.label = lab;
      records.push(o);
    }
    const collList = [...colls.values()].filter((c) => records.some((r) => r.coll === c.k));
    // Attach each pager control to the nearest collection before it.
    for (const e of els) {
      if (!e.pg) continue;
      const pe = el(e.i);
      let best = null;
      for (const [c, co] of colls) {
        if (!collList.includes(co)) continue;
        if (c.compareDocumentPosition(pe) & Node.DOCUMENT_POSITION_FOLLOWING || c.contains(pe)) best = co;
      }
      if (best && best.next === undefined) best.next = e.i;
    }
    return {
      docId: U.docId, version: U.version, url: location.href, title: document.title,
      truncated: U.truncated || undefined,
      els, texts, records, colls: collList, modal: !!modalEl,
      modalName: modalEl ? (nameOf(modalEl, modalEl.tagName, modalEl.getAttribute("role") || "dialog") || "") : null,
      ms: now() - t0,
    };
  };

  // Fields fab typed a secret into: their values never appear in a snapshot.
  const secretEls = new WeakSet();

  // ---------- actions ----------
  const el = (i) => {
    const r = byKey.get(i);
    const e = r && r.deref();
    if (!e || !e.isConnected) throw new Error("stale element e" + i);
    return e;
  };
  U.el = el;
  const target = (e) => {
    // Hidden styled checkbox/radio: act on its visible label instead.
    if (!visible(e) && e.labels) { const l = [...e.labels].find(visible); if (l) return l; }
    return e;
  };

  // Where a person clicks: the middle of the element's own first line of
  // text (a tree item or row holding nested items is clicked on its label,
  // not in the middle of its children), else its first visible box.
  const NEST_TAGS = new Set(["UL", "OL", "TABLE", "MENU"]);
  const NEST_ROLES = new Set(["group", "tree", "menu", "menubar", "list", "listbox", "grid", "treegrid", "table"]);
  const ownTextRect = (e) => {
    const d = e.ownerDocument, w = d.createTreeWalker(e, NodeFilter.SHOW_TEXT);
    for (let n = w.nextNode(), k = 0; n && k < 200; n = w.nextNode(), k++) {
      if (!n.nodeValue.trim()) continue;
      let p = n.parentElement, nested = false;
      while (p && p !== e) {
        if (NEST_TAGS.has(p.tagName) || NEST_ROLES.has(p.getAttribute("role") || "")) { nested = true; break; }
        p = p.parentElement;
      }
      if (nested) continue;
      const rg = d.createRange();
      rg.selectNodeContents(n);
      const r = [...rg.getClientRects()].find((r) => r.width > 0 && r.height > 0);
      if (r) return r;
    }
    return null;
  };
  const clickPoint = (e) => {
    const r = ownTextRect(e) || [...e.getClientRects()].find((r) => r.width > 0 && r.height > 0) || e.getBoundingClientRect();
    return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
  };
  // The deepest element at the point, if it belongs to `e` (else `e`).
  const hitAt = (e, p) => {
    const root = e.getRootNode();
    const h = (root && root.elementFromPoint ? root : e.ownerDocument).elementFromPoint(p.x, p.y);
    return h && (h === e || e.contains(h)) ? h : e;
  };

  U.point = (i) => {
    const e = target(el(i));
    e.scrollIntoView({ block: "center", inline: "center", behavior: "instant" });
    const p = clickPoint(e), off = frameOffset(e);
    return { x: off.x + p.x, y: off.y + p.y };
  };

  U.click = (i) => {
    const e = target(el(i));
    e.scrollIntoView({ block: "center", inline: "center", behavior: "instant" });
    const p = clickPoint(e);
    const h = hitAt(e, p);
    const W = e.ownerDocument.defaultView;
    const o = { bubbles: true, cancelable: true, composed: true, view: W,
      clientX: p.x, clientY: p.y, button: 0, buttons: 1 };
    h.dispatchEvent(new W.PointerEvent("pointerdown", o));
    h.dispatchEvent(new W.MouseEvent("mousedown", o));
    if (e.focus) e.focus({ preventScroll: true });
    h.dispatchEvent(new W.PointerEvent("pointerup", { ...o, buttons: 0 }));
    h.dispatchEvent(new W.MouseEvent("mouseup", { ...o, buttons: 0 }));
    // The click goes where the pointer is and bubbles up through `e`, as a
    // real click would.
    if (h === e || typeof h.click !== "function") e.click();
    else h.click();
    return true;
  };

  const nativeSet = (e, v) => {
    const proto = e.tagName === "TEXTAREA" ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    Object.getOwnPropertyDescriptor(proto, "value").set.call(e, v);
  };

  // Focus and select existing content so a trusted insertText replaces it.
  U.focusClear = (i) => {
    const e = el(i);
    e.scrollIntoView({ block: "center", behavior: "instant" });
    e.focus({ preventScroll: true });
    if (e.isContentEditable) {
      const s = e.ownerDocument.getSelection(), r = e.ownerDocument.createRange();
      r.selectNodeContents(e); s.removeAllRanges(); s.addRange(r);
    } else if (e.select) {
      try { e.select(); } catch (_) { nativeSet(e, ""); }
    }
    return true;
  };

  U.commit = (i) => {
    const e = el(i);
    e.dispatchEvent(new Event("change", { bubbles: true }));
    return true;
  };

  U.fill = (i, v) => {
    const e = el(i);
    e.scrollIntoView({ block: "center", behavior: "instant" });
    e.focus({ preventScroll: true });
    if (e.isContentEditable) {
      U.focusClear(i);
      e.ownerDocument.execCommand("insertText", false, v);
    } else {
      nativeSet(e, v);
      e.dispatchEvent(new InputEvent("input", { bubbles: true, composed: true, inputType: "insertText", data: v }));
    }
    e.dispatchEvent(new Event("change", { bubbles: true }));
    return true;
  };

  // Where a value is about to be typed: the page's and the field's origins,
  // and what kind of field it is.
  U.target = (i) => {
    const e = el(i);
    const tag = e.tagName, type = (e.type || "").toLowerCase();
    const text = ["text", "email", "password", "search", "tel", "url", "number", ""].includes(type);
    const lab = (e.labels && e.labels[0] && e.labels[0].innerText) || e.getAttribute("aria-label") || e.getAttribute("name") || "";
    return {
      origin: location.origin,
      frame: e.ownerDocument.location.origin,
      tag, type,
      ac: (e.getAttribute("autocomplete") || "").toLowerCase(),
      label: squash(lab, 80),
      ph: e.getAttribute("placeholder") || "",
      editable: !e.disabled && !e.readOnly && ((tag === "INPUT" && text) || tag === "TEXTAREA" || e.isContentEditable),
    };
  };

  // The sign-in form on the page: the username, password and one-time-code
  // fields and the button that submits them (as element keys), or the link
  // that opens a sign-in form. Reads the DOM directly (types, autocomplete,
  // names), which the snapshot doesn't carry.
  U.loginFields = () => {
    const all = [];
    const collect = (root) => {
      for (const e of root.querySelectorAll("input")) all.push(e);
      for (const f of root.querySelectorAll("iframe")) { try { if (f.contentDocument) collect(f.contentDocument); } catch (_) {} }
    };
    collect(document);
    const vis = all.filter((e) => e.type !== "hidden" && !e.disabled && !e.readOnly && visible(e));
    const lab = (e) => (e.labels && e.labels[0] && e.labels[0].innerText) || "";
    const words = (e) => [e.name, e.id, e.getAttribute("autocomplete"), e.getAttribute("placeholder"), e.getAttribute("aria-label"), lab(e)].filter(Boolean).join(" ").toLowerCase();
    const ac = (e) => (e.getAttribute("autocomplete") || "").toLowerCase();
    const pass = vis.find((e) => e.type === "password" && !ac(e).includes("new-password") && !hasAny(words(e), ["confirm", "repeat", "again", "new"])) || null;
    const codeWords = (w) => hasAny(w, ["otp", "2fa", "mfa", "totp", "verification", "authenticat", "security code"]) ||
      near(w, "one", "time") || near(w, "two", "factor") || hasWord(w, "code");
    let otp = vis.find((e) => ac(e).includes("one-time-code")) ||
      vis.find((e) => ["text", "tel", "number", ""].includes(e.type) && codeWords(words(e)) && !hasAny(words(e), ["promo", "coupon", "zip", "postal", "country", "discount"])) || null;
    const userWords = ["user", "email", "e-mail", "login", "account", "identifier", "phone"];
    let user = vis.find((e) => (hasWord(ac(e), "username") || hasWord(ac(e), "email")) && e.type !== "password") ||
      vis.find((e) => e.type === "email") ||
      vis.find((e) => ["text", "tel", ""].includes(e.type) && hasAny(words(e), userWords) && !words(e).includes("search")) || null;
    if (pass && user && pass.form && user.form && pass.form !== user.form) user = null;
    if (pass) otp = null;
    if (otp && user === otp) user = null;
    const text = (b) => (b.innerText || b.value || b.getAttribute("aria-label") || "").trim();
    // "Sign up", "Subscribe", "Create an account", "Try it free"…
    const signsUp = (s) => {
      if (hasAny(s, ["signup", "sign up", "subscribe", "get started", "join", "register", "create account", "create an account"])) return true;
      // "try … free" within one line (any line).
      let start = 0;
      for (let k = 0; k <= s.length; k++) {
        if (k < s.length && !isBreak(s.charCodeAt(k))) continue;
        const line = s.slice(start, k);
        const i = line.indexOf("try ");
        if (i >= 0 && line.indexOf(" free", i + 4) >= 0) return true;
        start = k + 1;
      }
      return false;
    };
    // A lone email box whose form signs you up (or subscribes you) isn't a sign-in form.
    if (user && !pass && !otp && user.form) {
      const acts = [...user.form.querySelectorAll("button, input[type=submit]")].map(text).join(" ");
      if (signsUp(lowerAscii(acts))) user = null;
    }
    const main = pass || otp || user;
    const form = main ? main.form : null;
    const btns = [...(form || document).querySelectorAll("button, input[type=submit], [role=button]")].filter((b) => visible(b) && !b.disabled);
    const SUB = ["signin", "sign in", "login", "log in", "continue", "next", "submit", "verify", "confirm", "go"];
    const submit = btns.find((b) => startsWord(lowerAscii(text(b)), SUB)) || btns.find((b) => b.type === "submit") || null;
    let signin = null, alt = null;
    if (!main) {
      const links = [...document.querySelectorAll("a, button, [role=button], [role=link]")].filter(visible);
      signin = links.find((b) => ["signin", "sign in", "login", "log in"].includes(lowerAscii(text(b)))) || null;
      // A passkey or push step: the way to a code from an authenticator app.
      const toCode = ["authenticator app", "authentication app", "verification code"];
      for (const a of ["a", "an", "your"]) for (const b of ["code", "authenticator"]) toCode.push(`use ${a} ${b}`);
      const other = ["more options", "try another way", "other options", "use another method", "choose another method"];
      alt = links.find((b) => hasAny(lowerAscii(text(b)), toCode)) ||
        links.find((b) => other.includes(lowerAscii(text(b)))) || null;
    }
    const k = (e) => (e ? keyOf(e) : null);
    return { user: k(user), pass: k(pass), otp: k(otp), submit: k(submit), signin: k(signin), alt: k(alt), inForm: !!form, userValue: user ? user.value : "" };
  };

  // Hides a field's value from snapshots and from view.
  U.markSecret = (i) => {
    const e = el(i);
    secretEls.add(e);
    if ((e.type || "").toLowerCase() !== "password") e.style.setProperty("-webkit-text-security", "disc");
    return true;
  };

  U.select = (i, label) => {
    const e = el(i);
    const want = label.trim().toLowerCase();
    const opts = [...e.options];
    const o = opts.find((x) => x.text.trim() === label.trim()) ||
      opts.find((x) => x.text.trim().toLowerCase() === want) ||
      opts.find((x) => x.value.toLowerCase() === want) ||
      opts.find((x) => x.text.toLowerCase().includes(want));
    if (!o) throw new Error("no option " + label);
    e.focus({ preventScroll: true });
    e.value = o.value;
    e.dispatchEvent(new Event("input", { bubbles: true }));
    e.dispatchEvent(new Event("change", { bubbles: true }));
    return o.text;
  };

  U.enter = (i) => {
    const e = i == null ? document.activeElement : el(i);
    const o = { key: "Enter", code: "Enter", keyCode: 13, which: 13, bubbles: true, cancelable: true };
    const go = e.dispatchEvent(new KeyboardEvent("keydown", o));
    e.dispatchEvent(new KeyboardEvent("keypress", o));
    e.dispatchEvent(new KeyboardEvent("keyup", o));
    if (go && e.form && e.tagName === "INPUT") e.form.requestSubmit ? e.form.requestSubmit() : e.form.submit();
    return true;
  };

  // Cheap CSS path so REST backends (camofox) can target an element by selector.
  U.selector = (i) => {
    let e = target(el(i));
    const parts = [];
    while (e && e.nodeType === 1 && e.tagName !== "HTML") {
      if (e.id && isAlpha(e.id.charCodeAt(0)) && [...e.id].every((c) => c === "-" || isWord(c.charCodeAt(0)))) { parts.unshift("#" + e.id); break; }
      const p = e.parentNode;
      let s = e.tagName.toLowerCase();
      if (p && p.children) {
        const same = [...p.children].filter((x) => x.tagName === e.tagName);
        if (same.length > 1) s += `:nth-of-type(${same.indexOf(e) + 1})`;
      }
      parts.unshift(s);
      e = p && p.host ? p.host : p;
    }
    return parts.join(" > ");
  };
})();
