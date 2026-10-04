import { readFile } from 'node:fs/promises';
import { test, expect, type Page } from '@playwright/test';
import { GUIDE_PNGS, TEMPLATES, expectKitDownloads, kitFile } from '../fixtures/design-kit';

// The Head & Tail Studio guide (/customizations/studio/guide) and the ways in to the
// studio. Logged out, on chromium and on the WebKit iPad project.

const GUIDE = '/customizations/studio/guide';
const STUDIO = '/customizations/studio';
/** The guide sections the studio's checks link to ("Learn more"). */
const RULE_ANCHORS = ['colour', 'holes', 'neck', 'direction', 'small', 'fill', 'margins', 'guides'];

let pageErrors: string[] = [];
test.beforeEach(async ({ page }) => {
  pageErrors = [];
  page.on('pageerror', (err) => pageErrors.push(err.message));
});
test.afterEach(() => {
  expect(pageErrors, 'uncaught errors on the page').toEqual([]);
});

async function openGuide(page: Page, hash = '') {
  const response = await page.goto(GUIDE + hash);
  expect(response?.status()).toBe(200);
  await expect(page.getByRole('heading', { level: 1, name: 'Make your own head & tail' })).toBeVisible();
  // Measure in the real web fonts, not the fallback.
  await page.evaluate(() => document.fonts.ready);
}

test.describe('Studio guide', () => {
  test('renders every section the checks and the contents link to', async ({ page }) => {
    await openGuide(page);
    const guide = page.getByTestId('studio-guide');
    await expect(page).toHaveTitle(/Make your own head & tail/);
    await expect(page.locator('meta[name="description"]')).toHaveAttribute('content', /draw a Battlesnake head or tail/);

    for (const id of RULE_ANCHORS) {
      const rule = guide.locator(`[id="${id}"]`);
      await expect(rule, `#${id}`).toHaveCount(1);
      await expect(rule.locator('h3'), `#${id} has a heading`).toBeVisible();
    }
    // Every in-page link lands on exactly one element.
    const hashes = await guide.locator('a[href^="#"]').evaluateAll((els) => els.map((el) => el.getAttribute('href')));
    expect(hashes.length).toBeGreaterThanOrEqual(8);
    for (const hash of hashes) {
      await expect(page.locator(`[id="${(hash as string).slice(1)}"]`), `${hash} exists`).toHaveCount(1);
    }
    // The illustrations: the four-directions board and the two anatomy close-ups.
    await expect(guide.locator('#direction svg.studio-board[role="img"]')).toHaveAttribute('aria-label', /facing right, left, up and down/);
    await expect(guide.locator('#anatomy svg.guide-closeup[role="img"]')).toHaveCount(2);
    // The way back to the studio, and on to Discord.
    await expect(guide.getByRole('link', { name: 'Open the Head & Tail Studio' })).toHaveAttribute('href', STUDIO);
    await expect(guide.getByRole('link', { name: 'Battlesnake Discord' })).toHaveAttribute('href', '/discord');
  });

  test('a link to a rule scrolls to it', async ({ page }) => {
    await openGuide(page, '#neck');
    const rule = page.locator('#neck');
    await expect(rule).toBeInViewport();
    expect(await rule.evaluate((el) => el.matches(':target'))).toBe(true);
    await expect(rule.locator('h3')).toHaveText('The neck is the whole left edge');
  });

  test('every download has a download attribute and serves the committed file', async ({ page, request }) => {
    await openGuide(page);
    await expectKitDownloads(request, page.getByTestId('studio-guide'), [...TEMPLATES, ...GUIDE_PNGS]);
    // Named by app, and by kind for screen readers.
    const templates = page.locator('#templates');
    for (const kind of ['head', 'tail']) {
      await expect(templates.getByRole('link', { name: `Procreate (PSD), ${kind} template`, exact: true })).toBeVisible();
      await expect(templates.getByRole('link', { name: `Illustrator · Inkscape · Affinity (SVG), ${kind} template`, exact: true })).toBeVisible();
      await expect(templates.getByRole('link', { name: `Guides only (PNG), ${kind} guides`, exact: true })).toBeVisible();
    }
  });

  test('tapping the SVG template saves it rather than opening it', async ({ page }) => {
    await openGuide(page);
    const link = page.getByRole('link', { name: 'Illustrator · Inkscape · Affinity (SVG), head template', exact: true });
    const [download] = await Promise.all([page.waitForEvent('download'), link.click()]);
    expect(download.suggestedFilename()).toBe('battlesnake-head-template.svg');
    expect((await readFile(await download.path())).equals(await kitFile('battlesnake-head-template.svg'))).toBe(true);
    expect(new URL(page.url()).pathname, 'still on the guide').toBe(GUIDE);
  });

  test('the four-directions board follows the dark site theme', async ({ page }) => {
    await page.emulateMedia({ colorScheme: 'dark' });
    await openGuide(page);
    expect(await page.evaluate(() => document.documentElement.getAttribute('data-app-theme'))).toBe('dark');
    await expect(page.locator('#direction svg.studio-board')).toHaveCSS('background-color', 'rgb(15, 11, 25)');
  });
});

test.describe('Studio guide layout', () => {
  for (const width of [320, 375]) {
    test(`no horizontal overflow and 44px tap targets at ${width}px`, async ({ page }) => {
      await page.setViewportSize({ width, height: 812 });
      await openGuide(page);

      const scrollWidth = await page.evaluate(() => document.documentElement.scrollWidth);
      expect(scrollWidth, `the guide overflows at ${width}px`).toBeLessThanOrEqual(width);
      const sticking = await page.getByTestId('studio-guide').evaluate((root, w) =>
        [root, ...root.querySelectorAll('*')]
          .filter((el) => el.getBoundingClientRect().width > 0 && el.getBoundingClientRect().right > w + 0.5)
          .map((el) => `${el.tagName.toLowerCase()}.${el.getAttribute('class') ?? ''}`), width);
      expect(sticking, `elements past the right edge at ${width}px`).toEqual([]);

      // Every control is a 44px target, except links inside a sentence (WCAG 2.5.8's
      // inline exception), which are listed so a new one shows up here.
      const controls = await page.getByTestId('studio-guide').locator(':is(a[href], button, summary, input, select)').evaluateAll((els) =>
        els.filter((el) => el.getClientRects().length > 0).map((el) => {
          const box = el.getBoundingClientRect();
          const text = el.textContent?.trim() ?? '';
          const inline = getComputedStyle(el).display === 'inline' &&
            (el.parentElement?.textContent?.trim().length ?? 0) > text.length;
          return { name: el.getAttribute('aria-label') || text, width: box.width, height: box.height, inline };
        }));
      const targets = controls.filter((c) => !c.inline);
      // Contents (7), templates (4), Open the studio.
      expect(targets.length, JSON.stringify(controls.map((c) => c.name))).toBeGreaterThanOrEqual(12);
      for (const c of targets) {
        expect(c.width, `${c.name} width at ${width}px`).toBeGreaterThanOrEqual(44);
        expect(c.height, `${c.name} height at ${width}px`).toBeGreaterThanOrEqual(44);
      }
      expect(controls.filter((c) => c.inline).map((c) => c.name).sort()).toEqual([
        'Battlesnake Discord',
        'Guides only (PNG), head guides',
        'Guides only (PNG), tail guides',
        'Head & Tail Studio',
        'studio',
        'your first head in 10 minutes',
      ]);
    });
  }
});

test.describe('Ways in to the studio', () => {
  test('/studio redirects to the studio, keeping the query', async ({ page, request }) => {
    const bare = await request.get('/studio', { maxRedirects: 0 });
    expect([301, 302, 303, 307, 308]).toContain(bare.status());
    expect(bare.headers()['location']).toBe(STUDIO);

    const response = await page.goto('/studio?from=discord');
    expect(response?.status()).toBe(200);
    const url = new URL(page.url());
    expect(url.pathname + url.search).toBe(`${STUDIO}?from=discord`);
    await expect(page.getByRole('heading', { level: 1, name: 'Head & Tail Studio' })).toBeVisible();
  });

  test('/customizations links to the studio', async ({ page }) => {
    const response = await page.goto('/customizations');
    expect(response?.status()).toBe(200);
    const note = page.locator('.cz-note').filter({ hasText: 'Design your own head or tail in the' });
    await expect(note).toBeVisible();
    const link = note.getByRole('link', { name: 'Head & Tail Studio' });
    await expect(link).toHaveAttribute('href', STUDIO);
    await link.click();
    await expect(page).toHaveURL(new RegExp(`${STUDIO}$`));
    await expect(page.getByRole('heading', { level: 1, name: 'Head & Tail Studio' })).toBeVisible();
  });

  test('the footer links to the studio', async ({ page }) => {
    await page.goto('/customizations');
    const link = page.locator('footer').getByRole('link', { name: 'Head & Tail Studio' });
    await expect(link).toHaveAttribute('href', STUDIO);
    await expect(page.locator('footer').getByRole('link', { name: 'Code of Conduct' })).toBeVisible();
  });
});
