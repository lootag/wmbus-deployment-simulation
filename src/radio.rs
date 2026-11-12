//! wM-Bus C-mode PHYs (EN 13757-4) and the meter's electrical model.

use std::time::Duration;

use radiofreccia_sim::{
    DriverTiming, EnergyModel, LengthMode, McuCurrents, Modulation, PacketFormat, PhyConfig, RadioCurrents,
    RadioTiming, ReceiverSettings, SyncWord, TxCurrent,
};

use crate::params::Params;

/// C-mode meter-to-other channel.
pub const M2O_HZ: u32 = 868_950_000;
/// C-mode other-to-meter channel.
pub const O2M_HZ: u32 = 869_525_000;

/// Format B second sync word, sent after the C-mode sync word.
const FORMAT_B: u64 = 0x54CD;

fn packet() -> PacketFormat {
    // The radio's length field is the wM-Bus L-field; Format B CRCs are part
    // of the payload (see `frames::on_air_len`).
    PacketFormat {
        length: LengthMode::Variable { field_bits: 8 },
        max_payload_len: 255,
        crc: None,
        whitening: None,
    }
}

/// Meter to gateway: 100 kcps NRZ 2-FSK, ±45 kHz, at least 19 × "01"
/// preamble and sync word 0000111101 `[CS 4.1]`.
pub fn m2o(gw_sensitivity_dbm: f64) -> PhyConfig {
    PhyConfig {
        modulation: Modulation::Fsk2,
        bit_rate_bps: 100_000,
        deviation_hz: 45_000,
        rx_bandwidth_hz: 250_000,
        preamble_bits: 2 * 19,
        sync_word: SyncWord::new((0b00_0011_1101 << 16) | FORMAT_B, 26),
        packet: packet(),
        receiver: ReceiverSettings {
            sensitivity_dbm: gw_sensitivity_dbm,
            ..ReceiverSettings::default()
        },
        ..PhyConfig::default()
    }
}

/// Gateway to meter: 50 kcps GFSK, ±25 kHz, at least 15 × "01" preamble
/// and sync word 0001110110 `[CS 4.1]`.
pub fn o2m(meter_sensitivity_dbm: f64) -> PhyConfig {
    PhyConfig {
        modulation: Modulation::Gfsk2 { bt: 0.5 },
        bit_rate_bps: 50_000,
        deviation_hz: 25_000,
        rx_bandwidth_hz: 150_000,
        preamble_bits: 2 * 15,
        sync_word: SyncWord::new((0b00_0111_0110 << 16) | FORMAT_B, 26),
        packet: packet(),
        receiver: ReceiverSettings {
            sensitivity_dbm: meter_sensitivity_dbm,
            ..ReceiverSettings::default()
        },
        ..PhyConfig::default()
    }
}

fn ms(v: f64) -> Duration {
    Duration::from_secs_f64(v / 1000.0)
}

/// The meter's supply currents.
pub fn meter_energy(p: &Params, tx_curve: &[(f64, f64)]) -> EnergyModel {
    EnergyModel {
        supply_voltage_v: p.radio.supply_v,
        radio: RadioCurrents {
            sleep_ma: p.radio.sleep_ma,
            idle_ma: p.radio.idle_ma,
            rx_ma: p.radio.rx_ma,
            switching_ma: None,
            tx: tx_curve.iter().map(|&(dbm, ma)| TxCurrent { dbm, ma }).collect(),
        },
        mcu: McuCurrents {
            sleep_ma: p.mcu.sleep_ua / 1000.0,
            active_ma: p.mcu.active_ma,
            wake_time: ms(p.mcu.wake_time_ms),
        },
    }
}

pub fn meter_timing(p: &Params) -> RadioTiming {
    let r = &p.radio;
    RadioTiming {
        sleep_to_idle: ms(r.sleep_to_idle_ms),
        idle_to_sleep: ms(0.01),
        idle_to_rx: ms(r.idle_to_rx_ms),
        idle_to_tx: ms(r.idle_to_tx_ms),
        rx_to_idle: ms(r.to_idle_ms),
        tx_to_idle: ms(r.to_idle_ms),
        frequency_change: ms(r.frequency_change_ms),
        // DSMR5 LBT fixed listen time tF.
        carrier_sense: ms(5.0),
        ..RadioTiming::default()
    }
}

/// STM32WL: the radio sits on an internal SPI. Small, but the simulator
/// charges it.
pub fn meter_driver() -> DriverTiming {
    DriverTiming {
        command_latency: Duration::from_micros(20),
        irq_latency: Duration::from_micros(10),
        payload_write_per_byte: Duration::from_nanos(500),
        payload_read_per_byte: Duration::from_nanos(500),
        ..DriverTiming::default()
    }
}

/// DSMR5 hourly replies, T2 other-to-meter: Manchester at 32.768 kcps on
/// the same 868.95 MHz channel, 15 × "01" preamble (EN 13757-4 T-mode).
pub fn t2_o2m(meter_sensitivity_dbm: f64) -> PhyConfig {
    PhyConfig {
        modulation: Modulation::Fsk2,
        bit_rate_bps: 32_768,
        deviation_hz: 50_000,
        rx_bandwidth_hz: 250_000,
        coding: radiofreccia_sim::LineCoding::Manchester,
        preamble_bits: 2 * 15,
        sync_word: SyncWord::new(0b00_0111_0110, 10),
        packet: packet(),
        receiver: ReceiverSettings {
            sensitivity_dbm: meter_sensitivity_dbm,
            ..ReceiverSettings::default()
        },
    }
}

/// Where a meter listens for, and a gateway sends, replies after a push
/// that offers a FAC.
#[derive(Debug, Clone)]
pub struct ReplyLink {
    pub phy: PhyConfig,
    pub frequency_hz: u32,
}

impl ReplyLink {
    pub fn for_params(p: &Params) -> Self {
        match p.protocol.profile {
            crate::params::Profile::Dsmr6 => Self {
                phy: o2m(p.radio.meter_sensitivity_dbm),
                frequency_hz: O2M_HZ,
            },
            crate::params::Profile::Dsmr5 => Self {
                phy: t2_o2m(p.radio.meter_sensitivity_dbm),
                frequency_hz: M2O_HZ,
            },
        }
    }
}

/// Time from the first preamble bit until the sync word has been received.
pub fn acquisition(phy: &PhyConfig) -> Duration {
    phy.acquisition_time()
}
