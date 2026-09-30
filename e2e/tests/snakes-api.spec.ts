import { test, expect } from '../fixtures/test';

test.describe('Snakes API', () => {
  test('region defaults, updates and rejects unknown values', async ({ authenticatedPage }) => {
    const created = await authenticatedPage.request.post('/api/snakes', {
      data: { name: `Region ${Date.now()}`, url: 'https://example.com/region' },
    });
    expect(created.status()).toBe(201);
    const snake = await created.json();
    expect(snake.engine_region).toBe('us-west1');

    const changed = await authenticatedPage.request.put(`/api/snakes/${snake.id}`, {
      data: { engine_region: 'us-east4' },
    });
    expect(changed.status()).toBe(200);
    expect((await changed.json()).engine_region).toBe('us-east4');

    const retained = await authenticatedPage.request.put(`/api/snakes/${snake.id}`, {
      data: { url: 'https://example.com/region-updated' },
    });
    expect((await retained.json()).engine_region).toBe('us-east4');

    const invalid = await authenticatedPage.request.put(`/api/snakes/${snake.id}`, {
      data: { engine_region: 'moon' },
    });
    expect(invalid.status()).toBe(400);

    const nullRegion = await authenticatedPage.request.put(`/api/snakes/${snake.id}`, {
      data: { engine_region: null },
    });
    expect(nullRegion.status()).toBe(400);
  });

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
    });

    test('PUT rejects a blank name but leaves the name alone when omitted', async ({ authenticatedPage }) => {
      const name = `Updatable ${Date.now()}`;
      const created = await authenticatedPage.request.post('/api/snakes', {
        data: { name, url: 'https://example.com/updatable' },
      });
      expect(created.status()).toBe(201);
      const snake = await created.json();

      const blank = await authenticatedPage.request.put(`/api/snakes/${snake.id}`, {
        data: { name: ' ' },
      });
      expect(blank.status()).toBe(400);
      expect(await blank.text()).toContain('Name is required');

      const urlOnly = await authenticatedPage.request.put(`/api/snakes/${snake.id}`, {
        data: { url: 'https://example.com/moved' },
      });
      expect(urlOnly.status()).toBe(200);
      expect((await urlOnly.json()).name).toBe(name);
    });
  });
});
