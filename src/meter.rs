//! The gas meter (EMM): periodic and billing pushes, the FAC after billing
//! pushes, the daily heartbeat FAC, DTLS sessions (rekeyed monthly,
//! re-established on a gateway's Channel Request) and binding mode.
//!
//! It drives a `VirtualRadio` directly rather than the core `Radio` trait,
//! because C-mode switches PHY between transmitting (M2O) and receiving
//! (O2M), and the core traits have no notion of a PHY.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use radiofreccia_core::{CarrierSense, Channel, Clock, RadioError, RxRadio, TxRadio};
use radiofreccia_sim::{PhyConfig, SimClock, VirtualRadio};
use serde::Serialize;

use crate::frames::{self, Header, Kind, Turn};
use crate::params::{Params, Profile};
use crate::radio::{self, ReplyLink, M2O_HZ};

/// What the meter spends charge on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Activity {
    /// 5-minute periodic push: wake, transmit, sleep.
    Periodic,
    /// Hourly billing push: wake, transmit, sleep.
    Billing,
    /// The receive window after each billing push in which the gateway may
    /// open a FAC; almost always empty.
    FacWindow,
    /// The FAC opened by the gateway's daily heartbeat.
    Heartbeat,
    /// DTLS handshakes, from ClientHello to the close of their FAC,
    /// including failed attempts in binding mode.
    Handshake,
}

/// Charge per activity, mAh. `sim` is what the simulator measured on the
/// radio and MCU; `compute` is public-key cryptography charged on top.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Ledger {
    pub sim: BTreeMap<Activity, f64>,
    pub compute: BTreeMap<Activity, f64>,
}

impl Ledger {
    pub fn compute_total(&self) -> f64 {
        self.compute.values().sum()
    }
}

/// Why a handshake started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Trigger {
    /// The session reached its lifetime.
    Rekey,
    /// The gateway sent a Channel Request after a restart.
    ChannelRequest,
    /// Binding mode: no session, the meter tries again.
    Binding,
}

/// One handshake attempt.
#[derive(Debug, Clone, Serialize)]
pub struct HandshakeRecord {
    pub trigger: Trigger,
    pub ok: bool,
    /// When it started, s since the start of the run (meter clock).
    pub start_s: f64,
    pub duration_s: f64,
    pub meter_tx: u32,
    pub gw_frames_received: u32,
    pub charge_mah: f64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Counters {
    pub periodic_sent: u64,
    pub billing_sent: u64,
    /// Periodic slots not sent for lack of a session.
    pub periodic_no_session: BTreeSet<u64>,
    /// Billing intervals not sent for lack of a session.
    pub billing_no_session: BTreeSet<u64>,
    pub fac_windows: u64,
    pub heartbeats_heard: u64,
    pub channel_requests_heard: u64,
    pub fac_transmissions: u64,
    /// Pushes deferred by listen before talk at least once.
    pub lbt_deferred: u64,
    /// Pushes sent although the channel stayed busy.
    pub lbt_forced: u64,
    pub handshakes: Vec<HandshakeRecord>,
}

/// Where a meter starts.
#[derive(Debug, Clone, Copy)]
pub struct MeterSetup {
    pub id: u64,
    /// Offset of the meter's push grid from the clock: zero when pushes are
    /// clock-aligned, as `[CS 4.9]` reads.
    pub phase: Duration,
    /// When its current session reaches its lifetime and it rekeys
    /// (`Duration::MAX`: never).
    pub first_rekey: Duration,
}

pub struct Meter {
    pub id: u64,
    phase: Duration,
    first_rekey: Duration,
    p: Arc<Params>,
    m2o: PhyConfig,
    reply: ReplyLink,
    rng: StdRng,
    pub ledger: Ledger,
    pub counters: Counters,
}

struct FacOutcome {
    ok: bool,
    turns_completed: usize,
    tx: u32,
    rx: u32,
}

fn ms(v: f64) -> Duration {
    Duration::from_secs_f64(v / 1000.0)
}

fn secs(v: f64) -> Duration {
    Duration::from_secs_f64(v)
}

impl Meter {
    pub fn new(setup: MeterSetup, p: Arc<Params>) -> Self {
        let seed = p.seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ setup.id;
        Self {
            id: setup.id,
            phase: setup.phase,
            first_rekey: setup.first_rekey,
            m2o: radio::m2o(p.radio.gw_sensitivity_dbm),
            reply: ReplyLink::for_params(&p),
            rng: StdRng::seed_from_u64(seed),
            p,
            ledger: Ledger::default(),
            counters: Counters::default(),
        }
    }

    /// The instant of periodic slot `k`, meter clock.
    pub fn slot_time(p: &Params, phase: Duration, k: u64) -> Duration {
        phase + Duration::from_secs(p.protocol.periodic_interval_s * k)
    }

    /// Periodic slots per billing interval.
    pub fn slots_per_billing(p: &Params) -> u64 {
        (p.protocol.billing_interval_s / p.protocol.periodic_interval_s).max(1)
    }

    fn charge(radio: &VirtualRadio) -> f64 {
        radio.energy().total_mah()
    }

    fn book(&mut self, a: Activity, mah: f64) {
        *self.ledger.sim.entry(a).or_default() += mah;
    }

    fn book_compute(&mut self, a: Activity, ms_active: f64) -> f64 {
        let mah = self.p.mcu.active_ma * ms_active / 3_600_000.0;
        *self.ledger.compute.entry(a).or_default() += mah;
        mah
    }

    pub async fn run(&mut self, radio: &mut VirtualRadio) -> Result<(), RadioError> {
        let clock = radio.clock();
        let pr = self.p.protocol.clone();
        radio.set_tx_power(self.p.radio.tx_power_dbm).await?;
        radio.enter_sleep().await?;

        let lifetime = secs(pr.handshake_interval_days * 86_400.0);
        let binding_retry = secs(pr.handshake_retry_s);
        let per_billing = Self::slots_per_billing(&self.p);
        // The session in use: established before the run started.
        let mut epoch: u16 = 0;
        let mut session = true;
        // While in session: when to rekey. In binding mode: next attempt.
        let mut next_handshake = self.first_rekey;
        // Next periodic slot and when it goes out.
        let mut k: u64 = 0;
        let mut periodic_at = self.slot_time_jittered(k);
        // A billing push waiting to go out: when, and its interval.
        let mut billing: Option<(Duration, u64)> = None;
        loop {
            let billing_at = billing.map_or(Duration::MAX, |(at, _)| at);
            if next_handshake <= periodic_at.min(billing_at) {
                clock.sleep_until(next_handshake).await;
                let trigger = if session { Trigger::Rekey } else { Trigger::Binding };
                let ok = self.handshake(radio, &clock, clock.now(), epoch.wrapping_add(1), trigger).await?;
                if ok {
                    epoch = epoch.wrapping_add(1);
                    session = true;
                    next_handshake = clock.now() + lifetime;
                } else {
                    session = false;
                    next_handshake = clock.now() + binding_retry;
                }
                continue;
            }
            if billing_at < periodic_at {
                let (at, interval) = billing.take().expect("billing pending");
                clock.sleep_until(at).await;
                if !session {
                    self.counters.billing_no_session.insert(interval);
                    continue;
                }
                let t = interval * pr.billing_interval_s;
                let midnight = t.is_multiple_of(86_400);
                let fac_credit = t.is_multiple_of(pr.fac_interval_s.max(pr.billing_interval_s));
                if let Some(start) = self.billing(radio, &clock, interval, epoch, midnight, fac_credit).await? {
                    // Channel Request: the gateway lost the session.
                    session = false;
                    let first = start + secs(pr.fac_tx_delay_s);
                    let ok = self
                        .handshake(radio, &clock, first, epoch.wrapping_add(1), Trigger::ChannelRequest)
                        .await?;
                    if ok {
                        epoch = epoch.wrapping_add(1);
                        session = true;
                        next_handshake = clock.now() + lifetime;
                    } else {
                        next_handshake = clock.now() + binding_retry;
                    }
                }
                continue;
            }
            clock.sleep_until(periodic_at).await;
            if session {
                self.periodic(radio, &clock, k, epoch).await?;
            } else {
                self.counters.periodic_no_session.insert(k);
            }
            if k.is_multiple_of(per_billing) {
                let slot_at = Self::slot_time(&self.p, self.phase, k);
                let at = match pr.billing_spread_s {
                    Some([lo, hi]) => slot_at + secs(self.rng.random_range(lo..hi)),
                    None => clock.now() + ms(pr.billing_after_periodic_ms),
                };
                billing = Some((at, k / per_billing));
            }
            k += 1;
            periodic_at = self.slot_time_jittered(k);
        }
    }

    fn slot_time_jittered(&mut self, k: u64) -> Duration {
        let window = self.p.protocol.randomisation_window_s;
        let jitter = if window > 0.0 { self.rng.random_range(0.0..window) } else { 0.0 };
        Self::slot_time(&self.p, self.phase, k) + secs(jitter)
    }

    async fn transmit(
        &mut self,
        radio: &mut VirtualRadio,
        kind: Kind,
        turn: usize,
        bytes: usize,
        epoch: u16,
        slot: u64,
    ) -> Result<(), RadioError> {
        let header = Header {
            kind,
            meter: self.id,
            turn: turn as u16,
            epoch,
            slot: slot as u32,
        };
        radio.transmit(&frames::encode_bytes(header, bytes)).await
    }

    /// CSMA as in the DSMR6 G-meter CSMA requirements' preferred
    /// implementation (and DSMR5 P2 CS 11.1): check, back off with the
    /// receiver off, check again; after the last busy check, send anyway at
    /// a random moment in what is left of the window ending at `window_end`
    /// (meter clock), or shortly after it.
    async fn lbt(&mut self, radio: &mut VirtualRadio, clock: &SimClock, window_end: Duration) -> Result<(), RadioError> {
        if !self.p.protocol.lbt {
            return Ok(());
        }
        let pr = self.p.protocol.clone();
        let mut timing = radio.timing();
        let mut busy_before = false;
        for i in 0..=pr.csma_backoffs_s.len() {
            let tps = if busy_before {
                let [lo, hi] = pr.csma_tps_ms;
                let steps = ((hi - lo) / 0.5).round() as u32;
                lo + 0.5 * f64::from(self.rng.random_range(0..=steps))
            } else {
                0.0
            };
            timing.carrier_sense = ms(pr.csma_listen_ms + tps);
            radio.set_timing(timing);
            if radio.is_channel_clear(pr.lbt_threshold_dbm).await? {
                return Ok(());
            }
            if i == 0 {
                self.counters.lbt_deferred += 1;
            }
            busy_before = true;
            radio.enter_sleep().await?;
            if let Some(&[lo, hi]) = pr.csma_backoffs_s.get(i) {
                let steps = ((hi - lo) / 0.1).round() as u32;
                let wait = lo + 0.1 * f64::from(self.rng.random_range(0..=steps));
                clock.sleep(secs(wait)).await;
            }
        }
        // Every check busy: guaranteed transmission (GM-GM-603).
        self.counters.lbt_forced += 1;
        let now = clock.now();
        let at = if now < window_end {
            now + secs(self.rng.random_range(0.0..(window_end - now).as_secs_f64()))
        } else {
            now + secs(pr.csma_late_delay_s)
        };
        clock.sleep_until(at).await;
        Ok(())
    }

    async fn periodic(
        &mut self,
        radio: &mut VirtualRadio,
        clock: &SimClock,
        k: u64,
        epoch: u16,
    ) -> Result<(), RadioError> {
        let before = Self::charge(radio);
        let signed = self.p.protocol.sign_periodic;
        let l = match self.p.protocol.profile {
            Profile::Dsmr6 => frames::on_air_len(frames::periodic_l(signed, self.p.protocol.security_mode)),
            Profile::Dsmr5 => frames::on_air_len(frames::DSMR5_PERIODIC_L),
        };
        let window_end = Self::slot_time(&self.p, self.phase, k) + secs(self.p.protocol.csma_window_s);
        self.lbt(radio, clock, window_end).await?;
        self.transmit(radio, Kind::Periodic, 0, l, epoch, k).await?;
        radio.enter_sleep().await?;
        self.counters.periodic_sent += 1;
        self.book(Activity::Periodic, Self::charge(radio) - before);
        if signed {
            self.book_compute(Activity::Periodic, self.p.crypto.ecdsa_sign_ms);
        }
        Ok(())
    }

    /// A billing push and its FAC window. Returns the push's start time if
    /// the gateway answered with a Channel Request.
    async fn billing(
        &mut self,
        radio: &mut VirtualRadio,
        clock: &SimClock,
        interval: u64,
        epoch: u16,
        midnight: bool,
        fac_credit: bool,
    ) -> Result<Option<Duration>, RadioError> {
        let before = Self::charge(radio);
        let signed = self.p.protocol.sign_billing;
        let kind = if midnight && self.p.protocol.daily_heartbeat {
            Kind::BillingMidnight
        } else {
            Kind::Billing
        };
        let l = match self.p.protocol.profile {
            Profile::Dsmr6 => frames::on_air_len(frames::billing_l(signed, self.p.protocol.security_mode)),
            // T1: 3-out-of-6 coded, Format A. The long message once a day.
            Profile::Dsmr5 if midnight => frames::t1_on_air_len(frames::DSMR5_HOURLY_LONG_L),
            Profile::Dsmr5 => frames::t1_on_air_len(frames::DSMR5_HOURLY_L),
        };
        let hour = self.phase + Duration::from_secs(interval * self.p.protocol.billing_interval_s);
        let window = match self.p.protocol.billing_spread_s {
            Some([_, hi]) => hi.max(self.p.protocol.csma_window_s),
            None => self.p.protocol.csma_window_s,
        };
        self.lbt(radio, clock, hour + secs(window)).await?;
        let start = clock.now();
        self.transmit(radio, kind, 0, l, epoch, interval).await?;
        let end = clock.now();
        self.counters.billing_sent += 1;
        if signed {
            self.book_compute(Activity::Billing, self.p.crypto.ecdsa_sign_ms);
        }
        if !fac_credit {
            radio.enter_sleep().await?;
            self.book(Activity::Billing, Self::charge(radio) - before);
            return Ok(None);
        }
        self.book(Activity::Billing, Self::charge(radio) - before);

        // The billing push announces limited access: listen for the gateway.
        let before = Self::charge(radio);
        self.counters.fac_windows += 1;
        let heard = self.listen(radio, clock, end).await?;
        self.book(Activity::FacWindow, Self::charge(radio) - before);

        match heard.map(|h| h.kind) {
            Some(Kind::GwNke) => {
                self.counters.heartbeats_heard += 1;
                let before = Self::charge(radio);
                let tail = [Turn {
                    meter_l: frames::METER_ACK_L,
                    gw_l: None,
                }];
                let next = start + secs(self.p.protocol.fac_tx_delay_s);
                let out = self.fac(radio, clock, next, &tail, Kind::FacAck, epoch).await?;
                self.counters.fac_transmissions += u64::from(out.tx);
                self.book(Activity::Heartbeat, Self::charge(radio) - before);
                Ok(None)
            }
            Some(Kind::GwChannelRequest) => {
                self.counters.channel_requests_heard += 1;
                Ok(Some(start))
            }
            _ => Ok(None),
        }
    }

    async fn handshake(
        &mut self,
        radio: &mut VirtualRadio,
        clock: &SimClock,
        first: Duration,
        new_epoch: u16,
        trigger: Trigger,
    ) -> Result<bool, RadioError> {
        let before = Self::charge(radio);
        let start = first.max(clock.now());
        let turns = frames::handshake();
        let out = self.fac(radio, clock, start, &turns, Kind::Handshake, new_epoch).await?;
        let sim = Self::charge(radio) - before;
        self.book(Activity::Handshake, sim);
        // Public-key work happens once the server's flight is in.
        let mut compute = 0.0;
        if out.turns_completed > frames::handshake_crypto_turn() {
            let c = &self.p.crypto;
            let active_ms = f64::from(c.handshake_verifies) * c.ecdsa_verify_ms
                + f64::from(c.handshake_signs) * c.ecdsa_sign_ms
                + f64::from(c.handshake_ecdh) * c.ecdh_ms;
            compute = self.book_compute(Activity::Handshake, active_ms);
        }
        self.counters.fac_transmissions += u64::from(out.tx);
        self.counters.handshakes.push(HandshakeRecord {
            trigger,
            ok: out.ok,
            start_s: start.as_secs_f64(),
            duration_s: (clock.now() - start).as_secs_f64(),
            meter_tx: out.tx,
            gw_frames_received: out.rx,
            charge_mah: sim + compute,
        });
        Ok(out.ok)
    }

    /// A frequent access cycle: transmit every tTxD, listen tRO after each
    /// frame, advance through `turns` on every expected reply. Once the
    /// current turn expects no reply, the FAC runs out its tail.
    async fn fac(
        &mut self,
        radio: &mut VirtualRadio,
        clock: &SimClock,
        first: Duration,
        turns: &[Turn],
        kind: Kind,
        epoch: u16,
    ) -> Result<FacOutcome, RadioError> {
        let pr = self.p.protocol.clone();
        let mut out = FacOutcome {
            ok: false,
            turns_completed: 0,
            tx: 0,
            rx: 0,
        };
        let mut at = first;
        let mut silent = 0u32;
        loop {
            clock.sleep_until(at).await;
            let idx = out.turns_completed.min(turns.len() - 1);
            let turn = turns[idx];
            let start = clock.now();
            self.transmit(radio, kind, idx, frames::on_air_len(turn.meter_l), epoch, 0).await?;
            let end = clock.now();
            out.tx += 1;
            let heard = self.listen(radio, clock, end).await?;
            let replied = turn.gw_l.is_some()
                && matches!(heard, Some(h) if h.kind == Kind::GwHandshake && usize::from(h.turn) == idx);
            if replied {
                out.turns_completed += 1;
                out.rx += 1;
                silent = 0;
            } else {
                silent += 1;
            }
            if turn.gw_l.is_none() && silent >= pr.fac_tail_transmissions {
                out.ok = true;
                return Ok(out);
            }
            if turn.gw_l.is_some() && silent >= pr.fac_abort_after_silent {
                radio.enter_sleep().await?;
                return Ok(out);
            }
            at = start + secs(pr.fac_tx_delay_s);
        }
    }

    /// Listen for the gateway's reply to a frame that ended at `tx_end`
    /// (MCU clock): retune to O2M, sleep, open the receiver so it listens
    /// from tRO − guard, give up if no sync word by tRO + guard, then go
    /// back to sleep on M2O. A frame for another meter that starts in the
    /// window still holds the receiver until it ends, and counts as no reply.
    async fn listen(
        &mut self,
        radio: &mut VirtualRadio,
        clock: &SimClock,
        tx_end: Duration,
    ) -> Result<Option<Header>, RadioError> {
        let pr = &self.p.protocol;
        let guard = ms(pr.rx_guard_ms);
        let timeout = 2 * guard + radio::acquisition(&self.reply.phy) + ms(pr.rx_extra_listen_ms);
        let r = &self.p.radio;
        // A 1 s tRO (C-mode) is worth sleeping through; a 2–3 ms one (T2) is
        // not: the radio stays in standby and only needs to turn on RX.
        let short = pr.response_delay_ms < 20.0;
        let wake_lead = if short {
            ms(r.idle_to_rx_ms)
        } else {
            ms(r.sleep_to_idle_ms + r.idle_to_rx_ms)
        };
        let open = (tx_end + ms(pr.response_delay_ms)).saturating_sub(guard + wake_lead);

        radio.set_phy(self.reply.phy.clone()).expect("valid reply PHY");
        radio.set_frequency(self.reply.frequency_hz).await?;
        if !short {
            radio.enter_sleep().await?;
        }
        clock.sleep_until(open).await;
        let mut buf = [0u8; 300];
        let heard = match radio.receive_timeout(&mut buf, timeout).await {
            Ok((len, _)) => frames::decode(&buf[..len]).filter(|h| h.meter == self.id),
            Err(RadioError::RxTimeout | RadioError::CrcError) => None,
            Err(e) => return Err(e),
        };
        radio.enter_sleep().await?;
        radio.set_phy(self.m2o.clone()).expect("valid M2O PHY");
        radio.set_frequency(M2O_HZ).await?;
        Ok(heard)
    }
}
