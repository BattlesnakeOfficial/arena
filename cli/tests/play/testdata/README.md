# play testdata

Golden fixtures for the `battlesnake play` tests.

## JSON files

Verbatim copies of `BattlesnakeOfficial/rules@87e094e`
`cli/commands/testdata/` — minus `snake_request_body_wrapped.json` and
`snake_request_body_squad.json`, whose game types this CLI does not run:

- `snake_request_body*.json` — Go's `TestGetIndividualBoardStateForSnake` /
  `TestSettingsRequestSerialization` request bodies (rebuilt as engine state
  and compared field-for-field; the only normalized difference is
  `game.source`, `""` in the fixtures vs `"custom"` from this CLI).
- `jsonl_game.json`, `jsonl_turn_{0,1}.json`, `jsonl_game_complete.json` —
  Go's `TestOutputFile` export (stub ruleset, `snk_0`, one move).

## viewmap_*.txt

Generated at 87e094e by a throwaway in-package Go test calling
`GameState.printMap` on a fixed board (11x11, s1 `ONE` #123456 alive at
(3,3) health 55, s2 `TWO` wall-collision turn 2 with its head off-board,
food at (5,5), hazard at (2,2)). Line 1 is a provenance header; everything
after it is the exact Go output, compared byte-for-byte by
`cli/src/play/board_view.rs`'s tests.
