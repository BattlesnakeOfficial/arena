import { readFile } from 'node:fs/promises';
import path from 'node:path';
import { test, expect, type Locator, type Page, type Response } from '@playwright/test';
import { TEMPLATES, expectKitDownloads, kitFile } from '../fixtures/design-kit';

// Head & Tail Studio (/customizations/studio). Runs logged out (no login is needed),
// on chromium and on the WebKit iPad project.
//
// "Your snake" has two slots, a head card and a tail card. Each slot wears the artist's
// upload or a catalog style, and every board wears both. Per-slot ids are
// `studio-<part>-<kind>`.

const STUDIO = '/customizations/studio';
const GUIDE = '/customizations/studio/guide';
const ENDPOINT = '/customizations/studio/process';
const FIXTURES = path.join(__dirname, '..', 'fixtures', 'studio');
const fixture = (name: string) => path.join(FIXTURES, name);
const STORE = 'arena:studio:v2';
const STORE_V1 = 'arena:studio:v1';

type Kind = 'head' | 'tail';
const KINDS: Kind[] = ['head', 'tail'];

/** The parts of a 200 from the processing endpoint the page uses. */
interface Processed {
  path_d: string;
  fill_rule: 'nonzero' | 'evenodd';
  input: string;
  lints: { head: { code: string; fix?: string | null }[]; tail: { code: string; fix?: string | null }[] };
}

// The endpoint has one processing slot and a global token bucket (20, refilling
// 1/s). Most tests here upload a head and a tail, so a run (two projects sharing one
// server locally) empties the bucket; those are capacity answers, not failures, so
// retry them, as the server's Retry-After says.
const CAPACITY = [429, 503];
const CAPACITY_RETRIES = 8;

// The upload tests share one server-wide bucket and slot: keep this file's tests in
// order on one worker.
// Capacity retries can add up, so allow more than the default 30s.
test.describe.configure({ mode: 'default', timeout: 90_000 });

let pageErrors: string[] = [];
test.beforeEach(async ({ page }) => {
  pageErrors = [];
  page.on('pageerror', (err) => pageErrors.push(err.message));
});
test.afterEach(() => {
  expect(pageErrors, 'uncaught errors on the page').toEqual([]);
});

/** A per-slot element: `#studio-<part>-<kind>`. */
const part = (page: Page, name: string, kind: Kind) => page.locator(`#studio-${name}-${kind}`);

/** studio.js is deferred; render() has run once the page carries the colour inline. */
const ready = (page: Page) => expect(page.getByTestId('studio')).toHaveAttribute('style', /--studio-snake/);

async function openStudio(page: Page) {
  const response = await page.goto(STUDIO);
  expect(response?.status()).toBe(200);
  await expect(page.getByRole('heading', { level: 1, name: 'Head & Tail Studio' })).toBeVisible();
  await ready(page);
}

const isProcess = (r: Response) => r.url().includes(ENDPOINT) && r.request().method() === 'POST';
/** For page.route/unroute: the processing endpoint. */
const isEndpoint = (url: URL) => url.pathname === ENDPOINT;

/** Do `act` and return the endpoint's answer to it, retrying capacity answers. */
async function answerTo(page: Page, act: () => Promise<unknown>, match = isProcess): Promise<Response> {
  for (let attempt = 0; ; attempt++) {
    const answer = page.waitForResponse(match);
    await act();
    const response = await answer;
    if (CAPACITY.includes(response.status()) && attempt < CAPACITY_RETRIES) {
      const wait = Number(response.headers()['retry-after'] ?? '2');
      await page.waitForTimeout(Math.min(Math.max(wait, 1), 10) * 1000);
      continue;
    }
    return response;
  }
}

/** Upload `name` with the `kind` card's button and return the endpoint's answer. */
async function upload(page: Page, name: string, kind: Kind = 'head'): Promise<Processed> {
  const response = await answerTo(page, () => part(page, 'file', kind).setInputFiles(fixture(name)));
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

/** Drag a fixture file over `target` and drop it there, as a DragEvent with the file. */
async function dropFile(page: Page, target: Locator, name: string, mimeType: string) {
  const bytes = (await readFile(fixture(name))).toString('base64');
  const dataTransfer = await page.evaluateHandle(({ bytes, name, mimeType }) => {
    const bin = Uint8Array.from(atob(bytes), (c) => c.charCodeAt(0));
    const dt = new DataTransfer();
    dt.items.add(new File([bin], name, { type: mimeType }));
    return dt;
  }, { bytes, name, mimeType });
  await target.dispatchEvent('dragenter', { dataTransfer });
  await expect(target, 'the card lights up under the file').toHaveClass(/\bdragging\b/);
  await target.dispatchEvent('dragover', { dataTransfer });
  await target.dispatchEvent('drop', { dataTransfer });
}

/** The SVG file the page builds from a saved path (downloads, and Flip/Fit after a reload). */
const pageSvg = (r: Processed) =>
  `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><path fill-rule="${r.fill_rule}" d="${r.path_d}"/></svg>`;

/** Every attribute value of `attr` on the elements `locator` matches. */
async function attrs(locator: Locator, attr: string): Promise<(string | null)[]> {
  return locator.evaluateAll((els, a) => els.map((el) => el.getAttribute(a)), attr);
}

/** Every board's `kind` placeholder (all views), and the slot's close-up and thumbnail, wear `d`. */
async function expectAllPaths(page: Page, kind: Kind, d: string, fillRule?: string) {
  const paths = page.locator(`path.studio-${kind}`);
  // Live loop (16 frames) + All directions (4 snakes) + Game size (2 boards x 4 snakes).
  expect(await paths.count()).toBe(28);
  expect(new Set(await attrs(paths, 'd')), `every ${kind} on every board`).toEqual(new Set([d]));
  if (fillRule) expect(new Set(await attrs(paths, 'fill-rule'))).toEqual(new Set([fillRule]));
  await expect(part(page, 'closeup-path', kind), `the ${kind} close-up`).toHaveAttribute('d', d);
  await expect(part(page, 'thumb-path', kind), `the ${kind} card's thumbnail`).toHaveAttribute('d', d);
}

/** The catalog path of `slug` in the `kind` card's style select. */
async function refPath(page: Page, kind: Kind, slug: string): Promise<string> {
  const d = await part(page, 'style', kind).locator(`option[value="${slug}"]`).getAttribute('data-d');
  expect(d, `the ${kind} styles have ${slug}`).toBeTruthy();
  return d as string;
}

/** What a card says it wears, and whether its upload's actions show. */
async function expectCard(page: Page, kind: Kind, name: string, own: boolean) {
  await expect(part(page, 'name', kind)).toHaveText(name);
  for (const action of ['download', 'remove']) {
    if (own) await expect(part(page, action, kind), `${kind} ${action}`).toBeVisible();
    else await expect(part(page, action, kind), `${kind} ${action}`).toBeHidden();
  }
  if (own) await expect(part(page, 'checks', kind)).toBeVisible();
  else await expect(part(page, 'checks', kind)).toBeHidden();
}

test.describe('Head & Tail Studio', () => {
  test('loads logged out with a default head card and tail card, both on every board', async ({ page }) => {
    await openStudio(page);
    expect(page.url()).toContain(STUDIO); // no redirect to a sign-in page
    expect(await page.locator('a[href="/auth/github"]').count(), 'signed out').toBeGreaterThan(0);
    await expect(page.locator('#studio-status')).toHaveText('Try it with your own drawing.');
    await expect(page.getByRole('heading', { level: 2, name: 'Your snake' })).toBeVisible();

    for (const kind of KINDS) {
      const card = page.getByTestId(`studio-slot-${kind}`);
      await expect(card).toBeVisible();
      await expect(card).toHaveAttribute('role', 'group');
      await expect(card).toHaveAccessibleName(kind === 'head' ? 'Head' : 'Tail');
      await expect(part(page, 'file', kind)).toHaveAttribute('accept', /\.png.*\.svg/);
      await expect(part(page, 'upload-text', kind)).toHaveText(`Upload ${kind}`);
      // The upload button is the file input, named by its label.
      await expect(part(page, 'file', kind)).toHaveAccessibleName(`Upload ${kind}`);
      const style = part(page, 'style', kind);
      await expect(style).toHaveValue('default');
      await expect(style).toHaveAccessibleName(`${kind === 'head' ? 'Head' : 'Tail'} style`);
      await expect(style.locator('option[value="user"]'), 'no "Your …" before an upload').toHaveCount(0);
      await expectCard(page, kind, `Default ${kind}`, false);
      await expect(part(page, 'relabel', kind)).toBeHidden();
      await expect(part(page, 'confirm', kind)).toBeHidden();
      await expectAllPaths(page, kind, await refPath(page, kind, 'default'));
      await expect(part(page, 'closeup', kind)).toHaveAttribute('aria-label', `Close-up of the default ${kind}`);
    }
    // Every board is a labelled image.
    for (const board of await page.locator('svg.studio-board[role="img"]').all()) {
      await expect(board).toHaveAttribute('aria-label', /default head and the default tail/);
    }
    // Nothing checked yet.
    await expect(page.getByTestId('studio-summary')).toBeHidden();
    await expect(page.locator('#studio-lints-empty')).toBeVisible();
    await expect(page.getByTestId('studio-top-warning')).toBeHidden();
  });

  for (const [name, input] of [['head.png', 'png'], ['head.svg', 'svg'], ['head.jpg', 'jpeg']] as const) {
    test(`a ${input.toUpperCase()} upload fills the head card and every head`, async ({ page }) => {
      await openStudio(page);
      const defaultTail = await refPath(page, 'tail', 'default');
      const result = await upload(page, name);
      expect(result.input).toBe(input);
      expect(result, 'the page builds the SVG file itself').not.toHaveProperty('svg');
      expect(result.lints.head, 'a catalog head passes every head check').toEqual([]);

      await expectAllPaths(page, 'head', result.path_d, result.fill_rule);
      await expectAllPaths(page, 'tail', defaultTail);
      await expectCard(page, 'head', 'Your head', true);
      await expectCard(page, 'tail', 'Default tail', false);
      await expect(part(page, 'upload-text', 'head')).toHaveText('Upload a new version');
      await expect(part(page, 'file', 'head')).toHaveAccessibleName('Upload a new version of your head');
      await expect(part(page, 'upload-text', 'tail')).toHaveText('Upload tail');
      await expect(page.locator('#studio-result-heading')).toBeFocused();
      await expect(page.getByTestId('studio-pass-head')).toBeVisible();
      await expect(page.getByTestId('studio-summary')).toHaveText('Head: passes');
      await expect(page.locator('#studio-lints-empty')).toBeHidden();
      await expect(page.locator('.studio-all')).toHaveAttribute('aria-label', /wearing your head and the default tail/);
    });
  }

  test('a head and a tail upload: every view wears both, and neither upload touches the other slot', async ({ page }) => {
    await openStudio(page);
    const defaultHead = await refPath(page, 'head', 'default');
    // The tail first: the head stays the default.
    const tail = await upload(page, 'tail.svg', 'tail');
    expect(tail.lints.tail, 'a catalog tail passes every tail check').toEqual([]);
    await expectAllPaths(page, 'tail', tail.path_d, tail.fill_rule);
    await expectAllPaths(page, 'head', defaultHead);
    await expectCard(page, 'head', 'Default head', false);

    // Then the head: the tail stays yours.
    const head = await upload(page, 'head.png', 'head');
    expect(head.path_d).not.toBe(tail.path_d);
    await expectAllPaths(page, 'head', head.path_d, head.fill_rule);
    await expectAllPaths(page, 'tail', tail.path_d, tail.fill_rule);
    await expectCard(page, 'head', 'Your head', true);
    await expectCard(page, 'tail', 'Your tail', true);
    await expect(part(page, 'style', 'tail')).toHaveValue('user');

    // In every view.
    for (const view of ['closeup', 'live', 'all', 'game']) {
      const option = page.locator(`input[name="studio-view"][value="${view}"]`);
      if (await option.isVisible()) await option.check(); // Live sits beside Close-up from 640px
      for (const [kind, d] of [['head', head.path_d], ['tail', tail.path_d]] as const) {
        const shown = page.locator(`.studio-pane:visible path.studio-${kind}, .studio-pane:visible #studio-closeup-path-${kind}`);
        expect(await shown.count(), `${kind} in the ${view} view`).toBeGreaterThan(0);
        expect(new Set(await attrs(shown, 'd')), `${kind} in the ${view} view`).toEqual(new Set([d]));
      }
    }
    await page.locator('input[name="studio-view"][value="closeup"]').check();
    for (const kind of KINDS) {
      await expect(part(page, 'closeup', kind)).toHaveAttribute('aria-label', `Close-up of your ${kind}`);
      await expect(part(page, 'closeup', kind), 'both close-ups show at once').toBeVisible();
    }
    for (const board of await page.locator('svg.studio-board[role="img"]').all()) {
      await expect(board).toHaveAttribute('aria-label', /wearing your head and your tail/);
    }
    // Checks for both, each with its own pass.
    await expect(page.getByTestId('studio-summary')).toHaveText('Head: passes · Tail: passes');
    await expect(page.getByTestId('studio-pass-head')).toBeVisible();
    await expect(page.getByTestId('studio-pass-tail')).toBeVisible();
    await expect(part(page, 'checks-title', 'head')).toHaveText('Head');
    await expect(part(page, 'checks-title', 'tail')).toHaveText('Tail');
    await expect(page.getByTestId('studio-pass-tail')).toContainText('the official tails pass');

    // A new version of the head replaces the head only.
    const again = await upload(page, 'mirrored-head.png', 'head');
    await expectAllPaths(page, 'head', again.path_d);
    await expectAllPaths(page, 'tail', tail.path_d);
    await expect(part(page, 'name', 'tail')).toHaveText('Your tail');
  });

  test('each card\'s style select lists "Your …" first, picks it after an upload, and swaps only its slot', async ({ page }) => {
    await openStudio(page);
    const head = await upload(page, 'head.png', 'head');
    const tail = await upload(page, 'tail.svg', 'tail');

    for (const [kind, mine, slug] of [['head', head, 'fang'], ['tail', tail, 'curled']] as const) {
      const otherKind: Kind = kind === 'head' ? 'tail' : 'head';
      const otherD = kind === 'head' ? tail.path_d : head.path_d;
      const select = part(page, 'style', kind);
      await expect(select).toHaveValue('user');
      const first = select.locator('option').first();
      await expect(first).toHaveAttribute('value', 'user');
      await expect(first).toHaveText(`Your ${kind}`);
      expect(await select.locator('option[value="user"]').count(), 'listed once').toBe(1);

      // A catalog style instead: this slot only. The upload stays in the list, and its
      // own buttons and checks go away while the card isn't showing it.
      await select.selectOption(slug);
      const ref = await refPath(page, kind, slug);
      await expectAllPaths(page, kind, ref);
      await expectAllPaths(page, otherKind, otherD);
      const label = (await select.locator(`option[value="${slug}"]`).textContent())?.trim();
      await expectCard(page, kind, `${label} ${kind}`, false);
      await expect(part(page, 'relabel', kind)).toBeHidden();
      await expect(select.locator('option[value="user"]')).toHaveCount(1);
      await expect(page.getByTestId('studio-summary')).toHaveText(`${otherKind === 'head' ? 'Head' : 'Tail'}: passes`);
      await expect(page.locator('.studio-all')).toHaveAttribute('aria-label', new RegExp(`the ${label} ${kind}`));

      // Back to yours.
      await select.selectOption('user');
      await expectAllPaths(page, kind, mine.path_d);
      await expectCard(page, kind, `Your ${kind}`, true);
      await expect(page.getByTestId('studio-summary')).toHaveText('Head: passes · Tail: passes');
    }

    // A new upload while a catalog style is showing: the card switches to it.
    await part(page, 'style', 'tail').selectOption('bolt');
    const again = await upload(page, 'tail.svg', 'tail');
    await expect(part(page, 'style', 'tail')).toHaveValue('user');
    await expectAllPaths(page, 'tail', again.path_d);
  });

  test('Remove puts one slot back to the catalog and leaves the other', async ({ page }) => {
    await openStudio(page);
    const status = page.locator('#studio-status');
    await upload(page, 'head.png', 'head');
    const tail = await upload(page, 'tail.svg', 'tail');

    await part(page, 'remove', 'head').click();
    await expect(status).toHaveText('Removed your head. The board shows the default head.');
    await expectAllPaths(page, 'head', await refPath(page, 'head', 'default'));
    await expectAllPaths(page, 'tail', tail.path_d);
    await expectCard(page, 'head', 'Default head', false);
    await expectCard(page, 'tail', 'Your tail', true);
    await expect(part(page, 'style', 'head')).toHaveValue('default');
    await expect(part(page, 'style', 'head').locator('option[value="user"]')).toHaveCount(0);
    await expect(part(page, 'upload-text', 'head')).toHaveText('Upload head');
    await expect(part(page, 'file', 'head'), 'focus goes to the emptied card').toBeFocused();
    await expect(page.getByTestId('studio-summary')).toHaveText('Tail: passes');

    // And the tail: the studio is back where it started.
    await part(page, 'remove', 'tail').click();
    await expect(status).toHaveText('Removed your tail. The board shows the default tail.');
    await expectAllPaths(page, 'tail', await refPath(page, 'tail', 'default'));
    await expectCard(page, 'tail', 'Default tail', false);
    await expect(page.getByTestId('studio-summary')).toBeHidden();
    await expect(page.locator('#studio-lints-empty')).toBeVisible();

    // Nothing comes back after a reload.
    await page.reload();
    await ready(page);
    await expect(status).toHaveText('Try it with your own drawing.');
    await expectAllPaths(page, 'head', await refPath(page, 'head', 'default'));
    await expectAllPaths(page, 'tail', await refPath(page, 'tail', 'default'));
  });

  test('a round blob warns about the neck, with brackets on the head close-up only', async ({ page }) => {
    await openStudio(page);
    const result = await upload(page, 'round-blob.png');
    expect(result.lints.head.map((l) => l.code)).toContain('neck_gap');

    const warning = page.locator('#studio-warnings-head li.studio-lint[data-code="neck_gap"]');
    await expect(warning).toBeVisible();
    await expect(warning).toContainText('The left edge is where the body joins');
    const top = page.getByTestId('studio-top-warning');
    await expect(top).toBeVisible();
    await expect(page.locator('#studio-top-warning-text')).toHaveText(/^Head: /);
    await expect(page.locator('#studio-status')).toContainText('to check below');
    await expect(page.getByTestId('studio-summary')).toHaveText(`Head: ${result.lints.head.length} things to check`);
    await expect(page.getByTestId('studio-pass-head')).toBeHidden();
    // Red brackets mark the gaps beside the head close-up's left edge.
    expect(await page.locator('#studio-gaps-head rect').count()).toBeGreaterThan(0);
    expect(await page.locator('#studio-gaps-tail rect').count()).toBe(0);
  });

  test('a mirrored head offers Flip, and Flip clears the warning', async ({ page }) => {
    await openStudio(page);
    const mirrored = await upload(page, 'mirrored-head.png');
    expect(mirrored.lints.head.map((l) => l.code)).toContain('faces_left');

    const warning = page.locator('#studio-warnings-head li.studio-lint[data-code="faces_left"]');
    await expect(warning).toBeVisible();
    const flip = warning.locator('.studio-fix[data-fix="flip"]');
    await expect(flip).toHaveText('Flip your head');
    await expect(flip).toHaveAttribute('data-kind', 'head');

    const flipped = await applyFix(page, flip, 'flip');
    expect(flipped.lints.head).toEqual([]);
    await expect(page.locator('#studio-status')).toContainText('It passes every check');
    await expect(page.locator('#studio-warnings-head li')).toHaveCount(0);
    await expect(page.getByTestId('studio-top-warning')).toBeHidden();
    await expect(page.getByTestId('studio-pass-head')).toBeVisible();
    expect(flipped.path_d).not.toBe(mirrored.path_d);
    await expectAllPaths(page, 'head', flipped.path_d);
  });

  test('checks are per slot: separate pass states, and each Flip fixes its own slot', async ({ page }) => {
    await openStudio(page);
    // The same mirrored drawing in both slots: a head facing left, a tail on backwards.
    const head = await upload(page, 'mirrored-head.png', 'head');
    const tail = await upload(page, 'mirrored-head.png', 'tail');
    expect(head.lints.head.map((l) => l.code)).toEqual(['faces_left']);
    expect(tail.lints.tail.map((l) => l.code)).toEqual(['tail_reversed']);

    const summary = page.getByTestId('studio-summary');
    const topText = page.locator('#studio-top-warning-text');
    const topFix = page.locator('#studio-top-fix');
    await expect(summary).toHaveText('Head: 1 thing to check · Tail: 1 thing to check');
    for (const kind of KINDS) {
      const code = kind === 'head' ? 'faces_left' : 'tail_reversed';
      await expect(page.getByTestId(`studio-pass-${kind}`)).toBeHidden();
      const items = page.locator(`#studio-warnings-${kind} li.studio-lint`);
      await expect(items).toHaveCount(1);
      await expect(items.first()).toHaveAttribute('data-code', code);
      const fix = items.first().locator('.studio-fix');
      await expect(fix).toHaveAttribute('data-kind', kind);
      await expect(fix).toHaveAccessibleName(`Flip your ${kind}`);
    }
    // The top warning is the head's, with the head's Flip.
    await expect(topText).toHaveText(/^Head: /);
    await expect(topFix).toHaveAttribute('data-kind', 'head');
    await expect(topFix).toHaveAccessibleName('Flip your head');

    // The tail's own Flip: only the tail changes.
    const tailFlip = page.locator('#studio-warnings-tail .studio-fix[data-fix="flip"]');
    const tailFlipped = await applyFix(page, tailFlip, 'flip');
    expect(tailFlipped.lints.tail).toEqual([]);
    await expect(page.locator('#studio-status')).toHaveText('Done: your tail is on the board. It passes every check.');
    await expectAllPaths(page, 'tail', tailFlipped.path_d);
    await expectAllPaths(page, 'head', head.path_d);
    await expect(page.getByTestId('studio-pass-tail')).toBeVisible();
    await expect(page.getByTestId('studio-pass-head')).toBeHidden();
    await expect(page.locator('#studio-warnings-head li.studio-lint')).toHaveCount(1);
    await expect(summary).toHaveText('Head: 1 thing to check · Tail: passes');
    await expect(topText).toHaveText(/^Head: /);

    // The top Flip is the head's: the head is fixed, the tail stays as it is.
    const headFlipped = await applyFix(page, topFix, 'flip');
    expect(headFlipped.lints.head).toEqual([]);
    await expectAllPaths(page, 'head', headFlipped.path_d);
    await expectAllPaths(page, 'tail', tailFlipped.path_d);
    await expect(summary).toHaveText('Head: passes · Tail: passes');
    await expect(page.getByTestId('studio-top-warning')).toBeHidden();
    await expect(page.getByTestId('studio-pass-head')).toBeVisible();
  });

  test('the top warning acts on the tail when only the tail has one', async ({ page }) => {
    await openStudio(page);
    const head = await upload(page, 'head.png', 'head');
    const tail = await upload(page, 'mirrored-head.png', 'tail');
    await expect(page.getByTestId('studio-summary')).toHaveText('Head: passes · Tail: 1 thing to check');
    await expect(page.locator('#studio-top-warning-text')).toHaveText(/^Tail: /);
    const topFix = page.locator('#studio-top-fix');
    await expect(topFix).toHaveAttribute('data-kind', 'tail');
    const flipped = await applyFix(page, topFix, 'flip');
    expect(flipped.path_d).not.toBe(tail.path_d);
    await expectAllPaths(page, 'tail', flipped.path_d);
    await expectAllPaths(page, 'head', head.path_d);
    await expect(page.getByTestId('studio-top-warning')).toBeHidden();
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
    for (const kind of KINDS) {
      await expect(part(page, 'closeup-path', kind)).toHaveCSS('fill', 'rgb(58, 134, 255)');
      await expect(part(page, 'thumb-path', kind)).toHaveCSS('fill', 'rgb(58, 134, 255)');
    }
    await expect(page.locator('.studio-all')).toHaveAttribute('aria-label', /^Four blue snakes/);

    // A preset with a hint shows it.
    await page.getByRole('radio', { name: 'Charcoal #3d3d3d' }).check();
    await expect(page.locator('#studio-color-hint')).toContainText('#393939');

    // The custom picker.
    await page.getByLabel('Custom colour').fill('#123456');
    await expect(head).toHaveCSS('fill', 'rgb(18, 52, 86)');
    await expect(page.locator('#studio-color-hint')).toBeHidden();
  });

  test('the All directions board turns your head and your tail all four ways', async ({ page }) => {
    await openStudio(page);
    const head = await upload(page, 'head.png', 'head');
    const tail = await upload(page, 'tail.svg', 'tail');
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
      await expect(g.locator('path.studio-head')).toHaveAttribute('d', head.path_d);
    }
    const tails = page.locator('.studio-all path.studio-tail');
    await expect(tails).toHaveCount(4);
    expect(new Set(await attrs(tails, 'd'))).toEqual(new Set([tail.path_d]));
  });

  test('each card downloads its own slot as an SVG file', async ({ page }) => {
    await openStudio(page);
    const head = await upload(page, 'head.png', 'head');
    const tail = await upload(page, 'tail.svg', 'tail');
    for (const [kind, result] of [['head', head], ['tail', tail]] as const) {
      const button = part(page, 'download', kind);
      await expect(button).toHaveAccessibleName(`Download SVG of your ${kind}`);
      const [download] = await Promise.all([page.waitForEvent('download'), button.click()]);
      expect(download.suggestedFilename()).toBe(`my-battlesnake-${kind}.svg`);
      expect(await readFile(await download.path(), 'utf8'), kind).toBe(pageSvg(result));
    }
  });

  test('Save preview image downloads a PNG of both slots when sharing files is unavailable', async ({ page }) => {
    // Force the fallback: no Web Share API. And keep the SVG the image is drawn from.
    await page.addInitScript(() => {
      // @ts-expect-error removing the API on purpose
      delete Navigator.prototype.canShare;
      // @ts-expect-error removing the API on purpose
      delete Navigator.prototype.share;
      const serialize = XMLSerializer.prototype.serializeToString;
      XMLSerializer.prototype.serializeToString = function (node: Node) {
        const out = serialize.call(this, node);
        ((window as unknown as { studioExports: string[] }).studioExports ??= []).push(out);
        return out;
      };
    });
    await openStudio(page);
    const head = await upload(page, 'head.png', 'head');
    const tail = await upload(page, 'tail.svg', 'tail');
    await page.locator('input[name="studio-theme"][value="dark"]').check();
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

    // The image is the composed snake: four of your heads and four of your tails, on the
    // dark board.
    const svg = await page.evaluate(() => {
      const all = (window as unknown as { studioExports?: string[] }).studioExports ?? [];
      return all[all.length - 1] ?? '';
    });
    const count = (needle: string) => svg.split(needle).length - 1;
    expect(count(`d="${head.path_d}"`), 'your head, four times').toBe(4);
    expect(count(`d="${tail.path_d}"`), 'your tail, four times').toBe(4);
    expect(svg).toContain('fill="#0f0b19"');
  });

  test('both slots and your settings come back after a reload', async ({ page }) => {
    await openStudio(page);
    const head = await upload(page, 'head.png', 'head');
    const tail = await upload(page, 'mirrored-head.png', 'tail');
    await page.getByRole('radio', { name: 'Green #2bb673' }).check();
    await page.locator('input[name="studio-theme"][value="dark"]').check();
    await page.locator('input[name="studio-view"][value="all"]').check();

    const saved = await page.evaluate((key) => JSON.parse(localStorage.getItem(key) || 'null'), STORE);
    expect(saved).toMatchObject({ v: 2, pick: { head: 'user', tail: 'user' }, color: '#2bb673', theme: 'dark', view: 'all' });
    expect(saved.slots.head.d).toBe(head.path_d);
    expect(saved.slots.tail.d).toBe(tail.path_d);

    await page.reload();
    await expect(page.locator('#studio-status')).toHaveText('Welcome back: your last preview is restored.');
    await expectAllPaths(page, 'head', head.path_d);
    await expectAllPaths(page, 'tail', tail.path_d);
    await expectCard(page, 'head', 'Your head', true);
    await expectCard(page, 'tail', 'Your tail', true);
    await expect(page.getByTestId('studio-summary')).toHaveText('Head: passes · Tail: 1 thing to check');
    await expect(page.locator('#studio-warnings-tail li[data-code="tail_reversed"]')).toBeVisible();
    await expect(page.getByRole('radio', { name: 'Green #2bb673' })).toBeChecked();
    await expect(page.locator('.studio-all svg.head').first()).toHaveCSS('fill', 'rgb(43, 182, 115)');
    await expect(page.locator('input[name="studio-theme"][value="dark"]')).toBeChecked();
    for (const board of await page.locator('svg.studio-board').all()) {
      await expect(board).toHaveClass(/\bdark\b/);
    }
    await expect(page.locator('#studio-panes')).toHaveAttribute('data-view', 'all');

    // A catalog style over a kept upload comes back too, with the upload still listed.
    await part(page, 'style', 'tail').selectOption('curled');
    await page.reload();
    await expect(page.locator('#studio-status')).toHaveText('Welcome back: your last preview is restored.');
    await expect(part(page, 'style', 'tail')).toHaveValue('curled');
    await expect(part(page, 'style', 'tail').locator('option[value="user"]')).toHaveText('Your tail');
    await expectAllPaths(page, 'tail', await refPath(page, 'tail', 'curled'));
    await expectAllPaths(page, 'head', head.path_d);
    await part(page, 'style', 'tail').selectOption('user');
    await expectAllPaths(page, 'tail', tail.path_d);
  });

  test('a saved preview from before the two cards (v1) is moved over once', async ({ page }) => {
    await openStudio(page);
    const fang = await refPath(page, 'head', 'fang');
    const curled = await refPath(page, 'tail', 'curled');
    const bolt = await refPath(page, 'tail', 'bolt');
    const lint = { code: 'faces_left', severity: 'warn', message: 'Your head faces left.', fix: 'flip', guide: '#direction' };
    const slot = (d: string, lints = {}) => ({ d, fillRule: 'nonzero', lints: { head: [], tail: [], ...lints }, info: [], gaps: [], example: false });

    // An uploaded head, worked on with "Pair your head with: Curled".
    await page.evaluate(([key, value]) => {
      localStorage.removeItem('arena:studio:v2');
      localStorage.setItem(key, value);
    }, [STORE_V1, JSON.stringify({
      v: 1, kind: 'head', color: '#3a86ff', theme: 'dark', view: 'all',
      slots: { head: slot(fang, { head: [lint] }), tail: null },
      pair: { head: 'default', tail: 'curled' },
    })]);
    await page.reload();
    await expect(page.locator('#studio-status')).toHaveText('Welcome back: your last preview is restored.');
    await expectAllPaths(page, 'head', fang);
    await expectAllPaths(page, 'tail', curled);
    await expectCard(page, 'head', 'Your head', true);
    await expect(part(page, 'style', 'tail')).toHaveValue('curled');
    await expect(page.locator('#studio-warnings-head li[data-code="faces_left"]')).toBeVisible();
    await expect(page.getByRole('radio', { name: 'Blue #3a86ff' })).toBeChecked();
    await expect(page.locator('#studio-panes')).toHaveAttribute('data-view', 'all');
    // Saved in the new format, and the old one is gone.
    const after = await page.evaluate(([v2, v1]) => [localStorage.getItem(v2), localStorage.getItem(v1)], [STORE, STORE_V1]);
    expect(after[1]).toBeNull();
    expect(JSON.parse(after[0] as string)).toMatchObject({ v: 2, pick: { head: 'user', tail: 'curled' }, color: '#3a86ff' });

    // Both slots filled in v1: both cards wear their uploads, whatever was paired.
    await page.evaluate(([key, value]) => {
      localStorage.removeItem('arena:studio:v2');
      localStorage.setItem(key, value);
    }, [STORE_V1, JSON.stringify({
      v: 1, kind: 'tail', color: '#ff4f86', theme: 'light', view: 'closeup',
      slots: { head: slot(fang), tail: slot(bolt) },
      pair: { head: 'smile', tail: 'curled' },
    })]);
    await page.reload();
    await expect(page.locator('#studio-status')).toHaveText('Welcome back: your last preview is restored.');
    await expectAllPaths(page, 'head', fang);
    await expectAllPaths(page, 'tail', bolt);
    await expectCard(page, 'head', 'Your head', true);
    await expectCard(page, 'tail', 'Your tail', true);

    // A v2 save wins over a stale v1 one.
    await page.evaluate(([key, value]) => localStorage.setItem(key, value), [STORE_V1, JSON.stringify({
      v: 1, kind: 'head', slots: { head: slot(curled), tail: null }, pair: { head: 'default', tail: 'default' },
    })]);
    await page.reload();
    await ready(page);
    await expectAllPaths(page, 'head', fang);
    expect(await page.evaluate((k) => localStorage.getItem(k), STORE_V1)).toBeNull();
  });

  for (const [version, key] of [['v2', STORE], ['v1', STORE_V1]] as const) {
    test(`tampered saved state (${version}) is ignored`, async ({ page }) => {
      await openStudio(page);
      const defaultHead = await refPath(page, 'head', 'default');
      const defaultTail = await refPath(page, 'tail', 'default');
      const bad = {
        view: 'nope', theme: 'purple', color: 'red;background:url(x)',
        slots: {
          head: { d: 'M0 0"/><script>alert(1)</script>', fillRule: 'nonzero' },
          tail: { d: 'M0 0L1 1Z', fillRule: 'url(#x)' },
        },
      };
      const value = version === 'v2'
        ? { v: 2, ...bad, pick: { head: 'user', tail: '../../etc' } }
        : { v: 1, kind: 'head', ...bad, pair: { head: 'user', tail: '../../etc' } };
      await page.evaluate(([k, v]) => {
        localStorage.clear();
        localStorage.setItem(k, v);
      }, [key, JSON.stringify(value)]);
      await page.reload();
      await ready(page);
      await expect(page.locator('#studio-status')).toHaveText('Try it with your own drawing.');
      await expectAllPaths(page, 'head', defaultHead);
      await expectAllPaths(page, 'tail', defaultTail);
      for (const kind of KINDS) {
        await expect(part(page, 'style', kind)).toHaveValue('default');
        await expectCard(page, kind, `Default ${kind}`, false);
      }
      expect(await page.getByTestId('studio').evaluate((el) => el.style.getPropertyValue('--studio-snake'))).toBe('#ff4f86');
    });
  }

  test('a PSD gets a friendly "export a PNG" message', async ({ page, request }) => {
    await openStudio(page);
    const posted: string[] = [];
    page.on('request', (r) => { if (r.url().includes(ENDPOINT)) posted.push(r.url()); });
    const status = page.locator('#studio-status');
    for (const kind of KINDS) {
      await page.evaluate(() => { const s = document.getElementById('studio-status'); if (s) s.textContent = ''; });
      await part(page, 'file', kind).setInputFiles(fixture('template.psd'));
      await expect(status).toContainText("That's a PSD");
      await expect(status).toContainText('export a PNG');
      await expect(status).toHaveClass(/error/);
      await expect(part(page, 'download', kind)).toBeHidden();
    }
    expect(posted, 'the browser answers without uploading').toEqual([]);

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

test.describe('Head & Tail Studio: drop and move', () => {
  test('a file dropped on a card fills that slot only', async ({ page }) => {
    await openStudio(page);
    const defaultHead = await refPath(page, 'head', 'default');
    const tailCard = page.getByTestId('studio-slot-tail');
    const headCard = page.getByTestId('studio-slot-head');

    // Dragging over and away again leaves the card as it was.
    await headCard.dispatchEvent('dragenter');
    await expect(headCard).toHaveClass(/\bdragging\b/);
    await headCard.dispatchEvent('dragleave');
    await expect(headCard).not.toHaveClass(/\bdragging\b/);

    const tailResponse = await answerTo(page, () => dropFile(page, tailCard, 'tail.svg', 'image/svg+xml'));
    expect(tailResponse.status(), await tailResponse.text()).toBe(200);
    const tail = (await tailResponse.json()) as Processed;
    await expect(page.locator('#studio-status')).toHaveText('Done: your tail is on the board. It passes every check.');
    await expect(tailCard).not.toHaveClass(/\bdragging\b/);
    await expectAllPaths(page, 'tail', tail.path_d);
    await expectAllPaths(page, 'head', defaultHead);
    await expectCard(page, 'tail', 'Your tail', true);
    await expectCard(page, 'head', 'Default head', false);

    const headResponse = await answerTo(page, () => dropFile(page, headCard, 'head.png', 'image/png'));
    expect(headResponse.status(), await headResponse.text()).toBe(200);
    const head = (await headResponse.json()) as Processed;
    await expect(page.locator('#studio-status')).toHaveText('Done: your head is on the board. It passes every check.');
    await expectAllPaths(page, 'head', head.path_d);
    await expectAllPaths(page, 'tail', tail.path_d);

    // A dropped PSD is turned down in the browser, and nothing changes.
    const posted: string[] = [];
    page.on('request', (r) => { if (r.url().includes(ENDPOINT)) posted.push(r.url()); });
    await dropFile(page, headCard, 'template.psd', 'image/vnd.adobe.photoshop');
    await expect(page.locator('#studio-status')).toContainText("That's a PSD");
    expect(posted).toEqual([]);
    await expectAllPaths(page, 'head', head.path_d);
  });

  test('a head and a tail uploaded at once both land, each in its own slot', async ({ page }) => {
    await openStudio(page);
    const defaults = { head: await refPath(page, 'head', 'default'), tail: await refPath(page, 'tail', 'default') };
    // Let the server's token bucket refill a little: both uploads need a token.
    await page.waitForTimeout(3000);
    let release = () => {};
    const held = new Promise<void>((resolve) => { release = resolve; });
    await page.route(isEndpoint, async (route) => { await held; await route.continue(); });
    const answers: Response[] = [];
    page.on('response', (r) => { if (isProcess(r)) answers.push(r); });
    await part(page, 'file', 'head').setInputFiles(fixture('head.png'));
    await expect(page.locator('#studio-status')).toHaveText('Processing your head…');
    await part(page, 'file', 'tail').setInputFiles(fixture('tail.svg'));
    await expect(page.locator('#studio-status')).toHaveText('Processing your tail…');
    await expect(page.getByTestId('studio')).toHaveAttribute('aria-busy', 'true');
    release();
    await expect.poll(() => answers.length).toBe(2);
    await expect(page.getByTestId('studio')).not.toHaveAttribute('aria-busy', 'true');
    const headFile = await readFile(fixture('head.png'));
    const landed: Kind[] = [];
    for (const r of answers) {
      const kind: Kind = r.request().postDataBuffer()?.equals(headFile) ? 'head' : 'tail';
      if (r.status() === 200) {
        landed.push(kind);
        await expectAllPaths(page, kind, ((await r.json()) as Processed).path_d);
      } else {
        // Turned away for capacity (a shared server): that slot stays as it was.
        expect(CAPACITY, await r.text()).toContain(r.status());
        await expectAllPaths(page, kind, defaults[kind]);
      }
    }
    test.skip(landed.length === 0, 'the shared server was at capacity for both');
    await expect(page.getByTestId('studio-summary')).toHaveText(
      landed.length === 2 ? 'Head: passes · Tail: passes' : `${landed[0] === 'head' ? 'Head' : 'Tail'}: passes`);
  });

  test('each card\'s upload button works from the keyboard', async ({ page }) => {
    await openStudio(page);
    // Tab from the head card's button reaches the head's style, then the tail's button.
    await part(page, 'file', 'head').focus();
    await expect(part(page, 'upload', 'head'), 'a ring around the button').toHaveCSS('outline-style', 'solid');
    await page.keyboard.press('Tab');
    await expect(part(page, 'style', 'head')).toBeFocused();
    await page.keyboard.press('Tab');
    await expect(part(page, 'file', 'tail')).toBeFocused();
    const [chooser] = await Promise.all([page.waitForEvent('filechooser'), page.keyboard.press('Space')]);
    expect(chooser.element()).toBeTruthy();
    const response = await answerTo(page, () => chooser.setFiles(fixture('tail.svg')));
    expect(response.status()).toBe(200);
    const tail = (await response.json()) as Processed;
    await expect(page.locator('#studio-status')).toHaveText('Done: your tail is on the board. It passes every check.');
    await expectAllPaths(page, 'tail', tail.path_d);
    await expectAllPaths(page, 'head', await refPath(page, 'head', 'default'));
  });

  test('"This is actually a tail" moves an upload into an empty slot at once', async ({ page }) => {
    await openStudio(page);
    const defaultHead = await refPath(page, 'head', 'default');
    const status = page.locator('#studio-status');
    const tail = await upload(page, 'tail.svg', 'head'); // the wrong card
    const relabel = part(page, 'relabel', 'head');
    await expect(relabel).toHaveText('This is actually a tail');
    await relabel.click();
    await expect(status).toHaveText('Moved: that drawing is now your tail.');
    await expect(part(page, 'confirm', 'head')).toBeHidden();
    await expectAllPaths(page, 'tail', tail.path_d);
    await expectAllPaths(page, 'head', defaultHead);
    await expectCard(page, 'tail', 'Your tail', true);
    await expectCard(page, 'head', 'Default head', false);
    await expect(part(page, 'style', 'head').locator('option[value="user"]')).toHaveCount(0);
    await expect(page.getByTestId('studio-pass-tail')).toBeVisible();
    // Focus follows the drawing, to the button that would move it back.
    await expect(part(page, 'relabel', 'tail')).toBeFocused();
    await expect(part(page, 'relabel', 'tail')).toHaveText('This is actually a head');
  });

  test('moving an upload over the other slot\'s drawing asks first, and moving it back restores it', async ({ page }) => {
    await openStudio(page);
    const status = page.locator('#studio-status');
    const head = await upload(page, 'head.png', 'head');
    const tail = await upload(page, 'tail.svg', 'tail');
    // A head uploaded with the tail card by mistake: it replaces your tail.
    const mistake = await upload(page, 'mirrored-head.png', 'tail');
    await expectAllPaths(page, 'tail', mistake.path_d);

    const relabel = part(page, 'relabel', 'tail');
    const confirm = part(page, 'confirm', 'tail');
    await expect(relabel).toHaveText('This is actually a head');
    await relabel.click();
    // Not moved: a question, with the yes focused.
    await expect(confirm).toBeVisible();
    await expect(confirm).toContainText('Your head slot already has a drawing. Replace it with this one?');
    await expect(part(page, 'confirm-yes', 'tail')).toHaveText('Replace your head');
    await expect(part(page, 'confirm-yes', 'tail')).toBeFocused();
    await expect(relabel).toBeHidden();
    await expectAllPaths(page, 'head', head.path_d);
    await expectAllPaths(page, 'tail', mistake.path_d);

    // Cancel: nothing moves.
    await part(page, 'confirm-no', 'tail').click();
    await expect(confirm).toBeHidden();
    await expect(relabel).toBeVisible();
    await expect(relabel).toBeFocused();
    await expectAllPaths(page, 'head', head.path_d);
    await expectAllPaths(page, 'tail', mistake.path_d);

    // Yes: the drawing becomes the head, and the tail it replaced comes back.
    await relabel.click();
    await part(page, 'confirm-yes', 'tail').click();
    await expect(status).toHaveText('Moved: that drawing is now your head. Your previous tail is back.');
    await expectAllPaths(page, 'head', mistake.path_d);
    await expectAllPaths(page, 'tail', tail.path_d);
    await expect(confirm).toBeHidden();
    // Its head checks now (the response had both kinds').
    await expect(page.locator('#studio-warnings-head li[data-code="faces_left"]')).toBeVisible();
    await expect(page.getByTestId('studio-pass-tail')).toBeVisible();
    await expect(page.getByTestId('studio-summary')).toHaveText('Head: 1 thing to check · Tail: passes');

    // And back again: asks again, then your first head is back.
    await part(page, 'relabel', 'head').click();
    await expect(part(page, 'confirm', 'head')).toBeVisible();
    await expect(part(page, 'confirm-yes', 'head')).toHaveText('Replace your tail');
    await expectAllPaths(page, 'tail', tail.path_d);
    await part(page, 'confirm-yes', 'head').click();
    await expect(status).toHaveText('Moved: that drawing is now your tail. Your previous head is back.');
    await expectAllPaths(page, 'head', head.path_d);
    await expectAllPaths(page, 'tail', mistake.path_d);
  });

  test('a Flip still on its way never lands over a drawing moved into its slot', async ({ page }) => {
    await openStudio(page);
    const status = page.locator('#studio-status');
    const moved = await upload(page, 'tail.svg', 'head'); // a tail, with the head card
    const backwards = await upload(page, 'mirrored-head.png', 'tail'); // a tail on backwards: Flip
    // Hold the tail's Flip.
    let release = () => {};
    const held = new Promise<void>((resolve) => { release = resolve; });
    await page.route(isEndpoint, async (route) => { await held; await route.continue(); });
    await page.locator('#studio-warnings-tail .studio-fix[data-fix="flip"]').click();
    await expect(status).toHaveText('Processing your tail…');

    // Meanwhile, the artist moves their tail over the backwards one.
    await part(page, 'relabel', 'head').click();
    await part(page, 'confirm-yes', 'head').click();
    await expect(status).toHaveText('Moved: that drawing is now your tail.');
    await expectAllPaths(page, 'tail', moved.path_d);

    // The Flip of the drawing it replaced arrives, and changes nothing.
    const answer = page.waitForResponse(isProcess);
    release();
    expect([200, ...CAPACITY]).toContain((await answer).status());
    await page.evaluate(() => new Promise((resolve) => setTimeout(resolve, 300)));
    await expectAllPaths(page, 'tail', moved.path_d);
    await expect(status).toHaveText('Moved: that drawing is now your tail.');
    await expect(page.getByTestId('studio')).not.toHaveAttribute('aria-busy', 'true');
    // And moving it back still restores the backwards tail, as it was.
    await page.unroute(isEndpoint);
    await part(page, 'relabel', 'tail').click();
    await expect(status).toHaveText('Moved: that drawing is now your head. Your previous tail is back.');
    await expectAllPaths(page, 'tail', backwards.path_d);
    await expectAllPaths(page, 'head', moved.path_d);
  });

  test('a stored upload behind a catalog style still counts as a drawing to ask about', async ({ page }) => {
    await openStudio(page);
    const tail = await upload(page, 'tail.svg', 'tail');
    await part(page, 'style', 'tail').selectOption('curled');
    const wrong = await upload(page, 'head.png', 'head');
    await part(page, 'relabel', 'head').click();
    await expect(part(page, 'confirm', 'head')).toBeVisible();
    await expectAllPaths(page, 'head', wrong.path_d);
    await expectAllPaths(page, 'tail', await refPath(page, 'tail', 'curled'));
    // A colour change closes the question without moving anything.
    await page.getByRole('radio', { name: 'Blue #3a86ff' }).check();
    await expect(part(page, 'confirm', 'head')).toBeHidden();
    await part(page, 'style', 'tail').selectOption('user');
    await expectAllPaths(page, 'tail', tail.path_d);
  });
});

test.describe('Head & Tail Studio: recovering', () => {
  test('Flip still works after a reload, by re-posting the saved path', async ({ page }) => {
    await openStudio(page);
    const mirrored = await upload(page, 'mirrored-head.png');
    await page.reload();
    await expect(page.locator('#studio-status')).toHaveText('Welcome back: your last preview is restored.');
    const flip = page.locator('#studio-warnings-head li[data-code="faces_left"] .studio-fix[data-fix="flip"]');
    await expect(flip).toBeVisible();
    await expect(page.locator('#studio-top-fix')).toBeVisible();

    const sent = page.waitForRequest((r) => r.url().includes(ENDPOINT) && r.url().includes('fix=flip'));
    const flipped = await applyFix(page, flip, 'flip');
    expect((await sent).postDataBuffer()?.toString('utf8')).toBe(pageSvg(mirrored));
    expect(flipped.input).toBe('svg');
    expect(flipped.lints.head).toEqual([]);
    await expect(page.locator('#studio-warnings-head li')).toHaveCount(0);
    await expect(page.getByTestId('studio-pass-head')).toBeVisible();
    await expectAllPaths(page, 'head', flipped.path_d);
  });

  test('a failed upload keeps both slots, and Flip still works', async ({ page }) => {
    await openStudio(page);
    const mirrored = await upload(page, 'mirrored-head.png', 'head');
    const tail = await upload(page, 'tail.svg', 'tail');
    const status = page.locator('#studio-status');
    // Not a format the browser can rule out, so the server answers.
    for (const kind of KINDS) {
      const rejected = await answerTo(page, () =>
        part(page, 'file', kind).setInputFiles({ name: 'notes.png', mimeType: 'image/png', buffer: Buffer.from('just some text') }));
      expect(rejected.status()).toBe(422);
      await expect(status).toHaveText("We couldn't tell what kind of file this is. Upload a PNG, JPEG or SVG.");
      await expect(status).toHaveClass(/error/);
      await expectAllPaths(page, 'head', mirrored.path_d);
      await expectAllPaths(page, 'tail', tail.path_d);
    }

    const flip = page.locator('#studio-warnings-head .studio-fix[data-fix="flip"]');
    await expect(flip).toBeVisible();
    const flipped = await applyFix(page, flip, 'flip');
    expect(flipped.lints.head).toEqual([]);
    await expect(status).toContainText('It passes every check');
    await expectAllPaths(page, 'head', flipped.path_d);
    await expectAllPaths(page, 'tail', tail.path_d);
  });

  test("the instant format check gives the server's own advice", async ({ page }) => {
    await openStudio(page);
    const status = page.locator('#studio-status');
    const posted: string[] = [];
    page.on('request', (r) => { if (r.url().includes(ENDPOINT)) posted.push(r.url()); });
    await part(page, 'file', 'tail').setInputFiles({
      name: 'tail.svgz', mimeType: 'image/svg+xml', buffer: Buffer.from([0x1f, 0x8b, 0x08, 0, 0, 0, 0, 0]),
    });
    await expect(status).toContainText('Inkscape: Save As → Plain SVG');
    expect(posted, 'answered in the browser').toEqual([]);

    // A video shares HEIC's container but isn't a photo: no HEIC advice; the server decides.
    const video = Buffer.concat([Buffer.from([0, 0, 0, 0x18]), Buffer.from('ftypisom\0\0\0\0isommp41', 'latin1')]);
    const response = await answerTo(page, () =>
      part(page, 'file', 'head').setInputFiles({ name: 'clip.mp4', mimeType: 'video/mp4', buffer: video }));
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
      await upload(page, 'round-blob.png', 'head');
      await upload(page, 'tail.svg', 'tail');
      // Scrolled so the target's top sits in the top half of the screen.
      await expect.poll(() => page.locator(target).evaluate((el) => {
        const top = el.getBoundingClientRect().top;
        return top >= 0 && top < window.innerHeight / 2;
      })).toBe(true);
      await expect(page.locator('#studio-result-heading')).toBeFocused();
      await expect(page.locator('#studio-closeup-head')).toBeInViewport({ ratio: name === 'phone' ? 0.2 : 0.5 });
    });
  }

  test('the live board stays beside the close-ups while you scroll down to the tail', async ({ page }) => {
    await page.emulateMedia({ reducedMotion: 'reduce' });
    await page.setViewportSize({ width: 820, height: 1180 });
    await openStudio(page);
    await upload(page, 'head.png', 'head');
    await upload(page, 'tail.svg', 'tail');
    // The head close-up scrolled off the top: the tail close-up and the live board show.
    const at = await page.evaluate(() => {
      const panes = document.getElementById('studio-panes') as HTMLElement;
      window.scrollTo(0, window.scrollY + panes.getBoundingClientRect().top + 250);
      const live = (document.querySelector('[data-pane="live"]') as HTMLElement).getBoundingClientRect();
      const tail = (document.getElementById('studio-closeup-tail') as HTMLElement).getBoundingClientRect();
      return { liveTop: live.top, liveBottom: live.bottom, tailTop: tail.top, tailBottom: tail.bottom };
    });
    expect(at.liveTop, JSON.stringify(at)).toBeGreaterThanOrEqual(0);
    expect(at.liveTop, JSON.stringify(at)).toBeLessThanOrEqual(20);
    expect(at.tailTop, JSON.stringify(at)).toBeLessThan(at.liveBottom);
    await expect(page.locator('.studio-live')).toBeInViewport({ ratio: 0.9 });
  });

  // The status line sits under the cards, and Flip far below it in the checks: what
  // happens to a new file or a fix must come into view wherever the page is, or the
  // last result (and its green pass) reads as the new one's.
  for (const [name, size] of [
    ['iPad portrait', { width: 820, height: 1180 }],
    ['phone', { width: 375, height: 812 }],
  ] as const) {
    test(`a new version's progress and errors come into view (${name})`, async ({ page }) => {
      await page.emulateMedia({ reducedMotion: 'reduce' }); // instant scrolls, so positions settle
      await page.setViewportSize(size);
      await openStudio(page);
      await upload(page, 'mirrored-head.png', 'head');
      await upload(page, 'tail.svg', 'tail');
      const status = page.locator('#studio-status');
      const away = async () => {
        await page.locator('#studio-save-image').scrollIntoViewIfNeeded();
        await expect(status, 'the status line is off screen').not.toBeInViewport();
      };

      // Turned down by the browser's own check.
      await away();
      await part(page, 'file', 'tail').setInputFiles(fixture('template.psd'));
      await expect(status).toContainText("That's a PSD");
      await expect(status).toHaveClass(/error/);
      await expect(status).toBeInViewport();

      // A slow upload says it's processing, and the server's answer comes into view
      // even after scrolling back down.
      let release = () => {};
      const held = new Promise<void>((resolve) => { release = resolve; });
      await page.route(isEndpoint, async (route) => { await held; await route.continue(); });
      await away();
      await part(page, 'file', 'tail').setInputFiles({ name: 'notes.png', mimeType: 'image/png', buffer: Buffer.from('just some text') });
      await expect(status).toHaveText('Processing your tail…');
      await expect(status).toBeInViewport();
      await away();
      const answer = page.waitForResponse(isProcess);
      release();
      const rejected = await answer;
      expect([422, ...CAPACITY], await rejected.text()).toContain(rejected.status());
      if (rejected.status() === 422) {
        await expect(status).toHaveText("We couldn't tell what kind of file this is. Upload a PNG, JPEG or SVG.");
      }
      await expect(status).toHaveClass(/error/);
      await expect(status).toBeInViewport();
      // The last results stay.
      await expect(page.getByTestId('studio-pass-tail')).toBeVisible();
      await expect(page.getByTestId('studio-summary')).toHaveText('Head: 1 thing to check · Tail: passes');

      // Flip, down in the checks: its progress comes into view too.
      await page.unroute(isEndpoint);
      let releaseFix = () => {};
      const heldFix = new Promise<void>((resolve) => { releaseFix = resolve; });
      await page.route(isEndpoint, async (route) => { await heldFix; await route.continue(); });
      const flip = page.locator('#studio-warnings-head .studio-fix[data-fix="flip"]');
      await flip.scrollIntoViewIfNeeded();
      await expect(status, 'the status line is off screen from Flip').not.toBeInViewport();
      await flip.click();
      await expect(status).toHaveText('Processing your head…');
      await expect(status).toBeInViewport();
      const fixed = page.waitForResponse(isProcess);
      releaseFix();
      expect([200, ...CAPACITY]).toContain((await fixed).status());
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
    // Before "Your snake", so it's the first thing on the page.
    expect(await start.evaluate((el) =>
      !!(el.compareDocumentPosition(document.querySelector('.studio-snake') as Node) & Node.DOCUMENT_POSITION_FOLLOWING))).toBe(true);

    await upload(page, 'head.png');
    await expect.poll(() => isOpen(start), 'closed after the first upload').toBe(false);
    await expect(summary).toBeVisible();
    await expect(example).toBeHidden();
    await summary.click();
    expect(await isOpen(start)).toBe(true);
    await expect(example).toBeVisible();
    // It stays however the artist left it: another upload doesn't close it.
    await upload(page, 'tail.svg', 'tail');
    expect(await isOpen(start)).toBe(true);
    await summary.click();
    expect(await isOpen(start)).toBe(false);

    // A restored preview counts as an upload. Removing one slot leaves it closed; once
    // neither slot has a drawing of the artist's own, it opens again.
    await page.reload();
    await expect(page.locator('#studio-status')).toHaveText('Welcome back: your last preview is restored.');
    expect(await isOpen(start)).toBe(false);
    await part(page, 'remove', 'head').click();
    expect(await isOpen(start)).toBe(false);
    await part(page, 'remove', 'tail').click();
    await expect.poll(() => isOpen(start), 'open again once both are removed').toBe(true);
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

  test('"Try an example" fills the head slot only, and never replaces your own head', async ({ page }) => {
    await openStudio(page);
    const defaultTail = await refPath(page, 'tail', 'default');
    const status = page.locator('#studio-status');
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
    // It says it's the example, not "your head".
    await expect(status).toHaveText('This is the example head. It passes every check. Upload your own drawing to replace it.');
    await expectAllPaths(page, 'head', result.path_d, result.fill_rule);
    await expectAllPaths(page, 'tail', defaultTail);
    await expectCard(page, 'head', 'Example head', true);
    await expectCard(page, 'tail', 'Default tail', false);
    await expect(part(page, 'style', 'head')).toHaveValue('user');
    await expect(part(page, 'style', 'head').locator('option').first()).toHaveText('Example head');
    await expect(part(page, 'relabel', 'head'), 'the example is a head: no move').toBeHidden();
    await expect(part(page, 'upload-text', 'head'), 'not a version of your own').toHaveText('Upload head');
    await expect(page.getByTestId('studio-summary')).toHaveText('Example head: passes');
    await expect(page.getByTestId('studio-pass-head')).toBeVisible();
    await expect(page.locator('#studio-warnings-head li')).toHaveCount(0);
    await expect(page.getByTestId('studio-top-warning')).toBeHidden();
    await expect(page.locator('#studio-result-heading')).toBeFocused();
    // The example isn't an upload of the artist's own: the templates stay one glance away.
    const start = page.getByTestId('studio-start');
    expect(await isOpen(start)).toBe(true);
    await expect(part(page, 'closeup', 'head')).toHaveAttribute('aria-label', /the example head/);

    // Still the example after a reload, with "Start here" open.
    await page.reload();
    await expect(status).toHaveText(
      'Welcome back: the example head is still on the board. Upload your own drawing to replace it.');
    expect(await isOpen(start)).toBe(true);
    await expectAllPaths(page, 'head', result.path_d, result.fill_rule);

    // Your own tail beside it: the example head stays.
    const tail = await upload(page, 'tail.svg', 'tail');
    await expectAllPaths(page, 'head', result.path_d);
    await expectAllPaths(page, 'tail', tail.path_d);
    await expect(page.getByTestId('studio-summary')).toHaveText('Example head: passes · Tail: passes');
    await expect.poll(() => isOpen(start), 'closed after their own upload').toBe(false);

    // Your own head replaces it.
    const head = await upload(page, 'head.png', 'head');
    await expect(status).toHaveText(/^Done: your head is on the board\./);
    await expectCard(page, 'head', 'Your head', true);
    await expectAllPaths(page, 'head', head.path_d);

    // And the example never replaces yours.
    const posted: string[] = [];
    page.on('request', (r) => { if (r.url().includes(ENDPOINT)) posted.push(r.url()); });
    await start.locator('summary').click();
    await page.locator('#studio-example').click();
    await expect(status).toHaveText(
      'The example is a head, and your head slot holds your own drawing. Remove your head first to try the example.');
    await expect(status).toHaveClass(/error/);
    expect(posted).toEqual([]);
    await expectAllPaths(page, 'head', head.path_d);
    await expectAllPaths(page, 'tail', tail.path_d);
  });

  test('"Learn more" on each check opens its section of the guide', async ({ page }) => {
    await openStudio(page);
    const anchors = new Map<string, string>();
    for (const [name, kind] of [['mirrored-head.png', 'head'], ['round-blob.png', 'head'], ['mirrored-head.png', 'tail']] as const) {
      await upload(page, name, kind);
      const checks = page.locator(`#studio-warnings-${kind} li.studio-lint`);
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
    expect(anchors.get('tail_reversed')).toBe('direction');

    // The guide has every one of those sections.
    await page.goto(GUIDE);
    for (const [code, anchor] of anchors) {
      await expect(page.locator(`.guide [id="${anchor}"]`), `${code} -> #${anchor}`).toHaveCount(1);
    }

    // The top warning's link goes to the first warning's section (the head's).
    await page.goto(STUDIO);
    await expect(page.locator('#studio-status')).toHaveText('Welcome back: your last preview is restored.');
    const firstHref = await page.locator('#studio-warnings-head li.studio-lint.warn a.studio-learn').first().getAttribute('href');
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
  // labels, file inputs their upload buttons), links, and the Details summaries.
  const CONTROLS = '#studio :is(button, select, input, summary, a[href])';

  /** Every visible studio control is at least 44x44 and inside the screen. */
  async function expectTargets(page: Page, width: number, where: string): Promise<number> {
    const found = await page.locator(CONTROLS).evaluateAll((els) => els.flatMap((el) => {
      const box = el.getBoundingClientRect();
      const style = getComputedStyle(el);
      if (!box.width || !box.height || style.visibility === 'hidden' || (el as HTMLElement).closest('[hidden]')) return [];
      const name = `${el.tagName.toLowerCase()}#${el.id || ''}[${el.getAttribute('aria-label') || el.textContent?.trim().slice(0, 30) || (el as HTMLInputElement).value || ''}]`;
      return [{ name, width: box.width, height: box.height, right: box.right + window.scrollX, left: box.left }];
    }));
    for (const c of found) {
      expect(c.width, `${c.name} width at ${width}px (${where})`).toBeGreaterThanOrEqual(44);
      expect(c.height, `${c.name} height at ${width}px (${where})`).toBeGreaterThanOrEqual(44);
      expect(c.right, `${c.name} inside the screen at ${width}px (${where})`).toBeLessThanOrEqual(width + 0.5);
      expect(c.left, `${c.name} inside the screen at ${width}px (${where})`).toBeGreaterThanOrEqual(-0.5);
    }
    return found.length;
  }

  for (const width of [320, 375, 820, 1180]) {
    test(`both cards filled: no horizontal overflow and 44px controls at ${width}px`, async ({ page }) => {
      await page.setViewportSize({ width, height: 900 });
      await openStudio(page);
      // A mirrored head and a mirrored tail, so both groups of checks, both Flips, the
      // top warning, and every card action show.
      await upload(page, 'mirrored-head.png', 'head');
      await upload(page, 'mirrored-head.png', 'tail');
      await expect(page.locator('#studio-top-fix')).toBeVisible();
      for (const kind of KINDS) {
        await expect(part(page, 'relabel', kind)).toBeVisible();
        await expect(part(page, 'download', kind)).toBeVisible();
      }
      await page.evaluate(() => document.fonts.ready);

      // The cards: side by side from 560px, stacked on phones, both whole.
      // Measured together: the page may still be scrolling to the result.
      const [head, tail] = await page.locator('.studio-slot').evaluateAll((els) => els.map((el) => {
        const b = el.getBoundingClientRect();
        return { x: b.x, y: b.y, width: b.width, height: b.height };
      }));
      if (width >= 560) {
        expect(Math.abs(head.y - tail.y), 'side by side').toBeLessThanOrEqual(1);
        expect(tail.x).toBeGreaterThan(head.x + head.width - 1);
      } else {
        expect(tail.y, 'stacked').toBeGreaterThanOrEqual(head.y + head.height - 1);
      }
      for (const kind of KINDS) {
        const card = await page.getByTestId(`studio-slot-${kind}`).boundingBox();
        const inside = await page.getByTestId(`studio-slot-${kind}`).evaluate((el) => {
          const box = el.getBoundingClientRect();
          return Array.from(el.querySelectorAll('*')).filter((c) => {
            const b = c.getBoundingClientRect();
            return b.width > 0 && (b.right > box.right + 0.5 || b.left < box.left - 0.5);
          }).map((c) => `${c.tagName}#${c.id}.${c.getAttribute('class')}`);
        });
        expect(inside, `everything in the ${kind} card stays in it`).toEqual([]);
        expect((card?.x ?? 0) + (card?.width ?? 0)).toBeLessThanOrEqual(width);
      }

      for (const view of ['closeup', 'live', 'all', 'game']) {
        const option = page.locator(`input[name="studio-view"][value="${view}"]`);
        // At 640px+ Live sits beside Close-up and has no option of its own.
        if (await option.isVisible()) await option.check();
        const scrollWidth = await page.evaluate(() => document.documentElement.scrollWidth);
        expect(scrollWidth, `${view} view overflows at ${width}px`).toBeLessThanOrEqual(width);
        const checked = await expectTargets(page, width, view);
        // Uploads x2, styles x2, card actions x6, views, Flip x3, Learn more x3,
        // swatches x10, theme x2, save.
        expect(checked, `controls checked in the ${view} view`).toBeGreaterThanOrEqual(30);
      }

      // The move question, open on both cards at once.
      for (const kind of KINDS) {
        await part(page, 'relabel', kind).click();
        await expect(part(page, 'confirm', kind)).toBeVisible();
      }
      expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(width);
      await expectTargets(page, width, 'move question');

      for (const input of await page.locator('#studio :is(input, select, textarea)').all()) {
        const size = await input.evaluate((el) => parseFloat(getComputedStyle(el).fontSize));
        expect(size, `font size of ${await input.evaluate((el) => el.outerHTML.slice(0, 80))}`).toBeGreaterThanOrEqual(16);
      }
    });
  }
});
