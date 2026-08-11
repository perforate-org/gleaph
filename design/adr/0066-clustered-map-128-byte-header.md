# 0066. Clustered hash map uses a 128-byte V1 metadata boundary

Date: 2026-08-11
Status: accepted
Last revised: 2026-08-11
Anchor timestamp: 2026-08-11 19:54:21 UTC +0000

## Context

The clustered hash map persists table metadata and entries in one `Memory` region. Its current
metadata fields use offsets through byte 62, leaving no practical space for another persisted
field without moving the entry boundary. The map has no deployed consumer in the current Gleaph
release, so this pre-release layout can be improved as one V1 design decision.

## Decision

Use a 128-byte metadata prefix for the current V1 layout. Keep `LAYOUT_VERSION = 1`, preserve the
existing field offsets, and set `DATA_OFFSET = 128` for every entry and capacity calculation. Fresh
maps clear the full prefix before writing entries.

No migration reader, dual layout, compatibility shim, or version-number change is part of this
decision. Future metadata additions use the extension within the 128-byte prefix until a separate
layout decision is required.

## Consequences

The map gains 64 bytes of metadata capacity per instance without changing entry stride, hashing,
incremental resize behavior, or public mutation APIs. The first allocation may use one additional
stable-memory page at a page boundary; subsequent data address calculations use the same single
`DATA_OFFSET` source of truth.

## Alternatives considered

A smaller prefix would leave no meaningful extension area for persisted state. A 256-byte prefix
would reserve more stable memory than the current metadata needs. Changing the layout version or
adding a migration reader would add compatibility machinery that is outside the current pre-release
contract.
