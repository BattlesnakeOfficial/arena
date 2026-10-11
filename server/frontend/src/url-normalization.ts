(() => {
  const el = document.getElementById('url') as HTMLInputElement | null;
  if (!el) return;
  el.addEventListener('change', () => {
    const value = el.value.trim();
    if (value && value.indexOf('://') === -1) el.value = 'https://' + value;
  });
})();
