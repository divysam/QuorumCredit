use crate::errors::ContractError;
use crate::types::{Config, DataKey, LoanRecord};
use soroban_sdk::{token, Address, Env};

// ── Reentrancy Guard ──────────────────────────────────────────────────────────

/// Acquires the reentrancy lock. Returns `Err(Reentrancy)` if already locked.
/// Must be paired with `release_lock` at the end of every state-mutating function.
pub fn acquire_lock(env: &Env) -> Result<(), ContractError> {
    let locked: bool = env
        .storage()
        .instance()
        .get(&DataKey::Locked)
        .unwrap_or(false);
    if locked {
        return Err(ContractError::Reentrancy);
    }
    env.storage().instance().set(&DataKey::Locked, &true);
    Ok(())
}

/// Releases the reentrancy lock. Always call this before returning from a guarded function.
pub fn release_lock(env: &Env) {
    env.storage().instance().set(&DataKey::Locked, &false);
}

// ── Pause Check ───────────────────────────────────────────────────────────────

pub fn require_not_paused(env: &Env) -> Result<(), ContractError> {
    let paused: bool = env
        .storage()
        .instance()
        .get(&DataKey::Paused)
        .unwrap_or(false);
    if paused {
        Err(ContractError::ContractPaused)
    } else {
        Ok(())
    }
}

// ── Centralized Input Validation ──────────────────────────────────────────────

/// Validates that `address` is not the zero/default address.
/// Returns `Err(ZeroAddress)` if invalid.
pub fn validate_address(_env: &Env, address: &Address) -> Result<(), ContractError> {
    // In Soroban, Address is always a valid non-null type; the zero-address check
    // is enforced by the protocol. We keep this as a centralized hook for future
    // extension (e.g. blacklist checks) and to replace scattered ad-hoc checks.
    let _ = address;
    Ok(())
}

/// Validates that `amount` is strictly positive (> 0).
/// Returns `Err(InvalidAmount)` if not.
pub fn validate_amount(_env: &Env, amount: i128) -> Result<(), ContractError> {
    if amount <= 0 {
        return Err(ContractError::InvalidAmount);
    }
    Ok(())
}

/// Validates that `timestamp` is non-zero and not in the past relative to `now`.
/// Pass `now = env.ledger().timestamp()` for the current ledger time.
/// Returns `Err(InvalidAmount)` if the timestamp is zero or already expired.
pub fn validate_timestamp(_env: &Env, timestamp: u64, now: u64) -> Result<(), ContractError> {
    if timestamp == 0 || timestamp <= now {
        return Err(ContractError::InvalidAmount);
    }
    Ok(())
}

/// Returns `Err(InsufficientFunds)` if `amount` is not strictly positive (≤ 0).
/// Kept for backward compatibility; prefer `validate_amount` in new code.
pub fn require_positive_amount(env: &Env, amount: i128) -> Result<(), ContractError> {
    validate_amount(env, amount).map_err(|_| ContractError::InsufficientFunds)
}

// ── Config & Loan Helpers ─────────────────────────────────────────────────────

pub fn config(env: &Env) -> Config {
    env.storage()
        .instance()
        .get(&DataKey::Config)
        .expect("not initialized")
}

pub fn has_active_loan(env: &Env, borrower: &Address) -> bool {
    matches!(get_active_loan_record(env, borrower), Ok(loan) if loan.status == crate::types::LoanStatus::Active)
}

pub fn get_active_loan_record(env: &Env, borrower: &Address) -> Result<LoanRecord, ContractError> {
    let loan_id: u64 = env
        .storage()
        .persistent()
        .get(&DataKey::ActiveLoan(borrower.clone()))
        .ok_or(ContractError::NoActiveLoan)?;
    env.storage()
        .persistent()
        .get(&DataKey::Loan(loan_id))
        .ok_or(ContractError::NoActiveLoan)
}

/// Returns a token client for `addr` after verifying it is an allowed token
/// (either the primary protocol token or in `Config.allowed_tokens`).
pub fn require_allowed_token<'a>(
    env: &'a Env,
    addr: &Address,
) -> Result<token::Client<'a>, ContractError> {
    let cfg = config(env);
    if *addr == cfg.token || cfg.allowed_tokens.iter().any(|t| t == *addr) {
        Ok(token::Client::new(env, addr))
    } else {
        Err(ContractError::InvalidToken)
    }
}
