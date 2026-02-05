#Example 1: No position, trend is Up

BTC 15m: Up ask 0.52, Down ask 0.49. Last 5 ticks: Up went from 0.50 → 0.52 → UpRising.
No position (0 Up, 0 Down).
Rule: “No position + UpRising” → buy Up, up to 2 times.
Action: Buy 24 Up @ 0.52, Buy 24 Up @ 0.52, Should remember this position (Buy price and Amount of this time buying)
Position: 48 Up @ 0.52. PnL if Up wins = 48 − 24.96, if Down wins = −24.96.


#Example 2: Only Up, Down gets cheap → lock

Position: 48 Up @ 0.52 (no Down).
Market: Rising Up, now Up 0.57, Down 0.44 (Down cheap).
Lock rule: “among the previosu up tokens' prices + Down price < $1 " $0.52 + $0.44 < $0.99 → yes
Action: Buy 24 Down @ 0.44, Buy 24 Down @0.44
New pairs: 48. Cost per pair ≈ 0.52 + 0.44 = 0.96 (0.99).
Position: 48 Up, 48 Down. We’ve “locked” 24 pairs at good cost; the rest is still riding Up.
If we had not required “Down cheap”: when Down was 0.40 we would not lock (0.40 &gt; 0.35), and we’d keep buying Up to ride the winner.

#Example 3: Only Up, Down gets expensive -> create new position

Position: 48 Up @ 0.52 (no Down).
Market: Rising Down Up 0.47, Down 0.54 (Up cheap).
Expansion Rule: "No matching Up tokens' prices can lock with Down @0.54 + Down token price is rising + Pnl if Down wins is lower (or negative) than Up wins case" -> yes
Action: Buy 24 Down @ 0.54
No pairs:
Position: 48 Up @0.52, 24 Down @0.54. We’ve "unlocked" two positions.

#Example 4: Have both sides; Up winning → ride Up

Position: 48 Up @ 0.52, 24 Down @ 0.54. PnL if Up wins = 48 - 48 * 0.52 - 24 * 0.54, if Down wins = 24 - 24 * 0.54 - 48 * 0.52
Market: Rising Up, Up $0.56, Down 0.45. Trend: UpRising.
Lock? Among the positions, we can lock with current Down @0.45.
Lock rule: “among the previous up tokens' prices + Down price < $1 " $0.52 + $0.45 = $0.97 < $0.99 → yes
Action: Buy 24 Down @ 0.45, Buy 24 Down @ 0.45,
New pairs: 48. Cost of pair ≈ 0.52 + 0.44 = 0.96 (0.99).
Unlocked Position: 24 Down @0.54
Position: 48 Up @0.52(average price), 72 Down @0.48(average price),  PnL if Up wins increases 48 - 48 * 0.52 - 72 * 0.48, if Down wins drops 72 - 72 * 0.48 - 48 * 0.52


#Example 5: Have both sides; Up winning → Down Up

Position: 48 Up @ 0.52, 24 Down @ 0.54. PnL if Up wins = 48 - 48 * 0.52 - 24 * 0.54, if Down wins = 24 - 24 * 0.54 - 48 * 0.52
Market: Rising Up, Up $0.41, Down $0.60. Trend: Down Rising.
Expansion Rule: "No matching Up tokens' prices can lock with Down @0.54 + Down token price is rising + Pnl if Down wins is lower (or negative) than Up wins case" -> yes
Action: Buy 24 Down @ 0.60, Buy 24 Down @ 0.60 (in this case, buy amount is related with Pnl expectation, so bot should buy enough Down token till the Pnl of down token win case be larger(or positive) than Up case),
New Position: 48 Down @0.60
Unlocked Position: 48 Up @ 0.52, 24 Down @ 0.54, 48 Down @0.60
Position: 48 Up @0.52(average price), 72 Down @0.58(average price),  PnL if Up wins increases 48 - 48 * 0.52 - 72 * 0.58, if Down wins drops 72 - 72 * 0.58 - 48 * 0.52


#Example 6: Have both sides; No price progress for both token, 

Position: 48 Up @0.52(average price), 72 Down @0.58(average price),  PnL if Up wins increases 48 - 48 * 0.52 - 72 * 0.58, if Down wins drops 72 - 72 * 0.58 - 48 * 0.52
Market: No progress for both tokens' prices Up $0.41, Down $0.60
Expansion Rule: "No matching Up tokens' prices can lock with Down @0.54 + Down token price is rising + Pnl if Down wins is lower (or negative) than Up wins case" -> No
Action: Don't buy, keep monitoring prices

#Example 7: Market closes → actual PnL

Market period ends; API later reports market.closed = true.
Closure task (runs every 20s) sees it, fetches the market, finds the winner (Up or Down).
Payout: If Up won → you get $1 per Up share; if Down won → $1 per Down share.
Actual PnL = Payout − total cost for that market.

