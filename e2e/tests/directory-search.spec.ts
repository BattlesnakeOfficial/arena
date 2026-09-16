import { test, expect } from '../fixtures/test';
import { query } from '../fixtures/db';

test('public snake search is literal, case-insensitive, paginated, and excludes private snakes', async ({ authenticatedPage, mockUser, page }) => {
  const term = `Find ${Date.now()} 100%_&+ café`;
  await query(
    `INSERT INTO battlesnakes (user_id, name, url, visibility)
     SELECT u.user_id, $2 || ' ' || to_char(i, 'FM000'), 'https://example.com/search', 'public'
     FROM users u CROSS JOIN generate_series(1, 51) i WHERE u.github_login = $1`,
    [mockUser.login, term],
  );
  await query(
    `INSERT INTO battlesnakes (user_id, name, url, visibility)
     SELECT user_id, $2, 'https://example.com/private', 'private' FROM users WHERE github_login = $1`,
    [mockUser.login, `${term} private`],
  );
  await query(
    `INSERT INTO battlesnakes (user_id, name, url, visibility)
     SELECT user_id, $2, 'https://example.com/decoy', 'public' FROM users WHERE github_login = $1`,
    [mockUser.login, term.replace('%_', 'anything')],
  );

  // Search from an empty, out-of-range result, as an anonymous player.
  await page.goto('/snakes?page=99999&q=no-such-launch-snake');
  await page.getByRole('searchbox', { name: 'Search public battlesnakes' }).fill(`  ${term.toUpperCase()}  `);
  await page.getByRole('button', { name: 'Search', exact: true }).click();
  await expect(page.locator('table.data tbody tr')).toHaveCount(50);
  await expect(page.locator('.pager')).toContainText('of 51 public snakes');
  expect(new URL(page.url()).searchParams.has('page')).toBe(false);
  await expect(page.getByRole('searchbox')).toHaveValue(term.toUpperCase());
  await page.getByRole('link', { name: 'Next ›', exact: true }).click();
  expect(new URL(page.url()).searchParams.get('q')).toBe(term.toUpperCase());
  await expect(page.locator('table.data tbody tr')).toHaveCount(1);
  await expect(page.locator('table.data tbody')).toContainText(`${term} 051`);
  await page.getByRole('link', { name: '‹ Prev', exact: true }).click();
  await expect(page.locator('table.data tbody tr')).toHaveCount(50);

  // Owner-handle search includes their public decoy but never their private snake.
  await page.getByRole('searchbox').fill(mockUser.login.toUpperCase());
  await page.getByRole('button', { name: 'Search', exact: true }).click();
  await expect(page.locator('.pager')).toContainText('of 52 public snakes');
  await page.getByRole('searchbox').fill(`${term} private`);
  await page.getByRole('button', { name: 'Search', exact: true }).click();
  await expect(page.getByText('No public battlesnakes match your search.')).toBeVisible();
  await page.getByRole('link', { name: 'Clear', exact: true }).click();
  await expect(page).toHaveURL('/snakes');
  await expect(page.getByRole('searchbox')).toHaveValue('');
});

test('player search preserves its query across filters and pages and searches handles', async ({ page }) => {
  const term = `Player ${Date.now()} 100%_&+ café`;
  const loginPrefix = `lookup-${Date.now()}`;
  const users = await query<{ user_id: string }>(
    `INSERT INTO users (external_github_id, github_login, github_access_token, display_name)
     SELECT $1::bigint + i, $2 || '-' || i, 'test-token', $3 || ' ' || to_char(i, 'FM000')
     FROM generate_series(1, 52) i RETURNING user_id`,
    [Date.now() * 1000 + 700_000, loginPrefix, term],
  );
  const ids = users.map((u) => u.user_id);
  try {
    // 51 active matches span two pages. The last matching player is inactive.
    await query(
      `INSERT INTO battlesnakes (user_id, name, url)
       SELECT user_id, 'Search entry', 'https://example.com/search'
       FROM users WHERE user_id = ANY($1::uuid[]) AND github_login <> $2`,
      [ids, `${loginPrefix}-52`],
    );
    await query(
      `INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id)
       SELECT l.leaderboard_id, b.battlesnake_id FROM battlesnakes b CROSS JOIN leaderboards l
       WHERE b.user_id = ANY($1::uuid[]) AND l.name = 'Standard 11x11'`,
      [ids],
    );
    await page.goto('/players?page=99999&q=no-such-launch-player');
    await page.getByRole('searchbox', { name: 'Search players' }).fill(`  ${term.toUpperCase()}  `);
    await page.getByRole('button', { name: 'Search', exact: true }).click();
    expect(new URL(page.url()).searchParams.has('page')).toBe(false);
    await expect(page.locator('table.data tbody tr')).toHaveCount(50);
    await expect(page.locator('.pager')).toContainText('of 52 players');
    await page.getByRole('link', { name: 'Next ›', exact: true }).click();
    await expect(page.locator('table.data tbody tr')).toHaveCount(2);
    await page.getByRole('link', { name: 'With active snakes', exact: true }).click();
    expect(new URL(page.url()).searchParams.get('q')).toBe(term.toUpperCase());
    expect(new URL(page.url()).searchParams.has('page')).toBe(false);
    await expect(page.locator('.pager')).toContainText('of 51 players');
    await page.getByRole('link', { name: 'Next ›', exact: true }).click();
    expect(new URL(page.url()).searchParams.get('active')).toBe('true');
    expect(new URL(page.url()).searchParams.get('q')).toBe(term.toUpperCase());
    await expect(page.locator('table.data tbody tr')).toHaveCount(1);

    await page.getByRole('searchbox').fill(`${loginPrefix}-52`.toUpperCase());
    await page.getByRole('button', { name: 'Search', exact: true }).click();
    expect(new URL(page.url()).searchParams.get('active')).toBe('true');
    await expect(page.getByText('No players match your search with active snakes.')).toBeVisible();
    await page.getByRole('link', { name: 'All players', exact: true }).click();
    await expect(page.locator('table.data tbody tr')).toHaveCount(1);
    await expect(page.locator('table.data tbody')).toContainText(`${term} 052`);
    await page.getByRole('link', { name: 'With active snakes', exact: true }).click();
    await page.getByRole('link', { name: 'Clear', exact: true }).click();
    await expect(page).toHaveURL('/players?active=true');
    await expect(page.getByRole('searchbox')).toHaveValue('');
  } finally {
    await query('DELETE FROM battlesnakes WHERE user_id = ANY($1::uuid[])', [ids]);
    await query('DELETE FROM users WHERE user_id = ANY($1::uuid[])', [ids]);
  }
});
