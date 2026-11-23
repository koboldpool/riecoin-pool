"""Exact checks of payout.py (Riecoin), from bound-pool's test_payout.py: per-block fees, solo blocks, PPLNS without
solo shares, orphans and refused blocks, the PPLNS window, halvings, and the wallet path with Riecoin Core's
behaviour (encrypted wallets report unlocked_until, sendmany takes only named amounts): a locked wallet pays nothing,
one sendmany pays every due balance, payments are confirmed, and a lost one is credited back. A fake node answers.

    python -m unittest test_payout
"""
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import payout

REWARD = 625_000_000  # 6.25 RIC: mainnet heights from 2,520,000 (3 halvings)
H = 2_600_000


class FakeNode:
    def __init__(self, tip, chain):
        self.tip, self.chain = tip, chain  # chain: height -> block id on the best chain
        self.encrypted, self.locked, self.balance = True, True, 0.0
        self.sent, self.mempool, self.txs = [], [], {}
        self.own = set()  # addresses of the pool wallet itself

    def rpc(self, args, method, *params):
        if method == "getblockcount":
            return self.tip
        if method == "getblockhash":
            return self.chain.get(params[0], "f" * 64)
        if method == "getrawmempool":
            return list(self.mempool)
        if method == "getwalletinfo":
            info = {"walletname": "pool"}
            if self.encrypted:
                info["unlocked_until"] = 0 if self.locked else 2_000_000_000
            return info
        if method == "walletpassphrase":
            assert params[0] == "unit-test-only"
            self.locked = False
            return None
        if method == "walletlock":
            self.locked = True
            return None
        if method == "getbalance":
            return self.balance
        if method == "sendmany":
            assert not (self.encrypted and self.locked)
            assert len(params) == 1 and set(params[0]) == {"amounts"}, params
            txid = f"{len(self.sent) + 1:064x}"
            self.sent.append((params[0]["amounts"], txid))
            self.mempool.append(txid)
            self.txs[txid] = {"confirmations": 0, "fee": -0.0000226}
            return txid
        if method == "getaddressinfo":
            return {"address": params[0], "ismine": params[0] in self.own}
        if method == "gettransaction":
            if params[0] not in self.txs:
                raise RuntimeError("Invalid or non-wallet transaction id")
            return self.txs[params[0]]
        raise AssertionError(f"unexpected RPC {method}")


def run_payout(data: Path, *extra, code=0):
    argv = ["payout.py", "--data", str(data), "--window", "2", "--network", "mainnet", "--cookie", "x", *extra]
    with mock.patch.object(sys, "argv", argv):
        assert payout.main() == code
    return json.loads((data / "payouts.json").read_text())


class PayoutTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        d = self.data = Path(self.tmp.name)
        found = lambda bid, h, ts, finder, **kw: {"event": "found", "id": bid, "height": h, "ts": ts,
                                                  "finder": finder, "block_work": 1000.0, **kw}
        result = lambda bid, ok=True: {"event": "result", "id": bid, "accepted": ok}
        blocks = [
            found("a" * 64, H + 10, 1000, "alice.rig", mode="pplns", fee_bp=0), result("a" * 64),
            found("b" * 64, H + 20, 2000, "carol.gpu", mode="pplns", fee_bp=200), result("b" * 64),
            found("c" * 64, H + 25, 2500, "bob.solo1", mode="solo", fee_bp=150), result("c" * 64),
            found("d" * 64, H + 30, 3000, "alice.rig"), result("d" * 64),  # old record: no mode/fee_bp
            found("e" * 64, H + 40, 3500, "alice.rig", mode="pplns", fee_bp=200), result("e" * 64),  # orphaned
            found("9" * 64, H + 45, 3600, "carol.gpu", mode="pplns", fee_bp=200), result("9" * 64, False),  # refused
        ]
        (d / "blocks.jsonl").write_text("\n".join(json.dumps(b) for b in blocks) + "\n")
        sh = lambda ts, addr, work, mode="pplns": {"ts": ts, "addr": addr, "worker": "w", "work": work, "mode": mode, "k": 5}
        shares = [sh(900, "alice", 300), sh(950, "carol", 100), sh(1900, "bob", 1000, "solo"),
                  sh(1900, "alice", 100), sh(1950, "carol", 300)]
        (d / "shares.jsonl").write_text("\n".join(json.dumps(s) for s in shares) + "\n")
        chain = {H + 10: "a" * 64, H + 20: "b" * 64, H + 25: "c" * 64, H + 30: "d" * 64, H + 40: "0" * 64, H + 45: "9" * 64}
        self.node = FakeNode(H + 200, chain)
        self.patch = mock.patch.object(payout, "rpc", self.node.rpc)
        self.patch.start()

    def tearDown(self):
        self.patch.stop()
        self.tmp.cleanup()

    def test_per_block_fees_solo_and_pplns(self):
        led = run_payout(self.data, "--fee", "1.0")
        cr = led["credited"]
        self.assertEqual(cr["a" * 64]["fee"], 0)                      # 0 % when found
        self.assertEqual(cr["b" * 64]["fee"], REWARD * 200 // 10_000)  # 2 % when found
        self.assertEqual(cr["c" * 64]["fee"], REWARD * 150 // 10_000)  # solo 1.5 %
        self.assertEqual(cr["d" * 64]["fee"], REWARD * 100 // 10_000)  # old record: --fee 1.0
        self.assertEqual(cr["c" * 64]["mode"], "solo")
        self.assertIn("e" * 64, led["orphaned"])
        self.assertNotIn("9" * 64, cr)                                 # refused by the node
        b = led["balances"]
        # a: 0 % over alice 300 / carol 100. b and d: alice 400 / carol 400 (bob's solo share excluded).
        half_b, half_d = (REWARD - REWARD * 2 // 100) // 2, (REWARD - REWARD // 100) // 2
        self.assertEqual(b["alice"], REWARD * 3 // 4 + half_b + half_d)
        self.assertEqual(b["carol"], REWARD // 4 + half_b + half_d)
        self.assertEqual(b["bob"], REWARD - REWARD * 150 // 10_000)    # the whole solo block, less 1.5 %
        # Every credited base unit is accounted for: balances + fees = rewards.
        self.assertEqual(sum(b.values()) + led["pool_fees"], 4 * REWARD)

    def test_pool_own_address_is_not_paid(self):
        # bob = the pool wallet's own address: its solo block stays in the wallet, nothing owed or sent.
        self.node.own = {"bob"}
        led = run_payout(self.data, "--fee", "1.0")
        self.assertNotIn("bob", led["balances"])
        self.assertIn("c" * 64, led["credited"])
        self.assertEqual(sum(led["balances"].values()) + led["pool_fees"], 4 * REWARD)
        self.assertGreaterEqual(led["pool_fees"], REWARD)

    def test_pplns_window(self):
        # block_work 1 and window 2: only the last 2 units of work before each block count.
        lines = [json.loads(x) for x in (self.data / "blocks.jsonl").read_text().splitlines()]
        for x in lines:
            if x.get("event") == "found":
                x["block_work"] = 1.0
        (self.data / "blocks.jsonl").write_text("\n".join(json.dumps(x) for x in lines) + "\n")
        sh = lambda ts, addr: {"ts": ts, "addr": addr, "worker": "w", "work": 1.0, "mode": "pplns", "k": 5}
        shares = [sh(800, "old"), sh(900, "alice"), sh(950, "carol")]
        (self.data / "shares.jsonl").write_text("\n".join(json.dumps(s) for s in shares) + "\n")
        self.node.tip = H + 115  # only block a is mature
        led = run_payout(self.data)
        self.assertEqual(led["balances"], {"alice": REWARD // 2, "carol": REWARD // 2})

    def test_fee_change_later_does_not_touch_credited_blocks(self):
        first = run_payout(self.data, "--fee", "1.0")
        second = run_payout(self.data, "--fee", "5.0")
        self.assertEqual(first["balances"], second["balances"])
        self.assertEqual(first["credited"], second["credited"])

    def test_immature_blocks_wait(self):
        self.node.tip = H + 115  # 100 confirmations reached for heights <= H + 15 only
        led = run_payout(self.data)
        self.assertEqual(set(led["credited"]), {"a" * 64})

    def test_halving(self):
        r = lambda net, h: payout.pool_reward(mock.Mock(network=net), h)
        self.assertEqual(r("mainnet", 839_999), 50 * payout.COIN)
        self.assertEqual(r("mainnet", 840_000), 25 * payout.COIN)
        self.assertEqual(r("mainnet", 2_675_756), 625_000_000)  # the node's coinbasevalue at that height (no fees)
        self.assertEqual(r("mainnet", 3_360_000), 312_500_000)
        self.assertEqual(r("regtest", 150), 25 * payout.COIN)

    def test_locked_wallet_pays_nothing(self):
        self.node.balance = 1000.0
        with mock.patch.dict("os.environ", {}, clear=True):
            led = run_payout(self.data, "--pay", code=1)
        self.assertEqual(self.node.sent, [])
        self.assertEqual(led["payments"], [])

    def test_unencrypted_wallet_needs_no_passphrase(self):
        self.node.balance = 1000.0
        self.node.encrypted = False
        with mock.patch.dict("os.environ", {}, clear=True):
            led = run_payout(self.data, "--pay")
        self.assertEqual(len(self.node.sent), 1)
        self.assertEqual(len(led["payments"]), 3)

    def test_low_balance_pays_what_it_can(self):
        # 5 RIC spendable (less the 0.01 reserve): only bob's 6.15625 is too much; alice and carol are larger still,
        # so nothing is sent and every balance stays.
        self.node.balance = 5.0
        pw = self.data / "pass"
        pw.write_text("unit-test-only\n")
        led = run_payout(self.data, "--pay", "--passphrase-file", str(pw))
        self.assertEqual(self.node.sent, [])
        self.assertTrue(self.node.locked)
        # With 7 RIC, bob (the smallest due balance) is paid and the others wait.
        self.node.balance = 7.0
        led = run_payout(self.data, "--pay", "--passphrase-file", str(pw))
        self.assertEqual([set(a) for a, _ in self.node.sent], [{"bob"}])
        self.assertEqual(led["balances"]["bob"], 0)
        self.assertGreater(led["balances"]["alice"], 0)

    def test_pay_confirm_and_lost(self):
        self.node.balance = 1000.0
        with mock.patch.dict("os.environ", {"RIC_WALLET_PASSPHRASE": "unit-test-only"}):
            led = run_payout(self.data, "--pay")
        self.assertTrue(self.node.locked)  # locked again after the run
        self.assertEqual(len(self.node.sent), 1)  # one sendmany for all three
        amounts, txid = self.node.sent[0]
        b = {"alice": 0, "carol": 0, "bob": 0}
        self.assertEqual(set(amounts), set(b))
        self.assertEqual(amounts["bob"], f"{(REWARD - REWARD * 150 // 10_000) / 1e8:.8f}")
        self.assertEqual(sum(led["balances"].values()), 0)
        self.assertEqual({p["status"] for p in led["payments"]}, {"pending"})
        self.assertEqual(sum(p["fee"] for p in led["payments"]), 2260)  # the tx fee, once
        paid = dict(led["paid"])
        # Not mined, not in the mempool: after 10 blocks every payment of that tx is credited back.
        self.node.mempool = []
        del self.node.txs[txid]
        self.node.tip = H + 205
        led = run_payout(self.data)
        self.assertEqual({p["status"] for p in led["payments"]}, {"pending"})  # only 5 blocks yet
        self.node.tip = H + 210
        led = run_payout(self.data)
        self.assertEqual({p["status"] for p in led["payments"]}, {"failed"})
        self.assertEqual(led["paid"], {a: 0 for a in paid})
        self.assertEqual(led["balances"]["bob"], REWARD - REWARD * 150 // 10_000)
        # Paid again, then mined 6 deep: confirmed, paid once.
        with mock.patch.dict("os.environ", {"RIC_WALLET_PASSPHRASE": "unit-test-only"}):
            led = run_payout(self.data, "--pay")
        txid2 = self.node.sent[-1][1]
        self.node.txs[txid2] = {"confirmations": 6, "blockheight": H + 211}
        led = run_payout(self.data)
        st = [p["status"] for p in led["payments"] if p["txid"] == txid2]
        self.assertEqual(st, ["confirmed"] * 3)
        self.assertEqual(led["paid"], paid)


if __name__ == "__main__":
    unittest.main()
