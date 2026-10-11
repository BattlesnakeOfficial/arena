(() => {
  const bell = document.getElementById('notification-bell');
  const dialog = document.getElementById('notification-drawer');
  if (!bell) return;

  const badge = bell.querySelector<HTMLElement>('.notification-badge');
  if (!badge) return;
  const updateBadge = (count: number) => {
    if (!Number.isInteger(count) || count < 0 || count > 10) return;
    const label = count >= 10 ? '9 or more unread notifications' : count === 1 ? '1 unread notification' : `${count} unread notifications`;
    const shown = count >= 10 ? '9+' : String(count);
    if (bell.getAttribute('aria-label') !== label) bell.setAttribute('aria-label', label);
    if (badge.textContent?.trim() !== shown) badge.textContent = shown;
    if (badge.hidden !== (count === 0)) badge.hidden = count === 0;
  };

  let timer: ReturnType<typeof setInterval> | undefined;
  let stopped = false;
  let pollGeneration = 0;
  async function refreshBadge() {
    const generation = ++pollGeneration;
    try {
      const response = await fetch('/notifications/count', { credentials: 'same-origin', cache: 'no-store' });
      if (generation !== pollGeneration) return;
      if (!response.ok || !response.headers.get('content-type')?.includes('application/json')) {
        stopped = true;
        clearInterval(timer);
        return;
      }
      const data: { count: number } = await response.json();
      if (generation === pollGeneration) updateBadge(data.count);
    } catch {
      // A transient network failure keeps the ordinary bell link usable.
    }
  }
  function setPolling() {
    clearInterval(timer);
    pollGeneration++;
    if (document.visibilityState !== 'visible' || stopped) return;
    refreshBadge();
    timer = setInterval(refreshBadge, 60000);
  }
  document.addEventListener('visibilitychange', setPolling);
  window.addEventListener('pagehide', () => { clearInterval(timer); pollGeneration++; });
  setPolling();

  if (!(dialog instanceof HTMLDialogElement) || typeof dialog.showModal !== 'function') return;
  const drawer = dialog;
  const close = document.getElementById('notification-drawer-close');
  const status = document.getElementById('notification-drawer-status');
  const content = document.getElementById('notification-drawer-content');
  if (!close || !status || !content) return;
  let drawerGeneration = 0;

  function showError() {
    status!.replaceChildren();
    const message = document.createElement('span');
    message.textContent = 'Notifications could not load. ';
    const retry = document.createElement('button');
    retry.type = 'button';
    retry.textContent = 'Retry';
    retry.addEventListener('click', loadFirst);
    status!.append(message, retry);
  }
  async function loadFirst() {
    const generation = ++drawerGeneration;
    status!.textContent = 'Loading notifications…';
    try {
      const response = await fetch('/notifications/drawer', { credentials: 'same-origin', cache: 'no-store' });
      if (!response.ok) throw new Error('Unable to load notifications');
      const parsed = new DOMParser().parseFromString(await response.text(), 'text/html');
      if (generation !== drawerGeneration || !drawer.open) return;
      content!.replaceChildren(...parsed.body.children);
      status!.textContent = '';
    } catch {
      if (generation === drawerGeneration && drawer.open) showError();
    }
  }
  bell.addEventListener('click', (event) => {
    if (event.metaKey || event.ctrlKey || event.shiftKey || event.altKey || event.button !== 0) return;
    event.preventDefault();
    drawer.showModal();
    close.focus();
    loadFirst();
  });
  close.addEventListener('click', () => drawer.close());
  drawer.addEventListener('close', () => { drawerGeneration++; bell.focus(); });

  content.addEventListener('click', async (event) => {
    const button = event.target instanceof Element ? event.target.closest<HTMLButtonElement>('.notification-load-more') : null;
    if (!button || button.disabled) return;
    button.disabled = true;
    const generation = drawerGeneration;
    const query = new URLSearchParams({ before: button.dataset.before ?? '', before_id: button.dataset.beforeId ?? '' });
    try {
      const response = await fetch(`/notifications/more?${query}`, { credentials: 'same-origin', cache: 'no-store' });
      if (!response.ok) throw new Error('Unable to load more');
      const parsed = new DOMParser().parseFromString(await response.text(), 'text/html');
      const batch = parsed.querySelector('.notification-batch');
      if (!batch || generation !== drawerGeneration || !drawer.open) return;
      const list = content.querySelector('.notification-items');
      if (!list) return;
      list.append(...batch.querySelectorAll('.notification-items > li'));
      button.replaceWith(...batch.querySelectorAll('.notification-load-more'));
      status.textContent = '';
    } catch {
      button.disabled = false;
      status.textContent = 'Could not load more notifications. Try again.';
    }
  });

  content.addEventListener('submit', async (event) => {
    const form = event.target instanceof Element ? event.target.closest<HTMLFormElement>('.notification-mark-all') : null;
    if (!form) return;
    event.preventDefault();
    const button = form.querySelector<HTMLButtonElement>('button');
    if (!button) return;
    button.disabled = true;
    try {
      const response = await fetch(form.action, {
        method: 'POST', body: new FormData(form), credentials: 'same-origin', redirect: 'manual'
      });
      if (response.type !== 'opaqueredirect' && !response.ok) throw new Error('Unable to mark all read');
      content.querySelectorAll('.notification-item.unread').forEach((item) => {
        item.classList.remove('unread');
        item.querySelector('.notification-unread-marker')?.remove();
        item.querySelector('.notification-unread-label')?.remove();
      });
      refreshBadge();
      status.textContent = '';
    } catch {
      status.textContent = 'Could not mark notifications read. Try again.';
    } finally {
      button.disabled = false;
    }
  });
})();
