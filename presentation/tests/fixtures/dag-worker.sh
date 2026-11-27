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
        # An optional fourth marker keeps the session alive after the report, as agents do.
        if test -n "${4:-}"; then
            wait_for_marker "$4"
        fi
        ;;
    linger)
        # Reports success, then keeps running with a child and a grandchild that
        # left the session, the way an agent keeps its MCP servers and shells.
        wait_for_marker "$2"
        sleep 600 &
        child=$!
        setsid sleep 600 &
        escaped=$!
        printf '%s %s %s\n' "$$" "$child" "$escaped" > "$fixture_dir/$4.tmp"
        mv "$fixture_dir/$4.tmp" "$fixture_dir/$4"
        report success "$3"
        wait
        ;;
    detach)
        # Reports success, starts a child in its own session, and exits first.
        wait_for_marker "$2"
        setsid sleep 600 &
        printf '%s %s\n' "$$" "$!" > "$fixture_dir/$4.tmp"
        mv "$fixture_dir/$4.tmp" "$fixture_dir/$4"
        report success "$3"
        ;;
    escape|escape-stay)
        # Starts a helper that leaves the session and drops its whole environment,
        # POWER_SESSION included, so it is reparented once its parent exits. Then
        # reports success, and exits or, with escape-stay, keeps running.
        wait_for_marker "$2"
        helper="$fixture_dir/$4.helper"
        setsid -f env -i /bin/sh -c 'echo $$ > "$0.tmp"; mv "$0.tmp" "$0"; exec sleep 600' "$helper"
        until test -f "$helper"; do sleep 0.05; done
        printf '%s %s\n' "$$" "$(cat "$helper")" > "$fixture_dir/$4.tmp"
        mv "$fixture_dir/$4.tmp" "$fixture_dir/$4"
        report success "$3"
        if test "$1" = escape-stay; then
            exec sleep 600
        fi
        ;;
    stubborn)
        # Like escape-stay, but the helper ignores SIGTERM, so only a kill ends it.
        wait_for_marker "$2"
        helper="$fixture_dir/$4.helper"
        setsid -f env -i /bin/sh -c 'trap "" TERM; echo $$ > "$0.tmp"; mv "$0.tmp" "$0"; while :; do sleep 1; done' "$helper"
        until test -f "$helper"; do sleep 0.05; done
        printf '%s %s\n' "$$" "$(cat "$helper")" > "$fixture_dir/$4.tmp"
        mv "$fixture_dir/$4.tmp" "$fixture_dir/$4"
        report success "$3"
        exec sleep 600
        ;;
    pausable)
        # A parent and its child that each append a tick every tenth of a second,
        # so a pause shows as no new ticks, until the marker reports success.
        tick() { while :; do echo "$1" >> "$fixture_dir/$2.ticks"; sleep 0.1; done; }
        tick child "$3" &
        child=$!
        printf '%s %s\n' "$$" "$child" > "$fixture_dir/$3.tmp"
        mv "$fixture_dir/$3.tmp" "$fixture_dir/$3"
        until test -f "$fixture_dir/$2"; do echo parent >> "$fixture_dir/$3.ticks"; sleep 0.1; done
        report success "Ran to the end"
        kill "$child"
        ;;
    termable)
        # Records its PID, then reports success once the marker exists. A SIGTERM
        # writes "$3.term" and exits without reporting, so a stop that ends it
        # gracefully leaves its attempt interrupted.
        trap 'echo term > "$fixture_dir/$3.term"; exit 0' TERM
        printf '%s\n' "$$" > "$fixture_dir/$3.tmp"
        mv "$fixture_dir/$3.tmp" "$fixture_dir/$3"
        # `wait` returns at once for a trapped signal, so the handler runs promptly.
        until test -f "$fixture_dir/$2"; do sleep 0.1 & wait $! || true; done
        report success "Ran on after a stop"
        ;;
    term-ignoring)
        # Ignores SIGTERM and reports success as soon as the marker exists, the way a
        # worker that defers or ignores the end of a stop reports during the teardown.
        trap '' TERM
        printf '%s\n' "$$" > "$fixture_dir/$3.tmp"
        mv "$fixture_dir/$3.tmp" "$fixture_dir/$3"
        until test -f "$fixture_dir/$2"; do sleep 0.05; done
        "$POWER_BIN" dag report success --summary "Reported after stop" > "$fixture_dir/$3.report" 2>&1 || true
        : > "$fixture_dir/$3.reported"
        exec sleep 600
        ;;
    graced)
        # Reports success, then marks each tenth of a second it still runs.
        wait_for_marker "$2"
        report success "$3"
        while :; do
            date +%s%N >> "$fixture_dir/$4"
            sleep 0.1
        done
        ;;
    input)
        report needs-input "Which option should I use?"
        wait_for_marker "$2"
        report working "Proceeding with the selected option"
        wait_for_marker "$3"
        report success "Used the selected option"
        ;;
    idle)
        # Sits silent with a child that does nothing, like an agent whose build was killed.
        sleep 600 &
        wait
        ;;
    busy)
        # Sits silent while a child keeps working, like an agent waiting on a build.
        /bin/sh -c 'while :; do sleep 0.2; done' &
        wait
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
