# ic-morph-dict

Thin Internet Computer adapter for [morph-dict](../morph-dict): `ByteImage`
implementations over IC stable memory (`StableImage` over any
`ic_stable_structures::Memory`; one `mem.read` = one batched `stable64_read`
syscall) plus an optional 64 KiB-frame LRU cache (`CachedStableImage`) for lazy
regions with temporal locality.

Placement notes and the residency policy are documented in the crate docs.
