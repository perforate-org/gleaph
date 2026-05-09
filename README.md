# Gleaph

**Gleaph** is a graph database designed to run on the decentralized blockchain platform [Internet Computer](https://internetcomputer.org/).

Its internal storage model is based on LARA (Localized Adjacency Relocation Array), a graph representation derived from Compressed Sparse Row (CSR). LARA is specifically optimized for the execution environment of Internet Computer, allowing Gleaph to operate efficiently within the platform’s resource constraints.

## GQL (Graph Query Language)

Gleaph uses [GQL](https://www.gqlstandards.org/) (Graph Query Language, [ISO/IEC 39075](https://www.iso.org/standard/76120.html)) as its query language.

GQL is a standardized graph query language, comparable to SQL in relational databases, designed for querying, traversing, and analyzing graph data.

### `Principal` and `msg_caller()`

Gleaph extends GQL for Internet Computer by providing the `Principal` type and the `msg_caller()` function.

These extensions allow queries to directly access the caller’s Principal, making it easier to work with authentication and caller identity within Internet Computer applications.

## Prepared Query

**Prepared Query** is one of Gleaph’s core features.

It allows database administrators to pre-register queries that can be executed by unprivileged users, preventing arbitrary query execution and improving security.

This makes it possible for frontend applications to send queries directly to Gleaph safely, without requiring intermediary canisters or backend servers.

In combination with `msg_caller()`, Prepared Queries can also be used to implement access control patterns where users are only allowed to access data related to themselves.

## License

This project is licensed under either of [Apache License, Version 2.0](./LICENSE-APACHE) or [MIT License](./LICENSE-MIT) at your option.
