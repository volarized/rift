//! The trigram rule the regex prefilter shares with FTS5's `trigram` tokenizer.
//!
//! The trigram index holds every run of three characters of the stored text, each
//! character case-folded as the tokenizer folds it (`fts5TriTokenize`,
//! `sqlite3.c:266923-266990` in `libsqlite3-sys` 0.38.2). A formula built from these
//! trigrams reads the same against the index and against a text's own trigrams.

use std::collections::BTreeSet;

/// The FTS5 tokenizer string of the trigram index.
///
/// Its defaults are the ones [`fold`] ports: case folding on (`bFold = 1`) and
/// `remove_diacritics` off (`iFoldParam = 0`), `sqlite3.c:266884-266885`. Case folding
/// makes the index's candidates a superset for a case-sensitive pattern, and a
/// case-insensitive one needs no second index.
pub const TRIGRAM_TOKENIZER: &str = "trigram";

/// One fold rule of `sqlite3Fts5UnicodeFold` (`sqlite3.c:267188`): the first
/// code point, the flags byte, and the range length.
type FoldRule = (u16, u8, u8);

/// `aEntry[]` of `sqlite3Fts5UnicodeFold`, copied in order.
const FOLD_RULES: [FoldRule; 163] = [
    (65, 14, 26),
    (181, 64, 1),
    (192, 14, 23),
    (216, 14, 7),
    (256, 1, 48),
    (306, 1, 6),
    (313, 1, 16),
    (330, 1, 46),
    (376, 116, 1),
    (377, 1, 6),
    (383, 104, 1),
    (385, 50, 1),
    (386, 1, 4),
    (390, 44, 1),
    (391, 0, 1),
    (393, 42, 2),
    (395, 0, 1),
    (398, 32, 1),
    (399, 38, 1),
    (400, 40, 1),
    (401, 0, 1),
    (403, 42, 1),
    (404, 46, 1),
    (406, 52, 1),
    (407, 48, 1),
    (408, 0, 1),
    (412, 52, 1),
    (413, 54, 1),
    (415, 56, 1),
    (416, 1, 6),
    (422, 60, 1),
    (423, 0, 1),
    (425, 60, 1),
    (428, 0, 1),
    (430, 60, 1),
    (431, 0, 1),
    (433, 58, 2),
    (435, 1, 4),
    (439, 62, 1),
    (440, 0, 1),
    (444, 0, 1),
    (452, 2, 1),
    (453, 0, 1),
    (455, 2, 1),
    (456, 0, 1),
    (458, 2, 1),
    (459, 1, 18),
    (478, 1, 18),
    (497, 2, 1),
    (498, 1, 4),
    (502, 122, 1),
    (503, 134, 1),
    (504, 1, 40),
    (544, 110, 1),
    (546, 1, 18),
    (570, 70, 1),
    (571, 0, 1),
    (573, 108, 1),
    (574, 68, 1),
    (577, 0, 1),
    (579, 106, 1),
    (580, 28, 1),
    (581, 30, 1),
    (582, 1, 10),
    (837, 36, 1),
    (880, 1, 4),
    (886, 0, 1),
    (902, 18, 1),
    (904, 16, 3),
    (908, 26, 1),
    (910, 24, 2),
    (913, 14, 17),
    (931, 14, 9),
    (962, 0, 1),
    (975, 4, 1),
    (976, 140, 1),
    (977, 142, 1),
    (981, 146, 1),
    (982, 144, 1),
    (984, 1, 24),
    (1008, 136, 1),
    (1009, 138, 1),
    (1012, 130, 1),
    (1013, 128, 1),
    (1015, 0, 1),
    (1017, 152, 1),
    (1018, 0, 1),
    (1021, 110, 3),
    (1024, 34, 16),
    (1040, 14, 32),
    (1120, 1, 34),
    (1162, 1, 54),
    (1216, 6, 1),
    (1217, 1, 14),
    (1232, 1, 88),
    (1329, 22, 38),
    (4256, 66, 38),
    (4295, 66, 1),
    (4301, 66, 1),
    (7680, 1, 150),
    (7835, 132, 1),
    (7838, 96, 1),
    (7840, 1, 96),
    (7944, 150, 8),
    (7960, 150, 6),
    (7976, 150, 8),
    (7992, 150, 8),
    (8008, 150, 6),
    (8025, 151, 8),
    (8040, 150, 8),
    (8072, 150, 8),
    (8088, 150, 8),
    (8104, 150, 8),
    (8120, 150, 2),
    (8122, 126, 2),
    (8124, 148, 1),
    (8126, 100, 1),
    (8136, 124, 4),
    (8140, 148, 1),
    (8152, 150, 2),
    (8154, 120, 2),
    (8168, 150, 2),
    (8170, 118, 2),
    (8172, 152, 1),
    (8184, 112, 2),
    (8186, 114, 2),
    (8188, 148, 1),
    (8486, 98, 1),
    (8490, 92, 1),
    (8491, 94, 1),
    (8498, 12, 1),
    (8544, 8, 16),
    (8579, 0, 1),
    (9398, 10, 26),
    (11264, 22, 47),
    (11360, 0, 1),
    (11362, 88, 1),
    (11363, 102, 1),
    (11364, 90, 1),
    (11367, 1, 6),
    (11373, 84, 1),
    (11374, 86, 1),
    (11375, 80, 1),
    (11376, 82, 1),
    (11378, 0, 1),
    (11381, 0, 1),
    (11390, 78, 2),
    (11392, 1, 100),
    (11499, 1, 4),
    (11506, 0, 1),
    (42560, 1, 46),
    (42624, 1, 24),
    (42786, 1, 14),
    (42802, 1, 62),
    (42873, 1, 4),
    (42877, 76, 1),
    (42878, 1, 10),
    (42891, 0, 1),
    (42893, 74, 1),
    (42896, 1, 4),
    (42912, 1, 10),
    (42922, 72, 1),
    (65313, 14, 26),
];

/// `aiOff[]` of `sqlite3Fts5UnicodeFold`, copied in order.
const FOLD_OFFSETS: [u16; 77] = [
    1, 2, 8, 15, 16, 26, 28, 32, 37, 38, 40, 48, 63, 64, 69, 71, 79, 80, 116, 202, 203, 205, 206,
    207, 209, 210, 211, 213, 214, 217, 218, 219, 775, 7264, 10792, 10795, 23228, 23256, 30204,
    54721, 54753, 54754, 54756, 54787, 54793, 54809, 57153, 57274, 57921, 58019, 58363, 61722,
    65268, 65341, 65373, 65406, 65408, 65410, 65415, 65424, 65436, 65439, 65450, 65462, 65472,
    65476, 65478, 65480, 65482, 65488, 65506, 65511, 65514, 65521, 65527, 65528, 65529,
];

/// The code point past which `sqlite3Fts5UnicodeFold` reads [`FOLD_RULES`] instead of
/// folding ASCII directly.
const ASCII_END: u32 = 128;

/// The Deseret capitals, which `sqlite3Fts5UnicodeFold` folds by [`DESERET_OFFSET`] outside
/// its 16-bit table.
const DESERET_CAPITALS: std::ops::Range<u32> = 66_560..66_600;

/// What a Deseret capital adds to reach its small letter.
const DESERET_OFFSET: u32 = 40;

/// The flag bit saying a rule folds every second code point of its range alone.
const EVERY_SECOND_FLAG: u8 = 0x01;

/// Folds one character as FTS5's `trigram` tokenizer does, with fold parameter 0:
/// `sqlite3Fts5UnicodeFold(c, 0)`, `sqlite3.c:267188-267322`.
///
/// Characters newer than FTS5's Unicode tables keep their spelling, so a pair such as
/// Georgian `ა` and `Ა` folds to two characters here and in the index alike.
#[must_use]
pub fn fold(character: char) -> char {
    let code = u32::from(character);
    if code < ASCII_END {
        return character.to_ascii_lowercase();
    }
    if DESERET_CAPITALS.contains(&code) {
        return char::from_u32(code + DESERET_OFFSET).unwrap_or(character);
    }
    let Ok(code) = u16::try_from(code) else {
        return character;
    };
    let index = FOLD_RULES.partition_point(|&(first, _, _)| first <= code);
    let Some(&(first, flags, range)) = index.checked_sub(1).and_then(|at| FOLD_RULES.get(at))
    else {
        return character;
    };
    let within = u32::from(code) < u32::from(first) + u32::from(range);
    let every_second_skips = u16::from(flags & EVERY_SECOND_FLAG) & (first ^ code) != 0;
    if !within || every_second_skips {
        return character;
    }
    let offset = FOLD_OFFSETS[usize::from(flags >> 1)];
    char::from_u32(u32::from(code.wrapping_add(offset))).unwrap_or(character)
}

/// Every trigram of `text`, folded as the index folds it, in text order.
///
/// The tokenizer skips a character that folds to U+0000 (`sqlite3.c:266949`, `:266974`),
/// so this rule skips it too.
#[must_use]
pub fn trigrams(text: &str) -> Vec<String> {
    let folded: Vec<char> = text.chars().map(fold).filter(|&c| c != '\0').collect();
    folded
        .windows(3)
        .map(|window| window.iter().collect())
        .collect()
}

/// The distinct trigrams of `text`, as a set a formula is read against.
#[must_use]
pub fn trigram_set(text: &str) -> BTreeSet<String> {
    trigrams(text).into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::{fold, trigram_set, trigrams};

    #[test]
    fn ascii_and_latin_fold_to_lowercase() {
        assert_eq!(fold('A'), 'a');
        assert_eq!(fold('z'), 'z');
        assert_eq!(fold('É'), 'é');
        assert_eq!(fold('('), '(');
    }

    #[test]
    fn folds_follow_the_ported_table() {
        assert_eq!(fold('\u{017F}'), 's', "long s");
        assert_eq!(fold('\u{212A}'), 'k', "Kelvin sign");
        assert_eq!(fold('Ω'), 'ω');
        assert_eq!(fold('Ж'), 'ж');
        assert_eq!(fold('\u{10400}'), '\u{10428}', "Deseret");
        assert_eq!(fold('\u{1F600}'), '\u{1F600}', "past the 16-bit table");
        assert_eq!(fold('Ა'), 'Ა', "Mtavruli is newer than the table");
        assert_eq!(
            fold('ᲀ'),
            'ᲀ',
            "Cyrillic small rounded ve is newer than the table"
        );
    }

    #[test]
    fn every_second_code_point_rules_skip_the_other() {
        assert_eq!(fold('\u{0100}'), '\u{0101}');
        assert_eq!(fold('\u{0101}'), '\u{0101}');
    }

    #[test]
    fn fold_is_idempotent() {
        for character in (0..=0x10_FFFF).filter_map(char::from_u32) {
            assert_eq!(fold(fold(character)), fold(character), "{character:?}");
        }
    }

    #[test]
    fn trigrams_fold_and_skip_nul() {
        assert_eq!(trigrams("AbCd"), ["abc", "bcd"]);
        assert_eq!(trigrams("a\0bc"), ["abc"]);
        assert!(trigrams("ab").is_empty());
        assert_eq!(trigram_set("abab").len(), 2);
    }
}
