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
        'talk-as '*)
            # Runs power talk from inside this worker's session: talk-as KEY SESSION ARGS...
            # SESSION "-" keeps this worker's POWER_SESSION, "none" drops it, and anything
            # else claims that session instead.
            set -- ${line#talk-as }
            key=$1
            claim=$2
            shift 2
            status=0
            case $claim in
                -) "$POWER_BIN" talk "$@" ;;
                none) env -u POWER_SESSION "$POWER_BIN" talk "$@" ;;
                *) POWER_SESSION=$claim "$POWER_BIN" talk "$@" ;;
            esac > "$root/talk-$key.out" 2> "$root/talk-$key.err" || status=$?
            printf '%s\n' "$status" > "$root/talk-$key.tmp"
            mv "$root/talk-$key.tmp" "$root/talk-$key.status"
            ;;
        'report-choices '*)
            # Asks from inside this worker's session: report-choices TASK ATTEMPT CHOICES.
            set -- ${line#report-choices }
            status=0
            "$POWER_BIN" dag report needs-input --summary 'Ship it?' --task "$1" \
                --attempt "$2" --choices "$3" \
                > "$root/report-$name.out" 2> "$root/report-$name.err" || status=$?
            printf '%s\n' "$status" > "$root/report-$name.tmp"
            mv "$root/report-$name.tmp" "$root/report-$name.status"
            ;;
    esac
done
