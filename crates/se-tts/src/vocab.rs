//! Kokoro-82M v1.0 phoneme vocabulary: exactly the `vocab` map of `config.json` in
//! hexgrad/Kokoro-82M @ f3ff3571791e39611d31c381e3a41a3af07b4987 (the ONNX export's
//! `tokenizer.json` is identical plus `$` = 0, the pad id placed at both ends of `input_ids`).

/// Pad / boundary token id.
pub const PAD: i64 = 0;

/// Maximum phoneme tokens per inference (the model's context is 512 incl. the two pads).
pub const MAX_TOKENS: usize = 510;

/// `(phoneme, token id)`, sorted by id.
pub const VOCAB: &[(char, i64)] = &[
    (';', 1),
    (':', 2),
    (',', 3),
    ('.', 4),
    ('!', 5),
    ('?', 6),
    ('—', 9),
    ('…', 10),
    ('"', 11),
    ('(', 12),
    (')', 13),
    ('“', 14),
    ('”', 15),
    (' ', 16),
    ('\u{303}', 17),
    ('ʣ', 18),
    ('ʥ', 19),
    ('ʦ', 20),
    ('ʨ', 21),
    ('ᵝ', 22),
    ('ꭧ', 23),
    ('A', 24),
    ('I', 25),
    ('O', 31),
    ('Q', 33),
    ('S', 35),
    ('T', 36),
    ('W', 39),
    ('Y', 41),
    ('ᵊ', 42),
    ('a', 43),
    ('b', 44),
    ('c', 45),
    ('d', 46),
    ('e', 47),
    ('f', 48),
    ('h', 50),
    ('i', 51),
    ('j', 52),
    ('k', 53),
    ('l', 54),
    ('m', 55),
    ('n', 56),
    ('o', 57),
    ('p', 58),
    ('q', 59),
    ('r', 60),
    ('s', 61),
    ('t', 62),
    ('u', 63),
    ('v', 64),
    ('w', 65),
    ('x', 66),
    ('y', 67),
    ('z', 68),
    ('ɑ', 69),
    ('ɐ', 70),
    ('ɒ', 71),
    ('æ', 72),
    ('β', 75),
    ('ɔ', 76),
    ('ɕ', 77),
    ('ç', 78),
    ('ɖ', 80),
    ('ð', 81),
    ('ʤ', 82),
    ('ə', 83),
    ('ɚ', 85),
    ('ɛ', 86),
    ('ɜ', 87),
    ('ɟ', 90),
    ('ɡ', 92),
    ('ɥ', 99),
    ('ɨ', 101),
    ('ɪ', 102),
    ('ʝ', 103),
    ('ɯ', 110),
    ('ɰ', 111),
    ('ŋ', 112),
    ('ɳ', 113),
    ('ɲ', 114),
    ('ɴ', 115),
    ('ø', 116),
    ('ɸ', 118),
    ('θ', 119),
    ('œ', 120),
    ('ɹ', 123),
    ('ɾ', 125),
    ('ɻ', 126),
    ('ʁ', 128),
    ('ɽ', 129),
    ('ʂ', 130),
    ('ʃ', 131),
    ('ʈ', 132),
    ('ʧ', 133),
    ('ʊ', 135),
    ('ʋ', 136),
    ('ʌ', 138),
    ('ɣ', 139),
    ('ɤ', 140),
    ('χ', 142),
    ('ʎ', 143),
    ('ʒ', 147),
    ('ʔ', 148),
    ('ˈ', 156),
    ('ˌ', 157),
    ('ː', 158),
    ('ʰ', 162),
    ('ʲ', 164),
    ('↓', 169),
    ('→', 171),
    ('↗', 172),
    ('↘', 173),
    ('ᵻ', 177),
];

/// Token id of a phoneme character, `None` when Kokoro has no such symbol.
pub fn id(c: char) -> Option<i64> {
    VOCAB.iter().find(|(p, _)| *p == c).map(|(_, i)| *i)
}

pub fn contains(c: char) -> bool {
    id(c).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_matches_upstream_shape() {
        assert_eq!(VOCAB.len(), 114);
        assert!(VOCAB.windows(2).all(|w| w[0].1 < w[1].1), "ids strictly increasing");
        let mut chars: Vec<char> = VOCAB.iter().map(|(c, _)| *c).collect();
        chars.sort_unstable();
        chars.dedup();
        assert_eq!(chars.len(), VOCAB.len(), "no duplicate phonemes");
        assert!(VOCAB.iter().all(|(_, i)| *i != PAD), "pad id is reserved");
        assert_eq!(id(' '), Some(16));
        assert_eq!(id('ˈ'), Some(156));
        assert_eq!(id('ɹ'), Some(123));
        assert_eq!(id('-'), None);
        assert_eq!(id('^'), None);
    }
}
