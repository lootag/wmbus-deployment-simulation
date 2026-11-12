# wmbus-battery-simulation

Reliability simulation of DSMR5 and DSMR6 gas meters (EMM) talking wireless
M-Bus to shared gateways in a dense district, built on
[radiofreccia](https://github.com/lootag/radiofreccia). It runs every meter and gateway in virtual
time and scores the readings each gateway receives:

1. a periodic push every 5 minutes (C1, SND_NR, no receive window; unsigned);
2. an hourly billing push (signed in DSMR6), followed by a receive window in
   which the gateway may open a FAC;
3. the gateway's daily SND_NKE heartbeat after the midnight billing push;
4. DSMR6's DTLS 1.2 handshake, monthly and after a gateway power cut,
   replayed frame by frame from the real capture in
   `../app-svc-wmbus/hal-dtls.pcap`: 25 meter frames, 21 gateway frames.

```sh
cargo run --release                                         # all variants, dashboards in dashboards/
cargo run --release -- --variants dsmr5,dsmr6-by-the-book   # a selection
cargo run --release -- --config configs/worst-case-6000.json --out dashboards/worst-case-6000
cargo run --release -- --print-config > my.json             # every parameter, as JSON
cargo run --release -- --config my.json                     # your own numbers
cargo test                                                  # frame sizes checked against the capture
```

### Reliability in a dense district: DSMR5 against DSMR6

A dense Dutch city district: about 6,000 people per km², so 3,000 households
(one gas meter each) per km². 750 meters on 500 × 500 m, 49 shared gateways
80 m apart, identical layout for all three variants; each meter's clock is
off by a random 0–2 s and drifts within ±20 ppm. Only the 270 meters at
least 100 m from the edge are scored. Target: 99.5 % of readings per meter,
the DSMR5 requirement. CSMA follows the preferred implementation in the
DSMR6 G-meter CSMA requirements, which is also DSMR5's
procedure.

The comparison: both standards as documented, with DSMR6 signing only its
hourly values, and DSMR6 with its hourly pushes spread like DSMR5's.

- **DSMR5:** 5-minute messages in the 10 s after each boundary, hourly
  messages spread over 1–10 minutes (T1), mode 9, nothing signed.
- **DSMR6 by the book:** 5-minute and hourly pushes each at a random moment
  in the 10 s after their boundary `[CS 4.9]`, only hourly pushes signed,
  DTLS.
- **DSMR6 hourly 10 min:** the same, but the signed hourly push goes at a
  random moment in the 10 minutes after the hour.

| Steady state, 2 days | DSMR5 | DSMR6 by the book | DSMR6 hourly 10 min |
|---|---|---|---|
| 5-minute readings delivered (mean / worst 1 %) | 83.5 % / 79.4 % | 77.9 % / 72.6 % | 79.0 % / 73.6 % |
| Hourly readings delivered (mean / worst 1 %) | 98.6 % / 93.5 % | 66.2 % / 50.0 % | **97.8 % / 91.3 %** |
| Meters reaching 99.5 % | 0 % | 0 % | 0 % |
| Pushes deferred by CSMA | 5.4 % | 7.7 % | 6.3 % |
| Meter average current | 1.85 µA | 2.1 µA | 2.1 µA |

| Gateway power cut (10 min) | DSMR5 | DSMR6 by the book | DSMR6 hourly 10 min |
|---|---|---|---|
| Session back (median / 99 % / last meter) | not needed | 1.5 h / 5.5 h / 7.5 h | **0.65 h / 2.5 h / 2.7 h** |
| Handshakes succeeding | – | 271 of 457 | 271 of 331 |
| Gateway replies missed (radio busy) | 0 | 257 | 86 |
| 5-min readings lost to the cut itself, per meter (median, approx.)¹ | ~4 | ~18 | ~12 |

¹ Readings lost from the cut on, minus what the steady-state loss rate
would have cost over the same hours anyway.

Dashboards: `dashboards/dsmr5.html`, `dashboards/dsmr6-by-the-book.html`,
`dashboards/dsmr6-hourly-10min.html`.

**Worst case: 6,000 meters per km²**, the density of the very densest
neighbourhoods (`--config configs/worst-case-6000.json`, dashboards in
`dashboards/worst-case-6000/`). Same pattern, larger losses:

| 6,000 per km² | DSMR5 | DSMR6 by the book | DSMR6 hourly 10 min |
|---|---|---|---|
| 5-minute readings delivered (mean) | 73.6 % | 68.1 % | 68.4 % |
| Hourly readings delivered (mean) | 97.3 % | 65.1 % | 96.5 % |
| Power cut: session back (median / last meter) | not needed | 2.5 h / 7.5 h | 0.8 h / 3.6 h |

- **Both standards squeeze every 5-minute push into the same 10 s after each
  boundary** (DSMR5 P2 3.1.8, `[CS 4.9]`), concentrating the traffic
  30-fold. CSMA moves retries out by seconds, but only 5–8 % of pushes find
  the channel busy, so it can't undo that.
- **DSMR6 loses 5–6 points more of its 5-minute readings.** Even unsigned,
  a DSMR6 push is 70 bytes against DSMR5's 51, because of the DTLS record
  overhead, and the signed hourly pushes share the same 10 s.
- **The hourly gap is the schedule, and spreading it closes it.** By the
  book, DSMR6 gets 66 % of hourly readings against DSMR5's 99 %; spreading
  the hourly push over 10 minutes brings DSMR6 to 98 %. The DSMR6 companion
  standard's own reference for the hourly interval points to a section that
  doesn't exist (§4.8.1, "section 5.9"), so this is worth raising with
  Netbeheer Nederland.
- **The 5-minute readings barely improve.** Their 10 s window is unchanged,
  and the gap to DSMR5 is DTLS's larger frames.
- **Power cuts cost DSMR6 hours, less with the spread.** DSMR5's key survives
  a power cut. Under DSMR6 every meter needs a new handshake, which can only
  start in the receive window after its hourly push; a gateway runs at most 8
  at once (configurable), and its single half-duplex radio misses replies
  while serving several meters. Spreading the hourly pushes spreads those
  windows over 10 minutes instead of 10 seconds: the median meter is back
  within 0.65 h instead of 1.5 h, and the last within 2.7 h instead of 7.5 h.
  That's still about an hour of readings per meter per cut, against a
  monthly budget of about 3.6 hours; gateway-side fixes (DTLS session
  resumption, timing-aware handshake scheduling) are the next lever.

**How far to trust this.** The absolute loss rates are probably too
pessimistic: if DSMR5 really lost a sixth of its 5-minute readings in
dense areas, it would be known. Two parts of the model push losses up:

- **Geometry:** meters sit on a flat plane with one path-loss slope
  (exponent 3.5, 6 dB shadowing), so every gateway hears the whole district.
  Real buildings isolate meters far more.
- **Receiver:** it can't re-lock onto a stronger frame mid-reception, which
  real gateways may do (both standards allow this capture).

The relative result is more robust: under the same conditions DSMR6 loses
more than DSMR5, mostly through its hourly schedule. To calibrate the
absolute level, tune the path-loss slope until the DSMR5 run matches real
DSMR5 5-minute availability for a dense area; the DSMR6 runs then become
predictions.

### Open questions

1. Which battery cell does the meter use, and what do metrology and the
   display draw?
2. How long does one P-256 sign take on the meter, and does it use the PKA
   accelerator?
3. What is the intended hourly randomisation interval in DSMR6 (the
   companion standard's reference is broken)?

## What it models

- **PHY:** C-mode M2O (100 kcps 2-FSK on 868.95 MHz, 19 × "01" preamble) for
  meter frames, and O2M (50 kcps GFSK on 869.525 MHz, 15 × "01") for gateway
  frames. The meter retunes and switches PHY for every receive window.
- **Frame sizes:** DLL, ELL, TPL (security mode 13), DTLS record (AES-GCM)
  and DSMR6 message layouts from the protocol spec, plus Format B CRCs. The
  layouts match the capture byte for byte.
- **Timing:** randomisation within 10 s of each slot, tRO, and the FAC
  cadence tTxD (2 s, as in the capture).
- **CSMA** on pushes, as in the preferred implementation of the DSMR6
  G-meter CSMA requirements (`CSMA.pdf`), also DSMR5's: listen 5 ms
  (+0.5–5 ms random after a busy check) at −85 dBm; if busy, receiver off and
  wait 0.5–2 s, 0.5–2 s, then 5 s; after a fourth busy check, send anyway in
  what's left of the window, or 1 s later (configurable under
  `protocol.csma_*`).
- **Sessions:** monthly rekey, the gateway's Channel Request after a
  restart, binding mode (a ClientHello every 15 min until a handshake
  succeeds). A push only counts if the gateway holds the session it was
  encrypted under.
- **Gateway:** one half-duplex radio that switches between listening on
  868.95 MHz and replying on 869.525 MHz, a cap on concurrent handshakes, and
  a 10 % duty cycle on its replies.
- **Energy:** the meter's receive windows are tRO ± guard. Radio state
  transitions, SPI latency and the MCU's sleep and wake-ups are all
  charged.
- **Cryptography:** P-256 ECDSA and ECDHE run in zero simulated time, so
  they are charged on top at the MCU's active current.
- **Links:** log-distance path loss (exponent 3.5) over the meters' and
  gateways' positions, with 6 dB of per-frame shadowing. Lost handshake
  frames are retransmitted by the FAC.

## The district and its variants

Gas meters at a dense Dutch city density (3,000 households/km² over
500 × 500 m by default; `configs/worst-case-6000.json` for the densest
neighbourhoods) on dedicated gateways 80 m apart, each serving the meters in
its cell. Both companion standards run on the same layout:

| | `dsmr5`: DSMR5 (Enexis P2 5.1) | `dsmr6-by-the-book`: DSMR6 (X2-1) | `dsmr6-hourly-10min` |
|---|---|---|---|
| 5-minute message | C1, 52 bytes, random moment in the 10 s after the boundary | C1, unsigned, DTLS record (70 bytes), same window | the same |
| Hourly message | T1 (3-out-of-6), spread over 1–10 min | C1, signed, random moment in the 10 s after the hour | C1, signed, random moment in the 10 min after the hour |
| Reply window | T2, 2–3 ms after the hourly message | C2, 1 s after the hourly push | the same |
| Security | mode 9, key installed once | DTLS, monthly rekey, re-handshake after a gateway restart | the same |
| CSMA | as specified (`CSMA.pdf`, DSMR5 P2 11.1) | the same | the same |

Each gets a steady-state run, scored against the DSMR5 target of 99.5 % of
readings per meter and written as a dashboard (`dashboards/<variant>.html`),
and a run with an area power cut. Meters near the edge make traffic but
aren't scored. `cargo run --release` runs all three (about 4 minutes at
3,000 per km², 8 at 6,000); `--variants dsmr5` runs one. Settings live
under `district` in the config (`--print-config`).

Simplifications: the gateway receives T1 frames with its C1 receiver settings
(the payload is stretched 1.5× for 3-out-of-6 coding), and both profiles use
the same meter transmit power.

## What it does not model

Metrology, display, sensors and anything else outside communication, orphan
mode (21 days without commands), firmware updates
and billing-log reads. Traffic other than the simulated gas meters on the
same channels. A receiver that re-locks onto a stronger frame mid-reception: the
simulator's receivers stay locked onto the first frame they detect.

## Numbers to replace

Every current and timing in `src/params.rs` marked `[EST]` is an estimate
of datasheet order for an STM32WL (which the Flonidan SBOM points to), not a
measurement. Measure the meter in each state (sleep, TX at its real power,
RX, standby, a P-256 sign/verify) and put the values in a config file.
