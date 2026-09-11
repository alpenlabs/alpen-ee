# Trust model

This document lists which components and inputs of the Alpen EE are trusted, which are not,
and how that decides whether a finding counts as a security issue and how severe it is. This also serves as
the repository-specific context for automated security audits.

It has two audiences:

- People triaging security reports, whether internal, from an external audit, or from a
  bounty submission.
- Automated PR reviewers (see the "Security Review Guidance" section of `AGENTS.md` and
  `.github/workflows/ai-security-review.yml`). They must apply the
  [classification rules](#classification-rules) before reporting anything.

If a reviewer's judgement and this document disagree, follow the document. If the document
is wrong or incomplete, fix it in a dedicated PR rather than working around it.

## System context

The EE (`alpen-client`) is a custom Reth node plus the services that connect it to the
Orchestration Layer (OL, the `strata` node) and to Bitcoin. It runs the user-facing EVM
chain, credits deposits delivered by the OL, turns withdrawal requests into bridge-out
intents, seals blocks into chunks and batches, posts batch state diffs to Bitcoin as DA,
proves chunks and batches, and submits proven updates to the EE's snark account on the OL.
It runs in one of two modes:

| Mode | What it does | Extra external connections |
|------|--------------|----------------------------|
| Full node | Re-executes blocks announced via gossip and fetched over reth P2P, serves user JSON-RPC, optionally forwards txs to the sequencer. | OL RPC (read), sequencer HTTP (tx forwarding) |
| Sequencer | Builds blocks, seals batches and chunks, generates or requests proofs, posts DA to L1, submits updates to OL. | OL submit RPC (authenticated), bitcoind RPC, prover backend |

Main components, for locating the code a finding touches:

| Component | Description | Code location |
|-----------|-------------|---------------|
| `alpen-client` | Node binary. Wires reth, the OL tracker and gossip, plus block building, batching, proving, DA and update submission in sequencer mode. | `bin/alpen-client` |
| Alpen Reth node | Reth with the Alpen chainspec, a custom EVM with the bridge-out precompile (`0x54…0001`) and Schnorr verification precompile (`0x54…0002`), the `alpen_gossip` RLPx subprotocol, `alpen_*` RPC methods and a state-diff exex. | `crates/reth/*` |
| OL tracker | Follows the paired OL node: epochs, checkpoints, finalization, inbox messages. | `crates/alpen-ee/ol-tracker`, `bin/alpen-client/src/ol` |
| Engine and exec chain | Drives the reth engine API and tracks the execution chain, including orphans. | `crates/alpen-ee/engine`, `crates/alpen-ee/exec-chain` |
| Block, chunk and batch assembly | Builds blocks, including deposit minting, and the chunk and batch packages the prover consumes. | `crates/alpen-ee/block-assembly`, `crates/alpen-ee/sequencer` |
| Prover | Chunk and account (batch) proofs via SP1 remote proving, or a signature-based native backend for tests. | `bin/alpen-client/src/sequencer/prover`, `crates/proof-impl/*`, `crates/ee-chunk-runtime`, `crates/ee-acct-runtime` |
| DA pipeline | Aggregates batch state diffs into blobs, posts them as SPS-51 chunked envelopes on Bitcoin, builds and verifies DA witnesses. | `crates/alpen-ee/da/{types,provider,runtime}` |
| Update submitter | Submits proven batches to the OL as snark account updates over the authenticated submit RPC. | `crates/alpen-ee/sequencer/src/update_submitter` |
| EE database | MDBX storage for EE account state, batches, chunks, proofs, DA envelopes and broadcast state. | `crates/alpen-ee/database`, `crates/db/*` |

The EE trusts its OL node the same way it trusts bitcoind: as an honest view of the layer
below. The recommended deployment runs the OL and EE clients paired, under one operator.
The sequencer is a single known party, and its public key is in every full node's config
(`sequencer_pubkey`).

## Funds and the chain of trust

Keeping funds safe is the most important security requirement. The trust chain:

1. An EE Client trusts its paired OL client.
2. An OL client trusts the SNARK proof produced by the OL sequencer, which guarantees the OL
   STF ran honestly.
3. The EE submits state updates together with a  SNARK
   (account) proof, and the OL accepts an update only if the proof verifies. Withdrawal
   outputs therefore reach the OL only as part of a proven update.
4. The OL STF maintains the balance invariants of the EE's account at the OL level: the
   balance cannot go negative, funds cannot be created out of nothing, a deposit is credited
   only when the bridge backs it, and a withdrawal is debited only against an output carried
   by a proven EE update.

Because of (4), the EE does not re-check the OL's accounting. Because of (3), the EE-side
proof runtime is the last line of defense for the invariants below: a bug there lets the EE
prove something false, and the OL will accept it.

What the EE is responsible for, and what stays in scope, is applying that accounting
faithfully inside the EVM. These are the invariants this repository upholds; a violation is a
security finding whichever boundary the triggering input crossed:

1. **Deposits are credited to the right recipient, for the right amount, exactly once, and
   in order.** Minting happens in block assembly (`alpen-ee/block-assembly`). The chunk and
   account proof runtimes enforce the ordering by matching each chunk's `subject_deposits`
   against the pending inputs (`ee-chunk-runtime`, `ee-acct-runtime`).
2. **A withdrawal burns native coin and creates a bridge-out intent of equal amount.** An
   intent must never exist without its burn. Enforced by the bridge-out precompile and the
   log extraction that collects intents (`reth/evm`).
3. **The native coin supply inside the EE never diverges from what the OL has credited
   minus what the EE has emitted as withdrawals.** Any EE-side path that mints without a
   matching OL deposit, credits a deposit twice, drops a deposit, or emits a withdrawal
   intent without burning the same amount violates this.
4. **The proof runtime accepts only valid state transitions.** Chunk proofs are verified with
   the correct verifying key when consumed, the DA witness is tied to the update it backs,
   and no witness or input check can be bypassed (`ee-chunk-runtime`, `ee-acct-runtime`,
   `proof-impl/*`, `alpen-ee/da/runtime`).

## Trust levels

| Level | Meaning | Consequence for review |
|-------|---------|------------------------|
| Trusted | Controlled by the node operator, or by the same entity that runs this node. If it is compromised, the node is compromised. | Malformed or inconsistent data from it points to a bug on one side of the interface, usually protocol drift or a version mismatch. Panics and missing validation on this data are worth reporting as bugs, but they are not security findings unless they can affect funds or soundness. |
| Authenticated | A known counterparty whose messages are cryptographically bound to an identity we already trust for a specific role. | The authentication check itself is in scope. Once a message is authenticated, its payload is trusted only for the role that party holds (see row notes) and everything else still has to be validated. |
| Untrusted | Anyone on the internet, or anyone who can get data onto Bitcoin. | The node has to validate everything. Panics, unbounded allocation, hangs, and incorrect acceptance on this data are in scope. |

## Trust boundaries

These tables are the attack surfaces of the node: local (operator-controlled), internal
(paired services and authenticated counterparties) and internet-facing (untrusted). Each row
names who controls the input and what a reviewer should expect from the code handling it.

### Local / operator-controlled (Trusted)

| Component / input | Controlled by | Notes |
|-------------------|---------------|-------|
| Node databases: reth MDBX/static files, EE MDBX databases (`alpen-ee/database`) | Operator | Contents are assumed consistent with what the node itself wrote. DB corruption or tampering is out of scope. |
| Config file (TOML), CLI args, env vars (`STRATA_SUBMIT_RPC_TOKEN`, bitcoind credentials) | Operator | Bad config should fail fast with a clear error, but it is not an attack surface. Secret handling (logging a token, world-readable files) is in scope. |
| Params file (`--alpen-params`), genesis data | Operator | Assumed to match the network the node joins. |
| Key files: sequencer gossip privkey, native-prover signing keys | Operator | The files themselves are trusted. Ways a key could leak (logs, error messages, RPC responses) are in scope. |
| Prover guest ELFs (`chunk_elf_path`, `acct_elf_path`) | Operator | Assumed to be the audited build. The ELF supply chain belongs to the release process, so it is out of scope for code review. |
| Filesystem paths derived from config | Operator | Path traversal via config values is out of scope. |
| Reth engine API and internal channels between the embedded reth node and EE services | Same process | Everything here runs in one process, so there is no boundary to cross. |
| OTLP / metrics / health-check endpoints (`health_check_host:port`) | Operator network | Internal only, bound to an operator-controlled interface. Information disclosure over these is low severity unless secrets appear. |

### Paired services (Trusted)

| Component / input | Controlled by | Notes |
|-------------------|---------------|-------|
| OL node RPC (`ol.client_url`): epochs, checkpoints, finalization status, inbox messages | Operator (paired with the EE) | Responses are trusted as an honest view of OL state, with the same assumptions and risks as bitcoind. A malicious OL node is out of scope. OL data originates from L1, which is untrusted, but the OL node is trusted to have validated it. An operator who points the EE at an OL node they do not run takes on that risk themselves. |
| OL submit RPC (`sequencer.ol_submit_url`, bearer token from `STRATA_SUBMIT_RPC_TOKEN`) | Operator (paired with the EE) | Trusted endpoint. Handling of the bearer token (not logged, not in the config file) is in scope. |
| bitcoind RPC (`sequencer.bitcoind`) | Operator | Trusted as an honest view of the chain. A malicious bitcoind is out of scope. The chain data it returns is still untrusted content (see below), since blocks may contain arbitrary payloads. |
| Cross-layer messages delivered by the OL that originate from users: deposits, withdrawals, inbox messages | Operator (paired with the EE) | Their content is user-controlled at the source, but the OL STF has already applied its balance invariants (see [Funds and the chain of trust](#funds-and-the-chain-of-trust)), so the EE accepts them without re-validating. A panic on malformed message content usually means the OL and EE disagree on the message format; report it as a bug, not as a security finding, unless it can affect fund safety. How the EE applies a message (crediting the deposit, matching it in the proof runtime) is in scope. |
| Remote proving service (SP1 network) | Third party | Trusted for liveness only. A malicious prover can withhold or delay but cannot forge: chunk proofs are verified when they are consumed to build batch and account proofs, and account proofs are verified by the OL when the update is submitted. Proofs are not yet verified on receipt (see [Known accepted risks](#known-accepted-risks)). Proof verification code is in scope. |

### Authenticated counterparties

| Component / input | Controlled by | Notes |
|-------------------|---------------|-------|
| Sequencer-signed block gossip (`alpen_gossip`, validated against `sequencer_pubkey`) | The sequencer | Signature verification is in scope. After verification, the sequencer is trusted for ordering and liveness but not for validity: full nodes re-execute every block, so a signed block that fails execution has to be rejected. Anything that lets such a block become canonical is in scope. |
| Sequencer HTTP endpoint used for tx forwarding (`sequencer_http_url`) | The sequencer | Full node to sequencer direction only. The full node reveals nothing beyond the raw tx it would gossip anyway. |

### Untrusted

| Component / input | Controlled by | Notes |
|-------------------|---------------|-------|
| Public JSON-RPC (`eth_*`, `alpen_*`, `net_*`, ...) on a full node, and any sequencer RPC method reachable from outside the operator | Anyone | All parameters are attacker-controlled. DoS via expensive queries, panics on malformed input, and information leaks are in scope. Tx forwarding from third-party full nodes means the sequencer's `eth_sendRawTransaction` is always reachable from outside, so it falls under this row regardless of how the rest of the sequencer RPC is firewalled. |
| User transactions (RPC submission, forwarded, or P2P txpool gossip) | Anyone | Standard EVM threat model: gas accounting, precompile input handling, mempool resource limits. Custom Alpen precompiles (`reth/evm`) get extra scrutiny. |
| Reth P2P peers (devp2p, txpool gossip, block/header requests) | Anyone | Standard reth peer threat model. `alpen_gossip` messages are untrusted until the sequencer signature check passes. |
| Bitcoin block contents: SPS-50 tagged txs, SPS-51 envelopes, DA payloads, inscriptions | Anyone who pays L1 fees | Arbitrary bytes. Envelope and blob decoders, including the DA witness verification in `alpen-ee/da/runtime` that runs inside the account proof, must not panic, allocate without bound, or accept a payload that does not match the update it claims to back. Node-side code should skip a malformed payload and keep running. |
| L1 reorgs | Bitcoin network | Reorgs up to `l1_reorg_safe_depth` are expected and the node has to handle them without ending up in an inconsistent state. |
| Proof inputs and witnesses as data (block witnesses, state diffs, chunk inputs, DA witnesses) | Derived from untrusted chain data | Code inside the guest (`ee-chunk-runtime`, `ee-acct-runtime`, `proof-impl/*`, `alpen-ee/da/runtime`) processes attacker-influenced data. Soundness bugs, meaning acceptance of an invalid state transition, are the highest-severity class in this repo. |

## Classification rules

Apply these in order and stop at the first rule that decides the case. They refine the
general audit rules for this repository; the general rules on validity, exploitability and
the caps for recoverable funds, availability-only findings and substantial prerequisites
still apply on top.

1. Identify the attacker. Name the boundary in the tables above that the malicious input
   crosses. If you cannot name one, it is not a security finding.
2. Data from a Trusted source cannot produce a security finding on its own. Panics,
   `unwrap()`s, missing bounds checks, or inconsistent handling of data from a Trusted row
   usually indicate protocol drift between components. They are bugs worth reporting as
   bugs, and a security review may leave them out entirely, unless rule 6 (fund safety) applies.
3. Secrets are always in scope, wherever they sit. Logging, echoing over RPC, or persisting a
   private key, RPC token, or bitcoind password in plaintext where it did not previously exist
   is reportable even if every party involved is Trusted.
4. For an Authenticated source, the scope is the party's role. The authentication check is in
   scope. Past the check, only claims outside the role are in scope (for example, the sequencer
   asserting that a block is valid).
5. For an Untrusted source, the standard rules apply. Panics, unbounded memory or CPU use,
   hangs, incorrect acceptance, and information disclosure are all in scope.
6. Fund safety and soundness override the other rules. A path that violates one of the
   [invariants](#funds-and-the-chain-of-trust) above (mint without deposit, double credit,
   dropped deposit, withdrawal intent without an equal burn, or acceptance of an invalid
   state transition, invalid DA payload, or forged proof) has Severe impact no matter which
   boundary the input crossed. When an ordinary user or an L1 payload can trigger it, the
   finding is Critical.
7. Sequencer availability counts as a security issue only when the trigger is Untrusted input.
   A sequencer that halts on its own bug is a correctness or operations issue.

### Severity calibration

Default impact for patterns that come up in this repository. These take precedence over a
reviewer's own estimate; when in doubt between two adjacent tiers, prefer the lower one
unless the higher impact can be concretely demonstrated.

| Pattern | Default impact |
|---------|----------------|
| Native coin minted without a matching OL deposit, or a deposit credited twice or dropped | Severe |
| Bridge-out intent without an equal burn, or a burn without its intent | Severe |
| Proof runtime or DA witness verification accepts an invalid state transition, invalid DA payload, or forged proof | Severe |
| A signed block that fails re-execution becomes canonical on full nodes | Severe |
| Sequencer gossip or native-prover private key exposed (logs, RPC, disk) | Severe |
| `STRATA_SUBMIT_RPC_TOKEN` or bitcoind credentials exposed (logs, RPC, disk) | Major |
| Sequencer halts, panics, or stops sealing batches on Untrusted input (crafted tx, RPC call, P2P message, L1 payload) | Major |
| Batch, DA, or update submission pipeline stalls on Untrusted input, delaying withdrawals | Major |
| A full node panics or hangs on public RPC or P2P input, recoverable by restart | Moderate |
| Precompile gas mis-charging without unbounded work | Minor |
| Information disclosure over the health-check or metrics endpoints without secrets | Minor |
| Panic, unbounded allocation or hang on Trusted-source data with no invariant impact | Minor; not a security finding, file as a bug |
| Sequencer halting on its own bug, without Untrusted input | Informational |

### In scope (examples)

- Panic in an SPS-51 envelope or DA payload decoder on a crafted Bitcoin transaction.
- `eth_call` or `alpen_getChunkProofCoverage` with parameters that trigger unbounded work.
- A gossiped block that fails re-execution but is still marked canonical.
- A precompile that mis-charges gas or panics on specific calldata.
- The bridge-out precompile creating an intent for more than it burned, or burning without
  creating the intent.
- A deposit credited twice, or skipped, when a chunk or batch boundary falls between the
  OL delivering it and the EE applying it.
- A chunk proof accepted with the wrong verifying key, or a witness check that can be
  bypassed.
- Bearer token or bitcoind password written to logs at any level.
- CI configuration under `.github/workflows`.

### Out of scope (examples)

- Test code and documentation.
- Panic when the EE database returns unexpected bytes.
- Decoding panic on an OL inbox message with an unexpected shape. That is protocol drift;
  file it as a bug.
- Path traversal through a path in the TOML config.
- OL RPC returning inconsistent epochs and stalling the tracker.
- A malicious bitcoind returning fake block headers.
- The OL over-crediting or under-crediting the EE account. That is the OL STF's invariant,
  covered by the OL proof, and belongs in the OL repository's threat model.
- Missing rate limits on the health-check endpoint.
- Issues that require the operator to run with test-only settings
  (`ol.source = "dummy"`, `ol.epoch_tracking_mode = "latest"`,
  `sequencer.prover.backend = "native"`).
- Anything listed under [Known accepted risks](#known-accepted-risks).

## Known accepted risks

Deliberate decisions that a reviewer may flag but that should not be reported again. Keep
each entry to one line plus a reason or tracking ticket.

- Plaintext `rpc_user` / `rpc_password` in the `[sequencer.bitcoind]` TOML section. Tracked
  in STR-4177; the config file is operator-controlled.
- Tx forwarding sends bytes to the sequencer after checking only encoding and signature,
  before full local pool validation. Intentional; the sequencer re-validates. See
  `FullNodeConfig::sequencer_http_url`.
- Proofs from the remote prover are not verified on receipt; verification is deferred to
  batch/account proof construction (chunk proofs) and OL update submission (account proofs).
  Known gap, verifying on receipt is the intended end state.

## Maintenance

- Changes to this file change what automated reviewers will suppress. Treat them as
  security-relevant and keep the file under CODEOWNERS.
- When a row's trust level changes (for example, third parties start running full nodes
  against a hosted OL RPC), update the table and the summary in `AGENTS.md` in the same PR.
- When the general audit context changes its methodology or vocabulary, re-check the
  classification rules and severity calibration here for consistency.
