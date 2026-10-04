import { describe, expect, it } from "vitest";
import { match, parseVideo, pick } from "../src/art";

const song = (artistName: string, trackName: string, id: string) => ({
  kind: "song",
  artistName,
  trackName,
  collectionName: `${trackName} (Deluxe)`,
  artworkUrl100: `https://is1-ssl.mzstatic.com/image/thumb/Music/${id}.jpg/100x100bb.jpg`,
});

const accept = (title: string, channel: string) => {
  const m = match(title, channel);
  if (!m) throw new Error(`no match rule for ${title}`);
  return m.accept;
};

describe("album art", () => {
  it("parses artist and song from separated YouTube titles", () => {
    expect(parseVideo("Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)")).toEqual({
      artist: "Rick Astley",
      song: "Never Gonna Give You Up",
    });
    expect(parseVideo("Metallica - Enter Sandman feat. Someone")).toEqual({ artist: "Metallica", song: "Enter Sandman" });
    expect(parseVideo("Bitter Sweet Symphony [Official HD]")).toBeNull();
  });

  it("skips covers and other songs by the same artist", () => {
    const ok = accept("The Verve - Bitter Sweet Symphony (Official Music Video)", "TheVerveVEVO");
    const art = pick([song("David Garrett", "Bitter Sweet Symphony", "cover"), song("The Verve", "Bitter Sweet Symphony", "verve")], ok);
    expect(art?.artist).toBe("The Verve");
    expect(art?.url).toBe("https://is1-ssl.mzstatic.com/image/thumb/Music/verve.jpg/600x600bb.jpg");
    expect(pick([song("David Garrett", "Bitter Sweet Symphony", "cover")], ok)).toBeNull();
    expect(pick([song("The Verve", "Lucky Man", "other")], ok)).toBeNull();
  });

  it("matches unseparated titles only when they contain the artist or come from the artist channel", () => {
    const label = accept("Converge Bitter And Then Some", "Equal Vision Records");
    expect(pick([song("Converge", "Bitter and Then Some (Live)", "live")], label)?.song).toBe("Bitter and Then Some");
    expect(pick([song("Someone Else", "Converge Bitter And Then Some", "wrong")], label)).toBeNull();
    const vevo = accept("Bitter Sweet Symphony [Official HD]", "TheVerveVEVO");
    expect(pick([song("David Garrett", "Bitter Sweet Symphony", "cover"), song("The Verve", "Bitter Sweet Symphony", "verve")], vevo)?.artist).toBe("The Verve");
    expect(pick([song("The Verve", "Bitter Sweet Symphony", "x")], accept("Bitter Sweet Symphony", "Some Label"))).toBeNull();
  });

  it("rejects artwork from untrusted hosts", () => {
    const ok = accept("Converge - Bitter and Then Some", "Converge");
    expect(pick([{ ...song("Converge", "Bitter and Then Some", "x"), artworkUrl100: "https://evil.example/100x100bb.jpg" }], ok)).toBeNull();
  });
});
