import { readFile } from 'node:fs/promises';
import path from 'node:path';
import { expect, type APIRequestContext, type Locator } from '@playwright/test';

/** The committed design kit (server/static/design-kit/), which the server embeds. */
export const KIT_DIR = path.join(__dirname, '..', '..', 'server', 'static', 'design-kit');

export const kitFile = (name: string) => readFile(path.join(KIT_DIR, name));

/** The four layered templates: a PSD (Procreate) and an SVG (vector apps) per kind. */
export const TEMPLATES = [
  'battlesnake-head-template.psd',
  'battlesnake-head-template.svg',
  'battlesnake-tail-template.psd',
  'battlesnake-tail-template.svg',
];
/** The guides alone, per kind, for other apps. */
export const GUIDE_PNGS = ['battlesnake-head-guide.png', 'battlesnake-tail-guide.png'];

// What /static serves each kind of file as. PSDs have no registered type in the
// server's table, and octet-stream is what makes Safari save one to Files.
const CONTENT_TYPES: Record<string, RegExp> = {
  psd: /^(application\/octet-stream|image\/vnd\.adobe\.photoshop)$/,
  svg: /^image\/svg\+xml\b/,
  png: /^image\/png$/,
};

/**
 * Every design-kit link inside `scope` has a `download` attribute naming its file, points
 * at a versioned /static/design-kit/ URL, and serves the committed file byte for byte.
 * Together the links offer exactly `files`, once each.
 */
export async function expectKitDownloads(request: APIRequestContext, scope: Locator, files: string[]) {
  const links = await scope.locator('a[href*="design-kit/"]').evaluateAll((els) =>
    els.map((el) => ({
      href: el.getAttribute('href') ?? '',
      download: el.getAttribute('download'),
      name: el.getAttribute('aria-label') ?? el.textContent?.trim() ?? '',
    })));
  expect(links.map((l) => l.download).sort(), 'one link with a download attribute per file').toEqual([...files].sort());

  for (const link of links) {
    const file = link.download as string;
    const url = new URL(link.href, 'http://localhost');
    expect(url.pathname, link.name).toBe(`/static/design-kit/${file}`);
    // /static is cached for a year: every link carries the content version.
    expect(url.search, `${link.name} is versioned`).toMatch(/^\?v=[0-9a-f]{16}$/);

    const response = await request.get(link.href);
    expect(response.status(), link.href).toBe(200);
    const type = CONTENT_TYPES[file.split('.').pop() ?? ''];
    expect(type, `a known kind of file: ${file}`).toBeTruthy();
    expect(response.headers()['content-type'], file).toMatch(type);
    const body = await response.body();
    const committed = await kitFile(file);
    expect(body.length, `${file} size`).toBe(committed.length);
    expect(body.equals(committed), `${file} is the committed file`).toBe(true);
  }
}
