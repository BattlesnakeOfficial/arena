import { test, expect } from '../fixtures/test';
import { query } from '../fixtures/db';

async function gameWithStatus(login: string, status: 'waiting' | 'running' = 'running'): Promise<string> {
  const users = await query<{ user_id: string }>('SELECT user_id FROM users WHERE github_login = $1', [login]);
  const snakes = await query<{ battlesnake_id: string }>(
    `INSERT INTO battlesnakes (user_id, name, url, visibility)
     VALUES ($1, $2, 'https://example.com', 'public') RETURNING battlesnake_id`,
    [users[0].user_id, `Live snake ${Date.now()}`],
  );
  const games = await query<{ game_id: string }>(
    `INSERT INTO games (board_size, game_type, status, created_by_user_id, rematch_battlesnake_ids)
     VALUES ('11x11', 'Standard', $1, $2, $3) RETURNING game_id`,
    [status, users[0].user_id, [snakes[0].battlesnake_id]],
  );
  await query('INSERT INTO game_battlesnakes (game_id, battlesnake_id) VALUES ($1, $2)', [games[0].game_id, snakes[0].battlesnake_id]);
  return games[0].game_id;
}

function terminalHtml(status: 'finished' | 'failed'): string {
  const label = status === 'failed' ? 'Incomplete' : 'Replay';
  const result = status === 'failed' ? 'No result' : '1st';
  return `<html><body>
    <span id="game-status-region">${label}</span>
    <div id="game-outcome-region">${status === 'failed' ? 'no results' : 'finished'}</div>
    <div id="game-actions-region">terminal actions</div>
    <div id="game-results-region">${result}</div>
    <div id="game-metadata-region">${status}</div>
  </body></html>`;
}

test.describe('live game viewer', () => {
  for (const terminal of ['running', 'failed'] as const) {
    test(`Waiting -> ${terminal} reloads into the real server-rendered state`, async ({ authenticatedPage, mockUser }) => {
      const game = await gameWithStatus(mockUser.login, 'waiting');
      await authenticatedPage.goto(`/games/${game}`);
      await query('UPDATE games SET status = $2 WHERE game_id = $1', [game, terminal]);
      await expect(authenticatedPage.locator('#game-status-region')).toContainText(
        terminal === 'running' ? 'Live' : 'Incomplete', { timeout: 8000 },
      );
      if (terminal === 'running') {
        await expect(authenticatedPage.locator('#board-viewer')).toBeVisible();
      } else {
        await expect(authenticatedPage.getByText('No result', { exact: true })).toBeVisible();
      }
    });
  }

  test('delayed status cannot overlap across hide/show and pagehide aborts it', async ({ authenticatedPage, mockUser }) => {
    const game = await gameWithStatus(mockUser.login);
    let active = 0;
    let maximum = 0;
    let calls = 0;
    let markStarted!: () => void;
    const started = new Promise<void>(resolve => { markStarted = resolve; });
    let releaseFirst!: () => void;
    const blocked = new Promise<void>(resolve => { releaseFirst = resolve; });
    await authenticatedPage.route(`**/api/games/${game}`, async route => {
      active += 1;
      maximum = Math.max(maximum, active);
      calls += 1;
      if (calls === 1) {
        markStarted();
        await blocked;
      }
      active -= 1;
      await route.fulfill({ json: { Game: { Status: 'running', ArenaStatus: 'running' } } });
    });
    await authenticatedPage.goto(`/games/${game}`);
    await started;
    await authenticatedPage.evaluate(() => {
      Object.defineProperty(document, 'hidden', { configurable: true, value: true });
      document.dispatchEvent(new Event('visibilitychange'));
      Object.defineProperty(document, 'hidden', { configurable: true, value: false });
      document.dispatchEvent(new Event('visibilitychange'));
    });
    await authenticatedPage.waitForTimeout(100);
    expect(maximum).toBe(1);
    expect(calls).toBe(1);

    await authenticatedPage.evaluate(() => window.dispatchEvent(new PageTransitionEvent('pagehide', { persisted: true })));
    const afterHide = calls;
    releaseFirst();
    await authenticatedPage.waitForTimeout(250);
    expect(calls).toBe(afterHide);
    await expect(authenticatedPage.locator('#game-status-region')).toContainText('Live');
    await authenticatedPage.evaluate(() => window.dispatchEvent(new PageTransitionEvent('pageshow', { persisted: true })));
    await expect.poll(() => calls).toBeGreaterThan(afterHide);
  });

  for (const terminal of ['finished', 'failed'] as const) {
    test(`Running -> ${terminal} replaces terminal regions without replacing playback or title`, async ({ authenticatedPage, mockUser }) => {
      const game = await gameWithStatus(mockUser.login);
      let statusCalls = 0;
      let htmlCalls = 0;
      await authenticatedPage.route(`**/api/games/${game}`, async route => {
        statusCalls += 1;
        await route.fulfill({ json: { Game: { Status: 'complete', ArenaStatus: terminal } } });
      });
      await authenticatedPage.goto(`/games/${game}`);
      const viewer = new URL(authenticatedPage.url());
      await authenticatedPage.locator('input[name="title"]').fill('partially edited title');
      await authenticatedPage.locator('input[name="title"]').focus();
      const iframe = authenticatedPage.locator('#board-viewer');
      const src = await iframe.getAttribute('src');
      await iframe.evaluate((node: HTMLIFrameElement) => { (node as any).__identity = 'kept'; });
      await authenticatedPage.route(url => {
        const candidate = new URL(url.toString());
        return candidate.origin === viewer.origin && candidate.pathname === viewer.pathname;
      }, async route => {
        htmlCalls += 1;
        if (htmlCalls === 1) {
          await route.fulfill({ status: 503, body: 'retry' });
        } else {
          await route.fulfill({ contentType: 'text/html', body: terminalHtml(terminal) });
        }
      });

      await expect(authenticatedPage.locator('#game-status-region')).toContainText(terminal === 'failed' ? 'Incomplete' : 'Replay', { timeout: 12000 });
      expect(htmlCalls).toBeGreaterThanOrEqual(2);
      expect(await iframe.evaluate((node: HTMLIFrameElement) => ({ connected: node.isConnected, marker: (node as any).__identity }))).toEqual({ connected: true, marker: 'kept' });
      expect(await iframe.getAttribute('src')).toBe(src);
      await expect(authenticatedPage.locator('input[name="title"]')).toHaveValue('partially edited title');
      await expect(authenticatedPage.locator('input[name="title"]')).toBeFocused();
      if (terminal === 'failed') await expect(authenticatedPage.locator('#game-results-region')).toContainText('No result');
      const settledCalls = statusCalls;
      await authenticatedPage.waitForTimeout(2500);
      expect(statusCalls).toBe(settledCalls);
    });
  }

  test('real Running -> Failed refreshes actual markup and preserves the iframe and edited title', async ({ authenticatedPage, mockUser }) => {
    const game = await gameWithStatus(mockUser.login);
    await query('UPDATE game_battlesnakes SET placement = 1 WHERE game_id = $1', [game]);
    await authenticatedPage.goto(`/games/${game}`);
    const iframe = authenticatedPage.locator('#board-viewer');
    const src = await iframe.getAttribute('src');
    await iframe.evaluate((node: HTMLIFrameElement) => { (node as any).__realIdentity = true; });
    const title = authenticatedPage.locator('input[name="title"]');
    await title.fill('keep this draft');
    await title.focus();
    await query("UPDATE games SET status = 'failed' WHERE game_id = $1", [game]);
    await expect(authenticatedPage.locator('#game-status-region')).toContainText('Incomplete', { timeout: 10000 });
    await expect(authenticatedPage.locator('#game-results-region')).toContainText('No result');
    await expect(authenticatedPage.locator('#game-results-region .scard.p1')).toHaveCount(0);
    expect(await iframe.evaluate((node: HTMLIFrameElement) => (node as any).__realIdentity)).toBe(true);
    expect(await iframe.getAttribute('src')).toBe(src);
    await expect(title).toHaveValue('keep this draft');
    await expect(title).toBeFocused();
    await expect(authenticatedPage.getByRole('button', { name: 'Rematch' })).toBeVisible();
  });

  test('malformed and non-OK statuses retry when storage is unavailable', async ({ authenticatedPage, mockUser }) => {
    const game = await gameWithStatus(mockUser.login);
    let calls = 0;
    await authenticatedPage.addInitScript(() => {
      Object.defineProperty(window, 'sessionStorage', { configurable: true, get() { throw new Error('disabled'); } });
      const real = window.setTimeout;
      window.setTimeout = ((fn: TimerHandler, delay?: number, ...args: unknown[]) =>
        real(fn, Math.min(delay || 0, 20), ...args)) as typeof window.setTimeout;
    });
    await authenticatedPage.route(`**/api/games/${game}`, async route => {
      calls += 1;
      if (calls === 1) return route.fulfill({ status: 503, body: 'nope' });
      if (calls === 2) return route.fulfill({ json: null });
      if (calls === 3) return route.fulfill({ json: { Game: { Status: 'running' } } });
      return route.fulfill({ json: { Game: { Status: 'running', ArenaStatus: 'running' } } });
    });
    await authenticatedPage.goto(`/games/${game}`);
    await expect.poll(() => calls).toBeGreaterThanOrEqual(4);
    await expect(authenticatedPage.locator('#game-manual-refresh')).toBeHidden();
  });

  test('a stalled status request is aborted and retried within the bounded polling budget', async ({ authenticatedPage, mockUser }) => {
    const game = await gameWithStatus(mockUser.login);
    await authenticatedPage.addInitScript(gameId => {
      const realFetch = window.fetch.bind(window);
      let statusCalls = 0;
      Object.defineProperty(window, '__statusCalls', {
        configurable: true,
        get: () => statusCalls,
      });
      window.fetch = ((input: RequestInfo | URL, init?: RequestInit) => {
        const url = typeof input === 'string' ? input : input instanceof URL ? input.toString() : input.url;
        if (url.endsWith(`/api/games/${gameId}`)) {
          statusCalls += 1;
          if (statusCalls === 1) {
            return new Promise<Response>((_resolve, reject) => {
              init?.signal?.addEventListener('abort', () => {
                reject(init.signal?.reason ?? new DOMException('request aborted', 'AbortError'));
              }, { once: true });
            });
          }
        }
        return realFetch(input, init);
      }) as typeof window.fetch;

      const realSetTimeout = window.setTimeout;
      window.setTimeout = ((fn: TimerHandler, delay?: number, ...args: unknown[]) =>
        realSetTimeout(fn, Math.min(delay || 0, 20), ...args)) as typeof window.setTimeout;
    }, game);

    await authenticatedPage.goto(`/games/${game}`);
    await expect.poll(
      () => authenticatedPage.evaluate(() => (window as typeof window & { __statusCalls: number }).__statusCalls),
      { timeout: 1000 },
    ).toBeGreaterThan(1);
  });

  for (const phase of ['status', 'terminal'] as const) {
    test(`a stalled ${phase} response body is aborted and retried`, async ({ authenticatedPage, mockUser }) => {
      const game = await gameWithStatus(mockUser.login);
      await authenticatedPage.addInitScript(({ gameId, phase, html }) => {
        const realFetch = window.fetch.bind(window);
        let bodyAttempts = 0;
        Object.defineProperty(window, '__bodyAttempts', { get: () => bodyAttempts });
        window.fetch = ((input: RequestInfo | URL, init?: RequestInit) => {
          const raw = typeof input === 'string' ? input : input instanceof URL ? input.toString() : input.url;
          const path = new URL(raw, window.location.href).pathname;
          const statusRequest = path === `/api/games/${gameId}`;
          const terminalRequest = path === `/games/${gameId}`;
          if (phase === 'terminal' && statusRequest) {
            return Promise.resolve(new Response(JSON.stringify({ Game: { ArenaStatus: 'failed' } })));
          }
          if ((phase === 'status' && statusRequest) || (phase === 'terminal' && terminalRequest)) {
            bodyAttempts += 1;
            if (bodyAttempts === 1) {
              const response = new Response('');
              const stalledBody = () => new Promise<never>((_resolve, reject) => {
                init?.signal?.addEventListener('abort', () => {
                  reject(init.signal?.reason ?? new DOMException('body aborted', 'AbortError'));
                }, { once: true });
              });
              if (phase === 'status') response.json = stalledBody;
              else response.text = stalledBody;
              return Promise.resolve(response);
            }
            if (phase === 'terminal') return Promise.resolve(new Response(html));
          }
          return realFetch(input, init);
        }) as typeof window.fetch;
        const realSetTimeout = window.setTimeout;
        window.setTimeout = ((fn: TimerHandler, delay?: number, ...args: unknown[]) =>
          realSetTimeout(fn, Math.min(delay || 0, 20), ...args)) as typeof window.setTimeout;
      }, { gameId: game, phase, html: terminalHtml('failed') });
      await authenticatedPage.goto(`/games/${game}`);
      await expect.poll(() => authenticatedPage.evaluate(() =>
        (window as typeof window & { __bodyAttempts: number }).__bodyAttempts)).toBeGreaterThan(1);
      if (phase === 'terminal') {
        await expect(authenticatedPage.locator('#game-status-region')).toContainText('Incomplete');
      }
    });
  }

  test('exhausted shared budget reveals manual Refresh', async ({ authenticatedPage, mockUser }) => {
    const game = await gameWithStatus(mockUser.login);
    await authenticatedPage.evaluate(([key]) => sessionStorage.setItem(key, '600'), [`arena-game-poll-${game}`]);
    await authenticatedPage.goto(`/games/${game}`);
    await expect(authenticatedPage.locator('#game-manual-refresh')).toBeVisible({ timeout: 5000 });
  });

  test('manual Refresh remains available without JavaScript', async ({ browser, authenticatedPage: _authenticatedPage, mockUser }) => {
    const game = await gameWithStatus(mockUser.login, 'waiting');
    const context = await browser.newContext({ javaScriptEnabled: false });
    const response = await context.request.get(`/games/${game}`);
    const body = await response.text();
    expect(body).toContain('id="game-manual-refresh"');
    expect(body).not.toContain('id="game-manual-refresh" hidden');
    await context.close();
  });
});
