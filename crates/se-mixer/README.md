# se-mixer

PreSonus StudioLive Series III adapter (built for the 16R, PLAN §8.7): UCNET discovery,
control session, meters, two-way sync with the engine state, mix snapshots, `mic.talking`.
Operator documentation: [`docs/mixer.md`](../../docs/mixer.md).

## Attribution

The UCNET protocol layer (`src/ucnet/`) is a Rust port of
[featherbear/presonus-studiolive-api](https://github.com/featherbear/presonus-studiolive-api)
by Andrew Wong, MIT License — see [`LICENSE-THIRD-PARTY`](LICENSE-THIRD-PARTY) for the file
mapping and the license text.

## Layout

| Module | Role |
|---|---|
| `ucnet::packet` | framing, TCP stream reassembly |
| `ucnet::msg` | `PV`/`PS`/`MS fdrs`/`JM`/`ZB`/`CK`/`FD` decoding; subscribe, keep-alive, meter hello, set builders |
| `ucnet::ubjson`, `ucnet::tree` | state payload (zlib UBJSON) → flat `group/chN/param` paths |
| `ucnet::meters` | UDP `levl` meter frames |
| `ucnet::discovery` | broadcast announcements + ARP/TCP LAN probe |
| `ucnet::client` | one session: handshake, keep-alive/liveness, meters, send queue |
| `map` | `mixer.16r.*` addresses ↔ console paths, coverage, fader law |
| `sync` | echo suppression, readback vs. engine intent, ≤ 50 Hz latest-value-wins |
| `snapshots`, `talk`, `config`, `service` | mixes, voice activity, `[mixer]`, the subsystem |

## Fixtures

`tests/fixtures/` holds traffic recorded from the owner's 16R (firmware 3.2.0.108461) with
`examples/ucnet_capture.rs`; re-record after a firmware update:

```sh
cargo run -p se-mixer --example ucnet_capture -- --host 10.0.0.187 --out crates/se-mixer/tests/fixtures --exercise 16
```

`--exercise N` toggles line channel N's mute and fader and restores both; it refuses a
channel with input signal.
