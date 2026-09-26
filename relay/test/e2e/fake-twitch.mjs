// Minimal fake of the Twitch identity + Helix endpoints the /mod login uses, for local
// end-to-end tests against `wrangler dev` (no Twitch account needed).
//
//   GET  /oauth2/authorize   → 302 back to redirect_uri with #access_token=…&state=…
//                              (?login=<user> picks which fake user "signs in")
//   GET  /oauth2/validate    → token info (Authorization: OAuth <token>)
//   GET  /helix/moderation/channels?user_id=… → channels the user moderates
//   POST /oauth2/revoke      → 200, records the revocation
//
// Usage as a module: `const t = await startFakeTwitch({ port, clientId, users })`.
import http from "node:http";

export async function startFakeTwitch({ port = 8799, clientId, users }) {
  const tokens = new Map(); // token → user
  const revoked = [];
  const log = [];
  const server = http.createServer(async (req, res) => {
    const url = new URL(req.url, `http://127.0.0.1:${port}`);
    log.push(`${req.method} ${url.pathname}`);
    const send = (status, body, headers = {}) => {
      res.writeHead(status, { "Content-Type": "application/json", ...headers });
      res.end(body === undefined ? "" : JSON.stringify(body));
    };
    if (url.pathname === "/oauth2/authorize") {
      const u = users.find((x) => x.login === (url.searchParams.get("login") ?? users[0].login));
      if (url.searchParams.get("client_id") !== clientId) return send(400, { message: "invalid client" });
      const token = `tok-${u.login}-${Math.random().toString(36).slice(2)}`;
      tokens.set(token, u);
      const back = new URL(url.searchParams.get("redirect_uri"));
      back.hash = new URLSearchParams({ access_token: token, scope: url.searchParams.get("scope") ?? "", state: url.searchParams.get("state") ?? "", token_type: "bearer" }).toString();
      res.writeHead(302, { Location: back.toString() });
      return res.end();
    }
    const bearer = (req.headers.authorization ?? "").replace(/^(OAuth|Bearer) /, "");
    const user = tokens.get(bearer);
    if (url.pathname === "/oauth2/validate") {
      if (!user) return send(401, { status: 401, message: "invalid access token" });
      return send(200, { client_id: clientId, login: user.login, user_id: user.id, scopes: ["user:read:moderated_channels"], expires_in: 14000 });
    }
    if (url.pathname === "/helix/moderation/channels") {
      if (!user || req.headers["client-id"] !== clientId) return send(401, { message: "unauthorized" });
      if (url.searchParams.get("user_id") !== user.id) return send(400, { message: "user_id must match the token" });
      return send(200, { data: (user.moderates ?? []).map((id) => ({ broadcaster_id: id, broadcaster_login: `channel${id}`, broadcaster_name: `Channel${id}` })), pagination: {} });
    }
    if (url.pathname === "/oauth2/revoke" && req.method === "POST") {
      let body = "";
      for await (const c of req) body += c;
      const t = new URLSearchParams(body).get("token");
      revoked.push(t);
      tokens.delete(t);
      return send(200);
    }
    send(404, { message: "not found" });
  });
  await new Promise((ok) => server.listen(port, "127.0.0.1", ok));
  return { url: `http://127.0.0.1:${port}`, revoked, log, close: () => new Promise((ok) => server.close(ok)) };
}
