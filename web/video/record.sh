#!/usr/bin/env bash
# Records what the hero's video shows: the full-screen views of a snob built
# with `--features testing`, drawn in tmux on the invented world that
# `serve_the_website_world` serves (crates/snob-cli/tests/headless.rs), and
# what `snob mcp` answers an agent on the same world. Every account in it is
# invented, and nothing here reaches Instagram.
#
# Usage, from anywhere in the repository:
#
#   bash web/video/record.sh
#
# It builds what it runs (a debug build of the `testing` features), and
# writes web/video/captures/:
#
# - one `.ans` file per frame: the pane as `tmux capture-pane -e` saw it, with
#   its colors, under a first line saying where the cursor was and whether it
#   showed (`cursor X Y SHOWN`). A frame is captured once the pane has held
#   still, after the keys the step below names;
# - `mcp-*.json`, what the MCP server answered (`web/video/mcp.mjs`).
#
# `pnpm video` (web/scripts/video.mjs) turns them into the video. Run this
# again when a view changes, and commit what it writes with what it draws.
#
# The world is the REST face, with `SNOB_NO_BROWSER=1`: the views draw the same
# on either path, and no browser has to start for a recording. The commands
# typed are the ones a person types; `snob` on the shell's PATH is a wrapper
# that adds the sandbox and the world's address.
set -euo pipefail

cols=68
rows=19

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/../.." && pwd)
out="$here/captures"
work=$(mktemp -d "${TMPDIR:-/tmp}/snob-video.XXXXXX")
socket="snob-video-$$"
server=""

finish() {
  tmux -L "$socket" kill-server 2>/dev/null || true
  [ -n "$server" ] && kill "$server" 2>/dev/null
  rm -rf "$work"
}
trap finish EXIT

for tool in tmux cargo node; do
  command -v "$tool" >/dev/null || { echo "$tool is needed" >&2; exit 2; }
done

cd "$repo"
cargo build -p snob-cli --features testing --locked
tests=$(cargo test -p snob-cli --features testing --locked --test headless --no-run \
  --message-format=json 2>/dev/null |
  grep -o '"executable":"[^"]*headless-[^"]*"' | tail -n 1 | cut -d'"' -f4)
[ -x "$tests" ] || { echo "the headless test binary was not built" >&2; exit 1; }

# The world, for as long as the recording takes and no longer.
SNOB_TEST_SERVE_SECS=900 "$tests" serve_the_website_world --exact --ignored --nocapture \
  > "$work/world.log" 2>&1 &
server=$!
uri=""
for _ in $(seq 1 100); do
  uri=$(grep -o 'http://127\.0\.0\.1:[0-9]*' "$work/world.log" || true)
  [ -n "$uri" ] && break
  sleep 0.1
done
[ -n "$uri" ] || { cat "$work/world.log" >&2; exit 1; }

# `snob` as typed, and openers that only say what they were asked to open.
mkdir -p "$work/bin" "$work/home" "$work/sandbox"
cat > "$work/bin/snob" <<EOF
#!/bin/sh
exec "$repo/target/debug/snob" --sandbox-root "$work/sandbox" --ig-base-url "$uri" "\$@"
EOF
for opener in xdg-open open; do
  printf '#!/bin/sh\necho "$@" >> "%s/opened.txt"\n' "$work" > "$work/bin/$opener"
done
chmod 755 "$work/bin"/*
export SNOB_NO_BROWSER=1
printf '%s\n' '42%3Aheadless%3A17' |
  PATH="$work/bin:$PATH" snob login --paste --csrftoken set-by-the-page-load > "$work/login.txt" 2>&1 ||
  { cat "$work/login.txt" >&2; exit 1; }

rm -rf "$out"
mkdir -p "$out"

# A shell with nothing of this one's: its prompt, the world's `snob`, and the
# 16 colors the views draw with.
tmux -L "$socket" -f /dev/null new-session -d -s video -x "$cols" -y "$rows" -c "$work/home" \
  "env -i HOME='$work/home' TERM=xterm-256color LANG=C.UTF-8 SNOB_NO_BROWSER=1 \
   PATH='$work/bin:/usr/bin:/bin' PS1='\$ ' bash --norc --noprofile"
tmux -L "$socket" set -g status off
tmux -L "$socket" resize-window -t video -x "$cols" -y "$rows"

pane() { tmux -L "$socket" capture-pane -p -e -t video; }

# Writes the pane once it has held still for a second, within a minute.
shot() {
  local name=$1 last="" now="" still=0 waited=0
  while [ "$waited" -lt 120 ]; do
    sleep 0.5
    waited=$((waited + 1))
    now=$(pane)
    if [ -n "$now" ] && [ "$now" = "$last" ]; then
      still=$((still + 1))
      [ "$still" -ge 2 ] && break
    else
      still=0
    fi
    last=$now
  done
  {
    tmux -L "$socket" display -p -t video 'cursor #{cursor_x} #{cursor_y} #{cursor_flag}'
    printf '%s\n' "$now"
  } > "$out/$name.ans"
}

keys() { tmux -L "$socket" send-keys -t video "$@"; }
typed() { tmux -L "$socket" send-keys -t video -l "$1"; }

# A clean screen and a prompt, between scenes; never recorded typing.
fresh() {
  keys clear Enter
  shot "$1"
}

# The whole account at a glance, and who does not follow back.
fresh scan-prompt
typed 'snob scan'
shot scan-typed
keys Enter
shot scan-tray
keys Enter
shot scan-unfollowers
keys Down
shot scan-down-1
keys Down
shot scan-down-2
keys Down
shot scan-down-3
keys Enter
shot scan-peek
keys Escape
shot scan-back
keys q
shot scan-receipts

# A profile, its stories, and one kept.
fresh profile-prompt
typed 'snob profile lena.frames'
shot profile-typed
keys Enter
shot profile-card
keys Down
shot profile-stories-row
keys Enter
shot profile-stories
keys d
shot profile-saved
keys q
shot profile-receipts

# The same world, through an agent: the list, then an unfollow asked first.
# Last, since the unfollow changes the world.
PATH="$work/bin:$PATH" node "$here/mcp.mjs" "$out"

echo "frames and answers in $out"
