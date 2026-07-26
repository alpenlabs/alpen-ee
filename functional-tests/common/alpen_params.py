"""
Composition of the consolidated Alpen params artifact (``--alpen-params``).

The alpen-client consumes a single JSON document carrying the EE account
identity, bridge economics, DA stream identity, the spec schedule, and the
embedded EVM chain spec. Tests compose it from the ``strata-datatool``
generated EE params (account id — shared with OL params generation so both
layers agree) plus the in-repo chain spec JSON. Once the pinned datatool
grows ``gen-alpen-params``, this composition moves there.
"""

import json
from pathlib import Path

# Repo-relative chain spec documents (the same files the old --custom-chain
# flag resolved by name). Named in-repo chains only: the old flag also
# accepted arbitrary chain-spec paths, but a bare chain spec is no longer a
# valid node input — custom setups must compose a full params artifact.
_CHAINSPEC_DIR = (
    Path(__file__).resolve().parents[2] / "crates" / "reth" / "chainspec" / "src" / "res"
)
CHAIN_SPEC_FILES = {
    "dev": _CHAINSPEC_DIR / "alpen-dev-chain.json",
    "devnet": _CHAINSPEC_DIR / "devnet-chain.json",
    "testnet": _CHAINSPEC_DIR / "testnet-chain.json",
    "testnet3": _CHAINSPEC_DIR / "testnet3-chain.json",
}


def compose_alpen_params(
    datadir: Path,
    ee_params_path: Path,
    chain: str = "dev",
    bridge_denomination: int = 100_000_000,
    max_withdrawal_amount: int | None = 1_000_000_000,
    max_withdrawal_descriptor_len: int = 81,
    da_magic_bytes: str = "ALPN",
) -> Path:
    """Writes ``alpen-params.json`` into ``datadir`` and returns its path.

    Args:
        ee_params_path: datatool-generated EE params (source of account_id).
        chain: named chain spec whose genesis document becomes ``evm_spec``.
        max_withdrawal_amount: withdrawal cap in sats; ``None`` disables the
            cap. The old CLI sentinel ``0`` is rejected: ``BridgeParams``
            requires a set cap to be a positive multiple of the denomination,
            so ``0`` would fail node startup far from the mistake.
    """
    if max_withdrawal_amount == 0:
        raise ValueError("max_withdrawal_amount=0 is not a valid cap; pass None to disable it")

    ee_params = json.loads(Path(ee_params_path).read_text())
    evm_spec = json.loads(CHAIN_SPEC_FILES[chain].read_text())

    params = {
        "account_id": ee_params["account_id"],
        "bridge_params": {
            "denomination": bridge_denomination,
            "max_withdrawal_amount": max_withdrawal_amount,
            "max_withdrawal_descriptor_len": max_withdrawal_descriptor_len,
        },
        "blob_spec": {"magic_bytes": da_magic_bytes},
        "spec_schedule": {"v0": 0},
        "evm_spec": evm_spec,
    }

    out_path = Path(datadir) / "alpen-params.json"
    out_path.write_text(json.dumps(params, indent=2))
    return out_path
