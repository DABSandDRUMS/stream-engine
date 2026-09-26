import type { Relay } from "./relay";

export interface Env {
  RELAY: DurableObjectNamespace<Relay>;
  /** Shared secret of the engine link (Worker secret; engine keyring `relay.secret`). */
  RELAY_SECRET?: string;
  /** Ko-fi webhook verification token (Worker secret). */
  KOFI_VERIFICATION_TOKEN?: string;
  /** Ko-fi payments kept while the engine is offline. */
  KOFI_BUFFER_MAX?: string;
  /** Heading of the public queue page. */
  QUEUE_TITLE?: string;
}

/** The single relay Durable Object. */
export function relayStub(env: Env): DurableObjectStub<Relay> {
  return env.RELAY.get(env.RELAY.idFromName("main"));
}
