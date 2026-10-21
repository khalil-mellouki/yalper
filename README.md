# Yalper

A dashcam for your AI coding agent. Yalper records what the agent does to your code, one step at a time,
so when something breaks you can see exactly which step did it.

> **Status: early.** Recording and bisect work today with [Claude Code](https://docs.claude.com/en/docs/claude-code).
> Expect rough edges, and expect things to change. Feedback and ideas are very welcome.

## What it does today

- **Records every step** of a Claude Code session: your prompts, every tool call, its input and output, and
  whether it failed.
- **Snapshots your code after each step**, including files changed by shell commands (`sed`, scripts, code
  generators), not only edits made through the agent's file tools.
- **Masks secrets** in known formats (API keys, tokens, passwords) before anything is recorded. It cannot
  catch every possible secret.
- **`yalper log`** lists the steps of a session. **`yalper show <step>`** shows what one step did, with a diff
  of the files it changed.
- **`yalper bisect`** runs your tests against the code of the steps and names the step that broke them, with
  the prompt behind it and its diff.
- **Stays out of the way:** a step usually costs a few tens of milliseconds, and Yalper never blocks or breaks
  the agent.

Everything stays on your machine: recordings in a `.yalper/` folder inside your project, bisect's test copies
in your cache folder. Yalper itself sends nothing. When bisect installs a step's dependencies, your package
manager downloads them as it normally would.

## What we want to build next

- **Replay:** a local timeline in the browser to scroll through a session.
- **Blame:** for any line, see which step wrote it and the prompt behind it.
- **Fork and compare:** restart a session from any step with a different prompt or model, and compare.
- **More agents** than Claude Code.

None of these exist yet. Ideas and help are welcome (see [Contributing](#contributing)).

## Quick start

Yalper is not on package managers yet. For now it installs from source, which needs
[Rust](https://rustup.rs) and Claude Code 2.1.139 or later.

```sh
cargo install --git https://github.com/khalil-mellouki/yalper --locked yalper
```

Then, inside a git project:

```sh
yalper init                       # once: tells Claude Code to call Yalper on every step
# ... use Claude Code as usual ...
yalper log                        # the steps of the latest session
yalper show 5                     # what step 5 did, with the diff of the files it changed
yalper bisect --test "npm test"   # which step broke the tests
```

To stop: `yalper uninstall` removes the hooks and keeps the recordings. `yalper uninstall --purge` also
deletes them, and the project's bisect data.

## Find the step that broke your tests

The agent took 20 steps and now your tests fail. Give Yalper the command that runs your tests:

```sh
yalper bisect --test "npm test"
```

Yalper runs that command against the code of a few steps of the latest session, halving the range each time
like `git bisect`, until it finds the first step where the tests fail. It shows that step, the prompt behind
it, and the diff of what it changed. As with `git bisect run`, exit code 0 from your command means the tests
pass, 125 means this step cannot be tested, anything else means they fail.

A real run on a small Node project (paths shortened):

```text
$ yalper bisect --test "node --test"
Bisect data (test copies and run logs): ~/.cache/yalper/durations-9860a044ef2f
Bisecting session 5f3c2a1e: 20 steps changed files after step 1 (the good end), up to step 22 (the bad end).
Testing step 1, the good end (about 5 runs left)... installed its dependencies in 3.4 s, passed in 0.4 s
Testing step 22, the bad end (about 4 runs left)... failed (exit code 1) in 0.4 s (first run in a new test copy: time limit raised to 600 s)
Testing step 12 (about 3 runs left)... failed (exit code 1) in 0.4 s
Testing step 7 (about 2 runs left)... passed in 0.4 s
Testing step 9 (about 1 run left)... failed (exit code 1) in 0.4 s
Testing step 8 (about 0 runs left)... failed (exit code 1) in 0.4 s

Step 8 broke the tests: they pass at step 7 and fail at step 8 (exit code 1). Found in 6 runs.
Its test output: ~/.cache/yalper/durations-9860a044ef2f/logs/run-6.log

Session 5f3c2a1e, step 8 of 23, 2026-10-11 18:13:06

Prompt (step 2):
  Add notes and keep the duration helper up to date.

Write src/duration.js, succeeded in 3 ms
`yalper show 8 --session 5f3c2a1e` shows its full input and output.

1 file changed:
  modified  src/duration.js

--- a/src/duration.js
+++ b/src/duration.js
@@ -2,7 +2,7 @@
 
 /** The number of seconds in a duration such as "2m". */
 function seconds(text) {
-  return ms(text) / 1000;
+  return ms(text) / 100;
 }
 
 module.exports = { seconds };
```

Steps whose lockfiles differ from your project's get their dependencies installed once, in a test copy of
their own, and reused. Running the same bisect again installs nothing.

Options:

- `--with-env`: copy your ignored `.env` and `.env.*` files into the test copies. Remembered until `--no-env`.
- `--with <path>`: copy another ignored file or folder into the test copies; repeat it for several.
  Remembered until `--no-with`.
- `--setup "<command>"`: the command that installs dependencies in a test copy, instead of the one Yalper
  picks from your lockfiles.
- `--timeout <time>`: the time limit of each run (`90`, `30s`, `5m`). By default it is picked from the first
  runs.
- `--retries <n>`: repeat a failing run up to `n` times (at most 10) to rule out flaky tests.
- `--session <id>`: bisect another session than the latest one.
- `--good <step>`, `--bad <step>`: a step where the tests pass, and one where they fail. With `--all` or
  `--since`, `<session>:<step>` names a step of another session.
- `--all`: bisect every recorded session of the project, and the changes made between them.
- `--since <date>`: the same, from a date (`2026-10-01` or `"2026-10-01 14:30"`).

Exit codes: 0 when a step (or a short range of steps) was named, 1 when none could be, 130 when you stopped it with Ctrl-C.

### Is it safe?

- **Bisect runs the code of each step**, including its scripts and its installs. Only bisect projects you
  trust.
- **Yalper never changes your project.** Tests run in a copy outside it. A few cases can still reach your
  real files: see [Known limits](#known-limits).
- **Your test command is never printed.** It is still visible in your shell history and to an agent that runs
  bisect, so keep secrets in `.env` and use `--with-env` rather than putting them in the command.
- **Secrets are masked** in the test output and the result Yalper prints. The full run logs in the cache
  folder are not masked; they stay until the next bisect of the project, `yalper uninstall --purge`, or
  deleting the folder.
- **Copied files are private and short-lived.** Env files and `--with` files are copied into test copies only
  you can read, and deleted when the bisect ends (after a crash or kill, at the next bisect or
  `yalper uninstall --purge`).
- **One bisect per project at a time.** A second one is refused until the first ends.

### Where the bisect data lives

Test copies and run logs live in your cache folder, one folder per project:

- Linux: `~/.cache/yalper/` (or `$XDG_CACHE_HOME/yalper/`)
- macOS: `~/Library/Caches/yalper/`
- Windows: `%LOCALAPPDATA%\yalper\`

Delete it with `yalper uninstall --purge`, or by hand while no bisect runs, with a tool that does not follow
links (Explorer, `rm -rf`, `rmdir /s`): a test copy can hold links to your project's installed packages. The
next bisect rebuilds what it needs.

Set `YALPER_CACHE_DIR` to move it, for example to a faster drive on Windows. Outside your user profile,
Yalper does not check Windows access rights: pick a folder only you can read and write (other users could
read the run logs).

Because tests run outside your project, ignored local configuration (an ignored `.cargo/config.toml` or
`.npmrc`) only reaches them if you pass it with `--with`.

### Known limits

- Tests use your current `.env` and `--with` files, not the ones from the time of each step.
- Absolute paths into your project inside settings or files (a `DATABASE_URL` such as
  `sqlite:///path/to/project/dev.db`) still point at your project.
- A test that writes inside an installed package folder, or runs `pip install`, changes your real environment.
- On macOS and Linux, links in a step's code stay links in the copy, so a test that writes through one changes
  what it points to.
- A step that needs its own install and holds a link pointing outside the project is not tested.
- A `package.json` or `pyproject.toml` without a lockfile installs nothing unless you pass `--setup`. Ruby,
  PHP and Go lockfiles are not checked: those steps run with your current dependencies.
- Packages that take their version from git (setuptools-scm, for example) may fail to install in a copy.
- With `YALPER_CACHE_DIR` on a FAT32 or exFAT drive, a read-only file a test leaves in a copy (for example
  from `git commit`) stops the next bisect with an error.
- On Windows, when Claude Code runs the bisect for you, call a project script with a path: `.\check.cmd`.

## How it works

```
yalper init          writes Claude Code hooks into .claude/settings.local.json (personal, never committed)
      |
Claude Code runs     after every step, Claude Code calls `yalper hook`, which:
      |                - masks secrets in the step's data
      |                - saves the step in a small SQLite database in .yalper/
      |                - snapshots the project's files (git-style, so unchanged files are stored once)
      |
yalper log / show    read that recording back
yalper bisect        runs your tests on those snapshots, in copies outside the project
```

Yalper never touches your own `.git`: snapshots live in a separate store inside `.yalper/`, and `.yalper/` is
excluded from git automatically.

## Contributing

This is a project by developers, for developers, and everyone is welcome to join in: bug reports, ideas,
questions, or code. Start a conversation in [Discussions](https://github.com/khalil-mellouki/yalper/discussions)
or open an issue. Before sending code, please read [CONTRIBUTING.md](CONTRIBUTING.md) and the
[Code of Conduct](CODE_OF_CONDUCT.md). To report a security issue, see [SECURITY.md](SECURITY.md).

## License

Licensed under the [Apache License, Version 2.0](LICENSE). The Yalper name and logo are covered by the
[trademark policy](TRADEMARKS.md).
