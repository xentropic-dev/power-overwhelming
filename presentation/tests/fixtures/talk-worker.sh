#!/bin/sh
set -eu

root=$TALK_TEST_ROOT
name=${1:-${POWER_TASK:?}}
stty -echo
printf 'ready\n' >> "$root/ready-$name"

while IFS= read -r line; do
    case $line in
        '[power talk]'*)
            printf '%s\n' "$line" >> "$root/notice-$name"
            if [ -e "$root/read-$name" ]; then
                "$POWER_BIN" talk inbox > "$root/inbox-$name.tmp"
                mv "$root/inbox-$name.tmp" "$root/inbox-$name"
            fi
            if [ -e "$root/reply-$name" ]; then
                rm "$root/reply-$name"
                read -r prefix marker id rest < "$root/inbox-$name"
                "$POWER_BIN" talk reply "$id" 'fixture reply'
            fi
            ;;
    esac
done
