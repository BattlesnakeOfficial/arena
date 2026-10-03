// Head & Tail Studio (/customizations/studio).
//
// Every board is server-rendered. This script posts an upload to the processing
// endpoint and then only sets attributes: `d`/`fill-rule` on the placeholder paths
// (path.studio-head, path.studio-tail, #studio-closeup-path), the --studio-snake
// colour variable, the board theme class, and the text of the checks. No innerHTML;
// every path and colour is validated before use, including what localStorage restores.
(function () {
  "use strict";
  const root = document.getElementById("studio");
  if (!root) return;

  const ENDPOINT = "/customizations/studio/process";
  const STORE = "arena:studio:v1";
  const D_RE = /^[MLQCZ0-9 .\-]*$/;
  const COLOR_RE = /^#[0-9a-f]{6}$/i;
  const CODE_RE = /^[a-z_]{1,40}$/;
  const MAX_D = 65536;
  const MAX_BYTES = 4 * 1024 * 1024;
  const MAX_SVG_BYTES = 512 * 1024;
  const FRAME_MS = 250;
  const SVG_NS = "http://www.w3.org/2000/svg";
  const VIEWS = ["closeup", "live", "all", "game"];
  const $ = (id) => document.getElementById(id);
  const all = (sel) => Array.from(root.querySelectorAll(sel));
  const other = (kind) => (kind === "head" ? "tail" : "head");
  const wide = window.matchMedia ? matchMedia("(min-width: 640px)") : { matches: false };
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
    slots: { head: null, tail: null }, // the artist's results: { d, fillRule, lints, info, gaps }
    pair: { head: "default", tail: "default" }, // the other slot: "user" or a reference slug
    color: "#ff4f86",
    theme: document.documentElement.getAttribute("data-app-theme") === "dark" ? "dark" : "light",
    view: "closeup",
  };
  // The file being worked on, kept in memory only so Flip/Fit can re-post it.
  let current = null; // { bytes, kind, fixes, slot, displaced }

  // ---- validation -----------------------------------------------------------------
  const cleanLints = (list) =>
    (Array.isArray(list) ? list : [])
      .filter((l) => l && CODE_RE.test(l.code) && ["warn", "tip", "info"].includes(l.severity) &&
        typeof l.message === "string" && l.message.length < 2000)
      .slice(0, 50)
      .map((l) => ({ code: l.code, severity: l.severity, message: l.message,
        fix: l.fix === "flip" || l.fix === "fit" ? l.fix : null }));
  const cleanGaps = (list) =>
    (Array.isArray(list) ? list : [])
      .filter((g) => Array.isArray(g) && g.length === 2 && g.every((n) => Number.isFinite(n) && n >= 0 && n <= 100))
      .slice(0, 16);
  function cleanSlot(s) {
    if (!s || typeof s.d !== "string" || s.d.length > MAX_D || !D_RE.test(s.d)) return null;
    if (s.fillRule !== "nonzero" && s.fillRule !== "evenodd") return null;
    const lints = s.lints || {};
    return { d: s.d, fillRule: s.fillRule, lints: { head: cleanLints(lints.head), tail: cleanLints(lints.tail) },
      info: cleanLints(s.info), gaps: cleanGaps(s.gaps) };
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
    if (own && slot) return { d: slot.d, fillRule: slot.fillRule, name: "your " + kind };
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
  function render() {
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
    for (const i of all('input[name="studio-color"]')) {
      i.checked = i.value === state.color;
      if (i.checked) hint = i.getAttribute("data-hint") || "";
    }
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
    const relabel = $("studio-relabel");
    relabel.hidden = !(current && current.slot && state.slots[current.kind] === current.slot);
    if (current) relabel.textContent = "Use it as a " + other(current.kind) + " instead";
  }

  const fixLabel = (fix) => (fix === "flip" ? "Flip" : "Fit");
  function lintItem(lint, withFix) {
    const warn = lint.severity === "warn";
    const li = el("li", { class: "studio-lint " + lint.severity, "data-code": lint.code });
    li.append(el("span", { class: "studio-lint-icon", "aria-hidden": "true" }, warn ? "!" : "i"),
      el("span", { class: "vh" }, warn ? "Warning: " : "Note: "),
      el("span", { class: "studio-lint-text" }, lint.message));
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
    // Fixes re-post the original file, which only lives in memory.
    const canFix = !!(current && current.slot && current.slot === slot && current.kind === state.kind);
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
  function setStatus(text, isError) {
    const s = $("studio-status");
    s.textContent = text;
    s.classList.toggle("error", !!isError);
  }
  // The same magic-byte checks as the server, for instant, friendly answers:
  // [signature, offset, advice].
  const ascii = (str) => Array.from(str, (c) => c.charCodeAt(0));
  const REJECTED = [
    [ascii("ftyp"), 4, "That's a HEIC or AVIF photo, which we can't read. Share or export it as JPEG or PNG, then upload that."],
    [ascii("GIF8"), 0, "GIFs aren't supported. Export your drawing as PNG."],
    [ascii("WEBP"), 8, "WebP isn't supported. Export your drawing as PNG or JPEG."],
    [ascii("8BPS"), 0, "That's a PSD, like the template. Draw on it, then export a PNG: in Procreate, Actions → Share → PNG."],
    [[0x50, 0x4b, 0x03, 0x04], 0, "That looks like a Procreate file. In Procreate, tap Actions → Share → PNG, then upload the PNG."],
    [[0x50, 0x4b, 0x05, 0x06], 0, "That looks like a Procreate file. In Procreate, tap Actions → Share → PNG, then upload the PNG."],
    [ascii("%PDF"), 0, "That's a PDF or Illustrator file. Use File → Export → SVG or PNG, then upload that."],
    [ascii("%!PS"), 0, "That's a PDF or Illustrator file. Use File → Export → SVG or PNG, then upload that."],
    [[0x1f, 0x8b], 0, "That's a compressed SVG (.svgz). Save it as a plain SVG instead, then upload that."],
  ].concat([[0xff, 0xfe], [0xfe, 0xff], [0x3c, 0], [0, 0x3c]].map((sig) =>
    [sig, 0, "That SVG is saved as UTF-16 text, which we can't read. Save it again as UTF-8, then upload it."]));
  async function preflight(file) {
    if (file.size === 0) return "That file is empty. Export your drawing again.";
    const b = new Uint8Array(await file.slice(0, 64).arrayBuffer());
    const at = (sig, offset) => sig.every((v, i) => b[offset + i] === v);
    if (at([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a], 0) || at([0xff, 0xd8, 0xff], 0)) {
      return file.size > MAX_BYTES ? "That file is too big (the limit is 4 MB). Export a PNG at 1000 × 1000 px, the template's size." : null;
    }
    const rejected = REJECTED.find(([sig, offset]) => at(sig, offset));
    if (rejected) return rejected[2];
    const text = (await file.slice(0, 64 * 1024).text()).replace(/^\uFEFF/, "").trimStart();
    if (text.startsWith("<") && text.includes("<svg")) {
      return file.size > MAX_SVG_BYTES ? "That SVG is too big (the limit is 512 KB). Simplify it, or export a PNG instead." : null;
    }
    return "We couldn't tell what kind of file this is. Upload a PNG, JPEG or SVG.";
  }
  async function handleFile(file) {
    if (!file) return;
    const problem = await preflight(file);
    if (problem) return setStatus(problem, true);
    current = { bytes: await file.arrayBuffer(), kind: state.kind, fixes: [], slot: null, seq: 0,
      displaced: state.slots[state.kind] };
    send(current, []);
  }
  // Post the file with `fixes` (a set: the server applies Flip before Fit).
  async function send(job, fixes) {
    const seq = ++job.seq;
    setStatus("Processing your drawing…");
    root.setAttribute("aria-busy", "true");
    const query = fixes.map((f) => "fix=" + f).join("&");
    let res = null;
    let body = null;
    try {
      res = await fetch(ENDPOINT + (query ? "?" + query : ""), { method: "POST", body: job.bytes, credentials: "omit",
        headers: { "Content-Type": "application/octet-stream" } });
      body = await res.json().catch(() => null);
    } catch (e) { /* network error: res stays null */ }
    if (job !== current || seq !== job.seq) return; // a newer upload or fix took over
    root.removeAttribute("aria-busy");
    const slot = res && res.ok && body ? cleanSlot({ d: body.path_d, fillRule: body.fill_rule, lints: body.lints,
      info: body.info, gaps: body.metrics && body.metrics.left_edge_gaps }) : null;
    if (!slot) {
      const message = body && body.error && typeof body.error.message === "string" ? body.error.message
        : !res ? "We couldn't reach the studio. Check your connection and try again."
        : "Something went wrong on our side. Please try again.";
      return setStatus(message, true);
    }
    job.slot = slot;
    job.fixes = fixes;
    state.slots[job.kind] = slot;
    state.pair[job.kind] = "user";
    state.kind = job.kind;
    render();
    const warns = slot.lints[job.kind].length;
    setStatus("Done: your " + job.kind + " is on the board." +
      (warns ? " " + warns + (warns === 1 ? " thing" : " things") + " to check below." : " It passes every check."));
    $("studio-result-heading").focus();
  }
  function applyFix(fix) {
    if (current && current.slot && !current.fixes.includes(fix)) send(current, current.fixes.concat(fix));
  }
  // Move the current file to the other slot (both kinds' checks are already here), and
  // put back what it replaced.
  function relabel() {
    if (!current || !current.slot || state.slots[current.kind] !== current.slot) return;
    const from = current.kind;
    const to = other(from);
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
    if (!slot) return;
    const svg = '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><path fill-rule="' +
      slot.fillRule + '" d="' + slot.d + '"/></svg>';
    download(new Blob([svg], { type: "image/svg+xml" }), "my-battlesnake-" + kind + ".svg");
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
  const reduceMotion = window.matchMedia && matchMedia("(prefers-reduced-motion: reduce)").matches;
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
    const btn = $("studio-play");
    btn.textContent = playing ? "Pause" : "Play";
    btn.setAttribute("aria-pressed", playing ? "false" : "true");
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
  const drop = $("studio-drop");
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
    if (t.name === "studio-kind") state.kind = t.value === "tail" ? "tail" : "head";
    else if (t.name === "studio-theme") state.theme = t.value === "dark" ? "dark" : "light";
    else if (t.name === "studio-view" && VIEWS.includes(t.value)) state.view = t.value;
    else if (t.name === "studio-color" && COLOR_RE.test(t.value)) state.color = t.value.toLowerCase();
    else if (t.classList.contains("studio-pair-select")) {
      const kind = t.getAttribute("data-kind") === "tail" ? "tail" : "head";
      if (t.value === "user" || refOption(kind, t.value)) state.pair[kind] = t.value;
    } else return;
    render();
  });
  $("studio-color-custom").addEventListener("input", (e) => {
    if (COLOR_RE.test(e.target.value)) { state.color = e.target.value.toLowerCase(); render(); }
  });
  root.addEventListener("click", (e) => {
    const fix = e.target.closest(".studio-fix");
    if (fix) applyFix(fix.getAttribute("data-fix") === "fit" ? "fit" : "flip");
  });
  $("studio-relabel").addEventListener("click", relabel);
  $("studio-play").addEventListener("click", () => { playing = !playing; syncLoop(); });
  $("studio-new-version").addEventListener("click", () => fileInput.click());
  $("studio-download-head").addEventListener("click", () => downloadSvg("head"));
  $("studio-download-tail").addEventListener("click", () => downloadSvg("tail"));
  $("studio-save-image").addEventListener("click", () => {
    saveImage().catch(() => setStatus("We couldn't make the image. Try again.", true));
  });
  $("studio-clear").addEventListener("click", () => {
    current = null;
    state.slots = { head: null, tail: null };
    state.pair = { head: "default", tail: "default" };
    render();
    setStatus("Cleared. Try it with your own drawing.");
  });

  restore();
  render();
  syncLoop();
  if (state.slots[state.kind]) setStatus("Welcome back: your last preview is restored.");
})();
