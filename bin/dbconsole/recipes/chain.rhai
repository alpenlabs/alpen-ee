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

/// Stages the removal of everything above `target`, which is a block hash or
/// a height.
///
/// A hash is the usual form: the EE tip OL has accepted, as OL reports it,
/// `0x` or not. `tip_height` checks the block can be left as the tip and
/// gives its height. A height is used as given, with no checks. It is for
/// when no hash is at hand, such as reth's head after the node was killed.
///
/// Above the height, it stages the removal of every exec block. That covers
/// the block, its payload, accessed state and witness, and its height and
/// finalized entries. It covers the witness environment's state diffs for
/// those heights, where that environment is present. It covers the OL epoch
/// entries whose accepted account state points at a dropped block, so the OL
/// tracker resumes from the last epoch whose block survives. Last, it
/// reverts the batches and chunks that end above the height, with their
/// prover work.
///
/// The chain, the witness environment, the batches and the chunks are each
/// cut back by their own top. So a witness that ran ahead of the exec tip is
/// trimmed even when the chain is already at the height, and so are chunks
/// of the open batch. Node edits are staged first, so they land first.
/// Returns the count staged.
fn drop_chain_above(target) {
    let kind = target.type_of();
    if kind != "string" && kind != "i64" {
        throw `drop_chain_above takes a block hash or a height, not ${kind}`;
    }
    let height = if kind == "string" { tip_height(target) } else { target };

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

    // A sealed batch that ends above `height` takes its chunks with it.
    // Without one, the open batch may still have chunks that end above it.
    let stale = scan_where("BatchByIdxSchema", |r| r.value.batch.last_blocknum > height, 1);
    if !stale.is_empty() {
        n += revert_batches_from(parse_int(stale[0].key));
    } else {
        let stale = scan_where("ChunkByIdxSchema", |r| r.value.chunk.last_blocknum > height, 1);
        if !stale.is_empty() {
            n += revert_chunks_from(parse_int(stale[0].key));
        }
    }
    n
}

/// The height of block `hash`, once it is checked to be a block that can be
/// left as the tip. Fails for a string that is not a 32-byte hash, such as a
/// height typed in quotes. Fails for a block the store does not hold or the
/// height index does not list. Fails for a block at a finalized height that
/// is not the finalized one. Fails for a block that shares an unfinalized
/// height with another, since the node could then pick either as its tip. It
/// does not check that the block connects to the chain below it. OL's
/// accepted tip always does.
fn tip_height(hash) {
    let hex = if hash.starts_with("0x") { hash.sub_string(2) } else { hash };
    if hex.len() != 64 {
        throw `"${hash}" is not a 32-byte block hash; pass a height as a number`;
    }
    let block = get("ExecBlockSchema", hash);
    if block == () { throw `no exec block ${hash}`; }
    let height = block.value.blocknum;

    let finalized = get("ExecBlockFinalizedSchema", height.to_string());
    if finalized != () {
        if finalized.value != block.key {
            throw `block ${block.key} is not on the finalized chain; height ${height} holds ${finalized.value}`;
        }
    } else {
        let at_height = get("ExecBlocksAtHeightSchema", height.to_string());
        if at_height == () || !(block.key in at_height.value) {
            throw `the height index does not list block ${block.key} at height ${height}`;
        }
        let others = at_height.value.filter(|h| h != block.key);
        if !others.is_empty() {
            throw `block ${block.key} shares height ${height} with ${others}; the node could pick either as its tip`;
        }
    }
    height
}

/// Stages the spec version that the next block built on `hash` runs under.
/// `version` is the version's number, 1 for v1.
///
/// This is for a key switch that sends the EE no message, such as the move
/// from v0 to v1 on the chain that 0.3.0 built. OL applies that switch at
/// once, so nothing on the chain moves the EE to the next version. Run it
/// once, with the same hash, after `drop_chain_above(hash)` is committed. It
/// checks committed rows, so it fails while the rollback is only staged.
///
/// The block builder reads the version from the tip's record. The batch
/// builder reads it from the record of the last sealed batch's last block.
/// So it fails unless `hash` is both. A block between the two would land in
/// a batch under the old version. It fails unless `version` is the one after
/// the version the record holds now, so a second run fails rather than
/// skipping a version. Returns the count staged.
fn set_next_spec_version(hash, version) {
    let kind = version.type_of();
    if kind != "i64" {
        throw `set_next_spec_version takes the version as a number, such as 1 for v1, not ${kind}`;
    }
    let height = tip_height(hash);
    let tip = last("ExecBlocksAtHeightSchema");
    if parse_int(tip.key) != height {
        throw `block at height ${height} is not the tip, which is at height ${tip.key}; commit drop_chain_above first`;
    }
    let block = get("ExecBlockSchema", hash);
    let batch = last("BatchByIdxSchema");
    if batch == () { throw `no sealed batch; this needs a sequencer's datadir`; }
    if batch.value.batch.last_block != block.key {
        throw `block ${block.key} does not end the last sealed batch ${batch.key}, which ends at ${batch.value.batch.last_block}`;
    }
    let current = block.value.next_spec_version;
    if version != current + 1 {
        throw `block ${block.key} sets the next version to ${current}; it can only move to ${current + 1}`;
    }
    set("ExecBlockSchema", block.key, "next_spec_version", version);
    1
}
