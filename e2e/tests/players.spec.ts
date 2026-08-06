import type { Page } from '@playwright/test';
import { test, expect } from '../fixtures/test';
import { query } from '../fixtures/db';

/**
 * The player directory is paginated at 50 rows/page over a database shared
 * with every other spec, so nothing here may assume a page number, a total
 * count, or that a seeded row lands on page 0. Instead we walk the pager to
 * the end and assert only against the rows this test seeded, identified by a
 * unique per-run prefix that also keeps them adjacent in the fixed ordering.
 */

interface DirectoryRow {
  label: string;
  href: string;
  /** Path + query of the directory page this row was read from. */
  pageUrl: string;
}

/** Path + query of the page currently loaded. */
function currentPath(page: Page): string {
  const url = new URL(page.url());
  return `${url.pathname}${url.search}`;
}

/**
 * Identify a directory page by what it actually selects rather than by its URL
 * spelling: `/players?active=true` and `/players?active=true&page=0` are the
 * same page, and the pager links to the explicit form.
 */
function pageIdentity(pathAndQuery: string): string {
  const url = new URL(pathAndQuery, 'http://localhost');
  const page = Number(url.searchParams.get('page') ?? '0');
  const active = url.searchParams.get('active') === 'true';
  return `${url.pathname}|active=${active}|page=${Number.isNaN(page) ? 0 : page}`;
}

/** Walk the directory from `startPath` to the last page, collecting every row. */
async function collectDirectory(page: Page, startPath: string): Promise<DirectoryRow[]> {
  const rows: DirectoryRow[] = [];
  const visited = new Set<string>();

  await page.goto(startPath);

  for (;;) {
    const pageUrl = currentPath(page);
    if (visited.has(pageUrl)) {
      throw new Error(`Pager looped: ${pageUrl} was visited twice`);
    }
    visited.add(pageUrl);

    for (const link of await page.locator('main table.data tbody tr td a.name').all()) {
      rows.push({
        label: (await link.innerText()).trim(),
        href: (await link.getAttribute('href')) ?? '',
        pageUrl,
      });
    }

    const next = page.getByRole('link', { name: 'Next ›', exact: true });
    if ((await next.count()) === 0) {
      return rows;
    }
    await next.click();
    await page.waitForURL((url) => `${url.pathname}${url.search}` !== pageUrl);
  }
}

/**
 * Traverse the directory until one full walk sees exactly `expectedLabels`
 * among this test's rows, and return those rows.
 *
 * Other specs create and delete users concurrently. A deletion that lands
 * mid-traversal shifts later rows toward earlier pages, so a single walk can
 * skip or repeat a row through no fault of the directory. Retrying until one
 * walk is internally consistent removes that noise without weakening the
 * assertion: a walk that saw every expected row also can't have skipped past
 * an unexpected one, so the caller can trust its exclusions and hrefs too. A
 * genuinely wrong listing never converges and fails with the diff below.
 */
async function collectExpectedRows(
  page: Page,
  startPath: string,
  prefix: string,
  expectedLabels: string[],
): Promise<DirectoryRow[]> {
  const MAX_ATTEMPTS = 10;
  let lastSeen: string[] = [];

  for (let attempt = 0; attempt < MAX_ATTEMPTS; attempt++) {
    const mine = (await collectDirectory(page, startPath)).filter((row) =>
      row.label.toLowerCase().startsWith(prefix),
    );
    lastSeen = mine.map((row) => row.label);

    if (JSON.stringify(lastSeen) === JSON.stringify(expectedLabels)) {
      return mine;
    }
  }

  throw new Error(
    `${startPath} never listed the expected rows for "${prefix}" in ${MAX_ATTEMPTS} traversals.\n` +
      `Expected: ${JSON.stringify(expectedLabels)}\n` +
      `Last saw: ${JSON.stringify(lastSeen)}`,
  );
}

async function insertUser(
  externalId: number,
  login: string,
  displayName: string | null,
): Promise<string> {
  const rows = await query<{ user_id: string }>(
    `INSERT INTO users (external_github_id, github_login, github_access_token, display_name)
     VALUES ($1, $2, 'test-token', $3)
     RETURNING user_id`,
    [externalId, login, displayName],
  );
  return rows[0].user_id;
}

/** Give a user a snake with a leaderboard entry, enabled or paused. */
async function giveActiveSnake(userId: string, name: string, disabled = false): Promise<void> {
  const rows = await query<{ battlesnake_id: string }>(
    `INSERT INTO battlesnakes (user_id, name, url)
     VALUES ($1, $2, 'https://example.com/snake')
     RETURNING battlesnake_id`,
    [userId, name],
  );

  await query(
    `INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id, disabled_at)
     SELECT leaderboard_id, $1, $2
     FROM leaderboards
     WHERE name = 'Standard 11x11'`,
    [rows[0].battlesnake_id, disabled ? new Date() : null],
  );
}

test.describe.serial('Player Directory', () => {
  test('lists players, filters to active snakes, and paginates', async ({ page }) => {
    // Sorts after every other spec's `testuser_*` rows, so the whole seeded
    // block stays contiguous in the directory's fixed ordering.
    const prefix = `zzplayer-${Date.now()}`;
    // `createMockUser` allocates `Date.now() * 1000 + random(0..999)`, and
    // `external_github_id` is BIGINT UNIQUE. Start well clear of that band so
    // a spec minting a mock user in the same millisecond can't collide with
    // this seed under `fullyParallel`.
    const idBase = Date.now() * 1000 + 500_000;

    try {
      const activeId = await insertUser(idBase + 1, `${prefix}-a-active`, `${prefix}-a-active`);
      await giveActiveSnake(activeId, 'Active Snake');

      // Only a paused entry — listed among all players, absent from active.
      const pausedId = await insertUser(idBase + 2, `${prefix}-b-paused`, null);
      await giveActiveSnake(pausedId, 'Paused Snake', true);

      // No snakes at all.
      await insertUser(idBase + 3, `${prefix}-c-snakeless`, null);

      // Two accounts whose logins differ only by case: `/users/{login}` can't
      // tell them apart, so directory links have to carry the UUID.
      const firstTwinId = await insertUser(idBase + 4, `${prefix}-twin`, `${prefix}-d-twin-one`);
      const secondTwinId = await insertUser(
        idBase + 5,
        `${prefix}-TWIN`,
        `${prefix}-e-twin-two`,
      );

      // 51 active players — one more than a page — to exercise the pager.
      await query(
        `INSERT INTO users (external_github_id, github_login, github_access_token, display_name)
         SELECT $1::bigint + i,
                $2 || '-page-' || to_char(i, 'FM000'),
                'test-token',
                $2 || '-page-' || to_char(i, 'FM000')
         FROM generate_series(0, 50) AS i`,
        [idBase + 100, prefix],
      );
      await query(
        `INSERT INTO battlesnakes (user_id, name, url)
         SELECT user_id, 'Paged Snake', 'https://example.com/paged'
         FROM users
         WHERE github_login LIKE $1`,
        [`${prefix}-page-%`],
      );
      await query(
        `INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id)
         SELECT l.leaderboard_id, b.battlesnake_id
         FROM battlesnakes b
         JOIN users u ON u.user_id = b.user_id
         CROSS JOIN leaderboards l
         WHERE l.name = 'Standard 11x11' AND u.github_login LIKE $1`,
        [`${prefix}-page-%`],
      );

      const pagedLabels = Array.from({ length: 51 }, (_, i) =>
        `${prefix}-page-${String(i).padStart(3, '0')}`,
      );

      // --- Default listing: every seeded player, whatever their snakes ---
      // The paused and snakeless players are here; the fallback label proves
      // a NULL display name shows the login instead.
      const allMine = await collectExpectedRows(page, '/players', prefix, [
        `${prefix}-a-active`,
        `${prefix}-b-paused`,
        `${prefix}-c-snakeless`,
        `${prefix}-d-twin-one`,
        `${prefix}-e-twin-two`,
        ...pagedLabels,
      ]);

      await expect(page.getByRole('heading', { name: 'Players', level: 1 })).toBeVisible();

      // Each row links to its own account, distinguished by UUID even where
      // the logins collide.
      const hrefFor = (label: string) => allMine.find((row) => row.label === label)?.href;
      expect(hrefFor(`${prefix}-a-active`)).toBe(`/users/${prefix}-a-active/${activeId}`);
      expect(hrefFor(`${prefix}-d-twin-one`)).toBe(`/users/${prefix}-twin/${firstTwinId}`);
      expect(hrefFor(`${prefix}-e-twin-two`)).toBe(`/users/${prefix}-TWIN/${secondTwinId}`);

      // --- Active filter: only players holding an enabled entry ---
      // The paused, snakeless and snake-less twin rows all drop out.
      const activeMine = await collectExpectedRows(page, '/players?active=true', prefix, [
        `${prefix}-a-active`,
        ...pagedLabels,
      ]);

      // --- The colliding-login links resolve to the right profiles ---
      for (const [label, expectedHeading] of [
        [`${prefix}-d-twin-one`, `${prefix}-d-twin-one`],
        [`${prefix}-e-twin-two`, `${prefix}-e-twin-two`],
      ]) {
        await page.goto(hrefFor(label)!);
        await expect(
          page.getByRole('heading', { name: expectedHeading, level: 1 }),
        ).toBeVisible();
      }

      // --- Pager: the seeded block is bigger than a page, so it spans two ---
      const activePages = [...new Set(activeMine.map((row) => row.pageUrl))];
      expect(activePages.length).toBeGreaterThanOrEqual(2);
      const [firstPage, secondPage] = activePages;
      for (const url of [firstPage, secondPage]) {
        expect(url).toContain('active=true');
      }

      await page.goto(firstPage);
      await page.getByRole('link', { name: 'Next ›', exact: true }).click();
      await expect.poll(() => pageIdentity(currentPath(page))).toBe(pageIdentity(secondPage));

      await page.getByRole('link', { name: '‹ Prev', exact: true }).click();
      await expect.poll(() => pageIdentity(currentPath(page))).toBe(pageIdentity(firstPage));

      // --- Switching filters starts over rather than keeping the page ---
      await page.goto(secondPage);
      await page.getByRole('link', { name: 'All players', exact: true }).click();
      await expect.poll(() => currentPath(page)).toBe('/players');

      await page.getByRole('link', { name: 'With active snakes', exact: true }).click();
      await expect.poll(() => currentPath(page)).toBe('/players?active=true');
    } finally {
      // Snakes and leaderboard entries cascade from the user rows.
      await query('DELETE FROM users WHERE LOWER(github_login) LIKE $1', [`${prefix}-%`]);
    }
  });

  test('is reachable anonymously from the nav', async ({ page }) => {
    await page.goto('/');
    await page.locator('nav.site-nav .links').getByRole('link', { name: 'Players', exact: true }).click();

    await expect(page).toHaveURL('/players');
    await expect(page.getByRole('heading', { name: 'Players', level: 1 })).toBeVisible();
    await expect(page.locator('nav.modes').getByText('All players')).toBeVisible();
  });
});
