//! TiPy fee routing.
//!
//! When protocol fees are collected, a share is forwarded to the TiPy
//! treasury (X1 tip router). Splits are basis-point based and immutable
//! at the program level so admins cannot reroute fees away from TiPy.

use anchor_lang::prelude::*;

/// X1 TiPy treasury — receives the routed share of protocol fees.
/// Mirrors the TREASURY constant in the tip-router program.
pub const TIPY_TREASURY: Pubkey =
    pubkey!("A1TRS3i2g62Zf6K4vybsW4JLx8wifqSoThyTQqXNaLDK");

/// TiPy tip-router program — for documentation / on-chain audit only.
pub const TIPY_PROGRAM_ID: Pubkey =
    pubkey!("HGVcd8ufhmkxdkLS1EVwEe9nCWYcitBwjmm1Jsh3RhoV");

/// Share of collected protocol fee routed to TiPy treasury (in bps).
pub const TIPY_ROUTE_BPS: u64 = 2000; // 20.00%

/// Basis-point denominator.
pub const TIPY_BPS_DENOM: u64 = 10_000;

/// Split a raw protocol-fee amount into `(admin_share, tipy_share)`.
/// `tipy_share` is `floor(amount * TIPY_ROUTE_BPS / 10_000)`.
#[inline]
pub fn split(amount: u64) -> (u64, u64) {
    let tipy = (amount as u128)
        .saturating_mul(TIPY_ROUTE_BPS as u128)
        / TIPY_BPS_DENOM as u128;
    let tipy = tipy as u64;
    let admin = amount.saturating_sub(tipy);
    (admin, tipy)
}
