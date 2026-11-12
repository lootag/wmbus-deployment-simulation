//! Reliability in a dense Dutch district: DSMR5 against DSMR6 gas meters,
//! on dedicated gateways shared by the meters around them.
//!
//! Both profiles run on the same layout (same seed): the same meters, at
//! the same places, on the same gateways. Each gets a steady-state run,
//! which the dashboard shows, and a run with an area power cut.
//!
//! The target, from the DSMR5 P2 companion standard: at least 99.5 % of
//! readings (5-minute and hourly) available for each individual meter, on
//! first transmission.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use radiofreccia_sim::dashboard::{default_battery, write_dashboard, DashboardInfo, LinkResult, MessageCounts, NodeResult, Power};
use radiofreccia_sim::{ClockModel, Environment, Fallback, LogDistance, NodeClocks, Position, Variability};
use serde::{Deserialize, Serialize};

use crate::gateway::GatewaySetup;
use crate::meter::{Meter, MeterSetup};
use crate::params::{Params, Profile};
use crate::sim::{self, MeterNode, MeterResult, Run};

/// Availability target per meter (DSMR5 P2 companion standard).
pub const TARGET: f64 = 0.995;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct District {
    /// Side of the square district, m.
    pub side_m: f64,
    /// Households (one gas meter each) per km². A dense Dutch city district
    /// has about 6,000 people per km², so about 3,000 households at a little
    /// over 2 people each; the densest neighbourhoods reach 6,000–7,000
    /// addresses per km² (`configs/worst-case-6000.json`). Homes without gas
    /// make the real meter count somewhat lower `[EST]`.
    pub households_per_km2: f64,
    /// Meters closer than this to the edge make traffic but are not scored:
    /// they have fewer neighbours than a meter in an endless city.
    pub margin_m: f64,
    /// Gateways on a square grid with this spacing, m; each serves the
    /// meters in its cell `[EST]`.
    pub gateway_spacing_m: f64,
    /// Log-distance path loss exponent (indoor, through walls and floors).
    pub exponent: f64,
    /// Per-frame shadowing, dB.
    pub shadowing_db: f64,
    /// Pushes on the clock-aligned grid both companion standards specify;
    /// false gives each meter a random phase instead.
    pub aligned: bool,
    /// Meter clocks disagree with true time by a random offset up to this
    /// much (what is left after the gateway's last time sync), s, and drift
    /// randomly within ±`mcu.drift_ppm` `[EST]`. Without transmit
    /// randomisation this is the only thing spreading meters apart.
    pub clock_spread_s: f64,
    /// Steady-state run, days.
    pub days: f64,
    /// Not scored at the start of each run, h.
    pub warmup_h: f64,
    /// Power cut: every gateway loses power at this hour, for this long.
    pub outage_at_h: f64,
    pub outage_minutes: f64,
    /// Power-cut run, h.
    pub outage_run_h: f64,
}

impl Default for District {
    fn default() -> Self {
        Self {
            side_m: 500.0,
            households_per_km2: 3000.0,
            margin_m: 100.0,
            gateway_spacing_m: 80.0,
            exponent: 3.5,
            shadowing_db: 6.0,
            aligned: true,
            clock_spread_s: 2.0,
            days: 2.0,
            warmup_h: 1.0,
            outage_at_h: 2.33,
            outage_minutes: 10.0,
            outage_run_h: 26.0,
        }
    }
}

/// What one run measured over its scored meters.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub meters_total: usize,
    pub meters_scored: usize,
    pub gateways: usize,
    /// Per scored meter: share of 5-minute and hourly readings delivered.
    pub periodic: Vec<f64>,
    pub billing: Vec<f64>,
    /// Per scored meter: expected and delivered readings (both kinds).
    pub expected: Vec<u64>,
    pub delivered: Vec<u64>,
    /// 5-minute readings never sent for lack of a session, and sent but not
    /// delivered, as shares of those expected.
    pub lost_no_session: f64,
    pub lost_on_air: f64,
    pub lbt_deferred: f64,
    pub lbt_forced: f64,
    pub handshakes: usize,
    pub handshakes_ok: usize,
    /// Power cut: per meter, hours from power-back until its gateway held a
    /// session again (`Some(0.0)` when no session was lost).
    pub recovery_h: Vec<Option<f64>>,
    /// Power cut: per meter, 5-minute readings lost from the cut on.
    pub outage_lost: Vec<u64>,
    pub replies_late: u64,
    pub handshakes_refused: u64,
    pub max_gw_duty: f64,
    pub meter_ua: Vec<f64>,
}

struct Layout {
    meters: Vec<MeterNode>,
    gateways: Vec<(GatewaySetup, Option<Position>)>,
    /// Meter → its gateway and the distance to it.
    served_by: HashMap<u64, (u64, f64)>,
}

fn layout(p: &Params, d: &District, outage: Option<(Duration, Duration)>) -> Layout {
    let mut rng = StdRng::seed_from_u64(p.seed ^ 0xD15_7121C7);
    let n = (d.households_per_km2 * d.side_m * d.side_m / 1e6).round() as usize;
    let lifetime = p.protocol.handshake_interval_days * 86_400.0;
    let period = p.protocol.periodic_interval_s as f64;
    let cells = (d.side_m / d.gateway_spacing_m).ceil().max(1.0) as usize;
    let mut gateways: Vec<(GatewaySetup, Option<Position>)> = Vec::new();
    for gy in 0..cells {
        for gx in 0..cells {
            let pos = Position::new(
                d.gateway_spacing_m * (gx as f64 + 0.5),
                d.gateway_spacing_m * (gy as f64 + 0.5),
            );
            let setup = GatewaySetup {
                id: 1_000_000 + (gy * cells + gx) as u64,
                meters: Vec::new(),
                outage,
            };
            gateways.push((setup, Some(pos)));
        }
    }
    let mut meters = Vec::with_capacity(n);
    let mut served_by = HashMap::new();
    for i in 0..n {
        let id = i as u64 + 1;
        // Draw everything for every meter, whatever the profile, so both
        // profiles get the same layout.
        let (x, y) = (rng.random_range(0.0..d.side_m), rng.random_range(0.0..d.side_m));
        let phase = rng.random_range(0.0..period);
        let rekey = rng.random_range(0.0..lifetime);
        let offset = Duration::from_secs_f64(rng.random_range(0.0..d.clock_spread_s.max(1e-9)));
        let drift = rng.random_range(-p.mcu.drift_ppm..=p.mcu.drift_ppm);
        let carrier = rng.random_range(-10.0..=10.0);
        let clocks = NodeClocks {
            mcu: ClockModel {
                offset,
                drift_ppm: drift,
            },
            radio: ClockModel {
                offset,
                drift_ppm: drift,
            },
            carrier_ppm: carrier,
        };
        let phase = if d.aligned { Duration::ZERO } else { Duration::from_secs_f64(phase) };
        let first_rekey = if p.protocol.security_mode == 9 {
            Duration::MAX
        } else {
            Duration::from_secs_f64(rekey)
        };
        let gx = ((x / d.gateway_spacing_m) as usize).min(cells - 1);
        let gy = ((y / d.gateway_spacing_m) as usize).min(cells - 1);
        let (gw, gw_pos) = &mut gateways[gy * cells + gx];
        gw.meters.push(id);
        let pos = Position::new(x, y);
        served_by.insert(id, (gw.id, pos.distance_to(&gw_pos.expect("gateway position"))));
        meters.push(MeterNode {
            setup: MeterSetup { id, phase, first_rekey },
            label: format!("meter {id}"),
            position: Some(pos),
            clocks: Some(clocks),
        });
    }
    gateways.retain(|(g, _)| !g.meters.is_empty());
    Layout {
        meters,
        gateways,
        served_by,
    }
}

fn environment(d: &District) -> Environment {
    Environment::from_links([])
        .with_fallback(Fallback::LogDistance(LogDistance {
            exponent: d.exponent,
            ..LogDistance::default()
        }))
        .with_variability(Variability::Normal {
            sigma_db: d.shadowing_db,
        })
}

fn scored(d: &District, r: &MeterResult) -> bool {
    let pos = r.node.position.expect("district meters have positions");
    let (lo, hi) = (d.margin_m, d.side_m - d.margin_m);
    (lo..=hi).contains(&pos.x) && (lo..=hi).contains(&pos.y)
}

/// A companion standard, configured one way.
#[derive(Debug, Clone)]
pub struct Variant {
    /// Column header and dashboard title.
    pub name: &'static str,
    /// Dashboard file name, without extension.
    pub file: &'static str,
    pub profile: Profile,
    pub sign_periodic: bool,
    pub sign_billing: bool,
    /// Hourly push spread, s after the hour; `None` keeps the profile's.
    pub hourly_spread_s: Option<[f64; 2]>,
    /// One line for the dashboard.
    pub what: &'static str,
}

/// The variants the simulation compares: both standards as documented, with
/// DSMR6 signing only its hourly values, and DSMR6 with its hourly pushes
/// spread over 10 minutes.
pub fn variants() -> Vec<Variant> {
    vec![
        Variant {
            name: "DSMR5",
            file: "dsmr5",
            profile: Profile::Dsmr5,
            sign_periodic: false,
            sign_billing: false,
            hourly_spread_s: None,
            what: "5-minute C1 messages and hourly T1 messages spread over 1–10 min, mode 9 (AES-GCM, \
                   pre-installed key), unsigned, CSMA",
        },
        Variant {
            name: "DSMR6 by the book",
            file: "dsmr6-by-the-book",
            profile: Profile::Dsmr6,
            sign_periodic: false,
            sign_billing: true,
            // Its own random moment in the 10 s after the hour [CS 4.9].
            hourly_spread_s: Some([0.0, 10.0]),
            what: "5-minute and hourly C1 pushes each at a random moment in the 10 s after their boundary [CS 4.9], \
                   only the hourly pushes signed, CSMA as in the G-meter CSMA requirements, DTLS (mode 13) \
                   with monthly rekey",
        },
        Variant {
            name: "DSMR6 hourly 10 min",
            file: "dsmr6-hourly-10min",
            profile: Profile::Dsmr6,
            sign_periodic: false,
            sign_billing: true,
            // A random moment in the 10 minutes after the hour.
            hourly_spread_s: Some([0.0, 600.0]),
            what: "5-minute C1 pushes unsigned at a random moment in the 10 s after the boundary [CS 4.9], hourly C1 \
                   pushes signed at a random moment in the 10 minutes after the hour, CSMA as in the G-meter CSMA \
                   requirements, DTLS (mode 13) with monthly rekey",
        },
    ]
}

/// The variant's parameters for a district run.
pub fn params(base: &Params, v: &Variant) -> Params {
    let mut p = base.clone();
    p.apply_profile(v.profile);
    p.protocol.sign_periodic = v.sign_periodic;
    p.protocol.sign_billing = v.sign_billing;
    if let Some(spread) = v.hourly_spread_s {
        p.protocol.billing_spread_s = Some(spread);
    }
    p
}

/// Run one variant: steady state (written as a dashboard to `out`) and a
/// power cut.
pub fn run_variant(base: &Params, d: &District, v: &Variant, out: &Path) -> std::io::Result<(Outcome, Outcome)> {
    // Steady state.
    let mut p = params(base, v);
    p.days = d.days;
    let l = layout(&p, d, None);
    let served_by = l.served_by.clone();
    let gateways = l.gateways.len();
    let run = sim::run(&p, environment(d), l.meters, l.gateways);
    report_errors(v, &run);
    let steady = score(&p, d, None, &run, gateways);
    write(&p, d, v, &run, &served_by, &steady, out)?;

    // Power cut.
    let mut p = params(base, v);
    p.days = d.outage_run_h / 24.0;
    let from = Duration::from_secs_f64(d.outage_at_h * 3600.0);
    let until = from + Duration::from_secs_f64(d.outage_minutes * 60.0);
    let l = layout(&p, d, Some((from, until)));
    let gateways = l.gateways.len();
    let run = sim::run(&p, environment(d), l.meters, l.gateways);
    report_errors(v, &run);
    let cut = score(&p, d, Some((from, until)), &run, gateways);
    Ok((steady, cut))
}

/// A meter whose firmware stopped with an error would quietly lose
/// readings: say so.
fn report_errors(v: &Variant, run: &Run) {
    for r in run.meters.iter().filter(|r| r.error.is_some()) {
        eprintln!("  !! {}: meter {} stopped with {}", v.name, r.id(), r.error.as_deref().unwrap_or("?"));
    }
}

/// Readings a meter was due to deliver in the scored window, and delivered.
struct Tally {
    exp_p: u64,
    got_p: u64,
    exp_b: u64,
    got_b: u64,
    no_session: u64,
    on_air: u64,
    lost_after: u64,
}

fn tally(p: &Params, d: &District, r: &MeterResult, outage_from: Option<Duration>) -> Tally {
    let end = Duration::from_secs_f64(p.days * 86_400.0);
    // Leave the last slot out: its push may still be in flight. With the
    // hourly push spread over 10 minutes, leave its last interval out too.
    let to = end.saturating_sub(Duration::from_secs(p.protocol.billing_interval_s));
    let from = Duration::from_secs_f64(d.warmup_h * 3600.0);
    let per_billing = Meter::slots_per_billing(p);
    let mut t = Tally {
        exp_p: 0,
        got_p: 0,
        exp_b: 0,
        got_b: 0,
        no_session: 0,
        on_air: 0,
        lost_after: 0,
    };
    for k in 0.. {
        let at = Meter::slot_time(p, r.node.setup.phase, k);
        if at >= to {
            break;
        }
        let delivered = r.gw.periodic.contains(&(k as u32));
        if outage_from.is_some_and(|o| at >= o) && !delivered {
            t.lost_after += 1;
        }
        if at < from {
            continue;
        }
        t.exp_p += 1;
        if delivered {
            t.got_p += 1;
        } else if r.counters.periodic_no_session.contains(&k) {
            t.no_session += 1;
        } else {
            t.on_air += 1;
        }
        if k.is_multiple_of(per_billing) {
            t.exp_b += 1;
            if r.gw.billing.contains(&((k / per_billing) as u32)) {
                t.got_b += 1;
            }
        }
    }
    t
}

fn score(p: &Params, d: &District, outage: Option<(Duration, Duration)>, run: &Run, gateways: usize) -> Outcome {
    let scored: Vec<&MeterResult> = run.meters.iter().filter(|r| scored(d, r)).collect();
    let mut out = Outcome {
        meters_total: run.meters.len(),
        meters_scored: scored.len(),
        gateways,
        periodic: Vec::new(),
        billing: Vec::new(),
        expected: Vec::new(),
        delivered: Vec::new(),
        lost_no_session: 0.0,
        lost_on_air: 0.0,
        lbt_deferred: 0.0,
        lbt_forced: 0.0,
        handshakes: 0,
        handshakes_ok: 0,
        recovery_h: Vec::new(),
        outage_lost: Vec::new(),
        replies_late: 0,
        handshakes_refused: 0,
        max_gw_duty: 0.0,
        meter_ua: Vec::new(),
    };
    let (mut exp_all, mut no_session, mut on_air) = (0u64, 0u64, 0u64);
    let (mut pushes, mut deferred, mut forced) = (0u64, 0u64, 0u64);
    for r in &scored {
        let t = tally(p, d, r, outage.map(|o| o.0));
        exp_all += t.exp_p;
        no_session += t.no_session;
        on_air += t.on_air;
        out.periodic.push(t.got_p as f64 / t.exp_p.max(1) as f64);
        out.billing.push(t.got_b as f64 / t.exp_b.max(1) as f64);
        out.expected.push(t.exp_p + t.exp_b);
        out.delivered.push(t.got_p + t.got_b);
        out.handshakes += r.counters.handshakes.len();
        out.handshakes_ok += r.counters.handshakes.iter().filter(|h| h.ok).count();
        out.meter_ua.push(r.average_ua());
        pushes += r.counters.periodic_sent + r.counters.billing_sent;
        deferred += r.counters.lbt_deferred;
        forced += r.counters.lbt_forced;
        if let Some((_, back)) = outage {
            out.outage_lost.push(t.lost_after);
            out.recovery_h.push(if p.protocol.security_mode == 9 {
                // The key survives the power cut: nothing to recover.
                Some(0.0)
            } else {
                r.gw.sessions_established
                    .iter()
                    .find(|&&s| s >= back)
                    .map(|s| (*s - back).as_secs_f64() / 3600.0)
            });
        }
    }
    out.lost_no_session = no_session as f64 / exp_all.max(1) as f64;
    out.lost_on_air = on_air as f64 / exp_all.max(1) as f64;
    out.lbt_deferred = deferred as f64 / pushes.max(1) as f64;
    out.lbt_forced = forced as f64 / pushes.max(1) as f64;
    for g in run.world.gateways.values() {
        out.max_gw_duty = out.max_gw_duty.max(g.max_hour_airtime_s / 3600.0);
        out.replies_late += g.replies_late;
        out.handshakes_refused += g.handshakes_refused;
    }
    out
}

fn write(
    p: &Params,
    d: &District,
    v: &Variant,
    run: &Run,
    served_by: &HashMap<u64, (u64, f64)>,
    o: &Outcome,
    out: &Path,
) -> std::io::Result<()> {
    let mut nodes = Vec::new();
    for r in run.meters.iter().filter(|r| scored(d, r)) {
        let t = tally(p, d, r, None);
        let (gw, distance) = served_by[&r.id()];
        let mut energy = r.energy;
        energy.mcu_active.charge_mah += r.ledger.compute_total();
        let readings = t.exp_p + t.exp_b;
        nodes.push(
            NodeResult::from_reports(r.id(), r.node.label.clone(), Power::Battery, energy, r.stats).with_link(
                LinkResult::to(gw).with_distance(distance).with_messages(MessageCounts {
                    // Readings due in the scored window, and delivered.
                    // Pushes are unacknowledged: one frame per reading.
                    sent: readings,
                    delivered: t.got_p + t.got_b,
                    transmissions: readings,
                }),
            ),
        );
    }
    for g in &run.gateways {
        nodes.push(NodeResult::from_reports(
            g.id,
            format!("gateway {} ({} meters)", g.id - 1_000_000, g.meters.len()),
            Power::Mains,
            g.energy,
            g.stats,
        ));
    }
    let (name, what) = (v.name, v.what);
    let pass = o
        .periodic
        .iter()
        .zip(&o.billing)
        .filter(|(a, b)| **a >= TARGET && **b >= TARGET)
        .count();
    let info = DashboardInfo {
        title: format!("{name} gas meters, shared gateways"),
        description: format!(
            "{} gas meters at {:.0} households/km² on a {:.0} × {:.0} m district, {} gateways {:.0} m apart, {:.0} days. \
             {what}; listen before talk. Shown: the {} meters at least {:.0} m from the edge, and the gateways. \
             Delivery is readings delivered of readings due; {} of {} meters ({:.0} %) reach the 99.5 % target.",
            o.meters_total,
            d.households_per_km2,
            d.side_m,
            d.side_m,
            o.gateways,
            d.gateway_spacing_m,
            p.days,
            o.meters_scored,
            d.margin_m,
            pass,
            o.meters_scored,
            100.0 * pass as f64 / o.meters_scored.max(1) as f64,
        ),
        battery: default_battery(),
    };
    std::fs::create_dir_all(out)?;
    let path = out.join(format!("{}.html", v.file));
    write_dashboard(&path, &nodes, &info)?;
    println!("dashboard written to {}", path.display());
    Ok(())
}

fn percentile(v: &[f64], q: f64) -> f64 {
    let mut v = v.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    if v.is_empty() {
        return f64::NAN;
    }
    v[((v.len() - 1) as f64 * q).round() as usize]
}

fn mean(v: &[f64]) -> f64 {
    v.iter().sum::<f64>() / v.len().max(1) as f64
}

fn hours(h: f64) -> String {
    if h.is_finite() {
        format!("{h:.2} h")
    } else {
        "never".into()
    }
}

/// Side-by-side comparison.
pub fn print(d: &District, rows: &[(Variant, Outcome, Outcome)]) {
    println!(
        "\nDistrict: {:.0} × {:.0} m, {:.0} households/km², gateways every {:.0} m; meters at least {:.0} m from the edge scored.",
        d.side_m, d.side_m, d.households_per_km2, d.gateway_spacing_m, d.margin_m
    );
    println!("Target: at least {:.1} % of readings for every meter.\n", 100.0 * TARGET);
    let col = |f: &dyn Fn(&Outcome, &Outcome) -> String| -> Vec<String> { rows.iter().map(|(_, s, c)| f(s, c)).collect() };
    let line = |label: &str, cells: Vec<String>| {
        print!("  {label:<46}");
        for c in cells {
            print!(" {c:>27}");
        }
        println!();
    };
    line("", rows.iter().map(|(v, _, _)| v.name.to_string()).collect());
    println!("  Steady state, {:.0} days", d.days);
    line("    meters scored / all, gateways", col(&|s, _| format!("{}/{}, {}", s.meters_scored, s.meters_total, s.gateways)));
    line("    5-min readings delivered, mean", col(&|s, _| format!("{:.2} %", 100.0 * mean(&s.periodic))));
    line("    5-min readings delivered, worst 1 %", col(&|s, _| format!("{:.2} %", 100.0 * percentile(&s.periodic, 0.01))));
    line("    hourly readings delivered, mean", col(&|s, _| format!("{:.2} %", 100.0 * mean(&s.billing))));
    line("    hourly readings delivered, worst 1 %", col(&|s, _| format!("{:.2} %", 100.0 * percentile(&s.billing, 0.01))));
    line(
        "    meters reaching 99.5 % on both",
        col(&|s, _| {
            let pass = s.periodic.iter().zip(&s.billing).filter(|(a, b)| **a >= TARGET && **b >= TARGET).count();
            format!("{:.1} %", 100.0 * pass as f64 / s.meters_scored.max(1) as f64)
        }),
    );
    line("    5-min lost: no session / on air", col(&|s, _| format!("{:.2} / {:.2} %", 100.0 * s.lost_no_session, 100.0 * s.lost_on_air)));
    line("    pushes deferred by LBT / sent anyway", col(&|s, _| format!("{:.1} / {:.2} %", 100.0 * s.lbt_deferred, 100.0 * s.lbt_forced)));
    line("    handshakes ok", col(&|s, _| format!("{}/{}", s.handshakes_ok, s.handshakes)));
    line("    meter average current", col(&|s, _| format!("{:.2}–{:.2} µA", percentile(&s.meter_ua, 0.0), percentile(&s.meter_ua, 1.0))));
    println!(
        "  Power cut: gateways off at {:.2} h for {:.0} min, run {:.0} h",
        d.outage_at_h, d.outage_minutes, d.outage_run_h
    );
    let rec = |c: &Outcome, q: f64| {
        let v: Vec<f64> = c.recovery_h.iter().map(|r| r.unwrap_or(f64::INFINITY)).collect();
        hours(percentile(&v, q))
    };
    line("    session back: median", col(&|_, c| rec(c, 0.5)));
    line("    session back: 99 %", col(&|_, c| rec(c, 0.99)));
    line("    session back: last", col(&|_, c| rec(c, 1.0)));
    line(
        "    5-min lost from the cut on: median / worst",
        col(&|_, c| {
            let v: Vec<f64> = c.outage_lost.iter().map(|&x| x as f64).collect();
            format!("{:.0} / {:.0}", percentile(&v, 0.5), percentile(&v, 1.0))
        }),
    );
    line("    handshakes ok", col(&|_, c| format!("{}/{}", c.handshakes_ok, c.handshakes)));
    line("    gateway replies late / handshakes refused", col(&|_, c| format!("{} / {}", c.replies_late, c.handshakes_refused)));
    line("    busiest gateway on air", col(&|_, c| format!("{:.2} % of an hour", 100.0 * c.max_gw_duty)));
    println!(
        "\n  A month's 0.5 % budget is {:.0} five-minute readings per meter. The steady-state loss continues during the \
         power-cut run, so its losses include it.",
        (1.0 - TARGET) * 30.0 * 288.0
    );
}
