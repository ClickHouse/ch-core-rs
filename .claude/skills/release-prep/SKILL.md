---
name: release-prep
description: Prepare a release by creating a release branch, updating the changelog, and bumping the crate version. Use when the user wants to cut a new release tag.
---

# Release Prep

Prepare a release for version `$ARGUMENTS`.

ch-core-rs is distributed as a git dependency pinned to a release tag, never via crates.io (`publish = false` in Cargo.toml). A release here is a version bump, a changelog section, and a `v$ARGUMENTS` tag with a GitHub release that downstream bindings (clickhouse-connect's `rust/ch-core-py`) repin against.

## Steps

### 1. Validate the version argument

The user must supply a semver version number (e.g. `0.2.0`, no `v` prefix). If `$ARGUMENTS` is empty or missing, ask the user to provide one and stop. Tags follow semver: no breaking changes on patch or minor releases. If the `## Unreleased` changelog section contains an entry that looks breaking and `$ARGUMENTS` is not a major bump, flag it to the user before proceeding.

### 2. Ensure you are on the correct release branch

First, switch to `main` and run `git pull` to ensure you have the latest changes. Then, if you are NOT already on `release/$ARGUMENTS`, create the branch `release/$ARGUMENTS` from `main` and switch to it. If the branch already exists remotely or locally, switch to it instead of creating a new one.

### 3. Update the changelog

Open `CHANGELOG.md` and find the `## Unreleased` section. Rename it to `## v$ARGUMENTS`. Then insert a new empty `## Unreleased` heading immediately above the newly renamed section, with a blank line separating them.

The result should look like:

```
## Unreleased

## v$ARGUMENTS

- ...
```

### 4. Bump the crate version

In `Cargo.toml`, set `version = "$ARGUMENTS"` under `[package]`. Then run `cargo check` so `Cargo.lock` picks up the new version, and confirm the lock diff contains only that.

### 5. Validate

Run the release gate and confirm everything is clean before handing back:

```
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

### 6. Generate release notes

Draft GitHub release notes for the `v$ARGUMENTS` release and print them to the console in a fenced markdown block (do NOT publish them, do NOT create the tag or release). The user will copy/edit them when they cut the GitHub release.

Match the tone and structure of the existing release at https://github.com/ClickHouse/ch-core-rs/releases/tag/v0.1.0 (readable via `gh release view v0.1.0 --repo ClickHouse/ch-core-rs`): a one-line or one-paragraph intro sized to the release, then a flat bullet list of changes, factual and brief. End with the standing line:

```
Downstream bindings pin this crate as a git dependency at this tag.
```

Sources of truth: the `CHANGELOG.md` section you just renamed (every entry there should be reflected) and `git log v<previous-tag>..HEAD` to catch anything the changelog missed. No installation block, this crate is not on crates.io.

### 7. Summary

After making the changes, show the user a short summary of what was done:
- The branch you are on
- The changelog update (old heading -> new heading)
- The version bump (old version -> new version)
- Test/clippy/fmt results
- That release notes were printed above for review
- A reminder of the downstream step: after the tag and GitHub release are cut, repin clickhouse-connect's `rust/` Cargo dependency to `v$ARGUMENTS` and rebuild

Do NOT commit the changes and do NOT create or push the tag or release. Let the user review and commit when ready.
