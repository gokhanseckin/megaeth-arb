// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

/// @title V3SwapProbe
/// @notice Off-chain quoting helper used by the searcher's parity tests.
///         Implements the Uniswap V3 swap callback so the pool can call back
///         into us during a simulated `swap`. We never pay the input — instead
///         we revert with the abi-encoded `(amount0Delta, amount1Delta)` so the
///         caller (an `eth_call` with `stateOverride` injecting this code at
///         the probe address) can decode the simulated amounts.
///
///         This is the same pattern Uniswap's own `Quoter` contract uses
///         internally; we just collapse the try/catch wrapper because the
///         caller reads the revert data directly from the `eth_call` response.
contract V3SwapProbe {
    /// @notice Uniswap V3 swap callback. Reverts with `abi.encode(a0, a1)`.
    function uniswapV3SwapCallback(
        int256 a0,
        int256 a1,
        bytes calldata
    ) external pure {
        bytes memory data = abi.encode(a0, a1);
        assembly {
            revert(add(data, 32), mload(data))
        }
    }
}
