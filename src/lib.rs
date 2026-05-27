#![no_std]

mod errors;
mod helpers;
mod oracle;
mod types;
mod vouch;

use soroban_sdk::{contract, contractimpl, symbol_short, Address, Env, String, Vec};

#[cfg(test)]
mod oracle_credit_score_test;

use crate::errors::ContractError;
use crate::helpers::{config, get_active_loan_record, has_active_loan, require_allowed_token, require_not_paused};
use crate::types::{
    Config, DataKey, ExternalCreditScore, LoanRecord, LoanStatus, VouchRecord,
    DEFAULT_LOAN_DURATION, DEFAULT_MAX_LOAN_TO_STAKE_RATIO, DEFAULT_MAX_VOUCHERS,
    DEFAULT_MIN_LOAN_AMOUNT, DEFAULT_MIN_VOUCH_AGE_SECS, DEFAULT_SLASH_BPS, DEFAULT_YIELD_BPS,
};

#[contract]
pub struct QuorumCreditContract;

#[contractimpl]
impl QuorumCreditContract {
    // ─────────────────────────────────────────────
    // Initialization
    // ─────────────────────────────────────────────

    pub fn initialize(
        env: Env,
        deployer: Address,
        admins: Vec<Address>,
        admin_threshold: u32,
        token: Address,
    ) -> Result<(), ContractError> {
        deployer.require_auth();

        if env.storage().instance().has(&DataKey::Config) {
            return Err(ContractError::AlreadyInitialized);
        }

        if admins.is_empty() || admin_threshold == 0 || admin_threshold > admins.len() {
            return Err(ContractError::InvalidAmount);
        }

        env.storage().instance().set(&DataKey::Deployer, &deployer);
        env.storage().instance().set(
            &DataKey::Config,
            &Config {
                admins,
                admin_threshold,
                token,
                allowed_tokens: Vec::new(&env),
                yield_bps: DEFAULT_YIELD_BPS,
                slash_bps: DEFAULT_SLASH_BPS,
                max_vouchers: DEFAULT_MAX_VOUCHERS,
                min_loan_amount: DEFAULT_MIN_LOAN_AMOUNT,
                loan_duration: DEFAULT_LOAN_DURATION,
                max_loan_to_stake_ratio: DEFAULT_MAX_LOAN_TO_STAKE_RATIO,
                grace_period: 0,
                min_vouch_age_secs: DEFAULT_MIN_VOUCH_AGE_SECS,
                prepayment_penalty_bps: 0,
            },
        );

        Ok(())
    }

    // ─────────────────────────────────────────────
    // Core Vouching
    // ─────────────────────────────────────────────

    pub fn vouch(
        env: Env,
        voucher: Address,
        borrower: Address,
        stake: i128,
        token: Address,
    ) -> Result<(), ContractError> {
        vouch::vouch(env, voucher, borrower, stake, token)
    }

    // ─────────────────────────────────────────────
    // Stake Management
    // ─────────────────────────────────────────────

    pub fn increase_stake(
        env: Env,
        voucher: Address,
        borrower: Address,
        additional: i128,
    ) -> Result<(), ContractError> {
        vouch::increase_stake(env, voucher, borrower, additional)
    }

    // ─────────────────────────────────────────────
    // Oracle Credit Score
    // ─────────────────────────────────────────────

    /// Register (or update) the trusted oracle contract address. Admin-only.
    pub fn set_oracle(
        env: Env,
        admin_signers: Vec<Address>,
        oracle: Address,
    ) -> Result<(), ContractError> {
        oracle::set_oracle(env, admin_signers, oracle)
    }

    /// Called by the registered oracle to push an external credit score for a borrower.
    /// `score` must be in range 0–1000.
    pub fn update_credit_score_from_oracle(
        env: Env,
        oracle: Address,
        borrower: Address,
        score: u32,
    ) -> Result<(), ContractError> {
        oracle::update_credit_score_from_oracle(env, oracle, borrower, score)
    }

    /// Returns the stored external credit score for a borrower, or `None` if not set.
    pub fn get_external_credit_score(
        env: Env,
        borrower: Address,
    ) -> Option<ExternalCreditScore> {
        oracle::get_external_credit_score(env, borrower)
    }

    // ─────────────────────────────────────────────
    // Loans
    // ─────────────────────────────────────────────

    /// Request a loan. Yield rate is adjusted by the borrower's oracle credit score.
    pub fn request_loan(
        env: Env,
        borrower: Address,
        amount: i128,
        threshold: i128,
        loan_purpose: String,
        token_addr: Address,
    ) -> Result<(), ContractError> {
        borrower.require_auth();
        require_not_paused(&env)?;

        if has_active_loan(&env, &borrower) {
            return Err(ContractError::ActiveLoanExists);
        }

        let token_client = require_allowed_token(&env, &token_addr)?;
        let cfg = config(&env);

        if amount < cfg.min_loan_amount {
            return Err(ContractError::LoanBelowMinAmount);
        }

        let vouches: Vec<VouchRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Vouches(borrower.clone()))
            .unwrap_or(Vec::new(&env));

        let total_stake: i128 = vouches
            .iter()
            .filter(|v| v.token == token_addr)
            .map(|v| v.stake)
            .sum();

        if total_stake < threshold {
            return Err(ContractError::InsufficientFunds);
        }

        let now = env.ledger().timestamp();
        let loan_id: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::LoanCounter)
            .unwrap_or(0u64)
            + 1;
        env.storage()
            .persistent()
            .set(&DataKey::LoanCounter, &loan_id);

        // Apply oracle credit score adjustment to yield rate
        let score_adjustment = oracle::credit_score_yield_adjustment(&env, &borrower);
        let effective_yield_bps = (cfg.yield_bps + score_adjustment).max(0);
        let total_yield = amount * effective_yield_bps / 10_000;

        let loan = LoanRecord {
            id: loan_id,
            borrower: borrower.clone(),
            co_borrowers: Vec::new(&env),
            amount,
            amount_repaid: 0,
            total_yield,
            status: LoanStatus::Active,
            created_at: now,
            disbursement_timestamp: now,
            repayment_timestamp: None,
            deadline: now + cfg.loan_duration,
            loan_purpose,
            token_address: token_addr.clone(),
            amortization_schedule: Vec::new(&env),
            reminder_sent: false,
            risk_score: 0,
        };

        env.storage()
            .persistent()
            .set(&DataKey::Loan(loan_id), &loan);
        env.storage()
            .persistent()
            .set(&DataKey::ActiveLoan(borrower.clone()), &loan_id);

        token_client.transfer(&env.current_contract_address(), &borrower, &amount);

        env.events().publish(
            (symbol_short!("loan"), symbol_short!("created")),
            (borrower, amount),
        );

        Ok(())
    }

    pub fn get_loan(env: Env, borrower: Address) -> Option<LoanRecord> {
        let loan_id: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::ActiveLoan(borrower.clone()))?;
        env.storage().persistent().get(&DataKey::Loan(loan_id))
    }

    pub fn get_vouches(env: Env, borrower: Address) -> Vec<VouchRecord> {
        env.storage()
            .persistent()
            .get(&DataKey::Vouches(borrower))
            .unwrap_or(Vec::new(&env))
    }
}
