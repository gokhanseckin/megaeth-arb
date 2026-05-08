// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import {Test} from "forge-std/Test.sol";
import {UniV2Math} from "../src/libs/UniV2Math.sol";

/// @dev `vm.expectRevert` only catches reverts at a *deeper* call depth than the
/// cheatcode itself — library `internal pure` calls get inlined and share the
/// test's frame. We wrap them in this external-call harness so revert assertions work.
contract UniV2MathHarness {
    function getAmountOut(uint256 amountIn, uint256 reserveIn, uint256 reserveOut, uint256 feeBps)
        external
        pure
        returns (uint256)
    {
        return UniV2Math.getAmountOut(amountIn, reserveIn, reserveOut, feeBps);
    }
}

contract UniV2MathTest is Test {
    UniV2MathHarness internal harness;

    function setUp() public {
        harness = new UniV2MathHarness();
    }

    function test_KnownVector_30bps() public pure {
        // amountIn=1e18, reserveIn=10e18, reserveOut=10e18, fee=30bps (UniV2 default).
        // amountInWithFee = 1e18 * 9970 = 9.97e21
        // numerator       = 9.97e21 * 10e18 = 9.97e40
        // denominator     = 10e18 * 10_000 + 9.97e21 = 1.0997e23
        // out             = 9.97e40 / 1.0997e23 ≈ 9.066109e17
        uint256 out = UniV2Math.getAmountOut(1e18, 10e18, 10e18, 30);
        assertEq(out, 906_610_893_880_149_131);
    }

    /// Fuzz over realistic V2 reserve ranges. Uniswap V2 stores reserves as uint112,
    /// so values at or near 2^112 are the extreme but valid inputs we care about.
    function testFuzz_NeverExceedsReserveOut(
        uint112 amountIn,
        uint112 reserveIn,
        uint112 reserveOut,
        uint16 feeBps
    ) public pure {
        amountIn = uint112(bound(amountIn, 1, type(uint112).max));
        reserveIn = uint112(bound(reserveIn, 1, type(uint112).max));
        reserveOut = uint112(bound(reserveOut, 1, type(uint112).max));
        feeBps = uint16(bound(feeBps, 0, 9_999));
        uint256 out = UniV2Math.getAmountOut(amountIn, reserveIn, reserveOut, feeBps);
        assertLt(out, reserveOut);
    }

    function test_RevertsOnZeroIn() public {
        vm.expectRevert(bytes("V2:AMOUNT_IN"));
        harness.getAmountOut(0, 1, 1, 30);
    }

    function test_RevertsOnZeroReserve() public {
        vm.expectRevert(bytes("V2:RESERVES"));
        harness.getAmountOut(1, 0, 1, 30);
    }

    function test_RevertsOnBadFee() public {
        vm.expectRevert(bytes("V2:FEE"));
        harness.getAmountOut(1, 1, 1, 10_000);
    }
}
