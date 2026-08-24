import { createPreparedGleaphClient } from "../generated";
import type { PreparedGleaphClient } from "../generated";

export type RouterClientOptions = {
  canisterId: string;
  host: string;
  fetchRootKey: boolean;
};

/**
 * Read the Vite-baked Router connection from .env.local (written by `pnpm write-env`).
 * Undefined when the demo state has not been brought up yet, so the page can render a
 * "run the quickstart" hint instead of failing on a missing variable.
 */
export const getRouterClientOptions = (): RouterClientOptions | undefined => {
  const canisterId = import.meta.env.VITE_GLEAPH_ROUTER_CANISTER_ID as string | undefined;
  if (!canisterId) {
    return undefined;
  }

  return {
    canisterId,
    host: (import.meta.env.VITE_IC_HOST as string | undefined) ?? "http://localhost:8000",
    fetchRootKey: import.meta.env.VITE_FETCH_ROOT_KEY === "true",
  };
};

export const createRouterClient = (options: RouterClientOptions): Promise<PreparedGleaphClient> =>
  createPreparedGleaphClient(options);
