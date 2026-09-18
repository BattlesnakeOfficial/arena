# Shout moderation eval results (DEV-1297)

Hand-written eval of the post-game shout screener against `jev-latest`,
run 2026-09-18 via the ignored test `shout_eval_manual`
(`scripts/shout-eval-cases.txt`, one multi-noul Jev call with
key-referencing instructions). 34 cases across four categories.

## Results at the shipped threshold (0.90)

| Category | Cases | Suppressed | noul min / max / mean / sd |
|---|---|---|---|
| playful trash talk — MUST NOT suppress | 12 | **0** | 0.020 / 0.200 / 0.074 / 0.063 |
| benign | 8 | **0** | 0.020 / 0.050 / 0.028 / 0.010 |
| clearly violating — MUST suppress | 8 | **8** | 0.980 / 0.980 / 0.980 / 0.000 |
| disguised — SHOULD suppress | 6 | **6** | 0.980 / 0.980 / 0.980 / 0.000 |

Hard requirement met: **zero** playful-trash-talk and **zero** benign
suppressions at 0.90.

## Threshold band check

Jev answers vary sd 0.02–0.05 near a threshold, so the shipped threshold
must sit at least ~0.07 from both clusters:

- Keep-side cluster max: **0.200** ("witness me"-style trash talk) — 0.70
  below the threshold.
- Suppress-side cluster min: **0.980** — 0.08 above the threshold.

Neither cluster enters the ±0.07 band around 0.90 ([0.83, 0.97]); the
nearest edge (0.98) clears it. 0.90 ships unchanged.

## Findings

- **Instructions must reference the record key.** The originally planned
  bare proposition ("Displaying this text publicly would violate…")
  returned a *uniform* noul for every question in a multi-question state
  (measured 0.96 for both "get rekt" and a slur — identical answers, no
  discrimination). Changing the instructions to "The text of the shout
  stored under `shouts.s1` in this state would violate…" separated the
  same pair to 0.33 vs 0.98. The shipped builder interpolates the key
  into each question's instructions.
- **Leetspeak and homoglyph disguises score identically to their plain
  forms** (0.980 across the board): the disguise clause in the
  instructions is doing its job.
- **Zero-width-separated slurs** (U+200B between every letter) also score
  0.980. In production the ingest sanitizer strips U+200B before
  persistence, so Jev sees the concatenated form — still 0.98 on the plain
  form. Defense in depth holds.
- **Playful trash talk has a long right tail** (max 0.200, mean 0.074):
  competitive language lands well clear of the threshold but is not
  indistinguishable from "hello world" — the `state.context` carve-out
  ("playful competitive trash talk is normal and allowed") is load-bearing
  and must not be removed.

## Cost

541 input tokens for a 3-shout state (measured on the probe); ~444 for a
2-shout state. At $0.042/M input tokens a typical 1–5 distinct-shout game
costs well under $0.001 to screen.
