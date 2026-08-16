/**
 * Type declarations for zstd-codec (no @types package available).
 */

export interface Simple {
  /**
   * Compress a buffer with the given compression level.
   * @param data Input buffer
   * @param level Compression level (1-22)
   */
  compress(data: Buffer | Uint8Array, level?: number): Uint8Array;
}

export interface ZstdCodec {
  /**
   * Create a simple compressor instance.
   */
  Simple: new () => Simple;
}

/**
 * Initialize the zstd codec.
 * @param callback Called with the initialized codec
 */
export function run(callback: (zstd: ZstdCodec) => void): void;
