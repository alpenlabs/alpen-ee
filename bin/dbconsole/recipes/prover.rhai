// Prover task recipes. A task lives in the table of its kind,
// `ChunkProverTaskSchema` or `AcctProverTaskSchema`, keyed by the spec
// version whose prover owns it and the two hashes of the range it proves:
// `0000` for the V0 prover, then `prev`, then `last`, 66 bytes. The receipt
// of either kind is keyed by the same two hashes as a `prev:last` pair, so a
// task key names its receipt. Every recipe below takes the task table first.

/// Prover tasks counted by status, chunk tasks and account tasks apart.
fn prover_summary() {
    #{
        chunk: count_by_status("ChunkProverTaskSchema", "status"),
        acct: count_by_status("AcctProverTaskSchema", "status"),
    }
}

/// One prover task from `table` and the receipt it points to, if any.
fn prover_task(table, key) {
    #{ task: get(table, key), receipt: prover_receipt(table, key) }
}

/// The receipt a task key from `table` points to: a chunk receipt for a chunk
/// task, an account receipt for an account task, or () when `table` is not a
/// task table, the key is not a task key, or nothing is stored.
fn prover_receipt(table, key) {
    let receipts = prover_receipt_table(table);
    let pair = task_pair(key);
    if receipts == () || pair == () { return (); }
    get(receipts, pair)
}

/// The receipt table of a task table, or () for any other table.
fn prover_receipt_table(table) {
    if table.ends_with("ChunkProverTaskSchema") { return "ChunkProofReceiptSchema"; }
    if table.ends_with("AcctProverTaskSchema") { return "AcctProofReceiptSchema"; }
    ()
}

/// The `prev:last` pair a task key ends with, or () for a string too short
/// to be one.
fn task_pair(key) {
    let key = if key.starts_with("0x") { key.sub_string(2) } else { key };
    let n = key.len();
    if n < 128 { return (); }
    key.sub_string(n - 128, 64) + ":" + key.sub_string(n - 64)
}

/// Stages a task back to `Pending`, so the prover picks it up again.
fn prover_reset(table, key) {
    set(table, key, "status", "Pending");
    1
}

/// Stages a task as permanently failed with `reason`, so the prover stops
/// retrying it.
fn prover_abandon(table, key, reason) {
    set(table, key, "status", #{ PermanentFailure: #{ error: reason } });
    1
}

/// Stages the removal of a task and whatever receipt it points to, including
/// the proof-id index entries of an account receipt. Returns the count staged.
fn prover_delete(table, key) {
    let n = 0;
    del(table, key);
    n += 1;
    let receipt = prover_receipt(table, key);
    if receipt == () { return n; }

    let receipts = prover_receipt_table(table);
    del(receipts, receipt.key);
    n += 1;
    if receipts == "AcctProofReceiptSchema" {
        let pair = task_pair(key);
        let prev = pair.sub_string(0, 64);
        let last = pair.sub_string(65);
        let stale = scan_where("AcctProofIdIndexSchema", |r| r.value.prev_block == prev && r.value.last_block == last);
        for entry in stale {
            del("AcctProofIdIndexSchema", entry.key);
            n += 1;
        }
    }
    n
}
