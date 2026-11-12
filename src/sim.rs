//! Builds the ether, runs meters and gateways, collects the results.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use radiofreccia_core::{Channel, Clock, TxRadio};
use radiofreccia_sim::{
    ClockModel, EnergyReport, Environment, Ether, EtherConfig, NodeClocks, NodeOptions, Position,
    RadioStats,
};

use crate::gateway::{Gateway, GatewaySetup, MeterLog, Shared, World};
use crate::meter::{Counters, Ledger, Meter, MeterSetup};
use crate::params::Params;
use crate::radio::{self, M2O_HZ};

/// A meter to simulate.
#[derive(Debug, Clone)]
pub struct MeterNode {
    pub setup: MeterSetup,
    pub label: String,
    pub position: Option<Position>,
    /// Its clocks; `None`: perfect at start, ±`mcu.drift_ppm` alternating.
    pub clocks: Option<NodeClocks>,
}

/// One meter's results.
#[derive(Debug, Clone)]
pub struct MeterResult {
    pub node: MeterNode,
    pub energy: EnergyReport,
    pub stats: RadioStats,
    pub ledger: Ledger,
    pub counters: Counters,
    /// What its gateway saw.
    pub gw: MeterLog,
    pub error: Option<String>,
}

impl MeterResult {
    pub fn id(&self) -> u64 {
        self.node.setup.id
    }

    pub fn days(&self) -> f64 {
        self.energy.elapsed.as_secs_f64() / 86_400.0
    }

    /// Total charge, mAh: everything the simulator measured plus the
    /// public-key work charged on top.
    pub fn total_mah(&self) -> f64 {
        self.energy.total_mah() + self.ledger.compute_total()
    }

    /// Average current, µA.
    pub fn average_ua(&self) -> f64 {
        1000.0 * self.total_mah() / (self.days() * 24.0)
    }

}

/// One gateway's results.
#[derive(Debug, Clone)]
pub struct GatewayResult {
    pub id: u64,
    pub meters: Vec<u64>,
    pub energy: EnergyReport,
    pub stats: RadioStats,
}

/// Everything a run produced.
pub struct Run {
    pub meters: Vec<MeterResult>,
    pub gateways: Vec<GatewayResult>,
    pub world: World,
}

/// Run meters and gateways in `environment` for `p.days`.
pub fn run(
    p: &Params,
    environment: Environment,
    meters: Vec<MeterNode>,
    gateways: Vec<(GatewaySetup, Option<Position>)>,
) -> Run {
    let p = Arc::new(p.clone());
    radiofreccia_sim::run(run_async(p, environment, meters, gateways))
}

async fn run_async(
    p: Arc<Params>,
    environment: Environment,
    meters: Vec<MeterNode>,
    gateways: Vec<(GatewaySetup, Option<Position>)>,
) -> Run {
    let ether = Ether::new(EtherConfig {
        environment,
        phy: radio::m2o(p.radio.gw_sensitivity_dbm),
        default_frequency_hz: M2O_HZ,
        default_tx_power_dbm: p.radio.tx_power_dbm,
        seed: p.seed,
        event_capacity: 1 << 16,
        ..Default::default()
    })
    .expect("valid ether config");
    let end = Duration::from_secs_f64(p.days * 86_400.0);
    let truth = ether.clock();
    let world: Shared = Arc::new(Mutex::new(World::default()));

    // Gateways: perfect clocks, no energy model (mains powered).
    let mut gw_handles = Vec::new();
    for (setup, position) in gateways {
        let mut radio = ether
            .attach_with(
                setup.id,
                NodeOptions {
                    position,
                    ..Default::default()
                },
            )
            .expect("gateway");
        radio.set_timing(radio::meter_timing(&p));
        radio.set_tx_power(p.radio.gw_tx_power_dbm).await.expect("power");
        let (id, meters) = (setup.id, setup.meters.clone());
        let mut gw = Gateway::new(setup, &p, world.clone());
        let truth = truth.clone();
        gw_handles.push(tokio::spawn(async move {
            let _ = truth.timeout(end, gw.run(&mut radio)).await;
            truth.sleep_until(end).await;
            GatewayResult {
                id,
                meters,
                energy: radio.energy(),
                stats: radio.stats(),
            }
        }));
    }

    // Meters.
    let energy = radio::meter_energy(&p, &p.radio.tx_curve);
    let mut handles = Vec::new();
    for node in meters {
        let id = node.setup.id;
        let sign = if id % 2 == 0 { 1.0 } else { -1.0 };
        let clocks = node.clocks.unwrap_or(NodeClocks {
            mcu: ClockModel::drifting(sign * p.mcu.drift_ppm),
            radio: ClockModel::drifting(sign * 10.0),
            carrier_ppm: sign * 10.0,
        });
        let mut radio = ether
            .attach_with(
                id,
                NodeOptions {
                    position: node.position,
                    clocks: Some(clocks),
                    energy: Some(energy.clone()),
                    ..Default::default()
                },
            )
            .expect("meter");
        radio.set_timing(radio::meter_timing(&p));
        radio.set_driver_timing(radio::meter_driver());
        radio.set_frequency(M2O_HZ).await.expect("tune");
        let (p, truth) = (p.clone(), truth.clone());
        handles.push(tokio::spawn(async move {
            let mut meter = Meter::new(node.setup, p);
            let left = end.saturating_sub(truth.now_exact());
            let result = truth.timeout(left, meter.run(&mut radio)).await;
            truth.sleep_until(end).await;
            MeterResult {
                node,
                energy: radio.energy(),
                stats: radio.stats(),
                ledger: meter.ledger,
                counters: meter.counters,
                gw: MeterLog::default(),
                error: match result {
                    Ok(Err(e)) => Some(format!("{e:?}")),
                    _ => None,
                },
            }
        }));
    }
    let mut results = Vec::new();
    for h in handles {
        results.push(h.await.expect("meter task"));
    }
    let mut gateways = Vec::new();
    for h in gw_handles {
        gateways.push(h.await.expect("gateway task"));
    }
    let world = std::mem::take(&mut *world.lock().unwrap());
    for r in &mut results {
        r.gw = world.meters.get(&r.id()).cloned().unwrap_or_default();
    }
    Run {
        meters: results,
        gateways,
        world,
    }
}
