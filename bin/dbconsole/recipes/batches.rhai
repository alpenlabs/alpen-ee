// Batch and chunk recipes.

/// Batches and chunks counted by status.
fn batch_summary() {
    #{
        batches: count_by_status("BatchByIdxSchema", "status"),
        chunks: count_by_status("ChunkByIdxSchema", "status"),
    }
}

/// Stages the removal of every batch with index `idx` or above, its id and
/// chunk-list entries, and every chunk belonging to those batches; then,
/// where the prover environment is present, the prover tasks and receipts of
/// those batches and chunks, which would otherwise be retried against a range
/// that no longer exists. Returns the count staged.
fn revert_batches_from(idx) {
    let n = 0;
    let top = last("BatchByIdxSchema");
    if top == () || parse_int(top.key) < idx { return 0; }
    let batch_ids = [];
    let batches = scan_range("BatchByIdxSchema", idx.to_string(), top.key, |r| true);
    for row in batches {
        let batch = row.value.batch;
        let id = batch.prev_block + ":" + batch.last_block;
        del("BatchByIdxSchema", row.key);
        n += 1;
        n += del_if_present("BatchIdToIdxSchema", id);
        n += del_if_present("BatchChunksSchema", id);
        batch_ids.push(id);
    }
    // Chunk indexes rise with batch indexes, so the chunks of these batches
    // are a range from the first one: find it, then walk to the end.
    let first = scan_where("ChunkByIdxSchema", |r| r.value.chunk.batch_idx >= idx, 1);
    let chunks = if first.len() == 0 {
        []
    } else {
        scan_range("ChunkByIdxSchema", first[0].key, last("ChunkByIdxSchema").key, |r| true)
    };
    let chunk_ids = [];
    for row in chunks {
        let chunk = row.value.chunk;
        let id = chunk.prev_block + ":" + chunk.last_block;
        del("ChunkByIdxSchema", row.key);
        n += 1;
        n += del_if_present("ChunkIdToIdxSchema", id);
        chunk_ids.push(id);
    }

    // Staged after the node edits so those land first. Prover tasks are
    // derived from chunks and batches, not the other way round.
    if has_env("prover") {
        n += prover_delete_ranges("ChunkProverTaskSchema", chunk_ids);
        n += prover_delete_ranges("AcctProverTaskSchema", batch_ids);
    }
    n
}
