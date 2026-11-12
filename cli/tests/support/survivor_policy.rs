//! The survivor snake's move policy, shared by tests that need a local game
//! to outlive a given turn. A pure function of the `/move` request body;
//! each test brings its own transport.
//!
//! Include with `#[path]`; `cli/tests/support/` isn't its own test target.

type Cell = (i64, i64);
const DIRS: [(i64, i64); 4] = [(0, 1), (0, -1), (-1, 0), (1, 0)];
const DIR_NAMES: [&str; 4] = ["up", "down", "left", "right"];

/// Deterministic survivor. Hungry (health <= 25): BFS to the nearest food
/// whose first step passes keeps_tail. Fed: maximize (passes keeps_tail,
/// doesn't eat, BFS distance from the tail). From request turn `suicide_turn`
/// on, always "up" so the uncapped game terminates (wall within <= 19 moves).
/// Verified against the real engine at 6c21780: survives seeds 1..=30.
pub fn survivor_move(body: &str, suicide_turn: i64) -> &'static str {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let pt = |p: &serde_json::Value| (p["x"].as_i64().unwrap_or(0), p["y"].as_i64().unwrap_or(0));
    let cells: Vec<Cell> = v["you"]["body"]
        .as_array()
        .map(|a| a.iter().map(pt).collect())
        .unwrap_or_default();
    let turn = v["turn"].as_i64().unwrap_or(0);
    if turn >= suicide_turn {
        return "up"; // suicide so the uncapped game ends
    }
    let w = v["board"]["width"].as_i64().unwrap_or(19);
    let h = v["board"]["height"].as_i64().unwrap_or(19);
    let on = |c: Cell| (0..w).contains(&c.0) && (0..h).contains(&c.1);
    let head = cells[0];
    let tail = *cells.last().expect("non-empty body");
    let health = v["you"]["health"].as_i64().unwrap_or(100);
    let food: Vec<Cell> = v["board"]["food"]
        .as_array()
        .map(|a| a.iter().map(pt).collect())
        .unwrap_or_default();
    let food_set: std::collections::HashSet<Cell> = food.iter().copied().collect();
    // The tail cell vacates this turn — but ONLY if that cell is
    // single-stacked. Spawn bodies are [P,P,P] (place_snakes_fixed) and
    // every meal pushes a tail copy (feed_snakes): a stacked tail clears
    // one tick late, so it stays blocked for the immediate move.
    let tail_single = cells.iter().filter(|c| **c == tail).count() == 1;
    let bl: std::collections::HashSet<Cell> = cells
        .iter()
        .copied()
        .filter(|c| *c != tail || !tail_single)
        .collect();
    let safe: Vec<(usize, Cell)> = DIRS
        .iter()
        .enumerate()
        .map(|(i, d)| (i, (head.0 + d.0, head.1 + d.1)))
        .filter(|(_, c)| on(*c) && !bl.contains(c))
        .collect();

    if health <= 25 && !food.is_empty() {
        // BFS from the head with parent pointers, then the nearest food
        // (distance, then board order) whose first step is safe AND passes
        // keeps_tail.
        let mut dist: std::collections::HashMap<Cell, i64> = std::collections::HashMap::new();
        let mut parent: std::collections::HashMap<Cell, Cell> = std::collections::HashMap::new();
        dist.insert(head, 0);
        let mut queue = std::collections::VecDeque::from([head]);
        while let Some(c) = queue.pop_front() {
            let d = dist[&c];
            for dlt in &DIRS {
                let x = (c.0 + dlt.0, c.1 + dlt.1);
                if on(x) && !bl.contains(&x) && !dist.contains_key(&x) {
                    dist.insert(x, d + 1);
                    parent.insert(x, c);
                    queue.push_back(x);
                }
            }
        }
        let mut order: Vec<(i64, usize)> = food
            .iter()
            .enumerate()
            .filter_map(|(i, f)| dist.get(f).map(|d| (*d, i)))
            .collect();
        order.sort_unstable();
        for (_, i) in order {
            let mut cur = food[i];
            while let Some(p) = parent.get(&cur) {
                if *p == head {
                    break;
                }
                cur = *p;
            }
            if let Some((di, _)) = safe.iter().find(|(_, c)| *c == cur)
                && keeps_tail(&cells, cur, &food_set, w, h)
            {
                return DIR_NAMES[*di];
            }
        }
        // Nothing reachable that keeps the tail: fall through to the
        // ranking below.
    }

    // Fed: maximize (passes keeps_tail, doesn't eat, BFS distance from the
    // tail). Ties keep the earlier direction (up, down, left, right).
    let dist_tail = bfs(tail, &bl, w, h);
    let mut best: Option<(i64, i64, i64, usize)> = None;
    for (di, c) in &safe {
        let k = keeps_tail(&cells, *c, &food_set, w, h) as i64;
        let not_eats = !food_set.contains(c) as i64;
        let td = dist_tail.get(c).copied().unwrap_or(-1);
        let key = (k, not_eats, td);
        if best.is_none_or(|(bk, be, btd, _)| key > (bk, be, btd)) {
            best = Some((k, not_eats, td, *di));
        }
    }
    match best {
        Some((_, _, _, di)) => DIR_NAMES[di],
        None => "up", // no legal move: die
    }
}

fn bfs(
    start: Cell,
    bl: &std::collections::HashSet<Cell>,
    w: i64,
    h: i64,
) -> std::collections::HashMap<Cell, i64> {
    let on = |c: Cell| (0..w).contains(&c.0) && (0..h).contains(&c.1);
    let mut dist = std::collections::HashMap::new();
    dist.insert(start, 0);
    let mut queue = std::collections::VecDeque::from([start]);
    while let Some(c) = queue.pop_front() {
        let d = dist[&c];
        for dlt in &DIRS {
            let n = (c.0 + dlt.0, c.1 + dlt.1);
            if on(n) && !bl.contains(&n) && !dist.contains_key(&n) {
                dist.insert(n, d + 1);
                queue.push_back(n);
            }
        }
    }
    dist
}

/// The body after this turn's move: insert the new head, pop the tail; if
/// the new head is on food, growth pushes a copy of the new tail
/// (`move_snakes` pops, `feed_snakes` re-pushes — net: stacked tail).
fn next_body(body: &[Cell], step: Cell, eats: bool) -> Vec<Cell> {
    let mut nb: Vec<Cell> = body.to_vec();
    nb.pop();
    nb.insert(0, step);
    if eats {
        let t = *nb.last().expect("non-empty");
        nb.push(t);
    }
    nb
}

/// Simulate the move (incl. growth) and check the tail stays reachable from
/// the new head. Vacate-aware: the body cell at position i (0 = new head,
/// n-1 = tail) pops after (n - i) turns assuming no further growth, so it
/// is enterable at BFS distance >= (n - i). A stacked tail (post-meal
/// growth, or the [P,P,P] spawn body) needs distance >= 2: treating it as
/// free at d=1 accepts doomed eats into pockets whose only exit is the
/// stacked tail (verified: seed 6, turn 316 SelfCollision); treating it as
/// never-free rejects every eating move and the snake starves next to food
/// (verified: 0/30 seeds). With this rule: 30/30 seeds survive.
fn keeps_tail(
    body: &[Cell],
    step: Cell,
    food: &std::collections::HashSet<Cell>,
    w: i64,
    h: i64,
) -> bool {
    let eats = food.contains(&step);
    let nb = next_body(body, step, eats);
    let n = nb.len();
    let tail = *nb.last().expect("non-empty");
    let mut first_pos: std::collections::HashMap<Cell, usize> = std::collections::HashMap::new();
    for (i, c) in nb.iter().enumerate() {
        first_pos.entry(*c).or_insert(i);
    }
    let free_at = |c: Cell| -> i64 {
        match first_pos.get(&c) {
            Some(i) => (n - i) as i64,
            None => 0,
        }
    };
    let mut dist: std::collections::HashMap<Cell, i64> = std::collections::HashMap::new();
    dist.insert(step, 0);
    let mut queue = std::collections::VecDeque::from([step]);
    while let Some(c) = queue.pop_front() {
        let d = dist[&c];
        for dlt in &DIRS {
            let x = (c.0 + dlt.0, c.1 + dlt.1);
            let arrival = d + 1;
            if (0..w).contains(&x.0)
                && (0..h).contains(&x.1)
                && !dist.contains_key(&x)
                && arrival >= free_at(x)
            {
                dist.insert(x, arrival);
                queue.push_back(x);
            }
        }
    }
    dist.contains_key(&tail)
}
