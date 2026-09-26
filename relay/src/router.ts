import type { Env } from "./env";
import { text } from "./util";

export type Handler = (req: Request, env: Env, ctx: ExecutionContext, url: URL) => Response | Promise<Response>;

/**
 * One route. `path` matches exactly; `prefix` matches the prefix itself and anything under it
 * (`/mod` matches `/mod` and `/mod/…`). Omit `method` to accept every method.
 */
export interface Route {
  method?: string | string[];
  path?: string;
  prefix?: string;
  handler: Handler;
}

function hits(r: Route, pathname: string): boolean {
  if (r.path !== undefined) return pathname === r.path;
  if (r.prefix !== undefined) return pathname === r.prefix || pathname.startsWith(r.prefix.endsWith("/") ? r.prefix : `${r.prefix}/`);
  return false;
}

/** First matching route wins; a path that exists with other methods answers 405. */
export async function dispatch(routes: Route[], req: Request, env: Env, ctx: ExecutionContext): Promise<Response> {
  const url = new URL(req.url);
  const allowed: string[] = [];
  for (const r of routes) {
    if (!hits(r, url.pathname)) continue;
    const methods = r.method === undefined ? undefined : Array.isArray(r.method) ? r.method : [r.method];
    if (methods && !methods.includes(req.method) && !(req.method === "HEAD" && methods.includes("GET"))) {
      allowed.push(...methods);
      continue;
    }
    return r.handler(req, env, ctx, url);
  }
  if (allowed.length) return text("method not allowed", 405, { allow: [...new Set(allowed)].join(", ") });
  return text("not found", 404);
}
