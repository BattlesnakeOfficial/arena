import type { Locator, Page } from '@playwright/test';
import { test, expect } from '../fixtures/test';
import { query } from '../fixtures/db';

/**
 * The public directory is paginated at 50 rows/page and Playwright workers
 * share one database, so a snake created by this test is not guaranteed to be
 * on page 0. Walk the pager until the row shows up.
 *
 * Returns the row locator, or null if the snake isn't listed at all.
 */
async function findSnakeRow(page: Page, name: string): Promise<Locator | null> {
  const MAX_PAGES = 100;

  await page.goto('/snakes');

  for (let visited = 0; visited < MAX_PAGES; visited++) {
    const row = page.locator('table.data tbody tr', {
      has: page.getByRole('link', { name, exact: true }),
    });
    if (await row.count()) {
      return row.first();
    }

    const next = page.getByRole('link', { name: 'Next ›', exact: true });
    if (!(await next.count())) {
      return null;
    }
    await next.click();
    await page.waitForURL(/\/snakes\?page=\d+/);
  }

  throw new Error(`Gave up looking for "${name}" after ${MAX_PAGES} pages of /snakes`);
}

async function createSnake(page: Page, name: string, visibility: 'public' | 'private') {
  await page.goto('/battlesnakes/new');
  await page.getByLabel('Name').fill(name);
  await page.getByLabel('URL').fill(`https://example.com/${encodeURIComponent(name)}`);
  await page.getByLabel('Visibility').selectOption(visibility);
  await page.getByRole('button', { name: 'Create Battlesnake' }).click();
  await expect(page).toHaveURL('/battlesnakes');
}

async function snakeIdByName(name: string, ownerLogin: string): Promise<string> {
  const rows = await query<{ battlesnake_id: string }>(
    `SELECT b.battlesnake_id
     FROM battlesnakes b
     JOIN users u ON b.user_id = u.user_id
     WHERE b.name = $1 AND u.github_login = $2`,
    [name, ownerLogin],
  );
  expect(rows).toHaveLength(1);
  return rows[0].battlesnake_id;
}

test.describe('Public Snakes Directory', () => {
  test('lists public snakes with owner links and hides private ones', async ({
    authenticatedPage,
    mockUser,
  }) => {
    const publicName = `Directory Public ${Date.now()}`;
    const privateName = `Directory Private ${Date.now()}`;

    await createSnake(authenticatedPage, publicName, 'public');
    await createSnake(authenticatedPage, privateName, 'private');

    const publicId = await snakeIdByName(publicName, mockUser.login);

    const row = await findSnakeRow(authenticatedPage, publicName);
    expect(row).not.toBeNull();

    await expect(row!.getByRole('link', { name: publicName, exact: true })).toHaveAttribute(
      'href',
      `/battlesnakes/${publicId}/profile`,
    );
    await expect(row!.getByRole('link', { name: mockUser.login, exact: true })).toHaveAttribute(
      'href',
      `/users/${mockUser.login}`,
    );

    // The private snake is nowhere in the directory, not just off page 0.
    expect(await findSnakeRow(authenticatedPage, privateName)).toBeNull();
  });

  test('is browsable anonymously with a sign-in prompt instead of Challenge', async ({
    authenticatedPage,
    page,
  }) => {
    const snakeName = `Directory Anonymous ${Date.now()}`;
    await createSnake(authenticatedPage, snakeName, 'public');

    const row = await findSnakeRow(page, snakeName);
    expect(row).not.toBeNull();

    await expect(page.getByRole('heading', { name: 'Public Battlesnakes', level: 1 })).toBeVisible();
    await expect(page.locator('nav.site-nav').getByRole('link', { name: 'Snakes', exact: true })).toBeVisible();

    const signIn = row!.getByRole('link', { name: 'Sign in to challenge' });
    await expect(signIn).toBeVisible();
    await expect(signIn).toHaveAttribute('href', '/auth/github');
    await expect(row!.getByRole('button', { name: 'Challenge' })).toHaveCount(0);
  });

  test('Challenge opens a game builder with the snake preselected', async ({
    authenticatedPage,
  }) => {
    const snakeName = `Directory Challenge ${Date.now()}`;
    await createSnake(authenticatedPage, snakeName, 'public');

    const row = await findSnakeRow(authenticatedPage, snakeName);
    expect(row).not.toBeNull();

    await row!.getByRole('button', { name: 'Challenge' }).click();

    await expect(authenticatedPage).toHaveURL(
      /\/games\/flow\/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/,
    );
    await expect(authenticatedPage.getByRole('heading', { name: 'Create New Game' })).toBeVisible();
    await expect(
      authenticatedPage.getByText('You have selected 1 of 4 possible battlesnakes.'),
    ).toBeVisible();

    const selected = authenticatedPage.locator('.gc-slot:not(.empty)');
    await expect(selected).toHaveCount(1);
    await expect(selected).toContainText(snakeName);
  });

  test('challenging a private or unknown snake is rejected', async ({
    authenticatedPage,
    mockUser,
  }) => {
    const privateName = `Directory Rejected ${Date.now()}`;
    await createSnake(authenticatedPage, privateName, 'private');
    const privateId = await snakeIdByName(privateName, mockUser.login);

    const privateResponse = await authenticatedPage.request.post(
      `/battlesnakes/${privateId}/challenge`,
      { maxRedirects: 0 },
    );
    expect(privateResponse.status()).toBe(404);

    const unknownResponse = await authenticatedPage.request.post(
      '/battlesnakes/00000000-0000-4000-8000-000000000000/challenge',
      { maxRedirects: 0 },
    );
    expect(unknownResponse.status()).toBe(404);

    // No half-created flow was left behind for the private snake.
    const flows = await query(
      'SELECT flow_id FROM game_flows WHERE $1 = ANY(selected_battlesnakes)',
      [privateId],
    );
    expect(flows).toHaveLength(0);
  });

  test('paginates at 50 rows per page and clamps out-of-range pages', async ({
    authenticatedPage,
    mockUser,
  }) => {
    const prefix = `Directory Page ${Date.now()}`;

    // Seed in bulk — 55 form submissions would be needlessly slow.
    await query(
      `INSERT INTO battlesnakes (user_id, name, url, visibility)
       SELECT u.user_id,
              $2 || ' ' || to_char(i, 'FM000'),
              'https://example.com/paged',
              'public'
       FROM users u, generate_series(0, 54) AS i
       WHERE u.github_login = $1`,
      [mockUser.login, prefix],
    );

    // Read the production ordering right before the request so the expected
    // row survives concurrent inserts from other workers.
    const [offsetFifty] = await query<{ name: string }>(
      `SELECT b.name
       FROM battlesnakes b
       WHERE b.visibility = 'public'
       ORDER BY b.name, b.battlesnake_id
       LIMIT 1 OFFSET 50`,
    );
    expect(offsetFifty).toBeDefined();

    await authenticatedPage.goto('/snakes?page=1');
    await expect(
      authenticatedPage.locator('table.data tbody').getByRole('link', {
        name: offsetFifty.name,
        exact: true,
      }),
    ).toBeVisible();
    await expect(authenticatedPage.locator('.pager .cur')).toContainText('Page 2');
    await expect(
      authenticatedPage.getByRole('link', { name: '‹ Prev', exact: true }),
    ).toHaveAttribute('href', '/snakes?page=0');

    // A negative page clamps to the first page...
    await authenticatedPage.goto('/snakes?page=-1');
    await expect(authenticatedPage.locator('.pager .cur')).toContainText('Page 1');
    await expect(
      authenticatedPage.getByRole('link', { name: '‹ Prev', exact: true }),
    ).toHaveCount(0);

    // ...and an oversized one clamps to the last, which has no Next link.
    await authenticatedPage.goto('/snakes?page=99999');
    await expect(authenticatedPage.getByRole('link', { name: 'Next ›', exact: true })).toHaveCount(0);
    await expect(
      authenticatedPage.getByRole('link', { name: '‹ Prev', exact: true }),
    ).toBeVisible();
  });

  test('is reachable and usable from the mobile nav', async ({ authenticatedPage }) => {
    const snakeName = `Directory Mobile ${Date.now()}`;
    await createSnake(authenticatedPage, snakeName, 'public');

    await authenticatedPage.setViewportSize({ width: 375, height: 812 });
    await authenticatedPage.goto('/');

    await authenticatedPage.locator('nav.site-nav details.mobile-menu summary').click();
    const sheet = authenticatedPage.locator('nav.site-nav details.mobile-menu .sheet');
    await sheet.getByRole('link', { name: 'Snakes', exact: true }).click();

    await expect(authenticatedPage).toHaveURL('/snakes');
    await expect(
      authenticatedPage.getByRole('heading', { name: 'Public Battlesnakes', level: 1 }),
    ).toBeVisible();

    const row = await findSnakeRow(authenticatedPage, snakeName);
    expect(row).not.toBeNull();
    await expect(row!.getByRole('link', { name: snakeName, exact: true })).toBeVisible();
    await expect(row!.getByRole('button', { name: 'Challenge' })).toBeVisible();
  });
});
