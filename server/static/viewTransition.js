window.addEventListener("pageswap", () => {
  // Save scroll position to sessionStorage before the transition
  sessionStorage.setItem("scrollPosition", window.scrollY.toString());
});

// On the loaded page after navigation
window.addEventListener("pagereveal", () => {
  // Restore scroll position before the new page renders.
  const savedPosition = sessionStorage.getItem("scrollPosition");
  if (!savedPosition) return;

  // `navigation.activation.from` is null on a fresh load (no previous entry),
  // so guard before reading `.url` — otherwise every such load throws
  // "Cannot read properties of null (reading 'url')" into the console.
  const activation = window.navigation?.activation;
  if (!activation?.from || !activation.entry) return;

  const fromURL = new URL(activation.from.url);
  const currentURL = new URL(activation.entry.url);
  if (fromURL.pathname === currentURL.pathname) {
    window.scrollTo(0, parseInt(savedPosition));
  }
});
