import { Actor, HttpAgent, type Identity } from "@icp-sdk/core/agent";
import { Principal } from "@icp-sdk/core/principal";
import { GraphClient } from "./graph.js";
import { graphIdlFactory, registryIdlFactory } from "./idl.js";
import { RegistryClient } from "./registry.js";

/** Options for creating a {@link GleaphClient}. */
export interface GleaphClientOptions {
	/** IC host URL. Defaults to `"https://icp-api.io"`. */
	host?: string;
	/** Identity for authenticated calls. Uses `AnonymousIdentity` if omitted. */
	identity?: Identity;
	/** Fetch the root key from the replica. Required for local development, must be `false` on mainnet. */
	fetchRootKey?: boolean;
}

/**
 * Entry point for the Gleaph SDK.
 *
 * Creates an IC agent and provides factory methods for connecting
 * to graph and registry canisters.
 *
 * @example
 * ```ts
 * import { GleaphClient } from "@gleaph/sdk";
 *
 * // Mainnet
 * const client = new GleaphClient();
 * const graph = client.graph("bkyz2-fmaaa-aaaaa-qaaaq-cai");
 *
 * // Local development
 * const local = new GleaphClient({
 *   host: "http://127.0.0.1:4943",
 *   fetchRootKey: true,
 * });
 * await local.ready();
 * ```
 */
export class GleaphClient {
	private agent: HttpAgent;
	private rootKeyFetched: Promise<void>;

	constructor(options: GleaphClientOptions = {}) {
		this.agent = HttpAgent.createSync({
			host: options.host ?? "https://icp-api.io",
			identity: options.identity,
		});
		this.rootKeyFetched = options.fetchRootKey
			? this.agent.fetchRootKey().then(() => {})
			: Promise.resolve();
	}

	/**
	 * Connect to a specific graph canister.
	 *
	 * @param canisterId - Canister ID as a string or `Principal`.
	 * @returns A {@link GraphClient} for querying and mutating the graph.
	 */
	graph(canisterId: string | Principal): GraphClient {
		const actor = Actor.createActor(graphIdlFactory, {
			agent: this.agent,
			canisterId:
				typeof canisterId === "string"
					? Principal.fromText(canisterId)
					: canisterId,
		});
		return new GraphClient(actor);
	}

	/**
	 * Connect to a registry canister.
	 *
	 * @param canisterId - Canister ID as a string or `Principal`.
	 * @returns A {@link RegistryClient} for managing graph lifecycles.
	 */
	registry(canisterId: string | Principal): RegistryClient {
		const actor = Actor.createActor(registryIdlFactory, {
			agent: this.agent,
			canisterId:
				typeof canisterId === "string"
					? Principal.fromText(canisterId)
					: canisterId,
		});
		return new RegistryClient(actor);
	}

	/**
	 * Wait for the root key to be fetched.
	 *
	 * Only needed when `fetchRootKey: true` (local development).
	 * On mainnet this resolves immediately.
	 */
	async ready(): Promise<void> {
		await this.rootKeyFetched;
	}
}
