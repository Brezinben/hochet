# hochet development guide

hochet is a machine-wide queue for heavy commands. Several coding agents on one machine, each
in its own worktree, start builds and browser suites at once and exhaust the disk and the
memory. `hochet CMD` runs CMD only when one of `HOCHET_SLOTS` slots (default 2) is free. The
`jouet/` folder is a jouet plugin: its rules deny a direct heavy call by an agent and give the
`hochet` command instead.

## Build and test

- Tests: `mise run test`. They run the built binary (`CARGO_BIN_EXE_hochet`) in a temporary
  `HOCHET_DIR`.
- Lint: `mise run lint` (rustfmt, clippy `-D warnings`, cargo-shear). `mise run lint-fix`
  applies the fixes.
- Everything CI runs: `mise run ci`. hk runs the same linters at each commit (`hk.pkl`).
- jouet rules: `jouet rule test` checks the `fires` and `quiet` examples of
  `jouet/rules/hochet.toml`. Change a matcher together with its examples.

## Code rules

- The lock is the design. Slots and queue tickets are `flock` locks, so the kernel frees them
  when a holder dies. A slot or a place that a file content or a PID decides is a bug.
- The slot lock must survive `exec`: `keep_across_exec` clears close-on-exec on its fd.
- Exit codes: 64 for usage and configuration errors, 127 when CMD cannot run, else the exit
  code of CMD. Never exit 2: a Claude Code Stop hook treats 2 as "block this reply".
- Each test fails when its mechanism breaks. Before you add a mechanism, add the test that
  goes red without it.
- No `#[allow(...)]`. Refactor until clippy passes.

## Release

Push a `v*` tag on `main`. `release.yml` builds the Linux and macOS binaries, attaches
`hochet-<target>.tar.gz` and its checksum, then publishes the draft release.
