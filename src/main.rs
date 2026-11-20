//! target-gc — evict idle Cargo build directories when the disk needs the
//! space. Argument parsing, the run loop, and printing; every decision lives in
//! the library so it can be tested.

mod setup;

use std::fs::{self, File};
use std::io::{BufRead, IsTerminal, Write};
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
use target_gc::propose::{self, Proposal};
use target_gc::state::{self, Record};
use target_gc::{human_age, human_bytes, node, protect, scratch, trim, Hold, Target, DAY};

/// A process snapshot older than this is retaken before the next eviction.
const EVIDENCE_TTL: Duration = Duration::from_secs(20);

/// Approved evictions, one JSON object per line.
const APPROVED_LOG: &str = "approved.jsonl";

struct Args {
    command: String,
    sub: Option<String>,
    /// Every positional after the command.
    paths: Vec<PathBuf>,
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
        "scan" | "run" | "propose" | "evict" | "review" => {
            let settings = match Settings::load(&config_path, &home) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("target-gc: {e}");
                    exit(2);
                }
            };
            let code = match args.command.as_str() {
                "scan" => scan(&settings, &home, &args),
                "run" => run(&settings, &home, &args),
                "propose" => propose_cmd(&settings, &home, &args),
                "evict" => evict_paths(&settings, &home, &args.paths),
                _ => review(&settings, &home),
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
        paths: Vec::new(),
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
                    args.sub.get_or_insert_with(|| other.to_string());
                    args.paths.push(PathBuf::from(other));
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
    sources: Vec<protect::Source>,
    /// Project `node_modules` and what holds each, when anything does.
    node: Vec<(Target, Option<Hold>)>,
    node_trash: Vec<PathBuf>,
}

/// The walk gets three fifths of the deadline, so the process snapshot, the
/// evictions and the proposals always get the rest.
fn walk_deadline(started: Instant, settings: &Settings) -> Instant {
    started + settings.deadline * 3 / 5
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
                .or_else(|| liveness::hold(&target, &evidence, &settings.writers));
            let occupied = liveness::occupied(&target, &evidence);
            Candidate {
                target,
                hold,
                occupied,
            }
        })
        .collect();
    let node_deps = if settings.node_idle.is_some() {
        found
            .node_modules
            .into_iter()
            .map(|path| {
                let deps = node::at(path);
                let hold = node::hold(&deps, home, &settings.protect, &sources, &evidence);
                (deps, hold)
            })
            .collect()
    } else {
        Vec::new()
    };
    Ok(Survey {
        space,
        candidates,
        trash: found.trash,
        unreadable: found.unreadable,
        incomplete: found.incomplete,
        evidence,
        sources,
        node: node_deps,
        node_trash: found.node_trash,
    })
}

/// Build directories a trim may touch: built recently, or held only because a
/// service names a file in them. A directory waiting to expire is left alone,
/// because a delete inside `deps/` restarts its idle clock.
fn trimmable(plan: &plan::Plan) -> Vec<&Target> {
    plan.recent
        .iter()
        .chain(
            plan.held
                .iter()
                .filter(|(_, h)| matches!(h, Hold::Referenced(_)))
                .map(|(t, _)| t),
        )
        .collect()
}

fn trim_plans<'a>(
    targets: &[&'a Target],
    cutoff: SystemTime,
    evidence: &[Evidence],
    sources: &[protect::Source],
) -> Vec<(&'a Target, trim::TrimPlan)> {
    let keep = |p: &Path| {
        let spelled = p.to_string_lossy();
        evidence.iter().any(|e| e.path == p)
            || sources.iter().any(|s| s.text.contains(spelled.as_ref()))
    };
    targets
        .iter()
        .filter(|t| liveness::held_lock(&t.path).is_none())
        .map(|t| (*t, trim::plan(&t.path, cutoff, &keep)))
        .filter(|(_, p)| !p.artifacts.is_empty())
        .collect()
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
        pressure_min_idle: settings.pressure_min_idle,
        max_idle: settings.max_idle,
    })
}

fn scan(settings: &Settings, home: &Path, args: &Args) -> i32 {
    let s = match survey(settings, home, walk_deadline(Instant::now(), settings)) {
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

    let s = match survey(settings, home, walk_deadline(started, settings)) {
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
    let trims = settings.trim_after.map_or_else(Vec::new, |after| {
        trim_plans(&trimmable(&plan), now - after, &s.evidence, &s.sources)
    });
    let node_held = s.node.iter().filter(|(_, h)| h.is_some()).count();
    let node_due = settings
        .node_idle
        .map_or_else(Vec::new, |idle| node::expired(s.node, idle, now));

    if args.dry_run {
        print_plan(&plan, &policy, s.space, now);
        print_trims(&trims);
        if let Some(idle) = settings.node_idle {
            println!(
                "\nwould remove node_modules of projects idle past {} ({node_held} held):",
                human_age(idle)
            );
            if node_due.is_empty() {
                println!("  (none)");
            }
            for d in &node_due {
                println!("  {} (idle {})", d.path.display(), human_age(d.idle(now)));
            }
        }
        if let Some(min) = settings.bloat_min {
            let (items, _) = proposals_from(&plan, &s.evidence, min, deadline, now);
            println!(
                "\nwould propose for approval (unused, {} or more):",
                human_bytes(min)
            );
            if items.is_empty() {
                println!("  (none)");
            }
            for p in &items {
                println!("  {}", propose::describe(p));
            }
        }
        return 0;
    }

    let mut log: Vec<Value> = Vec::new();
    for trash in &s.trash {
        let result = evict::remove_trash(trash);
        log.push(json!({ "trash": trash, "error": result.err() }));
    }
    for trash in &s.node_trash {
        let result = node::remove_trash(trash);
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
        let result = if target.idle(now) < policy.min_idle && liveness::occupied(target, &evidence)
        {
            Err(Refusal::Skipped(
                "a process moved into the project since the scan".into(),
            ))
        } else {
            evict::evict(target, &evidence, &settings.writers)
        };
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

    let mut trimmed = 0usize;
    for (target, trim_plan) in &trims {
        if Instant::now() >= deadline {
            break;
        }
        if !target.path.is_dir() {
            continue;
        }
        let before = available;
        let result = trim::trim(&target.path, trim_plan);
        let after = disk::space(home).map_or(before, |sp| sp.available);
        available = after;
        match result {
            Ok((removed, errors)) => {
                trimmed += removed;
                let freed = after.saturating_sub(before);
                if !args.quiet {
                    println!(
                        "trimmed  {} — {removed} superseded artifacts, {} allocated, free space moved {}",
                        target.path.display(),
                        human_bytes(trim_plan.allocated),
                        human_bytes(freed)
                    );
                }
                for e in &errors {
                    println!("FAILED   {e}");
                }
                log.push(json!({
                    "trimmed": target.path,
                    "artifacts": removed,
                    "allocated": trim_plan.allocated,
                    "freed": freed,
                    "errors": errors,
                }));
            }
            Err(lock) => {
                if !args.quiet {
                    println!(
                        "skipped  {} — build running (lock held: {})",
                        target.path.display(),
                        lock.display()
                    );
                }
                log.push(json!({ "trim_refused": target.path, "lock": lock }));
            }
        }
    }

    let mut node_removed = 0usize;
    for deps in &node_due {
        if Instant::now() >= deadline {
            break;
        }
        if evidence_at.elapsed() > EVIDENCE_TTL {
            let Some(fresh) = liveness::snapshot() else {
                break;
            };
            evidence = fresh;
            evidence_at = Instant::now();
        }
        let before = available;
        let result = match node::hold(deps, home, &settings.protect, &s.sources, &evidence) {
            Some(h) => Err(Refusal::Skipped(h.describe())),
            None => node::remove(deps),
        };
        let after = disk::space(home).map_or(before, |sp| sp.available);
        available = after;
        let idle = human_age(deps.idle(now));
        match result {
            Ok(()) => {
                node_removed += 1;
                let freed = after.saturating_sub(before);
                println!(
                    "removed  {} (project idle {idle}) — free space moved {}",
                    deps.path.display(),
                    human_bytes(freed)
                );
                log.push(
                    json!({ "node_modules_removed": deps.path, "idle": idle, "freed": freed }),
                );
            }
            Err(refusal) => {
                if !args.quiet || matches!(refusal, Refusal::Failed(_)) {
                    println!("skipped  {} — {}", deps.path.display(), refusal.reason());
                }
                log.push(json!({ "refused": deps.path, "reason": refusal.reason() }));
            }
        }
    }

    let swept = sweep_scratch(&plan.recent);
    let (proposals, sizing_cut) = settings.bloat_min.map_or((Vec::new(), false), |min| {
        proposals_from(&plan, &evidence, min, deadline, now)
    });
    save_proposals(&state_dir, &proposals);
    let proposed: u64 = proposals.iter().map(|p| p.allocated).sum();
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
        "artifacts_trimmed": trimmed,
        "node_modules_removed": node_removed,
        "node_modules_held": node_held,
        "proposed": proposals.len(),
        "proposed_allocated": proposed,
        "proposal_sizing_incomplete": sizing_cut,
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
            "target-gc: {} → {} free; {evicted} evicted, {trimmed} artifacts trimmed, \
             {node_removed} node_modules removed, {} held{}",
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
    if !args.json && !proposals.is_empty() && (!args.quiet || still_low) {
        println!(
            "target-gc: {} unused build directories ({}) wait for approval: `target-gc review`, \
             or `target-gc propose` to list them",
            proposals.len(),
            human_bytes(proposed)
        );
    }
    record.exit_code
}

/// Build directories the run left in place, held by nothing and with no
/// process in their project, sized until `deadline`. The bool is true when
/// sizing stopped at the deadline.
fn proposals_from(
    plan: &plan::Plan,
    evidence: &[Evidence],
    bloat_min: u64,
    deadline: Instant,
    now: SystemTime,
) -> (Vec<Proposal>, bool) {
    let mut sized = Vec::new();
    let mut cut = false;
    for t in plan.young.iter().chain(&plan.queue).chain(&plan.expired) {
        if !t.path.is_dir() || liveness::occupied(t, evidence) {
            continue;
        }
        if Instant::now() >= deadline {
            cut = true;
            break;
        }
        sized.push((t.clone(), discover::allocated_bytes(&t.path)));
    }
    (propose::select(sized, bloat_min, now), cut)
}

fn save_proposals(state_dir: &Path, items: &[Proposal]) {
    let value = propose::to_json(state::now_secs(), items);
    if let Err(e) = state::write_file(state_dir, propose::FILE, &value) {
        eprintln!("target-gc: could not write the proposals: {e}");
    }
}

fn load_proposals(state_dir: &Path) -> Vec<Proposal> {
    state::read_file(state_dir, propose::FILE)
        .as_ref()
        .and_then(propose::from_json)
        .map(|(_, items)| items)
        .unwrap_or_default()
}

fn propose_cmd(settings: &Settings, home: &Path, args: &Args) -> i32 {
    let Some(bloat_min) = settings.bloat_min else {
        println!("target-gc: proposals are off (bloat_min_size = \"0\")");
        return 0;
    };
    let started = Instant::now();
    let s = match survey(settings, home, walk_deadline(started, settings)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("target-gc: {e}");
            return 2;
        }
    };
    let policy = match policy(settings, s.space) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("target-gc: {e}");
            return 2;
        }
    };
    let now = SystemTime::now();
    let plan = plan::plan(s.candidates, &policy, s.space.available, now);
    let (items, cut) = proposals_from(
        &plan,
        &s.evidence,
        bloat_min,
        started + settings.deadline,
        now,
    );
    save_proposals(&config::state_dir(home), &items);
    if args.json {
        println!("{}", propose::to_json(state::now_secs(), &items));
        return 0;
    }
    println!(
        "{} free. Unused build directories of {} or more (no lock held, no process in the \
         project):",
        human_bytes(s.space.available),
        human_bytes(bloat_min)
    );
    if items.is_empty() {
        println!("  (none)");
    }
    for p in &items {
        println!("  {}", propose::describe(p));
    }
    if cut {
        println!("Sizing hit the deadline; directories after the cut were not measured.");
    }
    if !items.is_empty() {
        println!(
            "Approve with `target-gc evict <path>…` or `target-gc review`. Sizes are allocated \
             bytes; copy-on-write clones free less."
        );
    }
    0
}

/// Evict build directories a person approved. Every condition except age is
/// checked again here, from fresh evidence.
fn evict_paths(settings: &Settings, home: &Path, paths: &[PathBuf]) -> i32 {
    if paths.is_empty() {
        eprintln!("target-gc: name the build directories to evict: target-gc evict <path>…");
        return 2;
    }
    let state_dir = config::state_dir(home);
    let sources = protect::load_sources(&protect::service_dirs(home), &settings.scan_files);
    let mut evidence = Vec::new();
    let mut evidence_at: Option<Instant> = None;
    let mut gone = Vec::new();
    let mut code = 0;
    for raw in paths {
        if evidence_at.is_none_or(|at| at.elapsed() > EVIDENCE_TTL) {
            let Some(fresh) = liveness::snapshot() else {
                eprintln!(
                    "target-gc: could not list running processes, so nothing can be proven unused"
                );
                return 2;
            };
            evidence = fresh;
            evidence_at = Some(Instant::now());
        }
        let path = fs::canonicalize(raw).unwrap_or_else(|_| raw.clone());
        let before = disk::space(home).map_or(0, |sp| sp.available);
        let result = propose::check(
            &path,
            home,
            &settings.protect,
            &sources,
            &evidence,
            &settings.writers,
        )
        .and_then(|t| {
            evict::evict(&t, &evidence, &settings.writers).map_err(|r| r.reason().to_string())
        });
        let after = disk::space(home).map_or(before, |sp| sp.available);
        match result {
            Ok(()) => {
                let freed = after.saturating_sub(before);
                println!(
                    "evicted  {} — free space moved {}",
                    path.display(),
                    human_bytes(freed)
                );
                append_approved(
                    &state_dir,
                    &json!({
                        "evicted": path,
                        "freed": freed,
                        "at": state::now_secs(),
                    }),
                );
                gone.push(path);
            }
            Err(why) => {
                println!("refused  {} — {why}", path.display());
                code = 1;
            }
        }
    }
    if !gone.is_empty() {
        let left: Vec<Proposal> = load_proposals(&state_dir)
            .into_iter()
            .filter(|p| !gone.contains(&p.path))
            .collect();
        save_proposals(&state_dir, &left);
    }
    code
}

fn append_approved(state_dir: &Path, entry: &Value) {
    let line = format!("{entry}\n");
    let written = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state_dir.join(APPROVED_LOG))
        .and_then(|mut f| f.write_all(line.as_bytes()));
    if let Err(e) = written {
        eprintln!("target-gc: could not log the eviction: {e}");
    }
}

/// Ask, one directory at a time, about the proposals the last run or
/// `propose` saved.
fn review(settings: &Settings, home: &Path) -> i32 {
    if !std::io::stdin().is_terminal() {
        eprintln!(
            "target-gc: review asks on a terminal. From an agent: list with `target-gc propose`, \
             ask the user, then run `target-gc evict <path>…` on what they approve."
        );
        return 2;
    }
    let items: Vec<Proposal> = load_proposals(&config::state_dir(home))
        .into_iter()
        .filter(|p| p.path.is_dir())
        .collect();
    if items.is_empty() {
        println!("Nothing waits for approval. `target-gc propose` looks again.");
        return 0;
    }
    let mut approved = Vec::new();
    let mut lines = std::io::stdin().lock().lines();
    for p in &items {
        print!("{}\n  evict? [y/N/q] ", propose::describe(p));
        let _ = std::io::stdout().flush();
        let Some(Ok(answer)) = lines.next() else {
            break;
        };
        match answer.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => approved.push(p.path.clone()),
            "q" | "quit" => break,
            _ => {}
        }
    }
    if approved.is_empty() {
        return 0;
    }
    evict_paths(settings, home, &approved)
}

/// Orphaned rustc scratch older than a day, in recently built directories
/// that are not building right now. Idle directories are left alone: a delete
/// inside `deps/` moves its mtime, which would restart their idle clock and
/// postpone the expiry that reclaims far more.
fn sweep_scratch(recent: &[Target]) -> usize {
    let cutoff = SystemTime::now() - DAY;
    recent
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
    println!(
        "\nbuilt too recently to evict: {} (floor {}{})",
        plan.young.len(),
        human_age(policy.min_idle),
        if plan.pressure {
            format!(
                ", {} where no process sits in the project",
                human_age(policy.pressure_min_idle)
            )
        } else {
            String::new()
        }
    );
    println!("Sizes are allocated bytes: an upper bound. Copy-on-write clones free less.");
}

fn print_trims(trims: &[(&Target, trim::TrimPlan)]) {
    println!("\nwould trim superseded deps/ artifacts:");
    if trims.is_empty() {
        println!("  (none)");
    }
    let mut total = 0u64;
    for (t, p) in trims {
        total += p.allocated;
        println!(
            "  {:>10}  {} ({} artifacts)",
            human_bytes(p.allocated),
            t.path.display(),
            p.artifacts.len()
        );
    }
    if !trims.is_empty() {
        println!("  {:>10}  total", human_bytes(total));
    }
}
