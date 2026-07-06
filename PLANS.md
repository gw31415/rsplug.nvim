# GitHub Fetch 高速化 ExecPlan

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This document follows the repository-level `PLANS.md` convention described in `AGENTS.md` (ExecPlans section).


## Purpose / Big Picture

rsplug.nvim is an external Rust binary that installs Neovim plugins by fetching Git repositories into a cache directory. When the repository is on GitHub and a token is available (`GITHUB_TOKEN` / `GH_TOKEN`), rsplug downloads a tarball from `codeload.github.com` instead of using the Git smart-HTTP protocol. This tarball path is the "GitHub mode" that this plan targets.

The problem: GitHub-mode fetch is slower than it needs to be. The user wants to push against the GitHub rate limit ceiling and use every available technique — connection pooling, higher parallelism, faster decompression libraries, and protocol-level optimizations — to minimize download time.

After this plan is fully implemented, the user will observe:

- Installing a fresh set of 30+ plugins completes noticeably faster than before (target: 2-4x improvement on cold install).
- Re-running `rsplug --update` with no upstream changes returns quickly because unchanged tarballs are skipped via conditional requests.
- The `--install` log shows many plugins fetching concurrently without errors, and no spurious rate-limit failures.

Success is demonstrated by a before/after timing comparison on a fixed plugin set, plus passing the existing test suite.


## Progress

- [x] (2025-07-07) Analyzed current fetch implementation across `plugin.rs`, `util.rs`, `main.rs`.
- [x] (2025-07-07) Identified six bottlenecks (per-call Client creation, two-stage gzip, pure-Rust decompressor, git-protocol rev resolution, no conditional requests, low initial parallelism).
- [x] (2025-07-07) Confirmed GitHub rate-limit landscape: codeload tarballs are CDN-served and outside the core API quota; git smart-HTTP and REST API are rate-limited but generous with a token.
- [x] (2025-07-07) Phase 1.1: Shared `reqwest::Client` with pool/HTTP/2 settings, threaded through `FetchCtx` and `Plugin::load`.
- [x] (2025-07-07) Phase 1.2: Eliminated temp-file staging; gzip+tar extraction in single `spawn_blocking` via `bytes::Bytes` → `Cursor`.
- [x] (2025-07-07) Phase 2.1: Replaced `async-compression` with `flate2` (zlib-ng backend). Removed `async-compression`, `futures-util`, `tokio-util` deps.
- [x] (2025-07-07) Phase 3.1: Added `github::resolve_rev_via_api` (REST API rev resolution with `X-RateLimit-Remaining` threshold and wildcard fallback). `resolve_remote_oid` dispatches API-first, git-protocol-fallback.
- [x] (2025-07-07) Phase 4.1: Raised fetch semaphore from 8→32 initial, 256→512 max.
- [x] (2025-07-07) `cargo fmt`, `cargo test --workspace` (140 tests, 0 failed), `cargo clippy --workspace --all-targets` (0 warnings) — all clean.
- [ ] Phase 5.1: Run before/after benchmarks on a fixed plugin set.


## Surprises & Discoveries

- Observation: `TarballFetch::download_and_extract` calls `reqwest::Client::new()` on every invocation (`util.rs` line ~595). This defeats connection pooling, keep-alive, and HTTP/2 multi-streaming entirely. Every plugin pays a fresh TCP + TLS handshake to `codeload.github.com`.
  Evidence: `crates/rsplug/src/rsplug/util.rs`, `download_and_extract` method.

- Observation: The gzip decompression pipeline writes the decompressed tar to a temp file (`tempfile::tempdir()`) before extracting in `spawn_blocking`. This doubles the disk I/O for no benefit.
  Evidence: `util.rs` lines ~619-650.

- Observation: `async-compression` is a pure-Rust gzip implementation. The C-based `zlib-ng` (via `flate2`) is typically 1.5-3x faster for decompression.
  Evidence: `Cargo.toml` dependency list; `flate2` feature flags documentation.

- Observation: The fetch semaphore starts at 8 concurrent operations (`main.rs` line ~121). GitHub codeload is a Fastly CDN with effectively unlimited throughput for tarballs, so this is overly conservative.
  Evidence: `crates/rsplug/src/main.rs`, `AdaptiveSemaphore::with_limits(8, 1, 256, ...)`.

- Observation: `ls_remote` uses git smart-HTTP (`/info/refs` ref advertisement), which transfers the full ref list and requires protocol negotiation. A single GitHub REST API call (`/repos/{owner}/{repo}/commits/{ref}`) returns just the SHA and is lighter on the wire.
  Evidence: `util.rs`, `git::ls_remote` function.


## Decision Log

- Decision: Prioritize Phase 1.1 (shared Client) and Phase 4.1 (semaphore raise) as the first implementation step.
  Rationale: These two changes are the smallest in effort and deliver the largest improvement (connection reuse eliminates repeated handshakes; higher parallelism multiplies throughput on a CDN that can absorb it). They are also the lowest risk because they do not change the download logic itself.
  Date: 2025-07-07. Author: planning session.

- Decision: Keep the GitFetch fallback path intact.
  Rationale: Tokenless environments and non-GitHub HTTPS URLs still need git-protocol fetch. TarballFetch already falls back to GitFetch on failure (`plugin.rs`, `materialize`), and this structure must be preserved so failures degrade gracefully.
  Date: 2025-07-07.

- Decision: Use `flate2` with the `zlib-ng` backend rather than `async-compression`.
  Rationale: gzip decompression is CPU-bound. Running it inside `spawn_blocking` with the C-backed `flate2::read::GzDecoder` outperforms the pure-Rust async decoder in raw throughput. The tarball is already fully downloaded before extraction, so async streaming is not required for correctness.
  Date: 2025-07-07.

- Decision: Make REST-API rev resolution fall back to git `ls_remote` when the API rate-limit header (`X-RateLimit-Remaining`) is low or when a wildcard revision (e.g. `@v*`) is requested.
  Rationale: Wildcard refs require enumerating tags, which the single-commit API endpoint does not support. Git protocol handles wildcards natively. Falling back on low rate-limit remaining prevents lockout.
  Date: 2025-07-07.

- Decision: Store HTTP conditional-request metadata (ETag, Last-Modified) in `repos/<repo>/worktrees/<snapshot_key>/.rsplug_http_meta.json`.
  Rationale: Colocating the metadata with the snapshot it describes keeps cleanup simple — deleting the snapshot directory removes the metadata automatically.
  Date: 2025-07-07.


## Outcomes & Retrospective

No implementation has started yet. This section will be updated after Phase 1 + Phase 4 are merged and benchmarked, and again at full completion.


## Context and Orientation

rsplug.nvim is a Rust workspace rooted at the repository top level. The main binary crate is `crates/rsplug`. It uses Tokio for async runtime, `git2` (libgit2 bindings) for Git operations, and `reqwest` for HTTP tarball downloads.

Key terms:

- **TarballFetch**: The GitHub-HTTPS + token download path. Downloads a `.tar.gz` from `codeload.github.com`, decompresses, extracts, and creates a git2-compatible working tree.
- **GitFetch**: The fallback path using git smart-HTTP via libgit2 into a bare `source.git` object store, then a local clone into a snapshot worktree.
- **AdaptiveSemaphore**: A custom concurrency limiter (`crates/adaptive_semaphore`) that adjusts its permit count based on throughput and error rate. It starts at an initial limit, halves on regressions, and increments on improvements.
- **codeload.github.com**: GitHub's CDN endpoint for archive/tarball downloads. It is outside the core REST API rate limit.
- **snapshot**: A fixed checkout of a repository at a specific commit, stored under `repos/<repo>/worktrees/<snapshot_key>/`.

Key files:

- `crates/rsplug/Cargo.toml` — dependencies. Currently has `reqwest`, `async-compression`, `tar`, `tokio-util`, `futures-util`.
- `crates/rsplug/src/main.rs` — entry point. Creates the `AdaptiveSemaphore` (line ~121) and spawns parallel `Plugin::load` tasks.
- `crates/rsplug/src/rsplug/entities/plugin.rs` — `Plugin::load` orchestrates fetch strategy. `FetchCtx` struct (line ~608) bundles fetch arguments. `materialize` (line ~662) selects TarballFetch vs GitFetch.
- `crates/rsplug/src/rsplug/util.rs` — `git` module (ls_remote, fetch, snapshot init), `github` module (URL/token/tarball helpers), `fetch` module (TarballFetch implementation).

GitHub rate-limit landscape:

- `codeload.github.com` tarball downloads: served by Fastly CDN, not counted against the core API quota. Effectively unlimited for rsplug's use case.
- git smart-HTTP (`/info/refs`): subject to GitHub's git-operation limits, but generous with a token.
- REST API (`api.github.com`): 5,000 requests/hour authenticated, 60/hour anonymous. The `X-RateLimit-Remaining` and `X-RateLimit-Reset` response headers report the current budget.


## Plan of Work

The implementation proceeds in phases ordered by impact-to-effort ratio. Each phase leaves the binary in a working state — the test suite must pass after every phase.

**Phase 1.1 — Shared reqwest::Client.** Instead of constructing `reqwest::Client::new()` inside `download_and_extract`, build one `Client` at application startup with tuned pool and HTTP/2 settings, and pass it down through `FetchCtx`. This makes the second and subsequent tarball downloads reuse warm connections and lets HTTP/2 multiplex multiple downloads over a single connection to `codeload.github.com`.

**Phase 1.2 — Eliminate temp-file staging.** Currently the gzip-decompressed stream is copied to a temp file before tar extraction. After switching to `flate2` (Phase 2.1), the `GzDecoder` can feed `tar::Archive` directly inside a single `spawn_blocking` call, removing the intermediate disk write entirely. This is bundled with Phase 2.1 implementation since both touch the same function.

**Phase 2.1 — flate2 with zlib-ng.** Replace the `async-compression` gzip decoder with `flate2::read::GzDecoder` using the `zlib-ng` C backend. Add `flate2 = { version = "1", features = ["zlib-ng"] }` to `Cargo.toml`. Rewrite `download_and_extract` to download the raw gzip bytes, then decompress and extract in one `spawn_blocking` pass. After this, `async-compression` can be removed from dependencies if no other code uses it.

**Phase 3.1 — REST-API rev resolution.** Add a `github::resolve_rev_via_api` function that calls `GET /repos/{owner}/{repo}/commits/{ref}` and reads `.sha` from the JSON response. For the no-revision case (default branch), call `GET /repos/{owner}/{repo}` and read `.default_branch`, then resolve that branch. Use this in `ls_remote` when the source is GitHub HTTPS and a token is present. Check the `X-RateLimit-Remaining` header; if it falls below a threshold (e.g. 50), fall back to git protocol. Wildcard revisions always use git protocol.

**Phase 3.2 — Conditional requests for update.** When `--update` re-fetches a repository that already has a snapshot, attach `If-None-Match` (ETag) or `If-Modified-Since` headers from the stored `.rsplug_http_meta.json`. If GitHub returns `304 Not Modified`, skip the download and reuse the existing snapshot. Store new ETag/Last-Modified values from each successful response.

**Phase 4.1 — Raise semaphore initial limit.** Change `AdaptiveSemaphore::with_limits(8, ...)` to `with_limits(32, 1, 512, ...)` in `main.rs`. The adaptive logic remains the safety net — if errors spike, it halves automatically.

**Phase 5.1 — Benchmark.** Measure cold-install and update times on a fixed plugin set before and after. Record results in the Outcomes section.


## Concrete Steps

Unless noted, all commands run from the repository root (`/Users/ama/.herdr/worktrees/rsplug.nvim/fast-dl`).

**Step 1 — Add flate2 dependency.**

Edit `crates/rsplug/Cargo.toml`: add `flate2 = { version = "1", features = ["zlib-ng"] }` to `[dependencies]`. Keep `async-compression` for now (remove in Step 3 after confirming nothing else uses it).

Expected: `cargo build -p rsplug` succeeds.

**Step 2 — Create shared Client.**

In `crates/rsplug/src/main.rs`, before the plugin-loading loop, construct a `reqwest::Client`:

    let http_client = reqwest::Client::builder()
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .pool_max_idle_per_host(64)
        .tcp_keepalive(std::time::Duration::from_secs(60))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| Error::Io(std::io::Error::other(e.to_string())))?;

Thread this `Client` (wrapped in `Clone`) through the `plugin.load(...)` call chain into `FetchCtx`. Add an `http_client: reqwest::Client` field to `FetchCtx` in `plugin.rs`.

Expected: `cargo build -p rsplug` succeeds. Existing tests pass.

**Step 3 — Rewrite download_and_extract.**

In `crates/rsplug/src/rsplug/util.rs`, rewrite `TarballFetch::download_and_extract` to:

1. Use the shared `Client` from `FetchCtx` (passed as a new parameter) instead of `Client::new()`.
2. Send the GET request with the `Authorization` header when a token is present.
3. Read the full response body into `bytes::Bytes` (add `bytes` to deps if not already transitively available — reqwest re-exports it).
4. In a single `spawn_blocking` call: wrap the bytes in `std::io::Cursor`, create `flate2::read::GzDecoder`, create `tar::Archive`, iterate entries, strip the top-level directory, and unpack into `dest`.
5. Remove the `tempfile::tempdir()` temp-file logic.

After confirming the build, check if `async-compression` is still used anywhere. If not, remove it from `Cargo.toml` and remove the `futures-util` / `tokio-util` stream imports if they become unused.

Expected: `cargo test -p rsplug` passes, including `init_snapshot_checks_out_commit_into_a_detached_worktree`. A manual install of a GitHub plugin succeeds and produces the same file tree as before.

**Step 4 — Raise semaphore limit.**

In `crates/rsplug/src/main.rs`, change the `AdaptiveSemaphore::with_limits` call from initial limit 8 to 32 and max from 256 to 512.

Expected: `cargo build -p rsplug` succeeds. No test changes needed.

**Step 5 — REST-API rev resolution (Phase 3.1).**

In `crates/rsplug/src/rsplug/util.rs`, inside the `github` module, add:

    pub async fn resolve_rev_via_api(
        client: &reqwest::Client,
        owner: &str,
        repo: &str,
        rev: Option<&str>,
        token: Option<&str>,
    ) -> Result<String, Error>

This function calls the appropriate endpoint, parses the JSON `sha` or `default_branch` field, and returns the commit hash string. It reads `X-RateLimit-Remaining` and returns a distinct error variant (or `Option`) when the budget is low so the caller can fall back to git protocol.

In `git::ls_remote`, detect GitHub HTTPS sources with a token. If so, try `resolve_rev_via_api` first; on rate-limit-low or wildcard rev, fall back to the existing git-protocol logic.

Expected: existing `ls_remote` tests pass. A manual `--update` on a GitHub plugin resolves the rev correctly.

**Step 6 — Conditional requests (Phase 3.2).**

In `TarballFetch::fetch_to_snapshot`, before downloading, check for `.rsplug_http_meta.json` in the snapshot directory (if it already exists for an update). Attach `If-None-Match` / `If-Modified-Since` headers. If the response is `304`, return early without downloading. After a successful `200`, write the new ETag and Last-Modified to the meta file.

Expected: `--update` with no upstream changes skips the download and logs a "not modified" message.

**Step 7 — Benchmark.**

    # Cold install (clear cache first)
    rm -rf ~/.cache/rsplug/repos
    time rsplug --install

    # Update (no changes)
    time rsplug --update

Record before/after numbers in the Outcomes section.

**Step 8 — Lint and test (after every step).**

    cargo fmt
    cargo test --workspace
    cargo clippy --workspace --all-targets

Note: `AGENTS.md` says do not run `cargo check -q`.


## Validation and Acceptance

Behavior-based criteria:

1. A fresh `rsplug --install` of a GitHub plugin (e.g. `nvim-lua/plenary.nvim`) produces a working snapshot directory with the correct files, identical to the pre-change output. Verified by `diff -r` between old and new snapshot directories.

2. `cargo test --workspace` passes, including:
   - `init_snapshot_checks_out_commit_into_a_detached_worktree`
   - `tarball_url_formats_correctly`
   - `supports_tarball_classifies_correctly`
   - `parse_github_url_extracts_owner_repo`
   - All `AdaptiveSemaphore` unit tests.

3. `cargo clippy --workspace --all-targets` produces no new warnings.

4. Cold-install benchmark shows measurable speedup over the pre-change baseline. Target: at least 2x faster on a 30-plugin cold install. Record the actual numbers.

5. `--update` with no upstream changes completes faster than before (conditional-request skip) and does not produce a `200` download in the debug log.

6. Tokenless fallback: with `GITHUB_TOKEN` and `GH_TOKEN` both unset, a GitHub plugin still installs correctly via the GitFetch fallback path.

Manual verification commands:

    # Verify a plugin snapshot is correct
    ls ~/.cache/rsplug/repos/github.com/nvim-lua/plenary.nvim/worktrees/

    # Verify the semaphore starts at 32 (add a debug log temporarily, or check via a unit test)
    # Verify shared client is used (debug log or strace for connection count)


## Idempotence and Recovery

- Every phase can be reverted independently via `git revert` because the binary remains functional after each step.
- If `flate2` with `zlib-ng` fails to compile on a target platform, fall back to the `miniz_oxide` backend: `flate2 = { version = "1", features = ["miniz_oxide"] }`. This is slower but pure-Rust and always available.
- If the REST-API rev resolution returns an unexpected response format, the code must fall back to git protocol, not error out. The `ls_remote` function must never regress for tokenless users.
- If a conditional request returns an unexpected status (not 200 or 304), treat it as a normal download — discard the stored metadata and proceed with the full tarball.
- If the shared `Client` construction fails (rare — only on misconfigured TLS), the application should fall back to constructing a default `Client::new()` per the old behavior rather than refusing to start.
- Clearing the cache (`rm -rf ~/.cache/rsplug/repos`) is always a safe recovery action — it forces a full re-download but never corrupts state.
- The benchmark step (`rm -rf ~/.cache/rsplug/repos`) is idempotent and safe to repeat.


## Artifacts and Notes

Baseline measurements (to be filled in before implementation):

    Cold install (N plugins): ___s (before) / ___s (after)
    Update no-change (N plugins): ___s (before) / ___s (after)

Current dependency versions relevant to this plan:

    reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "stream"] }
    async-compression = { version = "0.4", features = ["gzip", "tokio"] }
    tar = "0.4"

Planned additions:

    flate2 = { version = "1", features = ["zlib-ng"] }

The `reqwest` `stream` feature and `futures-util` may become unnecessary after Phase 1.2/2.1 if the response body is read via `response.bytes()` instead of `bytes_stream()`. Check and clean up after Step 3.


## Interfaces and Dependencies

Public/internal interfaces that must exist after this plan:

- `FetchCtx` struct (`plugin.rs`) gains an `http_client: reqwest::Client` field. All call sites in `Plugin::load`, `materialize`, and `ensure_source_git` must pass it through.
- `TarballFetch::fetch_to_snapshot` signature changes to accept `&reqwest::Client` (or receive it via an expanded `FetchCtx`).
- `github::resolve_rev_via_api` — new async function in the `github` module. Takes `&reqwest::Client`, owner, repo, optional rev, optional token. Returns `Result<String, Error>` where the error distinguishes rate-limit-exhausted from genuine failure.
- `git::ls_remote` — modified to try REST-API resolution first for GitHub HTTPS + token, falling back to git protocol. Signature unchanged externally.
- `main.rs` — constructs the shared `Client` and passes it into the load chain. Semaphore initial limit changes from 8 to 32.

External dependencies added: `flate2`. Dependencies potentially removed: `async-compression`, possibly `futures-util` and `tokio-util::io` if the streaming path is fully replaced.

No changes to the lockfile format, CLI arguments, or Lua runtime integration.


## Revision Notes

- 2025-07-07: Initial ExecPlan created from planning session. Converted from a conventional phase-table format to the OpenAI/Codex living-document ExecPlan structure. Content and technical decisions unchanged from the prior version; section organization and required living-document sections added.
