"""Configuration regressions only: never starts nodes or submits proofs."""

import json
import os
import tempfile
import unittest
from contextlib import ExitStack
from pathlib import Path
from unittest.mock import patch

from common import prover_backend
from common.alpen_params import compose_alpen_params
from envconfigs import eest
from envconfigs.el_ol import EeOLEnv
from scripts import gen_eest_sp1_guest_params


class EestEnvironmentTests(unittest.TestCase):
    def setUp(self):
        contexts = ExitStack()
        self.addCleanup(contexts.close)
        contexts.enter_context(patch.dict(os.environ, {}, clear=True))
        self.artifacts = Path(contexts.enter_context(tempfile.TemporaryDirectory()))
        for name in ("ACCT_PREDICATE", "EE_PARAMS", "CHUNK_ELF", "ACCT_ELF"):
            path = self.artifacts / name
            path.write_text("Sp1Groth16:test-predicate" if name == "ACCT_PREDICATE" else "fixture")
            contexts.enter_context(patch.object(prover_backend, name, path))
        contexts.enter_context(
            patch.dict(
                prover_backend._SP1_ARTIFACTS,
                {
                    "v1": (
                        prover_backend.CHUNK_ELF,
                        prover_backend.ACCT_ELF,
                        prover_backend.ACCT_PREDICATE,
                    )
                },
            )
        )

    def test_unset_backend_retains_native_five_block_batches(self):
        env = eest.create_eest_env()
        self.assertEqual(env.alpen_env_params.batch_sealing_block_count, 5)
        self.assertEqual(env.alpen_env_params.prover, prover_backend.NATIVE_BACKEND)
        self.assertIs(env.strata_config.prover, env.alpen_env_params.prover)
        self.assertEqual(env.alpen_env_params.base_fee_floor, {"v0": 0})
        self.assertEqual(env.alpen_env_params.prover.spec_versions, ("v1",))
        self.assertEqual(env.alpen_env_params.fullnode_count, 0)
        self.assertEqual(env.strata_config.pre_generate_blocks, 110)

    def test_sp1_resolved_once_and_shared_by_ee_and_ol(self):
        os.environ.update(EE_PROVER_BACKEND="sp1", ALPEN_SP1_PROOF_DEADLINE_SECS="14400")
        with patch.object(
            eest, "resolve_prover_backend", wraps=prover_backend.resolve_prover_backend
        ) as resolve:
            env = eest.create_eest_env()
        resolve.assert_called_once_with()
        self.assertEqual(env.alpen_env_params.batch_sealing_block_count, 100)
        self.assertEqual(env.alpen_env_params.base_fee_floor, {"v0": 0})
        prover = env.alpen_env_params.prover
        self.assertIs(env.strata_config.prover, prover)
        self.assertEqual(prover.backend, "sp1")
        self.assertEqual(prover.genesis_predicate, "Sp1Groth16:test-predicate")
        config = prover.prover_config(self.artifacts)
        self.assertEqual(config.deadline_secs, 14400)
        self.assertEqual(set(config.programs), {"v1"})
        self.assertEqual(config.programs["v1"].chunk_path, str(prover_backend.CHUNK_ELF))
        self.assertEqual(config.programs["v1"].acct_path, str(prover_backend.ACCT_ELF))
        self.assertEqual(prover.genesis_spec_schedule, {"v0": 0, "v1": 0})

    def test_sp1_without_deadline_preserves_client_default(self):
        os.environ["EE_PROVER_BACKEND"] = "sp1"
        self.assertIsNone(eest.create_eest_env().alpen_env_params.prover.deadline_secs)

    def test_missing_guest_artifact_fails_without_native_fallback(self):
        os.environ["EE_PROVER_BACKEND"] = "sp1"
        for name in ("ACCT_PREDICATE", "EE_PARAMS", "CHUNK_ELF", "ACCT_ELF"):
            with (
                self.subTest(artifact=name),
                self.assertRaisesRegex(RuntimeError, "requires.*SP1 guest pair"),
            ):
                path = getattr(prover_backend, name)
                contents = path.read_bytes()
                path.unlink()
                try:
                    eest.create_eest_env()
                finally:
                    path.write_bytes(contents)

    def test_rotation_program_mapping_and_deadline_are_preserved(self):
        os.environ.update(EE_PROVER_BACKEND="sp1", ALPEN_SP1_PROOF_DEADLINE_SECS="14400")
        v0_predicate = self.artifacts / "v0-predicate"
        v0_predicate.write_text("Sp1Groth16:v0")
        with patch.dict(
            prover_backend._SP1_ARTIFACTS,
            {
                "v0": (prover_backend.CHUNK_ELF, prover_backend.ACCT_ELF, v0_predicate),
            },
        ):
            prover = prover_backend.resolve_prover_backend(prover_backend.ROTATION_SPEC_VERSIONS)
        self.assertEqual(prover.genesis_spec_schedule, {"v0": 0})
        self.assertEqual(prover.genesis_predicate, "Sp1Groth16:v0")
        self.assertEqual(prover.rotation_target_predicate, "Sp1Groth16:test-predicate")
        config = prover.prover_config(self.artifacts)
        self.assertEqual(set(config.programs), {"v0", "v1"})
        self.assertEqual(config.deadline_secs, 14400)

    def test_eest_guest_params_match_live_params(self):
        os.environ["EE_PROVER_BACKEND"] = "sp1"
        prover_backend.EE_PARAMS.write_text(json.dumps({"account_id": "test-account"}))
        guest_dir = self.artifacts / "guest"
        live_dir = self.artifacts / "live"
        live_dir.mkdir()
        with (
            patch.object(gen_eest_sp1_guest_params, "GUEST_PARAMS_DIR", guest_dir),
            patch.object(
                gen_eest_sp1_guest_params,
                "generate_ee_params",
                return_value=prover_backend.EE_PARAMS,
            ),
            patch("builtins.print"),
        ):
            gen_eest_sp1_guest_params.main()
        params = eest.create_eest_env().alpen_env_params
        live_path = compose_alpen_params(
            live_dir,
            prover_backend.EE_PARAMS,
            base_fee_floor=params.base_fee_floor,
            spec_schedule=params.prover.genesis_spec_schedule,
        )
        self.assertEqual(
            json.loads((guest_dir / "alpen-params.json").read_text()),
            json.loads(live_path.read_text()),
        )
        self.assertEqual(params.base_fee_floor, {"v0": 0})
        self.assertEqual(EeOLEnv().alpen_env_params.base_fee_floor, {"v0": 0, "v1": 1_000_000_000})

    def test_invalid_deadline_fails(self):
        os.environ["EE_PROVER_BACKEND"] = "sp1"
        for value in ("", "bad", "0", "-1", "1.5"):
            with (
                self.subTest(value=value),
                patch.dict(os.environ, ALPEN_SP1_PROOF_DEADLINE_SECS=value),
                self.assertRaisesRegex(ValueError, "must be a positive integer"),
            ):
                eest.create_eest_env()

    def test_unknown_backend_fails(self):
        os.environ["EE_PROVER_BACKEND"] = "typo"
        with self.assertRaisesRegex(ValueError, "Unknown EE_PROVER_BACKEND"):
            eest.create_eest_env()


if __name__ == "__main__":
    unittest.main()
