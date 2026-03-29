import type { ActorSubclass } from "@icp-sdk/core/agent";
import { Principal } from "@icp-sdk/core/principal";
import type { AccessLevel, GraphConfig, GraphInfo } from "./types.js";

type RegistryActor = ActorSubclass<
	Record<string, (...args: any[]) => Promise<any>>
>;

/**
 * Client for the Gleaph registry canister.
 *
 * The registry manages tenant graph canisters — creating, listing,
 * deleting graphs and controlling access.
 *
 * Obtain an instance via {@link import("./client.js").GleaphClient.registry | GleaphClient.registry()}.
 *
 * @example
 * ```ts
 * const registry = client.registry("r7inp-6aaaa-aaaaa-aaabq-cai");
 * const graphs = await registry.listGraphs();
 * ```
 */
export class RegistryClient {
	/** @internal */
	constructor(readonly actor: RegistryActor) {}

	/**
	 * Create a new graph canister.
	 *
	 * @param config - Graph configuration (name, vertex capacity, edge capacity).
	 * @returns Information about the newly created graph.
	 */
	async createGraph(config: GraphConfig): Promise<GraphInfo> {
		return await this.actor.create_graph(config);
	}

	/**
	 * Delete a graph by its registry ID.
	 *
	 * @param id - The graph's registry ID.
	 * @returns `true` if the graph was successfully deleted.
	 */
	async deleteGraph(id: bigint): Promise<boolean> {
		return await this.actor.delete_graph(id);
	}

	/**
	 * List all graphs accessible to the caller.
	 *
	 * @returns Array of graph info records.
	 */
	async listGraphs(): Promise<GraphInfo[]> {
		return await this.actor.list_graphs();
	}

	/**
	 * Grant access to another principal on a graph.
	 *
	 * @param graphId - The graph's registry ID.
	 * @param principal - Principal to grant access to (text or Principal).
	 * @param level - Access level: `"read"`, `"write"`, or `"admin"`.
	 * @returns `true` if access was successfully granted.
	 */
	async grantAccess(
		graphId: bigint,
		principal: string | Principal,
		level: "read" | "write" | "admin",
	): Promise<boolean> {
		const p =
			typeof principal === "string" ? Principal.fromText(principal) : principal;
		const accessLevel: AccessLevel =
			level === "read"
				? { Read: null }
				: level === "write"
					? { Write: null }
					: { Admin: null };
		return await this.actor.grant_access(graphId, p, accessLevel);
	}
}
