# gleaph-sdk

Rust application-client SDK for calling the Gleaph Router from outside a canister, using
`ic-agent` for transport. This is the application-side counterpart to `gleaph-cdk` (the canister
SDK): it mirrors the same `GleaphClient<Prepared>` surface so dynamic GQL, prepared operations,
bulk-load, and `IC.MSG_CALLER()`-aware prepared queries behave the same whether they run inside a
canister or in a standalone process.

The crate depends on `gleaph-router-wire` (never on `ic-cdk`) and owns the application caller
identity through the `ic_agent::Identity` bound to the transport.

## Adding the dependency

```toml
[dependencies]
gleaph-sdk.workspace = true        # in this workspace
# or, published:
gleaph-sdk = "0.1"
```

## Connecting

Build a client with [`connect`] (the `ic-agent` transport) or wrap a custom transport with
[`create_gleaph_client`].

```rust,no_run
use gleaph_sdk::{connect, transport::GleaphClientOptions};

let client = connect(GleaphClientOptions::new(
    "rrkah-fqaaa-aaaaa-aaaaq-cai".parse()?,
))?;
```

### Caller identity

The caller that the Router authorizes against is the `ic_agent::Identity` bound to the agent. Set
it on [`GleaphClientOptions::identity`]; without it the anonymous identity is used.

```rust,no_run
use gleaph_sdk::{connect, transport::GleaphClientOptions};
use ic_agent::identity::Secp256k1Identity;

let identity = Secp256k1Identity::from_pem_file("identity.pem")?;
let client = connect(GleaphClientOptions {
    identity: Some(Box::new(identity)),
    ..GleaphClientOptions::new("rrkah-fqaaa-aaaaa-aaaaq-cai".parse()?)
})?;

// The principal the Router sees as `IC.MSG_CALLER()`.
let me = client.caller()?;
```

Internet Identity delegations work the same way: build an `ic_agent::identity::DelegatedIdentity`
and pass it as `identity`.

### Sharing an `ic-agent` agent

To reuse an agent you already built (e.g. to call the Router and another canister as the same
caller), wrap it with `IcAgentTransport::from_agent`. The identity is owned by the `Agent`, so it
is shared with Gleaph automatically and cannot be overridden:

```rust,no_run
use gleaph_sdk::{create_gleaph_client, transport::IcAgentTransport};

let agent = ic_agent::Agent::builder()
    .with_url("https://icp-api.io")
    .with_identity(Box::new(identity))
    .build()?;

// Same agent (and caller identity) for Gleaph and another canister.
let gleaph = create_gleaph_client(std::sync::Arc::new(IcAgentTransport::from_agent(
    agent.clone(),
    router_id,
)));
let other = agent.query(&other_canister, "method").with_arg(args).call().await?;
```

`Agent` is `Clone` (it shares an internal `Arc`), so the same agent can drive Gleaph calls and
direct `ic-agent` calls concurrently.

## Dynamic GQL

Reads and idempotent mutations use the same [`gql!`](https://docs.rs/gleaph-gql-params) query
builder as the canister SDK.

```rust,no_run
use gleaph_sdk::{connect, transport::GleaphClientOptions, gql};

let client = connect(GleaphClientOptions::new(
    "rrkah-fqaaa-aaaaa-aaaaq-cai".parse()?,
))?;

// Read; defaults to `ReadMode::Eventual`.
let result = client
    .gql_query(gql!("MATCH (n:Person {id: $id}) RETURN n.name", { id: "alice" }))
    .await?;

// Idempotent mutation; reuse `client_mutation_key` only when retrying the same mutation.
let mutated = client
    .gql_mutate(
        gql!("MATCH (n:Person {id: $id}) SET n.name = $name", {
            id: "alice",
            name: "alicia",
        }),
        "rename-alice-1",
    )
    .await?;
```

For read-your-writes, pass the token from a mutation to `gql_query_with_mode`:

```rust,no_run
use gleaph_sdk::ReadMode;
# let token = None; // from the previous mutation result
let result = client
    .gql_query_with_mode(
        gql!("MATCH (n:Person {id: $id}) RETURN n.name", { id: "alice" }),
        ReadMode::AtLeast(token.expect("mutation token")),
    )
    .await?;
```

## Prepared operations (generated)

`gleaph-codegen` generates typed `*Params` / `*Row` types and a `PreparedExt` trait for
`gleaph_sdk::GleaphClient<Prepared>`, exactly as it does for the canister profile:

```sh
cargo run -p gleaph-codegen -- --manifest manifest.json \
  --target rust --format rust=never --output src/generated.rs
```

```rust,no_run
use gleaph_sdk::{GleaphClient, transport::GleaphClientOptions};
use gleaph_sdk::IcAgentTransport;
use generated::{FindUsersParams, Prepared, PreparedExt};

let transport = IcAgentTransport::connect(GleaphClientOptions::new(
    "rrkah-fqaaa-aaaaa-aaaaq-cai".parse()?,
))?;
let client = GleaphClient::with_prepared::<Prepared>(std::sync::Arc::new(transport));

let result = client
    .find_users(FindUsersParams { term: "al".into() })
    .await?;
for row in result.rows {
    println!("{}", row.user_name);
}
```

Dynamic GQL and prepared operations share one client: use the same
`GleaphClient::with_prepared::<Prepared>` value for both.

## Custom transport

For tests or a different HTTP stack, implement [`GleaphTransport`] and wrap it with
[`create_gleaph_client`]:

```rust,no_run
use gleaph_sdk::{GleaphClient, transport::GleaphTransport};

#[derive(Clone)]
struct FakeTransport;

impl GleaphTransport for FakeTransport {
    // Implement the required methods to return canned `GqlQueryResult`s.
}

let client = GleaphClient::new(std::sync::Arc::new(FakeTransport));
```

## Error handling

Every client method returns `Result<T, CallError>`:

- `CallError::Reject { code, message }` — the IC rejected the call at the transport layer.
- `CallError::Decode { message }` — the transport succeeded but the response could not be decoded.
- `CallError::Router(RouterError)` — the Router rejected the request with a structured error
  (for example `RouterError::NotAuthorized`, `RouterError::Busy`, `RouterError::ProjectionLag`).

`RouterError` is defined in `gleaph-router-wire` and shared with `gleaph-cdk`.
