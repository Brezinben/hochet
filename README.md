# hochet

A machine-wide queue for the heavy commands of coding agents.

When several agents work on one machine, each in its own worktree, they start cold builds,
browser suites and mutation tests at the same time. The disk fills up, memory runs out, and
on WSL the whole VM can crash. `hochet` lets at most two heavy commands run at once across
every worktree and every project on the machine. The others wait in line and print their
place.

```console
$ hochet cargo test
hochet: 2 slots busy, waiting at place 1 (hochet status)
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
hochet CMD...          run CMD once one of HOCHET_SLOTS slots is free (default 2)
hochet --force CMD...  run CMD now, without a slot (a person at a terminal only)
hochet status          list the held slots and the queue
```

- `HOCHET_SLOTS` sets the number of slots. Set it in your shell profile so every agent sees
  the same value.
- `HOCHET_DIR` moves the state (default `~/.cache/hochet`).
- A heavy script that calls `hochet` again inside a held slot runs at once, on the same slot.

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
