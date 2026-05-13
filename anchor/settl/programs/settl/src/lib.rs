use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

declare_id!("RZgHzaU8jKm4ydzwkmoooqd2TNek4L9W4o8tthekeLn");

// ═══════════════════════════════════════════════════════════
//  SETTL — Single Program
//
//  Instructions:
//
//  Config (run once at deploy):
//    initialize_config()           → sets treasury + fee rate
//    update_fee()                  → authority changes fee %
//    update_treasury()             → authority changes treasury
//
//  Merchant Management:
//    register_merchant()           → backend registers merchant
//    request_wallet_update()       → 24hr delayed wallet change
//    confirm_wallet_update()       → confirms after delay
//    deactivate_merchant()         → disables merchant
//
//  Escrow:
//    initialize_merchant_escrow()  → creates AUDD vault for merchant
//    deposit()                     → customer pays AUDD into escrow
//    release()                     → 6am cron releases funds
//                                    net  → merchant wallet
//                                    fee  → treasury wallet
//    update_escrow_wallet()        → syncs wallet after update
//
//  Fee Model: 1.5% (150 basis points)
//    fee = gross * 150 / 10_000
//    net = gross - fee
// ═══════════════════════════════════════════════════════════

#[program]
pub mod settl {
    use super::*;

    // ─────────────────────────────────────────────────────
    // CONFIG
    // ─────────────────────────────────────────────────────

    /// Initialize global Settl config.
    /// Called once at deploy.
    pub fn initialize_config(
        ctx: Context<InitializeConfig>,
        fee_basis_points: u16,
    ) -> Result<()> {
        require!(fee_basis_points <= 1_000, SettlError::FeeTooHigh);

        let config                  = &mut ctx.accounts.config;
        config.authority            = ctx.accounts.authority.key();
        config.treasury_wallet      = ctx.accounts.treasury_wallet.key();
        config.fee_basis_points     = fee_basis_points;
        config.total_fees_collected = 0;
        config.bump                 = ctx.bumps.config;

        emit!(ConfigInitialized {
            authority:        config.authority,
            treasury_wallet:  config.treasury_wallet,
            fee_basis_points: config.fee_basis_points,
        });

        Ok(())
    }

    /// Update the platform fee rate.
    /// Max 10% (1000 basis points).
    pub fn update_fee(
        ctx: Context<ManageConfig>,
        new_fee_basis_points: u16,
    ) -> Result<()> {
        require!(new_fee_basis_points <= 1_000, SettlError::FeeTooHigh);

        let old_fee                          = ctx.accounts.config.fee_basis_points;
        ctx.accounts.config.fee_basis_points = new_fee_basis_points;

        emit!(FeeUpdated { old_fee, new_fee: new_fee_basis_points });
        Ok(())
    }

    /// Update the treasury wallet that receives platform fees.
    pub fn update_treasury(ctx: Context<ManageConfig>) -> Result<()> {
        ctx.accounts.config.treasury_wallet = ctx.accounts.new_treasury.key();
        emit!(TreasuryUpdated {
            new_treasury: ctx.accounts.config.treasury_wallet,
        });
        Ok(())
    }

    // ─────────────────────────────────────────────────────
    // MERCHANT MANAGEMENT
    // ─────────────────────────────────────────────────────

    /// Register a merchant on-chain.
    /// Called by Settl backend only.
    /// Merchant never signs or interacts with this.
    pub fn register_merchant(
        ctx: Context<RegisterMerchant>,
        merchant_id: String,
        wallet_address: Pubkey,
    ) -> Result<()> {
        require!(merchant_id.len() <= 64, SettlError::MerchantIdTooLong);

        let m              = &mut ctx.accounts.merchant;
        m.merchant_id      = merchant_id.clone();
        m.wallet           = wallet_address;
        m.is_active        = true;
        m.registered_at    = Clock::get()?.unix_timestamp;
        m.total_released   = 0;
        m.total_fees_paid  = 0;
        m.authority        = ctx.accounts.authority.key();
        m.pending_wallet   = None;
        m.wallet_update_at = None;
        m.bump             = ctx.bumps.merchant;

        emit!(MerchantRegistered {
            merchant_id,
            wallet_address,
            registered_at: m.registered_at,
        });

        Ok(())
    }

    /// Request a wallet address change.
    /// Stored with a 24-hour security delay.
    pub fn request_wallet_update(
        ctx: Context<ManageMerchant>,
        new_wallet: Pubkey,
    ) -> Result<()> {
        let now            = Clock::get()?.unix_timestamp;
        let m              = &mut ctx.accounts.merchant;
        m.pending_wallet   = Some(new_wallet);
        m.wallet_update_at = Some(now + 86_400);

        emit!(WalletUpdateRequested {
            merchant_id: m.merchant_id.clone(),
            new_wallet,
            unlocks_at:  now + 86_400,
        });
        Ok(())
    }

    /// Confirm wallet update after 24-hour delay has passed.
    pub fn confirm_wallet_update(ctx: Context<ManageMerchant>) -> Result<()> {
        let now       = Clock::get()?.unix_timestamp;
        let m         = &mut ctx.accounts.merchant;
        let unlock_at = m.wallet_update_at.ok_or(SettlError::NoWalletUpdatePending)?;

        require!(now >= unlock_at, SettlError::WalletUpdateNotReady);

        let new_wallet     = m.pending_wallet.ok_or(SettlError::NoWalletUpdatePending)?;
        let old_wallet     = m.wallet;
        m.wallet           = new_wallet;
        m.pending_wallet   = None;
        m.wallet_update_at = None;

        emit!(WalletUpdated {
            merchant_id: m.merchant_id.clone(),
            old_wallet,
            new_wallet,
        });
        Ok(())
    }

    /// Deactivate a merchant — escrow stops accepting deposits.
    pub fn deactivate_merchant(ctx: Context<ManageMerchant>) -> Result<()> {
        ctx.accounts.merchant.is_active = false;
        emit!(MerchantDeactivated {
            merchant_id: ctx.accounts.merchant.merchant_id.clone(),
        });
        Ok(())
    }

    // ─────────────────────────────────────────────────────
    // ESCROW
    // ─────────────────────────────────────────────────────

    /// Create an AUDD escrow vault for a merchant.
    /// Called by backend at merchant registration.
    /// Merchant never signs or calls this.
    pub fn initialize_merchant_escrow(
        ctx: Context<InitializeEscrow>,
        merchant_id: String,
    ) -> Result<()> {
        require!(merchant_id.len() <= 64, SettlError::MerchantIdTooLong);

        let e              = &mut ctx.accounts.escrow;
        e.merchant_id      = merchant_id.clone();
        e.merchant_wallet  = ctx.accounts.merchant.wallet;
        e.pending_balance  = 0;
        e.total_payments   = 0;
        e.last_released_at = 0;
        e.authority        = ctx.accounts.authority.key();
        e.bump             = ctx.bumps.escrow;
        e.vault_bump       = ctx.bumps.vault;

        emit!(EscrowInitialized {
            merchant_id,
            merchant_wallet: ctx.accounts.merchant.wallet,
        });
        Ok(())
    }

    /// Customer deposits AUDD into a merchant escrow vault.
    /// Customer is the ONLY signer.
    /// Merchant does absolutely nothing.
    pub fn deposit(
        ctx: Context<Deposit>,
        merchant_id: String,
        amount: u64,
    ) -> Result<()> {
        require!(amount > 0, SettlError::ZeroAmount);
        require!(ctx.accounts.merchant.is_active, SettlError::MerchantInactive);
        require!(
            ctx.accounts.escrow.merchant_id == merchant_id,
            SettlError::MerchantMismatch
        );

        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from:      ctx.accounts.customer_ata.to_account_info(),
                    to:        ctx.accounts.vault.to_account_info(),
                    authority: ctx.accounts.customer.to_account_info(),
                },
            ),
            amount,
        )?;

        let now = Clock::get()?.unix_timestamp;
        let e   = &mut ctx.accounts.escrow;

        e.pending_balance = e
            .pending_balance
            .checked_add(amount)
            .ok_or(SettlError::Overflow)?;
        e.total_payments = e
            .total_payments
            .checked_add(1)
            .ok_or(SettlError::Overflow)?;

        emit!(PaymentReceived {
            merchant_id:     e.merchant_id.clone(),
            customer_wallet: ctx.accounts.customer.key(),
            amount,
            pending_balance: e.pending_balance,
            timestamp:       now,
        });

        Ok(())
    }

    /// Release all pending AUDD at 6am daily.
    /// Only callable by Settl backend authority.
    ///
    /// gross = pending escrow balance
    /// fee   = gross * 150 / 10_000  (1.5%)
    /// net   = gross - fee
    ///
    /// net → merchant wallet  (automatic)
    /// fee → treasury wallet  (automatic, same tx)
    /// Merchant receives funds — never needs to do anything.
    pub fn release(
        ctx: Context<Release>,
        merchant_id: String,
    ) -> Result<()> {
        require!(
            ctx.accounts.escrow.merchant_id == merchant_id,
            SettlError::MerchantMismatch
        );

        let gross = ctx.accounts.vault.amount;
        require!(gross > 0, SettlError::ZeroBalance);

        // ── Fee calculation ───────────────────────
        // 150 basis points = 1.5%
        // Example: 1000 AUDD gross
        //   fee = 1000 * 150 / 10_000 = 15 AUDD
        //   net = 1000 - 15 = 985 AUDD
        let fee_bps = ctx.accounts.config.fee_basis_points as u64;
        let fee = gross
            .checked_mul(fee_bps)
            .ok_or(SettlError::Overflow)?
            .checked_div(10_000)
            .ok_or(SettlError::Overflow)?;
        let net = gross
            .checked_sub(fee)
            .ok_or(SettlError::Overflow)?;

        // ── PDA signer for vault transfers ────────
        let mid_bytes    = merchant_id.as_bytes().to_vec();
        let bump         = ctx.accounts.escrow.bump;
        let seeds: &[&[u8]] = &[b"escrow", mid_bytes.as_slice(), &[bump]];
        let signer       = &[seeds];

        // ── Transfer net → merchant ───────────────
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from:      ctx.accounts.vault.to_account_info(),
                    to:        ctx.accounts.merchant_ata.to_account_info(),
                    authority: ctx.accounts.escrow.to_account_info(),
                },
                signer,
            ),
            net,
        )?;

        // ── Transfer fee → treasury ───────────────
        if fee > 0 {
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from:      ctx.accounts.vault.to_account_info(),
                        to:        ctx.accounts.treasury_ata.to_account_info(),
                        authority: ctx.accounts.escrow.to_account_info(),
                    },
                    signer,
                ),
                fee,
            )?;
        }

        let now = Clock::get()?.unix_timestamp;

        // ── Update merchant totals ────────────────
        let m = &mut ctx.accounts.merchant;
        m.total_released = m
            .total_released
            .checked_add(net)
            .ok_or(SettlError::Overflow)?;
        m.total_fees_paid = m
            .total_fees_paid
            .checked_add(fee)
            .ok_or(SettlError::Overflow)?;

        // ── Update escrow state ───────────────────
        let e              = &mut ctx.accounts.escrow;
        e.pending_balance  = 0;
        e.last_released_at = now;

        // ── Update global fee total ───────────────
        ctx.accounts.config.total_fees_collected = ctx
            .accounts
            .config
            .total_fees_collected
            .checked_add(fee)
            .ok_or(SettlError::Overflow)?;

        emit!(FundsReleased {
            merchant_id: e.merchant_id.clone(),
            merchant_wallet: e.merchant_wallet,
            gross,
            fee,
            net,
            released_at: now,
        });

        Ok(())
    }

    /// Sync escrow wallet after a confirmed registry wallet update.
    pub fn update_escrow_wallet(ctx: Context<UpdateEscrowWallet>) -> Result<()> {
        ctx.accounts.escrow.merchant_wallet = ctx.accounts.merchant.wallet;
        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════
// ACCOUNT CONTEXTS
// ═══════════════════════════════════════════════════════════

#[derive(Accounts)]
pub struct InitializeConfig<'info> {
    #[account(
        init,
        payer  = authority,
        space  = SettlConfig::SPACE,
        seeds  = [b"config"],
        bump
    )]
    pub config: Account<'info, SettlConfig>,

    /// CHECK: stored as destination only
    pub treasury_wallet: UncheckedAccount<'info>,

    #[account(mut)]
    pub authority: Signer<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ManageConfig<'info> {
    #[account(
        mut,
        seeds   = [b"config"],
        bump    = config.bump,
        has_one = authority @ SettlError::Unauthorized,
    )]
    pub config: Account<'info, SettlConfig>,

    /// CHECK: new treasury address
    pub new_treasury: UncheckedAccount<'info>,

    pub authority: Signer<'info>,
}

#[derive(Accounts)]
#[instruction(merchant_id: String)]
pub struct RegisterMerchant<'info> {
    #[account(
        init,
        payer  = authority,
        space  = MerchantAccount::SPACE,
        seeds  = [b"merchant", merchant_id.as_bytes()],
        bump
    )]
    pub merchant: Account<'info, MerchantAccount>,

    #[account(mut)]
    pub authority: Signer<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ManageMerchant<'info> {
    #[account(
        mut,
        has_one = authority @ SettlError::Unauthorized,
    )]
    pub merchant: Account<'info, MerchantAccount>,

    pub authority: Signer<'info>,
}

#[derive(Accounts)]
#[instruction(merchant_id: String)]
pub struct InitializeEscrow<'info> {
    #[account(
        seeds = [b"merchant", merchant_id.as_bytes()],
        bump  = merchant.bump,
    )]
    pub merchant: Account<'info, MerchantAccount>,

    #[account(
        init,
        payer  = authority,
        space  = EscrowAccount::SPACE,
        seeds  = [b"escrow", merchant_id.as_bytes()],
        bump
    )]
    pub escrow: Account<'info, EscrowAccount>,

    #[account(
        init,
        payer            = authority,
        token::mint      = audd_mint,
        token::authority = escrow,
        seeds            = [b"vault", merchant_id.as_bytes()],
        bump
    )]
    pub vault: Account<'info, TokenAccount>,

    pub audd_mint: Account<'info, Mint>,

    #[account(mut)]
    pub authority: Signer<'info>,

    pub token_program:  Program<'info, Token>,
    pub system_program: Program<'info, System>,
    pub rent:           Sysvar<'info, Rent>,
}

#[derive(Accounts)]
#[instruction(merchant_id: String, amount: u64)]
pub struct Deposit<'info> {
    #[account(
        seeds = [b"merchant", merchant_id.as_bytes()],
        bump  = merchant.bump,
    )]
    pub merchant: Account<'info, MerchantAccount>,

    #[account(
        mut,
        seeds = [b"escrow", merchant_id.as_bytes()],
        bump  = escrow.bump,
    )]
    pub escrow: Account<'info, EscrowAccount>,

    #[account(
        mut,
        seeds = [b"vault", merchant_id.as_bytes()],
        bump  = escrow.vault_bump,
    )]
    pub vault: Account<'info, TokenAccount>,

    #[account(mut)]
    pub customer_ata: Account<'info, TokenAccount>,

    #[account(mut)]
    pub customer: Signer<'info>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
#[instruction(merchant_id: String)]
pub struct Release<'info> {
    #[account(
        mut,
        seeds = [b"config"],
        bump  = config.bump,
    )]
    pub config: Account<'info, SettlConfig>,

    #[account(
        mut,
        seeds = [b"merchant", merchant_id.as_bytes()],
        bump  = merchant.bump,
    )]
    pub merchant: Account<'info, MerchantAccount>,

    #[account(
        mut,
        seeds   = [b"escrow", merchant_id.as_bytes()],
        bump    = escrow.bump,
        has_one = authority @ SettlError::Unauthorized,
    )]
    pub escrow: Account<'info, EscrowAccount>,

    #[account(
        mut,
        seeds = [b"vault", merchant_id.as_bytes()],
        bump  = escrow.vault_bump,
    )]
    pub vault: Account<'info, TokenAccount>,

    #[account(mut)]
    pub merchant_ata: Account<'info, TokenAccount>,

    #[account(mut)]
    pub treasury_ata: Account<'info, TokenAccount>,

    pub authority: Signer<'info>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct UpdateEscrowWallet<'info> {
    #[account(has_one = authority @ SettlError::Unauthorized)]
    pub merchant: Account<'info, MerchantAccount>,

    #[account(
        mut,
        has_one = authority @ SettlError::Unauthorized,
    )]
    pub escrow: Account<'info, EscrowAccount>,

    pub authority: Signer<'info>,
}

// ═══════════════════════════════════════════════════════════
// STATE
// ═══════════════════════════════════════════════════════════

#[account]
pub struct SettlConfig {
    pub authority:            Pubkey, // 32
    pub treasury_wallet:      Pubkey, // 32
    pub fee_basis_points:     u16,    // 2
    pub total_fees_collected: u64,    // 8
    pub bump:                 u8,     // 1
}
impl SettlConfig {
    pub const SPACE: usize =
        8  + // discriminator
        32 + // authority
        32 + // treasury_wallet
        2  + // fee_basis_points
        8  + // total_fees_collected
        1  + // bump
        32;  // padding
}

#[account]
pub struct MerchantAccount {
    pub merchant_id:      String,         // 4 + 64
    pub wallet:           Pubkey,         // 32
    pub is_active:        bool,           // 1
    pub registered_at:    i64,            // 8
    pub total_released:   u64,            // 8
    pub total_fees_paid:  u64,            // 8
    pub authority:        Pubkey,         // 32
    pub pending_wallet:   Option<Pubkey>, // 1 + 32
    pub wallet_update_at: Option<i64>,    // 1 + 8
    pub bump:             u8,             // 1
}
impl MerchantAccount {
    pub const SPACE: usize =
        8        + // discriminator
        (4 + 64) + // merchant_id
        32       + // wallet
        1        + // is_active
        8        + // registered_at
        8        + // total_released
        8        + // total_fees_paid
        32       + // authority
        (1 + 32) + // pending_wallet option
        (1 + 8)  + // wallet_update_at option
        1        + // bump
        64;        // padding
}

#[account]
pub struct EscrowAccount {
    pub merchant_id:      String, // 4 + 64
    pub merchant_wallet:  Pubkey, // 32
    pub pending_balance:  u64,    // 8
    pub total_payments:   u64,    // 8
    pub last_released_at: i64,    // 8
    pub authority:        Pubkey, // 32
    pub bump:             u8,     // 1
    pub vault_bump:       u8,     // 1
}
impl EscrowAccount {
    pub const SPACE: usize =
        8        + // discriminator
        (4 + 64) + // merchant_id
        32       + // merchant_wallet
        8        + // pending_balance
        8        + // total_payments
        8        + // last_released_at
        32       + // authority
        1        + // bump
        1        + // vault_bump
        64;        // padding
}

// ═══════════════════════════════════════════════════════════
// EVENTS
// ═══════════════════════════════════════════════════════════

#[event] pub struct ConfigInitialized {
    pub authority:        Pubkey,
    pub treasury_wallet:  Pubkey,
    pub fee_basis_points: u16,
}
#[event] pub struct FeeUpdated      { pub old_fee: u16, pub new_fee: u16 }
#[event] pub struct TreasuryUpdated { pub new_treasury: Pubkey }

#[event] pub struct MerchantRegistered {
    pub merchant_id:   String,
    pub wallet_address: Pubkey,
    pub registered_at: i64,
}
#[event] pub struct WalletUpdateRequested {
    pub merchant_id: String,
    pub new_wallet:  Pubkey,
    pub unlocks_at:  i64,
}
#[event] pub struct WalletUpdated {
    pub merchant_id: String,
    pub old_wallet:  Pubkey,
    pub new_wallet:  Pubkey,
}
#[event] pub struct MerchantDeactivated { pub merchant_id: String }

#[event] pub struct EscrowInitialized {
    pub merchant_id:     String,
    pub merchant_wallet: Pubkey,
}
#[event] pub struct PaymentReceived {
    pub merchant_id:     String,
    pub customer_wallet: Pubkey,
    pub amount:          u64,
    pub pending_balance: u64,
    pub timestamp:       i64,
}
#[event] pub struct FundsReleased {
    pub merchant_id:     String,
    pub merchant_wallet: Pubkey,
    pub gross:           u64,
    pub fee:             u64,
    pub net:             u64,
    pub released_at:     i64,
}

// ═══════════════════════════════════════════════════════════
// ERRORS
// ═══════════════════════════════════════════════════════════

#[error_code]
pub enum SettlError {
    #[msg("Fee cannot exceed 10% (1000 basis points)")]
    FeeTooHigh,
    #[msg("Merchant ID must be 64 characters or fewer")]
    MerchantIdTooLong,
    #[msg("Unauthorized — only the Settl authority can call this")]
    Unauthorized,
    #[msg("No wallet update is pending")]
    NoWalletUpdatePending,
    #[msg("24-hour wallet update delay has not passed yet")]
    WalletUpdateNotReady,
    #[msg("Amount must be greater than zero")]
    ZeroAmount,
    #[msg("No pending balance to release")]
    ZeroBalance,
    #[msg("Merchant ID does not match this escrow")]
    MerchantMismatch,
    #[msg("Merchant is inactive — not accepting payments")]
    MerchantInactive,
    #[msg("Arithmetic overflow")]
    Overflow,
}
