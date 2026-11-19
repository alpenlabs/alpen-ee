// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

/// Calls a precompile and reverts unless it returns the expected output, so
/// a wrong result fails the transaction and shows in the block's receipts.
contract PrecompileCheck {
    function check(address target, bytes calldata input, bytes calldata expected) external view {
        (bool ok, bytes memory out) = target.staticcall(input);
        require(ok, "precompile failed");
        require(keccak256(out) == keccak256(expected), "precompile output mismatch");
    }
}
