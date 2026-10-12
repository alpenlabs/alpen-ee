//! Calldata for the workload contracts and the precompiles it calls.

use alloy_primitives::{hex, Address, Bytes, B256, U256};
use alloy_sol_types::{sol, SolCall, SolValue};
use k256::schnorr;

sol! {
    interface IToken {
        function transfer(address to, uint256 value) external returns (bool);
        function approve(address spender, uint256 value) external returns (bool);
        function transferFrom(address from, address to, uint256 value) external returns (bool);
    }

    interface IPair {
        function addLiquidity(uint256 amount0, uint256 amount1) external;
        function swap(bool zeroForOne, uint256 amountIn, uint256 minOut) external returns (uint256);
    }
}

/// Init code of `contracts/Token.sol`, compiled by `contracts/compile.sh`.
const TOKEN_INIT_CODE_HEX: &str = include_str!("../../contracts/out/Token.bin");

/// Init code of `contracts/Pair.sol`, compiled by `contracts/compile.sh`.
const PAIR_INIT_CODE_HEX: &str = include_str!("../../contracts/out/Pair.bin");

/// Operator selection that lets any bridge operator take a withdrawal.
const ANY_OPERATOR: u32 = u32::MAX;

/// Init code deploying a token with `tag` that mints `supply` to the deployer.
pub(crate) fn token_init_code(tag: u64, supply: U256) -> Bytes {
    with_args(
        TOKEN_INIT_CODE_HEX,
        (U256::from(tag), supply).abi_encode_params(),
    )
}

/// Init code deploying a pool over `token0` and `token1`.
pub(crate) fn pair_init_code(token0: Address, token1: Address) -> Bytes {
    with_args(PAIR_INIT_CODE_HEX, (token0, token1).abi_encode_params())
}

fn with_args(init_code_hex: &str, args: Vec<u8>) -> Bytes {
    let mut code = hex::decode(init_code_hex.trim()).expect("compiled init code is hex");
    code.extend(args);
    code.into()
}

pub(crate) fn transfer(to: Address, value: U256) -> Bytes {
    IToken::transferCall { to, value }.abi_encode().into()
}

pub(crate) fn approve(spender: Address, value: U256) -> Bytes {
    IToken::approveCall { spender, value }.abi_encode().into()
}

pub(crate) fn transfer_from(from: Address, to: Address, value: U256) -> Bytes {
    IToken::transferFromCall { from, to, value }
        .abi_encode()
        .into()
}

pub(crate) fn add_liquidity(amount0: U256, amount1: U256) -> Bytes {
    IPair::addLiquidityCall { amount0, amount1 }
        .abi_encode()
        .into()
}

pub(crate) fn swap(zero_for_one: bool, amount_in: U256) -> Bytes {
    IPair::swapCall {
        zeroForOne: zero_for_one,
        amountIn: amount_in,
        minOut: U256::ZERO,
    }
    .abi_encode()
    .into()
}

/// Bridge-out calldata paying out to a P2WPKH script with `pubkey_hash`.
pub(crate) fn bridge_out(pubkey_hash: [u8; 20]) -> Bytes {
    let mut data = ANY_OPERATOR.to_be_bytes().to_vec();
    // BOSD type 0x03 is P2WPKH, followed by the 20-byte key hash.
    data.push(0x03);
    data.extend_from_slice(&pubkey_hash);
    data.into()
}

/// Schnorr precompile calldata checking a valid BIP-340 signature over
/// `message` by `key`: `pubkey || message || signature`.
pub(crate) fn schnorr_verify(key: B256, message: B256) -> Bytes {
    let signing_key = schnorr::SigningKey::from_bytes(key.as_slice()).expect("valid schnorr key");
    let signature = signing_key
        .sign_raw(message.as_slice(), &[0u8; 32])
        .expect("signing a 32-byte message cannot fail");

    let mut data = signing_key.verifying_key().to_bytes().to_vec();
    data.extend_from_slice(message.as_slice());
    data.extend_from_slice(&signature.to_bytes());
    data.into()
}
