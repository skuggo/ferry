# ferry

A small Rust CLI for keeping a local project tree in sync with an FTP server —
`push`, `pull`, `sync`, `status`, `rm`, and `init` (plus `cc` for remote
compile-checks against a MUD-style `check_compile` service). It's designed to
be driven from an editor (e.g. [Zed](https://zed.dev)'s `.zed/tasks.json`) or
from coding-agent PreToolUse hooks (Claude Code, Codex), so files are pulled
and pushed as you and your agents work.

> **Renamed from `zed-ftp`.** The binary is now `ferry`, the config file is
> `.ferry.toml`, and state lives in `.ferry/`. Existing projects are migrated
> automatically: the first time `ferry` runs in a project that still has
> `.zed-ftp.toml` / `.zed-ftp/`, it renames them in place. Update any hook or
> task wiring that referenced the old `zed-ftp` / `zed-ftp-lsp` binaries.

## Status

Functional and unit-tested. The FTP integration tests are gated behind a live
Docker daemon because they spin up a real `vsftpd` container. Verify the
round-trip against your own server before relying on Ferry for important data.

## Installation

From a checkout of this repo:

```sh
cargo install --path .
```

This installs both `ferry` and `ferry-lsp` in `~/.cargo/bin`. Make sure
that directory is on `PATH`.

### Install in Zed

Zed compiles development extensions for `wasm32-wasip2`. Install Rust through
[`rustup`](https://zed.dev/docs/extensions/developing-extensions) so Zed can add
that target automatically; with another Rust installation, make the target
available yourself.

Open Zed's Extensions page and click `Install Dev Extension`, or run the
`zed: install dev extension` action. Select this repository's
`extensions/ferry` directory.

After configuring a Ferry project, if its actions do not appear for a file,
fully quit and relaunch Zed, then reopen the project.

See [Native Zed integration](#native-zed-integration) for project configuration
and the available actions.

## Quick start

In your project root:

```sh
ferry init
```

The wizard prompts for host, username, password, and remote root, then
validates by listing the remote root and walking it against your local tree.
Use `--no-validate` to skip the remote walk if you just want the config file
written:

```sh
ferry init --no-validate
```

This writes a `.ferry.toml` to the project root and appends it to
`.gitignore`.

## Dry runs

Add `--dry-run` anywhere in a Ferry command to preview write-capable commands
without changing local files, the remote server, or Ferry's config and state:

```sh
ferry push src/example.c --dry-run
ferry sync --dry-run
ferry rm old/file.c --dry-run
```

Ferry still connects, reads and hashes files, validates paths and credentials,
and detects conflicts. A dry run can therefore fail with the same validation,
authentication, path, or conflict exit code as the real command. Adding
`--force` changes the planned overwrite but remains non-mutating.

Previewed actions use future-tense output such as `would push`, `would pull`,
`would upload`, `would download`, and `would delete`. This protection covers
`push`, `pull`, `sync`, `rm`, `init`, and `hook`, including config,
`.gitignore`, sync-state, and legacy-name migration writes. `status --dry-run`
also suppresses its normally hidden state-cache update. The observational
`ls` and `cc` / `check` commands otherwise behave normally.

## Scoped sync

`ferry sync` with no path retains Ferry's established project-wide behavior.
To limit reconciliation, pass exactly one file or directory, or use the
interactive picker:

```sh
ferry sync src/example.c
ferry sync areas
ferry sync --select
ferry sync areas --dry-run
ferry sync areas --force
```

`sync PATH` accepts one path relative to the configured `local_root`, or an
absolute path contained by that root. A directory path is recursive. `PATH`
and `--select` are mutually exclusive. The picker merges the current local and
remote directory, including remote-only roots, and closes after one file or
folder is selected. Selecting a folder runs the same recursive scoped sync as
a direct directory path.

Normal scoped sync is conflict-safe. `--force` is an explicit CLI-only option:
for both-changed and untracked files, local wins and is uploaded. No supplied
Zed Code Action or task uses force. `--dry-run` is non-mutating and previews
the scoped plan without changing local files, remote files or directories, or
Ferry state.

Remote-only directories are created locally, local-only directories are
created remotely, and empty selected directories and descendants are created
on the missing side. Sync never treats a one-sided entry as a deletion and
never removes files or directories. Clean entries in a selected directory may
finish before another entry reports a conflict; that partial clean progress
and its state are retained, and the command exits with conflict code `2` after
reporting every conflict.

## Zed Task Picker integration

Copy [`examples/tasks.json`](examples/tasks.json) into your project's
`.zed/tasks.json` (or merge the entries with your existing tasks). Then in
Zed, open the command palette and run `task: spawn`. The example provides:

- current-file Pull, Push, and Compile-check tasks;
- current-file and recursive current-folder Sync tasks;
- a `Ferry: choose path to sync...` terminal picker;
- a clearly labelled project-wide Status task;
- and a clearly labelled project-wide Sync task.

Zed's extension API does not expose literal Project Panel right-click actions
to Ferry. Use `Ctrl+.` for the current document, or these Task Picker entries
when you want terminal output or need to choose a path.

Current-file and current-folder tasks pass Zed's absolute `$ZED_FILE` or
`$ZED_DIRNAME` and run from `$ZED_DIRNAME`, so Ferry can find the nearest
project configuration. Status and project-wide Sync run from
`$ZED_WORKTREE_ROOT`. The chooser starts in `$ZED_DIRNAME`, browses local and
remote entries (including remote-only roots), and synchronizes one selection
before closing.

Run **Save All** before any terminal-backed directory or picker task. Tasks
cannot inspect dirty Zed buffers, so unsaved editor content cannot participate
in their safety checks. Status is the safe way to inspect the whole project.
Project-wide Sync is conflict-aware and does not use force, but it may transfer
files throughout the configured project; review Status first. There are
deliberately no whole-tree Pull, Push, force, or deletion tasks.

For a frequently used project folder, add a stable named task using one
relative path, for example `ferry sync areas`:

```json
{
  "label": "Ferry: sync areas",
  "command": "ferry",
  "args": ["sync", "areas"],
  "cwd": "$ZED_WORKTREE_ROOT",
  "use_new_terminal": false,
  "reveal": "always",
  "hide": "on_success"
}
```

## Claude Code / Codex hook

For LLM agents (Claude Code, Codex, etc.) that read and edit files on your
behalf, register `ferry hook` as a `PreToolUse` hook so every Read/Edit
tool call auto-pulls the file from FTP first. There's a configurable
cooldown (default 3600s) so a hot LLM turn doesn't hammer the server.

Example Claude Code `~/.claude/settings.json` (or project-local
`.claude/settings.local.json`):

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Edit|MultiEdit|Write",
        "hooks": [{"type": "command", "command": "ferry hook"}]
      },
      {
        "matcher": "Read",
        "hooks": [{"type": "command", "command": "ferry hook --cooldown 3600"}]
      }
    ]
  }
}
```

See [`examples/claude-code-settings.json`](examples/claude-code-settings.json)
for a copy-pasteable version. The hook exits 0 whether it pulled, skipped
by cooldown, or errored — the LLM's tool call is never blocked.

Behaviour:
- Reads the tool envelope on stdin; extracts `tool_input.file_path`.
- Walks upward to find `.ferry.toml`; if not in a ferry project, no-op.
- Compares `state.files[rel].last_synced` against the cooldown window; if
  fresh enough, skips the pull.
- Otherwise runs a fast single-file pull (bypasses the tree walk).
- On failure, logs to stderr (which the LLM host surfaces to you) but
  never denies the tool call.

## Native Zed integration

The companion extension at [`extensions/ferry/`](extensions/ferry/README.md)
starts `ferry-lsp` for Zed's C and C++ languages. The language server handles file-open
and file-save events directly and offers manual file actions. Follow
[Installation](#installation) to install both binaries and the development
extension in Zed.

Configure the editor behavior in the project's `.ferry.toml`:

```toml
[editor]
pull_on_open = false
push_on_save = false
```

Both settings default to `false`. Each project can opt into Pull on open,
Push on save, or both independently. For nested projects, the nearest
`.ferry.toml` above the file wins. The configuration is read again on every
open, save, and manual action, so changes apply on the next event without
restarting Zed.

When enabled, automatic Pull and Push are always non-force and conflict-safe.
A conflict or other failure produces a warning; automatic success is silent.
Zed initially opens the on-disk file, so it can briefly
show stale content before its external-file watcher observes a successful
Pull. No open or save event performs a whole-tree or directory sync; directory
sync is always an explicit manual action.

Automatic settings do not hide manual actions. For an explicit operation,
open the lightbulb menu or press `Ctrl-.`; for a file in a Ferry project the
actions appear in this order:

1. `Ferry: Pull`
2. `Ferry: Compare with Remote`
3. `Ferry: Force Pull (overwrite local)`
4. `Ferry: Push`
5. `Ferry: Sync Current File`
6. `Ferry: Sync Current Folder`
7. `Ferry: Compile-check`

Pull, Compare, Force Pull, and both Sync actions are save-first operations:
they use saved local files. If the current buffer has unsaved changes, Ferry
refuses the file-scoped action and asks you to save and retry. Sync Current
Folder recursively reconciles the current file's parent directory; it refuses
when any buffer known to Ferry beneath that folder is dirty and asks you to
Save All before retrying. Neither Sync action uses force.

Compare fetches the remote file into a private snapshot, then opens Zed's
native diff with the saved local file on the left (old) and the fetched remote
file on the right (new). It does not change the local file, Ferry state, or
sync settings. For native diff, the `zed` CLI must be on the `PATH` visible to
`ferry-lsp`.

Force Pull retrieves the remote file first, then displays Zed's native warning
confirmation. Only the exact `Overwrite local file` action applies it. Cancel,
dismissal, an edit, shutdown, or a change to the local file's identity leaves
the current file and Ferry state intact. A confirmed overwrite updates the
local file and state through a guarded atomic install. This confirmation
applies only to the Zed action; the existing `ferry pull --force` CLI remains
noninteractive and unchanged.

Manual actions report results with Info or Warning notifications and remain
scoped to the current file's nearest Ferry project. The Task Picker tasks
described above are terminal-backed alternatives when you want command
output, project Status/Sync, a recursive current-folder sync, or a single
interactive path selection.

The extension attaches to Zed's C and C++ languages by default. `.h` files are
covered either way — Zed classifies them as C by default, and setups that route
a header to C++ (LPC projects commonly do) are covered by the C++ entry; see the
extension README if your file is attached to some other language.

## Remote compile checks (`cc`)

For servers that run the companion **UDP compile service** — an authenticated
`check_compile` endpoint (as used by the 3Kingdoms/3Scapes LDMud MUD) — `ferry
cc` dry-compiles files on the server without loading them:

```sh
ferry cc <file>...
# e.g.
ferry cc cmds/secure/cc.c players/foo/room.c
```

For each file it prints `<path>: OK` or `<path>: FAIL` followed by any
compiler diagnostics (`<file>:<line>: error|warning: <message>`), and exits
non-zero if any file failed — so it works as a pre-push compile gate. `check`
is an alias: `ferry check <file>...`.

Under the hood each file is one authenticated request (your `.ferry.toml`
login/password) over a small tab-delimited UDP protocol; the server streams
back chunked diagnostics that the client reassembles. Nothing is loaded or
executed on the server — it's a pure compile check, and you can only check
files you're allowed to write.

### Configuration

`cc` connects to the server's UDP port, set under `[connection]` in
`.ferry.toml` (defaults to `3203`):

```toml
[connection]
host     = "your.mud.host"
port     = 3201          # FTP/TCP port
udp_port = 3203          # compile-service UDP port
user     = "yourwiz"
password = "..."
```

This feature requires the server-side compile service to be installed; against
a plain FTP server `cc` simply times out.

## Security

**`.ferry.toml` stores your FTP password in plaintext.** `init` automatically
appends the config filename to `.gitignore`, but the file is still readable by
anything on your machine that can read your working tree. FTPS and SFTP are
explicitly out of scope for v1 — if you need encrypted transport, use a
different tool.

## Exit codes

| Code | Meaning |
|------|---------|
| 0    | Success |
| 1    | Generic error (I/O, unexpected failure) |
| 2    | Conflict — local and remote diverge; re-run with `--force` to override |
| 3    | Configuration or authentication problem |

These are stable so you can branch on them from shell scripts or task runners.

## Troubleshooting

- **"MDTM not supported"** — some FTP servers don't implement `MDTM`, which
  ferry uses to skip re-hashing unchanged remote files. It falls back to
  always downloading and re-hashing. The decision is cached in `state.json`
  after the first run so the probe only happens once.
- **"Connection refused"** — check the port (default `21`) and your server's
  passive-mode setting. ferry uses passive mode.
- **"Conflict: ..."** — run `ferry status` to see which files diverge, then
  either `pull`/`push` the side you want to keep, or re-run with `--force` to
  blow away the other side.
- **"remote presence ... is indeterminate"** — single-file transfers try
  `SIZE`, then an exact `NLST` lookup. A server that supports neither `SIZE`
  nor an authoritative exact-`NLST` absence (for example, it errors when the
  requested name is missing) cannot safely prove absence. Ferry therefore
  refuses to create a new remote or local counterpart instead of guessing.

## Development

```sh
cargo test                  # unit + non-Docker integration tests
cargo test -- --ignored     # Docker-gated FTP integration tests
```

The `--ignored` suite requires a working Docker daemon and pulls
`delfer/alpine-ftp-server` to spin up a real FTP server per test.

## License

[PolyForm Noncommercial 1.0.0](LICENSE.md) — free to use, modify, and share
for any noncommercial purpose. Commercial use requires a separate license
from the copyright holder; get in touch if you want one.
