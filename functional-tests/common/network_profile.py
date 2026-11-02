"""Optional network profile for stress runs on a deployed network's params.

``ALPEN_FT_NETWORK_PROFILE`` names a JSON file whose keys override the params
composition defaults, so the guest-baked params (``scripts.gen_sp1_guest_params``)
and the live node's ``--alpen-params`` stay byte-for-byte identical. Keys:
``chain`` (``params/<chain>.json``), ``bridge_denomination``,
``max_withdrawal_amount``, ``base_fee_floor``, ``spec_schedule``, ``genesis_ol_balance_sats``
(the EE account's OL balance at genesis, matching the EE genesis allocation).
"""

import json
import os
from pathlib import Path

ENV = "ALPEN_FT_NETWORK_PROFILE"
KEYS = {"chain", "bridge_denomination", "max_withdrawal_amount", "base_fee_floor", "spec_schedule",
        "genesis_ol_balance_sats"}


def load() -> dict:
    """Returns the profile's overrides, or ``{}`` when no profile is set."""
    path = os.environ.get(ENV)
    if not path:
        return {}
    profile = json.loads(Path(path).read_text())
    unknown = set(profile) - KEYS
    if unknown:
        raise ValueError(f"{path}: unknown network profile keys {sorted(unknown)}")
    return profile
