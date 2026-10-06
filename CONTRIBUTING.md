# Contributing to Yalper

Thank you for your interest in Yalper. This guide explains how to propose changes.

## Before you start

- For bugs, open an issue using the bug report template.
- For new features or larger changes, open an issue or a discussion first so we can agree on the approach
  before you write code.
- For security issues, do not open a public issue. Follow [SECURITY.md](SECURITY.md) instead.

## Making a change

1. Fork the repository and create a branch from `main`.
2. Make your change. Keep pull requests small and focused on one thing.
3. Add or update tests for your change.
4. Make sure the project builds and all tests pass.
5. Sign off every commit (see below).
6. Open a pull request and fill in the template.

All changes reach `main` through pull requests. Direct pushes to `main` are blocked.

## Developer Certificate of Origin (DCO)

Yalper uses the [Developer Certificate of Origin](https://developercertificate.org/) instead of a contributor
license agreement. By signing off a commit, you certify that you wrote the change or otherwise have the right
to submit it under the project's license.

Sign off by adding the `-s` flag when you commit:

```
git commit -s -m "Describe your change"
```

This adds a line like this to your commit message:

```
Signed-off-by: Your Name <your.email@example.com>
```

The name and email must match your git configuration. If you forgot to sign off, fix the last commit with
`git commit --amend -s` and force push your branch.

## License of contributions

By contributing, you agree that your contributions are licensed under the
[Apache License, Version 2.0](LICENSE), the same license as the project.

## Code of Conduct

Everyone taking part in this project is expected to follow our [Code of Conduct](CODE_OF_CONDUCT.md).
