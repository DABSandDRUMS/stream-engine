//! Audio selection from the recording's configured sources. Keep every selected source by
//! default; optional user role rules can omit entire talk-clip tracks, never separate sounds
//! already mixed into one stream.

use crate::config::{AudioConfig, MixedPolicy};
use crate::session::TrackInfo;
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq)]
pub struct AudioPlan {
    /// Audio-stream indices mixed into the clip (empty = no audio).
    pub mix: Vec<usize>,
    /// Stream transcribed for captions/ranking.
    pub transcribe: Option<usize>,
    /// True when a user-configured music-role track was omitted (not source separation).
    pub music_dropped: bool,
    /// Human-readable explanation for the review UI.
    pub note: String,
}

/// Case-insensitive glob with `*` and `?`.
pub fn glob(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let (mut pi, mut ti, mut star, mut mark) = (0usize, 0usize, None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Roles of one track (matched against its name, sources, and devices).
pub fn roles(track: &TrackInfo, cfg: &AudioConfig) -> BTreeSet<String> {
    let names = std::iter::once(&track.name).chain(&track.sources).chain(&track.devices).filter(|s| !s.is_empty());
    let names: Vec<&String> = names.collect();
    cfg.roles.iter().filter(|(_, pats)| pats.iter().any(|p| names.iter().any(|n| glob(p, n)))).map(|(r, _)| r.clone()).collect()
}

/// Plan clip audio for a recording with `streams` audio streams.
pub fn plan(streams: usize, tracks: &[TrackInfo], cfg: &AudioConfig) -> AudioPlan {
    if streams == 0 {
        return AudioPlan { mix: Vec::new(), transcribe: None, music_dropped: true, note: "recording has no audio".into() };
    }
    // track info per stream: the recording's own layout, else `[clips.audio] tracks`
    let info: Vec<Option<TrackInfo>> = (0..streams)
        .map(|i| {
            tracks.iter().find(|t| t.index == i).cloned().or_else(|| cfg.tracks.get(i).map(|n| TrackInfo { index: i, name: n.clone(), ..Default::default() }))
        })
        .collect();
    let roles: Vec<BTreeSet<String>> = info.iter().map(|t| t.as_ref().map(|t| roles(t, cfg)).unwrap_or_default()).collect();
    let dropped = |i: usize| roles[i].iter().any(|r| cfg.drop.contains(r));
    let pick_transcribe = |mix: &[usize]| -> Option<usize> {
        for want in &cfg.transcribe {
            // a track carrying only this role beats a mixed one
            if let Some(i) = (0..streams).find(|i| roles[*i].len() == 1 && roles[*i].contains(want)) {
                return Some(i);
            }
            if let Some(i) = (0..streams).find(|i| roles[*i].contains(want) && !dropped(*i)) {
                return Some(i);
            }
        }
        mix.first().copied().or(Some(0))
    };
    if streams == 1 {
        // A role name cannot prove that a recorded stream excludes backing music.
        let mute = dropped(0) && matches!(cfg.mixed, MixedPolicy::Mute);
        return AudioPlan {
            mix: if mute { Vec::new() } else { vec![0] },
            transcribe: pick_transcribe(&[0]),
            music_dropped: mute,
            note: if mute {
                "one recorded audio stream muted by the mixed-track policy"
            } else {
                "one recorded audio stream kept intact; music cannot be removed separately"
            }
            .into(),
        };
    }
    let known = roles.iter().any(|r| !r.is_empty());
    if !known {
        let mix: Vec<usize> = (0..streams).collect();
        let t = pick_transcribe(&mix);
        return AudioPlan {
            mix,
            transcribe: t,
            music_dropped: false,
            note: format!("{streams} track(s) without role info (set [clips.audio] tracks or name OBS sources after engine nodes); music may be included"),
        };
    }
    let mix: Vec<usize> = (0..streams).filter(|i| !dropped(*i)).collect();
    if mix.is_empty() {
        let all: Vec<usize> = (0..streams).collect();
        let t = pick_transcribe(&all);
        return match cfg.mixed {
            MixedPolicy::Keep => AudioPlan {
                mix: all,
                transcribe: t,
                music_dropped: false,
                note: "every recorded source matched an exclusion; mixed-track policy keeps the complete mix".into(),
            },
            MixedPolicy::Mute => {
                AudioPlan { mix: Vec::new(), transcribe: t, music_dropped: true, note: "every recorded source excluded: clip muted by explicit policy".into() }
            }
        };
    }
    let names: Vec<String> =
        mix.iter().map(|i| roles[*i].iter().cloned().collect::<Vec<_>>().join("+")).map(|s| if s.is_empty() { "other".into() } else { s }).collect();
    let dropped_n = streams - mix.len();
    AudioPlan {
        transcribe: pick_transcribe(&mix),
        music_dropped: (0..streams).any(|i| dropped(i) && roles[i].contains("music")),
        note: format!("mix of {} ({} track(s) dropped)", names.join(", "), dropped_n),
        mix,
    }
}

/// Song clips keep every configured recording source; labels do not prove whether a feed
/// is an isolated instrument or an already-combined mix. Do not caption likely lyrics.
pub fn plan_song(streams: usize, tracks: &[TrackInfo], cfg: &AudioConfig) -> AudioPlan {
    if streams == 0 {
        return plan(streams, tracks, cfg);
    }
    AudioPlan {
        mix: (0..streams).collect(),
        transcribe: None,
        music_dropped: false,
        note: "selected recording sources kept intact; mixed components cannot be separated".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(i: usize, name: &str, devices: &[&str]) -> TrackInfo {
        TrackInfo { index: i, name: name.into(), devices: devices.iter().map(|s| s.to_string()).collect(), ..Default::default() }
    }

    #[test]
    fn globs() {
        assert!(glob("*music*", "PipeWire se-Music capture"));
        assert!(glob("se-?and", "se-band"));
        assert!(!glob("se-mic", "se-mic2"));
        assert!(glob("*", ""));
    }

    #[test]
    fn multitrack_drops_music_and_program_and_transcribes_the_mic() {
        let cfg = AudioConfig { drop: vec!["music".into(), "program".into()], ..AudioConfig::default() };
        let tracks = [t(0, "Track 1", &["se-program"]), t(1, "Mic", &["se-mic"]), t(2, "Track 3", &["se-music"]), t(3, "Band", &["se-band"])];
        let p = plan(4, &tracks, &cfg);
        assert_eq!(p.mix, vec![1, 3]);
        assert_eq!(p.transcribe, Some(1));
        assert!(p.music_dropped);
        // No preferred mic track: use the next configured transcription role.
        let p = plan(2, &[t(0, "Band", &["se-band"]), t(1, "Music", &["se-music"])], &cfg);
        assert_eq!((p.mix.clone(), p.transcribe), (vec![0], Some(0)));
    }

    #[test]
    fn single_mixed_track_follows_policy_and_config_names_fill_gaps() {
        let mut cfg = AudioConfig { drop: vec!["music".into()], ..AudioConfig::default() };
        let mixed = [TrackInfo { index: 0, name: "Track 1".into(), sources: vec!["se-music".into(), "se-band".into()], ..Default::default() }];
        let p = plan(1, &mixed, &cfg);
        assert_eq!(p.mix, vec![0]);
        assert!(!p.music_dropped);
        cfg.mixed = MixedPolicy::Mute;
        assert!(plan(1, &mixed, &cfg).mix.is_empty());
        // unknown layout → keep everything, flagged
        let p = plan(3, &[], &AudioConfig::default());
        assert_eq!(p.mix, vec![0, 1, 2]);
        assert!(!p.music_dropped);
        // `[clips.audio] tracks` names streams the recording didn't describe
        let cfg = AudioConfig { tracks: vec!["mic".into(), "music".into(), "band".into()], drop: vec!["music".into()], ..AudioConfig::default() };
        let p = plan(3, &[], &cfg);
        assert_eq!((p.mix, p.transcribe, p.music_dropped), (vec![0, 2], Some(0), true));
    }

    #[test]
    fn one_mixed_source_keeps_music_in_talk_and_song_clips() {
        let cfg = AudioConfig::default();
        let mixed = [t(0, "Live mix", &["se-band"])];
        let talk = plan(1, &mixed, &cfg);
        assert_eq!(talk.mix, vec![0]);
        assert!(!talk.music_dropped, "a band-labelled track can still contain backing music");
        let song = plan_song(1, &mixed, &cfg);
        assert_eq!(song.mix, vec![0]);
        assert!(!song.music_dropped);
        assert_eq!(song.transcribe, None);
    }
    #[test]
    fn selected_sources_are_not_dropped_based_on_names() {
        let cfg = AudioConfig::default();
        let tracks = [t(0, "Program", &["mixed-input"]), t(1, "Other feed", &["network"]), t(2, "Song", &["player"])];
        let song = plan_song(3, &tracks, &cfg);
        assert_eq!(song.mix, vec![0, 1, 2]);
        assert!(!song.music_dropped);
        assert_eq!(song.transcribe, None);
        let talk = plan(3, &tracks, &cfg);
        assert_eq!(talk.mix, vec![0, 1, 2]);
        assert!(!talk.music_dropped);
    }
}
