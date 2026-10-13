import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";

const source = readFileSync(new URL("./apple-music/background.js", import.meta.url), "utf8");

function controller(music) {
  const context = vm.createContext({
    MusicKit: { getInstance: () => music },
    location: { origin: "https://music.apple.com" },
    navigator: {},
    document: { querySelectorAll: () => [] },
    setTimeout,
    clearTimeout,
    console,
    chrome: {
      alarms: { get: async () => ({}), onAlarm: { addListener() {} } },
      runtime: {
        connectNative: () => ({ onMessage: { addListener() {} }, onDisconnect: { addListener() {} } }),
        onStartup: { addListener() {} },
        onInstalled: { addListener() {} },
      },
    },
  });
  vm.runInContext(source, context);
  return (action) => context.controlMusicKit(action);
}

test("toggle follows actual MusicKit playback and waits for delayed state", async () => {
  const item = { title: "Selected track" };
  const player = { isPlaying: false, volume: 0.13, nowPlayingItem: item };
  const music = {
    player,
    async play() { setTimeout(() => { player.isPlaying = true; }, 10); },
    async pause() { setTimeout(() => { player.isPlaying = false; }, 10); },
  };
  const control = controller(music);
  let result = await control("toggle");
  assert.equal(result.ok, true);
  assert.equal(result.status.playing, true);
  // An external pause must override the previous button result, not a local toggle flag.
  player.isPlaying = false;
  result = await control("toggle");
  assert.equal(result.ok, true);
  assert.equal(result.status.playing, true);
  result = await control("toggle");
  assert.equal(result.ok, true);
  assert.equal(result.status.playing, false);
  assert.equal(player.volume, 0.13);
  assert.equal(player.nowPlayingItem, item);
});

test("toggle refuses to start an empty queue but can pause it", async () => {
  const music = {
    isPlaying: false,
    volume: 0.5,
    async play() { throw new Error("must not start without a selected track"); },
    async pause() { this.isPlaying = false; },
  };
  const control = controller(music);
  const refused = await control("toggle");
  assert.equal(refused.ok, false);
  assert.match(refused.error, /Choose a song or album/);
  assert.equal(music.isPlaying, false);
  music.isPlaying = true;
  const paused = await control("toggle");
  assert.equal(paused.ok, true);
  assert.equal(paused.status.playing, false);
});
