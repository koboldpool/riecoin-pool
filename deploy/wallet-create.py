#!/usr/bin/env python3
"""Creates the Riecoin pool wallet ("pool", encrypted, loaded at node start) without the passphrase ever being on a
command line or in shell history: it is typed into a hidden prompt (twice), sent to the node's createwallet over
JSON-RPC (cookie auth, 127.0.0.1), and written to a 0600 file for the payout service (payout.py --passphrase-file).
From bound-pool's wallet-create.py.

Run as the pool user, by the wallet owner:
    sudo -u kobold python3 /srv/kobold/coins/riecoin/wallet-create.py \
        --cookie /srv/kobold/coins/riecoin/node/.cookie --pass-file /srv/kobold/coins/riecoin/.wallet-pass

Prints a new address for riecoin-pool --address. Back up the encrypted wallet folder node/wallets/pool
(never .wallet-pass) and keep the passphrase somewhere safe offline.
"""
import argparse
import base64
import getpass
import json
import os
import sys
import urllib.error
import urllib.request


def rpc(url, cookie, method, params):
    body = json.dumps({"jsonrpc": "1.0", "id": 1, "method": method, "params": params}).encode()
    req = urllib.request.Request(url, data=body, headers={
        "content-type": "application/json",
        "authorization": "Basic " + base64.b64encode(cookie.encode()).decode()})
    try:
        with urllib.request.urlopen(req, timeout=120) as r:
            out = json.loads(r.read())
    except urllib.error.HTTPError as e:
        out = json.loads(e.read() or b"{}")
    if out.get("error"):
        raise SystemExit(f"{method}: {out['error']}")
    return out["result"]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rpc", default="http://127.0.0.1:28332/")
    ap.add_argument("--cookie", required=True)
    ap.add_argument("--pass-file", required=True)
    ap.add_argument("--name", default="pool")
    a = ap.parse_args()
    cookie = open(a.cookie).read().strip()
    if os.path.exists(a.pass_file):
        sys.exit(f"{a.pass_file} exists already; not overwriting it")
    pw = getpass.getpass("Pool wallet passphrase: ")
    if not pw or pw != getpass.getpass("Again: "):
        sys.exit("empty, or the two did not match; nothing done")
    fd = os.open(a.pass_file, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w") as f:
        f.write(pw + "\n")
    rpc(a.rpc, cookie, "createwallet", {"wallet_name": a.name, "passphrase": pw, "load_on_startup": True})
    del pw
    addr = rpc(a.rpc.rstrip("/") + f"/wallet/{a.name}", cookie, "getnewaddress", ["pool", "bech32"])
    print("\npool address for riecoin-pool --address:", addr)
    print(f"passphrase saved to {a.pass_file} (mode 600)")
    print("back up the encrypted wallet folder node/wallets/" + a.name + " and keep the passphrase offline")


if __name__ == "__main__":
    main()
