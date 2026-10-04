// Injected by CEF only into the queue's real, empty YouTube iframe.
(() => {
  "use strict";
  if (window.__seYoutubeAccountBridge) return;
  window.__seYoutubeAccountBridge = true;
  const policy = window.__seYoutubeAccountPolicy;
  const parentOrigin = new URL(document.referrer).origin;
  const configFrom = html => {
    const match = html.match(/ytcfg\.set\((\{[\s\S]*?\})\);/);
    if (!match) throw new Error("YouTube account metadata unavailable");
    return JSON.parse(match[1]);
  };
  const selectedChannel = data => {
    const ids = new Set();
    const visit = value => {
      if (!value || typeof value !== "object") return;
      if (value.activeAccountHeaderRenderer) {
        for (const run of value.activeAccountHeaderRenderer.manageAccountTitle?.runs || []) {
          const id = run.navigationEndpoint?.browseEndpoint?.browseId;
          if (typeof id === "string" && /^UC[\w-]{22}$/.test(id)) ids.add(id);
        }
      }
      for (const child of Object.values(value)) visit(child);
    };
    visit(data);
    if (ids.size !== 1) throw new Error("Selected YouTube channel is unknown");
    return [...ids][0];
  };
  window.addEventListener("message", async event => {
    const request = event.data;
    if (event.source !== parent || event.origin !== parentOrigin || request?.type !== "se.youtube.account.check" || typeof request.nonce !== "string") return;
    const result = {type: "se.youtube.account.result", nonce: request.nonce, verified: false, channel: "", delegate: "", error: ""};
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), 8000);
    try {
      if (!policy?.channel || !policy?.delegate || request.required_channel !== policy.channel || request.required_delegate !== policy.delegate) throw new Error("YouTube account lock is not configured");
      const response = await fetch("/", {credentials: "include", cache: "no-store", signal: controller.signal});
      if (!response.ok || new URL(response.url).origin !== location.origin) throw new Error("YouTube session unavailable");
      const config = configFrom(await response.text());
      result.delegate = String(config.DELEGATED_SESSION_ID || "");
      if (config.LOGGED_IN !== true || result.delegate !== policy.delegate) throw new Error("YouTube is signed out or the selected account is not Covers");
      const cookies = document.cookie.split(";").map(value => value.trim());
      const cookie = cookies.find(value => value.startsWith("SAPISID=")) || cookies.find(value => value.startsWith("__Secure-3PAPISID="));
      if (!cookie) throw new Error("YouTube account authentication unavailable");
      const timestamp = Math.floor(Date.now() / 1000);
      const secret = cookie.slice(cookie.indexOf("=") + 1);
      const digest = await crypto.subtle.digest("SHA-1", new TextEncoder().encode(`${timestamp} ${secret} ${location.origin}`));
      const hash = Array.from(new Uint8Array(digest), value => value.toString(16).padStart(2, "0")).join("");
      const context = config.INNERTUBE_CONTEXT;
      if (!context?.client) throw new Error("YouTube account context unavailable");
      context.user = {...context.user, onBehalfOfUser: policy.delegate};
      const menu = await fetch("/youtubei/v1/account/account_menu?prettyPrint=false", {
        method: "POST", credentials: "include", cache: "no-store", signal: controller.signal,
        headers: {
          "Content-Type": "application/json", "Authorization": `SAPISIDHASH ${timestamp}_${hash}`,
          "X-Origin": location.origin, "X-Goog-AuthUser": String(config.SESSION_INDEX || 0), "X-Goog-PageId": policy.delegate,
          "X-Youtube-Client-Name": String(config.INNERTUBE_CONTEXT_CLIENT_NAME), "X-Youtube-Client-Version": config.INNERTUBE_CLIENT_VERSION
        }, body: JSON.stringify({context})
      });
      if (!menu.ok) throw new Error("YouTube account verification failed");
      result.channel = selectedChannel(await menu.json());
      if (result.channel !== policy.channel) throw new Error("Selected YouTube channel is not Covers");
      result.verified = true;
    } catch (error) {
      result.error = error instanceof Error ? error.message : "YouTube account verification failed";
    } finally {
      clearTimeout(timeout);
      parent.postMessage(result, parentOrigin);
    }
  });
})();
