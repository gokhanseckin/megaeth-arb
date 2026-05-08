// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

interface IUniswapV2Pair {
    function token0() external view returns (address);
    function token1() external view returns (address);
    function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);

    /// @notice Direct swap on a V2 pair. Caller must transfer the input token to the pair before calling.
    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data) external;
}
