// Type surface for scripts/embedding-core.mjs (plain JS, shared with Node scripts).
// The implementation is the single source of truth; this file only declares its shape so
// the page can import it without enabling allowJs.

export declare const EMBEDDING_DIMS: number;
export declare const SEED_PREFIX: string;

/** Derive one L2-normalized 768-dim vector from a 32-byte digest. */
export declare function unitVectorFromDigest(digest: Uint8Array): number[];
