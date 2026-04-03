import { makeExecutePreparedRequest } from "./values";
class TransportBackedGraphClient {
    transport;
    constructor(transport) {
        this.transport = transport;
    }
    plan(request) {
        return this.transport.plan(request);
    }
    execute(request) {
        return this.transport.execute(request);
    }
    prepare(request) {
        return this.transport.prepare(request);
    }
    listPrepared() {
        return this.transport.listPrepared();
    }
    executePrepared(requestOrName, params, sort) {
        const request = typeof requestOrName === "string"
            ? makeExecutePreparedRequest(requestOrName, params, sort)
            : requestOrName;
        return this.transport.executePreparedQuery(request);
    }
    executePreparedMutation(requestOrName, params, sort) {
        const request = typeof requestOrName === "string"
            ? makeExecutePreparedRequest(requestOrName, params, sort)
            : requestOrName;
        return this.transport.executePreparedUpdate(request);
    }
    dropPrepared(name) {
        return this.transport.dropPrepared(name);
    }
}
export function createGraphClient(transport) {
    return new TransportBackedGraphClient(transport);
}
