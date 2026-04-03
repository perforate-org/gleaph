export class GleaphSdkError extends Error {
    code;
    causeValue;
    constructor(message, code = "GLEAPH_SDK_ERROR", causeValue) {
        super(message);
        this.code = code;
        this.causeValue = causeValue;
        this.name = "GleaphSdkError";
    }
}
export class GleaphCanisterError extends GleaphSdkError {
    constructor(message, causeValue) {
        super(message, "GLEAPH_CANISTER_ERROR", causeValue);
        this.name = "GleaphCanisterError";
    }
}
