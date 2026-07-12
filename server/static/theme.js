// Two-axis theme controller (site theme + game theater).
//
// Resolution order for each axis: html data attribute (server-rendered,
// logged-in account setting) -> localStorage (anonymous) -> default.
// The same logic runs inline in <head> before first paint (no-FOUC);
// this file wires up the toggle button and the appearance popover.
(function () {
  var root = document.documentElement;
  var theaterPage = root.hasAttribute("data-theater-page");
  var authed = root.hasAttribute("data-authed");

  function pref(axis, fallback) {
    return (
      root.getAttribute("data-bs-" + axis) ||
      localStorage.getItem("bs-" + axis) ||
      fallback
    );
  }

  function resolved() {
    var site = pref("site", "system");
    var theater = pref("theater", "dark");
    var sys = matchMedia("(prefers-color-scheme: dark)").matches
      ? "dark"
      : "light";
    var s = site === "system" ? sys : site;
    if (!theaterPage) return s;
    return theater === "match" ? s : theater;
  }

  function apply() {
    root.setAttribute("data-app-theme", resolved());
  }

  function persist(axis, value) {
    localStorage.setItem("bs-" + axis, value);
    root.setAttribute("data-bs-" + axis, value);
    if (authed) {
      var body =
        "site=" +
        encodeURIComponent(pref("site", "system")) +
        "&theater=" +
        encodeURIComponent(pref("theater", "dark"));
      fetch("/settings/appearance", {
        method: "POST",
        headers: { "Content-Type": "application/x-www-form-urlencoded" },
        body: body,
      }).catch(function () {
        /* offline / transient — localStorage still holds the choice */
      });
    }
  }

  var pop = document.getElementById("appearance");

  function syncRadios() {
    if (!pop) return;
    var site = pref("site", "system");
    var theater = pref("theater", "dark");
    pop.querySelectorAll("input[name=site]").forEach(function (r) {
      r.checked = r.value === site;
    });
    pop.querySelectorAll("input[name=theater]").forEach(function (r) {
      r.checked = r.value === theater;
    });
  }

  var toggle = document.getElementById("theme-toggle");
  if (toggle) {
    toggle.addEventListener("click", function () {
      var next = resolved() === "dark" ? "light" : "dark";
      persist(theaterPage ? "theater" : "site", next);
      apply();
      syncRadios();
    });
  }

  var popBtn = document.getElementById("appearance-btn");
  if (popBtn && pop) {
    popBtn.addEventListener("click", function (e) {
      e.stopPropagation();
      pop.hidden = !pop.hidden;
    });
    pop.addEventListener("change", function (e) {
      persist(e.target.name === "site" ? "site" : "theater", e.target.value);
      apply();
    });
    document.addEventListener("click", function (e) {
      if (!pop.hidden && !pop.contains(e.target) && e.target !== popBtn) {
        pop.hidden = true;
      }
    });
    document.addEventListener("keydown", function (e) {
      if (e.key === "Escape") pop.hidden = true;
    });
  }

  matchMedia("(prefers-color-scheme: dark)").addEventListener("change", apply);

  syncRadios();
  apply();
})();
