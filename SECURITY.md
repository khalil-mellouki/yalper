# Security Policy

## Reporting a vulnerability

Please do not report security vulnerabilities through public issues, discussions, or pull requests.

Report them privately through GitHub instead:

1. Go to the [Security tab](https://github.com/khalil-mellouki/yalper/security) of this repository.
2. Click **Report a vulnerability**.
3. Describe the issue, the steps to reproduce it, and its possible impact.

You will receive a response as soon as possible. Once the issue is confirmed, a fix will be prepared and
released, and you will be credited in the advisory unless you prefer to stay anonymous.

## Supported versions

Yalper is in early development and has no stable release yet. Security fixes are applied to the latest version
on `main`.

## Scope

Yalper records AI agent sessions, which can contain sensitive data. Issues in these areas are especially
important:

- Secrets (API keys, tokens, passwords) that are not redacted before being written to disk.
- Recorded data leaving the local machine without the user asking for it.
- Commands or files outside the project being read or modified unexpectedly.
