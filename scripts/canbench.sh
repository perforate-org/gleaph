#!/bin/sh
# Run canbench in an external terminal on macOS.
# Usage: canbench.sh <project-root> [--inline] <crate> [pattern] [--close]
#
# Argument order matters: flags are parsed independently, but the first non-flag argument after
# <project-root> is the crate. Example: canbench.sh <root> --inline graph search_join
#
# crate:
#   graph    - run in crates/graph
#   router   - run in crates/router
#   ...      - any crate name under crates/
#
# pattern:
#   (none)   - run the full canbench suite for the crate
#   <name>   - run only benchmarks whose name contains <name>
#
# --inline:
#   Run canbench directly in the current terminal instead of opening Terminal.app.
#
# --close:
#   Close the Terminal.app window/tab and show a macOS notification when finished.

set -eu

ROOT="$1"
shift

CRATE=""
PATTERN=""
CLOSE=0
INLINE=0

for arg in "$@"; do
    case "$arg" in
        --close) CLOSE=1 ;;
        --inline) INLINE=1 ;;
        *)
            if [ -z "$CRATE" ]; then
                CRATE="$arg"
            elif [ -z "$PATTERN" ]; then
                PATTERN="$arg"
            fi
            ;;
    esac
done

if [ -z "$CRATE" ]; then
    echo "usage: canbench.sh <project-root> [--inline] <crate> [pattern] [--close]" >&2
    exit 1
fi

if [ "$INLINE" -eq 1 ]; then
    if [ -z "$PATTERN" ]; then
        exec sh -c "cd $ROOT/crates/$CRATE && canbench --persist"
    else
        exec sh -c "cd $ROOT/crates/$CRATE && canbench '$PATTERN'"
    fi
fi

if [ -z "$PATTERN" ]; then
    CMD="cd $ROOT/crates/$CRATE && canbench --persist"
    NOTIFICATION="canbench $CRATE finished"
else
    CMD="cd $ROOT/crates/$CRATE && canbench '$PATTERN'"
    NOTIFICATION="canbench $CRATE '$PATTERN' finished"
fi

FIFO="/tmp/gleaph-canbench-$CRATE.fifo"
WIN_ID_FILE="/tmp/gleaph-canbench-$CRATE.window-id"
LOG="/tmp/gleaph-canbench-$CRATE.log"
TMP=$(mktemp /tmp/gleaph_canbench_$CRATE.XXXXXX)

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
    set w to do script "CMD_PLACEHOLDER > LOG_PLACEHOLDER 2>&1; echo done > FIFO_PLACEHOLDER; exec zsh"
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

    ESCAPED_CMD=$(printf '%s' "$CMD" | sed 's/&/\\\&/g')
    sed -i '' "s|CMD_PLACEHOLDER|$ESCAPED_CMD|g" "$TMP"
    sed -i '' "s|LOG_PLACEHOLDER|$LOG|g" "$TMP"
    sed -i '' "s|FIFO_PLACEHOLDER|$FIFO|g" "$TMP"
    sed -i '' "s|WIN_ID_PLACEHOLDER|$WIN_ID_FILE|g" "$TMP"

    osascript "$TMP" > /dev/null

    # Block until the test command writes "done" into the FIFO.
    cat "$FIFO" > /dev/null

    # The PocketIC server spawned by canbench may outlive the Terminal tab,
    # so clean it up explicitly before closing the window.
    pkill -f 'pocket-ic --hard-ttl' 2> /dev/null || true

    WIN_ID=$(cat "$WIN_ID_FILE")
    osascript -e "tell application \"Terminal\" to do script \"exit\" in window id $WIN_ID" > /dev/null

    osascript -e "display notification \"$NOTIFICATION\" with title \"Gleaph\""
else
    cat > "$TMP" <<'EOF'
tell application "Terminal"
    activate
    do script "CMD_PLACEHOLDER; exec zsh"
end tell
EOF

    ESCAPED_CMD=$(printf '%s' "$CMD" | sed 's/&/\\\&/g')
    sed -i '' "s|CMD_PLACEHOLDER|$ESCAPED_CMD|g" "$TMP"

    osascript "$TMP" > /dev/null
fi
