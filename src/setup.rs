//! The commands around a run: the session hook, installing and removing the
//! schedule, the Claude Code skill, and help.

use std::fs;
use std::path::{Path, PathBuf};

use target_gc::config::{self, Settings};
use target_gc::{discover, disk, install, skill, state};

const CONFIG_TEMPLATE: &str = r#"# target-gc settings. Numbers are quoted strings.
[target-gc]
roots         = ["~"]
keep_free     = "10%"
target_free   = "20%"
min_idle_days = "2"
max_idle_days = "30"
protect       = []
"#;

pub fn hook(config_path: &Path, home: &Path) {
    let settings = match Settings::load(config_path, home) {
        Ok(s) => s,
        Err(e) => {
            println!("target-gc: config error, scheduled cleanup will fail: {e}");
            return;
        }
    };
    let space = disk::space(home);
    let keep_free = space.map_or(0, |sp| settings.keep_free.resolve(sp.total));
    let record = state::read(&config::state_dir(home));
    if let Some(line) = state::hook_line(
        record.as_ref(),
        space.map(|sp| sp.available),
        keep_free,
        state::now_secs(),
    ) {
        println!("{line}");
    }
}

pub fn install_cmd(config_path: &Path, home: &Path, dry_run: bool) -> i32 {
    let Some(exe) = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
    else {
        eprintln!("target-gc: cannot resolve my own path");
        return 2;
    };
    if exe.ancestors().any(discover::is_cargo_build_dir) {
        eprintln!(
            "target-gc: {} is inside a Cargo build directory, which target-gc itself may \
             evict. Install it first: cargo install target-gc",
            exe.display()
        );
        return 2;
    }
    let state_dir = config::state_dir(home);
    let units = install::units(&exe, home, &state_dir);
    let Some(uid) = install::uid() else {
        eprintln!("target-gc: could not read the user id");
        return 2;
    };
    let commands = install::load_commands(&units, &uid);
    if dry_run {
        for u in &units {
            println!("--- {}\n{}", u.path.display(), u.body);
        }
        for c in &commands {
            println!("$ {}", c.join(" "));
        }
        return 0;
    }
    if !config_path.exists() {
        let written = config_path
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|()| fs::write(config_path, CONFIG_TEMPLATE));
        if let Err(e) = written {
            eprintln!("target-gc: {}: {e}", config_path.display());
            return 2;
        }
        println!("wrote {}", config_path.display());
    }
    let result = fs::create_dir_all(&state_dir)
        .map_err(|e| e.to_string())
        .and_then(|()| install::write_units(&units))
        .and_then(|()| install::run_commands(&commands, true));
    match result {
        Ok(()) => {
            for u in &units {
                println!("installed {}", u.path.display());
            }
            println!(
                "Scheduled hourly, and the first run starts now. It can read folders this \
                 terminal may not; {} shows what it removed.\n\
                 For Claude Code: `/plugin marketplace add decebal/target-gc` then \
                 `/plugin install target-gc@target-gc` adds the skill and the session hook.\n\
                 Without the plugin: `target-gc skill install`, and add to \
                 ~/.claude/settings.json under SessionStart:\n  \
                 {{ \"type\": \"command\", \"command\": \"{} hook\", \"timeout\": 10 }}",
                state_dir.join(state::RECORD).display(),
                exe.display()
            );
            0
        }
        Err(e) => {
            eprintln!("target-gc: {e}");
            2
        }
    }
}

/// `target-gc skill` prints the skill; `target-gc skill install` writes it to
/// `~/.claude/skills/target-gc/` (or `--dir`).
pub fn skill_cmd(sub: Option<&str>, dir: Option<&Path>, home: &Path) -> i32 {
    match sub {
        None | Some("print") => {
            print!("{}", skill::SKILL_MD);
            0
        }
        Some("install") => {
            let dir = dir.map_or_else(|| skill::default_dir(home), Path::to_path_buf);
            match skill::install(&dir) {
                Ok(path) => {
                    println!("wrote {}", path.display());
                    0
                }
                Err(e) => {
                    eprintln!("target-gc: {e}");
                    2
                }
            }
        }
        Some(other) => {
            eprintln!("target-gc: unknown skill command {other:?}; use `print` or `install`");
            2
        }
    }
}

pub fn uninstall_cmd(home: &Path) -> i32 {
    let Some(uid) = install::uid() else {
        eprintln!("target-gc: could not read the user id");
        return 2;
    };
    let _ = install::run_commands(&install::unload_commands(&uid), true);
    let exe = PathBuf::from("target-gc");
    for u in install::units(&exe, home, &config::state_dir(home)) {
        if fs::remove_file(&u.path).is_ok() {
            println!("removed {}", u.path.display());
        }
    }
    0
}

pub fn print_help() {
    println!(
        "target-gc — evict idle Cargo build directories when the disk needs the space.

USAGE:
    target-gc scan [--sizes] [--json]      list every Cargo build directory and why it is held
    target-gc run [--dry-run] [--quiet] [--json]
                                           evict what the policy allows, measure what came back
    target-gc hook                         one line for a session, only when something is wrong
    target-gc install [--dry-run]          schedule `run` hourly (launchd / systemd user timer)
    target-gc uninstall                    remove the schedule
    target-gc skill [install] [--dir <d>]  print the Claude Code skill, or write it to
                                           ~/.claude/skills/target-gc/

    --config <path>   settings file (default: $XDG_CONFIG_HOME/target-gc/config.toml)

A directory is a Cargo build directory only if Cargo's CACHEDIR.TAG is in it.
It is held — never evicted — while a Cargo lock in it is held, while a process
has its cwd in the project or runs a binary from it, or while a service
definition or listed config file names a path inside it.

Exit codes: 0 done; 1 still under the floor after the run; 2 could not run."
    );
}
