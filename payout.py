#!/usr/bin/env python3
"""PPLNS payouts for riecoin-pool (from bound-pool's payout.py): credits mature pool blocks (once, only while on the
best chain) over the last WINDOW x block work of shares, and with --pay sends every balance above --min in one
sendmany from the node's wallet, unlocked for the run from --passphrase-file or RIC_WALLET_PASSPHRASE. Lost payments
are credited back. Ledger: <data>/payouts.json.
"""
from __future__ import annotations

import argparse
import base64
import json
import os
import sys
import time
import urllib.error
import urllib.request
from decimal import Decimal
from pathlib import Path

# Shared ledger helper (lock, dated copy, fsync'd saves, pending-before-send), loaded by path so it also works
# under python3 -I (which leaves the script's folder off sys.path).
import importlib.util as _ilu
_spec = _ilu.spec_from_file_location("kobold_ledger", Path(__file__).resolve().parent / "kobold_ledger.py")
kl = _ilu.module_from_spec(_spec)
_spec.loader.exec_module(kl)

COINBASE_MATURITY = 100
COIN = 100_000_000           # base units per RIC
# Per network (chainparams.cpp, chainparamsbase.cpp): (initial subsidy, halving interval, RPC port).
NETWORKS = {
    "mainnet": (50 * COIN, 840_000, 28332),
    "testnet": (50 * COIN, 840_000, 38332),
    "regtest": (50 * COIN, 150, 18443),
}
PAYMENT_CONFIRMATIONS = 6     # a payment counts as done this deep in the chain
PAYMENT_LOST_AFTER = 10       # blocks after sending with the tx neither mined nor in the mempool


def rpc(args, method: str, *params):
    cookie = Path(args.cookie).read_text().strip()
    p = params[0] if len(params) == 1 and isinstance(params[0], dict) else list(params)
    body = json.dumps({"jsonrpc": "1.0", "id": 1, "method": method, "params": p}).encode()
    url = args.rpc + (f"wallet/{args.wallet}" if args.wallet else "")
    req = urllib.request.Request(url, data=body, headers={
        "content-type": "application/json",
        "authorization": "Basic " + base64.b64encode(cookie.encode()).decode()})
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            out = json.loads(r.read())
    except urllib.error.HTTPError as e:  # the node answers RPC errors with HTTP 500 and a JSON body
        out = json.loads(e.read() or b"{}")
        if not out:
            raise RuntimeError(f"{method}: HTTP {e.code}") from None
    if out.get("error"):
        raise RuntimeError(f"{method}: {out['error']}")
    return out["result"]


def units(coins) -> int:
    """An amount as the node prints it (a JSON number, e.g. 1.5) in base units."""
    return int(Decimal(str(coins)) * COIN)


def pool_reward(args, height: int) -> int:
    """Base units of the block subsidy (GetBlockSubsidy); the half of tx fees miners keep stays with the pool."""
    initial, halving, _ = NETWORKS[args.network]
    halvings = height // halving
    return 0 if halvings >= 64 else initial >> halvings


def read_jsonl(path: Path) -> list[dict]:
    if not path.exists():
        return []
    out = []
    for line in path.read_text(encoding="utf-8").splitlines():
        try:
            out.append(json.loads(line))
        except json.JSONDecodeError:
            pass  # a line cut by a crash
    return out


def reconcile(args, ledger: dict, tip: int) -> None:
    """Checks every pending payment with the wallet's gettransaction. One that was neither mined
    nor is waiting in the mempool PAYMENT_LOST_AFTER blocks after it was sent (e.g. a node restart
    dropped its mempool, or it was double-spent) is marked failed and its amount goes back to the miner."""
    pending = [p for p in ledger["payments"] if p.get("status", "pending") == "pending"]
    if not pending:
        return
    mempool = set(rpc(args, "getrawmempool") or [])
    for p in pending:
        sent = p.setdefault("height_sent", max(0, tip - 200))
        try:
            tx = rpc(args, "gettransaction", p["txid"])
            conf = int(tx.get("confirmations", 0))
        except RuntimeError:
            tx, conf = {}, 0
        if conf > 0:
            p["mined_height"] = tx.get("blockheight", tip - conf + 1)
            if conf >= PAYMENT_CONFIRMATIONS:
                p["status"] = "confirmed"
                print(f"payment {p['txid'][:16]} to {p['addr'][:20]}... confirmed at height {p['mined_height']}")
        elif p["txid"] in mempool:
            continue
        elif tip - sent >= PAYMENT_LOST_AFTER:
            p["status"] = "failed"
            ledger["balances"][p["addr"]] = ledger["balances"].get(p["addr"], 0) + p["amount"]
            ledger["paid"][p["addr"]] = ledger["paid"].get(p["addr"], 0) - p["amount"]
            print(f"PAYMENT LOST: {p['amount'] / COIN:.8f} RIC to {p['addr']} ({p['txid']}) was never mined; "
                  f"credited back to the balance")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", default="pool-data")
    ap.add_argument("--network", default="mainnet", choices=sorted(NETWORKS))
    ap.add_argument("--rpc", default="", help="node JSON-RPC URL (default: http://127.0.0.1:<network port>/)")
    ap.add_argument("--cookie", default="", help="the node's cookie file (default: ~/.riecoin/.cookie)")
    ap.add_argument("--wallet", default="", help="wallet name, when the node has more than one loaded")
    ap.add_argument("--fee", type=float, default=0.0,
                    help="fee in percent for old block records that don't carry their own fee_bp")
    ap.add_argument("--window", type=float, default=2.0, help="PPLNS window in block works")
    ap.add_argument("--min", type=float, default=1.0, help="minimum payout in RIC")
    ap.add_argument("--reserve", type=float, default=0.01, help="RIC always left in the wallet for fees")
    ap.add_argument("--passphrase-file", default="", help="file whose first line is the wallet passphrase")
    ap.add_argument("--pay", action="store_true", help="actually send payments")
    args = ap.parse_args()
    if not args.rpc:
        args.rpc = f"http://127.0.0.1:{NETWORKS[args.network][2]}/"
    if not args.rpc.endswith("/"):
        args.rpc += "/"
    if not args.cookie:
        sub = {"mainnet": "", "testnet": "testnet2404", "regtest": "regtest"}[args.network]
        args.cookie = str(Path.home() / ".riecoin" / sub / ".cookie")

    data = Path(args.data)
    ledger_path = data / "payouts.json"
    led = kl.Ledger(ledger_path)  # lock + dated copy before the run
    ledger = led.open({
        "credited": {}, "orphaned": {}, "balances": {}, "paid": {}, "payments": [], "pool_fees": 0})

    def save():
        led.save(ledger)  # temp file, fsync, rename, fsync the folder

    tip = int(rpc(args, "getblockcount"))
    reconcile(args, ledger, tip)
    save()
    events = read_jsonl(data / "blocks.jsonl")
    found = {e["id"]: e for e in events if e.get("event") == "found"}
    accepted = {e["id"] for e in events if e.get("event") == "result" and e.get("accepted")}
    shares = read_jsonl(data / "shares.jsonl")
    own: dict[str, bool] = {}

    def is_own(addr: str) -> bool:
        """True for an address of the pool wallet itself (the node's wallet says ismine)."""
        if addr not in own:
            try:
                own[addr] = bool(rpc(args, "getaddressinfo", addr).get("ismine"))
            except RuntimeError:
                own[addr] = False  # not an address the node knows how to read: a normal miner
        return own[addr]

    for bid, blk in sorted(found.items(), key=lambda kv: kv[1]["height"]):
        if bid in ledger["credited"] or bid in ledger["orphaned"] or bid not in accepted:
            continue
        h = blk["height"]
        if tip - h < COINBASE_MATURITY:
            continue  # not mature yet
        if rpc(args, "getblockhash", h) != bid:
            ledger["orphaned"][bid] = {"height": h, "ts": blk["ts"]}
            print(f"block {h} {bid[:16]} is not on the best chain: orphaned, nothing credited")
            continue
        reward = pool_reward(args, h)
        # The fee in force when the block was found (recorded by the pool); --fee only for old
        # records without it. So a fee change never touches blocks found before it.
        fee_bp = int(blk["fee_bp"]) if "fee_bp" in blk else int(round(args.fee * 100))
        fee = reward * fee_bp // 10_000
        to_split = reward - fee
        mode = blk.get("mode", "pplns")
        work_by: dict[str, float] = {}
        total = 0.0
        if mode == "solo":
            # Solo: the whole block (less the solo fee) goes to the address that found it.
            work_by[blk["finder"].split(".", 1)[0]] = 1.0
            total = 1.0
        else:
            # PPLNS: walk back from the block over pool (non-solo) shares until the window is filled.
            window = args.window * float(blk.get("block_work") or 0)
            for s in reversed(shares):
                if s["ts"] > blk["ts"] or s.get("mode", "pplns") == "solo":
                    continue
                if window and total >= window:
                    break
                work_by[s["addr"]] = work_by.get(s["addr"], 0.0) + s["work"]
                total += s["work"]
        if total <= 0:
            print(f"block {h}: no shares in the window, kept as pool income")
            ledger["pool_fees"] += reward
        else:
            given = 0
            for addr, w in work_by.items():
                part = int(to_split * w / total)
                if is_own(addr):
                    # Mined to the pool wallet's own address: nothing to pay, the coins simply stay in the wallet.
                    print(f"  {part / COIN:.8f} RIC to the pool's own address {addr}: kept in the pool wallet")
                    continue
                ledger["balances"][addr] = ledger["balances"].get(addr, 0) + part
                given += part
            ledger["pool_fees"] += reward - given
        ledger["credited"][bid] = {"height": h, "reward": reward, "fee": fee, "fee_bp": fee_bp, "mode": mode,
                                   "miners": len(work_by), "ts": blk["ts"]}
        print(f"block {h} credited ({mode}, fee {fee_bp / 100:.2f} %): {reward / COIN:.8f} RIC over "
              f"{len(work_by)} miners (fee {fee / COIN:.8f})")
        save()

    due = {a: b for a, b in ledger["balances"].items() if b >= args.min * COIN}
    print(f"tip {tip}; {len(ledger['credited'])} blocks credited; "
          f"{len(due)} balances due (>= {args.min} RIC); owed in total "
          f"{sum(ledger['balances'].values()) / COIN:.8f} RIC")
    for a, b in sorted(ledger["balances"].items(), key=lambda kv: -kv[1]):
        print(f"  {a}  {b / COIN:.8f} RIC{'  (due)' if a in due else ''}")
    if args.pay and kl.hold(ledger, ledger_path):
        save()
        return kl.HOLD_EXIT
    if not args.pay or not due:
        save()
        return 0

    info = rpc(args, "getwalletinfo")
    passphrase = os.environ.get("RIC_WALLET_PASSPHRASE")
    if args.passphrase_file:
        lines = Path(args.passphrase_file).read_text(encoding="utf-8").splitlines()
        passphrase = lines[0] if lines else None
    unlocked_here = False
    # Only encrypted wallets report unlocked_until; 0 means locked.
    if info.get("unlocked_until") == 0:
        if not passphrase:
            print("the pool wallet is locked and no passphrase was given; nothing paid")
            save()
            return 1
        rpc(args, "walletpassphrase", passphrase, 300)
        unlocked_here = True
    try:
        available = units(rpc(args, "getbalance")) - int(args.reserve * COIN)
        batch: dict[str, int] = {}
        for addr, amount in sorted(due.items(), key=lambda kv: -kv[1]):
            if amount > available:
                print(f"wallet has {available / COIN:.8f} RIC spendable, not enough for {addr}; paid next time")
                continue
            batch[addr] = amount
            available -= amount
        if batch:
            amounts = {a: f"{v / COIN:.8f}" for a, v in batch.items()}
            # Pending before send: every payment of the batch is recorded and saved first.
            intent = kl.new_intent()
            for addr, amount in batch.items():
                kl.add_intent(ledger, intent, addr, amount, height_sent=tip)
            save()
            try:
                txid = rpc(args, "sendmany", {"amounts": amounts})  # Riecoin dropped Bitcoin's "dummy" first argument
            except Exception as e:  # noqa: BLE001
                if not kl.refused(e):
                    raise  # no answer: it may have gone out; stays 'sending', the next run holds
                print(f"payment of {len(batch)} balances failed: {e}")
                txid = None
            if not txid:
                kl.drop_intent(ledger, intent)
                save()
            if txid:
                fee = 0
                try:
                    fee = abs(units(rpc(args, "gettransaction", txid).get("fee", 0)))
                except RuntimeError:
                    pass
                height = int(rpc(args, "getblockcount"))
                # One transaction for all: its fee is recorded once, on the first payment.
                for i, p in enumerate(kl.mark_sent(ledger, intent, txid, height_sent=height)):
                    p["fee"] = fee if i == 0 else 0
                    print(f"paid {p['amount'] / COIN:.8f} RIC to {p['addr']}: {txid}")
                print(f"transaction {txid}: {len(batch)} payments, fee {fee / COIN:.8f} RIC")
                save()
    finally:
        if unlocked_here:
            rpc(args, "walletlock")
    save()
    return 0


if __name__ == "__main__":
    sys.exit(main())
