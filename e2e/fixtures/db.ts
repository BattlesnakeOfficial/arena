import { Pool } from 'pg';

// Use DATABASE_URL from environment or fall back to local test database
const TEST_DATABASE_URL = process.env.DATABASE_URL || 'postgresql://localhost:5432/arena_test';

let pool: Pool | null = null;

function getPool(): Pool {
  if (!pool) {
    pool = new Pool({ connectionString: TEST_DATABASE_URL });
  }
  return pool;
}

export async function closePool(): Promise<void> {
  if (pool) {
    await pool.end();
    pool = null;
  }
}

export async function query<T = unknown>(text: string, params?: unknown[]): Promise<T[]> {
  const result = await getPool().query(text, params);
  return result.rows as T[];
}

/**
 * Register every (league, snake) that has a raw-inserted fixture entry but no
 * registration yet, copying the entry's membership columns. The DB trigger
 * that used to do this was dropped (DEV-2642 PR 2); fixtures that feed
 * membership-sourced surfaces (rail, profile, rankings, directory) call this
 * right after their entry inserts.
 */
export async function registerFixtureEntries(): Promise<void> {
  await query(
    `INSERT INTO leaderboard_registrations (leaderboard_id, battlesnake_id, disabled_at,
       disabled_reason, health_consecutive_failures, health_consecutive_successes,
       health_last_checked_at, health_last_failure)
     SELECT DISTINCT ON (leaderboard_id, battlesnake_id)
            leaderboard_id, battlesnake_id, disabled_at, disabled_reason,
            health_consecutive_failures, health_consecutive_successes,
            health_last_checked_at, health_last_failure
     FROM leaderboard_entries
     ORDER BY leaderboard_id, battlesnake_id, created_at DESC
     ON CONFLICT (leaderboard_id, battlesnake_id) DO NOTHING`,
  );
}
