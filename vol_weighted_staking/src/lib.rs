use anchor_lang::prelude::*;
use anchor_lang::solana_program::account_info::AccountInfo;
use anchor_lang::solana_program::hash::hashv;
use anchor_lang::solana_program::sysvar::clock::Clock;

// pyth crates (Solana Playground-friendly)
use pyth_sdk::Price;
use pyth_sdk::PriceFeed;
use pyth_sdk_solana::load_price_feed_from_account_info;

declare_id!("35uJBHPvfJB91PtkhaeFSUEQ8RuGNBzaf2FnWaNGjGKC");

/// ------------------------------------------------------------
/// Volatility-Weighted Staking Allocator + Pyth Oracle (single-file Anchor)
/// ------------------------------------------------------------
/// Simulated vault that:
/// - Tracks staking exposure (staked_sol) and a simulated perp hedge notional (hedge_notional_usd)
/// - Uses Pyth SOL/USD and SOL/USDC feeds for:
///     - oracle spot price (mark-to-market + hedge sizing)
///     - oracle EMA price (drift trigger; less noisy)
///     - confidence/staleness gating
///     - price jump sanity bounds
/// - Computes realized vol on-chain from oracle returns (deterministic), with:
///     - STDEV proxy
///     - EWMA variance option
///     - MAD proxy (robust)
/// - Computes vol_score from realized + implied vol
/// - Updates hedge policy with cooldown + hysteresis + slew-rate limiting
///
/// Hedge flow:
/// - request_hedge(): permissionless if interval OK AND drift (EMA) exceeds band
///   emits HedgeRequested with:
///     - target_hedge_notional_usd (delta-neutral sizing)
///     - delta_gap_usd
///     - reason_code
///     - oracle fields + carry intent
/// - confirm_hedge(): keeper confirms off-chain execution; tracks fills + slippage + miss penalties
///
/// Admin/safety:
/// - paused + emergency_withdraw_enabled
/// - two-step authority transfer
/// - keeper_admin role separation
/// - config_version + config_hash
/// - circuit breaker: if oracle invalid, policy updates freeze and only extreme drift hedge can pass
///
/// Notes:
/// - No CPI calls; all accounting is simulated/deterministic.
///
/// IMPORTANT PYTH NOTE:
/// - Pyth `Price` gives `publish_time` (unix seconds), not a Solana slot.
/// - So we do staleness gating in *seconds* using `Clock::get()?.unix_timestamp`.
/// - We keep the field names `*_slot` for compatibility with the rest of the program,
///   but `oracle_publish_slot` actually stores `publish_time` (unix seconds) in this implementation.
pub const N_RETURNS: usize = 32;

// Fixed-point scales
pub const RET_FP_SCALE: i64 = 1_000_000; // returns i32 scaled 1e6
pub const PRICE_FP_SCALE: i64 = 1_000_000; // prices i64 scaled 1e6

pub const BPS_DENOM: u16 = 10_000;
pub const MAX_VOL_BPS: u16 = 10_000;

// Clamps/safety
pub const MAX_RETURN_ABS_FP: i32 = 250_000; // 25% per sample clamp (scaled 1e6)
pub const MAX_PRICE_FP: i64 = 10_000_000_000_000i64; // 10,000,000 * 1e6
pub const MAX_VAR_FP2: u128 = 10_000_000_000_000_000u128; // variance clamp (FP^2)

// Keepers
pub const MAX_KEEPERS: usize = 8;

// Default stability knobs
pub const DEFAULT_MAX_POLICY_SLEW_BPS: u16 = 1_000; // 10%
pub const DEFAULT_HYSTERESIS_BPS: u16 = 100; // 1%

// Oracle circuit breaker defaults
pub const DEFAULT_EXTREME_DRIFT_BPS: u16 = 2_000; // 20% drift allows hedge even in oracle-degraded mode

#[repr(u8)]
pub enum VolMode {
    Stdev = 0,
    Ewma = 1,
    Mad = 2,
}

#[repr(u8)]
pub enum OracleFeedChoice {
    SolUsd = 1,
    SolUsdc = 2,
    AutoPreferUsdThenUsdc = 3,
}

#[program]
pub mod vol_weighted_staking {
    use super::*;

    pub fn initialize_vault(ctx: Context<InitializeVault>, params: InitializeParams) -> Result<()> {
        // policy bounds
        require!(params.min_band_bps <= params.max_band_bps, ErrorCode::InvalidParams);
        require!(params.max_band_bps <= MAX_VOL_BPS, ErrorCode::InvalidParams);
        require!(params.min_interval_slots <= params.max_interval_slots, ErrorCode::InvalidParams);

        // weights sum to 10_000
        require!(params.vol_weight_realized_bps <= BPS_DENOM, ErrorCode::InvalidParams);
        require!(params.vol_weight_implied_bps <= BPS_DENOM, ErrorCode::InvalidParams);
        require!(
            (params.vol_weight_realized_bps as u32 + params.vol_weight_implied_bps as u32) == BPS_DENOM as u32,
            ErrorCode::InvalidParams
        );

        // anti-gaming / stability
        require!(params.min_samples > 0 && params.min_samples <= (N_RETURNS as u8), ErrorCode::InvalidParams);
        require!(params.min_return_spacing_slots > 0, ErrorCode::InvalidParams);
        require!(params.policy_update_min_slots > 0, ErrorCode::InvalidParams);
        require!(
            params.max_policy_slew_bps > 0 && params.max_policy_slew_bps <= BPS_DENOM,
            ErrorCode::InvalidParams
        );
        require!(params.hysteresis_bps <= BPS_DENOM, ErrorCode::InvalidParams);

        // caps/guardrails
        require!(params.max_staked_sol > 0, ErrorCode::InvalidParams);
        require!(params.max_abs_hedge_notional_usd > 0, ErrorCode::InvalidParams);
        require!(params.max_hedge_per_sol_usd_fp > 0, ErrorCode::InvalidParams);
        require!(params.min_reserve_bps <= BPS_DENOM, ErrorCode::InvalidParams);

        // vol mode
        require!(
            params.vol_mode == VolMode::Stdev as u8
                || params.vol_mode == VolMode::Ewma as u8
                || params.vol_mode == VolMode::Mad as u8,
            ErrorCode::InvalidParams
        );
        if params.vol_mode == VolMode::Ewma as u8 {
            require!(
                params.ewma_alpha_bps > 0 && params.ewma_alpha_bps <= BPS_DENOM,
                ErrorCode::InvalidParams
            );
        }

        // oracle params (NOTE: interpreted as seconds in this implementation)
        require!(params.max_price_age_slots > 0, ErrorCode::InvalidParams);
        require!(params.max_confidence_bps <= BPS_DENOM, ErrorCode::InvalidParams);
        require!(params.max_price_jump_bps <= BPS_DENOM, ErrorCode::InvalidParams);
        require!(
            params.oracle_feed_choice == OracleFeedChoice::SolUsd as u8
                || params.oracle_feed_choice == OracleFeedChoice::SolUsdc as u8
                || params.oracle_feed_choice == OracleFeedChoice::AutoPreferUsdThenUsdc as u8,
            ErrorCode::InvalidParams
        );

        // hedge targeting
        require!(params.target_delta_bps <= BPS_DENOM, ErrorCode::InvalidParams);
        require!(params.lst_beta_fp > 0, ErrorCode::InvalidParams); // fp 1e6

        // hedge confirm
        require!(params.max_confirm_delay_slots > 0, ErrorCode::InvalidParams);

        // keeper rate limits/bond (simulated)
        require!(params.max_updates_per_epoch > 0, ErrorCode::InvalidParams);

        let state = &mut ctx.accounts.vault_state;

        state.authority = ctx.accounts.authority.key();
        state.pending_authority = Pubkey::default();
        state.keeper_admin = ctx.accounts.authority.key();
        state.vault_bump = ctx.bumps.vault_state;

        state.config_version = 1;
        state.config_hash = [0u8; 32];

        state.epoch = 0;
        state.last_policy_update_slot = 0;

        // exposures
        state.staked_sol = 0;
        state.reserve_sol = 0;
        state.hedge_notional_usd = 0;

        // caps/guardrails
        state.max_staked_sol = params.max_staked_sol;
        state.max_abs_hedge_notional_usd = params.max_abs_hedge_notional_usd;
        state.max_hedge_per_sol_usd_fp = params.max_hedge_per_sol_usd_fp;
        state.min_reserve_bps = params.min_reserve_bps;

        // returns buffer (oracle-driven)
        state.returns_ring = [0i32; N_RETURNS];
        state.returns_idx = 0;
        state.nonzero_samples = 0;
        state.last_return_slot = 0;
        state.min_samples = params.min_samples;
        state.min_return_spacing_slots = params.min_return_spacing_slots;

        // realized vol model
        state.vol_mode = params.vol_mode;
        state.ewma_alpha_bps = params.ewma_alpha_bps;
        state.ewma_var_fp2 = 0;

        // implied/score
        state.realized_vol_bps = 0;
        state.implied_vol_bps = 0;
        state.vol_score_bps = 0;
        state.last_vol_score_bps = 0;
        state.vol_weight_realized_bps = params.vol_weight_realized_bps;
        state.vol_weight_implied_bps = params.vol_weight_implied_bps;

        // policy bounds + outputs
        state.min_band_bps = params.min_band_bps;
        state.max_band_bps = params.max_band_bps;
        state.min_interval_slots = params.min_interval_slots;
        state.max_interval_slots = params.max_interval_slots;

        state.band_bps = params.min_band_bps;
        state.min_hedge_interval_slots = params.min_interval_slots;

        // stability
        state.policy_update_min_slots = params.policy_update_min_slots;
        state.max_policy_slew_bps = params.max_policy_slew_bps;
        state.hysteresis_bps = params.hysteresis_bps;

        // oracle config + state
        state.oracle_feed_choice = params.oracle_feed_choice;
        state.max_price_age_slots = params.max_price_age_slots;
        state.max_confidence_bps = params.max_confidence_bps;
        state.max_price_jump_bps = params.max_price_jump_bps;

        state.oracle_price_fp = 0;
        state.oracle_ema_price_fp = 0;
        state.oracle_conf_fp = 0;
        state.oracle_publish_slot = 0; // actually publish_time (unix seconds) in this impl
        state.oracle_ok = false;

        state.last_oracle_price_fp = 0;
        state.last_oracle_ema_price_fp = 0;

        // hedge timing
        state.last_hedge_slot = 0;
        state.last_hedge_ema_price_fp = 0;

        // hedge sizing knobs
        state.target_delta_bps = params.target_delta_bps;
        state.lst_beta_fp = params.lst_beta_fp;

        // carry inputs (keeper-fed)
        state.funding_bps_per_day = 0;
        state.borrow_bps_per_day = 0;
        state.staking_bps_per_day = 0;

        // circuit breaker
        state.oracle_degraded = false;
        state.extreme_drift_bps = params.extreme_drift_bps;

        // hedge confirm tracking
        state.last_hedge_request_slot = 0;
        state.last_hedge_request_id = 0;
        state.request_outstanding = false;

        state.last_fill_slot = 0;
        state.hedge_fill_count = 0;
        state.avg_fill_slippage_bps = 0;
        state.missed_confirms = 0;
        state.max_confirm_delay_slots = params.max_confirm_delay_slots;

        // safety toggles
        state.paused = false;
        state.emergency_withdraw_enabled = false;

        // keepers + rate limits/bond (simulated)
        state.keepers = [Pubkey::default(); MAX_KEEPERS];
        state.keeper_count = 0;
        state.keeper_heartbeat_slot = [0u64; MAX_KEEPERS];
        state.keeper_miss_count = [0u32; MAX_KEEPERS];

        state.max_updates_per_epoch = params.max_updates_per_epoch;
        state.keeper_updates_this_epoch = [0u16; MAX_KEEPERS];

        state.keeper_bond_required_lamports = params.keeper_bond_required_lamports;
        state.keeper_bond_deposited_lamports = [0u64; MAX_KEEPERS];

        // compute initial config hash
        state.recompute_config_hash();

        emit!(VaultInitialized {
            authority: state.authority,
            keeper_admin: state.keeper_admin,
            config_version: state.config_version,
            config_hash: state.config_hash,
            epoch: state.epoch,

            min_band_bps: state.min_band_bps,
            max_band_bps: state.max_band_bps,
            min_interval_slots: state.min_interval_slots,
            max_interval_slots: state.max_interval_slots,

            vol_weight_realized_bps: state.vol_weight_realized_bps,
            vol_weight_implied_bps: state.vol_weight_implied_bps,

            min_samples: state.min_samples,
            min_return_spacing_slots: state.min_return_spacing_slots,

            policy_update_min_slots: state.policy_update_min_slots,
            max_policy_slew_bps: state.max_policy_slew_bps,
            hysteresis_bps: state.hysteresis_bps,

            vol_mode: state.vol_mode,
            ewma_alpha_bps: state.ewma_alpha_bps,

            max_staked_sol: state.max_staked_sol,
            max_abs_hedge_notional_usd: state.max_abs_hedge_notional_usd,
            max_hedge_per_sol_usd_fp: state.max_hedge_per_sol_usd_fp,
            min_reserve_bps: state.min_reserve_bps,

            oracle_feed_choice: state.oracle_feed_choice,
            max_price_age_slots: state.max_price_age_slots,
            max_confidence_bps: state.max_confidence_bps,
            max_price_jump_bps: state.max_price_jump_bps,

            target_delta_bps: state.target_delta_bps,
            lst_beta_fp: state.lst_beta_fp,

            max_confirm_delay_slots: state.max_confirm_delay_slots,
            extreme_drift_bps: state.extreme_drift_bps,

            max_updates_per_epoch: state.max_updates_per_epoch,
            keeper_bond_required_lamports: state.keeper_bond_required_lamports,
        });

        Ok(())
    }

    /// User: simulated staking deposit (no token transfers)
    pub fn deposit_and_stake(ctx: Context<UserWithVault>, amount_sol: u64) -> Result<()> {
        let state = &mut ctx.accounts.vault_state;
        state.require_not_paused()?;

        require!(amount_sol > 0, ErrorCode::InvalidParams);

        let new_staked = state.staked_sol.checked_add(amount_sol).ok_or(ErrorCode::MathOverflow)?;
        require!(new_staked <= state.max_staked_sol, ErrorCode::CapExceeded);

        state.staked_sol = new_staked;
        state.enforce_reserve_ratio()?;

        emit!(StakeAllocated {
            epoch: state.epoch,
            slot: Clock::get()?.slot,
            amount_sol,
            new_staked_sol: state.staked_sol,
            reserve_sol: state.reserve_sol,
        });

        Ok(())
    }

    /// User: simulated reserve buffer deposit (slashing buffer)
    pub fn deposit_reserve(ctx: Context<UserWithVault>, amount_sol: u64) -> Result<()> {
        let state = &mut ctx.accounts.vault_state;
        state.require_not_paused()?;
        require!(amount_sol > 0, ErrorCode::InvalidParams);

        state.reserve_sol = state.reserve_sol.checked_add(amount_sol).ok_or(ErrorCode::MathOverflow)?;
        state.enforce_reserve_ratio()?;

        emit!(ReserveUpdated {
            epoch: state.epoch,
            slot: Clock::get()?.slot,
            reserve_sol: state.reserve_sol,
            min_reserve_bps: state.min_reserve_bps,
        });

        Ok(())
    }

    /// Keeper: (optional) feed implied vol bps
    pub fn update_implied_vol(ctx: Context<KeeperWithVault>, implied_vol_bps: u16) -> Result<()> {
        let state = &mut ctx.accounts.vault_state;
        state.require_not_paused()?;

        state.require_keeper_feeder(&ctx.accounts.signer.key())?;
        state.require_keeper_rate_limit_ok(&ctx.accounts.signer.key())?;

        require!(implied_vol_bps <= MAX_VOL_BPS, ErrorCode::VolOutOfRange);
        state.implied_vol_bps = implied_vol_bps;

        let slot = Clock::get()?.slot;
        state.bump_keeper_heartbeat_and_updates(&ctx.accounts.signer.key(), slot)?;

        emit!(ImpliedVolUpdated {
            epoch: state.epoch,
            slot,
            implied_vol_bps,
        });

        Ok(())
    }

    /// Keeper: carry inputs (bps/day) for hedge intent
    pub fn update_carry_inputs(
        ctx: Context<KeeperWithVault>,
        funding_bps_per_day: i32,
        borrow_bps_per_day: i32,
        staking_bps_per_day: i32,
    ) -> Result<()> {
        let state = &mut ctx.accounts.vault_state;
        state.require_not_paused()?;

        state.require_keeper_feeder(&ctx.accounts.signer.key())?;
        state.require_keeper_rate_limit_ok(&ctx.accounts.signer.key())?;

        state.funding_bps_per_day = funding_bps_per_day;
        state.borrow_bps_per_day = borrow_bps_per_day;
        state.staking_bps_per_day = staking_bps_per_day;

        let slot = Clock::get()?.slot;
        state.bump_keeper_heartbeat_and_updates(&ctx.accounts.signer.key(), slot)?;

        emit!(CarryInputsUpdated {
            epoch: state.epoch,
            slot,
            funding_bps_per_day,
            borrow_bps_per_day,
            staking_bps_per_day,
            expected_carry_bps: state.expected_carry_bps(),
        });

        Ok(())
    }

    /// Keeper: update oracle price (spot + EMA) from Pyth accounts.
    /// Also updates oracle-driven return ring (deterministic) with min spacing gate.
    pub fn update_oracle_price(ctx: Context<UpdateOraclePrice>) -> Result<()> {
        let state = &mut ctx.accounts.vault_state;
        state.require_not_paused()?;

        // allow keeper/authority/keeper_admin
        let signer = ctx.accounts.signer.key();
        state.require_keeper_feeder(&signer)?;
        state.require_keeper_rate_limit_ok(&signer)?;

        let clock = Clock::get()?;
        let slot = clock.slot;
        let now_ts: i64 = clock.unix_timestamp;

        let (chosen, spot_price_fp, ema_price_fp, conf_fp, publish_time_u64, ok, reason) = read_pyth_best_effort(
            state.oracle_feed_choice,
            &ctx.accounts.pyth_sol_usd,
            &ctx.accounts.pyth_sol_usdc,
            slot,
            now_ts,
            state.max_price_age_slots, // interpreted as max_age_seconds here
            state.max_confidence_bps,
            state.max_price_jump_bps,
            state.last_oracle_price_fp,
        )?;

        // update oracle fields
        state.oracle_price_fp = spot_price_fp;
        state.oracle_ema_price_fp = ema_price_fp;
        state.oracle_conf_fp = conf_fp;
        state.oracle_publish_slot = publish_time_u64; // publish_time seconds
        state.oracle_ok = ok;

        // circuit breaker tracking
        if !ok {
            state.oracle_degraded = true;
            emit!(OracleDegraded {
                epoch: state.epoch,
                slot,
                feed_used: chosen,
                reason_code: reason,
                oracle_publish_slot: publish_time_u64,
            });
        } else {
            // If oracle OK now, clear degraded flag
            state.oracle_degraded = false;
            state.last_oracle_price_fp = spot_price_fp;
            state.last_oracle_ema_price_fp = ema_price_fp;
        }

        // oracle-driven return ring (only when ok AND we have previous price)
        if ok {
            state.try_record_oracle_return(slot, spot_price_fp)?;
        }

        state.bump_keeper_heartbeat_and_updates(&signer, slot)?;

        emit!(OraclePriceUpdated {
            epoch: state.epoch,
            slot,
            feed_used: chosen,
            oracle_price_fp: state.oracle_price_fp,
            oracle_ema_price_fp: state.oracle_ema_price_fp,
            oracle_conf_fp: state.oracle_conf_fp,
            oracle_publish_slot: state.oracle_publish_slot,
            oracle_ok: state.oracle_ok,
            oracle_degraded: state.oracle_degraded,
        });

        Ok(())
    }

    /// Keeper: epoch + policy update
    /// - policy cooldown
    /// - realized vol gate via min_samples non-zero returns
    /// - hysteresis + slew
    /// - if oracle degraded: freeze policy updates (keep existing band/interval)
    pub fn update_epoch_and_policy(ctx: Context<KeeperWithVault>) -> Result<()> {
        let state = &mut ctx.accounts.vault_state;
        state.require_not_paused()?;

        state.require_keeper_feeder(&ctx.accounts.signer.key())?;
        state.require_keeper_rate_limit_ok(&ctx.accounts.signer.key())?;

        let slot = Clock::get()?.slot;

        // Policy cooldown
        if state.last_policy_update_slot != 0 {
            let elapsed = slot.checked_sub(state.last_policy_update_slot).unwrap_or(0);
            require!(elapsed >= state.policy_update_min_slots, ErrorCode::PolicyCooldown);
        }
        state.last_policy_update_slot = slot;

        // bump epoch, reset per-keeper update counters for the epoch
        state.epoch = state.epoch.checked_add(1).ok_or(ErrorCode::MathOverflow)?;
        state.keeper_updates_this_epoch = [0u16; MAX_KEEPERS];

        // If oracle degraded, freeze policy mapping (but still emit snapshot)
        let mut realized_updated = false;
        let prev_band = state.band_bps;
        let prev_interval = state.min_hedge_interval_slots;

        if !state.oracle_degraded {
            // realized update gate
            if state.nonzero_samples >= (state.min_samples as u16) {
                let realized = compute_realized_vol_bps_mode(state.vol_mode, &state.returns_ring, state.ewma_var_fp2)?;
                state.realized_vol_bps = realized;
                realized_updated = true;
            }

            // compute vol score
            let vol_score_bps = weighted_vol_score_bps(
                state.realized_vol_bps,
                state.implied_vol_bps,
                state.vol_weight_realized_bps,
                state.vol_weight_implied_bps,
            )?;
            state.vol_score_bps = vol_score_bps;

            // hysteresis decision
            let hysteresis = state.hysteresis_bps;
            let last = state.last_vol_score_bps;
            let delta = if vol_score_bps >= last { vol_score_bps - last } else { last - vol_score_bps };
            let hysteresis_pass = delta >= hysteresis;

            // compute target policy if hysteresis passes (or first time)
            let mut target_band = state.band_bps;
            let mut target_interval = state.min_hedge_interval_slots;

            if hysteresis_pass || last == 0 {
                // base mapping
                target_band = map_u16_by_bps(vol_score_bps, state.min_band_bps, state.max_band_bps)?;
                target_interval = map_u64_by_bps(vol_score_bps, state.min_interval_slots, state.max_interval_slots)?;

                // funding-aware adjustment (small deterministic bias)
                let carry = state.expected_carry_bps();
                let (adj_band_bps, adj_interval_bps) = carry_policy_bias_bps(carry)?;
                target_band = apply_bps_bias_u16(target_band, adj_band_bps)?;
                target_interval = apply_bps_bias_u64(target_interval, adj_interval_bps)?;

                state.last_vol_score_bps = vol_score_bps;

                emit!(PolicyIntentComputed {
                    epoch: state.epoch,
                    slot,
                    vol_score_bps,
                    expected_carry_bps: carry,
                    bias_band_bps: adj_band_bps,
                    bias_interval_bps: adj_interval_bps,
                    target_band_bps: target_band,
                    target_interval_slots: target_interval,
                });
            }

            // slew-rate limit
            state.band_bps = slew_limit_u16(state.band_bps, target_band, state.max_policy_slew_bps)?;
            state.min_hedge_interval_slots =
                slew_limit_u64(state.min_hedge_interval_slots, target_interval, state.max_policy_slew_bps)?;

            emit!(PolicyUpdated {
                epoch: state.epoch,
                slot,
                band_bps: state.band_bps,
                min_hedge_interval_slots: state.min_hedge_interval_slots,
                vol_score_bps: state.vol_score_bps,
                hysteresis_pass: (delta >= hysteresis) || (last == 0),
                max_policy_slew_bps: state.max_policy_slew_bps,
            });
        } else {
            state.band_bps = prev_band;
            state.min_hedge_interval_slots = prev_interval;
            emit!(PolicyFrozen {
                epoch: state.epoch,
                slot,
                band_bps: state.band_bps,
                min_hedge_interval_slots: state.min_hedge_interval_slots,
                reason_code: 1,
            });
        }

        // NAV snapshot (simulated)
        let nav = state.compute_nav_usd()?;
        emit!(NavSnapshot {
            epoch: state.epoch,
            slot,
            nav_usd: nav,
            staked_value_usd: state.staked_value_usd()?,
            reserve_value_usd: state.reserve_value_usd()?,
            unrealized_pnl_usd: state.unrealized_pnl_usd()?,
            staking_accrued_usd: state.staking_accrued_usd,
            oracle_price_fp: state.oracle_price_fp,
            oracle_ok: state.oracle_ok,
        });

        emit!(EpochUpdated {
            epoch: state.epoch,
            slot,
            realized_vol_bps: state.realized_vol_bps,
            implied_vol_bps: state.implied_vol_bps,
            vol_score_bps: state.vol_score_bps,
            realized_updated,
            nonzero_samples: state.nonzero_samples,
            oracle_degraded: state.oracle_degraded,
        });

        emit!(VaultSnapshot {
            epoch: state.epoch,
            slot,
            staked_sol: state.staked_sol,
            reserve_sol: state.reserve_sol,
            hedge_notional_usd: state.hedge_notional_usd,
            band_bps: state.band_bps,
            min_hedge_interval_slots: state.min_hedge_interval_slots,
            realized_vol_bps: state.realized_vol_bps,
            implied_vol_bps: state.implied_vol_bps,
            vol_score_bps: state.vol_score_bps,
            keeper_count: state.keeper_count,
            paused: state.paused,
            emergency_withdraw_enabled: state.emergency_withdraw_enabled,
            slot_now: slot,
            oracle_price_fp: state.oracle_price_fp,
            oracle_ema_price_fp: state.oracle_ema_price_fp,
            oracle_conf_fp: state.oracle_conf_fp,
            oracle_publish_slot: state.oracle_publish_slot,
            oracle_ok: state.oracle_ok,
            oracle_degraded: state.oracle_degraded,
            expected_carry_bps: state.expected_carry_bps(),
            config_version: state.config_version,
            config_hash: state.config_hash,
        });

        Ok(())
    }

    /// Permissionless: request hedge if interval met AND EMA drift exceeds band.
    pub fn request_hedge(ctx: Context<UserWithVault>) -> Result<()> {
        let state = &mut ctx.accounts.vault_state;
        state.require_not_paused()?;

        let slot = Clock::get()?.slot;

        let elapsed = slot.checked_sub(state.last_hedge_slot).unwrap_or(u64::MAX);
        let interval_ok = elapsed >= state.min_hedge_interval_slots;

        require!(state.oracle_ema_price_fp > 0, ErrorCode::OracleNotReady);

        let drift_bps = compute_price_drift_bps(state.oracle_ema_price_fp, state.last_hedge_ema_price_fp)?;
        let drift_ok = drift_bps >= state.band_bps;

        require!(interval_ok, ErrorCode::HedgeTooSoon);

        if state.oracle_degraded {
            require!(drift_bps >= state.extreme_drift_bps, ErrorCode::OracleDegradedHedgeBlocked);
        } else {
            require!(drift_ok, ErrorCode::DriftNotMet);
        }

        if state.request_outstanding {
            let since_req = slot.checked_sub(state.last_hedge_request_slot).unwrap_or(u64::MAX);
            if since_req > state.max_confirm_delay_slots {
                state.missed_confirms = state.missed_confirms.saturating_add(1);
                emit!(HedgeConfirmMissed {
                    epoch: state.epoch,
                    slot,
                    request_id: state.last_hedge_request_id,
                    since_request_slots: since_req,
                    missed_confirms: state.missed_confirms,
                });
                state.request_outstanding = false;
            }
        }

        let sizing_price_fp = if state.oracle_ok && state.oracle_price_fp > 0 {
            state.oracle_price_fp
        } else {
            state.oracle_ema_price_fp
        };

        let target = compute_target_hedge_notional_usd_delta(
            state.staked_sol,
            sizing_price_fp,
            state.target_delta_bps,
            state.lst_beta_fp,
        )?;

        let delta_gap = target.checked_sub(state.hedge_notional_usd).ok_or(ErrorCode::MathOverflow)?;
        let reason_code = compute_reason_code(interval_ok, drift_ok);

        // update anchors
        state.last_hedge_slot = slot;
        state.last_hedge_ema_price_fp = state.oracle_ema_price_fp;

        state.last_hedge_request_id = state.last_hedge_request_id.saturating_add(1);
        state.last_hedge_request_slot = slot;
        state.request_outstanding = true;

        emit!(HedgeRequested {
            epoch: state.epoch,
            slot,
            request_id: state.last_hedge_request_id,

            band_bps: state.band_bps,
            min_hedge_interval_slots: state.min_hedge_interval_slots,

            staked_sol: state.staked_sol,
            reserve_sol: state.reserve_sol,
            hedge_notional_usd: state.hedge_notional_usd,

            target_hedge_notional_usd: target,
            delta_gap_usd: delta_gap,
            reason_code,

            drift_bps,
            ema_price_fp: state.oracle_ema_price_fp,
            last_hedge_ema_price_fp: state.last_hedge_ema_price_fp,

            oracle_price_fp: state.oracle_price_fp,
            oracle_conf_fp: state.oracle_conf_fp,
            oracle_publish_slot: state.oracle_publish_slot,
            oracle_ok: state.oracle_ok,
            oracle_degraded: state.oracle_degraded,

            target_delta_bps: state.target_delta_bps,
            beta_fp: state.lst_beta_fp,

            expected_carry_bps: state.expected_carry_bps(),
            config_version: state.config_version,
            config_hash: state.config_hash,
        });

        Ok(())
    }

    /// Keeper: confirm hedge execution (two-phase).
    pub fn confirm_hedge(
        ctx: Context<KeeperWithVault>,
        request_id: u64,
        new_hedge_notional_usd: i64,
        fill_price_fp: i64,
    ) -> Result<()> {
        let state = &mut ctx.accounts.vault_state;
        state.require_not_paused()?;

        let signer = ctx.accounts.signer.key();
        state.require_keeper_feeder(&signer)?;
        state.require_keeper_rate_limit_ok(&signer)?;

        require!(fill_price_fp > 0 && fill_price_fp <= MAX_PRICE_FP, ErrorCode::InvalidParams);
        require!(state.request_outstanding, ErrorCode::NoOutstandingRequest);
        require!(request_id == state.last_hedge_request_id, ErrorCode::WrongRequestId);

        let slot = Clock::get()?.slot;

        state.set_hedge_notional_checked(new_hedge_notional_usd)?;

        let ref_price_fp = if state.oracle_ok && state.oracle_price_fp > 0 {
            state.oracle_price_fp
        } else {
            state.oracle_ema_price_fp
        };
        require!(ref_price_fp > 0, ErrorCode::OracleNotReady);

        let slip_bps = compute_slippage_bps(fill_price_fp, ref_price_fp)?;
        state.avg_fill_slippage_bps = ewma_u16(state.avg_fill_slippage_bps, slip_bps, 2000)?;

        state.last_fill_slot = slot;
        state.hedge_fill_count = state.hedge_fill_count.saturating_add(1);
        state.request_outstanding = false;

        state.bump_keeper_heartbeat_and_updates(&signer, slot)?;

        emit!(HedgeConfirmed {
            epoch: state.epoch,
            slot,
            request_id,
            hedge_notional_usd: state.hedge_notional_usd,
            fill_price_fp,
            ref_price_fp,
            slippage_bps: slip_bps,
            avg_fill_slippage_bps: state.avg_fill_slippage_bps,
            hedge_fill_count: state.hedge_fill_count,
        });

        Ok(())
    }

    /// Keeper: (simulated) deposit bond counter (no SOL transfer)
    pub fn deposit_keeper_bond(ctx: Context<KeeperWithVault>, amount_lamports: u64) -> Result<()> {
        let state = &mut ctx.accounts.vault_state;
        state.require_not_paused()?;

        let signer = ctx.accounts.signer.key();
        state.require_keeper_feeder(&signer)?;
        require!(amount_lamports > 0, ErrorCode::InvalidParams);

        let idx = state.keeper_index(&signer).ok_or(ErrorCode::Unauthorized)?;
        state.keeper_bond_deposited_lamports[idx] = state.keeper_bond_deposited_lamports[idx]
            .checked_add(amount_lamports)
            .ok_or(ErrorCode::MathOverflow)?;

        emit!(KeeperBondUpdated {
            epoch: state.epoch,
            slot: Clock::get()?.slot,
            keeper: signer,
            deposited_lamports: state.keeper_bond_deposited_lamports[idx],
            required_lamports: state.keeper_bond_required_lamports,
        });

        Ok(())
    }

    /// Authority: pause/unpause
    pub fn set_paused(ctx: Context<AuthorityOnly>, paused: bool) -> Result<()> {
        let state = &mut ctx.accounts.vault_state;
        state.paused = paused;
        state.bump_config_version_and_hash();

        emit!(PausedSet {
            epoch: state.epoch,
            slot: Clock::get()?.slot,
            paused,
            config_version: state.config_version,
            config_hash: state.config_hash,
        });
        Ok(())
    }

    /// Authority: emergency mode flag
    pub fn set_emergency_withdraw_enabled(ctx: Context<AuthorityOnly>, enabled: bool) -> Result<()> {
        let state = &mut ctx.accounts.vault_state;
        state.emergency_withdraw_enabled = enabled;
        state.bump_config_version_and_hash();

        emit!(EmergencyModeSet {
            epoch: state.epoch,
            slot: Clock::get()?.slot,
            emergency_withdraw_enabled: enabled,
            config_version: state.config_version,
            config_hash: state.config_hash,
        });
        Ok(())
    }

    /// Authority: two-step authority transfer (set)
    pub fn set_pending_authority(ctx: Context<AuthorityOnly>, pending: Pubkey) -> Result<()> {
        require!(pending != Pubkey::default(), ErrorCode::InvalidParams);
        let state = &mut ctx.accounts.vault_state;
        state.pending_authority = pending;
        state.bump_config_version_and_hash();

        emit!(PendingAuthoritySet {
            epoch: state.epoch,
            slot: Clock::get()?.slot,
            pending_authority: pending,
            config_version: state.config_version,
            config_hash: state.config_hash,
        });
        Ok(())
    }

    /// Pending authority accepts transfer
    pub fn accept_authority(ctx: Context<AcceptAuthority>) -> Result<()> {
        let state = &mut ctx.accounts.vault_state;
        require!(state.pending_authority != Pubkey::default(), ErrorCode::InvalidParams);
        require!(ctx.accounts.pending_authority.key() == state.pending_authority, ErrorCode::Unauthorized);

        let old = state.authority;
        state.authority = state.pending_authority;
        state.pending_authority = Pubkey::default();

        // default: move keeper_admin to new authority
        state.keeper_admin = state.authority;

        state.bump_config_version_and_hash();

        emit!(AuthorityAccepted {
            epoch: state.epoch,
            slot: Clock::get()?.slot,
            old_authority: old,
            new_authority: state.authority,
            new_keeper_admin: state.keeper_admin,
            config_version: state.config_version,
            config_hash: state.config_hash,
        });
        Ok(())
    }

    /// Authority: set keeper admin delegate
    pub fn set_keeper_admin(ctx: Context<AuthorityOnly>, keeper_admin: Pubkey) -> Result<()> {
        require!(keeper_admin != Pubkey::default(), ErrorCode::InvalidParams);
        let state = &mut ctx.accounts.vault_state;
        state.keeper_admin = keeper_admin;
        state.bump_config_version_and_hash();

        emit!(KeeperAdminSet {
            epoch: state.epoch,
            slot: Clock::get()?.slot,
            keeper_admin,
            config_version: state.config_version,
            config_hash: state.config_hash,
        });
        Ok(())
    }

    /// Keeper admin: add keeper
    pub fn add_keeper(ctx: Context<KeeperAdminOnly>, keeper: Pubkey) -> Result<()> {
        require!(keeper != Pubkey::default(), ErrorCode::InvalidParams);
        let state = &mut ctx.accounts.vault_state;

        state.add_keeper(keeper)?;
        state.bump_config_version_and_hash();

        emit!(KeeperSet {
            epoch: state.epoch,
            slot: Clock::get()?.slot,
            keeper,
            is_added: true,
            keeper_count: state.keeper_count,
            config_version: state.config_version,
            config_hash: state.config_hash,
        });
        Ok(())
    }

    /// Keeper admin: remove keeper
    pub fn remove_keeper(ctx: Context<KeeperAdminOnly>, keeper: Pubkey) -> Result<()> {
        require!(keeper != Pubkey::default(), ErrorCode::InvalidParams);
        let state = &mut ctx.accounts.vault_state;

        state.remove_keeper(keeper)?;
        state.bump_config_version_and_hash();

        emit!(KeeperSet {
            epoch: state.epoch,
            slot: Clock::get()?.slot,
            keeper,
            is_added: false,
            keeper_count: state.keeper_count,
            config_version: state.config_version,
            config_hash: state.config_hash,
        });
        Ok(())
    }

    /// Authority: update policy bounds
    pub fn set_policy_bounds(
        ctx: Context<AuthorityOnly>,
        min_band_bps: u16,
        max_band_bps: u16,
        min_interval_slots: u64,
        max_interval_slots: u64,
    ) -> Result<()> {
        require!(min_band_bps <= max_band_bps, ErrorCode::InvalidParams);
        require!(max_band_bps <= MAX_VOL_BPS, ErrorCode::InvalidParams);
        require!(min_interval_slots <= max_interval_slots, ErrorCode::InvalidParams);

        let state = &mut ctx.accounts.vault_state;
        state.min_band_bps = min_band_bps;
        state.max_band_bps = max_band_bps;
        state.min_interval_slots = min_interval_slots;
        state.max_interval_slots = max_interval_slots;

        let target_band = map_u16_by_bps(state.vol_score_bps, min_band_bps, max_band_bps)?;
        let target_interval = map_u64_by_bps(state.vol_score_bps, min_interval_slots, max_interval_slots)?;
        state.band_bps = slew_limit_u16(state.band_bps, target_band, state.max_policy_slew_bps)?;
        state.min_hedge_interval_slots =
            slew_limit_u64(state.min_hedge_interval_slots, target_interval, state.max_policy_slew_bps)?;

        state.bump_config_version_and_hash();

        emit!(PolicyBoundsUpdated {
            epoch: state.epoch,
            slot: Clock::get()?.slot,
            min_band_bps,
            max_band_bps,
            min_interval_slots,
            max_interval_slots,
            config_version: state.config_version,
            config_hash: state.config_hash,
        });
        Ok(())
    }

    /// Authority: update policy stability knobs
    pub fn set_policy_stability(
        ctx: Context<AuthorityOnly>,
        policy_update_min_slots: u64,
        max_policy_slew_bps: u16,
        hysteresis_bps: u16,
        extreme_drift_bps: u16,
    ) -> Result<()> {
        require!(policy_update_min_slots > 0, ErrorCode::InvalidParams);
        require!(max_policy_slew_bps > 0 && max_policy_slew_bps <= BPS_DENOM, ErrorCode::InvalidParams);
        require!(hysteresis_bps <= BPS_DENOM, ErrorCode::InvalidParams);
        require!(extreme_drift_bps <= BPS_DENOM, ErrorCode::InvalidParams);

        let state = &mut ctx.accounts.vault_state;
        state.policy_update_min_slots = policy_update_min_slots;
        state.max_policy_slew_bps = max_policy_slew_bps;
        state.hysteresis_bps = hysteresis_bps;
        state.extreme_drift_bps = extreme_drift_bps;

        state.bump_config_version_and_hash();

        emit!(PolicyStabilityUpdated {
            epoch: state.epoch,
            slot: Clock::get()?.slot,
            policy_update_min_slots,
            max_policy_slew_bps,
            hysteresis_bps,
            extreme_drift_bps,
            config_version: state.config_version,
            config_hash: state.config_hash,
        });
        Ok(())
    }

    /// Authority: set vol model
    pub fn set_vol_model(
        ctx: Context<AuthorityOnly>,
        vol_mode: u8,
        ewma_alpha_bps: u16,
        min_samples: u8,
        min_return_spacing_slots: u64,
    ) -> Result<()> {
        require!(
            vol_mode == VolMode::Stdev as u8 || vol_mode == VolMode::Ewma as u8 || vol_mode == VolMode::Mad as u8,
            ErrorCode::InvalidParams
        );
        if vol_mode == VolMode::Ewma as u8 {
            require!(ewma_alpha_bps > 0 && ewma_alpha_bps <= BPS_DENOM, ErrorCode::InvalidParams);
        }
        require!(min_samples > 0 && min_samples <= (N_RETURNS as u8), ErrorCode::InvalidParams);
        require!(min_return_spacing_slots > 0, ErrorCode::InvalidParams);

        let state = &mut ctx.accounts.vault_state;
        state.vol_mode = vol_mode;
        state.ewma_alpha_bps = ewma_alpha_bps;
        state.min_samples = min_samples;
        state.min_return_spacing_slots = min_return_spacing_slots;

        state.bump_config_version_and_hash();

        emit!(VolModelUpdated {
            epoch: state.epoch,
            slot: Clock::get()?.slot,
            vol_mode,
            ewma_alpha_bps,
            min_samples,
            min_return_spacing_slots,
            config_version: state.config_version,
            config_hash: state.config_hash,
        });
        Ok(())
    }

    /// Authority: update oracle gating config
    pub fn set_oracle_config(
        ctx: Context<AuthorityOnly>,
        oracle_feed_choice: u8,
        max_price_age_slots: u64,
        max_confidence_bps: u16,
        max_price_jump_bps: u16,
    ) -> Result<()> {
        require!(
            oracle_feed_choice == OracleFeedChoice::SolUsd as u8
                || oracle_feed_choice == OracleFeedChoice::SolUsdc as u8
                || oracle_feed_choice == OracleFeedChoice::AutoPreferUsdThenUsdc as u8,
            ErrorCode::InvalidParams
        );
        require!(max_price_age_slots > 0, ErrorCode::InvalidParams);
        require!(max_confidence_bps <= BPS_DENOM, ErrorCode::InvalidParams);
        require!(max_price_jump_bps <= BPS_DENOM, ErrorCode::InvalidParams);

        let state = &mut ctx.accounts.vault_state;
        state.oracle_feed_choice = oracle_feed_choice;
        state.max_price_age_slots = max_price_age_slots;
        state.max_confidence_bps = max_confidence_bps;
        state.max_price_jump_bps = max_price_jump_bps;

        state.bump_config_version_and_hash();

        emit!(OracleConfigUpdated {
            epoch: state.epoch,
            slot: Clock::get()?.slot,
            oracle_feed_choice,
            max_price_age_slots,
            max_confidence_bps,
            max_price_jump_bps,
            config_version: state.config_version,
            config_hash: state.config_hash,
        });
        Ok(())
    }

    /// Authority: hedge sizing knobs
    pub fn set_hedge_sizing(ctx: Context<AuthorityOnly>, target_delta_bps: u16, lst_beta_fp: i64) -> Result<()> {
        require!(target_delta_bps <= BPS_DENOM, ErrorCode::InvalidParams);
        require!(lst_beta_fp > 0, ErrorCode::InvalidParams);

        let state = &mut ctx.accounts.vault_state;
        state.target_delta_bps = target_delta_bps;
        state.lst_beta_fp = lst_beta_fp;

        state.bump_config_version_and_hash();

        emit!(HedgeSizingUpdated {
            epoch: state.epoch,
            slot: Clock::get()?.slot,
            target_delta_bps,
            beta_fp: lst_beta_fp,
            config_version: state.config_version,
            config_hash: state.config_hash,
        });
        Ok(())
    }

    /// Authority: risk caps/guardrails
    pub fn set_risk_caps(
        ctx: Context<AuthorityOnly>,
        max_staked_sol: u64,
        max_abs_hedge_notional_usd: i64,
        max_hedge_per_sol_usd_fp: i64,
        min_reserve_bps: u16,
    ) -> Result<()> {
        require!(max_staked_sol > 0, ErrorCode::InvalidParams);
        require!(max_abs_hedge_notional_usd > 0, ErrorCode::InvalidParams);
        require!(max_hedge_per_sol_usd_fp > 0, ErrorCode::InvalidParams);
        require!(min_reserve_bps <= BPS_DENOM, ErrorCode::InvalidParams);

        let state = &mut ctx.accounts.vault_state;
        state.max_staked_sol = max_staked_sol;
        state.max_abs_hedge_notional_usd = max_abs_hedge_notional_usd;
        state.max_hedge_per_sol_usd_fp = max_hedge_per_sol_usd_fp;
        state.min_reserve_bps = min_reserve_bps;

        require!(state.staked_sol <= state.max_staked_sol, ErrorCode::CapExceeded);
        require!(abs_i64(state.hedge_notional_usd) <= state.max_abs_hedge_notional_usd, ErrorCode::CapExceeded);
        state.enforce_leverage_guardrail()?;
        state.enforce_reserve_ratio()?;

        state.bump_config_version_and_hash();

        emit!(RiskCapsUpdated {
            epoch: state.epoch,
            slot: Clock::get()?.slot,
            max_staked_sol,
            max_abs_hedge_notional_usd,
            max_hedge_per_sol_usd_fp,
            min_reserve_bps,
            config_version: state.config_version,
            config_hash: state.config_hash,
        });
        Ok(())
    }

    /// Authority: keeper rate limit + bond config (simulated)
    pub fn set_keeper_controls(ctx: Context<AuthorityOnly>, max_updates_per_epoch: u16, keeper_bond_required_lamports: u64) -> Result<()> {
        require!(max_updates_per_epoch > 0, ErrorCode::InvalidParams);

        let state = &mut ctx.accounts.vault_state;
        state.max_updates_per_epoch = max_updates_per_epoch;
        state.keeper_bond_required_lamports = keeper_bond_required_lamports;

        state.bump_config_version_and_hash();

        emit!(KeeperControlsUpdated {
            epoch: state.epoch,
            slot: Clock::get()?.slot,
            max_updates_per_epoch,
            keeper_bond_required_lamports,
            config_version: state.config_version,
            config_hash: state.config_hash,
        });
        Ok(())
    }

    /// Authority: hedge confirm timing
    pub fn set_confirm_config(ctx: Context<AuthorityOnly>, max_confirm_delay_slots: u64) -> Result<()> {
        require!(max_confirm_delay_slots > 0, ErrorCode::InvalidParams);

        let state = &mut ctx.accounts.vault_state;
        state.max_confirm_delay_slots = max_confirm_delay_slots;

        state.bump_config_version_and_hash();

        emit!(ConfirmConfigUpdated {
            epoch: state.epoch,
            slot: Clock::get()?.slot,
            max_confirm_delay_slots,
            config_version: state.config_version,
            config_hash: state.config_hash,
        });
        Ok(())
    }
}

/// -------------------------------
/// Accounts
/// -------------------------------

#[derive(Accounts)]
pub struct InitializeVault<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,

    #[account(
        init,
        payer = authority,
        space = VaultState::SPACE,
        seeds = [b"vault", authority.key().as_ref()],
        bump
    )]
    pub vault_state: Account<'info, VaultState>,

    pub system_program: Program<'info, System>,
}

/// Permissionless/user context
#[derive(Accounts)]
pub struct UserWithVault<'info> {
    #[account(mut)]
    pub vault_state: Account<'info, VaultState>,
}

/// Keeper context
#[derive(Accounts)]
pub struct KeeperWithVault<'info> {
    pub signer: Signer<'info>,
    #[account(mut)]
    pub vault_state: Account<'info, VaultState>,
}

/// Update oracle price (requires signer + two pyth accounts)
#[derive(Accounts)]
pub struct UpdateOraclePrice<'info> {
    pub signer: Signer<'info>,
    #[account(mut)]
    pub vault_state: Account<'info, VaultState>,

    /// CHECK: Pyth SOL/USD price account
    pub pyth_sol_usd: AccountInfo<'info>,
    /// CHECK: Pyth SOL/USDC price account
    pub pyth_sol_usdc: AccountInfo<'info>,
}

/// Authority-only
#[derive(Accounts)]
pub struct AuthorityOnly<'info> {
    pub authority: Signer<'info>,
    #[account(mut, has_one = authority)]
    pub vault_state: Account<'info, VaultState>,
}

/// Keeper-admin-only
#[derive(Accounts)]
pub struct KeeperAdminOnly<'info> {
    pub keeper_admin: Signer<'info>,
    #[account(mut)]
    pub vault_state: Account<'info, VaultState>,
}

/// Accept authority
#[derive(Accounts)]
pub struct AcceptAuthority<'info> {
    pub pending_authority: Signer<'info>,
    #[account(mut)]
    pub vault_state: Account<'info, VaultState>,
}

/// -------------------------------
/// State
/// -------------------------------

#[account]
pub struct VaultState {
    // roles
    pub authority: Pubkey,
    pub pending_authority: Pubkey,
    pub keeper_admin: Pubkey,
    pub vault_bump: u8,

    // config identity
    pub config_version: u64,
    pub config_hash: [u8; 32],

    // epoch + cooldown
    pub epoch: u64,
    pub last_policy_update_slot: u64,

    // exposures (simulated)
    pub staked_sol: u64,
    pub reserve_sol: u64,
    pub hedge_notional_usd: i64,

    // caps / guardrails
    pub max_staked_sol: u64,
    pub max_abs_hedge_notional_usd: i64,
    pub max_hedge_per_sol_usd_fp: i64, // USD per SOL fp 1e6
    pub min_reserve_bps: u16,

    // oracle-driven returns buffer
    pub returns_ring: [i32; N_RETURNS],
    pub returns_idx: u8,
    pub nonzero_samples: u16,
    pub last_return_slot: u64,
    pub min_samples: u8,
    pub min_return_spacing_slots: u64,

    // realized vol model
    pub vol_mode: u8,
    pub ewma_alpha_bps: u16,
    pub ewma_var_fp2: u128,

    // volatility outputs
    pub realized_vol_bps: u16,
    pub implied_vol_bps: u16,
    pub vol_score_bps: u16,
    pub last_vol_score_bps: u16,

    // score weights
    pub vol_weight_realized_bps: u16,
    pub vol_weight_implied_bps: u16,

    // policy bounds
    pub min_band_bps: u16,
    pub max_band_bps: u16,
    pub min_interval_slots: u64,
    pub max_interval_slots: u64,

    // policy outputs
    pub band_bps: u16,
    pub min_hedge_interval_slots: u64,

    // stability knobs
    pub policy_update_min_slots: u64,
    pub max_policy_slew_bps: u16,
    pub hysteresis_bps: u16,

    // oracle config (NOTE: max_price_age_slots interpreted as seconds in this impl)
    pub oracle_feed_choice: u8,
    pub max_price_age_slots: u64,
    pub max_confidence_bps: u16,
    pub max_price_jump_bps: u16,

    // oracle last observation
    pub oracle_price_fp: i64,
    pub oracle_ema_price_fp: i64,
    pub oracle_conf_fp: i64,
    pub oracle_publish_slot: u64, // actually publish_time seconds (unix) in this impl
    pub oracle_ok: bool,

    pub last_oracle_price_fp: i64,
    pub last_oracle_ema_price_fp: i64,

    // circuit breaker
    pub oracle_degraded: bool,
    pub extreme_drift_bps: u16,

    // hedge sizing knobs
    pub target_delta_bps: u16,
    pub lst_beta_fp: i64,

    // carry inputs (bps/day)
    pub funding_bps_per_day: i32,
    pub borrow_bps_per_day: i32,
    pub staking_bps_per_day: i32,

    // staking accrual (simulated)
    pub staking_accrued_usd: i64,

    // hedge timing + anchors
    pub last_hedge_slot: u64,
    pub last_hedge_ema_price_fp: i64,

    // hedge request/confirm
    pub last_hedge_request_slot: u64,
    pub last_hedge_request_id: u64,
    pub request_outstanding: bool,

    pub last_fill_slot: u64,
    pub hedge_fill_count: u64,
    pub avg_fill_slippage_bps: u16,
    pub missed_confirms: u32,
    pub max_confirm_delay_slots: u64,

    // safety toggles
    pub paused: bool,
    pub emergency_withdraw_enabled: bool,

    // keepers
    pub keepers: [Pubkey; MAX_KEEPERS],
    pub keeper_count: u8,
    pub keeper_heartbeat_slot: [u64; MAX_KEEPERS],
    pub keeper_miss_count: [u32; MAX_KEEPERS],

    // keeper controls
    pub max_updates_per_epoch: u16,
    pub keeper_updates_this_epoch: [u16; MAX_KEEPERS],
    pub keeper_bond_required_lamports: u64,
    pub keeper_bond_deposited_lamports: [u64; MAX_KEEPERS],
}

impl VaultState {
    pub const SPACE: usize = 8
        + 32
        + 32
        + 32
        + 1
        + 8
        + 32
        + 8
        + 8
        + 8
        + 8
        + 8
        + 8
        + 2
        + (4 * N_RETURNS)
        + 1
        + 2
        + 8
        + 1
        + 8
        + 1
        + 2
        + 16
        + 2
        + 2
        + 2
        + 2
        + 2
        + 2
        + 2
        + 2
        + 8
        + 8
        + 2
        + 8
        + 8
        + 2
        + 2
        + 1
        + 8
        + 2
        + 2
        + 8
        + 8
        + 8
        + 8
        + 1
        + 8
        + 8
        + 1
        + 2
        + 2
        + 8
        + 4
        + 4
        + 4
        + 8
        + 8
        + 8
        + 8
        + 1
        + 8
        + 8
        + 8
        + 2
        + 4
        + 8
        + 1
        + 1
        + (32 * MAX_KEEPERS)
        + 1
        + (8 * MAX_KEEPERS)
        + (4 * MAX_KEEPERS)
        + 2
        + (2 * MAX_KEEPERS)
        + 8
        + (8 * MAX_KEEPERS);

    pub fn require_not_paused(&self) -> Result<()> {
        require!(!self.paused, ErrorCode::Paused);
        Ok(())
    }

    pub fn bump_config_version_and_hash(&mut self) {
        self.config_version = self.config_version.saturating_add(1);
        self.recompute_config_hash();
    }

    pub fn recompute_config_hash(&mut self) {
        let mut bytes = Vec::<u8>::with_capacity(256);

        bytes.extend_from_slice(self.authority.as_ref());
        bytes.extend_from_slice(self.keeper_admin.as_ref());

        bytes.extend_from_slice(&self.min_band_bps.to_le_bytes());
        bytes.extend_from_slice(&self.max_band_bps.to_le_bytes());
        bytes.extend_from_slice(&self.min_interval_slots.to_le_bytes());
        bytes.extend_from_slice(&self.max_interval_slots.to_le_bytes());

        bytes.extend_from_slice(&self.vol_weight_realized_bps.to_le_bytes());
        bytes.extend_from_slice(&self.vol_weight_implied_bps.to_le_bytes());

        bytes.push(self.vol_mode);
        bytes.extend_from_slice(&self.ewma_alpha_bps.to_le_bytes());

        bytes.extend_from_slice(&self.min_samples.to_le_bytes());
        bytes.extend_from_slice(&self.min_return_spacing_slots.to_le_bytes());

        bytes.extend_from_slice(&self.policy_update_min_slots.to_le_bytes());
        bytes.extend_from_slice(&self.max_policy_slew_bps.to_le_bytes());
        bytes.extend_from_slice(&self.hysteresis_bps.to_le_bytes());

        bytes.push(self.oracle_feed_choice);
        bytes.extend_from_slice(&self.max_price_age_slots.to_le_bytes());
        bytes.extend_from_slice(&self.max_confidence_bps.to_le_bytes());
        bytes.extend_from_slice(&self.max_price_jump_bps.to_le_bytes());

        bytes.extend_from_slice(&self.target_delta_bps.to_le_bytes());
        bytes.extend_from_slice(&self.lst_beta_fp.to_le_bytes());

        bytes.extend_from_slice(&self.max_staked_sol.to_le_bytes());
        bytes.extend_from_slice(&self.max_abs_hedge_notional_usd.to_le_bytes());
        bytes.extend_from_slice(&self.max_hedge_per_sol_usd_fp.to_le_bytes());
        bytes.extend_from_slice(&self.min_reserve_bps.to_le_bytes());

        bytes.extend_from_slice(&self.max_confirm_delay_slots.to_le_bytes());
        bytes.extend_from_slice(&self.extreme_drift_bps.to_le_bytes());

        bytes.extend_from_slice(&self.max_updates_per_epoch.to_le_bytes());
        bytes.extend_from_slice(&self.keeper_bond_required_lamports.to_le_bytes());

        let h = hashv(&[b"vwsa-config-v1", &bytes]);
        self.config_hash = h.to_bytes();
    }

    pub fn is_keeper(&self, k: &Pubkey) -> bool {
        let n = (self.keeper_count as usize).min(MAX_KEEPERS);
        for i in 0..n {
            if self.keepers[i] == *k {
                return true;
            }
        }
        false
    }

    pub fn keeper_index(&self, k: &Pubkey) -> Option<usize> {
        let n = (self.keeper_count as usize).min(MAX_KEEPERS);
        for i in 0..n {
            if self.keepers[i] == *k {
                return Some(i);
            }
        }
        None
    }

    pub fn require_keeper_feeder(&self, k: &Pubkey) -> Result<()> {
        require!(
            self.is_keeper(k) || *k == self.keeper_admin || *k == self.authority,
            ErrorCode::Unauthorized
        );
        Ok(())
    }

    pub fn require_keeper_rate_limit_ok(&self, k: &Pubkey) -> Result<()> {
        if let Some(i) = self.keeper_index(k) {
            if self.keeper_bond_required_lamports > 0 {
                require!(
                    self.keeper_bond_deposited_lamports[i] >= self.keeper_bond_required_lamports,
                    ErrorCode::KeeperBondInsufficient
                );
            }
            require!(
                self.keeper_updates_this_epoch[i] < self.max_updates_per_epoch,
                ErrorCode::KeeperRateLimited
            );
        }
        Ok(())
    }

    pub fn bump_keeper_heartbeat_and_updates(&mut self, keeper: &Pubkey, slot: u64) -> Result<()> {
        if let Some(i) = self.keeper_index(keeper) {
            self.keeper_heartbeat_slot[i] = slot;
            self.keeper_updates_this_epoch[i] = self.keeper_updates_this_epoch[i].saturating_add(1);
        }
        Ok(())
    }

    pub fn add_keeper(&mut self, keeper: Pubkey) -> Result<()> {
        if self.is_keeper(&keeper) {
            return Ok(());
        }
        let n = self.keeper_count as usize;
        require!(n < MAX_KEEPERS, ErrorCode::InvalidParams);
        self.keepers[n] = keeper;
        self.keeper_heartbeat_slot[n] = 0;
        self.keeper_miss_count[n] = 0;
        self.keeper_updates_this_epoch[n] = 0;
        self.keeper_bond_deposited_lamports[n] = 0;
        self.keeper_count = (n + 1) as u8;
        Ok(())
    }

    pub fn remove_keeper(&mut self, keeper: Pubkey) -> Result<()> {
        let n = self.keeper_count as usize;
        if n == 0 {
            return Ok(());
        }
        let mut idx: Option<usize> = None;
        for i in 0..n.min(MAX_KEEPERS) {
            if self.keepers[i] == keeper {
                idx = Some(i);
                break;
            }
        }
        if idx.is_none() {
            return Ok(());
        }
        let i = idx.unwrap();
        let last = n - 1;

        self.keepers[i] = self.keepers[last];
        self.keeper_heartbeat_slot[i] = self.keeper_heartbeat_slot[last];
        self.keeper_miss_count[i] = self.keeper_miss_count[last];
        self.keeper_updates_this_epoch[i] = self.keeper_updates_this_epoch[last];
        self.keeper_bond_deposited_lamports[i] = self.keeper_bond_deposited_lamports[last];

        self.keepers[last] = Pubkey::default();
        self.keeper_heartbeat_slot[last] = 0;
        self.keeper_miss_count[last] = 0;
        self.keeper_updates_this_epoch[last] = 0;
        self.keeper_bond_deposited_lamports[last] = 0;

        self.keeper_count = last as u8;
        Ok(())
    }

    pub fn set_hedge_notional_checked(&mut self, hedge: i64) -> Result<()> {
        let abs = abs_i64(hedge);
        require!(abs <= self.max_abs_hedge_notional_usd, ErrorCode::CapExceeded);
        self.hedge_notional_usd = hedge;
        self.enforce_leverage_guardrail()?;
        Ok(())
    }

    pub fn enforce_leverage_guardrail(&self) -> Result<()> {
        if self.staked_sol == 0 {
            require!(self.hedge_notional_usd == 0, ErrorCode::LeverageExceeded);
            return Ok(());
        }
        let max_per_sol = self.max_hedge_per_sol_usd_fp as i128;
        require!(max_per_sol > 0, ErrorCode::InvalidParams);

        let lhs = abs_i64(self.hedge_notional_usd) as i128;
        let rhs = (self.staked_sol as i128)
            .checked_mul(max_per_sol)
            .ok_or(ErrorCode::MathOverflow)?
            / (PRICE_FP_SCALE as i128);

        require!(lhs <= rhs, ErrorCode::LeverageExceeded);
        Ok(())
    }

    pub fn enforce_reserve_ratio(&self) -> Result<()> {
        let req = (self.staked_sol as u128)
            .checked_mul(self.min_reserve_bps as u128)
            .ok_or(ErrorCode::MathOverflow)?
            / (BPS_DENOM as u128);
        require!((self.reserve_sol as u128) >= req, ErrorCode::ReserveTooLow);
        Ok(())
    }

    pub fn expected_carry_bps(&self) -> i32 {
        self.staking_bps_per_day
            .saturating_add(self.funding_bps_per_day)
            .saturating_sub(self.borrow_bps_per_day)
    }

    pub fn try_record_oracle_return(&mut self, slot: u64, price_fp: i64) -> Result<()> {
        if self.last_return_slot != 0 {
            let elapsed = slot.checked_sub(self.last_return_slot).unwrap_or(0);
            if elapsed < self.min_return_spacing_slots {
                return Ok(());
            }
        }

        if self.last_oracle_price_fp <= 0 {
            self.last_oracle_price_fp = price_fp;
            self.last_return_slot = slot;
            return Ok(());
        }

        let p = price_fp as i128;
        let p0 = self.last_oracle_price_fp as i128;
        let diff = p.checked_sub(p0).ok_or(ErrorCode::MathOverflow)?;

        let mut ret = diff
            .checked_mul(RET_FP_SCALE as i128)
            .ok_or(ErrorCode::MathOverflow)?
            / p0.max(1);

        if ret > (MAX_RETURN_ABS_FP as i128) {
            ret = MAX_RETURN_ABS_FP as i128;
        } else if ret < -(MAX_RETURN_ABS_FP as i128) {
            ret = -(MAX_RETURN_ABS_FP as i128);
        }
        let ret_i32 = ret as i32;

        let idx = (self.returns_idx as usize) % N_RETURNS;
        let prev = self.returns_ring[idx];
        self.returns_ring[idx] = ret_i32;
        self.returns_idx = self.returns_idx.wrapping_add(1);

        if prev == 0 && ret_i32 != 0 {
            self.nonzero_samples = self.nonzero_samples.checked_add(1).ok_or(ErrorCode::MathOverflow)?;
        } else if prev != 0 && ret_i32 == 0 {
            self.nonzero_samples = self.nonzero_samples.checked_sub(1).ok_or(ErrorCode::MathOverflow)?;
        }

        if self.vol_mode == VolMode::Ewma as u8 {
            let r_abs: i64 = if ret_i32 < 0 { -(ret_i32 as i64) } else { ret_i32 as i64 };
            let r2: u128 = (r_abs as u128).checked_mul(r_abs as u128).ok_or(ErrorCode::MathOverflow)?;
            let r2_clamped = r2.min(MAX_VAR_FP2);
            self.ewma_var_fp2 = ewma_update_u128(self.ewma_var_fp2, r2_clamped, self.ewma_alpha_bps)?;
        }

        self.last_return_slot = slot;
        self.last_oracle_price_fp = price_fp;

        emit!(OracleReturnRecorded {
            epoch: self.epoch,
            slot,
            idx: idx as u8,
            return_fp: ret_i32,
            nonzero_samples: self.nonzero_samples,
            oracle_price_fp: price_fp,
        });

        Ok(())
    }

    pub fn staked_value_usd(&self) -> Result<i64> {
        if self.staked_sol == 0 {
            return Ok(0);
        }
        let p = self.oracle_price_fp;
        require!(p > 0, ErrorCode::OracleNotReady);
        let v = (self.staked_sol as i128)
            .checked_mul(p as i128)
            .ok_or(ErrorCode::MathOverflow)?
            / (PRICE_FP_SCALE as i128);
        Ok(v.min(i64::MAX as i128) as i64)
    }

    pub fn reserve_value_usd(&self) -> Result<i64> {
        if self.reserve_sol == 0 {
            return Ok(0);
        }
        let p = self.oracle_price_fp;
        require!(p > 0, ErrorCode::OracleNotReady);
        let v = (self.reserve_sol as i128)
            .checked_mul(p as i128)
            .ok_or(ErrorCode::MathOverflow)?
            / (PRICE_FP_SCALE as i128);
        Ok(v.min(i64::MAX as i128) as i64)
    }

    pub fn unrealized_pnl_usd(&self) -> Result<i64> {
        Ok(0)
    }

    pub fn compute_nav_usd(&self) -> Result<i64> {
        let st = self.staked_value_usd()?;
        let rs = self.reserve_value_usd()?;
        let pnl = self.unrealized_pnl_usd()?;
        Ok(st
            .checked_add(rs)
            .ok_or(ErrorCode::MathOverflow)?
            .checked_add(pnl)
            .ok_or(ErrorCode::MathOverflow)?
            .checked_add(self.staking_accrued_usd)
            .ok_or(ErrorCode::MathOverflow)?)
    }
}

/// -------------------------------
/// Initialize Params
/// -------------------------------

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct InitializeParams {
    // policy bounds
    pub min_band_bps: u16,
    pub max_band_bps: u16,
    pub min_interval_slots: u64,
    pub max_interval_slots: u64,

    // vol score weights
    pub vol_weight_realized_bps: u16,
    pub vol_weight_implied_bps: u16,

    // anti-gaming (oracle-driven returns)
    pub min_samples: u8,
    pub min_return_spacing_slots: u64,

    // stability
    pub policy_update_min_slots: u64,
    pub max_policy_slew_bps: u16,
    pub hysteresis_bps: u16,

    // vol model
    pub vol_mode: u8,
    pub ewma_alpha_bps: u16,

    // caps/guardrails
    pub max_staked_sol: u64,
    pub max_abs_hedge_notional_usd: i64,
    pub max_hedge_per_sol_usd_fp: i64,
    pub min_reserve_bps: u16,

    // oracle config
    pub oracle_feed_choice: u8,
    pub max_price_age_slots: u64, // interpreted as max_age_seconds in this impl
    pub max_confidence_bps: u16,
    pub max_price_jump_bps: u16,

    // hedge sizing
    pub target_delta_bps: u16,
    pub lst_beta_fp: i64,

    // confirm hedge config
    pub max_confirm_delay_slots: u64,

    // circuit breaker extreme drift
    pub extreme_drift_bps: u16,

    // keeper controls
    pub max_updates_per_epoch: u16,
    pub keeper_bond_required_lamports: u64,
}

/// -------------------------------
/// Events
/// -------------------------------

#[event]
pub struct VaultInitialized {
    pub authority: Pubkey,
    pub keeper_admin: Pubkey,
    pub config_version: u64,
    pub config_hash: [u8; 32],
    pub epoch: u64,

    pub min_band_bps: u16,
    pub max_band_bps: u16,
    pub min_interval_slots: u64,
    pub max_interval_slots: u64,

    pub vol_weight_realized_bps: u16,
    pub vol_weight_implied_bps: u16,

    pub min_samples: u8,
    pub min_return_spacing_slots: u64,

    pub policy_update_min_slots: u64,
    pub max_policy_slew_bps: u16,
    pub hysteresis_bps: u16,

    pub vol_mode: u8,
    pub ewma_alpha_bps: u16,

    pub max_staked_sol: u64,
    pub max_abs_hedge_notional_usd: i64,
    pub max_hedge_per_sol_usd_fp: i64,
    pub min_reserve_bps: u16,

    pub oracle_feed_choice: u8,
    pub max_price_age_slots: u64,
    pub max_confidence_bps: u16,
    pub max_price_jump_bps: u16,

    pub target_delta_bps: u16,
    pub lst_beta_fp: i64,

    pub max_confirm_delay_slots: u64,
    pub extreme_drift_bps: u16,

    pub max_updates_per_epoch: u16,
    pub keeper_bond_required_lamports: u64,
}

#[event]
pub struct StakeAllocated {
    pub epoch: u64,
    pub slot: u64,
    pub amount_sol: u64,
    pub new_staked_sol: u64,
    pub reserve_sol: u64,
}

#[event]
pub struct ReserveUpdated {
    pub epoch: u64,
    pub slot: u64,
    pub reserve_sol: u64,
    pub min_reserve_bps: u16,
}

#[event]
pub struct ImpliedVolUpdated {
    pub epoch: u64,
    pub slot: u64,
    pub implied_vol_bps: u16,
}

#[event]
pub struct CarryInputsUpdated {
    pub epoch: u64,
    pub slot: u64,
    pub funding_bps_per_day: i32,
    pub borrow_bps_per_day: i32,
    pub staking_bps_per_day: i32,
    pub expected_carry_bps: i32,
}

#[event]
pub struct OraclePriceUpdated {
    pub epoch: u64,
    pub slot: u64,
    pub feed_used: u8,
    pub oracle_price_fp: i64,
    pub oracle_ema_price_fp: i64,
    pub oracle_conf_fp: i64,
    pub oracle_publish_slot: u64, // publish_time seconds in this impl
    pub oracle_ok: bool,
    pub oracle_degraded: bool,
}

#[event]
pub struct OracleReturnRecorded {
    pub epoch: u64,
    pub slot: u64,
    pub idx: u8,
    pub return_fp: i32,
    pub nonzero_samples: u16,
    pub oracle_price_fp: i64,
}

#[event]
pub struct OracleDegraded {
    pub epoch: u64,
    pub slot: u64,
    pub feed_used: u8,
    pub reason_code: u8,
    pub oracle_publish_slot: u64,
}

#[event]
pub struct EpochUpdated {
    pub epoch: u64,
    pub slot: u64,
    pub realized_vol_bps: u16,
    pub implied_vol_bps: u16,
    pub vol_score_bps: u16,
    pub realized_updated: bool,
    pub nonzero_samples: u16,
    pub oracle_degraded: bool,
}

#[event]
pub struct PolicyIntentComputed {
    pub epoch: u64,
    pub slot: u64,
    pub vol_score_bps: u16,
    pub expected_carry_bps: i32,
    pub bias_band_bps: i16,
    pub bias_interval_bps: i16,
    pub target_band_bps: u16,
    pub target_interval_slots: u64,
}

#[event]
pub struct PolicyUpdated {
    pub epoch: u64,
    pub slot: u64,
    pub band_bps: u16,
    pub min_hedge_interval_slots: u64,
    pub vol_score_bps: u16,
    pub hysteresis_pass: bool,
    pub max_policy_slew_bps: u16,
}

#[event]
pub struct PolicyFrozen {
    pub epoch: u64,
    pub slot: u64,
    pub band_bps: u16,
    pub min_hedge_interval_slots: u64,
    pub reason_code: u8,
}

#[event]
pub struct NavSnapshot {
    pub epoch: u64,
    pub slot: u64,
    pub nav_usd: i64,
    pub staked_value_usd: i64,
    pub reserve_value_usd: i64,
    pub unrealized_pnl_usd: i64,
    pub staking_accrued_usd: i64,
    pub oracle_price_fp: i64,
    pub oracle_ok: bool,
}

#[event]
pub struct VaultSnapshot {
    pub epoch: u64,
    pub slot: u64,
    pub staked_sol: u64,
    pub reserve_sol: u64,
    pub hedge_notional_usd: i64,
    pub band_bps: u16,
    pub min_hedge_interval_slots: u64,
    pub realized_vol_bps: u16,
    pub implied_vol_bps: u16,
    pub vol_score_bps: u16,
    pub keeper_count: u8,
    pub paused: bool,
    pub emergency_withdraw_enabled: bool,
    pub slot_now: u64,

    pub oracle_price_fp: i64,
    pub oracle_ema_price_fp: i64,
    pub oracle_conf_fp: i64,
    pub oracle_publish_slot: u64, // publish_time seconds in this impl
    pub oracle_ok: bool,
    pub oracle_degraded: bool,

    pub expected_carry_bps: i32,

    pub config_version: u64,
    pub config_hash: [u8; 32],
}

#[event]
pub struct HedgeRequested {
    pub epoch: u64,
    pub slot: u64,
    pub request_id: u64,

    pub band_bps: u16,
    pub min_hedge_interval_slots: u64,

    pub staked_sol: u64,
    pub reserve_sol: u64,
    pub hedge_notional_usd: i64,

    pub target_hedge_notional_usd: i64,
    pub delta_gap_usd: i64,
    pub reason_code: u8,

    pub drift_bps: u16,
    pub ema_price_fp: i64,
    pub last_hedge_ema_price_fp: i64,

    pub oracle_price_fp: i64,
    pub oracle_conf_fp: i64,
    pub oracle_publish_slot: u64,
    pub oracle_ok: bool,
    pub oracle_degraded: bool,

    pub target_delta_bps: u16,
    pub beta_fp: i64,

    pub expected_carry_bps: i32,
    pub config_version: u64,
    pub config_hash: [u8; 32],
}

#[event]
pub struct HedgeConfirmed {
    pub epoch: u64,
    pub slot: u64,
    pub request_id: u64,
    pub hedge_notional_usd: i64,
    pub fill_price_fp: i64,
    pub ref_price_fp: i64,
    pub slippage_bps: u16,
    pub avg_fill_slippage_bps: u16,
    pub hedge_fill_count: u64,
}

#[event]
pub struct HedgeConfirmMissed {
    pub epoch: u64,
    pub slot: u64,
    pub request_id: u64,
    pub since_request_slots: u64,
    pub missed_confirms: u32,
}

#[event]
pub struct PausedSet {
    pub epoch: u64,
    pub slot: u64,
    pub paused: bool,
    pub config_version: u64,
    pub config_hash: [u8; 32],
}

#[event]
pub struct EmergencyModeSet {
    pub epoch: u64,
    pub slot: u64,
    pub emergency_withdraw_enabled: bool,
    pub config_version: u64,
    pub config_hash: [u8; 32],
}

#[event]
pub struct PendingAuthoritySet {
    pub epoch: u64,
    pub slot: u64,
    pub pending_authority: Pubkey,
    pub config_version: u64,
    pub config_hash: [u8; 32],
}

#[event]
pub struct AuthorityAccepted {
    pub epoch: u64,
    pub slot: u64,
    pub old_authority: Pubkey,
    pub new_authority: Pubkey,
    pub new_keeper_admin: Pubkey,
    pub config_version: u64,
    pub config_hash: [u8; 32],
}

#[event]
pub struct KeeperAdminSet {
    pub epoch: u64,
    pub slot: u64,
    pub keeper_admin: Pubkey,
    pub config_version: u64,
    pub config_hash: [u8; 32],
}

#[event]
pub struct KeeperSet {
    pub epoch: u64,
    pub slot: u64,
    pub keeper: Pubkey,
    pub is_added: bool,
    pub keeper_count: u8,
    pub config_version: u64,
    pub config_hash: [u8; 32],
}

#[event]
pub struct PolicyBoundsUpdated {
    pub epoch: u64,
    pub slot: u64,
    pub min_band_bps: u16,
    pub max_band_bps: u16,
    pub min_interval_slots: u64,
    pub max_interval_slots: u64,
    pub config_version: u64,
    pub config_hash: [u8; 32],
}

#[event]
pub struct PolicyStabilityUpdated {
    pub epoch: u64,
    pub slot: u64,
    pub policy_update_min_slots: u64,
    pub max_policy_slew_bps: u16,
    pub hysteresis_bps: u16,
    pub extreme_drift_bps: u16,
    pub config_version: u64,
    pub config_hash: [u8; 32],
}

#[event]
pub struct VolModelUpdated {
    pub epoch: u64,
    pub slot: u64,
    pub vol_mode: u8,
    pub ewma_alpha_bps: u16,
    pub min_samples: u8,
    pub min_return_spacing_slots: u64,
    pub config_version: u64,
    pub config_hash: [u8; 32],
}

#[event]
pub struct OracleConfigUpdated {
    pub epoch: u64,
    pub slot: u64,
    pub oracle_feed_choice: u8,
    pub max_price_age_slots: u64,
    pub max_confidence_bps: u16,
    pub max_price_jump_bps: u16,
    pub config_version: u64,
    pub config_hash: [u8; 32],
}

#[event]
pub struct HedgeSizingUpdated {
    pub epoch: u64,
    pub slot: u64,
    pub target_delta_bps: u16,
    pub beta_fp: i64,
    pub config_version: u64,
    pub config_hash: [u8; 32],
}

#[event]
pub struct RiskCapsUpdated {
    pub epoch: u64,
    pub slot: u64,
    pub max_staked_sol: u64,
    pub max_abs_hedge_notional_usd: i64,
    pub max_hedge_per_sol_usd_fp: i64,
    pub min_reserve_bps: u16,
    pub config_version: u64,
    pub config_hash: [u8; 32],
}

#[event]
pub struct KeeperControlsUpdated {
    pub epoch: u64,
    pub slot: u64,
    pub max_updates_per_epoch: u16,
    pub keeper_bond_required_lamports: u64,
    pub config_version: u64,
    pub config_hash: [u8; 32],
}

#[event]
pub struct ConfirmConfigUpdated {
    pub epoch: u64,
    pub slot: u64,
    pub max_confirm_delay_slots: u64,
    pub config_version: u64,
    pub config_hash: [u8; 32],
}

#[event]
pub struct KeeperBondUpdated {
    pub epoch: u64,
    pub slot: u64,
    pub keeper: Pubkey,
    pub deposited_lamports: u64,
    pub required_lamports: u64,
}

/// reason_code:
/// 1 = interval met
/// 2 = drift met
/// 3 = both met
fn compute_reason_code(interval_ok: bool, drift_ok: bool) -> u8 {
    match (interval_ok, drift_ok) {
        (true, true) => 3,
        (true, false) => 1,
        (false, true) => 2,
        _ => 0,
    }
}

/// -------------------------------
/// Errors
/// -------------------------------

#[error_code]
pub enum ErrorCode {
    #[msg("Unauthorized")]
    Unauthorized,
    #[msg("Program is paused")]
    Paused,

    #[msg("Oracle not ready / missing price")]
    OracleNotReady,
    #[msg("Oracle degraded: hedge blocked unless extreme drift")]
    OracleDegradedHedgeBlocked,

    #[msg("Hedge request too soon (min interval not met)")]
    HedgeTooSoon,
    #[msg("Drift not met (price move within band)")]
    DriftNotMet,

    #[msg("No outstanding hedge request to confirm")]
    NoOutstandingRequest,
    #[msg("Wrong request id")]
    WrongRequestId,

    #[msg("Policy update cooldown not met")]
    PolicyCooldown,

    #[msg("Invalid parameters")]
    InvalidParams,
    #[msg("Math overflow")]
    MathOverflow,
    #[msg("Volatility out of range")]
    VolOutOfRange,

    #[msg("Cap exceeded")]
    CapExceeded,
    #[msg("Leverage exceeded")]
    LeverageExceeded,
    #[msg("Reserve too low (slashing buffer below minimum)")]
    ReserveTooLow,

    #[msg("Keeper rate limited")]
    KeeperRateLimited,
    #[msg("Keeper bond insufficient")]
    KeeperBondInsufficient,
}

/// -------------------------------
/// Oracle (Pyth) Helpers
/// -------------------------------

/// Read Pyth price feed from an AccountInfo, validate staleness/confidence/jump.
/// Returns (spot_fp, ema_fp, conf_fp, publish_time_u64, ok, reason_code)
fn read_pyth_checked(
    acct: &AccountInfo,
    current_slot: u64,
    now_unix_ts: i64,
    max_age_seconds: u64,
    max_conf_bps: u16,
    max_jump_bps: u16,
    last_price_fp: i64,
) -> Result<(i64, i64, i64, u64, bool, u8)> {
    let feed: PriceFeed = load_price_feed_from_account_info(acct).map_err(|_| error!(ErrorCode::OracleNotReady))?;

    // ✅ FIX: pyth_sdk::PriceFeed doesn't expose get_current_price()/get_ema_price()
    // in the Solana Playground-friendly crates. Use the unchecked getters and do
    // our own gating (staleness/confidence/jump) below.
    let spot: Price = feed.get_price_unchecked();
    let ema: Price = feed.get_ema_price_unchecked();

    // Convert to fp 1e6; publish_time comes from Price.publish_time (unix seconds)
    let (spot_fp, spot_conf_fp, spot_publish_time) = pyth_price_to_fp_and_time(&spot)?;
    let (ema_fp, _ema_conf_fp, _ema_publish_time) = pyth_price_to_fp_and_time(&ema)?;

    // Basic sanity (treat non-positive as "not ready")
    if spot_fp <= 0 || spot_fp > MAX_PRICE_FP || ema_fp <= 0 || ema_fp > MAX_PRICE_FP {
        return Ok((0, 0, 0, spot_publish_time, false, 10));
    }

    // Staleness (seconds)
    // If publish_time is 0 or in the future, fail safe.
    if spot_publish_time == 0 {
        return Ok((spot_fp, ema_fp, spot_conf_fp, spot_publish_time, false, 11));
    }
    let now_u64 = if now_unix_ts <= 0 { 0u64 } else { now_unix_ts as u64 };
    if now_u64 < spot_publish_time {
        return Ok((spot_fp, ema_fp, spot_conf_fp, spot_publish_time, false, 12));
    }
    let age_sec = now_u64 - spot_publish_time;
    if age_sec > max_age_seconds {
        return Ok((spot_fp, ema_fp, spot_conf_fp, spot_publish_time, false, 1));
    }

    // Confidence gating: conf <= max_conf_bps * price
    let max_conf_fp = (spot_fp as i128)
        .checked_mul(max_conf_bps as i128)
        .ok_or(ErrorCode::MathOverflow)?
        / (BPS_DENOM as i128);
    if (spot_conf_fp as i128) > max_conf_fp.max(0) {
        return Ok((spot_fp, ema_fp, spot_conf_fp, spot_publish_time, false, 2));
    }

    // Jump check vs last price (still in fp-space)
    if last_price_fp > 0 {
        let jump = compute_price_drift_bps(spot_fp, last_price_fp)?;
        if jump > max_jump_bps {
            return Ok((spot_fp, ema_fp, spot_conf_fp, spot_publish_time, false, 3));
        }
    }

    // (Optional) also sanity-check that publish_time isn't wildly old relative to slot cadence.
    // We keep it simple here; `current_slot` is unused but kept in signature for future extension.
    let _ = current_slot;

    Ok((spot_fp, ema_fp, spot_conf_fp, spot_publish_time, true, 0))
}

/// Choose feed per config:
/// Returns (feed_used, spot_fp, ema_fp, conf_fp, publish_time_u64, ok, reason_code)
fn read_pyth_best_effort(
    choice: u8,
    sol_usd: &AccountInfo,
    sol_usdc: &AccountInfo,
    current_slot: u64,
    now_unix_ts: i64,
    max_age_seconds: u64,
    max_conf_bps: u16,
    max_jump_bps: u16,
    last_price_fp: i64,
) -> Result<(u8, i64, i64, i64, u64, bool, u8)> {
    let try_one = |acct: &AccountInfo| -> Result<(i64, i64, i64, u64, bool, u8)> {
        read_pyth_checked(
            acct,
            current_slot,
            now_unix_ts,
            max_age_seconds,
            max_conf_bps,
            max_jump_bps,
            last_price_fp,
        )
    };

    match choice {
        x if x == OracleFeedChoice::SolUsd as u8 => {
            let (p, e, c, t, ok, r) = try_one(sol_usd)?;
            Ok((OracleFeedChoice::SolUsd as u8, p, e, c, t, ok, r))
        }
        x if x == OracleFeedChoice::SolUsdc as u8 => {
            let (p, e, c, t, ok, r) = try_one(sol_usdc)?;
            Ok((OracleFeedChoice::SolUsdc as u8, p, e, c, t, ok, r))
        }
        _ => {
            // AutoPreferUsdThenUsdc
            let (p1, e1, c1, t1, ok1, r1) = try_one(sol_usd)?;
            if ok1 {
                return Ok((OracleFeedChoice::SolUsd as u8, p1, e1, c1, t1, ok1, r1));
            }
            let (p2, e2, c2, t2, ok2, r2) = try_one(sol_usdc)?;
            if ok2 {
                return Ok((OracleFeedChoice::SolUsdc as u8, p2, e2, c2, t2, ok2, r2));
            }
            Ok((
                OracleFeedChoice::SolUsd as u8,
                p1,
                e1,
                c1,
                t1,
                false,
                if r1 != 0 { r1 } else { r2.max(1) },
            ))
        }
    }
}

/// Convert pyth_sdk::Price to fp(1e6) + publish_time (unix seconds).
fn pyth_price_to_fp_and_time(p: &Price) -> Result<(i64, i64, u64)> {
    let expo = p.expo;
    let price_i128 = p.price as i128;
    let conf_i128 = p.conf as i128;

    let (price_fp_i128, conf_fp_i128) = scale_to_fp_1e6(price_i128, conf_i128, expo)?;

    let price_fp = clamp_i128_to_i64(price_fp_i128, 0, MAX_PRICE_FP)?;
    let conf_fp = clamp_i128_to_i64(conf_fp_i128, 0, MAX_PRICE_FP)?;

    let publish_time_u64 = if p.publish_time <= 0 { 0u64 } else { p.publish_time as u64 };

    Ok((price_fp, conf_fp, publish_time_u64))
}

fn scale_to_fp_1e6(price: i128, conf: i128, expo: i32) -> Result<(i128, i128)> {
    // target fp is 1e6 -> exponent adjust by +6
    let expo_adj = (expo as i64).checked_add(6).ok_or(ErrorCode::MathOverflow)?;
    if expo_adj >= 0 {
        let m = pow10_i128(expo_adj as u32)?;
        Ok((
            price.checked_mul(m).ok_or(ErrorCode::MathOverflow)?,
            conf.checked_mul(m).ok_or(ErrorCode::MathOverflow)?,
        ))
    } else {
        let d = pow10_i128((-expo_adj) as u32)?;
        Ok((price / d.max(1), conf / d.max(1)))
    }
}

fn pow10_i128(exp: u32) -> Result<i128> {
    let mut v: i128 = 1;
    for _ in 0..exp {
        v = v.checked_mul(10).ok_or(ErrorCode::MathOverflow)?;
    }
    Ok(v)
}

fn clamp_i128_to_i64(x: i128, min: i64, max: i64) -> Result<i64> {
    if x < (min as i128) {
        return Ok(min);
    }
    if x > (max as i128) {
        return Ok(max);
    }
    Ok(x as i64)
}

/// -------------------------------
/// Deterministic math helpers
/// -------------------------------

fn abs_i64(x: i64) -> i64 {
    if x < 0 { -x } else { x }
}

fn weighted_vol_score_bps(realized_bps: u16, implied_bps: u16, w_realized_bps: u16, w_implied_bps: u16) -> Result<u16> {
    let wr = w_realized_bps as u64;
    let wi = w_implied_bps as u64;

    let sum = (wr.checked_mul(realized_bps as u64).ok_or(ErrorCode::MathOverflow)?)
        .checked_add(wi.checked_mul(implied_bps as u64).ok_or(ErrorCode::MathOverflow)?)
        .ok_or(ErrorCode::MathOverflow)?;

    Ok((sum / (BPS_DENOM as u64)).min(MAX_VOL_BPS as u64) as u16)
}

fn compute_realized_vol_bps_mode(mode: u8, returns: &[i32; N_RETURNS], ewma_var_fp2: u128) -> Result<u16> {
    if mode == VolMode::Ewma as u8 {
        let std_fp = isqrt_u128(ewma_var_fp2.min(MAX_VAR_FP2));
        return fp_to_bps(std_fp);
    }
    if mode == VolMode::Mad as u8 {
        return mad_vol_bps(returns);
    }
    stdev_vol_bps(returns)
}

fn fp_to_bps(std_fp: u128) -> Result<u16> {
    let bps_u128 = std_fp
        .checked_mul(BPS_DENOM as u128)
        .ok_or(ErrorCode::MathOverflow)?
        / (RET_FP_SCALE as u128);
    Ok((bps_u128.min(MAX_VOL_BPS as u128)) as u16)
}

fn stdev_vol_bps(returns: &[i32; N_RETURNS]) -> Result<u16> {
    let mut sum: i64 = 0;
    for &r in returns.iter() {
        sum = sum.checked_add(r as i64).ok_or(ErrorCode::MathOverflow)?;
    }
    let mean: i64 = sum / (N_RETURNS as i64);

    let mut var_acc: u128 = 0;
    for &r in returns.iter() {
        let dev: i64 = (r as i64).checked_sub(mean).ok_or(ErrorCode::MathOverflow)?;
        let dev_abs: u128 = if dev < 0 { (-dev) as u128 } else { dev as u128 };
        let dev_sq = dev_abs.checked_mul(dev_abs).ok_or(ErrorCode::MathOverflow)?;
        var_acc = var_acc.checked_add(dev_sq).ok_or(ErrorCode::MathOverflow)?;
    }
    let mut var = var_acc / (N_RETURNS as u128);
    if var > MAX_VAR_FP2 {
        var = MAX_VAR_FP2;
    }
    let std_fp = isqrt_u128(var);
    fp_to_bps(std_fp)
}

fn mad_vol_bps(returns: &[i32; N_RETURNS]) -> Result<u16> {
    let mut buf = *returns;
    let med = median_i32(&mut buf);

    let mut devs = [0i32; N_RETURNS];
    for i in 0..N_RETURNS {
        let d = (returns[i] as i64 - med as i64);
        let a = if d < 0 { -d } else { d };
        devs[i] = a.min(i32::MAX as i64) as i32;
    }

    let mut devs_copy = devs;
    let mad_fp = median_i32(&mut devs_copy) as u128;

    let mad_scaled = mad_fp.checked_mul(14826u128).ok_or(ErrorCode::MathOverflow)? / 10000u128;
    fp_to_bps(mad_scaled)
}

fn median_i32(arr: &mut [i32; N_RETURNS]) -> i32 {
    for i in 1..N_RETURNS {
        let key = arr[i];
        let mut j = i;
        while j > 0 && arr[j - 1] > key {
            arr[j] = arr[j - 1];
            j -= 1;
        }
        arr[j] = key;
    }
    let a = arr[(N_RETURNS / 2) - 1] as i64;
    let b = arr[(N_RETURNS / 2)] as i64;
    ((a + b) / 2) as i32
}

fn ewma_update_u128(prev: u128, x: u128, alpha_bps: u16) -> Result<u128> {
    let a = alpha_bps as u128;
    let one_minus = (BPS_DENOM as u128).checked_sub(a).ok_or(ErrorCode::MathOverflow)?;

    let left = prev.checked_mul(one_minus).ok_or(ErrorCode::MathOverflow)? / (BPS_DENOM as u128);
    let right = x.checked_mul(a).ok_or(ErrorCode::MathOverflow)? / (BPS_DENOM as u128);

    Ok(left.checked_add(right).ok_or(ErrorCode::MathOverflow)?)
}

fn map_u16_by_bps(score_bps: u16, min_v: u16, max_v: u16) -> Result<u16> {
    if min_v == max_v {
        return Ok(min_v);
    }
    require!(min_v <= max_v, ErrorCode::InvalidParams);

    let span: u32 = (max_v - min_v) as u32;
    let add: u32 = (score_bps as u32).checked_mul(span).ok_or(ErrorCode::MathOverflow)? / (BPS_DENOM as u32);

    Ok((min_v as u32).checked_add(add).ok_or(ErrorCode::MathOverflow)? as u16)
}

fn map_u64_by_bps(score_bps: u16, min_v: u64, max_v: u64) -> Result<u64> {
    if min_v == max_v {
        return Ok(min_v);
    }
    require!(min_v <= max_v, ErrorCode::InvalidParams);

    let span: u128 = (max_v - min_v) as u128;
    let add: u128 = (score_bps as u128).checked_mul(span).ok_or(ErrorCode::MathOverflow)? / (BPS_DENOM as u128);

    Ok((min_v as u128).checked_add(add).ok_or(ErrorCode::MathOverflow)? as u64)
}

fn slew_limit_u16(current: u16, target: u16, max_slew_bps: u16) -> Result<u16> {
    if current == target {
        return Ok(current);
    }
    if current == 0 {
        return Ok(target);
    }
    let max_delta = ((current as u32).checked_mul(max_slew_bps as u32).ok_or(ErrorCode::MathOverflow)? / (BPS_DENOM as u32)).max(1);

    let cur = current as i32;
    let tar = target as i32;
    let diff = tar - cur;

    let limited = if diff.abs() as u32 <= max_delta {
        target
    } else if diff > 0 {
        (cur + (max_delta as i32)) as u16
    } else {
        (cur - (max_delta as i32)) as u16
    };
    Ok(limited)
}

fn slew_limit_u64(current: u64, target: u64, max_slew_bps: u16) -> Result<u64> {
    if current == target {
        return Ok(current);
    }
    if current == 0 {
        return Ok(target);
    }
    let max_delta = ((current as u128).checked_mul(max_slew_bps as u128).ok_or(ErrorCode::MathOverflow)? / (BPS_DENOM as u128)).max(1);

    if target > current {
        let v = (current as u128).checked_add(max_delta).ok_or(ErrorCode::MathOverflow)? as u64;
        Ok(v.min(target))
    } else {
        let cur = current as u128;
        let sub = if max_delta > cur { cur } else { max_delta };
        let v = (cur - sub) as u64;
        Ok(v.max(target))
    }
}

fn compute_price_drift_bps(current_price_fp: i64, anchor_price_fp: i64) -> Result<u16> {
    if current_price_fp <= 0 {
        return Ok(0);
    }
    if anchor_price_fp <= 0 {
        return Ok(MAX_VOL_BPS);
    }

    let p = current_price_fp as i128;
    let p0 = anchor_price_fp as i128;

    let diff = if p >= p0 { p - p0 } else { p0 - p };
    let bps = diff.checked_mul(BPS_DENOM as i128).ok_or(ErrorCode::MathOverflow)? / p0.max(1);

    Ok((bps.min(MAX_VOL_BPS as i128)) as u16)
}

fn compute_target_hedge_notional_usd_delta(staked_sol: u64, price_fp: i64, target_delta_bps: u16, beta_fp: i64) -> Result<i64> {
    if staked_sol == 0 || price_fp <= 0 {
        return Ok(0);
    }
    let staked_value = (staked_sol as i128).checked_mul(price_fp as i128).ok_or(ErrorCode::MathOverflow)? / (PRICE_FP_SCALE as i128);

    let with_delta = staked_value.checked_mul(target_delta_bps as i128).ok_or(ErrorCode::MathOverflow)? / (BPS_DENOM as i128);

    let with_beta = with_delta.checked_mul(beta_fp as i128).ok_or(ErrorCode::MathOverflow)? / (PRICE_FP_SCALE as i128);

    let n = with_beta.abs().min(i64::MAX as i128) as i64;
    Ok(-n)
}

fn compute_slippage_bps(fill_price_fp: i64, ref_price_fp: i64) -> Result<u16> {
    require!(ref_price_fp > 0, ErrorCode::InvalidParams);
    let f = fill_price_fp as i128;
    let r = ref_price_fp as i128;
    let diff = if f >= r { f - r } else { r - f };
    let bps = diff.checked_mul(BPS_DENOM as i128).ok_or(ErrorCode::MathOverflow)? / r.max(1);
    Ok((bps.min(MAX_VOL_BPS as i128)) as u16)
}

fn ewma_u16(prev: u16, x: u16, alpha_bps: u16) -> Result<u16> {
    let a = alpha_bps as u32;
    let one_minus = (BPS_DENOM as u32).checked_sub(a).ok_or(ErrorCode::MathOverflow)?;

    let left = (prev as u32).checked_mul(one_minus).ok_or(ErrorCode::MathOverflow)? / (BPS_DENOM as u32);
    let right = (x as u32).checked_mul(a).ok_or(ErrorCode::MathOverflow)? / (BPS_DENOM as u32);

    let out = left.checked_add(right).ok_or(ErrorCode::MathOverflow)?;
    Ok(out.min(u16::MAX as u32) as u16)
}

/// Funding-aware bias
fn carry_policy_bias_bps(expected_carry_bps: i32) -> Result<(i16, i16)> {
    if expected_carry_bps >= 50 {
        return Ok((200, 200)); // +2%
    }
    if expected_carry_bps <= -50 {
        return Ok((-200, -200)); // -2%
    }
    Ok((0, 0))
}

fn apply_bps_bias_u16(v: u16, bias_bps: i16) -> Result<u16> {
    if bias_bps == 0 {
        return Ok(v);
    }
    let v_i128 = v as i128;
    let adj = v_i128.checked_mul(bias_bps as i128).ok_or(ErrorCode::MathOverflow)? / (BPS_DENOM as i128);
    let out = v_i128.checked_add(adj).ok_or(ErrorCode::MathOverflow)?;
    Ok(out.clamp(0, u16::MAX as i128) as u16)
}

fn apply_bps_bias_u64(v: u64, bias_bps: i16) -> Result<u64> {
    if bias_bps == 0 {
        return Ok(v);
    }
    let v_i128 = v as i128;
    let adj = v_i128.checked_mul(bias_bps as i128).ok_or(ErrorCode::MathOverflow)? / (BPS_DENOM as i128);
    let out = v_i128.checked_add(adj).ok_or(ErrorCode::MathOverflow)?;
    Ok(out.max(0) as u64)
}

fn isqrt_u128(n: u128) -> u128 {
    if n == 0 {
        return 0;
    }
    let mut x0 = n;
    let mut x1 = (x0 + 1) >> 1;
    while x1 < x0 {
        x0 = x1;
        x1 = (x1 + n / x1) >> 1;
    }
    x0
}

/*
USAGE NOTES (quick):
1) initialize_vault(params)
2) keeper_admin add keepers: add_keeper(...)
3) update_oracle_price(signer=keeper, pass pyth accounts)
4) update_implied_vol / update_carry_inputs (optional)
5) update_epoch_and_policy (keeper)
6) request_hedge (anyone) -> emits HedgeRequested intent
7) confirm_hedge (keeper) -> record execution + slippage stats
*/
