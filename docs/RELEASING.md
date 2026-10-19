# Releasing k8s-galaxy

## How the pipelines fit together

| When | Workflow | What happens |
|------|----------|--------------|
| every commit (locally) | `pre-commit` | gitleaks, `cargo fmt`, `cargo clippy`, YAML/TOML checks, no private words (from a git-ignored list) |
| pull request (**pre-merge gate**) | `CI` | fmt, clippy with warnings as errors, tests, `cargo check` on the minimum supported Rust (`rust-version`), release build of the Linux binary with a smoke test (version, help, the demo running in a real terminal) |
| | `Security` | gitleaks over the full history; SAST: CodeQL (Rust + workflows), zizmor (workflows), no `unsafe`, strict clippy; SCA: cargo-deny (advisories, licenses, bans, sources), cargo-audit, dependency review; DAST: every view rendered at thousands of terminal sizes, property tests feeding hostile input to every parser, the demo under a pseudo-terminal at odd sizes |
| merge to `main` (**post-merge**) | `Post-merge` | binaries for Linux x86_64/aarch64 and macOS arm64/x86_64, each smoke-tested, kept as artifacts; once enabled, the `nightly` pre-release is replaced with them (with build provenance) |
| tag `vX.Y.Z` (**release**) | `Release` | tag, changelog and `Cargo.toml` version checks, the full `CI` and `Security` workflows again, build and verification on every target, **manual approval**, GitHub release with archives, checksums and build provenance, crates.io (once enabled), downloading the published archive and verifying checksum, attestation and version on Linux and macOS |
| weekly | `Security`, Dependabot | new advisories surface without code changes; action and Cargo updates |

Merges to `main` require the `ci-passed` and `security-passed` checks.

Releases come from tags, not from merges: a published version is permanent, so a release is a
deliberate decision. Every merge still exercises the build and verification path.

## One-time setup

1. **Branch protection** for `main`: require pull requests and the status checks
   `ci-passed` and `security-passed`.

   ```bash
   gh api -X PUT repos/TBS093A/k8s.galaxy/branches/main/protection --input - <<'JSON'
   {"required_status_checks": {"strict": true, "contexts": ["ci-passed", "security-passed"]},
    "enforce_admins": false, "required_pull_request_reviews": null, "restrictions": null}
   JSON
   ```

2. **Environment `release`**: Settings → Environments → `release`, required reviewer: you,
   deployment tags: `v*`. Same for `crates-io` if you publish there.

3. **Code scanning** (CodeQL, dependency review): on a public repository it just works. On a
   private one it needs GitHub Advanced Security; once enabled:

   ```bash
   gh variable set CODE_SCANNING --body true --repo TBS093A/k8s.galaxy
   ```

   Until then those jobs are skipped; zizmor, cargo-deny and cargo-audit still run.

4. **Nightly pre-release** (optional): `gh variable set NIGHTLY_RELEASE --body true --repo TBS093A/k8s.galaxy`

5. **crates.io** (optional): publish the first version by hand (`cargo publish`), then add a
   trusted publisher on crates.io (crate settings → Trusted Publishing: repository
   `TBS093A/k8s.galaxy`, workflow `release.yml`, environment `crates-io`) — no API token is
   stored — and `gh variable set CRATES_PUBLISH --body true --repo TBS093A/k8s.galaxy`.

## Making a release

1. Set `version` in `Cargo.toml`, run `cargo build` (updates `Cargo.lock`), move the entries of
   `CHANGELOG.md` under `## X.Y.Z - YYYY-MM-DD`, and merge that through a pull request.
2. Tag the merge commit on `main` and push the tag:

   ```bash
   git switch main && git pull
   git tag -a v0.1.0 -m "k8s-galaxy 0.1.0"
   git push origin v0.1.0
   ```

3. Approve the `release` deployment in the run of the **Release** workflow.

A failed check before the approval publishes nothing: fix it, delete the tag
(`git push --delete origin v0.1.0 && git tag -d v0.1.0`) and tag again.
