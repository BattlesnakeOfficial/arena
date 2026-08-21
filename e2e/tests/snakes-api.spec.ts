import { test, expect } from '../fixtures/test';

test.describe('Snakes API', () => {
  test.describe('POST /api/snakes - name validation', () => {
    test('rejects an empty name', async ({ authenticatedPage }) => {
      const response = await authenticatedPage.request.post('/api/snakes', {
        data: { name: '', url: 'https://example.com/empty-name' },
      });
      expect(response.status()).toBe(400);
      expect(await response.text()).toContain('Name is required');
    });

    test('rejects a whitespace-only name', async ({ authenticatedPage }) => {
      const response = await authenticatedPage.request.post('/api/snakes', {
        data: { name: '   ', url: 'https://example.com/blank-name' },
      });
      expect(response.status()).toBe(400);
      expect(await response.text()).toContain('Name is required');
    });

    test('rejects a name longer than 64 characters', async ({ authenticatedPage }) => {
      const response = await authenticatedPage.request.post('/api/snakes', {
        data: { name: 'x'.repeat(65), url: 'https://example.com/long-name' },
      });
      expect(response.status()).toBe(400);
      expect(await response.text()).toContain('64 characters');
    });

    test('trims surrounding whitespace from the name', async ({ authenticatedPage }) => {
      const name = `Trimmed ${Date.now()}`;
      const response = await authenticatedPage.request.post('/api/snakes', {
        data: { name: `  ${name}  `, url: 'https://example.com/trimmed' },
      });
      expect(response.status()).toBe(201);
      const snake = await response.json();
      expect(snake.name).toBe(name);

      // PUT applies the same rules
      const update = await authenticatedPage.request.put(`/api/snakes/${snake.id}`, {
        data: { name: ' ' },
      });
      expect(update.status()).toBe(400);
      expect(await update.text()).toContain('Name is required');
    });
  });
});

test.describe('Battlesnake form - URL validation', () => {
  test('rejects a URL that is not a URL', async ({ authenticatedPage }) => {
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(`Bad URL ${Date.now()}`);
    await authenticatedPage.getByLabel('URL').fill('not a url');
    await authenticatedPage.getByLabel('Visibility').selectOption('private');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    await expect(authenticatedPage).toHaveURL(/\/battlesnakes\/new$/);
    await expect(authenticatedPage.getByText('Invalid URL format')).toBeVisible();
  });

  test('still accepts a bare hostname (normalized to https)', async ({ authenticatedPage }) => {
    const name = `Bare Host ${Date.now()}`;
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(name);
    await authenticatedPage.getByLabel('URL').fill('mysnake.fly.dev');
    await authenticatedPage.getByLabel('Visibility').selectOption('private');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    await expect(authenticatedPage.getByText('created successfully')).toBeVisible();
    await expect(authenticatedPage.getByText('https://mysnake.fly.dev')).toBeVisible();
  });
});
