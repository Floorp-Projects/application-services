# Floorp preferences Sync engine

This component transports Floorp Notes through Firefox Sync's encrypted
`prefs` collection. It is wire-compatible with Desktop's preferences engine:

- collection: `prefs`
- `meta/global` engine version: `2` (already declared by `sync15`)
- Firefox application record ID:
  `e2VjODAzMGY3LWMyMGEtNDY0Zi05YjBlLTEzYTNhOWU5NzM4NH0`
- Notes value: `floorp.browser.note.memos`
- control value:
  `services.sync.prefs.sync.floorp.browser.note.memos = true`

The engine never performs HTTP itself. Fetching, encryption, upload batching,
and `X-If-Unmodified-Since` are supplied by `sync15`. Every sync fetches the
single application record in full so an upload can preserve every unknown
entry in its aggregate `value` map. Unknown values and their entry order are
carried as raw JSON and are not normalized through `serde_json::Value`. The
engine only writes the Notes value and its control value.

The Sync Manager accepts `prefs` only when it is named in an explicit per-run
engine selection; the generic `All` selection excludes it. It omits `prefs`
from the toggleable/available-engine list and rejects both
`prefs = true` and `prefs = false` enabled-state changes. The generic disable
path would decline the shared engine account-wide and wipe the complete remote
`prefs` collection, including records and values that Floorp Notes does not
own. A Notes-only UI toggle must therefore remain local and must not be
translated into a global `prefs` enablement request.

## Embedding transaction

`FloorpPrefsSyncDelegate` is a synchronous UniFFI foreign callback. The Swift
implementation must provide a thread-safe, synchronous transaction adapter;
it must not wait for an actor scheduled on the same executor. Callbacks may
read the Rust Sync state, but must not synchronously start another prefs
Sync/reset/disconnect operation before returning.

1. `prepare` receives the typed remote Notes state and returns an opaque store
   token plus either no upload or the merged Notes JSON string.
2. Rust gives that token back to `sync_finished` only after `sync15` confirms
   the aggregate upload. A transport or upload failure does not commit it.
3. `sync_state_changed` persists the successful collection timestamp.
4. `association_reset` persists new Sync IDs, resets the timestamp, and tells
   the application to invalidate its three-way-merge base without deleting
   local Notes. Embedders that require a durable reset during sign-out should
   call Sync Manager's throwing `disconnect_checked` entry point. The legacy
   non-throwing `disconnect` API remains available for existing callers and
   reports persistence failures without returning them.

`maximum_notes_value_bytes` is dynamic: it subtracts the exact aggregate
framing and every preserved unknown entry from the local cleartext limit. It
counts the candidate Notes string as a JSON-encoded string value, including
the surrounding quotes, so the Swift adapter must measure the encoded value
rather than the unescaped UTF-8 string.

Payloads and transaction tokens are deliberately absent from logs. A
conservative cleartext limit leaves room beneath Sync's default 256 KiB
encrypted-record limit; `sync15` still enforces the server-advertised limit at
upload time.
