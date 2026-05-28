#![no_std]

mod errors;
mod helpers;
mod oracle;
mod types;
mod vouch;

use soroban_sdk::{contract, contractimpl, symbol_short, token, Address, Env, String, Vec};

#[cfg(test)]
mod withdrawal_queue_test;

use crate::errors::ContractError;
use crate::helpers::{config, get_active_loan_record, has_active_loan, require_admin_approval, require_allowed_token, require_not_paused};
use crate::types::{
    Config, DataKey, LoanRecord, LoanStatus, QueuedWithdrawal, VouchRecord,
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
                allowed_purposes: Vec::new(&env),
                insurance_premium_bps: 0,
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

    /// #642: Vouch with an explicit sector label for diversification enforcement.
    pub fn vouch_with_sector(
        env: Env,
        voucher: Address,
        borrower: Address,
        stake: i128,
        token: Address,
        sector: String,
    ) -> Result<(), ContractError> {
        vouch::vouch_with_sector(env, voucher, borrower, stake, token, sector)
    }

    pub fn batch_vouch(
        env: Env,
        voucher: Address,
        borrowers: Vec<Address>,
        stakes: Vec<i128>,
        token: Address,
    ) -> Result<(), ContractError> {
        vouch::batch_vouch(env, voucher, borrowers, stakes, token)
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

    /// Decrease stake. If borrower has an active loan, queues the withdrawal.
    pub fn decrease_stake(
        env: Env,
        voucher: Address,
        borrower: Address,
        amount: i128,
    ) -> Result<(), ContractError> {
        vouch::decrease_stake(env, voucher, borrower, amount)
    }

    /// Fully withdraw a vouch. If borrower has an active loan, queues the withdrawal.
    pub fn withdraw_vouch(
        env: Env,
        voucher: Address,
        borrower: Address,
    ) -> Result<(), ContractError> {
        vouch::withdraw_vouch(env, voucher, borrower)
    }

    // ─────────────────────────────────────────────
    // Withdrawal Queue
    // ─────────────────────────────────────────────

    /// Queue a withdrawal during an active loan.
    /// Optionally pay a priority fee (stroops) to be processed before others.
    /// Queue is processed automatically when the loan is repaid or slashed.
    pub fn request_withdrawal(
        env: Env,
        voucher: Address,
        borrower: Address,
        priority_fee: i128,
    ) -> Result<(), ContractError> {
        vouch::request_withdrawal(env, voucher, borrower, priority_fee)
    }

    /// Partial withdrawal: withdraw up to 50% of stake during an active loan.
    /// A 10% penalty is applied to the withdrawn amount and distributed to remaining vouchers.
    pub fn partial_withdraw(
        env: Env,
        voucher: Address,
        borrower: Address,
    ) -> Result<(), ContractError> {
        vouch::partial_withdraw(env, voucher, borrower)
    }

    /// Get the pending withdrawal queue for a borrower.
    pub fn get_withdrawal_queue(env: Env, borrower: Address) -> Vec<QueuedWithdrawal> {
        vouch::get_withdrawal_queue(env, borrower)
    }

    // ─────────────────────────────────────────────
    // Loans (minimal — for test support)
    // ─────────────────────────────────────────────

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

        // #643: Validate loan_purpose against allowed_purposes whitelist (empty = all allowed)
        if !cfg.allowed_purposes.is_empty() {
            let purpose_allowed = cfg.allowed_purposes.iter().any(|p| p == loan_purpose);
            if !purpose_allowed {
                return Err(ContractError::LoanPurposeNotAllowed);
            }
        }

        // #642: Enforce sector diversification — no single sector may contribute > 50% of total stake
        if total_stake > 0 {
            let mut sector_names: Vec<soroban_sdk::String> = Vec::new(&env);
            let mut sector_amounts: Vec<i128> = Vec::new(&env);
            for v in vouches.iter() {
                if v.token != token_addr {
                    continue;
                }
                let mut found = false;
                for i in 0..sector_names.len() {
                    if sector_names.get(i).unwrap() == v.sector {
                        let cur = sector_amounts.get(i).unwrap();
                        sector_amounts.set(i, cur + v.stake);
                        found = true;
                        break;
                    }
                }
                if !found {
                    sector_names.push_back(v.sector.clone());
                    sector_amounts.push_back(v.stake);
                }
            }
            for i in 0..sector_amounts.len() {
                let s_stake = sector_amounts.get(i).unwrap();
                if s_stake * 2 > total_stake {
                    return Err(ContractError::SectorConcentrationTooHigh);
                }
            }
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

        let total_yield = amount * cfg.yield_bps / 10_000;

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

        // #644: Collect insurance premium from borrower if configured
        if cfg.insurance_premium_bps > 0 {
            let premium = amount * cfg.insurance_premium_bps / 10_000;
            if premium > 0 {
                token_client.transfer(&borrower, &env.current_contract_address(), &premium);
                let pool_balance: i128 = env
                    .storage()
                    .instance()
                    .get(&DataKey::InsurancePool)
                    .unwrap_or(0);
                env.storage()
                    .instance()
                    .set(&DataKey::InsurancePool, &(pool_balance + premium));
            }
        }

        token_client.transfer(&env.current_contract_address(), &borrower, &amount);

        env.events().publish(
            (symbol_short!("loan"), symbol_short!("created")),
            (borrower, amount),
        );

        Ok(())
    }

    pub fn repay(env: Env, borrower: Address, payment: i128) -> Result<(), ContractError> {
        borrower.require_auth();
        require_not_paused(&env)?;

        let mut loan = get_active_loan_record(&env, &borrower)?;

        if payment <= 0 {
            return Err(ContractError::InvalidAmount);
        }

        let total_owed = loan.amount + loan.total_yield;
        let outstanding = total_owed - loan.amount_repaid;

        if payment > outstanding {
            return Err(ContractError::InvalidAmount);
        }

        let token_client = require_allowed_token(&env, &loan.token_address)?;
        token_client.transfer(&borrower, &env.current_contract_address(), &payment);

        loan.amount_repaid += payment;

        if loan.amount_repaid >= total_owed {
            loan.status = LoanStatus::Repaid;
            loan.repayment_timestamp = Some(env.ledger().timestamp());

            let vouches: Vec<VouchRecord> = env
                .storage()
                .persistent()
                .get(&DataKey::Vouches(borrower.clone()))
                .unwrap_or(Vec::new(&env));

            let total_stake: i128 = vouches
                .iter()
                .filter(|v| v.token == loan.token_address)
                .map(|v| v.stake)
                .sum();

            for v in vouches.iter() {
                if v.token != loan.token_address {
                    continue;
                }
                let yield_share = if total_stake > 0 {
                    loan.total_yield * v.stake / total_stake
                } else {
                    0
                };
                token_client.transfer(
                    &env.current_contract_address(),
                    &v.voucher,
                    &(v.stake + yield_share),
                );
            }

            // Process any queued withdrawals now that the loan is closed
            vouch::process_withdrawal_queue(&env, &borrower);

            env.storage()
                .persistent()
                .remove(&DataKey::ActiveLoan(borrower.clone()));
            env.storage()
                .persistent()
                .remove(&DataKey::Vouches(borrower.clone()));

            env.events().publish(
                (symbol_short!("loan"), symbol_short!("repaid")),
                (borrower.clone(), loan.amount),
            );
        }

        env.storage()
            .persistent()
            .set(&DataKey::Loan(loan.id), &loan);

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

    pub fn vouch_exists(env: Env, voucher: Address, borrower: Address) -> bool {
        let vouches: Vec<VouchRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Vouches(borrower))
            .unwrap_or(Vec::new(&env));
        vouches.iter().any(|v| v.voucher == voucher)
    }

    // ─────────────────────────────────────────────
    // Admin: #643 Loan Purpose Whitelist
    // ─────────────────────────────────────────────

    /// #643: Set the allowed loan purposes whitelist. Empty = all purposes allowed.
    pub fn set_allowed_purposes(
        env: Env,
        admin_signers: Vec<Address>,
        purposes: Vec<String>,
    ) -> Result<(), ContractError> {
        require_admin_approval(&env, &admin_signers)?;
        let mut cfg = config(&env);
        cfg.allowed_purposes = purposes;
        env.storage().instance().set(&DataKey::Config, &cfg);
        Ok(())
    }

    // ─────────────────────────────────────────────
    // Admin: #644 Insurance Premium
    // ─────────────────────────────────────────────

    /// #644: Set the insurance premium in basis points (0 = disabled).
    pub fn set_insurance_premium_bps(
        env: Env,
        admin_signers: Vec<Address>,
        bps: i128,
    ) -> Result<(), ContractError> {
        require_admin_approval(&env, &admin_signers)?;
        if bps < 0 || bps > 10_000 {
            return Err(ContractError::InvalidAmount);
        }
        let mut cfg = config(&env);
        cfg.insurance_premium_bps = bps;
        env.storage().instance().set(&DataKey::Config, &cfg);
        Ok(())
    }

    /// #644: Get the current insurance pool balance.
    pub fn get_insurance_pool_balance(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::InsurancePool)
            .unwrap_or(0)
    }

    // ─────────────────────────────────────────────
    // #645: Loan Restructuring
    // ─────────────────────────────────────────────

    /// #645: Borrower requests a loan restructure (extend deadline / reduce outstanding amount).
    /// Vouchers must all approve before the restructure takes effect.
    pub fn restructure_loan(
        env: Env,
        borrower: Address,
        new_deadline: u64,
        new_amount: i128,
    ) -> Result<(), ContractError> {
        borrower.require_auth();
        require_not_paused(&env)?;

        let loan_id: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::ActiveLoan(borrower.clone()))
            .ok_or(ContractError::NoActiveLoan)?;
        let loan: LoanRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Loan(loan_id))
            .ok_or(ContractError::NoActiveLoan)?;

        if new_deadline <= loan.deadline {
            return Err(ContractError::InvalidAmount);
        }
        if new_amount != 0 {
            let outstanding = loan.amount + loan.total_yield - loan.amount_repaid;
            if new_amount <= 0 || new_amount > outstanding {
                return Err(ContractError::InvalidAmount);
            }
        }

        if env
            .storage()
            .persistent()
            .has(&DataKey::RestructureRequest(borrower.clone()))
        {
            return Err(ContractError::RestructureAlreadyPending);
        }

        let request = crate::types::RestructureRequest {
            borrower: borrower.clone(),
            new_deadline,
            new_amount,
            requested_at: env.ledger().timestamp(),
            approvals: Vec::new(&env),
        };

        env.storage()
            .persistent()
            .set(&DataKey::RestructureRequest(borrower.clone()), &request);

        env.events().publish(
            (symbol_short!("loan"), symbol_short!("restruct")),
            (borrower, new_deadline, new_amount),
        );

        Ok(())
    }

    /// #645: A voucher approves a pending restructure request.
    /// Once all vouchers approve, the loan is updated.
    pub fn approve_restructure(
        env: Env,
        voucher: Address,
        borrower: Address,
    ) -> Result<(), ContractError> {
        voucher.require_auth();
        require_not_paused(&env)?;

        let mut request: crate::types::RestructureRequest = env
            .storage()
            .persistent()
            .get(&DataKey::RestructureRequest(borrower.clone()))
            .ok_or(ContractError::RestructureRequestNotFound)?;

        let vouches: Vec<VouchRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Vouches(borrower.clone()))
            .unwrap_or(Vec::new(&env));

        if !vouches.iter().any(|v| v.voucher == voucher) {
            return Err(ContractError::UnauthorizedCaller);
        }

        if request.approvals.iter().any(|a| a == voucher) {
            return Err(ContractError::AlreadyVoted);
        }

        request.approvals.push_back(voucher.clone());

        let all_approved = vouches
            .iter()
            .all(|v| request.approvals.iter().any(|a| a == v.voucher));

        if all_approved {
            let loan_id: u64 = env
                .storage()
                .persistent()
                .get(&DataKey::ActiveLoan(borrower.clone()))
                .ok_or(ContractError::NoActiveLoan)?;
            let mut loan: LoanRecord = env
                .storage()
                .persistent()
                .get(&DataKey::Loan(loan_id))
                .ok_or(ContractError::NoActiveLoan)?;

            loan.deadline = request.new_deadline;
            if request.new_amount > 0 {
                let outstanding = loan.amount + loan.total_yield - loan.amount_repaid;
                let forgiven = outstanding - request.new_amount;
                if forgiven > 0 && forgiven <= loan.total_yield {
                    loan.total_yield -= forgiven;
                }
            }

            env.storage()
                .persistent()
                .set(&DataKey::Loan(loan.id), &loan);
            env.storage()
                .persistent()
                .remove(&DataKey::RestructureRequest(borrower.clone()));

            env.events().publish(
                (symbol_short!("loan"), symbol_short!("rst_done")),
                (borrower, loan.deadline),
            );
        } else {
            env.storage()
                .persistent()
                .set(&DataKey::RestructureRequest(borrower.clone()), &request);
        }

        Ok(())
    }

    /// #645: Get the pending restructure request for a borrower, if any.
    pub fn get_restructure_request(
        env: Env,
        borrower: Address,
    ) -> Option<crate::types::RestructureRequest> {
        env.storage()
            .persistent()
            .get(&DataKey::RestructureRequest(borrower))
    }
}
