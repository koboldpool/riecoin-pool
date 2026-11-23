# riecoin-pool

The mining pool software behind [Kobold Pool](https://koboldpool.com/ric/) for **Riecoin (RIC)**, the coin whose
proof of work finds prime constellations ([riecoin.dev](https://riecoin.dev)). Published so miners can check how their
shares are counted and paid.

| Program | What it does |
|---|---|
| `riecoin-pool` | the pool server: rieMiner's stratum (PPLNS port and solo port), every share checked with the Stella rule, work from a stock Riecoin Core node |
| `payout.py` | PPLNS payouts from the node's wallet; tracks every payment until it is deep in the chain |
| `examples/testminer.rs` | a small test client that speaks the pool's stratum |

## Mine on Kobold Pool

Use the official [rieMiner](https://riecoin.dev/en/rieMiner) (no fee):

```
rieMiner noconf Mode=Pool Host=ric.koboldpool.com Port=3540 Username=ric1YOURADDRESS.rig1 Password=x Threads=0
```

Port 3540 is PPLNS, 3550 solo. `Threads=0` uses every core. The speed is shown in shares per second.

## How shares and payouts work

- **Work.** The pool asks its node for a block template (`getblocktemplate`) and sends rieMiner a Bitcoin-style stratum
  job plus the PoW version and the accepted constellation patterns. Each connection has its own extranonce.
- **Share check.** A share is a number whose pattern has its first two numbers prime and at least 5 of 7 prime
  (pattern length − 2, at least 4). The pool checks every submitted share itself (`src/stella.rs`, tested against real
  mainnet blocks and real rieMiner shares). Duplicates and stale jobs are refused. Every share counts the same.
- **Blocks.** A share that is a full constellation goes to the node with `submitblock`; the answer is logged with the
  fee in force when the block was found.
- **PPLNS.** When a block is mature (100 confirmations) and still on the best chain, `payout.py` splits its reward,
  minus that block's fee, over the pool shares in the last 2 × (expected shares per block) before it. Solo-port blocks
  go whole to their finder, minus the solo fee.
- **Payments.** Balances at or above the minimum (`--min`, 1 RIC on Kobold Pool) are paid in one `sendmany` per run;
  the pool wallet pays the network fee. Each payment is written to the ledger (`<data>/payouts.json`) and checked until
  it is deep in the chain; one that disappears is marked failed and credited back. A block is never credited twice.
  Blocks mined to the pool wallet's own address stay in the wallet.
- **Fees.** `<data>/pool-config.json`, `{"fee_percent": 0.0, "solo_fee_percent": 0.0}`, re-read every 10 s.
- The pool hands out no work while its node is syncing, has no peers, or its tip is older than `--max-tip-age`.

## Build

Rust 1.85 or later:

```
cargo build --release --locked
cargo test --release
python3 -m unittest test_payout
```

GitHub Actions (`.github/workflows/build.yml`) does the same on every push and attaches the Linux binary with
`SHA256SUMS` to each release (tag `v*`).

## Run

1. A stock Riecoin Core node (`riecoind`, v2606), synced, with RPC on 127.0.0.1 (cookie auth).
2. The pool wallet in that node: `deploy/wallet-create.py` makes an encrypted wallet and its first address. Back up
   the wallet and keep the passphrase offline. Every block pays this address.
3. The pool:

   ```
   riecoin-pool --network mainnet --address <pool address> --cookie <node dir>/.cookie --data pool-data \
                [--listen 0.0.0.0:3540] [--listen-solo 0.0.0.0:3550]
   ```

4. Payouts every 30 minutes or so; without `--pay` it only reports:

   ```
   python3 payout.py --network mainnet --data pool-data --cookie <node dir>/.cookie --wallet pool \
       --passphrase-file <file with the wallet passphrase> --min 1 --pay
   ```

`deploy/` has example systemd units for all of this.

## License

MIT (`LICENSE`). See `NOTICE` for where the protocol details come from.
