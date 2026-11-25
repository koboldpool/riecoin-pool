#!/usr/bin/env python3
"""Safe writes for a pool's payouts.json ledger, shared by every coin's payout.py (a copy sits next to each one).

- One run at a time: an exclusive flock on <ledger>.lock, held until the process ends.
- A dated copy of the ledger before each run in <data>/payouts-history/ (the newest KEEP are kept).
- Each save: write a temp file, fsync it, rename it over the ledger, fsync the folder. A power cut leaves either the
  old or the new ledger, never an empty or half-written one.
- "Pending before send": a payment is written to the ledger with status "sending" (balance already taken off)
  BEFORE the wallet is asked to send. If the process dies before the result is saved, the next run finds the
  "sending" record and pays nobody until a person checks the wallet and resolves it:

      python3 kobold_ledger.py list    <data>/payouts.json
      python3 kobold_ledger.py resolve <data>/payouts.json <intent> --sent <txid>   # it did go out
      python3 kobold_ledger.py resolve <data>/payouts.json <intent> --not-sent      # it did not: credit back

Linux only (fcntl).
"""
from __future__ import annotations

import argparse
import fcntl
import json
import os
import shutil
import sys
import time
from pathlib import Path

KEEP = 30              # dated copies kept
SENDING = "sending"    # status of a payment recorded before the send
HOLD_EXIT = 3          # exit code of a run that found unresolved payments (the systemd unit shows "failed")


def fsync_dir(path: Path) -> None:
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def write_atomic(path: Path, text: str) -> None:
    """Writes text to path so that a crash or power cut leaves the old file or the new one, nothing in between."""
    path = Path(path)
    tmp = path.with_name(path.name + ".tmp")
    with open(tmp, "w", encoding="utf-8") as f:
        f.write(text)
        f.flush()
        os.fsync(f.fileno())
    os.replace(tmp, path)
    fsync_dir(path.parent)


class Ledger:
    """payouts.json with a run lock, a dated copy per run and durable saves."""

    def __init__(self, path, keep: int = KEEP):
        self.path = Path(path)
        self.keep = keep
        self._lock = None

    def lock(self) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        f = open(self.path.with_name(self.path.name + ".lock"), "a")
        try:
            fcntl.flock(f, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            f.close()
            raise SystemExit(f"another payout run holds {f.name}; nothing done") from None
        self._lock = f  # released when the process ends

    def snapshot(self) -> Path | None:
        """Copies the ledger to payouts-history/payouts-YYYYMMDD-HHMMSS-micros.json (UTC) and keeps the newest self.keep."""
        if not self.path.exists():
            return None
        hist = self.path.parent / "payouts-history"
        hist.mkdir(exist_ok=True)
        now = time.time_ns()
        while True:  # UTC, to the microsecond: names sort by time
            stamp = time.strftime("%Y%m%d-%H%M%S", time.gmtime(now // 10 ** 9)) + f"-{now // 1000 % 10 ** 6:06d}"
            dst = hist / f"{self.path.stem}-{stamp}.json"
            if not dst.exists():
                break
            now += 1000
        tmp = dst.with_name(dst.name + ".tmp")
        shutil.copyfile(self.path, tmp)
        with open(tmp, "rb") as f:
            os.fsync(f.fileno())
        os.replace(tmp, dst)
        fsync_dir(hist)
        old = sorted(hist.glob(f"{self.path.stem}-*.json"))
        for p in old[:-self.keep] if len(old) > self.keep else []:
            p.unlink()
        return dst

    def open(self, default: dict) -> dict:
        """Lock, dated copy, then the ledger (or default when there is none yet)."""
        self.lock()
        self.snapshot()
        if self.path.exists():
            return json.loads(self.path.read_text(encoding="utf-8"))
        return default

    def save(self, ledger: dict) -> None:
        write_atomic(self.path, json.dumps(ledger, indent=1))


# ---- pending before send -------------------------------------------------------------------------------------------

def new_intent() -> str:
    return f"i{time.time_ns()}"


def add_intent(ledger: dict, intent: str, addr: str, amount: int, debit: int | None = None, **fields) -> dict:
    """Records a payment about to be sent: the balance goes down by debit (default amount), paid up by amount."""
    debit = amount if debit is None else debit
    ledger["balances"][addr] = ledger["balances"].get(addr, 0) - debit
    ledger["paid"][addr] = ledger["paid"].get(addr, 0) + amount
    p = {"ts": int(time.time()), "addr": addr, "amount": amount, "txid": "", **fields,
         "status": SENDING, "intent": intent}
    if debit != amount:
        p["debit"] = debit
    ledger["payments"].append(p)
    return p


def drop_intent(ledger: dict, intent: str) -> int:
    """The wallet refused the send (nothing left it): undoes the records of this intent. Returns how many."""
    keep, n = [], 0
    for p in ledger["payments"]:
        if p.get("intent") == intent and p.get("status") == SENDING:
            ledger["balances"][p["addr"]] = ledger["balances"].get(p["addr"], 0) + p.get("debit", p["amount"])
            ledger["paid"][p["addr"]] = ledger["paid"].get(p["addr"], 0) - p["amount"]
            n += 1
        else:
            keep.append(p)
    ledger["payments"] = keep
    return n


def mark_sent(ledger: dict, intent: str, txid: str, status: str = "pending", **fields) -> list[dict]:
    """The wallet sent it: the intent's records get the txid and the normal status."""
    out = []
    for p in ledger["payments"]:
        if p.get("intent") == intent and p.get("status") == SENDING:
            p.update(fields)
            p["txid"] = txid
            p["status"] = status
            out.append(p)
    return out


def refused(e: BaseException) -> bool:
    """True when an error from a wallet call proves nothing was sent: the wallet answered with an error (RuntimeError
    from our RPC helpers, an HTTP error status) or could not be reached at all (connection refused). A timeout or a
    connection lost mid-call proves nothing: the send may have happened."""
    import urllib.error
    if isinstance(e, (RuntimeError, urllib.error.HTTPError, ConnectionRefusedError)):
        return True
    return isinstance(e, urllib.error.URLError) and isinstance(e.reason, ConnectionRefusedError)


def unresolved(ledger: dict) -> list[dict]:
    return [p for p in ledger.get("payments", []) if p.get("status") == SENDING]


def hold(ledger: dict, ledger_path) -> bool:
    """True (and a loud message) when an earlier run died between recording and saving a send."""
    bad = unresolved(ledger)
    if not bad:
        return False
    print(f"HOLD: {len(bad)} payments are 'sending' from a run that stopped before it knew whether the wallet sent "
          f"them. Nothing is paid until each is resolved by hand:")
    for p in bad:
        print(f"  intent {p['intent']}  {p['addr']}  amount {p['amount']}  recorded {p['ts']}")
    print(f"Check the pool wallet's history, then: python3 kobold_ledger.py resolve {ledger_path} <intent> "
          f"--sent <txid> | --not-sent")
    return True


def resolve(ledger: dict, intent: str, txid: str | None, status: str = "pending") -> list[dict]:
    """By hand, after checking the wallet: txid given = it went out; None = it did not (credited back, 'failed')."""
    ps = [p for p in ledger["payments"] if p.get("intent") == intent and p.get("status") == SENDING]
    if not ps:
        raise SystemExit(f"no 'sending' payment with intent {intent}")
    for p in ps:
        p["resolved_ts"] = int(time.time())
        if txid:
            p["txid"] = txid
            p["status"] = status
            p["resolved"] = "sent (checked by hand)"
        else:
            p["status"] = "failed"
            p["failed_reason"] = "not sent (checked by hand after an interrupted run)"
            ledger["balances"][p["addr"]] = ledger["balances"].get(p["addr"], 0) + p.get("debit", p["amount"])
            ledger["paid"][p["addr"]] = ledger["paid"].get(p["addr"], 0) - p["amount"]
    return ps


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description="Lists or resolves payments left 'sending' by an interrupted payout run.")
    sub = ap.add_subparsers(dest="cmd", required=True)
    ls = sub.add_parser("list")
    ls.add_argument("ledger")
    rs = sub.add_parser("resolve")
    rs.add_argument("ledger")
    rs.add_argument("intent")
    g = rs.add_mutually_exclusive_group(required=True)
    g.add_argument("--sent", metavar="TXID", help="the wallet did send it: this is its transaction id")
    g.add_argument("--not-sent", action="store_true", help="the wallet did not send it: credit the balance back")
    rs.add_argument("--status", default="pending",
                    help="status for a sent payment (default pending, so payout.py follows it; quantus: confirmed)")
    a = ap.parse_args(argv)
    led = Ledger(a.ledger)
    ledger = led.open({})  # takes the run lock: never while payout.py runs
    if a.cmd == "list":
        for p in unresolved(ledger):
            print(json.dumps(p))
        return 0
    ps = resolve(ledger, a.intent, None if a.not_sent else a.sent, a.status)
    led.save(ledger)
    print(f"{len(ps)} payments of {a.intent}: {ps[0]['status']}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
