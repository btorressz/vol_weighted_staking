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

  ## 🔮 Oracle logic (Pyth) — how prices are validated

The program reads from **two Pyth price accounts**:

- SOL/USD feed
- SOL/USDC feed

A configurable `oracle_feed_choice` selects:

- SOL/USD only
- SOL/USDC only
- Auto: prefer USD feed, fallback to USDC feed

### 🧯 Oracle safety gates

Each update checks:

- **Staleness**: publish time must be within `max_price_age_slots`  
  _Note: in this implementation, `max_price_age_slots` is treated as seconds._
- **Confidence**: confidence interval must be below `max_confidence_bps` of price
- **Jump sanity**: price change vs last accepted price must be below `max_price_jump_bps`
- **Basic sanity**: price must be positive and within bounds

If checks fail:

- `oracle_ok = false`
- `oracle_degraded = true`
- Policy updates freeze (circuit breaker behavior)

---

## 🌪️ Realized volatility (computed on-chain)

Whenever oracle updates are valid, the vault records an oracle return into a ring buffer (32 samples).  
Returns are clamped to avoid extreme outliers and spaced out by `min_return_spacing_slots`.

### Available volatility modes

**1) 📏 STDEV proxy**  
Computes standard deviation over the return buffer.

**2) ⚡ EWMA variance**  
Maintains an EWMA variance accumulator (`ewma_var_fp2`) and converts it into a standard deviation proxy.

**3) 🧱 MAD proxy (robust)**  
Computes median absolute deviation and scales it to approximate standard deviation behavior.

The output is normalized into basis points:

- `realized_vol_bps` ∈ `[0, 10_000]`

---

## 🧮 Vol score (realized + implied)

Keepers can optionally feed `implied_vol_bps`.

The vault blends:
- Realized volatility (on-chain)
- Implied volatility (keeper-fed)

Using weights that must sum to 10,000 bps:
- `vol_weight_realized_bps`
- `vol_weight_implied_bps`

Result:
- `vol_score_bps` (0–10,000)

This score drives the hedge policy mapping.

---

## 🧭 Policy engine (band + hedge interval)

Each epoch update produces:

### ✅ `band_bps`
“How big the EMA drift must be before hedging is allowed.”

### ✅ `min_hedge_interval_slots`
“How long must pass between hedge intent requests.”

Both are mapped from `vol_score_bps` into configured bounds:
- `min_band_bps → max_band_bps`
- `min_interval_slots → max_interval_slots`

### 🧊 Stability controls

To avoid thrashing:
- Policy cooldown: `policy_update_min_slots`
- Hysteresis: only adjust if `vol_score` changes enough
- Slew rate limiting: gradual changes using `max_policy_slew_bps`

---

---
