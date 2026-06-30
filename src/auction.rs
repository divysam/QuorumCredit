//! Cross-Chain Auction for Defaulted Loans (#974, #91)
//!
//! This module enables auctioning defaulted loan collateral across chains.
//! When a loan defaults, its collateral (vouch stakes) can be auctioned to
//! bidders on any supported chain via bridge attestations.
//!
//! ## Auction Flow
//!
//! 1. **Auction Initialization**: When a loan defaults, an auction is created
//!    with the collateral as the auction item.
//! 2. **Cross-Chain Bidding**: Bidders on any chain can place bids via
//!    bridge-signed attestations.
//! 3. **Bid Verification**: The contract verifies bridge signatures and
//!    consumes nonces to prevent replay attacks.
//! 4. **Auction Settlement**: When the auction ends, the highest bid wins
//!    and collateral is transferred to the winner.
//! 5. **Cross-Chain Settlement**: The auction result is mirrored to all
//!    chains via bridge attestations.

use crate::{errors::ContractError, helpers::config, types::DataKey};
use soroban_sdk::{contracttype, symbol_short, Address, BytesN, Env, Vec};

/// Auction duration in seconds (7 days by default).
pub const DEFAULT_AUCTION_DURATION_SECS: u64 = 7 * 24 * 60 * 60;
/// Minimum bid increment in basis points (500 = 5%).
pub const MIN_BID_INCREMENT_BPS: u32 = 500;
/// Minimum starting bid as percentage of collateral value (1000 = 10%).
pub const MIN_START_BID_BPS: u32 = 1_000;

/// Auction status.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuctionStatus {
    /// Auction is active and accepting bids.
    Active,
    /// Auction has ended and winner is being determined.
    Ended,
    /// Auction has been settled and collateral transferred.
    Settled,
    /// Auction was cancelled (e.g., loan repaid during auction).
    Cancelled,
}

/// A bid placed in an auction.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuctionBid {
    /// The bidder's address.
    pub bidder: Address,
    /// Bid amount in stroops.
    pub amount: i128,
    /// Chain ID where the bid originated.
    pub chain_id: u32,
    /// Bridge nonce for replay protection.
    pub nonce: u64,
    /// Timestamp when the bid was placed.
    pub timestamp: u64,
    /// Bridge signature attesting to the bid.
    pub signature: BytesN<64>,
}

/// An auction for defaulted loan collateral.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Auction {
    /// Unique auction ID.
    pub auction_id: u64,
    /// The loan ID being auctioned.
    pub loan_id: u64,
    /// The borrower whose collateral is being auctioned.
    pub borrower: Address,
    /// Total collateral value (sum of vouch stakes) in stroops.
    pub collateral_value: i128,
    /// Token address for the collateral.
    pub collateral_token: Address,
    /// Current highest bid.
    pub current_highest_bid: Option<AuctionBid>,
    /// Auction status.
    pub status: AuctionStatus,
    /// Timestamp when auction was created.
    pub created_at: u64,
    /// Timestamp when auction ends.
    pub ends_at: u64,
    /// Chain ID where auction was created.
    pub origin_chain: u32,
}

/// Cross-chain auction attestation payload.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuctionAttestationPayload {
    /// The contract address.
    pub contract: Address,
    /// Auction ID.
    pub auction_id: u64,
    /// Bid details.
    pub bid: AuctionBid,
    /// Timestamp.
    pub timestamp: u64,
}

/// Storage keys for auction data.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
enum AuctionKey {
    /// Auction ID counter.
    AuctionCounter,
    /// Auction by ID.
    Auction(u64),
    /// Auction ID for a loan.
    AuctionForLoan(u64),
    /// All active auction IDs.
    ActiveAuctions,
    /// Used nonces for auction bids.
    UsedAuctionNonce(u32, u64),
}

/// Create a new auction for a defaulted loan.
pub fn create_auction(
    env: Env,
    loan_id: u64,
    borrower: Address,
    collateral_value: i128,
    collateral_token: Address,
    origin_chain: u32,
) -> Result<u64, ContractError> {
    // Check if auction already exists for this loan
    if env
        .storage()
        .persistent()
        .has(&AuctionKey::AuctionForLoan(loan_id))
    {
        return Err(ContractError::InvalidStateTransition);
    }

    let cfg = config(&env);
    let now = env.ledger().timestamp();
    
    // Get auction ID counter
    let auction_id: u64 = env
        .storage()
        .persistent()
        .get(&AuctionKey::AuctionCounter)
        .unwrap_or(0);
    let new_auction_id = auction_id + 1;

    // Calculate minimum starting bid
    let min_start_bid = collateral_value * MIN_START_BID_BPS as i128 / 10_000;

    let auction = Auction {
        auction_id: new_auction_id,
        loan_id,
        borrower: borrower.clone(),
        collateral_value,
        collateral_token: collateral_token.clone(),
        current_highest_bid: None,
        status: AuctionStatus::Active,
        created_at: now,
        ends_at: now + DEFAULT_AUCTION_DURATION_SECS,
        origin_chain,
    };

    // Store auction
    env.storage()
        .persistent()
        .set(&AuctionKey::Auction(new_auction_id), &auction);
    env.storage()
        .persistent()
        .set(&AuctionKey::AuctionForLoan(loan_id), &new_auction_id);
    env.storage()
        .persistent()
        .set(&AuctionKey::AuctionCounter, &new_auction_id);

    // Add to active auctions list
    let mut active_auctions: Vec<u64> = env
        .storage()
        .persistent()
        .get(&AuctionKey::ActiveAuctions)
        .unwrap_or(Vec::new(&env));
    active_auctions.push_back(new_auction_id);
    env.storage()
        .persistent()
        .set(&AuctionKey::ActiveAuctions, &active_auctions);

    env.events().publish(
        (symbol_short!("auction"), symbol_short!("created")),
        (new_auction_id, loan_id, borrower, collateral_value),
    );

    Ok(new_auction_id)
}

/// Place a bid on an auction with bridge attestation.
pub fn place_bid(
    env: Env,
    auction_id: u64,
    bid: AuctionBid,
    bridge_public_key: BytesN<32>,
) -> Result<(), ContractError> {
    // Get auction
    let mut auction: Auction = env
        .storage()
        .persistent()
        .get(&AuctionKey::Auction(auction_id))
        .ok_or(ContractError::ProposalNotFound)?;

    // Validate auction status
    if auction.status != AuctionStatus::Active {
        return Err(ContractError::InvalidStateTransition);
    }

    // Check if auction has ended
    let now = env.ledger().timestamp();
    if now >= auction.ends_at {
        auction.status = AuctionStatus::Ended;
        env.storage()
            .persistent()
            .set(&AuctionKey::Auction(auction_id), &auction);
        return Err(ContractError::VotingPeriodEnded);
    }

    // Verify bridge attestation
    verify_auction_bid_attestation(&env, &auction, &bid, &bridge_public_key)?;

    // Check minimum bid increment
    let min_bid = if let Some(ref highest) = auction.current_highest_bid {
        let increment = highest.amount * MIN_BID_INCREMENT_BPS as i128 / 10_000;
        highest.amount + increment
    } else {
        auction.collateral_value * MIN_START_BID_BPS as i128 / 10_000
    };

    if bid.amount < min_bid {
        return Err(ContractError::InsufficientFunds);
    }

    // Check for replay attack
    let nonce_key = AuctionKey::UsedAuctionNonce(bid.chain_id, bid.nonce);
    if env.storage().persistent().has(&nonce_key) {
        return Err(ContractError::ReplayAttackDetected);
    }

    // Mark nonce as used
    env.storage()
        .persistent()
        .set(&nonce_key, &true);

    // Update auction with new highest bid
    auction.current_highest_bid = Some(bid.clone());
    env.storage()
        .persistent()
        .set(&AuctionKey::Auction(auction_id), &auction);

    env.events().publish(
        (symbol_short!("auction"), symbol_short!("bid")),
        (auction_id, bid.bidder, bid.amount, bid.chain_id),
    );

    Ok(())
}

/// Verify bridge attestation for an auction bid.
fn verify_auction_bid_attestation(
    env: &Env,
    auction: &Auction,
    bid: &AuctionBid,
    bridge_public_key: &BytesN<32>,
) -> Result<(), ContractError> {
    // Check timestamp freshness
    let now = env.ledger().timestamp();
    const MAX_ATTESTATION_AGE_SECS: u64 = 60 * 60; // 1 hour
    const MAX_FUTURE_SKEW_SECS: u64 = 60; // 1 minute

    if bid.timestamp > now.saturating_add(MAX_FUTURE_SKEW_SECS) {
        return Err(ContractError::AttestationFromFuture);
    }
    if now.saturating_sub(bid.timestamp) > MAX_ATTESTATION_AGE_SECS {
        return Err(ContractError::AttestationExpired);
    }

    // Create attestation payload
    let payload = AuctionAttestationPayload {
        contract: env.current_contract_address(),
        auction_id: auction.auction_id,
        bid: bid.clone(),
        timestamp: bid.timestamp,
    };

    // Verify signature
    let message = payload.to_xdr(env);
    env.crypto()
        .ed25519_verify(bridge_public_key, &message, &bid.signature);

    Ok(())
}

/// Settle an auction and transfer collateral to the winner.
pub fn settle_auction(env: Env, auction_id: u64) -> Result<(), ContractError> {
    let mut auction: Auction = env
        .storage()
        .persistent()
        .get(&AuctionKey::Auction(auction_id))
        .ok_or(ContractError::ProposalNotFound)?;

    // Check if auction can be settled
    if auction.status != AuctionStatus::Active && auction.status != AuctionStatus::Ended {
        return Err(ContractError::InvalidStateTransition);
    }

    // Check if auction has ended
    let now = env.ledger().timestamp();
    if now < auction.ends_at && auction.status == AuctionStatus::Active {
        return Err(ContractError::TimelockDelayNotElapsed);
    }

    auction.status = AuctionStatus::Ended;

    // Get highest bid
    let highest_bid = auction
        .current_highest_bid
        .ok_or(ContractError::InsufficientFunds)?;

    // Transfer collateral to winner (this would integrate with token transfer)
    // For now, we'll mark as settled
    auction.status = AuctionStatus::Settled;
    env.storage()
        .persistent()
        .set(&AuctionKey::Auction(auction_id), &auction);

    // Remove from active auctions
    let mut active_auctions: Vec<u64> = env
        .storage()
        .persistent()
        .get(&AuctionKey::ActiveAuctions)
        .unwrap_or(Vec::new(&env));
    if let Some(pos) = active_auctions.iter().position(|&id| id == auction_id) {
        active_auctions.remove(pos as u32);
        env.storage()
            .persistent()
            .set(&AuctionKey::ActiveAuctions, &active_auctions);
    }

    env.events().publish(
        (symbol_short!("auction"), symbol_short!("settled")),
        (auction_id, highest_bid.bidder, highest_bid.amount),
    );

    Ok(())
}

/// Cancel an auction (e.g., if loan is repaid).
pub fn cancel_auction(env: Env, auction_id: u64) -> Result<(), ContractError> {
    let mut auction: Auction = env
        .storage()
        .persistent()
        .get(&AuctionKey::Auction(auction_id))
        .ok_or(ContractError::ProposalNotFound)?;

    if auction.status != AuctionStatus::Active {
        return Err(ContractError::InvalidStateTransition);
    }

    auction.status = AuctionStatus::Cancelled;
    env.storage()
        .persistent()
        .set(&AuctionKey::Auction(auction_id), &auction);

    // Remove from active auctions
    let mut active_auctions: Vec<u64> = env
        .storage()
        .persistent()
        .get(&AuctionKey::ActiveAuctions)
        .unwrap_or(Vec::new(&env));
    if let Some(pos) = active_auctions.iter().position(|&id| id == auction_id) {
        active_auctions.remove(pos as u32);
        env.storage()
            .persistent()
            .set(&AuctionKey::ActiveAuctions, &active_auctions);
    }

    env.events().publish(
        (symbol_short!("auction"), symbol_short!("cancelled")),
        auction_id,
    );

    Ok(())
}

/// Get auction by ID.
pub fn get_auction(env: Env, auction_id: u64) -> Option<Auction> {
    env.storage().persistent().get(&AuctionKey::Auction(auction_id))
}

/// Get auction ID for a loan.
pub fn get_auction_for_loan(env: Env, loan_id: u64) -> Option<u64> {
    env.storage()
        .persistent()
        .get(&AuctionKey::AuctionForLoan(loan_id))
}

/// Get all active auction IDs.
pub fn get_active_auctions(env: Env) -> Vec<u64> {
    env.storage()
        .persistent()
        .get(&AuctionKey::ActiveAuctions)
        .unwrap_or(Vec::new(&env))
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{contract, testutils::Address as _, Address};

    #[contract]
    struct TestContract;

    #[test]
    fn test_create_auction() {
        let env = Env::default();
        env.ledger().set_timestamp(10_000);
        let contract = env.register(TestContract, ());

        let borrower = Address::generate(&env);
        let collateral_token = Address::generate(&env);

        let auction_id = env.as_contract(&contract, || {
            create_auction(
                env.clone(),
                1,
                borrower.clone(),
                1_000_000,
                collateral_token.clone(),
                1,
            )
        })
        .unwrap();

        assert_eq!(auction_id, 1);

        let auction = env.as_contract(&contract, || get_auction(env.clone(), auction_id));
        assert!(auction.is_some());
        let auction = auction.unwrap();
        assert_eq!(auction.loan_id, 1);
        assert_eq!(auction.borrower, borrower);
        assert_eq!(auction.collateral_value, 1_000_000);
        assert_eq!(auction.status, AuctionStatus::Active);
    }

    #[test]
    fn test_duplicate_auction_for_loan() {
        let env = Env::default();
        let contract = env.register(TestContract, ());

        let borrower = Address::generate(&env);
        let collateral_token = Address::generate(&env);

        env.as_contract(&contract, || {
            create_auction(
                env.clone(),
                1,
                borrower.clone(),
                1_000_000,
                collateral_token.clone(),
                1,
            )
        })
        .unwrap();

        let result = env.as_contract(&contract, || {
            create_auction(
                env.clone(),
                1,
                borrower,
                1_000_000,
                collateral_token,
                1,
            )
        });

        assert_eq!(result, Err(ContractError::InvalidStateTransition));
    }

    #[test]
    fn test_cancel_auction() {
        let env = Env::default();
        let contract = env.register(TestContract, ());

        let borrower = Address::generate(&env);
        let collateral_token = Address::generate(&env);

        let auction_id = env.as_contract(&contract, || {
            create_auction(
                env.clone(),
                1,
                borrower.clone(),
                1_000_000,
                collateral_token.clone(),
                1,
            )
        })
        .unwrap();

        env.as_contract(&contract, || {
            cancel_auction(env.clone(), auction_id)
        })
        .unwrap();

        let auction = env.as_contract(&contract, || get_auction(env.clone(), auction_id));
        assert!(auction.is_some());
        assert_eq!(auction.unwrap().status, AuctionStatus::Cancelled);
    }

    #[test]
    fn test_get_active_auctions() {
        let env = Env::default();
        let contract = env.register(TestContract, ());

        let borrower = Address::generate(&env);
        let collateral_token = Address::generate(&env);

        env.as_contract(&contract, || {
            create_auction(
                env.clone(),
                1,
                borrower.clone(),
                1_000_000,
                collateral_token.clone(),
                1,
            )
        })
        .unwrap();

        env.as_contract(&contract, || {
            create_auction(
                env.clone(),
                2,
                borrower,
                1_000_000,
                collateral_token,
                1,
            )
        })
        .unwrap();

        let active = env.as_contract(&contract, || get_active_auctions(env.clone()));
        assert_eq!(active.len(), 2);
    }

    #[test]
    fn test_get_auction_for_loan() {
        let env = Env::default();
        let contract = env.register(TestContract, ());

        let borrower = Address::generate(&env);
        let collateral_token = Address::generate(&env);

        env.as_contract(&contract, || {
            create_auction(
                env.clone(),
                1,
                borrower.clone(),
                1_000_000,
                collateral_token.clone(),
                1,
            )
        })
        .unwrap();

        let auction_id = env.as_contract(&contract, || {
            get_auction_for_loan(env.clone(), 1)
        });
        assert_eq!(auction_id, Some(1));
    }
}
