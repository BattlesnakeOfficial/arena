use maud::{Markup, html};

use crate::models::snake_latency::{LatencyStats, RecentLatency};

/// Wide enough for "500ms" at the enlarged mobile label size.
const PLOT_LEFT: f64 = 76.0;
const PLOT_RIGHT: f64 = 610.0;
const PLOT_TOP: f64 = 28.0;
const PLOT_BOTTOM: f64 = 200.0;
/// Timeout markers sit above the plot so they never hide a latency point.
const MARKER_Y: f64 = 12.0;
const GRID_STEPS: u32 = 4;

/// Y-axis ceiling in ms: `GRID_STEPS` gridlines at a 1/2/5 step that covers
/// `max_ms` with ~10% headroom, capped at the move timeout (an answered move
/// can't take longer than that).
pub(crate) fn axis_max_ms(max_ms: f64, timeout_ms: f64) -> f64 {
    let target_step = (max_ms * 1.1 / f64::from(GRID_STEPS)).max(1.0);
    let magnitude = 10f64.powf(target_step.log10().floor());
    let step = [1.0, 2.0, 5.0]
        .into_iter()
        .map(|m| m * magnitude)
        .find(|step| *step >= target_step)
        .unwrap_or(10.0 * magnitude);
    (step * f64::from(GRID_STEPS)).min(timeout_ms)
}

/// Center of the `index`th of `count` equal-width game bands.
fn x_for(index: usize, count: usize) -> f64 {
    PLOT_LEFT + (index as f64 + 0.5) * band_width(count)
}

fn band_width(count: usize) -> f64 {
    (PLOT_RIGHT - PLOT_LEFT) / count.max(1) as f64
}

fn y_for(ms: f64, axis_max: f64) -> f64 {
    let fraction = if axis_max > 0.0 {
        (ms / axis_max).clamp(0.0, 1.0)
    } else {
        0.0
    };
    PLOT_BOTTOM - fraction * (PLOT_BOTTOM - PLOT_TOP)
}

fn fmt_ms(ms: Option<f64>) -> String {
    ms.map_or_else(|| "—".to_string(), |ms| format!("{ms:.0}ms"))
}

fn plural(count: i64, noun: &str) -> String {
    if count == 1 {
        format!("{count} {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// Render a snake's recent per-game move latency: p50 and p95 lines, one
/// clickable band per game linking to it, and a marker above any game where
/// the snake timed out.
///
/// Games are spaced evenly by index rather than by time so bursts of games
/// don't bunch up. Percentiles only cover answered moves; timeouts are shown
/// as markers and counts instead of being dropped.
pub fn latency_chart(recent: &RecentLatency, timeout_ms: i64) -> Markup {
    if recent.games.is_empty() {
        return html! {
            p class="empty" { "No latency data yet. It appears once this snake finishes a game." }
        };
    }

    let count = recent.games.len();
    let timeout = timeout_ms as f64;
    let max_ms = recent
        .games
        .iter()
        .filter_map(|g| g.stats.p95_ms)
        .fold(0.0, f64::max);
    let axis_max = axis_max_ms(max_ms, timeout);
    let at_timeout = axis_max >= timeout;
    let band = band_width(count);

    let points = |pick: fn(&LatencyStats) -> Option<f64>| -> Vec<(f64, f64)> {
        recent
            .games
            .iter()
            .enumerate()
            .filter_map(|(i, g)| pick(&g.stats).map(|ms| (x_for(i, count), y_for(ms, axis_max))))
            .collect()
    };
    let p50_points = points(|s| s.p50_ms);
    let p95_points = points(|s| s.p95_ms);
    let polyline = |pts: &[(f64, f64)]| -> String {
        pts.iter()
            .map(|(x, y)| format!("{x:.1},{y:.1}"))
            .collect::<Vec<_>>()
            .join(" ")
    };

    let overall = recent.overall;
    let total_moves = overall.answered + overall.timeouts;
    let first_date = recent
        .games
        .first()
        .map(|g| g.game_created_at.format("%b %-d").to_string());
    let last_date = recent
        .games
        .last()
        .map(|g| g.game_created_at.format("%b %-d").to_string());

    html! {
        p class="latency-summary" {
            "Last " (plural(count as i64, "game")) ": "
            strong { "p50 " (fmt_ms(overall.p50_ms)) }
            " · "
            strong { "p95 " (fmt_ms(overall.p95_ms)) }
            " · "
            @if overall.timeouts > 0 {
                span class="latency-timeouts" {
                    (overall.timeouts) " of " (plural(total_moves, "move")) " timed out"
                }
            } @else {
                "no timeouts in " (plural(total_moves, "move"))
            }
        }
        svg class="latency-chart" width="100%" viewBox="0 0 620 230" preserveAspectRatio="xMinYMid meet"
            aria-labelledby="latency-chart-title latency-chart-desc" {
            title id="latency-chart-title" { "Move latency over the last " (plural(count as i64, "game")) }
            desc id="latency-chart-desc" {
                "Median (p50) and 95th percentile (p95) /move response time per game, oldest to newest. "
                "Dots above the chart mark games where the snake timed out."
            }
            @for i in 0..=GRID_STEPS {
                @let ms = axis_max * f64::from(GRID_STEPS - i) / f64::from(GRID_STEPS);
                @let y = y_for(ms, axis_max);
                @let is_timeout_line = at_timeout && i == 0;
                line class=(if is_timeout_line { "timeout-line" } else { "grid" })
                    x1=(PLOT_LEFT) y1=(format!("{y:.1}")) x2=(PLOT_RIGHT) y2=(format!("{y:.1}")) {}
                text class=(if is_timeout_line { "lbl timeout-lbl" } else { "lbl" })
                    x=(PLOT_LEFT - 6.0) y=(format!("{:.1}", y + 4.0)) text-anchor="end" {
                    (format!("{ms:.0}ms"))
                }
            }
            // Hit bands go first so the plot marks draw on top of them.
            @for (i, game) in recent.games.iter().enumerate() {
                a href=(format!("/games/{}", game.game_id)) {
                    title {
                        (game.game_created_at.format("%Y-%m-%d %H:%M UTC"))
                        " · p50 " (fmt_ms(game.stats.p50_ms))
                        " · p95 " (fmt_ms(game.stats.p95_ms))
                        " · " (plural(game.stats.timeouts, "timeout"))
                        " in " (plural(game.stats.answered + game.stats.timeouts, "move"))
                    }
                    rect class="hit" x=(format!("{:.1}", PLOT_LEFT + i as f64 * band)) y="0"
                        width=(format!("{band:.1}")) height=(PLOT_BOTTOM) {}
                }
            }
            g class="plot" {
                polyline class="p95" points=(polyline(&p95_points)) {}
                polyline class="p50" points=(polyline(&p50_points)) {}
                @for (x, y) in &p95_points {
                    circle class="pt-p95" cx=(format!("{x:.1}")) cy=(format!("{y:.1}")) r="2.5" {}
                }
                @for (x, y) in &p50_points {
                    circle class="pt-p50" cx=(format!("{x:.1}")) cy=(format!("{y:.1}")) r="2.5" {}
                }
                @for (i, game) in recent.games.iter().enumerate() {
                    @if game.stats.timeouts > 0 {
                        circle class="timeout" cx=(format!("{:.1}", x_for(i, count))) cy=(MARKER_Y) r="4" {}
                    }
                }
            }
            @if let Some(first) = first_date {
                text class="lbl" x=(PLOT_LEFT) y="222" { (first) }
            }
            @if let Some(last) = last_date {
                text class="lbl" x=(PLOT_RIGHT) y="222" text-anchor="end" { (last) }
            }
        }
        div class="latency-legend" {
            span { span class="swatch p95" {} "p95" }
            span { span class="swatch p50" {} "p50 (median)" }
            span { span class="swatch timeout" {} "game with timeouts" }
            span { "Timeout is " (timeout_ms) "ms (dashed red line). Tap a game to open it." }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::snake_latency::GameLatency;
    use chrono::{TimeZone as _, Utc};
    use proptest::prelude::*;
    use uuid::Uuid;

    fn game(
        minute: u32,
        p50: Option<f64>,
        p95: Option<f64>,
        answered: i64,
        timeouts: i64,
    ) -> GameLatency {
        GameLatency {
            game_id: Uuid::new_v4(),
            game_created_at: Utc.with_ymd_and_hms(2026, 9, 29, 12, minute, 0).unwrap(),
            stats: LatencyStats {
                answered,
                timeouts,
                p50_ms: p50,
                p95_ms: p95,
            },
        }
    }

    fn render(recent: &RecentLatency) -> String {
        latency_chart(recent, 500).into_string()
    }

    #[test]
    fn axis_uses_nice_steps_with_headroom() {
        assert_eq!(axis_max_ms(0.0, 500.0), 4.0);
        assert_eq!(axis_max_ms(9.0, 500.0), 20.0);
        assert_eq!(axis_max_ms(48.0, 500.0), 80.0);
        assert_eq!(axis_max_ms(80.0, 500.0), 200.0);
    }

    #[test]
    fn axis_caps_at_the_timeout() {
        assert_eq!(axis_max_ms(480.0, 500.0), 500.0);
        assert_eq!(axis_max_ms(500.0, 500.0), 500.0);
    }

    proptest! {
        #[test]
        fn axis_covers_the_data_without_passing_the_timeout(
            timeout in 10.0f64..10_000.0,
            fraction in 0.0f64..=1.0,
        ) {
            let max_ms = timeout * fraction;
            let axis = axis_max_ms(max_ms, timeout);
            prop_assert!(axis >= max_ms, "axis {axis} below data {max_ms}");
            prop_assert!(axis <= timeout, "axis {axis} above timeout {timeout}");
        }

        #[test]
        fn points_stay_inside_the_plot(ms in 0.0f64..10_000.0, axis in 0.0f64..10_000.0) {
            let y = y_for(ms, axis);
            prop_assert!((PLOT_TOP..=PLOT_BOTTOM).contains(&y));
        }

        #[test]
        fn bands_tile_the_plot_width(count in 1usize..=200, index in 0usize..200) {
            let index = index % count;
            let x = x_for(index, count);
            prop_assert!(x > PLOT_LEFT && x < PLOT_RIGHT);
            let half = band_width(count) / 2.0;
            prop_assert!((x - half - (PLOT_LEFT + index as f64 * band_width(count))).abs() < 1e-9);
        }
    }

    #[test]
    fn empty_history_renders_a_placeholder_instead_of_a_chart() {
        let html = render(&RecentLatency::default());
        assert!(html.contains("No latency data yet"));
        assert!(!html.contains("<svg"));
    }

    #[test]
    fn links_every_game_and_marks_only_games_with_timeouts() {
        let games = vec![
            game(0, Some(10.0), Some(20.0), 100, 0),
            game(1, Some(12.0), Some(40.0), 98, 2),
            game(2, None, None, 0, 150),
        ];
        let recent = RecentLatency {
            overall: LatencyStats {
                answered: 198,
                timeouts: 152,
                p50_ms: Some(11.0),
                p95_ms: Some(38.0),
            },
            games: games.clone(),
        };

        let html = render(&recent);

        for g in &games {
            assert!(html.contains(&format!("href=\"/games/{}\"", g.game_id)));
        }
        assert_eq!(html.matches("class=\"timeout\"").count(), 2);
        // The fully timed-out game has no percentile, so it gets no point.
        assert_eq!(html.matches("class=\"pt-p95\"").count(), 2);
        assert_eq!(html.matches("class=\"pt-p50\"").count(), 2);
        assert!(html.contains("Last 3 games"));
        assert!(html.contains("p50 11ms"));
        assert!(html.contains("p95 38ms"));
        assert!(html.contains("152 of 350 moves timed out"));
        assert!(
            html.contains("p50 —"),
            "all-timeout game tooltip shows no percentile"
        );
    }

    #[test]
    fn clean_history_says_no_timeouts() {
        let recent = RecentLatency {
            overall: LatencyStats {
                answered: 1,
                timeouts: 0,
                p50_ms: Some(7.0),
                p95_ms: Some(7.0),
            },
            games: vec![game(0, Some(7.0), Some(7.0), 1, 0)],
        };

        let html = render(&recent);

        assert!(html.contains("Last 1 game:"));
        assert!(html.contains("no timeouts in 1 move"));
        assert!(!html.contains("class=\"timeout\""));
        assert!(
            !html.contains("timeout-line"),
            "a 7ms snake's axis stops well short of 500ms"
        );
    }

    #[test]
    fn slow_snakes_get_the_timeout_as_the_top_gridline() {
        let recent = RecentLatency {
            overall: LatencyStats {
                answered: 1,
                timeouts: 0,
                p50_ms: Some(450.0),
                p95_ms: Some(490.0),
            },
            games: vec![game(0, Some(450.0), Some(490.0), 1, 0)],
        };

        let html = render(&recent);

        assert_eq!(html.matches("class=\"timeout-line\"").count(), 1);
        assert!(html.contains("class=\"lbl timeout-lbl\""));
        assert!(html.contains(">500ms<"));
    }
}
