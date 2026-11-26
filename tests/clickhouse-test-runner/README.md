# clickhouse-test-runner

This harness runs the ClickHouse core SQL tests
([`tests/queries/0_stateless`](https://github.com/ClickHouse/ClickHouse/tree/master/tests/queries/0_stateless))
through ch-core-rs.

The upstream test runner
([`tests/clickhouse-test`](https://github.com/ClickHouse/ClickHouse/blob/master/tests/clickhouse-test))
runs each `.sql` test through `clickhouse-client` and compares the output with
the `.reference` file of the test. This harness is a small Rust program that
imitates `clickhouse-client` well enough for the runner to drive it without
changes: the same flags, the same stdin and `--query` behavior, and the
`extract-from-config` shortcut.

ch-core-rs is a codec, not a client. The program plays the part of a thin
binding (see [`INTEGRATING.md`](../../INTEGRATING.md)): it owns the transport,
gets each result as `FORMAT Native`, decodes it with the public API of the
two crates of this repository, and writes the decoded columns as
`TabSeparated` text, directed by the column types (`ChType`). Thus a test
passes only when ch-core-rs parses every type name and decodes every value
correctly. The `.reference` files are the oracle for the decoder.

The program has two protocols. `CLICKHOUSE_TEST_RUNNER_PROTOCOL` selects one
(default `http`):

- **`http`**: a small blocking HTTP client in the program (`ureq`). The body
  of each response goes to `ch_core_rs::native::stream_decoder::StreamDecoder`.
- **`tcp`**: the sans-I/O engine `ch-tcp-proto` (`Connection`) over one
  blocking `std::net::TcpStream`. The payload of each `Data` packet goes to
  `ch_core_rs::native::decode::decode_next_block`.

It is a port of the
[clickhouse-rs](https://github.com/ClickHouse/clickhouse-rs/pull/537) harness
(the statement splitter, test hints, arguments, runner and scripts), which is
a port of the
[Go](https://github.com/ClickHouse/clickhouse-go/tree/main/tests/clickhouse-test-runner)
harness. The renderer and the two protocols are new.

The program is a Cargo package of its own (with its own workspace, lock file
and `target/` directory). It is not a member of the root workspace, so the
jobs of `ci.yml` (and the MSRV check) do not build it. It uses the toolchain
of `rust-toolchain.toml` in the root of the repository. Its `Cargo.lock` is
in git, so CI and local runs use the same dependency versions (for example
`chrono-tz`, whose time zone data changes the rendering of `DateTime`
values). To update the dependencies, run `cargo update` in this directory and
commit the lock file. The library code does not change for the harness: the
library has no dependency on the program.

## Run the tests locally

You need Rust, Python 3 with `jinja2`, Docker, `git` and `curl`.

1. Start the pinned ClickHouse server. The version is in
   [`clickhouse-version`](clickhouse-version):

   ```bash
   docker run -d --name ch-core-rs-clickhouse-server -p 8123:8123 -p 9000:9000 \
     --hostname "$(hostname)" \
     --ulimit nofile=262144:262144 -e CLICKHOUSE_SKIP_USER_SETUP=1 \
     "clickhouse/clickhouse-server:$(tests/clickhouse-test-runner/scripts/pinned-version.sh)"
   ```

   The server gets the host name of the machine that runs the tests:
   `tests/clickhouse-test` replaces its own host name with `localhost` in
   the output of each test, and some `.reference` files have `localhost` for
   `hostName()`.

   This is the stock server configuration, as in CI. With
   `CLICKHOUSE_SKIP_USER_SETUP=1`, the `default` user of the image's
   `users.xml` has network access, access management and named-collection
   control; tests create users, roles and named collections. Do not use
   `CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=1` instead: then the image replaces
   the user with one that has no named-collection control, and more tests
   fail.

2. Get the upstream tests of the same version:

   ```bash
   tests/clickhouse-test-runner/scripts/fetch-upstream.sh
   ```

   The script checks out the release tag of the pinned version (for example
   `v26.10.4.207-stable`) into `.upstream/ClickHouse`, sparse and shallow.
   The `.reference` files of that tag were made by the same server build. The
   script also generates `src/errorcodes.rs` from the same checkout. To try
   another version, give it as the argument (`fetch-upstream.sh 26.11.1.10`),
   and start the server of that version.

   The run script stops when the server and the tests are of different
   versions, because then the `.reference` files do not match. Set
   `ALLOW_VERSION_MISMATCH=1` to run anyway.

3. Run the allowlisted tests, over each protocol:

   ```bash
   pip install jinja2
   CLICKHOUSE_TEST_RUNNER_PROTOCOL=http tests/clickhouse-test-runner/scripts/run-upstream-tests.sh -j 8
   CLICKHOUSE_TEST_RUNNER_PROTOCOL=tcp tests/clickhouse-test-runner/scripts/run-upstream-tests.sh -j 8
   ```

The script builds the program (`cargo build --release`), copies it to
`.bin/clickhouse` (with a `clickhouse-client` symlink), puts `.bin` first in
`PATH` for the runner only, and starts `tests/clickhouse-test`. It gives the
runner `--no-stateful --no-random-settings --no-random-merge-tree-settings
--dont_retry_failures`; other arguments go to the runner after these. For
example, to run one test (also a test that is not in the allowlist):

```bash
UPSTREAM_TEST_LIST=<(echo 00007_array) tests/clickhouse-test-runner/scripts/run-upstream-tests.sh
```

The runner writes the output of a failed test next to the test, in
`tests/clickhouse-test-runner/.upstream/ClickHouse/tests/queries/0_stateless/`
(`<test>.<pid>.stdout` and `.stderr` with `-j`, `<test>.stdout` without
it). The diff is only in the output of the runner.

The unit tests of the harness need no server. The live tests compare the
output of the program, over both protocols (some tests over one), mostly
with the server's own `TabSeparated` output. They need the server of step 1
on `localhost` (ports 8123 and 9000):

```bash
(cd tests/clickhouse-test-runner && cargo test)
(cd tests/clickhouse-test-runner && CLICKHOUSE_TEST_RUNNER_LIVE=1 cargo test live_tests)
```

To run one query through the program (from the root of the repository):

```bash
cargo run --manifest-path tests/clickhouse-test-runner/Cargo.toml -- -q "SELECT 1"
CLICKHOUSE_TEST_RUNNER_PROTOCOL=tcp cargo run --manifest-path tests/clickhouse-test-runner/Cargo.toml -- -q "SELECT 1"
```

### Environment variables

Relative paths in this table are relative to `tests/clickhouse-test-runner/`.

| Variable | Default | Description |
|---|---|---|
| `CLICKHOUSE_TEST_RUNNER_PROTOCOL` | `http` | The protocol of the program: `http` or `tcp`. |
| `UPSTREAM_CLICKHOUSE_DIR` | `.upstream/ClickHouse` | The checkout of `ClickHouse/ClickHouse`. |
| `UPSTREAM_REPO_URL` | the `ClickHouse/ClickHouse` repository on GitHub | For `fetch-upstream.sh`: the repository to fetch from. |
| `UPSTREAM_TEST_LIST` | `allowlist.txt` | The file with the tests to run. |
| `SHARD_INDEX`, `SHARD_TOTAL` | `1`, `1` | Run only shard `SHARD_INDEX` of `SHARD_TOTAL` round-robin shards of the list. |
| `KNOWN_FAILURES` | (none) | Run the known failures of these groups (comma-separated IDs, or `all`) instead of the allowlist. |
| `RETRY_FAILED` | `0` | Run the failed tests again, at most this many times. A test that passes on a retry is a warning. Do not use it to grow or check the allowlist. |
| `CLICKHOUSE_CLIENT_CLI_LOG` | (none) | A file. Each invocation of the program adds its arguments (the `--password` value hidden), statements and errors to it. |
| `CLICKHOUSE_HOST` | `localhost` | The server host, for the runner and for the version check. |
| `CLICKHOUSE_PORT_TCP`, `CLICKHOUSE_PORT_HTTP` | `9000`, `8123` | The ports of the server. When `CLICKHOUSE_PORT_TCP` is set, the runner gives `--port=$CLICKHOUSE_PORT_TCP`: the `tcp` protocol uses it, and the `http` protocol uses `CLICKHOUSE_PORT_HTTP` for it. |
| `CLICKHOUSE_PORT_TCP_SECURE`, `CLICKHOUSE_PORT_HTTPS` | `9440`, `8443` | The same with `--secure`. The program has no TLS: `--secure` fails over both protocols (`ureq` is built without a TLS provider). |
| `ALLOW_VERSION_MISMATCH` | `0` | `1`: run also when the server and the upstream tests are of different versions. |
| `CLICKHOUSE_TEST_RUNNER_LIVE` | (none) | `1`: `cargo test` also runs the live tests (they need the server). |
| `CLICKHOUSE_CONTAINER` | `ch-core-rs-clickhouse-server` | For `run-with-real-client.sh`: the server container for `docker exec`. |
| `REAL_CLICKHOUSE_BINARY` | (none) | For `run-with-real-client.sh`: a `clickhouse` binary to use instead of `docker exec`. |

The scripts unset `CLICKHOUSE_DATABASE`: with it, the runner gives the client
`--database` two times, and every test fails.

These variables of the program are for triage only (see
[`AGENTS.md`](AGENTS.md)). Do not set them for an allowlist run:

| Variable | Description |
|---|---|
| `CLICKHOUSE_TEST_RUNNER_COMPRESS=0` | No compression: over `http` the result comes without the `compress=1` envelope, over `tcp` the blocks come without compression. Use it to tell a framing or compression bug from a type bug. |
| `CLICKHOUSE_TEST_RUNNER_REVISION=<n>` | `http` only: request and decode the result at protocol revision `<n>` (`0`: bare `Native`) instead of `DBMS_TCP_PROTOCOL_VERSION`. |
| `CLICKHOUSE_TEST_RUNNER_FORMAT_FALLBACK=1` | A statement with an output format that the program cannot write runs without its `FORMAT` clause, and the result is written as `TabSeparated`. The test then fails with a diff, but the rest of it runs, so a later failure of another kind shows. |

## The allowlist

[`allowlist.txt`](allowlist.txt) lists the tests that pass reliably, one test
name (the file name without `.sql`) per line. `#` starts a comment. The script
gives the runner anchored regular expressions, so each name selects exactly
one test. (The runner itself matches each name as a substring, so
`00396_uuid` would also select `00396_uuid_v7`.)

The file has four kinds of lines:

| Line | Meaning |
|---|---|
| `00001_select_1` | The test runs over both protocols. |
| `00015_totals_having_constants  # only=tcp: <reason>` | The test runs over that protocol only. Over the other protocol it cannot pass for an environment cause (for example, the server sends no `WITH TOTALS` row in the `Native` output over HTTP). |
| `02737_session_timezone  # known=tcp: [engine-session-timezone]` | A **known failure** over that protocol: the test runs over the other protocol only. The ID in brackets names the group. |
| `# 00342_escape_sequences  # [decoder-name-not-utf8]` | A **known failure** over both protocols: the test does not run. |

The known-failure lines are under the header of their group, in the
known-failure part at the end of the file. The header describes the bug,
gives a small reproduction, and has the lines `Protocols:`, `Issue:` (for a
bug of the decoder or of the engine) and `Tests:`. The ID of a group starts
with its cause: `decoder-` (the Native decoder of `ch-core-rs`), `engine-`
(the TCP engine `ch-tcp-proto`), or `shim-` and `tcp-` (a gap of this
program).

To run the known failures of one or more groups, for example to check a fix:

```bash
KNOWN_FAILURES=decoder-qbit-int8-and-stride tests/clickhouse-test-runner/scripts/run-upstream-tests.sh
KNOWN_FAILURES=all CLICKHOUSE_TEST_RUNNER_PROTOCOL=tcp tests/clickhouse-test-runner/scripts/run-upstream-tests.sh
```

`KNOWN_FAILURES` selects a `known=<protocol>` line only over its own
protocol.

When a fix makes the tests of a group pass, remove the `# ` at the start and
the `# [<id>]` at the end of their lines (or the `# known=<protocol>: [<id>]`
at the end). When the fix makes a test pass over one protocol only, change
its line to `<test>  # known=<other protocol>: [<id>]`. Update the `Tests:`
line of the group header and the counts in the first lines of
`allowlist.txt`. When all tests of a group pass, remove the group header. A
test can fail for more than one cause. It is listed in the group of the
first cause, and the header of each other group lists it under `Also:`, with
the ID of the group that lists it.

### Grow the allowlist

1. Run the candidate tests through the harness, over each protocol. To try
   all `.sql` tests:

   ```bash
   ls tests/clickhouse-test-runner/.upstream/ClickHouse/tests/queries/0_stateless \
     | sed -nE '/\.gen\.sql$/d; s/\.sql(\.j2)?$//p' | sort -u > /tmp/candidates.txt
   CLICKHOUSE_TEST_RUNNER_PROTOCOL=http UPSTREAM_TEST_LIST=/tmp/candidates.txt \
     tests/clickhouse-test-runner/scripts/run-upstream-tests.sh -j 8 --no-long --timeout 120 \
       --max-failures-chain 1000000 | tee /tmp/run-http.log
   ```

   Then the same with `CLICKHOUSE_TEST_RUNNER_PROTOCOL=tcp`. `--max-failures-chain`
   turns off a limit of the runner: by default it stops after 20 failures in
   a row, which a list of failing tests reaches at once. Do not run two
   sweeps at the same time on one server: the memory pressure causes
   `MEMORY_LIMIT_EXCEEDED` failures that are not real.

2. Keep only the tests that report `[ OK ]`:

   ```bash
   sed -nE 's/^.*\] ([^ :]+): +\[ OK \].*/\1/p' /tmp/run-http.log | sort -u
   ```

   A test that passes over one protocol only gets an `only=` line (an
   environment cause) or a `known=` line (a bug), after you find the cause.

3. Run the new tests again (two or more times, and with `-j` higher than CI
   uses) and remove the tests that do not pass every time.

4. Make sure that the tests also pass on a fresh server, because CI starts
   from one. For example, a test that reads `system.query_log` can depend on
   the queries of other tests.

**Rule:** add only tests that pass reliably. Before you remove a test or
decide not to add it, find out whether it fails because of a bug in
ch-core-rs. The harness exists to find those bugs. See
[`AGENTS.md`](AGENTS.md) for the steps.

To compare with the real `clickhouse-client`, run the same tests through it:

```bash
UPSTREAM_TEST_LIST=<(echo 00007_array) tests/clickhouse-test-runner/scripts/run-with-real-client.sh
```

By default the script runs `clickhouse-client` in the server container
(`docker exec`; set `CLICKHOUSE_CONTAINER` to its name). With
`REAL_CLICKHOUSE_BINARY=/path/to/clickhouse` it runs that binary. A test that
passes there and fails through the program shows a bug in ch-core-rs, a gap
in this program, or a difference of the HTTP interface (see
[`AGENTS.md`](AGENTS.md#environment-causes)).

## How the program imitates clickhouse-client

The program sends nothing that `clickhouse-client` does not send, because some
tests read their own queries from `system.query_log`.

- **Statements.** The program splits the input into statements as
  `clickhouse-client` does: at a top-level `;`, with the same-line trailing
  comment kept with its statement. It sends each statement with its `;`. The
  inline data of `INSERT ... FORMAT <name>` (other than `Values`) ends at an
  empty line. The `-- Tags:` header is not part of a statement. Every
  statement runs, with or without `--multiquery`. Input bytes that are not
  valid UTF-8 go to the server as they are over `http`; over `tcp` such a
  statement fails, because `ch_tcp_proto::Query::new` takes the SQL as a
  `String` (group `[engine-query-not-utf8]`).
- **Test hints.** `-- { serverError X }`, `-- { clientError X }` and
  `-- { error X }` (codes or names) make the error the expected result. An
  error hint in a leading comment is ignored, as upstream. The server finds
  all errors, so a server error can satisfy a `clientError` hint.
  `-- { echo }` and `-- { echoOn }` (in any comment) start to print the
  statements and `-- { echoOff }` stops it; a statement with a syntax error
  is not printed (`clickhouse-client` parses before it echoes).
  `-- { retry ... }` is not supported.
- **Sessions over `http`.** All requests of an invocation have one
  `session_id`, so `SET`, `USE` and temporary tables last for the whole
  invocation, also after an error. Each request names the database
  (`database=`), so after a `USE` the program changes it. The user and the
  password go in the `X-ClickHouse-User` and `X-ClickHouse-Key` headers.
- **Sessions over `tcp`.** One connection is the session. The Client Hello
  has the name `ch-core-rs clickhouse-test-runner` and the version of the
  pinned server (26.10): with version 0.0, the server fails a distributed
  query with `LOGICAL_ERROR`. When the engine fails the connection
  (`State::Failed`, after a server exception that fails the connection, or
  a protocol error such as a block that it cannot decode), the program
  closes the socket. Before the next
  statement it opens a new connection, with the database of the last `USE`,
  and runs the `SET` statements of the invocation again, as
  `clickhouse-client` keeps its settings across a reconnect. Temporary tables
  are lost; the invocation log then has a `tcp_reconnect=` line.
- **Results over `http`.** Each request has `default_format=Native`,
  `client_protocol_version` equal to `DBMS_TCP_PROTOCOL_VERSION` (54485) and
  `compress=1`. The program takes the frames of the compression envelope
  apart with `ch_core_rs::compression::decompress_frame` and gives the
  plaintext to `StreamDecoder::feed`, one frame at a time, so blocks that
  cross frames use the streaming path. At revision 0 (bare `Native`), the
  server writes a top-level `DateTime('tz')` type without its time zone.
  The body of every statement is decoded (DDL gives an empty body). A server
  exception comes in one of three forms: a status other than 200 with the
  text (or, with an empty body, only the code in the
  `X-ClickHouse-Exception-Code` header); a status other than 200 after the
  complete Native blocks (the program finds their end with
  `native::decode::block_end`, or `block_end_binary_types` for binary type
  headers); or the `X-ClickHouse-Exception-Tag` header and the tagged
  exception at the end of the body (with status 200, or another status).
  The rows before an exception are written, as `clickhouse-client` does.
- **Results over `tcp`.** Compression is on (`with_compression(true)`), as
  in `clickhouse-client`; the server picks the codec. Each `Data` payload is
  one Native block, decoded at `effective_revision()` with
  `decode_next_block`. The first block has no rows: it gives the header of a
  `...WithNames` format also for an empty result. `Totals` and `Extremes`
  packets are written after the rows, each after an empty line, as
  `TabSeparated` does. Every `INSERT` uses `Connection::insert`; the inline
  data stays in the query text, and the server parses it. When the server
  asks the client for the data (`input()`, an `INSERT` without data), the
  program has no input-format parser: the statement fails (known shim gap
  `[tcp-client-fed-insert]`). `Log`, `Progress`, `ProfileInfo`,
  `ProfileEvents` and `TableColumns` packets are dropped.
- **Binary type headers.** After
  `SET output_format_native_encode_types_in_binary_format = 1` (or with the
  setting in the `SETTINGS` clause of the statement), the program decodes
  the result over `http` with `StreamDecoder::new_binary_types`. Over `tcp`
  the engine frames each block in the text mode (group
  `[engine-binary-type-headers]`).
- **Statement types.** Statements that start with `SELECT`, `WITH`, `FROM`,
  `SHOW`, `DESC`, `DESCRIBE`, `EXISTS`, `EXPLAIN`, `CHECK` or `KILL` return
  rows: the program takes their `FORMAT` clause. All other statements go as
  they are (the `FORMAT` of an `INSERT` names its data), and a result of
  such a statement is written as `TabSeparated`. After `SET dialect` to
  another language (KQL, PRQL, ...) or `SET implicit_select = 1`, more
  statements return rows: in another language all statements other than
  `SET`, and with `implicit_select` all statements that do not start with a
  DDL or DML keyword (`INSERT`, `CREATE`, `SET`, ...). A statement in
  another language goes without its trailing `;`. The program splits KQL
  statements by the SQL rules, which is not correct for all KQL (known shim
  gap `[shim-kql-split]`).
- **Rendering.** The renderer is directed by the `ChType` of each column
  and reads the `Column` buffers (the layout of each type is in
  [`CODEC_CONTRACT.md`](../../CODEC_CONTRACT.md)). It writes the values as
  the server writes `TabSeparated`: escaped strings and `\N` at the top
  level, quoted strings and `NULL` inside `Array`, `Tuple` and `Map`; the
  shortest float text in the layout of the server (`1e21`, `1.5e-7`, `nan`,
  `-0`); decimals without trailing zeros; `DateTime64(P)` with P digits;
  `Time` up to `999:59:59`; IPv6 as `formatIPv6` writes it; `JSON` as the
  text of the object. Map entries keep the order of the server. A `Dynamic`
  value in the shared variant and a `JSON` value in the shared data are
  decoded with
  `native::binary_value::{read_binary_type_prefix, decode_binary_value}`.
  The program does not follow the settings that change the text output of
  the server, such as `output_format_decimal_trailing_zeros` (known shim gap
  `[shim-output-setting]`).
- **Time zones.** A `DateTime` with a time zone in its type is written in
  that zone. A `DateTime` without a time zone is written in the time zone of
  the query: over `http` the `X-ClickHouse-Timezone` header of each response
  (it follows `session_timezone`), over `tcp` the time zone of the Server
  Hello (see `[engine-session-timezone]`). The rules come from the embedded
  database of `chrono-tz`, not from the system, so the result does not
  depend on the tz database of the machine (see `[shim-tz-database]`).
- **Output formats.** The program writes the result itself in the
  `TabSeparated` family (also `TSVRaw` and `...WithNames[AndTypes]`) or
  `Null`. A statement with any other output format fails (known shim gap
  `[shim-output-format]`: the program does not ask the server for a text
  format, because then the test checks only the transport). It keeps
  `FORMAT Null` in the query: over `http` the server writes the format
  itself and sends no rows; over `tcp` the server sends the blocks, and the
  program decodes them and writes nothing. For `EXPLAIN` the server ignores
  `FORMAT Null` and sends the plan, and the program writes it. The program
  also keeps the clause in another language than SQL. The `TSVRaw...` formats write the
  header lines without escapes, as their rows.
- **Settings.** Of the settings on the command line, only `async_insert`,
  `enable_parallel_replicas`, `log_comment`, `max_insert_threads`,
  `max_threads` and `send_logs_level` go to the server, with each query, as
  in `clickhouse-client`: over `http` as URL
  parameters, over `tcp` with `Query::with_setting`. A `SET` of the same
  setting in the test replaces the command-line value.
  `--param_<name>=<value>` gives a query parameter, the raw value, as
  `clickhouse-client` sends it: over `http` the URL parameter
  `param_<name>`, over `tcp` `Query::with_parameter`.
- **Errors.** An unexpected error stops the program. It writes the error to
  stderr (a server error with its code and name; over `tcp` an error of the
  engine with its `source()` chain), and the exit code is the error code
  modulo 256 (or 1).
- **Error names.** [`src/errorcodes.rs`](src/errorcodes.rs) maps the error
  names of test hints to codes.
  [`scripts/gen-error-codes.sh`](scripts/gen-error-codes.sh), which
  `fetch-upstream.sh` runs after each checkout, generates it from
  `src/Common/ErrorCodes.cpp` of the upstream checkout (with bash, sed and
  grep; no Rust toolchain), so it always matches the tests. Commit the file
  when it changes.

## Known failures

The end of [`allowlist.txt`](allowlist.txt) lists the tests that fail
because of a bug in ch-core-rs or a gap in this program, in groups. Each
group header describes the cause and gives a small reproduction. A test that
cannot pass for an environment cause (a cluster, ZooKeeper, server
configuration, features of `clickhouse-client` itself, the HTTP interface) is
not listed; see [`AGENTS.md`](AGENTS.md#environment-causes). The program does
not try `.sh` tests.

At the pinned version 26.10.4.207, of the 12,961 `.sql` and `.sql.j2` tests,
10,302 run over `http` and 10,419 over `tcp` (10,256 over both protocols, 163
over `tcp` only, 46 over `http` only). 411 tests are known failures in 17 groups: 6 groups of the
decoder, 5 of the TCP engine and 6 of the program. The other tests cannot
pass for an environment cause, are long tests (the sweep of "Grow the
allowlist" gives `--no-long`), the runner skips them on this server (tests
that need shards or the stateful datasets), or they depend on the load of
the server (see [`AGENTS.md`](AGENTS.md#environment-causes)). The first
lines of `allowlist.txt` give the current counts.

## CI

[`upstream-sql-tests.yml`](../../.github/workflows/upstream-sql-tests.yml)
runs the allowlist nightly, manually, on pushes to `main` and on pull
requests that change this directory or the workflow.

- The `lint` job runs `cargo fmt -- --check`, `cargo clippy --all-targets --
  -D warnings` and the unit tests of the program. The `ci.yml` workflow does
  not build this package, because it is not a member of the root workspace.
- The `upstream-sql-tests` jobs are the shards `1..N` of the allowlist
  (`SHARD_INDEX`, `SHARD_TOTAL`) for each protocol, at `-j 4`. There are 4
  shards per protocol of about 2,600 tests. One shard takes about 3 minutes
  of tests on a 4-vCPU runner, plus the build of the program, the upstream
  checkout and the server start. (The full list of one protocol takes about
  10 minutes at `-j 6` on 32 vCPUs.) The `http` job of shard 1 also runs the
  live tests of the program, over both protocols.
- The server is the stock image of the pinned version with
  `CLICKHOUSE_SKIP_USER_SETUP=1`, as in step 1 of
  [Run the tests locally](#run-the-tests-locally).
- CI does not run failed tests again (`RETRY_FAILED` is not set): no known
  bug makes random tests fail. The script also turns off the runner's own
  retries (`--dont_retry_failures`).
- On a failure, the job uploads the `.stdout` and `.stderr` files of the
  failed tests and the log of the program as an artifact. The diffs are in
  the job log.
- The server version and the upstream tests are pinned together in
  [`clickhouse-version`](clickhouse-version). A new ClickHouse release
  changes nothing in CI, so a pull request never fails because of a release
  that it does not touch. A bump is a pull request of its own; see
  [`AGENTS.md`](AGENTS.md#bump-the-server-version).
- The `clickhouse_version` input of a manual run tries another version
  without a commit, for example before a bump.
- When an allowlisted test is not in the checkout (upstream renamed or
  removed it), the run shows a warning with the names.
- If a shard takes much more than a few minutes, increase the number of
  shards: change the `shard` list and `SHARD_TOTAL` together.
