"""alpen_getChunkProofCoverage lives on the admin endpoint, not the public RPC.

The method walks the node's chunk history and exists for tests and internal
scripts, so the public RPC does not know it and the admin endpoint serves it
only with a valid JWT.
"""

import logging

import flexitest
import requests

from common.base_test import AlpenClientTest
from common.config.constants import ServiceType
from common.rpc import RpcError

logger = logging.getLogger(__name__)

METHOD_NOT_FOUND = -32601


@flexitest.register
class TestChunkProofCoverageAdmin(AlpenClientTest):
    def __init__(self, ctx: flexitest.InitContext):
        ctx.set_env("alpen_ee")

    def main(self, ctx):
        sequencer = self.get_service(ServiceType.AlpenSequencer)

        # Not on the public RPC, under either namespace.
        public = sequencer.create_rpc()
        for method in ("alpen_getChunkProofCoverage", "alpenadmin_getChunkProofCoverage"):
            try:
                getattr(public, method)(1, 10)
            except RpcError as err:
                assert err.code == METHOD_NOT_FOUND, f"{method} on the public RPC: {err}"
            else:
                raise AssertionError(f"{method} is reachable on the public RPC")

        # The admin endpoint refuses it without a token...
        try:
            sequencer.create_admin_rpc(jwt_token="not-a-token").alpenadmin_getChunkProofCoverage(
                1, 10
            )
        except requests.HTTPError as err:
            assert err.response is not None and err.response.status_code == 401, err
        else:
            raise AssertionError("admin RPC served the method without a valid token")

        # ...and answers with one.
        response = sequencer.create_admin_rpc().alpenadmin_getChunkProofCoverage(1, 10)
        assert response["start_block"] == 1 and response["end_block"] == 10, response
        assert "covered" in response and "first_uncovered_block" in response, response
        logger.info("admin chunk proof coverage: %s", response)
        return True
