use anchor_lang::prelude::*;

#[error_code]
pub enum ShieldError {
    #[msg("JIT back-run detected: liquidity removal blocked until lock window expires")]
    JitBackRunDetected,
    #[msg("Nebula Shield is disabled for this pool")]
    ShieldDisabled,
    #[msg("Arb sweep cooldown is still active — wait for cooldown_slots to elapse")]
    ArbCooldownActive,
    #[msg("Spread too small — does not meet min_spread_bps threshold")]
    SpreadTooSmall,
    #[msg("No arb profit captured in this sweep")]
    NoArbProfit,
    #[msg("Insufficient oracle observations for TWAP calculation")]
    InsufficientOracleData,
    #[msg("Invalid TiPy treasury account")]
    InvalidTreasury,
    #[msg("Arithmetic overflow")]
    ArithmeticOverflow,
    #[msg("Division by zero")]
    DivisionByZero,
    #[msg("Sweep amount is below minimum threshold")]
    AmountBelowMin,
    #[msg("Not the authorized arb authority for this pool")]
    NotArbAuthority,
    #[msg("Not the program admin")]
    NotAdmin,
    #[msg("Shield config mismatch: pool key does not match")]
    PoolMismatch,
    #[msg("JIT lock slots exceed the safety ceiling (MAX_LOCK_SLOTS = 100)")]
    LockSlotsExceedMax,
    #[msg("treasury_share_bps cannot exceed 10000 (100%)")]
    InvalidShareBps,
    #[msg("min_spread_bps must be greater than zero")]
    ZeroSpreadBps,
    #[msg("min_sweep_amount must be less than max_sweep_amount")]
    InvalidSweepRange,
    #[msg("twap_window_seconds must be at least 15")]
    TwapWindowTooShort,
}
