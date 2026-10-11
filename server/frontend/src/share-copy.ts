(() => {
  const btn = document.getElementById('share-copy');
  const input = document.getElementById('share-url') as HTMLInputElement | null;
  if (!btn || !input) return;
  function done(): void {
    btn!.textContent = 'Copied!';
    setTimeout(() => { btn!.textContent = 'Copy Link'; }, 1500);
  }
  function fallback(): void {
    input!.focus();
    input!.select();
    try { document.execCommand('copy'); done(); } catch (_) { /* unchanged fallback */ }
  }
  btn.addEventListener('click', () => {
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(input.value).then(done, fallback);
    } else {
      fallback();
    }
  });
})();
