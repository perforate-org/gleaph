export class GleaphSdkError extends Error {
  readonly code: string;
  readonly causeValue: unknown | undefined;

  constructor(message: string, code = "GLEAPH_SDK_ERROR", causeValue?: unknown) {
    super(message);
    this.name = "GleaphSdkError";
    this.code = code;
    this.causeValue = causeValue;
  }
}

export class GleaphCanisterError extends GleaphSdkError {
  constructor(message: string, causeValue?: unknown) {
    super(message, "GLEAPH_CANISTER_ERROR", causeValue);
    this.name = "GleaphCanisterError";
  }
}
