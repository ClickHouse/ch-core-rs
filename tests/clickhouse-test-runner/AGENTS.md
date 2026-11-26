# Instructions for agents: upstream SQL test harness

This harness runs the ClickHouse core SQL tests through ch-core-rs, over
HTTP and over the native TCP protocol. See [`README.md`](README.md) for how
it works and how to run it. The rules of the root
[`AGENTS.md`](../../AGENTS.md) (writing style, test data, server behavior is
authoritative) apply here too.

## The goal

The goal is to find bugs in ch-core-rs. To pass more tests is not the goal.
A failed test is information. Do not hide it.

## Rules

1. **Find the cause of a failure before you do anything with the test.** A
   test fails for one of four causes:
   - **The decoder** (the root crate `ch-core-rs`: the type parser, the
     Native decoder, the compression frames). This is what the harness is
     for.
   - **The engine** (`crates/ch-tcp-proto`, over `tcp` only).
   - **The shim** (the program in this directory). It does not do what
     `clickhouse-client` does.
   - **The environment.** The test cannot pass through any client on this
     server, or through any HTTP client (see
     [Environment causes](#environment-causes)).
2. **Do not fix a bug of the decoder or of the engine in a harness change.**
   Report it with a small reproduction, and fix it in a pull request of its
   own, with the tests that the root `AGENTS.md` requires (a `BlockBuilder`
   test, and a captured fixture in `tests/fixtures/` for a wire layout). Then
   enable the tests of its group.
3. **The default for a shim bug is a fix in the shim.** Make the shim copy
   `clickhouse-client` more exactly. Do not change the shim to hide a bug of
   the decoder or of the engine, for example with another decode path, bytes
   that the shim decodes itself, or a workaround for a wrong value. The shim
   must use the crates as a binding does (`INTEGRATING.md`): the public API
   only (`StreamDecoder`, `ByteReader`, `decode_next_block`, `block_end`,
   `block_end_binary_types`, `compression`, `binary_value`,
   `ch_tcp_proto::Connection`) and the `Column` buffers.
4. **Remove a test from the allowlist only when it cannot pass for an
   environment cause.** Write the reason in the commit message.
5. **A test that fails because of a bug or a shim gap stays visible.** Do
   not delete it. Put it in [`allowlist.txt`](allowlist.txt) as a known
   failure in the group of its cause: `# <test>  # [<id>]` when it fails
   over both protocols, `<test>  # known=<protocol>: [<id>]` when it fails
   over one protocol only. Each group has a header that describes the bug and
   gives a small reproduction, the protocols and the test count. The header
   of a decoder or engine group also has an `Issue:` line. Make a new group
   when no group fits. See the README for the format and for how to run one
   group (`KNOWN_FAILURES=<id>`).
6. **Do not list a test that cannot pass for an environment cause.** A test
   that cannot pass over one protocol for an environment cause gets
   `# only=<protocol>: <reason>`.

## How to find the cause

Do these steps in this order, over the protocol that fails.

1. **Run the test:**

   ```bash
   CLICKHOUSE_TEST_RUNNER_PROTOCOL=tcp UPSTREAM_TEST_LIST=<(echo <test>) \
     tests/clickhouse-test-runner/scripts/run-upstream-tests.sh
   ```

2. **Run the test through the real `clickhouse-client`** on the same server:

   ```bash
   UPSTREAM_TEST_LIST=<(echo <test>) tests/clickhouse-test-runner/scripts/run-with-real-client.sh
   ```

   If it also fails here, the cause is the environment. If it passes here,
   the cause is the decoder, the engine, the shim, or the HTTP interface
   (the real client uses the native protocol; see the list below). A test
   that passes over `tcp` and fails over `http` is often an HTTP-interface
   cause.
3. **Find the statement that fails.** Set `CLICKHOUSE_CLIENT_CLI_LOG` to a
   file; the log shows each statement and the error (and a
   `tcp_reconnect=` line when the engine failed the connection). The diff
   is in the output of the runner. The `.stdout` and `.stderr` files are
   next to the test.
4. **Compare the shim with the server on that one statement.** Run it
   through the shim over both protocols
   (`cargo run --manifest-path tests/clickhouse-test-runner/Cargo.toml -- -q "<SQL>"`)
   and through `curl` with `FORMAT TSV`. Then make the statement smaller
   until it is as small as possible. The triage variables of the README
   tell a framing or compression bug from a type bug:
   `CLICKHOUSE_TEST_RUNNER_COMPRESS=0`, and over `http`
   `CLICKHOUSE_TEST_RUNNER_REVISION=0` (bare `Native`).
5. **Decide between the crates and the shim with a small Rust program**
   that uses only the public API: the Native bytes of the statement (from
   `curl` with `FORMAT Native`, or with `client_protocol_version` and the
   same revision) through `StreamDecoder` or `ByteReader` +
   `decode_next_block`, or the statement through `ch_tcp_proto::Connection`.
   If the program also gives a wrong value or an error, the bug is in the
   decoder or the engine. If the program gives the correct value, the bug is
   in the shim. For a wire layout, confirm it against the server source as
   the root `AGENTS.md` says.

### Environment causes

A test cannot pass through any client on the CI server when it needs one of
these:

- A cluster, shards or replicas (`CLUSTER_DOESNT_EXIST`, `test_shard_localhost`).
- ZooKeeper or Keeper (`NO_ZOOKEEPER`, `Replicated*` engines).
- External services: S3, Azure, HDFS, Kafka, MySQL, PostgreSQL, MongoDB, URL
  sources.
- Server configuration of the upstream CI: named collections, dictionaries,
  storage policies, disks, users and grants, macros
  (`NAMED_COLLECTION_DOESNT_EXIST`, `NO_ELEMENTS_IN_CONFIG`, `UNKNOWN_POLICY`,
  `UNKNOWN_DISK`).
- Features that the server build or platform does not have
  (`SUPPORT_IS_DISABLED`).
- Stateful datasets (`--no-stateful`). (Long tests are not tried: the sweep
  of "Grow the allowlist" in the README gives `--no-long`.)
- Behavior of the HTTP interface itself, which every HTTP client has. Such a
  test gets `only=tcp`:
  - the server sends no `WITH TOTALS` row and no `extremes` in the `Native`
    format over HTTP;
  - the server sends nothing for an empty result, so the shim cannot write
    the header lines of `TSVWithNames` for an empty result;
  - an HTTP request fails when the current database of the session was
    dropped, detached or renamed, because the request names the database;
  - the server does not check the block structure over HTTP, so duplicate
    column names (`with 3 as "1" select 1, "1"`) give no
    `AMBIGUOUS_COLUMN_NAME`;
  - the memory use of a query over HTTP can be different from TCP, so an
    expected `MEMORY_LIMIT_EXCEEDED` does not come: over HTTP the server
    runs `FORMAT Null` itself, and with `compress=1` it counts its response
    compression buffer in the memory of the query;
  - the server logs an HTTP query differently: `system.query_log` has
    `interface = 'HTTP'`, and the thread counts, the pipeline and the stack
    traces in the logs are different; some settings (`discard_query_data`)
    have no effect over HTTP;
  - server bugs of the HTTP interface. For example, after
    `SET max_query_size = 18446744073709551615` every later request of the
    session fails as an empty query (`max_query_size + 1` becomes 0;
    ClickHouse/ClickHouse#103110). And with the HTTP parameter
    `client_protocol_version` (the shim sends it), `INSERT INTO FUNCTION
    file('f.native')` writes the file in the block layout of the protocol
    (with `BlockInfo`), which `file()` cannot read back
    (`CANNOT_READ_ALL_DATA`). Such a test passes with
    `CLICKHOUSE_TEST_RUNNER_REVISION=0`.
- Features of `clickhouse-client` itself that are not part of a driver:
  `INTO OUTFILE`, `FROM INFILE`, client-side parsing of `INSERT` data, and
  client-only settings.
- Tests that conflict with other tests of the set: a fixed table UUID, or
  server state that a test leaves behind (failpoints that a failed test did
  not disable, row policies). Such a test passes alone on a fresh server and
  fails in a full run. A failure message with "Injected failure" comes from a
  failpoint of another test. `DROP TABLE` in an `Atomic` database is
  asynchronous, so a test that drops a table and then creates or attaches a
  table with the same fixed UUID can fail with `TABLE_ALREADY_EXISTS`.
- The load of the server. The `EXPLAIN PIPELINE` and `EXPLAIN ANALYZE`
  output of a query with `max_threads` shows the threads that the server
  gives, and under load the server gives fewer (concurrency control), for
  example `Resize 1 -> 3` instead of `Resize 1 -> 4`. Such a test passes
  alone and fails now and then in a full run. These tests are not in the
  allowlist, because a CI runner has only 4 vCPUs: a test without the
  `no-parallel` tag that has `EXPLAIN PIPELINE` or `EXPLAIN ANALYZE`, and a
  test that sets `max_streams_to_max_threads_ratio`,
  `max_streams_multiplier_for_merge_tables` or
  `max_streams_for_merge_tree_reading`.
- Memory limits of the server, when the test also fails through the real
  `clickhouse-client`.
- The host name of the server. `tests/clickhouse-test` replaces its own host
  name with `localhost` in the output of a test, so a `hostName()` value
  differs when the server has another host name
  (`03217_datetime64_constant_to_ast`). CI and step 1 of the README give the
  server container the host name of the runner (`--hostname "$(hostname)"`).
  Do not remove such a test from the allowlist: fix the server setup.

These are **not** environment causes. Find the bug:

- A value that is different from the server's value.
- A decode error, a type name that the decoder does not parse, or an error
  such as `not enough data`.
- An error of the engine (a protocol state error, a packet in the wrong
  state).
- An error of the shim (`clickhouse-test-runner: ...`).
- A difference of the time zone database. The shim uses the embedded
  database of `chrono-tz`, which has other offsets than ClickHouse for some
  zones before 1970 (known shim gap `[shim-tz-database]`).

## Conditions for the allowlist

- Add a test only when it passes over the protocols of its line.
- Run new tests three times over each protocol, also with a higher `-j`
  than CI, on a **fresh** server with the configuration of CI (step 1 of
  "Run the tests locally" in the README: `CLICKHOUSE_SKIP_USER_SETUP=1`).
  Tests change the state of the server (row policies, settings profiles,
  users). A test that passes only after other tests ran must not go in.
- CI does not run failed tests again (`RETRY_FAILED`), because no known bug
  makes random tests fail. Do not use the retry when you grow or check the
  allowlist. If a bug makes random tests fail, document it in its group and
  in the workflow before you set `RETRY_FAILED` there.
- Do not run two sweeps at the same time on one server. The memory pressure
  causes `MEMORY_LIMIT_EXCEEDED` failures that are not real.
- When a test in the allowlist starts to fail intermittently, find the cause
  first (rule 1). Remove it only when the cause is in the environment or in
  the test itself, and write the reason.

## Bump the server version

The server version and the upstream tests are pinned together in
[`clickhouse-version`](clickhouse-version). A new ClickHouse release changes
nothing in CI until a pull request bumps the pin, so other pull requests do
not fail because of a release. Bump the pin in a pull request of its own,
and make that pull request pass before it merges:

1. Select the version: a full version with a server image tag and a
   `v<version>-stable` tag upstream, normally the version of the `latest`
   image.
2. Try it: start a manual run with the `clickhouse_version` input, or run
   the allowlist locally on that version over both protocols
   (`fetch-upstream.sh <version>`, and a fresh server of that version).
3. For each allowlisted test that now fails, find the cause (rule 1). A
   release changes the tests and the server output, so many tests can fail
   at the same time with no change in ch-core-rs. To see whether the crates
   are the cause, run the same test through the real client
   (`run-with-real-client.sh`) on the same server.
4. Run the known failures (`KNOWN_FAILURES=all`) over both protocols.
   Enable the tests that pass now.
5. Fix the names of allowlisted tests that the run reports as not in the
   checkout (upstream renamed or removed them).
6. Optionally, try the new tests of the release (see "Grow the allowlist" in
   the README).
7. Change `clickhouse-version`, run `fetch-upstream.sh`, and commit
   `src/errorcodes.rs` with it. A new server can also need a new Client
   Hello version in `src/tcp.rs` (the shim sends the version of the pinned
   server).

## Keep the documents in sync

When you change the behavior of the shim (flags, statement handling,
protocols, output formats, settings), update [`README.md`](README.md), this
file and the workflow in the same change. When the allowlist changes much,
update the counts in the first lines of `allowlist.txt`, in the README
("Known failures" and "CI") and in the shard comment of the workflow. Run
`cargo fmt` and `cargo clippy --all-targets -- -D warnings` in this
directory (it is its own Cargo workspace).
