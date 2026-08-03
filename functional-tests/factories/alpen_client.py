"""
Alpen-client node factory.
Creates alpen-client sequencer and fullnode instances.
"""

import contextlib
import os
import secrets
from pathlib import Path

import flexitest

from common.alpen_params import compose_alpen_params
from common.config import (
    AlpenClientConfig,
    AlpenFullNodeConfig,
    AlpenL1FeePolicyConfig,
    AlpenOlConfig,
    AlpenSequencerConfig,
    BitcoindConfig,
    EeDaConfig,
)
from common.config.constants import DEFAULT_EE_BLOCK_TIME_MS
from common.datatool import generate_ee_params
from common.prover_backend import NATIVE_BACKEND, ProverBackend
from common.services import AlpenClientProps, AlpenClientService


def generate_p2p_secret_key() -> str:
    """Generate a random 32-byte hex-encoded P2P secret key."""
    return secrets.token_hex(32)


def generate_sequencer_keypair() -> tuple[str, str]:
    """
    Generate a sequencer keypair (private key, X-only public key).

    Returns:
        Tuple of (private_key_hex, public_key_hex) - both 32 bytes hex-encoded

    Note:
        The public key is the X-only public key (32 bytes) derived from the
        private key using secp256k1. This is required for Schnorr signature
        verification in the gossip protocol.
    """
    # Use a deterministic test keypair with a properly derived public key
    # Private key: 0x0101...01 (32 bytes of 0x01)
    # Public key: derived X-only public key from the private key
    privkey = "0x" + "01" * 32
    # This X-only public key was derived from the private key using secp256k1
    pubkey = "0x1b84c5567b126440995d3ed5aaba0565d71e1834604819ff9c17f5e9d5dd078f"
    return privkey, pubkey


class AlpenClientFactory(flexitest.Factory):
    """
    Factory for creating alpen-client nodes.
    """

    def __init__(self, port_range: range):
        ports = list(port_range)
        if any(p < 1024 or p > 65535 for p in ports):
            raise ValueError(
                f"Port range must be between 1024 and 65535. "
                f"Got: {port_range.start}-{port_range.stop - 1}"
            )
        super().__init__(ports)

    @flexitest.with_ectx("ctx")
    def create_sequencer(
        self,
        sequencer_privkey: str,
        da_config: EeDaConfig,
        p2p_secret_key: str | None = None,
        enable_discovery: bool = False,
        custom_chain: str = "dev",
        ee_params_path: Path | None = None,
        ol_endpoint: str | None = None,
        ol_submit_endpoint: str | None = None,
        ol_submit_token: str | None = None,
        batch_sealing_block_count: int = 100,
        epoch_tracking_mode: str = "confirmed",
        bridge_denomination: int = 100_000_000,
        max_withdrawal_amount: int | None = 1_000_000_000,
        beneficiary_address: str | None = None,
        da_rate_wei_per_byte: int = 0,
        prover: ProverBackend = NATIVE_BACKEND,
        prover_programs: list[tuple[str, str, str]] | None = None,
        **kwargs,
    ) -> AlpenClientService:
        """
        Create an alpen-client sequencer node.

        Args:
            sequencer_privkey: Sequencer's private key (hex, 32 bytes) - set as env var.
                The gossip/DA-reveal pubkey is derived from this, not configured separately.
            da_config: DA pipeline configuration for posting state diffs to L1. Required,
                because a sequencer's `--alpen-config` always needs a `[sequencer.bitcoind]`
                table.
            p2p_secret_key: P2P secret key for deterministic enode (hex, 32 bytes)
            enable_discovery: Enable discv5 peer discovery (for bootnode mode)
            custom_chain: Chain spec to use
            ee_params_path: EE params file to use; generated when omitted
            prover: Which EE prover backend to run; see common/prover_backend.py
            prover_programs: List of (spec_version, chunk_signing_key_hex,
                acct_signing_key_hex) triples, each written as its own
                `[[sequencer.prover.programs]]` entry. `spec_version` is the
                `AlpenSpecId` the program is built for (e.g. "v0", "v1").
                Native-only; defaults to the backend's own single v0 program.
        """
        ctx: flexitest.EnvContext = kwargs["ctx"]

        datadir = Path(ctx.make_service_dir("ee_sequencer"))
        http_port = self.next_port()
        p2p_port = self.next_port()
        authrpc_port = self.next_port()
        logfile = datadir / "service.log"

        # Generate P2P secret key if not provided
        if p2p_secret_key is None:
            p2p_secret_key = generate_p2p_secret_key()

        # Write P2P secret key to file (alpen-client expects hex string in file)
        p2p_secret_key_file = datadir / "p2p_secret_key"
        # Remove 0x prefix if present
        key_hex = p2p_secret_key.removeprefix("0x")
        p2p_secret_key_file.write_text(key_hex)

        prover_config = prover.prover_config(datadir, prover_programs)

        if ee_params_path is None:
            ee_params_path = generate_ee_params(
                datadir,
                bridge_denomination_sats=bridge_denomination,
                max_withdrawal_amount_sats=max_withdrawal_amount,
            )
        alpen_params_path = compose_alpen_params(
            datadir,
            ee_params_path,
            chain=custom_chain,
            bridge_denomination=bridge_denomination,
            max_withdrawal_amount=max_withdrawal_amount,
            da_magic_bytes=da_config.magic_bytes.decode("ascii"),
        )

        ol_config = (
            AlpenOlConfig(
                source="rpc",
                client_url=ol_endpoint,
                epoch_tracking_mode=epoch_tracking_mode,
            )
            if ol_endpoint
            else AlpenOlConfig(source="dummy", epoch_tracking_mode=epoch_tracking_mode)
        )

        sequencer_config = AlpenSequencerConfig(
            bitcoind=BitcoindConfig(
                rpc_url=da_config.btc_rpc_url,
                rpc_user=da_config.btc_rpc_user,
                rpc_password=da_config.btc_rpc_password,
                network=da_config.network,
            ),
            # Required against a real OL node, meaningless against the dummy one.
            ol_submit_url=ol_submit_endpoint if ol_endpoint else None,
            beneficiary_address=beneficiary_address,
            blocktime_ms=DEFAULT_EE_BLOCK_TIME_MS,
            batch_sealing_block_count=batch_sealing_block_count,
            prover=prover_config,
            l1_fee_policy=AlpenL1FeePolicyConfig(fee_policy="fixed", fixed_fee_rate=1.0),
        )

        alpen_config = AlpenClientConfig(
            mode="sequencer",
            ol=ol_config,
            sequencer=sequencer_config,
            health_check_host="127.0.0.1",
            health_check_port=0,
            l1_reorg_safe_depth=da_config.l1_reorg_safe_depth,
            genesis_l1_height=da_config.genesis_l1_height,
        )
        alpen_config_path = datadir / "alpen-config.toml"
        alpen_config_path.write_text(alpen_config.as_toml_string())

        # fmt: off
        cmd = [
            "alpen-client",
            "--datadir", str(datadir),
            "--alpen-config", str(alpen_config_path),
            "--alpen-params", str(alpen_params_path),
            "--addr", "127.0.0.1",  # Force IPv4 for testing
            "--nat", "extip:127.0.0.1",  # Force enode to show 127.0.0.1
            "--port", str(p2p_port),
            "--http",
            "--http.port", str(http_port),
            "--http.api", "eth,net,admin,debug,alpen",
            "--authrpc.port", str(authrpc_port),
            "--p2p-secret-key", str(p2p_secret_key_file),
            "-vvvv",
        ]
        # fmt: on

        # Discovery mode configuration:
        # - enable_discovery=True: Use discv5 only (disable discv4)
        # - enable_discovery=False: Disable all discovery (rely on admin_addPeer/trusted-peers)
        if enable_discovery:
            discv5_port = self.next_port()
            # fmt: off
            cmd.extend([
                "--disable-discv4-discovery",  # Don't use legacy discv4
                "--enable-discv5-discovery",
                "--discovery.v5.addr", "127.0.0.1",
                "--discovery.v5.port", str(discv5_port),
            ])
            # fmt: on
        else:
            # Disable all discovery - peers connect via admin_addPeer or --trusted-peers
            cmd.append("-d")

        http_url = f"http://127.0.0.1:{http_port}"

        props: AlpenClientProps = {
            "http_port": http_port,
            "http_url": http_url,
            "p2p_port": p2p_port,
            "datadir": str(datadir),
            "mode": "sequencer",
            "enode": None,  # Will be populated after start
        }

        # Set environment variable for sequencer private key
        env = os.environ.copy()
        env["SEQUENCER_PRIVATE_KEY"] = sequencer_privkey
        # DA fee rate (wei per byte). 0 keeps the in-EVM DA fee charge dormant.
        env["ALPEN_DA_RATE_WEI_PER_BYTE"] = str(da_rate_wei_per_byte)
        if ol_submit_token:
            env["STRATA_SUBMIT_RPC_TOKEN"] = ol_submit_token

        svc = AlpenClientService(
            props,
            cmd,
            stdout=str(logfile),
            name="ee_sequencer",
            env=env,
        )
        svc.stop_timeout = 30

        try:
            svc.start()
        except Exception as e:
            with contextlib.suppress(Exception):
                svc.stop()
            raise RuntimeError(f"Failed to start alpen-client sequencer: {e}") from e

        return svc

    @flexitest.with_ectx("ctx")
    def create_fullnode(
        self,
        sequencer_pubkey: str,
        trusted_peers: list[str] | None = None,
        bootnodes: list[str] | None = None,
        enable_discovery: bool = False,
        p2p_secret_key: str | None = None,
        custom_chain: str = "dev",
        ee_params_path: Path | None = None,
        instance_id: int = 0,
        datadir_override: str | None = None,
        sequencer_http: str | None = None,
        ol_endpoint: str | None = None,
        bridge_denomination: int = 100_000_000,
        max_withdrawal_amount: int | None = 1_000_000_000,
        **kwargs,
    ) -> AlpenClientService:
        """
        Create an alpen-client fullnode.

        Args:
            sequencer_pubkey: Sequencer's public key for signature validation
            trusted_peers: List of enode URLs to connect to (direct connection)
            bootnodes: List of enode URLs for discovery bootstrap
            enable_discovery: Enable discv5 peer discovery
            p2p_secret_key: P2P secret key for deterministic enode
            custom_chain: Chain spec to use
            ee_params_path: EE params file to use; generated when omitted
            instance_id: Instance ID for multiple fullnodes
            datadir_override: Optional datadir path (bypasses EnvContext requirement)
            sequencer_http: Sequencer HTTP URL for transaction forwarding
        """
        if datadir_override:
            datadir = Path(datadir_override)
            datadir.mkdir(parents=True, exist_ok=True)
        else:
            ctx: flexitest.EnvContext = kwargs["ctx"]
            datadir = Path(ctx.make_service_dir(f"ee_fullnode_{instance_id}"))
        http_port = self.next_port()
        p2p_port = self.next_port()
        authrpc_port = self.next_port()
        logfile = datadir / "service.log"

        # Generate P2P secret key if not provided
        if p2p_secret_key is None:
            p2p_secret_key = generate_p2p_secret_key()

        # Write P2P secret key to file (alpen-client expects hex string in file)
        p2p_secret_key_file = datadir / "p2p_secret_key"
        # Remove 0x prefix if present
        key_hex = p2p_secret_key.removeprefix("0x")
        p2p_secret_key_file.write_text(key_hex)

        ol_config = (
            AlpenOlConfig(source="rpc", client_url=ol_endpoint)
            if ol_endpoint
            else AlpenOlConfig(source="dummy")
        )
        if ee_params_path is None:
            ee_params_path = generate_ee_params(
                datadir,
                bridge_denomination_sats=bridge_denomination,
                max_withdrawal_amount_sats=max_withdrawal_amount,
            )
        alpen_params_path = compose_alpen_params(
            datadir,
            ee_params_path,
            chain=custom_chain,
            bridge_denomination=bridge_denomination,
            max_withdrawal_amount=max_withdrawal_amount,
        )

        alpen_config = AlpenClientConfig(
            mode="full_node",
            ol=ol_config,
            full_node=AlpenFullNodeConfig(
                sequencer_pubkey=sequencer_pubkey,
                sequencer_http_url=sequencer_http,
            ),
            health_check_host="127.0.0.1",
            health_check_port=0,
        )
        alpen_config_path = datadir / "alpen-config.toml"
        alpen_config_path.write_text(alpen_config.as_toml_string())

        # fmt: off
        cmd = [
            "alpen-client",
            "--datadir", str(datadir),
            "--alpen-config", str(alpen_config_path),
            "--alpen-params", str(alpen_params_path),
            "--addr", "127.0.0.1",  # Force IPv4 for testing
            "--nat", "extip:127.0.0.1",  # Force enode to show 127.0.0.1
            "--port", str(p2p_port),
            "--http",
            "--http.port", str(http_port),
            "--http.api", "eth,net,admin,debug,alpen",
            "--authrpc.port", str(authrpc_port),
            "--p2p-secret-key", str(p2p_secret_key_file),
            "-vvvv",
        ]
        # fmt: on

        # Add trusted peers if provided
        if trusted_peers:
            cmd.extend(["--trusted-peers", ",".join(trusted_peers)])

        # Add bootnodes if provided (requires discovery to be enabled)
        if bootnodes:
            cmd.extend(["--bootnodes", ",".join(bootnodes)])

        # Discovery mode configuration:
        # - enable_discovery=True: Use discv5 only (disable discv4)
        # - enable_discovery=False: Disable all discovery (rely on admin_addPeer/trusted-peers)
        if enable_discovery:
            discv5_port = self.next_port()
            # fmt: off
            cmd.extend([
                "--disable-discv4-discovery",  # Don't use legacy discv4
                "--enable-discv5-discovery",
                "--discovery.v5.addr", "127.0.0.1",
                "--discovery.v5.port", str(discv5_port),
            ])
            # fmt: on
        else:
            # Disable all discovery - peers connect via admin_addPeer or --trusted-peers
            cmd.append("-d")

        http_url = f"http://127.0.0.1:{http_port}"

        props: AlpenClientProps = {
            "http_port": http_port,
            "http_url": http_url,
            "p2p_port": p2p_port,
            "datadir": str(datadir),
            "mode": "fullnode",
            "enode": None,
        }

        svc = AlpenClientService(
            props,
            cmd,
            stdout=str(logfile),
            name=f"ee_fullnode_{instance_id}",
        )
        svc.stop_timeout = 30

        try:
            svc.start()
        except Exception as e:
            with contextlib.suppress(Exception):
                svc.stop()
            raise RuntimeError(f"Failed to start alpen-client fullnode: {e}") from e

        return svc
