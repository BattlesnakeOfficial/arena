export type ThemeAxis = 'site' | 'theater';

export function preference(root: HTMLElement, axis: ThemeAxis, fallback: string): string {
  return root.getAttribute('data-bs-' + axis) || localStorage.getItem('bs-' + axis) || fallback;
}

export function resolvedTheme(root: HTMLElement): string {
  const site = preference(root, 'site', 'system');
  const theater = preference(root, 'theater', 'dark');
  const system = matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
  const resolvedSite = site === 'system' ? system : site;
  return root.hasAttribute('data-theater-page')
    ? (theater === 'match' ? resolvedSite : theater)
    : resolvedSite;
}
