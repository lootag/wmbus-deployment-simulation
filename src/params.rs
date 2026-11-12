//! Every number the simulation depends on, with where it comes from.
//!
//! All fields have defaults; a JSON file passed with `--config` only needs
//! the fields it changes, e.g. `{"radio": {"tx_power_dbm": 20}}`.
//!
//! Sources:
//! - `[CS]`  DSMR6 X2-1 companion standard (`app-svc-wmbus/dsmr6_companion_standard.pdf`)
//! - `[PS]`  DSMR6 protocol specification v1.0 (`app-svc-wmbus/protocol_spec.pdf`)
//! - `[OMS]` OMS Vol. 2 Annex F (`app-svc-wmbus/OMS-Spec_Vol2_AnnexF_C111.pdf`)
//! - `[PCAP]` the real handshake in `app-svc-wmbus/hal-dtls.pcap`
//! - `[SBOM]` Flonidan release SBOM: STM32CubeWL, mbedtls 3.6, micro-ecc,
//!   so an STM32WL (Cortex-M4 at 48 MHz plus the sub-GHz radio)
//! - `[EST]` estimates of datasheet order for an STM32WL. **Replace with
//!   measurements** (e.g. a Nordic PPK2 on a meter in each state).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Params {
    /// Simulated span, in days.
    pub days: f64,
    /// Seed for every random choice (jitter, shadowing).
    pub seed: u64,
    pub protocol: Protocol,
    pub radio: RadioHw,
    pub mcu: Mcu,
    pub crypto: Crypto,
    pub gateway: GatewayPolicy,
    /// The district: layout, density, run lengths, power cut.
    pub district: crate::district::District,
}

impl Params {
    /// Set every protocol field that differs between DSMR5 and DSMR6.
    pub fn apply_profile(&mut self, profile: Profile) {
        let pr = &mut self.protocol;
        pr.profile = profile;
        match profile {
            Profile::Dsmr6 => {
                let d = Protocol::default();
                pr.security_mode = d.security_mode;
                pr.sign_periodic = d.sign_periodic;
                pr.sign_billing = d.sign_billing;
                pr.response_delay_ms = d.response_delay_ms;
                pr.rx_guard_ms = d.rx_guard_ms;
                pr.billing_spread_s = d.billing_spread_s;
                pr.daily_heartbeat = d.daily_heartbeat;
            }
            Profile::Dsmr5 => {
                // Mode 9, key installed once: no handshake, nothing signed.
                pr.security_mode = 9;
                pr.sign_periodic = false;
                pr.sign_billing = false;
                // T2: the reply follows 2–3 ms after the meter's T1 frame
                // (EN 13757-4 T-mode tRO).
                pr.response_delay_ms = 2.5;
                pr.rx_guard_ms = 0.5;
                // Hourly messages 1–10 min after the hour (DSMR5 P2 3.1.7).
                pr.billing_spread_s = Some([60.0, 600.0]);
                // One FAC credit per 24 h, used only when the E-meter has a
                // command: no routine heartbeat.
                pr.daily_heartbeat = false;
            }
        }
    }
}

impl Default for Params {
    fn default() -> Self {
        Self {
            days: 30.0,
            seed: 1,
            protocol: Protocol::default(),
            radio: RadioHw::default(),
            mcu: Mcu::default(),
            crypto: Crypto::default(),
            gateway: GatewayPolicy::default(),
            district: crate::district::District::default(),
        }
    }
}

/// Which companion standard the meter follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    /// DSMR6 X2-1: C-mode throughout, DTLS (mode 13), signed pushes.
    #[default]
    Dsmr6,
    /// DSMR5 P2 (Enexis companion standard 5.1): 5-minute messages in C1,
    /// hourly messages in T1 with a T2 reply window, mode 9 with a
    /// pre-installed key, no signatures.
    Dsmr5,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Protocol {
    /// Companion standard; `Params::for_profile` sets the fields that go
    /// with it.
    pub profile: Profile,
    /// Periodic push interval, s. `[PS 4.2.9]` allows 1–5 min. The case asked
    /// for: 5 min.
    pub periodic_interval_s: u64,
    /// Billing push interval, s. `[PS 4.2.7]` allows 15, 30 or 60 min. The
    /// case asked for: hourly.
    pub billing_interval_s: u64,
    /// Transmissions happen at a random point within this window after
    /// each interval, s `[CS 4.9]`.
    pub randomisation_window_s: f64,
    /// A billing push due in the same slot as a periodic push is sent this
    /// long after the periodic push completes, ms `[CS 4.9]`.
    pub billing_after_periodic_ms: f64,
    /// wM-Bus security mode: 13 (DTLS 1.2, `[CS 5.2]`) or 9 (AES-128-GCM
    /// with a pre-installed key, as in DSMR5: no handshake, 23 bytes of
    /// transport and security overhead instead of 43).
    pub security_mode: u8,
    /// CSMA on pushes: a channel occupancy check before every transmission
    /// (GM-GM-602), following the preferred implementation in the DSMR6
    /// G-meter CSMA requirements (Netbeheer Nederland, GM-TXT-103/104), which is also
    /// DSMR5's (P2 CS 11.1):
    /// listen `csma_listen_ms` (plus a random `csma_tps_ms` after a busy
    /// check); if busy, turn the receiver off and wait each of
    /// `csma_backoffs_s` in turn before checking again; if the last check is
    /// busy too, send at a random moment in what is left of the message's
    /// window, or `csma_late_delay_s` later if it has passed (GM-GM-603).
    pub lbt: bool,
    /// Channel busy at or above this level, dBm (GM-GM-606: −85 dBm in
    /// 200 kHz, corrected for antenna gain; the simulated antenna has 0 dBi).
    pub lbt_threshold_dbm: i16,
    /// Fixed listening time Tf, ms.
    pub csma_listen_ms: f64,
    /// Pseudo-random listening time Tps after a busy check, ms, in 0.5 ms
    /// steps.
    pub csma_tps_ms: [f64; 2],
    /// Waits between checks, s, each drawn in 100 ms steps: 0.5–2 s, 0.5–2 s,
    /// then a fixed 5 s.
    pub csma_backoffs_s: Vec<[f64; 2]>,
    /// The 5-minute message's window, s after the boundary (also the hourly
    /// message's, unless `billing_spread_s` gives a wider one).
    pub csma_window_s: f64,
    /// Delay before the guaranteed send when the window has passed, s.
    pub csma_late_delay_s: f64,
    /// Hourly push spread: sent at a random point this many seconds after
    /// the hour, instead of right after the periodic push in the same 10 s
    /// window `[CS 4.9]`. DSMR5 spreads them over 1–10 min. Must end before
    /// the next hour.
    pub billing_spread_s: Option<[f64; 2]>,
    /// Sign periodic values (ECDSA, 64-byte signature) `[CS 5.7, Table 12]`.
    /// Off: only the hourly values are signed `[CS 5.4]`.
    pub sign_periodic: bool,
    /// Sign billing (hourly) values. On.
    pub sign_billing: bool,
    /// How often a frequent access cycle (FAC) may be granted, s. Attribute
    /// 150 defaults to 60 min `[PS App. 1]`. A FAC is only offered after a
    /// billing push `[CS 4.8.1]`.
    pub fac_interval_s: u64,
    /// Response delay tRO: from the end of a meter frame to the start of
    /// the gateway's reply, ms `[CS Fig. 5: tro(min) = 1000 ms]`.
    pub response_delay_ms: f64,
    /// Tolerance on tRO on either side, which the meter's receive window
    /// covers, ms. EN 13757-4 gives ±0.5 ms in C-mode; the extra margin
    /// covers wake-up timing and clock drift.
    pub rx_guard_ms: f64,
    /// Extra listening per receive window beyond what the protocol needs,
    /// ms. Real stacks often listen much longer than the minimum; use this
    /// to see what that costs.
    pub rx_extra_listen_ms: f64,
    /// FAC transmission delay: the meter transmits every tTxD during a FAC,
    /// s. The capture shows 2 s throughout `[PCAP]`; `[CS 4.8.1]` names
    /// N = 5 normally and N = 2 during firmware updates.
    pub fac_tx_delay_s: f64,
    /// Transmissions a FAC stays open for once the gateway has nothing more
    /// to send. In the capture, the meter sent its final RSP_UD 4 times
    /// `[PCAP #68–#74]`; `[CS 4.8.1]` caps a FAC at 6 transmissions / 30 s.
    pub fac_tail_transmissions: u32,
    /// A handshake is abandoned after this many consecutive meter
    /// transmissions without the expected reply `[CS 4.8.1]`.
    pub fac_abort_after_silent: u32,
    /// The gateway sends an SND_NKE heartbeat in the FAC after the midnight
    /// billing push every day `[CS 4.10]`.
    pub daily_heartbeat: bool,
    /// Full DTLS handshake interval, days. `[OMS F.4]` caps a session at
    /// 31 days. The case asked for: monthly.
    pub handshake_interval_days: f64,
    /// Binding mode: after a failed handshake, the meter has no session and
    /// sends a new ClientHello this often, s. The companion standard names
    /// slow and fast binding modes but no intervals `[CS 3.1]` `[EST]`.
    pub handshake_retry_s: f64,
}

impl Default for Protocol {
    fn default() -> Self {
        Self {
            profile: Profile::Dsmr6,
            periodic_interval_s: 300,
            billing_interval_s: 3600,
            randomisation_window_s: 10.0,
            billing_after_periodic_ms: 100.0,
            security_mode: 13,
            lbt: true,
            lbt_threshold_dbm: -85,
            csma_listen_ms: 5.0,
            csma_tps_ms: [0.5, 5.0],
            csma_backoffs_s: vec![[0.5, 2.0], [0.5, 2.0], [5.0, 5.0]],
            csma_window_s: 10.0,
            csma_late_delay_s: 1.0,
            billing_spread_s: None,
            sign_periodic: false,
            sign_billing: true,
            fac_interval_s: 3600,
            response_delay_ms: 1000.0,
            rx_guard_ms: 1.0,
            rx_extra_listen_ms: 0.0,
            fac_tx_delay_s: 2.0,
            fac_tail_transmissions: 4,
            fac_abort_after_silent: 6,
            daily_heartbeat: true,
            handshake_interval_days: 30.0,
            handshake_retry_s: 900.0,
        }
    }
}

/// How a gateway shares itself between its meters.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GatewayPolicy {
    /// Handshakes (and Channel Requests) a gateway runs at once; further
    /// ClientHellos are ignored, Channel Requests postponed `[EST]`.
    pub max_concurrent_handshakes: usize,
    /// Share of any hour the gateway may transmit on 869.525 MHz: 10 %,
    /// the limit for 869.4–869.65 MHz under ERC Rec. 70-03 as I understand
    /// it (check against the current edition). Replies beyond it are dropped.
    pub duty_cycle: f64,
}

impl Default for GatewayPolicy {
    fn default() -> Self {
        Self {
            max_concurrent_handshakes: 8,
            duty_cycle: 0.10,
        }
    }
}

/// The meter's sub-GHz radio `[SBOM: STM32WL]`, currents at the battery
/// `[EST]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RadioHw {
    /// Battery voltage, V (Li-SOCl₂: 3.6 V).
    pub supply_v: f64,
    /// Meter transmit power, dBm.
    pub tx_power_dbm: i8,
    /// Transmit current per output power, `(dBm, mA)`, interpolated.
    /// Default: STM32WL low-power PA with the SMPS on `[EST]`. The
    /// high-power PA draws far more: roughly 45 mA at +14 dBm, 85 mA at
    /// +20 dBm.
    pub tx_curve: Vec<(f64, f64)>,
    /// Receive current, mA `[EST]`.
    pub rx_ma: f64,
    /// Radio standby (between operations), mA `[EST]`.
    pub idle_ma: f64,
    /// Radio sleep, mA. 0 by default because it is folded into
    /// `mcu.sleep_ua`.
    pub sleep_ma: f64,
    /// Wake-up from sleep to standby, ms (warm start plus oscillator) `[EST]`.
    pub sleep_to_idle_ms: f64,
    /// Standby to transmitting (PLL lock, PA ramp), ms `[EST]`.
    pub idle_to_tx_ms: f64,
    /// Standby to receiving, ms `[EST]`.
    pub idle_to_rx_ms: f64,
    /// Back to standby after TX / RX, ms `[EST]`.
    pub to_idle_ms: f64,
    /// Retuning between 868.95 and 869.525 MHz, ms `[EST]`.
    pub frequency_change_ms: f64,
    /// Gateway sensitivity for meter frames (C-mode M2O, 100 kcps), dBm.
    pub gw_sensitivity_dbm: f64,
    /// Meter sensitivity for gateway frames (C-mode O2M, 50 kcps), dBm.
    pub meter_sensitivity_dbm: f64,
    /// Gateway transmit power, dBm.
    pub gw_tx_power_dbm: i8,
}

impl Default for RadioHw {
    fn default() -> Self {
        Self {
            supply_v: 3.6,
            tx_power_dbm: 14,
            tx_curve: vec![(0.0, 10.0), (10.0, 15.0), (14.0, 24.0), (15.0, 26.0)],
            rx_ma: 5.5,
            idle_ma: 0.9,
            sleep_ma: 0.0,
            sleep_to_idle_ms: 0.5,
            idle_to_tx_ms: 0.15,
            idle_to_rx_ms: 0.15,
            to_idle_ms: 0.05,
            frequency_change_ms: 0.05,
            gw_sensitivity_dbm: -105.0,
            meter_sensitivity_dbm: -108.0,
            gw_tx_power_dbm: 14,
        }
    }
}

/// The meter's MCU `[SBOM: STM32WL, Cortex-M4 at 48 MHz]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Mcu {
    /// Deep sleep (Stop2, RTC on, radio asleep), µA `[EST]`.
    pub sleep_ua: f64,
    /// Running, mA `[EST]`.
    pub active_ma: f64,
    /// MCU run time charged per wake-up (scheduling, building and
    /// encrypting a DTLS record with AES-GCM), ms `[EST]`.
    pub wake_time_ms: f64,
    /// MCU (RTC) clock error, ppm; meters alternate between + and −.
    pub drift_ppm: f64,
}

impl Default for Mcu {
    fn default() -> Self {
        Self {
            sleep_ua: 1.2,
            active_ma: 3.0,
            wake_time_ms: 2.0,
            drift_ppm: 20.0,
        }
    }
}

/// Public-key operations, which the simulator does not see (it runs
/// firmware in zero time), so they are charged on top at `mcu.active_ma`.
/// Times are for P-256 on a 48 MHz Cortex-M4 with micro-ecc `[SBOM]`
/// `[EST]`. With the STM32WL's PKA accelerator they would be shorter.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Crypto {
    pub ecdsa_verify_ms: f64,
    pub ecdsa_sign_ms: f64,
    /// One scalar multiplication (ECDHE key generation or shared secret).
    pub ecdh_ms: f64,
    /// Per handshake: the server's certificate chain plus its
    /// ServerKeyExchange signature.
    pub handshake_verifies: u32,
    /// Per handshake: CertificateVerify.
    pub handshake_signs: u32,
    /// Per handshake: ephemeral key generation plus the shared secret.
    pub handshake_ecdh: u32,
}

impl Default for Crypto {
    fn default() -> Self {
        Self {
            ecdsa_verify_ms: 150.0,
            ecdsa_sign_ms: 120.0,
            ecdh_ms: 110.0,
            handshake_verifies: 3,
            handshake_signs: 1,
            handshake_ecdh: 2,
        }
    }
}
