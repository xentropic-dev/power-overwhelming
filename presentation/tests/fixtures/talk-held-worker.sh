#!/bin/sh
# A notify-only provider whose turns end only when the test releases them, like a long
# Codex turn that reports `notify` well after its prompt was submitted. Turn N ends when
# "$root/release-<name>-N" exists. A DAG worker (POWER_TASK set) starts with its task
# prompt, which starts turn 1.
set -eu

root=$TALK_TEST_ROOT
name=$(basename "$PWD")
turns=0
stty -echo

turn() {
    turns=$((turns + 1))
    printf '%s\n' "$1" >> "$root/turn-$name"
    printf 'working on turn %s\n' "$turns"
    while [ ! -e "$root/release-$name-$turns" ]; do sleep 0.02; done
    "$POWER_BIN" talk ready '{"type":"agent-turn-complete"}'
    printf '\033[2K\r> prompt\n'
}

case " $* " in
    *notify=*) ;;
    *) echo "Power did not pass the notify callback" >&2; exit 1 ;;
esac
if [ -n "${POWER_TASK:-}" ]; then
    printf 'ready\n' >> "$root/ready-$name"
    turn initial
else
    "$POWER_BIN" talk ready '{"type":"agent-turn-complete"}'
    printf 'ready\n' >> "$root/ready-$name"
fi

while IFS= read -r line; do
    case $line in
        '[power talk]'*)
            printf '%s\n' "$line" >> "$root/notice-$name"
            turn notice
            ;;
    esac
done
