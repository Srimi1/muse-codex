# Security policy

Security is part of Muse Codex's compatibility contract. Please read the
[security model](../docs/SECURITY.md) for trust boundaries, credential handling,
network controls, and residual risks.

## Supported versions

Muse Codex has not published a stable binary release yet.

| Version | Security support |
| --- | --- |
| Latest commit on `main` | Supported |
| Older commits and forks | Best effort |
| Unofficial binary builds | Not supported |

The only supported host baseline is Muse Code `1.0.3-R2198.1` on Apple-silicon
macOS. Reports involving other Muse builds are still useful, but may not be
reproducible in the supported configuration.

## Report a vulnerability privately

Use [GitHub private vulnerability reporting](https://github.com/Srimi1/muse-codex/security/advisories/new).
Do not open a public issue for a suspected vulnerability.

Include, when safe:

- the affected Muse Codex commit or version;
- the stock Muse version and macOS version;
- a minimal, redacted reproduction;
- impact and attack prerequisites; and
- any suggested mitigation.

Never include live credentials, signing keys, private-feed URLs, proprietary
Muse artifacts, prompts, or user data. Replace them with synthetic values.

The maintainer aims to acknowledge a complete report within seven days, assess
severity, coordinate a fix, and agree on disclosure timing. Response time is a
target, not a service-level guarantee.

## Scope

In scope are credential isolation, loopback authentication, provider routing,
request and response translation, tool-call integrity, process isolation, and
release verification implemented by this repository. Vulnerabilities in Muse,
OpenAI services, GitHub, macOS, or a third-party dependency should also be
reported to the affected upstream project when appropriate.
