#!/usr/bin/env bash
# TPM entry point for tmux-agent-dock.
#
# Binds a key that opens the agent sidebar in a full-screen tmux popup, plus
# optional vim-aware pane/window navigation. Sourced by TPM at tmux start, so it
# only wires key bindings — it never builds or downloads (that happens lazily on
# first use, from the launcher script).
#
# Options (set before this plugin is loaded):
#   set -g @agent_dock_popup_key 'C-n'  # key that opens the popup (default C-n)
#   set -g @agent_dock_toggle_key 'b'   # prefix key toggling the dock (default b)
#   set -g @agent_dock_nav 'on'    # vim-aware C-h/C-j/C-k/C-l nav (default on)
#   set -g @agent_dock_tab_status 'on' # agent indicator in window tabs (default on)
set -euo pipefail

CURRENT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
POPUP="$CURRENT_DIR/bin/tmux-agent-dock-popup"

tmux_option() {
  local value
  value="$(tmux show-option -gqv "$1")"
  if [[ -z "$value" ]]; then echo "$2"; else echo "$value"; fi
}

# --- tmux version guard: display-popup -B/-e require tmux >= 3.3 ---
version="$(tmux -V | grep -oE '[0-9]+\.[0-9]+' | head -n1 || true)"
if [[ -n "$version" ]]; then
  major="${version%%.*}"
  minor="${version##*.}"
  if (( major < 3 || (major == 3 && minor < 3) )); then
    tmux display-message "tmux-agent-dock: requires tmux >= 3.3 (found $(tmux -V))"
    exit 0
  fi
fi

open_key="$(tmux_option @agent_dock_popup_key C-n)"
nav="$(tmux_option @agent_dock_nav on)"
tab_status="$(tmux_option @agent_dock_tab_status on)"

configure_tab_status() {
  local option="$1"
  local marker='#{@tmux_agent_dock_window_icon}'
  local current
  current="$(tmux show-option -gqv "$option")"

  # Keep the user's format intact and make reloads idempotent.
  current="${current//$marker/}"
  if [[ "$tab_status" == "on" ]]; then
    current="${current}${marker}"
  fi
  tmux set-option -gq "$option" "$current"
}

configure_tab_status window-status-format
configure_tab_status window-status-current-format

# Dedicated switcher opener.
if [[ -n "$open_key" ]]; then
  tmux unbind-key -n "$open_key" 2>/dev/null || true
  tmux bind-key -n "$open_key" run-shell -b "$POPUP '#{window_id}' '#{session_name}'"
fi

# Vim-aware navigation: pass C-h/j/k/l through to Vim when the focused pane
# runs it, otherwise:
#   C-h / C-l  move panes (or wrap to prev/next window at an edge)
#   C-j / C-k  switch to the next/previous session
is_vim="ps -o state= -o comm= -t '#{pane_tty}' | grep -iqE '^[^TXZ ]+ +(\\S+/)?g?(view|n?vim?x?)(diff)?\$'"

# tmux keeps its key tables across config reloads, so switching the option off
# has to actively remove what an earlier load bound — otherwise the keys keep
# navigating until the server is restarted, and C-l never reaches the shell.
# Only this plugin's own binding is dropped: it is recognized by the exact
# vim-passthrough-plus-action pair below, which a binding the user installed
# under the same key will not carry. Read the whole table once — `list-keys`
# with both -T and a key argument returns nothing.
root_keys=""
if [[ "$nav" != "on" ]]; then
  root_keys="$(tmux list-keys -T root 2>/dev/null || true)"
fi

configure_nav_key() {
  local key="$1" action="$2"

  if [[ "$nav" == "on" ]]; then
    tmux bind-key -n "$key" if-shell "$is_vim" "send-keys $key" "$action"
  elif [[ "$root_keys" == *"\"send-keys $key\" \"$action\""* ]]; then
    tmux unbind-key -n "$key"
  fi
}

configure_nav_key C-h "if -F '#{pane_at_left}' 'previous-window' 'select-pane -L'"
configure_nav_key C-l "if -F '#{pane_at_right}' 'next-window' 'select-pane -R'"
configure_nav_key C-j "switch-client -n"
configure_nav_key C-k "switch-client -p"

if [[ "$nav" == "on" ]]; then
  # Keep the nav keys from being swallowed by tmux's tree-mode.
  tmux unbind-key -q -T tree-mode C-j 2>/dev/null || true
  tmux unbind-key -q -T tree-mode C-k 2>/dev/null || true
fi

# --- docked sidebar -------------------------------------------------------
# A pane, not a popup: it stays while you work and follows you between windows.
dock_key="$(tmux_option @agent_dock_toggle_key b)"
LAUNCHER="$CURRENT_DIR/bin/tmux-agent-dock"

if [[ -n "$dock_key" ]]; then
  tmux bind-key "$dock_key" run-shell -b "$LAUNCHER dock-toggle"
fi

# The hooks run the binary directly when there is one, rather than the launcher.
#
# The launcher is a bash script that finds, downloads or builds the binary before
# exec'ing it — right for an interactive open, pure overhead on a hook. Measured:
# 40ms through the launcher against 20ms straight to the binary. That time is the
# destination window being drawn *without* the sidebar, before the follow moves it
# in, so every millisecond of it is on screen.
#
# Falls back to the launcher when nothing is built yet, so a fresh install still
# works; the next config reload picks the binary up.
BINARY="$CURRENT_DIR/target/release/tmux-agent-dock"
FOLLOWER="$BINARY"
[[ -x "$FOLLOWER" ]] || FOLLOWER="$LAUNCHER"

# --- status daemon --------------------------------------------------------
# The daemon writes the per-window options the status line and tab icons read,
# and lends its tick to whatever borrows the heartbeat. Nothing else starts it:
# `ensure_status_daemon` is called from `load_cards`, which only runs when the
# dock or the popup opens. So on a fresh server the agent section of the status
# line stays blank — and continuum stops saving — until you happen to open the
# sidebar once. Start it here, where the server already is.
#
# Guarded rather than fired blind: `run_status_daemon` claims the pid option
# unconditionally, so an unguarded spawn on every config reload would hand the
# job from a healthy daemon to a new one for no gain. The pid is matched against
# `ps` rather than merely signalled, because a daemon that was killed leaves the
# option behind — it only clears it on a clean exit — so "pid set, process gone"
# is the ordinary recovery path, not an edge case.
#
# Only ever the built binary: $FOLLOWER falls back to the launcher, which builds
# before it execs, and a config reload is not the moment to start a compile.
status_daemon_alive() {
  local pid="$1"
  [[ -n "$pid" ]] || return 1
  ps -o command= -p "$pid" 2>/dev/null | grep -q -- ' status-daemon'
}

if [[ -x "$BINARY" ]] &&
  ! status_daemon_alive "$(tmux show-option -gqv @tmux_agent_dock_status_daemon_pid)"; then
  tmux run-shell -b "$(printf '%q' "$BINARY") status-daemon"
fi

# Hooks are arrays. Writing at a reserved index is idempotent across the config
# reloads that re-run this file, and leaves any other plugin's entry at another
# index alone — a plain `set-hook -g` would replace it silently.
#
# `session-window-changed` covers prefix+n, select-window and tree mode;
# `client-session-changed` covers session switches. tmux has no
# `after-switch-client` hook, so that pair is the whole surface.
tmux set-hook -g "session-window-changed[50]" "run-shell -b '$FOLLOWER dock-follow'"
tmux set-hook -g "client-session-changed[50]" "run-shell -b '$FOLLOWER dock-follow'"
