## Summary

Describe what changed and why.

## Validation

List the commands or manual checks you ran.

## Checklist

- [ ] The change is focused and does not modify vendored source unnecessarily.
- [ ] `cargo fmt -p codex-transport -p muse-codex -p muse-codex-gateway -- --check` passes.
- [ ] `cargo clippy --workspace --all-targets --locked -- -D warnings` passes.
- [ ] `cargo test --workspace --all-targets --locked` passes.
- [ ] `bash tests/scripts/release-tooling-test.sh` passes when release tooling changes.
- [ ] Tests cover new behavior or the PR explains why tests are not applicable.
- [ ] Documentation and `CHANGELOG.md` are updated when user-visible behavior changes.
- [ ] No credentials, private URLs, proprietary Muse artifacts, or signing keys are included.
- [ ] Security, provider-boundary, retry, and tool-call implications were considered.

## Security and compatibility impact

Explain any effect on authentication, data flow, supported Muse versions,
provider routing, release integrity, or tool-call behavior. Write `None` when
there is no impact.
