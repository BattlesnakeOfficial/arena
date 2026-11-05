//! Go-parity JSONL export for `play --output`: line 1 the game, one snake
//! request per frame, final result line. Ported from the Go CLI's
//! `cli/commands/output.go`.

use color_eyre::Result;

use engine_core::wire;

#[derive(serde::Serialize)]
struct JsonResult {
    #[serde(rename = "winnerId")]
    winner_id: String,
    #[serde(rename = "winnerName")]
    winner_name: String,
    #[serde(rename = "isDraw")]
    is_draw: bool,
}

/// Collects the game line, one snake-request line per frame, and the final
/// result line; `write_to` emits them newline-terminated. Empty winner
/// fields serialize as `""` (Go's result struct has no omitempty).
pub struct GameExporter {
    game: wire::NestedGame,
    frames: Vec<wire::Game>,
    winner_id: String,
    winner_name: String,
    is_draw: bool,
}

impl GameExporter {
    pub fn new(game: wire::NestedGame) -> Self {
        Self {
            game,
            frames: Vec::new(),
            winner_id: String::new(),
            winner_name: String::new(),
            is_draw: false,
        }
    }

    pub fn add_frame(&mut self, frame: wire::Game) {
        self.frames.push(frame);
    }

    pub fn set_result(&mut self, winner_id: &str, winner_name: &str, is_draw: bool) {
        self.winner_id = winner_id.to_string();
        self.winner_name = winner_name.to_string();
        self.is_draw = is_draw;
    }

    /// Write every line. Returns the number of lines written (Go's
    /// `FlushToFile` count, which `play` logs as
    /// `Wrote N lines to output file: ...`).
    pub fn write_to(&self, w: &mut impl std::io::Write) -> Result<usize> {
        let mut line = serde_json::to_string(&self.game)?;
        line.push('\n');
        w.write_all(line.as_bytes())?;
        for frame in &self.frames {
            let mut line = serde_json::to_string(frame)?;
            line.push('\n');
            w.write_all(line.as_bytes())?;
        }
        let result = JsonResult {
            winner_id: self.winner_id.clone(),
            winner_name: self.winner_name.clone(),
            is_draw: self.is_draw,
        };
        let mut line = serde_json::to_string(&result)?;
        line.push('\n');
        w.write_all(line.as_bytes())?;
        Ok(self.frames.len() + 2)
    }
}
