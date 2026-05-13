# `reth-research` systemd unit

`reth-research.service` is the unit file used to run the research ExEx
under systemd on a long-lived host. It's checked into the repo so the
on-host `/etc/systemd/system/reth-research.service` doesn't drift from
what's been reviewed.

## Installing

```bash
sudo cp /home/ubuntu/reth/bin/reth-research/systemd/reth-research.service \
        /etc/systemd/system/reth-research.service
sudo systemctl daemon-reload
sudo systemctl enable --now reth-research
```

## Refreshing after a repo change

The unit lives at `/etc/systemd/system/` and is **not** auto-synced from
the working tree. After a `git pull` that touches this file:

```bash
sudo cp /home/ubuntu/reth/bin/reth-research/systemd/reth-research.service \
        /etc/systemd/system/reth-research.service
sudo systemctl daemon-reload
sudo systemctl restart reth-research
```

`ExecStartPre` removes the cached `target/release/reth-research` so the
service rebuilds on restart and picks up any code changes from the pull.

## Notes

- The flags in `ExecStart` are sized for the `gas-repricing` /
  Xeon-5412U / 128 GB host. For other shapes, override
  `--research.backfill-concurrency` (typical: `cores − 2`),
  `MemoryHigh` / `MemoryMax`, and the `RETH_*` environment knobs.
- `--research.db-path` must point at a *writable* SQLite file. The
  service runs as `ubuntu`, so the parent directory needs `chown
  ubuntu:ubuntu`.
- `--research.csv 7904-prelim=...` references a host-local file. If the
  CSV path differs on a new host, update it in the unit before
  installing.
- `--authrpc.jwtsecret` must match the consensus client's JWT secret.
