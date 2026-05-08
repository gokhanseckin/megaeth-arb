// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

/// @notice Minimal Aave V3 Pool surface needed to take a flash loan.
interface IPool {
    /// @notice Single-asset flash loan (Aave V3).
    /// @dev `params` is forwarded to `executeOperation` on the receiver.
    function flashLoanSimple(
        address receiverAddress,
        address asset,
        uint256 amount,
        bytes calldata params,
        uint16 referralCode
    ) external;

    /// @notice The fee charged on flash loans, expressed in bps (e.g. 5 = 0.05%).
    function FLASHLOAN_PREMIUM_TOTAL() external view returns (uint128);
}

/// @notice Aave V3 flash-loan-simple receiver callback.
interface IFlashLoanSimpleReceiver {
    function executeOperation(
        address asset,
        uint256 amount,
        uint256 premium,
        address initiator,
        bytes calldata params
    ) external returns (bool);
}
