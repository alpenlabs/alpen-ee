// Chain recipes: where the execution chain stands, and cutting it back.

/// Where the chain stands: tip and finalized heights, block, batch and
/// chunk counts, and the latest batch and chunk.
fn chain_summary() {
    let tip = last("ExecBlocksAtHeightSchema");
    let fin = last("ExecBlockFinalizedSchema");
    let batch = last("BatchByIdxSchema");
    let chunk = last("ChunkByIdxSchema");
    #{
        tip_height: if tip == () { () } else { parse_int(tip.key) },
        blocks_at_tip: if tip == () { 0 } else { tip.value.len() },
        finalized_height: if fin == () { () } else { parse_int(fin.key) },
        finalized_hash: if fin == () { () } else { fin.value },
        exec_blocks: count("ExecBlockSchema"),
        batches: count("BatchByIdxSchema"),
        last_batch: if batch == () { () } else { batch.value.batch },
        chunks: count("ChunkByIdxSchema"),
        last_chunk: if chunk == () { () } else { chunk.value.chunk },
    }
}

/// Stages the removal of every exec block above `height`: the block, its
/// payload, accessed state and witness, the height index and finalized
/// entries, and the witness environment's state diff for those heights; the
/// OL epoch entries whose accepted account state points at a dropped block,
/// so the OL tracker resumes from the last epoch whose block survives; then
/// reverts every batch whose last block is above `height`, with its chunks
/// and prover work. The node chain, the witness environment and the batches
/// are each cut back by their own top, so a witness that ran ahead of the
/// exec tip is trimmed even when the chain itself is already at `height`. A
/// full node has no witness environment, and that step is skipped. Node edits
/// are staged first, so they land first. Returns the count staged.
fn drop_chain_above(height) {
    let n = 0;
    let from = (height + 1).to_string();

    let tip = last("ExecBlocksAtHeightSchema");
    if tip != () && parse_int(tip.key) > height {
        let dropped = #{};
        let at_height = scan_range("ExecBlocksAtHeightSchema", from, tip.key, |r| true);
        for row in at_height {
            for hash in row.value {
                del("ExecBlockSchema", hash);
                dropped[hash] = true;
                n += 1;
                n += del_if_present("ExecBlockPayloadSchema", hash);
                n += del_if_present("BlockAccessedStateSchema", hash);
                n += del_if_present("BlockWitnessSchema", hash);
            }
            del("ExecBlocksAtHeightSchema", row.key);
            n += 1;
        }
        let finalized = keys_range("ExecBlockFinalizedSchema", from, tip.key);
        if finalized.len() > 0 {
            n += del("ExecBlockFinalizedSchema", finalized);
        }

        // An OL epoch whose accepted account state names a dropped block
        // would come back on restart as the best account state, pointing at
        // a block that no longer exists. Those epochs go with their blocks.
        for epoch in scan_where("OLBlockAtEpochSchema", |r| true) {
            let state = get("AccountStateAtOLEpochSchema", epoch.value);
            if state != () && state.value.account_state.last_exec_blkid in dropped {
                del("AccountStateAtOLEpochSchema", epoch.value);
                del("OLBlockAtEpochSchema", epoch.key);
                n += 2;
            }
        }
    }

    // The witness environment numbers blocks on its own and may run ahead of
    // the exec tip, so it is cut back to `height` by its own top, not the
    // chain's, and whether or not the chain needed cutting.
    let witness_top = if has_env("witness") { last("BlockHashByNumber") } else { () };
    if witness_top != () && parse_int(witness_top.key) > height {
        for number in keys_range("BlockHashByNumber", from, witness_top.key) {
            let hash = get("BlockHashByNumber", number).value;
            n += del_if_present("BlockStateChangesSchema", hash);
            del("BlockHashByNumber", number);
            n += 1;
        }
    }

    let stale = scan_where("BatchByIdxSchema", |r| r.value.batch.last_blocknum > height, 1);
    if stale.len() > 0 {
        n += revert_batches_from(parse_int(stale[0].key));
    }
    n
}

/// Stages what `drop_chain_above` does for the height of block `hash`, so
/// that block becomes the tip. Takes the hash as OL reports it, `0x` or not.
/// Refuses a block the store does not hold, a block at a finalized height
/// that is not the finalized one, and a block that shares an unfinalized
/// height with another, since the node could then pick either as its tip.
/// Returns the count staged.
fn drop_chain_above_block(hash) {
    let block = get("ExecBlockSchema", hash);
    if block == () { throw `no exec block ${hash}`; }
    let height = block.value.blocknum;

    let finalized = get("ExecBlockFinalizedSchema", height.to_string());
    if finalized != () {
        if finalized.value != block.key {
            throw `block ${block.key} is not on the finalized chain; height ${height} holds ${finalized.value}`;
        }
    } else {
        let others = get("ExecBlocksAtHeightSchema", height.to_string()).value.filter(|h| h != block.key);
        if !others.is_empty() {
            throw `block ${block.key} shares height ${height} with ${others}; the node could pick either as its tip`;
        }
    }
    drop_chain_above(height)
}
