# Functional Tests - New Architecture

Clean, simple functional test suite for Strata.

## Philosophy

**Explicit over implicit. Simple over clever.**

- Tests explicitly start services they need
- No magic setup, no hidden state
- Clear error messages
- Easy to debug

## Binaries

`alpen-client` is built from this workspace. The strata binaries (`strata`,
`strata-signer`, `strata-datatool`, `strata-test-cli`) are
built from the strata git revision pinned in the root `Cargo.toml` via
`./build_strata_bins.sh` (checked out and built under `target/strata-git`).
`run_tests.sh` handles both automatically.

The suite contains only the alpen-relevant tests (`alpen_client`, `evm`);
the strata-node tests live in the strata repo.

## Quick Start

```bash
# Run all tests
./run_tests.sh

# Run specific test(s)
./run_tests.sh -t test_node_version
./run_tests.sh -t tests/test_node_version.py
./run_tests.sh tests/test_node_version.py
./run_tests.sh -t test_foo test_bar

# Run test group (directory-based)
./run_tests.sh -g evm
./run_tests.sh -g alpen_client evm

# List available tests
./run_tests.sh --list

# Keep-alive mode for debugging (starts env and waits, no tests run)
./run_tests.sh --keep-alive el_ol

# Get help
./run_tests.sh --help
```

## EEST proving

The shared `alpen_eest` environment resolves `EE_PROVER_BACKEND` once for both
the EE prover and OL genesis predicate. Native remains the local default and
seals every five blocks. SP1 seals every 100 blocks; the client's chunk block
count follows the batch count when no separate chunk limit is configured.
This reduces request frequency, not total proof cost by a guaranteed factor,
and is not a spending cap.

The daily `main-eest.yml` workflow explicitly selects `sp1` / `network`, builds
the guest pair using the same generated parameters as the live environment,
and requires the repository/organization Actions secret `NETWORK_PRIVATE_KEY`.
It fails if credentials or guest artifacts are absent rather than falling back
to native signed proofs. Merging this workflow change enables paid proving on
its existing daily schedule; a PR push does not start that scheduled job.

For local SP1 runs, install the matching SP1 toolchain and set
`EE_PROVER_BACKEND=sp1`, `SP1_PROVER=network`, and `NETWORK_PRIVATE_KEY` before
running `./run_tests.sh --keep-alive alpen_eest`. Never put the key in a command
line or a tracked file. `ALPEN_SP1_PROOF_DEADLINE_SECS` optionally overrides the
client's request deadline; it must be positive. The workflow's `proof_timeout`
is a separate timeout for waiting for block coverage.

Configuration-only tests (no nodes, credentials or proof requests):

```bash
uv run python -m unittest discover -s unit_tests -v
```

## Structure

```
common/       Core library (service, RPC, waiting)
factories/    Service factories
envconfigs/   Environment configs
tests/        Test files
```

## Writing a Test

```python
import flexitest
from common.base_test import StrataNodeTest
from common.config import ServiceType

@flexitest.register
class TestExample(StrataNodeTest):
    def __init__(self, ctx: flexitest.InitContext):
        ctx.set_env("el_ol")  # Bitcoin + strata + alpen-client

    def run(self) -> bool:
        # Access to runcontext is available via self.runctx
        # (set automatically by BaseTest.main)

        # Get services using ServiceType enum
        bitcoin = self.get_service(ServiceType.Bitcoin)
        strata = self.get_service(ServiceType.Strata)

        # Create RPC clients
        btc_rpc = bitcoin.create_rpc()
        strata_rpc = strata.create_rpc()

        # Wait for ready
        self.wait_for_rpc_ready(strata_rpc)

        # Do test logic
        version = strata_rpc.strata_protocolVersion()
        assert version == 1

        return True
```

## Core Utilities

### Waiting

```python
# Simple condition wait
self.wait_for(lambda: service.is_ready(), timeout=30)

# Wait for RPC
self.wait_for_rpc_ready(rpc, method="strata_protocolVersion")

# Custom wait
from common.wait import wait_until
wait_until(condition, error_with="Custom error", timeout=30, step=0.5)
```

### RPC Calls

```python
# Attribute style (uses __getattr__ for dynamic method dispatch)
version = rpc.strata_protocolVersion()
balance = rpc.eth_getBalance("0x123...", "latest")

# Explicit style
version = rpc.call("strata_protocolVersion")
balance = rpc.call("eth_getBalance", "0x123...", "latest")
```

### Service Access

```python
from common.config import ServiceType

# Get service from test (uses self.runctx internally)
bitcoin = self.get_service(ServiceType.Bitcoin)

# Access properties
rpc_port = bitcoin.get_prop("rpc_port")
datadir = bitcoin.get_prop("datadir")

# Create RPC client
rpc = bitcoin.create_rpc()
```

## Environment Configs

Environments define which services to start:

```python
# envconfigs/basic.py
from common.config import ServiceType

class BasicEnvConfig(flexitest.EnvConfig):
    def init(self, ectx: flexitest.EnvContext):
        btc_factory = ectx.get_factory(ServiceType.Bitcoin)
        strata_factory = ectx.get_factory(ServiceType.Strata)

        # Start Bitcoin
        bitcoin = btc_factory.create_regtest()
        bitcoin.wait_for_ready(timeout=10)

        # Start Strata
        strata = strata_factory.create_node(...)
        strata.wait_for_ready(timeout=10)

        return flexitest.LiveEnv({
            ServiceType.Bitcoin: bitcoin,
            ServiceType.Strata: strata,
        })
```

Use in tests:

```python
def __init__(self, ctx: flexitest.InitContext):
    ctx.set_env("el_ol")
```

## Factories

Factories create services. They should be dumb - just build command and start process.

```python
# factories/bitcoin.py
from common.services import BitcoinServiceWrapper, BitcoinServiceProps

class BitcoinFactory(flexitest.Factory):
    @flexitest.with_ectx("ctx")
    def create_regtest(self, **kwargs):
        ctx: flexitest.EnvContext = kwargs["ctx"]

        # Set up service directories and ports
        datadir = ctx.make_service_dir(ServiceType.Bitcoin)
        rpc_port = self.next_port()
        logfile = os.path.join(datadir, "service.log")

        # Build command
        cmd = ["bitcoind", "-regtest", f"-rpcport={rpc_port}", ...]

        # Create props (validated by dataclass)
        props = BitcoinServiceProps(
            rpc_port=rpc_port,
            rpc_url=f"http://localhost:{rpc_port}",
            datadir=datadir,
        )

        # Create service wrapper with RPC factory
        svc = BitcoinServiceWrapper(
            props, cmd, stdout=logfile,
            rpc_factory=lambda: BitcoindClient(...)
        )
        svc.start()
        return svc
```

## Running Tests

### Test Selection

Tests can be filtered by name or group (directory structure):

```bash
# Run all tests
./run_tests.sh

# Run specific tests by name (basename; paths or positional are OK)
./run_tests.sh -t test_node_version
./run_tests.sh -t tests/test_node_version.py
./run_tests.sh tests/test_node_version.py
./run_tests.sh -t test_foo test_bar test_baz

# Run tests by group (subdirectory under tests/)
# Example: tests/evm/test_genesis.py is in group "evm"
./run_tests.sh -g evm
./run_tests.sh -g alpen_client evm

# Combine filters (tests OR groups)
./run_tests.sh -t test_genesis -g alpen_client

# List all available tests and groups
./run_tests.sh --list
```

### Keep-Alive Mode

For debugging, start an environment and keep it running:

```bash
./run_tests.sh --keep-alive el_ol
```

This starts all services in the "el_ol" environment and keeps them alive until you press Ctrl+C. **No tests are run** - this is purely for debugging. Useful for:
- Manual testing via RPC
- Inspecting service state
- Debugging service startup issues

Service connection info will be printed on startup.

### Disabling Tests

Permanently disabled tests are defined in `entry.py` `disabled_tests()`. For temporary disabling (e.g., local debugging), use the `DISABLED_TESTS` environment variable:

```bash
# Disable single test
DISABLED_TESTS=test_flaky ./run_tests.sh

# Disable multiple tests (comma-separated)
DISABLED_TESTS=test_foo,test_bar,test_flaky ./run_tests.sh

# Works with other flags
DISABLED_TESTS=test_slow ./run_tests.sh -t test_fast test_slow
```

The env var extends the base disabled list, so both are applied.

## Debugging

### Service Logs

Logs are in test data directory:

```
_dd/
  <test_run_id>/        # Unique ID for each test run
    <env_name>/         # Environment name (e.g., "el_ol")
      bitcoin/service.log
      <service>/service.log  # e.g., strata_sequencer, alpen_sequencer
```

Example: `_dd/9-13-wpbec/el_ol/bitcoin/service.log`

### Test Logs

Each test gets its own logger:

```python
self.info("Something happened")
self.debug("Debug info")
self.error("Error occurred")
```

Set log level with environment variable:
```bash
LOG_LEVEL=DEBUG ./run_tests.sh
```

### Common Issues

**RPC not ready**: Increase timeout or check service logs
```python
self.wait_for_rpc_ready(rpc, timeout=60)
```

**Service crashed**: Check `service.log` in datadir

**Timeout errors**: Check exception message for last error and attempt count
