# target-gc

Evict idle Cargo build directories when the disk needs the space, and never one
that a build, a running program or a service is using. Ships a Claude Code
skill and plugin, so an agent frees space the same safe way instead of running
`rm -rf target/`.

A machine that builds Rust for several projects and agent worktrees fills its
disk with `target/` directories nobody is using. Measured on one laptop: 38 of
them, 152 GB by `du`, 9.3 GiB free. Clearing them by hand is where the damage
happens: a dev server writing into `target/`, a launchd service running its
binary out of `target/release`.

## Install

```sh
cargo install target-gc          # or: cargo binstall target-gc
target-gc scan                   # every Cargo build directory, and why each is held
target-gc run --dry-run          # what a run would remove today
target-gc install                # run hourly (launchd on macOS, systemd user timer on Linux)
```

For Claude Code, add the plugin. It brings the skill and a session-start line
that speaks only when the disk is under the floor, the last run failed, or runs
stopped:

```
/plugin marketplace add decebal/target-gc
/plugin install target-gc@target-gc
```

Without the plugin, `target-gc skill install` writes the same skill to
`~/.claude/skills/target-gc/`, and the session hook is one entry in
`~/.claude/settings.json`:

```json
{ "hooks": { "SessionStart": [
  { "hooks": [{ "type": "command", "command": "target-gc hook", "timeout": 10 }] }
] } }
```

## What it does

| | |
|---|---|
| **Finds** | Every directory holding the `CACHEDIR.TAG` Cargo writes, under the configured roots. A directory merely *named* `target` is somebody's source, and is never touched |
| **Holds** | A directory whose Cargo profile lock is held (`<target>/[<triple>/]<profile>/.cargo-lock`, `.cargo-build-lock`, `.cargo-artifact-lock`), whose project is the cwd of a known build writer (`cargo`, `cargo-watch`, `cargo-leptos`, `bacon`, `dx`, `trunk`, `watchexec`, `rust-analyzer`, plus `writers` from the config), whose files include a running executable, or whose path a launchd plist, systemd unit or listed config file names |
| **Evicts** | Directories idle past `max_idle_days` (30); and when free space is under `keep_free` (10%), the least recently built until it reaches `target_free` (20%). Nothing built in the last `min_idle_days` (2) |
| **Removes safely** | Re-checks, takes every profile lock, renames the directory aside, releases the locks, deletes. A Cargo that starts meanwhile waits, then builds cold, never into a half-deleted directory. An interrupted delete is finished by the next run |
| **Measures** | Free space with `df` before and after every eviction. Copy-on-write clones share blocks, so allocated size overstates what a delete frees |
| **Reports** | `~/.local/state/target-gc/last-run.json`: every eviction, refusal and held directory |

Settings live in `~/.config/target-gc/config.toml`; [`config.example.toml`](config.example.toml)
is the defaults, and `target-gc install` writes it if missing.

## Why not an existing tool

Checked against source on 2026-09-28:

| Tool | Finding |
|---|---|
| [cargo-sweep](https://github.com/holmgr/cargo-sweep) | Prunes inside one build directory by age or toolchain. No liveness check; its README says it lacks a maintainer |
| [cargo-clean-all](https://github.com/dnlmlr/cargo-clean-all) | Whole-directory deletion by last compile. Activity is the directory's mtime; no lock or process check |
| [kondo](https://github.com/tbillington/kondo), cargo-wipe, cargo-cleaner | Cross-ecosystem artifact cleaners. No liveness check, no disk budget |
| [cargo-broom](https://github.com/casoon/broom), [rldyour-cleaner](https://github.com/NDDev-OpenNetwork/rldyour-cleaner) | Lock `<target>/.cargo-lock`, which Cargo never creates (the lock is per profile), so the "build running" check can never fire |
| [worktree-gc](https://github.com/wycats/worktree-gc) | The most complete, and the source of much of this design: per-profile locks, process ownership, hysteresis, rename before delete. But it matches build directories by name (on a real machine it planned to delete a JavaScript package's own `target/` inside `node_modules`), its pressure mode is not in the published crate, and it only sees git repositories |
| Cargo | Automatic GC covers `~/.cargo` only. Build-directory GC is accepted upstream (rust-lang/cargo#13136) with no schedule |

## Limits

- **A dry run from your terminal can see less than the scheduled job.** macOS
  privacy protection decides per process whether `~/Documents`, `~/Desktop` and
  `~/Downloads` are readable. Read `last-run.json` after the first scheduled
  run, and add those folders to `skip` if they should never be touched.
- **A deletion inside a build directory restarts its idle clock.** Activity is
  read from directory mtimes; `cargo sweep` on a directory makes it look freshly
  built.
- **Only a known build writer's cwd holds a build directory.** A shell, an agent
  session or a service sitting in the project root does not: on 2026-10-07 two
  such sessions and one service held 225 GiB for four to six days without
  building. A dev server missing from the built-in list is still covered by
  `min_idle_days` while it builds, and belongs in `writers` if it recreates
  `target/` straight after a delete.
- **macOS and Linux only.**

## License

MIT
