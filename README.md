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
