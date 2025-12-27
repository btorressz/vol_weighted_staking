# vol_weighted_staking

# 📈 Volatility-Weighted Staking Allocator (VWSA) — Anchor + Pyth Oracle

A single-file **Anchor** program that simulates a staking vault and a **delta-hedge policy** that adapts to market volatility using **Pyth** price feeds.

It does **no token transfers** and makes **no CPI calls** — all balances, PnL, and hedge changes are accounted for **deterministically on-chain** for testing, research, and vault-policy prototyping.

---

## 🧠 What this program is

This program models a vault that:

- Tracks staking exposure in SOL (`staked_sol`)
- Tracks a reserve / slashing buffer in SOL (`reserve_sol`)
- Tracks a simulated perp hedge notional in USD (`hedge_notional_usd`)
- Pulls SOL/USD and/or SOL/USDC pricing from Pyth
- Computes realized volatility on-chain from oracle returns (deterministic)
- Combines realized vol + keeper-fed implied vol into a single `vol_score`
- Uses that score to adjust hedge behavior:
  - How wide the “don’t hedge unless price moves” band is
  - How often hedges are allowed (minimum interval)

The result is a policy engine that says:  
**“If volatility is high, hedge more often / react faster. If volatility is low, hedge less.”**

---

## 🎯 Core idea (why it’s useful)

In real systems, delta-hedged staking strategies need:

- A reliable spot price and smoothing signal (EMA)
- A robust way to measure volatility
- Guardrails to prevent over-trading or oracle manipulation
- A two-step “intent then execution” hedge flow (on-chain signal, off-chain execution)

This vault is a clean simulation of that decision logic, suitable for:

- Prototyping keeper automation
- Experimenting with volatility regimes
- Validating policy stability knobs (hysteresis, slew limits, cooldowns)
- Anchoring later upgrades to real perps via CPI

---

## 🧱 Vault State (what’s stored)

The main account is `VaultState`, which stores:

### ✅ Roles & governance
- `authority` (owner)
- `pending_authority` (two-step transfer)
- `keeper_admin` (manages keepers)
- `keepers[]` (up to 8)

### ✅ Simulated exposures
- `staked_sol` — user “staked” SOL (simulated)
- `reserve_sol` — reserve buffer for slashing / safety (simulated)
- `hedge_notional_usd` — perp hedge in USD notional (simulated)

### ✅ Oracle snapshot
- `oracle_price_fp` — spot price (scaled 1e6)
- `oracle_ema_price_fp` — EMA price (scaled 1e6)
- `oracle_conf_fp` — confidence interval (scaled 1e6)
- `oracle_publish_slot` — publish time (unix seconds)
- `oracle_ok` — whether oracle passed gating checks
- `oracle_degraded` — circuit breaker mode

### ✅ Volatility engine
- `returns_ring[32]` — rolling oracle returns buffer
- `realized_vol_bps` — realized volatility
- `implied_vol_bps` — keeper-fed implied volatility
- `vol_score_bps` — weighted blend of realized + implied
- `vol_mode` — STDEV, EWMA variance, or MAD (robust)

### ✅ Hedge policy outputs
- `band_bps` — drift band required to trigger a hedge
- `min_hedge_interval_slots` — cooldown between hedge intents
- Hysteresis + slew controls to prevent noisy oscillation

---
