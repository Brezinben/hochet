//! hochet: a machine-wide queue for heavy commands (builds, browser and mutation suites).
//!
//! Slots and queue tickets are flock(2) locks: the kernel frees them when their holder dies,
//! even after `kill -9` or a reboot, so a dead command never holds a slot or a place.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Seek, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, process, thread};

const USAGE: &str = "\
hochet: a machine-wide queue for heavy commands (builds, browser and mutation suites).
  hochet CMD...          run CMD once one of HOCHET_SLOTS slots is free (default 2)
  hochet -- CMD...       run CMD, even when its name starts with '-'
  hochet --force CMD...  run CMD now, without a slot (a person at a terminal only)
  hochet status          list the held slots and the queue
  hochet --version       print the version";

// Not 2: in a Claude Code Stop hook, exit 2 blocks every reply.
const EXIT_USAGE: u8 = 64;
const POLL: Duration = Duration::from_millis(500);

fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    let slots = match env::var("HOCHET_SLOTS").as_deref() {
        Err(_) => 2,
        Ok(text) => match text.parse::<usize>() {
            Ok(n) if n > 0 => n,
            _ => {
                eprintln!("hochet: HOCHET_SLOTS must be a positive integer, not '{text}'");
                return ExitCode::from(EXIT_USAGE);
            }
        },
    };
    let limits = match Limits::from_env() {
        Ok(limits) => limits,
        Err(message) => {
            eprintln!("hochet: {message}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    if let Some(text) = env::var_os("HOCHET_JOBS") {
        let text = text.to_string_lossy();
        // cargo reads its jobs as an i32.
        let jobs = match text.parse::<i32>() {
            Ok(n) if n > 0 => n,
            _ => {
                eprintln!("hochet: HOCHET_JOBS must be a positive integer, not '{text}'");
                return ExitCode::from(EXIT_USAGE);
            }
        };
        // An explicit CARGO_BUILD_JOBS wins; in a nested call it holds the outer value.
        if env::var_os("CARGO_BUILD_JOBS").is_none_or(|v| v.is_empty()) {
            // SAFETY: single-threaded at this point; the variable reaches CMD only.
            unsafe { env::set_var("CARGO_BUILD_JOBS", jobs.to_string()) };
        }
    }
    let queue = Queue::new(state_dir(), slots);

    let cmd = match args.first().and_then(|a| a.to_str()) {
        None | Some("-h" | "--help") => return usage(),
        Some("--version") => {
            println!("hochet {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Some("status") => return queue.status(),
        Some("--force") => {
            let rest = &args[1..];
            let rest = rest
                .strip_prefix(&[OsString::from("--")][..])
                .unwrap_or(rest);
            return if rest.is_empty() { usage() } else { run(rest) };
        }
        Some("--") if args.len() == 1 => return usage(),
        Some("--") => &args[1..],
        Some(option) if option.starts_with('-') => {
            eprintln!("hochet: unknown option '{option}'\n{USAGE}");
            return ExitCode::from(EXIT_USAGE);
        }
        _ => &args[..],
    };
    // A heavy script run through hochet calls hochet again: the outer slot covers it.
    if env::var_os("HOCHET_HELD").is_some() {
        return run(cmd);
    }
    queue.wait_then_run(cmd, &limits)
}

fn usage() -> ExitCode {
    println!("{USAGE}");
    ExitCode::from(EXIT_USAGE)
}

fn state_dir() -> PathBuf {
    if let Some(dir) = env::var_os("HOCHET_DIR") {
        return dir.into();
    }
    let cache = env::var_os("XDG_CACHE_HOME").map_or_else(
        || PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".cache"),
        PathBuf::from,
    );
    cache.join("hochet")
}

fn run(args: &[OsString]) -> ExitCode {
    let err = Command::new(&args[0]).args(&args[1..]).exec();
    eprintln!("hochet: cannot run {}: {err}", args[0].to_string_lossy());
    ExitCode::from(127)
}

/// True while another open file description holds the lock: the holder is alive.
fn held(path: &Path) -> bool {
    File::open(path).is_ok_and(|f| matches!(f.try_lock(), Err(TryLockError::WouldBlock)))
}

struct Queue {
    dir: PathBuf,
    slots: usize,
}

impl Queue {
    fn new(dir: PathBuf, slots: usize) -> Self {
        Self { dir, slots }
    }

    fn slot(&self, i: usize) -> PathBuf {
        self.dir.join(format!("slot-{i}.lock"))
    }

    fn queue_dir(&self) -> PathBuf {
        self.dir.join("queue")
    }

    /// Live tickets in arrival order; a ticket whose holder died is deleted on the way.
    fn tickets(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = fs::read_dir(self.queue_dir())
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .collect();
        paths.sort();
        paths.retain(|p| {
            let alive = held(p);
            if !alive {
                let _ = fs::remove_file(p);
            }
            alive && !is_hidden(p)
        });
        paths
    }

    /// The start time and the description of the command that holds slot `i`.
    fn holder(&self, i: usize) -> Option<(u64, String)> {
        let path = self.slot(i);
        if !held(&path) {
            return None;
        }
        let text = fs::read_to_string(&path).unwrap_or_default();
        let (since, who) = text.trim_end().split_once('\t').unwrap_or(("", ""));
        Some((
            since.parse().unwrap_or_else(|_| unix_secs()),
            who.to_owned(),
        ))
    }

    /// A slot locked but not yet written has no holder to name; the next poll will.
    fn holders(&self) -> Vec<(u64, String)> {
        (0..self.slots)
            .filter_map(|i| self.holder(i))
            .filter(|(_, who)| !who.is_empty())
            .collect()
    }

    fn status(&self) -> ExitCode {
        let now = unix_secs();
        for i in 0..self.slots {
            match self.holder(i) {
                None => println!("slot {i}: free"),
                Some((since, who)) => println!("slot {i}: {}s  {who}", now.saturating_sub(since)),
            }
        }
        for (n, path) in self.tickets().iter().enumerate() {
            let who = fs::read_to_string(path).unwrap_or_default();
            println!("queue {}: {}", n + 1, who.trim_end());
        }
        ExitCode::SUCCESS
    }

    fn wait_then_run(&self, args: &[OsString], limits: &Limits) -> ExitCode {
        let who = describe(args);
        let ticket = match self.enter(&who) {
            Ok(t) => t,
            Err(err) => {
                eprintln!(
                    "hochet: cannot join the queue in {}: {err}",
                    self.dir.display()
                );
                return ExitCode::FAILURE;
            }
        };
        let mut announced = (0, "", Vec::new());
        loop {
            let tickets = self.tickets();
            let place = tickets
                .iter()
                .position(|p| *p == ticket.path)
                .map_or(0, |i| i + 1);
            // A command alone always starts, so a threshold can never block the queue for ever.
            let short = if place == 1 && self.any_slot_held() {
                limits.shortage()
            } else {
                None
            };
            // Only the head of the queue tries, so a late command cannot pass an earlier one.
            if place == 1
                && short.is_none()
                && let Some(slot) = self.take_slot(&who)
            {
                let _ = fs::remove_file(&ticket.path);
                drop(ticket);
                keep_across_exec(&slot);
                // SAFETY: single-threaded at this point; the variable reaches CMD only.
                unsafe { env::set_var("HOCHET_HELD", "1") };
                return run(args);
            }
            let reason = short.as_ref().map_or("slots", |s| s.resource);
            let holders = self.holders();
            // The ages are left out of the comparison: they change every second.
            if place != announced.0
                || reason != announced.1
                || (!holders.is_empty() && holders != announced.2)
            {
                let lead = match &short {
                    Some(s) => format!(
                        "{} short, {:.1} GiB free, below {}={}, waiting at place {place}",
                        s.resource, s.free_gib, s.variable, s.min_gib
                    ),
                    None => format!("waiting at place {place}"),
                };
                eprintln!("{}", waiting_message(&lead, &holders));
                announced = (place, reason, holders);
            }
            thread::sleep(POLL);
        }
    }

    /// The ticket is locked before it enters the queue, so no waiter sees it unlocked and
    /// deletes it.
    fn enter(&self, who: &str) -> std::io::Result<Ticket> {
        let queue = self.queue_dir();
        fs::create_dir_all(&queue)?;
        let pid = process::id();
        let draft = queue.join(format!(".new-{pid}"));
        let mut file = File::create(&draft)?;
        file.lock()?;
        writeln!(file, "{who}")?;
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = queue.join(format!("{nanos:020}-{pid}"));
        fs::rename(&draft, &path)?;
        Ok(Ticket { path, _lock: file })
    }

    fn any_slot_held(&self) -> bool {
        (0..self.slots).any(|i| held(&self.slot(i)))
    }

    fn take_slot(&self, who: &str) -> Option<File> {
        (0..self.slots).find_map(|i| {
            let mut file = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(self.slot(i))
                .ok()?;
            file.try_lock().ok()?;
            let _ = file
                .set_len(0)
                .and_then(|()| file.rewind())
                .and_then(|()| writeln!(file, "{}\t{who}", unix_secs()));
            Some(file)
        })
    }
}

const GIB: f64 = (1u64 << 30) as f64;

/// Minimum free amounts in GiB; 0 turns a check off.
struct Limits {
    memory: f64,
    disk: f64,
}

struct Shortage {
    resource: &'static str,
    variable: &'static str,
    free_gib: f64,
    min_gib: f64,
}

impl Limits {
    fn from_env() -> Result<Self, String> {
        Ok(Self {
            memory: gib_var("HOCHET_MIN_FREE_GB")?,
            disk: gib_var("HOCHET_MIN_DISK_GB")?,
        })
    }

    fn shortage(&self) -> Option<Shortage> {
        let checks = [
            (
                "memory",
                "HOCHET_MIN_FREE_GB",
                self.memory,
                free_memory as fn() -> Option<f64>,
            ),
            ("disk", "HOCHET_MIN_DISK_GB", self.disk, free_disk),
        ];
        checks
            .into_iter()
            .find_map(|(resource, variable, min_gib, measure)| {
                if min_gib == 0.0 {
                    return None;
                }
                // An unmeasurable resource (memory on macOS) never holds a command back.
                let free_gib = measure()?;
                (free_gib < min_gib).then_some(Shortage {
                    resource,
                    variable,
                    free_gib,
                    min_gib,
                })
            })
    }
}

fn gib_var(name: &str) -> Result<f64, String> {
    match env::var(name).as_deref() {
        Err(_) | Ok("") => Ok(0.0),
        Ok(text) => match text.parse::<f64>() {
            Ok(n) if n.is_finite() && n >= 0.0 => Ok(n),
            _ => Err(format!(
                "{name} must be a number of GiB, 0 or more, not '{text}'"
            )),
        },
    }
}

/// `MemAvailable` exists on Linux only. `HOCHET_TEST_MEMINFO` replaces `/proc/meminfo` in tests.
fn free_memory() -> Option<f64> {
    let path = env::var_os("HOCHET_TEST_MEMINFO").unwrap_or_else(|| "/proc/meminfo".into());
    let text = fs::read_to_string(path).ok()?;
    let line = text.lines().find_map(|l| l.strip_prefix("MemAvailable:"))?;
    let kib: f64 = line.trim().strip_suffix("kB")?.trim().parse().ok()?;
    Some(kib * 1024.0 / GIB)
}

/// Free space for CMD's working directory. `HOCHET_TEST_DISK_FILE` names a file that holds the
/// free GiB, so tests do not depend on the real disk.
fn free_disk() -> Option<f64> {
    if let Some(path) = env::var_os("HOCHET_TEST_DISK_FILE") {
        return fs::read_to_string(path).ok()?.trim().parse().ok();
    }
    let stat = rustix::fs::statvfs(env::current_dir().ok()?).ok()?;
    Some((stat.f_bavail * stat.f_frsize) as f64 / GIB)
}

struct Ticket {
    path: PathBuf,
    _lock: File,
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Rust opens files close-on-exec; the slot lock must live in CMD and its children.
fn keep_across_exec(file: &File) {
    let _ = rustix::io::fcntl_setfd(file, rustix::io::FdFlags::empty());
}

fn describe(args: &[OsString]) -> String {
    let cmd: Vec<_> = args.iter().map(|a| a.to_string_lossy()).collect();
    let cwd = env::current_dir().unwrap_or_default();
    let session = env::var("CLAUDE_CODE_SESSION_ID")
        .or_else(|_| env::var("CODEX_THREAD_ID"))
        .unwrap_or_else(|_| "terminal".into());
    format!(
        "{}\t{}\t{session}\tpid {}",
        cmd.join(" "),
        cwd.display(),
        process::id()
    )
}

fn waiting_message(lead: &str, holders: &[(u64, String)]) -> String {
    let now = unix_secs();
    let running: Vec<String> = holders
        .iter()
        .map(|(since, who)| {
            let mut fields = who.split('\t');
            let cmd = fields.next().unwrap_or_default();
            let dir = Path::new(fields.next().unwrap_or_default())
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            format!("{cmd} in {dir} ({})", age(now.saturating_sub(*since)))
        })
        .collect();
    if running.is_empty() {
        return format!("hochet: {lead}");
    }
    format!("hochet: {lead}; running: {}", running.join(", "))
}

fn age(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m{:02}s", secs / 60, secs % 60),
        _ => format!("{}h{:02}m", secs / 3600, secs % 3600 / 60),
    }
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with('.'))
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
