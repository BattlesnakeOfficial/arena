document.addEventListener('click', (event) => {
  const target = event.target instanceof Element ? event.target : null;
  const confirmed = target?.closest<HTMLElement>('[data-confirm]');
  if (confirmed && !window.confirm(confirmed.dataset.confirm ?? '')) {
    event.preventDefault();
    return;
  }
  if (target?.closest('[data-manual-refresh]')) {
    event.preventDefault();
    window.location.reload();
  }
});
