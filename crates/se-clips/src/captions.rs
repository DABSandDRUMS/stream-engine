//! Burned-in captions: short caption lines built from Whisper word timestamps, written as ASS
//! subtitles (per canvas: size, margin, line length) with the spoken word highlighted
//! (karaoke `\kf`). Times are relative to the clip's in-point.

use crate::config::CaptionConfig;
use crate::transcribe::Word;
use std::fmt::Write;

/// `#rrggbb` → ASS `&H00BBGGRR`.
pub fn ass_color(hex: &str) -> Option<String> {
    let h = hex.strip_prefix('#')?;
    if h.len() != 6 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("&H00{}{}{}", &h[4..6], &h[2..4], &h[0..2]).to_uppercase())
}

/// Caption geometry for one output canvas.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub width: u32,
    pub height: u32,
    pub font_size: u32,
    pub max_chars: usize,
    pub margin_v: u32,
}

impl Layout {
    pub fn for_canvas(canvas: &str, size: [u32; 2], cfg: &CaptionConfig) -> Layout {
        let tall = canvas == "tall";
        Layout {
            width: size[0],
            height: size[1],
            font_size: if tall { cfg.size_tall } else { cfg.size_wide },
            max_chars: if tall { cfg.max_chars_tall } else { cfg.max_chars_wide }.max(8),
            margin_v: if tall { cfg.margin_tall } else { cfg.margin_wide },
        }
    }
}

/// One caption on screen: words with clip-relative times.
#[derive(Clone, Debug, PartialEq)]
pub struct Cue {
    pub start: f64,
    pub end: f64,
    pub words: Vec<(f64, f64, String)>,
}

impl Cue {
    pub fn text(&self) -> String {
        self.words.iter().map(|w| w.2.as_str()).collect::<Vec<_>>().join(" ")
    }
}

fn ends_sentence(w: &str) -> bool {
    w.trim_end_matches(['"', '\'', ')']).ends_with(['.', '!', '?'])
}

/// Group spoken words inside `[t_in, t_out]` into caption cues.
pub fn cues(words: &[Word], t_in: f64, t_out: f64, max_chars: usize, max_caption: f64) -> Vec<Cue> {
    let dur = (t_out - t_in).max(0.0);
    let spoken: Vec<(f64, f64, String)> = words
        .iter()
        .filter(|w| !w.annotation && w.t1 > t_in && w.t0 < t_out)
        .map(|w| ((w.t0 - t_in).clamp(0.0, dur), (w.t1 - t_in).clamp(0.0, dur), w.text.trim().to_string()))
        .filter(|w| !w.2.is_empty())
        .collect();
    let mut out: Vec<Cue> = Vec::new();
    let mut cur: Vec<(f64, f64, String)> = Vec::new();
    let mut chars = 0usize;
    for (i, w) in spoken.iter().enumerate() {
        let add = w.2.chars().count() + usize::from(!cur.is_empty());
        let too_long = !cur.is_empty() && (chars + add > max_chars || w.1 - cur[0].0 > max_caption);
        let paused = cur.last().is_some_and(|l| w.0 - l.1 > 1.0);
        if too_long || paused {
            out.push(Cue { start: cur[0].0, end: cur.last().map(|l| l.1).unwrap_or(0.0), words: std::mem::take(&mut cur) });
            chars = 0;
        }
        chars += w.2.chars().count() + usize::from(!cur.is_empty());
        cur.push(w.clone());
        let last = i + 1 == spoken.len();
        if ends_sentence(&w.2) && !last {
            out.push(Cue { start: cur[0].0, end: w.1, words: std::mem::take(&mut cur) });
            chars = 0;
        }
    }
    if !cur.is_empty() {
        out.push(Cue { start: cur[0].0, end: cur.last().map(|l| l.1).unwrap_or(0.0), words: cur });
    }
    // readable timing: linger a little, never overlap the next cue, at least 0.7 s on screen
    let n = out.len();
    for i in 0..n {
        let next = if i + 1 < n { out[i + 1].start } else { dur };
        let end = (out[i].end + 0.4).max(out[i].start + 0.7).min(next - 0.02).min(dur);
        out[i].end = end.max(out[i].start + 0.05).min(dur.max(out[i].start + 0.05));
    }
    out
}

fn ass_time(t: f64) -> String {
    let cs = (t.max(0.0) * 100.0).round() as u64;
    format!("{}:{:02}:{:02}.{:02}", cs / 360_000, (cs / 6000) % 60, (cs / 100) % 60, cs % 100)
}

fn ass_text(s: &str, upper: bool) -> String {
    let s: String = s
        .chars()
        .map(|c| match c {
            '{' => '(',
            '}' => ')',
            '\\' => '/',
            '\n' | '\r' => ' ',
            c => c,
        })
        .collect();
    if upper { s.to_uppercase() } else { s }
}

/// A complete ASS document for the cues.
pub fn ass(cues: &[Cue], layout: &Layout, cfg: &CaptionConfig) -> String {
    let primary = ass_color(&cfg.highlight).unwrap_or_else(|| "&H003DD8FF".into());
    let secondary = ass_color(&cfg.color).unwrap_or_else(|| "&H00FFFFFF".into());
    let outline = ass_color(&cfg.outline).unwrap_or_else(|| "&H00000000".into());
    let border = (layout.font_size as f64 / 14.0).round().max(2.0);
    let mut s = String::new();
    let _ = write!(
        s,
        "[Script Info]\nScriptType: v4.00+\nPlayResX: {w}\nPlayResY: {h}\nWrapStyle: 0\nScaledBorderAndShadow: yes\nYCbCr Matrix: TV.709\n\n\
         [V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n\
         Style: Caption,{font},{size},{primary},{secondary},{outline},&H80000000,-1,0,0,0,100,100,0,0,1,{border},1,2,{ml},{ml},{mv},1\n\n\
         [Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n",
        w = layout.width,
        h = layout.height,
        font = cfg.font.replace(',', " "),
        size = layout.font_size,
        ml = layout.width / 16,
        mv = layout.margin_v,
    );
    for c in cues {
        let mut text = String::new();
        for (i, (t0, t1, w)) in c.words.iter().enumerate() {
            // highlight sweeps each word from its start to the next word's start
            let until = c.words.get(i + 1).map(|n| n.0).unwrap_or(*t1).max(*t0);
            let lead = if i == 0 { t0 - c.start } else { 0.0 };
            if lead > 0.005 {
                let _ = write!(text, "{{\\k{}}}", (lead * 100.0).round() as u64);
            }
            let k = ((until - t0) * 100.0).round().max(1.0) as u64;
            if i > 0 {
                text.push(' ');
            }
            let _ = write!(text, "{{\\kf{k}}}{}", ass_text(w, cfg.uppercase));
        }
        let _ = writeln!(s, "Dialogue: 0,{},{},Caption,,0,0,0,,{}", ass_time(c.start), ass_time(c.end), text);
    }
    s
}

/// Plain caption text of a clip (review UI, upload metadata).
pub fn plain(cues: &[Cue]) -> String {
    cues.iter().map(Cue::text).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(t0: f64, t1: f64, s: &str) -> Word {
        Word { t0, t1, text: s.into(), p: 0.9, annotation: false }
    }

    #[test]
    fn cues_follow_word_timing_relative_to_the_in_point() {
        let words = vec![
            w(99.0, 99.8, "before"),
            w(100.2, 100.5, "Oh"),
            w(100.5, 100.9, "my"),
            w(100.9, 101.4, "god."),
            w(101.6, 101.9, "Did"),
            w(101.9, 102.1, "you"),
            w(102.1, 102.5, "see"),
            w(102.5, 102.9, "that?"),
            Word { t0: 103.0, t1: 104.0, text: "(laughing)".into(), p: 0.5, annotation: true },
            w(106.0, 106.4, "Anyway"),
        ];
        let c = cues(&words, 100.0, 107.0, 42, 3.5);
        assert_eq!(c.len(), 3, "{c:?}");
        assert_eq!(c[0].text(), "Oh my god.");
        assert!((c[0].start - 0.2).abs() < 1e-9);
        // lingers 0.4 s but ends before the next sentence starts
        assert!((c[0].end - 1.58).abs() < 1e-9, "{}", c[0].end);
        assert_eq!(c[1].text(), "Did you see that?");
        assert_eq!(c[2].text(), "Anyway");
        assert!(c[2].end <= 7.0);
        // a word straddling the in-point is clipped to 0, never negative
        let c = cues(&words, 100.7, 102.0, 42, 3.5);
        assert_eq!(c[0].words[0].2, "my");
        assert_eq!(c[0].words[0].0, 0.0);
    }

    #[test]
    fn long_speech_splits_by_line_length_and_duration() {
        let words: Vec<Word> = (0..20).map(|i| w(i as f64 * 0.3, i as f64 * 0.3 + 0.25, "word")).collect();
        let c = cues(&words, 0.0, 10.0, 22, 3.5);
        // "word word word word" = 19 chars; a 5th word would exceed 22
        assert!(c.iter().all(|c| c.text().chars().count() <= 22));
        assert_eq!(c[0].words.len(), 4);
        let c = cues(&words, 0.0, 10.0, 200, 1.0);
        assert!(c.iter().all(|c| c.words.last().unwrap().1 - c.words[0].0 <= 1.0 + 1e-9));
    }

    #[test]
    fn ass_document_has_karaoke_timing_and_escapes_text() {
        let cfg = CaptionConfig::default();
        let layout = Layout::for_canvas("tall", [1080, 1920], &cfg);
        let c = vec![Cue { start: 1.0, end: 2.5, words: vec![(1.25, 1.5, "{hi}".into()), (1.5, 2.0, "there\\".into())] }];
        let doc = ass(&c, &layout, &cfg);
        assert!(doc.contains("PlayResX: 1080\nPlayResY: 1920"));
        assert!(doc.contains(&format!(",{},", cfg.size_tall)));
        assert!(doc.contains("Dialogue: 0,0:00:01.00,0:00:02.50,Caption,,0,0,0,,{\\k25}{\\kf25}(hi) {\\kf50}there/"), "{doc}");
        assert_eq!(ass_color("#ffd83d").unwrap(), "&H003DD8FF");
        assert_eq!(ass_color("ffd83d"), None);
        assert_eq!(ass_time(3723.456), "1:02:03.46");
    }
}
