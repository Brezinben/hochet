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
    let queue = Queue::new(state_dir(), slots);

    let cmd = match args.first().and_then(|a| a.to_str()) {
        None | Some("-h" | "--help") => return usage(),
        Some("--version") => {
            println!("hochet {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Some("status") => return queue.status(),
        Some("--force") if args.len() > 1 => return run(&args[1..]),
        Some("--" | "--force") if args.len() == 1 => return usage(),
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
    queue.wait_then_run(cmd)
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

    fn status(&self) -> ExitCode {
        let now = unix_secs();
        for i in 0..self.slots {
            let path = self.slot(i);
            if !held(&path) {
                println!("slot {i}: free");
                continue;
            }
            let text = fs::read_to_string(&path).unwrap_or_default();
            let (since, who) = text.trim_end().split_once('\t').unwrap_or(("", ""));
            let age = since.parse::<u64>().map_or(0, |s| now.saturating_sub(s));
            println!("slot {i}: {age}s  {who}");
        }
        for (n, path) in self.tickets().iter().enumerate() {
            let who = fs::read_to_string(path).unwrap_or_default();
            println!("queue {}: {}", n + 1, who.trim_end());
        }
        ExitCode::SUCCESS
    }

    fn wait_then_run(&self, args: &[OsString]) -> ExitCode {
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
        let mut announced = 0;
        loop {
            let tickets = self.tickets();
            let place = tickets
                .iter()
                .position(|p| *p == ticket.path)
                .map_or(0, |i| i + 1);
            // Only the head of the queue tries, so a late command cannot pass an earlier one.
            if place == 1
                && let Some(slot) = self.take_slot(&who)
            {
                let _ = fs::remove_file(&ticket.path);
                drop(ticket);
                keep_across_exec(&slot);
                // SAFETY: single-threaded at this point; the variable reaches CMD only.
                unsafe { env::set_var("HOCHET_HELD", "1") };
                return run(args);
            }
            if place != announced {
                eprintln!(
                    "hochet: {} slots busy, waiting at place {place} (hochet status)",
                    self.slots
                );
                announced = place;
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
