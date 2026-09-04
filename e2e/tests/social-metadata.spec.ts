import type { Page } from '@playwright/test';
import { test, expect } from '../fixtures/test';
import { query } from '../fixtures/db';

const socialTags = [
  ['meta[property="og:url"]', 'og:url'],
  ['meta[property="og:image"]', 'og:image'],
  ['meta[property="og:image:type"]', 'og:image:type'],
  ['meta[property="og:image:width"]', 'og:image:width'],
  ['meta[property="og:image:height"]', 'og:image:height'],
  ['meta[property="og:image:alt"]', 'og:image:alt'],
  ['meta[name="twitter:card"]', 'twitter:card'],
  ['meta[name="twitter:image"]', 'twitter:image'],
  ['meta[name="twitter:image:alt"]', 'twitter:image:alt'],
] as const;

async function assertMetadata(page: Page, baseURL: string, pathname: string): Promise<void> {
  for (const [selector] of socialTags) await expect(page.locator(selector)).toHaveCount(1);

  await expect(page.locator('meta[property="og:url"]')).toHaveAttribute(
    'content',
    `${baseURL}${pathname}`,
  );
  const image = await page.locator('meta[property="og:image"]').getAttribute('content');
  expect(image).toMatch(new RegExp(`^${baseURL.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}/static/og-card\\.png\\?v=.+`));
  await expect(page.locator('meta[name="twitter:image"]')).toHaveAttribute('content', image!);
  await expect(page.locator('meta[property="og:image:type"]')).toHaveAttribute('content', 'image/png');
  await expect(page.locator('meta[property="og:image:width"]')).toHaveAttribute('content', '1200');
  await expect(page.locator('meta[property="og:image:height"]')).toHaveAttribute('content', '630');
  await expect(page.locator('meta[property="og:image:alt"]')).toHaveAttribute('content', 'Battlesnake Arena');
  await expect(page.locator('meta[name="twitter:image:alt"]')).toHaveAttribute('content', 'Battlesnake Arena');
  await expect(page.locator('meta[name="twitter:card"]')).toHaveAttribute('content', 'summary_large_image');
}

test('serves the social card as a 1200x630 PNG', async ({ page }) => {
  const response = await page.request.get('/static/og-card.png');
  expect(response.status()).toBe(200);
  expect(response.headers()['content-type']).toContain('image/png');
  const bytes = await response.body();
  expect(bytes.subarray(0, 8)).toEqual(Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]));
  expect(bytes.readUInt32BE(16)).toBe(1200);
  expect(bytes.readUInt32BE(20)).toBe(630);
});

test('emits complete social metadata for home, leaderboard, and game pages', async ({ page, baseURL }) => {
  const leaderboards = await query<{ leaderboard_id: string }>(
    "SELECT leaderboard_id::text FROM leaderboards WHERE name = 'Standard 11x11'",
  );
  expect(leaderboards).toHaveLength(1);
  const games = await query<{ game_id: string }>(
    `INSERT INTO games (board_size, game_type, status, created_at, updated_at)
     VALUES ('small', 'standard', 'waiting', NOW(), NOW())
     RETURNING game_id::text`,
  );
  const gameId = games[0].game_id;

  try {
    for (const pathname of ['/', `/leaderboards/${leaderboards[0].leaderboard_id}`]) {
      await page.goto(pathname);
      await assertMetadata(page, baseURL!, pathname);
    }
    const gamePath = `/games/${gameId}`;
    await page.goto(`${gamePath}?turn=1`);
    await assertMetadata(page, baseURL!, gamePath);
  } finally {
    await query('DELETE FROM games WHERE game_id = $1::uuid', [gameId]);
  }
});
