#!/usr/bin/env bash
# TPM entry point for ymir-agent-sidebar.
#
# Binds a key that opens the agent sidebar in a full-screen tmux popup, plus
# optional vim-aware pane/window navigation. Sourced by TPM at tmux start, so it
# only wires key bindings — it never builds or downloads (that happens lazily on
# first use, from the launcher script).
#
# Options (set before this plugin is loaded):
#   set -g @agent_sidebar_key 'C-n'   # key that opens the sidebar (default C-n)
#   set -g @agent_sidebar_nav 'on'    # vim-aware C-h/C-j/C-k/C-l nav (default on)
set -euo pipefail

CURRENT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
POPUP="$CURRENT_DIR/bin/ymir-agent-sidebar-popup"

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
    tmux display-message "ymir-agent-sidebar: requires tmux >= 3.3 (found $(tmux -V))"
    exit 0
  fi
fi

open_key="$(tmux_option @agent_sidebar_key C-n)"
nav="$(tmux_option @agent_sidebar_nav on)"

# Open the sidebar. Free the key first in case tmux binds it by default.
tmux unbind-key -n "$open_key" 2>/dev/null || true
tmux bind-key -n "$open_key" run-shell -b "$POPUP '#{window_id}' '#{session_name}'"

if [[ "$nav" == "on" ]]; then
  # Vim-aware navigation: pass C-h/j/k/l through to Vim when the focused pane
  # runs it, otherwise:
  #   C-h / C-l  move panes (or wrap to prev/next window at an edge)
  #   C-j / C-k  open the sidebar pre-moved one row down / up
  is_vim="ps -o state= -o comm= -t '#{pane_tty}' | grep -iqE '^[^TXZ ]+ +(\\S+/)?g?(view|n?vim?x?)(diff)?\$'"

  tmux bind-key -n C-h if-shell "$is_vim" "send-keys C-h" "if -F '#{pane_at_left}' 'previous-window' 'select-pane -L'"
  tmux bind-key -n C-l if-shell "$is_vim" "send-keys C-l" "if -F '#{pane_at_right}' 'next-window' 'select-pane -R'"
  tmux bind-key -n C-j if-shell "$is_vim" "send-keys C-j" "run-shell -b \"$POPUP '#{window_id}' '#{session_name}' down\""
  tmux bind-key -n C-k if-shell "$is_vim" "send-keys C-k" "run-shell -b \"$POPUP '#{window_id}' '#{session_name}' up\""

  # Keep the nav keys from being swallowed by tmux's tree-mode.
  tmux unbind-key -q -T tree-mode C-j 2>/dev/null || true
  tmux unbind-key -q -T tree-mode C-k 2>/dev/null || true
  tmux unbind-key -q -T tree-mode C-n 2>/dev/null || true
fi
