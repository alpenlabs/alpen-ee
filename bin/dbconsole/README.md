# `dbconsole`

A console over the EE MDBX store. Values are decoded through the node's own
codecs, so what it prints is what the node reads — never a hand-rolled parser
that can drift from the schema.

It attaches to every environment under `<datadir>/mdbx` — `node`, `prover`,
`witness`, `da` — and routes a table name to the environment it lives in. An
environment whose directory is missing (a full node has no `prover`) is listed
as absent rather than failing the attach.

It attaches **read-only by default** and never takes the environment write lock,
so it can be pointed at a running sequencer as safely as at a stopped one.
Writes need `--allow-writes`, which attaches exclusively and therefore only
works with the node stopped — see [Writes](#writes).

```bash
cargo build -p alpen-dbconsole
./target/debug/dbconsole --datadir <EE datadir>
```

`--datadir` is the directory that *contains* `mdbx/`, not the `mdbx/prover`
environment itself — e.g. `functional-tests/_dd/<RUN>/el_ol/ee_sequencer`.

---

## Contents

- [Architecture](#architecture) — how the pieces fit
- [Command reference](#command-reference) — reading
- [Scripting](#scripting) — functions, files, interrupts, history
- [Recipes](#recipes) — the shipped operator procedures
- [How values read](#how-values-read)
- [Writes](#writes) — editing, staging, committing
- [Type fidelity](#type-fidelity) — what is guaranteed, and why
- [Restrictions](#restrictions) — what it cannot do yet
- [Extending it](#extending-it) — adding a table
- [Getting a store to play with](#getting-a-store-to-play-with)
- [Troubleshooting](#troubleshooting)

---

## Architecture

Two halves, split on purpose:

```
  bin/dbconsole/src/dbconsole/          the shell — rhai, rendering, prompts
      repl.rs      the prompt: line editing, meta-commands, rendering
      session.rs   what persists between inputs: scope, function library, interrupt
      engine.rs    rhai engine; registers the verbs
      value.rs     FieldValue <-> rhai Dynamic
      handle.rs    a held value + the table it came from
                            |
                            | Record / FieldValue  (engine-neutral)
                            v
  crates/alpen-ee/database/src/console/   the core — codecs, transactions
      db.rs        ConsoleDb: attach every env, route, read, stage, commit
      registry.rs  TableReflect: per-table get/scan/put; the envs and their tables
      reflect.rs   ValueReflector: value <-> FieldValue, both ways
      key.rs       ConsoleKey: text <-> typed table key
      value.rs     Record, FieldValue
```

The core never depends on the scripting engine. It hands out `Record`s and
`FieldValue`s; the shell decides how those look in rhai and at the prompt. A
different shell could sit on the same core.

### The type layer

Four traits carry the whole design.

**`Schema`** (from `alpen-store-mdbx`) is what a table already declares for the
node: a name, a key type, a value type. The console adds nothing to it — it
consumes the same schema the node writes through, which is what keeps the two
from drifting.

**`ConsoleKey`** converts a table's key between its typed form and the text a
user types. Implemented once per *key type*, not per table, so a new table whose
key type is already covered gets `get` for free:

| Key type | Written as | Example |
|---|---|---|
| `Vec<u8>` | hex, `0x` optional | `"61044945…41d91"` |
| `Buf32` / `Hash`, `B256`, `TxNodeId` | 32-byte hex | `"0389ca9e…631e"` |
| `u32`, `u64` | decimal | `"41"` |
| `DBBatchId`, `DBChunkId` | `prev:last` hex pair | `"3324fbd0…:044945f0…"` |
| `DBOLBlockId` | 32-byte hex | `"c9a033ca…0caf"` |

Rendering and parsing must round-trip — a key printed by a scan has to paste
back into a `get` — and there is a test per key type asserting exactly that.

**`ValueReflector<V>`** converts a table's stored value to `FieldValue` and back.
It is a *strategy chosen by the table*, not a trait on the value type. That
distinction is what lets a blanket impl and per-table overrides coexist, which a
trait on the value could not do without specialization:

```rust
pub trait ValueReflector<V> {
    fn to_value(value: &V) -> Result<FieldValue, ReflectError>;
    fn from_value(value: &FieldValue) -> Result<V, ReflectError>;
}

pub struct SerdeReflector;   // the default: blankets every Serialize + Deserialize value
pub struct BytesReflector;   // a table whose whole value is Vec<u8>: one byte string
pub struct MirrorReflector<M>; // reflects through a serde mirror M (see below)
pub struct Unreflectable;    // refuses, for a value no strategy can read yet
```

`SerdeReflector` is a `Serializer`/`Deserializer` pair over `FieldValue`. Because
it works on the *decoded value*, the table's storage codec is irrelevant —
borsh, CBOR, bincode and `strata-codec` tables all reflect identically. The codec
only has to get from bytes to the Rust type.

Byte payloads are the one place serde's shape is wrong for a console: a
`Vec<u8>` is a *sequence* to serde, so a megabyte proof would reflect as a
million integers, twice over, before a predicate ran (measured: 6.6 GiB peak
for 200 one-megabyte receipts). Two reflectors say "these are bytes" without
guessing. `BytesReflector` covers a table whose whole value is `Vec<u8>`. A
`Mirror` redeclares a foreign type's fields under the same names with its byte
vectors marked as bytes, and `MirrorReflector<M>` reflects through it — the
proof receipt tables use `ProofReceiptMirror`. A repo-owned mirror type needs
neither: mark the field with `#[serde(with = "serde_bytes")]` directly.

**`TableReflect`** is the object-safe per-table face of all this: `count`, `get`,
`scan`, `put`, `delete`, plus the checks staging runs. `Reflected<S, R>`
implements it generically for any `S: Schema` whose key is a `ConsoleKey` and
whose value some reflector `R` can read — so the mechanical half (read
transactions, key parsing, the scan loop and its predicate-abort dance) is
written once, not per table.

### Environments

The EE store is four MDBX environments, laid out by `open_ee_db`:

| Environment | Tables | Holds | Present on |
|---|---|---|---|
| `node` | 14 | chain state: exec blocks, heights, payloads, batches, chunks, account state per OL epoch, the accessed-state and witness caches | every node |
| `prover` | 4 | prover tasks and proof receipts | sequencer |
| `witness` | 3 | reth state diffs per block, block hashes by number, published code hashes (schemas from `alpen-reth-db`) | sequencer |
| `da` | 5 | L1 broadcast queue, replacement chains, chunked envelopes | sequencer |

Every table each environment creates is registered, and a test compares the
registry against the node's own table lists so the two cannot drift. A second
test seeds all 26 tables through the node's write paths and checks that every
row reflects, that its rendered key parses back, and that its value survives
the round trip a write is gated on.

`ConsoleDb` opens each one that exists and records the rest as absent. The
registry mirrors the layout: one `console_tables!` block per environment, and
`ee_envs()` listing them in the order `.tables` prints.

A table is named bare (`"ProverTaskSchema"`) or qualified (`"prover/ProverTaskSchema"`).
A bare name resolves while it is unique across environments, which every EE
table is today and a registry test enforces; the qualified form is accepted
everywhere and is required only if that ever changes. A table in an absent
environment is found and refused with a message saying which environment is
missing, rather than reported as unknown.

MDBX has no transaction spanning environments, so `commit()` runs one
transaction per environment — see [Writes](#writes).

### Data model

```rust
pub struct Record {
    pub key: String,        // rendered, exactly as `get` accepts it
    pub value: FieldValue,  // the decoded value
}
```

The same two halves MDBX stores, named the same way at every layer — in the Rust
types, in the reflector, and at the prompt. A record's key is *where the value
lives*, never a field of it, so a value with a field of its own called `key`
keeps it.

`FieldValue` mirrors serde's data model exactly — see
[Type fidelity](#type-fidelity) for why each distinction is load-bearing:

| Variant | Holds |
|---|---|
| `Null`, `Unit` | `None`, and `()` — kept apart |
| `Bool`, `F64` | as named |
| `U64`, `I64`, `U128`, `I128` | integers, widened |
| `Str`, `Bytes` | a string; a byte string |
| `Enum(name)` | a fieldless variant |
| `Variant { name, value }` | a variant with a payload |
| `List` | sequence, tuple, tuple struct |
| `Map(Vec<(FieldValue, FieldValue)>)` | struct or map — keys are *values* |

### The rhai layer

rhai 1.26 with `default` + `std`. Its value model is the constraint that shapes
this layer:

| rhai primitive | Rust |
|---|---|
| `Unit` | `()` |
| `Bool` | `bool` |
| `Char` | `char` |
| `Str` | `ImmutableString` |
| `Int` | **`i64` — the only integer type** |
| `Float` | `f64` |

| rhai compound | Rust |
|---|---|
| `Array` | `Vec<Dynamic>` |
| `Blob` | `Vec<u8>` |
| `Map` | object map, `ImmutableString` keys |
| `FnPtr`, `TimeStamp` | as named |
| `Variant` | any Rust type (this is how `Value` is registered) |

`Decimal` exists behind a feature flag that is **not** enabled.

A record crosses as `{ key, value }`. Values that do not fit rhai's model
degrade explicitly rather than wrapping:

- a `u64`/`u128` past `i64::MAX` crosses as its **big-endian bytes**, not a
  negative number
- a `FieldValue::Map` with structured keys has those keys rendered to strings
  (display only — a write never rebuilds from this side)

The engine keeps rhai's depth limits (64 call levels, expression depth
128/64), which guard the stack. There is no operation cap: a loop over a large
table has no natural budget, and a slow evaluation is stopped by hand with
Ctrl-C — see [Scripting](#scripting).

### Accessors

`edit()` returns a **`Value` handle**, not a plain map — a decoded value plus the
table it came from:

```rhai
let v = edit("ProverTaskSchema", K);
v["updated_at_secs"]        // indexer read
v.get("status")             // same thing
v.set("status", "Pending"); // checked against the table's real type, immediately
v.table()                   // "prover/ProverTaskSchema"
```

Carrying the table is what lets `set` validate at the point of the mistake
instead of when the write is staged. The value alone knows its *shape* (which
fields exist); only the table knows their *types*.

---

## Command reference

Lines beginning with `.` are meta-commands. Everything else is a
[rhai](https://rhai.rs) expression: a bare expression prints its value, and
statements need a trailing `;`.

### Meta-commands

| Command | Does |
|---|---|
| `.tables` | every table with its row count |
| `.schema <table>` | one table's key shape and value shape |
| `.staged` | edits waiting for a `commit()` |
| `.fns` | the functions this session knows, with their doc comments |
| `.recipes` | the shipped recipes, with what each stages |
| `.load <file>` | run a `.rhai` file here; its functions stay defined |
| `.help` | verbs and meta-commands |
| `.quit` / `.exit` | leave |

```
db> .tables
[node]
  table                            rows
  OLBlockAtEpochSchema                7
  AccountStateAtOLEpochSchema         7
  ExecBlockSchema                   370
  ExecBlocksAtHeightSchema          370
  ExecBlockFinalizedSchema            1
  ExecBlockPayloadSchema            370
  BatchByIdxSchema                   37
  …
[prover]
  table                            rows
  ProverTaskSchema                   49
  ChunkProofReceiptSchema            25
  …
[witness]
  table                            rows
  BlockStateChangesSchema           369
  BlockHashByNumber                 369
  PublishedCodeHashSchema             0
[da]
  table                            rows
  L1BroadcastTxIdSchema              72
  …
```

Check `.schema` before trusting a key format:

```
db> .schema ProverTaskSchema
  table: ProverTaskSchema
  env:   prover
  key:   tag-prefixed ProofSpec::Task bytes ([u8] -> hex)
  value: TaskRecordData { status, updated_at_secs, retry_after_secs, metadata }
```

Every verb takes a table name bare or as `env/Table`; the two are
interchangeable while the bare name is unique.

### `get(table, key)` — one row by key

```rhai
get("ProverTaskSchema", "61044945f0dd…41d91")
// → { key: 61044945…, value: { status: Completed, updated_at_secs: …, … } }

get("ProverTaskSchema", "61044945…").value.status    // → Completed
get("ProverTaskSchema", "61044945…").key             // → 61044945…
```

A miss returns unit rather than erroring, so test it explicitly:

```rhai
get("ProverTaskSchema", "deadbeef") == ()            // → true
```

Key formats vary by table — `.schema` names the one you need:

| Key type | Written as | Example |
|---|---|---|
| bytes / hash | hex, `0x` optional | `"61044945…41d91"` |
| `u64` / `u32` | decimal | `"41"` |
| batch / chunk id | `prev:last` hex pair | `"3324fbd0…48fc:044945f0…dda56"` |

```rhai
get("AcctProofReceiptSchema", "3324fbd0…48fc:044945f0…dda56")
```

### `count(table)` — row count

O(1) through MDBX's table statistics; the same number `.tables` shows. Use it
instead of `count_where(t, |r| true)`, which walks and decodes every row.

### `count_where(table, pred)` — count matching rows

```rhai
count_where("ProverTaskSchema", |r| true)                                      // 84
count_where("ProverTaskSchema", |r| r.value.status == "Completed")             // 84
count_where("ChunkProofReceiptSchema", |r| r.value.metadata.zkvm == "Native")  // 42
count_where("ProverTaskSchema", |r| r.key.starts_with("6109"))                 // by key
```

### `scan_where(table, pred)` — list matching rows

```rhai
scan_where("ProverTaskSchema", |r| true)
scan_where("ProverTaskSchema", |r| r.value.status != "Completed")
scan_where("ProverTaskSchema", |r| r.value.updated_at_secs > 1789600000)
```

### Limits, direction, and the ends of a table

A scan takes an optional limit, counted in matches, and stops as soon as it is
reached; `scan_rev_where` walks from the end. `first` and `last` are the
one-record cases, returning unit on an empty table.

```rhai
scan_where("ProverTaskSchema", |r| r.value.status == "Pending", 10)   // first 10 pending
scan_rev_where("ProverTaskSchema", |r| true, 5)                         // last 5 by key
last("ProverTaskSchema")                                                // the highest key
first("BatchByIdxSchema").value                                         // idx 0
```

"Last by key" is only "newest" for a table whose key is an index or height —
the big-endian `u64` tables. A table keyed by hash has no useful order, and
"newest" there still means a full scan and a sort in rhai:

```rhai
let rows = scan_where("ProverTaskSchema", |r| true);
rows.sort(|a, b| b.value.updated_at_secs - a.value.updated_at_secs);
rows.extract(0, 5)                    // newest 5 records
rows.extract(0, 5).map(|r| r.key)     // just their keys
```

`rows[0]`, `rows.len()`, `.filter()`, `.map()` all work — it is an ordinary
array of records. The prompt prints the first 100 rows of a result and says
how many more there are; the array itself is whole.

### Ranges and prefixes on ordered keys

Where a table's key bytes sort the way its keys do, a walk can start and stop
inside the table instead of reading it end to end. That holds for the
big-endian integer keys (heights, indices) and the raw task-key bytes; it does
not hold for a borsh-encoded key, and a range over one is refused with a
message rather than answered wrongly.

```rhai
scan_range("ExecBlockFinalizedSchema", "100", "200", |r| true)      // heights 100..=200
scan_rev_range("BatchByIdxSchema", "0", "41", |r| true, 5)          // the 5 batches up to 41
keys_range("ExecBlocksAtHeightSchema", "100", "200")                // just the heights
scan_prefix("ProverTaskSchema", "0001", |r| r.value.status != "Completed")
keys_prefix("ProverTaskSchema", "0001")                             // one kind of task
scan_rev_prefix("ProverTaskSchema", "0001", |r| true, 1)            // the last of that kind
```

`from` and `to` are written exactly as `get` takes them and both ends are
included. A prefix is hex, whole bytes; for a `prev:last` pair key it is a
prefix of `prev`, written without the colon. The cost is the rows in the range
plus one cursor seek, whatever the table's size.

### `keys(table)`, `keys_where(table, pred [, limit])` — keys only

Walk the keys without decoding a single value. The predicate receives the
rendered key string. This is the cheap way to select rows by key shape, and
the natural input to a batch of `del`s:

```rhai
keys("AcctProofIdIndexSchema")                                  // every key
keys_where("ProverTaskSchema", |k| k.starts_with("0001"))       // by prefix
keys_where("ProverTaskSchema", |k| k.starts_with("0001"), 100)  // first 100 of them
```

`r.key` is rendered in exactly the form `get` accepts, so a key copied out of a
scan pastes straight into a lookup.

### From the shell

```bash
# evaluate one expression and exit
dbconsole --datadir <dir> -c 'count_where("ProverTaskSchema", |r| true)'

# run a script file and exit; its last value is printed
dbconsole --datadir <dir> --script ops/find_stuck.rhai

# -c and --script do not take dot meta-commands; pipe those on stdin
printf '.tables\n.quit\n' | dbconsole --datadir <dir>
```

Piped input is read line by line, and an unfinished input (an open `fn` body,
a `for` block) is joined with the lines after it, so a script piped in behaves
as it would typed, with one difference: the first input that fails ends the
run with exit status 1, like a shell under `set -e`, so a `commit()` further
down never applies a batch that was only half staged. A meta-command that
fails (a `.load` whose file errors part-way, an unknown table in `.schema`)
and input that ends inside an unfinished expression count as failures too.
At the prompt the error is shown and you decide.

---

## Scripting

The prompt is a rhai session, not a line evaluator.

**Functions persist.** A `fn` defined at the prompt stays defined for the
session, and a later definition of the same name and arity replaces it. This
is what makes a recipe possible: a function written once, called with
arguments as often as needed.

```
db> fn stuck(secs) {
...   scan_where("ProverTaskSchema", |r| status_name(r.value.status) == "Proving" && r.value.updated_at_secs < secs)
... }
db> stuck(1789600000).len()
3
```

Variables persist too, as rhai's scope. A rhai function sees only its
parameters, never the prompt's variables, so pass what it needs.

**Inputs span lines.** An open block or expression continues on the next line
until it parses; a genuine syntax error is reported at once. `.fns` lists the
session's functions with their `///` doc comments.

**Files.** `.load <file>` runs a `.rhai` file in the session, so its functions
stay defined and its top-level statements run; `--script <file>` runs one and
exits. An operator's own helpers live in a file and load in one line.

**Ctrl-C stops an evaluation, not the session.** The interrupt is checked
between rhai operations, including the operations of a predicate running
inside a native scan, so a slow `scan_where` stops within a row. Staged edits
are kept; `.staged` shows them. A verb that runs no script at all — `keys(t)`,
`count(t)`, `commit()` — is not cut short, which is the point for `commit()`:
a transaction in flight always completes, and the interrupt lands after it.
At an empty prompt, Ctrl-C clears the line as in a shell.

**Line editing and history.** Arrow keys, editing, and a history that
persists across sessions in the user's data directory
(`$XDG_DATA_HOME/alpen-ee/dbconsole_history`, or the platform equivalent),
never inside the node's datadir.

---

## Recipes

A recipe is a rhai function shipped with the binary, defined in every session
before the first input, for a procedure an operator would otherwise script by
hand each time. They live in `bin/dbconsole/recipes/*.rhai`, are embedded at
build time, and are tested against a store seeded through the node's own
write paths, so they are versioned, reviewed and checked like code.

A recipe **stages** its edits and returns how many; it never commits.
`.staged` is the preview and `commit()` the act, so every recipe is a dry run
until you say otherwise. `.recipes` lists them:

| Recipe | Stages |
|---|---|
| `prover_summary()` | nothing; tasks counted by status |
| `prover_task(key)` | nothing; one task and the receipt it points to |
| `prover_reset(key)` | the task's status back to `Pending` |
| `prover_abandon(key, reason)` | the task's status to `PermanentFailure` |
| `prover_delete(key)` | the task, its receipt, and the proof-id index entries of an account receipt |
| `chain_summary()` | nothing; tip and finalized heights, counts, latest batch and chunk |
| `drop_chain_above(height)` | every exec block above `height` with its payload, accessed state and witness; the height and finalized entries; the witness environment's diffs above `height`; the OL epoch entries whose accepted account state points at a dropped block, so the tracker resumes from the last surviving epoch; then `revert_batches_from` for the first batch ending above `height`. Chain, witness and batches are each cut by their own top, so a witness that ran ahead is trimmed even when the chain is already at `height` |
| `batch_summary()` | nothing; batches and chunks counted by status |
| `revert_batches_from(idx)` | batches from `idx`, their id and chunk-list entries, and every chunk of those batches |
| `broadcast_summary()` | nothing; the L1 queue by status, replacement chains, envelopes |

```
db> chain_summary()
{ batches: 37, blocks_at_tip: 1, chunks: 36, exec_blocks: 370, finalized_height: 0, … }
db> drop_chain_above(300)
513
db> .staged
513 staged change(s); `commit()` applies them:
  node/ExecBlockSchema: 69 del, 0 put
  node/ExecBlockPayloadSchema: 69 del, 0 put
  …
  witness/BlockStateChangesSchema: 69 del, 0 put
  …
  node/BatchByIdxSchema: 6 del, 0 put
  (`.staged full` lists every edit)
db> commit()
committed 375 edit(s) to `node`
committed 138 edit(s) to `witness`
513
```

A recipe that touches two environments stages the authoritative one first,
because `commit()` applies environments in first-staged order and cannot be
atomic across them. If the witness environment fails after `node` landed, a
regenerable cache is stale and nothing authoritative is inconsistent.

To add a recipe: write the function with a `///` doc comment in the fitting
file under `recipes/`, add a test in `src/dbconsole/recipes.rs` against the
seeded store, and the listing test checks it is documented.

---

## How values read

Everything is a **record**: `{ key, value }`. `r.key` is where it lives,
`r.value` is what it holds. Predicates receive a record and scans return one, so
what you filter on and what you get back are the same shape.

Values are reflected through their serde shape, so a row reads the way the Rust
type is written rather than a flattened re-spelling of it.

A fieldless enum variant is a bare string — the case predicates lean on most:

```
value: { status: Completed, … }

db> count_where("ProverTaskSchema", |r| r.value.status == "Completed")
```

A variant that carries data nests under its own name, so it is a map, not a
string, and comparing it to a string never matches. Two idioms cover both
shapes. `status_name`, shipped with the recipes, gives the variant's name
whether or not it carries data:

```
value: { status: { Blocked: { reason: "chunk receipt missing for …", … } }, … }

db> scan_where("ProverTaskSchema", |r| status_name(r.value.status) == "Blocked")
```

To reach the payload, check the shape first: `.Blocked` on a row whose status
is the plain string `Completed` is an error in rhai, not unit.

```
db> scan_where("ProverTaskSchema", |r| { let s = r.value.status; s.type_of() == "map" && s.Blocked.reason.contains("missing") })
```

Nested structs nest too, and predicates walk them with dotted syntax:

```
value: { metadata: { program_id: 0x0000…, proof_type: Groth16, zkvm: Native }, … }

db> count_where("ChunkProofReceiptSchema", |r| r.value.metadata.proof_type == "Groth16")
```

Byte runs of 16 or more **print** as hex rather than as a list of numbers. That
is a display rule only: a `[u8; 32]` reaches serde as a sequence of integers
with nothing marking it as bytes, and the row keeps it that way so it can be
written back unchanged. A predicate still sees the array, and a list of 16+
small integers that is not really bytes prints as hex without being altered.

A value the table declares as bytes (a proof, a payload) crosses as a rhai
blob: `.len()` works in a predicate, and it prints abbreviated past 64 bytes —
`0x349d…c6f0 (1048576 bytes)` — with every byte still in the record.

---

## Performance notes

Measured in a release build on a real 6,569-block, 53 MB store the node
produced, one process per query through `-c`, attach and process start
included:

| Operation | Wall | Peak RSS |
|---|---|---|
| point read, `first`/`last`, any range or limit, any summary recipe | 0.03–0.08 s | 9–20 MiB |
| `count_where` over every block | 0.05 s | 35 MiB |
| `scan_where` every block, materialised | 0.06 s | 54 MiB |
| `del` 6,568 rows by key list and commit | 0.06 s | 15 MiB |
| `drop_chain_above` half the chain, 24,864 edits, stage | 0.14 s | 72 MiB |
| the same with `commit()` | 0.19 s | 83 MiB |

Everything interactive is under a tenth of a second; the floor is process
start and attach. Memory is the thing to watch, not time:

- `scan_where` without a limit materialises every match as a rhai map;
  100k matches is about 200 MiB. Use a limit, `keys_where`, or `count_where`
  (which keeps nothing) when you do not need the rows.
- A limited scan stops at the limit, and `scan_rev_where` reads from the end,
  so "last N by key" reads N rows.
- A range or prefix walk seeks straight to its first key and stops at its
  last, so its cost is the size of the range, not the table.
- Every walk reads in pages of 10,000 rows, each its own short read
  transaction, so a slow predicate beside a running node never pins the
  store's snapshot and blocks page reclamation. The price is that such a walk
  is not atomic: a row the node writes or removes while it runs may or may
  not be seen. With the node stopped nothing changes underneath it.
- `sort` and `extract` run in the console *after* the scan returns, so "newest
  N" on a hash-keyed table still reads every row.
- `keys`/`keys_where` never decode a value, which matters on a table whose
  values are megabytes.
- `count(table)` is O(1) via MDBX stat; `count_where` walks and decodes.
- `v.set(...)` decodes the whole value once to check the edit. Negligible for a
  prover task; real work for a value carrying a megabyte proof blob.

## Writes

Writes are **offline only**. `--allow-writes` opens the environment with MDBX's
exclusive flag, which fails outright while any other process holds it — so the
attach cannot race a running node. That is deliberate rather than cautious: a
node keeps state in memory the store alone does not capture, so an outside edit
would be clobbered by its next flush or never observed.

```bash
dbconsole --datadir <dir> --allow-writes
```

Edits are **staged**, never applied as typed — an MDBX environment admits one
writer, and holding a transaction open across a prompt would stall whoever wants
it next. `commit()` applies the batch in one short transaction **per
environment**, so a batch is atomic within an environment; `abort()` discards
it. MDBX offers nothing across environments: a batch touching two lands in two
steps, in the order they were first staged. An environment's edits leave the
batch as soon as they land, so if a later one fails, `.staged` shows exactly
what was not applied and the error names what was. A batch that must be
all-or-nothing should stay within one environment.

```rhai
del("ProverTaskSchema", "61044945…")                           // stage a deletion
del("ProverTaskSchema", keys_prefix("ProverTaskSchema", "0002")) // stage many, all or none
set("ProverTaskSchema", "61044945…", "updated_at_secs", 4242)  // stage a field edit
commit()                                                       // → number applied
abort()                                                        // → number discarded
```

A key can have one staged edit at a time. A second `del`, `put` or `set` on a
key that already has one is refused naming the first — two edits to one key
would apply in staging order, which is easy to get wrong, and two deletes
would fail the whole environment's batch at commit. `commit()` or `abort()`
first. The list form of `del` checks every key's presence in one read
transaction and stages all of them or none.

`.staged` lists each edit while there are few and summarises by table past
twenty; `.staged full` lists every edit. When a batch spans environments,
`commit()` prints how many edits landed in each.

### Whole values

`set` is the shortcut for one field. To change several, or to write a value at a
different key, read it out and edit it in hand:

```rhai
let v = edit("ProverTaskSchema", "61044945…");  // decoded value, no key attached
v["updated_at_secs"]                            // read a field
v.set("updated_at_secs", 4242);                 // change fields
v.set("status", "Pending");
put("ProverTaskSchema", "61044945…", v);        // write it back
put("ProverTaskSchema", "aabbcc", v);           // or at another key — a copy
commit()
```

`put` overwrites whatever is at the key; `.staged` says `(create)` or
`(overwrite)` so you can see which before committing.

A held value stays in its decoded form for the whole trip — it is never rebuilt
out of the script. That matters because the script boundary is many-to-one: `()`
could be `None` or unit, a string could be a field or an enum variant, a blob
could be bytes or a large integer. Editing the held value keeps every field you
did not touch exactly as it was read.

The handle remembers which table it came from, so every edit is checked against
that table's real type **as it is made**:

```
db> v.set("status", #{ Nonsense: 1 })
error: unknown variant `Nonsense`, expected one of `Pending`, `Proving`, …

db> v.set("updated_at_secs", "not a number")
error: expected u64, found "not a number"
```

A rejected edit leaves the value untouched, and an accepted one is stored in
canonical form — so `v["status"]` after `v.set("status", "Pending")` reads back
`Pending`, exactly what a write would store. `v.table()` names its origin, and a
value can only be written to the table it came from.

A fieldless enum variant can be set by name (`v.set("status", "Pending")`) since
a prompt has no enum literal. The name is checked against the real variant set,
and the canonical variant is what gets stored.

```
db> .staged
1 staged change(s); `commit()` applies them:
  [0] put prover/ProverTaskSchema 61044945… (updated_at_secs=4242)
```

Staging validates the table, the key, the row's presence, the field name and the
value up front, so a mistake is reported next to the command rather than failing
the batch at commit.

Every write is a read-modify-write: the value is decoded, part of it is
replaced, and the whole thing is encoded back. Before accepting one the console
converts it to the exact form the table's decoder produces and checks that form
round-trips unchanged — a table whose reflector is lossy is refused rather than
having its untouched fields quietly rewritten.

### Writing every shape

The prompt has no literal for a Rust enum, and rhai has one integer type, so
some values are written in a looser form and **resolved against the field's real
type**. Nothing is guessed: serde asks the type what it expects, an input it
cannot account for is refused, and the canonical form is what gets stored.

| Field's type | Write it as | Stored as |
|---|---|---|
| fieldless variant | `"Pending"` | `Pending` |
| variant with a payload | `#{ Blocked: #{ reason: "…", counts: #{ … } } }` | `Blocked({ … })` |
| nested struct | `#{ version: "9.9.9", … }` | the struct |
| list | `[1, 2, 3]` | the list |
| integer past rhai's `i64` | `"18446744073709551615"` | the number |
| bytes | a blob | the bytes |

A held value nests directly, so a sub-struct can be pulled out, changed, and put
back:

```rhai
let v = edit("ChunkProofReceiptSchema", K);
let m = v["metadata"];
m.version = "9.9.9";
v.set("metadata", m);
put("ChunkProofReceiptSchema", K, v);
```

`.staged` prints the canonical value before you commit, so what will be written
is visible first. Anything malformed is named precisely:

```
db> v.set("status", #{ Nonsense: #{} }); put("ProverTaskSchema", K, v);
error: unknown variant `Nonsense`, expected one of `Pending`, `Proving`,
       `Completed`, `Blocked`, `TransientFailure`, `PermanentFailure`

db> v.set("status", #{ Blocked: #{ reason: "x" } }); put(…)
error: missing field `counts`
```

## Type fidelity

Editing a field is a read-modify-write: the value is decoded, part of it is
changed, and the whole thing is encoded back. So `to_value` followed by
`from_value` must reproduce the original **exactly**, or an edit would silently
rewrite fields nobody named. This is a database; that is not acceptable.

`FieldValue` therefore mirrors serde's model precisely. Several distinctions
look redundant and are not:

- **`Null` vs `Unit`** — `None` and `()` are different values. One variant for
  both would flip `Option<()>` on write-back.
- **`Bytes` only from `serialize_bytes`** — a `[u8; 32]` reaches serde as a
  sequence of integers with nothing saying it is bytes, so it stays a `List`.
  Calling it bytes would be a guess, and a guess cannot round-trip. (The hex
  *display* is the renderer's business, where being wrong changes nothing.)
  A `BytesReflector` or a `Mirror` is the table saying so, which is not a guess.
- **`Map` keyed by `FieldValue`** — a `BTreeMap<u64, _>` field must survive.
  Stringifying keys would lose it.
- **`Variant` distinct from a one-entry `Map`** — so an enum payload is never
  confused with a map that happens to have one key.
- **Integers widen out, range-check back** — lossless, because the widened value
  came from the narrower type. An out-of-range edit is refused, never truncated.

This is verified, not asserted: a proptest in `reflect.rs` asserts
`from_value(to_value(v)) == v` over generated values covering `u128::MAX`,
`Option<()>`, `BTreeMap<u64, String>`, `[u8; 4]`, unit/newtype/tuple/struct
variants, and `f32`/`f64`.

Before any write is staged the value is **canonicalised**: converted to the
exact form the table's decoder produces, and that form verified to round-trip. A
table whose reflector is lossy is refused rather than having its untouched
fields quietly rewritten. This is also what lets loose prompt input be accepted
without weakening the guarantee — the loose spelling is input-only, and never
what gets stored.

---

## Restrictions

**The exec block's package is opaque.** `ExecBlockSchema` stores the block
package as SSZ bytes inside a borsh record, and the console shows it as bytes.
Decoding it into fields needs a reflector that runs the SSZ decoder, which is
a follow-up.

**Writes are offline only**, by design. There is no supported way to edit a
store a node is using, and the exclusive attach enforces it.

**Integers above `i64::MAX` cannot be typed as numbers.** rhai has one integer
type. Such a value *reads* as a big-endian blob and can be *written* as a
decimal string, but it cannot be compared numerically in a predicate — `>` on a
blob is an unknown operator, which aborts the scan. No EE table currently has a
field wider than `u64`.

**A field that is sometimes huge changes rhai type between records** — `Int` on
most rows, `Blob` where it exceeds `i64::MAX`. Forced by rhai's single integer
type; it cannot be fixed in the representation, only in how values are
presented.

**One error at a time.** A trial conversion stops at the first failure, so a
value with two bad fields reports one, then the next once it is fixed. Each
`v.set` is checked separately, so each error lands on the line that caused it.

**Ranges only on ordered keys.** A range or prefix walk needs the key's byte
order to be its logical order, which the big-endian integer keys and the raw
task-key bytes have and the borsh-encoded keys (hashes, batch and chunk ids,
the chunk receipt's task bytes) do not. On those, select by key with
`keys_where` and a predicate; there are no secondary indexes.

**No transactions across commits, or across environments.** `commit()` is one
transaction per environment. There is no way to hold a transaction open across
several prompts — deliberately, since that would stall the single writer slot —
and none to make edits in two environments land together, since MDBX has no
such transaction.

**Class-based write guards were removed.** Nothing currently distinguishes a
canonical table from a regenerable one, because the storage layer does not model
that. Such a guard belongs back once it can be enforced rather than asserted.

---

## Extending it

Adding a table is one entry in `console_tables!`:

```rust
console_tables! {
    pub(crate) fn prover_env_tables() {
        ProverTaskSchema => {
            key: "tag-prefixed ProofSpec::Task bytes ([u8] -> hex)",
            value: "TaskRecordData { status, updated_at_secs, … }",
        },
        // a table whose value no strategy can read yet:
        AwkwardSchema => {
            key: "…",
            value: "…",
            reflector: Unreflectable,
        },
    }
}
```

The value's shape comes from its reflector, which defaults to `SerdeReflector`,
so no per-table mapping code is needed. Requirements:

1. `S::Value: Serialize + DeserializeOwned` — add the derives if missing.
2. `S::Key: ConsoleKey` — free if the key type is already covered; otherwise one
   impl plus a round-trip test.
3. For a value type you own, prefer `#[serde(with = "hex::serde")]` on `[u8; N]`
   fields and `#[serde(with = "serde_bytes")]` on `Vec<u8>` fields. Both are
   exact, and avoid leaning on the display-only hex heuristic.
4. A table whose whole value is `Vec<u8>` names `reflector: BytesReflector`.
5. A foreign type with byte vectors gets a `Mirror` in `console/mirrors.rs` —
   the same fields under the same names, byte fields marked — and names
   `reflector: MirrorReflector<TheMirror>`. The round-trip check at staging
   catches a mirror that drops a field.
6. A table declared in another crate needs its schema marker public: declare
   it as `(pub Name)` in the `define_table*!` macro and re-export it, as
   `alpen-reth-db` does for the witness tables.
7. Seed a row in the new table in the crate's `test_db.rs`, the store the
   recipe tests run against, so a recipe touching it has something to find.

A different reflection strategy (borsh schema, `strata-codec`, a hand mapping)
is a new marker type implementing `ValueReflector`; nothing else changes.

---

## Getting a store to play with

Any EE datadir works. To make one from scratch, run a long-lived environment
and feed it.

```bash
# terminal 1 — start the environment and leave it up
cd functional-tests
./run_tests.sh --keep-alive el_ol
```

It prints the run directory name:

```
DATADIR NAME 16-22-lwndm
```

```bash
# terminal 2 — drive it
cd functional-tests
uv run python -m scripts.drive_keepalive
```

**Step 2 is not optional.** `--keep-alive` starts the services but runs no miner
and submits no transactions — in a normal test run the test body does both.
Without a driver, bitcoin sits at `pre_generate_blocks` forever. The EE chain
still builds blocks on its own timer, so the environment *looks* healthy, but OL
epochs never advance, the prover pipeline never receives a batch, and every
prover table stays empty with nothing in the logs to explain why.

Both inputs are required: mining alone seals empty batches with nothing to
prove, and transactions alone pile up on an EE chain whose OL side is frozen.

`drive_keepalive` takes `--mine-interval`, `--tx-batch`, `--duration`,
`--no-mine`, and `--bitcoin-url` / `--eth-rpc` overrides. Stop it with Ctrl-C;
the environment keeps running.

```bash
# terminal 3 — attach
./target/debug/dbconsole \
    --datadir functional-tests/_dd/16-22-lwndm/el_ol/ee_sequencer
```

A single session sees new writes without reattaching — each query opens its own
short read transaction — so you can leave it open and re-run `.tables` to watch
the store fill.

To shut the environment down, stop `run_tests.sh`; if services outlive it, kill
`entry.py` first (it supervises) and then any remaining `bitcoind` / `strata` /
`alpen-client`. The datadir survives, and the console reads it offline exactly
as it did live.

## Troubleshooting

**`no EE environment under …/mdbx`** — `--datadir` points at the wrong place.
It must contain `mdbx/`, e.g. `.../el_ol/ee_sequencer`.

**`… lives in the `prover` environment, which is not present`** — the datadir
has no such environment. A full node never creates the sequencer-only
`prover`, `witness` and `da` directories.

**`… exists in more than one environment`** — name the table as `env/Table`.

**All tables report 0 rows** — nothing is driving the environment. See above.

**`failed to decode value for table …`** — the store was written by a binary
whose on-disk format differs from the one you built. Rebuild `alpen-ee` from the
branch that produced the datadir, or regenerate the datadir.

**`no reflector configured for this table's value type`** — that table is
registered for inventory but has no reflector yet.

**`another process has this environment open`** — a node, or another console, is
holding it. Writes need the node stopped; the attach holds every present
environment exclusively, so a node with any of them open blocks it.

**A predicate errors mid-scan** — probably a comparison against a field that
crossed as a blob (an integer past `i64::MAX`). Compare the blob, or filter on
something else.

**A predicate never matches** — either you forgot `r.value.` (a predicate sees
the whole record, not just its fields), or the field is a data-carrying enum
variant, which nests rather than comparing equal to a string. Print one record
with `first(…)` and look at the real shape.
