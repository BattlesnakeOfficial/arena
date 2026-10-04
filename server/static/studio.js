// Head & Tail Studio (/customizations/studio).
//
// Every board is server-rendered. This script posts an upload to the processing
// endpoint and then only sets attributes: `d`/`fill-rule` on the placeholder paths
// (path.studio-head, path.studio-tail, #studio-closeup-path), the --studio-snake
// colour variable, the board theme class, and the text of the checks (with links into
// the guide). No innerHTML; every path, colour and link is validated before use,
// including what localStorage restores.
(function () {
  "use strict";
  const root = document.getElementById("studio");
  if (!root) return;

  const ENDPOINT = "/customizations/studio/process";
  const STORE = "arena:studio:v1";
  const D_RE = /^[MLQCZ0-9 .\-]*$/;
  const COLOR_RE = /^#[0-9a-f]{6}$/i;
  const CODE_RE = /^[a-z_]{1,40}$/;
  const ANCHOR_RE = /^#[a-z-]{1,24}$/;
  // The guide's sections the checks link to, with what each is about ("Learn more
  // about ..."), from the page (data-guide-topics); anything else gets no link.
  const GUIDE_TOPICS = (() => {
    let pairs = [];
    try { pairs = JSON.parse(root.getAttribute("data-guide-topics") || "[]"); } catch (e) { /* no links */ }
    return new Map((Array.isArray(pairs) ? pairs : []).filter((p) => Array.isArray(p) && p.length === 2 &&
      typeof p[0] === "string" && /^[a-z-]{1,24}$/.test(p[0]) && typeof p[1] === "string" && p[1].length < 80));
  })();
  const MAX_D = 65536;
  const FRAME_MS = 250;
  const SVG_NS = "http://www.w3.org/2000/svg";
  const VIEWS = ["closeup", "live", "all", "game"];
  const $ = (id) => document.getElementById(id);
  const all = (sel) => Array.from(root.querySelectorAll(sel));
  const other = (kind) => (kind === "head" ? "tail" : "head");
  const media = (q) => (window.matchMedia ? matchMedia(q) : { matches: false });
  const wide = media("(min-width: 640px)"); // Close-up and Live side by side
  const sideBySide = media("(min-width: 980px)"); // the checks beside the preview
  const reduceMotion = media("(prefers-reduced-motion: reduce)").matches;
  // createElement(NS) + setAttribute + textContent: the only way this script builds DOM.
  function el(tag, attrs, text, ns) {
    const node = ns ? document.createElementNS(ns, tag) : document.createElement(tag);
    for (const [k, v] of Object.entries(attrs || {})) node.setAttribute(k, v);
    if (text !== undefined) node.textContent = text;
    return node;
  }
  const empty = (node) => { while (node.firstChild) node.removeChild(node.firstChild); };

  const state = {
    kind: "head",
    slots: { head: null, tail: null }, // results: { d, fillRule, lints, info, gaps, example }
    pair: { head: "default", tail: "default" }, // the other slot: "user" or a reference slug
    color: "#ff4f86",
    theme: document.documentElement.getAttribute("data-app-theme") === "dark" ? "dark" : "light",
    view: "closeup",
  };
  // The last processed file, kept in memory only, so Flip/Fit can re-post it and it can
  // move to the other slot: { body, kind, fixes, slot, displaced, info }.
  let current = null;
  let pending = null; // the request in flight; a newer one supersedes it

  // ---- validation -----------------------------------------------------------------
  const cleanLints = (list) =>
    (Array.isArray(list) ? list : [])
      .filter((l) => l && CODE_RE.test(l.code) && ["warn", "tip", "info"].includes(l.severity) &&
        typeof l.message === "string" && l.message.length < 2000)
      .slice(0, 50)
      .map((l) => ({ code: l.code, severity: l.severity, message: l.message,
        fix: l.fix === "flip" || l.fix === "fit" ? l.fix : null,
        guide: typeof l.guide === "string" && ANCHOR_RE.test(l.guide) ? l.guide : null }));
  const cleanGaps = (list) =>
    (Array.isArray(list) ? list : [])
      .filter((g) => Array.isArray(g) && g.length === 2 && g.every((n) => Number.isFinite(n) && n >= 0 && n <= 100))
      .slice(0, 16);
  function cleanSlot(s) {
    if (!s || typeof s.d !== "string" || s.d.length > MAX_D || !D_RE.test(s.d)) return null;
    if (s.fillRule !== "nonzero" && s.fillRule !== "evenodd") return null;
    const lints = s.lints || {};
    return { d: s.d, fillRule: s.fillRule, lints: { head: cleanLints(lints.head), tail: cleanLints(lints.tail) },
      info: cleanLints(s.info), gaps: cleanGaps(s.gaps), example: s.example === true };
  }

  // ---- persistence ----------------------------------------------------------------
  function persist() {
    try {
      localStorage.setItem(STORE, JSON.stringify({ v: 1, kind: state.kind, slots: state.slots, pair: state.pair,
        color: state.color, theme: state.theme, view: state.view }));
    } catch (e) { /* storage full or disabled: the preview still works */ }
  }
  function restore() {
    let saved = null;
    try { saved = JSON.parse(localStorage.getItem(STORE) || "null"); } catch (e) { return; }
    if (!saved || saved.v !== 1) return;
    if (saved.kind === "head" || saved.kind === "tail") state.kind = saved.kind;
    for (const k of ["head", "tail"]) {
      state.slots[k] = cleanSlot(saved.slots && saved.slots[k]);
      const p = saved.pair && saved.pair[k];
      if (p === "user" || refOption(k, p)) state.pair[k] = p;
    }
    if (typeof saved.color === "string" && COLOR_RE.test(saved.color)) state.color = saved.color.toLowerCase();
    if (saved.theme === "light" || saved.theme === "dark") state.theme = saved.theme;
    if (VIEWS.includes(saved.view)) state.view = saved.view;
  }

  // ---- shapes ---------------------------------------------------------------------
  const pairSelect = (kind) => $("studio-pair-" + kind);
  function refOption(kind, slug) {
    const select = pairSelect(kind);
    if (!select || typeof slug !== "string") return null;
    return Array.from(select.options).find((o) => o.value === slug && o.value !== "user") || null;
  }
  function refShape(kind, slug) {
    const o = refOption(kind, slug) || refOption(kind, "default");
    const d = o ? o.getAttribute("data-d") || "" : "";
    const fillRule = o && o.getAttribute("data-fill-rule") === "evenodd" ? "evenodd" : "nonzero";
    const name = !o || o.value === "default" ? "the default " + kind : "the " + o.textContent + " " + kind;
    return { d: D_RE.test(d) ? d : "", fillRule: fillRule, name: name };
  }
  // What a slot shows: the active slot shows the upload (or the default before one);
  // the other slot shows its "Pair with" choice.
  function shapeFor(kind) {
    const slot = state.slots[kind];
    const own = kind === state.kind || state.pair[kind] === "user";
    if (own && slot) return { d: slot.d, fillRule: slot.fillRule, name: (slot.example ? "the example " : "your ") + kind };
    return refShape(kind, kind === state.kind ? "default" : state.pair[kind]);
  }
  function setPath(path, shape) {
    path.setAttribute("d", shape.d);
    path.setAttribute("fill-rule", shape.fillRule);
  }

  // ---- rendering ------------------------------------------------------------------
  function colorName() {
    const preset = all('input[name="studio-color"]').find((i) => i.value === state.color);
    return preset ? preset.getAttribute("data-name").toLowerCase() : "custom-coloured (" + state.color + ")";
  }
  // "Start here" is open until the artist's first upload of their own (the example
  // doesn't count), then closes, and opens again after Clear. Only on those changes, so
  // it stays however the artist leaves it.
  const start = $("studio-start");
  let startFor = null;
  function syncStart(own) {
    if (!start || startFor === own) return;
    startFor = own;
    start.open = !own;
  }
  const ownSlot = (kind) => !!(state.slots[kind] && !state.slots[kind].example);
  function render() {
    const any = !!(state.slots.head || state.slots.tail);
    syncStart(ownSlot("head") || ownSlot("tail"));
    root.classList.toggle("studio-has-upload", any); // the drop zone shrinks to a row
    $("studio-drop-title").textContent = any ? "Choose another drawing" : "Choose a drawing";
    const head = shapeFor("head");
    const tail = shapeFor("tail");
    all("path.studio-head").forEach((p) => setPath(p, head));
    all("path.studio-tail").forEach((p) => setPath(p, tail));
    const active = state.kind === "head" ? head : tail;
    setPath($("studio-closeup-path"), active);
    renderGaps();
    root.style.setProperty("--studio-snake", state.color);
    all(".studio-board").forEach((b) => {
      b.classList.toggle("light", state.theme === "light");
      b.classList.toggle("dark", state.theme === "dark");
    });

    const wearing = "wearing " + head.name + " and " + tail.name;
    const color = colorName();
    const label = (el, text) => el && el.setAttribute("aria-label", text);
    label($("studio-closeup"), "Close-up of " + active.name);
    label(root.querySelector(".studio-live"), "A " + color + " snake " + wearing + ", moving around the board");
    label(root.querySelector(".studio-all"), "Four " + color + " snakes " + wearing + ", facing right, left, up and down");
    all(".studio-game").forEach((b, i) => label(b, (i ? "iPad" : "Phone") + " size: four " + color + " snakes " + wearing));

    renderControls();
    renderLints();
    persist();
  }
  // Red brackets beside the close-up's left edge, where the neck has gaps.
  function renderGaps() {
    const g = $("studio-gaps");
    empty(g);
    const slot = state.slots[state.kind];
    for (const [y0, y1] of slot ? slot.gaps : []) {
      g.appendChild(el("rect", { x: "-4", y: String(y0), width: "3", height: String(Math.max(y1 - y0, 0.5)) }, undefined, SVG_NS));
    }
  }
  function renderControls() {
    if (state.view === "live" && wide.matches) state.view = "closeup"; // shown side by side
    for (const i of all('input[name="studio-kind"]')) i.checked = i.value === state.kind;
    for (const i of all('input[name="studio-theme"]')) i.checked = i.value === state.theme;
    for (const i of all('input[name="studio-view"]')) i.checked = i.value === state.view;
    $("studio-panes").setAttribute("data-view", state.view);
    let hint = "";
    let preset = false;
    for (const i of all('input[name="studio-color"]')) {
      i.checked = i.value === state.color;
      if (i.checked) { preset = true; hint = i.getAttribute("data-hint") || ""; }
    }
    $("studio-swatch-custom").classList.toggle("active", !preset);
    $("studio-color-custom").value = state.color;
    $("studio-color-hint").textContent = hint;
    $("studio-color-hint").hidden = !hint;

    // "Pair with" lists the other kind, plus "Your head"/"Your tail" once uploaded.
    for (const kind of ["head", "tail"]) {
      const select = pairSelect(kind);
      $("studio-pair-" + kind + "-field").hidden = kind === state.kind;
      let mine = Array.from(select.options).find((o) => o.value === "user");
      if (state.slots[kind] && !mine) {
        select.insertBefore(el("option", { value: "user" }, "Your " + kind), select.firstChild);
      } else if (!state.slots[kind] && mine) {
        mine.remove();
      }
      if (state.pair[kind] === "user" && !state.slots[kind]) state.pair[kind] = "default";
      select.value = state.pair[kind];
    }

    const any = state.slots.head || state.slots.tail;
    $("studio-download-head").hidden = !state.slots.head;
    $("studio-download-tail").hidden = !state.slots.tail;
    $("studio-new-version").hidden = !any;
    $("studio-clear").hidden = !any;
    const to = relabelTarget();
    const relabelBtn = $("studio-relabel");
    relabelBtn.hidden = !to;
    if (to === state.kind) relabelBtn.textContent = "Use the file you just uploaded as your " + to;
    else if (to) relabelBtn.textContent = "Use it as a " + to + " instead" + (state.slots[to] ? " (replaces your " + to + ")" : "");
  }

  const fixLabel = (fix) => (fix === "flip" ? "Flip" : "Fit");
  const GUIDE = (root.getAttribute("data-guide") || "").startsWith("/") ? root.getAttribute("data-guide") : "";
  // Point a "Learn more" link at the guide's section for `lint`; false if there's none.
  function setLearn(a, lint) {
    const topic = GUIDE && lint.guide ? GUIDE_TOPICS.get(lint.guide.slice(1)) : null;
    if (!topic) return false;
    a.setAttribute("href", GUIDE + lint.guide);
    empty(a);
    a.append("Learn more", el("span", { class: "vh" }, " about " + topic));
    return true;
  }
  function lintItem(lint, withFix) {
    const warn = lint.severity === "warn";
    const li = el("li", { class: "studio-lint " + lint.severity, "data-code": lint.code });
    li.append(el("span", { class: "studio-lint-icon", "aria-hidden": "true" }, warn ? "!" : "i"),
      el("span", { class: "vh" }, warn ? "Warning: " : "Note: "),
      el("span", { class: "studio-lint-text" }, lint.message));
    const learn = el("a", { class: "studio-learn" });
    if (setLearn(learn, lint)) li.appendChild(learn);
    if (withFix && lint.fix) {
      li.appendChild(el("button", { type: "button", class: "btn sm studio-fix", "data-fix": lint.fix }, fixLabel(lint.fix)));
    }
    return li;
  }
  function fill(list, items) {
    empty(list);
    items.forEach((li) => list.appendChild(li));
  }
  function renderLints() {
    const slot = state.slots[state.kind];
    const canFix = !!slot; // see jobFor
    const warns = slot ? slot.lints[state.kind] : [];
    const tips = slot ? slot.info.filter((l) => l.severity === "tip") : [];
    const infos = slot ? slot.info.filter((l) => l.severity === "info") : [];
    $("studio-lints-empty").hidden = !!slot;
    fill($("studio-warnings"), warns.map((l) => lintItem(l, canFix)));
    fill($("studio-tips"), tips.map((l) => lintItem(l, false)));
    fill($("studio-info"), infos.map((l) => lintItem(l, false)));
    $("studio-details").hidden = infos.length === 0;
    $("studio-details-summary").textContent = "Details (" + infos.length + ")";
    $("studio-pass").hidden = !slot || warns.length > 0;
    $("studio-pass-text").textContent = "Passes every check the official " + state.kind + "s pass.";

    const top = $("studio-top-warning");
    top.hidden = warns.length === 0;
    $("studio-top-learn").hidden = !(warns.length && setLearn($("studio-top-learn"), warns[0]));
    if (warns.length) {
      $("studio-top-warning-text").textContent = warns[0].message;
      const fixBtn = $("studio-top-fix");
      fixBtn.hidden = !(canFix && warns[0].fix);
      if (warns[0].fix) {
        fixBtn.setAttribute("data-fix", warns[0].fix);
        fixBtn.textContent = fixLabel(warns[0].fix);
      }
    }
  }

  // ---- uploads --------------------------------------------------------------------
  // An error is only ever said in the status line. It sits at the top of the upload
  // panel, but "Upload a new version" and "Save preview image" are at the bottom of the
  // page and Fix can be in the checks, so bring it into view (when it isn't already) or
  // the last result seems to stand for the new file.
  function revealStatus() {
    const s = $("studio-status");
    const box = s.getBoundingClientRect();
    if (box.top >= 0 && box.bottom <= window.innerHeight) return;
    s.scrollIntoView({ block: "center", behavior: reduceMotion ? "auto" : "smooth" });
  }
  function setStatus(text, isError) {
    const s = $("studio-status");
    s.textContent = text;
    s.classList.toggle("error", !!isError);
    if (isError) revealStatus();
  }
  const summary = (warns) => warns ? " " + warns + (warns === 1 ? " thing" : " things") + " to check below."
    : " It passes every check.";
  // The server's own rules for formats it rejects (rendered into data-sniff), for an
  // instant answer without uploading. Anything else is posted and the server decides.
  const drop = $("studio-drop");
  const SNIFF = (() => {
    try {
      const s = JSON.parse(drop.getAttribute("data-sniff") || "null");
      return s && Array.isArray(s.rejected) && Number.isFinite(s.max_bytes) ? s : null;
    } catch (e) { return null; }
  })();
  async function preflight(file) {
    if (!SNIFF) return null;
    const b = new Uint8Array(await file.slice(0, 64).arrayBuffer());
    const hit = SNIFF.rejected.find((sig) => sig.at.every(([offset, alternatives]) =>
      alternatives.some((magic) => magic.every((v, i) => b[offset + i] === v))));
    if (hit) return hit.message;
    return file.size > SNIFF.max_bytes ? SNIFF.too_large : null;
  }
  // "Try an example": a finished head drawing from the design kit, through the real
  // endpoint like any upload, but marked as the example: it says so, and it doesn't
  // close "Start here".
  async function tryExample() {
    const src = $("studio-example").getAttribute("data-src") || "";
    if (!src.startsWith("/static/")) return;
    setStatus("Loading the example…");
    let body = null;
    try {
      const res = await fetch(src, { credentials: "omit" });
      if (res.ok) body = await res.arrayBuffer();
    } catch (e) { /* network error: body stays null */ }
    if (!body) return setStatus("We couldn't load the example. Check your connection and try again.", true);
    send({ body: body, kind: "head", fixes: [], slot: null, displaced: state.slots.head, info: null, example: true }, []);
  }
  async function handleFile(file) {
    if (!file) return;
    const problem = await preflight(file);
    if (problem) return setStatus(problem, true);
    const body = await file.arrayBuffer();
    send({ body: body, kind: state.kind, fixes: [], slot: null, displaced: state.slots[state.kind], info: null }, []);
  }
  // The file as the page saves it: the clean path in the design kit's SVG template.
  const svgFor = (slot) => '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><path fill-rule="' +
    slot.fillRule + '" d="' + slot.d + '"/></svg>';
  // The file behind a slot, for Flip and Fit: the upload while it's in memory, else (after
  // a reload, or once another file was uploaded) the saved path as an SVG. Both fixes are
  // rewrites of the clean path, so they land in the same place; the notes about the
  // original file are kept.
  function jobFor(kind) {
    const slot = state.slots[kind];
    if (!slot) return null;
    if (current && current.kind === kind && current.slot === slot) return current;
    return { body: svgFor(slot), kind: kind, fixes: [], slot: slot, displaced: null, info: slot.info, example: slot.example };
  }
  // Post `job.body` with `fixes` (a set: the server applies Flip before Fit). Only a
  // success changes anything: after an error, the last result and its buttons stay.
  async function send(job, fixes) {
    const ticket = {};
    pending = ticket;
    setStatus("Processing your drawing…");
    revealStatus(); // a slow upload must not look like nothing happened
    root.setAttribute("aria-busy", "true");
    const query = fixes.map((f) => "fix=" + f).join("&");
    let res = null;
    let body = null;
    try {
      res = await fetch(ENDPOINT + (query ? "?" + query : ""), { method: "POST", body: job.body, credentials: "omit",
        headers: { "Content-Type": "application/octet-stream" } });
      body = await res.json().catch(() => null);
    } catch (e) { /* network error: res stays null */ }
    if (pending !== ticket) return; // a newer upload or fix took over
    pending = null;
    root.removeAttribute("aria-busy");
    const slot = res && res.ok && body ? cleanSlot({ d: body.path_d, fillRule: body.fill_rule, lints: body.lints,
      info: body.info, gaps: body.metrics && body.metrics.left_edge_gaps }) : null;
    if (!slot) {
      const message = body && body.error && typeof body.error.message === "string" ? body.error.message
        : !res ? "We couldn't reach the studio. Check your connection and try again."
        : "Something went wrong on our side. Please try again.";
      return setStatus(message, true);
    }
    if (job.info) slot.info = job.info;
    slot.example = !!job.example;
    job.slot = slot;
    job.fixes = fixes;
    current = job;
    state.slots[job.kind] = slot;
    state.pair[job.kind] = "user";
    state.kind = job.kind;
    render();
    setStatus(slot.example ? "This is the example " + job.kind + "." + summary(slot.lints[job.kind].length) +
      " Upload your own drawing to replace it."
      : "Done: your " + job.kind + " is on the board." + summary(slot.lints[job.kind].length));
    showResult();
  }
  // Focus the result, and bring it into view: the preview where the checks sit beside it,
  // else the status line (the top warning and the preview follow it).
  function showResult() {
    $("studio-result-heading").focus({ preventScroll: true });
    const target = sideBySide.matches ? root.querySelector(".studio-preview") : $("studio-status");
    target.scrollIntoView({ block: "start", behavior: reduceMotion ? "auto" : "smooth" });
  }
  function applyFix(fix) {
    const job = jobFor(state.kind);
    if (job && !job.fixes.includes(fix)) send(job, job.fixes.concat(fix));
  }
  // Where the last file can move without re-posting (both kinds' checks are already
  // here): to the other slot while its own slot is showing, or into the showing slot
  // while that one is empty (after tapping Tail to say "that was a tail").
  function relabelTarget() {
    if (!current || !current.slot || state.slots[current.kind] !== current.slot) return null;
    if (current.kind === state.kind) return other(current.kind);
    return state.slots[state.kind] ? null : state.kind;
  }
  // Move it, and put back what it replaced.
  function relabel() {
    const to = relabelTarget();
    if (!to) return;
    const from = current.kind;
    state.slots[from] = current.displaced;
    if (!current.displaced && state.pair[from] === "user") state.pair[from] = "default";
    current.displaced = state.slots[to];
    state.slots[to] = current.slot;
    state.pair[to] = "user";
    current.kind = to;
    state.kind = to;
    render();
    setStatus("Moved: this file is now your " + to + ".");
  }
  // "Upload as" switched: say what's showing now, and what the next upload fills.
  function kindChanged() {
    const slot = state.slots[state.kind];
    if (slot) setStatus("Showing " + (slot.example ? "the example " : "your ") + state.kind + "." + summary(slot.lints[state.kind].length));
    else setStatus("Your next upload will be your " + state.kind + ".");
  }

  // ---- downloads ------------------------------------------------------------------
  function download(blob, name) {
    const a = el("a", { href: URL.createObjectURL(blob), download: name });
    document.body.appendChild(a);
    a.click();
    a.remove();
    setTimeout(() => URL.revokeObjectURL(a.href), 10000);
  }
  function downloadSvg(kind) {
    const slot = state.slots[kind];
    if (slot) download(new Blob([svgFor(slot)], { type: "image/svg+xml" }), "my-battlesnake-" + kind + ".svg");
  }
  // A PNG of the All-directions board in the current colour, theme and pairing. The
  // page's CSS doesn't apply inside an image, so the copy carries its own colours and
  // an SVG drop shadow matching the board's CSS one.
  async function saveImage() {
    const board = root.querySelector(".studio-all");
    const vb = board.viewBox.baseVal;
    const scale = 4;
    const dark = state.theme === "dark";
    const copy = board.cloneNode(true);
    copy.setAttribute("width", String(vb.width * scale));
    copy.setAttribute("height", String(vb.height * scale));
    // drop-shadow(0.1em 0.1em 0.05em rgba(0,0,0,.3)) at 16px: 1.6 offset, 0.8 blur radius.
    const defs = el("defs", {}, undefined, SVG_NS);
    const filter = el("filter", { id: "studio-export-shadow" }, undefined, SVG_NS);
    filter.appendChild(el("feDropShadow", { dx: "1.6", dy: "1.6", stdDeviation: "0.4", "flood-color": "#000",
      "flood-opacity": "0.3" }, undefined, SVG_NS));
    defs.appendChild(filter);
    copy.insertBefore(el("rect", { width: "100%", height: "100%", fill: dark ? "#0f0b19" : "#ffffff" }, undefined, SVG_NS), copy.firstChild);
    copy.insertBefore(defs, copy.firstChild);
    copy.querySelectorAll(".grid").forEach((r) => r.setAttribute("fill", dark ? "#393939" : "#f1f1f1"));
    copy.querySelectorAll(".snake > svg").forEach((s) => s.setAttribute("fill", state.color));
    copy.querySelectorAll(".snake > polyline").forEach((p) => p.setAttribute("stroke", state.color));
    copy.querySelectorAll(".snake, .food").forEach((g) => g.setAttribute("filter", "url(#studio-export-shadow)"));
    const img = new Image();
    await new Promise((resolve, reject) => {
      img.addEventListener("load", resolve);
      img.addEventListener("error", reject);
      img.src = "data:image/svg+xml;charset=utf-8," + encodeURIComponent(new XMLSerializer().serializeToString(copy));
    });
    const canvas = document.createElement("canvas");
    canvas.width = Math.round(vb.width * scale);
    canvas.height = Math.round(vb.height * scale);
    canvas.getContext("2d").drawImage(img, 0, 0, canvas.width, canvas.height);
    const blob = await new Promise((resolve) => canvas.toBlob(resolve, "image/png"));
    if (!blob) return setStatus("We couldn't make the image. Try again.", true);
    const name = "my-battlesnake-preview.png";
    const file = typeof File === "function" ? new File([blob], name, { type: "image/png" }) : null;
    if (file && navigator.canShare && navigator.canShare({ files: [file] })) {
      try {
        await navigator.share({ files: [file], title: "My Battlesnake design" });
        return;
      } catch (e) {
        if (e && e.name === "AbortError") return; // the artist closed the share sheet
      }
    }
    download(blob, name);
  }

  // ---- the live loop --------------------------------------------------------------
  const live = root.querySelector(".studio-live");
  const frames = live ? Array.from(live.querySelectorAll(".studio-frame")) : [];
  let playing = !reduceMotion;
  let onScreen = true;
  let frame = 0;
  let timer = null;
  function step() {
    frames[frame].setAttribute("hidden", "");
    frame = (frame + 1) % frames.length;
    frames[frame].removeAttribute("hidden");
  }
  function syncLoop() {
    const run = playing && onScreen && !document.hidden && frames.length > 1;
    if (run && !timer) timer = setInterval(step, FRAME_MS);
    if (!run && timer) { clearInterval(timer); timer = null; }
    // A plain button whose name says what it does next (no aria-pressed: a toggle
    // button's name must not change with its state).
    const btn = $("studio-play");
    btn.textContent = playing ? "Pause" : "Play";
    btn.setAttribute("aria-label", (playing ? "Pause" : "Play") + " the live preview");
  }
  document.addEventListener("visibilitychange", syncLoop);
  if (live && "IntersectionObserver" in window) {
    new IntersectionObserver((entries) => {
      onScreen = entries.some((e) => e.isIntersecting);
      syncLoop();
    }).observe(live);
  }

  // ---- wiring ---------------------------------------------------------------------
  const fileInput = $("studio-file");
  fileInput.addEventListener("change", () => {
    handleFile(fileInput.files && fileInput.files[0]);
    fileInput.value = ""; // choosing the same file again still fires change
  });
  for (const type of ["dragenter", "dragover"]) {
    drop.addEventListener(type, (e) => { e.preventDefault(); drop.classList.add("dragging"); });
  }
  drop.addEventListener("dragleave", () => drop.classList.remove("dragging"));
  drop.addEventListener("drop", (e) => {
    e.preventDefault();
    drop.classList.remove("dragging");
    handleFile(e.dataTransfer && e.dataTransfer.files[0]);
  });
  root.addEventListener("change", (e) => {
    const t = e.target;
    if (t.name === "studio-kind") {
      state.kind = t.value === "tail" ? "tail" : "head";
      render();
      return kindChanged();
    } else if (t.name === "studio-theme") state.theme = t.value === "dark" ? "dark" : "light";
    else if (t.name === "studio-view" && VIEWS.includes(t.value)) state.view = t.value;
    else if (t.name === "studio-color" && COLOR_RE.test(t.value)) state.color = t.value.toLowerCase();
    else if (t.classList.contains("studio-pair-select")) {
      const kind = t.getAttribute("data-kind") === "tail" ? "tail" : "head";
      if (t.value === "user" || refOption(kind, t.value)) state.pair[kind] = t.value;
    } else return;
    render();
  });
  // Live has no option of its own once it sits beside Close-up: a Split View resize or a
  // rotation that crosses 640px re-renders, so a view option stays selected.
  if (wide.addEventListener) wide.addEventListener("change", render);
  $("studio-color-custom").addEventListener("input", (e) => {
    if (COLOR_RE.test(e.target.value)) { state.color = e.target.value.toLowerCase(); render(); }
  });
  root.addEventListener("click", (e) => {
    const fix = e.target.closest(".studio-fix");
    if (fix) applyFix(fix.getAttribute("data-fix") === "fit" ? "fit" : "flip");
  });
  $("studio-relabel").addEventListener("click", relabel);
  $("studio-example").addEventListener("click", () => { tryExample(); });
  $("studio-play").addEventListener("click", () => { playing = !playing; syncLoop(); });
  $("studio-new-version").addEventListener("click", () => fileInput.click());
  $("studio-download-head").addEventListener("click", () => downloadSvg("head"));
  $("studio-download-tail").addEventListener("click", () => downloadSvg("tail"));
  $("studio-save-image").addEventListener("click", () => {
    saveImage().catch(() => setStatus("We couldn't make the image. Try again.", true));
  });
  $("studio-clear").addEventListener("click", () => {
    current = null;
    pending = null;
    root.removeAttribute("aria-busy");
    state.slots = { head: null, tail: null };
    state.pair = { head: "default", tail: "default" };
    render();
    setStatus("Cleared. Try it with your own drawing.");
  });

  restore();
  render();
  syncLoop();
  if (state.slots[state.kind]) {
    setStatus(state.slots[state.kind].example ? "Welcome back: the example " + state.kind +
      " is still on the board. Upload your own drawing to replace it." : "Welcome back: your last preview is restored.");
  }
})();
