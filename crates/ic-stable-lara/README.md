# LARA: Localized Adjacency Relocation Array

`ic-stable-lara` stores a CSR-style adjacency graph in Internet Computer stable
memory while allowing local physical relocation of dense adjacency regions.

The design keeps the read path direct and predictable:

- A clean scan reads the vertex row and walks edge slots
  `[base_slot_start, base_slot_start + degree)`.
- Clean scans must not consult vertex `capacity`, segment span metadata, or the
  free span manager.
- Updates may read and rewrite `base_slot_start`, `degree`, and `capacity`.
  `capacity` is the owned slab span used to decide whether an insertion fits,
  whether relocation is needed, and which retired spans can be reused.

## Storage Model

Each default vertex row stores:

- `base_slot_start`: first edge slot owned by the vertex.
- `degree`: number of live neighbors in the clean prefix.
- `capacity`: number of slab slots owned by the vertex, including the live
  prefix.
- `log_head`: per-segment overflow log head, or `-1` when the whole
  neighborhood is on the slab.

The core invariant is:

```text
[base_slot_start, base_slot_start + degree)
    is contained in
[base_slot_start, base_slot_start + capacity)
```

Segment relocation may move a group of vertex spans out of vertex-id physical
order. Segment span metadata records where a segment currently lives, while
free span metadata records retired physical ranges that can be reused by later
relocation. Both are update/maintenance metadata and stay off the clean scan
path.

## Reference

The main external reference for the dynamic adjacency idea is
[DGAP](https://github.com/DIR-LAB/DGAP). This crate uses its own persisted
layout and names the implementation around LARA's local relocation and explicit
capacity contracts.
