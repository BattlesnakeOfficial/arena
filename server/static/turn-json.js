// Turn JSON form on the game page (routes/game/view.rs).
//
// Without JS the form is a plain GET whose attachment response downloads in
// place, and an error (a 404 like "eliminated on turn N...") replaces the page
// with its plain-text reason. This script layers on top of that:
// - keeps the turn input on the turn the board iframe is showing,
// - downloads through fetch, so errors show inline under the form,
// - reveals a Copy button that puts the JSON on the clipboard.
(function () {
  "use strict";
  const form = document.getElementById("move-request-form");
  if (!form) return;
  const turnInput = document.getElementById("move-request-turn");
  const error = document.getElementById("move-request-error");
  const copyButton = document.getElementById("move-request-copy");
  const downloadButton = form.querySelector("button[type=submit]");

  // The board posts {event: "TURN", data: {turn}} on every playback change.
  window.addEventListener("message", (e) => {
    if (e.origin !== "https://board.battlesnake.com") return;
    if (e.data && e.data.event === "TURN") turnInput.value = e.data.data.turn;
  });

  function showError(message) {
    error.textContent = message;
    error.hidden = false;
  }

  // The request for the form's turn and snake. Rejects with the server's
  // plain-text reason, or a network message.
  async function fetchRequest() {
    error.hidden = true;
    const url = form.action + "?" + new URLSearchParams(new FormData(form));
    let res;
    try {
      res = await fetch(url);
    } catch (e) {
      throw new Error("Couldn't reach Arena. Try again.");
    }
    const body = await res.text();
    if (!res.ok) throw new Error(body || `Request failed (${res.status}).`);
    const name = /filename="([^"]+)"/.exec(res.headers.get("Content-Disposition") || "");
    return { body, filename: name ? name[1] : "move-request.json" };
  }

  async function whileBusy(button, work) {
    button.disabled = true;
    try {
      await work;
    } finally {
      button.disabled = false;
    }
  }

  function download(body, filename) {
    const a = document.createElement("a");
    a.href = URL.createObjectURL(new Blob([body], { type: "application/json" }));
    a.download = filename;
    document.body.appendChild(a);
    a.click();
    a.remove();
    setTimeout(() => URL.revokeObjectURL(a.href), 10000);
  }

  form.addEventListener("submit", (e) => {
    e.preventDefault();
    const request = fetchRequest();
    whileBusy(downloadButton, request).catch(() => {});
    request.then((r) => download(r.body, r.filename), (err) => showError(err.message));
  });

  if (!window.isSecureContext || !navigator.clipboard) return;
  copyButton.hidden = false;
  copyButton.addEventListener("click", () => {
    if (!form.reportValidity()) return;
    let fetchError = null;
    const text = fetchRequest().then(
      (r) => r.body,
      (err) => {
        fetchError = err;
        throw err;
      },
    );
    // Safari only allows clipboard writes during the click itself, so hand it
    // an item that resolves once the fetch does.
    const write = window.ClipboardItem && navigator.clipboard.write
      ? navigator.clipboard.write([
        new ClipboardItem({ "text/plain": text.then((t) => new Blob([t], { type: "text/plain" })) }),
      ])
      : text.then((t) => navigator.clipboard.writeText(t));
    whileBusy(copyButton, write).then(
      () => {
        copyButton.textContent = "Copied!";
        setTimeout(() => { copyButton.textContent = "Copy"; }, 1500);
      },
      () => showError(fetchError ? fetchError.message : "Couldn't copy to the clipboard. Try Download."),
    );
  });
})();
