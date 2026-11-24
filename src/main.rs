//! IaR Live Audio for Trunk Recorder Pro: sends live call audio straight to IamResponding Live
//! Audio channels, with no simplestream or IaR-Stream in between.
//!
//! The plugin's settings hold the IaR API key and the channels that key can send to. Each enabled
//! channel has talkgroup rules; live audio for a matching call is framed, Opus-encoded and sent
//! over that channel's WebSocket (see `channel` and `ws`).

mod channel;
mod config;
mod iar_api;
mod opus;
mod ws;

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, Hasher};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::json;
use trunk_recorder_plugin::{
    Action, AudioChunk, CallInfo, Endpoint, EndpointState, Host, Manifest, Metrics, NoConfig, Page, Plugin, Reply, Setup, State, Status, topic,
};

use channel::{Candidate, Cmd, Lane};
use config::{Channel, ChannelConfig, Config, compile_all, fill_sending, merge};
use iar_api::{CHANNELS_URL, WS_BASE};

/// Where the plugin talks to IaR. Production unless a test says otherwise.
#[derive(Clone, Debug)]
pub struct Endpoints {
    pub channels_url: String,
    pub ws_base: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Endpoints { channels_url: CHANNELS_URL.into(), ws_base: WS_BASE.into() }
    }
}

/// One enabled channel: its lane (what it's sending) and its connection.
struct Route {
    lane: Lane,
    link: ws::Link,
    seen_losses: u64,
}

/// What the plugin knows of a call, from its `call.start` and `call.update`.
struct Call {
    talkgroup: u32,
    talkgroup_tag: String,
    patched: Vec<String>,
    unit: u32,
    unit_tag: String,
}

impl Call {
    fn of(c: &CallInfo) -> Call {
        let last = c.sources.last();
        Call {
            talkgroup: c.talkgroup,
            talkgroup_tag: c.talkgroup_tag.clone(),
            patched: c.patched.iter().map(|p| if p.alpha_tag.is_empty() { p.talkgroup.to_string() } else { p.alpha_tag.clone() }).collect(),
            unit: last.map_or(0, |s| s.unit),
            unit_tag: last.map_or_else(String::new, |s| s.tag.clone()),
        }
    }
}

pub struct IarLiveAudio {
    host: Host,
    endpoints: Endpoints,
    /// The settings last applied.
    config: Config,
    routes: Vec<Route>,
    calls: HashMap<u32, Call>,
    /// The channel IDs IaR listed at the last successful listing. `None` until one arrives.
    listed: Option<HashSet<String>>,
    /// The key the channel list was last fetched for, so a settings save doesn't fetch it again.
    discovered_key: String,
    random_id: u16,
    found_tx: Sender<Vec<String>>,
    found_rx: Receiver<Vec<String>>,
    warned: HashSet<String>,
}

impl IarLiveAudio {
    fn with_endpoints(host: Host, config: Config, endpoints: Endpoints) -> Result<IarLiveAudio, String> {
        let (found_tx, found_rx) = mpsc::channel();
        let mut p = IarLiveAudio {
            host,
            endpoints,
            config: Config::default(),
            routes: Vec::new(),
            calls: HashMap::new(),
            listed: None,
            discovered_key: String::new(),
            random_id: random_id(),
            found_tx,
            found_rx,
            warned: HashSet::new(),
        };
        p.apply(config)?;
        Ok(p)
    }

    /// Takes new settings. Nothing changes if they don't check out.
    fn apply(&mut self, config: Config) -> Result<(), String> {
        compile_all(&config)?;
        let key_changed = config.api_key != self.config.api_key;
        self.config = config;
        // Each card's summary line comes from its rules. Saving it makes the recorder send the settings
        // back, and then nothing has changed, so that is the end of it.
        if fill_sending(&mut self.config.channels) {
            self.host.save_settings(&json!({ "channels": self.config.channels }));
        }
        self.sync_routes(key_changed);
        if !self.config.api_key.is_empty() && self.config.api_key != self.discovered_key {
            let (key, saved) = (self.config.api_key.clone(), self.config.channels.clone());
            self.discover(key, saved, false, None);
        }
        Ok(())
    }

    /// Makes the routes match the settings: one per channel that is on, has a key, and is listed.
    /// A channel whose link is kept keeps any transmission in progress.
    fn sync_routes(&mut self, rebuild_links: bool) {
        let channels = compile_all(&self.config).unwrap_or_default();
        let key = self.config.api_key.clone();
        let mut old: HashMap<String, Route> = self.routes.drain(..).map(|r| (r.lane.channel.id.clone(), r)).collect();
        let mut routes = Vec::new();
        for ch in channels {
            let wanted = ch.enabled && !key.is_empty() && self.listed.as_ref().is_none_or(|l| l.contains(&ch.id));
            match old.remove(&ch.id) {
                Some(mut r) if wanted && !rebuild_links => {
                    r.lane.channel = ch;
                    routes.push(r);
                }
                previous => {
                    if let Some(r) = previous {
                        stop(r);
                    }
                    if wanted {
                        let link = ws::Link::start(&self.endpoints.ws_base, &ch.id, &key);
                        routes.push(Route { lane: Lane::new(ch, self.random_id), link, seen_losses: 0 });
                    }
                }
            }
        }
        for (_, r) in old {
            stop(r);
        }
        self.routes = routes;
    }

    /// Fetches the channels for `api_key` on a thread. The listing is merged into `saved` and saved
    /// to the settings, and `reply` (if any) is answered with how it went.
    fn discover(&mut self, api_key: String, saved: Vec<ChannelConfig>, save_key: bool, reply: Option<Reply>) {
        self.discovered_key = api_key.clone();
        let (host, url, found) = (self.host.clone(), self.endpoints.channels_url.clone(), self.found_tx.clone());
        thread::spawn(move || match iar_api::fetch(&url, &api_key) {
            Ok(listed) => {
                let mut merged = merge(&saved, &listed);
                fill_sending(&mut merged.channels);
                let missing = merged.missing.len();
                let mut settings = json!({ "channels": merged.channels });
                if save_key {
                    settings["apiKey"] = json!(api_key);
                }
                host.save_settings(&settings);
                host.info(format!("IaR Live Audio: this key can send to {} channel(s)", listed.len()));
                if missing > 0 {
                    host.warn(format!("IaR Live Audio no longer lists {missing} saved channel(s): their rules are kept, but nothing is sent to them"));
                }
                let _ = found.send(listed.iter().map(|c| c.id.clone()).collect());
                if let Some(r) = reply {
                    r.ok(format!("{} channel(s)", listed.len()));
                }
            }
            Err(e) => {
                host.warn(format!("IaR Live Audio channels: {e}"));
                if let Some(r) = reply {
                    r.fail(e);
                }
            }
        });
    }

    /// Takes in a channel listing that finished since the last call.
    fn poll(&mut self) {
        while let Ok(ids) = self.found_rx.try_recv() {
            let ids: HashSet<String> = ids.into_iter().collect();
            for c in &self.config.channels {
                if c.enabled && !ids.contains(&c.id) && self.warned.insert(c.id.clone()) {
                    self.host.warn(format!("IaR Live Audio doesn't list channel {} for this key: nothing is sent to it", c.id));
                }
            }
            self.listed = Some(ids);
            self.sync_routes(false);
        }
    }

    /// A lost connection ends the transmission it was carrying, as the server no longer has it.
    fn check_connections(&mut self) {
        let unix = unix_now();
        for r in &mut self.routes {
            let losses = r.link.stats().losses;
            if losses != r.seen_losses {
                r.seen_losses = losses;
                let cmds = r.lane.reset(unix);
                deliver(r, cmds);
            }
        }
    }

    /// One status line for the interface, and the endpoint's health for the dashboard.
    fn report(&self) {
        if self.config.api_key.trim().is_empty() {
            self.host.status(State::Warning, "Enter your IaR Live Audio API key, then Refresh channels");
            return;
        }
        if self.routes.is_empty() {
            self.host.status(State::Ok, "No channels are on. Turn one on and add a talkgroup rule.");
            return;
        }
        let mut up = 0;
        let mut problems = Vec::new();
        for r in &self.routes {
            let st = r.link.stats();
            if st.connected {
                up += 1;
            } else if !st.last_error.is_empty() {
                problems.push(format!("{}: {}", label(&r.lane.channel), st.last_error));
            }
        }
        let total = self.routes.len();
        let (state, message) =
            if problems.is_empty() { (State::Ok, format!("{up} of {total} channel(s) connected")) } else { (State::Warning, problems.join("; ")) };
        self.host.status(state, message.clone());
        let endpoint_state = match (up, problems.is_empty()) {
            (n, true) if n == total => EndpointState::Up,
            (0, _) => EndpointState::Down,
            _ => EndpointState::Degraded,
        };
        self.host.metrics(&Metrics {
            endpoints: vec![Endpoint {
                name: "IaR Live Audio".into(),
                state: endpoint_state,
                last_error: problems.first().cloned().unwrap_or_default(),
                ..Default::default()
            }],
            ..Default::default()
        });
    }
}

impl Plugin for IarLiveAudio {
    type Config = Config;
    type SystemConfig = NoConfig;

    fn manifest() -> Manifest {
        Manifest {
            name: "IaR Live Audio".into(),
            subscribe: [topic::CALL_START, topic::CALL_UPDATE, topic::CALL_END, topic::AUDIO, topic::STATUS].iter().map(|t| t.to_string()).collect(),
            live_settings: true,
            page: Some(Page { title: "IaR Live Audio".into() }),
            ..trunk_recorder_plugin::manifest!()
        }
    }

    fn start(host: Host, setup: Setup<Config, NoConfig>) -> Result<Self, String> {
        IarLiveAudio::with_endpoints(host, setup.config, Endpoints::default())
    }

    fn call_start(&mut self, call: CallInfo) {
        self.poll();
        self.calls.insert(call.id, Call::of(&call));
    }

    fn call_update(&mut self, call: CallInfo) {
        self.poll();
        self.calls.insert(call.id, Call::of(&call));
    }

    fn call_end(&mut self, call: CallInfo) {
        self.poll();
        let unix = unix_now();
        for r in &mut self.routes {
            let cmds = r.lane.end_call(call.system_id, call.id, unix);
            deliver(r, cmds);
        }
        self.calls.remove(&call.id);
    }

    fn audio(&mut self, chunk: AudioChunk) {
        self.poll();
        self.check_connections();
        let samples = chunk.samples();
        if samples.is_empty() || chunk.sample_rate == 0 {
            return;
        }
        let (now, unix) = (Instant::now(), unix_now());
        let call = self.calls.get(&chunk.call_id);
        for r in &mut self.routes {
            let Some(rank) = r.lane.channel.rank(&chunk.short_name, chunk.talkgroup) else { continue };
            let cand = Candidate { system_id: chunk.system_id, call_id: chunk.call_id, rank };
            let source = source_label(call, chunk.talkgroup, r.lane.channel.multi());
            match r.lane.feed(now, unix, cand, &source, chunk.sample_rate, &samples) {
                Ok(cmds) => deliver(r, cmds),
                Err(e) => self.host.warn(format!("{}: {e}", label(&r.lane.channel))),
            }
        }
    }

    fn status(&mut self, _status: Status) {
        self.poll();
        let (now, unix) = (Instant::now(), unix_now());
        for r in &mut self.routes {
            let cmds = r.lane.tick(now, unix);
            deliver(r, cmds);
        }
        self.report();
    }

    fn settings(&mut self, setup: Setup<Config, NoConfig>) -> Result<(), String> {
        self.poll();
        self.apply(setup.config)
    }

    fn action(&mut self, a: Action, reply: Reply) {
        if a.action != "refresh" {
            return reply.fail("This plugin has no such action");
        }
        let edited: Config = match serde_json::from_value(a.value) {
            Ok(c) => c,
            Err(e) => return reply.fail(format!("those settings don't read ({e})")),
        };
        if edited.api_key.trim().is_empty() {
            return reply.fail("Enter your IaR Live Audio API key first");
        }
        let saved = edited.channels.clone();
        self.discover(edited.api_key, saved, true, Some(reply));
    }

    fn shutdown(&mut self, grace: Duration) {
        let unix = unix_now();
        let mut sent = false;
        for r in &mut self.routes {
            let cmds = r.lane.reset(unix);
            sent |= !cmds.is_empty();
            deliver(r, cmds);
        }
        if sent {
            // Give the connections a moment to send the transmissions' ends.
            thread::sleep(grace.min(Duration::from_millis(500)));
        }
        self.routes.clear();
    }
}

fn deliver(r: &Route, cmds: Vec<Cmd>) {
    for cmd in cmds {
        r.link.send(cmd);
    }
}

/// Ends a route's transmission before its connection is dropped.
fn stop(mut r: Route) {
    let cmds = r.lane.reset(unix_now());
    deliver(&r, cmds);
}

/// A channel's name for messages: its IaR name, else its ID.
fn label(ch: &Channel) -> &str {
    if ch.name.is_empty() { &ch.id } else { &ch.name }
}

/// The source name IaR shows: the talker. When a channel carries several talkgroups, the talkgroup
/// comes first, as IaR-Stream's scan mode names it.
fn source_label(call: Option<&Call>, talkgroup: u32, multi: bool) -> String {
    let unit = match call {
        Some(c) if !c.unit_tag.is_empty() => c.unit_tag.clone(),
        Some(c) if c.unit != 0 => c.unit.to_string(),
        _ => String::new(),
    };
    if !multi {
        return unit;
    }
    let own = match call {
        Some(c) if !c.talkgroup_tag.is_empty() => c.talkgroup_tag.clone(),
        Some(c) => c.talkgroup.to_string(),
        None => talkgroup.to_string(),
    };
    let talkgroups = match call {
        Some(c) if !c.patched.is_empty() => std::iter::once(own).chain(c.patched.iter().cloned()).collect::<Vec<_>>().join(", "),
        _ => own,
    };
    format!("{talkgroups} {unit}").trim().to_string()
}

fn unix_now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// A random ID for this run, sent with every transmission.
fn random_id() -> u16 {
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64));
    h.finish() as u16
}

fn main() {
    trunk_recorder_plugin::run::<IarLiveAudio>();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use trunk_recorder_plugin::testing::{self, MockServer};
    use trunk_recorder_plugin::{CallSource, HostInfo, PluginMessage};
    use tungstenite::Message;

    /// A WebSocket server on localhost that forwards every message it gets.
    fn ws_server() -> (String, Receiver<Message>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("ws://{}/", listener.local_addr().expect("addr"));
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let tx = tx.clone();
                thread::spawn(move || {
                    let Ok(mut ws) = tungstenite::accept(stream) else { return };
                    while let Ok(m) = ws.read() {
                        if tx.send(m).is_err() {
                            break;
                        }
                    }
                });
            }
        });
        (base, rx)
    }

    /// Everything the server gets for the next `for_` time, in order.
    fn drain(rx: &Receiver<Message>, for_: Duration) -> Vec<Message> {
        let mut out = Vec::new();
        while let Ok(m) = rx.recv_timeout(for_) {
            out.push(m);
        }
        out
    }

    fn actions(msgs: &[Message]) -> Vec<String> {
        msgs.iter()
            .filter_map(|m| match m {
                Message::Text(t) => serde_json::from_str::<serde_json::Value>(t).ok().and_then(|v| v["action"].as_str().map(String::from)),
                _ => None,
            })
            .collect()
    }

    fn texts(msgs: &[Message]) -> Vec<serde_json::Value> {
        msgs.iter().filter_map(|m| if let Message::Text(t) = m { serde_json::from_str(t).ok() } else { None }).collect()
    }

    fn packets(msgs: &[Message]) -> Vec<Vec<u8>> {
        msgs.iter().filter_map(|m| if let Message::Binary(b) = m { Some(b.to_vec()) } else { None }).collect()
    }

    /// A channel that sends `talkgroups` on system `county` (id 1), with the given scan mode.
    fn settings_with(api_key: &str, mode: config::ScanMode, rules: &[(&[&str], &[&str])]) -> Config {
        let sources = rules
            .iter()
            .map(|(systems, tgs)| config::SourceRule {
                systems: systems.iter().map(|s| s.to_string()).collect(),
                talkgroups: tgs.iter().map(|s| s.to_string()).collect(),
            })
            .collect();
        Config {
            api_key: api_key.into(),
            channels: vec![ChannelConfig { id: "42".into(), name: "Dispatch".into(), enabled: true, scan_mode: mode, sources, ..Default::default() }],
        }
    }

    /// A plugin wired to a mock IaR channel list (`listing`) and a local WebSocket server.
    fn plugin(cfg: Config, listing: &str) -> (IarLiveAudio, Receiver<Message>, Host, testing::Captured, MockServer) {
        let (host, captured) = testing::capture();
        let api = MockServer::start({
            let listing = listing.to_string();
            move |_| (200, listing.clone())
        });
        let (ws_base, msgs) = ws_server();
        let endpoints = Endpoints { channels_url: format!("{}/api/apikey/channels", api.url()), ws_base };
        let p = IarLiveAudio::with_endpoints(host.clone(), cfg, endpoints).expect("plugin");
        (p, msgs, host, captured, api)
    }

    fn call(id: u32, tg: u32, system_id: u32, unit: Option<(u32, &str)>) -> CallInfo {
        CallInfo {
            id,
            system: 0,
            system_id,
            short_name: "county".into(),
            talkgroup: tg,
            talkgroup_tag: format!("TG {tg}"),
            sources: unit.map(|(u, tag)| CallSource { unit: u, tag: tag.into(), ..Default::default() }).into_iter().collect(),
            ..Default::default()
        }
    }

    fn audio(call_id: u32, tg: u32, system_id: u32, samples: usize) -> AudioChunk {
        audio_from("county", call_id, tg, system_id, samples)
    }

    fn audio_from(short_name: &str, call_id: u32, tg: u32, system_id: u32, samples: usize) -> AudioChunk {
        let pcm: Vec<i16> = (0..samples).map(|i| ((i as f64 * 0.2).sin() * 5000.0) as i16).collect();
        AudioChunk { system_id, short_name: short_name.into(), ..AudioChunk::new(call_id, 0, tg, 8000, &pcm) }
    }

    const LISTING: &str = r#"[{"id":"42","name":"Dispatch","description":"Main"}]"#;

    #[test]
    fn a_call_on_a_rule_is_sent_as_tx_start_packets_and_tx_end() {
        let (mut p, ws, _host, _cap, _api) = plugin(settings_with("k", config::ScanMode::Scan, &[(&[], &["101"])]), LISTING);
        p.call_start(call(7, 101, 1, Some((1234, "Engine 1"))));
        p.audio(audio(7, 101, 1, 960));
        p.call_end(call(7, 101, 1, None));
        let got = drain(&ws, Duration::from_millis(800));

        assert_eq!(actions(&got), ["login", "tx_start", "tx_end"]);
        let login = texts(&got)[0].clone();
        assert_eq!(login["api_key"], "k", "the login carries the key");
        let start = texts(&got)[1].clone();
        assert_eq!((start["channelId"].as_str(), start["sourceName"].as_str()), (Some("42"), Some("Engine 1")));

        // 960 samples at 8 kHz are two 60 ms frames: two packets, numbered from 0.
        let pk = packets(&got);
        assert_eq!(pk.len(), 2, "two frames of 60 ms");
        assert_eq!(&pk[0][0..2], &[0, 1]);
        assert_eq!(&pk[0][8..12], &(start["startTimeKey"].as_u64().unwrap() as u32).to_be_bytes(), "the packets name the transmission");
        assert_eq!((&pk[0][12..14], &pk[1][12..14]), (&[0u8, 0][..], &[0u8, 1][..]));
        assert_eq!(&pk[0][6..8], &p.random_id.to_be_bytes(), "the random id is the plugin's");
    }

    #[test]
    fn a_second_call_on_the_channel_waits_for_the_first_to_be_quiet() {
        let (mut p, ws, _host, _cap, _api) = plugin(settings_with("k", config::ScanMode::Scan, &[(&["county"], &["101"])]), LISTING);
        p.call_start(call(7, 101, 1, Some((1, "A"))));
        p.audio(audio(7, 101, 1, 480));
        // A copy of the same call on another site (same system), and the same talkgroup on another system.
        p.call_start(call(8, 101, 1, Some((2, "B"))));
        p.audio(audio(8, 101, 1, 480));
        p.call_start(call(9, 101, 2, Some((3, "C"))));
        p.audio(audio_from("other", 9, 101, 2, 480));
        p.call_end(call(7, 101, 1, None));
        let got = drain(&ws, Duration::from_millis(800));
        assert_eq!(actions(&got), ["login", "tx_start", "tx_end"], "one transmission: the copy and the other system are dropped");
    }

    #[test]
    fn priority_scan_preempts_for_a_higher_rule_and_scan_does_not() {
        let rules: &[(&[&str], &[&str])] = &[(&[], &["1000-1099"]), (&[], &["2001"])];

        let (mut p, ws, _h, _c, _a) = plugin(settings_with("k", config::ScanMode::PriorityScan, rules), LISTING);
        p.call_start(call(1, 2001, 1, Some((1, "Low"))));
        p.audio(audio(1, 2001, 1, 480));
        p.call_start(call(2, 1050, 1, Some((2, "High"))));
        p.audio(audio(2, 1050, 1, 480));
        p.call_end(call(1, 2001, 1, None));
        p.call_end(call(2, 1050, 1, None));
        let got = drain(&ws, Duration::from_millis(800));
        assert_eq!(actions(&got), ["login", "tx_start", "tx_end", "tx_start", "tx_end"]);
        // Two entries means several talkgroups are configured, so the source names the talkgroup too.
        assert_eq!(texts(&got)[3]["sourceName"], "TG 1050 High");

        let (mut p, ws, _h, _c, _a) = plugin(settings_with("k", config::ScanMode::Scan, rules), LISTING);
        p.call_start(call(1, 2001, 1, Some((1, "Low"))));
        p.audio(audio(1, 2001, 1, 480));
        p.call_start(call(2, 1050, 1, Some((2, "High"))));
        p.audio(audio(2, 1050, 1, 480));
        p.call_end(call(1, 2001, 1, None));
        let got = drain(&ws, Duration::from_millis(800));
        assert_eq!(actions(&got), ["login", "tx_start", "tx_end"], "scan holds: the second talkgroup waits");
    }

    #[test]
    fn several_talkgroups_name_the_talkgroup_in_the_source() {
        let (mut p, ws, _h, _c, _a) = plugin(settings_with("k", config::ScanMode::Scan, &[(&[], &["101", "102"])]), LISTING);
        p.call_start(call(1, 101, 1, Some((1234, "Engine 1"))));
        p.audio(audio(1, 101, 1, 480));
        p.call_end(call(1, 101, 1, None));
        let got = drain(&ws, Duration::from_millis(800));
        assert_eq!(texts(&got)[1]["sourceName"], "TG 101 Engine 1");
    }

    #[test]
    fn a_channel_that_is_off_or_has_no_rule_sends_nothing() {
        let mut cfg = settings_with("k", config::ScanMode::Scan, &[(&[], &["101"])]);
        cfg.channels[0].enabled = false;
        let (mut p, ws, _h, _c, _a) = plugin(cfg, LISTING);
        p.call_start(call(1, 101, 1, Some((1, "A"))));
        p.audio(audio(1, 101, 1, 480));
        p.call_end(call(1, 101, 1, None));
        assert!(drain(&ws, Duration::from_millis(300)).is_empty(), "off: nothing is sent");
    }

    /// Every settings save the plugin has made so far.
    fn saves(cap: &testing::Captured) -> Vec<serde_json::Value> {
        cap.output()
            .messages
            .iter()
            .filter_map(|m| match m {
                PluginMessage::SettingsSave { settings, .. } => Some(serde_json::Value::Object(settings.clone())),
                _ => None,
            })
            .collect()
    }

    /// Waits for the save the channel listing makes (the one carrying IaR's description) and returns it.
    fn listing_save(p: &mut IarLiveAudio, cap: &testing::Captured) -> serde_json::Value {
        for _ in 0..100 {
            p.poll();
            if let Some(s) = saves(cap).into_iter().find(|s| s["channels"][0]["description"] == "Main") {
                return s;
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("the channel listing was never saved");
    }

    #[test]
    fn entering_a_key_fetches_the_channels_and_keeps_the_rules() {
        let saved = settings_with("", config::ScanMode::Scan, &[(&[], &["101"])]);
        let (mut p, _ws, _h, cap, api) = plugin(saved.clone(), LISTING);
        p.settings(Setup { config: Config { api_key: "k".into(), ..saved }, ..setup_for_tests() }).expect("settings");
        let s = listing_save(&mut p, &cap);
        assert_eq!(s["channels"][0]["id"], "42");
        assert_eq!(s["channels"][0]["name"], "Dispatch");
        assert_eq!(s["channels"][0]["enabled"], true, "a channel that was set up stays on");
        assert_eq!(s["channels"][0]["sources"][0]["talkgroups"], json!(["101"]), "and keeps its rules");
        assert_eq!(s["channels"][0]["sending"], "Scan · 101");
        assert_eq!(api.requests()[0].header("x-api-key"), Some("k"));
    }

    #[test]
    fn a_channel_found_for_the_first_time_starts_off_with_nothing_to_send() {
        let (mut p, _ws, _h, cap, _api) = plugin(Config::default(), LISTING);
        p.settings(Setup { config: Config { api_key: "k".into(), ..Default::default() }, ..setup_for_tests() }).expect("settings");
        let s = listing_save(&mut p, &cap);
        assert_eq!(s["channels"][0]["enabled"], false, "a new channel starts off");
        assert_eq!(s["channels"][0]["sending"], "Scan · no talkgroups yet");
    }

    #[test]
    fn each_cards_summary_is_saved_when_the_rules_change_and_then_left_alone() {
        let (mut p, _ws, _h, cap, _api) = plugin(settings_with("", config::ScanMode::Scan, &[(&[], &["101", "102"])]), LISTING);
        let first = saves(&cap);
        assert_eq!(first.len(), 1, "the summary is filled in and saved at the start");
        assert_eq!(first[0]["channels"][0]["sending"], "Scan · 101, 102");

        // The recorder sends the saved settings back: they already say so, so nothing is saved again.
        let mut edited = p.config.clone();
        p.apply(edited.clone()).expect("same settings");
        assert_eq!(saves(&cap).len(), 1, "no save, so no loop");

        // The rules change, and the line follows them.
        edited.channels[0].scan_mode = config::ScanMode::PriorityScan;
        edited.channels[0].sources[0].talkgroups = vec!["102".into(), "101".into()];
        p.apply(edited).expect("edited settings");
        let all = saves(&cap);
        assert_eq!(all.len(), 2);
        assert_eq!(all[1]["channels"][0]["sending"], "Priority scan · 102 > 101");
    }

    #[test]
    fn settings_that_dont_check_out_keep_the_old_ones() {
        let (mut p, _ws, _h, _c, _a) = plugin(settings_with("k", config::ScanMode::Scan, &[(&[], &["101"])]), LISTING);
        let mut bad = settings_with("k", config::ScanMode::Scan, &[(&[], &["10x"])]);
        bad.channels[0].enabled = true;
        let err = p.apply(bad).unwrap_err();
        assert!(err.contains("10x"), "{err}");
        assert_eq!(p.config.channels[0].sources[0].talkgroups, ["101"], "the rules are the old ones");
    }

    fn setup_for_tests() -> Setup<Config, NoConfig> {
        Setup {
            config: Config::default(),
            systems: Vec::new(),
            host: HostInfo { name: "test".into(), version: "0".into() },
            capture_dir: std::env::temp_dir(),
            data_dir: std::env::temp_dir(),
            audio_formats: vec!["wav".into()],
        }
    }

    #[test]
    fn the_manifest_is_live_and_has_its_page_and_topics() {
        let m = IarLiveAudio::manifest();
        assert!(m.live_settings);
        assert_eq!(m.page.as_ref().map(|p| p.title.as_str()), Some("IaR Live Audio"));
        for t in [topic::CALL_START, topic::CALL_UPDATE, topic::CALL_END, topic::AUDIO, topic::STATUS] {
            assert!(m.subscribes(t), "{t}");
        }
        assert_eq!(m.id, "iarliveaudio");
    }
}
