import { readFile } from 'node:fs/promises';
import path from 'node:path';
import { test, expect, type Locator, type Page, type Response } from '@playwright/test';

// Head & Tail Studio (/customizations/studio). Runs logged out (no login is needed),
// on chromium and on the WebKit iPad project.

const STUDIO = '/customizations/studio';
const ENDPOINT = '/customizations/studio/process';
const FIXTURES = path.join(__dirname, '..', 'fixtures', 'studio');
const fixture = (name: string) => path.join(FIXTURES, name);

/** The parts of a 200 from the processing endpoint the page uses. */
interface Processed {
  svg: string;
  path_d: string;
  fill_rule: 'nonzero' | 'evenodd';
  input: string;
  lints: { head: { code: string; fix?: string | null }[]; tail: { code: string }[] };
}

// The endpoint has one processing slot and a global token bucket (20, refilling
// 1/s). Runs that share one server (two projects locally) can briefly hit either;
// those are capacity answers, not failures, so retry them a couple of times.
const CAPACITY = [429, 503];

// The upload tests share one server-wide bucket and slot: keep this file's tests in
// order on one worker.
test.describe.configure({ mode: 'default' });

let pageErrors: string[] = [];
test.beforeEach(async ({ page }) => {
  pageErrors = [];
  page.on('pageerror', (err) => pageErrors.push(err.message));
});
test.afterEach(() => {
  expect(pageErrors, 'uncaught errors on the page').toEqual([]);
});

async function openStudio(page: Page) {
  const response = await page.goto(STUDIO);
  expect(response?.status()).toBe(200);
  await expect(page.getByRole('heading', { level: 1, name: 'Head & Tail Studio' })).toBeVisible();
  // studio.js is deferred; render() has run once the pair selects carry a value.
  await expect(page.locator('#studio-pair-tail')).toHaveValue('default');
}

const isProcess = (r: Response) => r.url().includes(ENDPOINT) && r.request().method() === 'POST';

/** Pick the kind ("This file is a"), upload `name`, and return the endpoint's answer. */
async function upload(page: Page, name: string, kind: 'head' | 'tail' = 'head'): Promise<Processed> {
  await page.locator(`input[name="studio-kind"][value="${kind}"]`).check();
  for (let attempt = 0; ; attempt++) {
    const answer = page.waitForResponse(isProcess);
    await page.locator('#studio-file').setInputFiles(fixture(name));
    const response = await answer;
    if (CAPACITY.includes(response.status()) && attempt < 2) {
      const wait = Number(response.headers()['retry-after'] ?? '2');
      await page.waitForTimeout(Math.min(Math.max(wait, 1), 10) * 1000);
      continue;
    }
    expect(response.status(), `${name}: ${await response.text()}`).toBe(200);
    const body = (await response.json()) as Processed;
    await expect(page.locator('#studio-status')).toContainText(`Done: your ${kind} is on the board`);
    return body;
  }
}

/** Every attribute value of `attr` on the elements `locator` matches. */
async function attrs(locator: Locator, attr: string): Promise<(string | null)[]> {
  return locator.evaluateAll((els, a) => els.map((el) => el.getAttribute(a)), attr);
}

async function expectAllPaths(page: Page, selector: string, d: string, fillRule?: string) {
  const paths = page.locator(selector);
  // Live loop (16 frames) + All directions (4 snakes) + Game size (2 boards x 4 snakes).
  expect(await paths.count()).toBe(28);
  expect(new Set(await attrs(paths, 'd'))).toEqual(new Set([d]));
  if (fillRule) expect(new Set(await attrs(paths, 'fill-rule'))).toEqual(new Set([fillRule]));
}

async function refPath(page: Page, select: string, slug: string): Promise<string> {
  const d = await page.locator(`${select} option[value="${slug}"]`).getAttribute('data-d');
  expect(d, `${select} has ${slug}`).toBeTruthy();
  return d as string;
}

test.describe('Head & Tail Studio', () => {
  test('loads logged out with the default head and tail on every board', async ({ page }) => {
    await openStudio(page);
    expect(page.url()).toContain(STUDIO); // no redirect to a sign-in page
    expect(await page.locator('a[href="/auth/github"]').count(), 'signed out').toBeGreaterThan(0);
    await expect(page.locator('#studio-status')).toHaveText('Try it with your own drawing.');
    await expect(page.locator('#studio-file')).toHaveAttribute('accept', /\.png.*\.svg/);

    const defaultHead = await refPath(page, '#studio-pair-head', 'default');
    const defaultTail = await refPath(page, '#studio-pair-tail', 'default');
    await expectAllPaths(page, 'path.studio-head', defaultHead);
    await expectAllPaths(page, 'path.studio-tail', defaultTail);
    await expect(page.locator('#studio-closeup-path')).toHaveAttribute('d', defaultHead);
    // Every board is a labelled image.
    for (const board of await page.locator('svg.studio-board[role="img"]').all()) {
      await expect(board).toHaveAttribute('aria-label', /default head and the default tail/);
    }
    // Nothing to download or clear yet.
    await expect(page.locator('#studio-download-head')).toBeHidden();
    await expect(page.locator('#studio-clear')).toBeHidden();
  });

  for (const [name, input] of [['head.png', 'png'], ['head.svg', 'svg'], ['head.jpg', 'jpeg']] as const) {
    test(`a ${input.toUpperCase()} upload puts the response path on every head`, async ({ page }) => {
      await openStudio(page);
      const result = await upload(page, name);
      expect(result.input).toBe(input);
      expect(result.svg.startsWith('<svg')).toBe(true);
      expect(result.lints.head, 'a catalog head passes every head check').toEqual([]);

      await expectAllPaths(page, 'path.studio-head', result.path_d, result.fill_rule);
      await expect(page.locator('#studio-closeup-path')).toHaveAttribute('d', result.path_d);
      await expect(page.locator('#studio-result-heading')).toBeFocused();
      await expect(page.getByTestId('studio-pass')).toBeVisible();
      await expect(page.locator('.studio-all')).toHaveAttribute('aria-label', /wearing your head/);
    });
  }

  test('a round blob warns about the neck', async ({ page }) => {
    await openStudio(page);
    const result = await upload(page, 'round-blob.png');
    expect(result.lints.head.map((l) => l.code)).toContain('neck_gap');

    const warning = page.locator('#studio-warnings li.studio-lint[data-code="neck_gap"]');
    await expect(warning).toBeVisible();
    await expect(warning).toContainText('The left edge is where the body joins');
    await expect(page.getByTestId('studio-top-warning')).toBeVisible();
    await expect(page.locator('#studio-status')).toContainText('to check below');
    await expect(page.getByTestId('studio-pass')).toBeHidden();
    // Red brackets mark the gaps beside the close-up's left edge.
    expect(await page.locator('#studio-gaps rect').count()).toBeGreaterThan(0);
  });

  test('a mirrored head offers Flip, and Flip clears the warning', async ({ page }) => {
    await openStudio(page);
    const mirrored = await upload(page, 'mirrored-head.png');
    expect(mirrored.lints.head.map((l) => l.code)).toContain('faces_left');

    const warning = page.locator('#studio-warnings li.studio-lint[data-code="faces_left"]');
    await expect(warning).toBeVisible();
    const flip = warning.locator('.studio-fix[data-fix="flip"]');
    await expect(flip).toHaveText('Flip');

    const answer = page.waitForResponse((r) => isProcess(r) && r.url().includes('fix=flip'));
    await flip.click();
    const flipped = (await (await answer).json()) as Processed;
    expect(flipped.lints.head).toEqual([]);
    await expect(page.locator('#studio-status')).toContainText('It passes every check');
    await expect(page.locator('#studio-warnings li')).toHaveCount(0);
    await expect(page.getByTestId('studio-top-warning')).toBeHidden();
    await expect(page.getByTestId('studio-pass')).toBeVisible();
    expect(flipped.path_d).not.toBe(mirrored.path_d);
    await expectAllPaths(page, 'path.studio-head', flipped.path_d);
  });

  test('a colour change updates the snake colour variable and the computed fill', async ({ page }) => {
    await openStudio(page);
    const studio = page.getByTestId('studio');
    const head = page.locator('.studio-all svg.head').first();
    const body = page.locator('.studio-all .snake > polyline').first();
    await expect(head).toHaveCSS('fill', 'rgb(255, 79, 134)');

    await page.getByRole('radio', { name: 'Blue #3a86ff' }).check();
    expect(await studio.evaluate((el) => el.style.getPropertyValue('--studio-snake'))).toBe('#3a86ff');
    await expect(head).toHaveCSS('fill', 'rgb(58, 134, 255)');
    await expect(body).toHaveCSS('stroke', 'rgb(58, 134, 255)');
    await expect(page.locator('.studio-closeup-path')).toHaveCSS('fill', 'rgb(58, 134, 255)');
    await expect(page.locator('.studio-all')).toHaveAttribute('aria-label', /^Four blue snakes/);

    // A preset with a hint shows it.
    await page.getByRole('radio', { name: 'Charcoal #3d3d3d' }).check();
    await expect(page.locator('#studio-color-hint')).toContainText('#393939');

    // The custom picker.
    await page.getByLabel('Custom colour').fill('#123456');
    await expect(head).toHaveCSS('fill', 'rgb(18, 52, 86)');
    await expect(page.locator('#studio-color-hint')).toBeHidden();
  });

  test('"Pair with" swaps the tail on every board and keeps your head', async ({ page }) => {
    await openStudio(page);
    const result = await upload(page, 'head.png');
    const curled = await refPath(page, '#studio-pair-tail', 'curled');

    await page.getByLabel('Pair your head with').selectOption('curled');
    await expectAllPaths(page, 'path.studio-tail', curled);
    await expectAllPaths(page, 'path.studio-head', result.path_d);
    await expect(page.locator('.studio-all')).toHaveAttribute('aria-label', /your head and the Curled tail/i);
  });

  test('head and tail slots coexist, and "Your tail" appears in Pair', async ({ page }) => {
    await openStudio(page);
    const head = await upload(page, 'head.png', 'head');
    const tail = await upload(page, 'tail.svg', 'tail');
    expect(tail.lints.tail, 'a catalog tail passes every tail check').toEqual([]);

    await expectAllPaths(page, 'path.studio-head', head.path_d);
    await expectAllPaths(page, 'path.studio-tail', tail.path_d);
    await expect(page.locator('#studio-closeup-path')).toHaveAttribute('d', tail.path_d);
    // Working on the tail: pair it with a head, "Your head" chosen.
    const pairHead = page.getByLabel('Pair your tail with');
    await expect(pairHead).toBeVisible();
    await expect(pairHead).toHaveValue('user');
    await expect(pairHead.locator('option[value="user"]')).toHaveText('Your head');
    await expect(page.locator('#studio-pair-tail')).toBeHidden();

    // Back to the head: pair it with a tail, "Your tail" chosen.
    await page.locator('input[name="studio-kind"][value="head"]').check();
    const pairTail = page.getByLabel('Pair your head with');
    await expect(pairTail).toBeVisible();
    await expect(pairTail).toHaveValue('user');
    await expect(pairTail.locator('option[value="user"]')).toHaveText('Your tail');
    await expect(page.locator('#studio-closeup-path')).toHaveAttribute('d', head.path_d);
    await expectAllPaths(page, 'path.studio-tail', tail.path_d);
    await expect(page.locator('#studio-download-head')).toBeVisible();
    await expect(page.locator('#studio-download-tail')).toBeVisible();

    // A reference tail instead, then back to yours.
    await pairTail.selectOption('bolt');
    await expectAllPaths(page, 'path.studio-tail', await refPath(page, '#studio-pair-tail', 'bolt'));
    await pairTail.selectOption('user');
    await expectAllPaths(page, 'path.studio-tail', tail.path_d);

    // Clear puts the defaults back and removes "Your …".
    await page.locator('#studio-clear').click();
    await expectAllPaths(page, 'path.studio-head', await refPath(page, '#studio-pair-head', 'default'));
    await expectAllPaths(page, 'path.studio-tail', await refPath(page, '#studio-pair-tail', 'default'));
    await expect(pairTail.locator('option[value="user"]')).toHaveCount(0);
    await expect(page.locator('#studio-download-head')).toBeHidden();
  });

  test('the All directions board turns your head all four ways', async ({ page }) => {
    await openStudio(page);
    const result = await upload(page, 'head.png');
    const expected: Record<string, string> = {
      right: '',
      left: 'scale(-1,1) translate(-100, 0)',
      up: 'rotate(-90, 50, 50)',
      down: 'rotate(90, 50, 50)',
    };
    const heads = page.locator('.studio-all svg.head');
    await expect(heads).toHaveCount(4);
    for (const [dir, transform] of Object.entries(expected)) {
      const g = page.locator(`.studio-all svg.head.${dir} > g`);
      await expect(g).toHaveCount(1);
      expect(await g.getAttribute('transform'), dir).toBe(transform);
      await expect(g.locator('path.studio-head')).toHaveAttribute('d', result.path_d);
    }
  });

  test('Download head SVG saves the clean path as an SVG file', async ({ page }) => {
    await openStudio(page);
    const result = await upload(page, 'head.png');
    const [download] = await Promise.all([
      page.waitForEvent('download'),
      page.locator('#studio-download-head').click(),
    ]);
    expect(download.suggestedFilename()).toBe('my-battlesnake-head.svg');
    const file = await download.path();
    const svg = await readFile(file, 'utf8');
    expect(svg.startsWith('<svg')).toBe(true);
    expect(svg).toContain(`d="${result.path_d}"`);
    expect(svg).toContain('viewBox="0 0 100 100"');
  });

  test('Save preview image downloads a PNG when sharing files is unavailable', async ({ page }) => {
    // Force the fallback: no Web Share API.
    await page.addInitScript(() => {
      // @ts-expect-error removing the API on purpose
      delete Navigator.prototype.canShare;
      // @ts-expect-error removing the API on purpose
      delete Navigator.prototype.share;
    });
    await openStudio(page);
    const [download] = await Promise.all([
      page.waitForEvent('download'),
      page.locator('#studio-save-image').click(),
    ]);
    expect(download.suggestedFilename()).toBe('my-battlesnake-preview.png');
    const png = await readFile(await download.path());
    expect(png.subarray(0, 8)).toEqual(Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]));
    // 4x the All directions board.
    const { width, height } = await page.locator('.studio-all').evaluate((el) => {
      const vb = (el as SVGSVGElement).viewBox.baseVal;
      return { width: vb.width, height: vb.height };
    });
    expect(png.readUInt32BE(16)).toBe(Math.round(width * 4));
    expect(png.readUInt32BE(20)).toBe(Math.round(height * 4));
    await expect(page.locator('#studio-status')).not.toHaveClass(/error/);
  });

  test('your last preview and settings come back after a reload', async ({ page }) => {
    await openStudio(page);
    const result = await upload(page, 'head.png');
    await page.getByRole('radio', { name: 'Green #2bb673' }).check();
    await page.locator('input[name="studio-theme"][value="dark"]').check();
    await page.locator('input[name="studio-view"][value="all"]').check();
    await page.getByLabel('Pair your head with').selectOption('curled');

    const saved = await page.evaluate(() => JSON.parse(localStorage.getItem('arena:studio:v1') || 'null'));
    expect(saved).toMatchObject({ v: 1, kind: 'head', color: '#2bb673', theme: 'dark', view: 'all' });

    await page.reload();
    await expect(page.locator('#studio-status')).toHaveText('Welcome back: your last preview is restored.');
    await expectAllPaths(page, 'path.studio-head', result.path_d);
    await expectAllPaths(page, 'path.studio-tail', await refPath(page, '#studio-pair-tail', 'curled'));
    await expect(page.locator('#studio-pair-tail')).toHaveValue('curled');
    await expect(page.getByRole('radio', { name: 'Green #2bb673' })).toBeChecked();
    await expect(page.locator('.studio-all svg.head').first()).toHaveCSS('fill', 'rgb(43, 182, 115)');
    await expect(page.locator('input[name="studio-theme"][value="dark"]')).toBeChecked();
    for (const board of await page.locator('svg.studio-board').all()) {
      await expect(board).toHaveClass(/\bdark\b/);
    }
    await expect(page.locator('#studio-panes')).toHaveAttribute('data-view', 'all');
    await expect(page.locator('#studio-download-head')).toBeVisible();
  });

  test('tampered saved state is ignored', async ({ page }) => {
    await openStudio(page);
    const defaultHead = await refPath(page, '#studio-pair-head', 'default');
    await page.evaluate(() => {
      localStorage.setItem('arena:studio:v1', JSON.stringify({
        v: 1, kind: 'head', view: 'nope', theme: 'purple', color: 'red;background:url(x)',
        slots: { head: { d: 'M0 0"/><script>alert(1)</script>', fillRule: 'nonzero' }, tail: null },
        pair: { head: 'user', tail: '../../etc' },
      }));
    });
    await page.reload();
    await expect(page.locator('#studio-status')).toHaveText('Try it with your own drawing.');
    await expectAllPaths(page, 'path.studio-head', defaultHead);
    await expect(page.locator('#studio-pair-tail')).toHaveValue('default');
    expect(await page.getByTestId('studio').evaluate((el) => el.style.getPropertyValue('--studio-snake'))).toBe('#ff4f86');
    await expect(page.locator('#studio-download-head')).toBeHidden();
  });

  test('a PSD gets a friendly "export a PNG" message', async ({ page, request }) => {
    await openStudio(page);
    const posted: string[] = [];
    page.on('request', (r) => { if (r.url().includes(ENDPOINT)) posted.push(r.url()); });
    await page.locator('#studio-file').setInputFiles(fixture('template.psd'));
    const status = page.locator('#studio-status');
    await expect(status).toContainText("That's a PSD");
    await expect(status).toContainText('export a PNG');
    await expect(status).toHaveClass(/error/);
    expect(posted, 'the browser answers without uploading').toEqual([]);
    await expect(page.locator('#studio-download-head')).toBeHidden();

    // The server says the same if the file gets past the browser's check.
    const response = await request.post(ENDPOINT, {
      data: await readFile(fixture('template.psd')),
      headers: { 'Content-Type': 'application/octet-stream' },
    });
    expect(response.status()).toBe(422);
    const body = await response.json();
    expect(body.error.code).toBe('unsupported_format');
    expect(body.error.message).toContain('PNG');
  });
});

test.describe('Head & Tail Studio layout', () => {
  // Every visible studio control: buttons, selects, inputs (radios cover their
  // labels), the file input (covers the drop zone), and the Details summary.
  const CONTROLS = '#studio :is(button, select, input, summary, a[href])';

  for (const width of [320, 375, 820]) {
    test(`no horizontal overflow and 44px controls at ${width}px`, async ({ page }) => {
      await page.setViewportSize({ width, height: 900 });
      await openStudio(page);
      // A mirrored head, so the warning, Flip, relabel, download and clear show too.
      await upload(page, 'mirrored-head.png');
      await expect(page.locator('#studio-top-fix')).toBeVisible();
      await page.evaluate(() => document.fonts.ready);

      for (const view of ['closeup', 'live', 'all', 'game']) {
        const option = page.locator(`input[name="studio-view"][value="${view}"]`);
        // At 640px+ Live sits beside Close-up and has no option of its own.
        if (await option.isVisible()) await option.check();
        const scrollWidth = await page.evaluate(() => document.documentElement.scrollWidth);
        expect(scrollWidth, `${view} view overflows at ${width}px`).toBeLessThanOrEqual(width);

        let checked = 0;
        for (const control of await page.locator(CONTROLS).all()) {
          if (!(await control.isVisible())) continue;
          checked++;
          const box = await control.boundingBox();
          const name = await control.evaluate((el) =>
            `${el.tagName.toLowerCase()}#${el.id || ''}[${el.getAttribute('aria-label') || el.textContent?.trim().slice(0, 30) || (el as HTMLInputElement).value || ''}]`);
          expect(box?.width, `${name} width at ${width}px (${view})`).toBeGreaterThanOrEqual(44);
          expect(box?.height, `${name} height at ${width}px (${view})`).toBeGreaterThanOrEqual(44);
        }
        // File, kind x2, views, Flip x2, relabel, swatches x10, theme x2, pair, actions.
        expect(checked, `controls checked in the ${view} view`).toBeGreaterThanOrEqual(20);
      }
      for (const input of await page.locator('#studio :is(input, select, textarea)').all()) {
        const size = await input.evaluate((el) => parseFloat(getComputedStyle(el).fontSize));
        expect(size, `font size of ${await input.evaluate((el) => el.outerHTML.slice(0, 80))}`).toBeGreaterThanOrEqual(16);
      }
    });
  }
});
