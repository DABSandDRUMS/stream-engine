//! Free song metadata from MusicBrainz (no API key): artist, genres and first-release year for
//! queued songs. Metadata only — this never plays, cues, preloads or opens a YouTube video; it
//! reads the video's title and channel name we already have.
//!
//! Flow per video: [`title::parse`] guesses (artist, title) pairs → one recording search per
//! guess (`/recording?query=recording:"…" AND artist:"…"`) until a confident match (score ≥
//! [`MIN_SCORE`], normalized title and artist equal), narrowed to the earliest release with
//! `firstreleasedate:[* TO …]` re-searches when there are more copies than one page → genres
//! from the recording, else its release group, else the artist (MB `genres`, else top `tags`)
//! → cached by video id in the runtime DB (`song_meta`), misses too, each with a retry-after.
//!
//! Requests go through one [`Worker`] task: at most one request per [`MIN_GAP`] (MusicBrainz
//! allows 1/s), short timeouts, a descriptive User-Agent; failures are logged and cached
//! briefly, never surfaced to playback.

use crate::store::{MetaRow, Store};
use crate::text;
use crate::title::{self, Guess};
use se_proto::Value;
use serde::{Deserialize, Serialize};
use serde_json::Value as J;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

pub const DEFAULT_BASE: &str = "https://musicbrainz.org/ws/2";
pub const USER_AGENT: &str = concat!("stream-engine/", env!("CARGO_PKG_VERSION"), " ( https://github.com/DABSandDRUMS/stream-engine )");
/// Minimum MusicBrainz search score for a match.
pub const MIN_SCORE: i64 = 90;
/// Spacing between requests (MusicBrainz: 1 request/s per client on average).
pub const MIN_GAP: Duration = Duration::from_millis(1100);
/// Cache rows written by an older matcher are looked up again.
pub const VERSION: i64 = 1;
/// A confident match is re-checked after this long (genres get voted on over time).
pub const HIT_TTL_S: i64 = 180 * 86_400;
/// No confident match: try again after this long.
pub const MISS_RETRY_S: i64 = 14 * 86_400;
/// Network/HTTP failure (or a match whose genre lookup failed): try again after this long.
pub const ERROR_RETRY_S: i64 = 3600;
const MAX_GENRES: usize = 5;
const MAX_TAGS: usize = 3;
/// Recordings per search page.
const SEARCH_LIMIT: &str = "25";
/// Extra "released before year N" searches while narrowing to the original recording.
const MAX_REFINE: usize = 3;

/// What we know about a song. Without a confident match only `title`/`artist` from the video
/// title are filled in ([`SongInfo::parsed`]).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SongInfo {
    pub title: String,
    pub artist: Option<String>,
    /// Lowercase, most-voted first.
    pub genres: Vec<String>,
    /// First-release year.
    pub year: Option<i32>,
    /// MusicBrainz recording id of the match.
    pub recording: Option<String>,
}

impl SongInfo {
    /// Title/artist read from the video title alone (no lookup).
    pub fn parsed(video_title: &str, channel: &str) -> SongInfo {
        let p = title::parse(video_title, channel);
        SongInfo { title: p.title, artist: p.artist, ..Default::default() }
    }

    /// Queue entry fields `artist` (string or null), `genres` (list), `year` (int or null).
    pub fn with_fields(&self, v: Value) -> Value {
        v.with("artist", self.artist.clone().map(Value::from).unwrap_or(Value::Null))
            .with("genres", Value::from(self.genres.clone()))
            .with("year", self.year.map(Value::from).unwrap_or(Value::Null))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// Confident match; `complete` is false when a genre lookup failed (retried sooner).
    Found { info: SongInfo, complete: bool },
    NoMatch,
    Failed(String),
}

// ---- HTTP ------------------------------------------------------------------------------------

/// GET `<base>/<path>?<params>&fmt=json` → JSON. Implemented by [`Client`]; tests replay
/// fixtures through it.
pub trait Fetch {
    fn get(&self, path: &str, params: &[(&str, &str)]) -> impl Future<Output = Result<J, String>> + Send;
}

pub struct Client {
    http: reqwest::Client,
    base: String,
    last: tokio::sync::Mutex<Option<tokio::time::Instant>>,
}

impl Client {
    pub fn new(base: &str) -> anyhow::Result<Client> {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(8)).connect_timeout(Duration::from_secs(4)).user_agent(USER_AGENT).build()?;
        Ok(Client { http, base: base.trim_end_matches('/').to_string(), last: tokio::sync::Mutex::new(None) })
    }

    /// One paced request: waits until [`MIN_GAP`] after the previous one (the lock is held
    /// while waiting so callers queue up; `extra` pushes the next slot back after a 503).
    async fn send(&self, url: &str, params: &[(&str, &str)], extra: Duration) -> Result<reqwest::Response, String> {
        {
            let mut last = self.last.lock().await;
            if let Some(t) = *last {
                tokio::time::sleep_until(t + MIN_GAP + extra).await;
            }
            *last = Some(tokio::time::Instant::now());
        }
        self.http.get(url).query(params).query(&[("fmt", "json")]).send().await.map_err(|e| format!("MusicBrainz unreachable: {e}"))
    }
}

impl Fetch for Client {
    async fn get(&self, path: &str, params: &[(&str, &str)]) -> Result<J, String> {
        let url = format!("{}/{path}", self.base);
        let mut resp = self.send(&url, params, Duration::ZERO).await?;
        if resp.status().as_u16() == 503 {
            // overloaded / rate-limited: one retry, after Retry-After (2–10 s)
            let wait = resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(2).clamp(2, 10);
            resp = self.send(&url, params, Duration::from_secs(wait)).await?;
        }
        match resp.status().as_u16() {
            200 => resp.json::<J>().await.map_err(|e| format!("unexpected MusicBrainz response: {e}")),
            503 => Err("MusicBrainz busy or rate-limited (HTTP 503)".to_string()),
            s => Err(format!("MusicBrainz HTTP {s}")),
        }
    }
}

// ---- matching --------------------------------------------------------------------------------

/// Lucene phrase with `\` and `"` escaped.
fn phrase(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Recording search query for a guess: the title without brackets/feat./` - suffix`, and the
/// main credited artist.
pub fn search_query(g: &Guess) -> String {
    let core = title::core_text(&g.title);
    let t = if core.is_empty() { g.title.as_str() } else { core.as_str() };
    format!("recording:{} AND artist:{}", phrase(t), phrase(title::primary_artist(&g.artist)))
}

/// Same song title: normalized equality, or equal once bracketed parts / ` - suffix` /
/// feat. credits are removed from both (`Don't You (Forget About Me)`).
pub fn title_matches(mb: &str, wanted: &str) -> bool {
    let (a, b) = (title::norm(mb), title::norm(wanted));
    if !a.is_empty() && a == b {
        return true;
    }
    let (a, b) = (title::core(mb), title::core(wanted));
    !a.is_empty() && a == b
}

/// The credit names the wanted artist: the whole credit equals the wanted artist or its main
/// artist, or one credited artist equals the wanted main artist (`Post Malone & Swae Lee` for
/// `Post Malone, Swae Lee`). Containment is not enough (`Queen Tribute Orchestra` ≠ `Queen`).
pub fn artist_matches(credit: &str, names: &[&str], wanted: &str) -> bool {
    let primary = title::norm(title::primary_artist(wanted));
    let whole = title::norm(wanted);
    let c = title::norm(credit);
    if primary.is_empty() || c.is_empty() {
        return false;
    }
    let squash = |s: &str| s.replace(' ', "");
    c == whole
        || c == primary
        || squash(&c) == squash(&primary)
        || names.iter().map(|n| title::norm(n)).any(|n| n == primary || squash(&n) == squash(&primary))
}

/// A confident recording match from a search response.
#[derive(Clone, Debug, PartialEq)]
pub struct Match {
    pub recording: String,
    pub title: String,
    /// Full credit (`Post Malone & Swae Lee`).
    pub artist: String,
    pub artist_id: Option<String>,
    pub year: Option<i32>,
    pub release_group: Option<String>,
    /// Tags on the search result: a tagged recording can have genres of its own, and among
    /// same-year duplicates the most-tagged one is the canonical original.
    pub tags: usize,
    pub score: i64,
}

impl Match {
    /// Ordering key: earliest first release, then most tags, then score.
    fn rank(&self) -> (i32, std::cmp::Reverse<usize>, std::cmp::Reverse<i64>) {
        (self.year.unwrap_or(i32::MAX), std::cmp::Reverse(self.tags), std::cmp::Reverse(self.score))
    }
}

fn score(rec: &J) -> i64 {
    match rec.get("score") {
        Some(J::Number(n)) => n.as_i64().unwrap_or(0),
        Some(J::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

fn str_at<'a>(v: &'a J, k: &str) -> Option<&'a str> {
    v.get(k).and_then(J::as_str).filter(|s| !s.is_empty())
}

/// `1975-10-31` / `1975` → 1975.
pub fn year(date: &str) -> Option<i32> {
    let y = date.get(..4)?;
    let y: i32 = y.parse().ok()?;
    (1000..=2100).contains(&y).then_some(y)
}

/// The release group to ask for genres: an official studio album/single/EP (no
/// compilation/live/soundtrack/DJ-mix secondary type), earliest. A compilation's or
/// soundtrack's genres describe the collection, not the song, so those are skipped (the
/// artist is asked instead).
fn release_group(rec: &J) -> Option<String> {
    let releases = rec.get("releases").and_then(J::as_array)?;
    releases
        .iter()
        .filter_map(|r| {
            let rg = r.get("release-group")?;
            let studio = matches!(str_at(rg, "primary-type"), Some("Album" | "Single" | "EP"))
                && rg.get("secondary-types").and_then(J::as_array).is_none_or(|a| a.is_empty());
            if !studio {
                return None;
            }
            let official = str_at(r, "status") == Some("Official");
            Some(((!official, str_at(r, "date").unwrap_or("9999")), str_at(rg, "id")?))
        })
        .min_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, id)| id.to_string())
}

/// Best confident recording in a search response: score ≥ [`MIN_SCORE`], title and artist
/// match; among those the earliest first release (the original, not a remaster/live/DJ-mix
/// copy), then the most-tagged, then score.
pub fn best_match(search: &J, g: &Guess) -> Option<Match> {
    let recs = search.get("recordings").and_then(J::as_array)?;
    recs.iter()
        .filter_map(|rec| {
            let sc = score(rec);
            let mb_title = str_at(rec, "title")?;
            if sc < MIN_SCORE || !title_matches(mb_title, &g.title) {
                return None;
            }
            let credits = rec.get("artist-credit").and_then(J::as_array)?;
            let credit: String = credits.iter().map(|c| format!("{}{}", str_at(c, "name").unwrap_or(""), c.get("joinphrase").and_then(J::as_str).unwrap_or(""))).collect();
            let names: Vec<&str> = credits.iter().flat_map(|c| [str_at(c, "name"), c.get("artist").and_then(|a| str_at(a, "name"))]).flatten().collect();
            if !artist_matches(&credit, &names, &g.artist) {
                return None;
            }
            Some(Match {
                recording: str_at(rec, "id")?.to_string(),
                title: mb_title.to_string(),
                artist: credit.trim().to_string(),
                artist_id: credits.first().and_then(|c| c.get("artist")).and_then(|a| str_at(a, "id")).map(String::from),
                year: str_at(rec, "first-release-date").and_then(year),
                release_group: release_group(rec),
                tags: rec.get("tags").and_then(J::as_array).map_or(0, Vec::len),
                score: sc,
            })
        })
        .min_by_key(Match::rank)
}

/// Tags that say nothing about the sound.
fn junk_tag(t: &str) -> bool {
    t.starts_with(|c: char| c.is_ascii_digit())
        || t.contains("favorite")
        || t.contains("favourite")
        || matches!(
            t,
            "seen live"
                | "american"
                | "british"
                | "english"
                | "uk"
                | "usa"
                | "canadian"
                | "australian"
                | "german"
                | "swedish"
                | "male vocalists"
                | "female vocalists"
                | "awesome"
                | "love"
                | "beautiful"
                | "spotify"
                | "check out"
        )
}

fn counted(v: Option<&J>) -> Vec<(String, i64)> {
    let mut out: Vec<(String, i64)> = v
        .and_then(J::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    let name = x.get("name")?.as_str()?.trim().to_lowercase();
                    let count = x.get("count").and_then(J::as_i64).unwrap_or(1);
                    (!name.is_empty() && count > 0).then_some((name, count))
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort_by_key(|x| std::cmp::Reverse(x.1));
    let mut seen = std::collections::HashSet::new();
    out.retain(|(n, _)| seen.insert(n.clone()));
    out
}

/// Genres of one MusicBrainz entity (recording, release group or artist lookup with
/// `inc=genres tags`): its `genres` by votes, else its top `tags` minus non-genre tags.
pub fn genres(entity: &J) -> Vec<String> {
    let g = counted(entity.get("genres"));
    if !g.is_empty() {
        return g.into_iter().take(MAX_GENRES).map(|(n, _)| n).collect();
    }
    counted(entity.get("tags")).into_iter().filter(|(n, _)| !junk_tag(n)).take(MAX_TAGS).map(|(n, _)| n).collect()
}

/// Genres for a match: recording → release group → artist. Returns (genres, release-group
/// year, every needed request succeeded).
async fn genres_for<F: Fetch>(f: &F, m: &Match) -> (Vec<String>, Option<i32>, bool) {
    let inc = [("inc", "genres tags")];
    let mut complete = true;
    let mut rg_year = None;
    let mut steps: Vec<String> = Vec::with_capacity(3);
    if m.tags > 0 {
        steps.push(format!("recording/{}", m.recording));
    }
    if let Some(rg) = &m.release_group {
        steps.push(format!("release-group/{rg}"));
    }
    if let Some(a) = &m.artist_id {
        steps.push(format!("artist/{a}"));
    }
    for path in steps {
        match f.get(&path, &inc).await {
            Ok(j) => {
                if path.starts_with("release-group/") {
                    rg_year = str_at(&j, "first-release-date").and_then(year);
                }
                let g = genres(&j);
                if !g.is_empty() {
                    return (g, rg_year, complete);
                }
            }
            Err(e) => {
                tracing::debug!("musicbrainz {path}: {e}");
                complete = false;
            }
        }
    }
    (Vec::new(), rg_year, complete)
}

/// Search for a guess and narrow toward the original recording. Popular songs have dozens of
/// equally scored copies (remasters, compilations, DJ mixes) in arbitrary order, so when the
/// result set is larger than one page, ask again for recordings released before the best
/// year found so far (at most [`MAX_REFINE`] times). Returns (best match, all requests ok).
async fn search<F: Fetch>(f: &F, g: &Guess) -> Result<(Option<Match>, bool), String> {
    let base = search_query(g);
    let mut q = base.clone();
    let mut best: Option<Match> = None;
    for _ in 0..=MAX_REFINE {
        let page = match f.get("recording", &[("query", q.as_str()), ("limit", SEARCH_LIMIT)]).await {
            Ok(j) => j,
            Err(e) if best.is_none() => return Err(e),
            Err(e) => {
                tracing::debug!("musicbrainz refine: {e}");
                return Ok((best, false));
            }
        };
        let Some(m) = best_match(&page, g) else { break };
        if best.as_ref().is_some_and(|b| m.rank() >= b.rank()) {
            break;
        }
        let year = m.year;
        best = Some(m);
        let returned = page.get("recordings").and_then(J::as_array).map_or(0, Vec::len);
        let total = page.get("count").and_then(J::as_u64).unwrap_or(0) as usize;
        let Some(y) = year else { break };
        if total <= returned {
            break;
        }
        q = format!("{base} AND firstreleasedate:[* TO {}-12-31]", y - 1);
    }
    Ok((best, true))
}

/// Look a video up: guesses in order until a confident match, then its genres.
pub async fn resolve<F: Fetch>(f: &F, video_title: &str, channel: &str) -> Outcome {
    let parsed = title::parse(video_title, channel);
    for g in &parsed.guesses {
        let (m, searched) = match search(f, g).await {
            Ok(r) => r,
            Err(e) => return Outcome::Failed(e),
        };
        if let Some(m) = m {
            let (genres, rg_year, complete) = genres_for(f, &m).await;
            let info = SongInfo { title: m.title, artist: Some(m.artist), genres, year: m.year.or(rg_year), recording: Some(m.recording) };
            return Outcome::Found { info, complete: complete && searched };
        }
    }
    Outcome::NoMatch
}

// ---- cache + worker --------------------------------------------------------------------------

/// A lookup request (title/channel as YouTube reports them).
#[derive(Clone, Debug)]
pub struct Job {
    pub video: String,
    pub title: String,
    pub channel: String,
}

/// A finished lookup: the metadata to show (`None`: no confident match, or lookups off).
#[derive(Clone, Debug)]
pub struct Done {
    pub video: String,
    pub info: Option<SongInfo>,
}

/// Write an outcome to the cache; returns the metadata to show. A failure keeps an earlier
/// match (stale but still right) and retries after [`ERROR_RETRY_S`].
pub fn record(store: &Store, video: &str, prev: Option<&MetaRow>, outcome: &Outcome, now: i64) -> Option<SongInfo> {
    let (info, fetched, retry) = match outcome {
        Outcome::Found { info, complete } => (Some(info.clone()), now, now + if *complete { HIT_TTL_S } else { ERROR_RETRY_S }),
        Outcome::NoMatch => (None, now, now + MISS_RETRY_S),
        Outcome::Failed(e) => {
            tracing::warn!("songs: metadata lookup for {video} failed: {e}");
            let keep = prev.and_then(|r| r.info.clone());
            (keep, prev.map(|r| r.fetched_at).unwrap_or(now), now + ERROR_RETRY_S)
        }
    };
    if let Err(e) = store.put_song_meta(video, info.as_ref(), fetched, retry) {
        tracing::warn!("songs: metadata cache write: {e:#}");
    }
    info
}

/// Cache first; look up only when the row is missing, due, or from an older matcher.
pub async fn process<F: Fetch>(store: &Store, f: &F, job: &Job, now: i64) -> Option<SongInfo> {
    let row = store.song_meta(&job.video);
    if let Some(r) = &row
        && !r.due(now)
    {
        return r.info.clone();
    }
    let outcome = resolve(f, &job.title, &job.channel).await;
    match &outcome {
        Outcome::Found { info, .. } => tracing::info!(
            "songs: {} is {} – {} ({}; {})",
            job.video,
            info.artist.as_deref().unwrap_or("?"),
            info.title,
            if info.genres.is_empty() { "no genres".to_string() } else { info.genres.join(", ") },
            info.year.map(|y| y.to_string()).unwrap_or_else(|| "year unknown".into())
        ),
        Outcome::NoMatch => tracing::info!("songs: no confident MusicBrainz match for {} ({:?})", job.video, job.title),
        Outcome::Failed(_) => {}
    }
    record(store, &job.video, row.as_ref(), &outcome, now)
}

/// The one task that talks to MusicBrainz (so the rate limit holds across all lookups).
#[derive(Clone)]
pub struct Worker {
    tx: mpsc::UnboundedSender<Job>,
    client: Arc<parking_lot::Mutex<Option<Arc<Client>>>>,
}

impl Worker {
    /// `client: None` = lookups off (jobs answer from the cache only). `done` gets every result.
    pub fn spawn(store: Store, client: Option<Arc<Client>>, done: impl Fn(Done) + Send + 'static) -> Worker {
        let (tx, mut rx) = mpsc::unbounded_channel::<Job>();
        let shared = Arc::new(parking_lot::Mutex::new(client));
        let current = shared.clone();
        tokio::spawn(async move {
            while let Some(job) = rx.recv().await {
                let client = current.lock().clone();
                let info = match client {
                    Some(c) => process(&store, &*c, &job, text::now_ms() / 1000).await,
                    None => store.song_meta(&job.video).and_then(|r| r.info),
                };
                done(Done { video: job.video, info });
            }
        });
        Worker { tx, client: shared }
    }

    /// Swap the client after a settings change (`None` turns lookups off).
    pub fn set_client(&self, client: Option<Arc<Client>>) {
        *self.client.lock() = client;
    }

    pub fn submit(&self, job: Job) {
        let _ = self.tx.send(job);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use se_store::Db;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn fixture(name: &str) -> Option<J> {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/musicbrainz").join(name);
        std::fs::read_to_string(p).ok().map(|s| serde_json::from_str(&s).unwrap())
    }

    /// Replays tests/fixtures/musicbrainz: searches by the artist in the query, lookups by id.
    #[derive(Default)]
    struct Fake {
        calls: Mutex<Vec<String>>,
        down: AtomicBool,
    }

    impl Fake {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().clone()
        }
    }

    impl Fetch for Fake {
        async fn get(&self, path: &str, params: &[(&str, &str)]) -> Result<J, String> {
            let q = params.iter().find(|(k, _)| *k == "query").map(|(_, v)| *v).unwrap_or("");
            self.calls.lock().push(if q.is_empty() { path.to_string() } else { format!("{path}?{q}") });
            if self.down.load(Ordering::SeqCst) {
                return Err("MusicBrainz unreachable: connection refused".into());
            }
            if path == "recording" {
                let file = match q.split_once(" AND firstreleasedate:") {
                    // the only "released before" page we have: Queen before 1977
                    Some((base, range)) => (base.contains("artist:\"Queen\"") && range == "[* TO 1976-12-31]").then_some("search_queen_bohemian_before_1977.json"),
                    None => [("artist:\"Rick Astley\"", "search_rick_astley.json"), ("artist:\"Queen\"", "search_queen_bohemian.json"), ("artist:\"Panic! At The Disco\"", "search_panic_bohemian.json")]
                        .iter()
                        .find(|(needle, _)| q.contains(needle))
                        .map(|(_, f)| *f),
                };
                return Ok(fixture(file.unwrap_or("search_empty.json")).unwrap());
            }
            assert_eq!(params, [("inc", "genres tags")], "lookups ask for genres and tags");
            fixture(&format!("{}.json", path.replace('/', "_"))).ok_or_else(|| "MusicBrainz HTTP 404".to_string())
        }
    }

    fn guess(a: &str, t: &str) -> Guess {
        Guess { artist: a.into(), title: t.into() }
    }

    #[test]
    fn query_escapes_and_uses_core_title_and_main_artist() {
        assert_eq!(search_query(&guess("Post Malone, Swae Lee", "Sunflower (Spider-Man: Into the Spider-Verse)")), r#"recording:"Sunflower" AND artist:"Post Malone""#);
        assert_eq!(search_query(&guess("Weird \"Al\" Yankovic", "White & Nerdy")), r#"recording:"White & Nerdy" AND artist:"Weird \"Al\" Yankovic""#);
        assert_eq!(search_query(&guess("A", "(Interlude)")), r#"recording:"(Interlude)" AND artist:"A""#);
    }

    #[test]
    fn confident_match_rules() {
        let s = fixture("search_rick_astley.json").unwrap();
        let m = best_match(&s, &guess("Rick Astley", "Never Gonna Give You Up")).unwrap();
        // the 1980 row scores 88 (< 90) and the cover is by another artist: the 1987 original wins
        // over the later video/remaster recordings
        assert_eq!(m.recording, "c95fd3f9-1a2b-4c3d-8e4f-5a6b7c8d9e01");
        assert_eq!((m.artist.as_str(), m.year, m.score), ("Rick Astley", Some(1987), 100));
        assert_eq!(m.release_group.as_deref(), Some("a1b2c3d4-0000-4000-8000-000000000001"), "the official single, not the compilation");
        assert_eq!(m.tags, 3);
        // wrong artist or wrong title: nothing
        assert_eq!(best_match(&s, &guess("Cake", "Never Gonna Give You Up")).map(|m| m.artist), Some("Cake".into()), "the cover, for the cover's artist");
        assert!(best_match(&s, &guess("Rick Astley", "Together Forever")).is_none());
        assert!(best_match(&s, &guess("Rickroll Tribute Band", "Never Gonna Give You Up")).is_none());
        // a tribute act whose name contains the artist is not the artist
        let q = fixture("search_queen_bohemian.json").unwrap();
        assert!(!artist_matches("Queen Tribute Orchestra", &["Queen Tribute Orchestra"], "Queen"));
        assert!(best_match(&q, &guess("Queen Tribute", "Bohemian Rhapsody")).is_none());
        // first page: only live copies; the earliest is the best this page can do
        assert_eq!(best_match(&q, &guess("Queen", "Bohemian Rhapsody")).map(|m| m.year), Some(Some(1977)));
        let before = fixture("search_queen_bohemian_before_1977.json").unwrap();
        assert_eq!(best_match(&before, &guess("Queen", "Bohemian Rhapsody")).map(|m| m.recording), Some("b1a9c0e9-d987-4042-ae91-78d6a3267d69".into()));
        // same year: the most-tagged copy is taken as the original
        let twins = serde_json::json!({"recordings": [
            {"id": "copy", "score": 100, "title": "Song", "first-release-date": "1990", "artist-credit": [{"name": "Band", "artist": {"id": "b", "name": "Band"}}]},
            {"id": "orig", "score": 100, "title": "Song", "first-release-date": "1990-03-01", "tags": [{"name": "rock", "count": 2}], "artist-credit": [{"name": "Band", "artist": {"id": "b", "name": "Band"}}]}
        ]});
        assert_eq!(best_match(&twins, &guess("Band", "Song")).map(|m| m.recording), Some("orig".into()));

        let inline = serde_json::json!({"recordings": [
            {"id": "r1", "score": "95", "title": "Don't You (Forget About Me)", "first-release-date": "1985-02-20",
             "artist-credit": [{"name": "Simple Minds", "artist": {"id": "a1", "name": "Simple Minds"}}]},
            {"id": "r2", "score": 100, "title": "Africa Unite", "first-release-date": "1979",
             "artist-credit": [{"name": "Toto", "artist": {"id": "a2", "name": "Toto"}}]},
            {"id": "r3", "score": 100, "title": "Sunflower", "first-release-date": "2018-10-18",
             "artist-credit": [{"name": "Post Malone", "joinphrase": " & ", "artist": {"id": "a3", "name": "Post Malone"}}, {"name": "Swae Lee", "artist": {"id": "a4", "name": "Swae Lee"}}]},
            {"id": "r4", "score": 100, "title": "Everlong", "first-release-date": "1997-05-20",
             "artist-credit": [{"name": "Foo Fighters", "artist": {"id": "a5", "name": "Foo Fighters"}}]}
        ]});
        // string scores, bracketed title parts on either side
        assert_eq!(best_match(&inline, &guess("Simple Minds", "Don't You")).map(|m| m.recording), Some("r1".into()));
        assert_eq!(best_match(&inline, &guess("Simple Minds", "Don't You (Forget About Me)")).map(|m| m.year), Some(Some(1985)));
        // score 100 is not enough without the title
        assert!(best_match(&inline, &guess("Toto", "Africa")).is_none());
        // collaborations: the full credit is kept
        let m = best_match(&inline, &guess("Post Malone, Swae Lee", "Sunflower (Spider-Man: Into the Spider-Verse)")).unwrap();
        assert_eq!(m.artist, "Post Malone & Swae Lee");
        assert_eq!(best_match(&inline, &guess("Swae Lee", "Sunflower")).map(|m| m.recording), Some("r3".into()), "any credited artist");
        assert_eq!(best_match(&inline, &guess("foofighters", "Everlong")).map(|m| m.recording), Some("r4".into()), "squashed VEVO names");
        assert!(best_match(&inline, &guess("Foo", "Everlong")).is_none(), "a partial name is not the artist");
    }

    #[test]
    fn genre_and_year_extraction() {
        let rec = fixture("recording_c95fd3f9-1a2b-4c3d-8e4f-5a6b7c8d9e01.json").unwrap();
        assert_eq!(genres(&rec), ["dance-pop", "synth-pop", "pop"]);
        // no genres: top tags without the non-genre ones
        let artist = fixture("artist_b4d32cff-f19e-455f-86c4-f347d824ca61.json").unwrap();
        assert_eq!(genres(&artist), ["pop punk", "emo"]);
        let rg = fixture("release-group_a1b2c3d4-0000-4000-8000-000000000002.json").unwrap();
        assert_eq!(genres(&rg), ["progressive rock", "hard rock", "art rock", "pop rock", "rock"], "capped at five, by votes");
        let none = serde_json::json!({"genres": [{"name": "rock", "count": 0}], "tags": [{"name": "Seen Live", "count": 9}, {"name": "1980s", "count": 4}]});
        assert!(genres(&none).is_empty(), "zero-vote genres and junk tags don't count");
        let mixed = serde_json::json!({"genres": [{"name": "Hip Hop", "count": 2}, {"name": "hip hop", "count": 1}, {"name": "Trap", "count": 5}]});
        assert_eq!(genres(&mixed), ["trap", "hip hop"], "lowercase, deduped, most votes first");
        assert_eq!(year("1975-10-31"), Some(1975));
        assert_eq!(year("1987"), Some(1987));
        assert_eq!(year(""), None);
        assert_eq!(year("19"), None);
        assert_eq!(year("0000-01-01"), None);
    }

    #[tokio::test]
    async fn genres_fall_back_recording_release_group_artist() {
        // recording has tags → its own genres (search + 1 lookup)
        let f = Fake::default();
        let Outcome::Found { info, complete } = resolve(&f, "Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)", "Rick Astley").await else { panic!() };
        assert!(complete);
        assert_eq!(info.title, "Never Gonna Give You Up");
        assert_eq!(info.artist.as_deref(), Some("Rick Astley"));
        assert_eq!(info.genres, ["dance-pop", "synth-pop", "pop"]);
        assert_eq!(info.year, Some(1987));
        assert_eq!(f.calls().len(), 2);

        // more results than one page: narrow to "released before 1977" and find the 1975
        // original; it is untagged → release group genres, no recording lookup
        let f = Fake::default();
        let Outcome::Found { info, complete } = resolve(&f, "Queen – Bohemian Rhapsody (Official Video Remastered)", "Queen Official").await else { panic!() };
        assert!(complete);
        assert_eq!(info.recording.as_deref(), Some("b1a9c0e9-d987-4042-ae91-78d6a3267d69"));
        assert_eq!(info.genres[..2], ["progressive rock", "hard rock"]);
        assert_eq!(info.year, Some(1975));
        let calls = f.calls();
        assert!(calls[1].ends_with(r#"AND firstreleasedate:[* TO 1976-12-31]"#), "{calls:?}");
        assert_eq!(&calls[2..], ["release-group/a1b2c3d4-0000-4000-8000-000000000002"], "the refined page holds every result: no further narrowing");

        // reversed title: the first guess finds nothing, the swapped one matches; its only
        // release group is a soundtrack (not the song's genres) → artist tags
        let f = Fake::default();
        let Outcome::Found { info, complete } = resolve(&f, "Bohemian Rhapsody - Panic! At The Disco (Suicide Squad OST)", "Fueled By Ramen").await else { panic!() };
        assert!(complete);
        assert_eq!(info.artist.as_deref(), Some("Panic! at the Disco"));
        assert_eq!(info.genres, ["pop punk", "emo"]);
        assert_eq!(info.year, Some(2016));
        let calls = f.calls();
        assert_eq!(calls.len(), 3, "{calls:?}");
        assert!(calls[0].contains(r#"artist:"Bohemian Rhapsody""#) && calls[1].contains(r#"artist:"Panic! At The Disco""#));
        assert_eq!(calls[2], "artist/b4d32cff-f19e-455f-86c4-f347d824ca61");

        // nothing confident anywhere
        let f = Fake::default();
        assert_eq!(resolve(&f, "Ten Hours of Drum Solos", "Drum Archive").await, Outcome::NoMatch);
        // nothing to ask about: no request at all
        let f = Fake::default();
        assert_eq!(resolve(&f, "Some Song", "").await, Outcome::NoMatch);
        assert!(f.calls().is_empty());
        // network down
        let f = Fake::default();
        f.down.store(true, Ordering::SeqCst);
        assert!(matches!(resolve(&f, "Rick Astley - Never Gonna Give You Up", "Rick Astley").await, Outcome::Failed(_)));
    }

    #[tokio::test]
    async fn cache_hits_misses_and_retries() {
        let store = Store::open(Db::memory().unwrap()).unwrap();
        let rick = Job { video: "dQw4w9WgXcQ".into(), title: "Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)".into(), channel: "Rick Astley".into() };
        let t0 = 1_800_000_000;
        // miss → lookup → cached hit
        let f = Fake::default();
        let first = process(&store, &f, &rick, t0).await.unwrap();
        assert_eq!(first.genres, ["dance-pop", "synth-pop", "pop"]);
        assert_eq!(f.calls().len(), 2);
        let row = store.song_meta("dQw4w9WgXcQ").unwrap();
        assert_eq!((row.retry_at, row.version), (t0 + HIT_TTL_S, VERSION));
        // hit: no requests, same answer
        let f = Fake::default();
        assert_eq!(process(&store, &f, &rick, t0 + 86_400).await, Some(first.clone()));
        assert!(f.calls().is_empty());
        // stale hit + MusicBrainz down: keep showing the old match, retry in an hour
        let f = Fake::default();
        f.down.store(true, Ordering::SeqCst);
        assert_eq!(process(&store, &f, &rick, t0 + HIT_TTL_S).await, Some(first.clone()));
        assert_eq!(f.calls().len(), 1);
        assert_eq!(store.song_meta("dQw4w9WgXcQ").unwrap().retry_at, t0 + HIT_TTL_S + ERROR_RETRY_S);
        let f = Fake::default();
        assert_eq!(process(&store, &f, &rick, t0 + HIT_TTL_S + 60).await, Some(first.clone()));
        assert!(f.calls().is_empty(), "no retry before retry_at");

        // a miss is cached too, with its own retry-after
        let drums = Job { video: "LongSong001".into(), title: "Ten Hours of Drum Solos".into(), channel: "Drum Archive".into() };
        let f = Fake::default();
        assert_eq!(process(&store, &f, &drums, t0).await, None);
        assert_eq!(f.calls().len(), 1);
        let row = store.song_meta("LongSong001").unwrap();
        assert!(row.info.is_none());
        assert_eq!(row.retry_at, t0 + MISS_RETRY_S);
        let f = Fake::default();
        assert_eq!(process(&store, &f, &drums, t0 + MISS_RETRY_S - 1).await, None);
        assert!(f.calls().is_empty(), "cached miss");
        let f = Fake::default();
        process(&store, &f, &drums, t0 + MISS_RETRY_S).await;
        assert_eq!(f.calls().len(), 1, "retried once due");

        // rows from an older matcher are looked up again
        store.put_song_meta("dQw4w9WgXcQ", Some(&first), t0, t0 + HIT_TTL_S).unwrap();
        store.db().with(|c| c.execute("UPDATE song_meta SET version = 0", [])).unwrap();
        assert!(store.song_meta("dQw4w9WgXcQ").unwrap().due(t0));
    }

    #[test]
    fn entry_fields() {
        let v = SongInfo { title: "Numb".into(), artist: Some("Linkin Park".into()), genres: vec!["nu metal".into()], year: Some(2003), recording: None }.with_fields(Value::map());
        assert_eq!(v.get_path("artist").and_then(Value::as_str), Some("Linkin Park"));
        assert_eq!(v.get_path("genres.0").and_then(Value::as_str), Some("nu metal"));
        assert_eq!(v.get_path("year").and_then(Value::as_i64), Some(2003));
        let v = SongInfo::parsed("Ten Hours of Drum Solos", "Drum Archive").with_fields(Value::map());
        assert!(v.get_path("artist").unwrap().is_null() && v.get_path("year").unwrap().is_null());
        assert_eq!(v.get_path("genres"), Some(&Value::List(vec![])));
    }
}
