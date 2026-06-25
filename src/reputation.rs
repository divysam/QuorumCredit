#![allow(unused)]

use soroban_sdk::{contract, contractclient, contractimpl, contracttype, panic_with_error, Address, Env};
use crate::errors::ContractError;
use crate::types::{ReputationNFTRecord, DataKey};

#[contracttype]
pub enum RepKey {
    Minter,         // Address authorised to mint/burn (the main lending contract)
    Score(Address), // borrower → u32 reputation score
}

#[contractclient(name = "ReputationNftExternalClient")]
pub trait ReputationNftContractTrait {
    fn initialize(env: Env, minter: Address);
    fn mint(env: Env, to: Address);
    fn burn(env: Env, from: Address);
    fn balance(env: Env, addr: Address) -> u32;
}

/// Mint an excellent credit tier badge for a borrower.
/// Eligibility: credit score >= 850, >= 2 successful repayments, no defaults.
pub fn mint_excellent_badge(env: &Env, borrower: &Address) -> Result<(), ContractError> {
    // Check if already has badge
    if env
        .storage()
        .persistent()
        .has(&DataKey::ReputationNFTBadge(borrower.clone()))
    {
        return Ok(()); // Already minted
    }

    // Check eligibility: score >= 850
    let credit_score = env
        .storage()
        .persistent()
        .get::<DataKey, crate::types::CreditScore>(&DataKey::CreditScore(borrower.clone()))
        .ok_or(ContractError::InvalidAmount)?;

    if credit_score.score < 850 {
        return Err(ContractError::InvalidAmount);
    }

    // Check >= 2 successful repayments
    if credit_score.successful_repayments < 2 {
        return Err(ContractError::InvalidAmount);
    }

    // Check no defaults
    if credit_score.defaults > 0 {
        return Err(ContractError::InvalidAmount);
    }

    // Mint the badge
    let now = env.ledger().timestamp();
    let badge = ReputationNFTRecord {
        borrower: borrower.clone(),
        minted_at: now,
    };

    env.storage()
        .persistent()
        .set(&DataKey::ReputationNFTBadge(borrower.clone()), &badge);

    Ok(())
}

/// Burn an excellent credit tier badge on default.
pub fn burn_excellent_badge(env: &Env, borrower: &Address) {
    env.storage()
        .persistent()
        .remove(&DataKey::ReputationNFTBadge(borrower.clone()));
}

/// Check if a borrower has an excellent credit tier badge.
pub fn has_excellent_badge(env: &Env, borrower: &Address) -> bool {
    env.storage()
        .persistent()
        .has(&DataKey::ReputationNFTBadge(borrower.clone()))
}

/// Get the excellent badge record for a borrower.
pub fn get_excellent_badge(env: &Env, borrower: &Address) -> Option<ReputationNFTRecord> {
    env.storage()
        .persistent()
        .get(&DataKey::ReputationNFTBadge(borrower.clone()))
}

#[cfg(test)]
#[contract]
pub struct ReputationNftContract;

#[cfg(test)]
#[contractimpl]
impl ReputationNftContract {
    /// One-time setup: record the authorised minter (the lending contract).
    pub fn initialize(env: Env, minter: Address) {
        if env.storage().instance().has(&RepKey::Minter) {
            panic_with_error!(&env, ContractError::AlreadyInitialized);
        }
        env.storage().instance().set(&RepKey::Minter, &minter);
    }

    /// Mint one reputation point to `to`. Only callable by the registered minter.
    pub fn mint(env: Env, to: Address) {
        let minter: Address = env
            .storage()
            .instance()
            .get(&RepKey::Minter)
            .expect("not initialized");
        minter.require_auth();

        let score: u32 = env
            .storage()
            .persistent()
            .get(&RepKey::Score(to.clone()))
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&RepKey::Score(to), &(score + 1));
    }

    /// Burn one reputation point from `from` (floor at 0). Only callable by the registered minter.
    pub fn burn(env: Env, from: Address) {
        let minter: Address = env
            .storage()
            .instance()
            .get(&RepKey::Minter)
            .expect("not initialized");
        minter.require_auth();

        let score: u32 = env
            .storage()
            .persistent()
            .get(&RepKey::Score(from.clone()))
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&RepKey::Score(from), &score.saturating_sub(1));
    }

    /// Returns the reputation score for `addr`.
    pub fn balance(env: Env, addr: Address) -> u32 {
        env.storage()
            .persistent()
            .get(&RepKey::Score(addr))
            .unwrap_or(0)
    }
}
