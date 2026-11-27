# Security policy

Quod decides which tests may be skipped and signs that decision. A bug
that lets a test be skipped when the change can affect it is a security
issue here, not only a correctness bug. So is anything that lets a
certificate or Verdict be forged, replayed, or accepted by `quod-verify`
when it shouldn't be.

## Reporting a vulnerability

Report privately through GitHub:
**Security → Report a vulnerability** on
[zenedgexlabs/quod](https://github.com/zenedgexlabs/quod/security/advisories/new).
Please don't open a public issue.

Include what you can of:

- the version (`quod --version`, probe versions) and OS;
- a repository or a minimal reproduction: the base commit, the change, the
  test that was skipped and the failure it hid;
- for signing or verification issues, the certificate or Verdict and the
  `quod-verify` command line.

We aim to acknowledge a report within 3 working days and to agree on a
disclosure date with you. Fixes are released as a new version, with an
advisory that credits you unless you'd rather not be named.

## What counts

In scope:

- **False skips (escapes).** A test that Quod certified as skippable fails
  when run on the change. This includes probe gaps (a dependency the probe
  didn't record) and engine gaps (a change the engine mapped to too few
  nodes).
- **Signature and verification bypasses** in `quod-cert`, `quod-verdict`
  and `quod-verify`.
- **The GitHub Action** running untrusted input in a privileged context.

Not a vulnerability, but please open an issue:

- Quod running more tests than needed (that is a precision problem).
- Failures in advisory mode (`--advisory`), whose certificates are never
  accepted as proof.

## Supported versions

Quod is in alpha. Only the latest release gets security fixes.
