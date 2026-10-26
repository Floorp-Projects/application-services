# Floorp workspace discovery

This component reads Desktop's `floorp-workspaces` engine version 1. It never
captures browser tabs, generates clocks or IDs, creates outgoing records, or
writes/deletes Workspace BSOs. Opened iOS copies have no relationship with this
cache. The embedding application retains the registered store strongly.

## Swift boundary

```swift
let store = try FloorpWorkspacesStore(path: profileCachePath)
try store.bindAccount(uid: account.uid)
store.registerWithSyncManager()
let snapshot = try store.snapshotJson()
// Feature stop, logout, or account removal:
try store.invalidate()
```

Request `floorp-workspaces` explicitly in `SyncEngineSelection.some`. It is
excluded from `all` and the toggleable engine list, and `enabledChanges` rejects
both enable and disable. In particular, disabling reception cannot decline or
wipe the server's collection. This does not prohibit ordinary Sync control
requests for authentication, collection keys, or `meta/global` initialization.
The workspace definition is deliberately absent from `DEFAULT_ENGINES`.

`snapshotJson` returns camelCase JSON:

```json
{"schemaVersion":1,"accountUid":"uid","revision":4,"globalSyncId":"global","collectionSyncId":"collection","engineVersion":1,"status":"ready","lastReceivedMillis":0,"workspaces":[{"id":"UUID","name":"Desktop","icon":null,"tabs":[{"id":"UUID","url":"https://example.org/","title":"Page","pinned":false,"position":0}]}]}
```

Statuses are `unavailable`, `unpublished`, `unsupportedVersion`, and `ready`.
A network, decryption, validation, or partial-download error fails the sync run
and preserves the last confirmed snapshot; the Swift coordinator adds its
loading/offline/failure state. The constructor rejects a corrupt or future cache
schema, and never exposes a persisted account until `bindAccount` succeeds.
Store errors must be presented as errors, not an empty received list.

The public `url` is the serialized WHATWG navigation URL. Original wire strings
remain intact in the cache for Desktop's canonical conflict comparisons. The
embedding application can therefore open unusual but valid Desktop URL input
without relying on Foundation to reinterpret backslashes or whitespace.

## Commit and generation boundaries

The account, association, revision, merge cache, retirement proofs, and incoming
cursor live in one file, written with a private temporary file, file fsync,
atomic rename, and directory fsync. No key material or browser session state is
stored. The same mutex fences commit against account, feature, and association
changes. Any such change increments the revision before an old run can commit.
Negative eligibility decisions (logout, reset, unsupported/missing version and
lost account setup) first fsync a separate `received.invalidated` journal before
replacing `received.json`. The constructor prefers that journal to an older
ready cache, including when `received.pending` could not be written. A corrupt
journal fails closed; explicit recovery removes all three files before fetching
again. A successful complete state commit durably removes the journal.
The collection state machine validates the remote engine version **before**
association lookup or reset. Missing/declined definitions withdraw the cache as
`unpublished`; versions other than 1 withdraw it as `unsupportedVersion`.
Read-only providers still reach this preflight when the driver already knows
that the engine was declined, and are excluded from pre-validation local resets.

A run containing only read-only engines uses a discovery setup policy. It never
repairs or writes `meta/global`, and refuses the destructive fresh-start path
when `meta/global`, crypto keys or supported storage setup are missing. Those
definitive setup failures withdraw prior cache eligibility; a network failure
preserves the last confirmed offline snapshot. Ordinary and mixed write-engine
runs retain the existing full Sync setup policy.

Incoming transport sends a fixed `X-If-Unmodified-Since` boundary on every page,
keeps `X-Weave-Next-Offset` opaque, and checks every page timestamp. No incoming
records are staged to the engine until every page has succeeded. A 412 or later
failure discards all temporary incoming records; the next scheduled run starts
from the unchanged committed cursor. Staging is intentionally transient: a
crash redownloads from the previous durable cursor. Validation and merging also
finish before the atomic snapshot/cursor commit. Parent/child arrival order is
irrelevant; children stay hidden until metadata arrives. Tombstones dominate
all live clocks, survive restart, and are cleared on account/association change.

Snapshot `revision` also changes after an ordinary successful refresh. A copy
operation should freeze its selected snapshot and use the account, feature
epoch, global/collection sync IDs, version and eligible status to authorize
completion; an unrelated incoming refresh must not replace its selected URLs.

## Interoperability and tests

The golden fixture executes the pinned Desktop parser, rather than reimplementing
its expected outputs. To regenerate from the pinned `common` source directory:

```sh
node --experimental-strip-types components/floorp-workspaces-sync/tests/generate-desktop-fixture.mjs /path/to/desktop/common
cargo test -p floorp-workspaces-sync
cargo test -p sync15 --features sync-client
cargo test -p floorp-prefs-sync -p sync_manager
python3 -m unittest discover -s automation/floorp/tests
```

The fixture records the Desktop source revision and Node runtime. It checks
UUID case, clock 0, JS safe integers, Unicode/UTF-16 limits, JSON field order and
escaping, WHATWG input/navigation forms, credentials, and normalized size.
The tie-break compares UTF-16 code units. Preview tabs sort by position then
UUID collation (case-insensitive ASCII base, lowercase before uppercase), which
matches `localeCompare` for UUIDs under the pinned runtime's `en-US` and `ja-JP`
collation. IDs retain their original spelling and are never parsed/reformatted
through a UUID library.

Host crypto tests require the repository's normal NSS environment. Distributable
XCFrameworks and Swift bindings must be generated together from this source;
`verify-ios-xcframework-release.py` checks workspace symbols and headers in each
architecture, excludes them from Focus, and typechecks the generated Swift
store API alongside Notes and Sync Manager.

For local Apple Silicon simulator integration only, after building simulator
NSS, run `automation/floorp/build-workspaces-simulator-dev.sh /absolute/new/output`.
It packages one debug simulator slice and its actual generated bindings,
records dirty-source and library hashes in `developer-build.json`, and marks
it ineligible for release. It does not update production pins or publish files.
