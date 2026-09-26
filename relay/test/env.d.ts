// Bindings seen by tests (`env` from "cloudflare:test") are the Worker's own `Env`.
import type { Env as RelayEnv } from "../src/env";

declare global {
  namespace Cloudflare {
    interface Env extends RelayEnv {}
  }
}
