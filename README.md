# hochet

A machine-wide queue for the heavy commands of coding agents.

When several agents work on one machine, each in its own worktree, they start cold builds,
browser suites and mutation tests at the same time. The disk fills up, memory runs out, and
on WSL the whole VM can crash. `hochet` lets at most two heavy commands run at once across
every worktree and every project on the machine. The others wait in line and print their
place.

```console
$ hochet cargo test
hochet: waiting at place 1; running: playwright test in app.feature-x (5m12s), cargo build in api.fix-y (1m35s)
...
$ hochet status
slot 0: 312s  playwright test   /home/me/app.feature-x   9f1c…   pid 4242
slot 1: 95s   cargo build       /home/me/api.fix-y       terminal pid 4300
queue 1: cargo test   /home/me/api.main   3b7e…   pid 4417
```

## Install

```sh
mise use -g github:Brezinben/hochet
```

Or build it from source: `cargo install --git https://github.com/Brezinben/hochet --locked`.

hochet runs on Linux and macOS.

## Use

```text
A machine-wide queue for heavy commands (builds, browser and mutation suites)

Usage: hochet [OPTIONS] CMD...
       hochet -- CMD...
       hochet --force [--] CMD...
       hochet status

Commands:
  status  List the held slots and the queue

Arguments:
  <CMD>...  The command to run once a slot is free; put `--` before it when its name starts with '-'

Options:
      --force                      Run CMD now, without a slot (a person at a terminal only)
      --slots <SLOTS>              Number of commands that run at once across the machine [env: HOCHET_SLOTS=] [default: 2]
      --jobs <JOBS>                Set CARGO_BUILD_JOBS for CMD, unless a non-empty one is already set [env: HOCHET_JOBS=]
      --min-free-gb <MIN_FREE_GB>  Wait while the available memory is below this many GiB (0 turns the check off) [env: HOCHET_MIN_FREE_GB=] [default: 0]
      --min-disk-gb <MIN_DISK_GB>  Wait while the free disk space is below this many GiB (0 turns the check off) [env: HOCHET_MIN_DISK_GB=] [default: 0]
      --dir <DIR>                  State directory [default: $XDG_CACHE_HOME/hochet, else ~/.cache/hochet] [env: HOCHET_DIR=]
  -h, --help                       Print help
  -V, --version                    Print version
```

hochet reads each option from its environment variable when the option is not on the command
line. A first argument that starts with `-` and is not an option is an error: hochet prints the
usage and exits 64. An invalid value for an option or a variable also exits 64. Everything
after CMD goes to CMD, options included.

- `HOCHET_SLOTS` sets the number of slots. Set it in your shell profile so every agent sees
  the same value.
- `HOCHET_DIR` moves the state (default `$XDG_CACHE_HOME/hochet`, else `~/.cache/hochet`). An empty value is an error.
- `HOCHET_JOBS=N` sets `CARGO_BUILD_JOBS=N` for CMD, so that two Rust builds in their slots
  do not exhaust the memory. A `CARGO_BUILD_JOBS` already in the environment wins.
- A heavy script that calls `hochet` again inside a held slot runs at once, on the same slot.

### Wait for memory and disk space

A slot count does not know how large each command is. Two cold builds can fill the memory
with a slot still free. Set a minimum, in GiB, and a command waits while the machine has less:

- `HOCHET_MIN_FREE_GB`: the minimum available memory (`MemAvailable` in `/proc/meminfo`).
- `HOCHET_MIN_DISK_GB`: the minimum free space on the file system of the working directory.

Unset, empty or `0` turns a check off. A value that is not a number of 0 or more exits 64. The check
applies only while another hochet command holds a slot. A command alone always starts, so a
threshold never blocks the queue for ever. The wait message gives the short resource and its
free amount:

```console
hochet: memory short, 1.3 GiB free, below HOCHET_MIN_FREE_GB=4, waiting at place 1; running: cargo build in api.fix (3m10s)
```

macOS has no `MemAvailable`, so hochet skips the memory check there. The disk check works on
Linux and macOS. A measure that hochet cannot read skips its check without a message. The disk
check measures the file system of the working directory only, and inside a container
`/proc/meminfo` shows the memory of the host, not the container limit.

The tests replace the measures: `HOCHET_TEST_MEMINFO` names a file read instead of
`/proc/meminfo`, and `HOCHET_TEST_DISK_FILE` names a file that holds the free disk space in GiB.

To keep the Claude Code Stop hook working on a machine without hochet, prefix it like this:

```json
"command": "$(command -v hochet) mise x -- hk agent stop-hook --target claude-code"
```

## Stop agents from skipping the queue

The `jouet/` folder is a [jouet](https://github.com/jdx/jouet) plugin. Its rule `hochet/heavy`
denies a direct `cargo test`, `playwright test`, `npx stryker run` and similar calls by an
agent, and tells it to run `hochet <the command>` instead. The rule `hochet/force` keeps
`--force` for people. Pin it once per machine, in your own terminal:

```sh
jouet plugin add https://github.com/Brezinben/hochet --dir jouet --rev <tag>
```

The rules guide an agent; they do not lock it out. A command that never reaches `hochet`,
such as `playwright` called from inside a shell script, is not counted.

## How it works

Each slot is a file under `~/.cache/hochet` that a running command holds with `flock`. Each
waiting command holds a lock on its own ticket in `queue/`. The kernel releases both when the
process exits, including after `kill -9` or a reboot, so a crashed agent never keeps a slot.
Only the oldest ticket tries to take a slot, which keeps the queue first in, first out. The
slot lock stays open through `exec`, so it lasts as long as the command and its children.

## License

MIT
