# Wayfinder Agents

Use this guide when coordinating focused agents to build the Wayfinder terminal file manager. Each agent should report back with status notes and highlight blockers early. All agents share the same Rust workspace (Rust 1.78+) and may add crates with Cargo as needed, but must keep the binary lean.

## Mission

Create a terminal UI file manager with vim-like navigation. The app must let users browse the filesystem, inspect file details, and perform basic actions (open, rename, delete) entirely from the keyboard. Favor responsive layouts and minimal redraw flicker.

## Architecture Outline

1. **Core model**
   - Path state struct that tracks current directory, selected entry, sort mode, and pending actions.
   - Abstractions over filesystem operations so UI code can run unit tests using temp dirs.
2. **TUI layer**
   - Build with `ratatui` (preferred) or `crossterm` primitives.
   - Maintain an event loop that multiplexes input, tick events, and async filesystem refresh work.
3. **Command palette**
   - Modal input similar to Vim’s command line for rename/filter operations.
4. **Action queue**
   - Debounce expensive filesystem scans; refresh directory listings asynchronously with `tokio` or `async-std`.

## Proposed Crates

| Purpose        | Crate        | Notes                                   |
|----------------|--------------|-----------------------------------------|
| Terminal UI    | `ratatui`    | High-level widgets for panes/lists.     |
| Input handling | `crossterm`  | Reliable cross-platform key events.     |
| Async runtime  | `tokio`      | For background file scans.              |
| Filesystem     | `walkdir`    | Efficient recursive discovery.          |
| Config         | `directories`| OS-specific config/cache paths.         |

Add more utilities (e.g., `anyhow`, `thiserror`, `serde`) as the codebase grows.

## Agent Roles

### 1. Product Strategist
- Define MVP scope: dual-pane browser, preview pane, modal key map.
- Document exact key bindings (hjkl, gg/G, `:` commands).
- Capture UX flows for copy/move, rename, delete, search.

### 2. Architect
- Finalize crate selection and module layout (`app`, `fs`, `ui`, `input`).
- Specify data structures (AppState, DirEntry, CommandMode, Clipboard buffer).
- Ensure state transitions stay immutable-friendly for testing.

### 3. TUI Engineer
- Implement the render loop using ratatui layout constraints.
- Create reusable widgets: breadcrumb bar, file list with icons, info pane, command line overlay.
- Guarantee redraw budget <16ms for smooth feel; batch updates when possible.

### 4. Input & Command Agent
- Map key events to actions via configurable keymap (YAML/TOML later).
- Handle Vim-like sequences (e.g., `d d`, `y y`, counts before motions).
- Manage command palette parsing and validation.

### 5. Filesystem & Ops Agent
- Provide async directory scanning, caching, and file ops (copy/move/delete).
- Implement safe-guard prompts and trash integration where available.
- Deliver unit tests covering edge cases (symlinks, permission errors).

### 6. QA & Tooling Agent
- Set up `cargo fmt`, `cargo clippy`, and `cargo test` workflows.
- Create snapshot tests for widgets using ratatui’s buffer testing utilities.
- Draft a smoke-test checklist for manual validation in various terminals.

## Development Phases

1. **Scaffolding**
   - Introduce crates, AppState skeleton, and event loop stub.
   - Hard-code directory listing for the working dir to unblock UI iteration.
2. **Navigation**
   - Implement fs module with async listing, selection movement, and directory enter/exit.
   - Wire up hjkl, gg/G, search (`/`), and colon commands with minimal validation.
3. **Actions**
   - Add rename, delete, and copy/move flows with confirmation modals.
4. **Polish**
   - Support user config, color themes, and better error overlays.

Agents should update this document if scope or responsibilities change. Keep commits focused: one feature or fix per commit, each with appropriate tests.
