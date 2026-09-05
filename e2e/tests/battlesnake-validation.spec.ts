import { test, expect, createMockUser } from '../fixtures/test';
import { query } from '../fixtures/db';

const sixLanguageTags = ['Python', 'Rust', 'Go', 'TypeScript', 'Elixir', 'Ruby'];

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
    const submittedUrl = 'https://example.com/distinct-second';
    await authenticatedPage.getByLabel('URL').fill(submittedUrl);
    await authenticatedPage.getByLabel('Visibility').selectOption('private');
    await authenticatedPage.getByLabel('Python', { exact: true }).check();
    await authenticatedPage.getByLabel('Rust', { exact: true }).check();
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    // Should stay on form page (not redirect to list)
    await expect(authenticatedPage).toHaveURL('/battlesnakes/new');
    await expect(authenticatedPage.getByText('already have a battlesnake named')).toBeVisible();
    await expect(authenticatedPage.getByLabel('Name')).toHaveValue(duplicateName);
    await expect(authenticatedPage.getByLabel('URL')).toHaveValue(submittedUrl);
    await expect(authenticatedPage.getByLabel('Visibility')).toHaveValue('private');
    await expect(authenticatedPage.getByLabel('Python', { exact: true })).toBeChecked();
    await expect(authenticatedPage.getByLabel('Rust', { exact: true })).toBeChecked();
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
    const submittedUrl = 'https://example.com/failed-edit';
    await authenticatedPage.getByLabel('URL').fill(submittedUrl);
    await authenticatedPage.getByLabel('Visibility').selectOption('private');
    await authenticatedPage.getByLabel('Go', { exact: true }).check();
    await authenticatedPage.getByLabel('TypeScript', { exact: true }).check();
    await authenticatedPage.getByRole('button', { name: 'Update Battlesnake' }).click();

    // Should stay on edit form (not redirect to list)
    await expect(authenticatedPage).toHaveURL(/\/battlesnakes\/.*\/edit/);
    await expect(authenticatedPage.getByText('already have a battlesnake named')).toBeVisible();
    await expect(authenticatedPage.getByLabel('Name')).toHaveValue(firstName);
    await expect(authenticatedPage.getByLabel('URL')).toHaveValue(submittedUrl);
    await expect(authenticatedPage.getByLabel('Visibility')).toHaveValue('private');
    await expect(authenticatedPage.getByLabel('Go', { exact: true })).toBeChecked();
    await expect(authenticatedPage.getByLabel('TypeScript', { exact: true })).toBeChecked();

    const editUrl = authenticatedPage.url();
    await authenticatedPage.goto('/battlesnakes');
    await authenticatedPage.goto(editUrl);
    await expect(authenticatedPage.getByLabel('Name')).toHaveValue(secondName);
    await expect(authenticatedPage.getByLabel('URL')).toHaveValue('https://example.com/second');
    await expect(authenticatedPage.getByLabel('Visibility')).toHaveValue('public');
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
    const name = `Bad URL ${Date.now()}`;
    const res = await authenticatedPage.request.post('/battlesnakes', {
      form: { name, url: 'not a url', visibility: 'private' },
      maxRedirects: 0,
    });
    expect(res.headers()['location']).toBe('/battlesnakes/new');
    await authenticatedPage.goto('/battlesnakes/new');
    await expect(authenticatedPage.getByText('Invalid URL format')).toBeVisible();
    await expect(authenticatedPage.getByLabel('Name')).toHaveValue(name);
    await expect(authenticatedPage.getByLabel('URL')).toHaveValue('not a url');
    await expect(authenticatedPage.getByLabel('Visibility')).toHaveValue('private');
  });

  test('client script normalizes a bare hostname before submit', async ({ authenticatedPage }) => {
    const name = `Bare Host ${Date.now()}`;
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(name);
    await authenticatedPage.getByLabel('URL').fill('mysnake.fly.dev');
    await authenticatedPage.getByLabel('URL').blur();
    await expect(authenticatedPage.getByLabel('URL')).toHaveValue('https://mysnake.fly.dev');
    await authenticatedPage.getByLabel('Visibility').selectOption('private');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();
    await expect(authenticatedPage).toHaveURL('/battlesnakes');
    await expect(authenticatedPage.getByText('https://mysnake.fly.dev')).toBeVisible();
  });

  test('server normalizes a bare hostname to https', async ({ authenticatedPage }) => {
    const name = `Server Bare Host ${Date.now()}`;
    const res = await authenticatedPage.request.post('/battlesnakes', {
      form: { name, url: 'server-snake.fly.dev', visibility: 'private' },
      maxRedirects: 0,
    });

    expect(res.headers()['location']).toBe('/battlesnakes');
    await authenticatedPage.goto('/battlesnakes');
    await expect(authenticatedPage.getByText(name)).toBeVisible();
    await expect(authenticatedPage.getByText('https://server-snake.fly.dev')).toBeVisible();
  });

  test('preserves all submitted values when create exceeds the tag cap', async ({ authenticatedPage }) => {
    const name = `Six Tag Create ${Date.now()}`;
    const url = 'https://example.com/six-tag-create';
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(name);
    await authenticatedPage.getByLabel('URL').fill(url);
    await authenticatedPage.getByLabel('Visibility').selectOption('private');
    for (const tag of sixLanguageTags) {
      await authenticatedPage.getByLabel(tag, { exact: true }).check();
    }

    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    await expect(authenticatedPage).toHaveURL('/battlesnakes/new');
    await expect(authenticatedPage.getByText('at most 5 tags')).toBeVisible();
    await expect(authenticatedPage.getByLabel('Name')).toHaveValue(name);
    await expect(authenticatedPage.getByLabel('URL')).toHaveValue(url);
    await expect(authenticatedPage.getByLabel('Visibility')).toHaveValue('private');
    for (const tag of sixLanguageTags) {
      await expect(authenticatedPage.getByLabel(tag, { exact: true })).toBeChecked();
    }
  });

  test('preserves failed six-tag edit without mutating the snake', async ({ authenticatedPage }) => {
    const savedName = `Six Tag Edit ${Date.now()}`;
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(savedName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/saved-edit');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    const snakeRow = authenticatedPage.locator('tr', { hasText: savedName });
    await snakeRow.getByRole('link', { name: 'Edit', exact: true }).click();
    const editUrl = authenticatedPage.url();
    const submittedName = `${savedName} changed`;
    const submittedUrl = 'https://example.com/failed-six-tag-edit';
    await authenticatedPage.getByLabel('Name').fill(submittedName);
    await authenticatedPage.getByLabel('URL').fill(submittedUrl);
    await authenticatedPage.getByLabel('Visibility').selectOption('private');
    for (const tag of sixLanguageTags) {
      await authenticatedPage.getByLabel(tag, { exact: true }).check();
    }

    await authenticatedPage.getByRole('button', { name: 'Update Battlesnake' }).click();

    await expect(authenticatedPage).toHaveURL(editUrl);
    await expect(authenticatedPage.getByText('at most 5 tags')).toBeVisible();
    await expect(authenticatedPage.getByLabel('Name')).toHaveValue(submittedName);
    await expect(authenticatedPage.getByLabel('URL')).toHaveValue(submittedUrl);
    await expect(authenticatedPage.getByLabel('Visibility')).toHaveValue('private');
    for (const tag of sixLanguageTags) {
      await expect(authenticatedPage.getByLabel(tag, { exact: true })).toBeChecked();
    }

    await authenticatedPage.goto('/battlesnakes');
    await authenticatedPage.goto(editUrl);
    await expect(authenticatedPage.getByLabel('Name')).toHaveValue(savedName);
    await expect(authenticatedPage.getByLabel('URL')).toHaveValue('https://example.com/saved-edit');
    await expect(authenticatedPage.getByLabel('Visibility')).toHaveValue('public');
    for (const tag of sixLanguageTags) {
      await expect(authenticatedPage.getByLabel(tag, { exact: true })).not.toBeChecked();
    }
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

  // A pending form draft is only cleared by a subsequent GET to the matching
  // form (session::take_pending_form_data), never by the success path of
  // create_battlesnake/update_battlesnake. Two direct POSTs to the same
  // target with no intervening GET (a real pattern: manual-redirect clients,
  // retried submissions) leave the failed draft in place, so the NEXT GET
  // resurrects stale, already-superseded rejected input instead of the
  // value that was actually just saved -- silently, with no error flash.
  test('successful edit does not resurrect a stale pending draft on the next visit', async ({ authenticatedPage }) => {
    const savedName = `Stale Draft Snake ${Date.now()}`;
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(savedName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/stale-draft-initial');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    const snakeRow = authenticatedPage.locator('tr', { hasText: savedName });
    await snakeRow.getByRole('link', { name: 'Edit', exact: true }).click();
    const editUrl = authenticatedPage.url();
    const battlesnakeId = editUrl.match(/battlesnakes\/([^/]+)\/edit/)?.[1];
    const updatePath = `/battlesnakes/${battlesnakeId}/update`;

    // A malformed URL fails validation and parks a draft targeted at this
    // snake. Don't follow the redirect -- nothing consumes the draft yet.
    await authenticatedPage.request.post(updatePath, {
      form: { name: savedName, url: 'not a url', visibility: 'private' },
      maxRedirects: 0,
    });

    // A second, valid submission succeeds, again without an intervening GET
    // to the edit page.
    const goodUrl = 'https://example.com/stale-draft-good-final';
    await authenticatedPage.request.post(updatePath, {
      form: { name: savedName, url: goodUrl, visibility: 'public' },
      maxRedirects: 0,
    });

    // Visiting the edit page now must show what was actually saved, not the
    // rejected draft from the failed attempt that preceded it.
    await authenticatedPage.goto(editUrl);
    await expect(authenticatedPage.getByLabel('URL')).toHaveValue(goodUrl);
    await expect(authenticatedPage.getByLabel('Visibility')).toHaveValue('public');
  });
});
