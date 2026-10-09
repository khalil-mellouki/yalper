# Yalper

A dashcam for your AI coding agent. Yalper records what the agent does to your code, one step at a time,
so when something breaks you can see exactly which step did it.

> **Status: early.** Recording works today with [Claude Code](https://docs.claude.com/en/docs/claude-code).
> Expect rough edges, and expect things to change. Feedback and ideas are very welcome.

## What it does today

- **Records every step** of a Claude Code session: your prompts, every tool call, its input and output, and
  whether it failed.
- **Snapshots your code after each step**, including files changed by shell commands (`sed`, scripts, code
  generators), not only edits made through the agent's file tools.
- **Masks secrets** (API keys, tokens, passwords) before anything is written to disk.
- **`yalper log`** lists the steps of a session. **`yalper show <step>`** shows what one step did, with a diff
  of the files it changed.
- **Stays out of the way:** a step usually costs a few tens of milliseconds, and Yalper never blocks or breaks
  the agent.

Everything stays on your machine, in a `.yalper/` folder inside your project. Nothing is sent anywhere.

## What we want to build next

- **Bisect:** give Yalper your test command and it finds the exact step that broke your tests.
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
yalper init              # once: tells Claude Code to call Yalper on every step
# ... use Claude Code as usual ...
yalper log               # the steps of the latest session
yalper show 5            # what step 5 did, with the diff of the files it changed
```

To stop: `yalper uninstall` removes the hooks and keeps the recordings. `yalper uninstall --purge` also
deletes them.

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
