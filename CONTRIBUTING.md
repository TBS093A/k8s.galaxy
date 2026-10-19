# Contributing

```bash
pip install pre-commit && pre-commit install   # gitleaks, fmt, clippy and the private-word check on every commit
cargo test                                     # unit, property and render tests
cargo run -- --demo                            # simulated cluster, no kubeconfig needed
DAST=1 PROPTEST_CASES=20000 cargo test --release dast   # the deep profile the Security workflow runs
```

Formatting is `cargo fmt` (`rustfmt.toml`: 140 columns), lints are `cargo clippy --all-targets`
with warnings as errors, and the crate forbids `unsafe` code.

Private words — your domains, host names, cluster or customer names — go into `.private-patterns`,
one case-insensitive regular expression per line. The file is git-ignored, so the list itself stays
private; the `private-patterns` hook checks every staged file and file name against it.

Pull requests need the `ci-passed` and `security-passed` checks (see [docs/RELEASING.md](docs/RELEASING.md)).
Add a line to `CHANGELOG.md` under `## Unreleased` for anything users notice.
