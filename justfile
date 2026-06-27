# Gleaph development tasks.
# Run `just -l` to list recipes.

# Default recipe lists all available recipes.
default:
    @just -l

# Run PocketIC E2E tests in Terminal.app.
# An editor-hosted integrated terminal can interfere with PocketIC's canister-sandbox
# process chain on macOS, so this delegates to an external terminal.
#
# Usage:
#   just ic-e2e                           - run the full E2E suite (window stays open)
#   just ic-e2e --close                   - run the full E2E suite and close the window when done
#   just ic-e2e --all                    - alias for the full E2E suite
#   just ic-e2e smoke                    - run only the smoke test
#   just ic-e2e smoke --close
#   just ic-e2e --inline                 - run directly in the current terminal (default smoke; pass a target to override)
#   just ic-e2e router_graph_resolution
#   just ic-e2e router_graph_resolution --close
# Note: `--inline` can appear before or after the target; flags are parsed independently.
[macos]
ic-e2e *ARGS:
    @sh {{justfile_directory()}}/scripts/ic-e2e.sh {{justfile_directory()}} {{ARGS}}

# Run canbench in Terminal.app.
# canbench exercises compiled canister Wasm and, like the PocketIC E2E tests, may spawn
# a canister-sandbox process chain that can hang in an editor-hosted integrated terminal on
# macOS. This delegates to an external terminal.
#
# Usage:
#   just canbench graph                          - run all graph canbench benchmarks (window stays open)
#   just canbench graph --close                  - run all graph canbench benchmarks and close when done
#   just canbench graph search_join              - run graph canbench benchmarks matching "search_join"
#   just canbench graph search_join --close      - run the matching benchmarks and close when done
#   just canbench --inline graph search_join     - run directly in the current terminal
#   just canbench graph search_join --inline     - same; flags may appear before or after positional arguments
# Note: scripts/canbench.sh parses `--inline` and `--close` as flags, so they can appear anywhere.
[macos]
canbench *ARGS:
    @sh {{justfile_directory()}}/scripts/canbench.sh {{justfile_directory()}} {{ARGS}}
