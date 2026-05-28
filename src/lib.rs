#![no_std]

mod errors;
mod fraud_detection;
mod helpers;
mod liquidity_mining;
mod oracle;
mod staking_derivatives;
mod types;
mod vouch;
mod vouch_snapshot;

use soroban_sdk::{contract, contractimpl, symbol_short, Address, Env, String, Vec};

#[cfg(test)]
mod withdrawal_queue_test;
#[cfg(test)]
mod credential_test;

use crate::errors::ContractError;
use crate::helpers::{
    acquire_lock, config, get_active_loan_record, has_active_loan, release_lock,
    require_allowed_token, require_not_paused, validate_address, validate_amount,
};
use crate::types::{
    Config, CredentialRecord, CredentialStatus, DataKey, LoanRecord, LoanStatus, QueuedWithdrawal,
    VouchRecord, DEFAULT_LIQUIDITY_MINING_RATE_BPS, DEFAULT_LOAN_DURATION,
    DEFAULT_MAX_LOAN_TO_STAKE_RATIO, DEFAULT_MAX_VOUCHERS, DEFAULT_MIN_LOAN_AMOUNT,
    DEFAULT_MIN_VOUCH_AGE_SECS, DEFAULT_SLASH_BPS, DEFAULT_YIELD_BPS,
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
                liquidity_mining_rate_bps: DEFAULT_LIQUIDITY_MINING_RATE_BPS,
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
        acquire_lock(&env)?;
        let result = vouch::vouch(env.clone(), voucher, borrower, stake, token);
        release_lock(&env);
        result
    }

    pub fn batch_vouch(
        env: Env,
        voucher: Address,
        borrowers: Vec<Address>,
        stakes: Vec<i128>,
        token: Address,
    ) -> Result<(), ContractError> {
        acquire_lock(&env)?;
        let result = vouch::batch_vouch(env.clone(), voucher, borrowers, stakes, token);
        release_lock(&env);
        result
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
        acquire_lock(&env)?;
        let result = vouch::increase_stake(env.clone(), voucher, borrower, additional);
        release_lock(&env);
        result
    }

    /// Decrease stake. If borrower has an active loan, queues the withdrawal.
    pub fn decrease_stake(
        env: Env,
        voucher: Address,
        borrower: Address,
        amount: i128,
    ) -> Result<(), ContractError> {
        acquire_lock(&env)?;
        let result = vouch::decrease_stake(env.clone(), voucher, borrower, amount);
        release_lock(&env);
        result
    }

    /// Fully withdraw a vouch. If borrower has an active loan, queues the withdrawal.
    pub fn withdraw_vouch(
        env: Env,
        voucher: Address,
        borrower: Address,
    ) -> Result<(), ContractError> {
        acquire_lock(&env)?;
        let result = vouch::withdraw_vouch(env.clone(), voucher, borrower);
        release_lock(&env);
        result
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
        acquire_lock(&env)?;
        let result = vouch::request_withdrawal(env.clone(), voucher, borrower, priority_fee);
        release_lock(&env);
        result
    }

    /// Partial withdrawal: withdraw up to 50% of stake during an active loan.
    /// A 10% penalty is applied to the withdrawn amount and distributed to remaining vouchers.
    pub fn partial_withdraw(
        env: Env,
        voucher: Address,
        borrower: Address,
    ) -> Result<(), ContractError> {
        acquire_lock(&env)?;
        let result = vouch::partial_withdraw(env.clone(), voucher, borrower);
        release_lock(&env);
        result
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
        acquire_lock(&env)?;

        if has_active_loan(&env, &borrower) {
            release_lock(&env);
            return Err(ContractError::ActiveLoanExists);
        }

        let token_client = require_allowed_token(&env, &token_addr)?;
        let cfg = config(&env);

        if amount < cfg.min_loan_amount {
            release_lock(&env);
            return Err(ContractError::LoanBelowMinAmount);
        }

        if amount <= 0 {
            release_lock(&env);
            return Err(ContractError::InvalidAmount);
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
            release_lock(&env);
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

        token_client.transfer(&env.current_contract_address(), &borrower, &amount);

        env.events().publish(
            (symbol_short!("loan"), symbol_short!("created")),
            (borrower, amount),
        );

        release_lock(&env);
        Ok(())
    }

    pub fn repay(env: Env, borrower: Address, payment: i128) -> Result<(), ContractError> {
        borrower.require_auth();
        require_not_paused(&env)?;
        acquire_lock(&env)?;

        let mut loan = match get_active_loan_record(&env, &borrower) {
            Ok(l) => l,
            Err(e) => { release_lock(&env); return Err(e); }
        };

        if let Err(e) = validate_amount(&env, payment) {
            release_lock(&env);
            return Err(e);
        }

        let total_owed = loan.amount + loan.total_yield;
        let outstanding = total_owed - loan.amount_repaid;

        if payment > outstanding {
            release_lock(&env);
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

        release_lock(&env);
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
    // Credentials
    // ─────────────────────────────────────────────

    /// Issue a credential to a holder. Only callable by an admin.
    ///
    /// # Arguments
    /// * `issuer` - Admin address issuing the credential (must sign)
    /// * `holder` - Address receiving the credential
    /// * `credential_type` - Human-readable type string (e.g. "KYC")
    /// * `expiry_timestamp` - Optional expiry; `None` means no expiry
    ///
    /// # Errors
    /// * `InvalidAmount` if `credential_type` is empty
    /// * `ZeroAddress` if `holder` is invalid
    /// * `Reentrancy` if called re-entrantly
    pub fn issue_credential(
        env: Env,
        issuer: Address,
        holder: Address,
        credential_type: String,
        expiry_timestamp: Option<u64>,
    ) -> Result<u64, ContractError> {
        issuer.require_auth();
        acquire_lock(&env)?;

        // Validate inputs
        validate_address(&env, &holder)?;
        if credential_type.len() == 0 {
            release_lock(&env);
            return Err(ContractError::InvalidAmount);
        }

        // Verify issuer is an admin
        let cfg = config(&env);
        if !cfg.admins.iter().any(|a| a == issuer) {
            release_lock(&env);
            return Err(ContractError::UnauthorizedCaller);
        }

        let now = env.ledger().timestamp();
        let id: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::CredentialCounter)
            .unwrap_or(0u64)
            + 1;
        env.storage()
            .persistent()
            .set(&DataKey::CredentialCounter, &id);

        let record = CredentialRecord {
            id,
            holder: holder.clone(),
            attestor: issuer,
            credential_type,
            expiry_timestamp,
            issued_at: now,
            status: CredentialStatus::Active,
        };

        let mut creds: Vec<CredentialRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Credentials(holder.clone()))
            .unwrap_or(Vec::new(&env));
        creds.push_back(record);
        env.storage()
            .persistent()
            .set(&DataKey::Credentials(holder.clone()), &creds);

        env.events().publish(
            (symbol_short!("cred"), symbol_short!("issued")),
            (holder, id),
        );

        release_lock(&env);
        Ok(id)
    }

    /// Permanently revoke a credential. Revocation is irreversible.
    ///
    /// # Arguments
    /// * `admin` - Admin address (must sign)
    /// * `holder` - Credential holder address
    /// * `credential_id` - ID of the credential to revoke
    ///
    /// # Errors
    /// * `CredentialNotFound` if no matching credential exists
    /// * `CredentialAlreadyRevoked` if already revoked
    /// * `UnauthorizedCaller` if caller is not an admin
    pub fn revoke_credential(
        env: Env,
        admin: Address,
        holder: Address,
        credential_id: u64,
    ) -> Result<(), ContractError> {
        admin.require_auth();
        acquire_lock(&env)?;

        let cfg = config(&env);
        if !cfg.admins.iter().any(|a| a == admin) {
            release_lock(&env);
            return Err(ContractError::UnauthorizedCaller);
        }

        let creds: Vec<CredentialRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Credentials(holder.clone()))
            .unwrap_or(Vec::new(&env));

        let mut found = false;
        let mut updated = Vec::new(&env);
        for mut c in creds.iter() {
            if c.id == credential_id {
                found = true;
                if c.status == CredentialStatus::Revoked {
                    release_lock(&env);
                    return Err(ContractError::CredentialAlreadyRevoked);
                }
                c.status = CredentialStatus::Revoked;
            }
            updated.push_back(c);
        }

        if !found {
            release_lock(&env);
            return Err(ContractError::CredentialNotFound);
        }

        env.storage()
            .persistent()
            .set(&DataKey::Credentials(holder.clone()), &updated);

        env.events().publish(
            (symbol_short!("cred"), symbol_short!("revoked")),
            (holder, credential_id),
        );

        release_lock(&env);
        Ok(())
    }

    /// Suspend a credential (temporary; can be re-activated).
    /// A revoked credential cannot be suspended.
    ///
    /// # Arguments
    /// * `admin` - Admin address (must sign)
    /// * `holder` - Credential holder address
    /// * `credential_id` - ID of the credential to suspend
    ///
    /// # Errors
    /// * `CredentialNotFound` if no matching credential exists
    /// * `CredentialAlreadyRevoked` if already permanently revoked
    /// * `CredentialStatusUnchanged` if already suspended
    /// * `UnauthorizedCaller` if caller is not an admin
    pub fn suspend_credential(
        env: Env,
        admin: Address,
        holder: Address,
        credential_id: u64,
    ) -> Result<(), ContractError> {
        admin.require_auth();
        acquire_lock(&env)?;

        let cfg = config(&env);
        if !cfg.admins.iter().any(|a| a == admin) {
            release_lock(&env);
            return Err(ContractError::UnauthorizedCaller);
        }

        let creds: Vec<CredentialRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Credentials(holder.clone()))
            .unwrap_or(Vec::new(&env));

        let mut found = false;
        let mut updated = Vec::new(&env);
        for mut c in creds.iter() {
            if c.id == credential_id {
                found = true;
                match c.status {
                    CredentialStatus::Revoked => {
                        release_lock(&env);
                        return Err(ContractError::CredentialAlreadyRevoked);
                    }
                    CredentialStatus::Suspended => {
                        release_lock(&env);
                        return Err(ContractError::CredentialStatusUnchanged);
                    }
                    CredentialStatus::Active => {
                        c.status = CredentialStatus::Suspended;
                    }
                }
            }
            updated.push_back(c);
        }

        if !found {
            release_lock(&env);
            return Err(ContractError::CredentialNotFound);
        }

        env.storage()
            .persistent()
            .set(&DataKey::Credentials(holder.clone()), &updated);

        env.events().publish(
            (symbol_short!("cred"), symbol_short!("suspend")),
            (holder, credential_id),
        );

        release_lock(&env);
        Ok(())
    }

    /// Re-activate a suspended credential.
    ///
    /// # Arguments
    /// * `admin` - Admin address (must sign)
    /// * `holder` - Credential holder address
    /// * `credential_id` - ID of the credential to activate
    ///
    /// # Errors
    /// * `CredentialNotFound` if no matching credential exists
    /// * `CredentialAlreadyRevoked` if permanently revoked
    /// * `CredentialStatusUnchanged` if already active
    /// * `UnauthorizedCaller` if caller is not an admin
    pub fn activate_credential(
        env: Env,
        admin: Address,
        holder: Address,
        credential_id: u64,
    ) -> Result<(), ContractError> {
        admin.require_auth();
        acquire_lock(&env)?;

        let cfg = config(&env);
        if !cfg.admins.iter().any(|a| a == admin) {
            release_lock(&env);
            return Err(ContractError::UnauthorizedCaller);
        }

        let creds: Vec<CredentialRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Credentials(holder.clone()))
            .unwrap_or(Vec::new(&env));

        let mut found = false;
        let mut updated = Vec::new(&env);
        for mut c in creds.iter() {
            if c.id == credential_id {
                found = true;
                match c.status {
                    CredentialStatus::Revoked => {
                        release_lock(&env);
                        return Err(ContractError::CredentialAlreadyRevoked);
                    }
                    CredentialStatus::Active => {
                        release_lock(&env);
                        return Err(ContractError::CredentialStatusUnchanged);
                    }
                    CredentialStatus::Suspended => {
                        c.status = CredentialStatus::Active;
                    }
                }
            }
            updated.push_back(c);
        }

        if !found {
            release_lock(&env);
            return Err(ContractError::CredentialNotFound);
        }

        env.storage()
            .persistent()
            .set(&DataKey::Credentials(holder.clone()), &updated);

        release_lock(&env);
        Ok(())
    }

    /// Get all credentials for a holder (for the credential viewer).
    /// Returns all credentials regardless of status so the holder can see
    /// verification status, expiry, and attestors.
    pub fn get_credentials(env: Env, holder: Address) -> Vec<CredentialRecord> {
        env.storage()
            .persistent()
            .get(&DataKey::Credentials(holder))
            .unwrap_or(Vec::new(&env))
    }

    /// Export credentials as a structured list (JSON-serializable via SDK).
    /// Filters to only Active credentials that have not expired.
    /// This is the "export credentials" function for the credential viewer.
    pub fn export_credentials(env: Env, holder: Address) -> Vec<CredentialRecord> {
        let now = env.ledger().timestamp();
        let creds: Vec<CredentialRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Credentials(holder))
            .unwrap_or(Vec::new(&env));

        let mut result = Vec::new(&env);
        for c in creds.iter() {
            let expired = c
                .expiry_timestamp
                .map(|exp| exp <= now)
                .unwrap_or(false);
            if c.status == CredentialStatus::Active && !expired {
                result.push_back(c);
            }
        }
        result
    }
}
