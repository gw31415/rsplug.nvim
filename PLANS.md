# Release automation ExecPlan

This document contains the active plan for making tag-triggered releases
consistent, recoverable, and auditable. Keep it current while implementing the
work: record discoveries and decisions, check off completed acceptance items,
and remove detailed recipes once Git history and tests own them.

## Prior plan status

The previous four-phase performance plan has been implemented and validated for
its core I1, S1-S4, L2, C1, and D1 milestones. Its completed recipes have been
removed from this file; Git history owns the implementation details and
before/after reports.

Two expanded follow-ups from that plan were described as both complete and
incomplete in the old document. They are not release blockers and are deferred:

- extend M0 with the complete network/tarball/100k-entry fixture matrix;
- split every remaining C1 scheduler/planner responsibility into separate
  source modules.

If either is resumed, write a focused ExecPlan from the current code rather than
restoring the stale recipes.

## Goal

Make one release version identify one immutable source and artifact set across:

- the annotated Git tag and GitHub-generated source archives;
- every publishable workspace crate and its internal dependency requirements;
- `Cargo.lock`;
- binaries returned by `rsplug --version`;
- GitHub Release assets and checksums;
- crates.io packages;
- direct tag consumers such as `cargo install --git --tag` and
  `nix build github:gw31415/rsplug.nvim`.

The tag remains the automatic release trigger, but it is no longer the moment
at which source files are rewritten. A preparation command updates and validates
the version before the release commit is tagged. The tag workflow is read-only
with respect to the checked-out source: it verifies, tests, builds, stages,
publishes, and attests exactly that commit.

This plan covers repository files and the documented GitHub/crates.io settings
needed by the workflow. It does not authorize publishing a real release during
implementation. Use local fixtures and a clearly named temporary prerelease tag
for the final end-to-end exercise only after explicit approval.

## Current problem and evidence

Read these files before implementation:

- `.github/workflows/release.yml`;
- `scripts/set-version.sh`;
- root `Cargo.toml` and `Cargo.lock`;
- `crates/*/Cargo.toml`;
- `crates/walker/vendor/fts/Cargo.toml` and `Cargo.toml.orig`;
- `crates/walker/vendor/fts/README.md` and `NOTICE.md`;
- `rust-toolchain.toml`, `flake.nix`, and the installation section of
  `README.md`.

Before this implementation, the workflow checked out a tag and then ran
`scripts/set-version.sh` in each build/publish job. That produced dirty,
job-local manifests rather than a versioned source commit. Historical tags
demonstrate the mismatch:

| tag | workspace version in tagged `Cargo.toml` |
| --- | --- |
| `v0.1.1` | `0.1.0` |
| `v0.2.1` | `0.2.0` |
| `v0.2.2` | `0.2.0` |

The GitHub-generated source archive and Nix build therefore differ from the
binary and crates.io artifacts carrying the same release name.

The failed `v0.2.4` run also established a second failure mode:
`cargo pkgid` read stale versions from `Cargo.lock`, the workflow treated new
internal crates as already published, `rsplug` publication then failed because
its required internal versions were absent, and the independent GitHub Release
job still succeeded. The same tag was subsequently moved and run again.

The current publish dependency graph is:

    rsplug-adaptive-semaphore ─┐
    rsplug-dag ────────────────┤
    rsplug-file-specifier ─────┼──> rsplug-walker ───> rsplug
    rsplug-fts ────────────────┘

The first four leaf crates may be processed independently, but a release should
publish them deterministically and must wait until each is resolvable from the
registry before publishing its dependent.

## Decisions

1. All six publishable crates use one release version:
   `rsplug-adaptive-semaphore`, `rsplug-dag`, `rsplug-file-specifier`,
   `rsplug-fts`, `rsplug-walker`, and `rsplug`.
2. Version preparation happens before tagging and is committed together with
   the resulting `Cargo.lock`. The release workflow never calls a mutating
   version command.
3. Release tags use `v<SemVer>` without build metadata. Stable and prerelease
   forms such as `v0.4.0` and `v0.4.0-rc.1` are supported. Short or arbitrary
   `v*` names are rejected before any privileged job.
4. A release tag is annotated, points to a commit reachable from the protected
   default branch, and is never moved or reused. Signature verification is a
   separate policy until the trusted signing keys are explicitly configured.
5. The source commit, not `Cargo.lock` lookup behavior, is authoritative for
   package versions. Version discovery uses `cargo metadata`.
6. Uploading to crates.io remains sequential by dependency level. Fixed sleeps
   are replaced with bounded exponential backoff against the registry/index.
7. An already-published crate may be skipped only after its downloaded package
   checksum matches the locally packaged crate. “The version exists” alone is
   insufficient.
8. GitHub Release assets are staged on a draft first. The draft becomes public
   only after all crate publications succeed.
9. Release runs are globally serialized and never automatically canceled.
10. Actions receive least privilege and are pinned to reviewed full commit
    SHAs. The human-readable upstream version remains in a comment for
    Dependabot review.

## Release invariants

These conditions are mandatory:

- `v<VERSION>` equals every publishable package version and every internal
  registry version requirement after stripping the leading `v`.
- Relevant package entries in `Cargo.lock` have `VERSION`; non-rsplug
  dependency changes are not introduced accidentally by version preparation.
- A clean checkout of the tag passes all checks with `--locked`; no workflow
  step mutates tracked files.
- Every built binary prints `VERSION`, and every release archive contains only
  the expected executable plus deliberately included license/readme files.
- Release asset names, `SHA256SUMS`, attestations, GitHub Release metadata, and
  crate packages are derived from the same tag SHA.
- No public GitHub Release is created while a required build, package
  verification, or crate publication is incomplete or failed.
- Publishing can be retried after any failure without moving the tag,
  overwriting an existing crate, or silently accepting different bytes.
- No cancellation can interrupt a release after an irreversible upload.
- Stable tags create stable releases. A SemVer prerelease suffix creates a
  GitHub prerelease and never becomes `latest` automatically.

## R0: characterize the release contract

### Implementation

1. Add script fixtures under `scripts/tests/` using temporary repository copies;
   tests must not modify the working tree or contact GitHub/crates.io.
2. Capture the six-package list and dependency levels in one repository-owned
   data source used by preparation, verification, and publication. Avoid three
   independent shell lists that can drift.
3. Add a command that emits machine-readable release metadata containing tag,
   version, prerelease status, package names/versions, dependency level, and
   expected asset names.
4. Add regression fixtures for the historical mismatches: manifest changed but
   lock stale, `fts` left at the old version, `walker` special-cased, and tag
   version different from all manifests.
5. Record the current successful and failed workflow run URLs in test comments
   or the decision log only where they explain a regression; tests themselves
   must remain local and deterministic.

### Acceptance

- One command lists exactly the six publishable packages and the graph above.
- Fixtures fail with messages naming the mismatched file, package, expected
  version, and actual version.
- No test depends on the current checked-in release number.

## R1: replace tag-time mutation with release preparation

### Target files

- `scripts/set-version.sh`;
- new `scripts/prepare-release.sh` and `scripts/check-release.sh`, or equivalent
  clearly separated mutating/non-mutating entry points;
- root and crate manifests, including the vendored `fts` metadata and examples;
- `Cargo.lock`.

### Implementation

1. Make `prepare-release <version>` accept a version without a leading `v`.
   Validate the complete supported SemVer grammar before editing. Reject build
   metadata, leading zeroes, whitespace, shell metacharacters, and partial
   versions.
2. Update the root workspace version, all five workspace dependency
   requirements, `crates/walker/Cargo.toml`, the vendored `fts` manifest and
   `Cargo.toml.orig`, and exact-version examples in its README/NOTICE.
3. Remove the historical `0.1.0` walker exception. Historical tags remain
   unchanged; future preparation has one uniform rule.
4. Replace broad “first version line” substitutions with explicitly scoped
   edits. Each expected field must be changed or confirmed exactly once; zero or
   multiple matches are an error.
5. Refresh only workspace package entries in `Cargo.lock`. Snapshot the
   non-rsplug package name/version/source/checksum tuples before and after and
   fail if preparation unexpectedly upgrades an external dependency.
6. Run the non-mutating checker after edits. On failure, return non-zero and
   identify all mismatches. Do not hide a partial edit, and document that the
   caller should review/revert the working-tree diff.
7. Make preparation idempotent: a second invocation with the same version
   produces no diff and succeeds.
8. Keep `set-version.sh` only as a short compatibility wrapper if another
   repository entry point still uses it; otherwise remove it after all callers
   and documentation migrate.

### Acceptance

- Tests cover stable, prerelease, already-current, leading-`v`, malformed,
  injection-like, walker, `fts`, and stale-lock inputs.
- `prepare-release 0.4.0 && prepare-release 0.4.0` leaves the second invocation
  with no diff.
- `scripts/check-release.sh 0.4.0` verifies manifests, internal requirements,
  `Cargo.lock`, metadata, and documentation examples without changing files.
- The intended preparation diff contains version-only changes and no external
  dependency upgrade.

## R2: add a read-only preflight gate

### Job order

The workflow graph becomes:

    preflight
      -> test-and-package
      -> build matrix
      -> assemble-and-attest
      -> stage draft release
      -> publish crates
      -> publish GitHub Release

No job after `preflight` may reconstruct or rewrite the version.

### Implementation

1. Keep the tag trigger broad enough for GitHub to start the workflow, then
   reject anything outside the supported `vX.Y.Z[-prerelease]` grammar in
   `preflight`. This gives invalid tags a visible, safe failure.
2. Checkout full tag/default-branch history. Verify:
   - the ref is an annotated tag;
   - the peeled commit equals the workflow commit;
   - the commit is reachable from the fetched default branch;
   - the tag version equals `cargo metadata` and the release checker output.
3. Run `cargo metadata --locked`, then assert `git diff --exit-code` and no
   untracked files. Repeat the clean-tree assertion after every preflight tool
   that could resolve dependencies.
4. Run the normal quality gates before privileged jobs:

       cargo fmt --all -- --check
       cargo test --workspace --locked
       cargo clippy --workspace --all-targets --locked -- -D warnings

   Never run `cargo check -q`.
5. Package leaf crates with Cargo verification. For dependent crates whose new
   registry dependencies cannot be verified before the leaves exist, create and
   inspect their `.crate` archives without upload, validate normalized manifests
   and file lists, and rely on verified `cargo publish` after dependencies are
   visible. Document any preflight-only `--no-verify`; it must never appear on
   an uploading `cargo publish`.
6. Validate the workflow and scripts statically with pinned versions of
   `actionlint` and ShellCheck, or equivalent repository-controlled checks.

### Acceptance

- Invalid/ref-moved/non-main/unannotated/stale-lock/mismatched tags fail before
  OIDC or `contents: write` is available.
- Preflight leaves the checkout byte-for-byte clean.
- A commit that has not passed tests/package inspection cannot reach a publish
  job.

## R3: build one traceable artifact set

### Implementation

1. Build all target binaries using the pinned `rust-toolchain.toml` and:

       cargo build --release --locked --target <target> -p rsplug

2. Execute each native binary with `--version` and compare it with release
   metadata. For a binary that cannot run on its build host, add a format-aware
   inspection or arrange a native runner; do not silently omit the check.
3. Package from a clean staging directory. Include the version in archive names
   and use one documented layout on every platform.
4. Normalize archive metadata where supported: sorted entries, fixed owner and
   group, commit-derived timestamp, gzip without a current-time header, and ZIP
   extra-field stripping. Record unavoidable platform variance.
5. Upload one uniquely named workflow artifact per target. A later aggregation
   job downloads all of them, rejects duplicate/missing target names, generates
   `SHA256SUMS`, and verifies every listed checksum before continuing.
6. Generate GitHub build-provenance attestations for the final archives and
   checksum file. Attestations refer to the tag SHA and release workflow.
7. Pin fixed runner images where GitHub offers a supported label; document any
   target that must temporarily remain on a moving image.

### Acceptance

- The matrix produces exactly one archive for every declared target.
- Extracting every archive yields the expected executable and its version
  matches the tag.
- `SHA256SUMS` covers all and only public assets and verifies successfully.
- Rebuilding the same tag on the same runner image produces identical archives;
  any cross-image difference is documented before release.

## R4: stage an idempotent draft release

### Implementation

1. After all builds and attestations succeed, create or reuse a draft release
   for the exact tag. Upload the complete asset set and generated notes while it
   remains non-public.
2. If a draft already exists, compare each remote asset checksum before
   skipping it. Never overwrite different bytes under the same name.
3. Derive `prerelease` and “latest” behavior from parsed SemVer rather than a
   manually duplicated flag.
4. Fail on an existing public release during staging. Published releases are
   immutable and are never converted back to drafts or repaired in place.
5. Prefer the GitHub CLI already present on hosted runners for simple release
   operations; any action retained for release upload must be pinned to a full
   reviewed SHA.

### Acceptance

- Build/package failure creates no draft.
- A staging failure leaves at most a recoverable draft, never a public partial
  release.
- Re-running staging with identical assets is a no-op; differing assets fail.

## R5: publish crates safely in dependency order

### Implementation

1. Configure `publish-crates` with the crates.io trusted-publishing OIDC action,
   a protected `crates-io` environment, a finite timeout, and no long-lived
   registry token.
2. Remove `--allow-dirty` and uploading `--no-verify`. Use `--locked`, allow
   Cargo to package and build the normalized crate, and assert a clean tree
   before and after each package.
3. Publish levels in this order:
   - `rsplug-adaptive-semaphore`, `rsplug-dag`,
     `rsplug-file-specifier`, `rsplug-fts`;
   - `rsplug-walker`;
   - `rsplug`.
4. Before upload, query crates.io. Treat only a confirmed 404 as absent; fail on
   authentication, rate-limit, server, malformed-response, or network errors.
5. If a version exists, download its `.crate`, verify the registry checksum,
   and compare it with the local package bytes. Skip only an exact match.
6. After publishing each level, poll until every required name/version is
   resolvable from the registry/index. Use bounded exponential backoff with
   jitter, a clear timeout, and diagnostic status. Remove the fixed ten-second
   sleep.
7. Before publishing a dependent, run its verified `cargo publish --dry-run
   --locked` now that its dependencies are visible, then perform the real
   publish.
8. Write a job summary listing each package as matched, published, waited,
   failed, or pending. Do not expose credentials or OIDC material.

### Acceptance

- A clean new release publishes all six crates in graph order.
- An exact rerun safely skips all six; an existing same version with different
  bytes stops the release.
- Registry delay, 404, 429, 5xx, timeout, and malformed-response fixtures have
  distinct bounded behavior.
- Failure after any leaf upload leaves the GitHub Release in draft and a rerun
  continues from the matching published crates.

## R6: publish and harden the GitHub Release

### Implementation

1. The final job needs both successful crate publication and the staged draft.
   It rechecks tag SHA, draft asset checksums, and prerelease status, then
   publishes the draft.
2. Set workflow-level permissions to `contents: read`. Grant only:
   - `id-token: write` to crates.io authentication;
   - `attestations: write` and `id-token: write` to provenance generation;
   - `contents: write` to draft staging/finalization.
3. Pin every `uses:` reference to a full commit SHA, including first-party
   actions. Enable Dependabot updates for Actions so SHA changes arrive as
   reviewable PRs.
4. Use one repository-wide release concurrency group with
   `cancel-in-progress: false`. Do not use a tag-specific group because two
   different versions must not publish shared crates concurrently.
5. Set job timeouts and artifact retention deliberately. Ensure logs and job
   summaries contain no tokens, signed URLs, or package authorization data.
6. Update README maintainer documentation with preparation, review, tagging,
   monitoring, retry, and failure-recovery commands.

### Acceptance

- `github-release` cannot start before `publish-crates` succeeds.
- Stable and prerelease fixture tags create the correct draft/final metadata.
- The workflow contains no moving action tag, broad default write permission,
  tag-time source mutation, fixed registry sleep, `--allow-dirty`, or uploading
  `--no-verify`.
- A second release waits rather than canceling or overlapping the first.

## Repository and registry settings

These settings cannot be proven by repository tests. Record the final values in
maintainer documentation and verify them before the first real release:

- Enable GitHub Release immutability for future releases.
- Add a tag ruleset for `v*` that restricts creation, blocks update/deletion,
  and limits bypass. Require the tagged commit to be on the protected default
  branch with required checks satisfied where GitHub supports that policy.
- Configure the `crates-io` environment with selected-tag deployment rules and,
  if desired, a required reviewer/prevent-self-review rule. Disable
  administrator bypass if the project wants a strict two-person release.
- Make the crates.io trusted publisher match the exact repository, workflow
  filename, and GitHub environment.
- Set default `GITHUB_TOKEN` permissions to read-only and allow write
  permissions only in declared jobs.
- Restrict allowed Actions to GitHub-owned and explicitly approved pinned
  actions where repository policy permits.

Existing published releases are not retroactively immutable. Do not move or
repair their tags as part of this plan; document the historical mismatch.

## Failure recovery and rollback

crates.io publication is irreversible. “Rollback” means stopping public
finalization, fixing forward, and resuming safely:

- Before any crate upload: delete or reuse the draft after checksum validation;
  rerun the same immutable tag.
- After some crates upload: keep the release draft, do not move the tag, fix
  only workflow/repository-external transient causes, and rerun. Exact published
  bytes are verified and skipped.
- If published bytes differ from the tagged source: stop permanently for that
  version, yank affected crates if appropriate, document the incident, and
  prepare a new patch version. Never overwrite or repoint the old tag.
- After all crates publish but draft finalization fails: rerun finalization
  after rechecking tag and assets.
- After a public immutable release: any code or asset correction requires a new
  version.

Workflow bugs that require a repository commit cannot be repaired by moving an
already-triggered tag. Prepare a new version after the fix, unless the failure
occurred before all irreversible actions and the existing tag's workflow can be
safely rerun without changing its source.

## Validation matrix

Run focused script tests during R0-R2, then the complete local matrix after each
workflow milestone:

    bash -n scripts/*.sh scripts/tests/*.sh
    scripts/tests/release.sh
    cargo fmt --all -- --check
    cargo test --workspace --locked
    cargo clippy --workspace --all-targets --locked -- -D warnings
    actionlint .github/workflows/release.yml

Never run `cargo check -q`.

| scenario | expected result |
| --- | --- |
| stable `vX.Y.Z` | stable metadata; all versions match |
| prerelease `vX.Y.Z-rc.1` | prerelease metadata; not latest |
| `v1`, `vfoo`, leading-zero, `+build` | preflight rejection |
| tag/manifests mismatch | preflight rejection, clean tree |
| stale internal lock entry | preparation/check failure |
| stale `fts` or walker requirement | preparation/check failure |
| unannotated or moved tag | preflight rejection |
| tagged commit not on default branch | preflight rejection |
| one matrix build fails | no draft, no crate upload |
| archive contains wrong-version binary | aggregation failure |
| duplicate/missing asset | aggregation failure |
| draft upload fails | no crate upload |
| leaf crate already exists, same bytes | verified skip |
| leaf crate exists, different bytes | hard failure |
| crates API/index delayed | bounded retry, then continue/fail |
| failure after two leaf publications | draft remains; rerun resumes |
| walker/root verification fails | draft remains unpublished |
| all crates publish, finalization fails | safe finalization rerun |
| two releases start together | global serialization, no cancellation |
| rerun same completed tag | immutable public release is unchanged |

Use local HTTP/registry fixtures for API status, checksum, retry, and partial
publication behavior. Do not make ordinary tests depend on crates.io timing.
GitHub API behavior may be exercised with a temporary draft/prerelease tag only
in the explicitly approved final drill; delete only the temporary draft and tag
created for that drill, and never reuse its version.

## Progress

- [ ] R0 release contract and regression fixtures are implemented.
- [x] R1 preparation/check scripts update all six crates and lock data
      idempotently; local temporary-copy tests pass.
- [x] R2 tag preflight and quality/package gates are read-only and green in
      local script/YAML validation; hosted execution remains pending.
- [ ] R3 all platform artifacts, versions, checksums, and attestations validate.
- [ ] R4 draft staging is complete and idempotent.
- [ ] R5 crates publish in dependency order with checksum-safe resume.
- [ ] R6 final release dependency, permissions, SHA pins, and concurrency are
      complete.
- [ ] Maintainer documentation and external GitHub/crates.io settings are
      verified.
- [ ] The full validation matrix passes without publishing a production
      version.
- [ ] An explicitly approved temporary prerelease drill has passed, or its
      deferral and remaining risk are documented.

Implementation status: R1 is locally verified. R2-R6 repository changes are in
place, including the read-only preflight, build/checksum/attestation pipeline,
draft asset staging, dependency-ordered crate publication, finalization gate,
Action SHA pins, Dependabot configuration, and recovery checks. Hosted
workflow execution, an approved draft-tag drill, and the external GitHub and
crates.io settings remain unchecked until a maintainer performs them.

## Discoveries and decision log

- 2026-07-24: Historical tags `v0.1.1`, `v0.2.1`, and `v0.2.2` do not contain
  matching workspace versions. Tag-time rewriting cannot provide consistent
  GitHub source archives, direct Git/Nix builds, binaries, and crates.
- 2026-07-24: The failed `v0.2.4` publication showed that `cargo pkgid` can
  observe stale lock versions after manifest rewriting and that GitHub Release
  can currently succeed independently of crates.io.
- 2026-07-24: Chose one version for all six publishable crates, including the
  vendored `rsplug-fts`, because the root manifest already describes internal
  versions as matching and the unified policy is easier to verify and resume.
- 2026-07-24: Chose release-preparation commit then immutable annotated tag.
  The tag triggers automation but never authorizes source mutation.
- 2026-07-24: Chose draft-assets before crate upload and public finalization
  after crate success. This cannot make crates.io transactional, but it prevents
  a public GitHub Release from advertising a failed partial publication.
- 2026-07-24: The artifact smoke check exposed that the CLI did not expose a
  version flag. Added Clap's `version` metadata so every native release binary
  can assert its tag version before packaging.
- 2026-07-24: Existing performance recipes were removed after completion.
  Their two ambiguous expanded follow-ups are deferred and require a fresh plan
  if resumed.
- 2026-07-24: Hosted Ubuntu runs exposed two environment-sensitive release gates:
  Neovim must be installed explicitly for the workspace tests, and the M0
  reference copier must create each destination root before `fs::copy` because
  filesystem directory-entry order is not portable. Both are now covered by
  the workflow or test implementation.
