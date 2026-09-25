// Prover task recipes. A task key is a prefix, a kind tag, and the two
// hashes of the range it proves; a chunk's receipt is keyed by the tag and
// hashes, a batch's receipt by the hashes as a `prev:last` pair.

/// Prover tasks counted by status.
fn prover_summary() {
    count_by_status("ProverTaskSchema", "status")
}

/// One prover task and the receipt it points to, if any.
fn prover_task(key) {
    #{ task: get("ProverTaskSchema", key), receipt: prover_receipt(key) }
}

/// The receipt for a task key: a chunk receipt, an account receipt, or ()
/// when the key has neither shape or nothing is stored.
fn prover_receipt(key) {
    let n = key.len();
    if n < 130 { return (); }
    let tag = key.sub_string(n - 130, 2);
    let pair = key.sub_string(n - 128);
    if tag == "63" {
        return get("ChunkProofReceiptSchema", tag + pair);
    }
    if tag == "61" {
        return get("AcctProofReceiptSchema", pair.sub_string(0, 64) + ":" + pair.sub_string(64));
    }
    ()
}

/// Stages a task back to `Pending`, so the prover picks it up again.
fn prover_reset(key) {
    set("ProverTaskSchema", key, "status", "Pending");
    1
}

/// Stages a task as permanently failed with `reason`, so the prover stops
/// retrying it.
fn prover_abandon(key, reason) {
    set("ProverTaskSchema", key, "status", #{ PermanentFailure: #{ error: reason } });
    1
}

/// Stages the removal of a task and whatever receipt it points to, including
/// the proof-id index entries for an account receipt. Returns the count staged.
fn prover_delete(key) {
    let n = 0;
    del("ProverTaskSchema", key);
    n += 1;
    let receipt = prover_receipt(key);
    if receipt == () { return n; }

    let len = key.len();
    let tag = key.sub_string(len - 130, 2);
    let pair = key.sub_string(len - 128);
    if tag == "63" {
        del("ChunkProofReceiptSchema", receipt.key);
        n += 1;
    } else {
        del("AcctProofReceiptSchema", receipt.key);
        n += 1;
        let prev = pair.sub_string(0, 64);
        let last = pair.sub_string(64);
        let stale = scan_where("AcctProofIdIndexSchema", |r| r.value.prev_block == prev && r.value.last_block == last);
        for entry in stale {
            del("AcctProofIdIndexSchema", entry.key);
            n += 1;
        }
    }
    n
}
