//! Liquidation checks that can only read prices through the shared oracle
//! staleness gate.

pub mod checks;

pub use checks::{
    LiquidationCheckError, LiquidationChecks, LiquidationDecision, Position, BPS_DIVISOR,
    MAX_INCENTIVE_BPS,
};
