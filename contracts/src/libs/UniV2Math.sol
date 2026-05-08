// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

/// @notice Constant-product (x*y=k) swap math for Uniswap V2-style pools.
/// @dev MUST stay byte-for-byte equivalent to the Rust simulation in
///      `searcher/crates/searcher-core/src/math.rs::v2_amount_out`.
///      The math-parity test compares both implementations.
library UniV2Math {
    /// @notice Compute output for a V2 swap given reserves and a fee in bps (e.g. 30 = 0.3%).
    /// @param amountIn input amount, must be > 0
    /// @param reserveIn reserve of input token, must be > 0
    /// @param reserveOut reserve of output token, must be > 0
    /// @param feeBps swap fee in bps (numerator over 10_000)
    function getAmountOut(
        uint256 amountIn,
        uint256 reserveIn,
        uint256 reserveOut,
        uint256 feeBps
    ) internal pure returns (uint256 amountOut) {
        require(amountIn > 0, "V2:AMOUNT_IN");
        require(reserveIn > 0 && reserveOut > 0, "V2:RESERVES");
        require(feeBps < 10_000, "V2:FEE");

        uint256 amountInWithFee = amountIn * (10_000 - feeBps);
        uint256 numerator = amountInWithFee * reserveOut;
        uint256 denominator = (reserveIn * 10_000) + amountInWithFee;
        amountOut = numerator / denominator;
    }
}
