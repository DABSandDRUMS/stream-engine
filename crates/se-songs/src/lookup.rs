//! Request text → candidate videos, spending as little quota as possible (§13.1):
//! library/cache first, `videos.list` (1 unit) for links, `search.list` (100 units) only for
//! uncached text while the ledger allows it, and graceful fallback to the library.

use crate::policy::Reject;
use crate::quota::{Kind, Ledger};
use crate::store::{Cached, Store};
use crate::text;
use crate::youtube::{self, ApiError, Client};
use jiff::Timestamp;
use std::sync::Arc;

/// Query-cache entries older than this are re-searched when search quota is available.
const QUERY_TTL_S: i64 = 30 * 86_400;
/// Minimum library match score for text requests answered from the library.
const LIBRARY_MIN_SCORE: f32 = 0.75;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Fresh cache hit (0 units).
    Cache,
    /// Fetched from the API now.
    Api,
    /// Answered from the library because the API couldn't be used (stale metadata allowed).
    Library,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Cache => "cache",
            Source::Api => "api",
            Source::Library => "library",
        }
    }
}

#[derive(Debug)]
pub struct Found {
    /// In preference order; the first that passes every check is queued.
    pub candidates: Vec<Cached>,
    pub source: Source,
}

#[derive(Debug)]
pub struct Failed {
    pub reject: Reject,
    /// The API problem behind it (for health/UI), if any.
    pub api: Option<ApiError>,
}

impl Failed {
    fn plain(r: Reject) -> Failed {
        Failed { reject: r, api: None }
    }
}

/// Everything a lookup needs; cheap to clone into a task.
#[derive(Clone)]
pub struct Lookup {
    pub yt: Arc<Client>,
    pub store: Store,
    pub ledger: Ledger,
    pub key: Option<String>,
    pub region: String,
    pub cache_days: u32,
    pub search_results: u32,
    pub library_only: bool,
}

impl Lookup {
    fn fresh(&self, c: &Cached) -> bool {
        text::now_ms() / 1000 - c.fetched_at < self.cache_days as i64 * 86_400
    }

    /// Resolve request text (a link, an id, or a search) into candidates.
    pub async fn resolve(&self, input: &str) -> Result<Found, Failed> {
        match youtube::extract_video_id(input) {
            Some(id) => self.by_id(&id).await,
            None => self.by_text(input).await,
        }
    }

    async fn by_id(&self, id: &str) -> Result<Found, Failed> {
        let now = Timestamp::now();
        let cached = self.store.video(id);
        if let Some(c) = &cached
            && (self.fresh(c) || self.library_only)
        {
            return Ok(Found { candidates: vec![c.clone()], source: Source::Cache });
        }
        let stale = |api: Option<ApiError>, otherwise: Reject| match &cached {
            Some(c) => Ok(Found { candidates: vec![c.clone()], source: Source::Library }),
            None => Err(Failed { reject: otherwise, api }),
        };
        if self.library_only {
            return stale(None, Reject::NotInLibrary);
        }
        let Some(key) = self.key.as_deref() else { return stale(None, Reject::NoKey) };
        if !self.ledger.can_list(now) {
            return stale(None, Reject::LookupsPaused);
        }
        let _ = self.ledger.charge(now, Kind::List);
        match self.yt.videos(key, &[id.to_string()]).await {
            Ok(v) if v.is_empty() => Err(Failed::plain(Reject::NotFound)),
            Ok(v) => {
                let _ = self.store.put_videos(&v);
                match self.store.video(id) {
                    Some(c) => Ok(Found { candidates: vec![c], source: Source::Api }),
                    None => Err(Failed::plain(Reject::Lookup("library write failed".into()))),
                }
            }
            Err(e) => {
                if e == ApiError::QuotaExceeded {
                    let _ = self.ledger.mark_exhausted(now);
                    return stale(Some(e), Reject::LookupsPaused);
                }
                let msg = e.to_string();
                stale(Some(e), Reject::Lookup(msg))
            }
        }
    }

    async fn by_text(&self, input: &str) -> Result<Found, Failed> {
        let now = Timestamp::now();
        let key_q = text::query_key(input);
        if key_q.is_empty() {
            return Err(Failed::plain(Reject::Empty));
        }
        let can_search = !self.library_only && self.key.is_some() && self.ledger.can_search(now);
        if let Some((ids, at)) = self.store.query_ids(&key_q)
            && (text::now_ms() / 1000 - at < QUERY_TTL_S || !can_search)
        {
            if ids.is_empty() {
                return Err(Failed::plain(Reject::NoResults));
            }
            self.refresh_stale(&ids, now).await;
            let candidates: Vec<Cached> = ids.iter().filter_map(|id| self.store.video(id)).collect();
            if !candidates.is_empty() {
                return Ok(Found { candidates, source: Source::Cache });
            }
        }
        if !can_search {
            return self.library_fallback(&key_q, None, now);
        }
        let key = self.key.as_deref().unwrap_or_default();
        let _ = self.ledger.charge(now, Kind::Search);
        let q: String = input.trim().chars().take(text::MAX_QUERY_CHARS).collect();
        let ids = match self.yt.search(key, &q, &self.region, self.search_results).await {
            Ok(ids) => ids,
            Err(e) => {
                if e == ApiError::QuotaExceeded {
                    let _ = self.ledger.mark_exhausted(now);
                }
                return self.library_fallback(&key_q, Some(e), now);
            }
        };
        let _ = self.store.put_query(&key_q, &ids);
        if ids.is_empty() {
            return Err(Failed::plain(Reject::NoResults));
        }
        self.refresh_stale(&ids, now).await;
        let candidates: Vec<Cached> = ids.iter().filter_map(|id| self.store.video(id)).collect();
        if candidates.is_empty() {
            return Err(Failed::plain(Reject::Lookup("no metadata for search results".into())));
        }
        Ok(Found { candidates, source: Source::Api })
    }

    /// One `videos.list` call (1 unit) for ids that are missing or stale, when allowed.
    async fn refresh_stale(&self, ids: &[String], now: Timestamp) {
        if self.library_only {
            return;
        }
        let Some(key) = self.key.as_deref() else { return };
        let need: Vec<String> = ids.iter().filter(|id| self.store.video(id).is_none_or(|c| !self.fresh(&c))).cloned().collect();
        if need.is_empty() || !self.ledger.can_list(now) {
            return;
        }
        let _ = self.ledger.charge(now, Kind::List);
        match self.yt.videos(key, &need).await {
            Ok(v) => {
                let _ = self.store.put_videos(&v);
            }
            Err(ApiError::QuotaExceeded) => {
                let _ = self.ledger.mark_exhausted(now);
            }
            Err(e) => tracing::warn!("refresh video metadata: {e}"),
        }
    }

    fn library_fallback(&self, key_q: &str, api: Option<ApiError>, now: Timestamp) -> Result<Found, Failed> {
        let hits = self.store.search_library(key_q, LIBRARY_MIN_SCORE);
        if !hits.is_empty() {
            return Ok(Found { candidates: hits.into_iter().take(self.search_results as usize).collect(), source: Source::Library });
        }
        let reject = if self.library_only {
            Reject::NotInLibrary
        } else if self.key.is_none() {
            Reject::NoKey
        } else if matches!(api, Some(ApiError::QuotaExceeded)) || !self.ledger.can_list(now) {
            if self.ledger.can_list(now) { Reject::SearchPaused } else { Reject::LookupsPaused }
        } else if let Some(e) = &api {
            Reject::Lookup(e.to_string())
        } else {
            Reject::SearchPaused
        };
        Err(Failed { reject, api })
    }
}
