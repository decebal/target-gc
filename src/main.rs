//! target-gc — evict idle Cargo build directories when the disk needs the
//! space. Argument parsing, the run loop, and printing; every decision lives in
//! the library so it can be tested.

mod setup;

use std::fs::{self, File};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::time::{Duration, Instant, SystemTime};

use serde_json::{json, Value};
use target_gc::config::{self, Settings};
use target_gc::discover::{self, Walk};
use target_gc::disk::{self, Space};
use target_gc::evict::{self, Refusal};
use target_gc::liveness::{self, Evidence};
use target_gc::plan::{self, Candidate, Policy};
use target_gc::state::{self, Record};
use target_gc::{human_age, human_bytes, protect, scratch, Hold, Target, DAY};

/// A process snapshot older than this is retaken before the next eviction.
const EVIDENCE_TTL: Duration = Duration::from_secs(20);

struct Args {
    command: String,
    sub: Option<String>,
    config: Option<PathBuf>,
    dir: Option<PathBuf>,
    dry_run: bool,
    quiet: bool,
    json: bool,
    sizes: bool,
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = parse_args(&argv);
    let Some(home) = config::home() else {
        eprintln!("target-gc: HOME is unset or not an absolute path; refusing to guess one");
        exit(2);
    };
    let config_path = args
        .config
        .clone()
        .unwrap_or_else(|| config::default_path(&home));

    match args.command.as_str() {
        "hook" => setup::hook(&config_path, &home),
        "install" => exit(setup::install_cmd(&config_path, &home, args.dry_run)),
        "uninstall" => exit(setup::uninstall_cmd(&home)),
        "skill" => exit(setup::skill_cmd(
            args.sub.as_deref(),
            args.dir.as_deref(),
            &home,
        )),
        "scan" | "run" => {
            let settings = match Settings::load(&config_path, &home) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("target-gc: {e}");
                    exit(2);
                }
            };
            let code = if args.command == "scan" {
                scan(&settings, &home, &args)
            } else {
                run(&settings, &home, &args)
            };
            exit(code);
        }
        _ => {
            setup::print_help();
            exit(if args.command == "help" { 0 } else { 2 });
        }
    }
}

fn parse_args(argv: &[String]) -> Args {
    let mut args = Args {
        command: "help".into(),
        sub: None,
        config: None,
        dir: None,
        dry_run: false,
        quiet: false,
        json: false,
        sizes: false,
    };
    let mut positional = 0;
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--config" => args.config = it.next().map(PathBuf::from),
            "--dir" => args.dir = it.next().map(PathBuf::from),
            "--dry-run" => args.dry_run = true,
            "--quiet" => args.quiet = true,
            "--json" => args.json = true,
            "--sizes" => args.sizes = true,
            "-h" | "--help" => {
                args.command = "help".into();
                return args;
            }
            other if !other.starts_with('-') => {
                if positional == 0 {
                    args.command = other.to_string();
                } else {
                    args.sub = Some(other.to_string());
                }
                positional += 1;
            }
            other => {
                eprintln!("target-gc: unknown flag {other}");
                exit(2);
            }
        }
    }
    args
}

/// Everything a scan knows about the build directories on this machine.
struct Survey {
    space: Space,
    candidates: Vec<Candidate>,
    trash: Vec<PathBuf>,
    unreadable: usize,
    incomplete: bool,
    evidence: Vec<Evidence>,
}

fn survey(settings: &Settings, home: &Path, deadline: Instant) -> Result<Survey, String> {
    let space = disk::space(home).ok_or("could not read free space with /bin/df")?;
    let found = discover::find(&Walk {
        roots: &settings.roots,
        skip_paths: &settings.skip,
        max_depth: settings.max_depth,
        deadline,
    });
    let evidence = liveness::snapshot()
        .ok_or("could not list running processes, so nothing can be proven idle")?;
    let sources = protect::load_sources(&protect::service_dirs(home), &settings.scan_files);
    let candidates = found
        .targets
        .into_iter()
        .map(|target| {
            let hold = protect::protection(&target, home, &settings.protect, &sources)
                .or_else(|| liveness::hold(&target, &evidence));
            Candidate { target, hold }
        })
        .collect();
    Ok(Survey {
        space,
        candidates,
        trash: found.trash,
        unreadable: found.unreadable,
        incomplete: found.incomplete,
        evidence,
    })
}

fn policy(settings: &Settings, space: Space) -> Result<Policy, String> {
    let keep_free = settings.keep_free.resolve(space.total);
    let target_free = settings.target_free.resolve(space.total);
    if target_free < keep_free {
        return Err("target_free must not be below keep_free".into());
    }
    Ok(Policy {
        keep_free,
        target_free,
        min_idle: settings.min_idle,
        max_idle: settings.max_idle,
    })
}

fn scan(settings: &Settings, home: &Path, args: &Args) -> i32 {
    let deadline = Instant::now() + settings.deadline;
    let s = match survey(settings, home, deadline) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("target-gc: {e}");
            return 2;
        }
    };
    let now = SystemTime::now();
    let rows: Vec<Value> = s
        .candidates
        .iter()
        .map(|c| {
            json!({
                "path": c.target.path,
                "idle_days": c.target.idle(now).as_secs() / 86_400,
                "held": c.hold.as_ref().map(Hold::describe),
                "allocated": args.sizes.then(|| discover::allocated_bytes(&c.target.path)),
            })
        })
        .collect();
    if args.json {
        println!(
            "{}",
            json!({ "available": s.space.available, "targets": rows })
        );
        return 0;
    }
    println!(
        "{} free of {}. {} Cargo build directories{}:",
        human_bytes(s.space.available),
        human_bytes(s.space.total),
        s.candidates.len(),
        if s.incomplete {
            " (scan hit its deadline)"
        } else {
            ""
        },
    );
    for (c, row) in s.candidates.iter().zip(&rows) {
        let size = row["allocated"]
            .as_u64()
            .map_or_else(String::new, |b| format!("{:>10}  ", human_bytes(b)));
        let status = c.hold.as_ref().map_or_else(
            || format!("idle {}", human_age(c.target.idle(now))),
            Hold::describe,
        );
        println!("  {size}{}  — {status}", c.target.path.display());
    }
    if s.unreadable > 0 {
        println!(
            "{} directories could not be read and were skipped.",
            s.unreadable
        );
    }
    0
}

fn run(settings: &Settings, home: &Path, args: &Args) -> i32 {
    let started = Instant::now();
    let deadline = started + settings.deadline;
    let state_dir = config::state_dir(home);

    let Some(_run_lock) = run_lock(&state_dir) else {
        if !args.quiet {
            println!("target-gc: another run is in progress");
        }
        return 0;
    };

    let s = match survey(settings, home, deadline) {
        Ok(s) => s,
        Err(e) => return fail(&state_dir, &e, args),
    };
    let policy = match policy(settings, s.space) {
        Ok(p) => p,
        Err(e) => return fail(&state_dir, &e, args),
    };
    let home_dev = fs::metadata(home).map(|m| m.dev()).ok();
    let now = SystemTime::now();
    let plan = plan::plan(s.candidates, &policy, s.space.available, now);

    if args.dry_run {
        print_plan(&plan, &policy, s.space, now);
        return 0;
    }

    let mut log: Vec<Value> = Vec::new();
    for trash in &s.trash {
        let result = evict::remove_trash(trash);
        log.push(json!({ "trash": trash, "error": result.err() }));
    }

    let mut available = s.space.available;
    let mut evidence = s.evidence;
    let mut evidence_at = Instant::now();
    let mut evicted = 0usize;
    let mut run_one = |target: &Target, available: &mut u64, log: &mut Vec<Value>| {
        if evidence_at.elapsed() > EVIDENCE_TTL {
            match liveness::snapshot() {
                Some(fresh) => {
                    evidence = fresh;
                    evidence_at = Instant::now();
                }
                None => return false,
            }
        }
        let allocated = discover::allocated_bytes(&target.path);
        let before = *available;
        let result = evict::evict(target, &evidence);
        let after = disk::space(home).map_or(before, |sp| sp.available);
        *available = after;
        let idle = human_age(target.idle(now));
        match result {
            Ok(()) => {
                let freed = after.saturating_sub(before);
                println!(
                    "evicted  {} (idle {idle}) — {} allocated, free space moved {}",
                    target.path.display(),
                    human_bytes(allocated),
                    human_bytes(freed)
                );
                log.push(json!({
                    "evicted": target.path,
                    "idle": idle,
                    "allocated": allocated,
                    "freed": freed,
                }));
                true
            }
            Err(refusal) => {
                let tag = if matches!(refusal, Refusal::Failed(_)) {
                    "FAILED "
                } else {
                    "skipped"
                };
                if !args.quiet || tag == "FAILED " {
                    println!("{tag}  {} — {}", target.path.display(), refusal.reason());
                }
                log.push(json!({ "refused": target.path, "reason": refusal.reason() }));
                false
            }
        }
    };

    for target in &plan.expired {
        if Instant::now() >= deadline {
            break;
        }
        if run_one(target, &mut available, &mut log) {
            evicted += 1;
        }
    }
    for target in &plan.queue {
        if Instant::now() >= deadline || !plan::keep_evicting(&policy, plan.pressure, available) {
            break;
        }
        let same_volume = fs::metadata(&target.path).map(|m| m.dev()).ok() == home_dev;
        if same_volume && run_one(target, &mut available, &mut log) {
            evicted += 1;
        }
    }

    let swept = sweep_scratch(&plan.young);
    let still_low = plan.pressure && available < policy.keep_free;
    let record = Record {
        finished_at: state::now_secs(),
        exit_code: i32::from(still_low),
        available_before: s.space.available,
        available_after: available,
        evicted,
        held: plan.held.len(),
        error: None,
    };
    let detail = json!({
        "log": log,
        "held": plan.held.iter().map(|(t, h)| json!({ "path": t.path, "why": h.describe() })).collect::<Vec<_>>(),
        "scratch_dirs_removed": swept,
        "unreadable_dirs": s.unreadable,
        "scan_incomplete": s.incomplete,
        "seconds": started.elapsed().as_secs(),
    });
    let value = record.to_json(detail);
    if let Err(e) = state::write(&state_dir, &value) {
        eprintln!("target-gc: could not write the run record: {e}");
    }
    if args.json {
        println!("{value}");
    } else if !args.quiet || still_low {
        println!(
            "target-gc: {} → {} free; {evicted} evicted, {} held{}",
            human_bytes(s.space.available),
            human_bytes(available),
            plan.held.len(),
            if still_low {
                ". Still under the floor: everything left is in use, protected or recent"
            } else {
                ""
            }
        );
    }
    record.exit_code
}

/// Orphaned rustc scratch older than a day, in recently built directories
/// that are not building right now. Idle directories are left alone: a delete
/// inside `deps/` moves its mtime, which would restart their idle clock and
/// postpone the expiry that reclaims far more.
fn sweep_scratch(young: &[Target]) -> usize {
    let cutoff = SystemTime::now() - DAY;
    young
        .iter()
        .filter(|t| t.path.is_dir() && liveness::held_lock(&t.path).is_none())
        .map(|t| scratch::sweep(&scratch::inspect(&t.path, cutoff)).0)
        .sum()
}

fn run_lock(state_dir: &Path) -> Option<File> {
    fs::create_dir_all(state_dir).ok()?;
    let file = File::create(state_dir.join("run.lock")).ok()?;
    file.try_lock().ok()?;
    Some(file)
}

fn fail(state_dir: &Path, error: &str, args: &Args) -> i32 {
    eprintln!("target-gc: {error}");
    let record = Record {
        finished_at: state::now_secs(),
        exit_code: 2,
        available_before: 0,
        available_after: 0,
        evicted: 0,
        held: 0,
        error: Some(error.to_string()),
    };
    let value = record.to_json(Value::Null);
    let _ = state::write(state_dir, &value);
    if args.json {
        println!("{value}");
    }
    2
}

fn print_plan(plan: &plan::Plan, policy: &Policy, space: Space, now: SystemTime) {
    println!(
        "{} free of {} — floor {}, target {}. {}",
        human_bytes(space.available),
        human_bytes(space.total),
        human_bytes(policy.keep_free),
        human_bytes(policy.target_free),
        if plan.pressure {
            "UNDER PRESSURE"
        } else {
            "no pressure"
        },
    );
    let line = |t: &Target| {
        format!(
            "{:>10}  {} (idle {})",
            human_bytes(discover::allocated_bytes(&t.path)),
            t.path.display(),
            human_age(t.idle(now))
        )
    };
    println!("\nwould evict (idle past max_idle):");
    for t in &plan.expired {
        println!("  {}", line(t));
    }
    println!("\nwould evict under pressure, oldest first, until free space reaches the target:");
    if !plan.pressure {
        println!("  (none: free space is above the floor)");
    }
    for t in plan.queue.iter().filter(|_| plan.pressure) {
        println!("  {}", line(t));
    }
    println!("\nheld:");
    for (t, h) in &plan.held {
        println!("  {} — {}", t.path.display(), h.describe());
    }
    println!("\nbuilt too recently to touch: {}", plan.young.len());
    println!("Sizes are allocated bytes: an upper bound. Copy-on-write clones free less.");
}
