# Task Shark panel

- From the repository root, use `cargo check -p task_shark_panel` to check this crate and `cargo build -p zed` to build the app.
- Open the panel with `task shark: toggle focus` in the command palette.
- The panel uses the installed `tasks` CLI. Check it with `tasks api lists` and `tasks api snapshot --list <name>`.
- For UI checks, launch `target/debug/zed --user-data-dir <temporary-home>/zed-data <temporary-home>/tasks-lists` with `HOME` set to a persistent temporary directory outside a tool sandbox. Put a link to the real tasks executable at `<temporary-home>/.local/bin/tasks`. This isolates task writes from the user's lists.
- Check creation with Enter, cancellation with Escape, section and subtask expansion, and Today creation with an explicit owner list. Read the isolated CLI snapshots to confirm saved task IDs and ownership.
- On macOS, multiple Zed processes share a name. Target the new process ID through Accessibility; name-based activation can focus the installed app instead.
- Ticks belong to the chat module, not this panel. See [`../agent_ui/AGENTS.md`](../agent_ui/AGENTS.md) for isolated tick checks.
- On multi-display Macs, other windows can sit above the test window. Check the topmost window at a point before posting synthetic clicks.
