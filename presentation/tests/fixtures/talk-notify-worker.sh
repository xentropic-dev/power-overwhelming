#!/bin/sh
# A notify-only provider: it reports turn ends the way Codex `notify` does, then
# repaints its prompt after the callback.
set -eu

root=$TALK_TEST_ROOT
for arg; do name=$arg; done
case " $* " in
    *notify=*) ;;
    *) echo "Power did not pass the notify callback" >&2; exit 1 ;;
esac
stty -echo
"$POWER_BIN" talk busy
"$POWER_BIN" talk ready '{"type":"agent-turn-complete"}'
printf '\033[2K\r> prompt\n'
printf 'ready\n' >> "$root/ready-$name"

while IFS= read -r line; do
    case $line in
        '[power talk]'*) printf '%s\n' "$line" >> "$root/notice-$name" ;;
    esac
done
