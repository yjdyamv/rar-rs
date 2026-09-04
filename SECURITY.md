# Security Policy

## Supported versions

Security fixes target the latest released version and the current `main`
branch. Older releases are not maintained separately unless a release note says
otherwise.

## Reporting a vulnerability

Please report vulnerabilities privately when the repository host offers a
private security-report or advisory channel. If no private channel is
available, contact a maintainer through a private channel listed on their host
profile. As a last resort, open a public issue asking for a private contact,
but do not include exploit details or attach a malicious archive.

Include, when possible:

- the affected version or commit;
- the archive format and operation involved;
- a minimal reproducer shared privately;
- expected and observed behavior;
- impact, including resource exhaustion, path traversal, memory safety, data
  loss, or integrity concerns;
- any suggested mitigation.

Treat crafted archives, passwords, filesystem paths, and extracted content as
potentially sensitive. Remove unrelated personal data before sharing samples.

## Scope and disclosure

Parser and decoder robustness, extraction path safety, resource limits,
cryptographic handling, archive rewrite integrity, CLI destructive operations,
and native/WASI binding boundaries are in scope. Reports about the proprietary
RAR/WinRAR products should be sent to their vendor instead.

Maintainers will coordinate validation, remediation, release timing, and public
disclosure with the reporter. No fixed response-time SLA is currently offered.
