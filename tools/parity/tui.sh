#!/usr/bin/env bash
# Draws the full-screen views of two snob binaries on the fake Instagram and
# diffs what each showed, frame by frame.
#
# Usage:
#   tools/parity/tui.sh MAIN_BIN MAIN_URI DEV_BIN DEV_URI [OUT_DIR]
#
# MAIN_BIN and DEV_BIN are binaries built with `--features testing` (they take
# `--sandbox-root` and `--ig-base-url`); this script builds nothing. The two
# URIs are the faces `serve_the_fake_world` in crates/snob-cli/tests/headless.rs
# prints: main's binary has no browser and goes on the REST face, and the dev
# binary goes on the web face through the browser (`--through-the-browser`,
# which `DEV_FLAGS` replaces; `DEV_FLAGS= SNOB_NO_BROWSER=1` puts it on the
# REST face instead, and is the one way the dev side reads `SNOB_NO_BROWSER`
# from the caller). With `--through-the-browser` the dev side must leave a
# `browser-owner.log` in its sandbox, or the run fails: a comparison said to go
# through the browser that never started one compares nothing. Run it as the
# user the browser runs as: Chromium refuses root.
#
# Each binary gets a sandbox of its own, logs in with the fake's session, and
# then each view below is drawn in tmux at 100x30 and driven by the same keys.
# After every key the pane is captured once it stops changing, and last once
# the view has left; times, the sandbox and the fake's address are masked,
# and the two binaries' frames are diffed. The two sandboxes' paths are the
# same length, so a border drawn around one is the same width on both sides.
# Nothing is opened outside the terminal: `xdg-open` and `open` are replaced
# on PATH by a stub that only records what it was asked to open.
#
# The frames that differ by design between the REST face and the browser's
# are the ones `EXPECTED_DIFFERENCES` in crates/snob-cli/tests/headless.rs
# lists between the two paths: through the browser, highlights whose item
# count is a dash until the folder is opened and whose date never shows, and
# a profile picture with no size.
set -u

if [ $# -lt 4 ]; then
  sed -n '2,33p' "$0" | sed 's/^# \{0,1\}//'
  exit 2
fi
main_bin=$1 main_uri=${2%/} dev_bin=$3 dev_uri=${4%/}
out=${5:-$(mktemp -d "${TMPDIR:-/tmp}/snob-tui.XXXXXX")}
dev_flags=${DEV_FLAGS---through-the-browser}
sessionid='42%3Aheadless%3A17'
csrftoken='set-by-the-page-load'
socket="snob-parity-$$"

command -v tmux >/dev/null || { echo "tmux is needed" >&2; exit 2; }
mkdir -p "$out"
stub="$out/stub-bin"
mkdir -p "$stub"
for opener in xdg-open open; do
  printf '#!/bin/sh\necho "$@" >> "%s/opened.txt"\n' "$out" > "$stub/$opener"
  chmod 755 "$stub/$opener"
done

# The views, each with the arguments that open it.
views=(
  "profile|profile someone -i"
  "followers|followers someone -i --yes"
  "stories|stories someone -i"
  "highlights|highlights someone -i"
  "pfp|pfp someone -i"
)
# The keys, in order, the same for every view; each is captured after.
# The last, q, is sent by `exited` until the view has left.
keys=(Down Down Enter d Left Up)

# The pane once it has held still for a second, within a minute.
settled() {
  local pane=$1 last="" now="" still=0 waited=0
  while [ "$waited" -lt 120 ]; do
    sleep 0.5
    waited=$((waited + 1))
    now=$(tmux -L "$socket" capture-pane -p -t "$pane" 2>/dev/null)
    if [ -n "$now" ] && [ "$now" = "$last" ]; then
      still=$((still + 1))
      [ "$still" -ge 2 ] && break
    else
      still=0
    fi
    last=$now
  done
  printf '%s\n' "$now"
}

# The pane once the view has left, `q` sent until it does: what the command
# printed after the full screen, the exit status last.
exited() {
  local pane=$1 tries=0 waited
  while [ "$tries" -lt 4 ]; do
    waited=0
    while [ "$waited" -lt 40 ]; do
      if tmux -L "$socket" capture-pane -p -t "$pane" | grep -q '^\[exited'; then
        tmux -L "$socket" capture-pane -p -t "$pane"
        return
      fi
      sleep 0.5
      waited=$((waited + 1))
    done
    tmux -L "$socket" send-keys -t "$pane" q
    tries=$((tries + 1))
  done
  tmux -L "$socket" capture-pane -p -t "$pane"
}

# A frame with what depends on the run made the same on both sides.
masked() {
  local root=$1 uri=$2
  sed -E \
    -e "s#${root}#<root>#g" \
    -e "s#${uri}#<instagram>#g" \
    -e 's#[A-Z][a-z]{2} [0-9]{1,2} at [0-9]{2}:[0-9]{2}#<when>#g' \
    -e 's#[0-9]{4}-[0-9]{2}-[0-9]{2}( [0-9]{2}:[0-9]{2}(:[0-9]{2})?)?#<date>#g' \
    -e 's#\b[0-9]{1,2}:[0-9]{2}(:[0-9]{2})?\b#<time>#g' \
    -e 's#\b[0-9]+ requests?\b#<n> requests#g' \
    -e 's#\([0-9]+ s\)#(<n> s)#g' \
    -e 's#/run-[0-9]+/#/run-<n>/#g' \
    -e 's#[[:space:]]+$##'
}

# Logs `bin` in on `uri` in a fresh sandbox under `$out/<place>` and draws
# every view, the frames under `$out/<side>`. The places are `a` and `b`, so
# both sandboxes' paths are the same length.
drive() {
  local side=$1 place=$2 bin=$3 uri=$4 flags=$5
  local root="$out/$place/sandbox" work="$out/$place/work" frames="$out/$side/frames"
  rm -rf "$out/$side" "$out/$place"
  mkdir -p "$root" "$work" "$frames"
  local run="env PATH=$stub:$PATH NO_COLOR=1 $bin --sandbox-root $root --ig-base-url $uri $flags"
  if ! printf '%s\n' "$sessionid" | (cd "$work" && $run login --paste --csrftoken "$csrftoken") \
      > "$out/$side/login.txt" 2>&1; then
    echo "$side: the login failed; see $out/$side/login.txt" >&2
    return 1
  fi
  local view name args n key
  for view in "${views[@]}"; do
    name=${view%%|*} args=${view#*|}
    tmux -L "$socket" -f /dev/null new-session -d -s "$name" -x 100 -y 30 -c "$work" \
      "$run $args; echo '[exited' \$?']'; sleep 3600"
    settled "$name" | masked "$root" "$uri" > "$frames/$name-0-start.txt"
    n=1
    for key in "${keys[@]}"; do
      tmux -L "$socket" send-keys -t "$name" "$key"
      settled "$name" | masked "$root" "$uri" > "$frames/$name-$n-$key.txt"
      n=$((n + 1))
    done
    # Then out of the view altogether, and what it printed once it left.
    exited "$name" | masked "$root" "$uri" > "$frames/$name-$n-end.txt"
    tmux -L "$socket" kill-session -t "$name"
  done
  ls "$work" > "$out/$side/downloaded.txt"
}

trap 'tmux -L "$socket" kill-server 2>/dev/null' EXIT
# The owner of the browsers is never turned off from the caller, and the
# browser only by `DEV_FLAGS=` with `SNOB_NO_BROWSER=1` (above).
unset SNOB_NO_OWNER
(unset SNOB_NO_BROWSER; drive main a "$main_bin" "$main_uri" "") || exit 1
if [ -n "$dev_flags" ]; then
  (unset SNOB_NO_BROWSER; drive dev b "$dev_bin" "$dev_uri" "$dev_flags") || exit 1
  if [ -z "$(find "$out/b/sandbox" -name browser-owner.log 2>/dev/null)" ]; then
    echo "dev: no browser-owner.log in its sandbox, so no browser drew its views" >&2
    exit 1
  fi
else
  drive dev b "$dev_bin" "$dev_uri" "" || exit 1
fi

status=0
for frame in "$out/main/frames"/*.txt; do
  name=$(basename "$frame")
  if ! diff -u --label "main/$name" --label "dev/$name" "$frame" "$out/dev/frames/$name" \
      >> "$out/frames.diff"; then
    echo "differs: $name"
    status=1
  fi
done
if ! diff -u --label main/downloaded --label dev/downloaded \
    "$out/main/downloaded.txt" "$out/dev/downloaded.txt" >> "$out/frames.diff"; then
  echo "differs: the files the views downloaded"
  status=1
fi
echo "frames in $out/main/frames and $out/dev/frames; differences in $out/frames.diff"
exit $status
