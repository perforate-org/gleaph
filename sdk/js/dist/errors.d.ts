export declare class GleaphSdkError extends Error {
    readonly code: string;
    readonly causeValue?: unknown | undefined;
    constructor(message: string, code?: string, causeValue?: unknown | undefined);
}
export declare class GleaphCanisterError extends GleaphSdkError {
    constructor(message: string, causeValue?: unknown);
}
//# sourceMappingURL=errors.d.ts.map