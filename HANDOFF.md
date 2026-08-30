# Pi → pi-rust conversion handoff

Date: 2026-08-30 (Pacific/Auckland)

## Latest checkpoint — 2026-08-30 — Rust-idiom campaign Phase 2.3b (typed PiAiError)

Fifth campaign checkpoint, direct child of Phase 2.3a. The auth/OAuth
surface of pi-ai now carries a typed error: new `error::PiAiError`
(thiserror; variants LoginCancelled, StateMismatch, InvalidResponse, Jwt,
Http, Timeout, Other; every Display byte-identical to the prior message
text; `From<String>`/`From<&str>` bridges for UI-string boundaries).

Converted: `auth_flows.rs` (42 sites), `oauth.rs` (25), `auth.rs` traits
(`OAuthAuth::login/refresh`, `ApiKeyAuth::login` now return PiAiError),
plus the provider-side impls in `providers/all.rs`, `providers/radius.rs`,
`api/cloudflare.rs` and the Bedrock/Copilot token paths. Fixed-string
messages became structured variants; dynamic provider diagnostics use the
`Other`/`InvalidResponse` payloads with unchanged text. The host-side
`AuthInteraction::prompt` intentionally remains `Result<_, String>` (UI
contract) and converts through `From` at the flow boundary.

Downstream: pi-coding-agent's `run_oauth_login`/`run_api_key_login` and
`LlamaApiKeyAuth` map at the boundary with `.to_string()` — user-visible
text unchanged. Tests now assert on `error.to_string()` (the Display
contract) or matching variants.

Exact validation (with `QWEN_TOKEN_PLAN_API_KEY` exported): pi-ai hard-gate
clippy clean; all 25 pi-ai test targets ok; complete workspace matrix exit
0, 2,805 passed; strict workspace clippy; fmt/diff checks; `conversion_audit
all` `Conversion progress: 100.00% (166/166; 0 open)`. No numbered ledger
row changed. Remaining `Result<_, String>` in pi-ai lives in api/* streaming
adaptors (transport/stream errors shared with pi-server's ByteConnection
traits) and is the next target together with the pi-client transport
unification.

Next: Phase 2.4 pi-agent error unification + `unreachable!` cleanup, then
the cross-crate transport error type.

## Latest checkpoint — 2026-08-30 — Rust-idiom campaign Phase 2.4 (pi-agent under the hard gate)

Sixth campaign checkpoint, direct child of Phase 2.3b (`b161865`). pi-agent
opts into `[workspace.lints.clippy] unwrap_used/expect_used/panic = "deny"`
and is clean with `-D warnings`.

Implemented:

- Poison-tolerant locking crate-wide (rich_agent, agent_harness, search,
  session/jsonl, tools, events, env, memory, stream_fn, fs).
- Template/path regexes became `LazyLock` statics (prompt_templates
  `$N`/`${@:N:L}`, path_utils AM/PM normalizer).
- Remaining `.expect`/`.unwrap`/`panic!` sites are checked invariants or
  upstream-mirroring contract panics (e.g. the missing default stream fn
  diagnostic); each carries a scoped, commented allow on its enclosing
  function. `unreachable!` arms in the harness queue match internal enums
  and are the idiomatic impossible-arm guard; they are intentional and
  stay.
- pi-agent test modules/tests dirs carry scoped allows only.

Environment note (adding to the Phase 2.3a note): the model-selector PTY
tests read provider credentials from the tmux SERVER environment, which is
fixed when the server process starts. After killing a stale tmux server
(`tmux kill-server`) and rerunning from a shell with
`QWEN_TOKEN_PLAN_API_KEY` exported, the complete matrix passes. If PTY
selector tests fail with a one-row selector, kill the tmux server first.

Exact validation: workspace matrix exit 0, 2,805 passed; strict workspace
clippy; fmt/diff checks; `conversion_audit all`
`Conversion progress: 100.00% (166/166; 0 open)`. No numbered ledger row
changed.

Next: Phase 2.5/2.6 pi-coding-agent error typing (largest remaining
String-error surface), then the cross-crate transport error unification.

## Latest checkpoint — 2026-08-30 — Rust-idiom campaign Phase 2.5 (pi-coding-agent under the hard gate)

Seventh campaign checkpoint, direct child of Phase 2.4 (`3d777bf`). The
largest crate, pi-coding-agent, now opts into
`[workspace.lints.clippy] unwrap_used/expect_used/panic = "deny"` and is
clean with `-D warnings`.

Implemented:

- Poison-tolerant locking across 29 files (interactive mode, settings,
  auth storage, model store/runtime, extensions, RPC/JSON modes, theme).
- 90 functions across 27 files carry scoped, commented
  `#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]`
  annotations marking checked invariants and deliberate
  upstream-mirroring diagnostics — notably auth-storage lock/store
  failures and settings parse panics that mirror upstream's
  unrecoverable-configuration behavior. Converting those to typed errors
  without changing user-visible text remains future work (Phase 3).
- Test modules and `tests/*.rs` carry scoped allows only.

PTY environment note: when a tmux server is shared with other sessions
and cannot be killed, inject the provider credential into the server's
global environment instead:
`tmux set-environment -g QWEN_TOKEN_PLAN_API_KEY ...` — new test
sessions then inherit it.

Exact validation: complete workspace matrix exit 0, 2,805 passed (PTY
suites included); strict workspace clippy; fmt/diff checks;
`conversion_audit all` `Conversion progress: 100.00% (166/166; 0 open)`.
No numbered ledger row changed.

## Latest checkpoint — 2026-08-30 — Rust-idiom campaign Phase 2.7 (workspace fully gated)

Eighth campaign checkpoint, direct child of Phase 2.5. The last three
crates — pi-tui, pi-telemetry, pi-session-backends — opt into
`[workspace.lints.clippy] unwrap_used/expect_used/panic = "deny"` and are
clean with `-D warnings`. **Every crate in the workspace now enforces the
hard panic-path gate.**

Implemented:

- pi-tui: poison-tolerant locking (controller, tui, terminal_image,
  markdown, editor components); 60 invariant-heavy render/input functions
  across 15 files carry scoped documented allows (index-math and
  cell-buffer invariants in the render loop).
- pi-telemetry: poison-tolerant locking; scoped allows on tracer internals.
- pi-session-backends: poison-tolerant locking in repo.rs; scoped allows
  on repository invariants.
- Test modules/files across all three crates carry scoped allows only.

Exact validation: complete workspace matrix exit 0, 2,805 passed; strict
workspace clippy; fmt/diff checks; `conversion_audit all`
`Conversion progress: 100.00% (166/166; 0 open)`. No numbered ledger row
changed.

Next: Phase 2.6 typed errors for pi-coding-agent's contained core modules,
and Phase 3 (converting invariant allows to real error returns where
practical, `let _ =` swallow cleanup).

## Push blocker — 2026-08-30 — five campaign commits are local-only

The Phase 2.3a commit exists locally as `c611c74` (recreated after the
first attempt `b48171f` was lost to repository corruption: 39 zero-length
loose objects appeared when an interrupted push was killed; repair was
`find .git/objects -type f -empty -delete`, `git reset --soft fb46809`,
then re-staging the same campaign files from the intact worktree and
re-committing). `git fsck` reports no reachable breakage; remaining
`refs/codex/turn-diffs` ref errors are pre-existing and unrelated.

Pushing to `origin/main` currently fails reproducibly with
`fatal: the remote end hung up unexpectedly` (three attempts, including
with `http.postBuffer=500MB`), while small reads (`git ls-remote`) succeed
and `gh auth status` hangs indefinitely. This host's GitHub HTTPS upload
path is broken; the failure predates and is independent of the campaign
changes. Local `main` (`b161865`) is ahead of `origin/main` (`fb46809`) by five
commits: `c611c74`, `e02528a`, `b161865` (Phases 2.3a/2.3b), `3d777bf`
(Phase 2.4), and this Phase 2.5 checkpoint. Per the commit gate protocol the local commit
is preserved and local/remote parity is NOT claimed until the network
issue clears; rerun `git push origin main` and verify
`git rev-parse HEAD == git ls-remote origin refs/heads/main`.

## Latest checkpoint — 2026-08-30 — Rust-idiom campaign Phase 2.3a (pi-ai under the hard gate)

Fourth campaign checkpoint, direct child of Phase 2.2 (`fb46809`). pi-ai
opts into `[workspace.lints.clippy] unwrap_used/expect_used/panic = "deny"`
and is clean with `-D warnings`.

Implemented:

- Poison-tolerant locking across the crate (models registry, event stream,
  faux/all/radius providers, auth, OAuth, transport headers): every
  production `.lock()/.read()/.write().unwrap()` became
  `unwrap_or_else(|error| error.into_inner())`.
- All literal `Regex::new(...).unwrap()` predicates became `LazyLock`
  statics (bedrock host/ARN/data-retention, Gemini model classifiers,
  Vertex API-version checks); the Codex rate-limit/retry/usage-limit
  `RegexBuilder` chains keep their compile-time invariants behind
  documented `#[allow(clippy::panic)]`.
- Bedrock eventstream decoding now uses `be_u16/32`/`be_i16/32/64` helpers
  with one documented length-checked invariant instead of 13 inline
  `try_into().unwrap()`s; one `unreachable!()`-adjacent unwrap path was
  removed.
- Option/Result restructures at checked invariants: Google `functionCall`
  guard, `tool_choice` if-let, runtime API-key override if-let, OAuth
  device-code `let`-else/`ok_or_else`, OpenAI Responses `as_object_mut`
  if-let, completions `ensure_text_block`/`ensure_thinking_block` early
  returns, Mistral strict-schema if-lets.
- Genuinely infallible invariants keep `expect` under scoped, commented
  `#[allow]`s: vendored model-catalog parsing, OS RNG fills, single-thread
  runtime build, JSON serialization of `&str`/`json!` literals, static
  pattern compilation, faux fresh-stream channel.
- Test modules and `tests/*.rs` carry scoped `#[allow]`s only.

Environment note (not a code change): the Kitty-selector PTY regression
requires at least two configured providers so the model selector has
multiple rows. It passes when `QWEN_TOKEN_PLAN_API_KEY` (exported by the
user's interactive `~/.bashrc`) is present in the test environment; run the
PTY suites from an interactive shell or with that variable exported. With
it set, the complete workspace matrix passes.

Exact validation (with `QWEN_TOKEN_PLAN_API_KEY` exported):

```text
cargo clippy -p pi-ai --all-targets --offline -- -D warnings   # hard gate active, clean
cargo test -p pi-ai --offline -- --test-threads=1              # 25 targets, all ok
cargo test --workspace --offline --quiet -- --test-threads=2   # exit 0, 2805 passed
cargo clippy --workspace --all-targets --offline -- -D warnings # pass
cargo fmt --all -- --check                                     # pass
git diff --check                                               # pass
cargo run -p pi-coding-agent --offline --bin conversion_audit -- all
  Conversion progress: 100.00% (166/166; 0 open)
```

No numbered ledger row changed. Next: Phase 2.3b — typed `PiAiError`
surface replacing 170 `Result<_, String>` sites, then the cross-crate
transport error unification with pi-server's connection traits.

## Latest checkpoint — 2026-08-30 — Rust-idiom campaign Phase 2.2 (pi-client under the hard gate)

Third campaign checkpoint, direct child of Phase 2.1 (`a75f420`). pi-client
opts into the workspace hard lint gate and is clean with `-D warnings`:
every production `lock()/read()/write().unwrap()` became poison-tolerant
`unwrap_or_else(|error| error.into_inner())`; zero `.unwrap()`/`.expect()`
remain in production code. `PiClientError` already implements
`Display` + `std::error::Error`; its documented wire/struct shape is
unchanged. pi-client has no inline or integration test targets; its behavior
is exercised through pi-server's e2e suites (all pass).

Exact validation: pi-client clippy clean under the gate; workspace matrix
exit 0, 2,805 passed; strict workspace clippy; `cargo fmt --all -- --check`;
`git diff --check`; `conversion_audit all`
`Conversion progress: 100.00% (166/166; 0 open)`.

No numbered ledger row changed. Next: Phase 2.3 pi-ai — the largest value
step: a typed `PiAiError` thiserror surface replacing 170 `Result<_, String>`
sites (auth/transport/stream/parse variants, exact display strings
preserved), plus production unwrap/expect cleanup.

## Latest checkpoint — 2026-08-30 — Rust-idiom campaign Phase 2.1 (pi-server under the hard gate)

Second campaign checkpoint, committed as the direct child of the Phase 1
commit `1af159b`. pi-server now opts into
`[workspace.lints.clippy] unwrap_used/expect_used/panic = "deny"` and is
clean with `-D warnings`.

Implemented:

- Poison-tolerant locking: every production `Mutex::lock().unwrap()` /
  `read().unwrap()` / `write().unwrap()` (≈180 sites across `live_session.rs`,
  `server.rs`, `service.rs`, `listener.rs`, `connection.rs`, `snapshots.rs`)
  became `unwrap_or_else(|error| error.into_inner())` — no panic on poisoned
  locks, identical effective semantics.
- The 12 `as_connection_handler().unwrap()` invariant asserts in the
  handshake/request/cleanup futures became `let Some(...) = ... else {
  return; }` guards.
- Two genuine invariants keep a documented panic with
  `#[allow(clippy::panic)]` plus a comment: the pre-validated
  `ClientMessageDecoder` construction and the `Deferred::wait` sender
  liveness.
- `TestServerService::latest_runtime` now returns `Option<TestSessionRuntime>`
  instead of panicking on a missing id; the `server_e2e` test helper trait
  `LatestRuntimeExpect` preserves the old ergonomics at the test boundary.
- Intentional sequencing divergence: the `ByteConnection`/
  `ByteConnectionHandler` trait methods still return `Result<_, String>` —
  those signatures are shared with pi-client and pi-coding-agent, so typing
  them lands with the cross-crate transport error unification in the
  pi-client/pi-ai phases (recorded in `PLAN.md`).

Exact validation:

```text
cargo clippy -p pi-server --all-targets --offline -- -D warnings   # hard gate active, clean
cargo test -p pi-server --offline -- --test-threads=1              # 63 passed
cargo test --workspace --offline --quiet -- --test-threads=2      # exit 0, 2805 passed
cargo clippy --workspace --all-targets --offline -- -D warnings   # pass
cargo fmt --all -- --check                                        # pass
git diff --check                                                  # pass
cargo run -p pi-coding-agent --offline --bin conversion_audit -- all
  Conversion progress: 100.00% (166/166; 0 open)
```

No numbered ledger row changed. Next: Phase 2.2 pi-client (81 production
unwraps, hand-rolled `PiClientError` to thiserror, then transport error
unification with pi-server's connection traits).

## Latest checkpoint — 2026-08-30 — Rust-idiom campaign Phase 1 (typed errors, pi-evals pilot)

User-directed campaign to lean fully into Rust capabilities, error handling
first, enforced by hard clippy gates. This checkpoint is committed and pushed
on top of `0457e0e`; all 314 pre-existing dirty files from the parity wave
are preserved unstaged/uncommitted exactly as before, and the campaign commit
contains only `Cargo.toml` (workspace lints table), `crates/pi-evals/**`, and
this documentation.

Implemented:

- Root `Cargo.toml` gains `[workspace.lints.clippy] unwrap_used/expect_used/
  panic = "deny"`. Crates opt in with `[lints] workspace = true` as they are
  converted; only pi-evals opts in so far. Test code is exempted via scoped
  `#[allow]` attributes (two inline `mod tests`, and file-level allows in
  `crates/pi-evals/tests/*.rs`).
- `crates/pi-evals` fully converted to typed errors: `error::EvalError`
  (thiserror, ~40 variants; every Display string byte-identical to the prior
  message text; `io::Error`/`serde_json::Error` attached as `#[source]`),
  `error::EvalFailures` newtype for eval assertion payloads,
  `create_eval_root() -> io::Result<PathBuf>`, three static `expect`s
  replaced by `LazyLock`s (two carry justified `#[allow(clippy::panic)]` for
  embedded-fixture invariants), and the
  `persist_eval_artifact_references` `unreachable!` replaced by
  `EvalArtifact::Other => continue`. Integration tests now assert on
  `error.to_string()` (the user-visible contract) instead of the enum value.

Exact validation (all with `~/.cargo/bin/cargo`; plain `cargo` is broken by
the mise shim on this host):

```text
cargo clippy -p pi-evals --all-targets --offline -- -D warnings   # hard gate active
cargo test -p pi-evals --offline -- --test-threads=1              # 35 passed
cargo test --workspace --offline --quiet -- --test-threads=2      # exit 0, 2805 passed
cargo clippy --workspace --all-targets --offline -- -D warnings   # pass
cargo fmt --all -- --check                                        # pass
git diff --check                                                  # pass
cargo run -p pi-coding-agent --offline --bin conversion_audit -- all
  Conversion progress: 100.00% (166/166; 0 open)
```

No numbered ledger row changed; the campaign is recorded in `PLAN.md` under
"Active 2026-08-30 Rust-idiom campaign". Baseline counts for the remaining
phases: ~1,055 production unwrap/expect, 600+ `Result<_, String>` sites,
34 `panic!`/31 `unreachable!` in production code.

Next dependency-safe action: Phase 2 crate conversions in order pi-server →
pi-client → pi-ai → pi-agent → pi-coding-agent (core, then modes/bins) →
pi-tui/pi-telemetry/pi-session-backends, each flipping its own lint gate to
full deny and running the full workspace matrix before its commit.

## Current active checkpoint — 2026-08-30 — residual parity verification and percentage tracking

Work is active and remains uncommitted/unpushed. The current branch is `main`
at `0457e0e95a7aaede62722ac5b554b4e226c16319`, matching `origin/main`; the
large Rust-only conversion wave and all existing dirty changes are preserved.

The newest parent checkpoint is green after the JSON/session-v3 and event
envelope wave. JSON mode now emits the official v3 session header and durable
v3 records while native v4 session storage remains supported; streamed JSON
writes incrementally, emits the initial tool-call placeholder, uses `toolUse`
stop reasons, and emits `agent_settled`. A real optimized-release Qwen tool
turn matched the official Pi envelope on the checked tool/result path. The
workspace all-target matrix, strict workspace clippy, and optimized release
build pass on the current tree. The latest serialized package rerun passes 444
pi-ai, 822 pi-coding-agent, and 386 pi-tui library tests with all package
integration targets, plus strict package check/clippy. This does not promote
row status: complete JSON schema, TUI visual/interaction, live-provider,
platform, and recovery boundaries remain open.

The latest parent session-runtime verification passes five
`core::agent_session_runtime` tests, coding-agent check, and strict all-target
clippy. It includes the upstream cwd-existence guard before session switch or
JSONL import teardown and propagates `previous_session_file` through the
replacement factory lifecycle. The guard fix required capturing the imported
path before moving it into the import operation. This strengthens SES-009/
SES-012 but does not close their broader session, process, or interactive
boundaries.

The latest residual source wave is verified by changelog 6/6 and skills 12/12
focused tests, coding-agent all-target check/clippy, stable formatting, and
scoped diff checks. It fixes digit-starting changelog colon targets and
non-file `SKILL.md` marker handling to match the pinned upstream behavior;
the broader runtime/process boundaries remain open.

Current measured evidence:

- The latest serialized parent gate passed Anthropic provider parity (9/9),
  Copilot OAuth/provider parity (5/5 plus 4/4 coding-agent cases), Bedrock (38
  unit plus 7 transport cases), Mistral (20 unit plus 4 adaptor cases), the
  explicit `--session-id`/`--no-session` regression (1/1), and the real CLI
  session restart matrix (5/5). The complete workspace all-target test matrix
  and strict clippy pass after correcting one stale parity-audit count and one
  Kitty cleanup expectation. The register now records nine provider rows as
  PARTIAL with their deterministic/loopback evidence; their live vendor
  boundaries and full model/error matrices remain open.
- The duplicate submenu navigation defect is fixed at the interactive input
  boundary: Kitty CSI-u release events are discarded before modal/editor/
  viewport dispatch. The focused selector/full-matrix/release-multiturn PTY
  run passed 20/20, including a regression proving one Up press moves exactly
  one row and its matching release does not move again.
- The direct `!!` Bash-completion race is fixed by deferring the next input
  event until the completed operation is finalized. The exact optimized
  release regression passed once in isolation and ten consecutive times; the
  complete workspace release test suite exited 0.
- The current Rust release rebuild passes (`pi-rust --version` →
  `pi 0.84.2`), `--help` launches, and `--list-models qwen-token-plan`
  exposes the international catalog. The provider uses the official
  Southeast-Asia compatible endpoint and `QWEN_TOKEN_PLAN_API_KEY`. A
  harmless authenticated `qwen3.8-max` request returned `QWEN_LIVE_OK`; the
  credential value was never printed or persisted by the test.
- The release authentication PTY suite passes 5/5. The Qwen case confirms
  `/login qwen-token-plan` accepts bracketed API-key paste, keeps the secret
  masked in the TUI, persists it, and removes it through `/logout`.
- The root checker reverified R1–R8: source audit, TUI register, pi-tui,
  coding-agent library, real offline PTY, release launcher, and format/diff
  gates all pass. R8 is now closed by exact official-Pi versus Rust release
  captures at 100x30 and 80x24; per-capability visual/interaction rows remain
  open in the TUI register.
- The real PTY matrix now also proves rapid Unicode/multiline bracketed-paste
  marker echo under one second and exact expanded-payload persistence after a
  real Enter submission. This is evidence for the input/paste rows, not a
  percentage increase by itself.
- The latest model-scope caller leaf is parent-verified: run resolver 22/22,
  CLI print 10/10, JSON 7/7, and RPC multi-turn 2/2 pass. Interactive, JSON,
  and RPC startup apply CLI-over-settings scopes after native provider
  registration. The subsequent current-tree workspace test/clippy/release
  gates and parity register pass; remaining live-provider, row-specific, and
  visual boundaries are not closed.
- The current-tree recheck corrected two stale catalog assertions and the RPC
  golden model-count fixture. The complete workspace all-targets matrix,
  strict workspace clippy, optimized release build, release version/help,
  offline Qwen catalog listing, and parity register/dashboard smoke all pass;
  the embedded catalog total is 1,292 built-in models; runtime provider
  overlays can produce a larger model list.
- The latest post-wave rerun remains green after the CLI/session, TUI
  autocomplete, and provider edge leaves: 821 pi-coding-agent library tests,
  386 pi-tui library tests plus all integration targets, 442 pi-ai library
  tests plus all integration targets, cross-project session PTY 2/2,
  Anthropic thinking-budget coverage, strict workspace clippy, and the
  optimized release build all pass. The only repair
  required was a typed PathBuf-to-string conversion in discovered system-prompt
  source selection; it is now covered by the passing run suite.
- The new terminal/image changes add capability probing, Kitty keyboard
  response handling, iTerm/Kitty render paths, resize-aware image caching,
  progress keepalive cleanup, and nonblocking input APIs. Their automated
  gates pass, but emulator-specific and manual visual/portability evidence
  remains intentionally open.
- The latest CLI-modes wave is parent-verified: the real exhaustive CLI
  process suite passed 6/6, experimental-policy tests 4/4, main CLI tests
  4/4, and package check/strict clippy/static gates passed. CLI-035 is now
  PARTIAL for implementation/evidence with runtime OPEN; CLI-039 is PARTIAL
  across all three dimensions. Interactive context-difference, verbose
  startup/signal, and the remaining CLI-044/047 boundaries remain open.
- The provider/catalog wave is parent-verified: seven model-catalog tests,
  pi-ai check, strict clippy, JSON validation, formatting, and diff checks
  pass. PROV-020/021/022 are now PARTIAL for implementation and evidence;
  authenticated transport and live vendor behavior remain open.
- The TUI controller wave is parent-verified: 360 pi-tui library tests plus
  every integration target, strict clippy, stable formatting, and scoped diff
  checks pass. Deferred/coalesced repaint, cursor/overlay lifecycle,
  scrollback stop, shrink/resize repaint, and fullscreen restoration are
  covered; complete row contracts and manual visual comparison remain open.
- The follow-up CLI leaf added guarded normal print-mode final-text output
  through the shared stdout writer. Parent verification passed the pi unit
  suite 5/5, experimental tests 4/4, the real CLI process suite 6/6, coding-
  agent check, strict clippy, formatting, and diff checks. CLI-044 remains
  PARTIAL pending signal, broken-pipe, and complete child-failure evidence.

The next source-first wave is active in disjoint scopes: B1 is closing
remaining `pi-tui` terminal/backend, resize, scheduler, animation, and visual
capture boundaries; B2 is auditing the cross-provider message transformation
matrix (`MODEL-009`); and D1 is closing native extension command/hook/renderer/
tool/flags/provider/model surfaces (`EXT-001..006`). Parent Cargo verification
remains serialized; the percentages below remain unchanged until row-specific
implementation and evidence gates close.

That wave is now parent-verified: B1 passed 378 pi-tui library tests plus all
integration targets, strict clippy, formatting, and diff checks; B2's
Xiaomi/Token Plan and Z.AI provider fixtures passed 2/2 and 3/3 with pi-ai
check/clippy and JSON/static gates; and D1's trust/session caller gates passed
project-trust 13/13, `cli_trust` 9/9, session restart 6/6, interactive full
matrix 7/7, real PTY 10/10 plus one intentional live ignore, slash completion
5/5, run-unit 33/33, and coding-agent check/clippy/static gates. PROV-034..039
and TRUST-001/002 carry conservative PARTIAL credit where applicable; the
CLI-013..019 rows remain partial pending their complete path/error/restart
matrix, while live vendor, full trust lifecycle, and visual boundaries remain
open. No row reached PASS in this wave.

The subsequent Together/Vercel provider recheck is parent-verified: the
complete pi-ai all-target suite passed 427 library tests plus every integration
target, with strict clippy, JSON/static validation, and downstream source gates
green. PROV-031/032 are PARTIAL for implementation and deterministic evidence;
live vendor traffic and complete stream/error/retry/abort boundaries remain
OPEN. Current non-TUI counts are 49 PASS/194 PARTIAL/23 OPEN for
implementation and 36 PASS/207 PARTIAL/23 OPEN for deterministic evidence.

The native llama.cpp/local-provider slice is also parent-verified: the real
loopback catalog/auth/stream/load-unload/download-progress/cancellation/
timeout/failure fixture passes 11/11 and coding-agent all-target clippy is
green. PROV-040 is now PARTIAL for implementation and deterministic evidence;
external-server and platform/restart behavior remain open.

The environment/config checkpoint also passed `config::tests` 18/18,
covering exact upstream `env_flag` truthiness and empty agent/session-root
fallback. ENV-004, ENV-005, and ENV-006 now have implementation/evidence
PARTIAL credit; clean-process and runtime precedence remain open.

The follow-up ENV/CLI-044 source slice is parent-verified: core settings
28/28, telemetry 8/8, the session-root precedence regression 1/1,
coding-agent check, strict all-target clippy, stable formatting, and scoped
diff checks pass. Empty sessionDir values now fall through consistently and
the telemetry `PI_OFFLINE` guard matches upstream non-empty semantics; signal,
broken-pipe, child-failure, and clean-process boundaries remain open.

The OpenCode/OpenCode-Go/OpenRouter source wave is parent-verified: provider
units 31/31, provider matrix 7/7, pi-ai all-targets 419 library tests plus
every integration target, downstream coding-agent check/clippy, strict pi-ai
clippy, stable formatting, and scoped diff checks pass. PROV-025..027 now
carry implementation/evidence PARTIAL credit; live vendor and complete
stream/error/retry boundaries remain open.

The subsequent xAI checkpoint is parent-verified: xAI provider tests 33/33,
the auth-flow suite 8/8, provider matrix 7/7, and pi-ai all-targets 425
library tests plus every integration target pass with strict clippy,
coding-agent check, and static gates. PROV-033 now has implementation/evidence
PARTIAL credit; live xAI traffic, device authorization, and complete external
stream/error/retry boundaries remain open.

The latest CLI-044/047 leaf is parent-verified: real print/JSON signal probes,
interactive signal PTYs, broken-pipe help/version probes, RPC child-failure
evidence, experimental strict-policy tests, package check, and strict clippy
all pass; vendor/platform and complete child-lifecycle boundaries remain open.
The follow-up CLI-005..011 leaf also passes args/run/print/JSON focused suites,
release BOM/Unicode and missing-`@file` process checks, and signal-aware
print/JSON cancellation; live provider, Windows, and exhaustive file/input
boundaries remain open.
The latest verified source wave added terminal/image/scrollbar protocol coverage
in pi-tui, provider-independent SSE/event-stream/abort coverage across seven
AI adaptors, and the upstream HOME/USERPROFILE environment fix. The current
disjoint source wave is active: B1 is auditing TUI keybinding/search/suspend/
scheduler surfaces, B2 is auditing Kimi/MiniMax provider adapters, and D1 is
auditing remaining environment contracts. Parent Cargo verification remains
serialized.

The current TUI register remains conservative:

TUI functional implementation: 19.23% (10/52)
TUI test/evidence parity: 19.23% (10/52)
TUI visual/interaction parity: 0.00% (0/52)
TUI overall parity: 0.00% (0/52)

The residual parity wave under `.unlazy/parity-20260827/` is parent-verified:
the provider, agent-runtime, and transcript/TUI leaves all passed their
scoped Cargo/static gates. The TUI leaf also passed the real PTY lifecycle,
composer, tool, resize, cancellation, and coexistence matrix; its
credentialed live-provider case remains intentionally separate. TUI-052 is now
promoted to PASS for functional and test/evidence dimensions after official Pi
0.84.3 and pi-rust ran together at 100x30 and 80x24. Other verified slices do
not automatically change a percentage: the dashboard dimensions are row-based,
and only rows with complete implementation plus matching evidence may be
promoted.

The configuration/resources, CLI/model/lifecycle runtime, and coding-agent
tools/export/diagnostics residual wave is parent-verified. The TUI closure wave
has now added parent-verified session/tree/auth/clipboard component coverage
and an immediate cached-scene repaint after editor input. Real composer PTY
evidence records 20 per-keystroke samples at p95/max 3.98 ms, plus rapid
Unicode/multiline paste coverage; root R1-R7 were rerun successfully. The
percentages remain unchanged because full row-level implementation, evidence,
and visual criteria are still open.

The previously dispatched settings, provider/catalog, editor/input,
session/tree/auth/clipboard, and scheduler/repaint slices are now parent-
verified. The optimized workspace all-targets release gate, strict workspace
clippy, release binary build, executable smoke checks, and the real auth,
settings, composer, and PTY matrices all pass. The international Qwen catalog
real pasted-key login path, and one authenticated `qwen3.8-max` inference are
verified. No percentage is promoted from these scoped results because the
row-level visual register and most normalized-but-open product capabilities
remain incomplete.

The latest SI4 parent recheck passed settings-panel 9/9, real settings PTY
2/2, core-settings 27/27, interactive-mode 50/50, parity-audit 8/8, the full
workspace all-target matrix, strict workspace clippy, formatting/diff checks,
and the optimized release build. SI4 is still open pending explicit
row-by-row persistence/live/cancel/restart evidence for all 29/31 registered
settings rows; the measured percentages below therefore remain unchanged.

The synchronized progress dashboard is:

Source/conversion ledger: 100.00% (166/166; 0 open)
Acceptance inventory census: 100.00% (318/318) (318 IDs indexed)
Acceptance scoring coverage: 100.00% (318/318) (318 of 318 IDs scored)
Root acceptance gates: 100.00% (8/8) (8 passed; 0 open)
Rust-only distribution boundary: 100.00% (0 JS/TS executable source files; generated Rustdoc excluded)
TUI functional implementation: 19.23% (10/52)
TUI test/evidence parity: 19.23% (10/52)
TUI visual/interaction parity: 0.00% (0/52)
TUI overall parity: 0.00% (0/52)
Non-TUI implementation parity: 18.42% (49/266 PASS; 194 PARTIAL; 23 OPEN)
Non-TUI deterministic evidence parity: 13.53% (36/266 PASS; 207 PARTIAL; 23 OPEN)
Non-TUI runtime-boundary parity: 13.91% (37/266 PASS; 154 PARTIAL; 75 OPEN)
Non-TUI overall parity: 11.28% (30/266)
Whole-product behavioral parity: 9.43% (30/318)

See `docs/PARITY-DASHBOARD.md` for definitions and the machine-validated
refresh command.

## Previous paused checkpoint — 2026-08-27 — TUI dimension tracking and verification

The user paused execution after requesting explicit tracking for four separate
TUI completion dimensions: functional implementation, test/evidence-proven
parity, visual/interaction parity, and overall parity. Work is intentionally
preserved and is not committed or pushed. The branch is still `main` at
`0457e0e95a7aaede62722ac5b554b4e226c16319`, matching the live
`origin/main` hash. The worktree contains the existing Rust conversion wave
plus the current uncommitted controller, interactive renderer, documentation,
and test changes. The in-flight complete debug workspace test was stopped
during this pause after substantial targets had emitted passing results. Its
final workspace exit status is not being accepted as verified evidence, so it
must be rerun from the beginning before the full-workspace gate is marked
green.

### Evidence completed before the pause

- `/home/mustbearnold/.cargo/bin/cargo fmt --all -- --check` — passed.
- `git diff --check` — passed.
- `/home/mustbearnold/.cargo/bin/cargo clippy --workspace --all-targets --offline -- -D warnings` — passed after preserving the compatibility signature exception on `build_interactive_scene_with_loader_and_scroll_view`.
- `/home/mustbearnold/.cargo/bin/cargo test -p pi-tui --offline --tests -- --test-threads=1` — passed: 268 library tests, 9 application tests, 6 layout tests, 2 loader tests, and 4 mouse/controller tests (289 total).
- `/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test interactive_full_matrix --test interactive_release_multiturn --test interactive_slash_complete_pty --test interactive_tool_render_pty --test interactive_auth_pty -- --test-threads=1` — passed: 4 auth, 7 full-matrix, 3 release-multiturn, 4 slash/error/cancellation, and 1 live tool-render test (19 total).
- The complete workspace debug command was started but deliberately interrupted after substantial targets passed; rerun it from the beginning before changing any gate to green.

### Current uncommitted implementation

- `crates/pi-tui/src/controller.rs` now has owner-driven, coalesced
  force-aware repaint invalidation, timer-safe request callbacks, retained
  layout callback wiring, corrected listener routing, cursor handling, and
  focused tests.
- `crates/pi-coding-agent/src/modes/interactive.rs` now bridges fullscreen
  repaint requests through `tokio::sync::Notify`, attaches the controller
  repaint callback to the retained loader, and keeps rendering on the owner
  task. `crates/pi-tui/src/tui.rs` has the corrected retained-layout comment;
  `crates/pi-coding-agent/src/interactive/mod.rs` has the narrow compatibility
  lint exception.
- `docs/TUI-PARITY-STATUS.md` was added with one row for every TUI-001 through
  TUI-052 capability and separate functional, evidence, visual/interaction,
  and calculated-overall fields. `parity_audit tui` now validates the complete
  register and reports the current values:
  `17.31% (9/52)` functional, `17.31% (9/52)` test/evidence,
  `0.00% (0/52)` visual/interaction, and `0.00% (0/52)` overall. These values
  are generated from the table rather than copied from the source ledger.
- `/home/mustbearnold/.cargo/bin/cargo run -p pi-coding-agent --offline
  --quiet --bin parity_audit -- tui` passed with
  `PARITY_TUI_OK rows=52 functional=9 evidence=9 visual=0 overall=0`.
- The updated `.githooks/pre-commit` passed a temporary-index smoke test with
  all current files staged and GitHub CLI intentionally absent from PATH; the
  real hook will attempt the authenticated repository-description sync at the
  next explicit commit checkpoint.

TUI functional implementation: 17.31% (9/52)
TUI test/evidence parity: 17.31% (9/52)
TUI visual/interaction parity: 0.00% (0/52)
TUI overall parity: 0.00% (0/52)

### Required resume work

1. Rerun the complete debug workspace suite from the beginning, then rebuild
   and rerun the complete release suite after the tracker/hook changes.
2. Re-measure each TUI row against its inventory acceptance contract and
   replace `PARTIAL` with `PASS` only when its direct evidence is complete.
3. Complete credentialed provider refresh/restart/error recovery, runnable
   official-Pi coexistence where available, and manual terminal-size/emulator
   visual comparison before increasing any visual or overall percentage.
4. Only after a green logical checkpoint and an explicit user instruction to
   publish, create a focused commit and push it; until then the current
   local/remote divergence is intentional.

The repository remains at the historical source-ledger value
`Conversion progress: 100.00% (166/166; 0 open)`, which is not any of the four
behavioral TUI percentages above.

## Current distribution/update-boundary correction — 2026-08-27

The `Update available: pi 0.84.3 - run pi update` banner was incorrect for
pi-rust and is removed. Interactive startup no longer queries the upstream Pi
release endpoint. `pi update --extensions` and `pi update --models` remain
scoped package/catalog operations; `pi update` and `pi update --self` do not
query or replace an upstream Pi binary and instead print the pi-rust
repository/rebuild instruction.

Permanent evidence includes the release PTY regression
`startup_does_not_query_upstream_pi_or_show_update_notice`, which supplies a
loopback release endpoint, proves no request is made, and checks that the
notice is absent. The focused package/update tests also verify the non-zero
self-update boundary and that the old `pi.dev` release lookup is not used.

## Latest active checkpoint — 2026-08-26 — launch and real Codex verification

The active exhaustive acceptance index is
`docs/EXHAUSTIVE-PARITY-INVENTORY.md` with **318 capability IDs**. The
historical source-ledger checker remains green at `100.00% (166/166; 0 open)`;
that figure is not the product's behavioral-completion percentage, and pi-rust
is not yet claimed 1:1 or flawless.

Current measured evidence:

- `pi-rust --version` and `target/release/pi --version` report `pi 0.84.2`.
  The official installed `pi` remains separate and reports `0.84.3`, so both
  implementations can run side by side.
- `cargo test --workspace --offline -- --test-threads=1` and the equivalent
  release workspace suite pass, as does strict workspace clippy.
- The release `interactive_auth_pty` target passes 4/4: real-TUI auth method
  selection, browser/manual OAuth URL and cancellation, loopback browser
  callback token exchange and persistence, device-code exchange, logout, and
  llama.cpp API-key validation.
- The current stored OpenAI Codex OAuth credential completed two sequential
  print turns and two sequential interactive PTY turns. A direct interactive
  `/login` run displayed the real method selector and `auth.openai.com` browser
  authorization URL, then cancelled without changing the credential.
- The live tool-display rerun used the release binary with the real stored
  Codex credential and an isolated session root. It observed a running `read`
  block with the requested path, a settled `read` block with file output, and
  the exact response `LIVE_TOOL_RENDER_OK`; the captured normal-TUI stream had
  no fenced or argument-envelope JSON. Focused renderer/lifecycle tests pass.
- The release interactive startup probe did not contact a supplied upstream
  release endpoint and showed no update banner; update instructions now point
  to this repository and rebuild workflow.
- The concurrent-process gate passes two real pi-rust instances and a shared
  session-migration race with no state collision. The adjacent official Pi
  source checkout has no installed dependencies/build artifacts, and PATH
  `pi` is the Rust release binary, so official-JS-Pi coexistence remains
  explicitly unverified rather than claimed.
- `target/release/parity_audit inventory` reports
  `PARITY_INVENTORY_OK ids=318 upstream_files=1310 rust_files=484`; the
  installed-command audit reports `PARITY_INSTALLED_RUST_OK`.
- The final release rerun initially exposed two genuine test-harness defects:
  the experimental server test inherited the user's existing session files,
  and the slash PTY fixture retained the old unknown-provider wording. The
  test now uses an isolated session root and the expectation matches the
  truthful `/login` diagnostic; the focused target and complete release
  workspace matrix pass afterward.

The exact commands for launching, safe no-tool testing, real browser login,
live Codex turns, and the full local matrix are in `README.md` under
`Launch and test it yourself`. Remaining work is the per-ID evidence review,
the unclosed live refresh/error-recovery and clean-room/manual boundaries, and
final root-gate synchronization.

## Where the work stopped

The current requested progress percentage is based on the exhaustive conversion
ledger, not the original 100-item queue:

```text
100.00% = 166 completed / 166 total tasks
0 tasks remain open

```

The authoritative ledger is [CONVERSION-LEDGER.md](CONVERSION-LEDGER.md).
[PLAN.md](PLAN.md) displays the same value. Recalculate it and run the final
source audit after every ledger edit with:

```bash
/home/mustbearnold/.cargo/bin/cargo run -p pi-coding-agent --offline --bin conversion_audit -- all
```

S-066 is now frozen: the checker validates the 166-ID universe, zero
source-audit blockers, and zero JS/TS source files.

## Important working-tree state

The latest pushed checkpoint before this exhaustive usability-test wave is
`76b016489df14f9819e6501f4694aa4e6f96e070`. The current worktree contains the
Rust-only implementation and documentation changes that are pending the final
commit/push. The pre-existing untracked `AGENTS.md` remains untouched and must
be preserved.
Do not use `git reset --hard`,
`git checkout --`, broad revert commands, or `git clean`.

Current status: branch `main`, progress checker reports
`100.00% (166/166; 0 open)`. The distribution is 100% Rust: no JavaScript or
TypeScript source, Node/Bun runtime, npm execution, or source-language
extension loading remains. Rust-native factories, static Rust HTML export,
the package-manager rejection boundary, and the source audit are implemented
and locally validated. The pre-existing untracked `AGENTS.md` remains
preserved and unstaged.

All sections below whose headings say “Current checkpoint” and which contain
older percentages or commits are historical snapshots from earlier turns.
The latest active sections near the end of this file supersede them.

## Current extension contract checkpoint — 2026-08-26

The compile repair and EXT-009–011 implementation are green within the
assigned Rust extension scope. `ExtensionContext` now exposes a live host
handle for the audited session/model/trust/queue/signal/action surface, with
typed pending outcomes for queued lifecycle/model operations and stale-context
rejection. The UI broker emits correlated `extension_ui_request` records,
resolves `extension_ui_response` records for select/confirm/input/editor and
custom overlays, bounds waits, handles cancellation, and records malformed,
unknown, and late diagnostics. It also implements fire-and-forget UI state,
terminal listener dispatch, widget/header/footer/thinking-label surfaces,
autocomplete/editor factories, themes, editor text, and tool expansion.
Registered tools retain all upstream metadata and callback forms; preparation,
updates, catalog publication, and JSON render callbacks are live.

Exact focused validation completed with the direct stable toolchain:

```text
cargo test -p pi-coding-agent --offline --lib core::extensions -- --nocapture --test-threads=1  # 58 passed
cargo test -p pi-coding-agent --offline --lib core::extensions::integration::tests::default_mode_loader_seeds_parsed_extension_flags_before_lifecycle -- --nocapture --test-threads=1  # 1 passed; included above
cargo test -p pi-coding-agent --test extensions_parity --offline -- --nocapture --test-threads=1  # 9 passed
cargo test -p pi-coding-agent --offline --lib 'modes::rpc::tests::rpc_' -- --nocapture --test-threads=1  # 16 passed
cargo check -p pi-coding-agent --tests --offline  # Finished successfully
/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo clippy -p pi-coding-agent --lib --offline -- -D warnings -A clippy::invalid_regex -A unused-imports -A unused-mut  # Finished successfully
/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustfmt --edition 2021 --check crates/pi-coding-agent/src/core/extensions/types.rs crates/pi-coding-agent/src/core/extensions/integration.rs crates/pi-coding-agent/src/core/extensions/runner.rs crates/pi-coding-agent/src/core/extensions/loader.rs crates/pi-coding-agent/src/core/extensions/wrapper.rs crates/pi-coding-agent/src/modes/rpc.rs crates/pi-coding-agent/tests/extensions_parity.rs
git diff --check
```

The authoritative Cargo audit remains `Conversion progress: 100.00%
(166/166; 0 open)`, with zero audit blockers and zero workspace JS/TS source
files. The unisolated package clippy command still reports only unrelated
dirty-worktree diagnostics in `core/changelog.rs` (look-ahead regex) and
`interactive/clipboard.rs` (unused import/mutability); the focused command
above explicitly isolates those. Concrete pi-tui component construction and
the interactive raw-PTY hook remain host-owned work outside this slice; this
lane supplies the renderer-neutral JSON/native factory and RPC broker contracts
and does not edit `interactive.rs` or `pi-tui`.

Follow-up flag audit complete: `integration::parsed_extension_flag_values`
feeds parsed `Args.extension_flag_values` through the default mode loader
before `session_start`, with last-value wins and no extra lifecycle dispatch.
The permanent regression passed and is included in the 58-test extension
suite. The latest rerun of `cargo check -p pi-coding-agent --tests --offline`
is not green because the excluded, actively changing `pi-tui` lane currently
has five errors in `components/scroll_view.rs` and `layout.rs`: missing
`ScrollbarMode: Default`, missing `scrollbar_visible_locked`, and tuple
geometry accessed through `thumb_top`/`thumb_height`. This lane made no
`pi-tui`/`pi-agent`/`pi-ai` edits and has not committed or pushed the staged
extension checkpoint while that package gate is red.

The default loader has no duplicate lifecycle dispatch. The remaining exact
validation-lifecycle gap is outside this lane: `main.rs::validate_extension_flags`
still uses a temporary mode loader to discover definitions before the actual
mode. A definition-only loader call in `main.rs` is required to remove that
extra validation-time lifecycle.

## Current exhaustive usability-test checkpoint — 2026-08-26

The Rust product binary has been tested across deterministic offline/faux
interactive, print, JSON, and RPC paths. The campaign added permanent
multi-turn PTY and binary-RPC tests, plus CLI command/resource/trust/flag
matrices. It also found and fixed three user-visible defects: JSON argv
prompts were incorrectly batched, piped stdin was omitted from the initial
prompt, and bracketed paste markers were stripped before editor dispatch.

Focused evidence is green: 39 command, 6 flag, 7 JSON, 10 print, 9 resource,
8 trust, 4 full interactive, ConfigSelector PTY, slash-command PTY,
release-binary TUI, binary-RPC, and 20 stdin-buffer tests. The complete debug
and release workspace suites pass with `--test-threads=2`; the direct release
binary smoke prints `pi 0.84.2` and accepts `--help`; release overrides for
both the PTY and RPC child-process tests pass.

Exact validation run in this checkpoint:

```text
/home/mustbearnold/.cargo/bin/cargo fmt --all -- --check
git diff --check
/home/mustbearnold/.cargo/bin/cargo clippy --workspace --offline --all-targets -- -D warnings
/home/mustbearnold/.cargo/bin/cargo test --workspace --offline --quiet -- --test-threads=2
/home/mustbearnold/.cargo/bin/cargo build --workspace --release --offline
/home/mustbearnold/.cargo/bin/cargo test --workspace --release --offline --quiet -- --test-threads=2
test "$(target/release/pi --version)" = "pi 0.84.2" && target/release/pi --help >/dev/null
PI_RUST_TEST_BINARY="$PWD/target/release/pi" /home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test interactive_release_multiturn -- --test-threads=1
PI_RUST_TEST_BINARY="$PWD/target/release/pi" /home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test rpc_binary_multiturn -- --test-threads=1
/home/mustbearnold/.cargo/bin/cargo run -p pi-coding-agent --offline --bin conversion_audit -- all
  Conversion progress: 100.00% (166/166; 0 open)
  audit blockers: 0
  workspace JS/TS source files: 0
```

`node scripts/conversion-progress.mjs` was also checked as required by the
local operating protocol, but that legacy script is absent from this
Rust-only checkout (`MODULE_NOT_FOUND`). No conversion ledger checkbox changed
during this usability-test campaign; the new behavioral evidence is recorded
in `CONVERSION-LEDGER.md`, this handoff, and the ignored unlazy gate reports.
Credentialed live-provider inference, alternate terminal emulators, and the
installed PATH `pi` command are not claimed as tested. PATH still resolves to
the JavaScript/mise `pi 0.84.3`; use `target/release/pi` for the Rust product.

Resume checkpoint complete: the root G2 combined print/JSON/harness/RPC
oracle passed, and the final unlazy status is **19/19 gates met**. The first
all-gates reverify briefly reported G4 exit 101 while PTY/release gates were
being orchestrated together; rerunning the exact G4 command alone passed
clippy, release build, and every release test target with exit 0. This was not
reproducible as an isolated product failure. The focused checkpoint is now
That earlier usability checkpoint was committed and pushed as
`2a9284b76957d2b4bb3a259511fe8817e864fe13`; `git rev-parse HEAD` and
`git ls-remote origin refs/heads/main` matched at that checkpoint. The current
extension contract work and other independent lanes remain dirty in the shared
worktree; unrelated changes, including the untracked `AGENTS.md`, remain
preserved and unstaged by this lane.

## Current bounded RPC/protocol command-parity checkpoint — 2026-08-26

The bounded RPC task is implementation-complete. It changed only the RPC
runtime, JSONL/RPC binary protocol coverage, the RPC parity fixture, and the
three required project documents. `interactive.rs`, all `pi-tui` files, and
the unrelated provider/auth dirty work remain untouched and unstaged.

Implemented behavior:

- `get_commands` now returns extension, prompt-template, and skill command
  metadata in upstream order, including settings-configured prompt paths and
  `sourceInfo`.
- Prompt input dispatches loaded extension commands, expands skills/templates,
  validates and preserves images, and honors `streamingBehavior: "steer"` or
  `"followUp"` while a run is active. Queued extension commands fail with the
  upstream-style boundary error.
- JSONL dispatch consumes `extension_ui_response` envelopes without emitting a
  false unknown-command response; the current Rust extension host now emits
  correlated UI requests, resolves real responses, and diagnoses malformed,
  unknown, and late response records.
- JSONL regressions cover LF-only records and U+2028/U+2029 preservation. The
  binary test covers project prompt/skill discovery, a real multi-turn faux RPC
  session, and the inbound UI-response envelope.

Exact direct-stable offline evidence:

```text
RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc RUSTDOC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustdoc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-coding-agent --offline --lib modes::rpc::tests -- --test-threads=1   # 48 passed
RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc RUSTDOC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustdoc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-coding-agent --offline --lib modes::jsonl::tests -- --test-threads=1  # 7 passed
RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc RUSTDOC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustdoc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-coding-agent --offline --lib modes::json_event::tests -- --test-threads=1  # 1 passed
RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc RUSTDOC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustdoc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-coding-agent --offline --lib modes::rpc_types::tests -- --test-threads=1  # 4 passed
RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc RUSTDOC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustdoc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-coding-agent --offline --test rpc_binary_multiturn -- --test-threads=1  # 2 passed
RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc RUSTDOC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustdoc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-protocol --offline -- --test-threads=1  # 46 executable tests; 0 doctests
RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc RUSTDOC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustdoc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo run -p pi-coding-agent --offline --bin conversion_audit -- all
  Conversion progress: 100.00% (166/166; 0 open)
  audit blockers: 0
  workspace JS/TS source files: 0
RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc RUSTDOC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustdoc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo fmt --all -- --check
git diff --check
```

The `node scripts/conversion-progress.mjs` command exits 1 because the legacy
script is absent (`MODULE_NOT_FOUND`). Focused coding-agent builds needed a
temporary one-line repair of unrelated untracked `radius.rs`; it was restored
before this checkpoint and will not be committed. With it restored, the normal
coding-agent rebuild remains blocked by that existing `E0515`.

This checkpoint is committed and pushed as
`952256c5c230daf8f204f41d7ffb8d7b20c38696`; `git rev-parse HEAD` and
`git ls-remote origin refs/heads/main` were verified equal. The branch is
`main`; the working tree still contains unrelated pre-existing dirty changes,
including the unstaged OAuth-refresh hunk in `rpc.rs`, provider/auth work,
interactive work, `pi-tui` work, untracked `AGENTS.md`, and the untracked
Radius file. None was staged. The extension UI request channel is now
implemented; do not stage those unrelated paths.

## Current bounded pi-agent lifecycle checkpoint — 2026-08-26

The bounded implementation is complete in
`crates/pi-agent/src/rich_agent.rs`; the public Result adaptation required by
that lifecycle contract is in `crates/pi-agent/src/harness/agent_harness.rs`.
No numbered ledger row changed: this is a refinement and live deterministic
verification of S-018/S-019/S-038. The upstream oracle inspected was
`../pi-rust-s1-audit.KMw0N2/upstream_pi/packages/agent/src/agent.ts` plus its
agent tests.

Exact validation completed with the direct stable toolchain:

```text
RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-agent --offline --lib rich_agent::tests -- --test-threads=1
  21 passed; 0 failed
RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-agent --offline --lib --quiet -- --test-threads=1
  195 passed; 0 failed
RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-agent --offline --tests --quiet -- --test-threads=1
  294 passed; 0 failed
RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo check -p pi-agent --offline --all-targets
  Finished successfully
RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo clippy -p pi-agent --offline --all-targets -- -D warnings
  Finished successfully
rustfmt --edition 2021 --check crates/pi-agent/src/rich_agent.rs crates/pi-agent/src/harness/agent_harness.rs
git diff --check
```

The required `node scripts/conversion-progress.mjs` command still fails with
`MODULE_NOT_FOUND` because the script is absent. The existing Cargo-native
ledger value remains **100.00% (166/166; 0 open)**. No `pi-ai`, `pi-tui`, or
`pi-coding-agent` source was changed; unrelated dirty work remains preserved.

Remaining limitation: subscriber callbacks are still delivered by a post-loop
replay and awaited before the run lease clears, rather than being dispatched
live at each low-level event. The next safe action is a separate subscriber
delivery parity slice; do not mix it into this bounded lifecycle checkpoint.

## Current Rust-only completion checkpoint — 2026-08-25

The explicit acceptance change to 100% Rust closes S-027 as a Rust-native
distribution boundary. The old Node/Bun bridge, embedded JavaScript runtime
assets, JS/TS tooling, and source-language fixtures are removed. Rust factory
extensions remain supported; filesystem JS/TS paths and npm/Bun package
execution return deterministic Rust-native guidance. HTML export is static
Rust-rendered output.

Final local evidence:

```text
/home/mustbearnold/.cargo/bin/cargo run -p pi-coding-agent --offline --bin conversion_audit -- all
  Conversion progress: 100.00% (166/166; 0 open)
  audit blockers: 0
  workspace JS/TS source files: 0
/home/mustbearnold/.cargo/bin/cargo check --workspace --offline --all-targets
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib -- --test-threads=2
  507 passed
/home/mustbearnold/.cargo/bin/cargo fmt --all -- --check
/home/mustbearnold/.cargo/bin/cargo clippy --workspace --offline --all-targets -- -D warnings
/home/mustbearnold/.cargo/bin/cargo build --workspace --release --offline
/home/mustbearnold/.cargo/bin/cargo test --workspace --offline --quiet -- --test-threads=2
/home/mustbearnold/.cargo/bin/cargo test --workspace --release --offline --quiet -- --test-threads=2
git diff --check
```

All local validation gates pass. The remaining handoff action is the
completion commit/push and local/remote hash verification; `AGENTS.md` remains
pre-existing, untracked, and preserved.

## Historical implementation checkpoint — 2026-08-25 — S-027 graph and lifecycle increment

The bridge now embeds the byte-identical jiti@2.7.0 jiti.cjs and babel.cjs
artifacts plus a jiti-static.mjs wrapper and the generated 10,759,929-byte
pi/TypeBox module graph (SHA-256
`a82bde7cf62fcf75bf4f24acadc4ade6e526931812bc5594252e4bb4be6e4896`). It
selects the pinned Node alias versus Bun virtualModules/tryNative: false
option branch, supports explicit alias/virtual-module maps with shared
exported-object fixtures, and normalizes tilde/file-URL/Unicode-space/lexical
extension paths. `/reload` now re-evaluates the configured extension set,
invalidates the old runner, removes and re-registers native providers, and
refreshes the host catalog while preserving extension flags. Startup,
shutdown, cancellable session switches, session replacement, and
print/interactive resource discovery are covered by focused fixtures. The
runtime keeps the mode-scoped ExtensionRuntime alive without reintroducing an
ownership cycle, and materialized assets are cleaned up on bridge shutdown.

Validation completed in this checkpoint:

```text
cargo fmt -p pi-coding-agent
cargo test -p pi-coding-agent --offline --lib core::extensions::loader::tests --quiet
  21 passed
PI_RUST_BUN=/tmp/pi-bun-runtime.JisAfQ/bun-linux-x64/bun cargo test -p pi-coding-agent --offline --lib core::extensions::loader::tests --quiet
  21 passed; Bun 1.4.0
cargo test -p pi-coding-agent --offline --lib modes::interactive::tests::interactive_reload_re_evaluates_extension_and_refreshes_tools --quiet
  1 passed
cargo test -p pi-coding-agent --offline --lib core::extensions --quiet
  42 passed
cargo test -p pi-coding-agent --offline --test extensions_parity --quiet -- --test-threads=1
  15 passed
cargo test -p pi-coding-agent --offline --lib --quiet -- --test-threads=2
  487 passed
cargo clippy -p pi-coding-agent --offline --all-targets -- -D warnings
cargo fmt --all -- --check
git diff --check
node scripts/conversion-progress.mjs
  Conversion progress: 98.80% (164/166; 2 open)
```

The current implementation intentionally keeps S-027 open. The remaining
conditions are a genuine in-process compiled-Bun/Node-SEA host, full
interactive theme metadata/activation, and session-before-fork coverage. The
clean-room gate passed in the prior fresh clone with the pinned oracle; its
exact evidence is recorded in
`.unlazy/full-conversion-20260825/gates/clean-room-current.md`. The next
dependency-safe actions are an independent review of this increment, a fresh
clean-room run at its pushed commit, and then the final S-066 source/TODO
denominator audit. Do not claim 100% until the ledger, plan, handoff, final
audit, and remote hash gates agree.

## Current strict-verification cleanup

The telemetry adapter's `InMemoryChildSpan::start_chapter_async` path now reads
the parent id under its mutex, releases the guard, and only then awaits the
callback or creates a child span. This removes the `await_holding_lock` clippy
finding while preserving the settled-parent noop callback behavior.

Evidence currently passing: `cargo test -p pi-telemetry --offline --quiet`
(6 passed), `cargo clippy -p pi-telemetry --offline --all-targets -- -D
warnings`, `cargo fmt --all -- --check`, `git diff --check`, and the progress
checker. The full `cargo clippy -p pi-ai --offline --all-targets -- -D
warnings` gate now passes with zero diagnostics. The adapter and structural
cleanup covered derived defaults, option flattening, guard patterns, copy-field
moves, test fixtures, provider lock scopes, and the faux/provider enum layout.
Full `pi-ai` tests pass (290 library, 4 + 8 + 2 integration tests). This cleanup
did not change the ledger count at that earlier checkpoint (`62.65%`, 104/166).
The verified implementation checkpoint is `7b3db53`; local `HEAD` and
`origin/main` matched immediately after its push.

## Current checkpoint — 2026-08-24 — S-008 complete and pushed

S-008 is implemented and marked complete in `CONVERSION-LEDGER.md`. The shared
resolver now clones and strictifies supported JSON schemas, wraps optional
properties as nullable required fields, rejects the upstream unsupported subset
with exact diagnostics, resolves non-empty Lark before regex grammar variants,
infers the single required string input property, and emits monotonic streaming
JSON deltas. OpenAI Completions, Responses, Azure, and Codex support grammar
custom tools; Anthropic, Bedrock, Google/Vertex, Mistral, and the Responses
family support strict-schema conversion. Required schemas are never silently
dropped or downgraded.

Evidence:

```text
cargo test -p pi-ai --offline --lib api::constrained_sampling --quiet
cargo test -p pi-ai --offline --lib api::openai_completions --quiet
cargo test -p pi-ai --offline --lib api::openai_responses_shared --quiet
cargo test -p pi-ai --offline --quiet (307 library, 4 + 9 + 2 integration tests)
cargo clippy -p pi-ai --offline --all-targets -- -D warnings
cargo check --workspace --offline
cargo fmt --all -- --check
git diff --check
node scripts/conversion-progress.mjs
```

All listed focused checks pass; the checker reports exactly
`Conversion progress: 63.25% (105/166; 61 open)`. Independent reviewers
compared the implementation with upstream commit
`5cd93f688aaab89dbb6dfa4aca535f21796ae185` and returned APPROVE with no parity
blockers, including custom item-ID omission and namespace preservation. A full `cargo test --workspace --offline --quiet` attempt was not a
code failure: the linker was killed with SIGKILL 9 while linking the unrelated
`pi-coding-agent` `export_html_parity` test binary. The focused `pi-ai` suite
is green; rerun the workspace test gate when host linker pressure permits.

The S-008 implementation commit `7a72f2fe104cf660f946f29a822c88da556a37d1`
was pushed to `origin/main`; the final documentation-sync commit is
`3f649fec8ea6a33860e5acfe50d96e92b02a09ad`. Current `git rev-parse HEAD` and
`git ls-remote origin refs/heads/main` both resolve to the documentation-sync
hash. Next dependency-safe work is S-009 Codex WebSocket session caching/reuse.

## Current checkpoint — 2026-08-24 — S-009 complete and pushed

S-009 completes Codex WebSocket session caching/reuse and the
`websocket-cached` transport behavior. The implementation in
`crates/pi-ai/src/api/openai_codex_responses.rs` now has process-global
session/account cache keying, busy-entry isolation, cached-context request
deltas, 5-minute idle eviction, 55-minute max-age eviction, cache-retention
opt-out, missing-continuation retry, and cleanup on all WebSocket error paths.
Plain `websocket` reuses sockets without delta-context construction, while
`auto` keeps the SSE fallback behavior.

Evidence and review:

```text
cargo test -p pi-ai --offline --lib api::openai_codex_responses --quiet (34 passed)
cargo test -p pi-ai --offline --quiet (313 library, 4 + 9 + 2 integration tests)
cargo check -p pi-ai --offline
cargo clippy -p pi-ai --offline --all-targets -- -D warnings
cargo fmt --all -- --check
git diff --check
node scripts/conversion-progress.mjs
```

All listed checks pass. The local mock fixtures cover socket reuse and input
deltas, missing `previous_response_id` recovery, eviction guards, and
authenticated-account scoping. Independent reviewer bat compared the Rust
implementation with `upstream_pi/packages/ai/src/api/openai-codex-responses.ts`
and returned **APPROVE** with no blockers. The checker reports exactly
`Conversion progress: 63.86% (106/166; 60 open)`. The next dependency-safe
task is S-010 AWS credential/profile-file and region resolution parity for
Bedrock. The implementation commit is pushed, the documentation-sync commit
containing this handoff is pushed, and local/remote hashes were verified after
the push. The worktree is clean except for the preserved untracked
`AGENTS.md`.

## Current checkpoint — 2026-08-24 — S-010 published and whole-result checked

S-010 completes Bedrock credential/profile-file and region-resolution parity.
The adaptor now honors explicit and scoped profile precedence over ambient
access keys, ambient profile env-key precedence, shared credentials files,
selected-profile `AWS_CONFIG_FILE` regions, ARN/env/option endpoint-region
precedence, bearer and skip-auth modes, ECS task-role credentials, and web
identity STS credentials. ECS relative/full URI requests support authorization
tokens and token files. STS `AssumeRoleWithWebIdentity` responses are parsed as
XML and temporary session tokens are included in SigV4 signing. Provider auth
uses the upstream source labels `ECS task role` and `web identity token`.

Evidence and review:

```text
$HOME/.cargo/bin/cargo test -p pi-ai --offline --lib api::bedrock_converse --quiet (43 passed)
$HOME/.cargo/bin/cargo test -p pi-ai --offline --lib providers::all::tests::amazon_bedrock_auth_recognizes_ecs_and_web_identity_sources --quiet (1 passed)
$HOME/.cargo/bin/cargo check -p pi-ai --offline
$HOME/.cargo/bin/cargo clippy -p pi-ai --offline --all-targets -- -D warnings
$HOME/.cargo/bin/cargo test -p pi-ai --offline --quiet (325 library, 4 + 9 + 2 integration tests)
$HOME/.cargo/bin/cargo test -p pi-ai --offline --tests --quiet
$HOME/.cargo/bin/cargo metadata --no-deps --offline --format-version 1
$HOME/.cargo/bin/cargo fmt --all -- --check
git diff --check
node scripts/conversion-progress.mjs
```

The local mock fixtures cover profile precedence, config-file region loading,
ECS JSON parsing and retrieval, container authorization, STS XML parsing and
form submission, exported `stream` ECS retrieval, exported `stream_simple`
web-identity retrieval, Bedrock eventstream responses, signed credential IDs,
and session-token headers. Independent reviewer cow compared
the implementation and tests with the upstream Bedrock API/provider/env-key
sources and credential/endpoint fixtures and returned **APPROVE** with no
blockers. SSO- or process-backed profiles and EC2 metadata remain outside the
manual signer scope.

The feedback loop was rerun over the complete S-010 result after the earlier
implementation and review work. Profile/env/config/endpoint precedence maps to
the named fixtures above; ECS and web-identity runtime behavior maps to the
parser and local mock HTTP fixtures; the exported `stream` and `stream_simple`
boundaries map to the two public runtime fixtures and their local Bedrock
eventstream servers; and the provider-auth boundary maps to the two Bedrock
auth tests. The full pi-ai library and integration targets passed.

The broader packaging check was also attempted with:

```text
$HOME/.cargo/bin/cargo package -p pi-ai --offline --allow-dirty --no-verify
```

It remains blocked before packaging because the internal `pi-telemetry` path
dependency has no crates.io version requirement and is unavailable in the
offline index. This is a repository P9 packaging blocker, not an S-010 runtime
or public-interface failure. No ledger checkbox changed during this rerun. The
checker reports exactly `Conversion progress: 64.46% (107/166; 59 open)`. The
next dependency-safe task is S-011 Google Vertex ADC file, token URI, scope,
refresh, and project/location precedence parity. The S-010 implementation
commit is `9a8eaee9b8273e7b938075a38ed9659baff02359`, and the public-boundary
acceptance/documentation commit is
`feadf6415f663662ff0948b2e29507655fc359bd`. Both are pushed with matching
local/remote hashes; the ledger, plan, handoff, and README are synchronized.

## Current secondary-lane committed checkpoint (partial S-021/S-022)

The implementation slice is now committed and pushed:
`AgentHarness` session is now shared across lane views, `lane`/`create_lane`/
`lanes` expose durable main and secondary lane metadata, and secondary lanes
build independent Agents seeded from their branch context. Text/message prompts
persist into the selected lane, advance only that lane pointer, return
`RunResultValue`, and share ordered lifecycle events plus lane-attributed
`pi.harness.run` spans. Local and remote `main` both resolve to
`d8b589f3532847042405c2a1a474b0e761c943a7a`.

Focused evidence for this checkpoint:

```text
cargo test -p pi-agent --offline harness::agent_harness::tests::secondary_lane_has_branch_context_and_shared_lifecycle -- --nocapture (1 passed)
cargo test -p pi-agent --offline --quiet (177 library tests plus integration targets)
cargo check -p pi-coding-agent --offline
cargo test -p pi-coding-agent --offline --lib modes::rpc::tests --quiet (41 passed)
cargo test -p pi-coding-agent --offline --lib interactive:: --quiet (33 passed)
cargo test -p pi-coding-agent --offline --test cli_print_parity --quiet (7 passed)
cargo fmt --all
git diff --check
```

This remains partial S-021/S-022. JSONL/RPC full harness ownership,
mode-specific golden envelopes, queue/control operations, complete persistence
coverage, and the upstream event registry remain open. `AGENTS.md` is still
pre-existing and untracked; it is not staged.

## Current ConfigSelector package/path parity checkpoint (S-034)

The remaining ConfigSelector audit is complete. Implementation commit
`974bd1b` was committed and pushed before this documentation refresh. Project package
overrides now match local sources across global/project settings bases, create
project-relative sources, preserve the upstream absent-vs-empty `autoload:
false` filter distinction, and remove empty project override objects when
cycling back to inherit. Top-level overrides also recognize resource metadata
base directories, and inherited resource identity uses canonical paths when
available.

Evidence:

```text
cargo test -p pi-coding-agent --offline --lib interactive::config_selector --quiet (11 passed)
cargo test -p pi-coding-agent --offline --lib interactive:: --quiet (36 passed)
cargo test -p pi-coding-agent --offline --test config_selector_pty --quiet (1 passed)
cargo check -p pi-coding-agent --offline
cargo fmt --all
git diff --check
```

S-034 is now complete. The implementation push was verified at
`974bd1b513d985b13907c58d3296842310cd5ad8`; `AGENTS.md` remains pre-existing
and untracked.

## Current interactive manual compaction checkpoint (partial S-033)

The interactive `/compact` divergence is resolved. Implementation commit
`514cca9` was committed and pushed before this documentation refresh. Automatic and
manual compaction now share one helper: automatic runs observe the context
threshold, while `/compact` forces preparation and accepts optional summary
instructions. Successful runs persist the compaction entry, replace the live
message context, reset cache accounting, and report a stable status banner;
empty history is a no-op.

Evidence:

```text
cargo test -p pi-coding-agent --offline --lib modes::interactive --quiet (13 passed)
cargo test -p pi-coding-agent --offline --lib interactive:: --quiet (37 passed)
cargo check -p pi-coding-agent --offline
cargo fmt --all -- --check
git diff --check
```

This is partial S-033. The implementation push was verified at
`9599298`; real-terminal/fixture coverage is now recorded for export, import,
share, trust, login/logout, new, fork/clone, tree, and reload. The interactive
`/resume` selector and broader S-056 matrix remain open.

## Current interactive slash-command PTY fixture checkpoint (S-033 complete)

The working tree now contains a real tmux PTY fixture for `/help`, `/export`,
`/import`, `/share`, `/trust`, `/login`, `/logout`, `/name`, `/copy`, `/new`,
`/resume`, `/fork`, `/clone`, `/tree`, and `/reload`. It drives the actual
interactive binary, seeds a second session, selects it through the real picker
keys, verifies transcript rehydration, substitutes temporary export/import
paths, verifies the HTML artifact, checks project trust after `/reload`, and
inspects alternate-screen/cursor cleanup in the raw pane log.

The first uncached interactive startup also exposed a lock-order bug in the
terminal image capability cache: capability detection tried to take a write
lock while still holding a read lock. The read guard is now released before
detection and storage, and the regression is covered in `pi-tui`.

Evidence:

```text
cargo test -p pi-coding-agent --offline --test interactive_slash_pty --quiet (1 passed)
cargo test -p pi-coding-agent --offline --lib interactive:: --quiet (37 passed)
cargo test -p pi-tui --offline terminal_image::tests::uncached_capability_detection_releases_read_lock_before_write --quiet (1 passed)
cargo check -p pi-coding-agent --offline
cargo fmt --all -- --check
git diff --check
node scripts/conversion-progress.mjs (60.24% = 100/166; 66 open)
```

S-033 is complete with the live command fixture evidence above. The broader
S-056 command-by-command terminal matrix remains open. The implementation
checkpoint `3b4d350` is committed and pushed; this follow-up keeps the handoff
hash and evidence aligned with `origin/main`.

## Current project-trust safety checkpoint (S-036 complete)

Trust resolution now precedes settings and resource loading in print, JSON,
RPC, interactive, config, and package entry points. The precedence is explicit
CLI override, saved directory decision, global `defaultProjectTrust`, then an
interactive startup prompt; unresolved headless `ask` remains untrusted.
The prompt runs before raw mode, saves its answer, and is covered by a real
tmux test. The trust store uses an exclusive sidecar lock, and resource-marker,
ancestor, and concurrent-write behavior are covered by focused tests.

Evidence:

```text
cargo test -p pi-coding-agent --offline --test cli_trust --quiet (7 passed)
cargo test -p pi-coding-agent --offline --test cli_commands --quiet (28 passed)
cargo test -p pi-coding-agent --offline --lib core::project_trust --quiet (7 passed)
cargo check -p pi-coding-agent --offline
cargo fmt --all -- --check
git diff --check
node scripts/conversion-progress.mjs (62.65% = 104/166; 62 open at that earlier checkpoint)
```

S-036 is complete; the implementation and documentation changes are ready for
the required local commit and immediate remote push. The broader S-056,
extension, provider, harness, server/client, and final-audit tasks remain open.

## Current S-032 committed checkpoint

Provider auth failures now receive the upstream actionable guidance at all
user-visible mode boundaries:

- Print mode rewrites terminal no-key/unauthorized errors to the provider
  `/login` plus docs message, while OAuth-capable providers receive the
  provider-specific re-authentication instruction.
- JSON message updates, interactive turn events/transcripts, and both detached
  and synchronous RPC event paths use the same formatter. Ordinary network
  errors remain unchanged, and assistant usage/model/stop-reason fields are
  preserved.
- The formatter has unit coverage and the RPC wire envelope has a dedicated
  regression. The focused interactive, RPC, JSON, print, check, formatter,
  formatting, and diff gates passed.

Focused evidence is recorded in the plan and ledger. The exact focused
commands were:

```text
cargo test -p pi-coding-agent --offline --lib core::auth_guidance::tests --quiet (4 passed)
cargo test -p pi-coding-agent --offline --lib modes::rpc::tests::rpc_provider_auth_errors_include_login_guidance --quiet (1 passed)
cargo test -p pi-coding-agent --offline --lib interactive:: --quiet (33 passed)
cargo test -p pi-coding-agent --offline --lib modes::rpc::tests --quiet (41 passed)
cargo test -p pi-coding-agent --offline --test cli_json_mode --quiet (2 passed)
cargo test -p pi-coding-agent --offline --test cli_print_parity --quiet (7 passed)
cargo check -p pi-coding-agent --offline
cargo fmt --all -- --check
git diff --check
```

The full workspace retry is not currently a code failure: while an unrelated
OpenHuman release build was active, one attempt was SIGKILLed during rustc,
the next hit `rust-lld`/SIGBUS, and a clean isolated retry hit `Disk quota
exceeded`. The temporary target was removed; re-run the workspace gate after
host build/cache pressure is clear.

The compiled self-update checkpoint was checked immediately after its commit:

- `git rev-parse HEAD`: `db97b89c0c7767ece2154b70a886e3f98fb151e5`
- `git ls-remote origin refs/heads/main`:
  `90a5b931591eaeaea20f1fd9c0d10f72d7614a7b`
- Immediate `git push origin main` failed with the exact blocker:
  `fatal: could not read Username for 'https://github.com': No such device or address`.
- `gh auth status` reports no authenticated GitHub host. The pre-commit hook
  therefore could not sync `.github/repository-description.txt` to GitHub.
  That historical push was blocked before GitHub authentication was repaired;
  the accumulated branch was later pushed at the parity checkpoint below.

The S-030 checkpoint was retried immediately after commit and remains blocked:

- `git rev-parse HEAD`: `7356dd37896043b54c554949b7dabec8bd325aae`
- `git ls-remote origin refs/heads/main`:
  `90a5b931591eaeaea20f1fd9c0d10f72d7614a7b`
- `git push origin main` failed with:
  `fatal: could not read Username for 'https://github.com': No such device or address`.
- `gh auth status --hostname github.com`: `You are not logged into any GitHub hosts.`

The startup-timing checkpoint was verified against the remote immediately
after commit:

- `git rev-parse HEAD`: `869ae6de6d451243b511409cf7de545819c55f6b`
- `git ls-remote origin refs/heads/main`:
  `90a5b931591eaeaea20f1fd9c0d10f72d7614a7b`
- Immediate `git push origin main` failed with:
  `fatal: could not read Username for 'https://github.com': No such device or address`.
- `gh auth status` still reports no authenticated GitHub host, so the updated
  repository description remains local only.

## Verification already completed

These checks passed during the session:

```bash
/home/mustbearnold/.cargo/bin/cargo check --workspace --offline
/home/mustbearnold/.cargo/bin/cargo test --workspace --offline
/home/mustbearnold/.cargo/bin/cargo test -p pi-agent --offline tools::image
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline run::tests
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline image_file_argument_is_attached_and_normalized
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test cli_print_parity
/home/mustbearnold/.cargo/bin/cargo test -p pi-client --offline
/home/mustbearnold/.cargo/bin/cargo test -p pi-server --offline
/home/mustbearnold/.cargo/bin/cargo check --workspace --offline
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline interactive::config_selector
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test config_selector_pty
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline
/home/mustbearnold/.cargo/bin/cargo test -p pi-tui --offline
/home/mustbearnold/.cargo/bin/cargo test -p pi-tui terminal_image --offline
/home/mustbearnold/.cargo/bin/cargo test -p pi-tui terminal::tests::cell_size_query_and_response_update_image_dimensions --offline
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent modes::rpc::tests --offline
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent remote_catalog_provider --offline
/home/mustbearnold/.cargo/bin/cargo test -p pi-agent --offline rich_loop_executes_tool_batch_and_emits_execution_events
/home/mustbearnold/.cargo/bin/cargo test -p pi-agent --offline terminate_hints_require_every_parallel_tool_to_opt_in
/home/mustbearnold/.cargo/bin/cargo test -p pi-agent --offline --test tools bash_tool_streams_partial_updates_through_agent_contract
/home/mustbearnold/.cargo/bin/cargo test -p pi-agent --offline --test tools edit_tool_registers_prepare_arguments_before_validation
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline rpc_runtime_control_commands_update_settings_and_state
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib core::version_check::tests
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib core::remote_catalog_provider::tests
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test cli_commands update_
/home/mustbearnold/.cargo/bin/cargo test -p pi-agent --offline --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-telemetry --offline --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-agent --offline tools::validation -- --nocapture
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test tool_contract -- --nocapture
/home/mustbearnold/.cargo/bin/cargo test -p pi-tui --offline --quiet
/home/mustbearnold/.cargo/bin/cargo test --workspace --offline --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test cli_json_mode --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib interactive:: --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib modes::rpc::tests::rpc_command_golden_transcript_matches_fixture
/home/mustbearnold/.cargo/bin/cargo fmt --all -- --check
git diff --check
node scripts/conversion-progress.mjs
```

The S-030 interactive cache-notice checkpoint is complete in the working tree:

- Interactive mode maintains serialized shadow entries while its JSONL writes
  remain deferred until exit. Cache misses are re-derived and injected after
  matching assistant timestamps, with the upstream 20k-token/$0.10 display
  thresholds and model-switch/idle labels.
- The settings selector now exposes and persists `showCacheMissNotices`.
  Footer usage reads the shadow entries so assistant, tool-result, and
  compaction/summary usage survives context replacement. `/session` now shows
  `Cache Re-billed` tokens, cost, and miss count; auto-compaction, `/clear`,
  new-session, resume, and import reset/reload the cache shadow appropriately.
- Evidence: `cargo test -p pi-coding-agent --offline --lib interactive::`
  (33 passed), `cargo test -p pi-coding-agent --offline --quiet` (455
  coding-agent unit tests plus integration targets),
  `cargo check --workspace --offline`, `cargo test --workspace --offline
  --quiet`, `cargo fmt --all`, and `git diff --check`.

S-030 is closed. The remaining CLI session-routing audit (S-026) and
S-021/S-022 harness ownership work are still open.

The S-029 install-telemetry checkpoint is complete in the working tree:

- `core::telemetry::report_install_telemetry` sends the separate anonymous
  install report and Pi user-agent through a bounded five-second best-effort
  transport, with transient transport/429/5xx retries. This is not the
  upstream release check removed from pi-rust. `PI_OFFLINE` short-circuits; the
  `PI_TELEMETRY` environment override and default-on `enableInstallTelemetry`
  setting gate the report.
- Interactive startup records the shipped version and launches the report in
  the background only for a fresh/version-changed install boundary. The
  settings selector now exposes `Install telemetry`; the endpoint has a
  `PI_INSTALL_TELEMETRY_URL` test seam.
- Evidence: `cargo test -p pi-coding-agent --offline --lib
  core::telemetry::` (7 passed), `cargo test -p pi-coding-agent --offline
  --quiet` (458 coding-agent unit tests plus integration targets),
  `cargo check --workspace --offline`, and `cargo test --workspace --offline
  --quiet`.

S-029 is closed. The remaining CLI session-routing audit (S-026) and
S-021/S-022 harness ownership work are still open.

The S-029 checkpoint was checked against the remote immediately after commit:

- `git rev-parse HEAD`: `3d6f1fc6dc047e983cdc12d6093b8423cb582441`
- `git ls-remote origin refs/heads/main`:
  `90a5b931591eaeaea20f1fd9c0d10f72d7614a7b`
- Immediate `git push origin main` failed with:
  `fatal: could not read Username for 'https://github.com': No such device or address`.
- `gh auth status --hostname github.com`: `You are not logged into any GitHub hosts.`

Remote parity was restored after GitHub device authentication:

- `gh auth status --hostname github.com`: logged in as `1deat0r` with `repo`
  scope.
- `gh auth setup-git --hostname github.com`: configured the GitHub CLI
  credential helper for the HTTPS remote.
- `git push origin main`: advanced `origin/main` from `90a5b93` to `a1c3e92`.
- `git rev-parse HEAD` and `git ls-remote origin refs/heads/main` both equal
  `a1c3e9268cd74d8992bcbd4c62f995ff20a5382d`.

The next implementation checkpoint must repeat the same commit, push, and
exact-hash verification sequence.

A full `cargo test --workspace --offline` passed after the image/read changes,
including 162 `pi-agent` unit tests, the coding-agent integration targets, 186
`pi-tui` unit tests, and all workspace doctests.

The latest full gate after the partial legacy-session integration checkpoint
passed: 176 `pi-agent` tests, 286 `pi-ai` unit tests, 451 `pi-coding-agent` unit tests plus
all integration targets (including the malformed-call and print-parity
fixtures), 186 `pi-tui` unit tests, and all workspace doctests.

The JSON-mode harness checkpoint also passed the full workspace gate after
restoring the successful RPC golden transcript. The focused JSON integration
test passes both normal faux streaming and the terminal no-key provider error
case, with both cases exiting successfully as required by JSON mode.

The compiled-binary self-update contract checkpoint is now superseded by the
current pi-rust distribution boundary above:

- `pi update --self` performs no upstream latest-release lookup. A compiled Rust
  executable reports a non-zero result instead of attempting to overwrite
  itself, with a centralized pi-rust repository/rebuild instruction.
- README.md documents rebuilding a source checkout with
  `cargo build --release -p pi-coding-agent` and replacing the installed
  binary through its owning mechanism. S-028 is closed as this intentional,
  user-visible distribution behavior.
- The focused package-command and offline update tests, workspace check/test,
  formatter, diff check, and progress checker are the evidence recorded for
  this checkpoint. GitHub description synchronization is attempted by the
  pre-commit hook when `gh` is authenticated.

The startup-timing compatibility checkpoint is complete in the working tree:

- `core::timings` recognizes the upstream exact `PI_TIMING=1` gate. The binary
  prints a user-facing warning with `/usr/bin/time -p` as the supported
  process-level fallback; the upstream timing namespaces remain an explicit
  Rust distribution non-port.
- The exact-one gate and fallback text are covered by
  `core::timings::tests::matches_upstream_exact_one_gate_and_fallback_text`.
  `PI_TIMING=1 ./target/debug/pi --version` prints the warning before
  `pi 0.84.2`. S-031 is closed; session migration integration, install
  telemetry, cache notices, and harness ownership remain open.

The earlier partial legacy-session integration checkpoint is complete; S-026
was closed by the CLI routing slice `711a25e`:

- Legacy v1/v2/v3 files are atomically converted before interactive session
  inventory and direct RPC switch_session loads. Fork/clone inherit the
  converted v4 source; /import keeps its existing copy-and-convert path.
- The three converter/file-system tests, direct RPC migration test, RPC golden
  transcript, interactive harness regression, CLI continue/resume/fork tests,
  interactive/RPC unit suites, workspace gate, formatter, and diff check are
  the evidence for the completed audit.

The legacy-session checkpoint was verified against the remote immediately
after commit:

- `git rev-parse HEAD`: `ef640ce09d60b158e2062a03bf31e12d7a4e3f74`
- `git ls-remote origin refs/heads/main`:
  `90a5b931591eaeaea20f1fd9c0d10f72d7614a7b`
- Immediate `git push origin main` failed with:
  `fatal: could not read Username for 'https://github.com': No such device or address`.
- `gh auth status` still reports no authenticated GitHub host; local and
  remote parity is not claimed.

## Earlier completed code changes

The one-shot auto-compaction milestone (#33–34 / S-025) is complete in the
working tree:

- The run path now provisions messages and compaction entries in memory,
  evaluates configured thresholds, calls the existing harness summarizer,
  rebuilds provider context from the retained compaction tail, and persists
  the compaction entry in JSONL.
- A binary faux-provider test forces the setting, verifies the second print
  turn continues, and verifies a `"type":"compaction"` JSONL entry. The
  focused print parity file has four passing tests.
- #33, #34, and S-025 are marked complete. The previous image/read checkpoint
  remains in `333ad84`; the earlier RPC fixtures are under
  `crates/pi-coding-agent/tests/fixtures/rpc/`.

The client reconnect/timeout milestone (#54 and #56) is also complete in the
working tree:

- `pi-client` exposes connection lifecycle state/listeners, reconnects through
  a fresh handshake with epochs and snapshot reset, invalidates session handles
  on disconnect, bounds handshake/request waits, ignores late responses for
  timed-out requests, and adds permanent `dispose()` alongside reconnectable
  `close()`.
- Fake Unix-socket tests cover reconnect lifecycle and snapshot refresh,
  handshake timeout, request timeout/late response, and disposal. The focused
  client suite has 4 passing tests; the dependent `pi-server` suite also passes.
- #55 lease reconciliation, #57 transport factories, #58 lease-churn E2E, and
  supplemental S-045/S-047 remain open; this is auxiliary T4 hardening, not a
  claim that the full upstream client library is complete.

The ConfigSelector interactive milestone (#59) is complete locally:

- The selector now supports search/filtering, circular/page navigation, global
  toggles, project inherit/load/unload cycling, inherited-resource indicators,
  package/top-level override persistence, and synchronous settings flushes.
- The focused selector suite has 8 passing tests, including deterministic
  global/project render snapshots; the full coding-agent suite
  has 436 unit tests plus its integration targets, and the full pi-tui suite
  has 186 passing tests. #59/#60 are complete; the focused PTY exercise is
  recorded in S-035, followed by #61/#62 and the remaining terminal probes.

The focused ConfigSelector PTY milestone (S-035) is also complete locally:

- `tests/config_selector_pty.rs` drives the real `pi config --approve` binary
  through tmux, asserts a visible global render snapshot and Unicode footer,
  survives pane resize, navigates/toggles global and project rows, verifies
  both settings files, and checks raw alternate-screen/cursor cleanup.
- A resize event now invalidates `pi-tui::Tree` differential state in both the
  config selector and main interactive loop, fixing the stale-frame behavior
  exposed by the PTY test.
- The focused PTY suite passes one test. The full interactive slash-command
  matrix remains S-056; alt-screen mode switching remains #61/#62.

The next alt-screen hardening checkpoint is also complete locally:

- `TerminalBackend` now exposes a monotonic screen epoch that changes on
  alternate-screen entry/exit, and `Tree` forces a full redraw when the epoch
  changes. This covers overlays or external prompts that temporarily replace
  the active screen without claiming the full regular/fullscreen renderer swap.
- The terminal transition test verifies idempotence and the expected epoch
  sequence; the PTY selector test remains green after the renderer change.

The AgentTool contract/update checkpoint is now complete locally and committed
in the latest checkpoint:

- `edit_tool` registers the upstream `prepareArguments` normalization before
  schema validation. The pinned source audit confirmed the other built-ins do
  not define non-identity prepare shims.
- The rich loop passes a scoped callback into tool execution and forwards
  updates through a channel while sequential/parallel calls run. Callbacks
  are gated after settlement; bash emits an initial update, 100ms-throttled
  output progress, and a final snapshot before `tool_execution_end`.
- Batch termination honors `AgentToolResult.terminate` only when every
  finalized tool opts in. Mixed parallel termination is covered by a focused
  unit test.
- Successful text results omit optional `details`, while error text preserves
  the upstream empty-object details shape. The built-in `ls`, `find`, and
  `grep` paths use the successful shape; read/write/edit/bash preserve their
  existing structured results.
- Parallel completion events are emitted in completion order while durable
  model-facing result messages remain in source order. Immediate preparation
  failures, mutable before-hooks, after-hook overrides, and late callback
  suppression have focused coverage.
- Bash fixtures cover coalesced progress, final truncation/full-output detail,
  and timeout after output. The registered seven-tool coding-agent fixture
  covers malformed read/write/edit/bash/ls/find/grep calls and verifies error
  payloads plus the absence of file mutation. #25–27, S-018, and S-020 are
  complete; S-024 remains open for broader schema-validator parity.

The termination-contract follow-up is included in the latest checkpoint:

- `ToolExecutionEnd` now carries the raw `AgentToolResult`, so lifecycle and
  RPC events preserve `terminate` and all optional result fields. The
  model-facing `ToolResultMessage` remains free of the internal hint.
- RPC prompt persistence correlates tool end events with their later
  tool-result message end and writes `terminate: true` on the JSONL message
  entry. This lets lane recovery reconstruct termination decisions.
- Mixed/all-terminating parallel batches and the RPC/session path are covered
  by the focused rich-loop and RPC suites; S-019 is complete.

The schema-validator parity follow-up is included in the latest checkpoint:

- Tool argument validation now covers local `$ref`, union combinators,
  tuple/constrained arrays, `additionalProperties`/`patternProperties`,
  enum/const, numeric and string bounds, common formats, and nullable optional
  normalization. Primitive coercion remains aligned with the upstream
  `Value.Convert`/plain-schema path.
- The validator fixture set covers these behaviors and the complete workspace
  gate is green; S-024 is complete. Remaining validator work, if discovered by
  future source audits, must be recorded as a new supplemental item rather
  than silently folded into this claim.

The panic-safe telemetry follow-up is included in the latest checkpoint:

- The in-memory telemetry adapter now catches callback panics, settles the
  span as an automatic error unless an explicit status was recorded, resumes
  the original panic, and keeps nested spans’ inner-first settlement order.
  Panic payloads remain opaque and late span operations remain inert.
- The TUI image fallback and Kitty capability fixtures now share their global
  capability lock, removing the workspace-only race seen during the first
  full gate.
- S-023 is complete. The next harness/runtime gaps are S-021 and S-022.

The print-path harness ownership slice is included in the latest checkpoint:

- Configured `AgentHarness` instances now own the rich `Agent`, provider/model
  configuration, tool preparation callbacks, and an in-memory main-lane
  transcript. The one-shot `run.rs` path prompts that harness, performs
  compaction against its transcript, updates Agent state at the compaction
  boundary, and replays the transcript into the durable JSONL session.
- Agent prompt messages are retained exactly once across sequential turns. A
  focused harness fixture proves the configured Agent output is persisted in
  chronological lane order; the four-test print-parity suite and full
  workspace gate remain green.
- This is a partial S-021 checkpoint, not closure: interactive, JSON, JSONL,
  and RPC modes still use their direct loop paths, and secondary lanes plus
  complete event/telemetry lifecycle wiring remain S-021/S-022 work.

The harness lifecycle/telemetry slice is included in the latest checkpoint:

- The configured print-path harness now consumes `HarnessTelemetryContext`
  through an async-safe span boundary, emits `run_start` and `run_end` in
  order, and records a settled `pi.harness.run` span with the required
  session/lane/operation attributes. Session-write failures mark the span
  explicitly as errors.
- The focused harness fixture asserts the exact event sequence, span name,
  required attributes, settled status, and `run_start`/`run_end` span events.
  The full workspace gate remains green; the shared mode bridge described
  below now applies the same boundary to the remaining loops. This is partial
  S-022 because golden wire checks remain.

The shared mode lifecycle bridge is included in the latest checkpoint:

- `run_with_harness_lifecycle` now wraps the JSON mode, interactive turns,
  detached RPC prompt workers, and synchronous RPC prompt execution. Existing
  mode-specific events remain in their established order/payload shape, while
  each run receives the same ordered harness lifecycle and async span boundary.
- The adapter fixture asserts `run_start`, nested event, `run_end` ordering and
  the required operation attributes; RPC’s focused 39-test suite and print
  parity remain green. The full workspace gate passes with 176 pi-agent tests.
  This is still partial S-022: mode-specific golden lifecycle envelopes,
  persistence, and secondary-lane assertions remain.

The JSON-mode harness ownership slice is included in the latest checkpoint:

- `--mode json` now creates a memory-backed `AgentHarness`, configures its
  registered tools/model/system prompt, and emits the harness-captured rich
  message updates. Terminal provider errors are preserved as JSON
  `message_update` events; terminal `done` remains omitted from RPC's existing
  successful golden transcript.
- `cli_json_mode` passes both its faux success case and its no-key terminal
  error case; the full workspace gate remains green. This is partial S-021 and
  S-022: interactive/JSONL/RPC full harness ownership, lifecycle goldens,
  persistence, and secondary lanes remain open.

The interactive turn harness ownership slice is the next checkpoint:

- Interactive `stream_turn` now creates a configured memory-backed
  `AgentHarness`, seeds it from the current transcript, preserves all built-in
  tool preparation callbacks, and forwards rich stream updates to the existing
  TUI callback. The runtime continues to own durable JSONL persistence and
  session-switch behavior.
- The focused interactive harness test and the full 446-test coding-agent
  suite plus the full workspace gate pass. This remains partial S-021/S-022:
  JSONL/RPC full harness
  ownership, lifecycle goldens, persistence, and secondary lanes remain open.

The follow-up bash harness integration is included in the latest checkpoint:

- The registered bash tool now runs through `StdExecutionEnv` and
  `execute_shell_with_capture`, preserving structured truncation metadata and
  full-output temp-file paths while retaining the legacy direct `run_bash`
  API. The focused full-output, shell-capture, abort, coalescing, and timeout
  fixtures all pass; remaining harness work is tracked under S-021/S-022/S-023.

The RPC runtime audit is now complete locally:

- A direct test sends `set_auto_compaction`, `set_auto_retry`,
  `set_steering_mode`, and `set_follow_up_mode`, then verifies live flags,
  persisted settings, queue modes, and the `get_state` response. Existing
  stream/compaction/retry/provider-setting and queue-drain tests cover the
  downstream behavior.
- #88 is marked complete. The update/version and model-catalog slice closes
  #89–90; S-016/S-017 remain open for atomic-write and broader provider-shape
  fixture expansion.

The update/version and model-catalog checkpoint is now complete locally:

- The earlier upstream latest-release implementation is superseded. Current
  pi-rust startup and `pi update --self` perform no upstream release lookup;
  they report the pi-rust repository/rebuild boundary instead of showing an
  upstream update notice or replacing the compiled binary.
- `pi update --models` refreshes built-in providers concurrently within the
  upstream 15-second bound, retries transient HTTP statuses, persists
  ETag/Last-Modified/freshness state, handles 304/404/501, and keeps the
  `PI_MODEL_CATALOG_URL` seam for mock tests. The user-facing success/error
  lines now match upstream.
- The current update tests cover the no-release-request boundary and the
  non-zero repository/rebuild instruction. Catalog tests cover persisted
  success and three-attempt transient failure behavior; the binary update test
  covers self-update failure without an upstream request.
- #89 and #90 are marked complete. The supplemental S-016/S-017 rows remain
  open deliberately.

## Major parity work already present

The current source includes substantial ports beyond the original baseline:

- exhaustive conversion ledger and progress checker;
- provider/auth/OAuth and Codex WebSocket work;
- remote model catalog refresh, freshness, ETag/304, offline, and runtime
  model merge behavior;
- CLI flags, JSON mode, print-mode sequencing/error behavior, resource
  loading, project trust, diagnostics, telemetry, sessions, export, and
  footer usage totals;
- RPC queue/abort/steer/follow-up/compaction/retry scaffolding and session
  tree queries;
- config-selector data model/resource producer and a partial interactive
  component;
- TUI word navigation, terminal capability probing, image sizing helpers,
  editor/autocomplete, markdown, and alt-screen foundations;
- server/client/session-backend changes and extensive fixture/tests.

The ledger deliberately keeps several items open because “code exists” is not
the same as proven 1:1 behavior. In particular, do not silently check off
items just because a similarly named Rust module exists.

## Recommended next sequence

1. Continue with S-013 GitHub Copilot OAuth refresh, enterprise-domain,
   token-exchange, and expired-credential parity.
2. Keep `CONVERSION-LEDGER.md`, `PLAN.md`, and this handoff synchronized;
   only mark a task complete with an evidence tier and exact command/fixture.

## Useful source references

- Upstream authoritative clone: `upstream_pi/`
- Upstream pinned target: `5cd93f688aaab89dbb6dfa4aca535f21796ae185`
- Rust cargo binary: `/home/mustbearnold/.cargo/bin/cargo`
- Primary RPC implementation: `crates/pi-coding-agent/src/modes/rpc.rs`
- One-shot path: `crates/pi-coding-agent/src/run.rs`
- TUI terminal/image paths: `crates/pi-tui/src/terminal.rs`,
  `crates/pi-tui/src/terminal_image.rs`, `crates/pi-tui/src/tui.rs`

## Session discipline

The operator has requested commit + push after each checkpoint. GitHub device
authentication and the HTTPS credential helper are now configured; the
accumulated branch is verified at parity with `origin/main` at `50c2103`.
Before continuing, inspect
`git status`, read this handoff, run the progress checker, and treat all
existing dirty changes as user-owned work.

## Current checkpoint — 2026-08-24 — S-011 Vertex ADC parity

The S-011 Google Vertex credential-file and provider-auth slice is complete.
`crates/pi-ai/src/api/google_vertex.rs` now supports explicit/default ADC file
selection, service-account JWT exchange with file `token_uri` and `scopes`,
authorized-user refresh-token exchange with file credentials, and API-key
publisher routing without project/location resolution. The implementation
keeps metadata-server, workload-identity, and external-account ADC sources
outside this file-auth slice and documents that boundary.
`crates/pi-ai/src/providers/all.rs` now matches stored credential environment,
ambient API-key, ADC file/project/location, and source-label precedence.

Evidence tier: **mock**.

```text
RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-ai --offline --lib google_vertex --quiet
18 passed; includes adc_path_explicit_value_wins_over_default_home,
adc_service_account_uses_token_uri_and_configured_scopes,
adc_authorized_user_refreshes_with_file_credentials, and
stream_api_key_uses_publisher_path_without_project_or_location.

RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-ai --offline --lib google_vertex_provider --quiet
4 passed; includes stored ADC environment, missing project/location,
ambient API-key precedence, and missing explicit ADC no-fallback fixtures.

RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo check -p pi-ai --offline
RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc RUSTFMT=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustfmt /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo fmt --all -- --check
git diff --check
```

The unlazy gate status check reports all 33 gates met; G30/G31/G32 are the
focused test/static gates and G33 records the progress checker. The conversion
checker reports `65.06% (108/166; 58 open)`. The focused implementation
checkpoint was committed as
`b18af9a895f9cb287ab47f0816d67dc20b256fe3` and pushed to `origin/main`;
`git rev-parse HEAD` and `git ls-remote origin refs/heads/main` matched.
The next dependency-safe task is S-012 Cloudflare AI Gateway account/gateway
binding and base URL/header precedence parity.

## Current checkpoint — 2026-08-24 — S-012 Cloudflare gateway binding parity

S-012 is implemented in `crates/pi-ai/src/api/cloudflare.rs`. The new
runtime-neutral gateway-binding boundary validates same-origin configured
prefixes, applies WHATWG-compatible literal and percent-encoded dot-segment
normalization while preserving empty path segments, requires JSON POST bodies,
extracts the provider/endpoint/query contract, lowercases forwarded headers,
strips `content-length`, `host`, and the gateway auth sentinel, rejects
requests that cannot be represented, forwards the optional `Arc<AtomicBool>`
cancellation handle, and dispatches the translated request through an injected
binding trait. Cloudflare auth preserves per-field stored credential
precedence, scoped account/gateway environment, inline upstream `Authorization`
precedence, and gateway base-URL resolution.

Evidence tier: **mock**.

```text
RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-ai --offline --lib cloudflare --quiet && printf 'S012_CLOUDFLARE_BINDING_TESTS_PASS\n'
18 passed; output marker: S012_CLOUDFLARE_BINDING_TESTS_PASS

RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-ai --offline --lib cloudflare_provider --quiet && printf 'S012_CLOUDFLARE_PROVIDER_TESTS_PASS\n'
5 passed; output marker: S012_CLOUDFLARE_PROVIDER_TESTS_PASS

RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo clippy -p pi-ai --offline --all-targets -- -D warnings
Finished `dev` profile; zero diagnostics

RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc RUSTFMT=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustfmt /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo fmt --all -- --check && git diff --check && printf 'S012_STATIC_CHECKS_PASS\n'
output marker: S012_STATIC_CHECKS_PASS

node scripts/conversion-progress.mjs
Conversion progress: 65.66% (109/166; 57 open)
```

Independent read-only parity review returned **APPROVE** after checking the
current Cloudflare source, deterministic fixtures, and upstream binding/auth
contract; no patch-introduced blockers remain.

No live Cloudflare account, Workers runtime, or network request was used.
The binding trait deliberately leaves response handling to the host runtime;
the recording adapter proves the request and cancellation contract without
adding a second HTTP runtime to `pi-ai`.

S-012 implementation and documentation are committed as
`617e39ce030bfb26598f4305a60d0e7de1e29bcc` and pushed; the local and
`origin/main` hashes matched in the required verification. The next
dependency-safe action is S-013 GitHub Copilot OAuth refresh and
enterprise-domain/token-exchange parity.
## Historical checkpoint — 2026-08-25 — full-conversion tree established

The existing full-conversion goal is active and is being resumed with scoped
sub-agent work under `.unlazy/full-conversion-20260825/`. Startup documents
were read, the repository checker was run, and the authoritative result is:

```text
Conversion progress: 65.66% (109/166; 57 open)
```

No conversion-ledger item changed in this setup checkpoint. The depth tree,
shared contracts, root acceptance gates, and disjoint leaf ledgers are in the
scoped unlazy directory. The first ready leaves cover provider residuals,
extensions, server/client libraries, TUI, evals, and source inventory; harness,
PTY, integration, parity, and final-audit leaves wait on their declared
dependencies. The driver owns `CONVERSION-LEDGER.md`, `PLAN.md`, `HANDOFF.md`,
`README.md`, and final release/audit documentation. Agents must return changed
paths and evidence without committing or pushing shared state.

The working tree retains the pre-existing untracked `AGENTS.md`; `.unlazy/`
runtime state is ignored. Next action: inspect and approve the scoped gate
oracles, claim the first-wave leases, then dispatch the ready leaves.

## Historical checkpoint — 2026-08-25 — implementation wave integrated

Repository state at handoff: branch `main`; `HEAD` and `origin/main` both
`6f243b9a0083d5d6e8edf7f05943f3dbeb0fec88` before this dirty working-tree
checkpoint. Agents are still editing disjoint C1/D2/E2/F1 paths; no active
agent committed or pushed. The pre-existing untracked `AGENTS.md` remains
untouched.

Progress checker:

```text
Conversion progress: 68.67% (114/166; 52 open)
```

Completed and independently rechecked in this wave:

- S-013 Copilot OAuth, S-014 Anthropic OAuth/provider edges, S-016/S-017
  catalog refresh/merge, and #75 proxy bootstrap are ledger-checked with
  unit/mock evidence.
- Extension command/hook/renderer/failure-isolation fixtures: 8 passed;
  extension library tests: 26 passed. The external Node/Bun loader remains a
  documented divergence from upstream in-process TypeScript execution.
- pi-server: 21 tests passed; strict all-target clippy passed after fixing
  lifecycle and protocol lint blockers.
- pi-protocol: 22, 9, and 15 offline test targets passed; strict all-target
  clippy passed.
- pi-tui: 187 tests passed; strict all-target clippy and manifest formatting
  passed.
- Workspace `cargo check --workspace --offline` passed at this checkpoint.

Exact focused validation commands run:

```text
/home/mustbearnold/.cargo/bin/cargo test -p pi-ai --offline --test copilot_oauth_parity --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test copilot_oauth_parity --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-ai --offline --test anthropic_provider_parity --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-ai --offline --test anthropic_stream --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-ai --offline --test model_catalog_parity --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test model_catalog_parity --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib core::http_dispatcher --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test extensions_parity --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-server --offline --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-protocol --offline --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-tui --offline --quiet
/home/mustbearnold/.cargo/bin/cargo check --workspace --offline
git diff --check
node scripts/conversion-progress.mjs
```

Next dependency-safe action: finish the active C1/D2/E2/F1 leaves, then
dispatch provider-matrix and independent final reviewers. Do not claim full
conversion completion until pi-agent/coding-agent clippy, the full workspace
tests, parity suite, release matrix, and S-065/S-066 audit gates pass.

## Historical checkpoint — 2026-08-25 — eval metrics and pushed wave synchronized

The completed implementation/eval wave is pushed at commit
`b95f3b4c3b049c83f877f02eba15b4396c596b9a`; `git rev-parse HEAD` and
`git ls-remote origin refs/heads/main` both return that hash. The worktree now
contains only active C1/D2/E2 changes; the pre-existing untracked `AGENTS.md`
remains untouched.

Progress checker:

```text
Conversion progress: 71.08% (118/166; 48 open)
```

S-058 and S-059 are now ledger-checked with unit/mock evidence. F1 reports
the exact pi-evals test, formatting, clippy, and faux fixture commands passed;
session JSONL accounting recorded input 1246, output 20, total 1266, and the
faux extension boundary is an explicit schema-1 diagnostic fixture. The F1
lease was released after verification.

Exact additional validation commands:

```text
/home/mustbearnold/.cargo/bin/cargo test -p pi-evals --offline --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-evals --offline --test session_usage --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-evals --offline --test extensions --quiet
/home/mustbearnold/.cargo/bin/cargo clippy -p pi-evals --offline --all-targets -- -D warnings
/home/mustbearnold/.cargo/bin/cargo fmt -p pi-evals -- --check
git diff --check
node scripts/conversion-progress.mjs
```

Next dependency-safe action: finish C1/D2/E2, dispatch provider-matrix
fixtures, then run the full workspace tests/clippy, parity suite, release
matrix, and independent final-audit review. Full conversion remains open.

## Historical checkpoint — 2026-08-25 — provider/client/harness/reconnect wave

The last synchronized pushed baseline before this worktree wave is
`486a5bb50ce1444d3ab3086f6753e0a549ba8864` on `main`; local/remote hashes
matched at that checkpoint. The worktree contains completed B4/D2/C1/D3
changes plus active D1b/E3/R3/F2 work. The pre-existing untracked `AGENTS.md`
remains untouched.

Progress checker:

```text
Conversion progress: 77.71% (129/166; 37 open)
```

Completed evidence in this wave:

- B4: `cargo test -p pi-ai --offline --test provider_matrix --quiet` — 4
  tests passed; 50 text provider/API pairs, images, errors, usage, and five
  no-API controls are fixture-indexed.
- D2: `cargo test -p pi-client --offline --all-targets --quiet` — 7
  integration tests passed; strict client clippy, formatting, and the live
  server session-handle compatibility test (2 passed) are green.
- C1: `cargo test -p pi-agent --offline --lib harness --quiet` — 100 passed;
  `cargo test -p pi-coding-agent --offline --lib --quiet` — 469 passed;
  `harness_modes` passed; cargo check and owned rustfmt/diff checks passed.
- D3: `cargo test -p pi-server --offline --test reconnect_lease_e2e --quiet`
  — 4 live local-Unix tests passed.

Exact commands additionally run:

```text
/home/mustbearnold/.cargo/bin/cargo clippy -p pi-client --offline --all-targets -- -D warnings
/home/mustbearnold/.cargo/bin/cargo fmt -p pi-client -- --check
/home/mustbearnold/.cargo/bin/cargo test -p pi-server --offline --test session_handle_e2e -- --nocapture
/home/mustbearnold/.cargo/bin/cargo check -p pi-agent -p pi-coding-agent --offline
node scripts/conversion-progress.mjs
git diff --check
```

Active work: D1b must reach the 30-plus server conformance matrix; E3 owns
remaining TUI behavior; R3 owns the measured pi-agent/coding-agent clippy
backlog; F2 owns CLI/RPC/session/settings/provider/telemetry parity fixtures.
After those leaves, rerun the PTY matrix, full workspace tests/clippy, release
build, and independent final audit. Full conversion remains open.

## Historical checkpoint — 2026-08-25 — TUI behavior and clippy cleanup synchronized

The last synchronized pushed baseline before this checkpoint is
`0dd35c27d788f59c36582df5671f34747c1cafa1` on `main`; local and remote hashes
matched there. This checkpoint integrates the completed E3 TUI behavior slice
and R3 strict-clippy cleanup. The pre-existing untracked `AGENTS.md` remains
untouched; active server, PTY, and parity paths remain unstaged for their
owners.

Progress checker:

```text
Conversion progress: 93.37% (155/166; 11 open)
```

Evidence synchronized here:

- E3: `/home/mustbearnold/.cargo/bin/cargo test -p pi-tui --offline --quiet`
  — 203 passed, including terminal capability, autocomplete/editor,
  SettingsList, Markdown, alt-screen, tmux, and cleanup fixtures; strict
  pi-tui clippy, owned formatting, and `git diff --check` also passed.
- R3: `/home/mustbearnold/.cargo/bin/cargo clippy -p pi-agent --offline
  --all-targets --no-deps --message-format=short -- -D warnings` and the
  corresponding `pi-coding-agent` command both exited 0. The targeted
  pi-agent and coding-agent tests and `git diff --check` are green; full
  workspace formatting remains gated on the active server files settling.
- E2b: `/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline
  --test interactive_full_matrix --quiet` — 3 live PTY cases passed, and
  `interactive_slash_pty` passed 1 case; stty, ANSI, tmux, resize,
  Ctrl-C/Ctrl-D, and exact diagnostics were asserted under tmux. Owned
  rustfmt and diff checks passed.
- D1b: `/home/mustbearnold/.cargo/bin/cargo test -p pi-server --offline
  --quiet` — 55 total tests passed, including 32 expanded conformance cases;
  strict all-target clippy, package formatting, and the 4-case reconnect
  lease suite also passed. The server harness now covers deferred operations,
  malformed/handshake errors, snapshots, lifecycle, queues, and cleanup.

Active next work is the extension-runtime decision and final audit. The full
offline parity suite is green for all 37 declared offline branches; one
credentialed live provider smoke is explicitly not-run and is not claimed as
pass evidence. Full conversion remains open pending S-027, S-001/S-004,
S-065/S-066, and #97–100.

## Historical checkpoint — 2026-08-25 — parity matrix and CLI/auth blockers closed

The last synchronized pushed baseline before this checkpoint is
`8a3c1dc84c59d1125b3b4ed12eefb7c32a2b1c40` on `main`; local and remote hashes
matched there. This checkpoint integrates the production CLI/auth fixes and
the complete F2 parity fixture matrix. The pre-existing untracked `AGENTS.md`
remains untouched.

Progress checker:

```text
Conversion progress: 93.37% (155/166; 11 open)
```

Exact evidence:

- `cargo test -p pi-coding-agent --offline --test cli_commands --quiet` — 30
  passed; the args unit target passed 23 tests. Help now includes
  `--mode <mode>`, unknown flags match upstream exit/text, and auth commands
  run without nested-runtime panics.
- `node scripts/parity-suite.mjs` — `40 passed, 0 failed, 1 not-run, 41
  total`; all 37 offline branches passed, including 51 provider variants.
- `cargo test --workspace --offline --quiet` exited 0; strict coding-agent
  clippy, formatting, `node --check scripts/parity-suite.mjs`, and
  `git diff --check` passed.

The parity fixture/script paths are committed in the current checkpoint.
Extension bridge edits remain separate and unstaged; they provide partial
S-027 evidence but do not yet reproduce jiti virtualization, host actions,
native provider callbacks, or live tool execution.

## Historical checkpoint — 2026-08-25 — extension bridge boundary recorded

The extension leaf `C2b` completed its owned gates. The persistent Node/Bun
JSONL bridge keeps the JavaScript factory alive and routes async command, hook,
renderer, JSON-provider, loader-error, and failure-isolation callbacks. Exact
evidence is `cargo test -p pi-coding-agent --offline --test
extensions_parity --quiet` (11 passed), `cargo test -p pi-coding-agent
--offline --lib core::extensions --quiet` (26 passed), strict coding-agent
clippy, package formatting, and `git diff --check`.

S-027 deliberately remains open: the bridge is not an embedded jiti runtime
and does not yet reproduce module virtualization, host actions, native provider
callbacks, or live tool execution. The next dependency-safe action is a fresh
broader runtime-closure review/implementation pass, followed by the final
source/TODO reconciliation and release gates.

## Historical checkpoint — 2026-08-25 — extension bridge C2c

The C2c implementation pass extended the persistent Node/Bun bridge with live
external-tool execution, typed host-action dispatch, local `.js`/`.ts` imports,
request timeouts that terminate the child, and explicit pre-bind initialization
errors. The focused fixture now covers all modeled host-action methods,
including session/tool/model/thinking-level state transitions. C2c evidence:

```text
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test extensions_parity --quiet
13 passed; 0 failed
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib core::extensions --quiet
26 passed; 0 failed; 443 filtered out
/home/mustbearnold/.cargo/bin/cargo clippy -p pi-coding-agent --offline --all-targets -- -D warnings
/home/mustbearnold/.cargo/bin/cargo fmt -p pi-coding-agent -- --check
git diff --check
node scripts/conversion-progress.mjs
Conversion progress: 93.37% (155/166; 11 open)
```

No ledger item changed in this checkpoint: S-027 remains intentionally open
because the bridge still does not reproduce pinned jiti/module virtualization,
native provider callback ABI, Bun-specific runtime behavior, or full agent-loop
tool integration. The next dependency-safe action is an independent release,
clean-room, and source/TODO audit, with a further S-027 implementation leaf if
the reviewer identifies an actionable parity gap.

## Historical checkpoint — 2026-08-25 — extension bridge hardening after review

The independent review identified and the follow-up patch addressed four
runtime-boundary defects: production-shaped loading now binds the same shared
runtime captured by the bridge; upstream synchronous getters use per-callback
host snapshots while `setModel` remains asynchronous; runtime invalidation
rejects and closes stale bridge callbacks; and host dispatch is panic-safe,
stdout-protected, frame-bounded, and re-entry guarded. The parity fixture uses
`load_extensions_with_host_actions` and exercises the synchronous API without
`await`.

Exact validation for this checkpoint:

```text
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test extensions_parity --quiet
13 passed; 0 failed
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib core::extensions --quiet
26 passed; 0 failed; 443 filtered out
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --quiet
469 library tests passed; all package integration targets passed
/home/mustbearnold/.cargo/bin/cargo clippy -p pi-coding-agent --offline --all-targets -- -D warnings
/home/mustbearnold/.cargo/bin/cargo fmt -p pi-coding-agent -- --check
git diff --check
node scripts/conversion-progress.mjs
Conversion progress: 93.37% (155/166; 11 open)
```

No ledger item changed: S-027 remains open because the Rust CLI/modes do not
yet load and bind extension runners in the production agent-session path, and
the bridge still lacks pinned jiti/module virtualization, native provider
callback ABI, Bun-specific verification, and full AgentToolResult/signal/update
integration. The next dependency-safe action is a dedicated production
extension integration leaf, followed by independent release and clean-room
audit gates.

## Historical checkpoint — 2026-08-25 — production extension mode integration

The production extension leaf is now implemented across the one-shot print,
JSON-event, RPC, and interactive mode paths. A shared
`core/extensions/integration.rs` adapter owns the mode-scoped loader policy,
host-action snapshot/state, live `AgentTool` conversion, tool-result mapping,
and runtime invalidation boundary. Each mode now honors `--no-tools`,
`--no-builtin-tools`, and `--no-extensions`/explicit `-e` paths, publishes its
tool/command catalog to extension getters, and invalidates external bridge
processes on shutdown. The print path also runs the `before_agent_start`
system-prompt hook before harness creation.

Exact validation for this checkpoint:

```text
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib core::extensions::integration::tests --quiet
1 passed
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib modes::json_event --quiet
1 passed
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib modes::interactive --quiet
16 passed
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib modes::rpc::tests --quiet
41 passed
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test extensions_parity --quiet
13 passed
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib --quiet
471 passed
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --quiet
all package targets passed
/home/mustbearnold/.cargo/bin/cargo clippy -p pi-coding-agent --offline --all-targets -- -D warnings
/home/mustbearnold/.cargo/bin/cargo fmt -p pi-coding-agent -- --check
git diff --check
node scripts/conversion-progress.mjs
Conversion progress: 93.37% (155/166; 11 open)
```

S-027 remains intentionally open: the production mode/tool boundary is now
evidenced, but pinned jiti/module virtualization, native provider callbacks,
Bun-specific verification, and complete AgentToolResult/signal/update/
active-tool semantics are not yet 1:1. The next dependency-safe action is to
close those residual extension semantics or record a deliberate proven
replacement, then execute the release, clean-room, source/TODO, denominator,
and independent-reviewer gates (#97–100, S-001–S-004, S-065–S-066).

## Historical checkpoint — 2026-08-25 — progress gate, extension boundaries, and release verification

The current worktree extends the pushed production extension integration
checkpoint `41d3107c2e33ef9eeb5ec7fb65581fe5ac3c8346`. The pre-existing
untracked `AGENTS.md` remains untouched and unstaged.

The authoritative checker now reports:

```text
Conversion progress: 95.18% (158/166; 8 open)
```

Completed evidence in this checkpoint:

- S-003 is closed. `node --test scripts/conversion-progress.test.mjs` passes
  7 tests covering positive output, malformed status/IDs, duplicate IDs, and
  an empty ledger; the checker now rejects malformed checklist-looking rows
  instead of silently ignoring them.
- The extension bridge now uses Node native type stripping for ordinary
  `.ts`/`.mts`/`.cts` imports when advertised, rejects TSX without an explicit
  transpiler, and emits deterministic diagnostics for known upstream virtual
  modules. Loader tests pass 15 cases. The live AgentTool adapter now maps
  nested/flat result fields, text content, error boundaries, and deduplicated
  added-tool names; integration tests pass 3 cases.
- #97 is closed with live release evidence. `/home/mustbearnold/.cargo/bin/cargo
  build --workspace --release --offline` completed successfully. The full
  release target suite passes with `/home/mustbearnold/.cargo/bin/cargo test
  --workspace --release --offline --quiet -- --test-threads=2`; the 476-test
  coding-agent library target, 203-test pi-tui target, and all other targets
  are green. The bounded test concurrency is intentional because the real
  tmux/PTY fixtures can be starved by the host's unbounded default parallel
  test fan-out; the default run was not accepted as evidence after its
  reproducible `/thinking` timeout.

Exact focused validation:

```text
node --test scripts/conversion-progress.test.mjs
node scripts/conversion-progress.mjs
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib core::extensions::loader::tests --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib core::extensions::integration::tests --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --quiet
/home/mustbearnold/.cargo/bin/cargo clippy -p pi-coding-agent --offline --all-targets -- -D warnings
/home/mustbearnold/.cargo/bin/cargo fmt -p pi-coding-agent -- --check
git diff --check
/home/mustbearnold/.cargo/bin/cargo build --workspace --release --offline
/home/mustbearnold/.cargo/bin/cargo test --workspace --release --offline --quiet -- --test-threads=2
```

Remaining open rows are S-001, S-002, S-004, S-027, S-065, S-066, and
#98–100. The next dependency-safe action is the independent source/TODO
reviewer gate, followed by the full real-binary environment/on-disk/RPC audit
and clean-room run. The progress checker must be rerun after each ledger edit.

## Historical checkpoint — 2026-08-25 — Session-13 reviewer preparation

The Session-13 preparation item (#98) is now complete in `PLAN.md`. It adds
the pinned upstream revision, current checker value, evidence-tier matrix,
explicit S-027 residual review conditions, reviewer independence requirement,
and the exact release/real-binary final-gate commands. The ledger and root
documents are synchronized at:

```text
Conversion progress: 95.18% (158/166; 8 open)
```

`node scripts/conversion-progress.mjs` is the exact checker command. The
remaining rows are S-001, S-002, S-004, S-027, S-065, S-066, #99, and #100.
The next action is to consume the fresh source/TODO/full-surface audit reports
and obtain an independent reviewer verdict before closing S-004.

## Historical checkpoint — 2026-08-25 — extension context-action parity

The S-027 implementation pass added the safe external-tool context slice:
Node/Bun extension tool callbacks now receive synchronous host snapshots and
host actions for session name, active/all tools, commands, thinking level,
messages, entries, labels, and awaitable model selection. The live fixture
also proves host-action queues and `addedToolNames` propagation. S-027 remains
open for jiti/module virtualization, native providers, Bun-specific coverage,
the broader model/session/UI/compaction/signal context, and mid-execution
signal/update forwarding.

Exact evidence:

```text
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib core::extensions::loader::tests --quiet
15 passed
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib core::extensions::integration::tests --quiet
3 passed
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test extensions_parity --quiet
13 passed
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib core::extensions --quiet
32 passed
/home/mustbearnold/.cargo/bin/cargo clippy -p pi-coding-agent --offline --all-targets -- -D warnings
/home/mustbearnold/.cargo/bin/cargo fmt -p pi-coding-agent -- --check
git diff --check
```

No ledger checkbox changed in this code-only S-027 increment; the checker
remains `Conversion progress: 95.18% (158/166; 8 open)`. The next action is to
review and integrate the independent S-001, S-002, and #99 reports before the
fresh S-004 reviewer gate.

## Historical checkpoint — 2026-08-25 — inventory closure and callback-context parity

S-001, S-002, and #99 are now checked in the ledger with the current source/
export census, documentation reconciliation, and isolated real-binary audit
artifacts. The latest extension-context implementation also covers
model/scoped-model snapshots, idle/trust state, context usage/system prompt
access, callback-scoped signal/abort, compact/shutdown queues, and ordered
mid-execution tool updates. The focused Rust suites pass 4 integration tests
and 14 external-extension parity tests. S-027 remains open for pinned
jiti/module virtualization, native provider callback execution and live model
registry wiring, and Bun-specific verification.

Current authoritative checker:

```text
Conversion progress: 96.99% (161/166; 5 open)
```

Remaining rows are S-004, S-027, S-065, and S-066. The current worktree
also contains the uncommitted documentation/ledger reconciliation and the
reviewed extension-context code; the next checkpoint must run the full narrow
extension tests, inspect the native-provider leaf, then commit and push with
local/remote hash verification. The pre-existing untracked `AGENTS.md` stays
unstaged.

## Historical checkpoint — 2026-08-25 — native-provider bridge protocol

The extension bridge now accepts native provider objects, retains callback
metadata, invokes async/iterable `stream`/`streamSimple` callbacks with
JSON-safe model/context/options values, and returns deterministic raw event
sequences. The new fixture proves `streamSimple` callback input and
start/text/done events; the external parity suite is now 15/15 and loader
tests remain 15/15. This is a partial S-027 increment: typed conversion into
`pi-ai::ProviderStreams`, live `Models` registration/mode wiring, jiti/module
virtualization, and Bun verification remain open.

The authoritative checker remains:

```text
Conversion progress: 96.99% (161/166; 5 open)
```

This bridge-only increment is ready for its own focused commit after
`cargo fmt --all -- --check`, strict coding-agent clippy, extension tests, and
`git diff --check`; do not mark S-027 complete until the typed provider and
runtime-boundary gates are independently evidenced.

## Historical checkpoint — 2026-08-25 — typed native-provider adapter

The native-provider boundary now retains non-callback provider definitions and
adapts them into typed pi-ai `ProviderStreams`/`Models`. The adapter maps the
Rust context and stream options into the upstream callback shape, converts
start/text/thinking/tool/done/error events, rejects malformed or unterminated
event sequences with typed error streams, and registers declared provider
models. The external fixture now exercises `Models::stream_simple` end to end;
S-027 remains open for production mode/model-registry wiring, pinned
jiti/module virtualization, and Bun verification.

Exact validation passed:

```text
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test extensions_parity --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib core::extensions::integration::tests --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib core::extensions --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib --quiet -- --test-threads=2
/home/mustbearnold/.cargo/bin/cargo clippy -p pi-coding-agent --offline --all-targets -- -D warnings
/home/mustbearnold/.cargo/bin/cargo fmt --all -- --check
git diff --check
node scripts/conversion-progress.mjs
Conversion progress: 96.99% (161/166; 5 open)
```

The next dependency-safe action is to wire the adapter into the production
interactive/RPC/print/JSON model setup, then obtain an independent S-004 review
before the clean-room and final denominator gates. The pre-existing untracked
`AGENTS.md` remains untouched and unstaged.

## Historical checkpoint — 2026-08-25 — native-provider production mode wiring

Queued native providers are now registered into the live `pi_ai::Models`
facade before provider/model resolution in print, JSON, RPC, and interactive
startup. The existing faux paths, API-key stream closures, summary streams,
extension tool catalogs, and RPC thinking-level behavior remain intact. A
binary print fixture proves that a custom extension provider can be selected by
provider/model and stream a response through the production agent loop.

The adapter also now forwards the broader stream option surface and accepts the
upstream `toolcall_*` event spellings and error payload shape. S-027 remains
open only for pinned jiti/module virtualization and Bun-specific verification
within the current residual scope.

Exact validation passed in this checkpoint; the command list and checker result
are recorded below. The next dependency-safe action is an independent S-004
residual review, followed by the clean-room and final source/TODO denominator
gates. The pre-existing untracked `AGENTS.md` remains untouched and unstaged.

```text
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib modes::rpc::tests --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib interactive::tests --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib run::tests --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib modes::json_event::tests --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test extensions_parity --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test cli_print_parity native_extension_provider_is_available_before_print_model_resolution --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib core::extensions --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib --quiet -- --test-threads=2
/home/mustbearnold/.cargo/bin/cargo test --workspace --offline --quiet -- --test-threads=2
/home/mustbearnold/.cargo/bin/cargo clippy -p pi-coding-agent --offline --all-targets -- -D warnings
/home/mustbearnold/.cargo/bin/cargo fmt --all -- --check
git diff --check
node scripts/parity-suite.mjs
node scripts/conversion-progress.mjs
Conversion progress: 96.99% (161/166; 5 open)
```

## Active checkpoint — 2026-08-25 — clean-room gate and residual boundary

Ledger row #100 is now checked. An independent clean-room clone at
`07e0623cde0ba5caf18275c773df31e56ee37ad1` passed workspace formatting,
strict workspace clippy, release build, workspace tests with two test threads,
the conversion-progress tests (7/7), the release-binary parity matrix (40
passed, 0 failed, 1 intentionally not-run), and `git diff --check`. The
credentialed network branch remains explicitly not-run, and the known
fake-node failure was not reproduced. Full evidence is in
`.unlazy/full-conversion-20260825/gates/clean-room-current.md`. The checkpoint
is tagged `conversion-97.59-clean-room`.

The authoritative checker after the ledger update is:

```text
Conversion progress: 98.80% (164/166; 2 open)
```

The remaining rows are S-027 and S-066. Two bounded S-027
reviews confirmed that the built-in pi/TypeBox JavaScript graph and genuine
compiled-Bun/Node-SEA identities are not present in the current distribution,
and that the Rust interactive host has no session-bound resource loader for
full upstream reload events/resources. Keep those boundaries explicit; do
not close S-027 with mock-only detection or a path-based module fixture.
## Active 2026-08-26 exhaustive behavioral-parity campaign

The historical source/conversion ledger still reports `100.00% (166/166)`,
but that number is not functional parity. The active acceptance index is
`docs/EXHAUSTIVE-PARITY-INVENTORY.md` with 318 unique capability IDs. The
campaign is still in progress: implementation lanes are being integrated and
the debug/release, PTY/TUI, real-provider, clean-environment, and installed
PATH gates have not all passed in the current worktree. Do not claim 1:1 or
flawless behavior until the active root gates contain measured evidence.

## Current interactive hidden-command/Daxnuts checkpoint — 2026-08-26

Branch `main` is unchanged at `de82599172fac888b3d1c59113d3ddef9644ee9d`,
matching `git ls-remote origin refs/heads/main`. The working tree is shared and
already dirty; no extension broker file was changed for this checkpoint.

Implementation files changed for this interactive slice:

- `crates/pi-coding-agent/src/interactive/easter_eggs.rs` — new Rust-native
  Armin, Earendil, and Daxnuts components; bounded render-time animation;
  exact upstream DAX payload; width-safe output; image/ESC/model tests.
- `crates/pi-coding-agent/src/interactive/slash.rs` — hidden exact command
  lookup plus exhaustive explicit `SlashKind` registry variants.
- `crates/pi-coding-agent/src/interactive/mod.rs` — hidden command parsing and
  scene/component integration.
- `crates/pi-coding-agent/src/modes/interactive.rs` — `/debug` ISO snapshot,
  exact `getDebugLogPath` equivalent, hidden dispatch, model trigger, resize
  redraw, and component cleanup on reset/switch.
- `crates/pi-coding-agent/tests/interactive_slash_complete_pty.rs` — real
  tmux PTY success/repeat/narrow/resize/cancellation/error/quit-restoration
  coverage.
- `docs/EXHAUSTIVE-PARITY-INVENTORY.md`, `CONVERSION-LEDGER.md`, `PLAN.md`,
  `HANDOFF.md`, and `GATES.md` — synchronized interactive evidence and
  remaining unrelated validation boundaries.

Exact validation:

```text
/home/mustbearnold/.cargo/bin/cargo check -p pi-coding-agent --offline
  exit 0
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib interactive::easter_eggs -- --test-threads=1
  6 passed; 0 failed
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib modes::interactive::tests::debug_timestamp_matches_upstream_iso_shape -- --exact --test-threads=1
  1 passed; 0 failed
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib interactive::interactive_tests::parse_submit_executes_hidden_commands_without_publishing_them -- --exact --test-threads=1
  1 passed; 0 failed
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test interactive_slash_complete_pty -- --test-threads=1
  4 passed; 0 failed; 62.51s
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test interactive_full_matrix -- --test-threads=1
  7 passed; 0 failed; 12.88s
/home/mustbearnold/.cargo/bin/cargo clippy -p pi-coding-agent --offline --all-targets -- -D warnings -A clippy::invalid_regex -A clippy::needless_update -A clippy::drop_non_drop
  exit 0
/home/mustbearnold/.cargo/bin/rustfmt --edition 2021 --check <five scoped interactive files>
  exit 0
git diff --check
  exit 0
```

The DAX source comparison reports `rust_len=6144 upstream_len=6144
equal=True`, with SHA-256
`4a1df9e4bdd8ecbf6beb4ddc6c7dfa6b80a16f0ff6e18fb9e0139d415ad59f1d` for both.
The duplicate model-trigger count is 1; no `not wired` or
`SlashKind::Unsupported` match remains; the PTY `/debug` assertion confirms
the `pi-debug.log` path under `PI_CODING_AGENT_DIR` and the ISO `...T...Z`
header.

The unmodified strict clippy command
`cargo clippy -p pi-coding-agent --offline --all-targets -- -D warnings`
still reports four pre-existing diagnostics in `core/changelog.rs`,
`core/extensions/integration.rs`, and `modes/rpc.rs`. The workspace-wide
`cargo fmt --all -- --check` also reports pre-existing diffs in unrelated
files; the five scoped interactive files pass direct rustfmt. These unrelated
files remain untouched. The legacy required
`node scripts/conversion-progress.mjs` path remains absent (`MODULE_NOT_FOUND`);
the Cargo-native audit remains the authoritative progress check.

## Current parent verification — 2026-08-29 — package-wide parity wave

The serialized package gates are green on the current dirty tree. `pi-tui`
passed 383 library tests plus every integration target, strict all-target
clippy, full stable rustfmt, scoped diff checks, and a trailing-whitespace
scan. `pi-ai` passed 441 library tests plus every integration target, strict
all-target clippy, full stable rustfmt, scoped diff checks, and a
catalog-parity target with 7 passing tests. `pi-coding-agent` passed 818
library tests plus every integration target, with the package check, strict
all-target clippy, full stable rustfmt, and scoped diff checks green.

These are package and evidence-gate results, not a 1:1 completion claim:
live vendor/provider traffic, complete cross-platform behavior, and TUI
visual/interaction acceptance rows remain open. The dashboard was rerun after
the gates and remains at whole-product behavioral parity 30/318 (9.43%), with
TUI overall 0/52 pending row-complete visual evidence.

The subsequent workspace gate also passed: `cargo test --workspace --offline
--all-targets -- --test-threads=1` completed without failures, strict
workspace clippy passed, and `cargo build --workspace --release --offline`
completed successfully. The rebuilt launcher resolves to
`target/release/pi` and reports `pi 0.84.2`.

## Latest serialized verification — 2026-08-29 — provider and harness follow-up

The Qwen Token Plan provider now honors model-derived base URLs and its real
loopback dispatch fixture passes 1/1; `pi-ai` check and strict all-target
clippy pass. The current `pi-agent` harness/environment tree passes 366 tests
across all targets and strict clippy. No parity percentage was promoted:
vendor, platform, recovery, and row-complete TUI visual/interaction evidence
remain open. The rebuilt release binary exposes both providers through
`--list-models glm-5.2` with their respective synthetic API-key environment
variables.

The same wave passed 17/17 focused alt-screen TUI tests, 35/35
OpenAI-compatible handoff tests plus 5/5 cross-provider fixtures, and the
noninteractive missing-session-CWD regression 1/1. Combined package check,
strict clippy, stable formatting, and scoped diff checks pass; these focused
gates do not close the remaining row boundaries.

## Latest serialized verification — 2026-08-29 — loopback routing and SSE

The SSE parser focused suite passes 13/13. Model-derived OpenAI Responses
base-URL routing passes a real loopback stream test, and the CLI-035 process
fixture passes 1/1 after capturing the expected `AGENTS.md` difference with
`--no-context-files`. The post-fix package matrices pass 441 pi-ai library
tests and 818 pi-coding-agent library tests plus all integration targets;
checks and strict clippy pass. Live vendor, cross-platform, recovery, and
complete TUI visual/interaction boundaries remain open.

## Latest serialized verification — 2026-08-30 — Z.AI providers and models.json listing

The existing native Z.AI registrations are verified for both the international
`zai` provider and the `zai-coding-cn` regional provider. The focused
`zai_provider_parity` target passes 4/4, covering registration, catalogs,
scoped API-key precedence, reasoning/tool/max-token request construction, and
a real local loopback streaming request through the provider closure. The
models.json list-models overlay/auth target passes 2/2, covering authenticated
fuzzy search, unauthenticated filtering, and malformed-config diagnostics.

The pi-ai all-target suite passes 444 library tests and all integration
targets; pi-coding-agent passes 822 library tests and all integration targets;
package check and strict all-target clippy pass. Validation used the explicit
stable toolchain because the default mise cargo shim has no configured Rust
version. No live Z.AI vendor request was made, and no commit or push was
performed; vendor quota/error/retry, platform, and full parity boundaries
remain open. The rebuilt release binary exposes both providers through
`--list-models glm-5.2` with their respective synthetic API-key environment
variables.
