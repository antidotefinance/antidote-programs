# Antidote programs

Source of the Solana programs behind Antidote, published so anyone can rebuild them and check
the bytes deployed on mainnet (Solana Explorer / Solscan "Program is verified", via OtterSec).

| Program | Directory | Library name | Program id |
| --- | --- | --- | --- |
| Antidote router (routed trades, loss receipts) | programs/tilt-spot | tilt_spot | 5iWcJpM7vGksXq7LxusmmfpPLZNmmWxjNC8aRaFJtoE8 |
| Antidote fair-open hook (Token-2022 transfer hook) | programs/levpad-hook | levpad_hook | 3ri134ez6K1QPUV4XMhAKeJbfhBDW55bQCoGJZL6cmxc |
| Antidote cap (1B capped mint authority) | programs/levcap | levcap | Gq9eEG2mnAefVGtcgnmZeutUitKvZoxErxL27RALJ7U |
| Antidote fees (fee router, buy and burn) | programs/levfee | levfee | 6MBSd18Ce5pEJ28qXktoxxtJEvopukTBEBCVknvTkhbM |
| Antidote ideas (holder suggestions and votes) | programs/antidote-ideas | antidote_ideas | 6a4WDHiWcpGV1WB8QjEoT3bxRGCHo2HnvFgiF8NW4bqz |

levcap's deployed bytes are verified from https://github.com/antidotefinance/antidote-cap; its source is
also here because the workspace builds it.

## Reproduce

Anchor 0.31.1, solana-cargo-build-sbf 3.1.12, platform-tools v1.52, built with solana-verify 0.5.2
in the image pinned by digest:

```sh
cargo install solana-verify --version 0.5.2 --locked
solana-verify build --library-name tilt_spot \
  --base-image solanafoundation/solana-verifiable-build@sha256:5d7b60cdda413d3a482b29f988e434606e206556590d1175cbee69a2728ddfc6
solana-verify get-executable-hash target/deploy/tilt_spot.so
solana-verify -u https://api.mainnet-beta.solana.com get-program-hash 5iWcJpM7vGksXq7LxusmmfpPLZNmmWxjNC8aRaFJtoE8
```

(same for levpad_hook, levcap, levfee and antidote_ideas). Or in one step:
`solana-verify verify-from-repo -u https://api.mainnet-beta.solana.com https://github.com/antidotefinance/antidote-programs --program-id <id> --library-name <lib> --base-image <image above>`.

The workspace member `tests` holds only its manifest here: it keeps Cargo.lock exactly as built.
Security: report vulnerabilities privately, see SECURITY.md.

License: Business Source License 1.1 (BUSL-1.1), see LICENSE. Licensor Antidote; no Additional Use
Grant; each version converts to the Apache License, Version 2.0 on its Change Date (two years after
its release; LICENSE names the date).
