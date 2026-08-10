# tmux-agent-dock

A tmux sidebar that shows every session and window **and** the live state of the
AI coding agents running in them — as a docked pane that stays open while you
work, or as a full-screen popup.

```
┌────────────────────────────────┬───────────────────────────────────────────┐
│ Sessions                       │                                           │
│                                │                                           │
│   default              5 ▾     │   your work pane, unchanged               │
│ ○   ├─> 1: momeppkt            │                                           │
│ ○   └─> 2: Developer           │   the dock follows you between windows    │
│   dotfiles-config      3 ▾     │   and sessions; it is a real pane, so     │
│ ● ● ├─> 1: .config             │   nothing is drawn over your work         │
│ ○   └─> 2: nvim                │                                           │
│                                │                                           │
│ ────────────────────────────── │                                           │
│ Agents                         │                                           │
│                                │                                           │
│ ● .config                      │                                           │
│   idle · claude_1              │                                           │
└────────────────────────────────┴───────────────────────────────────────────┘
      prefix + b                                   Ctrl+n for the popup
```

> [!NOTE]
> Detection is **fully passive** (see [How it works](#how-it-works)). The plugin
> never wraps, shims, or launches your agents. You run `claude`, `codex`, or
> `opencode` as usual; the sidebar reads tmux and the process table.

## Features

- **A docked sidebar** (<kbd>prefix</kbd>+<kbd>b</kbd>) that stays open beside
  the pane you are working in and follows you across windows and sessions. It is
  an ordinary tmux pane, so it never covers your work, and clicking a row
  switches to it without closing the dock.
- **A popup switcher** (<kbd>Ctrl</kbd>+<kbd>n</kbd>) for when you want the
  full screen and a live preview of the highlighted window.
- **Two sections.** *Sessions* is a collapsible tree of every session and its
  windows; *Agents* lists each running agent with what it is doing.
- **Agent monitoring**: every pane running Claude Code, Codex, or OpenCode is
  tagged Working / Blocked / Idle with a run timer. An agent that finishes
  **out of sight** raises an unread dot; one you watched finish does not.
- **Agents inside editor floats are folded into their host.**
  [sidekick.nvim](https://github.com/folke/sidekick.nvim) runs each agent in its
  own embedded tmux session, which would otherwise list as a peer of your real
  sessions with the Neovim hosting it showing no agent at all. Those are traced
  back to the pane they live in and rolled up, and they stay folded while the
  float is shut.
- **Tab status indicators**: the same rolled-up state is appended to each tmux
  window tab without replacing your existing format.
- **A heartbeat you can borrow** for periodic work that would otherwise depend on
  a visible status line — see [below](#borrowing-the-daemons-heartbeat).
- **Vim-aware navigation**: <kbd>Ctrl</kbd>+<kbd>h/j/k/l</kbd> move between
  panes, windows and sessions but pass through to Vim/Neovim when it is focused.

## Requirements

- **tmux ≥ 3.3** (for `display-popup -B -e`)
- **bash** and **ps** (present on macOS and Linux)
- One or more agents (`claude`, `codex`, `opencode`) running inside tmux panes.
  That is all detection needs; launch them however you like.
- To build from source: a **Rust toolchain** (only needed if no prebuilt binary
  exists for your platform).

## Install

### With [TPM](https://github.com/tmux-plugins/tpm) (recommended)

```tmux
set -g @plugin 'MomePP/tmux-agent-dock'
```

Then press <kbd>prefix</kbd>+<kbd>I</kbd> to install. On first use the plugin
downloads a prebuilt binary for your platform, falling back to a source build if
you have Rust and no prebuilt exists.

### Manual

```sh
git clone https://github.com/MomePP/tmux-agent-dock \
  ~/.config/tmux/plugins/tmux-agent-dock
```

```tmux
run-shell "~/.config/tmux/plugins/tmux-agent-dock/tmux-agent-dock.tmux"
```

See [`examples/tmux.conf`](examples/tmux.conf) for a manual keybinding snippet.

### From source

```sh
cargo install --git https://github.com/MomePP/tmux-agent-dock
```

## Usage

| Key | Action |
| --- | --- |
| <kbd>prefix</kbd>+<kbd>b</kbd> | Toggle the docked sidebar |
| <kbd>Ctrl</kbd>+<kbd>n</kbd> | Open the popup switcher |
| <kbd>Ctrl</kbd>+<kbd>j</kbd> / <kbd>Ctrl</kbd>+<kbd>k</kbd> | Next / previous session (passes through to Vim if focused) |
| <kbd>Ctrl</kbd>+<kbd>h</kbd> / <kbd>Ctrl</kbd>+<kbd>l</kbd> | Move panes, wrap to prev/next window (passes through to Vim) |

### In the sidebar

- <kbd>Tab</kbd> moves focus between **Sessions** and **Agents**.
- <kbd>j</kbd>/<kbd>k</kbd> or <kbd>↑</kbd>/<kbd>↓</kbd> move the cursor;
  <kbd>Enter</kbd> or <kbd>Space</kbd> opens the selected row.
- <kbd>h</kbd>/<kbd>l</kbd> (or <kbd>←</kbd>/<kbd>→</kbd>) collapse and expand a
  session.
- <kbd>Ctrl</kbd>+<kbd>j</kbd>/<kbd>Ctrl</kbd>+<kbd>k</kbd> move **and** open in
  one keystroke.
- Clicking any row selects it. In the dock this switches without closing.
- <kbd>r</kbd> renames the selected window, <kbd>n</kbd>/<kbd>N</kbd> create a
  window/session, <kbd>?</kbd> shows every shortcut.
- <kbd>Shift</kbd>+<kbd>Tab</kbd> switches between keys and search;
  <kbd>v</kbd> cycles the view (**sidebar**, **sidebar-right**, **palette**).
- In the dock, <kbd>Esc</kbd> or <kbd>q</kbd> hands the keyboard back to your
  work pane without closing the sidebar — only
  <kbd>prefix</kbd>+<kbd>b</kbd> closes it.

The cursor is only drawn while the sidebar holds the keyboard, so a dock sitting
beside the pane you are typing in shows what is attached and what is running,
and nothing else.

## Configuration

Set these **before** the plugin loads:

```tmux
set -g @agent_dock_popup_key 'C-n'  # key that opens the popup (default: C-n)
set -g @agent_dock_toggle_key 'b'   # prefix key that toggles the dock (default: b)
set -g @agent_dock_width '30'       # dock width in columns (default: 30)
set -g @agent_dock_nav 'on'         # vim-aware C-h/C-j/C-k/C-l nav (default: on)
set -g @agent_dock_view 'sidebar'   # 'sidebar' (left), 'sidebar-right', or 'palette'
set -g @agent_dock_input 'keys'     # 'keys' (default), 'search', or 'numbers' (palette only)
set -g @agent_dock_tab_status 'on'  # show agent state in tmux window tabs
set -g @agent_dock_expand_default 'all' # 'all' (default), 'attached', or 'none'
```

Set `@agent_dock_nav 'off'` if you already bind
<kbd>Ctrl</kbd>+<kbd>h/j/k/l</kbd> yourself. Set `@agent_dock_tab_status 'off'`
to leave tmux's window status formats untouched.

### Borrowing the daemon's heartbeat

**tmux has no timer.** Anything periodic has to hang off something tmux redraws,
and the usual choice is the status line, whose `#()` interpolations re-run every
`status-interval`. That is how
[tmux-continuum](https://github.com/tmux-plugins/tmux-continuum) saves sessions —
and why `set -g status off` stops it saving, quietly, until you notice weeks
later that there is nothing to restore.

The status daemon is already a heartbeat: it runs for the life of the tmux server
and is respawned if it dies. It will run a command of your choosing on an
interval, so periodic work need not depend on a visible status line:

```tmux
set -g @agent_dock_tick_command ''     # shell command to run periodically (default: none)
set -g @agent_dock_tick_interval '60'  # seconds between runs (default: 60)
```

Keeping continuum saving with no status line at all:

```tmux
set -g status off
set -g @agent_dock_tick_command '${TMUX_PLUGIN_MANAGER_PATH:-$HOME/.tmux/plugins/}tmux-continuum/scripts/continuum_save.sh'
```

The command runs through `sh -c`, so arguments, `~` and pipelines all work. It is
started and not waited for, so a slow one cannot stall status polling — and
nothing throttles it beyond the interval, so a command that must not run too
often should keep its own timestamp, as continuum's save script already does.

## How it works

The plugin observes; it never intercepts. Agent state comes from:

- `tmux` pane metadata: `pane_current_command` and the OSC pane title,
- `tmux capture-pane`: the visible screen text, and
- a `ps` process-tree snapshot that attributes agents to panes.

**Working** is inferred from the agent's on-screen activity indicators,
**Blocked** from a prompt awaiting input, and **Idle** once activity settles
(with debouncing, so a single stray sample cannot flash a false "done"). There
are no wrappers, shims, PID files, FIFOs, `LD_PRELOAD`, or log scraping.

The dock is one pane that is *moved*, not one pane per window. A tmux pane
belongs to exactly one window, so "always visible" has to be built: a hook
carries the pane into whatever window becomes active, and a switch made from the
sidebar itself puts that move *in front of* the switch in the same tmux command
list, so the destination is drawn with the sidebar already in it.

> [!WARNING]
> Because detection reads the agents' on-screen output, it is **heuristic and
> version-sensitive**: a Claude/Codex/OpenCode UI change, a custom theme, or a
> non-English locale can throw off state classification. It is best-effort and
> expected to need occasional upkeep as the agent CLIs evolve.

## Development

```sh
cargo test          # unit + integration tests
cargo clippy --all-targets
cargo build --release
```

The `bin/tmux-agent-dock` launcher prefers an existing release binary, otherwise
downloads a prebuilt one, otherwise builds from source. Editing `src/` and
reopening the sidebar picks up your changes — though a **running dock keeps the
binary it started with**, so toggle it off and on after a rebuild.

## Credit

Forked from [Ymirke/tmux-agent-switcher](https://github.com/Ymirke/tmux-agent-switcher)
by Ymir Egilson, which is where the passive-detection approach, the popup
switcher, and the Rust/ratatui foundation come from. Everything above is built on
that work.

This fork has since diverged enough to warrant its own name — the docked sidebar,
the two-section layout, embedded-session folding, the unread semantics, and the
heartbeat hook are additions here, across ~58 commits and about 6,600 lines.

## License

[MIT](LICENSE), as upstream. The original copyright notice is retained.
