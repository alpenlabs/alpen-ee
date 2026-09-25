// Batch and chunk recipes.

/// Batches and chunks counted by status.
fn batch_summary() {
    #{
        batches: count_by_status("BatchByIdxSchema", "status"),
        chunks: count_by_status("ChunkByIdxSchema", "status"),
    }
}

/// Stages the removal of every batch with index `idx` or above, its id and
/// chunk-list entries, and every chunk belonging to those batches. Returns
/// the count staged.
fn revert_batches_from(idx) {
    let n = 0;
    let top = last("BatchByIdxSchema");
    if top == () || parse_int(top.key) < idx { return 0; }
    let batches = scan_range("BatchByIdxSchema", idx.to_string(), top.key, |r| true);
    for row in batches {
        let batch = row.value.batch;
        let id = batch.prev_block + ":" + batch.last_block;
        del("BatchByIdxSchema", row.key);
        n += 1;
        n += del_if_present("BatchIdToIdxSchema", id);
        n += del_if_present("BatchChunksSchema", id);
    }
    // Chunk indexes rise with batch indexes, so the chunks of these batches
    // are a range from the first one: find it, then walk to the end.
    let first = scan_where("ChunkByIdxSchema", |r| r.value.chunk.batch_idx >= idx, 1);
    let chunks = if first.len() == 0 {
        []
    } else {
        scan_range("ChunkByIdxSchema", first[0].key, last("ChunkByIdxSchema").key, |r| true)
    };
    for row in chunks {
        let chunk = row.value.chunk;
        del("ChunkByIdxSchema", row.key);
        n += 1;
        n += del_if_present("ChunkIdToIdxSchema", chunk.prev_block + ":" + chunk.last_block);
    }
    n
}
