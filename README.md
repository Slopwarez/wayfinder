# Wayfinder

Wayfinder is a Vim-inspired terminal file manager built with Rust, Ratatui, and Crossterm. It features async directory scanning, search overlays, command palette, preview panes, and filesystem actions.

## Features

- Async directory listing with smooth navigation
- Vim keybindings (hjkl, gg/G, counts)
- Search (`/`), command (`:`) overlays with inline feedback
- Copy, move, rename, delete, mkdir, touch commands
- Shell/edit integration using `$SHELL` and `$EDITOR`
- Preview pane for text files/directories with MIME fallback
- Command aliases via TOML config at `~/.config/wayfinder/config.toml`

## Usage

```bash
cargo run
```

Key highlights:
- `h/j/k/l` navigate
- `gg/G` jump, `n/N` cycle search matches
- `:` open command palette (e.g., `:copy /tmp/`)
- `/` search filenames
- `:sh` launch a shell in current dir, `:edit` open with `$EDITOR`

## Configuration

Create `~/.config/wayfinder/config.toml` (XDG config dir) to declare command aliases:

```toml
[command_aliases]
rm = "delete"
cp = "copy"
mv = "move"
```

## Development

```bash
cargo fmt
cargo check
cargo test
```

See `AGENTS.md` for the multi-agent roadmap.
