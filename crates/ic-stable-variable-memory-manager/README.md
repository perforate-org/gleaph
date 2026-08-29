# ic-stable-variable-memory-manager

A memory manager for Internet Computer stable memory that allows variable page sizes per virtual memory.

## Background

In the conventional `ic-stable-structures` crate, the `MemoryManager` uses a fixed page size for all managed stable structures. Because the page size is global and constant, it is impossible to specify different initial capacities or maximum capacity limits for each individual virtual memory. This forces a "one size fits all" approach to memory allocation, which can be inefficient for canisters managing multiple structures with vastly different size requirements.

## Solution

`ic-stable-variable-memory-manager` addresses this limitation by providing a `MemoryManager` that allows an arbitrary page size to be specified for each virtual memory.

By allowing per-virtual-memory page size configuration, this crate enables:

- **Custom Initial Capacities**: Set a specific starting size for each virtual memory based on expected initial data.
- **Independent Capacity Limits**: Define different maximum capacity upper bounds for different virtual memories.
- **Optimized Memory Utilization**: Reduce memory waste and improve allocation efficiency by tailoring the page size to the specific access patterns and size of each structure.

## Key Features

- **Variable Page Sizes**: Decouples page size from a global constant, allowing each virtual memory to have its own configuration.
- **Granular Control**: Provides precise control over the initial and maximum stable memory allocation per virtual memory.
- **Enhanced Flexibility**: Solves the capacity management constraints found in traditional stable memory managers.
