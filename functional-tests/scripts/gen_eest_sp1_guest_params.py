"""Generate the zero-floor Alpen params artifact baked into EEST guest programs."""

from common.alpen_params import EEST_BASE_FEE_FLOOR, compose_alpen_params
from common.datatool import generate_ee_params
from common.prover_backend import ALPEN_PARAMS, GUEST_PARAMS_DIR


def main() -> None:
    GUEST_PARAMS_DIR.mkdir(parents=True, exist_ok=True)
    compose_alpen_params(
        GUEST_PARAMS_DIR,
        generate_ee_params(GUEST_PARAMS_DIR),
        base_fee_floor=EEST_BASE_FEE_FLOOR,
    )
    print(ALPEN_PARAMS)


if __name__ == "__main__":
    main()
