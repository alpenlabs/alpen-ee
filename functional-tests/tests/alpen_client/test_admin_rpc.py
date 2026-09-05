"""Test the JWT-authenticated alpenadmin RPC endpoint."""

import logging
import secrets
import time

import flexitest
import requests

from common.base_test import BaseTest
from common.config.constants import ServiceType
from common.rpc import JsonRpcClient, RpcError
from common.services.alpen_client import AlpenClientService, build_admin_jwt
from common.wait import wait_until
from envconfigs import AlpenClientEnv

logger = logging.getLogger(__name__)


def assert_unauthorized(rpc: JsonRpcClient) -> None:
    """Assert the admin call is rejected with HTTP 401 before the RPC layer."""
    try:
        rpc.alpenadmin_getAdminStatus()
    except requests.HTTPError as e:
        assert e.response is not None and e.response.status_code == 401, (
            f"expected HTTP 401, got {e.response.status_code if e.response else None}"
        )
    else:
        raise AssertionError("admin call without valid JWT should be rejected")


@flexitest.register
class TestAdminRpc(BaseTest):
    def __init__(self, ctx: flexitest.InitContext):
        # --sequencer requires the DA args, so the minimal sequencer env
        # still carries the L1 DA pipeline (bitcoin regtest included).
        ctx.set_env(AlpenClientEnv(fullnode_count=0))

    def main(self, ctx: flexitest.RunContext) -> bool:
        alpen_seq: AlpenClientService = self.get_service(ServiceType.AlpenSequencer)

        # The admin server binds shortly after node launch; the main RPC can
        # come up first, so wait for the authenticated call to land.
        wait_until(
            lambda: alpen_seq.get_admin_status() is not None,
            error_with="admin RPC did not become ready",
            timeout=30,
        )

        # A token signed with the node-generated secret is accepted, and the
        # skeleton method reports sequencer mode.
        status = alpen_seq.get_admin_status()
        logger.info("admin status: %s", status)
        assert isinstance(status["version"], str) and status["version"], (
            f"expected non-empty version string, got {status!r}"
        )
        assert status["sequencer"] is True, f"expected sequencer mode, got {status!r}"

        # No Authorization header at all.
        assert_unauthorized(JsonRpcClient(alpen_seq.props["admin_rpc_url"]))

        # A token signed with the wrong secret.
        assert_unauthorized(alpen_seq.create_admin_rpc(build_admin_jwt(secrets.token_hex(32))))

        # A token signed with the right secret but an iat outside the
        # +-60 second validity window.
        stale_token = build_admin_jwt(
            alpen_seq.read_admin_jwt_secret(), iat=int(time.time()) - 3600
        )
        assert_unauthorized(alpen_seq.create_admin_rpc(stale_token))

        # The admin namespace is not exposed on the public RPC port.
        try:
            alpen_seq.create_rpc().alpenadmin_getAdminStatus()
        except RpcError as e:
            logger.info("public RPC rejected alpenadmin call as expected: %s", e)
        else:
            raise AssertionError("public RPC should not serve the alpenadmin namespace")

        return True
