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
   held: `build running (lock held: …)`, `in use by pid N (cmd)` (a build writer such as `dx` or `bacon` working in the
   project, or a process running a binary from the build directory),
   `binary referenced by <file>`, or `under protected path …`.
   Sizes are allocated bytes: an upper bound, because copy-on-write clones
   share blocks.

2. **Preview.**

   ```sh
   target-gc run --dry-run
   ```

   Lists what would go: directories idle past `max_idle_days`, then, only if
   free space is under `keep_free`, the least recently built until free space
   reaches `target_free`; under pressure a directory whose project no process
   sits in qualifies once idle past `pressure_min_idle_hours`, not only past
   `min_idle_days`. Last, the trim: in directories built recently or
   kept because a service names a binary in them, the `deps/` artifacts
   superseded by two newer builds of the same crate and untouched for
   `trim_after_days`. Cargo never deletes an old hash, so a long-lived or
   cloned build directory carries every build it ever did. Then the
   `node_modules` of projects whose git checkout has not changed for
   `node_modules_idle_days`, with no process in the checkout and no service
   or MCP config naming the project.

3. **Run it.**

   ```sh
   target-gc run
   ```

   Each eviction and trim prints the allocated size and how far free space
   actually moved. Exit code 1 means the disk is still under the floor because
   everything left is in use, protected or built too recently.

4. **Ask about the proposals.** The run lists build directories it will not
   evict on its own but that no session is using: no lock held, no process
   with its working directory in the project, at least `bloat_min_size`
   allocated, whatever their age.

   ```sh
   target-gc propose
   ```

   Show the user the list (path, size, when it was last built) and ask which
   to remove. Never leave them alone silently and never evict them without an
   answer. Then run, with exactly the paths they approved:

   ```sh
   target-gc evict <path> <path>…
   ```

   `evict` checks everything again except age, refuses a directory a process
   has since moved into, and prints how far free space moved. In a terminal,
   `target-gc review` asks y/n for each instead.

5. **Report what happened** from `~/.local/state/target-gc/last-run.json`,
   which lists every eviction, every refusal and every held directory;
   approved evictions are in `approved.jsonl` beside it.

## When the disk is still full after a run

Ask about the proposals first (step 4). Everything else remaining is held for
a reason `target-gc scan` names. Tell the user which directories and which
processes hold them. Do not kill a process or a build to free space; that is
the user's call, and a killed cold build costs more than the space it frees.

## Settings

`~/.config/target-gc/config.toml` (numbers are quoted strings):

| Key | Default | Meaning |
|---|---|---|
| `roots` | home, `/tmp`, `$TMPDIR` | Where to look, each resolved to its real path; setting it replaces all three |
| `skip` | `~/Library`, `~/Pictures`, `~/Music`, `~/Movies`, `~/OrbStack` | Never descended into |
| `keep_free` / `target_free` | `"10%"` / `"20%"` | Pressure starts below the first, continues to the second |
| `min_idle_days` | `"2"` | Nothing built more recently is evicted while a process sits in the project (minimum 1) |
| `pressure_min_idle_hours` | `"1"` | Under pressure, the floor for a project no process sits in (minimum 1, at most `min_idle_days`) |
| `max_idle_days` | `"30"` | Evicted past this whatever the free space; `"0"` = only under pressure |
| `trim_after_days` | `"3"` | Superseded `deps/` artifacts older than this are trimmed from kept directories; `"0"` = never |
| `bloat_min_size` | `"5G"` | Unused directories this big are proposed for approval whatever their age; `"0"` = never |
| `node_modules_idle_days` | `"60"` | A project's `node_modules` is removed once its checkout is idle this long and nothing uses or names it; `"0"` = never |
| `protect` | `[]` | Paths never touched |
| `scan_files` | shell rc files, `~/.claude.json`, … | Files that may name a binary inside a build directory |

To keep a directory permanently, add its path (or a parent) to `protect`.

## The schedule

`target-gc install` runs `target-gc run --quiet` hourly (launchd `dev.target-gc`
on macOS, a systemd user timer on Linux). `target-gc hook` prints one line at
session start only when the disk is under the floor, the last run failed, no
run has happened for three hours, or proposals wait for approval. A proposal
line is a request to ask the user (step 4), not information to pass over. On macOS check the job with
`launchctl print gui/$(id -u)/dev.target-gc`.

## Limits to state when reporting

- A scan from this session may not see what the scheduled job sees: macOS
  privacy protection can hide `~/Documents`, `~/Desktop` and `~/Downloads` from
  an agent's shell but not from the job. `last-run.json` is the record of what
  was actually removed.
- Deleting files inside a build directory (`cargo sweep`, or the trim)
  restarts its idle clock, so the trim never touches a directory waiting to
  expire.
- A trimmed artifact a build still wanted costs a rebuild of that crate. The
  trim holds every Cargo lock while it deletes, so it never races a build.
- macOS and Linux only.
