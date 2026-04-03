export class GleaphSdkError extends Error {
  constructor(
    message: string,
    readonly code = "GLEAPH_SDK_ERROR",
    readonly causeValue?: unknown,
  ) {
    super(message);
    this.name = "GleaphSdkError";
  }
}

export class GleaphCanisterError extends GleaphSdkError {
  constructor(message: string, causeValue?: unknown) {
    super(message, "GLEAPH_CANISTER_ERROR", causeValue);
    this.name = "GleaphCanisterError";
  }
}
