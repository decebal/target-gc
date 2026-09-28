---
name: target-gc
description: Free disk space held by Rust/Cargo build directories (target/) without breaking a build, a dev server or a service. Use when the disk is low or full, when a command fails with "No space left on device", when asked to clean up target directories, clear Rust build caches or free space, and before deleting any target/ directory by hand.
---

# Freeing disk space from Cargo build directories

Use the `target-gc` binary. Do not `rm -rf` a `target/` directory or run
`cargo clean` in someone else's project to free space: a directory that looks
idle may be written by a dev server (`dx serve`, `trunk`, `bacon`), hold the
binary a launchd or systemd service runs, or be mid-build in another session.
`target-gc` checks all of that before it deletes anything; a hand deletion
checks none of it.

If `target-gc` is not installed: `cargo install target-gc` (or
`cargo binstall target-gc`), then continue below.

## Steps

1. **See what exists and why each directory is held.**

   ```sh
   target-gc scan --sizes
   ```

   Every line is a Cargo build directory (identified by the `CACHEDIR.TAG`
   Cargo writes, never by name) with either its idle time or the reason it is
   held: `build running (lock held: …)`, `in use by pid N (cmd)`,
   `binary referenced by <file>`, or `under protected path …`.
   Sizes are allocated bytes: an upper bound, because copy-on-write clones
   share blocks.

2. **Preview.**

   ```sh
   target-gc run --dry-run
   ```

   Lists what would go: directories idle past `max_idle_days`, then, only if
   free space is under `keep_free`, the least recently built until free space
   reaches `target_free`.

3. **Run it.**

   ```sh
   target-gc run
   ```

   Each eviction prints the allocated size and how far free space actually
   moved. Exit code 1 means the disk is still under the floor because
   everything left is in use, protected or built too recently.

4. **Report what happened** from `~/.local/state/target-gc/last-run.json`,
   which lists every eviction, every refusal and every held directory.

## When the disk is still full after a run

Everything remaining is held for a reason `target-gc scan` names. Tell the user
which directories and which processes hold them. Do not kill a process or a
build to free space; that is the user's call, and a killed cold build costs
more than the space it frees.

## Settings

`~/.config/target-gc/config.toml` (numbers are quoted strings):

| Key | Default | Meaning |
|---|---|---|
| `roots` | `["~"]` | Where to look |
| `skip` | `~/Library`, `~/Pictures`, `~/Music`, `~/Movies` | Never descended into |
| `keep_free` / `target_free` | `"10%"` / `"20%"` | Pressure starts below the first, continues to the second |
| `min_idle_days` | `"2"` | Nothing built more recently is ever touched (minimum 1) |
| `max_idle_days` | `"30"` | Evicted past this whatever the free space; `"0"` = only under pressure |
| `protect` | `[]` | Paths never touched |
| `scan_files` | shell rc files, `~/.claude.json`, … | Files that may name a binary inside a build directory |

To keep a directory permanently, add its path (or a parent) to `protect`.

## The schedule

`target-gc install` runs `target-gc run --quiet` hourly (launchd `dev.target-gc`
on macOS, a systemd user timer on Linux). `target-gc hook` prints one line at
session start only when the disk is under the floor, the last run failed, or no
run has happened for three hours. On macOS check the job with
`launchctl print gui/$(id -u)/dev.target-gc`.

## Limits to state when reporting

- A scan from this session may not see what the scheduled job sees: macOS
  privacy protection can hide `~/Documents`, `~/Desktop` and `~/Downloads` from
  an agent's shell but not from the job. `last-run.json` is the record of what
  was actually removed.
- Deleting files inside a build directory (`cargo sweep`) restarts its idle
  clock.
- macOS and Linux only.
