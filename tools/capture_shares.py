#!/usr/bin/env python3
"""Records real rieMiner shares as test vectors for riecoin-pool, the way rieMiner's TestServer.cpp talks to it:
subscribe -> [[["mining.notify", "00"]], "00", 1], authorize -> true, then a notify with a zero coinbase (42 zero
bytes + the height push), no merkle branch, version 20000000, nBits = difficulty * 256, the current time, clean,
PoW version 1 and the patterns. Each submit is answered true and checked here with a second, independent
implementation of the share rule (plain Python integers), so the Rust check is compared against it.

    python3 capture_shares.py --port 3004 --out shares.json --phase 768:5:12 --phase 540:7:12
    (phase = difficulty:pattern length (5 = TestServer's quintuplets, 7 = mainnet's septuplets):shares wanted)
"""
import argparse
import hashlib
import json
import socket
import time

PATTERNS = {5: [[0, 4, 2, 4, 2], [0, 2, 4, 2, 4]], 7: [[0, 2, 4, 2, 4, 6, 2], [0, 2, 6, 4, 2, 4, 2]]}


def dsha(b: bytes) -> bytes:
    return hashlib.sha256(hashlib.sha256(b).digest()).digest()


def first_primes(count: int) -> list[int]:
    out, x = [], 2
    while len(out) < count:
        if all(x % p for p in out if p * p <= x):
            out.append(x)
        x += 1
    return out


PRIMES = first_primes(2000)  # rieMiner's primorial numbers stay far below this


def height_push(h: int) -> bytes:
    n = (h.bit_length() + 8) // 8  # TestServer: 1, 2 or 3 bytes
    return bytes([n]) + h.to_bytes(n, "little")


def k_of(header80: bytes, nbits: int, noffset: bytes, patterns) -> int:
    """The share's prime count, from the consensus description (not from the Rust code)."""
    h = int.from_bytes(dsha(header80), "little")
    lo16 = int.from_bytes(noffset[0:2], "little")
    if lo16 & 31 != 2:
        return -1
    bits = nbits + (0 if lo16 == 2 else (lo16 & 0xFFE0) << 8)
    zeros = (bits >> 8) + 1 - 265
    df = nbits & 255
    lead = 256 + ((10 * df ** 3 + 7383 * df ** 2 + 5840720 * df + 3997440) >> 23)
    target = ((lead << 256) + h) << zeros
    pn = int.from_bytes(noffset[30:32], "little")
    prim = 1
    for p in PRIMES[:pn]:
        prim *= p
    factor = int.from_bytes(noffset[14:30], "little")
    poff = int.from_bytes(noffset[2:14], "little")
    offset = prim - target % prim + factor * prim + poff
    if offset >= 1 << zeros or prim > 1 << zeros:
        return -2
    n = target + offset
    best = 0
    for pat in patterns:
        x, count = n, 0
        for i, step in enumerate(pat):
            x += step
            if pow(2, x - 1, x) == 1:
                count += 1
            elif i < 2:
                count = 0
                break
        best = max(best, count)
    return best


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=3004)
    ap.add_argument("--out", default="shares.json")
    ap.add_argument("--phase", action="append", required=True)
    args = ap.parse_args()
    srv = socket.socket()
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("0.0.0.0", args.port))
    srv.listen(1)
    print(f"waiting for rieMiner on port {args.port}", flush=True)
    conn, _ = srv.accept()
    f = conn.makefile("rwb", buffering=0)

    def send(obj):
        f.write((json.dumps(obj) + "\n").encode())

    def read():
        line = f.readline()
        if not line:
            raise SystemExit("rieMiner disconnected")
        return json.loads(line)

    req = read()
    send({"id": req["id"], "result": [[["mining.notify", "00"]], "00", 1], "error": None})
    req = read()
    send({"id": req["id"], "result": True, "error": None})
    vectors, height, job = [], 1000, 0
    for phase in args.phase:
        diff, plen, wanted = (int(x) for x in phase.split(":"))
        pats = PATTERNS[plen]
        coinb1 = bytes(42) + height_push(height)
        ntime = int(time.time())
        notify = [f"{job:x}", "00" * 32, coinb1.hex(), "", [], "20000000", f"{diff * 256:08x}", f"{ntime:08x}", True, 1, pats]
        send({"id": None, "method": "mining.notify", "params": notify})
        print(f"phase difficulty {diff}, {plen}-tuples, height {height}", flush=True)
        got = 0
        while got < wanted:
            req = read()
            if req.get("method") != "mining.submit":
                send({"id": req.get("id"), "result": True, "error": None})
                continue
            user, jid, en2, nt, noff = req["params"]
            send({"id": req["id"], "result": True, "error": None})
            coinbase = coinb1 + bytes.fromhex("00") + bytes.fromhex(en2)
            root = dsha(coinbase)
            hdr = (0x20000000).to_bytes(4, "little") + bytes(32) + root + int(nt, 16).to_bytes(8, "little") + (diff * 256).to_bytes(4, "little")
            k = k_of(hdr, diff * 256, bytes.fromhex(noff)[::-1], pats)
            vectors.append({"notify": notify, "extranonce1": "00", "submit": req["params"], "k": k})
            got += 1
            print(f"  share {got}: k = {k}", flush=True)
        height += 1
        job += 1
    with open(args.out, "w") as o:
        json.dump(vectors, o, indent=1)
    print(f"wrote {len(vectors)} vectors to {args.out}")
    conn.close()


if __name__ == "__main__":
    main()
