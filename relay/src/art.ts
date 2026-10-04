// Album art for the public queue page. The local queue service looks each queued video up
// through the iTunes Search API using only its public YouTube title and channel. Matches must
// agree on artist and song, so covers and similarly named tracks don't borrow the wrong art;
// misses fall back to YouTube thumbnails on the page.

export interface Art {
  url: string;
  artist: string;
  song: string;
  album: string;
}

export interface Query {
  artist: string;
  song: string;
}

export interface Video {
  video: string;
  title: string;
  channel: string;
}

const BRACKETS = /\s*(?:\([^)]*\)|\[[^\]]*\]|\{[^}]*\}|【[^】]*】)/g;
const FEAT = /\s+(?:ft\.?|feat\.?|featuring)\s.*$/i;
const MAX_KNOWN = 1000;
const MAX_QUEUED = 200;
const RETRY_MS = 10 * 60_000;
const GAP_MS = 250;
const TIMEOUT_MS = 6000;

/** Comparable form: no accents, case, punctuation, `&`/`and` difference, or leading "the". */
export function norm(s: string): string {
  return s
    .normalize("NFKD")
    .replace(/[\u0300-\u036f]/g, "")
    .toLowerCase()
    .replace(/&/g, "and")
    .replace(/[^a-z0-9]/g, "")
    .replace(/^the(?=.)/, "");
}

function clean(s: string): string {
  return s.replace(BRACKETS, "").replace(/["“”]/g, "").replace(/\s+/g, " ").trim();
}

/** Artist and song from an `Artist - Song (Official Video)` title. */
export function parseVideo(title: string): Query | null {
  const m = /^(.+?)\s+[-–—]\s+(.+)$/.exec(clean(title).replace(/\s+\|.*$/, ""));
  if (!m) return null;
  const artist = (m[1] as string).replace(FEAT, "").trim();
  const song = (m[2] as string).replace(FEAT, "").trim();
  return norm(artist).length >= 2 && norm(song) ? { artist, song } : null;
}

/** iTunes search term plus the rule a result must pass; `accept` takes `norm`ed values. */
export interface Match {
  term: string;
  accept: (artist: string, song: string) => boolean;
}

export function match(title: string, channel: string): Match | null {
  const q = parseVideo(title);
  if (q) {
    const qa = norm(q.artist);
    const qs = norm(q.song);
    return { term: `${q.artist} ${q.song}`, accept: (a, s) => (a.includes(qa) || qa.includes(a)) && s === qs };
  }
  const t = clean(title).replace(/\s+\|.*$/, "").replace(FEAT, "");
  const whole = norm(t);
  if (whole.length < 3) return null;
  const ch = norm(channel.replace(/\s*-\s*Topic$/i, "").replace(/VEVO$/i, ""));
  // No separator: the title must hold exactly artist + song (`Converge Bitter and Then Some`),
  // or be the song from the artist's own channel (`Artist - Topic`, `ArtistVEVO`). Label
  // channels never count as the artist.
  return {
    term: t,
    accept: (a, s) => a + s === whole || s + a === whole || (s === whole && ch.length >= 2 && (a.includes(ch) || ch.includes(a))),
  };
}

/** The first iTunes song result `accept`ed by its artist and bracket-free track name. */
export function pick(results: unknown, accept: Match["accept"]): Art | null {
  if (!Array.isArray(results)) return null;
  for (const r of results) {
    if (typeof r !== "object" || r === null) continue;
    const { kind, artistName, trackName, collectionName, artworkUrl100 } = r as Record<string, unknown>;
    if (kind !== "song" || typeof artistName !== "string" || typeof trackName !== "string" || typeof artworkUrl100 !== "string") continue;
    const a = norm(artistName);
    const song = clean(trackName);
    if (a.length < 2 || !accept(a, norm(song))) continue;
    let url: URL;
    try {
      url = new URL(artworkUrl100.replace(/\/\d+x\d+bb\.(?:jpg|png|webp)$/, "/600x600bb.jpg"));
    } catch {
      continue;
    }
    if (url.protocol !== "https:" || !url.hostname.endsWith(".mzstatic.com")) continue;
    return { url: url.href, artist: artistName, song: song || trackName, album: typeof collectionName === "string" ? clean(collectionName) : "" };
  }
  return null;
}

/** Serial, cached lookups; `changed` runs after new art is found. */
export class ArtLookup {
  private readonly known = new Map<string, Art | null>();
  private readonly retryAt = new Map<string, number>();
  private readonly queue: Video[] = [];
  private readonly queued = new Set<string>();
  private running = false;

  constructor(
    private readonly changed: () => void,
    private readonly fetcher: typeof fetch = fetch,
  ) {}

  art(video: string): Art | null {
    return this.known.get(video) ?? null;
  }

  want(videos: Iterable<Video>): void {
    const now = Date.now();
    for (const v of videos) {
      if (!v.video || this.known.has(v.video) || this.queued.has(v.video) || this.queue.length >= MAX_QUEUED) continue;
      if ((this.retryAt.get(v.video) ?? 0) > now) continue;
      this.queued.add(v.video);
      this.queue.push(v);
    }
    void this.drain();
  }

  private async drain(): Promise<void> {
    if (this.running) return;
    this.running = true;
    try {
      for (let v = this.queue.shift(); v; v = this.queue.shift()) {
        try {
          const found = await this.lookup(v);
          this.retryAt.delete(v.video);
          if (this.known.size >= MAX_KNOWN) this.known.delete(this.known.keys().next().value as string);
          this.known.set(v.video, found);
          if (found) this.changed();
        } catch {
          if (this.retryAt.size >= MAX_KNOWN) this.retryAt.clear();
          this.retryAt.set(v.video, Date.now() + RETRY_MS);
        } finally {
          this.queued.delete(v.video);
        }
        const { promise, resolve } = Promise.withResolvers<void>();
        setTimeout(resolve, GAP_MS);
        await promise;
      }
    } finally {
      this.running = false;
    }
  }

  private async lookup(v: Video): Promise<Art | null> {
    const m = match(v.title, v.channel);
    if (!m) return null;
    const url = new URL("https://itunes.apple.com/search");
    url.search = new URLSearchParams({ term: m.term, entity: "song", limit: "10", country: "US" }).toString();
    const res = await this.fetcher(url, { signal: AbortSignal.timeout(TIMEOUT_MS), headers: { accept: "application/json" } });
    if (!res.ok) throw new Error(`iTunes search ${res.status}`);
    const body = (await res.json()) as { results?: unknown };
    return pick(body.results, m.accept);
  }
}
