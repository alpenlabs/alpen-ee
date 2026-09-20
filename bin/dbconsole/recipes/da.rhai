// DA pipeline recipes.

/// The L1 broadcast queue by status, and the replacement chains still
/// eligible for fee bumping.
fn broadcast_summary() {
    #{
        queued: count("L1BroadcastTxIdSchema"),
        by_status: count_by_status("L1BroadcastTxSchema", "status"),
        replacement_chains: count("L1BroadcastTxNodeSchema"),
        active_chains: count("L1BroadcastActiveTxNodeSchema"),
        chunked_envelopes: count("L1ChunkedEnvelopeSchema"),
    }
}
