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
/// to be one. The pair is lowercase, the way keys render, so it can be
/// matched against a scan's keys.
fn task_pair(key) {
    let key = key.to_lower();
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
    del(table, key);
    1 + prover_delete_receipts(table, [task_pair(key)])
}

/// Stages the removal of every task in `table` over one of the `prev:last`
/// ranges in `pairs`, under any spec version. Also stages the removal of the
/// receipts of those ranges, which can outlive their tasks. Returns the count
/// staged.
fn prover_delete_ranges(table, pairs) {
    if pairs.is_empty() { return 0; }
    let wanted = #{};
    for pair in pairs { wanted[pair] = true; }
    let tasks = keys_where(table, |key| task_pair(key) in wanted);
    let n = if tasks.is_empty() { 0 } else { del(table, tasks) };
    n + prover_delete_receipts(table, pairs)
}

/// Stages the removal of the receipts behind `table`'s tasks for each
/// `prev:last` pair in `pairs`, with the proof-id index entries of account
/// receipts. It matches receipts by key, so no proof is decoded. Returns the
/// count staged.
fn prover_delete_receipts(table, pairs) {
    let receipts = prover_receipt_table(table);
    let wanted = #{};
    for pair in pairs {
        if pair != () { wanted[pair] = true; }
    }
    if receipts == () || wanted.is_empty() { return 0; }

    let stale = keys_where(receipts, |key| key in wanted);
    let n = if stale.is_empty() { 0 } else { del(receipts, stale) };
    if receipts == "AcctProofReceiptSchema" {
        let index = scan_where("AcctProofIdIndexSchema", |r| `${r.value.prev_block}:${r.value.last_block}` in wanted)
            .map(|r| r.key);
        if !index.is_empty() { n += del("AcctProofIdIndexSchema", index); }
    }
    n
}
