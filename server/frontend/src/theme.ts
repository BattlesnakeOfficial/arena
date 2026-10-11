import { preference, resolvedTheme, type ThemeAxis } from './theme-preferences';

(() => {
  const root = document.documentElement;
  const theaterPage = root.hasAttribute('data-theater-page');
  const authed = root.hasAttribute('data-authed');
  const pop = document.getElementById('appearance');

  function apply(): void {
    root.setAttribute('data-app-theme', resolvedTheme(root));
  }

  function persist(axis: ThemeAxis, value: string): void {
    localStorage.setItem('bs-' + axis, value);
    root.setAttribute('data-bs-' + axis, value);
    if (authed) {
      const body = 'site=' + encodeURIComponent(preference(root, 'site', 'system')) +
        '&theater=' + encodeURIComponent(preference(root, 'theater', 'dark'));
      fetch('/settings/appearance', {
        method: 'POST',
        headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
        body,
      }).catch(() => { /* offline / transient — localStorage still holds the choice */ });
    }
  }

  function syncRadios(): void {
    if (!pop) return;
    const site = preference(root, 'site', 'system');
    const theater = preference(root, 'theater', 'dark');
    pop.querySelectorAll<HTMLInputElement>('input[name=site]').forEach((radio) => {
      radio.checked = radio.value === site;
    });
    pop.querySelectorAll<HTMLInputElement>('input[name=theater]').forEach((radio) => {
      radio.checked = radio.value === theater;
    });
  }

  const toggle = document.getElementById('theme-toggle');
  toggle?.addEventListener('click', () => {
    const next = resolvedTheme(root) === 'dark' ? 'light' : 'dark';
    persist(theaterPage ? 'theater' : 'site', next);
    apply();
    syncRadios();
  });

  const popBtn = document.getElementById('appearance-btn');
  if (popBtn && pop) {
    popBtn.addEventListener('click', (event) => {
      event.stopPropagation();
      pop.hidden = !pop.hidden;
    });
    pop.addEventListener('change', (event) => {
      if (!(event.target instanceof HTMLInputElement)) return;
      persist(event.target.name === 'site' ? 'site' : 'theater', event.target.value);
      apply();
    });
    document.addEventListener('click', (event) => {
      if (!pop.hidden && event.target instanceof Node && !pop.contains(event.target) && event.target !== popBtn) {
        pop.hidden = true;
      }
    });
    document.addEventListener('keydown', (event) => {
      if (event.key === 'Escape') pop.hidden = true;
    });
  }

  matchMedia('(prefers-color-scheme: dark)').addEventListener('change', apply);
  syncRadios();
  apply();
})();
