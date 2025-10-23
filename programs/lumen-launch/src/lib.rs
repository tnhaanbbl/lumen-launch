#![allow(deprecated)]
use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount, Mint, Burn, MintTo, Transfer};
use anchor_spl::associated_token::AssociatedToken;

declare_id!("DdHjSxotiVveS9reai5KvdBFC9xd5HPUeDwPp88LZ98Z");

#[error_code]
pub enum LaunchError {
    #[msg("Amount zero")] ZeroAmount,
    #[msg("Launch ended")] Ended,
    #[msg("Max wallet 0.1%")] MaxWallet,
    #[msg("Max tx 0.1%")] MaxTx,
    #[msg("Not failed")] NotFailed,
    #[msg("Overflow")] Overflow,
    #[msg("Slippage")] Slippage,
    #[msg("Deadline")] Deadline,
    #[msg("Not ended")] NotEnded,
    #[msg("Too early")] TooEarly,
    #[msg("Zero holding")] ZeroHolding,
    #[msg("Zero entitled")] ZeroEntitled,
    #[msg("Min 10 USDC for virtual liquidity")] Min10USDC,
    #[msg("First 20 s: max buy 0.1% of supply")] SnipeSize,
    #[msg("First 20 s: max slippage 0.5%")] SnipeSlippage,
    #[msg("First 20 s: time restriction active")] SnipeTime,
    #[msg("Migration not allowed")] MigrationNotAllowed,
    #[msg("Still locked")] StillLocked,
    #[msg("Never paid")] NeverPaid,
    #[msg("Not your ledger")] NotYourLedger,
    #[msg("Below minimum refund threshold")] BelowMinRefund,
    #[msg("Above auto threshold")] AboveAutoThreshold,
    #[msg("Reentrancy detected")] Reentrancy,
}

#[event]
pub struct BuyEvent {
    pub buyer: Pubkey,
    pub usdc_in: u64,
    pub tokens_out: u64,
    pub burned: u64,
}

#[event]
pub struct SellEvent {
    pub seller: Pubkey,
    pub tokens_in: u64,
    pub usdc_out: u64,
    pub tax: u64,
}

#[event]
pub struct ClaimEvent {
    pub user: Pubkey,
    pub amount: u64,
}

#[event]
pub struct LaunchFailedEvent {
    pub launch: Pubkey,
    pub total_raised: u64,
}

#[event]
pub struct LaunchSucceededEvent {
    pub launch: Pubkey,
    pub total_raised: u64,
}

#[event]
pub struct MigratedToAMMEvent {
    pub launch: Pubkey,
    pub token_amount: u64,
    pub usdc_amount: u64,
}

#[event]
pub struct PlatformWithdrawnEvent {
    pub launch: Pubkey,
    pub amount: u64,
}

#[account]
#[derive(InitSpace)]
pub struct LaunchConfig {
    pub total_raised: u64,
    pub closed: bool,
    pub failed: bool,
    pub creator: Pubkey,
    pub platform_wallet: Pubkey,
    pub start_time: i64,
    pub deadline: i64,
    pub bump: u8,
    pub usdc_decimals: u8,
    pub virtual_token: u64,
    pub virtual_usdc: u64,
    pub k: u128, // use u128 to avoid overflow
    pub anti_snipe_blocks: u8,
    pub snipe_max_pct: u16,   // in 1/10,000 of supply (e.g., 10 => 0.1%)
    pub snipe_slippage: u16,  // unused currently
    pub auto_withdraw_threshold: u64,
    pub total_supply: u64,
    pub migrated: bool,
    pub creator_paid_usdc: u64,
    pub platform_fees_collected: u64,
    pub platform_auto_transferred: u64,
    pub last_withdraw: i64,      // cool-down 24h
    pub in_trade: bool,          // nonReentrant
    // reserves and holders accumulator
    pub creator_reserve_usdc: u64,
    pub holders_reserve_usdc: u64,
    pub holders_index: u128, // cumulative USDC per token scaled
}

#[account]
#[derive(InitSpace)]
pub struct BuyerLedger {
    pub buyer: Pubkey,
    pub last_claim: i64,
    pub bump: u8,
    pub paid_usdc: u64,
    pub last_index_claimed: u128,
}

#[account]
#[derive(InitSpace)]
pub struct LPLock {
    pub amm_id: Pubkey,
    pub lp_mint: Pubkey,
    pub vault_ata: Pubkey,
    pub unlock_timestamp: i64,
    pub migration_allowed: bool,
    pub migration_target: Pubkey,
    pub authority: Pubkey,
    pub bump: u8,
}

const LOCK_DURATION: i64 = 5 * 365 * 24 * 60 * 60;
const USDC_DEVNET: Pubkey = pubkey!("4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU");
const PLATFORM_WALLET_PUBKEY: Pubkey = pubkey!("7XB2PEWYd5be12CpJ9e4ZTZTHCgNrcTbL7HigciPd1C6");
const PLATFORM_FEE_USDC: u64 = 5_000_000;          // 5 USDC (6 decimals)
const BURN_BUY_PCT: u8 = 1;                        // 1%
const MIN_VIRTUAL_USDC: u64 = 10_000_000;          // 10 USDC
const PLATFORM_AUTO_TRANSFER_THRESHOLD: u64 = 1_000_000_000; // 1000 USDC
const ACC_SCALE: u128 = 1_000_000_000_000; // 1e12 scaling for holder index

fn bonding_curve_buy(usdc_in: u64, virtual_usdc: u64, virtual_token: u64, _decimals: u8) -> u64 {
    // simple constant-product k = virtual_usdc * virtual_token
    let new_usdc = virtual_usdc.saturating_add(usdc_in);
    let new_token = (virtual_token as u128)
        .saturating_mul(virtual_usdc as u128)
        .checked_div(new_usdc as u128)
        .unwrap_or(0) as u64;
    virtual_token.saturating_sub(new_token)
}

fn bonding_curve_sell(token_in: u64, virtual_usdc: u64, virtual_token: u64, _decimals: u8) -> u64 {
    let new_token = virtual_token.saturating_add(token_in);
    let new_usdc = (virtual_usdc as u128)
        .saturating_mul(virtual_token as u128)
        .checked_div(new_token as u128)
        .unwrap_or(0) as u64;
    virtual_usdc.saturating_sub(new_usdc)
}

#[derive(PartialEq)]
pub enum CallerType {
    Platform,
    Creator,
    Holder,
}

fn classify_caller(
    caller: &Pubkey,
    launch_config: &LaunchConfig,
    _ledger: Option<&BuyerLedger>,
) -> Result<CallerType> {
    if *caller == PLATFORM_WALLET_PUBKEY {
        return Ok(CallerType::Platform);
    }
    if *caller == launch_config.creator {
        return Ok(CallerType::Creator);
    }
    Ok(CallerType::Holder)
}

#[program]
pub mod lumen_launch {
    use super::*;

    pub fn create_token(
        ctx: Context<CreateToken>,
        _usdc_mint: Pubkey,
        virtual_usdc_amount: u64,
    ) -> Result<()> {
        require!(virtual_usdc_amount >= MIN_VIRTUAL_USDC, LaunchError::Min10USDC);

        // 1) Collect fixed platform fee to platform USDC ATA
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.creator_usdc.to_account_info(),
                    to: ctx.accounts.platform_usdc_ata.to_account_info(),
                    authority: ctx.accounts.creator.to_account_info(),
                },
            ),
            PLATFORM_FEE_USDC,
        )?;

        // 2) Seed USDC vault with virtual liquidity (kept in vault)
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.creator_usdc.to_account_info(),
                    to: ctx.accounts.usdc_vault.to_account_info(),
                    authority: ctx.accounts.creator.to_account_info(),
                },
            ),
            virtual_usdc_amount,
        )?;

        let config = &mut ctx.accounts.launch_config;
        let decimals = ctx.accounts.usdc_mint.decimals; // USDC decimals
        let supply = 1_000_000_000u64
            .checked_mul(10u64.pow(decimals as u32))
            .ok_or(LaunchError::Overflow)?;

        config.total_supply = supply;
        config.virtual_usdc = virtual_usdc_amount;
        config.virtual_token = supply / 2;
        config.k = (config.virtual_usdc as u128) * (config.virtual_token as u128);
        config.start_time = Clock::get()?.unix_timestamp;
        config.deadline = config.start_time + 72 * 60 * 60;
        config.anti_snipe_blocks = 2;
        config.snipe_max_pct = 10; // 0.1% of supply as default (in 1/10,000 => 10)
        config.snipe_slippage = 50;
        config.platform_wallet = PLATFORM_WALLET_PUBKEY;
        config.creator = ctx.accounts.creator.key();
        config.bump = ctx.bumps.launch_config;
        config.usdc_decimals = decimals;
        config.closed = false;
        config.failed = false;
        config.migrated = false;
        config.creator_paid_usdc = virtual_usdc_amount;
        config.platform_fees_collected = PLATFORM_FEE_USDC;
        config.platform_auto_transferred = 0;
        config.last_withdraw = 0;
        config.in_trade = false;
        config.auto_withdraw_threshold = PLATFORM_AUTO_TRANSFER_THRESHOLD;
        config.creator_reserve_usdc = 0;
        config.holders_reserve_usdc = 0;
        config.holders_index = 0;

        Ok(())
    }

    pub fn buy(
        ctx: Context<Buy>,
        usdc_amount: u64,
        min_tokens_out: u64,
        deadline: i64,
    ) -> Result<()> {
        require!(usdc_amount > 0, LaunchError::ZeroAmount);
        require!(Clock::get()?.unix_timestamp <= deadline, LaunchError::Deadline);

        let config = &mut ctx.accounts.launch_config;
        require!(!config.closed, LaunchError::Ended);
        require!(!config.in_trade, LaunchError::Reentrancy); // reentrancy guard
        config.in_trade = true;

        let decimals = config.usdc_decimals;

        // calculate tokens out based on bonding curve
        let tokens_out = bonding_curve_buy(usdc_amount, config.virtual_usdc, config.virtual_token, decimals);
        require!(tokens_out >= min_tokens_out, LaunchError::Slippage);

        // anti-snipe: first 20s max 0.1% of supply
        let time_passed = Clock::get()?.unix_timestamp - config.start_time;
        if time_passed <= 20 {
            // snipe_max_pct in 1/10,000 of supply; enforce tokens_out <= snipe_max_pct/10000 * total_supply
            let max_tokens = (config.total_supply as u128)
                .saturating_mul(config.snipe_max_pct as u128)
                .checked_div(10_000)
                .unwrap_or(0) as u64;
            require!(tokens_out <= max_tokens, LaunchError::SnipeSize);
        }

        // burn on buy
        let burn_amount = tokens_out.saturating_mul(BURN_BUY_PCT as u64) / 100;
        let user_tokens = tokens_out.saturating_sub(burn_amount);

        // transfer USDC into the USDC vault
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.buyer_usdc.to_account_info(),
                    to: ctx.accounts.usdc_vault.to_account_info(),
                    authority: ctx.accounts.buyer.to_account_info(),
                },
            ),
            usdc_amount,
        )?;

        // mint to buyer
        token::mint_to(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                MintTo {
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.buyer_x_ata.to_account_info(),
                    authority: ctx.accounts.mint_auth.to_account_info(),
                },
                &[&[b"mint-auth", ctx.accounts.mint.key().as_ref(), &[ctx.bumps.mint_auth]]],
            ),
            user_tokens,
        )?;

        // mint and burn for fee
        if burn_amount > 0 {
            token::mint_to(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    MintTo {
                        mint: ctx.accounts.mint.to_account_info(),
                        to: ctx.accounts.burn_ata.to_account_info(),
                        authority: ctx.accounts.mint_auth.to_account_info(),
                    },
                    &[&[b"mint-auth", ctx.accounts.mint.key().as_ref(), &[ctx.bumps.mint_auth]]],
                ),
                burn_amount,
            )?;
            token::burn(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Burn {
                        mint: ctx.accounts.mint.to_account_info(),
                        from: ctx.accounts.burn_ata.to_account_info(),
                        authority: ctx.accounts.burn_auth.to_account_info(),
                    },
                    &[&[b"mint-auth", ctx.accounts.mint.key().as_ref(), &[ctx.bumps.mint_auth]]],
                ),
                burn_amount,
            )?;
        }

        // update curve state
        config.virtual_usdc = config.virtual_usdc.saturating_add(usdc_amount);
        config.virtual_token = config.virtual_token.saturating_sub(tokens_out);
        config.k = (config.virtual_usdc as u128) * (config.virtual_token as u128);
        config.total_raised = config.total_raised.saturating_add(usdc_amount);

        // ensure buyer ledger exists and update paid_usdc
        let ledger = &mut ctx.accounts.buyer_ledger;
        ledger.paid_usdc = ledger.paid_usdc.saturating_add(usdc_amount);

        emit!(BuyEvent {
            buyer: ctx.accounts.buyer.key(),
            usdc_in: usdc_amount,
            tokens_out: user_tokens,
            burned: burn_amount,
        });

        // finalize if after deadline
        let now = Clock::get()?.unix_timestamp;
        if now > config.deadline && !config.closed {
            let total_raised = config.total_raised;
            if total_raised < 5_000_000_000 { // < 5,000 USDC
                config.failed = true;
                config.closed = true;
                emit!(LaunchFailedEvent {
                    launch: config.key(),
                    total_raised,
                });
                // refund creator virtual funds from vault if available
                let creator_refund = config.creator_paid_usdc;
                let available = ctx.accounts.usdc_vault.amount;
                let refund = core::cmp::min(creator_refund, available);
                if refund > 0 {
                    token::transfer(
                        CpiContext::new_with_signer(
                            ctx.accounts.token_program.to_account_info(),
                            Transfer {
                                from: ctx.accounts.usdc_vault.to_account_info(),
                                to: ctx.accounts.creator_usdc_ata.to_account_info(),
                                authority: ctx.accounts.usdc_vault_auth.to_account_info(),
                            },
                            &[&[b"usdc-vault", ctx.accounts.mint.key().as_ref(), &[ctx.bumps.usdc_vault_auth]]],
                        ),
                        refund,
                    )?;
                    config.creator_paid_usdc = config.creator_paid_usdc.saturating_sub(refund);
                }
            } else {
                config.failed = false;
                config.closed = true;
                emit!(LaunchSucceededEvent {
                    launch: config.key(),
                    total_raised,
                });
            }
        }

        config.in_trade = false;
        Ok(())
    }

    pub fn sell(
        ctx: Context<Sell>,
        token_amount: u64,
        min_usdc_out: u64,
        deadline: i64,
    ) -> Result<()> {
        require!(token_amount > 0, LaunchError::ZeroAmount);
        require!(Clock::get()?.unix_timestamp <= deadline, LaunchError::Deadline);

        let config = &mut ctx.accounts.launch_config;
        require!(!config.closed, LaunchError::Ended);
        require!(!config.in_trade, LaunchError::Reentrancy);
        config.in_trade = true;

        let decimals = config.usdc_decimals;
        let usdc_out = bonding_curve_sell(token_amount, config.virtual_usdc, config.virtual_token, decimals);
        require!(usdc_out >= min_usdc_out, LaunchError::Slippage);

        // burn seller tokens
        token::burn(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Burn {
                    mint: ctx.accounts.mint.to_account_info(),
                    from: ctx.accounts.seller_x_ata.to_account_info(),
                    authority: ctx.accounts.seller.to_account_info(),
                },
            ),
            token_amount,
        )?;

        // dynamic tax tiers by trade size vs total supply
        let total_supply = config.total_supply;
        let sell_percentage = (token_amount as u128)
            .saturating_mul(100)
            .checked_div(total_supply as u128)
            .unwrap_or(0) as u64;
        let tax_rate = match sell_percentage {
            0..=30 => 5,      // 0.5 %
            31..=70 => 10,    // 1 %
            _ => 30,          // 3 %
        };
        let tax = (usdc_out as u128)
            .saturating_mul(tax_rate as u128)
            .checked_div(1000)
            .unwrap_or(0) as u64;

        let platform_share = (tax as u128).saturating_mul(20).checked_div(100).unwrap_or(0) as u64;
        let creator_share = (tax as u128).saturating_mul(30).checked_div(100).unwrap_or(0) as u64;
        let holders_share = tax.saturating_sub(platform_share).saturating_sub(creator_share);

        // Update reserves and platform accounting
        config.creator_reserve_usdc = config.creator_reserve_usdc.saturating_add(creator_share);
        config.holders_reserve_usdc = config.holders_reserve_usdc.saturating_add(holders_share);
        config.platform_fees_collected = config.platform_fees_collected.saturating_add(platform_share);

        // Auto transfer platform share if threshold reached and liquidity allows
        let available_for_platform = config.platform_fees_collected.saturating_sub(config.platform_auto_transferred);
        if available_for_platform >= config.auto_withdraw_threshold {
            let reserved = config.holders_reserve_usdc.saturating_add(config.creator_reserve_usdc);
            let vault_bal = ctx.accounts.usdc_vault.amount;
            let free_liquidity = vault_bal.saturating_sub(reserved);
            let amount = core::cmp::min(available_for_platform, free_liquidity);
            if amount > 0 {
                token::transfer(
                    CpiContext::new_with_signer(
                        ctx.accounts.token_program.to_account_info(),
                        Transfer {
                            from: ctx.accounts.usdc_vault.to_account_info(),
                            to: ctx.accounts.platform_profit_ata.to_account_info(),
                            authority: ctx.accounts.usdc_vault_auth.to_account_info(),
                        },
                        &[&[b"usdc-vault", ctx.accounts.mint.key().as_ref(), &[ctx.bumps.usdc_vault_auth]]],
                    ),
                    amount,
                )?;
                config.platform_auto_transferred = config.platform_auto_transferred.saturating_add(amount);
                config.virtual_usdc = config.virtual_usdc.saturating_sub(amount);
                config.k = (config.virtual_usdc as u128) * (config.virtual_token as u128);
            }
        }

        // compute user payout after tax
        let user_usdc = usdc_out.saturating_sub(tax);

        // pay user from vault
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.usdc_vault.to_account_info(),
                    to: ctx.accounts.seller_usdc_ata.to_account_info(),
                    authority: ctx.accounts.usdc_vault_auth.to_account_info(),
                },
                &[&[b"usdc-vault", ctx.accounts.mint.key().as_ref(), &[ctx.bumps.usdc_vault_auth]]],
            ),
            user_usdc,
        )?;

        // update curve state
        config.virtual_usdc = config.virtual_usdc.saturating_sub(usdc_out);
        config.virtual_token = config.virtual_token.saturating_add(token_amount);
        config.k = (config.virtual_usdc as u128) * (config.virtual_token as u128);

        // update holders accumulator (distribute holders_share to current holders)
        let new_supply = ctx.accounts.mint.supply; // post-burn supply
        if new_supply > 0 && holders_share > 0 {
            let inc = (holders_share as u128)
                .saturating_mul(ACC_SCALE)
                .checked_div(new_supply as u128)
                .unwrap_or(0);
            config.holders_index = config.holders_index.saturating_add(inc);
        }

        emit!(SellEvent {
            seller: ctx.accounts.seller.key(),
            tokens_in: token_amount,
            usdc_out: user_usdc,
            tax: tax,
        });

        config.in_trade = false;
        Ok(())
    }

    pub fn claim_profits(ctx: Context<ClaimProfits>) -> Result<()> {
        let user_balance = ctx.accounts.user_x_ata.amount; // current token holdings
        require!(user_balance > 0, LaunchError::ZeroHolding);

        let config = &mut ctx.accounts.launch_config;
        let ledger = &mut ctx.accounts.buyer_ledger;

        // compute claimable using accumulator (bounded by reserves and free liquidity)
        let delta_index = config.holders_index.saturating_sub(ledger.last_index_claimed);
        let theoretical = (user_balance as u128)
            .saturating_mul(delta_index)
            .checked_div(ACC_SCALE)
            .unwrap_or(0) as u64;
        let reserved_holders = config.holders_reserve_usdc;
        let reserved_platform = config.platform_fees_collected.saturating_sub(config.platform_auto_transferred);
        let reserved_creator = config.creator_reserve_usdc;
        let vault_bal = ctx.accounts.usdc_vault.amount;
        let free_liquidity = vault_bal
            .saturating_sub(reserved_platform)
            .saturating_sub(reserved_creator);
        let claim_amount = core::cmp::min(theoretical, core::cmp::min(reserved_holders, free_liquidity));
        require!(claim_amount > 0, LaunchError::ZeroEntitled);

        // update ledger and reserves
        ledger.last_claim = Clock::get()?.unix_timestamp;
        ledger.last_index_claimed = config.holders_index;
        config.holders_reserve_usdc = config.holders_reserve_usdc.saturating_sub(claim_amount);

        // pay user
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.usdc_vault.to_account_info(),
                    to: ctx.accounts.user_usdc_ata.to_account_info(),
                    authority: ctx.accounts.usdc_vault_auth.to_account_info(),
                },
                &[&[b"usdc-vault", ctx.accounts.mint.key().as_ref(), &[ctx.bumps.usdc_vault_auth]]],
            ),
            claim_amount,
        )?;
        config.virtual_usdc = config.virtual_usdc.saturating_sub(claim_amount);
        config.k = (config.virtual_usdc as u128) * (config.virtual_token as u128);

        emit!(ClaimEvent { user: ctx.accounts.user.key(), amount: claim_amount });
        Ok(())
    }

    pub fn reclaim_virtual_funds(ctx: Context<ReclaimVirtualFunds>) -> Result<()> {
        let caller = ctx.accounts.creator.key();
        let config = &mut ctx.accounts.launch_config;

        let caller_type = classify_caller(&caller, config, None)?;
        require!(caller_type == CallerType::Creator, LaunchError::NotYourLedger);
        require!(config.closed || config.migrated, LaunchError::NotEnded);

        let creator_refund = config.creator_paid_usdc;
        if creator_refund > 0 {
            let available = ctx.accounts.usdc_vault.amount;
            let refund = core::cmp::min(creator_refund, available);
            if refund > 0 {
                token::transfer(
                    CpiContext::new_with_signer(
                        ctx.accounts.token_program.to_account_info(),
                        Transfer {
                            from: ctx.accounts.usdc_vault.to_account_info(),
                            to: ctx.accounts.creator_usdc_ata.to_account_info(),
                            authority: ctx.accounts.usdc_vault_auth.to_account_info(),
                        },
                        &[&[b"usdc-vault", ctx.accounts.mint.key().as_ref(), &[ctx.bumps.usdc_vault_auth]]],
                    ),
                    refund,
                )?;
                config.creator_paid_usdc = config.creator_paid_usdc.saturating_sub(refund);
            }
        }
        Ok(())
    }

    pub fn close_and_migrate_to_raydium(
        ctx: Context<CloseAndMigrateToRaydium>,
    ) -> Result<()> {
        let creator_refund = ctx.accounts.launch_config.creator_paid_usdc;

        // Read immutable state and prepare seeds before taking a mutable borrow
        require!(ctx.accounts.launch_config.closed, LaunchError::NotEnded);
        require!(!ctx.accounts.launch_config.migrated, LaunchError::MigrationNotAllowed);

        // split balances (50%)
        let token_half = ctx.accounts.bonding_curve_ata.amount / 2;
        let usdc_half = ctx.accounts.usdc_vault.amount / 2;

        // Seeds and authority for launch_config PDA
        let lc_bump = ctx.accounts.launch_config.bump;
        let mint_key = ctx.accounts.mint.key();
        let lc_ai = ctx.accounts.launch_config.to_account_info();
        let lc_bump_arr = [lc_bump];
        let lc_seeds: [&[u8]; 3] = [b"launch", mint_key.as_ref(), &lc_bump_arr];

        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.bonding_curve_ata.to_account_info(),
                    to: ctx.accounts.coin_vault.to_account_info(),
                    authority: lc_ai,
                },
                &[&lc_seeds],
            ),
            token_half,
        )?;

        // move USDC from vault
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.usdc_vault.to_account_info(),
                    to: ctx.accounts.pc_vault.to_account_info(),
                    authority: ctx.accounts.usdc_vault_auth.to_account_info(),
                },
                &[&[b"usdc-vault", ctx.accounts.mint.key().as_ref(), &[ctx.bumps.usdc_vault_auth]]],
            ),
            usdc_half,
        )?;

        // refund creator virtual funds from vault if available
        if creator_refund > 0 {
            let available = ctx.accounts.usdc_vault.amount;
            let refund = core::cmp::min(creator_refund, available);
            if refund > 0 {
                token::transfer(
                    CpiContext::new_with_signer(
                        ctx.accounts.token_program.to_account_info(),
                        Transfer {
                            from: ctx.accounts.usdc_vault.to_account_info(),
                            to: ctx.accounts.creator_usdc_ata.to_account_info(),
                            authority: ctx.accounts.usdc_vault_auth.to_account_info(),
                        },
                        &[&[b"usdc-vault", ctx.accounts.mint.key().as_ref(), &[ctx.bumps.usdc_vault_auth]]],
                    ),
                    refund,
                )?;
                let cfg = &mut ctx.accounts.launch_config;
                cfg.creator_paid_usdc = cfg.creator_paid_usdc.saturating_sub(refund);
            }
        }

        let lock = &mut ctx.accounts.lp_lock;
        lock.amm_id = ctx.accounts.amm_id.key();
        lock.lp_mint = ctx.accounts.lp_mint.key();
        lock.vault_ata = ctx.accounts.lp_lock_vault.key();
        lock.unlock_timestamp = Clock::get()?.unix_timestamp + LOCK_DURATION;
        lock.migration_allowed = false;
        lock.migration_target = Pubkey::default();
        lock.authority = ctx.accounts.payer.key();
        lock.bump = ctx.bumps.lp_lock;

        let config = &mut ctx.accounts.launch_config;
        config.migrated = true;

        emit!(MigratedToAMMEvent {
            launch: ctx.accounts.launch_config.key(),
            token_amount: token_half,
            usdc_amount: usdc_half,
        });

        Ok(())
    }

    pub fn migrate_lp(ctx: Context<MigrateLP>) -> Result<()> {
        let lock = &mut ctx.accounts.lock;
        require!(lock.migration_allowed, LaunchError::MigrationNotAllowed);
        require!(Clock::get()?.unix_timestamp >= lock.unlock_timestamp, LaunchError::StillLocked);
        require!(ctx.accounts.authority.key() == lock.authority, LaunchError::NotYourLedger);

        let amount = ctx.accounts.old_vault.amount;
        let lock_bump = ctx.bumps.lp_lock_auth;
        let vault_seeds: &[&[u8]; 3] = &[b"lp-lock", lock.amm_id.as_ref(), &[lock_bump]];
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.old_vault.to_account_info(),
                    to: ctx.accounts.new_vault.to_account_info(),
                    authority: ctx.accounts.lp_lock_auth.to_account_info(),
                },
                &[vault_seeds],
            ),
            amount,
        )?;

        lock.amm_id = ctx.accounts.new_amm.key();
        lock.vault_ata = ctx.accounts.new_vault.key();
        lock.migration_target = ctx.accounts.new_amm.key();
        lock.migration_allowed = false;

        Ok(())
    }

    pub fn withdraw_platform_remaining(ctx: Context<WithdrawPlatformRemaining>) -> Result<()> {
        let config = &mut ctx.accounts.launch_config;
        let now = Clock::get()?.unix_timestamp;
        require!(now - config.last_withdraw >= 24 * 60 * 60, LaunchError::TooEarly); // 24h cool-down
        let available = config.platform_fees_collected.saturating_sub(config.platform_auto_transferred);
        require!(available > 0, LaunchError::ZeroEntitled);
        require!(available < config.auto_withdraw_threshold, LaunchError::AboveAutoThreshold);

        let reserved = config.holders_reserve_usdc.saturating_add(config.creator_reserve_usdc);
        let vault_bal = ctx.accounts.usdc_vault.amount;
        let free_liquidity = vault_bal.saturating_sub(reserved);
        let amount = core::cmp::min(available, free_liquidity);
        require!(amount > 0, LaunchError::ZeroEntitled);

        config.platform_auto_transferred = config.platform_auto_transferred.saturating_add(amount);
        config.last_withdraw = now;

        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.usdc_vault.to_account_info(),
                    to: ctx.accounts.platform_usdc_ata.to_account_info(),
                    authority: ctx.accounts.usdc_vault_auth.to_account_info(),
                },
                &[&[b"usdc-vault", ctx.accounts.mint.key().as_ref(), &[ctx.bumps.usdc_vault_auth]]],
            ),
            amount,
        )?;
        config.virtual_usdc = config.virtual_usdc.saturating_sub(amount);
        config.k = (config.virtual_usdc as u128) * (config.virtual_token as u128);

        emit!(PlatformWithdrawnEvent { launch: ctx.accounts.launch_config.key(), amount: amount });
        Ok(())
    }
pub fn finalize(ctx: Context<Finalize>) -> Result<()> {
        let config = &mut ctx.accounts.launch_config;
        require!(!config.closed, LaunchError::NotEnded);
        let now = Clock::get()?.unix_timestamp;
        require!(now > config.deadline, LaunchError::TooEarly);

        let total_raised = config.total_raised;
        if total_raised < 5_000_000_000 {
            config.failed = true;
            config.closed = true;
            emit!(LaunchFailedEvent { launch: config.key(), total_raised });
            let creator_refund = config.creator_paid_usdc;
            let available = ctx.accounts.usdc_vault.amount;
            let refund = core::cmp::min(creator_refund, available);
            if refund > 0 {
                token::transfer(
                    CpiContext::new_with_signer(
                        ctx.accounts.token_program.to_account_info(),
                        Transfer {
                            from: ctx.accounts.usdc_vault.to_account_info(),
                            to: ctx.accounts.creator_usdc_ata.to_account_info(),
                            authority: ctx.accounts.usdc_vault_auth.to_account_info(),
                        },
                        &[&[b"usdc-vault", ctx.accounts.mint.key().as_ref(), &[ctx.bumps.usdc_vault_auth]]],
                    ),
                    refund,
                )?;
                config.creator_paid_usdc = config.creator_paid_usdc.saturating_sub(refund);
                config.virtual_usdc = config.virtual_usdc.saturating_sub(refund);
                config.k = (config.virtual_usdc as u128) * (config.virtual_token as u128);
            }
        } else {
            config.failed = false;
            config.closed = true;
            emit!(LaunchSucceededEvent { launch: config.key(), total_raised });
        }
        Ok(())
    }

    pub fn withdraw_creator_reserve(ctx: Context<WithdrawCreatorReserve>, requested: u64) -> Result<()> {
        let cfg = &mut ctx.accounts.launch_config;
        let reserved = cfg.creator_reserve_usdc;
        require!(reserved > 0, LaunchError::ZeroEntitled);
        let platform_reserved = cfg.platform_fees_collected.saturating_sub(cfg.platform_auto_transferred);
        let holders_reserved = cfg.holders_reserve_usdc;
        let vault_bal = ctx.accounts.usdc_vault.amount;
        let free_liquidity = vault_bal
            .saturating_sub(platform_reserved)
            .saturating_sub(holders_reserved);
        let amount = core::cmp::min(requested, core::cmp::min(reserved, free_liquidity));
        require!(amount > 0, LaunchError::ZeroEntitled);

        cfg.creator_reserve_usdc = cfg.creator_reserve_usdc.saturating_sub(amount);

        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.usdc_vault.to_account_info(),
                    to: ctx.accounts.creator_usdc_ata.to_account_info(),
                    authority: ctx.accounts.usdc_vault_auth.to_account_info(),
                },
                &[&[b"usdc-vault", ctx.accounts.mint.key().as_ref(), &[ctx.bumps.usdc_vault_auth]]],
            ),
            amount,
        )?;
        cfg.virtual_usdc = cfg.virtual_usdc.saturating_sub(amount);
        cfg.k = (cfg.virtual_usdc as u128) * (cfg.virtual_token as u128);
        Ok(())
    }
}

// ------------------- Accounts -------------------

#[derive(Accounts)]
pub struct CreateToken<'info> {
    #[account(mut)]
    pub creator: Signer<'info>,

    #[account(
        init,
        payer = creator,
        mint::decimals = 6,
        mint::authority = mint_auth,
    )]
    pub mint: Account<'info, Mint>,

    /// CHECK: PDA mint authority (scoped by mint)
    #[account(seeds = [b"mint-auth", mint.key().as_ref()], bump)]
    pub mint_auth: UncheckedAccount<'info>,

    #[account(
        init,
        payer = creator,
        space = 8 + LaunchConfig::INIT_SPACE,
        seeds = [b"launch", mint.key().as_ref()],
        bump,
    )]
    pub launch_config: Box<Account<'info, LaunchConfig>>,

    #[account(
        init,
        payer = creator,
        associated_token::mint = mint,
        associated_token::authority = launch_config,
    )]
    pub bonding_curve_ata: Box<Account<'info, TokenAccount>>,

    #[account(
        init,
        payer = creator,
        associated_token::mint = usdc_mint,
        associated_token::authority = usdc_vault_auth,
    )]
    pub usdc_vault: Box<Account<'info, TokenAccount>>,

    /// CHECK: vault authority PDA (scoped by mint)
    #[account(seeds = [b"usdc-vault", mint.key().as_ref()], bump)]
    pub usdc_vault_auth: UncheckedAccount<'info>,

    /// USDC mint (devnet constant)
    #[account(mut, address = USDC_DEVNET)]
    pub usdc_mint: Account<'info, Mint>,

    #[account(
        mut,
        associated_token::mint = usdc_mint,
        associated_token::authority = creator,
    )]
    pub creator_usdc: Box<Account<'info, TokenAccount>>,

    /// CHECK: platform wallet (SystemAccount)
    #[account(mut, address = PLATFORM_WALLET_PUBKEY)]
    pub platform_wallet: SystemAccount<'info>,

    #[account(
        init_if_needed,
        payer = creator,
        associated_token::mint = usdc_mint,
        associated_token::authority = platform_wallet,
    )]
    pub platform_usdc_ata: Box<Account<'info, TokenAccount>>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Buy<'info> {
    #[account(mut)]
    pub buyer: Signer<'info>,

    #[account(
        mut,
        seeds = [b"launch", mint.key().as_ref()],
        bump = launch_config.bump,
    )]
    pub launch_config: Box<Account<'info, LaunchConfig>>,

    pub mint: Account<'info, Mint>,

    /// CHECK: USDC devnet constant
    #[account(address = USDC_DEVNET)]
    pub usdc_mint: UncheckedAccount<'info>,

    #[account(mut)]
    pub buyer_usdc: Box<Account<'info, TokenAccount>>,

    #[account(mut)]
    pub usdc_vault: Box<Account<'info, TokenAccount>>,

    #[account(mut)]
    pub bonding_curve_ata: Box<Account<'info, TokenAccount>>,

    #[account(
        init_if_needed,
        payer = buyer,
        associated_token::mint = mint,
        associated_token::authority = buyer,
    )]
    pub buyer_x_ata: Box<Account<'info, TokenAccount>>,

    #[account(
        init_if_needed,
        payer = buyer,
        space = 8 + BuyerLedger::INIT_SPACE,
        seeds = [b"buyer_ledger", mint.key().as_ref(), buyer.key().as_ref()],
        bump,
    )]
    pub buyer_ledger: Box<Account<'info, BuyerLedger>>,

    /// CHECK: burn authority PDA (same as mint_auth)
    #[account(seeds = [b"mint-auth", mint.key().as_ref()], bump)]
    pub burn_auth: UncheckedAccount<'info>,

    #[account(
        init_if_needed,
        payer = buyer,
        associated_token::mint = mint,
        associated_token::authority = burn_auth,
    )]
    pub burn_ata: Box<Account<'info, TokenAccount>>,

    /// CHECK: mint authority PDA
    #[account(seeds = [b"mint-auth", mint.key().as_ref()], bump)]
    pub mint_auth: UncheckedAccount<'info>,

    /// CHECK: vault authority PDA
    #[account(seeds = [b"usdc-vault", mint.key().as_ref()], bump)]
    pub usdc_vault_auth: UncheckedAccount<'info>,

    /// CHECK: creator account from config
    #[account(address = launch_config.creator)]
    pub creator: UncheckedAccount<'info>,

    #[account(
        init_if_needed,
        payer = buyer,
        associated_token::mint = usdc_mint,
        associated_token::authority = creator,
    )]
    pub creator_usdc_ata: Box<Account<'info, TokenAccount>>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Sell<'info> {
    #[account(mut)]
    pub seller: Signer<'info>,

    #[account(
        mut,
        seeds = [b"launch", mint.key().as_ref()],
        bump = launch_config.bump,
    )]
    pub launch_config: Box<Account<'info, LaunchConfig>>,

    pub mint: Account<'info, Mint>,

    #[account(mut)]
    pub seller_x_ata: Box<Account<'info, TokenAccount>>,

    #[account(mut)]
    pub bonding_curve_ata: Box<Account<'info, TokenAccount>>,

    #[account(mut)]
    pub usdc_vault: Box<Account<'info, TokenAccount>>,

    /// CHECK: USDC vault authority PDA
    #[account(seeds = [b"usdc-vault", mint.key().as_ref()], bump)]
    pub usdc_vault_auth: UncheckedAccount<'info>,

    #[account(
        init_if_needed,
        payer = seller,
        associated_token::mint = usdc_mint,
        associated_token::authority = seller,
    )]
    pub seller_usdc_ata: Box<Account<'info, TokenAccount>>,

    /// CHECK: USDC devnet constant
    #[account(address = USDC_DEVNET)]
    pub usdc_mint: UncheckedAccount<'info>,

    /// CHECK: platform wallet (SystemAccount)
    #[account(mut, address = PLATFORM_WALLET_PUBKEY)]
    pub platform_wallet: SystemAccount<'info>,

    #[account(
        init_if_needed,
        payer = seller,
        associated_token::mint = usdc_mint,
        associated_token::authority = platform_wallet,
    )]
    pub platform_profit_ata: Box<Account<'info, TokenAccount>>,

    #[account(
        init_if_needed,
        payer = seller,
        associated_token::mint = usdc_mint,
        associated_token::authority = launch_config,
    )]
    pub creator_profit_ata: Box<Account<'info, TokenAccount>>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ClaimProfits<'info> {
    #[account(mut)]
    pub user: Signer<'info>,

    #[account(
        mut,
        seeds = [b"launch", mint.key().as_ref()],
        bump = launch_config.bump,
    )]
    pub launch_config: Box<Account<'info, LaunchConfig>>,

    #[account(
        init_if_needed,
        payer = user,
        space = 8 + BuyerLedger::INIT_SPACE,
        seeds = [b"buyer_ledger", mint.key().as_ref(), user.key().as_ref()],
        bump,
    )]
    pub buyer_ledger: Box<Account<'info, BuyerLedger>>,

    pub mint: Account<'info, Mint>,

    #[account(mut)]
    pub user_x_ata: Box<Account<'info, TokenAccount>>,

    #[account(
        init_if_needed,
        payer = user,
        associated_token::mint = usdc_mint,
        associated_token::authority = user,
    )]
    pub user_usdc_ata: Box<Account<'info, TokenAccount>>,

    #[account(mut)]
    pub usdc_vault: Box<Account<'info, TokenAccount>>,

    /// CHECK: USDC vault authority PDA
    #[account(seeds = [b"usdc-vault", mint.key().as_ref()], bump)]
    pub usdc_vault_auth: UncheckedAccount<'info>,

    /// CHECK: USDC devnet constant
    #[account(address = USDC_DEVNET)]
    pub usdc_mint: UncheckedAccount<'info>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Finalize<'info> {
    #[account(mut)]
    pub caller: Signer<'info>,

    #[account(
        mut,
        seeds = [b"launch", mint.key().as_ref()],
        bump = launch_config.bump,
    )]
    pub launch_config: Box<Account<'info, LaunchConfig>>,

    pub mint: Account<'info, Mint>,

    #[account(mut)]
    pub usdc_vault: Box<Account<'info, TokenAccount>>,

    /// CHECK: USDC vault authority PDA
    #[account(seeds = [b"usdc-vault", mint.key().as_ref()], bump)]
    pub usdc_vault_auth: UncheckedAccount<'info>,

    /// CHECK: creator from config
    #[account(address = launch_config.creator)]
    pub creator: UncheckedAccount<'info>,

    /// USDC mint (devnet)
    #[account(address = USDC_DEVNET)]
    pub usdc_mint: Account<'info, Mint>,

    #[account(
        init_if_needed,
        payer = caller,
        associated_token::mint = usdc_mint,
        associated_token::authority = creator,
    )]
    pub creator_usdc_ata: Box<Account<'info, TokenAccount>>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ReclaimVirtualFunds<'info> {
    #[account(mut)]
    pub creator: Signer<'info>,

    #[account(
        mut,
        has_one = creator,
        seeds = [b"launch", mint.key().as_ref()],
        bump = launch_config.bump,
        constraint = launch_config.closed || launch_config.migrated,
    )]
    pub launch_config: Box<Account<'info, LaunchConfig>>,

    pub mint: Account<'info, Mint>,

    #[account(mut)]
    pub usdc_vault: Box<Account<'info, TokenAccount>>,

    /// CHECK: USDC vault authority PDA
    #[account(seeds = [b"usdc-vault", mint.key().as_ref()], bump)]
    pub usdc_vault_auth: UncheckedAccount<'info>,

    /// USDC mint (devnet)
    #[account(address = USDC_DEVNET)]
    pub usdc_mint: Account<'info, Mint>,

    #[account(
        init_if_needed,
        payer = creator,
        associated_token::mint = usdc_mint,
        associated_token::authority = creator,
    )]
    pub creator_usdc_ata: Box<Account<'info, TokenAccount>>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CloseAndMigrateToRaydium<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,

    #[account(
        mut,
        constraint = launch_config.creator == payer.key(),
        seeds = [b"launch", mint.key().as_ref()],
        bump = launch_config.bump,
        constraint = launch_config.closed,
        close = payer,
    )]
    pub launch_config: Box<Account<'info, LaunchConfig>>,

    pub mint: Account<'info, Mint>,

    #[account(mut)]
    pub bonding_curve_ata: Box<Account<'info, TokenAccount>>,

    #[account(mut)]
    pub usdc_vault: Box<Account<'info, TokenAccount>>,

    /// CHECK: USDC vault authority PDA
    #[account(seeds = [b"usdc-vault", mint.key().as_ref()], bump)]
    pub usdc_vault_auth: UncheckedAccount<'info>,

    /// CHECK: AMM ID for Raydium pool
    #[account(mut)]
    pub amm_id: UncheckedAccount<'info>,

    pub lp_mint: Account<'info, Mint>,

    #[account(mut)]
    pub coin_vault: Box<Account<'info, TokenAccount>>,

    #[account(mut)]
    pub pc_vault: Box<Account<'info, TokenAccount>>,

    #[account(
        init,
        payer = payer,
        space = 8 + LPLock::INIT_SPACE,
        seeds = [b"lp-lock", amm_id.key().as_ref()],
        bump,
    )]
    pub lp_lock: Box<Account<'info, LPLock>>,

    #[account(
        init_if_needed,
        payer = payer,
        associated_token::mint = lp_mint,
        associated_token::authority = lp_lock_auth,
    )]
    pub lp_lock_vault: Box<Account<'info, TokenAccount>>,

    /// CHECK: LP lock authority PDA
    #[account(seeds = [b"lp-lock", amm_id.key().as_ref()], bump)]
    pub lp_lock_auth: UncheckedAccount<'info>,

    /// CHECK: mint authority PDA
    #[account(seeds = [b"mint-auth", mint.key().as_ref()], bump)]
    pub mint_auth: UncheckedAccount<'info>,

    /// CHECK: creator from config
    #[account(address = launch_config.creator)]
    pub creator: UncheckedAccount<'info>,

    #[account(
        init_if_needed,
        payer = payer,
        associated_token::mint = usdc_mint,
        associated_token::authority = creator,
    )]
    pub creator_usdc_ata: Box<Account<'info, TokenAccount>>,

    /// CHECK: USDC devnet constant
    #[account(address = USDC_DEVNET)]
    pub usdc_mint: UncheckedAccount<'info>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct MigrateLP<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,

    #[account(
        mut,
        seeds = [b"lp-lock", lock.amm_id.as_ref()],
        bump = lock.bump,
        constraint = lock.migration_allowed @ LaunchError::MigrationNotAllowed,
        constraint = Clock::get()?.unix_timestamp >= lock.unlock_timestamp @ LaunchError::StillLocked,
        constraint = lock.authority == authority.key() @ LaunchError::NotYourLedger,
    )]
    pub lock: Box<Account<'info, LPLock>>,

    /// CHECK: new AMM ID
    #[account(mut)]
    pub new_amm: UncheckedAccount<'info>,

    pub lp_mint: Account<'info, Mint>,

    #[account(mut)]
    pub old_vault: Box<Account<'info, TokenAccount>>,

    #[account(mut)]
    pub new_vault: Box<Account<'info, TokenAccount>>,

    #[account(
        init_if_needed,
        payer = authority,
        associated_token::mint = lp_mint,
        associated_token::authority = lp_lock_auth,
    )]
    pub new_lp_lock_vault: Box<Account<'info, TokenAccount>>,

    /// CHECK: LP lock authority PDA
    #[account(seeds = [b"lp-lock", lock.amm_id.as_ref()], bump)]
    pub lp_lock_auth: UncheckedAccount<'info>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct WithdrawCreatorReserve<'info> {
    #[account(mut)]
    pub creator: Signer<'info>,

    #[account(
        mut,
        has_one = creator,
        seeds = [b"launch", mint.key().as_ref()],
        bump = launch_config.bump,
    )]
    pub launch_config: Box<Account<'info, LaunchConfig>>,

    pub mint: Account<'info, Mint>,

    #[account(mut)]
    pub usdc_vault: Box<Account<'info, TokenAccount>>,

    /// CHECK: USDC vault authority PDA
    #[account(seeds = [b"usdc-vault", mint.key().as_ref()], bump)]
    pub usdc_vault_auth: UncheckedAccount<'info>,

    /// CHECK: USDC mint (devnet constant)
    #[account(address = USDC_DEVNET)]
    pub usdc_mint: UncheckedAccount<'info>,

    #[account(
        init_if_needed,
        payer = creator,
        associated_token::mint = usdc_mint,
        associated_token::authority = creator,
    )]
    pub creator_usdc_ata: Box<Account<'info, TokenAccount>>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct WithdrawPlatformRemaining<'info> {
    #[account(
        mut,
        address = PLATFORM_WALLET_PUBKEY,
    )]
    pub platform: Signer<'info>,

    #[account(
        mut,
        seeds = [b"launch", mint.key().as_ref()],
        bump = launch_config.bump,
    )]
    pub launch_config: Box<Account<'info, LaunchConfig>>,

    pub mint: Account<'info, Mint>,

    #[account(
        init_if_needed,
        payer = platform,
        associated_token::mint = usdc_mint,
        associated_token::authority = platform,
    )]
    pub platform_usdc_ata: Box<Account<'info, TokenAccount>>,

    #[account(mut)]
    pub usdc_vault: Box<Account<'info, TokenAccount>>,

    /// CHECK: vault authority PDA
    #[account(seeds = [b"usdc-vault", mint.key().as_ref()], bump)]
    pub usdc_vault_auth: UncheckedAccount<'info>,

    /// CHECK: USDC mint (devnet constant)
    #[account(address = USDC_DEVNET)]
    pub usdc_mint: UncheckedAccount<'info>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}