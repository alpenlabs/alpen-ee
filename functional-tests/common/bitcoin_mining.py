"""Small, bounded Bitcoin regtest mining requests for test setup."""

BLOCKS_PER_RPC = 10


def generate_blocks_in_chunks(btc_rpc, block_count: int, mining_address: str) -> None:
    """Mine exactly ``block_count`` blocks without one long-running RPC call.

    RPC failures propagate: a timed-out request may already have mined blocks,
    so blindly retrying it could advance the chain further than intended.
    """
    if isinstance(block_count, bool) or not isinstance(block_count, int):
        raise TypeError("block_count must be an integer")
    if block_count < 0:
        raise ValueError("block_count must be non-negative")

    remaining = block_count
    while remaining > 0:
        chunk = min(remaining, BLOCKS_PER_RPC)
        btc_rpc.proxy.generatetoaddress(chunk, mining_address)
        remaining -= chunk
