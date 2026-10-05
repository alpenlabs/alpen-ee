"""
Composition of the consolidated Alpen params artifact (``--alpen-params``).

The alpen-client consumes a single JSON document carrying the EE account
identity, bridge economics, DA stream identity, the spec schedule, and the
embedded EVM chain spec. Tests compose it from the ``strata-datatool``
generated EE params (account id — shared with OL params generation so both
layers agree) plus the ``evm_spec`` of an in-repo ``params/<network>.json``.
Once the pinned datatool grows ``gen-alpen-params``, this composition moves
there.
"""

import json
from pathlib import Path

# Per-network params files, one ``<network>.json`` each.
PARAMS_DIR = Path(__file__).resolve().parents[2] / "params"

#: Base-fee floor of each spec version, in wei. A version with no entry keeps
#: its predecessor's floor. Matches the deployed networks: plain EIP-1559 under
#: v0, and the 1 gwei floor from v1 on.
DEFAULT_BASE_FEE_FLOOR = {"v0": 0, "v1": 1_000_000_000}
#: EEST runs the standard Ethereum recurrence under every version.
EEST_BASE_FEE_FLOOR = {"v0": 0}


#: Spec schedule a chain launched from current source runs: every known
#: version active from genesis (coordinate 0). A test rehearsing an upgrade
#: launches further back instead, leaving the version it upgrades to
#: unscheduled.
LAUNCH_SPEC_SCHEDULE = {"v0": 0, "v1": 0}


def compose_alpen_params(
    datadir: Path,
    ee_params_path: Path,
    chain: str = "dev",
    bridge_denomination: int = 100_000_000,
    max_withdrawal_amount: int | None = 1_000_000_000,
    max_withdrawal_descriptor_len: int = 81,
    da_magic_bytes: str = "ALPN",
    spec_schedule: dict[str, int] | None = None,
    base_fee_floor: dict[str, int] = DEFAULT_BASE_FEE_FLOOR,
) -> Path:
    """Writes ``alpen-params.json`` into ``datadir`` and returns its path.

    Args:
        ee_params_path: datatool-generated EE params (source of account_id).
        chain: network in ``params/`` whose ``evm_spec`` is used.
        max_withdrawal_amount: withdrawal cap in sats; ``None`` disables the
            cap. The old CLI sentinel ``0`` is rejected: ``BridgeParams``
            requires a set cap to be a positive multiple of the denomination,
            so ``0`` would fail node startup far from the mistake.
        spec_schedule: spec version -> activation coordinate. Everything
            scheduled at 0 is active at genesis, so this decides which version
            the chain launches on. Defaults to `LAUNCH_SPEC_SCHEDULE`.
            Comes from the prover
            backend, which owns the version-to-program mapping the chain has
            to agree with -- see common/prover_backend.py.
        base_fee_floor: spec version -> minimum EIP-1559 base fee in wei. A
            version with no entry keeps its predecessor's floor, so v0 must be
            set. Defaults to the deployed networks' floors. EEST sets every
            version to zero to exercise the standard Ethereum recurrence.
    """
    if max_withdrawal_amount == 0:
        raise ValueError("max_withdrawal_amount=0 is not a valid cap; pass None to disable it")
    if "v0" not in base_fee_floor:
        raise ValueError("base_fee_floor must set v0; later versions inherit its floor")
    for version, floor in base_fee_floor.items():
        if isinstance(floor, bool) or not isinstance(floor, int):
            raise TypeError(f"base_fee_floor[{version!r}] must be an integer")
        if not 0 <= floor <= 2**64 - 1:
            raise ValueError(f"base_fee_floor[{version!r}] must be between 0 and 2**64 - 1")

    ee_params = json.loads(Path(ee_params_path).read_text())
    evm_spec = json.loads((PARAMS_DIR / f"{chain}.json").read_text())["evm_spec"]

    params = {
        "strata_exec_account_id": ee_params["account_id"],
        "bridge_params": {
            "denomination": bridge_denomination,
            "max_withdrawal_amount": max_withdrawal_amount,
            "max_withdrawal_descriptor_len": max_withdrawal_descriptor_len,
        },
        "blob_spec": {"magic_bytes": da_magic_bytes},
        "spec_schedule": LAUNCH_SPEC_SCHEDULE if spec_schedule is None else spec_schedule,
        "evm_spec": evm_spec,
        "fee_spec": {"base_fee_floor": base_fee_floor},
    }

    out_path = Path(datadir) / "alpen-params.json"
    out_path.write_text(json.dumps(params, indent=2))
    return out_path
