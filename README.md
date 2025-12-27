# vol_weighted_staking

# 📈 Volatility-Weighted Staking Allocator (VWSA) — Anchor + Pyth Oracle

A single-file **Anchor** program that simulates a staking vault and a **delta-hedge policy** that adapts to market volatility using **Pyth** price feeds.

It does **no token transfers** and makes **no CPI calls** — all balances, PnL, and hedge changes are accounted for **deterministically on-chain** for testing, research, and vault-policy prototyping.

---
