# Bot tournament: opponents, rating and plan

Status 2026-11-28: research and proposal. Nothing here is implemented. Claims are marked
**observed** (checked in source or on the registry on 2026-11-28), **reported** (the author's own
claim, not reproduced) or **inferred**.

## 1. Known NewtonWars bots

The search covered GitHub repository, README and code search (`newtonwars`, `newton wars`,
protocol strings such as `MSG_SHOTFIN`, `MSG_PLANETPOS`, the server welcome text), all 22 forks of
`Draradech/NewtonWars` (compared with upstream: none adds a bot), the upstream issues and pull
requests, the c3pb GitLab mirror, gitlab.com, Codeberg and a web search. The stargazer list of
upstream is no longer readable through the API, so it was not scanned. Public bots are rare. If the
user knows of stronger bots, they are not public under these names.

| Bot | Language, licence | Last commit | Protocol | Needs `sendplanets 1`? | How it aims | Plausibly strong? |
|---|---|---|---|---|---|---|
| [Savo2610/newton-bot](https://github.com/Savo2610/newton-bot) | JavaScript (Node 18+, no dependencies), MIT | 2026-11-12 (1 commit) | `b 9` | No. It reconstructs the planets from the paths. **But** its default `useServerPlanets: true` uses disclosed planets whenever the server sends them. | Planets from second differences of path points (NNLS on a 22-px grid, then joint Levenberg-Marquardt fit of x, y, m, r, constraints from free paths and impacts). Aims with angle sweeps per velocity, golden-section refinement, hit-window bisection, and a hit probability from an ensemble of perturbed maps. Multicore. Scores targets by probability × value / (energy + flight time). | **Yes, the strongest found.** It was written for a bot championship without disclosed planets. The author reports 236 kills and 5 deaths in one 5-minute round at that event, and 24 of 24 planets found with 0.03 px median error (**reported**). The design matches ours in depth (**observed** in source). |
| [LinuxlasGNUleaf/appleBot](https://github.com/LinuxlasGNUleaf/appleBot) | Python 3 + numpy, numba, scipy; no licence file | 2023-01-21 | `b 9` | **Yes.** It does not simulate until it has received planets (**observed**). The same author added `sendplanets` to the server (upstream PR #16). | Brute-force scan over angle and velocity with a numba simulator on the true map; targets the nearest player. | Only in an oracle league. With true planets, an exact simulator is strong by construction (**inferred**). Without them it does not shoot. |
| Upstream example bots, [`bots/`](https://github.com/Draradech/NewtonWars/tree/master/bots) (`nwbot.py`, `nwbot_oop.py`, `nwbot.pl`, `nwbot.hs`) | Python, Perl, Haskell; repository licence | 2016-2017 | `b` (version 4) or `b 8` | No | Random first angle, then fixed angle steps. | No. Useful as baseline opponents and as live targets. |
| [memo42/newtonwars_client](https://github.com/memo42/newtonwars_client) | Python, no licence | 2016-05-08 | text only | No | Fixed-step or random angles; `mux` starts several random bots. | No (baseline). |
| `tools/dummies.js` in newton-bot | JavaScript | 2026-11-12 | text only | No | Random angles every few seconds. | No (baseline). |
| Einsteinbot (this repository) | Rust | current | `b 8` | No. `--oracle-map` exists for diagnosis only. | See [README](../README.md). | Ours. Variants: angle step 8/10/12, `--dummy`. |

Clients that are not bots: [lubro/NewtonWars-GUI](https://github.com/lubro/NewtonWars-GUI),
[chaostreff-flensburg/nw-client](https://github.com/chaostreff-flensburg/nw-client),
[Grauly/NewtonWarsClient](https://github.com/Grauly/NewtonWarsClient) (manual clients);
[chaostreff-flensburg/node-newton-wars](https://github.com/chaostreff-flensburg/node-newton-wars)
is a separate reimplementation of the game, not compatible. False positives checked and dropped:
`lordPitsaed/nwbot` (a Telegram bot), `Advanced-Panic-Programming/*` (another planet game).

**Launching the opponents locally** (commands from their READMEs and source):

| Bot | Command |
|---|---|
| newton-bot | `npm start -- --connect --host=127.0.0.1 --name=NewtonBot --http=8091 --config=nb.json --allow-sleep` with `nb.json` = `{"autoNetwork": false, "useServerPlanets": false}`. Without `autoNetwork: false` it may try to set a static Wi-Fi address on macOS. Each instance needs its own `--http` port and config file. |
| appleBot | `pip install -r requirements.txt && python main.py`. Host, port and protocol are constants in `main.py` (`127.0.0.1:3490`); there is no CLI. |
| upstream bots | `python3 bots/nwbot_oop.py`; host and name are hard-coded (`nwbot.py` even targets `192.168.2.123`), so edit a copy. |
| Einsteinbot | `cargo run --release -- --name E8 ...`, or `--clients N` for the angle-step variants. |

## 2. What the server gives a referee

These facts decide the harness design (all **observed** in `../NewtonWars`, upstream commit
`5f25791`, which is the latest upstream commit):

| Fact | Consequence |
|---|---|
| Per round the server counts `kills` and `deaths`. A kill adds `1 + victim's extrapoints`; with the default `extrapoints` mode the round leader is worth +1. A self-hit is a death. The killer gains half the victim's energy. | "Score" is points including bounty, not raw kills. Deaths are not subtracted. |
| The scoreboard is only drawn on screen. "A killed B" text goes only to non-bot telnet clients; every connection is a player on the field. | No network source of authoritative scores. A passive observer connection would itself be a target. |
| Server stdout prints each received command line with the player's name and slot index (unless `fastmode`). | Slot to bot-name mapping is available from the server log without a patch. |
| Bot connections receive every shot's full path, shot begin/end, all player positions (a victim moves on respawn) and their own slot. They do not receive names. | Kills can be reconstructed from any bot's capture (Einsteinbot already detects kills this way in `src/map_change.rs`), then named via the server log. |
| `sendplanets 1` sends the true map to every bot connection (server-wide switch). | Planet disclosure cannot be given to some bots only. It must be a league setting. |
| `127.0.0.1` and `::1` count as local: no rate limit, and admin commands (`t` adds 100 energy to a slot, `F` fast mode, `T` throttle, `z`, `D`, `p`, `a`). Remote clients: one connection per IP, 512-byte burst then one byte per three loop steps, about 10 s IP block after a connect. | Bots on loopback are not under competitive conditions. Other addresses such as `127.0.0.2` are not on the local list (**inferred** from the `strcmp` list), so separate addresses or containers give remote rules on one host. |
| A joining player starts at 0:0 with 20 energy; all players are reset when the next round starts. | Join between rounds; treat mid-round presence as partial exposure. |

The local checkout carries uncommitted user changes (`rate 5`, `roundTime 30`, board pause 5 s
instead of 20 s, `sendplanets 1`, windowed). Tournament settings should be passed as `nw`
arguments, not taken from that build's defaults.

## 3. Rating: recommendation

Elo is not the right primary tool here. A round is one joint outcome of up to 12 players with
correlated results: one bad spawn drives all of a player's pairwise "games". Splitting it into
pairwise Elo games counts the same evidence many times, and there is no calibrated uncertainty
([Ebtekar & Liu, Elo-MMR](https://arxiv.org/abs/2101.00400)). Online systems (Elo, Glicko-2,
TrueSkill, OpenSkill) also depend on game order. For a small fixed pool that can be refit after
every session, a batch fit is simpler and at least as accurate: on 95.5k Halite games, a batch
Plackett-Luce fit had lower pairwise prediction error than TrueSkill or Weng-Lin
([janzert's Halite rating report](http://janzert.com/halite/rating-report/), secondary but
data-driven).

| Question | Method | Why |
|---|---|---|
| Leaderboard: who wins rounds | **Batch Plackett-Luce** on each round's finishing order by server points (ties by fewer deaths, as the server's own leader rule), with a bootstrap over rounds for intervals. Library: [`choix`](https://github.com/lucasmaystre/choix) 0.4.1 (MIT), MM algorithm of [Hunter 2004](https://projecteuclid.org/journals/annals-of-statistics/volume-32/issue-1/MM-algorithms-for-generalized-Bradley-Terry-models/10.1214/aos/1079120141.full). | Uses the game's own scoring, handles any lobby size, independent of order. |
| Who beats whom, with partial presence | **Pairwise kill model**: kills of i on j ~ Poisson/negative-binomial with log(seconds both present) as offset; strength per bot. `statsmodels` GLM, or Bradley-Terry on the head-to-head kill counts in `choix`. | Uses every kill, handles mid-round joins and lobby mixes. Kill counts are overdispersed ([TrueSkill 2](https://www.microsoft.com/en-us/research/publication/trueskill-2-improved-bayesian-skill-rating-system/), section on kill counts), so use the bootstrap, not Poisson standard errors. |
| Is variant A better than B | **Paired round-level difference**: in rounds where both played the whole round, A's points/min minus B's, mean and bootstrap interval over rounds. | Answers the actual question directly; the map and lobby are shared within a round, so pairing removes most map noise. |
| Readable sanity table | Points/min, kills/min, deaths/min, hit rate, self-hits, per bot, with round-bootstrap intervals. | Catches model artefacts. K/D alone would penalise self-hits that cost no points. |
| Live display (optional) | [`openskill`](https://openskill.me/en/stable/manual.html) 6.2.0 (MIT), `PlackettLuce`, `rate(teams, ranks=...)`. In Rust: [`skillratings`](https://docs.rs/skillratings) 0.29.3, `weng_lin_multi_team`. | Only for a running leaderboard; the batch fit is the result. Avoid the `trueskill` Python package (0.4.5, unmaintained since 2018, trademark limited to non-commercial use). |

The resampling unit is always the round, never the kill or shot. Analysis is offline in Python
over a results table; nothing needs to be in the bot's Rust code.

**How many rounds.** Coarse placement needs few rounds: the TrueSkill authors give about 3
games per player for an 8-player free-for-all, two to three times that in practice
([MSR TrueSkill page](https://www.microsoft.com/en-us/research/project/trueskill-ranking-system/)).
Separating close variants needs far more. Run a pilot (about 20 rounds of 300 s, about 2 hours
with the 20 s pause), measure the spread of the paired A-B difference, then choose the round
count for the interval width wanted. Do not fix a count in advance.

## 4. Runnable plan

**Leagues.**

| League | Server | Who |
|---|---|---|
| Main | `sendplanets 0` | newton-bot, Einsteinbot variants, baselines |
| Oracle (optional) | `sendplanets 1` | appleBot, newton-bot with server planets, Einsteinbot `--oracle-map`. Measures aiming only. |

Never mix results of the two leagues.

**Server settings for both leagues** (passed as arguments): upstream defaults `rate 2`,
`limit 200`, 24 planets, 12 players maximum, `roundtime 300`, plus `throttle` if the display does
not vsync (the remote rate limit and the energy rate are per loop step, so the loop pace must be
the same in every session). Record the full command line with the results. Open question:
whether to use the user's 30 s rounds instead.

**Fair network conditions.** Run each bot from its own address so that every bot is treated as
remote: one container per bot on a Docker bridge network (works for all bots without changes),
or on Linux bots bound to `127.0.0.2`, `127.0.0.3`, ... if a bot supports a source address. The
server can run headless under Xvfb, as in
[jandie1505/NewtonWars-Docker](https://github.com/jandie1505/NewtonWars-Docker). The orchestrator
itself may use loopback only to read stdout; it sends no commands.

**Harness outline.** One orchestrator script (Python) owns everything:

1. Build the server once from the clean upstream commit (a separate copy, not `../NewtonWars`) and
   start it with the league settings; keep its stdout as `server.log`.
2. Draw the lobby for the next block of rounds from a fixed schedule: every bot (or variant) in
   every lobby when there are at most 12 entries; otherwise balanced incomplete lobbies where each
   pair meets equally often. Shuffle connection order (the slot) per block.
3. Start the lobby's bots during the board pause; keep them connected for the block of rounds
   (reconnects cost an IP block). Record each bot's connect and disconnect time.
4. One Einsteinbot instance in every lobby records the SQLite capture (it already stores
   everything a bot connection sees, one capture per map). Its slot-to-name mapping comes from
   `server.log` (the `n name` lines).
5. After the block, an offline extractor builds one row per bot per round: slot, name, seconds
   present, shots, hits, kills by victim, deaths by killer, self-hits, reconstructed points
   (1 + bounty per kill), and an `ambiguous` count where a death cannot be matched to exactly one
   shot. Kills are a shot ending within player radius of a player who then moves.
6. Validation before trusting it: for the pilot rounds, compare the reconstructed per-round
   kills and deaths with the server scoreboard (screenshot of the board screen, or a one-line
   `printf` in `playerHit` in the referee copy, see open questions). Report any mismatch.
7. Fit the methods of section 3 on all rows; publish the table, the Plackett-Luce ranking with
   intervals, and the paired variant comparisons.

## 5. Open questions for the user

1. **Referee patch.** May the referee copy of the server get a single logging line in
   `playerHit` (round, killer, victim, bounty)? It makes scores authoritative and removes the
   attribution risk. Without it, the plan relies on reconstruction validated against screenshots.
2. **Round length.** Upstream 300 s rounds and 20 s pause, or the local 30 s rounds? Short
   rounds favour bots that learn the map fast; newton-bot reports needing about 20-30 s.
3. **Which opponents count.** The search found one strong public opponent (newton-bot). Do you
   have names or links for the other strong bots you know of? Many may be private or event-only.
4. **Network isolation.** Docker containers per bot (simplest, fully fair), or plain loopback
   with the understanding that bots are then rate-limit-free and could use admin commands?
5. **Oracle league.** Worth running at all, or main league only?
6. **Variants.** Which Einsteinbot variants to enter, and what margin between variants matters
   (it sets the number of rounds)?
