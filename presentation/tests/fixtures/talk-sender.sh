#!/bin/sh
# A coding agent started by hand in a shell tab. It inherits the shell's POWER_SESSION and
# sends at once on launch, before Power's periodic foreground check can have seen it. It
# exits when the test creates "$root/release-sender".
set -u

root=$TALK_TEST_ROOT
"$POWER_BIN" talk send "$1" "sent on launch" --json >"$root/sent-launch.json" 2>"$root/sent-launch.err"
printf 'sent\n' >"$root/launched"
while [ ! -e "$root/release-sender" ]; do sleep 0.02; done
printf 'exited\n' >"$root/sender-exited"
