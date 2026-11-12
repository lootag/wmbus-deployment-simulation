//! Frame sizes on air, and the handshake as it happens in a real capture.
//!
//! Sizes are wM-Bus L-field values *without* CRCs, as the HAL reports them
//! (`read_hal_pcap.py` prints them as `DLL L=...`). On air, Format B adds a
//! 2-byte CRC per block: one block up to 123 bytes, two above `[CS 4.2]`.
//! The radio's length field carries L itself.

/// C, M (2), A (6): the DLL header after the L-field.
pub const DLL_HEADER: usize = 9;
/// Short ELL: CI 8Ch, CC, ACC `[CS 4.5]`. Billing pushes only.
pub const ELL_SHORT: usize = 3;
/// Short TPL with security mode 13: CI, ACC, ST, CW (2), CFE `[CS 5.2.2]`.
/// Checked against `[PCAP #3]`: L=111 = 9 + 3 + 6 + 93-byte DTLS record.
pub const TPL_SHORT_MODE13: usize = 6;
/// DTLS 1.2 record with AES-128-GCM: 13-byte header, 8-byte explicit
/// nonce, 16-byte tag `[CS 5.1]`. Checked against `[PCAP #67]`: a 24-byte
/// Finished becomes a 48-byte record body.
pub const DTLS_RECORD_OVERHEAD: usize = 13 + 8 + 16;
/// DSMR6 command header: DIF, protocol id, message code, length (2) `[PS 4.1]`.
pub const DSMR6_HEADER: usize = 5;

/// Mode 9 as DSMR5 uses it: short TPL (CI, ACC, ST, CW) 5, Len E, Len U,
/// 4-byte counter, 12-byte GCM tag (DSMR5 P2 companion standard 4.3.3).
pub const MODE9_OVERHEAD: usize = 5 + 1 + 1 + 4 + 12;

/// Transport plus security bytes per frame. Mode 13: 43, the companion
/// standard's "43 bytes header data" `[CS 4.2]`.
pub fn security_overhead(mode: u8) -> usize {
    match mode {
        9 => MODE9_OVERHEAD,
        _ => TPL_SHORT_MODE13 + DTLS_RECORD_OVERHEAD,
    }
}

/// DSMR5 5-minute message: 52 bytes with L and CRC, so L = 49 (DSMR5 P2
/// 4.3.3).
pub const DSMR5_PERIODIC_L: usize = 49;
/// DSMR5 hourly message: DLL 9, ELL 3, short TPL 5, mode 9 header 6, tag 12
/// and the records: a 9-byte timestamp plus a 23-byte compact profile with
/// the last 4 hourly values (DSMR5 P2 6.4.5.1).
pub const DSMR5_HOURLY_L: usize = 9 + 3 + 5 + 6 + 12 + 9 + 23;
/// The daily (long) hourly message adds the status word and equipment id
/// `[EST: ~26 bytes of records]`.
pub const DSMR5_HOURLY_LONG_L: usize = DSMR5_HOURLY_L + 26;

/// Bytes the radio sends for a T1 frame of L-field `l`: Format A (a CRC
/// after the 10-byte first block and after every 16 bytes) and 3-out-of-6
/// coding, 1.5 chips per bit. The simulated radio has no 3-out-of-6
/// coding, so the payload is stretched instead.
pub fn t1_on_air_len(l: usize) -> usize {
    let crcs = 2 + 2 * (l.saturating_sub(9)).div_ceil(16);
    (3 * (1 + l + crcs)).div_ceil(2) - 1
}

/// Periodic push, Vb `[PS 4.2.4 / 4.2.5]`.
pub fn periodic_l(signed: bool, mode: u8) -> usize {
    // timestamp 4, index 4, gas temperature 2, status 1; signed adds the
    // equipment id 17, AMR status 1, signature length 1 and signature 64.
    let message = if signed { 4 + 17 + 4 + 1 + 1 + 64 + 2 + 1 } else { 4 + 4 + 2 + 1 };
    DLL_HEADER + security_overhead(mode) + DSMR6_HEADER + message
}

/// Billing push, Vb, with the short ELL that announces the FAC `[PS 4.2.2 / 4.2.3]`.
pub fn billing_l(signed: bool, mode: u8) -> usize {
    let message = if signed {
        4 + 17 + 4 + 1 + 1 + 64 + 2 + 1
    } else {
        4 + 18 + 4 + 1 + 2 + 1
    };
    DLL_HEADER + ELL_SHORT + security_overhead(mode) + DSMR6_HEADER + message
}

/// A meter ACK during a FAC (C=0x00, CI 8Ah, no APL) `[PCAP #6]`.
pub const METER_ACK_L: usize = 17;
/// A meter RSP_UD with an empty response (CI 7Ah) `[PCAP #68]`.
pub const METER_EMPTY_RSP_L: usize = 17;
/// Gateway SND_NKE heartbeat, sized like its other link-only frames
/// (long ELL, no TPL) `[PCAP #44]`.
pub const GW_SND_NKE_L: usize = 20;

/// Bytes on air after the length field: L bytes plus Format B CRCs.
pub fn on_air_len(l: usize) -> usize {
    l + if l <= 123 { 2 } else { 4 }
}

/// One turn of a FAC: the meter transmits, then listens for the gateway's
/// reply `tRO` later. `gw_l: None` means no reply is expected: the FAC's
/// tail, where the meter repeats its frame until the FAC closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Turn {
    pub meter_l: usize,
    pub gw_l: Option<usize>,
}

const fn turn(meter_l: usize, gw_l: usize) -> Turn {
    Turn {
        meter_l,
        gw_l: Some(gw_l),
    }
}

/// The full DTLS 1.2 handshake (ECDHE-ECDSA-AES128-GCM, mutual
/// certificates) as captured in `hal-dtls.pcap` between a Flonidan DSMR6
/// meter and the gateway. Frame numbers are `read_hal_pcap.py`'s.
///
/// 25 meter frames and 21 gateway frames. In the capture, the meter
/// transmits every 2 s and the handshake takes 42 s.
pub fn handshake() -> Vec<Turn> {
    let mut turns = vec![
        turn(111, 86),  // #3 SND_NR ClientHello            -> #5 SND_UD HelloVerifyRequest
        turn(17, 25),   // #6 ACK                           -> #8 REQ_UD2
        turn(143, 251), // #9 RSP_UD ClientHello (cookie)   -> #11 server flight, fragment 1/10
    ];
    // #12..#33: ACK -> fragments 2..9 (2064 bytes: ServerHello, Certificate,
    // ServerKeyExchange, CertificateRequest, ServerHelloDone).
    turns.extend(std::iter::repeat_n(turn(17, 251), 8));
    turns.push(turn(17, 47)); // #36 ACK -> #38 fragment 10/10
    turns.push(turn(17, 25)); // #39 ACK -> #41 REQ_UD2 for the client flight
    // #42..#62: client flight fragments 1..7 (Certificate, ClientKeyExchange,
    // CertificateVerify, ChangeCipherSpec, Finished: 1776 bytes), each
    // pulled with a REQ_UD2.
    turns.extend(std::iter::repeat_n(turn(246, 20), 7));
    turns.push(turn(182, 101)); // #65 fragment 8/8 -> #67 SND_UD2 ChangeCipherSpec + Finished
    // #68..#74: RSP_UD, repeated until the FAC closes.
    turns.push(Turn {
        meter_l: METER_EMPTY_RSP_L,
        gw_l: None,
    });
    turns
}

/// Index of the turn after which the meter holds the server's whole
/// flight: from there on it has verified the chain and computed ECDHE.
pub fn handshake_crypto_turn() -> usize {
    3 + 8 + 1 // the ACK of fragment 10
}

/// Gateway Channel Request, sent unencrypted after a gateway restart to ask
/// for a new DTLS session `[CS 4.11]`, `[OMS F.4]`. Size: long ELL, long TPL
/// and an 8-byte CMAC `[EST]`.
pub const GW_CHANNEL_REQUEST_L: usize = 40;

/// What a frame is, in the first byte of the simulated payload. The rest of
/// a real frame is opaque to an energy model; the simulation only needs to
/// know who sent what.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Kind {
    #[default]
    Periodic = 1,
    Billing = 2,
    /// The midnight billing push, after which the gateway sends a heartbeat.
    BillingMidnight = 3,
    Handshake = 4,
    FacAck = 5,
    GwHandshake = 10,
    GwNke = 11,
    GwChannelRequest = 12,
}

impl Kind {
    fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            1 => Kind::Periodic,
            2 => Kind::Billing,
            3 => Kind::BillingMidnight,
            4 => Kind::Handshake,
            5 => Kind::FacAck,
            10 => Kind::GwHandshake,
            11 => Kind::GwNke,
            12 => Kind::GwChannelRequest,
            _ => return None,
        })
    }
}

/// Header of a simulated frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Header {
    pub kind: Kind,
    /// The meter's address (also on gateway frames: the addressee).
    pub meter: u64,
    /// FAC turn.
    pub turn: u16,
    /// DTLS session the frame is encrypted under (pushes), or the session a
    /// handshake establishes.
    pub epoch: u16,
    /// Push: the 5-minute slot (periodic) or billing interval (billing)
    /// whose value it carries.
    pub slot: u32,
}

const HEADER_LEN: usize = 17;

/// A C-mode Format B frame of L-field `l` carrying `header`.
pub fn encode(header: Header, l: usize) -> Vec<u8> {
    encode_bytes(header, on_air_len(l))
}

/// A frame of `bytes` bytes after the length field, carrying `header`.
pub fn encode_bytes(header: Header, bytes: usize) -> Vec<u8> {
    let mut frame = vec![0u8; bytes.max(HEADER_LEN)];
    frame[0] = header.kind as u8;
    frame[1..9].copy_from_slice(&header.meter.to_le_bytes());
    frame[9..11].copy_from_slice(&header.turn.to_le_bytes());
    frame[11..13].copy_from_slice(&header.epoch.to_le_bytes());
    frame[13..17].copy_from_slice(&header.slot.to_le_bytes());
    frame
}

pub fn decode(frame: &[u8]) -> Option<Header> {
    if frame.len() < HEADER_LEN {
        return None;
    }
    Some(Header {
        kind: Kind::from_u8(frame[0])?,
        meter: u64::from_le_bytes(frame[1..9].try_into().ok()?),
        turn: u16::from_le_bytes(frame[9..11].try_into().ok()?),
        epoch: u16::from_le_bytes(frame[11..13].try_into().ok()?),
        slot: u32::from_le_bytes(frame[13..17].try_into().ok()?),
    })
}

/// Whether `turn` is the handshake's last turn with a gateway reply: the
/// gateway's ChangeCipherSpec + Finished, after which the session is up.
pub fn is_final_gw_turn(turn: usize) -> bool {
    let hs = handshake();
    hs.get(turn).is_some_and(|t| t.gw_l.is_some()) && hs.get(turn + 1).is_some_and(|t| t.gw_l.is_none())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_matches_the_capture() {
        let hs = handshake();
        assert_eq!(hs.len(), 22, "turns before the FAC tail, plus the tail");
        let gw: Vec<usize> = hs.iter().filter_map(|t| t.gw_l).collect();
        assert_eq!(gw.len(), 21, "21 gateway frames in the capture");
        // Server flight: 9 full fragments + 1 short, as reassembled to 2064 B.
        assert_eq!(gw.iter().filter(|&&l| l == 251).count(), 9);
        // Client flight: 7 x 246 + 182 (reassembled to 1776 B).
        let client: usize = hs.iter().map(|t| t.meter_l).filter(|&l| l == 246 || l == 182).sum();
        assert_eq!(client, 7 * 246 + 182);
        assert!(hs[handshake_crypto_turn()].gw_l == Some(25));
        assert!(is_final_gw_turn(20));
        assert!(!is_final_gw_turn(19) && !is_final_gw_turn(21));
    }

    #[test]
    fn frames_fit_format_b() {
        for mode in [9, 13] {
            for signed in [false, true] {
                assert!(on_air_len(periodic_l(signed, mode)) <= 255);
                assert!(on_air_len(billing_l(signed, mode)) <= 255);
            }
        }
        assert_eq!(periodic_l(false, 13), 68);
        assert_eq!(billing_l(false, 13), 90);
        assert_eq!(security_overhead(13), 43);
        assert_eq!(security_overhead(9), 23);
    }

    #[test]
    fn header_round_trips() {
        let h = Header {
            kind: Kind::Handshake,
            meter: 0x0102_0304_0506_0708,
            turn: 17,
            epoch: 3,
            slot: 123_456,
        };
        assert_eq!(decode(&encode(h, METER_ACK_L)), Some(h));
    }
}
