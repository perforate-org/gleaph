import { createPreparedGleaphClient } from "~/generated";
import type { PreparedGleaphClient } from "~/generated";

export type RouterClientOptions = {
  canisterId: string;
  host: string;
  fetchRootKey: boolean;
};

export const getRouterClientOptions = (): RouterClientOptions | undefined => {
  const canisterId = import.meta.env.VITE_GLEAPH_ROUTER_CANISTER_ID as string | undefined;
  if (!canisterId) {
    return undefined;
  }

  return {
    canisterId,
    host: (import.meta.env.VITE_IC_HOST as string | undefined) ?? "https://icp-api.io",
    fetchRootKey: import.meta.env.VITE_FETCH_ROOT_KEY === "true",
  };
};

export const createRouterClient = (options: RouterClientOptions): Promise<PreparedGleaphClient> =>
  createPreparedGleaphClient(options);
