// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

interface IToken {
    function transfer(address to, uint256 value) external returns (bool);
    function transferFrom(address from, address to, uint256 value) external returns (bool);
}

/// Constant-product pool with a 0.3% fee for the workload generator.
///
/// Shaped like a Uniswap V2 pair reached through a router: traders approve the
/// pool once, then each swap is one call that pulls the input token, pays out
/// the other, and updates the packed reserves slot.
contract Pair {
    IToken public immutable token0;
    IToken public immutable token1;
    uint112 public reserve0;
    uint112 public reserve1;
    uint32 public blockTimestampLast;

    event Sync(uint112 reserve0, uint112 reserve1);
    event Swap(address indexed sender, uint256 amountIn, uint256 amountOut, bool zeroForOne);

    constructor(IToken token0_, IToken token1_) {
        token0 = token0_;
        token1 = token1_;
    }

    function addLiquidity(uint256 amount0, uint256 amount1) external {
        require(token0.transferFrom(msg.sender, address(this), amount0), "token0");
        require(token1.transferFrom(msg.sender, address(this), amount1), "token1");
        _update(uint256(reserve0) + amount0, uint256(reserve1) + amount1);
    }

    function swap(bool zeroForOne, uint256 amountIn, uint256 minOut) external returns (uint256 amountOut) {
        (IToken tokenIn, IToken tokenOut, uint256 reserveIn, uint256 reserveOut) = zeroForOne
            ? (token0, token1, uint256(reserve0), uint256(reserve1))
            : (token1, token0, uint256(reserve1), uint256(reserve0));

        require(tokenIn.transferFrom(msg.sender, address(this), amountIn), "input");
        uint256 inWithFee = amountIn * 997;
        amountOut = (inWithFee * reserveOut) / (reserveIn * 1000 + inWithFee);
        require(amountOut > 0 && amountOut >= minOut, "output");
        require(tokenOut.transfer(msg.sender, amountOut), "payout");

        if (zeroForOne) {
            _update(reserveIn + amountIn, reserveOut - amountOut);
        } else {
            _update(reserveOut - amountOut, reserveIn + amountIn);
        }
        emit Swap(msg.sender, amountIn, amountOut, zeroForOne);
    }

    function _update(uint256 balance0, uint256 balance1) private {
        require(balance0 <= type(uint112).max && balance1 <= type(uint112).max, "overflow");
        reserve0 = uint112(balance0);
        reserve1 = uint112(balance1);
        blockTimestampLast = uint32(block.timestamp);
        emit Sync(uint112(balance0), uint112(balance1));
    }
}
