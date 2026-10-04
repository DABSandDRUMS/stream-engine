//! YouTube video title + channel name → song title, artist and the (artist, title) pairs worth
//! asking MusicBrainz about. Pure text work; nothing here touches the network or the video.
//!
//! Handles the usual upload shapes: `Artist - Title (Official Video)`, en/em dashes,
//! `Artist "Title"`, `Artist: Title [OFFICIAL VIDEO]`, `Title | Artist`, feat./ft. credits,
//! `(Lyrics)`/`[HD]`/`(4K Remaster)` noise, `#shorts`, `Artist - Topic` and `ArtistVEVO`
//! channels, and the reversed `Title - Artist` (tried second).

use crate::text;

/// Best-effort reading of a video title.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Parsed {
    /// Artist when the upload says so clearly (`Artist - Title`, `Artist "Title"`,
    /// `Artist: Title`, a `- Topic`/VEVO channel). Weak hints (a plain channel name, `A | B`)
    /// only become guesses unless the channel agrees.
    pub artist: Option<String>,
    /// The song title with video noise removed.
    pub title: String,
    /// (artist, title) pairs to try against MusicBrainz, most likely first.
    pub guesses: Vec<Guess>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Guess {
    pub artist: String,
    pub title: String,
}

/// At most this many lookups per video (each costs one rate-limited request).
pub const MAX_GUESSES: usize = 3;

/// Words that mark bracket contents as describing the upload rather than the song.
const NOISE_WORDS: &[&str] = &[
    "official",
    "video",
    "audio",
    "lyric",
    "lyrics",
    "letra",
    "visualizer",
    "visualiser",
    "mv",
    "m v",
    "hd",
    "hq",
    "4k",
    "8k",
    "1080p",
    "720p",
    "remaster",
    "remastered",
    "remasterizado",
    "live",
    "explicit",
    "clean",
    "uncensored",
    "dirty",
    "version",
    "edit",
    "ost",
    "soundtrack",
    "feat",
    "ft",
    "featuring",
    "with",
    "prod",
    "karaoke",
    "instrumental",
    "acoustic",
    "remix",
    "cover",
    "legendado",
    "subtitulado",
    "traducida",
    "sub",
    "color coded",
    "from",
    "music",
    "clip",
    "teaser",
    "trailer",
    "performance",
    "session",
    "single",
    "album",
    "extended",
    "radio",
];

/// Trailing unbracketed phrases (lowercase) that are noise.
const TRAILING_NOISE: &[&str] = &[
    "official music video",
    "official lyric video",
    "official lyrics video",
    "official visualizer",
    "official video",
    "official audio",
    "official mv",
    "music video",
    "lyric video",
    "lyrics video",
    "with lyrics",
    "lyrics",
    "audio only",
    "full song",
    "mv",
    "m/v",
    "hd",
    "hq",
    "4k",
    "official",
];

/// Parse a video title (and its channel name) into a song title, artist and lookup guesses.
pub fn parse(video_title: &str, channel: &str) -> Parsed {
    let clean = clean(video_title);
    let (ch_artist, ch_strong) = channel_artist(channel);
    let mut guesses: Vec<Guess> = Vec::new();
    let mut push = |artist: &str, title: &str| {
        let (artist, title) = (tidy_artist(artist), tidy_title(title));
        if artist.is_empty() || title.is_empty() || guesses.len() >= MAX_GUESSES {
            return;
        }
        if !guesses.iter().any(|g| norm(&g.artist) == norm(&artist) && norm(&g.title) == norm(&title)) {
            guesses.push(Guess { artist, title });
        }
    };
    let (artist, title) = match split(&clean) {
        Some(Split { left: l, right: r, strong }) => {
            let names = |side: &str| ch_artist.as_deref().is_some_and(|c| same_artist(c, side));
            // `- Topic` uploads carry the bare release title (`Song - Live in Paris`): when
            // the channel names neither side, the dash belongs to the title
            let topic = channel.trim_end().ends_with(" - Topic") && ch_strong && !names(&l) && !names(&r);
            if topic && let Some(c) = &ch_artist {
                push(c, &clean);
            }
            // `Title - Artist`: trust the channel when it names the right-hand side
            let reversed = names(&r) && !names(&l);
            let (a, t) = if reversed { (&r, &l) } else { (&l, &r) };
            push(a, t);
            push(t, a);
            if ch_strong
                && !names(&l)
                && !names(&r)
                && let Some(c) = &ch_artist
            {
                push(c, &clean);
            }
            // pipes and glued dashes only name the artist when the channel agrees
            if strong && !topic || names(a) {
                (Some(tidy_artist(a)).filter(|a| !a.is_empty()), tidy_title(t))
            } else {
                (ch_artist.filter(|_| ch_strong), tidy_title(&clean))
            }
        }
        None => {
            if let Some(c) = &ch_artist {
                push(c, &clean);
            }
            (ch_artist.filter(|_| ch_strong), tidy_title(&clean))
        }
    };
    let title = if title.is_empty() { text::display(video_title, 200) } else { title };
    Parsed { artist, title, guesses }
}

/// Comparison form: folded (case, punctuation, width), Latin diacritics removed, `and`/`&`
/// and a leading `the` dropped.
pub fn norm(s: &str) -> String {
    let folded: String = text::fold(s).chars().map(plain).collect();
    let mut out = String::with_capacity(folded.len());
    for (i, w) in folded.split_whitespace().enumerate() {
        if w == "and" || (i == 0 && w == "the") {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    out
}

/// Title text without any bracketed part, feat. credit or ` - suffix` (not normalized).
pub fn core_text(s: &str) -> String {
    let no_brackets = strip_brackets(&dashes(s), |_| true);
    let cut = cut_feat(&no_brackets);
    cut.split(" - ").next().unwrap_or("").trim().to_string()
}

/// [`core_text`], normalized.
pub fn core(s: &str) -> String {
    norm(&core_text(s))
}

/// Same artist, allowing one name to contain the other (`Queen` ⊂ `Queen Official`) and
/// ignoring spaces (`foofighters` from a VEVO channel = `Foo Fighters`).
pub fn same_artist(a: &str, b: &str) -> bool {
    let (a, b) = (norm(a), norm(b));
    !a.is_empty() && !b.is_empty() && (a == b || text::contains_phrase(&a, &b) || text::contains_phrase(&b, &a) || a.replace(' ', "") == b.replace(' ', ""))
}

/// Main artist of a credit like `Post Malone, Swae Lee` / `Marshmello x Bastille`.
pub fn primary_artist(s: &str) -> &str {
    let lower = s.to_ascii_lowercase();
    let cut = [", ", " & ", " x ", " × ", " vs. ", " vs ", " and ", " with "]
        .iter()
        .filter_map(|sep| lower.find(sep))
        .filter(|&i| i > 0)
        .min()
        .unwrap_or(s.len());
    s[..cut].trim()
}

// ---- cleaning ------------------------------------------------------------------------------

/// Latin letters with diacritics → plain ASCII (only the common ones; other scripts unchanged).
fn plain(c: char) -> char {
    match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => 'a',
        'ç' | 'ć' | 'č' => 'c',
        'ď' | 'đ' => 'd',
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ė' | 'ę' | 'ě' => 'e',
        'ğ' => 'g',
        'ì' | 'í' | 'î' | 'ï' | 'ī' | 'ı' => 'i',
        'ł' | 'ľ' => 'l',
        'ñ' | 'ń' | 'ň' => 'n',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ő' => 'o',
        'ř' => 'r',
        'ś' | 'š' | 'ş' => 's',
        'ť' => 't',
        'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' | 'ű' => 'u',
        'ý' | 'ÿ' => 'y',
        'ź' | 'ż' | 'ž' => 'z',
        other => other,
    }
}

/// Dash lookalikes → `-`; whitespace collapsed; `Artist– Title` / `Artist -Title` spacing
/// repaired (glued dashes like `blink-182` stay).
fn dashes(s: &str) -> String {
    let mapped: String = s
        .chars()
        .map(|c| match c {
            '–' | '—' | '―' | '‒' | '−' | '‐' | '‑' | '~' => '-',
            c => c,
        })
        .collect();
    let out = collapse(&mapped).replace(" -- ", " - ");
    let b = out.as_bytes();
    let mut fixed = String::with_capacity(out.len() + 2);
    for (i, ch) in out.char_indices() {
        if ch == '-' && i > 0 && i + 1 < b.len() {
            let before = b[i - 1] == b' ';
            let after = b[i + 1] == b' ';
            if before != after {
                if !before {
                    fixed.push(' ');
                }
                fixed.push('-');
                if !after {
                    fixed.push(' ');
                }
                continue;
            }
        }
        fixed.push(ch);
    }
    fixed
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn closer(open: char) -> Option<char> {
    match open {
        '(' => Some(')'),
        '[' => Some(']'),
        '{' => Some('}'),
        '【' => Some('】'),
        '〔' => Some('〕'),
        _ => None,
    }
}

/// Remove bracket groups whose content satisfies `drop` (unbalanced brackets are kept).
fn strip_brackets(s: &str, drop: impl Fn(&str) -> bool) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if let Some(close) = closer(c) {
            let mut depth = 0usize;
            let mut end = None;
            for (j, &d) in chars.iter().enumerate().skip(i) {
                if d == c {
                    depth += 1;
                } else if d == close {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(j);
                        break;
                    }
                }
            }
            if let Some(j) = end {
                let inner: String = chars[i + 1..j].iter().collect();
                if drop(&inner) {
                    i = j + 1;
                    continue;
                }
            }
        }
        out.push(c);
        i += 1;
    }
    collapse(&out)
}

/// Does bracket content describe the upload (official video, lyrics, 4K, feat. …, a year)?
fn is_noise(inner: &str) -> bool {
    let f = norm(inner);
    if f.is_empty() {
        return true;
    }
    if f.split(' ').all(|w| w.len() == 4 && w.bytes().all(|b| b.is_ascii_digit())) {
        return true; // (1985)
    }
    NOISE_WORDS.iter().any(|w| text::contains_phrase(&f, w))
}

/// Cut a ` feat. X` / ` ft. X` / ` featuring X` credit off the end.
fn cut_feat(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let cut = [" feat. ", " feat ", " ft. ", " ft ", " featuring "].iter().filter_map(|p| lower.find(p)).min();
    match cut {
        Some(i) => s[..i].trim().to_string(),
        None => s.to_string(),
    }
}

/// Strip trailing noise phrases (`Official Video`, `Lyrics`, `HD`) and dangling separators.
fn strip_trailing_noise(s: &str) -> String {
    let mut s = s.trim().to_string();
    loop {
        let before = s.len();
        s = s.trim_end_matches([' ', '-', '|', ':', '/', ',', '·', '•']).to_string();
        let lower = s.to_ascii_lowercase();
        if let Some(p) = TRAILING_NOISE.iter().find(|p| lower.ends_with(*p) && (lower.len() == p.len() || lower.as_bytes()[lower.len() - p.len() - 1] == b' ')) {
            s.truncate(s.len() - p.len());
        }
        if s.len() == before {
            return s.trim().to_string();
        }
    }
}

/// Whole-title cleanup before splitting: dashes, hashtags, noise brackets, noise segments
/// after the first (`A - B | Official Video`, `A - B // Lyrics`, `A - B - Remastered 2011`).
fn clean(title: &str) -> String {
    let s = dashes(title);
    let s: String = s.split(' ').filter(|w| !(w.starts_with('#') && w.len() > 1)).collect::<Vec<_>>().join(" ");
    let s = strip_brackets(&s, is_noise);
    let segs: Vec<String> = s
        .split(" | ")
        .flat_map(|x| x.split(" // "))
        .enumerate()
        .map(|(i, x)| (i, strip_trailing_noise(x)))
        .filter(|(i, x)| !x.is_empty() && (*i == 0 || !is_noise(x)))
        .map(|(_, x)| x)
        .collect();
    let s = segs.join(" | ");
    let parts: Vec<&str> = s.split(" - ").collect();
    let keep: Vec<&str> = parts.iter().enumerate().filter(|(i, p)| *i < 2 || !is_noise(p)).map(|(_, p)| *p).collect();
    strip_trailing_noise(&keep.join(" - "))
}

struct Split {
    left: String,
    right: String,
    /// The separator itself says "artist, title" (dash, quotes, colon); pipes and glued
    /// dashes need the channel to agree before the left side is published as the artist.
    strong: bool,
}

/// Split into left/right at the first title separator.
fn split(s: &str) -> Option<Split> {
    if let Some((l, r)) = s.split_once(" - ") {
        return sides(l, r, true);
    }
    if let Some((l, r)) = quoted(s) {
        return sides(&l, &r, true);
    }
    if let Some((l, r)) = s.split_once(" | ") {
        return sides(l, r.split(" | ").next().unwrap_or(r), false);
    }
    if let Some((l, r)) = s.split_once(": ")
        && l.split_whitespace().count() <= 5
    {
        return sides(l, r, true);
    }
    // `Foo Fighters-Everlong`: a glued dash after a multi-word left side
    if let Some(i) = s.find('-')
        && s[..i].contains(' ')
        && !s[i + 1..].starts_with(' ')
    {
        return sides(&s[..i], &s[i + 1..], false);
    }
    None
}

fn sides(l: &str, r: &str, strong: bool) -> Option<Split> {
    let (l, r) = (l.trim(), r.trim());
    (!l.is_empty() && !r.is_empty()).then(|| Split { left: l.to_string(), right: r.to_string(), strong })
}

/// `Artist "Title"`, `Artist “Title”`, `Artist 'Title' Official MV`.
fn quoted(s: &str) -> Option<(String, String)> {
    for (open, close) in [('"', '"'), ('“', '”'), ('«', '»'), ('「', '」'), ('『', '』'), ('‘', '’'), ('\'', '\'')] {
        let Some(a) = s.find(open) else { continue };
        let rest = &s[a + open.len_utf8()..];
        let Some(b) = rest.find(close) else { continue };
        let inner = &rest[..b];
        let after = &rest[b + close.len_utf8()..];
        if open == '\'' || open == '‘' {
            // apostrophes: require `<space>'Title'<space|end>`
            let spaced_before = s[..a].ends_with(' ');
            let spaced_after = after.is_empty() || after.starts_with(' ');
            if !spaced_before || !spaced_after || inner.starts_with(' ') || inner.ends_with(' ') {
                continue;
            }
        }
        let artist = s[..a].trim().trim_end_matches([':', '-']).trim();
        if !artist.is_empty() && !inner.trim().is_empty() {
            return Some((artist.to_string(), inner.trim().to_string()));
        }
    }
    None
}

/// Artist side: feat. credits and bracketed alt names (`BTS (방탄소년단)`) removed.
fn tidy_artist(s: &str) -> String {
    let s = strip_brackets(s, |_| true);
    let s = cut_feat(&s);
    strip_trailing_noise(&s).trim_matches(|c: char| matches!(c, '"' | '“' | '”' | ':') || c.is_whitespace()).to_string()
}

/// Title side: feat. credits, surrounding quotes and trailing noise removed.
fn tidy_title(s: &str) -> String {
    let s = cut_feat(&strip_brackets(s, is_noise));
    let s = strip_trailing_noise(&s);
    let quotes: &[char] = &['"', '“', '”', '«', '»', '「', '」', '『', '』'];
    let s = s.trim().trim_matches(quotes).trim();
    let s = s.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')).unwrap_or(s);
    strip_trailing_noise(s)
}

/// Artist named by the channel and whether that's a strong signal.
/// `X - Topic` (YouTube auto-generated) and `XVEVO` are strong; `X Official` and plain
/// names are weak (labels and fan channels upload too).
pub fn channel_artist(channel: &str) -> (Option<String>, bool) {
    let c = collapse(channel.trim());
    if c.is_empty() {
        return (None, false);
    }
    if let Some(a) = c.strip_suffix(" - Topic").map(str::trim) {
        let generic = matches!(norm(a).as_str(), "various artists" | "release" | "");
        return if generic { (None, false) } else { (Some(a.to_string()), true) };
    }
    for suffix in ["VEVO", "Vevo", "vevo"] {
        if let Some(a) = c.strip_suffix(suffix).map(str::trim)
            && !a.is_empty()
        {
            let a = if a.contains(' ') { a.to_string() } else { split_camel(a) };
            return (Some(a), true);
        }
    }
    let lower = c.to_ascii_lowercase();
    for suffix in [" official youtube channel", " official channel", " - official", " official", "official"] {
        if lower.ends_with(suffix) && lower.len() > suffix.len() {
            let a = c[..c.len() - suffix.len()].trim();
            let a = if a.contains(' ') { a.to_string() } else { split_camel(a) };
            return (Some(a), false);
        }
    }
    (Some(c), false)
}

/// `RickAstley` → `Rick Astley` (lower→upper boundaries only; `ACDC` stays).
fn split_camel(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    let mut prev_lower = false;
    for c in s.chars() {
        if c.is_uppercase() && prev_lower {
            out.push(' ');
        }
        prev_lower = c.is_lowercase();
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pa(title: &str, channel: &str) -> (Option<String>, String) {
        let p = parse(title, channel);
        (p.artist, p.title)
    }

    fn g(artist: &str, title: &str) -> Guess {
        Guess { artist: artist.into(), title: title.into() }
    }

    #[test]
    fn real_world_title_shapes() {
        // (video title, channel, artist, song title)
        let cases: &[(&str, &str, &str, &str)] = &[
            ("Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)", "Rick Astley", "Rick Astley", "Never Gonna Give You Up"),
            ("Queen – Bohemian Rhapsody (Official Video Remastered)", "Queen Official", "Queen", "Bohemian Rhapsody"),
            ("Rick Astley — Never Gonna Give You Up", "", "Rick Astley", "Never Gonna Give You Up"),
            ("Nirvana - Smells Like Teen Spirit (Official Music Video)", "NirvanaVEVO", "Nirvana", "Smells Like Teen Spirit"),
            ("blink-182 - All The Small Things (Official Music Video)", "blink-182", "blink-182", "All The Small Things"),
            ("AC/DC - Back In Black (Official Video)", "acdcVEVO", "AC/DC", "Back In Black"),
            ("My Chemical Romance - Welcome To The Black Parade [Official Music Video] [HD]", "My Chemical Romance", "My Chemical Romance", "Welcome To The Black Parade"),
            ("Green Day - Basket Case [Official Music Video] (4K Upgrade)", "Green Day", "Green Day", "Basket Case"),
            ("Slipknot - Duality [OFFICIAL VIDEO] [HD]", "Slipknot", "Slipknot", "Duality"),
            ("Tool - Schism (Lyrics)", "Some Lyrics Channel", "Tool", "Schism"),
            ("Paramore: Misery Business [OFFICIAL VIDEO]", "Paramore", "Paramore", "Misery Business"),
            ("Twenty One Pilots: Stressed Out [OFFICIAL VIDEO]", "Fueled By Ramen", "Twenty One Pilots", "Stressed Out"),
            ("Metallica: Enter Sandman (Official Music Video)", "Metallica", "Metallica", "Enter Sandman"),
            ("Daft Punk - Get Lucky (Official Audio) ft. Pharrell Williams, Nile Rodgers", "Daft Punk", "Daft Punk", "Get Lucky"),
            ("Mark Ronson - Uptown Funk (Official Video) ft. Bruno Mars", "Mark Ronson", "Mark Ronson", "Uptown Funk"),
            ("Dua Lipa - Levitating Featuring DaBaby (Official Music Video)", "Dua Lipa", "Dua Lipa", "Levitating"),
            ("Calvin Harris feat. Rihanna - This Is What You Came For", "CalvinHarrisVEVO", "Calvin Harris", "This Is What You Came For"),
            ("Post Malone, Swae Lee - Sunflower (Spider-Man: Into the Spider-Verse)", "Post Malone", "Post Malone, Swae Lee", "Sunflower (Spider-Man: Into the Spider-Verse)"),
            ("Led Zeppelin - Immigrant Song (Live 1972) [Official Video]", "Led Zeppelin", "Led Zeppelin", "Immigrant Song"),
            ("Fall Out Boy - Sugar, We're Goin Down (Official Music Video)", "Fall Out Boy", "Fall Out Boy", "Sugar, We're Goin Down"),
            ("Arctic Monkeys - Do I Wanna Know? (Official Video)", "ArcticMonkeysVEVO", "Arctic Monkeys", "Do I Wanna Know?"),
            ("Simple Minds - Don't You (Forget About Me)", "Simple Minds", "Simple Minds", "Don't You (Forget About Me)"),
            ("Linkin Park \"Numb\" [Official Music Video]", "Linkin Park", "Linkin Park", "Numb"),
            ("BTS (방탄소년단) 'Dynamite' Official MV", "HYBE LABELS", "BTS", "Dynamite"),
            ("Måneskin - Beggin' (Lyrics)", "Lyrics Vault", "Måneskin", "Beggin'"),
            ("Gorillaz - Feel Good Inc. (Official Video)", "Gorillaz", "Gorillaz", "Feel Good Inc."),
            ("Queen - Bohemian Rhapsody - Remastered 2011", "Queen - Topic", "Queen", "Bohemian Rhapsody"),
            ("System Of A Down - Chop Suey! (Official HD Video)", "systemofadownVEVO", "System Of A Down", "Chop Suey!"),
            ("Survivor - Eye Of The Tiger (Official HD Video) #shorts", "Survivor", "Survivor", "Eye Of The Tiger"),
            ("Toto - Africa | Official Video", "TotoVEVO", "Toto", "Africa"),
            ("Muse - Uprising [Official Video] // Lyrics", "Muse", "Muse", "Uprising"),
            ("Foo Fighters -Everlong (Official HD Video)", "foofightersVEVO", "Foo Fighters", "Everlong"),
            ("Foo Fighters-Everlong (Official HD Video)", "foofightersVEVO", "Foo Fighters", "Everlong"),
            ("Kendrick Lamar - HUMBLE.", "KendrickLamarVEVO", "Kendrick Lamar", "HUMBLE."),
            ("Billie Eilish - bad guy (Official Music Video)", "Billie Eilish", "Billie Eilish", "bad guy"),
            ("Rammstein - Du Hast (Official 4K Video)", "Rammstein Official", "Rammstein", "Du Hast"),
            ("Panic! At The Disco: High Hopes [OFFICIAL VIDEO]", "Fueled By Ramen", "Panic! At The Disco", "High Hopes"),
            ("Bring Me The Horizon - Throne (Official Video)", "BMTHOfficialVEVO", "Bring Me The Horizon", "Throne"),
            ("Ｑｕｅｅｎ - Ｂｏｈｅｍｉａｎ Ｒｈａｐｓｏｄｙ", "", "Ｑｕｅｅｎ", "Ｂｏｈｅｍｉａｎ Ｒｈａｐｓｏｄｙ"),
        ];
        for (title, channel, artist, song) in cases {
            let p = parse(title, channel);
            assert_eq!(p.artist.as_deref(), Some(*artist), "artist of {title:?}");
            assert_eq!(p.title, *song, "title of {title:?}");
            assert_eq!(p.guesses.first(), Some(&g(artist, song)), "first guess of {title:?}");
            assert!(p.guesses.len() <= MAX_GUESSES);
        }
    }

    #[test]
    fn channel_only_titles() {
        // auto-generated `- Topic` uploads and VEVO channels name the artist
        assert_eq!(pa("Bohemian Rhapsody (Remastered 2011)", "Queen - Topic"), (Some("Queen".into()), "Bohemian Rhapsody".into()));
        assert_eq!(pa("Never Gonna Give You Up", "RickAstleyVEVO"), (Some("Rick Astley".into()), "Never Gonna Give You Up".into()));
        assert_eq!(pa("Billie Jean (Official Video)", "Michael Jackson VEVO"), (Some("Michael Jackson".into()), "Billie Jean".into()));
        // a title made of noise words is still a title
        assert_eq!(pa("Video Games", "LanaDelReyVEVO"), (Some("Lana Del Rey".into()), "Video Games".into()));
        // a plain channel is only a guess, not a published artist
        let p = parse("Everlong (Acoustic)", "Foo Fighters");
        assert_eq!(p.artist, None);
        assert_eq!(p.title, "Everlong");
        assert_eq!(p.guesses, vec![g("Foo Fighters", "Everlong")]);
        let p = parse("Bohemian Rhapsody (Live Aid 1985)", "Queen Official");
        assert_eq!(p.guesses, vec![g("Queen", "Bohemian Rhapsody")]);
        // generic topic channels and empty channels give nothing to ask about
        assert!(parse("Some Song", "Various Artists - Topic").guesses.is_empty());
        assert!(parse("Some Song", "").guesses.is_empty());
        assert_eq!(parse("Some Song", "").title, "Some Song");
        // nothing usable left: the raw title is kept
        assert_eq!(parse("(Official Video)", "").title, "(Official Video)");
    }

    #[test]
    fn reversed_and_weak_separators() {
        // label channel: left-to-right first, then swapped
        let p = parse("Bohemian Rhapsody - Panic! At The Disco (Suicide Squad OST)", "Fueled By Ramen");
        assert_eq!(p.guesses, vec![g("Bohemian Rhapsody", "Panic! At The Disco"), g("Panic! At The Disco", "Bohemian Rhapsody")]);
        // the channel names the right-hand side: swapped first
        let p = parse("Numb - Linkin Park (Official Video)", "Linkin Park");
        assert_eq!((p.artist.as_deref(), p.title.as_str()), (Some("Linkin Park"), "Numb"));
        assert_eq!(p.guesses[0], g("Linkin Park", "Numb"));
        // pipes: the channel decides which side is the artist
        let p = parse("Bad Guy | Billie Eilish", "Billie Eilish");
        assert_eq!((p.artist.as_deref(), p.title.as_str()), (Some("Billie Eilish"), "Bad Guy"));
        assert_eq!(p.guesses[0], g("Billie Eilish", "Bad Guy"));
        // …and without the channel's word it's only a guess
        let p = parse("Bad Guy | Billie Eilish", "Lyric Hub");
        assert_eq!(p.artist, None);
        assert_eq!(p.guesses[..2], [g("Bad Guy", "Billie Eilish"), g("Billie Eilish", "Bad Guy")]);
        // `- Topic` channel that names neither side: the dash is part of the release title
        let p = parse("Song Title - Live in Paris 2019 Set", "Some Band - Topic");
        assert_eq!((p.artist.as_deref(), p.title.as_str()), (Some("Some Band"), "Song Title - Live in Paris 2019 Set"));
        assert_eq!(p.guesses[0], g("Some Band", "Song Title - Live in Paris 2019 Set"));
        // a VEVO channel with an abbreviated name keeps the title's artist
        let p = parse("Song Title - Other Words", "BMTHOfficialVEVO");
        assert_eq!(p.artist.as_deref(), Some("Song Title"));
        assert_eq!(p.guesses.last(), Some(&g("BMTHOfficial", "Song Title - Other Words")));
    }

    #[test]
    fn noise_and_feat_detection() {
        assert!(is_noise("Official Video"));
        assert!(is_noise("4K Remaster"));
        assert!(is_noise("feat. Pharrell Williams"));
        assert!(is_noise("1985"));
        assert!(is_noise("Live at Wembley"));
        assert!(!is_noise("Forget About Me"));
        assert!(!is_noise("I Can't Get No"));
        assert_eq!(cut_feat("Get Lucky ft. Pharrell"), "Get Lucky");
        assert_eq!(cut_feat("Left Feat Right"), "Left");
        assert_eq!(cut_feat("Soft Cell"), "Soft Cell", "no false cut inside words");
        assert_eq!(cut_feat("Daft Punk"), "Daft Punk");
        assert_eq!(strip_trailing_noise("Africa Official Music Video"), "Africa");
        assert_eq!(strip_trailing_noise("Video Killed the Radio Star"), "Video Killed the Radio Star");
        assert_eq!(dashes("blink-182 - Dammit"), "blink-182 - Dammit", "glued dashes stay");
        assert_eq!(dashes("Foo Fighters -Everlong"), "Foo Fighters - Everlong");
        assert_eq!(dashes("Foo Fighters– Everlong"), "Foo Fighters - Everlong");
    }

    #[test]
    fn normalized_comparisons() {
        assert_eq!(norm("Don’t Stop Me Now!"), "dont stop me now");
        assert_eq!(norm("Beyoncé"), "beyonce");
        assert_eq!(norm("Mumford & Sons"), norm("Mumford and Sons"));
        assert_eq!(norm("The Killers"), norm("Killers"));
        assert_eq!(core("Don't You (Forget About Me)"), "dont you");
        assert_eq!(core("Bohemian Rhapsody - Remastered 2011"), "bohemian rhapsody");
        assert_eq!(core("Get Lucky (feat. Pharrell Williams)"), "get lucky");
        assert!(same_artist("Queen Official", "Queen"));
        assert!(same_artist("The Beatles", "Beatles"));
        assert!(same_artist("foofighters", "Foo Fighters"));
        assert!(!same_artist("Queen", "Queens of the Stone Age"));
        assert_eq!(primary_artist("Post Malone, Swae Lee"), "Post Malone");
        assert_eq!(primary_artist("Marshmello x Bastille"), "Marshmello");
        assert_eq!(primary_artist("AC/DC"), "AC/DC");
        assert_eq!(split_camel("RickAstley"), "Rick Astley");
        assert_eq!(split_camel("ACDC"), "ACDC");
        assert_eq!(channel_artist("acdcVEVO"), (Some("acdc".into()), true));
        assert_eq!(channel_artist("Queen - Topic"), (Some("Queen".into()), true));
        assert_eq!(channel_artist("Queen Official"), (Some("Queen".into()), false));
    }
}
