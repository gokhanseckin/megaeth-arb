// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import {Test} from "forge-std/Test.sol";
import {UniV2Math} from "../src/libs/UniV2Math.sol";

contract UniV2MathTest is Test {
    function test_KnownVector_30bps() public pure {
        // amountIn=1e18, reserveIn=10e18, reserveOut=10e18, fee=30bps (UniV2 default).
        // amountInWithFee = 1e18 * 9970 = 9.97e21
        // numerator       = 9.97e21 * 10e18 = 9.97e40
        // denominator     = 10e18 * 10_000 + 9.97e21 = 1.0997e23
        // out             = 9.97e40 / 1.0997e23 ≈ 9.066109e17
        uint256 out = UniV2Math.getAmountOut(1e18, 10e18, 10e18, 30);
        assertEq(out, 906_610_893_880_149_131);
    }

    function testFuzz_NeverExceedsReserveOut(
        uint128 amountIn,
        uint128 reserveIn,
        uint128 reserveOut,
        uint16 feeBps
    ) public pure {
        amountIn = uint128(bound(amountIn, 1, type(uint128).max));
        reserveIn = uint128(bound(reserveIn, 1, type(uint128).max));
        reserveOut = uint128(bound(reserveOut, 1, type(uint128).max));
        feeBps = uint16(bound(feeBps, 0, 9_999));
        uint256 out = UniV2Math.getAmountOut(amountIn, reserveIn, reserveOut, feeBps);
        assertLt(out, reserveOut);
    }

    function test_RevertsOnZeroIn() public {
        vm.expectRevert(bytes("V2:AMOUNT_IN"));
        UniV2Math.getAmountOut(0, 1, 1, 30);
    }

    function test_RevertsOnZeroReserve() public {
        vm.expectRevert(bytes("V2:RESERVES"));
        UniV2Math.getAmountOut(1, 0, 1, 30);
    }
}
