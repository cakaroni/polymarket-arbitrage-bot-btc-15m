# Target Strategy Analysis (Corrected)

Analysis of the target's trading history. Polymarket and the target's actual behavior are different from a naive "buy both when sum ≤ 0.98" strategy.

---

## 1. Polymarket mechanics

- The **sum of Up and Down token prices is intentionally ≥ 1.00** (typically 1.00–1.01) at any moment.
- There is **no** real-world situation where "up_ask + down_ask ≤ 0.98" at the same time. So a bot that only buys when sum ≤ 0.98 will (correctly) never trade.

---

## 2. How the target actually trades

From the target's history (eth-15m-1, btc-15m-3, eth-1h, btc-1h):

- The target **does not buy both sides at the same moment**. It buys **one side at a time**: sometimes a run of Up, sometimes a run of Down.
- It **tracks balance** for each market: how many Up shares at what average price, how many Down shares at what average price.
- On **every price update** it effectively asks:
  - "I have X Down at avg $d. Current Up ask is $u. If I buy N Up at $u, my cost per pair is (X*d + N*u) / min(X, N) or similar—is that ≤ $1 (or acceptable)?"
  - "I have Y Up at avg $u. Current Down ask is $d. If I buy N Down at $d, cost per pair = … — is that acceptable?"
- It **rebalances dynamically**: if the market trends Up (prices 0.77, 0.79, 0.81), it keeps buying Up carefully while considering the Down balance; if the market reverts to Down, it buys more Down to recover or minimize loss so that whichever side wins, loss is near zero.
- So the strategy is **balance-aware, one-side-at-a-time, cost-per-pair and PnL in every case**.

---

## 3. Correct bot logic (target-like)

1. **State per market:** Track `up_shares`, `up_avg_price` (or up_cost), `down_shares`, `down_avg_price` (or down_cost). Total cost = up_shares×up_avg + down_shares×down_avg.

2. **Cost per pair:** After adding N Up at current up_ask, new pairs = min(up_shares+N, down_shares). Cost per pair = (up_cost + N×up_ask + down_cost) / new_pairs (when new_pairs > 0). Similarly for adding Down.

3. **When to buy (one side at a time):**
   - **Buy Up** if adding `size` Up at up_ask gives cost per pair ≤ `cost_per_pair_max` (e.g. 1.0 or 1.01). If we have no Down yet, we can still buy Up as the first leg (no "pair" yet).
   - **Buy Down** if adding `size` Down at down_ask gives cost per pair ≤ `cost_per_pair_max`. If we have no Up yet, we can buy Down as the first leg.
   - Each tick: at most one action—either buy Up, or buy Down, or do nothing. Prefer the side that gives lower cost per pair or that rebalances (we're underweight that side).

4. **PnL in every case:**  
   - If Up wins: PnL = up_shares×1.0 − total_cost.  
   - If Down wins: PnL = down_shares×1.0 − total_cost.  
   The bot should use these to decide whether adding more Up or more Down improves or protects the position.

5. **Size:** Same as before: base size per market type (e.g. BTC 15m=24), optionally reduced near market end (time-based).

---

## 4. Config (strategy)

- **cost_per_pair_max** (e.g. 1.0 or 1.01): Add a side only if the resulting cost per pair (after that add) is ≤ this. Replaces the old "sum_target" (buy both when sum ≤ 0.98).
- Keep: cooldown (0 = react every tick), size_reduce_after_secs, size_min_ratio, size_min_shares, base sizes per market.

---

## 5. Summary

| Wrong (previous)              | Correct (target-like)                          |
|------------------------------|-----------------------------------------------|
| Buy both when sum ≤ 0.98     | Buy **one side at a time** when it improves cost per pair / balance |
| Sum of prices can be &lt; 1   | Sum of prices is ≥ 1.00 on Polymarket         |
| Same moment for Up and Down  | Alternate Up and Down over time; rebalance    |
| No position memory           | Track Up and Down inventory and cost; PnL in every case |
