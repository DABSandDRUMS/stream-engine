"use strict";

const NATIVE_HOST = "com.stream_engine.apple_music";
const APPLE_ORIGIN = "https://music.apple.com";
const RECONNECT_ALARM = "apple-music-native-reconnect";
const ACTIONS = new Set(["play", "pause", "next", "previous", "volume_up", "volume_down", "status"]);
let nativePort = null;
let requests = Promise.resolve();

function errorMessage(error) {
  return typeof error?.message === "string" ? error.message : String(error);
}

function isAppleUrl(url) {
  try {
    return new URL(url).origin === APPLE_ORIGIN;
  } catch {
    return false;
  }
}

async function selectAppleTab() {
  const tabs = (await chrome.tabs.query({ url: `${APPLE_ORIGIN}/*` }))
    .filter((tab) => Number.isInteger(tab.id) && isAppleUrl(tab.url));
  if (tabs.length === 0) {
    throw new Error("No Apple Music tab is open at https://music.apple.com.");
  }
  if (tabs.length === 1) {
    return tabs[0];
  }

  const windowIds = [...new Set(tabs.map((tab) => tab.windowId))];
  const windows = await Promise.all(windowIds.map((id) => chrome.windows.get(id)));
  const appWindowIds = new Set(windows
    .filter((window) => window.type === "popup" || window.type === "app")
    .map((window) => window.id));
  const appTabs = tabs.filter((tab) => appWindowIds.has(tab.windowId));
  if (appTabs.length === 1) {
    return appTabs[0];
  }
  throw new Error(
    `Apple Music target is ambiguous: ${tabs.length} tabs, ${appTabs.length} in app/popup windows. Close extra Apple Music tabs or leave one app window.`
  );
}

// This function is serialized by Chromium into the Apple tab's MAIN world.
// It must not reference service-worker variables or configure/authorize MusicKit.
async function controlMusicKit(action) {
  let music;
  let player;

  function messageOf(error) {
    return typeof error?.message === "string" ? error.message : String(error);
  }

  function propertyType(object, name) {
    try {
      return typeof object?.[name];
    } catch {
      return "throws";
    }
  }

  function diagnostics() {
    const namespace = (() => {
      try {
        return globalThis.MusicKit;
      } catch {
        return undefined;
      }
    })();
    const apis = [
      `MusicKit=${propertyType(globalThis, "MusicKit")}`,
      `MusicKit.getInstance=${propertyType(namespace, "getInstance")}`,
      `navigator.mediaSession=${propertyType(navigator, "mediaSession")}`,
      `HTMLMediaElement=${propertyType(globalThis, "HTMLMediaElement")}`,
      `audio elements=${document.querySelectorAll("audio").length}`,
      `video elements=${document.querySelectorAll("video").length}`,
    ];
    if (music) {
      for (const name of ["play", "pause", "isPlaying", "volume", "player"]) {
        apis.push(`MusicKit instance.${name}=${propertyType(music, name)}`);
      }
      let nestedPlayer;
      try {
        nestedPlayer = music.player;
      } catch {
        nestedPlayer = undefined;
      }
      if (nestedPlayer) {
        for (const name of ["play", "pause", "isPlaying", "volume"]) {
          apis.push(`MusicKit instance.player.${name}=${propertyType(nestedPlayer, name)}`);
        }
      }
    }
    // Names only: do not inspect account data, cookies, or arbitrary global values.
    const relatedGlobals = Object.getOwnPropertyNames(globalThis)
      .filter((name) => /music|player/i.test(name)).slice(0, 30);
    apis.push(`music/player globals=${relatedGlobals.join(",") || "none"}`);
    return `Available APIs: ${apis.join("; ")}. No alternative player was controlled.`;
  }

  function hasState(candidate) {
    return typeof candidate?.isPlaying === "boolean"
      && typeof candidate?.volume === "number"
      && Number.isFinite(candidate.volume)
      && candidate.volume >= 0 && candidate.volume <= 1;
  }

  function readStatus() {
    if (!hasState(player)) {
      throw new Error("MusicKit did not expose valid isPlaying and volume state.");
    }
    const status = { playing: player.isPlaying, volume: player.volume };
    const item = player.nowPlayingItem ?? music.nowPlayingItem;
    const title = item?.title ?? item?.attributes?.name;
    if (typeof title === "string") {
      status.title = title;
    }
    return status;
  }

  async function callWithDeadline(owner, method, shouldPlay) {
    let timer;
    let expired = false;
    try {
      await Promise.race([
        (async () => {
          await owner[method]();
          // MusicKit's method promise can resolve before playback state updates.
          while (!expired && shouldPlay !== undefined && readStatus().playing !== shouldPlay) {
            await new Promise((resolve) => setTimeout(resolve, 50));
          }
        })(),
        new Promise((_, reject) => {
          timer = setTimeout(() => {
            expired = true;
            reject(new Error(
              `MusicKit.${method} did not finish within 5 seconds; playback outcome is unknown. The command will not be retried.`
            ));
          }, 5000);
        }),
      ]);
    } finally {
      clearTimeout(timer);
    }
  }

  try {
    if (location.origin !== "https://music.apple.com") {
      return { ok: false, error: "The selected tab left the exact Apple Music origin; no command was sent." };
    }
    if (!globalThis.MusicKit || typeof globalThis.MusicKit.getInstance !== "function") {
      throw new Error("MusicKit.getInstance is unavailable in the Apple Music page MAIN world.");
    }
    music = globalThis.MusicKit.getInstance();
    // MusicKit exposes playback state on the instance in some versions and on
    // its documented player in others. Never substitute a DOM/media-key toggle.
    if (hasState(music)) {
      player = music;
    } else if (hasState(music?.player)) {
      player = music.player;
    } else {
      throw new Error("MusicKit playback/volume API is unavailable or not initialized.");
    }

    const before = readStatus();
    let requestedVolume;
    if (action === "play" || action === "pause") {
      const shouldPlay = action === "play";
      if (before.playing !== shouldPlay) {
        if (shouldPlay && !player.nowPlayingItem && !music.nowPlayingItem) {
          throw new Error("Choose a song or album in Apple Music first, then press Play.");
        }
        const owner = typeof music[action] === "function" ? music : player;
        if (typeof owner[action] !== "function") {
          throw new Error(`MusicKit.${action} is unavailable.`);
        }
        await callWithDeadline(owner, action, shouldPlay);
      }
    } else if (action === "next" || action === "previous") {
      if (!player.nowPlayingItem && !music.nowPlayingItem) {
        throw new Error("Choose a song or album in Apple Music first, then use Forward or Back.");
      }
      const method = action === "next" ? "skipToNextItem" : "skipToPreviousItem";
      const owner = typeof music[method] === "function" ? music : player;
      if (typeof owner[method] !== "function") {
        throw new Error(`MusicKit.${method} is unavailable.`);
      }
      // MusicKit owns queue boundaries and playback behavior for track skips.
      await callWithDeadline(owner, method);
    } else if (action === "volume_up" || action === "volume_down") {
      const step = action === "volume_up" ? 0.05 : -0.05;
      requestedVolume = Math.max(0, Math.min(1, before.volume + step));
      player.volume = requestedVolume;
    } else if (action !== "status") {
      throw new Error(`Unsupported Apple Music action: ${action}.`);
    }

    const status = readStatus();
    if ((action === "play" && !status.playing) || (action === "pause" && status.playing)) {
      throw new Error(`MusicKit.${action} returned without reaching the requested playback state.`);
    }
    if (requestedVolume !== undefined && Math.abs(status.volume - requestedVolume) > 0.000001) {
      throw new Error(`MusicKit volume did not reach ${requestedVolume}; actual volume is ${status.volume}.`);
    }
    return { ok: true, status };
  } catch (error) {
    const response = { ok: false, error: `${messageOf(error)} ${diagnostics()}` };
    if (player) {
      try {
        response.status = readStatus();
      } catch {
        // An unavailable API cannot provide real playback state.
      }
    }
    return response;
  }
}

async function respondToRequest(message, port) {
  const id = typeof message?.id === "string" ? message.id : "";
  try {
    if (!id) {
      throw new Error("Native request must contain a non-empty string id.");
    }
    if (!ACTIONS.has(message?.action)) {
      throw new Error("Unsupported action; expected play, pause, next, previous, volume_up, volume_down, or status.");
    }
    const tab = await selectAppleTab();
    // A disconnected host's queued commands are discarded, not replayed on its replacement.
    if (nativePort !== port) {
      return null;
    }
    const results = await chrome.scripting.executeScript({
      target: { tabId: tab.id, frameIds: [0] },
      world: "MAIN",
      func: controlMusicKit,
      args: [message.action],
    });
    const response = results[0]?.result;
    if (!response || typeof response.ok !== "boolean") {
      throw new Error("Apple Music page returned no usable response; it may have closed or navigated.");
    }
    // Closing/navigating the tab during a command is an error, not stale success.
    const currentTab = await chrome.tabs.get(tab.id);
    if (!isAppleUrl(currentTab.url)) {
      throw new Error("Apple Music tab navigated away while the command was executing.");
    }
    return { id, ...response };
  } catch (error) {
    return { id, ok: false, error: errorMessage(error) };
  }
}

function connectNativeHost() {
  if (nativePort) {
    return;
  }
  let port;
  try {
    port = chrome.runtime.connectNative(NATIVE_HOST);
    nativePort = port;
  } catch (error) {
    console.error(`Apple Music native host connection failed: ${errorMessage(error)}`);
    return;
  }
  port.onMessage.addListener((message) => {
    requests = requests.then(async () => {
      if (nativePort !== port) {
        return;
      }
      const response = await respondToRequest(message, port);
      if (response && nativePort === port) {
        port.postMessage(response);
      }
    }).catch((error) => {
      console.error(`Apple Music native response failed: ${errorMessage(error)}`);
    });
  });
  port.onDisconnect.addListener(() => {
    const error = chrome.runtime.lastError;
    if (nativePort === port) {
      nativePort = null;
    }
    if (error) {
      console.error(`Apple Music native host disconnected: ${error.message}`);
    }
    // The repeating alarm reconnects even if this service worker is suspended.
  });
}

async function startConnection() {
  try {
    if (!(await chrome.alarms.get(RECONNECT_ALARM))) {
      await chrome.alarms.create(RECONNECT_ALARM, { periodInMinutes: 1 });
    }
  } catch (error) {
    console.error(`Apple Music reconnect alarm failed: ${errorMessage(error)}`);
  }
  connectNativeHost();
}

chrome.alarms.onAlarm.addListener((alarm) => {
  if (alarm.name === RECONNECT_ALARM) {
    connectNativeHost();
  }
});
chrome.runtime.onStartup.addListener(startConnection);
chrome.runtime.onInstalled.addListener(startConnection);
// Opening the native port never plays, pauses, or adjusts volume.
startConnection();
