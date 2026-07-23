"""
Composition of the consolidated Alpen params artifact (``--params``).

The alpen-client consumes a single JSON document carrying the EE account
identity, bridge economics, DA stream identity, spec activations, and the
embedded EVM chain spec. Tests compose it from the ``strata-datatool``
generated EE params (account id — shared with OL params generation so both
layers agree) plus the in-repo chain spec JSON.
"""

import json
from pathlib import Path

# Repo-relative chain spec documents (the same files the old --custom-chain
# flag resolved by name).
_CHAINSPEC_DIR = (
    Path(__file__).resolve().parents[2] / "crates" / "reth" / "chainspec" / "src" / "res"
)
CHAIN_SPEC_FILES = {
    "dev": _CHAINSPEC_DIR / "alpen-dev-chain.json",
    "devnet": _CHAINSPEC_DIR / "devnet-chain.json",
    "testnet": _CHAINSPEC_DIR / "testnet-chain.json",
    "testnet3": _CHAINSPEC_DIR / "testnet3-chain.json",
}

# The Alpen snark account's genesis predicate, matching
# `gen-ol-params --alpen-predicate bip340-schnorr-test` (the EE acct
# program's deterministic test predicate key).
DEFAULT_GENESIS_UPDATE_VK = (
    "Bip340Schnorr:4d4b6cd1361032ca9bd2aeb9d900aa4d45d9ead80ac9423374c451a7254d0766"
)


def compose_alpen_params(
    datadir: Path,
    ee_params_path: Path,
    chain: str = "dev",
    genesis_update_vk: str = DEFAULT_GENESIS_UPDATE_VK,
    bridge_denomination: int = 100_000_000,
    max_withdrawal_amount: int | None = 1_000_000_000,
    max_withdrawal_descriptor_len: int = 81,
    da_magic_bytes: str = "ALPN",
    pending_evm_forks: list[str] | None = None,
) -> Path:
    """Writes ``alpen-params.json`` into ``datadir`` and returns its path.

    Args:
        ee_params_path: datatool-generated EE params (source of account_id).
        chain: named chain spec whose genesis document becomes ``evm_spec``.
        genesis_update_vk: the account's predicate key at genesis, in the
            standard predicate string form.
        pending_evm_forks: stock EVM forks (e.g. ``["osaka"]``) the node
            activates at the next VK-update boundary.
    """
    ee_params = json.loads(Path(ee_params_path).read_text())
    evm_spec = json.loads(CHAIN_SPEC_FILES[chain].read_text())

    params = {
        "account_id": ee_params["account_id"],
        "genesis_update_vk": genesis_update_vk,
        "bridge_params": {
            "denomination": bridge_denomination,
            "max_withdrawal_amount": max_withdrawal_amount,
            "max_withdrawal_descriptor_len": max_withdrawal_descriptor_len,
        },
        "blob_spec": {"magic_bytes": da_magic_bytes},
        "spec_activations": {},
        "evm_spec": evm_spec,
    }
    if pending_evm_forks:
        params["pending_upgrade"] = {"evm_forks": pending_evm_forks}

    out_path = Path(datadir) / "alpen-params.json"
    out_path.write_text(json.dumps(params, indent=2))
    return out_path
