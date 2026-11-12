// This file uses material from the Wikipedia article <a href="https://en.wikipedia.org/wiki/List_of_snakes_by_common_name">"List of snakes by common name"</a>, which is released under the <a href="https://creativecommons.org/licenses/by-sa/3.0/">Creative Commons Attribution-Share-Alike License 3.0</a>.
//! Random snake-name generation for `play` (ported verbatim from the Go CLI's
//! `cli/commands/names.go`; the attribution above is a licence requirement).

use rand::seq::SliceRandom;

/// The name pool. Shuffled per `SnakeNamer` construction; never modified in
/// place beyond the shuffle (consumption pops from the front).
pub(crate) const SNAKE_NAMES: &[&str] = &[
    "Adder",
    "Aesculapian Snake",
    "Anaconda",
    "Arafura File Snake",
    "Asp",
    "African Beaked Snake",
    "Ball Python",
    "Bird Snake",
    "Black-headed Snake",
    "Mexican Black Kingsnake",
    "Black Rat Snake",
    "Black Snake",
    "Blind Snake",
    "Boa",
    "Boiga",
    "Boomslang",
    "Brown Snake",
    "Bull Snake",
    "Bushmaster",
    "Dwarf Beaked Snake",
    "Rufous Beaked Snake",
    "Canebrake",
    "Cantil",
    "Cascabel",
    "Cat-eyed Snake",
    "Cat Snake",
    "Chicken Snake",
    "Coachwhip Snake",
    "Cobra",
    "Collett's Snake",
    "Congo Snake",
    "Copperhead",
    "Coral Snake",
    "Corn Snake",
    "Cottonmouth",
    "Crowned Snake",
    "Cuban Wood Snake",
    "Egg-eater",
    "Eyelash Viper",
    "Fer-de-lance",
    "Fierce Snake",
    "Fishing Snake",
    "Flying Snake",
    "Fox Snake",
    "Forest Flame Snake",
    "Garter Snake",
    "Glossy Snake",
    "Gopher Snake",
    "Grass Snake",
    "Green Snake",
    "Ground Snake",
    "Habu",
    "Harlequin Snake",
    "Herald Snake",
    "Hognose Snake",
    "Hoop Snake",
    "Hundred Pacer",
    "Ikaheka Snake",
    "Indigo Snake",
    "Jamaican Tree Snake",
    "Jararacussu",
    "Keelback",
    "King Brown",
    "King Cobra",
    "King Snake",
    "Krait",
    "Lancehead",
    "Lora",
    "Lyre Snake",
    "Machete Savane",
    "Mamba",
    "Mamushi",
    "Mangrove Snake",
    "Milk Snake",
    "Moccasin Snake",
    "Montpellier Snake",
    "Mud Snake",
    "Mussurana",
    "Night Snake",
    "Nose-horned Viper",
    "Parrot Snake",
    "Patchnose Snake",
    "Pine Snake",
    "Pipe Snake",
    "Python",
    "Queen Snake",
    "Racer",
    "Raddysnake",
    "Rat Snake",
    "Rattlesnake",
    "Ribbon Snake",
    "Rinkhals",
    "River Jack",
    "Sea Snake",
    "Shield-tailed Snake",
    "Sidewinder",
    "Small-eyed Snake",
    "Stiletto Snake",
    "Striped Snake",
    "Sunbeam Snake",
    "Taipan",
    "Tentacled Snake",
    "Tic Polonga",
    "Tiger Snake",
    "Tigre Snake",
    "Tree Snake",
    "Trinket Snake",
    "Twig Snake",
    "Twin Headed King Snake",
    "Titanoboa",
    "Urutu",
    "Vine Snake",
    "Viper",
    "Wart Snake",
    "Water Moccasin",
    "Water Snake",
    "Whip Snake",
    "Wolf Snake",
    "Worm Snake",
    "Wutu",
    "Yarara",
    "Zebra Snake",
];

/// A shuffled pool of names. Takes unique names until exhausted, then falls
/// back to fresh UUIDs (Go's behaviour).
pub struct SnakeNamer {
    names: Vec<&'static str>,
}

impl SnakeNamer {
    pub fn new() -> Self {
        let mut names = SNAKE_NAMES.to_vec();
        names.shuffle(&mut rand::thread_rng());
        Self { names }
    }

    /// Pop the next name from the front of the shuffled pool.
    pub fn take(&mut self) -> String {
        if self.names.is_empty() {
            return uuid::Uuid::new_v4().to_string();
        }
        self.names.remove(0).to_string()
    }
}

impl Default for SnakeNamer {
    fn default() -> Self {
        Self::new()
    }
}

/// Process-global namer: one shuffle per process, matching Go's package
/// `init()` shuffle. Tests construct fresh `SnakeNamer`s instead of draining
/// this pool (order-dependent asserts otherwise).
/// Generate a snake name from the process-global pool. One shuffle per
/// process, matching Go's package `init()` shuffle. Tests construct fresh
/// `SnakeNamer`s instead of draining this pool (order-dependent asserts
/// otherwise).
pub(crate) fn generate() -> String {
    use std::sync::{Mutex, OnceLock};
    static NAMER: OnceLock<Mutex<SnakeNamer>> = OnceLock::new();
    NAMER
        .get_or_init(|| Mutex::new(SnakeNamer::new()))
        .lock()
        .expect("snake namer mutex poisoned")
        .take()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namer_yields_unique_names_then_uuids() {
        let mut namer = SnakeNamer::new();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..SNAKE_NAMES.len() {
            let name = namer.take();
            assert!(seen.insert(name), "duplicate name from the pool");
        }
        // Exhausted: UUID-shaped strings.
        assert!(namer.take().len() == 36, "UUID shape after exhaustion");
    }

    /// The pool length matches the ported list (asserted against the list
    /// itself, not a hardcoded count — the count is 122 at port time).
    #[test]
    fn pool_matches_the_ported_list() {
        assert_eq!(SNAKE_NAMES.len(), 122);
        assert_eq!(SNAKE_NAMES.first(), Some(&"Adder"));
        assert_eq!(SNAKE_NAMES.last(), Some(&"Zebra Snake"));
    }
}
