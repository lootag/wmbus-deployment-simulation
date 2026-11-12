//! The gateway (GW), mains powered, with one half-duplex radio: it listens
//! on M2O and switches to O2M to reply, deaf in the meantime.
//!
//! It answers only its own meters; frames from other meters still occupy
//! its receiver. It counts a push as delivered only if it can decrypt it:
//! the push's session must be the one the gateway holds for that meter.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use radiofreccia_core::{Channel, Clock, RadioError, RxRadio, ScheduledTxRadio};
use radiofreccia_sim::{PhyConfig, VirtualRadio};

use crate::frames::{self, Header, Kind};
use crate::params::Params;
use crate::radio::{self, ReplyLink, M2O_HZ};

/// What happened to one meter, as the gateway saw it.
#[derive(Debug, Clone, Default)]
pub struct MeterLog {
    /// Periodic slots delivered (decrypted).
    pub periodic: HashSet<u32>,
    /// Billing intervals delivered.
    pub billing: HashSet<u32>,
    /// Pushes received under a session the gateway does not hold.
    pub undecryptable: u64,
    /// When the gateway completed a handshake with it, true time.
    pub sessions_established: Vec<Duration>,
    pub heartbeats_sent: u64,
    pub channel_requests_sent: u64,
}

/// One gateway's own counters.
#[derive(Debug, Clone, Default)]
pub struct GwLog {
    pub replies_sent: u64,
    /// Replies that could not go out at tRO (radio busy or too late).
    pub replies_late: u64,
    /// Replies dropped to stay within the duty cycle.
    pub replies_duty_limited: u64,
    /// ClientHellos ignored because enough handshakes were running.
    pub handshakes_refused: u64,
    /// Highest airtime over any rolling hour, s.
    pub max_hour_airtime_s: f64,
}

#[derive(Debug, Default)]
pub struct World {
    pub meters: BTreeMap<u64, MeterLog>,
    pub gateways: BTreeMap<u64, GwLog>,
}

pub type Shared = Arc<Mutex<World>>;

/// How one gateway is set up.
#[derive(Debug, Clone)]
pub struct GatewaySetup {
    pub id: u64,
    pub meters: Vec<u64>,
    /// Power outage: from, until (true time). Sessions are lost.
    pub outage: Option<(Duration, Duration)>,
}

struct Reply {
    at_ticks: u64,
    frame: Vec<u8>,
}

pub struct Gateway {
    setup: GatewaySetup,
    mine: HashSet<u64>,
    m2o: PhyConfig,
    reply_link: ReplyLink,
    /// DTLS sessions live in RAM and die with the power; a mode 9 key
    /// does not.
    restart_loses_sessions: bool,
    delay_ticks: u64,
    heartbeat: bool,
    max_handshakes: usize,
    duty_budget: Duration,
    world: Shared,
    /// Session held per meter.
    sessions: HashMap<u64, Option<u16>>,
    /// Handshakes in progress: meter, last frame seen.
    active: HashMap<u64, Duration>,
    /// Meters to send a Channel Request to.
    pending: HashSet<u64>,
    queue: BinaryHeap<Reverse<(u64, u64)>>,
    replies: HashMap<u64, Reply>,
    next_reply_id: u64,
    airtime: VecDeque<(Duration, Duration)>,
}

/// The radio must stop listening this long before a reply goes on air:
/// retune, switch PHY, ramp up.
const SWITCH_LEAD: Duration = Duration::from_millis(2);
/// A handshake with no frame for this long is considered abandoned.
const HANDSHAKE_IDLE: Duration = Duration::from_secs(30);

impl Gateway {
    pub fn new(setup: GatewaySetup, p: &Params, world: Shared) -> Self {
        let sessions = setup.meters.iter().map(|&m| (m, Some(0))).collect();
        world.lock().unwrap().gateways.insert(setup.id, GwLog::default());
        Self {
            mine: setup.meters.iter().copied().collect(),
            setup,
            m2o: radio::m2o(p.radio.gw_sensitivity_dbm),
            reply_link: ReplyLink::for_params(p),
            restart_loses_sessions: p.protocol.security_mode == 13,
            delay_ticks: (p.protocol.response_delay_ms * 1000.0).round() as u64,
            heartbeat: p.protocol.daily_heartbeat,
            max_handshakes: p.gateway.max_concurrent_handshakes,
            duty_budget: Duration::from_secs_f64(3600.0 * p.gateway.duty_cycle),
            world,
            sessions,
            active: HashMap::new(),
            pending: HashSet::new(),
            queue: BinaryHeap::new(),
            replies: HashMap::new(),
            next_reply_id: 0,
            airtime: VecDeque::new(),
        }
    }

    pub async fn run(&mut self, radio: &mut VirtualRadio) -> Result<(), RadioError> {
        let clock = radio.clock();
        let mut outage = self.setup.outage;
        let mut buf = [0u8; 300];
        loop {
            let now = clock.now();
            if let Some((from, until)) = outage {
                if now >= from {
                    radio.enter_sleep().await?;
                    clock.sleep_until(until).await;
                    self.restart();
                    outage = None;
                    continue;
                }
            }
            let next_reply = self.queue.peek().map(|Reverse((at, _))| Duration::from_micros(*at));
            let reply_deadline = next_reply.map(|at| at.saturating_sub(SWITCH_LEAD));
            if let Some(d) = reply_deadline {
                if now >= d {
                    self.send_next(radio).await?;
                    continue;
                }
            }
            let deadline = match (reply_deadline, outage) {
                (Some(d), Some((from, _))) => Some(d.min(from)),
                (Some(d), None) => Some(d),
                (None, Some((from, _))) => Some(from),
                (None, None) => None,
            };
            let received = match deadline {
                Some(d) => match clock.timeout(d.saturating_sub(now), radio.receive(&mut buf)).await {
                    Ok(r) => r,
                    Err(_) => continue,
                },
                None => radio.receive(&mut buf).await,
            };
            match received {
                Ok((len, meta)) => {
                    if let Some(h) = frames::decode(&buf[..len]) {
                        self.handle(h, meta.timestamp_ticks, clock.now());
                    }
                }
                Err(RadioError::CrcError | RadioError::RxTimeout) => {}
                Err(e) => return Err(e),
            }
        }
    }

    fn restart(&mut self) {
        self.queue.clear();
        self.replies.clear();
        self.active.clear();
        if self.restart_loses_sessions {
            for s in self.sessions.values_mut() {
                *s = None;
            }
            self.pending = self.mine.clone();
        }
    }

    fn reply(&mut self, at_ticks: u64, header: Header, l: usize) {
        let id = self.next_reply_id;
        self.next_reply_id += 1;
        self.queue.push(Reverse((at_ticks, id)));
        self.replies.insert(
            id,
            Reply {
                at_ticks,
                frame: frames::encode(header, l),
            },
        );
    }

    fn handle(&mut self, h: Header, end_ticks: u64, now: Duration) {
        if !self.mine.contains(&h.meter) {
            return;
        }
        let m = h.meter;
        // +1 µs: timestamps are rounded down to the microsecond.
        let at = end_ticks + self.delay_ticks + 1;
        let to = |kind, turn| Header {
            kind,
            meter: m,
            turn,
            epoch: 0,
            slot: 0,
        };
        self.active.retain(|_, last| now.saturating_sub(*last) < HANDSHAKE_IDLE);
        let held = self.sessions.get(&m).copied().flatten();
        let mut world = self.world.lock().unwrap();
        let log = world.meters.entry(m).or_default();
        match h.kind {
            Kind::Periodic | Kind::Billing | Kind::BillingMidnight => {
                if held == Some(h.epoch) {
                    if h.kind == Kind::Periodic {
                        log.periodic.insert(h.slot);
                    } else {
                        log.billing.insert(h.slot);
                    }
                } else {
                    log.undecryptable += 1;
                }
                if h.kind == Kind::Periodic {
                    return;
                }
                if self.pending.contains(&m) && self.active.len() < self.max_handshakes {
                    log.channel_requests_sent += 1;
                    drop(world);
                    self.active.insert(m, now);
                    self.reply(at, to(Kind::GwChannelRequest, 0), frames::GW_CHANNEL_REQUEST_L);
                } else if h.kind == Kind::BillingMidnight && self.heartbeat && held.is_some() {
                    log.heartbeats_sent += 1;
                    drop(world);
                    self.reply(at, to(Kind::GwNke, 0), frames::GW_SND_NKE_L);
                }
            }
            Kind::Handshake => {
                let turn = usize::from(h.turn);
                let Some(gw_l) = frames::handshake().get(turn).and_then(|t| t.gw_l) else {
                    return;
                };
                if !self.active.contains_key(&m) && self.active.len() >= self.max_handshakes {
                    world.gateways.get_mut(&self.setup.id).unwrap().handshakes_refused += 1;
                    return;
                }
                if frames::is_final_gw_turn(turn) {
                    log.sessions_established.push(now);
                    drop(world);
                    self.sessions.insert(m, Some(h.epoch));
                    self.pending.remove(&m);
                    self.active.remove(&m);
                } else {
                    drop(world);
                    self.active.insert(m, now);
                }
                self.reply(at, to(Kind::GwHandshake, h.turn), gw_l);
            }
            _ => {}
        }
    }

    async fn send_next(&mut self, radio: &mut VirtualRadio) -> Result<(), RadioError> {
        let Some(Reverse((_, id))) = self.queue.pop() else {
            return Ok(());
        };
        let reply = self.replies.remove(&id).expect("queued reply");
        let now = radio.clock().now();
        while self.airtime.front().is_some_and(|(t, _)| now.saturating_sub(*t) > Duration::from_secs(3600)) {
            self.airtime.pop_front();
        }
        let used: Duration = self.airtime.iter().map(|(_, a)| *a).sum();
        let air = self.reply_link.phy.air_time(reply.frame.len());
        if used + air > self.duty_budget {
            self.world.lock().unwrap().gateways.get_mut(&self.setup.id).unwrap().replies_duty_limited += 1;
            return Ok(());
        }
        radio.set_phy(self.reply_link.phy.clone()).expect("valid reply PHY");
        radio.set_frequency(self.reply_link.frequency_hz).await?;
        let sent = radio.transmit_at(&reply.frame, reply.at_ticks).await;
        radio.set_phy(self.m2o.clone()).expect("valid M2O PHY");
        radio.set_frequency(M2O_HZ).await?;
        let mut world = self.world.lock().unwrap();
        let gw = world.gateways.get_mut(&self.setup.id).unwrap();
        match sent {
            Ok(()) => {
                gw.replies_sent += 1;
                self.airtime.push_back((now, air));
                let hour: Duration = self.airtime.iter().map(|(_, a)| *a).sum();
                gw.max_hour_airtime_s = gw.max_hour_airtime_s.max(hour.as_secs_f64());
            }
            Err(RadioError::InfeasibleSchedule) => gw.replies_late += 1,
            Err(e) => return Err(e),
        }
        Ok(())
    }
}
