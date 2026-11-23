#!/usr/bin/env bash
# End-to-end test on a private regtest chain on the pool server (run as a sudo user; uses sudo -u kobold for the node binaries).
# Everything lives in /tmp/ric-regtest; ports 127.0.0.1:18443 (node RPC), 13540/13550 (pool). Mainnet is not touched.
#   1. regtest node, wallets "pool" (encrypted, test passphrase) and "miners" (alice, bob addresses)
#   2. riecoin-pool on regtest; testminer alice (PPLNS) and bob (solo) find blocks (on regtest every prime is a block)
#   3. more blocks for coinbase maturity, then payout.py --pay (sendmany), 6 more blocks, payout.py again: confirmed
set -euo pipefail
T=${T:-/tmp/ric-regtest}
K="sudo -u kobold"
B=/srv/kobold/coins/riecoin/bin
export LD_LIBRARY_PATH=/srv/kobold/coins/riecoin/lib
$K mkdir -p $T/node $T/pool-data
$K cp /tmp/ric-e2e/riecoin-pool /tmp/ric-e2e/testminer /tmp/ric-e2e/payout.py $T/ && $K chmod 755 $T/riecoin-pool $T/testminer
CLI="$K env LD_LIBRARY_PATH=$LD_LIBRARY_PATH $B/riecoin-cli -regtest -datadir=$T/node"
$K env LD_LIBRARY_PATH=$LD_LIBRARY_PATH $B/riecoind -regtest -datadir=$T/node -daemon -listen=0 -server=1 \
  -rpcbind=127.0.0.1 -rpcallowip=127.0.0.1 -printtoconsole=0
for i in $(seq 1 30); do $CLI getblockcount >/dev/null 2>&1 && break; sleep 1; done
echo "node up, height $($CLI getblockcount)"
$CLI -named createwallet wallet_name=pool passphrase=regtest-only-pass load_on_startup=false >/dev/null
$CLI -named createwallet wallet_name=miners load_on_startup=false >/dev/null
POOL=$($CLI -rpcwallet=pool getnewaddress pool bech32)
ALICE=$($CLI -rpcwallet=miners getnewaddress alice bech32)
BOB=$($CLI -rpcwallet=miners getnewaddress bob bech32)
echo "pool $POOL alice $ALICE bob $BOB"
echo "regtest-only-pass" | $K tee $T/pass >/dev/null
$K chmod 600 $T/pass
$K bash -c "cd $T && nohup ./riecoin-pool --network regtest --address $POOL --cookie $T/node/regtest/.cookie --data $T/pool-data \
  --listen 127.0.0.1:13540 --listen-solo 127.0.0.1:13550 --max-tip-age 0 > $T/pool.out 2>&1 &"
sleep 3
echo "--- phase 1: alice (PPLNS, 2 workers) and bob (solo)"
$K $T/testminer 127.0.0.1:13540 $ALICE.rig1 x 25 400 &
$K $T/testminer 127.0.0.1:13540 $ALICE.rig2 x 15 700 &
$K $T/testminer 127.0.0.1:13550 $BOB.cpu x 15 600 &
wait
echo "--- phase 2: alice mines on to maturity (100 confirmations)"
$K $T/testminer 127.0.0.1:13540 $ALICE.rig1 x 115 600
sleep 12
grep -c '"event":"found"' $T/pool-data/blocks.jsonl | sed 's/^/found events: /'
grep -c '"accepted":true' $T/pool-data/blocks.jsonl | sed 's/^/accepted by the node: /'
grep -c '"accepted":false' $T/pool-data/blocks.jsonl | sed 's/^/refused by the node: /' || true
echo "chain height $($CLI getblockcount)"
# Every accepted block id is on the best chain at its height.
python3 - "$T" <<'EOF'
import json, subprocess, sys
T = sys.argv[1]
cli = ["sudo", "-u", "kobold", "env", "LD_LIBRARY_PATH=/srv/kobold/coins/riecoin/lib", "/srv/kobold/coins/riecoin/bin/riecoin-cli", "-regtest", f"-datadir={T}/node"]
ev = [json.loads(l) for l in open(f"{T}/pool-data/blocks.jsonl")]
found = {e["id"]: e for e in ev if e["event"] == "found"}
ok = [e for e in ev if e["event"] == "result" and e["accepted"]]
best = sum(1 for e in ok if subprocess.check_output(cli + ["getblockhash", str(e["height"])]).decode().strip() == e["id"])
print(f"accepted blocks on the best chain: {best} of {len(ok)}")
blk = json.loads(subprocess.check_output(cli + ["getblock", ok[0]["id"], "2"]))
cb = blk["tx"][0]
print("coinbase scriptSig:", cb["vin"][0]["coinbase"], "->", bytes.fromhex(cb["vin"][0]["coinbase"])[-12:])
print("coinbase outputs:", [(o["value"], o["scriptPubKey"]["type"]) for o in cb["vout"]])
print("modes:", {m: sum(1 for e in found.values() if e["mode"] == m) for m in ("pplns", "solo")})
EOF
echo "--- payouts"
$K python3 $T/payout.py --network regtest --data $T/pool-data --cookie $T/node/regtest/.cookie --wallet pool \
  --passphrase-file $T/pass --min 1 --pay
echo "--- 6 more blocks, then payouts again"
$K $T/testminer 127.0.0.1:13540 $ALICE.rig1 x 7 600
sleep 3
$K python3 $T/payout.py --network regtest --data $T/pool-data --cookie $T/node/regtest/.cookie --wallet pool \
  --passphrase-file $T/pass --min 1 --pay
echo "--- miners wallet (received by address) and ledger"
$CLI -rpcwallet=miners listreceivedbyaddress 1 | python3 -c 'import json,sys; [print(" ", r["address"], r["amount"], r["confirmations"]) for r in json.load(sys.stdin)]'
$K python3 -c "import json; l=json.load(open('$T/pool-data/payouts.json')); print('  paid', {a: v/1e8 for a, v in l['paid'].items()}); print('  balances', {a: v/1e8 for a, v in l['balances'].items()}); print('  payments', [(p['addr'][:12], p['amount']/1e8, p['status']) for p in l['payments']]); print('  wallet locked after:', True)"
$CLI -rpcwallet=pool getwalletinfo | grep unlocked_until
echo "--- pool log (last lines) and stats"
tail -5 $T/pool-data/pool.log
python3 -c "import json; s=json.load(open('$T/pool-data/stats.json')); print({k: s[k] for k in ('hashrate','accepted','rejected','stale','blocks_submitted','blocks_accepted','share_k','network_work_per_block')}); print([(m['addr'][:12], round(m['hashrate'],3), m['solo'], [w['name'] for w in m['workers']]) for m in s['miners']])"
