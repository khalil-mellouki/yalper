# Yalper

Record, replay, and debug AI coding agent sessions. Yalper snapshots your code at every agent step, finds the
exact step that broke your tests, and lets you fork a session from any point. Local-first and open source.

> **Status: early development.** Yalper is not ready for use yet. Watch the repo to follow progress.

## Why Yalper

When an AI coding agent breaks something halfway through a long session, you are left with a final diff and a
long chat log. Yalper records every step and gives you:

- **Bisect:** run your tests against each step's snapshot to find the exact step that broke your code.
- **Fork and compare:** restore your repo at any step, rerun with a different prompt, model, or agent, and diff
  the results.
- **Step-level blame:** for any line, see which step wrote it, the prompt behind it, and what the agent read
  just before.
- **Full snapshots:** captures every change, including files modified by shell commands.

Yalper starts with [Claude Code](https://docs.claude.com/en/docs/claude-code) and will support more agents later.

## Principles

- **Local-first:** recordings stay on your machine.
- **Private by default:** secrets are redacted before anything is written to disk.
- **Fast:** recording must never slow down the agent or your machine.
- **Simple:** one binary, one command to set up.

## Contributing

Contributions are welcome. Please read [CONTRIBUTING.md](CONTRIBUTING.md) and our
[Code of Conduct](CODE_OF_CONDUCT.md) first. To report a security issue, see [SECURITY.md](SECURITY.md).

## License

Licensed under the [Apache License, Version 2.0](LICENSE). The Yalper name and logo are covered by the
[trademark policy](TRADEMARKS.md).
