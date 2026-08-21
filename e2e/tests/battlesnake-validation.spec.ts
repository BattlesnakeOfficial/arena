import { test, expect, createMockUser } from '../fixtures/test';
import { query } from '../fixtures/db';

test.describe('Battlesnake Validation', () => {
  test('cannot create battlesnake with duplicate name', async ({ authenticatedPage }) => {
    const duplicateName = `Duplicate Snake ${Date.now()}`;

    // Create first battlesnake
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(duplicateName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/first');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();
    await expect(authenticatedPage).toHaveURL('/battlesnakes');
    await expect(authenticatedPage.getByText(duplicateName)).toBeVisible();

    // Try to create second with same name - should stay on form
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(duplicateName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/second');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    // Should stay on form page (not redirect to list)
    await expect(authenticatedPage).toHaveURL('/battlesnakes/new');
  });

  test('cannot update battlesnake to use duplicate name', async ({ authenticatedPage }) => {
    const firstName = `First Snake ${Date.now()}`;
    const secondName = `Second Snake ${Date.now()}`;

    // Create first battlesnake
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(firstName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/first');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    // Create second battlesnake
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(secondName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/second');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    // Try to rename second to first's name
    const secondRow = authenticatedPage.locator('tr', { hasText: secondName });
    await secondRow.getByRole('link', { name: 'Edit', exact: true }).click();
    await authenticatedPage.getByLabel('Name').fill(firstName);
    await authenticatedPage.getByRole('button', { name: 'Update Battlesnake' }).click();

    // Should stay on edit form (not redirect to list)
    await expect(authenticatedPage).toHaveURL(/\/battlesnakes\/.*\/edit/);
  });

  test('can use same name after deleting original', async ({ authenticatedPage }) => {
    const reuseName = `Reuse Name Snake ${Date.now()}`;

    // Create battlesnake
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(reuseName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/original');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();
    await expect(authenticatedPage.getByText(reuseName)).toBeVisible();

    // Delete it
    authenticatedPage.on('dialog', (dialog) => dialog.accept());
    const snakeRow = authenticatedPage.locator('tr', { hasText: reuseName });
    await snakeRow.getByRole('button', { name: 'Delete' }).click();
    await expect(authenticatedPage.getByText(reuseName)).not.toBeVisible();

    // Create new one with same name - should succeed
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(reuseName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/new');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    // Should redirect to list with new snake
    await expect(authenticatedPage).toHaveURL('/battlesnakes');
    await expect(authenticatedPage.getByText(reuseName)).toBeVisible();
  });

  test('different users can have same snake name', async ({ authenticatedPage, loginAsUser }) => {
    const sharedName = `Shared Name Snake ${Date.now()}`;

    // First user creates a snake
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(sharedName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/user1');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();
    await expect(authenticatedPage).toHaveURL('/battlesnakes');
    await expect(authenticatedPage.getByText(sharedName)).toBeVisible();

    // Log out first user
    await authenticatedPage.goto('/auth/logout');

    // Log in as second user
    const secondUser = createMockUser('user2');
    await loginAsUser(authenticatedPage, secondUser);

    // Second user creates snake with same name - should succeed
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(sharedName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/user2');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    // Should succeed and redirect to list
    await expect(authenticatedPage).toHaveURL('/battlesnakes');
    await expect(authenticatedPage.getByText(sharedName)).toBeVisible();
  });

  test('rejects a URL that is not a URL (flash, stays on form)', async ({ authenticatedPage }) => {
    // POST directly: the input's type=url would block this in the browser.
    // Don't follow the redirect, or the one-shot flash is consumed before we look.
    const res = await authenticatedPage.request.post('/battlesnakes', {
      form: { name: `Bad URL ${Date.now()}`, url: 'not a url', visibility: 'private' },
      maxRedirects: 0,
    });
    expect(res.headers()['location']).toBe('/battlesnakes/new');
    await authenticatedPage.goto('/battlesnakes/new');
    await expect(authenticatedPage.getByText('Invalid URL format')).toBeVisible();
  });

  test('server normalizes a bare hostname to https', async ({ authenticatedPage }) => {
    const name = `Bare Host ${Date.now()}`;
    await authenticatedPage.request.post('/battlesnakes', {
      form: { name, url: 'mysnake.fly.dev', visibility: 'private' },
    });
    await authenticatedPage.goto('/battlesnakes');
    await expect(authenticatedPage.getByText('https://mysnake.fly.dev')).toBeVisible();
  });

  test('rejects an over-long name with a flash, not an error page', async ({ authenticatedPage }) => {
    const res = await authenticatedPage.request.post('/battlesnakes', {
      form: { name: 'x'.repeat(65), url: 'https://example.com/long', visibility: 'private' },
      maxRedirects: 0,
    });
    expect(res.headers()['location']).toBe('/battlesnakes/new');
    await authenticatedPage.goto('/battlesnakes/new');
    await expect(authenticatedPage.getByText('64 characters')).toBeVisible();
  });

  test('legacy snake with an over-long name can still be edited without renaming', async ({ authenticatedPage, mockUser }) => {
    const legacyName = `Legacy ${'y'.repeat(70)} ${Date.now()}`;
    const users = await query<{ user_id: string }>('SELECT user_id FROM users WHERE github_login = $1', [mockUser.login]);
    const rows = await query<{ battlesnake_id: string }>(
      `INSERT INTO battlesnakes (user_id, name, url) VALUES ($1, $2, 'https://example.com/legacy') RETURNING battlesnake_id`,
      [users[0].user_id, legacyName]
    );
    const id = rows[0].battlesnake_id;

    await authenticatedPage.goto(`/battlesnakes/${id}/edit`);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/legacy-moved');
    await authenticatedPage.getByRole('button', { name: 'Update Battlesnake' }).click();

    await expect(authenticatedPage.getByText('updated successfully')).toBeVisible();
    const after = await query<{ name: string; url: string }>('SELECT name, url FROM battlesnakes WHERE battlesnake_id = $1', [id]);
    expect(after[0]).toEqual({ name: legacyName, url: 'https://example.com/legacy-moved' });
  });
});
