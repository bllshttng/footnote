//! The yard's sprite table (x-b2bf): the f[no]nimals. 18 species x 3 frames
//! x 5 rows x 12 columns of plain monospace text - the same cell grid the
//! mux already renders, which is why there is no image pipeline to build.
//! The art here is ORIGINAL to this repo (the 3x5x12 cell-grid format and
//! the swapped-eye convention are the format; no prior sprite set's
//! drawings are copied).
//!
//! The rule that keeps a sprite honest: the eye is the ONLY status-carrying
//! cell, and it is computed at render time from the same badge/need values
//! the roster row already renders. Same body, different eye - a status
//! change never redraws the animal, and a sprite cannot disagree with its
//! row because there is no second lookup table to drift. Everything else on
//! the sprite (species, frame, hat) is an identity or flavour channel that
//! claims no truth and so cannot lie.

/// Sprite cell geometry, pinned by test against the table.
pub const SPRITE_W: usize = 12;
pub const SPRITE_H: usize = 5;
pub const FRAME_COUNT: usize = 3;
pub const SPECIES_COUNT: usize = 18;

pub const SPECIES_NAMES: [&str; SPECIES_COUNT] = [
    "cat", "crow", "frog", "fox", "whale", "bee", "bat", "crab", "yak", "moth", "sloth", "newt",
    "hare", "trout", "boar", "ram", "wren", "ibex",
];

/// frames[species][frame][row]. Original art for the fno agents yard, drawn
/// for this repo; the 3x5x12 cell-grid format is the only
/// inheritance. The eye cells carry `EYE_DEFAULT` ('·');
/// [`render_frame`] swaps every occurrence for the status eye.
pub const SPECIES_FRAMES: [[[&str; SPRITE_H]; FRAME_COUNT]; SPECIES_COUNT] = [
    // cat
    [
        [
            "            ",
            "   /\\ /\\    ",
            "  ( ·   ·)  ",
            "   (  ω )   ",
            "  (_)(_)    ",
        ],
        [
            "            ",
            "   /\\ /\\    ",
            "  ( ·   ·)  ",
            "   (  ω )   ",
            "  (_)(_)~   ",
        ],
        [
            "            ",
            "  /\\  /\\    ",
            "  ( ·   ·)  ",
            "   (  ω )~  ",
            "  (_)(_)    ",
        ],
    ],
    // crow
    [
        [
            "            ",
            "     \\/     ",
            "   <( ·)    ",
            "    /||\\    ",
            "   ~  ~     ",
        ],
        [
            "            ",
            "    \\/      ",
            "   <( ·)~   ",
            "    /||\\    ",
            "   ~  ~     ",
        ],
        [
            "            ",
            "     \\/     ",
            "    (· )>   ",
            "    /||\\    ",
            "     ~  ~   ",
        ],
    ],
    // frog
    [
        [
            "            ",
            "  _.._      ",
            " ( ·  · )   ",
            " ( ____ )   ",
            "  _    _    ",
        ],
        [
            "            ",
            "  _.._      ",
            " ( ·  · )   ",
            " ( ____ )   ",
            "  __   __   ",
        ],
        [
            "            ",
            "  -..-      ",
            " ( ·  - )   ",
            " ( ____ )   ",
            "  _    _    ",
        ],
    ],
    // fox
    [
        [
            "            ",
            "  /\\   /\\   ",
            " ( ·\\_/· )  ",
            "  > ~~~ <   ",
            "   \\___/    ",
        ],
        [
            "            ",
            "  /\\   /\\   ",
            " ( ·\\_/· )  ",
            "  > ~~~ >   ",
            "   \\___/    ",
        ],
        [
            "            ",
            "  /\\   /\\   ",
            " ( ·\\_/· )  ",
            "  < ~~~ <   ",
            "   \\___/~   ",
        ],
    ],
    // whale
    [
        [
            "            ",
            "   _____    ",
            "  ( · ___)  ",
            "   \\____\\   ",
            "    ~ ~ ~   ",
        ],
        [
            "            ",
            "   _____    ",
            "  ( · ___)  ",
            "   \\____\\   ",
            "   ~ ~ ~    ",
        ],
        [
            "            ",
            "  ______    ",
            " ( · ___ )  ",
            "  \\____\\    ",
            "    ~ ~ ~   ",
        ],
    ],
    // bee
    [
        [
            "            ",
            "  \\ /       ",
            " ( ·)@)     ",
            "  (   )     ",
            "   ~ ~      ",
        ],
        [
            "            ",
            "      \\ /   ",
            "   ((@ (·)  ",
            "    (   )   ",
            "     ~ ~    ",
        ],
        [
            "            ",
            "  \\ /       ",
            " ( ·)@)     ",
            "  (   )~    ",
            "   ~ ~      ",
        ],
    ],
    // bat
    [
        [
            "            ",
            " \\/     \\/  ",
            "  ( ·   · ) ",
            "   \\ --- /  ",
            "     ^ ^    ",
        ],
        [
            "            ",
            " \\_/\\ /\\_/  ",
            "  ( ·   · ) ",
            "   \\ --- /  ",
            "     ^ ^    ",
        ],
        [
            "            ",
            " \\/     \\/  ",
            "  ( ·   · ) ",
            "   / --- \\  ",
            "     ^ ^    ",
        ],
    ],
    // crab
    [
        [
            "            ",
            " \\        / ",
            "  ( ·    · )",
            "   ) ____ ( ",
            "  / /  \\ \\  ",
        ],
        [
            "            ",
            " \\        / ",
            "  ( ·    · )",
            "   ( ____ ) ",
            "  / /  \\ \\  ",
        ],
        [
            "            ",
            " /        \\ ",
            "  ( ·    · )",
            "   ) ____ ( ",
            "  \\ \\  / /  ",
        ],
    ],
    // yak
    [
        [
            "            ",
            "  ^^    ^^  ",
            " ( · \\_/ · )",
            "  ) ~~~~ (  ",
            "  /|    |\\  ",
        ],
        [
            "            ",
            "  ^^    ^^  ",
            " ( · \\_/ · )",
            "  ( ~~~~ )  ",
            "  /|    |\\  ",
        ],
        [
            "            ",
            "  ^^    ^^  ",
            " ( · \\_/ · )",
            "  ) ~~~~ (  ",
            "  /|    |\\~ ",
        ],
    ],
    // moth
    [
        [
            "            ",
            "  ^  /\\  ^  ",
            "\\( · || · )/",
            "  \\  ||  /  ",
            "     ~~     ",
        ],
        [
            "            ",
            "  ^  \\/  ^  ",
            "\\( · || · )/",
            "  \\  ||  /  ",
            "     ~~     ",
        ],
        [
            "            ",
            "  ^  /\\  ^  ",
            "\\( · || · )/",
            "   \\ || /   ",
            "     ~~     ",
        ],
    ],
    // sloth
    [
        [
            "            ",
            "    ____    ",
            "   ( ·  )   ",
            "  / o~~o \\  ",
            " (__)  (_)  ",
        ],
        [
            "            ",
            "    ____    ",
            "   ( ·  )~  ",
            "  / o~~o \\  ",
            " (__)  (_)  ",
        ],
        [
            "            ",
            "    ____    ",
            "   ( -  )   ",
            "  / o~~o \\  ",
            " (__)  (_)  ",
        ],
    ],
    // newt
    [
        [
            "            ",
            "      /\\    ",
            "  ~ (·  )   ",
            "  ~~ /||\\   ",
            "    ~  ~    ",
        ],
        [
            "            ",
            "      /\\    ",
            "  (·  ) ~   ",
            "   /||\\ ~~  ",
            "    ~  ~    ",
        ],
        [
            "            ",
            "      /\\    ",
            "  ~ (·  )   ",
            "   /||\\~    ",
            "   ~  ~     ",
        ],
    ],
    // hare
    [
        [
            "            ",
            "  ( \\ / )   ",
            " ( ·    · ) ",
            "  (  ..  )  ",
            "  (\")_(\")   ",
        ],
        [
            "            ",
            " ( | \\ | )  ",
            " ( ·    · ) ",
            "  (  ..  )  ",
            "  (\")_(\")   ",
        ],
        [
            "            ",
            "  ( \\ / )   ",
            " ( ·    · ) ",
            "  ( .  . )  ",
            "  (\")_(\")~  ",
        ],
    ],
    // trout
    [
        [
            "            ",
            "    <\\)     ",
            "  <( ·   }=<",
            "    </      ",
            "   ~ ~      ",
        ],
        [
            "            ",
            "     \\)>    ",
            "  >={  · )> ",
            "    >\\      ",
            "   ~ ~      ",
        ],
        [
            "            ",
            "    <\\)     ",
            "  <( ·   }=<",
            "    </      ",
            "     ~ ~    ",
        ],
    ],
    // boar
    [
        [
            "            ",
            "  ,,    ,,  ",
            " ( · \\ / · )",
            "  ( ~~~~ )  ",
            "  /|    |\\  ",
        ],
        [
            "            ",
            "  ,,    ,,  ",
            " ( · \\ / · )",
            "  ( ~~~~ )~ ",
            "  /|    |\\  ",
        ],
        [
            "            ",
            " ,,      ,, ",
            " ( · \\ / · )",
            "  ( ~~~~ )  ",
            "  /|    |\\  ",
        ],
    ],
    // ram
    [
        [
            "            ",
            " @ @        ",
            " ( · \\_/ · )",
            "  ) ~~~~ (  ",
            "   |    |   ",
        ],
        [
            "            ",
            "  @ @       ",
            " ( · \\_/ · )",
            "  ( ~~~~ )  ",
            "   |    |   ",
        ],
        [
            "            ",
            " @ @        ",
            " ( · \\_/ · )",
            "  ) ~~~~ (  ",
            "   |  | |   ",
        ],
    ],
    // wren
    [
        [
            "            ",
            "    ><      ",
            "   <(· )    ",
            "    /||\\    ",
            "   ~~       ",
        ],
        [
            "            ",
            "    ><      ",
            "  ~ <(· )   ",
            "    /||\\    ",
            "     ~~     ",
        ],
        [
            "            ",
            "     ><     ",
            "   <(· )~   ",
            "    /||\\    ",
            "   ~~       ",
        ],
    ],
    // ibex
    [
        [
            "            ",
            " \\\\   //    ",
            "  ( ·  )    ",
            "  /~~~~\\    ",
            "   |  |(\\   ",
        ],
        [
            "            ",
            " \\\\   //    ",
            "  ( ·  )~   ",
            "  /~~~~\\    ",
            "   |  |(\\   ",
        ],
        [
            "            ",
            "  \\\\ //     ",
            "  ( ·  )    ",
            "  /~~~~\\    ",
            "   |  | )\\  ",
        ],
    ],
];

/// The eye set, in binding order: every variant's glyph is the ONE
/// status-carrying cell of a sprite. `Sick` (a stale CI verdict) has no
/// producer yet - the CI verdict is a network read the client does not hold -
/// so nothing constructs it today; it is carried so the ordering contract has
/// a home when its producer lands. `Reserved` (no reading at all) IS produced
/// (a row with no badge and no joined need) and renders its hollow glyph
/// rather than a dimming pass the plain-text grid does not have.
pub const EYES: [char; 6] = ['\u{b7}', '\u{2726}', '\u{d7}', '\u{25c9}', '@', '\u{b0}'];
/// The default eye: what a Working, no-need citizen wears, and the glyph the
/// table's frames embed at their eye cells.
pub const EYE_DEFAULT: char = '\u{b7}';

/// A sprite's status reading, derived at render time from the row's own
/// badge/need values (see [`crate::yard_overlay`]). Declaration order IS the
/// [`EYES`] binding order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eye {
    /// Working, nothing owed: the default body eye, no swap.
    Working,
    /// A gift is waiting: a PR is open on the row (`AgentRow.pr`).
    Gift,
    /// Sick: a stale/failed CI read. NO PRODUCER YET - reserved.
    Sick,
    /// No reading: never rendered as a content state (dimmed or absent).
    Reserved,
    /// The row wants the operator: `Decision` / `MailQuestion` / blocked.
    Attention,
    /// Wedged review or budget stop: faded, "no reading yet" rather than
    /// content.
    Faded,
}

impl Eye {
    pub fn glyph(self) -> char {
        EYES[self as usize]
    }
}

/// The only grounded hat: `crown_level >= 1` wears it (registry crown fields,
/// stamped by the spawn path - never self-declared). The reference's other
/// six hats key to an executor mapping not yet verified stable and stay
/// unrendered; a hat with no reading is the decorative-guard failure the
/// yard exists to refuse.
pub const HAT_CROWN: &str = "   \\^^^/    ";

// No rarity table here: the tier names ride the `fno agents yard --json` payload as
// strings, so Python's `fno.yard.RARITY_TIERS` is the one live copy. A Rust
// mirror would be a second tuple to keep in lockstep for nothing.

pub fn species_name(species: usize) -> &'static str {
    SPECIES_NAMES[species % SPECIES_COUNT]
}

/// One frame with every eye cell swapped for `eye`. Same body, different eye:
/// a status change is always the same cat, so the sprite can never render a
/// state the row does not carry.
pub fn render_frame(species: usize, frame: usize, eye: Eye) -> [String; SPRITE_H] {
    let frame = SPECIES_FRAMES[species % SPECIES_COUNT][frame % FRAME_COUNT];
    let glyph = eye.glyph();
    let mut rows: [String; SPRITE_H] = Default::default();
    for (i, row) in frame.iter().enumerate() {
        rows[i] = row.replace(EYE_DEFAULT, &glyph.to_string());
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vendoring contract: every row of every frame of every species is
    /// exactly the pinned geometry. A transcription slip fails here, not in a
    /// rendering blit six screens away.
    #[test]
    fn table_holds_its_geometry() {
        for species in SPECIES_FRAMES.iter() {
            for frame in species.iter() {
                assert_eq!(frame.len(), SPRITE_H);
                for row in frame.iter() {
                    assert_eq!(row.chars().count(), SPRITE_W, "row {row:?}");
                }
            }
        }
        assert_eq!(SPECIES_NAMES.len(), SPECIES_COUNT);
        assert_eq!(SPECIES_FRAMES.len(), SPECIES_COUNT);
    }

    /// Every species carries at least one eye cell; the swap is what makes
    /// the eye the row's own reading.
    #[test]
    fn every_species_has_eye_cells() {
        for (name, species) in SPECIES_NAMES.iter().zip(SPECIES_FRAMES.iter()) {
            let dots: usize = species
                .iter()
                .map(|f| {
                    f.iter()
                        .map(|r| r.chars().filter(|c| *c == EYE_DEFAULT).count())
                        .sum::<usize>()
                })
                .sum();
            assert!(dots > 0, "{name} has no eye cells to swap");
        }
    }

    #[test]
    fn render_swaps_every_eye_and_keeps_width() {
        let rows = render_frame(0, 0, Eye::Gift); // cat
                                                  // The eye cells live on the cat's face row, not every row.
        assert!(rows.iter().any(|r| r.contains('\u{2726}')));
        assert!(rows.iter().all(|r| !r.contains(EYE_DEFAULT)));
        for r in rows.iter() {
            assert_eq!(r.chars().count(), SPRITE_W);
        }
    }

    #[test]
    fn working_eye_is_a_no_op_swap() {
        let plain = render_frame(0, 0, Eye::Working);
        assert_eq!(plain[2], SPECIES_FRAMES[0][0][2]);
    }

    #[test]
    fn eye_glyphs_bind_in_order() {
        assert_eq!(Eye::Working.glyph(), '\u{b7}');
        assert_eq!(Eye::Gift.glyph(), '\u{2726}');
        assert_eq!(Eye::Sick.glyph(), '\u{d7}');
        assert_eq!(Eye::Reserved.glyph(), '\u{25c9}');
        assert_eq!(Eye::Attention.glyph(), '@');
        assert_eq!(Eye::Faded.glyph(), '\u{b0}');
    }
}
