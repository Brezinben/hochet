//! Each test fails when its mechanism breaks: without the slot locks the waits stop, and
//! without the ticket locks a stale ticket blocks the queue.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

struct Machine {
    dir: PathBuf,
    slots: usize,
    children: Vec<Child>,
}

impl Machine {
    fn new(name: &str, slots: usize) -> Self {
        let dir = std::env::temp_dir().join(format!("hochet-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self {
            dir,
            slots,
            children: Vec::new(),
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_hochet"));
        cmd.args(args)
            .env("HOCHET_DIR", &self.dir)
            .env("HOCHET_SLOTS", self.slots.to_string())
            .env_remove("HOCHET_HELD");
        cmd
    }

    fn spawn(&mut self, args: &[&str]) -> u32 {
        let child = self
            .command(args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        self.children.push(child);
        pid
    }

    fn output(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn status(&self) -> String {
        String::from_utf8(self.output(&["status"]).stdout).unwrap()
    }

    fn file(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn kill(&mut self, pid: u32) {
        let child = self.children.iter_mut().find(|c| c.id() == pid).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn within(limit: Duration, done: impl Fn() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if done() {
            return true;
        }
        sleep(Duration::from_millis(50));
    }
    false
}

fn touch_cmd(path: &Path) -> Vec<String> {
    vec!["touch".into(), path.display().to_string()]
}

#[test]
fn queue_waits_in_order_and_frees_a_killed_slot() {
    let mut m = Machine::new("order", 2);
    let holder = m.spawn(&["sleep", "30"]);
    m.spawn(&["sleep", "30"]);
    assert!(
        within(Duration::from_secs(3), || m
            .status()
            .matches("sleep 30")
            .count()
            == 2),
        "status does not list the two held slots"
    );
    let cwd = std::env::current_dir().unwrap();
    assert!(
        m.status().contains(&cwd.display().to_string()),
        "status does not show the worktree"
    );

    // A ticket left by a killed waiter: unlocked, so it must not block anyone.
    fs::create_dir_all(m.file("queue")).unwrap();
    fs::write(m.file("queue/00000000000000000000-1"), "stale\n").unwrap();

    let third = touch_cmd(&m.file("third"));
    let fourth = touch_cmd(&m.file("fourth"));
    m.spawn(&third.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(
        within(Duration::from_secs(3), || m
            .status()
            .contains("queue 1: touch")),
        "the third command is not first in the queue"
    );
    m.spawn(&fourth.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(
        within(Duration::from_secs(3), || m
            .status()
            .contains("queue 2: touch")),
        "the fourth command is not second in the queue"
    );
    assert!(
        !m.file("third").exists(),
        "a third command ran while both slots were held"
    );

    m.kill(holder);
    assert!(
        within(Duration::from_secs(1), || m.file("third").exists()),
        "kill -9 of a holder did not free its slot within 1 s"
    );
    assert!(
        within(Duration::from_secs(2), || m.file("fourth").exists()),
        "the fourth command did not follow the third"
    );
}

#[test]
fn a_waiter_prints_its_place() {
    let mut m = Machine::new("place", 1);
    m.spawn(&["sleep", "30"]);
    assert!(within(Duration::from_secs(3), || m
        .status()
        .contains("sleep 30")));
    let mut waiter = m.command(&["true"]).stderr(Stdio::piped()).spawn().unwrap();
    sleep(Duration::from_millis(800));
    waiter.kill().unwrap();
    let out = waiter.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("waiting at place 1"),
        "the waiter did not print its place: {stderr}"
    );
}

#[test]
fn force_runs_at_once_and_needs_a_command() {
    let mut m = Machine::new("force", 1);
    m.spawn(&["sleep", "30"]);
    assert!(within(Duration::from_secs(3), || m
        .status()
        .contains("sleep 30")));
    let forced = m.file("forced");
    assert!(
        m.output(&["--force", "touch", forced.to_str().unwrap()])
            .status
            .success()
    );
    assert!(forced.exists(), "--force did not run at once");
    assert_eq!(
        m.output(&["--force"]).status.code(),
        Some(64),
        "--force without a command did not print the usage"
    );
}

#[test]
fn a_nested_call_uses_the_outer_slot() {
    let m = Machine::new("nested", 1);
    let nested = m.file("nested");
    let bin = env!("CARGO_BIN_EXE_hochet");
    let mut child = m
        .command(&[bin, "touch", nested.to_str().unwrap()])
        .spawn()
        .unwrap();
    assert!(
        within(Duration::from_secs(5), || nested.exists()),
        "a nested hochet waited for a second slot"
    );
    child.wait().unwrap();
}

#[test]
fn usage_and_bad_slots_exit_64() {
    let m = Machine::new("usage", 2);
    let help = m.output(&["--help"]);
    assert_eq!(help.status.code(), Some(64));
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(
        help.contains("hochet status"),
        "--help does not list status"
    );
    assert!(help.contains("--version"), "--help does not list --version");
    assert!(help.contains("hochet -- CMD"), "--help does not list --");
    let mut bad = m.command(&["true"]);
    bad.env("HOCHET_SLOTS", "0");
    assert_eq!(
        bad.output().unwrap().status.code(),
        Some(64),
        "HOCHET_SLOTS=0 was accepted"
    );
}

#[test]
fn a_missing_command_exits_127() {
    let m = Machine::new("missing", 2);
    assert_eq!(
        m.output(&["hochet-no-such-command"]).status.code(),
        Some(127)
    );
}

#[test]
fn an_unknown_option_exits_64_and_runs_nothing() {
    let m = Machine::new("unknown", 2);
    let marker = m.file("ran");
    let out = m.output(&["--foo", "touch", marker.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(64), "--foo was not rejected");
    assert!(
        !marker.exists(),
        "hochet ran a command after an unknown option"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("hochet status"),
        "an unknown option did not print the usage on stderr"
    );
}

#[test]
fn version_prints_the_crate_version() {
    let m = Machine::new("version", 2);
    let out = m.output(&["--version"]);
    assert!(out.status.success(), "--version did not exit 0");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        concat!("hochet ", env!("CARGO_PKG_VERSION"), "\n")
    );
}

#[test]
fn double_dash_queues_and_runs_the_command() {
    let m = Machine::new("dashdash", 1);
    let marker = m.file("ran");
    let out = m.output(&["--", "touch", marker.to_str().unwrap()]);
    assert!(out.status.success(), "hochet -- CMD failed: {out:?}");
    assert!(marker.exists(), "hochet -- CMD did not run CMD");
}

#[test]
fn double_dash_ends_the_options_everywhere() {
    let m = Machine::new("dashes", 2);
    assert_eq!(
        m.output(&["--"]).status.code(),
        Some(64),
        "-- without a command was accepted"
    );
    let dash_named = m.output(&["--", "--force", "x"]);
    assert_eq!(
        dash_named.status.code(),
        Some(127),
        "-- --force was not run as a command"
    );
    let forced = m.file("forced");
    assert!(
        m.output(&["--force", "--", "touch", forced.to_str().unwrap()])
            .status
            .success()
    );
    assert!(forced.exists(), "--force -- CMD did not run CMD");
}

#[test]
fn a_nested_call_still_checks_its_options() {
    let m = Machine::new("nested-options", 1);
    let marker = m.file("ran");
    let mut unknown = m.command(&["--foo", "touch", marker.to_str().unwrap()]);
    unknown.env("HOCHET_HELD", "1");
    assert_eq!(
        unknown.output().unwrap().status.code(),
        Some(64),
        "a nested --foo was run"
    );
    assert!(
        !marker.exists(),
        "a nested hochet ran a command after an unknown option"
    );
    let mut dashed = m.command(&["--", "touch", marker.to_str().unwrap()]);
    dashed.env("HOCHET_HELD", "1");
    assert!(dashed.output().unwrap().status.success());
    assert!(marker.exists(), "a nested -- CMD did not run at once");
}

#[test]
fn a_waiter_names_each_holder_once_per_change() {
    let m = Machine::new("holders", 1);
    let slot = m.file("slot-0.lock");
    let lock = fs::File::create(&slot).unwrap();
    lock.lock().unwrap();
    fs::write(&slot, "1\tcargo build\t/w/api.fix\tterminal\tpid 1\n").unwrap();
    let mut waiter = m.command(&["true"]).stderr(Stdio::piped()).spawn().unwrap();
    sleep(Duration::from_millis(1600));
    fs::write(&slot, "1\tplaywright test\t/w/web.main\tterminal\tpid 2\n").unwrap();
    sleep(Duration::from_millis(1600));
    waiter.kill().unwrap();
    let out = waiter.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    let lines: Vec<&str> = stderr.lines().collect();
    assert_eq!(lines.len(), 2, "one line per change expected: {stderr}");
    assert!(
        lines[0].starts_with("hochet: waiting at place 1; running: cargo build in api.fix ("),
        "the waiter did not name the holder: {stderr}"
    );
    assert!(
        lines[1].contains("running: playwright test in web.main ("),
        "the waiter did not name the new holder: {stderr}"
    );
}

#[test]
fn a_slot_not_yet_written_is_announced_once() {
    let m = Machine::new("unwritten", 1);
    let slot = m.file("slot-0.lock");
    let lock = fs::File::create(&slot).unwrap();
    lock.lock().unwrap();
    let mut waiter = m.command(&["true"]).stderr(Stdio::piped()).spawn().unwrap();
    sleep(Duration::from_millis(1600));
    waiter.kill().unwrap();
    let out = waiter.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stderr.lines().collect::<Vec<_>>(),
        ["hochet: waiting at place 1"],
        "{stderr}"
    );
}

fn meminfo_gib(gib: u64) -> String {
    format!("MemTotal: 99999999 kB\nMemAvailable: {} kB\n", gib << 20)
}

/// A waiter at the head of the queue, with a free slot and another command in a slot.
fn spawn_resource_waiter(m: &mut Machine, name: &str, env: &[(&str, String)]) -> Child {
    m.spawn(&["sleep", "30"]);
    assert!(within(Duration::from_secs(3), || m
        .status()
        .contains("sleep 30")));
    let mut cmd = m.command(&["touch", m.file(name).to_str().unwrap()]);
    cmd.envs(env.iter().map(|(k, v)| (k, v)))
        .stderr(Stdio::piped());
    cmd.spawn().unwrap()
}

#[test]
fn short_memory_waits_then_starts_once_memory_is_back() {
    let mut m = Machine::new("memory", 2);
    let meminfo = m.file("meminfo");
    fs::write(&meminfo, meminfo_gib(1)).unwrap();
    let waiter = spawn_resource_waiter(
        &mut m,
        "ran",
        &[
            ("HOCHET_MIN_FREE_GB", "4".into()),
            ("HOCHET_TEST_MEMINFO", meminfo.display().to_string()),
        ],
    );
    sleep(Duration::from_millis(1000));
    assert!(
        !m.file("ran").exists(),
        "the command started with 1 GiB free and HOCHET_MIN_FREE_GB=4"
    );
    fs::write(&meminfo, meminfo_gib(8)).unwrap();
    assert!(
        within(Duration::from_secs(2), || m.file("ran").exists()),
        "the command did not start once memory was back above the threshold"
    );
    let stderr = String::from_utf8(waiter.wait_with_output().unwrap().stderr).unwrap();
    assert!(
        stderr.contains("memory short, 1.0 GiB free"),
        "the wait message does not say that memory is short: {stderr}"
    );
    assert!(
        stderr.contains("; running: sleep 30 in "),
        "the shortage message does not name the holder: {stderr}"
    );
}

#[test]
fn short_disk_waits_while_another_command_runs() {
    let mut m = Machine::new("disk", 2);
    let disk = m.file("disk");
    fs::write(&disk, "2").unwrap();
    let waiter = spawn_resource_waiter(
        &mut m,
        "ran",
        &[
            ("HOCHET_MIN_DISK_GB", "10".into()),
            ("HOCHET_TEST_DISK_FILE", disk.display().to_string()),
        ],
    );
    sleep(Duration::from_millis(1000));
    assert!(
        !m.file("ran").exists(),
        "the command started with 2 GiB of disk free and HOCHET_MIN_DISK_GB=10"
    );
    fs::write(&disk, "20").unwrap();
    assert!(
        within(Duration::from_secs(2), || m.file("ran").exists()),
        "the command did not start once disk space was back above the threshold"
    );
    let stderr = String::from_utf8(waiter.wait_with_output().unwrap().stderr).unwrap();
    assert!(
        stderr.contains("disk short, 2.0 GiB free"),
        "the wait message does not say that disk space is short: {stderr}"
    );
}

#[test]
fn a_command_alone_starts_whatever_the_thresholds() {
    let m = Machine::new("alone", 2);
    let meminfo = m.file("meminfo");
    let disk = m.file("disk");
    fs::write(&meminfo, meminfo_gib(0)).unwrap();
    fs::write(&disk, "0").unwrap();
    let ran = m.file("ran");
    let mut cmd = m.command(&["touch", ran.to_str().unwrap()]);
    cmd.env("HOCHET_MIN_FREE_GB", "1000")
        .env("HOCHET_MIN_DISK_GB", "1000")
        .env("HOCHET_TEST_MEMINFO", &meminfo)
        .env("HOCHET_TEST_DISK_FILE", &disk);
    let mut child = cmd.spawn().unwrap();
    assert!(
        within(Duration::from_secs(3), || ran.exists()),
        "a command alone on the machine waited for memory or disk"
    );
    child.wait().unwrap();
}

#[test]
fn bad_resource_thresholds_exit_64() {
    let m = Machine::new("thresholds", 2);
    for (name, value) in [
        ("HOCHET_MIN_FREE_GB", "lots"),
        ("HOCHET_MIN_DISK_GB", "-1"),
        ("HOCHET_MIN_FREE_GB", "inf"),
    ] {
        let mut bad = m.command(&["true"]);
        bad.env(name, value);
        assert_eq!(
            bad.output().unwrap().status.code(),
            Some(64),
            "{name}={value} was accepted"
        );
    }
}

/// Runs `hochet sh -c` with `env` and returns the `CARGO_BUILD_JOBS` that the command saw.
fn seen_jobs(m: &Machine, name: &str, prefix: &[&str], env: &[(&str, &str)]) -> String {
    let file = m.file(name);
    let script = format!("echo \"${{CARGO_BUILD_JOBS-unset}}\" > {}", file.display());
    let mut args = prefix.to_vec();
    args.extend(["sh", "-c", &script]);
    let mut cmd = m.command(&args);
    cmd.env_remove("HOCHET_JOBS").env_remove("CARGO_BUILD_JOBS");
    for (key, value) in env {
        cmd.env(key, value);
    }
    assert!(cmd.output().unwrap().status.success());
    fs::read_to_string(file).unwrap().trim_end().to_string()
}

#[test]
fn hochet_jobs_sets_cargo_build_jobs_unless_already_set() {
    let m = Machine::new("jobs", 2);
    assert_eq!(seen_jobs(&m, "set", &[], &[("HOCHET_JOBS", "4")]), "4");
    assert_eq!(
        seen_jobs(
            &m,
            "kept",
            &[],
            &[("HOCHET_JOBS", "4"), ("CARGO_BUILD_JOBS", "7")]
        ),
        "7",
        "HOCHET_JOBS replaced an explicit CARGO_BUILD_JOBS"
    );
    assert_eq!(seen_jobs(&m, "absent", &[], &[]), "unset");
}

#[test]
fn a_nested_call_keeps_the_outer_jobs() {
    let m = Machine::new("nested-jobs", 1);
    let bin = env!("CARGO_BIN_EXE_hochet");
    assert_eq!(
        seen_jobs(
            &m,
            "nested",
            &["env", "HOCHET_JOBS=8", bin],
            &[("HOCHET_JOBS", "4")]
        ),
        "4"
    );
}

#[test]
fn bad_jobs_exit_64() {
    let m = Machine::new("bad-jobs", 2);
    for bad in ["0", "four", ""] {
        let mut cmd = m.command(&["true"]);
        cmd.env("HOCHET_JOBS", bad);
        assert_eq!(
            cmd.output().unwrap().status.code(),
            Some(64),
            "HOCHET_JOBS='{bad}' was accepted"
        );
    }
}
