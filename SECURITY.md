# Security

## Report a problem

Email support@zriz.io. Do not open a public issue or pull request for a security problem.

You can also use GitHub's private report form: the Security tab of this repository, then "Report a vulnerability".

Say what you found and how to reproduce it. We answer, fix, and tell you when it is released.

## In scope

Anything that lets the zriz cloud, or a pipeline it runs, do one of these:

- read a secret held by the runner or the worker
- reach a host outside the resource's allowlist (including by redirect)
- write through a resource declared `read-only`
- run a command outside the declared command shapes
- get a secret into a result, a log line or an error message

## Supported versions

The latest release only.

Release images (`ghcr.io/zriztech/runner` and `ghcr.io/zriztech/worker`) are signed and have a build record. The verify steps are in the README, section "Verify the image".
