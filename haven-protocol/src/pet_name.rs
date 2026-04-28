//! Generates friendly session names in Haven's farm/woodland aesthetic
//! (e.g. `mossy-fox`, `golden-orchard`). Replaces the old `Session 2CA1`
//! hex style — names are the display label only, uniqueness is carried by
//! the session UUID.

use rand::seq::SliceRandom;
use std::collections::HashSet;

const ADJECTIVES: &[&str] = &[
    "mossy", "golden", "quiet", "humble", "misty", "hollow", "wild", "still",
    "drizzly", "bright", "foggy", "warm", "ripe", "rustic", "autumn", "snug",
    "tucked", "sleepy", "dusty", "dappled", "lazy", "hushed", "rosy", "sunny",
    "breezy", "homely", "soft", "murky", "amber", "copper", "pale", "wispy",
    "earthy", "plush", "cedar", "gentle", "shy", "brisk", "ember", "frosted",
];

const NOUNS: &[&str] = &[
    "barn", "field", "clover", "hay", "orchard", "fox", "owl", "fern",
    "birch", "cedar", "meadow", "ridge", "cove", "dusk", "apple", "wheat",
    "maple", "pumpkin", "hearth", "kettle", "quilt", "lantern", "moss",
    "willow", "harvest", "thistle", "badger", "hare", "wren", "sparrow",
    "acorn", "pine", "creek", "hollow", "hedge", "pasture", "bramble",
    "cottage", "lark", "heather",
];

/// Pick a fresh `adjective-noun` name that isn't already in `used`.
/// Falls back to suffixing a digit after a handful of retries so we
/// never loop forever if the curated space gets crowded.
pub fn generate(used: &HashSet<String>) -> String {
    let mut rng = rand::thread_rng();
    for _ in 0..16 {
        let adj = ADJECTIVES.choose(&mut rng).unwrap();
        let noun = NOUNS.choose(&mut rng).unwrap();
        let name = format!("{adj}-{noun}");
        if !used.contains(&name) {
            return name;
        }
    }
    // Unlucky streak — pick one and append a disambiguating digit.
    let adj = ADJECTIVES.choose(&mut rng).unwrap();
    let noun = NOUNS.choose(&mut rng).unwrap();
    for n in 2..=999 {
        let name = format!("{adj}-{noun}-{n}");
        if !used.contains(&name) {
            return name;
        }
    }
    format!("{adj}-{noun}")
}
