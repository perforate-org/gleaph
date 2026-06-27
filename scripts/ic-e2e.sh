#!/bin/sh
# Run PocketIC E2E tests in Terminal.app.
# Usage: ic-e2e.sh <project-root> [--inline | target] [--close]
#
# target:
#   (none)      - run the full E2E suite (default)
#   --all       - alias for the full E2E suite
#   smoke       - run only the smoke test
#   --inline [target] - run directly in the current terminal (default smoke; target overrides)
#   <test-name> - run a specific test file, e.g. router_graph_resolution
#
# --close:
#   Close the Terminal.app window/tab and show a macOS notification when finished.
#   Without --close the window stays open so the user can inspect the output.

set -eu

ROOT="$1"
shift

TARGET=""
CLOSE=0
INLINE=0
ALL=0

for arg in "$@"; do
    case "$arg" in
        --close) CLOSE=1 ;;
        --all) TARGET=""; ALL=1 ;;
        --inline) INLINE=1 ;;
        *)
            if [ -z "$TARGET" ]; then
                TARGET="$arg"
            fi
            ;;
    esac
done

if [ "$INLINE" -eq 1 ]; then
    if [ "$ALL" -eq 1 ]; then
        TEST_ARGS=""
    elif [ -z "$TARGET" ]; then
        TEST_ARGS="--test smoke"
    else
        case "$TARGET" in
            smoke)
                TEST_ARGS="--test smoke"
                ;;
            *)
                TEST_ARGS="--test ${TARGET}"
                ;;
        esac
    fi
    exec env -u POCKET_IC_BIN cargo test -p gleaph-pocket-ic-tests ${TEST_ARGS} -- --nocapture
fi

# Default: run the full E2E suite.
if [ -z "$TARGET" ]; then
    TEST_ARGS=""
else
    case "$TARGET" in
        smoke)
            TEST_ARGS="--test smoke"
            ;;
        *)
            TEST_ARGS="--test ${TARGET}"
            ;;
    esac
fi

FIFO="/tmp/gleaph-ic-e2e.fifo"
WIN_ID_FILE="/tmp/gleaph-ic-e2e.window-id"
LOG="/tmp/gleaph-ic-e2e.log"
TMP=$(mktemp /tmp/gleaph_ic_e2e.XXXXXX)

cleanup() {
    rm -f "$TMP" "$FIFO" "$WIN_ID_FILE"
}
trap cleanup EXIT

if [ "$CLOSE" -eq 1 ]; then
    rm -f "$FIFO"
    mkfifo "$FIFO"

    cat > "$TMP" <<'EOF'
tell application "Terminal"
    activate
    set initialWindowCount to count of windows
    set w to do script "cd ROOT_PLACEHOLDER && env -u POCKET_IC_BIN cargo test -p gleaph-pocket-ic-tests TEST_ARGS_PLACEHOLDER -- --nocapture > LOG_PLACEHOLDER 2>&1; echo done > FIFO_PLACEHOLDER; exec zsh"
    repeat 30 times
        if (count of windows) > initialWindowCount then exit repeat
        delay 0.1
    end repeat
    set winId to id of front window
end tell
set f to open for access "WIN_ID_PLACEHOLDER" with write permission
write (winId as string) to f
close access f
EOF

    sed -i '' "s|ROOT_PLACEHOLDER|$ROOT|g" "$TMP"
    sed -i '' "s|TEST_ARGS_PLACEHOLDER|$TEST_ARGS|g" "$TMP"
    sed -i '' "s|LOG_PLACEHOLDER|$LOG|g" "$TMP"
    sed -i '' "s|FIFO_PLACEHOLDER|$FIFO|g" "$TMP"
    sed -i '' "s|WIN_ID_PLACEHOLDER|$WIN_ID_FILE|g" "$TMP"

    osascript "$TMP" > /dev/null

    # Block until the test command writes "done" into the FIFO.
    cat "$FIFO" > /dev/null

    # The PocketIC server spawned by cargo test may outlive the Terminal tab,
    # so clean it up explicitly before closing the window.
    pkill -f 'pocket-ic --hard-ttl' 2> /dev/null || true

    WIN_ID=$(cat "$WIN_ID_FILE")
    osascript -e "tell application \"Terminal\" to do script \"exit\" in window id $WIN_ID" > /dev/null

    osascript -e 'display notification "IC E2E test finished" with title "Gleaph"'
else
    cat > "$TMP" <<'EOF'
tell application "Terminal"
    activate
    do script "cd ROOT_PLACEHOLDER && env -u POCKET_IC_BIN cargo test -p gleaph-pocket-ic-tests TEST_ARGS_PLACEHOLDER -- --nocapture; exec zsh"
end tell
EOF

    sed -i '' "s|ROOT_PLACEHOLDER|$ROOT|g" "$TMP"
    sed -i '' "s|TEST_ARGS_PLACEHOLDER|$TEST_ARGS|g" "$TMP"

    osascript "$TMP" > /dev/null
fi
