//! Phonemizer against the real `espeak-ng` binary.

use se_tts::phonemize::{Espeak, chunk, phonemize, tokens};
use se_tts::text::normalize;
use se_tts::vocab;

fn ps(text: &str, lang: &str) -> String {
    phonemize(&Espeak::default(), &normalize(text, 0), lang).expect("espeak-ng installed")
}

fn all_in_vocab(s: &str) -> bool {
    s.chars().all(vocab::contains)
}

#[test]
fn punctuation_is_preserved_between_clauses() {
    let p = ps("Thanks for the five hundred bits, let's go!", "en-us");
    assert!(all_in_vocab(&p), "{p}");
    let (a, b) = p.split_once(", ").unwrap_or_else(|| panic!("comma kept: {p}"));
    assert!(a.starts_with("θˈæŋks") && a.ends_with("bˈɪts"), "{p}");
    assert!(b.starts_with("lˈɛts") && b.ends_with('!'), "{p}");
    assert!(!p.contains('^') && !p.contains('\u{200D}'), "ties stripped: {p}");

    let p = ps("“Wait…” she said (quietly): no; really?", "en-us");
    for m in ['“', '”', '…', '(', ')', ':', ';', '?'] {
        assert!(p.contains(m), "{m} kept in {p}");
    }
    // decimals and clock times are not clause breaks
    let p = ps("It is 2.5 at 10:30.", "en-us");
    assert_eq!(p.matches('.').count(), 1, "{p}");
    assert!(p.contains("pYnt"), "point spoken: {p}");
}

#[test]
fn output_is_filtered_to_the_kokoro_vocabulary() {
    for (text, lang) in [
        ("Ünïcödé naïve café — über-cool ¿sí? «quoted» [[h@'loU]] 😀 x²", "en-us"),
        ("Loch Ness, the bothy and the rhythm.", "en-gb"),
        ("¿Cómo estás? ¡Muy bien, gracias!", "es"),
        ("Привет мир", "en-us"),
    ] {
        let p = ps(text, lang);
        assert!(!p.is_empty(), "{text}");
        assert!(all_in_vocab(&p), "{text} → {p}");
        assert!(!tokens(&p).contains(&vocab::PAD), "pad id never produced by text");
    }
    // espeak raw phoneme input never reaches espeak: brackets are folded away first
    let p = ps("say [[h@'loU]]", "en-us");
    assert!(!p.contains("həlˈO"), "{p}");
    // American and British English differ (misaki mapping per variant)
    let (us, gb) = (ps("go home", "en-us"), ps("go home", "en-gb"));
    assert!(us.contains("hˈOm") && !us.contains('Q'), "{us}");
    assert!(gb.contains("hˈQm") && !gb.contains('O'), "{gb}");
}

#[test]
fn long_text_chunks_at_clause_boundaries() {
    let sentence = "The drums were loud tonight, the lights were bright, and everyone in chat was having a great time";
    let text = std::iter::repeat_n(sentence, 12).collect::<Vec<_>>().join(". ") + ".";
    let p = ps(&text, "en-us");
    assert!(p.chars().count() > 2 * vocab::MAX_TOKENS, "{} phonemes", p.chars().count());
    let chunks = chunk(&p, vocab::MAX_TOKENS);
    assert!(chunks.len() >= 3);
    for c in &chunks {
        let n = tokens(c).len();
        assert!(n <= vocab::MAX_TOKENS && n > vocab::MAX_TOKENS / 5, "{n} tokens");
        assert!(c.ends_with('.'), "chunk ends at a sentence: …{}", c.chars().rev().take(20).collect::<String>().chars().rev().collect::<String>());
    }
    assert_eq!(chunks.join(" "), p, "no phoneme lost or duplicated");
}

#[test]
fn espeak_line_splitting_falls_back_per_segment() {
    // one clause long enough for espeak to split it itself, followed by a short one: the
    // per-segment fallback must keep the second clause aligned with its comma
    let long = (0..150).map(|i| ["drum", "bass", "snare"][i % 3]).collect::<Vec<_>>().join(" ");
    let p = ps(&format!("{long}, cymbal!"), "en-us");
    let tail: String = p.chars().rev().take(30).collect::<Vec<_>>().into_iter().rev().collect();
    assert!(tail.ends_with(", sˈɪmbᵊl!"), "…{tail}");
    assert_eq!(p.matches("dɹˈʌm").count(), 50, "every word phonemized once");
}

#[test]
fn missing_espeak_is_an_error() {
    let e = Espeak::with_program("/nonexistent/espeak-ng");
    assert!(e.version().is_err());
    assert!(phonemize(&e, "hello", "en-us").is_err());
    assert!(phonemize(&Espeak::default(), "hello", "--help").is_err(), "language is validated");
    assert!(Espeak::default().version().unwrap().starts_with("1."));
}
