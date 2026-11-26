#!/bin/sh
set -eu

# Refuse to report unless this worker inherited the isolated daemon's environment.
test "$POWER_SOCKET" = "$1"
test "$POWER_STATE_DIR" = "$2"
test "$(pwd -P)" = "$3"
shift 3
test -n "$POWER_SESSION"
test -n "$POWER_BOX"
test -n "$POWER_AGENT"
test -n "$POWER_RUN"
test -n "$POWER_TASK"
test -n "$POWER_ATTEMPT"

fixture_dir="${POWER_SOCKET%/*}"
: > "$fixture_dir/$2.started"

wait_for_marker() {
    marker="$fixture_dir/$1"
    remaining=1200
    until test -f "$marker"; do
        if test "$remaining" -eq 0; then
            printf 'Timed out waiting for %s\n' "$marker" >&2
            exit 1
        fi
        remaining=$((remaining - 1))
        sleep 0.05
    done
}

report() {
    "$POWER_BIN" dag report "$1" --summary "$2"
}

case "$1" in
    success)
        wait_for_marker "$2"
        report success "$3"
        ;;
    input)
        report needs-input "Which option should I use?"
        wait_for_marker "$2"
        report working "Proceeding with the selected option"
        wait_for_marker "$3"
        report success "Used the selected option"
        ;;
    exit)
        wait_for_marker "$2"
        printf 'Fixture exiting before reporting a result\n' >&2
        exit 23
        ;;
    label)
        wait_for_marker "$2"
        "$POWER_BIN" session task "$3"
        wait_for_marker "$4"
        exit 23
        ;;
    *)
        printf 'Unknown worker mode: %s\n' "$1" >&2
        exit 2
        ;;
esac
