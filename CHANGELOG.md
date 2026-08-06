# Changelog

All notable changes to `appstore-mcp`. This project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] — 2026-08-06

Hardening and context control. No breaking changes to existing tool calls: every
tool keeps its name and parameters, and every new knob is opt-in or defaults to
the previous behaviour.

### Added

- **`download_analytics_segment`** (tool 114) closes the analytics loop. The
  other analytics tools navigate to a presigned segment URL; this one fetches it,
  gunzips it, and returns the rows as JSON keyed by column name, with a
  `max_rows` sample and the true row count. Handles both tab- and
  comma-separated segments, and quoted fields.
- **Tool annotations.** Every tool now advertises `readOnlyHint`,
  `destructiveHint`, `idempotentHint`, and `openWorldHint`, so a client can tell
  `list_apps` from `remove_user` instead of treating every call alike. Effects
  are derived from each tool's leading verb, and a tool whose verb isn't
  recognised fails the test suite rather than shipping unlabelled.
- **`ASC_READ_ONLY`** serves only the 35 tools that cannot modify the account.
  `appstore_request` is kept but refuses any method other than `GET`, so the
  escape hatch can still reach uncovered endpoints without being a way around
  the restriction.
- **`ASC_TOOLS`** serves only the groups you name (or the `core` preset, 41
  tools), cutting the per-session context cost of tool definitions. An
  unrecognised group serves nothing and says so, rather than quietly falling
  back to everything.
- **`max_pages` on `appstore_list`** follows `links.next` and merges up to 20
  pages into one result, turning an N-turn cursor loop into one call.
  `meta.pagesFetched` and `meta.hasMore` report what happened.
- **Request timeouts** (`ASC_TIMEOUT_SECS`, `ASC_CONNECT_TIMEOUT_SECS`,
  `ASC_TRANSFER_TIMEOUT_SECS`). There were none before, so a stalled connection
  hung the tool call indefinitely with no way to cancel it over stdio.
- **Retries with backoff** (`ASC_MAX_RETRIES`, default 3). A `429` is replayed
  for any method since Apple rejected it without applying it; a `5xx` or
  mid-flight timeout is replayed only for idempotent methods — never a `POST`,
  which could otherwise create a duplicate resource. Honours `Retry-After`,
  bounded so a tool call can't stall for minutes.
- **Response shaping** (`ASC_COMPACT_RESPONSES`, `ASC_MAX_RESPONSE_BYTES`).
  Per-resource `self` links and link-only relationships are stripped — often
  most of a JSON:API page — and an oversized response sheds `included` and then
  trailing `data` items, carrying a `_truncated` key that says what went missing.
- Declared MSRV (1.88), verified by its own CI job, plus an advisory
  `cargo audit` job.

### Changed

- **Asset uploads stream from disk.** The file used to be read into memory whole
  and each chunk copied again; now the checksum is computed block by block and
  each operation reads only its own byte range, so peak memory is one chunk
  rather than the size of the asset. A chunk that fails is retried on its own
  instead of discarding the whole upload.
- **`appstore_request` validates the HTTP method** against the documented set.
  A typo like `FETCH` is a valid HTTP token, so it used to become a real request
  and come back as an opaque error.
- The crate is now a library plus a thin binary, so tests can drive a real server
  against a mock API.
- CI asserts the invariants of the tool surface — unique names, real
  descriptions, object schemas, correct annotations, and that the filtering knobs
  withhold exactly what they claim — instead of a hardcoded tool count that broke
  on every addition while proving little.

### Testing

96 → 202 tests. The new ones cover the HTTP layer that unit tests on body
builders can't reach: retry and no-retry decisions, timeouts, pagination and
merging, the three-step upload protocol (byte ranges, checksum, absent
`Authorization` header on presigned `PUT`s), segment download and parsing, and
the served tool surface under each configuration.

## [0.1.0]

Initial release: 113 tools across the App Store Connect API, plus two generic
JSON:API escape-hatch tools.
