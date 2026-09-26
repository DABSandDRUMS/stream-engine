// Ko-fi webhook (PLAN §14.5): `application/x-www-form-urlencoded` with one `data` field holding
// JSON. We verify `verification_token`, dedupe on `message_id` (in the Durable Object), and
// forward only the fields the engine needs — never the e-mail, shipping address or token.

import { type Env, relayStub } from "./env";
import { json, safeEqual } from "./util";

/** Ko-fi bodies are small; anything bigger isn't Ko-fi. */
const MAX_BODY = 64 * 1024;

/** Fields forwarded to the engine. */
const FORWARD = [
  "message_id",
  "timestamp",
  "type",
  "is_public",
  "from_name",
  "message",
  "amount",
  "currency",
  "is_subscription_payment",
  "is_first_subscription_payment",
  "kofi_transaction_id",
  "tier_name",
] as const;

export type KofiPayment = Partial<Record<(typeof FORWARD)[number], unknown>> & { message_id: string };

export type KofiParse = { ok: true; payment: KofiPayment } | { ok: false; status: number; error: string };

/** Parse and verify a Ko-fi webhook request. */
export async function parseKofi(req: Request, token: string | undefined): Promise<KofiParse> {
  if (!token) return { ok: false, status: 503, error: "Ko-fi verification token not configured" };
  const len = Number(req.headers.get("content-length") ?? "0");
  if (len > MAX_BODY) return { ok: false, status: 413, error: "body too large" };
  const body = new TextDecoder().decode(await req.arrayBuffer());
  if (body.length > MAX_BODY) return { ok: false, status: 413, error: "body too large" };
  const type = (req.headers.get("content-type") ?? "").toLowerCase();
  let raw: string | null;
  if (type.startsWith("application/x-www-form-urlencoded")) {
    raw = new URLSearchParams(body).get("data");
  } else if (type.startsWith("application/json")) {
    raw = body; // tolerated for manual tests (`curl -H 'content-type: application/json'`)
  } else {
    return { ok: false, status: 415, error: "expected application/x-www-form-urlencoded" };
  }
  if (!raw) return { ok: false, status: 400, error: "missing data field" };
  let data: unknown;
  try {
    data = JSON.parse(raw);
  } catch {
    return { ok: false, status: 400, error: "data is not JSON" };
  }
  if (typeof data !== "object" || data === null || Array.isArray(data)) return { ok: false, status: 400, error: "data is not an object" };
  const d = data as Record<string, unknown>;
  if (typeof d.verification_token !== "string" || !safeEqual(d.verification_token, token)) {
    return { ok: false, status: 401, error: "bad verification token" };
  }
  const id = d.message_id;
  if (typeof id !== "string" || id.length === 0 || id.length > 200) return { ok: false, status: 400, error: "missing message_id" };
  if (d.amount === undefined || d.amount === null) return { ok: false, status: 400, error: "missing amount" };
  const fields: Record<string, unknown> = {};
  for (const k of FORWARD) if (k in d) fields[k] = d[k];
  return { ok: true, payment: { ...fields, message_id: id } };
}

/** `POST /hooks/kofi` */
export async function kofiWebhook(req: Request, env: Env): Promise<Response> {
  const parsed = await parseKofi(req, env.KOFI_VERIFICATION_TOKEN);
  if (!parsed.ok) return json({ error: parsed.error }, parsed.status);
  const status = await relayStub(env).kofi(parsed.payment);
  return json({ status });
}
