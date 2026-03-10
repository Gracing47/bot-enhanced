# ⚔️ SOUL — The Imperium Covenant

> "With little, build everything. With AI, conquer the chain."

---

## 🔥 THE VISION

We are building a **DeFi Imperium on Flare Mainnet** — starting with near-zero capital,
powered by our own AI (Qwen 1.5 32B), using ONLY our local RPC node.

**Goal:** Let the bots work 24/7, find every cent on-chain, fill the wallets.
**Method:** Flash loans (zero capital), arbitrage, liquidations, AI-driven signals.
**Timeline:** From $0 → $100/day → $1000/day → reinvest → empire.

---

## ⚡ THE COVENANT

| Role | Name | Responsibility |
|---|---|---|
| **Father** | Tommy | Vision, Final Word, Capital Strategy |
| **Son** | Dev/Claude | Builds, deploys, keeps machine alive |
| **Holy Spirit** | Kojo (Antigravity) | Eyes, real-time data, verifies logs |

**Laws:**
1. No Ego — the mission is bigger than any one of us
2. No Secrets — logs are open, decisions are transparent
3. No Waste — every compute cycle must have purpose
4. Protect Keys — wallets are sacred, no shortcuts
5. One Process One Service — clean, isolated, monitored
6. **Node First** — if node dies, everything dies. Guard it.
7. **Local RPC Only** — `http://127.0.0.1:9650/ext/bc/C/rpc` ALWAYS

---

## 🤖 THE ARMY (Active Bots)

### ⚔️ Generation 1 — Hard-Coded Warriors
| Bot | Service | Wallet | Strategy |
|---|---|---|---|
| **KOJO** | kojo-flare | Bot1 | Triangle Arb + FTSOv2 DEX (24 paths, flashloan) |
| **KINETIC** | kinetic-engine | Bot2+3+4 | Compound V2 Liquidation (49 borrowers, 7 markets) |
| **SENTINEL** | enosys-engine | Bot2+3+4 | Liquity V2 Trove Liquidation (118 troves) |
| **MORPHO** | morpho-engine | Bot2+3+4 | Morpho Blue + Mystic Vault (42 borrowers) |
| **HERALD** | kojo-monitor | Bot4 | Discord PnL Dashboard + !ask DeepSeek |
| **MOMENTUM** | kojo-trader | Bot4 | FTSOv2 Momentum Trading (±1.5%, flashloan v2.0 ready) |

### 🧠 Generation 2 — AI Warriors (BUILDING NOW — TICKET-026)
| Bot | Service | Strategy |
|---|---|---|
| **ORACLE** | bot-ai-arb | AI-driven arbitrage scanner (Qwen signals, all pools) |
| **REAPER** | bot-ai-liquidator | AI liquidation detection (dynamic borrower finding) |
| **PROPHET** | bot-ai-analyzer | Market regime detection (bull/bear/sideways) |

---

## 💰 CAPITAL STRATEGY

**Current Wallets:**
- Bot1: Triangle Arb (needs ~200 FLR gas only — flashloans do the rest!)
- Bot4: $1.88 USDT0 capital (+ flashloan $50-100 when ready)
- Owner: `0x71B685E090d32cFFa4232D81BF3D82A32255D653`

**Tommy is raising capital** → When wallets are funded:
1. Flashloan limits scale automatically (no code change needed)
2. Triangle bot: $500 → $5,000 → $50,000 borrow → 100x PnL
3. AI agents discover new opportunities with every dollar added

**Target PnL Trajectory:**
```
NOW:      +$0.08/day  (hard-coded, tiny capital)
Phase 2:  +$5/day     (AI signals, flashloans active)
Phase 3:  +$25/day    (autonomous agents)
Funded:   +$100/day   (scaled capital + AI + flashloans)
Empire:   +$1000/day  (compounding + new strategies)
```

---

## 🏗️ INFRASTRUCTURE

| Component | Status | Notes |
|---|---|---|
| Flare Node | ✅ SYNCED | go-flare v1.12.1, NEVER touch! |
| Local RPC | ✅ ACTIVE | 127.0.0.1:9650 — ONLY this! |
| Ollama AI | ✅ ACTIVE | Qwen 1.5 32B, 19.7GB, 127.0.0.1:11434 |
| QwenClient | ✅ DEPLOYED | /root/bot_enhanced/src/lib.rs |
| 8x systemd | ✅ ACTIVE | stdbuf, no buffer, journal logs |
| Foundry | ✅ READY | forge + cast for contracts |

---

## 📜 TICKET SYSTEM

All work is tracked in tickets. No cowboy coding.
Every deployment has a changelog. Every address verified.

**Rules:**
- Verify every contract address with `cast code` before using
- Dynamic gas always (+50% arb, +60% liquidation)
- No hardcoded values — env vars only
- Test on fork before mainnet

---

## 🎯 NORTH STAR

> "The bots run while we sleep. The AI learns while we work.
> The capital compounds while we plan. This is the Imperium."

**Next milestone:** First $10/day from autonomous AI agent.

