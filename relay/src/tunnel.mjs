// Local-only queue publishing. cloudflared forwards to the read-only listener, never /link
// or the operator API. The user service stops this process with the engine/queue service.
import { spawn, execFileSync } from "node:child_process";

const origin = "http://127.0.0.1:8787";
const cloudflared = process.env.CLOUDFLARED || "cloudflared";
const tokenFile = process.env.TUNNEL_TOKEN_FILE;
const publicUrl = process.env.QUEUE_PUBLIC_URL;
const lifecycle = new AbortController();
if (Boolean(tokenFile) !== Boolean(publicUrl)) {
  throw new Error("Named tunnel requires both TUNNEL_TOKEN_FILE and QUEUE_PUBLIC_URL");
}

async function publishUrl(url) {
  // Read the existing keyring token into memory, not an argument, file or log.
  let token = execFileSync("streamctl", ["token"], { encoding: "utf8" }).trim();
  const info = JSON.parse(execFileSync("streamctl", ["--json", "query", "api.info"], { encoding: "utf8" }));
  await new Promise((resolve, reject) => {
    const ws = new WebSocket(info.ws);
    const timeout = setTimeout(() => finish(new Error("Engine did not publish the tunnel URL")), 20000);
    let settled = false;
    const cancelled = () => finish(new Error("Stopping queue tunnel"));
    lifecycle.signal.addEventListener("abort", cancelled, { once: true });
    function finish(error) {
      if (settled) return;
      settled = true;
      clearTimeout(timeout);
      lifecycle.signal.removeEventListener("abort", cancelled);
      token = "";
      ws.close();
      error ? reject(error) : resolve();
    }
    ws.onopen = () => ws.send(JSON.stringify({ t: "hello", client: "queue-tunnel", token, version: 1 }));
    ws.onerror = () => finish(new Error("Cannot reach the local engine"));
    ws.onclose = () => finish(new Error("Engine disconnected before publishing the tunnel URL"));
    ws.onmessage = (event) => {
      const message = JSON.parse(event.data);
      if (message.t === "welcome") {
        token = "";
        ws.send(JSON.stringify({ t: "subscribe", sub: { state: ["queue.url"] } }));
        ws.send(JSON.stringify({ t: "cmd", req: 1, cmd: {
          origin: "api", op: { kind: "action", name: "project.write", args: {
            path: "project.toml", set: { "relay.queue_url": url },
          } },
        } }));
      } else if (message.t === "ack" && message.req === 1 && !message.ok) {
        finish(new Error(message.error || "Engine rejected the queue URL"));
      } else if (message.t === "state" && message.changes.some(([address, value]) => address === "queue.url" && value === url)) {
        finish();
      } else if (message.t === "error") {
        finish(new Error(message.msg || "Engine API error"));
      }
    };
  });
  console.log(`Public song queue: ${url}`);
}

const args = tokenFile
  ? ["tunnel", "--no-autoupdate", "run", "--token-file", tokenFile]
  : ["tunnel", "--no-autoupdate", "--url", origin, "--protocol", "http2"];
const child = spawn(cloudflared, args, { stdio: ["ignore", "pipe", "pipe"] });
let announced = false;
let stopping = false;
let logWindow = "";
function stop(signal = "SIGTERM") {
  stopping = true;
  lifecycle.abort();
  child.kill(signal);
}
process.on("SIGTERM", () => stop());
process.on("SIGINT", () => stop("SIGINT"));
child.on("error", (error) => { console.error(error.message); process.exitCode = 1; });
child.on("exit", (code) => { process.exitCode = stopping ? 0 : (code || 1); });
function fail(error) {
  if (stopping) return;
  console.error(error.message);
  process.exitCode = 1;
  child.kill("SIGTERM");
}
function observe(chunk, output) {
  output.write(chunk);
  if (announced || stopping) return;
  logWindow = (logWindow + chunk.toString()).slice(-8192);
  const match = logWindow.match(/https:\/\/[a-z0-9-]+\.trycloudflare\.com\b/);
  if (match) {
    announced = true;
    publishUrl(`${match[0]}/queue`).catch(fail);
  }
}
child.stdout.on("data", (chunk) => observe(chunk, process.stdout));
child.stderr.on("data", (chunk) => observe(chunk, process.stderr));
if (publicUrl) {
  announced = true;
  publishUrl(publicUrl).catch(fail);
}
