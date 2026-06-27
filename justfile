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
#   just ic-e2e              - run the full E2E suite (window stays open)
#   just ic-e2e --close      - run the full E2E suite and close the window when done
#   just ic-e2e --all        - alias for the full E2E suite
#   just ic-e2e smoke        - run only the smoke test
#   just ic-e2e smoke --close
#   just ic-e2e --inline     - run the smoke test directly in the current terminal
#   just ic-e2e router_graph_resolution
#   just ic-e2e router_graph_resolution --close
[macos]
ic-e2e *ARGS:
    @sh {{justfile_directory()}}/scripts/ic-e2e.sh {{justfile_directory()}} {{ARGS}}
