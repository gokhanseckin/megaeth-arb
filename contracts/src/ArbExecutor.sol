// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import {IPool, IFlashLoanSimpleReceiver} from "./interfaces/IPool.sol";
import {IUniswapV2Pair} from "./interfaces/IUniswapV2Pair.sol";
import {IERC20} from "./interfaces/IERC20.sol";

/// @title ArbExecutor
/// @notice Atomic flash-loan arbitrage executor for MegaETH.
/// Receives an Aave V3 flash loan, runs an off-chain-computed sequence of V2-style swaps,
/// and reverts unless `minProfit` is realised.
///
/// Design rules:
///  - Holds no funds between transactions (any dust is sweepable by owner).
///  - Direct pool calls only — no router intermediaries.
///  - All `amountOut`s are computed off-chain; the contract only verifies final P&L.
///  - Owner-only entry; only Aave Pool can invoke the callback.
contract ArbExecutor is IFlashLoanSimpleReceiver {
    /// @notice Aave V3 Pool that issues flash loans on this network.
    address public immutable POOL;

    /// @notice Owner address — typically a multisig in production.
    address public owner;

    /// @notice Reentrancy guard. 1 = unlocked, 2 = locked.
    uint256 private _locked = 1;

    /// @notice One swap leg in the arb cycle.
    /// @dev Packed off-chain into the `params` blob; decoded inside `executeOperation`.
    struct Leg {
        address pair;       // V2 pair address
        address tokenIn;    // input token for this leg
        address tokenOut;   // output token for this leg
        uint256 amountOut;  // off-chain-computed expected output (used as `amount{0,1}Out`)
    }

    error NotOwner();
    error NotPool();
    error BadInitiator();
    error Reentrant();
    error InsufficientProfit(uint256 received, uint256 required);
    error EmptyRoute();

    event ArbExecuted(address indexed asset, uint256 loan, uint256 profit);
    event OwnershipTransferred(address indexed from, address indexed to);

    modifier onlyOwner() {
        if (msg.sender != owner) revert NotOwner();
        _;
    }

    modifier nonReentrant() {
        if (_locked == 2) revert Reentrant();
        _locked = 2;
        _;
        _locked = 1;
    }

    constructor(address pool, address owner_) {
        POOL = pool;
        owner = owner_;
    }

    /// @notice Entry point: kick off an Aave flash loan and execute the route.
    /// @param asset       The flash-loaned ERC-20 (also the cycle's start/end token).
    /// @param amount      Loan principal.
    /// @param route       ABI-encoded `Leg[]` describing the swap cycle.
    /// @param minProfit   Minimum profit (in `asset` wei) above loan + premium; revert otherwise.
    function startArb(
        address asset,
        uint256 amount,
        bytes calldata route,
        uint256 minProfit
    ) external onlyOwner nonReentrant {
        bytes memory params = abi.encode(route, minProfit);
        IPool(POOL).flashLoanSimple(address(this), asset, amount, params, 0);
    }

    /// @inheritdoc IFlashLoanSimpleReceiver
    function executeOperation(
        address asset,
        uint256 amount,
        uint256 premium,
        address initiator,
        bytes calldata params
    ) external override returns (bool) {
        if (msg.sender != POOL) revert NotPool();
        if (initiator != address(this)) revert BadInitiator();

        (bytes memory route, uint256 minProfit) = abi.decode(params, (bytes, uint256));
        Leg[] memory legs = abi.decode(route, (Leg[]));
        if (legs.length == 0) revert EmptyRoute();

        // Send the loan to the first pair, then walk the cycle.
        // Each leg sends the *next* leg's pair as `to`, so output lands where it's needed
        // for the next swap (saves a transfer per leg).
        IERC20(legs[0].tokenIn).transfer(legs[0].pair, amount);

        for (uint256 i = 0; i < legs.length; i++) {
            address recipient = (i + 1 < legs.length) ? legs[i + 1].pair : address(this);
            _v2Swap(legs[i], recipient);
        }

        // Profit check: contract must end with loan + premium + minProfit of `asset`.
        uint256 owed = amount + premium;
        uint256 bal = IERC20(asset).balanceOf(address(this));
        uint256 required = owed + minProfit;
        if (bal < required) revert InsufficientProfit(bal, required);

        IERC20(asset).approve(POOL, owed);
        emit ArbExecuted(asset, amount, bal - owed);
        return true;
    }

    /// @dev Issue a V2-style swap. Caller must have already transferred `tokenIn` to `leg.pair`.
    function _v2Swap(Leg memory leg, address to) internal {
        IUniswapV2Pair pair = IUniswapV2Pair(leg.pair);
        // Determine output side via lexicographic ordering on the pair.
        bool zeroForOne = leg.tokenIn < leg.tokenOut;
        (uint256 amount0Out, uint256 amount1Out) = zeroForOne
            ? (uint256(0), leg.amountOut)
            : (leg.amountOut, uint256(0));
        pair.swap(amount0Out, amount1Out, to, "");
    }

    /// @notice Owner-only sweep for any token left behind (e.g. rounding dust).
    function sweep(address token, address to, uint256 amount) external onlyOwner {
        IERC20(token).transfer(to, amount);
    }

    function transferOwnership(address next) external onlyOwner {
        emit OwnershipTransferred(owner, next);
        owner = next;
    }
}
