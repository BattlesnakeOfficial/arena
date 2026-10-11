type Settings = { board_size: string; game_type: string };
type DeferredAction = { kind: 'link'; href: string } | {
  kind: 'form'; action: string; method: string; entries: [string, FormDataEntryValue][];
};

(() => {
  const settings = document.getElementById('game-settings') as HTMLFormElement | null;
  const alert = document.getElementById('configure-error');
  if (!settings || !alert) return;
  let editVersion = 0;
  let persistedVersion = 0;
  let latest = snapshot();
  let inFlight = false;
  let paused = false;
  let retained: DeferredAction | null = null;

  function snapshot(): Settings {
    return {
      board_size: (settings!.elements.namedItem('board_size') as HTMLSelectElement).value,
      game_type: (settings!.elements.namedItem('game_type') as HTMLSelectElement).value,
    };
  }
  function setActionsDisabled(disabled: boolean): void {
    document.querySelectorAll<HTMLElement>('[data-discovery-action]').forEach((el) => {
      if (el.matches('a')) {
        el.setAttribute('aria-disabled', disabled ? 'true' : 'false');
        el.style.pointerEvents = disabled ? 'none' : '';
      }
      el.querySelectorAll<HTMLInputElement | HTMLButtonElement>('button, input').forEach((control) => {
        control.disabled = disabled;
      });
    });
  }
  function captureForm(form: HTMLFormElement, submitter: HTMLElement | null): DeferredAction {
    const data = submitter ? new FormData(form, submitter) : new FormData(form);
    return {
      kind: 'form', action: form.action,
      method: (form.method || 'get').toLowerCase(),
      entries: Array.from(data.entries()),
    };
  }
  function run(action: DeferredAction): void {
    retained = null;
    setActionsDisabled(false);
    if (action.kind === 'link') { window.location.assign(action.href); return; }
    const form = document.createElement('form');
    form.method = action.method;
    form.action = action.action;
    action.entries.forEach(([name, value]) => {
      const input = document.createElement('input');
      input.type = 'hidden'; input.name = name; input.value = String(value);
      form.appendChild(input);
    });
    document.body.appendChild(form);
    form.submit();
  }
  function maybeRun(): void {
    if (retained && !paused && !inFlight && persistedVersion === editVersion) run(retained);
  }
  function pump(): void {
    if (paused || inFlight || persistedVersion === editVersion) { maybeRun(); return; }
    const sendingVersion = editVersion;
    const sending = latest;
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 15000);
    inFlight = true;
    fetch(settings!.dataset.configureUrl!, {
      method: 'POST', body: new URLSearchParams(sending), signal: controller.signal,
    }).then((response) => {
      if (!response.ok) throw new Error('configure failed');
      persistedVersion = Math.max(persistedVersion, sendingVersion);
    }).catch(() => {
      paused = true;
      alert!.hidden = false;
    }).finally(() => {
      clearTimeout(timer);
      inFlight = false;
      if (!paused) pump();
    });
  }
  settings.querySelectorAll('select').forEach((el) => {
    el.addEventListener('change', () => { editVersion += 1; latest = snapshot(); pump(); });
  });
  document.addEventListener('click', (event) => {
    const link = event.target instanceof Element ? event.target.closest<HTMLAnchorElement>('a[data-discovery-action]') : null;
    if (!link) return;
    event.preventDefault();
    if (retained) return;
    retained = { kind: 'link', href: link.href };
    setActionsDisabled(true);
    pump();
  });
  document.addEventListener('submit', (event) => {
    const form = event.target instanceof Element ? event.target.closest<HTMLFormElement>('form[data-discovery-action]') : null;
    if (!form) return;
    event.preventDefault();
    if (retained) return;
    retained = captureForm(form, event.submitter);
    setActionsDisabled(true);
    pump();
  });
  alert.querySelector('[data-configure-retry]')?.addEventListener('click', () => {
    paused = false; alert.hidden = true; pump();
  });
  alert.querySelector('[data-configure-cancel]')?.addEventListener('click', () => {
    retained = null; paused = false; alert.hidden = true;
    setActionsDisabled(false); pump();
  });
})();
