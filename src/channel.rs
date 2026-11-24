//! One IaR channel's transmissions: which call holds it, when it switches, and the audio it sends.
//!
//! Nothing here touches the network. A [`Lane`] turns call audio into the [`Cmd`]s the channel's
//! WebSocket sends: `tx_start` and `tx_end` as text, and each 60 ms Opus frame as a binary packet.

use std::time::{Duration, Instant};

use serde_json::json;

use crate::config::{Channel, Rank, ScanMode};
use crate::opus::Encoder;

/// Sent as `clientSwName`, so IaR can tell this plugin's transmissions apart.
pub const CLIENT_NAME: &str = "trunk-plugin-iarliveaudio";
/// A holder that has been quiet this long gives the channel to another call.
pub const LOCK_IDLE: Duration = Duration::from_millis(500);
/// A transmission with no audio for this long is ended, even without the call's end.
pub const TX_IDLE_END: Duration = Duration::from_secs(2);
/// One packet of audio, as IaR-Stream sends it.
pub const FRAME_MS: u32 = 60;

/// What a channel sends to its WebSocket.
#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    /// A `tx_start` or `tx_end` message, as JSON text.
    Control(String),
    /// One audio packet: the 14-byte header and the Opus frame.
    Audio(Vec<u8>),
}

/// A call's audio, as the channel sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub system_id: u32,
    pub call_id: u32,
    pub rank: Rank,
}

/// The call holding a channel now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Holder {
    pub system_id: u32,
    pub call_id: u32,
    pub rank: Rank,
    pub last_audio: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Another call keeps the channel; this audio is dropped.
    Ignore,
    /// The holder's own audio: keep going.
    Continue,
    /// Take the channel. With `preempt`, the holder is ended first.
    Start { preempt: bool },
}

/// Who gets the channel. The holder keeps it until its call ends or it goes quiet for
/// [`LOCK_IDLE`]; in priority scan a strictly higher-priority talkgroup also takes it at once.
/// The same call's copies on several sites share one holder, so they never play twice.
pub fn decide(mode: ScanMode, holder: Option<&Holder>, cand: Candidate, now: Instant) -> Decision {
    let Some(h) = holder else {
        return Decision::Start { preempt: false };
    };
    if h.system_id == cand.system_id && h.call_id == cand.call_id {
        return Decision::Continue;
    }
    if mode == ScanMode::PriorityScan && cand.rank < h.rank {
        return Decision::Start { preempt: true };
    }
    if now.saturating_duration_since(h.last_audio) >= LOCK_IDLE {
        return Decision::Start { preempt: true };
    }
    Decision::Ignore
}

/// Samples in one [`FRAME_MS`] frame at `rate`.
pub fn frame_len(rate: u32) -> usize {
    (rate * FRAME_MS / 1000) as usize
}

/// IaR's `startTimeKey`: tenths of a second since the epoch, as 32 bits.
pub fn start_key(unix_s: f64) -> u32 {
    (unix_s * 10.0) as u64 as u32
}

/// One audio packet: big-endian type (1), sender (u32), random client ID (u16), start key (u32),
/// packet number (u16), then the Opus frame.
pub fn packet(sender: u32, random_id: u16, start_key: u32, packet_id: u16, opus: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(14 + opus.len());
    v.extend_from_slice(&1u16.to_be_bytes());
    v.extend_from_slice(&sender.to_be_bytes());
    v.extend_from_slice(&random_id.to_be_bytes());
    v.extend_from_slice(&start_key.to_be_bytes());
    v.extend_from_slice(&packet_id.to_be_bytes());
    v.extend_from_slice(opus);
    v
}

/// The fields of a `tx_start` or `tx_end` message.
pub struct TxMessage<'a> {
    pub channel_id: &'a str,
    pub random_id: u16,
    pub source: &'a str,
    pub start: f64,
    pub start_key: u32,
    pub end: Option<f64>,
}

pub fn tx_json(action: &str, m: &TxMessage) -> String {
    let mut v = json!({
        "action": action,
        "v": 1,
        "channelId": m.channel_id,
        "sendingMemberId": 0,
        "clientSwName": CLIENT_NAME,
        "clientRandomId": m.random_id,
        "sourceName": m.source,
        "priority": 3,
        "type": 1,
        "startTime": m.start,
        "startTimeKey": m.start_key,
    });
    if let Some(end) = m.end {
        v["endTime"] = json!(end);
    }
    v.to_string()
}

/// A transmission in progress.
#[derive(Clone, Debug)]
struct Tx {
    system_id: u32,
    call_id: u32,
    rank: Rank,
    source: String,
    start: f64,
    start_key: u32,
    packet_id: u16,
    last_audio: Instant,
}

/// One IaR channel's sending state.
pub struct Lane {
    pub channel: Channel,
    random_id: u16,
    holder: Option<Tx>,
    encoder: Option<Encoder>,
    pending: Vec<i16>,
}

impl Lane {
    pub fn new(channel: Channel, random_id: u16) -> Lane {
        Lane { channel, random_id, holder: None, encoder: None, pending: Vec::new() }
    }

    #[cfg(test)]
    pub fn is_transmitting(&self) -> bool {
        self.holder.is_some()
    }

    /// Feed one audio chunk of a call that the channel takes. `source` names the talker (and the
    /// talkgroup, when several are configured); `unix` is now, in Unix seconds.
    pub fn feed(&mut self, now: Instant, unix: f64, cand: Candidate, source: &str, rate: u32, samples: &[i16]) -> Result<Vec<Cmd>, String> {
        let mut out = Vec::new();
        if samples.is_empty() {
            return Ok(out);
        }
        if self.encoder.as_ref().is_none_or(|e| e.rate() != rate) {
            out.extend(self.end(unix));
            self.encoder = Some(Encoder::new(rate)?);
        }
        let holder = self.holder.as_ref().map(|t| Holder { system_id: t.system_id, call_id: t.call_id, rank: t.rank, last_audio: t.last_audio });
        match decide(self.channel.scan_mode, holder.as_ref(), cand, now) {
            Decision::Ignore => return Ok(out),
            Decision::Continue => {
                // A new talker on the same call starts a new transmission, as IaR-Stream does. An
                // unknown talker doesn't end the current one.
                let changed = self.holder.as_ref().is_some_and(|t| !source.is_empty() && t.source != source);
                if changed {
                    out.extend(self.end(unix));
                    out.extend(self.begin(unix, cand, source));
                }
            }
            Decision::Start { .. } => {
                out.extend(self.end(unix));
                out.extend(self.begin(unix, cand, source));
            }
        }
        if let Some(t) = self.holder.as_mut() {
            t.last_audio = now;
        }
        out.extend(self.push(samples)?);
        Ok(out)
    }

    /// The end of a call: its transmission ends, here or on another site's copy's behalf.
    pub fn end_call(&mut self, system_id: u32, call_id: u32, unix: f64) -> Vec<Cmd> {
        match &self.holder {
            Some(t) if t.system_id == system_id && t.call_id == call_id => self.end(unix),
            _ => Vec::new(),
        }
    }

    /// A transmission that has gone quiet for [`TX_IDLE_END`] ends.
    pub fn tick(&mut self, now: Instant, unix: f64) -> Vec<Cmd> {
        match &self.holder {
            Some(t) if now.saturating_duration_since(t.last_audio) > TX_IDLE_END => self.end(unix),
            _ => Vec::new(),
        }
    }

    /// Ends whatever is transmitting, as the channel's connection has been replaced.
    pub fn reset(&mut self, unix: f64) -> Vec<Cmd> {
        self.end(unix)
    }

    fn begin(&mut self, unix: f64, cand: Candidate, source: &str) -> Vec<Cmd> {
        let key = start_key(unix);
        self.holder = Some(Tx {
            system_id: cand.system_id,
            call_id: cand.call_id,
            rank: cand.rank,
            source: source.to_string(),
            start: unix,
            start_key: key,
            packet_id: 0,
            last_audio: Instant::now(),
        });
        vec![Cmd::Control(tx_json("tx_start", &self.message(unix, source, None)))]
    }

    /// Frames whole audio packets; the rest waits for the next chunk.
    fn push(&mut self, samples: &[i16]) -> Result<Vec<Cmd>, String> {
        let mut out = Vec::new();
        let Some(encoder) = self.encoder.as_mut() else { return Ok(out) };
        let Some(tx) = self.holder.as_mut() else { return Ok(out) };
        let n = frame_len(encoder.rate());
        self.pending.extend_from_slice(samples);
        while self.pending.len() >= n {
            let frame: Vec<i16> = self.pending.drain(..n).collect();
            let opus = encoder.encode(&frame)?;
            out.push(Cmd::Audio(packet(0, self.random_id, tx.start_key, tx.packet_id, &opus)));
            tx.packet_id = tx.packet_id.wrapping_add(1);
        }
        Ok(out)
    }

    /// Ends the transmission: the last partial frame is padded with silence and sent, then `tx_end`.
    fn end(&mut self, unix: f64) -> Vec<Cmd> {
        let Some(tx) = self.holder.take() else { return Vec::new() };
        let mut out = Vec::new();
        if let Some(encoder) = self.encoder.as_mut() {
            if !self.pending.is_empty() {
                self.pending.resize(frame_len(encoder.rate()), 0);
                let frame = std::mem::take(&mut self.pending);
                if let Ok(opus) = encoder.encode(&frame) {
                    out.push(Cmd::Audio(packet(0, self.random_id, tx.start_key, tx.packet_id, &opus)));
                }
            }
            encoder.reset();
        }
        self.pending.clear();
        out.push(Cmd::Control(tx_json("tx_end", &self.message(unix, &tx.source, Some(unix)).with_start(tx.start, tx.start_key))));
        out
    }

    fn message<'a>(&'a self, unix: f64, source: &'a str, end: Option<f64>) -> TxMessage<'a> {
        TxMessage { channel_id: &self.channel.id, random_id: self.random_id, source, start: unix, start_key: start_key(unix), end }
    }
}

impl<'a> TxMessage<'a> {
    /// `tx_end` names when the transmission began, not when it ends.
    fn with_start(mut self, start: f64, start_key: u32) -> TxMessage<'a> {
        self.start = start;
        self.start_key = start_key;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ChannelConfig, SourceRule};

    fn channel(mode: ScanMode, rules: Vec<(Vec<&str>, Vec<&str>)>) -> Channel {
        let sources = rules
            .into_iter()
            .map(|(systems, tgs)| SourceRule {
                systems: systems.into_iter().map(String::from).collect(),
                talkgroups: tgs.into_iter().map(String::from).collect(),
            })
            .collect();
        let c = ChannelConfig { id: "42".into(), name: "Dispatch".into(), enabled: true, scan_mode: mode, sources, ..Default::default() };
        Channel::compile(&c).unwrap()
    }

    fn cand(call_id: u32, rank: Rank) -> Candidate {
        Candidate { system_id: 9, call_id, rank }
    }

    fn holder(call_id: u32, rank: Rank, last_audio: Instant) -> Holder {
        Holder { system_id: 9, call_id, rank, last_audio }
    }

    #[test]
    fn scan_holds_the_channel_until_the_holder_goes_quiet() {
        let now = Instant::now();
        let h = holder(1, (0, 0), now);
        assert_eq!(decide(ScanMode::Scan, None, cand(2, (0, 1)), now), Decision::Start { preempt: false });
        assert_eq!(decide(ScanMode::Scan, Some(&h), cand(1, (0, 0)), now), Decision::Continue);
        assert_eq!(decide(ScanMode::Scan, Some(&h), cand(2, (0, 0)), now), Decision::Ignore, "a second call waits");
        assert_eq!(decide(ScanMode::Scan, Some(&h), cand(2, (0, 0)), now + Duration::from_millis(600)), Decision::Start { preempt: true });
    }

    #[test]
    fn scan_never_preempts_for_priority() {
        let now = Instant::now();
        let h = holder(1, (1, 0), now);
        assert_eq!(decide(ScanMode::Scan, Some(&h), cand(2, (0, 0)), now), Decision::Ignore);
    }

    #[test]
    fn priority_scan_preempts_for_a_higher_rank_only() {
        let now = Instant::now();
        let h = holder(1, (1, 2), now);
        assert_eq!(decide(ScanMode::PriorityScan, Some(&h), cand(2, (0, 5)), now), Decision::Start { preempt: true }, "rule 0 beats rule 1");
        assert_eq!(decide(ScanMode::PriorityScan, Some(&h), cand(2, (1, 1)), now), Decision::Start { preempt: true }, "earlier entry beats later");
        assert_eq!(decide(ScanMode::PriorityScan, Some(&h), cand(2, (1, 2)), now), Decision::Ignore, "same rank waits");
        assert_eq!(decide(ScanMode::PriorityScan, Some(&h), cand(2, (1, 3)), now), Decision::Ignore, "lower rank waits");
    }

    #[test]
    fn copies_of_one_call_on_several_sites_share_the_holder() {
        let now = Instant::now();
        let h = holder(1, (0, 0), now);
        let other_system = Candidate { system_id: 8, call_id: 1, rank: (0, 0) };
        assert_eq!(decide(ScanMode::Scan, Some(&h), other_system, now), Decision::Ignore, "same number, different system: a different call");
        assert_eq!(decide(ScanMode::PriorityScan, Some(&h), cand(1, (0, 0)), now), Decision::Continue);
    }

    #[test]
    fn frames_are_60ms_of_the_rate() {
        assert_eq!(frame_len(8000), 480);
        assert_eq!(frame_len(16000), 960);
    }

    #[test]
    fn start_keys_are_tenths_of_a_second_mod_2_32() {
        assert_eq!(start_key(1_700_000_000.0), 17_000_000_000u64 as u32);
        assert_eq!(start_key(1_700_000_000.25), 17_000_000_002u64 as u32);
    }

    #[test]
    fn a_packet_has_the_fourteen_byte_header_big_endian() {
        let p = packet(0x0102_0304, 0xBEEF, 0xAABB_CCDD, 7, &[9, 8, 7]);
        assert_eq!(&p[0..2], &[0, 1], "type 1");
        assert_eq!(&p[2..6], &[1, 2, 3, 4], "sender");
        assert_eq!(&p[6..8], &[0xBE, 0xEF], "random id");
        assert_eq!(&p[8..12], &[0xAA, 0xBB, 0xCC, 0xDD], "start key");
        assert_eq!(&p[12..14], &[0, 7], "packet number");
        assert_eq!(&p[14..], &[9, 8, 7], "the frame");
        assert_eq!(p.len(), 17);
    }

    #[test]
    fn tx_messages_carry_what_iar_expects() {
        let start = tx_json("tx_start", &TxMessage { channel_id: "42", random_id: 5, source: "Engine 1", start: 1000.5, start_key: 10005, end: None });
        let v: serde_json::Value = serde_json::from_str(&start).unwrap();
        assert_eq!(v["action"], "tx_start");
        assert_eq!((v["v"].as_u64(), v["channelId"].as_str(), v["sourceName"].as_str()), (Some(1), Some("42"), Some("Engine 1")));
        assert_eq!(
            (v["sendingMemberId"].as_u64(), v["clientRandomId"].as_u64(), v["priority"].as_u64(), v["type"].as_u64()),
            (Some(0), Some(5), Some(3), Some(1))
        );
        assert_eq!((v["clientSwName"].as_str(), v["startTimeKey"].as_u64()), (Some(CLIENT_NAME), Some(10005)));
        assert!(v.get("endTime").is_none());
        let end = tx_json(
            "tx_end",
            &TxMessage { end: Some(1003.0), ..TxMessage { channel_id: "42", random_id: 5, source: "Engine 1", start: 1000.5, start_key: 10005, end: None } },
        );
        let v: serde_json::Value = serde_json::from_str(&end).unwrap();
        assert_eq!((v["action"].as_str(), v["endTime"].as_f64()), (Some("tx_end"), Some(1003.0)));
    }

    /// Makes a lane's audio from `samples` of 8 kHz audio at `now`.
    fn chunk(n: usize) -> Vec<i16> {
        (0..n).map(|i| ((i as f64 * 0.2).sin() * 6000.0) as i16).collect()
    }

    fn controls(cmds: &[Cmd]) -> Vec<String> {
        cmds.iter()
            .filter_map(|c| {
                if let Cmd::Control(s) = c { Some(serde_json::from_str::<serde_json::Value>(s).unwrap()["action"].as_str().unwrap().to_string()) } else { None }
            })
            .collect()
    }

    fn audio(cmds: &[Cmd]) -> Vec<&Vec<u8>> {
        cmds.iter().filter_map(|c| if let Cmd::Audio(p) = c { Some(p) } else { None }).collect()
    }

    #[test]
    fn audio_becomes_start_packets_and_end() {
        let mut lane = Lane::new(channel(ScanMode::Scan, vec![(vec![], vec!["101"])]), 77);
        let now = Instant::now();
        let unix = 1_700_000_000.0;
        // 1.5 frames: one packet now, the rest waits.
        let cmds = lane.feed(now, unix, cand(1, (0, 0)), "Engine 1", 8000, &chunk(720)).unwrap();
        assert_eq!(controls(&cmds), ["tx_start"]);
        assert_eq!(audio(&cmds).len(), 1);
        let p = audio(&cmds)[0];
        assert_eq!((p[0..2].to_vec(), p[6..8].to_vec(), p[12..14].to_vec()), (vec![0, 1], vec![0, 77], vec![0, 0]));
        assert_eq!(p[8..12].to_vec(), start_key(unix).to_be_bytes().to_vec());
        // The rest of the frame, and then the call ends: the remainder is padded, sent, then tx_end.
        let cmds = lane.feed(now, unix, cand(1, (0, 0)), "Engine 1", 8000, &chunk(240)).unwrap();
        assert_eq!((controls(&cmds).len(), audio(&cmds).len()), (0, 1));
        assert_eq!(audio(&cmds)[0][12..14], [0, 1], "packet numbers count up");
        let cmds = lane.end_call(9, 1, unix + 1.0);
        assert_eq!(controls(&cmds), ["tx_end"]);
        assert!(!lane.is_transmitting());
        assert!(lane.end_call(9, 1, unix + 1.0).is_empty(), "already ended");
    }

    #[test]
    fn a_partial_frame_is_padded_when_the_transmission_ends() {
        let mut lane = Lane::new(channel(ScanMode::Scan, vec![(vec![], vec!["101"])]), 1);
        let now = Instant::now();
        lane.feed(now, 1.0, cand(1, (0, 0)), "x", 8000, &chunk(100)).unwrap();
        let cmds = lane.reset(2.0);
        assert_eq!(audio(&cmds).len(), 1, "the 100 samples go out as one padded frame");
        assert_eq!(controls(&cmds), ["tx_end"]);
    }

    #[test]
    fn a_new_talker_restarts_the_transmission() {
        let mut lane = Lane::new(channel(ScanMode::Scan, vec![(vec![], vec!["101"])]), 1);
        let now = Instant::now();
        lane.feed(now, 1.0, cand(1, (0, 0)), "Engine 1", 8000, &chunk(480)).unwrap();
        let cmds = lane.feed(now, 2.0, cand(1, (0, 0)), "Medic 4", 8000, &chunk(480)).unwrap();
        assert_eq!(controls(&cmds), ["tx_end", "tx_start"]);
        let cmds = lane.feed(now, 2.0, cand(1, (0, 0)), "", 8000, &chunk(480)).unwrap();
        assert!(controls(&cmds).is_empty(), "an unknown talker doesn't restart it");
    }

    #[test]
    fn priority_scan_preempts_with_end_then_start() {
        let mut lane = Lane::new(channel(ScanMode::PriorityScan, vec![(vec![], vec!["1000-1099"]), (vec![], vec!["2001"])]), 1);
        let now = Instant::now();
        lane.feed(now, 1.0, Candidate { system_id: 9, call_id: 1, rank: (1, 0) }, "low", 8000, &chunk(480)).unwrap();
        let cmds = lane.feed(now, 1.1, Candidate { system_id: 9, call_id: 2, rank: (0, 0) }, "high", 8000, &chunk(480)).unwrap();
        assert_eq!(controls(&cmds), ["tx_end", "tx_start"]);
    }

    #[test]
    fn scan_drops_the_second_call_until_the_first_is_quiet() {
        let mut lane = Lane::new(channel(ScanMode::Scan, vec![(vec![], vec!["101"])]), 1);
        let t0 = Instant::now();
        lane.feed(t0, 1.0, cand(1, (0, 0)), "a", 8000, &chunk(480)).unwrap();
        assert!(lane.feed(t0, 1.0, cand(2, (0, 0)), "b", 8000, &chunk(480)).unwrap().is_empty(), "the copy is ignored");
        let cmds = lane.feed(t0 + Duration::from_secs(1), 2.0, cand(2, (0, 0)), "b", 8000, &chunk(480)).unwrap();
        assert_eq!(controls(&cmds), ["tx_end", "tx_start"], "quiet for a while: the second call takes over");
    }

    #[test]
    fn a_quiet_transmission_ends_on_tick() {
        let mut lane = Lane::new(channel(ScanMode::Scan, vec![(vec![], vec!["101"])]), 1);
        let t0 = Instant::now();
        lane.feed(t0, 1.0, cand(1, (0, 0)), "a", 8000, &chunk(480)).unwrap();
        assert!(lane.tick(t0 + Duration::from_secs(1), 1.5).is_empty());
        assert_eq!(controls(&lane.tick(t0 + Duration::from_secs(3), 3.0)), ["tx_end"]);
    }
}
