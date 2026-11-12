---
name: review-pr
description: Thorough PR or branch review for ch-core-rs, the ClickHouse FORMAT Native decode/encode core, using this repo's expert agents. Checks that the change is non-breaking on the buffer/wire contract, root caused, fixed in the core (not a binding concern), scoped tight, free of hot-path regressions, documented, and not over-engineered, then ends in one verdict. Use when asked to review a PR, a branch, or a fix. Pass `recheck` to review only a fix round against the previous findings.
argument-hint: "[pr-number | branch] [recheck] [context]"
allowed-tools: Read, Grep, Glob, Bash, Agent, Write, Edit, AskUserQuestion
---

# PR Review (ch-core-rs)

This is the repo-local review skill for `ch-core-rs`. It shadows the global
`review-pr`. The crate is a zero-dependency Rust core that decodes and encodes
ClickHouse `FORMAT Native` wire bytes into Arrow-compatible columnar buffers.
Review against that framing, not a Python client's.

## Arguments

- A PR number, a branch, or nothing to review the current branch against `main`.
- `recheck` switches to fix-round mode. See "Recheck mode" below.
- Any other text is extra context. Treat it as a focus area, not a replacement
  for the checks.

## Read first

- `AGENTS.md` (the canonical instruction file, it wins on any conflict),
  `README.md` for the data model and scope, `CODEC_CONTRACT.md` for the
  definitive decoder output and encode input, and `COMPLETENESS.md` for the
  decode-parity tracker and handoff state.
- `.agents/server-map.md` before any wire-layout question, and `.server-ref` for
  the server tag this crate decodes against.
- The project's vault folder `INDEX.md` under
  `/Users/joe/obsidian/agent-brain/ch-core-rs/`. Search it for this PR, its
  linked issue, and any prior ruling on the same area. Do not relitigate a
  recorded ruling. Read `shared/` for ClickHouse server facts and `environment/`
  notes before running anything.

## Setup

1. `gh pr view <N> --json title,body,author,baseRefName,headRefName,headRefOid,isCrossRepository,files,additions,deletions,closingIssuesReferences`
   and `gh pr diff <N>`.
2. Read linked issues with `gh issue view`. Write down in one sentence what the
   PR claims to fix or add.
3. Review in a dedicated worktree, never in Joe's tree:
   `git fetch origin pull/<N>/head:pr-<N>` then
   `git worktree add /tmp/wt-<N> pr-<N>`. Reuse it if it exists and is at the PR
   head. For a local branch with no PR, review it where it is and skip the
   worktree. Never bare `git stash` in any shared tree (see the worktree rules in
   the environment); use a WIP commit if you must set work aside.
4. ch-core-rs specifics:
   - It is a Cargo workspace: the root package `ch-core-rs` plus
     `crates/ch-tcp-proto`. Run every gate command with `--workspace` so both
     crates are covered. MSRV is `rust-version = "1.81"`, edition 2021, stable
     only.
   - Wire-format work needs the server source at `.server-src/` pinned to
     `.server-ref` (both gitignored, so absent on a fresh worktree). If a check
     depends on server layout and the checkout is missing, set it up per
     `AGENTS.md` "Local server source checkout" or say the read was done without
     it and is less reliable.
   - Real-server fixtures live under `tests/fixtures/` and are captured by
     `scripts/gen_fixtures.sh` against a live server of the same version as
     `.server-ref`. Live capture needs `127.0.0.1` and `no_proxy`, not
     `localhost`.
   - Benchmarks are local-only. There is no bench code or perf number in git; a
     PR should not add any. Perf work uses the untracked `local-benches/` crate
     and results go to the vault.

## The checks

Every review answers all eight. Each answer needs evidence: a command that ran,
a `file:line`, or a cited server source. "Looks fine" is not an answer.

1. **Non-breaking.** No change to the public API, the columnar buffer layout, the
   Arrow C Data export (format strings, buffer order, null bitmap = bit 1 valid),
   `DecodeError`/`EncodeError` variants or messages callers match on, or decoded
   values for any already-supported type, unless Joe approved it. Everything
   `pub` is a contract. Compare old vs new behavior on real decode/encode, not by
   reading.
2. **Root caused.** For a bug fix, the PR names the real cause. For a wire-format
   bug, confirm the correct layout against the server source via
   `clickhouse-server-reader` and compare, rather than trusting the diff.
3. **Fixes that root cause.** The diff removes the cause, not one symptom or the
   one type/input from the report. Check sibling cases that hit the same cause
   (the Nullable wrapper, the zero-row block, multi-block, the big-endian
   fallback, encode vs decode parity).
4. **Right layer, core invariants intact.** The fix belongs in the core's
   decode/encode/FFI layer, not in host value policy that belongs in a binding.
   Re-check the non-negotiables in `AGENTS.md`: zero runtime dependencies
   (`Cargo.toml` has no `[dependencies]`), no binding code (PyO3/napi), columnar
   not row-major, blocks stay separate `ColBatch` chunks, Arrow-compatible buffer
   layout. A new dependency or a row-building API is an automatic finding.
5. **Scoped tight.** Every hunk traces to the task. No drive-by refactors,
   renames, or "while here" fixes. No cosmetic churn bundled in.
6. **No perf or safety regression on the hot path.** Decode is the product. If
   the diff touches the decode loop, reason explicitly about per-row allocations,
   bounds checks, and copies, and measure against base using `local-benches/` if
   the change is non-trivial. Untrusted wire bytes must never panic: no
   `.unwrap()`/`.expect()` on a wire path, `Result` on malformed or truncated
   input, `UnexpectedEof` preserved. Every `unsafe` block carries a correct
   `// Safety:` comment.
7. **Docs and changelog.** `CHANGELOG.md` under its unreleased section for any
   user-visible change. `CODEC_CONTRACT.md` updated for any new/changed type
   (wire payload, Arrow export, Rust buffer, server reference) and encode
   coverage. `COMPLETENESS.md` item checked off and its handoff block updated
   when the PR adds type support. `README.md` matches actual scope and behavior.
8. **No slop.** Flag only what has a concrete cost, and name a simpler
   alternative:
   - abstractions, wrappers, traits, generics, or options nothing needs
   - a new helper when one exists already (cite it) — e.g. duplicate alias
     resolution instead of the one shared helper
   - `match`/`if let` arms or guards on values already proven impossible
   - comments that narrate code instead of citing wire layout
   - near-duplicate tests that should be one case with shared `BlockBuilder` setup
   - a diff much bigger than the fix

## Fanout

Launch the expert agents the diff calls for, in one message so they run in
parallel, via the Agent tool with that `subagent_type`. Read-only agents only,
and none may spawn further agents.

Give each agent the PR number, the worktree path, the one-sentence claim, and
its question. Do not give it your own conclusions. An independent read is the
point.

| Diff touches | Agent | Question |
| --- | --- | --- |
| Any Rust in `src/` or `crates/` | `rust-reviewer` | Checks 1, 3, 4, 5, 6, 8 on the Rust side: bugs, unsoundness, panics on untrusted input, wire/decode hazards |
| The decode/encode hot loop, or a perf claim | `rust-perf-specialist` | Per-row allocations, bounds checks, copies, inlining; before/after vs base if non-trivial |
| `src/ffi/mod.rs`, Arrow C Data export, release callbacks, buffer lifetimes | `arrow-ffi-specialist` | Boundary safety, ownership, `Arc<ColBatch>` keep-alive, format strings, zero-copy handoff |
| Any wire-format or server-serialization claim | `clickhouse-server-reader` | The specific layout question, pinned to `.server-ref`, confirmed vs inferred |
| A guarantee the unsafe code relies on (std, Arrow C Data spec, API Guidelines) | `rust-source-reader` | The specific authoritative detail |

Skip a row when it clearly does not apply, and say which rows you skipped. Do
not launch reviewers to look thorough. `rust-implementer` is not a reviewer; it
is for the fix handoff only.

While agents run, do your own pass: read every changed file in full (review all
of `git diff main...HEAD`, not just the latest delta), trace callers, and run
the gate.

## The gate

Run these yourself in the worktree with `--workspace`. They decide the verdict
more than any reviewer opinion.

- **Regression test proves the fix.** Inside the worktree, run
  `git checkout <base> -- <non-test files>` and run the PR's new tests. They must
  fail. Run `git checkout HEAD -- <non-test files>` and they must pass. Report
  both commands and results. For a new type, the ground-truth check is the
  captured fixture in `tests/integration.rs`, not only the synthesized
  `BlockBuilder` unit tests (a shared wrong assumption passes a BlockBuilder test
  but fails a real fixture).
- `cargo build --workspace`.
- `cargo test --workspace`.
- `cargo fmt --check`.
- `cargo clippy --all-targets --workspace -- -D warnings`. Warnings are defects.
- Any perf number the change claims, reproduced with `local-benches/` (never
  from a debug build).

## Weigh the findings

- A correctness finding needs a repro, a failing test, or a cited server source.
  Without one it is a question, not a finding.
- Verify every agent finding yourself before reporting it. Drop what does not
  hold up, and note that you dropped it.
- Reviewers asked to find problems always find something. Fixing every nit
  produces over-engineering, which this crate's leanness invariant forbids.
  Report what affects correctness, the wire/buffer contract, safety, or the
  stated requirement. Put the rest in one short nits line.
- "No material issues" is a valid result. Say so plainly.

## Verdict

Exactly one:

- **SHIP.** Gate passes, no open correctness, safety, or contract finding.
- **SEND BACK.** Name each finding to fix. Include the handoff below.
- **NEEDS RULING.** A policy call only Joe can make (a new dependency, a buffer
  layout change, a wire-contract break). State the options and your pick.

## Output

1. Verdict in one line, with the fact that decided it.
2. The eight checks as a short table: pass, fail, or n/a, with evidence.
3. Findings by severity, each with a stable ID (F1, F2, ...) that later rounds
   reuse. Each: what is wrong, who it breaks, `file:line`, evidence, fix. Keep
   confirmed and inferred separate; decode logic built on an inferred layout must
   say so.
4. Gate results: commands and outcomes.
5. For SEND BACK, a **fix handoff** in a fenced block, ready to paste into a
   separate implementer session (it sees nothing from this session). Include:
   - the exact worktree path, the branch, and the reviewed head SHA
   - the vault note path
   - each fix as one line, with its finding ID
   - the tests that must fail before and pass after, and "do not edit existing
     correct assertions"
   - the gate commands to run (the `--workspace` set above)
   - "implement with the `rust-implementer` agent"
   - "do not add dependencies, do not change the public buffer layout, do not
     commit or push"
   - "when done, append a `## Fix round <N>` section to the vault note: per
     finding ID say fixed, declined, or partly fixed with one line of why; list
     new or changed files including untracked ones; and the gate commands you ran
     with results"
6. For a community PR, a paste-ready GitHub comment in a fenced block. Follow the
   `AGENTS.md` writing style: ASCII only, `->` for arrows, no em/en dashes or
   smart quotes, short and plain. No commit hashes.

## Recheck mode

For round 2 and later, after someone fixed the findings, usually in a separate
implementer session this session did not see. Work only from the tree and the
note.

1. Read the vault note (`pr-<N>` note, or for a branch the issue note the handoff
   named). Ask Joe if you cannot tell which. Get the tree path, the last reviewed
   SHA, the open finding IDs, and the `## Fix round <N>` section if there is one.
2. If the note has no fix-round section, ask Joe whether the fix is done and
   where it worked. Do not guess the tree. If Joe has no report, continue from the
   diff alone and say so.
3. Find everything that changed since the last review, committed or not:
   `git diff <last-sha>` in that tree, plus `git status --porcelain` for
   untracked files the diff misses. Check them against the files the fix-round
   section lists and flag any mismatch. Review only these changes.
4. For each open finding: fixed, not fixed, or fixed wrong, with evidence. Treat
   a "fixed" claim as a claim to verify. For "declined", weigh the reason; accept
   it or escalate to Joe, do not just repeat the finding.
5. Look for new defects in the new hunks only. Fix rounds often add them (a
   changed error variant, a lost bounds check, a new copy in the hot loop).
6. Rerun the full gate yourself. Do not rely on reported gate results.
7. Launch only the agents the new hunks call for.
8. Record the new reviewed state as `<head-sha> + uncommitted` in the note. From
   round 3 on, do not send back for anything below correctness or safety; list
   what is left and hand it to Joe as NEEDS RULING.

## Write back

Write or update the vault note at
`/Users/joe/obsidian/agent-brain/ch-core-rs/reviews/pr-<N>-<slug>-review-<YYYY-MM-DD>.md`
per `CONVENTIONS.md`. For a branch with no PR, use the branch name as the slug.
Record the round number, reviewed head SHA, verdict, open findings, and any
rulings. Recheck rounds update the same note in place. Update the folder
`INDEX.md`. Commit the vault after the session.

## Rules

- Do not comment, approve, push, commit, or label on GitHub. Hand Joe the text.
- Do not fix the code during a review. The deliverable is the verdict and the
  handoff.
- Ask Joe with AskUserQuestion when intent is unclear and it changes the verdict.
- Do not remove the worktree. Joe may want to keep working in it.
