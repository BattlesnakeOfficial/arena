import { test, expect } from '../fixtures/test';

test.describe('Battlesnake Edit', () => {
  test('can navigate to edit page from list', async ({ authenticatedPage }) => {
    const uniqueName = `Edit Nav Snake ${Date.now()}`;

    // Create a battlesnake first
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(uniqueName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/edit-nav');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();
    await expect(authenticatedPage).toHaveURL('/battlesnakes');

    // Click the Edit button for our snake (use exact match to avoid matching URL containing 'edit')
    const snakeRow = authenticatedPage.locator('tr', { hasText: uniqueName });
    await snakeRow.getByRole('link', { name: 'Edit', exact: true }).click();

    // Should be on edit page with correct heading
    await expect(authenticatedPage.getByRole('heading', { name: `Edit Battlesnake: ${uniqueName}` })).toBeVisible();
  });

  test('edit form is pre-populated with existing values', async ({ authenticatedPage }) => {
    const uniqueName = `Prepop Snake ${Date.now()}`;
    const originalUrl = 'https://example.com/original';

    // Create a battlesnake
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(uniqueName);
    await authenticatedPage.getByLabel('URL').fill(originalUrl);
    await authenticatedPage.getByLabel('Visibility').selectOption('private');
    await authenticatedPage.getByLabel('Engine region').selectOption('us-east4');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    // Navigate to edit
    const snakeRow = authenticatedPage.locator('tr', { hasText: uniqueName });
    await snakeRow.getByRole('link', { name: 'Edit', exact: true }).click();

    // Form should be pre-filled
    await expect(authenticatedPage.getByLabel('Name')).toHaveValue(uniqueName);
    await expect(authenticatedPage.getByLabel('URL')).toHaveValue(originalUrl);
    await expect(authenticatedPage.getByLabel('Visibility')).toHaveValue('private');
    await expect(authenticatedPage.getByLabel('Engine region')).toHaveValue('us-east4');
    await authenticatedPage.getByLabel('Engine region').selectOption('europe-west4');
    await authenticatedPage.getByRole('button', { name: 'Update Battlesnake' }).click();
    await authenticatedPage.locator('tr', { hasText: uniqueName })
      .getByRole('link', { name: 'Edit', exact: true }).click();
    await expect(authenticatedPage.getByLabel('Engine region')).toHaveValue('europe-west4');
    await authenticatedPage.goto('/battlesnakes');
    await authenticatedPage.locator('tr', { hasText: uniqueName }).getByRole('link', { name: uniqueName }).click();
    await expect(authenticatedPage.getByText('Europe (Netherlands)')).toBeVisible();
  });

  test('can update battlesnake name', async ({ authenticatedPage }) => {
    const originalName = `Original Name ${Date.now()}`;
    const updatedName = `Updated Name ${Date.now()}`;

    // Create a battlesnake
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(originalName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/update-name');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    // Navigate to edit
    const snakeRow = authenticatedPage.locator('tr', { hasText: originalName });
    await snakeRow.getByRole('link', { name: 'Edit', exact: true }).click();

    // Update the name
    await authenticatedPage.getByLabel('Name').fill(updatedName);
    await authenticatedPage.getByRole('button', { name: 'Update Battlesnake' }).click();

    // Should redirect to list
    await expect(authenticatedPage).toHaveURL('/battlesnakes');

    // Updated name should appear, original should not
    await expect(authenticatedPage.getByText(updatedName)).toBeVisible();
    await expect(authenticatedPage.getByText(originalName)).not.toBeVisible();
  });

  test('can update battlesnake URL', async ({ authenticatedPage }) => {
    const uniqueName = `URL Update Snake ${Date.now()}`;
    const originalUrl = 'https://example.com/original-url';
    const updatedUrl = 'https://example.com/updated-url';

    // Create a battlesnake
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(uniqueName);
    await authenticatedPage.getByLabel('URL').fill(originalUrl);
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    // Navigate to edit
    const snakeRow = authenticatedPage.locator('tr', { hasText: uniqueName });
    await snakeRow.getByRole('link', { name: 'Edit', exact: true }).click();

    // Update the URL
    await authenticatedPage.getByLabel('URL').fill(updatedUrl);
    await authenticatedPage.getByRole('button', { name: 'Update Battlesnake' }).click();

    // Should redirect to list with updated URL
    await expect(authenticatedPage).toHaveURL('/battlesnakes');
    await expect(authenticatedPage.getByText(updatedUrl)).toBeVisible();
  });

  test('can change visibility from public to private', async ({ authenticatedPage }) => {
    const uniqueName = `Visibility Change Snake ${Date.now()}`;

    // Create a public battlesnake
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(uniqueName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/visibility');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    // Verify it's public
    const snakeRow = authenticatedPage.locator('tr', { hasText: uniqueName });
    await expect(snakeRow.locator('.badge', { hasText: 'Public' })).toBeVisible();

    // Navigate to edit and change to private
    await snakeRow.getByRole('link', { name: 'Edit', exact: true }).click();
    await authenticatedPage.getByLabel('Visibility').selectOption('private');
    await authenticatedPage.getByRole('button', { name: 'Update Battlesnake' }).click();

    // Should now show Private badge
    await expect(authenticatedPage).toHaveURL('/battlesnakes');
    const updatedRow = authenticatedPage.locator('tr', { hasText: uniqueName });
    await expect(updatedRow.locator('.badge', { hasText: 'Private' })).toBeVisible();
  });

  test('cancel button returns to list without saving', async ({ authenticatedPage }) => {
    const originalName = `Cancel Test Snake ${Date.now()}`;

    // Create a battlesnake
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(originalName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/cancel');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    // Navigate to edit
    const snakeRow = authenticatedPage.locator('tr', { hasText: originalName });
    await snakeRow.getByRole('link', { name: 'Edit', exact: true }).click();

    // Make changes but click Cancel
    await authenticatedPage.getByLabel('Name').fill('Should Not Save');
    await authenticatedPage.getByRole('link', { name: 'Cancel' }).click();

    // Should be back on list with original name unchanged
    await expect(authenticatedPage).toHaveURL('/battlesnakes');
    await expect(authenticatedPage.getByText(originalName)).toBeVisible();
    await expect(authenticatedPage.getByText('Should Not Save')).not.toBeVisible();
  });

  test('edit page requires authentication', async ({ page }) => {
    // Try to access edit page without auth (using a random UUID)
    const response = await page.goto('/battlesnakes/00000000-0000-0000-0000-000000000000/edit');
    expect(response?.status()).toBe(401);
  });
});
