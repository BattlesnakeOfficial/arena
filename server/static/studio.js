// Head & Tail Studio (/customizations/studio).
//
// Every board is server-rendered. "Your snake" has two slots, the head and the tail:
// each wears the artist's upload or a catalog reference, and every board wears both.
// This script posts an upload to the processing endpoint and then only sets attributes
// and text: `d`/`fill-rule` on the placeholder paths (path.studio-head, path.studio-tail,
// and each slot's close-up and thumbnail), the --studio-snake colour variable, the board
// theme class, and the text of the cards and the checks (with links into the guide).
// Per-slot ids are `studio-<part>-<kind>`. No innerHTML; every path, colour and link is
// validated before use, including what localStorage restores.
(function () {
  "use strict";
  const root = document.getElementById("studio");
  if (!root) return;

  const ENDPOINT = "/customizations/studio/process";
  const STORE = "arena:studio:v2";
  const STORE_V1 = "arena:studio:v1"; // one "Upload as" slot at a time: read once, then replaced
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
  const KINDS = ["head", "tail"];
  const $ = (id) => document.getElementById(id);
  const part = (name, kind) => $("studio-" + name + "-" + kind);
  const all = (sel) => Array.from(root.querySelectorAll(sel));
  const other = (kind) => (kind === "head" ? "tail" : "head");
  const title = (kind) => (kind === "head" ? "Head" : "Tail");
  const kindOf = (node) => (node.getAttribute("data-kind") === "tail" ? "tail" : "head");
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
    slots: { head: null, tail: null }, // uploads: { d, fillRule, lints, info, gaps, example }
    pick: { head: "default", tail: "default" }, // what each slot wears: "user" (its upload) or a reference slug
    color: "#ff4f86",
    theme: document.documentElement.getAttribute("data-app-theme") === "dark" ? "dark" : "light",
    view: "closeup",
  };
  // In memory only: the file behind each upload, so Flip/Fit re-post the original
  // ({ body, fixes, info }); what each slot's upload replaced, so moving it to the other
  // slot puts that back ({ by, prev }); what Remove took out of each slot, for Undo,
  // until the slot holds a drawing again ({ slot, replaced }); and each slot's request in
  // flight ({ fixOf, example }; a newer one for the same slot supersedes it, except that
  // a Flip/Fit never supersedes an upload).
  const files = new WeakMap();
  const replaced = { head: null, tail: null };
  const removed = { head: null, tail: null };
  const pending = { head: null, tail: null };

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
      localStorage.setItem(STORE, JSON.stringify({ v: 2, slots: state.slots, pick: state.pick,
        color: state.color, theme: state.theme, view: state.view }));
      localStorage.removeItem(STORE_V1);
    } catch (e) { /* storage full or disabled: the preview still works */ }
  }
  // v2, else v1 (one "Upload as" slot, plus "Pair with" for the other): every slot
  // with an upload wears it, an empty one keeps its pairing. restore() validates either.
  function saved() {
    const read = (key) => {
      try { return JSON.parse(localStorage.getItem(key) || "null"); } catch (e) { return null; }
    };
    const v2 = read(STORE);
    if (v2 && v2.v === 2) return v2;
    const v1 = read(STORE_V1);
    if (!v1 || v1.v !== 1) return null;
    const pick = {};
    for (const k of KINDS) pick[k] = v1.slots && v1.slots[k] ? "user" : v1.pair && v1.pair[k];
    return { slots: v1.slots, pick: pick, color: v1.color, theme: v1.theme, view: v1.view };
  }
  function restore() {
    const s = saved();
    if (!s) return;
    for (const k of KINDS) {
      state.slots[k] = cleanSlot(s.slots && s.slots[k]);
      const p = s.pick && s.pick[k];
      if (p === "user" ? !!state.slots[k] : !!refOption(k, p)) state.pick[k] = p;
    }
    if (typeof s.color === "string" && COLOR_RE.test(s.color)) state.color = s.color.toLowerCase();
    if (s.theme === "light" || s.theme === "dark") state.theme = s.theme;
    if (VIEWS.includes(s.view)) state.view = s.view;
  }

  // ---- shapes ---------------------------------------------------------------------
  function refOption(kind, slug) {
    if (typeof slug !== "string" || slug === "user") return null;
    return Array.from(part("style", kind).options).find((o) => o.value === slug) || null;
  }
  // `name` reads in a sentence ("the Curled tail"), `label` stands alone ("Curled tail").
  function refShape(kind, slug) {
    const o = refOption(kind, slug) || refOption(kind, "default");
    const d = o ? o.getAttribute("data-d") || "" : "";
    const style = o && o.value !== "default" ? o.textContent : "";
    return { d: D_RE.test(d) ? d : "", fillRule: o && o.getAttribute("data-fill-rule") === "evenodd" ? "evenodd" : "nonzero",
      name: "the " + (style || "default") + " " + kind, label: (style || "Default") + " " + kind };
  }
  // The upload a slot wears, if it wears one.
  const own = (kind) => (state.pick[kind] === "user" && state.slots[kind]) || null;
  function shapeFor(kind) {
    const slot = own(kind);
    if (!slot) return refShape(kind, state.pick[kind]);
    return { d: slot.d, fillRule: slot.fillRule, name: (slot.example ? "the example " : "your ") + kind,
      label: (slot.example ? "Example " : "Your ") + kind };
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
  // doesn't count), then closes, and opens again once none is left. Only on those
  // changes, so it stays however the artist leaves it.
  const start = $("studio-start");
  let startFor = null;
  function syncStart(drawn) {
    if (!start || startFor === drawn) return;
    startFor = drawn;
    start.open = !drawn;
  }
  function render() {
    syncStart(KINDS.some((k) => state.slots[k] && !state.slots[k].example));
    const shapes = {};
    for (const kind of KINDS) {
      if (state.pick[kind] === "user" && !state.slots[kind]) state.pick[kind] = "default";
      if (state.slots[kind]) removed[kind] = null; // Undo only puts a drawing in an empty slot
      const shape = shapes[kind] = shapeFor(kind);
      all("path.studio-" + kind).forEach((p) => setPath(p, shape));
      setPath(part("closeup-path", kind), shape);
      setPath(part("thumb-path", kind), shape);
      part("closeup", kind).setAttribute("aria-label", "Close-up of " + shape.name);
      renderGaps(kind);
      renderCard(kind, shape);
      renderChecks(kind);
    }
    root.style.setProperty("--studio-snake", state.color);
    all(".studio-board").forEach((b) => {
      b.classList.toggle("light", state.theme === "light");
      b.classList.toggle("dark", state.theme === "dark");
    });

    const wearing = "wearing " + shapes.head.name + " and " + shapes.tail.name;
    const color = colorName();
    const label = (node, text) => node && node.setAttribute("aria-label", text);
    label(root.querySelector(".studio-live"), "A " + color + " snake " + wearing + ", moving around the board");
    label(root.querySelector(".studio-all"), "Four " + color + " snakes " + wearing + ", facing right, left, up and down");
    all(".studio-game").forEach((b, i) => label(b, (i ? "iPad" : "Phone") + " size: four " + color + " snakes " + wearing));

    renderControls();
    renderSummary();
    persist();
  }
  // Red brackets beside a close-up's left edge, where the neck has gaps.
  function renderGaps(kind) {
    const g = part("gaps", kind);
    empty(g);
    const slot = own(kind);
    for (const [y0, y1] of slot ? slot.gaps : []) {
      g.appendChild(el("rect", { x: "-4", y: String(y0), width: "3", height: String(Math.max(y1 - y0, 0.5)) }, undefined, SVG_NS));
    }
  }
  // A slot's card: what it wears, its upload button, its styles ("Your head" first
  // while there is an upload), and the upload's own actions.
  function renderCard(kind, shape) {
    const slot = state.slots[kind];
    const mine = own(kind);
    part("name", kind).textContent = shape.label;
    // A new upload replaces the slot's upload, even one it isn't wearing.
    const drawn = slot && !slot.example;
    part("upload-text", kind).textContent = drawn ? "Upload a new version" : "Upload " + kind;
    part("upload-vh", kind).textContent = drawn ? " of your " + kind : "";
    const select = part("style", kind);
    let user = select.querySelector('option[value="user"]');
    if (slot && !user) {
      user = el("option", { value: "user" });
      select.insertBefore(user, select.firstChild);
    } else if (!slot && user) {
      user.remove();
    }
    if (slot) user.textContent = (slot.example ? "Example " : "Your ") + kind;
    select.value = state.pick[kind];
    part("download", kind).hidden = !mine;
    part("remove", kind).hidden = !mine;
    part("relabel", kind).hidden = !mine || mine.example;
    part("undo", kind).hidden = !removed[kind];
    part("confirm", kind).hidden = true; // any change closes the question
  }
  function renderControls() {
    if (state.view === "live" && wide.matches) state.view = "closeup"; // shown side by side
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
  }

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
  // Flip or Fit, on the slot `kind`.
  function setFix(btn, kind, fix) {
    btn.setAttribute("data-fix", fix);
    btn.setAttribute("data-kind", kind);
    empty(btn);
    btn.append(fix === "flip" ? "Flip" : "Fit", el("span", { class: "vh" }, " your " + kind));
  }
  function lintItem(kind, lint, withFix) {
    const warn = lint.severity === "warn";
    const li = el("li", { class: "studio-lint " + lint.severity, "data-code": lint.code });
    li.append(el("span", { class: "studio-lint-icon", "aria-hidden": "true" }, warn ? "!" : "i"),
      el("span", { class: "vh" }, warn ? "Warning: " : "Note: "),
      el("span", { class: "studio-lint-text" }, lint.message));
    const learn = el("a", { class: "studio-learn" });
    if (setLearn(learn, lint)) li.appendChild(learn);
    if (withFix && lint.fix) {
      const btn = el("button", { type: "button", class: "btn sm studio-fix", "data-action": "fix" });
      setFix(btn, kind, lint.fix);
      li.appendChild(btn);
    }
    return li;
  }
  function fill(list, items) {
    empty(list);
    items.forEach((li) => list.appendChild(li));
  }
  // The checks of the upload a slot wears, under its own heading.
  function renderChecks(kind) {
    const slot = own(kind);
    const warns = slot ? slot.lints[kind] : [];
    const notes = slot ? slot.info : [];
    const infos = notes.filter((l) => l.severity === "info");
    part("checks", kind).hidden = !slot;
    fill(part("warnings", kind), warns.map((l) => lintItem(kind, l, true)));
    fill(part("tips", kind), notes.filter((l) => l.severity === "tip").map((l) => lintItem(kind, l, false)));
    fill(part("info", kind), infos.map((l) => lintItem(kind, l, false)));
    part("details", kind).hidden = infos.length === 0;
    part("details-summary", kind).textContent = "Details (" + infos.length + ")";
    part("pass", kind).hidden = !slot || warns.length > 0;
  }
  const things = (n) => n + (n === 1 ? " thing" : " things") + " to check";
  // Both slots in a line ("Head: passes · Tail: 1 thing to check"), and the first
  // warning of either, with its link and fix, at the top.
  function renderSummary() {
    const line = [];
    let first = null;
    for (const kind of KINDS) {
      const slot = own(kind);
      if (!slot) continue;
      const warns = slot.lints[kind];
      line.push((slot.example ? "Example " + kind : title(kind)) + ": " + (warns.length ? things(warns.length) : "passes"));
      if (warns.length && !first) first = { kind: kind, lint: warns[0] };
    }
    $("studio-summary").textContent = line.join(" · ");
    $("studio-summary").hidden = !line.length;
    $("studio-lints-empty").hidden = line.length > 0;
    $("studio-top-warning").hidden = !first;
    $("studio-top-learn").hidden = !(first && setLearn($("studio-top-learn"), first.lint));
    if (!first) return;
    $("studio-top-warning-text").textContent = title(first.kind) + ": " + first.lint.message;
    const fixBtn = $("studio-top-fix");
    fixBtn.hidden = !first.lint.fix;
    if (first.lint.fix) setFix(fixBtn, first.kind, first.lint.fix);
  }

  // ---- uploads --------------------------------------------------------------------
  // An error is only ever said in the status line, under the cards. "Upload a new
  // version" can be far down the second card and Fix in the checks, so bring it into
  // view (when it isn't already) or the last result seems to stand for the new file.
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
  function setBusy() {
    if (pending.head || pending.tail) root.setAttribute("aria-busy", "true");
    else root.removeAttribute("aria-busy");
  }
  const summary = (warns) => warns ? " " + things(warns) + " below." : " It passes every check.";
  // The server's own rules for formats it rejects (rendered into data-sniff), for an
  // instant answer without uploading. Anything else is posted and the server decides.
  const SNIFF = (() => {
    try {
      const s = JSON.parse($("studio-slots").getAttribute("data-sniff") || "null");
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
  // Why the example can't go into the head slot, if it can't: the slot holds the
  // artist's own drawing (worn, or kept in the style list behind a catalog style), or
  // one is on its way there.
  function exampleBlocked() {
    const head = state.slots.head;
    const lead = "The example is a head, and ";
    if (head && !head.example) {
      return lead + (own("head") ? "your head slot holds your own drawing. Remove your head first to try the example."
        : "your own head is kept in the head card's style list. Pick “Your head” there and tap " +
          "Remove first to try the example.");
    }
    if (pending.head && !pending.head.example) {
      return lead + "your own head is still processing. Remove it once it's on the board to try the example.";
    }
    return null;
  }
  // "Try an example": a finished head drawing from the design kit, through the real
  // endpoint into the head slot like any upload, but marked as the example: it says so,
  // and it doesn't close "Start here". It never replaces the artist's own head.
  async function tryExample() {
    const blocked = exampleBlocked();
    if (blocked) return setStatus(blocked, true);
    const src = $("studio-example").getAttribute("data-src") || "";
    if (!src.startsWith("/static/")) return;
    setStatus("Loading the example…");
    let body = null;
    try {
      const res = await fetch(src, { credentials: "omit" });
      if (res.ok) body = await res.arrayBuffer();
    } catch (e) { /* network error: body stays null */ }
    if (!body) return setStatus("We couldn't load the example. Check your connection and try again.", true);
    if (exampleBlocked()) return; // a head of the artist's own came along meanwhile: it wins
    send("head", { body: body, info: null, example: true, fixOf: null }, []);
  }
  async function handleFile(kind, file) {
    if (!file) return;
    let problem = null;
    let body = null;
    try {
      problem = await preflight(file);
      if (!problem) body = await file.arrayBuffer();
    } catch (e) { // gone, or not downloaded yet (iCloud)
      problem = "We couldn't read that file. Try choosing it again.";
    }
    if (problem) return setStatus(problem, true);
    send(kind, { body: body, info: null, example: false, fixOf: null }, []);
  }
  // The file as the page saves it: the clean path in the design kit's SVG template.
  const svgFor = (slot) => '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><path fill-rule="' +
    slot.fillRule + '" d="' + slot.d + '"/></svg>';
  // Post `job.body` with `fixes` (a set: the server applies Flip before Fit) for the slot
  // `kind`. Only a success changes anything: after an error, the last result and its
  // buttons stay.
  async function send(kind, job, fixes) {
    const ticket = { fixOf: job.fixOf, example: !!job.example };
    pending[kind] = ticket;
    setStatus(job.example ? "Processing the example…" : "Processing your " + kind + "…");
    revealStatus(); // a slow upload must not look like nothing happened
    setBusy();
    const query = fixes.map((f) => "fix=" + f).join("&");
    let res = null;
    let body = null;
    try {
      res = await fetch(ENDPOINT + (query ? "?" + query : ""), { method: "POST", body: job.body, credentials: "omit",
        headers: { "Content-Type": "application/octet-stream" } });
      body = await res.json().catch(() => null);
    } catch (e) { /* network error: res stays null */ }
    if (pending[kind] !== ticket) return; // a newer upload or fix for this slot took over
    pending[kind] = null;
    setBusy();
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
    files.set(slot, { body: job.body, fixes: fixes, info: job.info });
    // What it replaced. A fix replaces the same drawing, so keep what that one replaced.
    const was = replaced[kind];
    replaced[kind] = { by: slot, prev: !job.fixOf ? state.slots[kind] : was && was.by === job.fixOf ? was.prev : null };
    // A new upload is worn at once. A fix of one the artist has since swapped for a
    // catalog style is kept in the style list: their pick stands.
    const worn = !job.fixOf || state.pick[kind] === "user";
    state.slots[kind] = slot;
    if (worn) state.pick[kind] = "user";
    render();
    const warns = slot.lints[kind].length;
    if (!worn) {
      setStatus("Done: " + (slot.example ? "the example " : "your ") + kind + " is fixed. The " + kind + " card shows " +
        shapeFor(kind).name + ", so pick “" + (slot.example ? "Example " : "Your ") + kind +
        "” in its style list to see it.");
      return revealStatus();
    }
    setStatus(slot.example ? "This is the example " + kind + "." + summary(warns) + " Upload your own drawing to replace it."
      : "Done: your " + kind + " is on the board." + summary(warns));
    showResult();
  }
  // Focus the result, and bring it into view: the preview where the checks sit beside it,
  // else the status line (the top warning and the preview follow it).
  function showResult() {
    $("studio-result-heading").focus({ preventScroll: true });
    // While "Start here" is open (the example), it sits between the status and the preview.
    const target = sideBySide.matches || (start && start.open) ? root.querySelector(".studio-preview") : $("studio-status");
    target.scrollIntoView({ block: "start", behavior: reduceMotion ? "auto" : "smooth" });
  }
  // Flip and Fit re-post the upload while it's in memory, else (after a reload) the
  // saved path as an SVG. Both fixes are rewrites of the clean path, so they land in the
  // same place; the notes about the original file are kept.
  function applyFix(kind, fix) {
    const slot = own(kind);
    if (!slot) return;
    // A new upload still on its way to this slot would be dropped for the fix of the
    // drawing it replaces (its warning still shows meanwhile): not yet.
    const coming = pending[kind] && !pending[kind].fixOf ? pending[kind] : null;
    if (coming) {
      return setStatus((coming.example ? "The example" : (slot.example ? "Your " : "Your new ") + kind) +
        " is still processing, so nothing was " + (fix === "flip" ? "flipped" : "fitted") +
        ". Its own checks show once it's on the board.", true);
    }
    const file = files.get(slot) || { body: svgFor(slot), fixes: [], info: slot.info };
    if (!file.fixes.includes(fix)) {
      send(kind, { body: file.body, info: file.info, example: slot.example, fixOf: slot }, file.fixes.concat(fix));
    }
  }
  // "This is actually a tail": move the upload to the other slot without re-posting (the
  // response has both kinds' checks) and put back what it replaced. Over the other
  // slot's drawing only after "Replace your tail".
  function relabel(kind, confirmed) {
    const slot = own(kind);
    const to = other(kind);
    if (!slot || slot.example) return;
    // An upload still on its way to the other slot would land over the move: not yet.
    const coming = pending[to] && !pending[to].fixOf ? pending[to] : null;
    if (coming) {
      part("confirm", kind).hidden = true;
      part("relabel", kind).hidden = false;
      part("relabel", kind).focus();
      return setStatus((coming.example ? "The example" : "Your " + to) + " is still processing, so nothing moved. " +
        "Try again once it's on the board.", true);
    }
    if (state.slots[to] && !confirmed) {
      part("relabel", kind).hidden = true;
      part("confirm", kind).hidden = false;
      part("confirm-yes", kind).focus();
      return;
    }
    // A Flip or Fit still on its way for either drawing would land over the move (the
    // other slot's would bring back the drawing just replaced): drop it.
    for (const k of [kind, to]) {
      if (pending[k] && pending[k].fixOf && pending[k].fixOf === state.slots[k]) pending[k] = null;
    }
    const back = replaced[kind] && replaced[kind].by === slot ? replaced[kind].prev : null;
    replaced[kind] = null;
    state.slots[kind] = back;
    state.pick[kind] = back ? "user" : "default";
    replaced[to] = { by: slot, prev: state.slots[to] };
    state.slots[to] = slot;
    state.pick[to] = "user";
    setBusy();
    render();
    setStatus("Moved: that drawing is now your " + to + "." + (back ? " Your previous " + kind + " is back." : ""));
    part("relabel", to).focus();
  }
  function cancelRelabel(kind) {
    part("confirm", kind).hidden = true;
    part("relabel", kind).hidden = false;
    part("relabel", kind).focus();
  }
  // Remove: back to the catalog's default for that slot; the other slot is untouched.
  // Undo, in its place, puts the upload back until the slot holds another drawing.
  function remove(kind) {
    const slot = own(kind);
    if (!slot) return;
    removed[kind] = { slot: slot, replaced: replaced[kind] };
    pending[kind] = null;
    replaced[kind] = null;
    state.slots[kind] = null;
    state.pick[kind] = "default";
    setBusy();
    render();
    setStatus("Removed " + (slot.example ? "the example " : "your ") + kind + ". The board shows the default " + kind + ".");
    part("undo", kind).focus();
  }
  function undoRemove(kind) {
    const was = removed[kind];
    if (!was || state.slots[kind]) return;
    pending[kind] = null; // an upload started since doesn't land over the Undo
    replaced[kind] = was.replaced;
    state.slots[kind] = was.slot;
    state.pick[kind] = "user";
    setBusy();
    render();
    setStatus((was.slot.example ? "The example " : "Your ") + kind + " is back.");
    part("remove", kind).focus();
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
    const slot = own(kind);
    if (slot) download(new Blob([svgFor(slot)], { type: "image/svg+xml" }), "my-battlesnake-" + kind + ".svg");
  }
  // A PNG of the All-directions board (both slots) in the current colour and theme. The
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
  // Each card takes a dropped file into its own slot.
  for (const kind of KINDS) {
    const card = part("slot", kind);
    const dragging = (on) => card.classList.toggle("dragging", on);
    for (const type of ["dragenter", "dragover"]) {
      card.addEventListener(type, (e) => { e.preventDefault(); dragging(true); });
    }
    card.addEventListener("dragleave", (e) => { if (!card.contains(e.relatedTarget)) dragging(false); });
    card.addEventListener("drop", (e) => {
      e.preventDefault();
      dragging(false);
      handleFile(kind, e.dataTransfer && e.dataTransfer.files[0]);
    });
  }
  root.addEventListener("change", (e) => {
    const t = e.target;
    if (t.classList.contains("studio-file")) {
      handleFile(kindOf(t), t.files && t.files[0]);
      t.value = ""; // choosing the same file again still fires change
      return;
    }
    if (t.name === "studio-theme") state.theme = t.value === "dark" ? "dark" : "light";
    else if (t.name === "studio-view" && VIEWS.includes(t.value)) state.view = t.value;
    else if (t.name === "studio-color" && COLOR_RE.test(t.value)) state.color = t.value.toLowerCase();
    else if (t.classList.contains("studio-style-select")) {
      const kind = kindOf(t);
      if (t.value === "user" ? !!state.slots[kind] : !!refOption(kind, t.value)) state.pick[kind] = t.value;
    } else return;
    render();
  });
  // Live has no option of its own once it sits beside Close-up: a Split View resize or a
  // rotation that crosses 640px re-renders, so a view option stays selected.
  if (wide.addEventListener) wide.addEventListener("change", render);
  $("studio-color-custom").addEventListener("input", (e) => {
    if (COLOR_RE.test(e.target.value)) { state.color = e.target.value.toLowerCase(); render(); }
  });
  // Every per-slot button says what it does (data-action) and to which slot (data-kind).
  const ACTIONS = new Map([
    ["fix", (kind, btn) => applyFix(kind, btn.getAttribute("data-fix") === "fit" ? "fit" : "flip")],
    ["download", downloadSvg],
    ["remove", remove],
    ["undo", undoRemove],
    ["relabel", (kind) => relabel(kind, false)],
    ["relabel-confirm", (kind) => relabel(kind, true)],
    ["relabel-cancel", cancelRelabel],
  ]);
  root.addEventListener("click", (e) => {
    const btn = e.target.closest("button[data-action]");
    const action = btn && ACTIONS.get(btn.getAttribute("data-action"));
    if (action) action(kindOf(btn), btn);
  });
  $("studio-example").addEventListener("click", () => { tryExample(); });
  $("studio-play").addEventListener("click", () => { playing = !playing; syncLoop(); });
  $("studio-save-image").addEventListener("click", () => {
    saveImage().catch(() => setStatus("We couldn't make the image. Try again.", true));
  });

  restore();
  render();
  syncLoop();
  const example = KINDS.find((k) => state.slots[k] && state.slots[k].example);
  if (KINDS.some((k) => state.slots[k] && !state.slots[k].example)) {
    setStatus("Welcome back: your last preview is restored.");
  } else if (example) {
    setStatus("Welcome back: the example " + example + " is still on the board. Upload your own drawing to replace it.");
  }
})();
