(() => {
  const script = document.currentScript as HTMLScriptElement | null;
  const fallback = script?.previousElementSibling as HTMLElement | null;
  if (!script || !fallback) return;
  const revealFallback = (): void => { fallback.hidden = false; };
  const intervalMs = Number(script.dataset.intervalMs);
  const maxTicks = Number(script.dataset.maxTicks);
  const storageKey = `arena:live-refresh:${location.pathname}${location.search}:${script.dataset.stateKey}`;

  const whenVisible = (callback: () => void): void => {
    if (!document.hidden) { callback(); return; }
    document.addEventListener('visibilitychange', () => whenVisible(callback), { once: true });
  };

  const schedule = (): void => {
    if (document.hidden) { whenVisible(schedule); return; }
    let ticks: number;
    try {
      const stored = sessionStorage.getItem(storageKey);
      ticks = stored === null ? 0 : Number.parseInt(stored, 10);
      if (!Number.isSafeInteger(ticks) || ticks < 0) ticks = 0;
    } catch (_) { revealFallback(); return; }
    if (ticks >= maxTicks) { revealFallback(); return; }
    try { sessionStorage.setItem(storageKey, String(ticks + 1)); }
    catch (_) { revealFallback(); return; }
    setTimeout(() => whenVisible(() => location.reload()), intervalMs);
  };
  schedule();
})();
