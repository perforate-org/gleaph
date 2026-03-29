.PHONY: build test cli install-cli wasm wasm-e2e-fixtures e2e-legacy-registry-upgrade \
	smoke-start smoke-deploy smoke-call smoke-upgrade smoke-stop ci-smoke \
	bench bench-persist \
	wasm-bench-core bench-core bench-core-persist \
	wasm-bench-ecom bench-ecom-snapshot bench-ecom bench-ecom-persist \
	bench-ecom-quick bench-ecom-quick-persist \
	wasm-bench-social bench-social-snapshot bench-social bench-social-persist \
	bench-social-quick bench-social-quick-persist \
	wasm-bench-timeline bench-timeline-snapshot bench-timeline bench-timeline-persist \
	bench-timeline-quick bench-timeline-quick-persist

build:
	cargo build --workspace

test:
	cargo test --workspace

cli:
	cargo build -p gleaph-cli --release

install-cli:
	cargo install --path crates/cli

test-pocket-ic:
	$(MAKE) wasm
	cargo test --workspace -- --ignored --test-threads=1

wasm:
	cargo build -p gleaph-graph --target wasm32-unknown-unknown
	cargo build -p gleaph-registry --target wasm32-unknown-unknown

wasm-e2e-fixtures:
	cargo build -p gleaph-graph --target wasm32-unknown-unknown
	cargo build -p gleaph-registry -p gleaph-registry-legacy-fixture --target wasm32-unknown-unknown

e2e-legacy-registry-upgrade: wasm-e2e-fixtures
	cargo test -p gleaph-tests -- --ignored registry_upgrade_from_legacy_graph_map_snapshot_preserves_graphs_and_rebuilds_next_id

smoke-start:
	icp network start -d

smoke-deploy:
	icp deploy

smoke-call:
	icp canister call graph add_edge '(record { src = 0 : nat32; dst = 1 : nat32; weight = 1.0 : float32; timestamp = 100 : nat64 })'
	icp canister call graph get_neighbors '(0 : nat32)'
	icp canister call graph bulk_insert_edges '(vec { record { src = 0 : nat32; dst = 2 : nat32; weight = 1.0 : float32; timestamp = 101 : nat64 }; record { src = 0 : nat32; dst = 3 : nat32; weight = 1.0 : float32; timestamp = 102 : nat64 } })'
	icp canister call graph get_stats '()'
	icp canister call registry create_graph '(record { name = "smoke"; max_vertices = 128 : nat32; initial_edge_capacity = 0 : nat64 })'
	icp canister call registry list_graphs '()'

smoke-upgrade:
	icp canister install graph --mode upgrade
	icp canister call graph get_neighbors '(0 : nat32)'
	icp canister call graph get_stats '()'

smoke-stop:
	icp network stop

ci-smoke:
	@set -e; \
	trap 'icp network stop >/dev/null 2>&1 || true' EXIT; \
	$(MAKE) wasm; \
	icp network start -d; \
	icp deploy; \
	icp canister call graph add_edge '(record { src = 0 : nat32; dst = 1 : nat32; weight = 1.0 : float32; timestamp = 100 : nat64 })'; \
	icp canister call graph get_neighbors '(0 : nat32)'; \
	icp canister call graph bulk_insert_edges '(vec { record { src = 0 : nat32; dst = 2 : nat32; weight = 1.0 : float32; timestamp = 101 : nat64 }; record { src = 0 : nat32; dst = 3 : nat32; weight = 1.0 : float32; timestamp = 102 : nat64 } })'; \
	icp canister call graph get_stats '()'; \
	icp canister call registry create_graph '(record { name = "smoke"; max_vertices = 128 : nat32; initial_edge_capacity = 0 : nat64 })'; \
	icp canister call registry list_graphs '()'; \
	icp canister install graph --mode upgrade; \
	icp canister call graph get_neighbors '(0 : nat32)'; \
	icp canister call graph get_stats '()'

wasm-bench-core:
	cargo build --release -p gleaph-graph --target wasm32-unknown-unknown --features canbench-rs

bench-core: wasm-bench-core
	cd bench/core && canbench

bench-core-persist: wasm-bench-core
	cd bench/core && canbench --persist

bench:
	cd bench/core && canbench

bench-persist:
	cd bench/core && canbench --persist

wasm-bench-ecom:
	cargo build --release -p gleaph-graph --target wasm32-unknown-unknown --features bench-ecom

bench-ecom-snapshot: wasm-bench-ecom
	cargo test -p gleaph-tests -- --ignored gen_ecom_stable_snapshot --test-threads=1 --nocapture

bench-ecom: bench-ecom-snapshot
	cd bench/ecom && canbench

bench-ecom-persist: bench-ecom-snapshot
	cd bench/ecom && canbench --persist

bench-ecom-quick:
	cd bench/ecom && canbench

bench-ecom-quick-persist:
	cd bench/ecom && canbench --persist

wasm-bench-social:
	cargo build --release -p gleaph-graph --target wasm32-unknown-unknown --features bench-social

bench-social-snapshot: wasm-bench-social
	cargo test -p gleaph-tests -- --ignored gen_social_stable_snapshot --test-threads=1 --nocapture

bench-social: bench-social-snapshot
	cd bench/social && canbench

bench-social-persist: bench-social-snapshot
	cd bench/social && canbench --persist

bench-social-quick:
	cd bench/social && canbench

bench-social-quick-persist:
	cd bench/social && canbench --persist

wasm-bench-timeline:
	cargo build --release -p gleaph-graph --target wasm32-unknown-unknown --features bench-timeline

bench-timeline-snapshot: wasm-bench-timeline
	cargo test -p gleaph-tests -- --ignored gen_timeline_stable_snapshot --test-threads=1 --nocapture

bench-timeline: bench-timeline-snapshot
	cd bench/timeline && canbench

bench-timeline-persist: bench-timeline-snapshot
	cd bench/timeline && canbench --persist

bench-timeline-quick:
	cd bench/timeline && canbench

bench-timeline-quick-persist:
	cd bench/timeline && canbench --persist
