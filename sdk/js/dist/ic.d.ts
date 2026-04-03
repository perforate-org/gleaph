import { type Identity } from "@icp-sdk/core/agent";
import { Principal } from "@icp-sdk/core/principal";
import { type GraphClient, type GraphTransport } from "./client";
export interface IcGraphTransportOptions {
    canisterId: string | Principal;
    host?: string;
    identity?: Identity;
    fetchRootKey?: boolean;
}
export declare function createIcGraphTransport(options: IcGraphTransportOptions): Promise<GraphTransport>;
export declare function createIcGraphClient(options: IcGraphTransportOptions): Promise<GraphClient>;
//# sourceMappingURL=ic.d.ts.map