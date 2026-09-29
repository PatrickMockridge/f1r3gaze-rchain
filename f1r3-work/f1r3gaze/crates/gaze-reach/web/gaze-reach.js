// gaze-reach.js — F1R3Gaze's reach tier: runs f1r3lang pages in a stock
// browser. The module (gaze_reach.wasm) holds the tab executive and the DOM
// protocol; this file is its host: the DOM backend over the page's real
// DOM, the frame loop, and the net, store and nav services.
//
//   <script type="application/f1r3lang" src="app.knf" integrity="blake2b-256:..."></script>
//   <script type="module" src="gaze-reach.js"></script>
//
// JavaScript here is the host, never the page: pages are f1r3lang only.

const WASM_URL = new URL("gaze_reach.wasm", import.meta.url);
const enc = new TextEncoder();
const dec = new TextDecoder();
const VOID = new Set(["br", "img", "input", "meta", "link", "hr", "area", "base", "col", "source", "wbr"]);
const EVENTS = ["click", "dblclick", "input", "change", "keydown", "keyup", "pointerdown", "pointerup",
  "mousedown", "mouseup", "focusin", "focusout", "submit", "contextmenu"];

// --- node table: every node the module sees gets a stable u32 ------------
const nodes = [null];
const ids = new WeakMap();
function idOf(n) {
  let i = ids.get(n);
  if (i === undefined) { i = nodes.length; nodes.push(n); ids.set(n, i); }
  return i;
}
const node = (i) => nodes[i] || null;
const attachedNode = (n) => n && (n === document || document.contains(n));

function escapeText(s, attr) {
  s = s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  return attr ? s.replace(/"/g, "&quot;") : s;
}
// The canonical serialisation, identical to the native browser's: elements,
// attributes in order, text; no comments, no doctype. The module hashes it.
function serialize(n, out) {
  if (n.nodeType === Node.TEXT_NODE) { out.push(escapeText(n.data, false)); return; }
  if (n.nodeType === Node.DOCUMENT_NODE) { for (const c of n.childNodes) serialize(c, out); return; }
  if (n.nodeType !== Node.ELEMENT_NODE) return;
  const tag = n.localName;
  out.push("<", tag);
  for (const a of n.attributes) out.push(" ", a.name, '="', escapeText(a.value, true), '"');
  out.push(">");
  if (VOID.has(tag)) return;
  for (const c of n.childNodes) serialize(c, out);
  out.push("</", tag, ">");
}

function textOf(n) {
  if (n.nodeType === Node.TEXT_NODE) return n.data;
  let s = "";
  for (const c of n.childNodes) s += textOf(c);
  return s;
}

function build(f, refs) {
  if ("text" in f) return [document.createTextNode(f.text)];
  if ("html" in f) {
    const t = document.createElement("template");
    t.innerHTML = f.html;
    for (const s of t.content.querySelectorAll("script")) s.type = "text/inert"; // inert
    return [...t.content.childNodes];
  }
  const el = document.createElement(f.el);
  for (const [k, v] of f.attrs) el.setAttribute(k, v);
  if (f.ref !== null) refs.push([f.ref, idOf(el)]);
  for (const c of f.children) for (const k of build(c, refs)) el.appendChild(k);
  return [el];
}

function setClass(el, op, c) {
  const cs = (el.getAttribute("class") || "").split(/\s+/).filter(Boolean);
  const has = cs.includes(c);
  let out = cs;
  if ((op === "add" || op === "toggle") && !has) out = [...cs, c];
  if ((op === "remove" || op === "toggle") && has) out = cs.filter((x) => x !== c);
  if (out.length) el.setAttribute("class", out.join(" ")); else el.removeAttribute("class");
}

function setStyle(el, k, v) {
  let decls = (el.getAttribute("style") || "").split(";").map((d) => d.split(":"))
    .filter((p) => p.length >= 2).map(([a, ...b]) => [a.trim(), b.join(":").trim()]);
  decls = decls.filter(([a]) => a !== k);
  if (v !== null) decls.push([k, v]);
  if (decls.length) el.setAttribute("style", decls.map(([a, b]) => `${a}: ${b}`).join("; "));
  else el.removeAttribute("style");
}

function applyWrite(w) {
  const [op, i] = w;
  const el = node(i);
  if (!attachedNode(el)) return null;
  const refs = [];
  switch (op) {
    case "setAttr": el.setAttribute(w[2], w[3]); break;
    case "removeAttr": el.removeAttribute(w[2]); break;
    case "setText": el.replaceChildren(document.createTextNode(w[2])); break;
    case "setValue": el.setAttribute("value", w[2]); if ("value" in el) el.value = w[2]; break;
    case "class": setClass(el, w[2], w[3]); break;
    case "style": setStyle(el, w[2], w[3]); break;
    case "insert": {
      const made = build(w[3], refs);
      ({ append: () => el.append(...made), prepend: () => el.prepend(...made), before: () => el.before(...made),
         after: () => el.after(...made), replaceWith: () => el.replaceWith(...made) })[w[2]]();
      break;
    }
    case "setHTML": el.replaceChildren(...build({ html: w[2] }, refs)); break;
    case "remove": el.remove(); break;
    case "focus": el.focus && el.focus(); break;
    case "blur": el.blur && el.blur(); break;
  }
  return refs;
}

function hostOp(c) {
  const n = node(c.n);
  switch (c.op) {
    case "root": return idOf(document);
    case "parent": return n && n.parentNode ? idOf(n.parentNode) : null;
    case "children": return n ? [...n.childNodes].filter((x) => x.nodeType !== Node.COMMENT_NODE && x.nodeType !== Node.DOCUMENT_TYPE_NODE).map(idOf) : [];
    case "attached": return attachedNode(n);
    case "isElement": return !!n && n.nodeType === Node.ELEMENT_NODE;
    case "attr":
      if (!n || !n.getAttribute) return null;
      if (c.k === "value" && "value" in n) return n.value;
      return n.getAttribute(c.k);
    case "text": return n ? textOf(n) : "";
    case "query":
      try { return c.all ? [...n.querySelectorAll(c.sel)].map(idOf) : [n.querySelector(c.sel)].filter(Boolean).map(idOf); }
      catch (e) { return { err: String(e.message || e) }; }
    case "matches":
      try { return !!n && n.nodeType === Node.ELEMENT_NODE && n.matches(c.sel); }
      catch (e) { return { err: String(e.message || e) }; }
    case "rect": {
      if (!n || !n.getBoundingClientRect) return null;
      const r = n.getBoundingClientRect();
      return [Math.round(r.x), Math.round(r.y), Math.round(r.width), Math.round(r.height)];
    }
    case "apply": return c.writes.map(applyWrite);
    case "serialize": { const out = []; serialize(document, out); return out.join(""); }
  }
  return null;
}

// --- the module ------------------------------------------------------------
let wasm, mem, lastAnswer = new Uint8Array(0);
const bytesIn = (b) => { const p = wasm.gaze_alloc(b.length); new Uint8Array(mem.buffer, p, b.length).set(b); return [p, b.length]; };
const strIn = (s) => bytesIn(enc.encode(s));
const result = (n) => { const p = wasm.gaze_alloc(n); wasm.gaze_result_read(p); const b = new Uint8Array(mem.buffer, p, n).slice(); wasm.gaze_free(p, n); return b; };

async function instantiate() {
  const imports = {
    gaze: {
      host_call(p, n) {
        const cmd = JSON.parse(dec.decode(new Uint8Array(mem.buffer, p, n)));
        lastAnswer = enc.encode(JSON.stringify(hostOp(cmd) ?? null));
        return lastAnswer.length;
      },
      host_result(out) { new Uint8Array(mem.buffer, out, lastAnswer.length).set(lastAnswer); },
    },
  };
  const { instance } = await WebAssembly.instantiateStreaming(fetch(WASM_URL), imports);
  wasm = instance.exports;
  mem = wasm.memory;
}

// --- services ------------------------------------------------------------
const DB = "f1r3gaze-store:" + location.origin;
function idb() {
  return new Promise((ok, no) => {
    const r = indexedDB.open(DB, 1);
    r.onupgradeneeded = () => r.result.createObjectStore("kv");
    r.onsuccess = () => ok(r.result);
    r.onerror = () => no(r.error);
  });
}
async function storeOp(req) {
  const db = await idb();
  const tx = db.transaction("kv", req.op === "get" || req.op === "list" ? "readonly" : "readwrite");
  const s = tx.objectStore("kv");
  const done = (r) => new Promise((ok, no) => { r.onsuccess = () => ok(r.result); r.onerror = () => no(r.error); });
  switch (req.op) {
    case "get": return { value: (await done(s.get(req.key))) ?? null };
    case "put": await done(s.put(req.value, req.key)); return { ok: true };
    case "del": await done(s.delete(req.key)); return { ok: true };
    case "list": return { keys: (await done(s.getAllKeys())).filter((k) => k.startsWith(req.key)).sort() };
  }
}

function sameOrigin(u) { try { return new URL(u, location.href).origin === location.origin; } catch { return false; } }

class Tab {
  constructor(handle) { this.h = handle; this.pending = true; this.deadline = null; }
  schedule() { if (!this.raf) this.raf = requestAnimationFrame((t) => { this.raf = 0; this.frame(t); }); }
  frame(now) {
    const out = JSON.parse(dec.decode(result(wasm.gaze_frame(this.h, now))));
    for (const [lvl, text] of out.console) (console[lvl] || console.log)("[f1r3lang]", text);
    for (const r of out.net) this.fetch(r);
    for (const r of out.store) storeOp(r).catch((e) => ({ err: "io" })).then((a) => {
      const [p, n] = strIn(JSON.stringify(a)); wasm.gaze_store_done(this.h, r.id, p, n); wasm.gaze_free(p, n); this.schedule();
    });
    for (const r of out.nav) {
      const url = new URL(r.url || ".", location.href);
      if (r.op === "back") history.back();
      else if (url.origin !== location.origin) window.open(url, "_blank", "noopener");
      else if (r.op === "replace") location.replace(url); else if (r.op === "go") location.assign(url);
    }
    if (out.busy || out.net.length || out.store.length) this.schedule();
    else if (out.deadline !== null) setTimeout(() => this.schedule(), Math.max(0, out.deadline - performance.now()));
  }
  async fetch(r) {
    let status = 0, headers = [], body = new Uint8Array(0), fail = "";
    try {
      if (!sameOrigin(r.url)) throw new Error("the reach tier fetches from this origin only");
      const init = { method: r.method, headers: r.headers, credentials: "omit", redirect: "error" };
      if (r.method !== "GET" && r.method !== "HEAD") init.body = r.body;
      const res = await fetch(new URL(r.url, location.href), init);
      status = res.status; headers = [...res.headers]; body = new Uint8Array(await res.arrayBuffer());
    } catch (e) { fail = String(e.message || e) || "network"; }
    const [hp, hl] = strIn(JSON.stringify(headers));
    const [bp, bl] = bytesIn(body);
    const [ep, el] = strIn(fail);
    wasm.gaze_net_done(this.h, r.id, status, hp, hl, bp, bl, ep, el);
    wasm.gaze_free(hp, hl); wasm.gaze_free(bp, bl); wasm.gaze_free(ep, el);
    this.schedule();
  }
  event(e) {
    if (!e.target || !(e.target instanceof Node)) return;
    const f = { type: e.type };
    if ("clientX" in e) { f.x = Math.round(e.pageX); f.y = Math.round(e.pageY); f.button = e.button; }
    if ("key" in e) { f.key = e.key; f.code = e.code; f.repeat = e.repeat; }
    if (e.type === "input" || e.type === "change") f.value = e.target.value ?? "";
    if ("shiftKey" in e) f.mods = ["shift", "ctrl", "alt", "meta"].filter((m) => e[m + "Key"]);
    delete f.type;
    const [tp, tl] = strIn(e.type), [fp, fl] = strIn(JSON.stringify(f));
    const flags = wasm.gaze_dispatch(this.h, idOf(e.target), tp, tl, fp, fl, e.bubbles ? 1 : 0);
    wasm.gaze_free(tp, tl); wasm.gaze_free(fp, fl);
    if (flags & 1) e.preventDefault();
    if (flags & 2) e.stopPropagation();
    this.schedule();
  }
}

function seed() { return crypto.getRandomValues(new Uint8Array(32)); }

async function boot() {
  const scripts = [...document.querySelectorAll('script[type="application/f1r3lang"]')];
  if (!scripts.length) return;
  await instantiate();
  let tab = null;
  for (const s of scripts) {
    let ok, h;
    if (s.src) {
      const b = new Uint8Array(await (await fetch(s.src, { credentials: "omit" })).arrayBuffer());
      const [p, n] = bytesIn(b), [ip, il] = strIn(s.getAttribute("integrity") || "");
      if (!tab) { const [sp] = bytesIn(seed()); h = wasm.gaze_load_knf(p, n, ip, il, sp); ok = h !== 0; }
      else ok = wasm.gaze_add_knf(tab.h, p, n, ip, il) === 1;
    } else {
      const [p, n] = strIn(s.textContent), [ip, il] = strIn(s.getAttribute("imports") || ""), [lp, ll] = strIn(s.getAttribute("level") || "");
      if (!tab) { const [sp] = bytesIn(seed()); h = wasm.gaze_load_text(p, n, ip, il, lp, ll, sp); ok = h !== 0; }
      else ok = wasm.gaze_add_text(tab.h, p, n, ip, il, lp, ll) === 1;
    }
    if (!ok) { console.error("[f1r3gaze] script refused:", dec.decode(result(wasm.gaze_result_len())), s); return; }
    if (!tab) tab = new Tab(h);
  }
  for (const t of EVENTS) document.addEventListener(t, (e) => tab.event(e), { capture: true });
  tab.schedule();
}

if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", boot); else boot();
