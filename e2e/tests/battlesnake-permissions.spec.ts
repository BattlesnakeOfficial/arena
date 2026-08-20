import { test, expect, createMockUser } from '../fixtures/test';

test.describe('Battlesnake Permissions', () => {
  test('cannot edit non-existent battlesnake (404)', async ({ authenticatedPage }) => {
    // Try to edit a battlesnake that doesn't exist
    const nonExistentId = '00000000-0000-0000-0000-000000000000';
    const response = await authenticatedPage.goto(`/battlesnakes/${nonExistentId}/edit`);

    expect(response?.status()).toBe(404);
  });

  test('cannot update non-existent battlesnake (403 or 404)', async ({ authenticatedPage }) => {
    // First go to create page to get a valid form structure
    await authenticatedPage.goto('/battlesnakes/new');

    // Try to POST to update a non-existent battlesnake
    const nonExistentId = '00000000-0000-0000-0000-000000000000';
    const response = await authenticatedPage.request.post(
      `/battlesnakes/${nonExistentId}/update`,
      {
        form: {
          name: 'Test Snake',
          url: 'https://example.com/test',
          visibility: 'public',
        },
      }
    );

    // Should get 403 (forbidden) since it either doesn't exist or doesn't belong to user
    expect(response.status()).toBe(403);
  });

  test('cannot delete non-existent battlesnake (403)', async ({ authenticatedPage }) => {
    const nonExistentId = '00000000-0000-0000-0000-000000000000';

    const response = await authenticatedPage.request.post(
      `/battlesnakes/${nonExistentId}/delete`
    );

    // Should get 403 (forbidden)
    expect(response.status()).toBe(403);
  });

  test('create page requires authentication', async ({ page }) => {
    const response = await page.goto('/battlesnakes/new');
    expect(response?.status()).toBe(401);
  });

  test('list page requires authentication', async ({ page }) => {
    const response = await page.goto('/battlesnakes');
    expect(response?.status()).toBe(401);
  });

  test('unauthenticated page shows a branded sign-in page, not a blank 401', async ({ page }) => {
    // Regression: the 401 used to have an empty body — a blank white screen
    // for a signed-out visitor. It must render the branded "Sign in required"
    // page with a GitHub sign-in CTA (status stays 401 for API/htmx callers).
    const response = await page.goto('/battlesnakes/new');
    expect(response?.status()).toBe(401);
    await expect(page.getByRole('heading', { name: 'Sign in required' })).toBeVisible();
    // Scope to the page body — the nav also has a "Sign in with GitHub" link
    // when logged out, so an unscoped locator matches two elements.
    await expect(
      page.getByRole('main').getByRole('link', { name: 'Sign in with GitHub' })
    ).toBeVisible();
  });

  test('edit page requires authentication', async ({ page }) => {
    const response = await page.goto('/battlesnakes/00000000-0000-0000-0000-000000000000/edit');
    expect(response?.status()).toBe(401);
  });

  test('create POST requires authentication', async ({ page }) => {
    const response = await page.request.post('/battlesnakes', {
      form: {
        name: 'Unauthorized Snake',
        url: 'https://example.com/unauth',
        visibility: 'public',
      },
    });
    expect(response.status()).toBe(401);
  });

  test('update POST requires authentication', async ({ page }) => {
    const response = await page.request.post('/battlesnakes/00000000-0000-0000-0000-000000000000/update', {
      form: {
        name: 'Unauthorized Snake',
        url: 'https://example.com/unauth',
        visibility: 'public',
      },
    });
    expect(response.status()).toBe(401);
  });

  test('delete POST requires authentication', async ({ page }) => {
    const response = await page.request.post('/battlesnakes/00000000-0000-0000-0000-000000000000/delete');
    expect(response.status()).toBe(401);
  });

  test('can only see own battlesnakes in list', async ({ authenticatedPage, loginAsUser }) => {
    const user1SnakeName = `User1 Snake ${Date.now()}`;
    const user2SnakeName = `User2 Snake ${Date.now()}`;

    // First user creates a battlesnake
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(user1SnakeName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/user1');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();
    await expect(authenticatedPage).toHaveURL('/battlesnakes');
    await expect(authenticatedPage.getByText(user1SnakeName)).toBeVisible();

    // Logout first user
    await authenticatedPage.goto('/auth/logout');

    // Login as second user
    const secondUser = createMockUser('user2');
    await loginAsUser(authenticatedPage, secondUser);

    // Second user creates their own snake
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(user2SnakeName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/user2');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();
    await expect(authenticatedPage).toHaveURL('/battlesnakes');

    // Second user should see their snake but NOT first user's snake
    await expect(authenticatedPage.getByText(user2SnakeName)).toBeVisible();
    await expect(authenticatedPage.getByText(user1SnakeName)).not.toBeVisible();
  });
});
