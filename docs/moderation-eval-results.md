# Moderation threshold eval (DEV-1296)

Two full runs of the manual eval (`moderation_eval_manual`, ignored test)
against `jev-latest` on 2026-09-18, ~0.35s/call. Names:
`scripts/moderation-eval-names.txt` (60 names: 20 benign, 15 edgy-but-fine,
10 clearly offensive, 8 leetspeak-disguised, 7 staff impersonation).
Run with:

```bash
TYPESAFE_API_KEY=... cargo test -p arena --bin arena moderation_eval_manual -- --ignored --nocapture
```

## Decision counts per category (run 1 / run 2)

| category | names | allow | flagged | blocked | unchecked |
|---|---|---|---|---|---|
| benign | 20 | 20 / 20 | 0 / 0 | 0 / 0 | 0 / 0 |
| edgy-but-fine | 15 | 15 / 15 | 0 / 0 | 0 / 0 | 0 / 0 |
| clearly offensive | 10 | 1 / 1 | 2 / 2 | 7 / 7 | 0 / 0 |
| leetspeak-disguised | 8 | 0 / 0 | 1 / 2 | 7 / 6 | 0 / 0 |
| staff impersonation | 7 | 0 / 0 | 4 / 4 | 3 / 3 | 0 / 0 |

## `action_block_mass` distribution (run 2)

| category | min | mean | sd |
|---|---|---|---|
| benign | 0.000 | 0.001 | 0.002 |
| edgy-but-fine | 0.000 | 0.001 | 0.002 |
| clearly offensive | 0.030 | 0.865 | 0.287 |
| leetspeak-disguised | 0.810 | 0.930 | 0.073 |
| staff impersonation | 0.670 | 0.851 | 0.118 |

(min/mean/sd over Jev-answered calls; three leetspeak names blocked offline
by the local hard-block list before any Jev call — the spaced `n i g g e r`,
spaced `f a g g o t`, and zero-width-separated disguises all normalize to
exact matches.)

## Flag sources, reported separately

- **Action-driven flags** (Jev label `block`/`flag_for_review` with mass
  under the block threshold): [f-slur] Hunter (0.78–0.84), Retard Brigade
  (0.88), R3t4rd Force (0.81–0.84), Tr4nny Stalker (0.88–0.90), Official
  Battlesnake Team (0.85–0.87), Arena Support Team (0.69–0.76), Admin Snake
  (0.83–0.84), System Administrator (0.60–0.67).
- **Noul-upgrade flags** — names where the noul threshold itself flipped
  the decision (label `allow`, block mass under 0.90, and a noul ≥ 0.60):
  **0 in both runs, across all 60 names.** Every flag and block in the
  offensive categories was action-driven. For context, many names
  *carried* a qualifying noul while already being action-flagged or
  blocked (9–10 of 10 clearly-offensive, 5 of 8 leetspeak, 6 of 7
  impersonation) — the two signals agree wherever Jev answers
  confidently. On this data the 0.60 noul threshold is a second net, not a
  load-bearing signal: it flipped nothing the action question missed, and
  nothing benign or edgy came near it (all five nouls ≪ 0.6, zero false
  flags). It is kept as insurance against action-question blind spots —
  which do exist (see the miss below) — at zero observed false-positive
  cost.

## Surprising names

- **"[k-word] Exterminator" → allow both runs** (mass 0.03/0.06, no noul ≥
  0.60). A genuine Jev blind spot on this construction ("Kill All [k-word]s"
  and other [k-word] constructions elsewhere score 1.00). Known miss; the local
  hard-block list does not cover constructions like this by design
  (exact-match only). Human review of the flag queue remains the backstop
  for this class.
- **"Tr4nny Stalker" straddles the threshold** — 0.900 (block) run 1,
  0.880 (flag) run 2, a live demonstration of the measured 0.02–0.05
  inter-run variance. Both outcomes are non-silent by design: a
  block-labeled answer below the block threshold is a flag, never an
  unrecorded allow.

## Thresholds chosen (match the code literals)

- `MODERATION_BLOCK_THRESHOLD = 0.90` — kept. Benign/edgy block-mass
  ceiling is 0.01, so the false-block clearance is ~0.89, far beyond the
  ±0.07 band the sd-0.02–0.05 variance requires. Everything in the
  0.81–0.90 "Jev leans block" zone becomes a reviewable flag rather than a
  silent allow, so threshold-adjacent variance is harmless there.
- `MODERATION_NOUL_FLAG_THRESHOLD = 0.60` — kept. Zero benign/edgy names
  came anywhere near it (all five nouls ≪ 0.6, zero false flags), and zero
  upgrade-driven flips occurred — it is a second net for action-question
  blind spots, not a load-bearing signal on this data.

Both values live in `Thresholds::default()` (`server/src/moderation/mod.rs`),
`ModerationConfig` env defaults (`server/src/config.rs`), and the README.
