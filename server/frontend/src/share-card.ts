(() => {
  const root = document.querySelector<HTMLElement>('[data-share-page]');
  if (!root) return;
  const pageUrl = root.dataset.shareUrl || '';
  const base = root.dataset.shareBase || '';
  const post = (endpoint: string, data: Record<string, string>): void => {
    fetch(`${base}/${endpoint}`, {
      method: 'POST',
      body: new URLSearchParams(data),
      keepalive: true,
    }).catch(() => { /* metrics are best effort */ });
  };

  const native = document.getElementById('share-native');
  if (native && navigator.share) {
    native.hidden = false;
    native.addEventListener('click', () => {
      // Record only when the sheet completes: dismissing it is not a share.
      navigator.share({ url: pageUrl, title: document.title })
        .then(() => post('share-action', { channel: 'native' }))
        .catch(() => {});
    });
  }

  const copy = document.getElementById('share-copy') as HTMLButtonElement | null;
  const input = document.getElementById('share-url') as HTMLInputElement | null;
  if (copy && input) {
    const done = (): void => {
      post('share-action', { channel: 'copy' });
      copy.textContent = 'Copied!';
      setTimeout(() => { copy.textContent = 'Copy Link'; }, 1500);
    };
    const fallback = (): void => {
      input.focus();
      input.select();
      // execCommand signals failure by returning false, not by throwing;
      // a failed copy is not a share action.
      try {
        if (document.execCommand('copy')) done();
      } catch (_) { /* copy failed; nothing to record */ }
    };
    copy.addEventListener('click', () => {
      if (navigator.clipboard && navigator.clipboard.writeText) {
        navigator.clipboard.writeText(input.value).then(done, fallback);
      } else {
        fallback();
      }
    });
  }

  const download = document.getElementById('share-download');
  if (download) {
    download.addEventListener('click', () => post('share-action', { channel: 'download' }));
  }

  for (const cta of document.querySelectorAll<HTMLAnchorElement>('[data-share-cta]')) {
    cta.addEventListener('click', () => post('cta-click', { cta: cta.dataset.shareCta || '' }));
  }
})();
