import { readFile } from 'node:fs/promises';
import path from 'node:path';
import { test, expect, type Locator, type Page, type Response } from '@playwright/test';
import { TEMPLATES, expectKitDownloads, kitFile } from '../fixtures/design-kit';

// Head & Tail Studio (/customizations/studio). Runs logged out (no login is needed),
// on chromium and on the WebKit iPad project.

const STUDIO = '/customizations/studio';
const GUIDE = '/customizations/studio/guide';
const ENDPOINT = '/customizations/studio/process';
const FIXTURES = path.join(__dirname, '..', 'fixtures', 'studio');
const fixture = (name: string) => path.join(FIXTURES, name);

/** The parts of a 200 from the processing endpoint the page uses. */
interface Processed {
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

/** Do `act` and return the endpoint's answer to it, retrying capacity answers. */
async function answerTo(page: Page, act: () => Promise<unknown>, match = isProcess): Promise<Response> {
  for (let attempt = 0; ; attempt++) {
    const answer = page.waitForResponse(match);
    await act();
    const response = await answer;
    if (CAPACITY.includes(response.status()) && attempt < 2) {
      const wait = Number(response.headers()['retry-after'] ?? '2');
      await page.waitForTimeout(Math.min(Math.max(wait, 1), 10) * 1000);
      continue;
    }
    return response;
  }
}

/** Pick the kind ("Upload as"), upload `name`, and return the endpoint's answer. */
async function upload(page: Page, name: string, kind: 'head' | 'tail' = 'head'): Promise<Processed> {
  await page.locator(`input[name="studio-kind"][value="${kind}"]`).check();
  const response = await answerTo(page, () => page.locator('#studio-file').setInputFiles(fixture(name)));
  expect(response.status(), `${name}: ${await response.text()}`).toBe(200);
  const body = (await response.json()) as Processed;
  await expect(page.locator('#studio-status')).toContainText(`Done: your ${kind} is on the board`);
  return body;
}

/** Tap a Flip/Fit button and return the fixed shape. */
async function applyFix(page: Page, button: Locator, fix: 'flip' | 'fit'): Promise<Processed> {
  const response = await answerTo(page, () => button.click(), (r) => isProcess(r) && r.url().includes(`fix=${fix}`));
  expect(response.status(), await response.text()).toBe(200);
  return (await response.json()) as Processed;
}

/** The SVG file the page builds from a saved path (downloads, and Flip/Fit after a reload). */
const pageSvg = (r: Processed) =>
  `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><path fill-rule="${r.fill_rule}" d="${r.path_d}"/></svg>`;

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
      expect(result, 'the page builds the SVG file itself').not.toHaveProperty('svg');
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

    const flipped = await applyFix(page, flip, 'flip');
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
    expect(svg).toBe(pageSvg(result));
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

test.describe('Head & Tail Studio: recovering', () => {
  test('Flip still works after a reload, by re-posting the saved path', async ({ page }) => {
    await openStudio(page);
    const mirrored = await upload(page, 'mirrored-head.png');
    await page.reload();
    await expect(page.locator('#studio-status')).toHaveText('Welcome back: your last preview is restored.');
    const flip = page.locator('#studio-warnings li[data-code="faces_left"] .studio-fix[data-fix="flip"]');
    await expect(flip).toBeVisible();
    await expect(page.locator('#studio-top-fix')).toBeVisible();

    const sent = page.waitForRequest((r) => r.url().includes(ENDPOINT) && r.url().includes('fix=flip'));
    const flipped = await applyFix(page, flip, 'flip');
    expect((await sent).postDataBuffer()?.toString('utf8')).toBe(pageSvg(mirrored));
    expect(flipped.input).toBe('svg');
    expect(flipped.lints.head).toEqual([]);
    await expect(page.locator('#studio-warnings li')).toHaveCount(0);
    await expect(page.getByTestId('studio-pass')).toBeVisible();
    await expectAllPaths(page, 'path.studio-head', flipped.path_d);
  });

  test('a failed upload keeps the last result, and its Flip still works', async ({ page }) => {
    await openStudio(page);
    await upload(page, 'mirrored-head.png');
    const status = page.locator('#studio-status');
    // Not a format the browser can rule out, so the server answers.
    const rejected = await answerTo(page, () =>
      page.locator('#studio-file').setInputFiles({ name: 'notes.png', mimeType: 'image/png', buffer: Buffer.from('just some text') }));
    expect(rejected.status()).toBe(422);
    await expect(status).toHaveText("We couldn't tell what kind of file this is. Upload a PNG, JPEG or SVG.");
    await expect(status).toHaveClass(/error/);

    const flip = page.locator('#studio-warnings .studio-fix[data-fix="flip"]');
    await expect(flip).toBeVisible();
    const flipped = await applyFix(page, flip, 'flip');
    expect(flipped.lints.head).toEqual([]);
    await expect(status).toContainText('It passes every check');
    await expectAllPaths(page, 'path.studio-head', flipped.path_d);
  });

  test('Tail after uploading a tail as a head offers to move it, never over your own', async ({ page }) => {
    await openStudio(page);
    const defaultHead = await refPath(page, '#studio-pair-head', 'default');
    const status = page.locator('#studio-status');
    const relabel = page.locator('#studio-relabel');
    const tail = await upload(page, 'tail.svg', 'head'); // the wrong kind
    await expect(relabel).toHaveText('Use it as a tail instead');

    await page.locator('input[name="studio-kind"][value="tail"]').check();
    await expect(status).toHaveText('Your next upload will be your tail.');
    await expect(page.locator('#studio-closeup')).toHaveAttribute('aria-label', 'Close-up of the default tail');
    await expect(relabel).toHaveText('Use the file you just uploaded as your tail');
    await relabel.click();
    await expect(status).toHaveText('Moved: this file is now your tail.');
    await expectAllPaths(page, 'path.studio-tail', tail.path_d);
    await expectAllPaths(page, 'path.studio-head', defaultHead);
    await expect(page.locator('#studio-closeup-path')).toHaveAttribute('d', tail.path_d);
    await expect(page.getByTestId('studio-pass')).toBeVisible();

    // With both slots filled, switching shows the other one and offers no move that
    // would replace it.
    const head = await upload(page, 'head.png', 'head');
    await page.locator('input[name="studio-kind"][value="tail"]').check();
    await expect(status).toHaveText('Showing your tail. It passes every check.');
    await expect(relabel).toBeHidden();
    await page.locator('input[name="studio-kind"][value="head"]').check();
    await expect(status).toHaveText('Showing your head. It passes every check.');
    await expect(relabel).toHaveText('Use it as a tail instead (replaces your tail)');
    await expectAllPaths(page, 'path.studio-head', head.path_d);
    await expectAllPaths(page, 'path.studio-tail', tail.path_d);
  });

  test("the instant format check gives the server's own advice", async ({ page }) => {
    await openStudio(page);
    const status = page.locator('#studio-status');
    const posted: string[] = [];
    page.on('request', (r) => { if (r.url().includes(ENDPOINT)) posted.push(r.url()); });
    await page.locator('#studio-file').setInputFiles({
      name: 'head.svgz', mimeType: 'image/svg+xml', buffer: Buffer.from([0x1f, 0x8b, 0x08, 0, 0, 0, 0, 0]),
    });
    await expect(status).toContainText('Inkscape: Save As → Plain SVG');
    expect(posted, 'answered in the browser').toEqual([]);

    // A video shares HEIC's container but isn't a photo: no HEIC advice; the server decides.
    const video = Buffer.concat([Buffer.from([0, 0, 0, 0x18]), Buffer.from('ftypisom\0\0\0\0isommp41', 'latin1')]);
    const response = await answerTo(page, () =>
      page.locator('#studio-file').setInputFiles({ name: 'clip.mp4', mimeType: 'video/mp4', buffer: video }));
    expect(response.status()).toBe(422);
    expect((await response.json()).error.code).toBe('unknown_format');
    await expect(status).toHaveText("We couldn't tell what kind of file this is. Upload a PNG, JPEG or SVG.");
  });
});

test.describe('Head & Tail Studio: details', () => {
  test('Play/Pause is a plain button named for what it does next', async ({ page }) => {
    await openStudio(page);
    const play = page.locator('#studio-play');
    await expect(play).not.toHaveAttribute('aria-pressed');
    await expect(play).toHaveAccessibleName('Pause the live preview');
    await play.click();
    await expect(play).toHaveText('Play');
    await expect(play).toHaveAccessibleName('Play the live preview');
    await expect(play).not.toHaveAttribute('aria-pressed');
  });

  test('a resize from narrow to wide (Split View, rotation) keeps a view selected', async ({ page }) => {
    await page.setViewportSize({ width: 375, height: 812 });
    await openStudio(page);
    await page.locator('input[name="studio-view"][value="live"]').check();
    await expect(page.locator('#studio-panes')).toHaveAttribute('data-view', 'live');
    // At 640px+ Live sits beside Close-up and has no option of its own.
    await page.setViewportSize({ width: 1180, height: 820 });
    await expect(page.locator('input[name="studio-view"][value="closeup"]')).toBeChecked();
    await expect(page.locator('#studio-panes')).toHaveAttribute('data-view', 'closeup');
  });

  test('the colour swatches line up, the custom one included', async ({ page }) => {
    await openStudio(page);
    const geometry = () => page.locator('.studio-swatch').evaluateAll((els) => els.map((el) => {
      const box = el.getBoundingClientRect();
      const name = el.querySelector('.studio-swatch-name')?.getBoundingClientRect();
      return { height: Math.round(box.height), nameOffset: Math.round((name?.top ?? 0) - box.top) };
    }));
    const swatches = await geometry();
    expect(swatches.length).toBe(10);
    expect(new Set(swatches.map((s) => s.height)).size, JSON.stringify(swatches)).toBe(1);
    expect(new Set(swatches.map((s) => s.nameOffset)).size, JSON.stringify(swatches)).toBe(1);

    const custom = page.locator('#studio-swatch-custom');
    await expect(custom).not.toHaveClass(/active/);
    await page.getByLabel('Custom colour').fill('#123456');
    await expect(custom).toHaveClass(/active/);
    expect(await geometry()).toEqual(swatches);
  });

  for (const [name, size, target] of [
    ['iPad landscape', { width: 1180, height: 820 }, '#studio-closeup'],
    ['phone', { width: 375, height: 812 }, '#studio-status'],
  ] as const) {
    test(`after an upload the result is in view (${name})`, async ({ page }) => {
      await page.setViewportSize(size);
      await openStudio(page);
      const before = (await page.locator('#studio-drop').boundingBox())?.height ?? 0;
      await upload(page, 'round-blob.png');
      await expect(page.locator('#studio-drop-title')).toHaveText('Choose another drawing');
      const after = (await page.locator('#studio-drop').boundingBox())?.height ?? 0;
      expect(after, 'the drop zone shrinks to a row').toBeLessThan(Math.min(before, 80));
      // Scrolled so the target's top sits in the top half of the screen.
      await expect.poll(() => page.locator(target).evaluate((el) => {
        const top = el.getBoundingClientRect().top;
        return top >= 0 && top < window.innerHeight / 2;
      })).toBe(true);
      await expect(page.locator('#studio-result-heading')).toBeFocused();
      await expect(page.locator('#studio-closeup')).toBeInViewport({ ratio: name === 'phone' ? 0.2 : 0.5 });
    });
  }

  // "Upload a new version" is at the bottom of the page and the status line at the top:
  // what happens to the new file must come into view, or the last result (and its green
  // pass) reads as the new file's.
  for (const [name, size] of [
    ['iPad portrait', { width: 820, height: 1180 }],
    ['phone', { width: 375, height: 812 }],
  ] as const) {
    test(`a new version's progress and errors come into view (${name})`, async ({ page }) => {
      await page.emulateMedia({ reducedMotion: 'reduce' }); // instant scrolls, so positions settle
      await page.setViewportSize(size);
      await openStudio(page);
      await upload(page, 'head.png');
      const status = page.locator('#studio-status');
      const newVersion = page.locator('#studio-new-version');
      const pick = async (file: string | { name: string; mimeType: string; buffer: Buffer }) => {
        await newVersion.scrollIntoViewIfNeeded();
        await expect(status, 'the status line is off screen from the button').not.toBeInViewport();
        const [chooser] = await Promise.all([page.waitForEvent('filechooser'), newVersion.click()]);
        await chooser.setFiles(file);
      };

      // Turned down by the browser's own check.
      await pick(fixture('template.psd'));
      await expect(status).toContainText("That's a PSD");
      await expect(status).toHaveClass(/error/);
      await expect(status).toBeInViewport();

      // A slow upload says it's processing, and the server's answer comes into view
      // even after scrolling back down.
      let release = () => {};
      const held = new Promise<void>((resolve) => { release = resolve; });
      await page.route((url) => url.pathname === ENDPOINT, async (route) => { await held; await route.continue(); });
      await pick({ name: 'notes.png', mimeType: 'image/png', buffer: Buffer.from('just some text') });
      await expect(status).toHaveText('Processing your drawing…');
      await expect(status).toBeInViewport();
      await newVersion.scrollIntoViewIfNeeded();
      await expect(status).not.toBeInViewport();
      const answer = page.waitForResponse(isProcess);
      release();
      const rejected = await answer;
      expect([422, ...CAPACITY], await rejected.text()).toContain(rejected.status());
      if (rejected.status() === 422) {
        await expect(status).toHaveText("We couldn't tell what kind of file this is. Upload a PNG, JPEG or SVG.");
      }
      await expect(status).toHaveClass(/error/);
      await expect(status).toBeInViewport();
      // The last result stays.
      await expect(page.getByTestId('studio-pass')).toBeVisible();
    });
  }
});

test.describe('Head & Tail Studio: start here', () => {
  const isOpen = (start: Locator) => start.evaluate((el) => (el as HTMLDetailsElement).open);

  test('"Start here" is open before the first upload and one tap away after', async ({ page }) => {
    await openStudio(page);
    const start = page.getByTestId('studio-start');
    const summary = start.locator('summary');
    const example = page.locator('#studio-example');
    expect(await isOpen(start)).toBe(true);
    for (const step of ['Get a template', 'Draw', 'Upload here']) {
      await expect(start.getByRole('heading', { level: 3, name: step })).toBeVisible();
    }
    await expect(start.getByRole('link', { name: 'Read the guide' })).toHaveAttribute('href', GUIDE);
    await expect(example).toBeVisible();
    // Before the upload panel, so it's the first thing on the page.
    expect(await start.evaluate((el) =>
      !!(el.compareDocumentPosition(document.querySelector('.studio-upload') as Node) & Node.DOCUMENT_POSITION_FOLLOWING))).toBe(true);

    await upload(page, 'head.png');
    await expect.poll(() => isOpen(start), 'closed after the first upload').toBe(false);
    await expect(summary).toBeVisible();
    await expect(example).toBeHidden();
    await summary.click();
    expect(await isOpen(start)).toBe(true);
    await expect(example).toBeVisible();
    // It stays however the artist left it: another upload doesn't close it.
    await upload(page, 'mirrored-head.png');
    expect(await isOpen(start)).toBe(true);
    await summary.click();
    expect(await isOpen(start)).toBe(false);

    // A restored preview counts as an upload; Clear opens it again.
    await page.reload();
    await expect(page.locator('#studio-status')).toHaveText('Welcome back: your last preview is restored.');
    expect(await isOpen(start)).toBe(false);
    await page.locator('#studio-clear').click();
    await expect.poll(() => isOpen(start), 'open again after Clear').toBe(true);
    await expect(example).toBeVisible();
  });

  test('every template link has a download attribute and serves the committed file', async ({ page, request }) => {
    await openStudio(page);
    const start = page.getByTestId('studio-start');
    await expectKitDownloads(request, start, TEMPLATES);
    for (const kind of ['head', 'tail']) {
      await expect(start.getByRole('link', { name: `Procreate (PSD), ${kind} template`, exact: true })).toBeVisible();
      await expect(start.getByRole('link', { name: `Illustrator · Inkscape · Affinity (SVG), ${kind} template`, exact: true })).toBeVisible();
    }
    // Tapping one saves the file and leaves the studio where it was.
    const psd = start.getByRole('link', { name: 'Procreate (PSD), head template', exact: true });
    const [download] = await Promise.all([page.waitForEvent('download'), psd.click()]);
    expect(download.suggestedFilename()).toBe('battlesnake-head-template.psd');
    expect((await readFile(await download.path())).equals(await kitFile('battlesnake-head-template.psd'))).toBe(true);
    expect(new URL(page.url()).pathname).toBe(STUDIO);
    await expect(page.locator('#studio-status')).toHaveText('Try it with your own drawing.');
  });

  test('"Try an example" runs the example drawing through the endpoint into the head slot', async ({ page }) => {
    await openStudio(page);
    const defaultTail = await refPath(page, '#studio-pair-tail', 'default');
    // Even with Tail picked: the example is a head.
    await page.locator('input[name="studio-kind"][value="tail"]').check();
    const fetched = page.waitForResponse((r) => r.url().includes('/static/design-kit/example-drawing.png'));
    const response = await answerTo(page, () => page.locator('#studio-example').click());
    const asset = await fetched;
    expect(asset.status()).toBe(200);
    expect(asset.url(), 'a versioned asset URL').toMatch(/example-drawing\.png\?v=[0-9a-f]{16}$/);
    expect(response.status(), await response.text()).toBe(200);
    expect(response.request().postDataBuffer()?.equals(await kitFile('example-drawing.png')), 'posts the committed file').toBe(true);

    const result = (await response.json()) as Processed;
    expect(result.input).toBe('png');
    expect(result.lints.head, 'the example passes every head check').toEqual([]);
    await expect(page.locator('#studio-status')).toHaveText('Done: your head is on the board. It passes every check.');
    await expect(page.locator('input[name="studio-kind"][value="head"]')).toBeChecked();
    await expectAllPaths(page, 'path.studio-head', result.path_d, result.fill_rule);
    await expectAllPaths(page, 'path.studio-tail', defaultTail);
    await expect(page.locator('#studio-closeup-path')).toHaveAttribute('d', result.path_d);
    await expect(page.getByTestId('studio-pass')).toBeVisible();
    await expect(page.locator('#studio-warnings li')).toHaveCount(0);
    await expect(page.getByTestId('studio-top-warning')).toBeHidden();
    await expect(page.locator('#studio-result-heading')).toBeFocused();
    await expect(page.locator('#studio-download-head')).toBeVisible();
    expect(await isOpen(page.getByTestId('studio-start'))).toBe(false);
  });

  test('"Learn more" on each check opens its section of the guide', async ({ page }) => {
    await openStudio(page);
    const anchors = new Map<string, string>();
    for (const name of ['mirrored-head.png', 'round-blob.png']) {
      await upload(page, name);
      const checks = page.locator('#studio-warnings li.studio-lint');
      expect(await checks.count(), name).toBeGreaterThan(0);
      for (const check of await checks.all()) {
        const code = (await check.getAttribute('data-code')) as string;
        const learn = check.locator('a.studio-learn');
        await expect(learn, `${code} links to the guide`).toHaveCount(1);
        await expect(learn).toHaveText(/^Learn more/);
        await expect(learn).toHaveAccessibleName(/^Learn more about /);
        const href = (await learn.getAttribute('href')) as string;
        expect(href, code).toMatch(new RegExp(`^${GUIDE}#[a-z-]+$`));
        anchors.set(code, href.split('#')[1]);
        // The icon sits beside the message's first line, and "Learn more" lines up
        // under the message.
        const layout = await check.evaluate((li) => {
          const text = (li.querySelector('.studio-lint-text') as Element).getBoundingClientRect();
          const icon = (li.querySelector('.studio-lint-icon') as Element).getBoundingClientRect();
          const words = document.createRange();
          words.selectNodeContents((li.querySelector('a.studio-learn') as Element).firstChild as Node);
          return { textLeft: text.left, textTop: text.top, iconTop: icon.top, iconRight: icon.right, learnLeft: words.getBoundingClientRect().left };
        });
        expect(Math.abs(layout.learnLeft - layout.textLeft), `${code}: ${JSON.stringify(layout)}`).toBeLessThanOrEqual(1);
        expect(Math.abs(layout.iconTop - layout.textTop), `${code}: ${JSON.stringify(layout)}`).toBeLessThanOrEqual(6);
        expect(layout.iconRight, `${code}: ${JSON.stringify(layout)}`).toBeLessThanOrEqual(layout.textLeft);
      }
    }
    expect(anchors.get('faces_left')).toBe('direction');
    expect(anchors.get('neck_gap')).toBe('neck');

    // The guide has every one of those sections.
    await page.goto(GUIDE);
    for (const [code, anchor] of anchors) {
      await expect(page.locator(`.guide [id="${anchor}"]`), `${code} -> #${anchor}`).toHaveCount(1);
    }

    // The top warning's link goes to the first warning's section.
    await page.goto(STUDIO);
    await expect(page.locator('#studio-status')).toHaveText('Welcome back: your last preview is restored.');
    const firstHref = await page.locator('#studio-warnings li.studio-lint.warn a.studio-learn').first().getAttribute('href');
    const top = page.locator('#studio-top-learn');
    await expect(top).toBeVisible();
    await expect(top).toHaveAttribute('href', firstHref as string);
    const topLayout = await page.getByTestId('studio-top-warning').evaluate((box) => {
      const words = document.createRange();
      words.selectNodeContents((box.querySelector('#studio-top-learn') as Element).firstChild as Node);
      return {
        textLeft: (box.querySelector('#studio-top-warning-text') as Element).getBoundingClientRect().left,
        learnLeft: words.getBoundingClientRect().left,
      };
    });
    expect(Math.abs(topLayout.learnLeft - topLayout.textLeft), JSON.stringify(topLayout)).toBeLessThanOrEqual(1);
    await top.click();
    await expect(page).toHaveURL(new RegExp(`${GUIDE}#[a-z-]+$`));
    const section = page.locator(`[id="${(firstHref as string).split('#')[1]}"]`);
    await expect(section).toBeInViewport();
  });
});

test.describe('Head & Tail Studio layout', () => {
  for (const width of [320, 375, 820, 1180]) {
    test(`"Start here" has no overflow and 44px controls at ${width}px`, async ({ page }) => {
      await page.setViewportSize({ width, height: 900 });
      await openStudio(page);
      await page.evaluate(() => document.fonts.ready);
      expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(width);

      const controls = page.locator('#studio-start :is(a[href], button, summary)');
      // Summary, 4 templates, the guide, the example.
      expect(await controls.count()).toBe(7);
      for (const control of await controls.all()) {
        await expect(control).toBeVisible();
        const box = await control.boundingBox();
        const name = (await control.getAttribute('aria-label')) || (await control.innerText()).trim().slice(0, 40);
        expect(box?.width, `${name} width at ${width}px`).toBeGreaterThanOrEqual(44);
        expect(box?.height, `${name} height at ${width}px`).toBeGreaterThanOrEqual(44);
        expect((box?.x ?? 0) + (box?.width ?? 0), `${name} inside the screen`).toBeLessThanOrEqual(width);
      }
      // The short labels stay on one line.
      for (const label of ['Read the guide', 'Try an example', 'Procreate (PSD)']) {
        const box = await page.locator('#studio-start').getByText(label, { exact: true }).first().boundingBox();
        expect(box?.height, `"${label}" wraps at ${width}px`).toBeLessThan(56);
      }
    });
  }


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
      // The longest button: the offer to move the upload into the empty tail slot.
      await page.locator('input[name="studio-kind"][value="tail"]').check();
      const relabel = page.locator('#studio-relabel');
      await expect(relabel).toHaveText('Use the file you just uploaded as your tail');
      expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(width);
      const box = await relabel.boundingBox();
      expect(box?.height ?? 0).toBeGreaterThanOrEqual(44);
      expect((box?.x ?? 0) + (box?.width ?? 0)).toBeLessThanOrEqual(width);
      for (const input of await page.locator('#studio :is(input, select, textarea)').all()) {
        const size = await input.evaluate((el) => parseFloat(getComputedStyle(el).fontSize));
        expect(size, `font size of ${await input.evaluate((el) => el.outerHTML.slice(0, 80))}`).toBeGreaterThanOrEqual(16);
      }
    });
  }
});
