import { HttpAgent, type Agent } from "@icp-sdk/core/agent";
import { safeGetCanisterEnv } from "@icp-sdk/core/agent/canister-env";

import {
  createActor as createGeneratedActor,
  type GqlQueryResult,
  type SocialDemoScenario,
} from "~/generated/social_demo_gateway";

export type GatewayClientOptions = {
  canisterId: string;
  host: string;
  fetchRootKey: boolean;
  rootKey?: Uint8Array;
};

export type GatewayClient = {
  runScenario(scenario: SocialDemoScenario): Promise<GqlQueryResult>;
};

export const getGatewayClientOptions = ():
  | GatewayClientOptions
  | undefined => {
  const canisterEnv = safeGetCanisterEnv<{
    readonly "PUBLIC_CANISTER_ID:gleaph-social-demo-gateway"?: string;
    readonly IC_ROOT_KEY?: Uint8Array | string;
  }>();
  const canisterId =
    canisterEnv?.["PUBLIC_CANISTER_ID:gleaph-social-demo-gateway"] ??
    (import.meta.env.VITE_SOCIAL_DEMO_GATEWAY_CANISTER_ID as string | undefined);
  if (!canisterId) {
    return undefined;
  }

  return {
    canisterId,
    host:
      (import.meta.env.VITE_IC_HOST as string | undefined) ??
      (canisterEnv ? globalThis.location.origin : "https://icp-api.io"),
    fetchRootKey: canisterEnv ? false : import.meta.env.VITE_FETCH_ROOT_KEY === "true",
    rootKey: canisterEnv?.IC_ROOT_KEY,
  };
};

export const createGatewayClient = (
  options: GatewayClientOptions,
): GatewayClient => {
  const agent: Agent = HttpAgent.createSync({
    host: options.host,
    rootKey: options.rootKey,
  });

  const ready: Promise<void> = options.fetchRootKey
    ? agent.fetchRootKey().then(() => undefined)
    : Promise.resolve();

  const actor = createGeneratedActor(options.canisterId, { agent });

  return {
    async runScenario(scenario) {
      await ready;

      const result = await actor.execute_social_demo_scenario(scenario);

      if (result.__kind__ === "Err") {
        throw new Error(`Gateway scenario failed: ${JSON.stringify(result.Err)}`);
      }

      return result.Ok;
    },
  };
};
