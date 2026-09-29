// fab scraping runtime: finds repeated items, describes their fields as
// structural paths (an LLM maps requested fields to paths once per layout),
// then extracts every item and every detail page in code. Installed once per
// document as window.__fs.
(() => {
  if (window.__fs) return;
  // Text helpers: plain string code, no regular expressions.
  // White space as JS's \s and String.prototype.trim define it.
  const isWs = (n) => n === 32 || (n >= 9 && n <= 13) || n === 0xa0 || n === 0x1680 || (n >= 0x2000 && n <= 0x200a) ||
    n === 0x2028 || n === 0x2029 || n === 0x202f || n === 0x205f || n === 0x3000 || n === 0xfeff;
  const isBreak = (n) => n === 10 || n === 13 || n === 0x2028 || n === 0x2029;
  const isDigit = (n) => n >= 48 && n <= 57;
  const isWord = (n) => isDigit(n) || (n >= 65 && n <= 90) || (n >= 97 && n <= 122) || n === 95;
  // Runs of white space become one space; ends trimmed.
  const norm = (s) => {
    s = s || "";
    const parts = [];
    for (let i = 0; i < s.length;) {
      while (i < s.length && isWs(s.charCodeAt(i))) i++;
      const j = i;
      while (i < s.length && !isWs(s.charCodeAt(i))) i++;
      if (i > j) parts.push(s.slice(j, i));
    }
    return parts.join(" ");
  };
  // The words of a class attribute (or any white-space separated list).
  const tokens = (s) => norm(String(s)).split(" ");
  // Length of the longest run of digits.
  const digitRun = (s) => {
    let best = 0;
    for (let i = 0, r = 0; i < s.length; i++) best = Math.max(best, (r = isDigit(s.charCodeAt(i)) ? r + 1 : 0));
    return best;
  };
  const allDigits = (s) => s.length > 0 && [...s].every((c) => isDigit(c.charCodeAt(0)));
  // Leading digits then "|" (a row prefix, "1|"): its length, else 0.
  const rowPrefix = (s) => {
    let i = 0;
    while (i < s.length && isDigit(s.charCodeAt(i))) i++;
    return i > 0 && s[i] === "|" ? i + 1 : 0;
  };
  const NOT_DATA = new Set(["class", "style", "id", "slot", "part", "tabindex", "role", "hidden", "lang", "dir", "draggable", "href", "src"]);
  const SKIP = new Set(["SCRIPT", "STYLE", "NOSCRIPT", "TEMPLATE", "SVG", "PATH", "IFRAME", "HEAD", "META", "LINK"]);
  const live = (el) => el.ownerDocument === document;
  // Text as shown: spaces collapsed, paragraph and line breaks kept (as one
  // "\n"). A fetched page isn't rendered, so its breaks come from block tags.
  const BLOCK = new Set(["P", "DIV", "BR", "LI", "TR", "H1", "H2", "H3", "H4", "H5", "H6", "BLOCKQUOTE", "PRE", "UL", "OL", "DL", "DT", "DD", "TABLE", "SECTION", "ARTICLE", "HEADER", "FOOTER", "FIGCAPTION"]);
  const HEADING = new Set(["H1", "H2", "H3", "H4", "H5", "H6"]);
  const rendered = (el) => {
    let out = "";
    const walk = (n) => {
      if (n.nodeType === 3) return void (out += n.textContent);
      if (n.nodeType !== 1 || SKIP.has(n.tagName)) return;
      const b = BLOCK.has(n.tagName);
      if (b) out += "\n";
      for (const c of n.childNodes) walk(c);
      if (b && n.tagName !== "BR") out += "\n";
    };
    walk(el);
    return out;
  };
  const lines = (s) =>
    (s || "")
      .split("\n")
      .map(norm)
      .filter(Boolean)
      .join("\n");
  const textOf = (el) => lines(live(el) ? el.innerText ?? rendered(el) : rendered(el));
  const hidden = (el) => {
    if (el.hidden || el.getAttribute("aria-hidden") === "true") return true;
    if (!live(el)) {
      // An inline "display: none".
      const st = el.getAttribute("style") || "";
      for (let i = st.indexOf("display:"); i >= 0; i = st.indexOf("display:", i + 1)) {
        let j = i + 8;
        while (j < st.length && isWs(st.charCodeAt(j))) j++;
        if (st.startsWith("none", j)) return true;
      }
      return false;
    }
    return el.getClientRects().length === 0 && getComputedStyle(el).display === "none";
  };
  // Stable classes only: no hashes, no state.
  const STATE = ["active", "current", "selected", "on", "open", "is-", "has-", "hover", "focus", "odd", "even", "first", "last", "visible", "hidden", "show"];
  // A generated class: 3+ digits in a row, two capitals then a digit (on one
  // line), or "__" and 5+ lowercase letters or digits.
  const hashy = (c) => {
    if (digitRun(c) >= 3) return true;
    let caps = 0;
    for (let i = 0; i < c.length; i++) {
      const n = c.charCodeAt(i);
      if (isBreak(n)) caps = 0;
      else if (caps >= 2 && isDigit(n)) return true;
      else if (n >= 65 && n <= 90) caps++;
    }
    for (let i = c.indexOf("__"); i >= 0; i = c.indexOf("__", i + 1)) {
      let k = i + 2;
      while (k < c.length && (isDigit(c.charCodeAt(k)) || (c.charCodeAt(k) >= 97 && c.charCodeAt(k) <= 122))) k++;
      if (k - i - 2 >= 5) return true;
    }
    return false;
  };
  const classes = (el) =>
    [...el.classList].filter((c) => c.length < 32 && !hashy(c) && !STATE.some((w) => c.startsWith(w))).slice(0, 3);
  const sig = (el) => el.tagName.toLowerCase() + classes(el).map((c) => "." + c).join("");
  const seg = (el) => {
    const s = sig(el);
    const p = el.parentElement;
    if (!p) return s;
    const same = [...p.children].filter((c) => sig(c) === s);
    return same.length > 1 ? `${s}:${same.indexOf(el) + 1}` : s;
  };
  const cssEsc = (c) => (window.CSS && CSS.escape ? CSS.escape(c) : c);
  // CSS selector of an element from its nearest id ancestor (or body).
  const cssPath = (el) => {
    const parts = [];
    for (let e = el; e && e.tagName !== "BODY" && e.tagName !== "HTML"; e = e.parentElement) {
      if (e.id && digitRun(e.id) < 4) {
        parts.unshift("#" + cssEsc(e.id));
        return parts.join(" > ");
      }
      let s = e.tagName.toLowerCase() + classes(e).map((c) => "." + cssEsc(c)).join("");
      const p = e.parentElement;
      if (p && [...p.children].filter((c) => c.matches(s)).length > 1 && e !== el) {
        s += `:nth-child(${[...p.children].indexOf(e) + 1})`;
      }
      parts.unshift(s);
    }
    return "body > " + parts.join(" > ");
  };
  const abs = (doc, u) => {
    // Already absolute: as written (resolving would add a trailing "/").
    const t = u.trim();
    let i = 0;
    const n0 = t.charCodeAt(0) | 32;
    if (n0 >= 97 && n0 <= 122) {
      i = 1;
      while (i < t.length && (isWord(t.charCodeAt(i)) || "+.-".includes(t[i])) && t[i] !== "_") i++;
      if (t.startsWith("://", i)) return t;
    }
    try {
      return new URL(u, doc.__base || doc.baseURI || location.href).href;
    } catch {
      return u;
    }
  };
  const RICH = new Set(["P", "I", "B", "EM", "STRONG", "A", "CODE", "PRE", "BR", "SPAN", "U", "S", "SMALL", "SUB", "SUP", "BLOCKQUOTE", "UL", "OL", "LI", "MARK", "DEL", "INS", "Q", "CITE", "FONT", "ABBR", "TIME", "KBD"]);
  const LABELY = new Set(["DT", "TH", "LABEL", "B", "STRONG"]);
  const LABEL_CLASS = new Set(["k", "key", "label", "name", "term"]);
  const isLabel = (el, t) =>
    t.length > 0 && t.length <= 40 && (LABELY.has(el.tagName) || tokens(el.className || "").some((c) => LABEL_CLASS.has(c)) || t.endsWith(":"));
  const labelValue = (el, t) => {
    const n = el.nextElementSibling;
    if (n) return textOf(n);
    const p = el.parentElement;
    if (!p) return "";
    const v = norm(textOf(p).replace(t, ""));
    let i = 0;
    while (i < v.length && (":–-".includes(v[i]) || isWs(v.charCodeAt(i)))) i++;
    return v.slice(i);
  };

  // An item's own attributes as [name, true | short string] (a valueless
  // attribute is a boolean): a small bounded set, no attribute named in advance.
  const MAX_ATTRS = 8;
  const MAX_ATTR_VALUE = 40;
  const OWN_SKIP = ["aria-label", "title", "datetime", "content", "data-id"];
  const ownAttrs = (el) => {
    const out = [];
    for (const at of el.attributes) {
      const a = at.name;
      if (!/^[a-z][a-z0-9_-]*$/.test(a) || a.length > 32 || NOT_DATA.has(a) || a.startsWith("aria-") || a.startsWith("on") || OWN_SKIP.includes(a)) continue;
      const v = norm(at.value);
      if (v.length > MAX_ATTR_VALUE) continue;
      out.push([a, v === "" ? true : v]);
    }
    return out;
  };
  // Those of each item that tell it from the others (not shared by all).
  const distinctAttrs = (items) => {
    const all = items.map((it) => ownAttrs(it[0]));
    const count = new Map();
    for (const as of all) for (const [a, v] of as) count.set(a + "=" + v, (count.get(a + "=" + v) || 0) + 1);
    return all.map((as) => Object.fromEntries(as.filter(([a, v]) => items.length < 2 || count.get(a + "=" + v) < items.length).slice(0, MAX_ATTRS)));
  };

  // All leaves under `roots` as [path, value]: texts, link targets, labels.
  // Paths are relative to the roots (prefixed by the root's index when an
  // item spans several rows).
  const leaves = (roots, doc, cut = 160) => {
    const out = [];
    const seen = new Set();
    const add = (p, v) => {
      if (v == null || v === "" || seen.has(p)) return;
      seen.add(p);
      out.push([p, v.length > cut ? v.slice(0, cut - 3) + "…" : v]);
    };
    roots.forEach((root, ri) => {
      const pre = roots.length > 1 ? ri + "|" : "";
      // A thread's item contains its replies, items like itself: they are
      // theirs, not its fields.
      const own = sig(root);
      const walk = (el, path) => {
        if (SKIP.has(el.tagName) || hidden(el)) return;
        if (el !== root && sig(el) === own) return;
        const direct = [...el.childNodes].some((n) => n.nodeType === 3 && n.textContent.trim());
        // A rich-text block (a comment, a description: paragraphs and inline
        // formatting) is one value, whatever tag its text starts in.
        const rich = el.children.length > 0 && [...el.children].every((c) => RICH.has(c.tagName));
        const t = direct || rich || !el.children.length ? textOf(el) : "";
        if (t) {
          add(pre + (path || "."), t);
          if (isLabel(el, t)) add("label:" + (t.endsWith(":") ? t.slice(0, -1) : t), labelValue(el, t));
        }
        if (el.tagName === "A" && el.getAttribute("href")) add(pre + (path || ".") + "@href", abs(doc, el.getAttribute("href")));
        if (el.tagName === "IMG") {
          add(pre + (path || ".") + "@alt", el.getAttribute("alt"));
          add(pre + (path || ".") + "@src", el.getAttribute("src") && abs(doc, el.getAttribute("src")));
        }
        for (const a of ["aria-label", "title", "datetime", "content", "data-id"]) {
          const v = el.getAttribute(a);
          if (v && norm(v)) add(pre + (path || ".") + "@" + a, norm(v));
        }
        // A custom element (a web component) carries its data as attributes
        // (<shreddit-comment author="…" depth="0" …>): offer the short ones.
        // The item's own attributes too, valueless ones as "true".
        if (el === root && !el.tagName.includes("-")) {
          for (const [a, v] of ownAttrs(el)) add(pre + (path || ".") + "@" + a, v === true ? "true" : v);
        }
        if (el.tagName.includes("-")) {
          for (const at of el.attributes) {
            const a = at.name;
            if (NOT_DATA.has(a) || a.startsWith("aria-") || a.startsWith("on") || ["aria-label", "title", "datetime", "content", "data-id"].includes(a)) continue;
            const v = norm(at.value);
            if (v && v.length <= 2000) add(pre + (path || ".") + "@" + a, v);
          }
        }
        // Repeated children (tags, labels): also all of them, as one list path.
        const groups = new Map();
        for (const c of el.children) {
          const g = sig(c);
          if (!groups.has(g)) groups.set(g, []);
          groups.get(g).push(c);
        }
        for (const [g, cs] of groups) {
          if (cs.length < 2) continue;
          const vals = cs.filter((c) => !hidden(c)).map(textOf).filter(Boolean);
          if (vals.length >= 2) add(pre + (path ? path + ">" : "") + g + "*", vals.join(" · "));
        }
        for (const c of el.children) walk(c, (path ? path + ">" : "") + seg(c));
      };
      walk(root, "");
    });
    return out;
  };
  // List paths ("…*") extract as arrays.
  const LIST_SEP = " · ";

  // Repeated items: sibling groups sharing a signature. An item spans the
  // rows after it when every item is followed by the same kind of rows (a
  // title row and its details row).
  const groups = (doc) => {
    const out = [];
    const body = doc.body;
    if (!body) return out;
    const all = body.querySelectorAll("*");
    const parents = new Set();
    for (const el of all) if (el.children.length >= 3 && !SKIP.has(el.tagName)) parents.add(el);
    for (const p of parents) {
      if (hidden(p)) continue;
      const by = new Map();
      for (const c of p.children) {
        if (SKIP.has(c.tagName)) continue;
        const s = sig(c);
        if (!by.has(s)) by.set(s, []);
        by.get(s).push(c);
      }
      for (const [s, els] of by) {
        const withText = els.filter((e) => textOf(e).length > 0);
        if (withText.length < 3) continue;
        // Trailing rows: the siblings after each item, up to the next one.
        const kids = [...p.children];
        const trails = els.map((e) => {
          const t = [];
          for (let i = kids.indexOf(e) + 1; i < kids.length && sig(kids[i]) !== s; i++) t.push(kids[i]);
          return t;
        });
        const k = trails.slice(0, -1).map((t) => t.map(sig).join(","));
        const common = k.length && k.every((x) => x === k[0]) && k[0] ? trails[0].length : 0;
        const items = els.map((e, i) => [e, ...trails[i].slice(0, common)]);
        const sel = cssPath(p) + " |> " + s;
        const text = items.reduce((a, it) => a + it.reduce((b, e) => b + textOf(e).length, 0), 0);
        out.push({ sel, span: common, n: items.length, text, items });
      }
    }
    // Bigger and richer first; drop a group nested inside a better one's single item.
    out.sort((a, b) => b.n * Math.min(b.text / b.n, 200) - a.n * Math.min(a.text / a.n, 200));
    return out;
  };
  // `sel` is "<parent css> |> <item signature>".
  // The page around the items: its heading, title and the list's own heading.
  const pageLeaves = (doc, sel) => {
    const out = [];
    const h1 = doc.querySelector("main h1, h1");
    if (h1 && textOf(h1)) out.push(["page|h1", textOf(h1)]);
    if (doc.title) out.push(["page|title", norm(doc.title)]);
    const crumbs = doc.querySelector("nav.crumbs, .breadcrumb, .breadcrumbs, [aria-label=breadcrumb]");
    if (crumbs) out.push(["page|breadcrumb", textOf(crumbs)]);
    const first = sel && itemsOf(doc, sel, 0)[0];
    if (first) {
      for (let e = first[0].parentElement; e && e !== doc.body; e = e.parentElement) {
        let h = e.previousElementSibling;
        while (h && !HEADING.has(h.tagName)) h = h.previousElementSibling;
        if (h) {
          out.push(["page|list heading", textOf(h)]);
          break;
        }
      }
    }
    return out;
  };
  const itemsOf = (doc, sel, span) => {
    const [ps, s] = sel.split(" |> ");
    let ps2 = ps;
    let parents = [...doc.querySelectorAll(ps)];
    // A re-rendered list may have shifted position: drop nth-child steps.
    if (!parents.length) {
      let loose = "";
      for (let i = 0; i < ps.length;) {
        if (ps.startsWith(":nth-child(", i)) {
          let j = i + 11;
          while (j < ps.length && isDigit(ps.charCodeAt(j))) j++;
          if (j > i + 11 && ps[j] === ")") {
            i = j + 1;
            continue;
          }
        }
        loose += ps[i++];
      }
      parents = [...doc.querySelectorAll(loose)];
    }
    const els = parents.flatMap((p) => [...p.children].filter((c) => sig(c) === s && !hidden(c)));
    return els.map((e) => {
      const it = [e];
      for (let n = e.nextElementSibling, i = 0; n && i < span; n = n.nextElementSibling, i++) it.push(n);
      return it;
    });
  };
  const keyOf = (it) => {
    const a = it[0].querySelector("a[href]") || (it[0].matches("a[href]") ? it[0] : null);
    return (a ? a.getAttribute("href") + "|" : "") + it.map(textOf).join(" ").slice(0, 300);
  };
  // The part of a value a field spec asks for, by steps in this order (a step
  // that finds nothing leaves no value): "after" / "before": the text after /
  // before the first occurrence of a marker; "part": {"sep", "index"} one of
  // the parts split at sep (index from 0; -1 the last); "take": "number" its
  // first number ("1,234.50" from "Total: $1,234.50"); "not": null when the
  // text contains this (or any of these); "has": the word(s) the text must
  // contain, kept as the value (a yes/no field). "not" and "has" ignore case.
  const firstNumber = (s) => {
    let i = 0;
    while (i < s.length && !isDigit(s.charCodeAt(i))) i++;
    if (i === s.length) return null;
    let j = i + 1;
    while (j < s.length && (isDigit(s.charCodeAt(j)) || ((s[j] === "," || s[j] === ".") && isDigit(s.charCodeAt(j + 1))))) j++;
    return s.slice(i, j);
  };
  const list = (x) => (Array.isArray(x) ? x : [x]).filter((w) => typeof w === "string" && w);
  const refine = (v, spec) => {
    v = String(v);
    if (typeof spec.after === "string" && spec.after) {
      const k = v.indexOf(spec.after);
      if (k < 0) return null;
      v = v.slice(k + spec.after.length).trim();
    }
    if (typeof spec.before === "string" && spec.before) {
      const k = v.indexOf(spec.before);
      if (k < 0) return null;
      v = v.slice(0, k).trim();
    }
    if (spec.part && typeof spec.part.sep === "string" && spec.part.sep) {
      const ps = v.split(spec.part.sep);
      const n = Number(spec.part.index) || 0;
      v = ps[n < 0 ? ps.length + n : n];
      if (v == null) return null;
      v = v.trim();
    }
    if (spec.take === "number") {
      v = firstNumber(v);
      if (v == null) return null;
    }
    const low = v.toLowerCase();
    if (list(spec.not).some((w) => low.includes(w.toLowerCase()))) return null;
    if (spec.has != null) {
      const w = list(spec.has).find((w) => low.includes(w.toLowerCase()));
      if (!w) return null;
      const k = low.indexOf(w.toLowerCase());
      v = low.length === v.length ? v.slice(k, k + w.length) : w;
    }
    return v;
  };
  // `found`, when given, collects the fields whose path is on the page.
  const apply = (map, pairs, found) => {
    const m = new Map(pairs);
    const out = {};
    for (const [f, spec] of Object.entries(map)) {
      if (!spec) {
        out[f] = null;
        continue;
      }
      const path = typeof spec === "string" ? spec : spec.path;
      // An abbreviated path (".addr" for "div.addr", "a" for "h2>a"): the
      // shortest path that ends with it.
      const resolve = (p) => {
        if (m.has(p) || !p) return p;
        // "@author": the item's own attribute (".@author"), else the
        // nearest element's that has it.
        if (p.charCodeAt(0) === 64) {
          if (m.has("." + p)) return "." + p;
          let best = null;
          for (const k of m.keys()) if (k.endsWith(p) && (best === null || k.length < best.length)) best = k;
          if (best !== null) return best;
        }
        const bare = p.slice(rowPrefix(p));
        // A key's last step without its position ("div.addr:2" → "div.addr").
        const last = (k) => {
          const g = k.slice(Math.max(k.lastIndexOf(">"), k.lastIndexOf("|")) + 1);
          const c = g.lastIndexOf(":");
          return c >= 0 && allDigits(g.slice(c + 1)) ? g.slice(0, c) : g;
        };
        const hits = [...m.keys()].filter((k) => k.endsWith(">" + bare) || k.endsWith("|" + bare) || (bare.startsWith(".") && last(k).endsWith(bare.split("@")[0]) && (k.includes("@") === bare.includes("@"))));
        if (hits.length) return hits.sort((a, b) => a.length - b.length)[0];
        // The same element with other presentational classes ("commtext c00"
        // vs "commtext c5a"): same tags and positions, a class in common.
        // Steps of a path: "1|div>a@href" → ["1|", "div", "a", "@href"].
        const segs = (x) => {
          const r = rowPrefix(x);
          const out = r ? [x.slice(0, r)] : [];
          let cur = "";
          for (const c of x.slice(r)) {
            if (c === ">") {
              out.push(cur);
              cur = "";
            } else if (c === "@" && cur) {
              out.push(cur);
              cur = c;
            } else cur += c;
          }
          out.push(cur);
          return out;
        };
        const part = (g) => {
          const [base, nth] = g.split(":");
          const [tag, ...cls] = base.split(".");
          return { tag, cls, nth };
        };
        const like = (a, b) => {
          if (a.startsWith("@") || b.startsWith("@")) return a === b;
          const x = part(a), y = part(b);
          // Only presentational classes may differ (with digits: "c00",
          // "c5a", hashes); word classes carry meaning ("overdue", "paid").
          const words = (c) => c.filter((k) => digitRun(k) === 0).sort().join(".");
          return x.tag === y.tag && x.nth === y.nth && words(x.cls) === words(y.cls);
        };
        const want = segs(p);
        const near = [...m.keys()].filter((k) => {
          const ks = segs(k);
          return ks.length === want.length && ks.every((g, i) => like(g, want[i]));
        });
        if (near.length) return near[0];
        // The same steps at other sibling positions (an item with one row
        // fewer before it): only when exactly one element matches.
        const anyNth = (a, b) => {
          if (a.startsWith("@") || b.startsWith("@")) return a === b;
          const x = part(a), y = part(b);
          const words = (c) => c.filter((k) => digitRun(k) === 0).sort().join(".");
          return x.tag === y.tag && words(x.cls) === words(y.cls);
        };
        // Structural paths only: "label:Phone" is a name, not a position.
        const moved = want.length < 2 || p.startsWith("label:") || p.startsWith("page|") ? [] : [...m.keys()].filter((k) => {
          const ks = segs(k);
          return ks.length === want.length && ks.every((g, i) => anyNth(g, want[i]));
        });
        if (moved.length === 1) return moved[0];
        // A container whose own text is in one child ("td:1" for "td:1>a").
        const under = [...m.keys()].filter((k) => k.startsWith(p + ">") && !k.includes("@") && !k.endsWith("*"));
        return under.length === 1 ? under[0] : p;
      };
      // A paragraph of a rich-text block (a comment's first <p>): the whole
      // block, which is also a leaf. Comments without a <p> have only it.
      const block = (q) => {
        const cut = q.lastIndexOf(">");
        if (cut < 0 || q.includes("@") || q.endsWith("*")) return q;
        // Only the block's first paragraph ("…>p"): a positioned one
        // ("…>p:1") was chosen on purpose.
        const step = q.slice(cut + 1);
        return (step === "p" || step === "li" || step === "blockquote" || step === "pre") && m.has(q.slice(0, cut)) ? q.slice(0, cut) : q;
      };
      const path0 = block(resolve(path));
      let v = m.has(resolve(path0)) ? m.get(resolve(path0)) : null;
      // A list path an item doesn't repeat: its single value, as a list.
      if (v == null && path.endsWith("*")) {
        const one = path.slice(0, -1);
        const hit = [...m.keys()].find((k) => k === one || k.startsWith(one + ":"));
        if (hit) v = m.get(hit);
      }
      if (v != null && found) found.add(f);
      if (v != null && path.endsWith("*")) {
        out[f] = v.split(LIST_SEP).filter(Boolean);
        continue;
      }
      if (v != null && typeof spec === "object") v = refine(v, spec);
      out[f] = v == null || v === "" ? null : v;
    }
    return out;
  };
  const docs = new Map(); // url -> parsed Document
  const docRoots = (doc) => {
    const main = doc.querySelector("main,[role=main],article,#content,#main");
    return [main || doc.body];
  };

  // Up to k samples that together show the most paths (an item with a badge
  // or a missing field among them), in page order.
  const cover = (all, k = 3) => {
    const have = new Set();
    const pick = [];
    for (let r = 0; r < k; r++) {
      let best = -1, gain = 0;
      all.forEach((lv, i) => {
        if (pick.includes(i)) return;
        const g = lv.filter(([p]) => !have.has(p)).length;
        if (g > gain) (best = i), (gain = g);
      });
      if (best < 0) break;
      pick.push(best);
      all[best].forEach(([p]) => have.add(p));
    }
    return pick.sort((a, b) => a - b).map((i) => all[i]);
  };
  // Nesting level of each item in a threaded list (0 = top level), from an
  // explicit depth attribute, else the visual indent (live page), else a
  // spacer's width (fetched page). All 0 when the list isn't threaded.
  const DEPTH = ["aria-level", "indent", "data-depth", "data-level", "depth", "level"];
  const rawLevel = (it) => {
    const e = it[0];
    for (const el of [e, ...e.querySelectorAll(DEPTH.map((a) => `[${a}]`).join(","))].slice(0, 12)) {
      for (const a of DEPTH) {
        const v = el.getAttribute(a);
        if (v != null && allDigits(v.trim())) return +v;
      }
    }
    if (live(e)) {
      const w = document.createTreeWalker(e, NodeFilter.SHOW_TEXT, { acceptNode: (n) => (n.textContent.trim() ? 1 : 3) });
      const t = w.nextNode();
      if (t && t.parentElement) return Math.round(t.parentElement.getBoundingClientRect().left);
    }
    const sp = e.querySelector("img[width]:not([alt]), img[width][alt='']");
    return sp ? +sp.getAttribute("width") || 0 : 0;
  };
  const levels = (items) => {
    const raw = items.map(rawLevel);
    const distinct = [...new Set(raw)].sort((a, b) => a - b);
    return raw.map((r) => (distinct.length > 1 ? distinct.indexOf(r) : 0));
  };
  const D = (url) => (url ? docs.get(url) : document);
  // A threaded item's level, shown with its sample (a reply among the samples).
  const withLevel = (lv, l) => (l > 0 || lv.__threaded ? lv.concat([["item|level", String(l)]]) : lv.concat([["item|level", "0"]]));
  window.__fs = {
    lists(max = 6, url) {
      const d = D(url);
      if (!d) return [];
      return groups(d).slice(0, max).map((g) => {
        const its = g.items.slice(0, 60);
        const lv = levels(its);
        return {
          sel: g.sel,
          span: g.span,
          n: g.n,
          sample: cover(its.map((it, i) => withLevel(leaves(it, d), lv[i]))).map((x) => x.concat(pageLeaves(d, g.sel))),
        };
      });
    },
    sampleLeaves(sel, span, url) {
      const d = D(url);
      const pg = pageLeaves(d, sel);
      const its = itemsOf(d, sel, span).slice(0, 60);
      const lv = levels(its);
      return cover(its.map((it, i) => withLevel(leaves(it, d), lv[i]))).map((x) => x.concat(pg));
    },
    docSamples(urls) {
      return cover(urls.filter((u) => docs.has(u)).map((u) => leaves(docRoots(docs.get(u)), docs.get(u))));
    },
    // The fields of `map` whose path is on none of the sample pages (each
    // [path, value] pairs): a path the mapper named but was never shown.
    unresolved(map, pages) {
      const found = new Set();
      for (const pairs of pages) apply(map, pairs, found);
      return Object.keys(map).filter((f) => map[f] && !found.has(f));
    },
    count(sel, span, url) {
      const d = D(url);
      return d ? itemsOf(d, sel, span).length : 0;
    },
    items(sel, span, map, skip, url) {
      const d = D(url);
      const seen = new Set(skip || []);
      const out = [];
      const pg = pageLeaves(d, sel);
      const all = itemsOf(d, sel, span);
      const lv = levels(all);
      const at = distinctAttrs(all);
      all.forEach((it, i) => {
        const key = keyOf(it);
        if (seen.has(key)) return;
        seen.add(key);
        out.push({ i, key, level: lv[i], attrs: at[i], fields: map ? apply(map, leaves(it, d, 1e9).concat(pg)) : null });
      });
      return out;
    },
    // Leaves of item i (for mapping a link to follow).
    itemLeaves(sel, span, i, url) {
      const d = D(url);
      const it = itemsOf(d, sel, span)[i];
      return it ? leaves(it, d) : [];
    },
    // Fetches pages in parallel into the cache; reports what each holds.
    // Fetches pages in parallel into the cache; reports what each holds.
    // Sites throttle bursts (HTTP 429/503): a throttled page is retried with
    // backoff (Retry-After when given), the host's concurrency halves on each
    // throttle and grows back on success, and a page that never loads isn't
    // cached (opening it then navigates for real).
    async fetch(urls, conc = 8) {
      const res = new Array(urls.length);
      const host = (u) => {
        try {
          return new URL(u).host;
        } catch {
          return "";
        }
      };
      const lim = (window.__fsLimit = window.__fsLimit || {});
      const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
      const queue = urls.map((u, k) => ({ u, k, tries: 0 }));
      let active = 0;
      const one = async () => {
        while (queue.length) {
          const job = queue[0];
          const h = host(job.u);
          const cap = Math.max(1, Math.min(conc, lim[h] ?? conc));
          if (active >= cap) {
            await sleep(40);
            continue;
          }
          queue.shift();
          active++;
          const { u, k } = job;
          try {
            if (!docs.has(u)) {
              const r = await fetch(u, { credentials: "include" });
              if (r.status === 429 || r.status >= 500) {
                job.tries++;
                lim[h] = Math.max(1, Math.floor((lim[h] ?? conc) / 2));
                if (job.tries <= 6) {
                  const ra = parseFloat(r.headers.get("retry-after") || "");
                  const wait = ra > 0 ? Math.min(ra * 1000, 15000) : Math.min(300 * 2 ** job.tries, 8000);
                  active--;
                  await sleep(wait);
                  queue.push(job);
                  continue;
                }
                res[k] = { url: u, ok: false, status: r.status, err: `HTTP ${r.status} after ${job.tries} tries` };
                active--;
                continue;
              }
              if (!r.ok) {
                res[k] = { url: u, ok: false, status: r.status, err: `HTTP ${r.status}` };
                active--;
                continue;
              }
              const html = await r.text();
              const d = new DOMParser().parseFromString(html, "text/html");
              d.__base = r.url || u;
              docs.set(u, d);
              lim[h] = Math.min(conc, (lim[h] ?? conc) + 1);
            }
            const d = docs.get(u);
            const txt = norm([...docRoots(d)].map((e) => e.textContent).join(" "));
            res[k] = { url: u, ok: true, text: txt.length };
          } catch (e) {
            res[k] = { url: u, ok: false, err: String(e) };
          }
          active--;
        }
      };
      await Promise.all(Array.from({ length: Math.min(conc, urls.length) }, one));
      return res;
    },
    // Text length of a page as the browser renders it, in a hidden frame
    // (the list page stays as it is); -1 when the frame can't be read.
    renderedText(url, ms = 1500) {
      return new Promise((res) => {
        const f = document.createElement("iframe");
        f.style.cssText = "position:fixed;left:-10000px;top:0;width:1024px;height:768px;visibility:hidden";
        let done = false;
        const fin = () => {
          if (done) return;
          done = true;
          let n = -1;
          try {
            n = (f.contentDocument.body.innerText || "").trim().length;
          } catch {}
          f.remove();
          res(n);
        };
        f.onload = () => setTimeout(fin, ms);
        setTimeout(fin, 10000);
        f.src = url;
        document.body.appendChild(f);
      });
    },
    // The HTTP status of the page as loaded (0 when unknown).
    status() {
      const n = performance.getEntriesByType("navigation")[0];
      return (n && n.responseStatus) || 0;
    },
    docText(url) {
      const d = docs.get(url);
      return d ? norm([...docRoots(d)].map((e) => e.textContent).join(" ")).length : -1;
    },
    docLeaves(url) {
      const d = url ? docs.get(url) : document;
      return d ? leaves(docRoots(d), d) : [];
    },
    doc(url, map) {
      const d = url ? docs.get(url) : document;
      if (!d) return null;
      const roots = docRoots(d);
      return apply(map, leaves(roots, d, 1e9));
    },
    drop(url) {
      docs.delete(url);
    },
    // Clicks item i's link at `path` (or the item itself) in the live page.
    clickItem(sel, span, i, path) {
      const it = itemsOf(document, sel, span)[i];
      if (!it) return false;
      let target = it[0].matches("a[href],button,[onclick]") ? it[0] : it[0].querySelector("a[href]:not([href='#'])") || it[0];
      if (path) {
        // Without the attribute ("a@href" → "a").
        const at = path.indexOf("@");
        const want = at < 0 ? path : path.slice(0, at);
        const find = (roots) => {
          let hit = null;
          roots.forEach((root, ri) => {
            const pre = roots.length > 1 ? ri + "|" : "";
            const walk = (el, p) => {
              if (hit) return;
              if (pre + (p || ".") === want) hit = el;
              for (const c of el.children) walk(c, (p ? p + ">" : "") + seg(c));
            };
            walk(root, "");
          });
          return hit;
        };
        target = find(it) || target;
      }
      target.scrollIntoView({ block: "center" });
      target.click();
      return true;
    },
    // The control that shows the next page or batch, marked for clicking.
    next(url) {
      const document = D(url);
      // Not a jump within the page ("next" to the next comment) or a
      // control hidden from assistive tech.
      const inPage = (e) => {
        const h = e.getAttribute("href") || "";
        return h.length > 1 && h.startsWith("#") && !h.startsWith("#/") && !h.startsWith("#!");
      };
      const ok = (e) =>
        !e.disabled && e.getAttribute("aria-disabled") !== "true" && e.getAttribute("aria-hidden") !== "true" && !inPage(e) &&
        !tokens(e.className || "").includes("disabled") && !hidden(e);
      const cands = [...document.querySelectorAll("a[href],button,[role=button],[role=link],input[type=button],input[type=submit]")].filter(ok);
      const T = (e) => norm(e.innerText || e.value || "").toLowerCase();
      const L = (e) => (e.getAttribute("aria-label") || e.getAttribute("title") || "").toLowerCase();
      // Starts with one of `ws` as whole words.
      const leads = (s, ws) => ws.some((w) => s.startsWith(w) && !isWord(s.charCodeAt(w.length)));
      const NEXT_SAYS = ["next", "next page", "next ›", "next »", "next →", "next >", "›", "»", "→", ">", "more", "older", "older posts"];
      const MORE = ["load more", "show more", "see more", "view more", "get more", "more results", "more stories", "more posts", "more items", "more products"];
      const saysNext = (s) => NEXT_SAYS.includes(s) || leads(s, MORE) || ["next page", "next results", "next ›", "next »"].some((w) => s.startsWith(w));
      const pick =
        cands.find((e) => tokens(e.getAttribute("rel") || "").includes("next")) ||
        cands.find((e) => L(e).includes("next page") || leads(L(e), ["next"])) ||
        cands.find((e) => saysNext(T(e))) ||
        (() => {
          const cur = document.querySelector('[aria-current="page"], .pagination .active, .pager .current, .current');
          const n = cur && parseInt(norm(cur.textContent), 10);
          return n ? cands.find((e) => T(e) === String(n + 1)) : null;
        })();
      if (!pick) return null;
      if (url) return { text: norm(pick.textContent || pick.getAttribute("aria-label") || ""), href: null };
      document.querySelectorAll("[data-fs-next]").forEach((e) => e.removeAttribute("data-fs-next"));
      pick.setAttribute("data-fs-next", "1");
      return { text: norm(pick.innerText || pick.getAttribute("aria-label") || ""), href: pick.getAttribute("href") ? abs(document, pick.getAttribute("href")) : null };
    },
    clickNext() {
      const e = document.querySelector("[data-fs-next]");
      if (!e) return false;
      e.scrollIntoView({ block: "center" });
      e.click();
      return true;
    },
    scrollEnd() {
      window.scrollTo(0, document.documentElement.scrollHeight);
      const s = document.scrollingElement;
      if (s) s.scrollTop = s.scrollHeight;
      return document.documentElement.scrollHeight;
    },
    state(sel, span) {
      const it = sel ? itemsOf(document, sel, span) : [];
      return { url: location.href, n: it.length, first: it.length ? keyOf(it[0]) : "", h: document.documentElement.scrollHeight, busy: !!document.querySelector('[aria-busy="true"]') };
    },
  };
})();
