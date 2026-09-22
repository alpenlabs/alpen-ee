"""Tests for bounded regtest setup mining."""

import unittest
from unittest.mock import Mock, call

from common.bitcoin_mining import generate_blocks_in_chunks


class BitcoinMiningTests(unittest.TestCase):
    def test_mines_exact_count_in_bounded_requests(self) -> None:
        rpc = Mock()
        generate_blocks_in_chunks(rpc, 110, "regtest-address")
        self.assertEqual(
            rpc.proxy.generatetoaddress.call_args_list,
            [call(10, "regtest-address")] * 11,
        )

        rpc.proxy.generatetoaddress.reset_mock()
        generate_blocks_in_chunks(rpc, 101, "regtest-address")
        self.assertEqual(
            rpc.proxy.generatetoaddress.call_args_list,
            [call(10, "regtest-address")] * 10 + [call(1, "regtest-address")],
        )

    def test_rpc_failure_propagates_without_retry(self) -> None:
        rpc = Mock()
        rpc.proxy.generatetoaddress.side_effect = TimeoutError("Bitcoin RPC timed out")
        with self.assertRaisesRegex(TimeoutError, "Bitcoin RPC timed out"):
            generate_blocks_in_chunks(rpc, 110, "regtest-address")
        rpc.proxy.generatetoaddress.assert_called_once_with(10, "regtest-address")

    def test_invalid_count_is_rejected(self) -> None:
        rpc = Mock()
        for count in (-1, True, 1.5):
            with self.subTest(count=count), self.assertRaises((TypeError, ValueError)):
                generate_blocks_in_chunks(rpc, count, "regtest-address")
        rpc.proxy.generatetoaddress.assert_not_called()
