//! The plugin's settings: what the form shows, how each channel's rules are checked, and how
//! IaR's channel list is merged in without losing the rules already set.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use trunk_recorder_plugin::filter::{patterns, talkgroup_list, talkgroup_matches};

use crate::iar_api::IarChannel;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", default)]
#[schemars(extend("x-actions" = [{"id": "refresh", "label": "Refresh channels", "hint": "Fetch the channels this API key can use from IaR Live Audio"}]))]
pub struct Config {
    /// IaR Live Audio API key
    ///
    /// Your key from IaR Live Audio. It's kept as a secret.
    #[schemars(extend("x-secret" = true))]
    pub api_key: String,
    /// Channels
    ///
    /// The IaR Live Audio channels this key can send to. Turn one on and add the talkgroups it should carry.
    pub channels: Vec<ChannelConfig>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", default)]
#[schemars(extend("x-title" = "name", "x-summary" = ["{sending|Not set up yet}"]))]
pub struct ChannelConfig {
    /// Name
    #[schemars(extend("readOnly" = true))]
    pub name: String,
    /// Channel ID
    #[schemars(extend("readOnly" = true))]
    pub id: String,
    /// Description
    #[schemars(extend("readOnly" = true))]
    pub description: String,
    /// Send audio
    ///
    /// Off: nothing is sent to this channel.
    pub enabled: bool,
    /// Scan mode
    ///
    /// What happens when two of this channel's talkgroups are on the air at once. Scan: the one that started first keeps the channel until its call ends, and the other is skipped. Priority scan: a talkgroup with higher priority takes the channel from a lower one that is already on it, and a lower one never interrupts a higher one. Priority comes from the order of the talkgroup rules below.
    pub scan_mode: ScanMode,
    /// Talkgroup rules
    ///
    /// Each item names the systems and talkgroups to send. In Priority scan, priority runs from the top: inside one item, a talkgroup earlier in its list outranks a later one; across items, every talkgroup in an earlier item outranks every talkgroup in a later item. A talkgroup that is listed more than once counts at its first place. Scan mode ignores the order.
    pub sources: Vec<SourceRule>,
    /// Sending
    ///
    /// The scan mode and the talkgroups this channel carries, highest priority first (">" means "outranks"). The plugin keeps this up to date.
    #[schemars(extend("readOnly" = true))]
    pub sending: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScanMode {
    /// Scan: the first talkgroup to talk holds the channel until its call ends
    #[default]
    Scan,
    /// Priority scan: a higher-priority talkgroup preempts one already on the channel
    PriorityScan,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct SourceRule {
    /// Systems
    ///
    /// Leave empty for every system.
    #[schemars(extend("x-systems" = true))]
    pub systems: Vec<String>,
    /// Talkgroups
    ///
    /// Numbers (101), ranges (1000-1099), or patterns (507*, where * is any digits and ? is one digit). 0 is every talkgroup. In Priority scan, the first talkgroup listed has the highest priority in this item.
    #[serde(deserialize_with = "patterns")]
    #[schemars(with = "Vec<String>", extend("x-talkgroups" = true))]
    pub talkgroups: Vec<String>,
}

/// A channel's rules, checked and ready to match talkgroups against.
#[derive(Clone, Debug, PartialEq)]
pub struct Channel {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub scan_mode: ScanMode,
    rules: Vec<Rule>,
    entries: usize,
}

#[derive(Clone, Debug, PartialEq)]
struct Rule {
    systems: Vec<String>,
    talkgroups: Vec<String>,
}

/// Where a talkgroup sits in a channel's rules: `(rule, entry)`, and a smaller one is higher priority.
pub type Rank = (usize, usize);

impl Channel {
    /// Check one channel's rules. An entry that isn't a talkgroup is an error, named.
    pub fn compile(c: &ChannelConfig) -> Result<Channel, String> {
        let label = if c.name.is_empty() { c.id.as_str() } else { c.name.as_str() };
        let mut rules = Vec::new();
        for (i, rule) in c.sources.iter().enumerate() {
            let talkgroups = talkgroup_list(&rule.talkgroups).map_err(|e| format!("{label}, talkgroup rule {}: {e}", i + 1))?;
            if !talkgroups.is_empty() {
                rules.push(Rule { systems: rule.systems.clone(), talkgroups });
            }
        }
        let entries = rules.iter().map(|r| r.talkgroups.len()).sum();
        Ok(Channel { id: c.id.clone(), name: c.name.clone(), enabled: c.enabled, scan_mode: c.scan_mode, rules, entries })
    }

    /// More than one talkgroup is configured: the source names carry the talkgroup too, as in IaR-Stream's scan.
    pub fn multi(&self) -> bool {
        self.entries > 1
    }

    /// The rank of a call from `system` (its short name) on `talkgroup`, if this channel takes it.
    pub fn rank(&self, system: &str, talkgroup: u32) -> Option<Rank> {
        for (ri, rule) in self.rules.iter().enumerate() {
            if !rule.systems.is_empty() && !rule.systems.iter().any(|s| s == system) {
                continue;
            }
            if let Some(ei) = rule.talkgroups.iter().position(|e| entry_matches(e, talkgroup)) {
                return Some((ri, ei));
            }
        }
        None
    }
}

/// `0` is every talkgroup; anything else is a number, a range or a pattern.
fn entry_matches(entry: &str, talkgroup: u32) -> bool {
    entry == "0" || talkgroup_matches(entry, talkgroup)
}

/// Every channel of the settings, checked. The first error names its channel.
pub fn compile_all(config: &Config) -> Result<Vec<Channel>, String> {
    config.channels.iter().map(Channel::compile).collect()
}

/// How many characters of talkgroups a channel's summary line shows before it says "+N more".
const SUMMARY_WIDTH: usize = 64;

impl ScanMode {
    fn short(self) -> &'static str {
        match self {
            ScanMode::Scan => "Scan",
            ScanMode::PriorityScan => "Priority scan",
        }
    }
}

/// The line a channel's card shows without being opened: its scan mode and its talkgroups as typed, in
/// priority order. In Priority scan they're joined with ">" (outranks); a long list ends "+N more".
pub fn sending_line(c: &ChannelConfig) -> String {
    let mode = c.scan_mode.short();
    let entries: Vec<String> = c
        .sources
        .iter()
        .flat_map(|r| r.talkgroups.iter())
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .map(|t| if t == "0" { "every talkgroup".to_string() } else { t.to_string() })
        .collect();
    if entries.is_empty() {
        return format!("{mode} · no talkgroups yet");
    }
    let sep = if c.scan_mode == ScanMode::PriorityScan { " > " } else { ", " };
    let mut shown = String::new();
    let mut count = 0;
    for e in &entries {
        let next = if count == 0 { e.clone() } else { format!("{shown}{sep}{e}") };
        if count > 0 && next.chars().count() > SUMMARY_WIDTH {
            break;
        }
        shown = next;
        count += 1;
    }
    let rest = entries.len() - count;
    let more = if rest > 0 { format!("{sep}… +{rest} more") } else { String::new() };
    format!("{mode} · {shown}{more}")
}

/// Sets every channel's summary line from its rules. Whether any of them changed.
pub fn fill_sending(channels: &mut [ChannelConfig]) -> bool {
    let mut changed = false;
    for c in channels {
        let line = sending_line(c);
        if c.sending != line {
            c.sending = line;
            changed = true;
        }
    }
    changed
}

/// IaR's list merged into the saved channels, by ID.
#[derive(Clone, Debug, PartialEq)]
pub struct Merged {
    pub channels: Vec<ChannelConfig>,
    /// Saved channels IaR no longer lists. They're kept with their rules, but nothing is sent to them.
    pub missing: Vec<String>,
}

/// Each channel IaR lists keeps its saved settings and takes IaR's name and description. A new one
/// starts off. A saved channel IaR no longer lists is kept at the end, so a failed or changed
/// listing never deletes rules.
pub fn merge(existing: &[ChannelConfig], listed: &[IarChannel]) -> Merged {
    let mut channels: Vec<ChannelConfig> = listed
        .iter()
        .map(|l| {
            let name = if l.name.is_empty() { l.id.clone() } else { l.name.clone() };
            match existing.iter().find(|c| c.id == l.id) {
                Some(c) => ChannelConfig { name, description: l.description.clone(), ..c.clone() },
                None => ChannelConfig { name, id: l.id.clone(), description: l.description.clone(), ..Default::default() },
            }
        })
        .collect();
    let missing: Vec<String> = existing.iter().filter(|c| !listed.iter().any(|l| l.id == c.id)).map(|c| c.id.clone()).collect();
    channels.extend(existing.iter().filter(|c| missing.contains(&c.id)).cloned());
    Merged { channels, missing }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn saved(id: &str, name: &str, enabled: bool, mode: ScanMode, talkgroups: &[&str]) -> ChannelConfig {
        ChannelConfig {
            id: id.into(),
            name: name.into(),
            enabled,
            scan_mode: mode,
            sources: vec![SourceRule { systems: vec!["county".into()], talkgroups: talkgroups.iter().map(|s| s.to_string()).collect() }],
            ..Default::default()
        }
    }

    fn listed(id: &str, name: &str) -> IarChannel {
        IarChannel { id: id.into(), name: name.into(), description: format!("{name} desc") }
    }

    #[test]
    fn settings_read_as_the_form_writes_them() {
        let c: Config = serde_json::from_value(json!({
            "apiKey": "k",
            "channels": [{ "id": "42", "name": "Dispatch", "enabled": true, "scanMode": "priority_scan",
                           "sources": [{ "systems": ["county"], "talkgroups": ["1000-1099", 2001] }] }]
        }))
        .unwrap();
        assert_eq!(c.api_key, "k");
        assert_eq!(c.channels[0].scan_mode, ScanMode::PriorityScan);
        assert_eq!(c.channels[0].sources[0].talkgroups, ["1000-1099", "2001"], "numbers as well as strings");
        // Left out entirely: off, scan, no rules.
        let empty: Config = serde_json::from_value(json!({ "channels": [{ "id": "1" }] })).unwrap();
        assert!(!empty.channels[0].enabled && empty.channels[0].scan_mode == ScanMode::Scan && empty.channels[0].sources.is_empty());
    }

    #[test]
    fn compiles_rules_and_names_a_bad_talkgroup() {
        let c = Channel::compile(&saved("1", "Dispatch", true, ScanMode::Scan, &["101 102", "1000-1099"])).unwrap();
        assert_eq!(c.entries, 3);
        assert!(c.multi());
        let bad = Channel::compile(&saved("2", "Bad", true, ScanMode::Scan, &["10x"])).unwrap_err();
        assert!(bad.contains("Bad") && bad.contains("10x"), "{bad}");
    }

    #[test]
    fn rank_follows_rule_then_entry_order_and_systems() {
        let mut c = ChannelConfig { id: "1".into(), enabled: true, ..Default::default() };
        c.sources = vec![
            SourceRule { systems: vec!["county".into()], talkgroups: vec!["1000-1099".into()] },
            SourceRule { systems: vec![], talkgroups: vec!["2001".into(), "507*".into()] },
        ];
        let ch = Channel::compile(&c).unwrap();
        assert_eq!(ch.rank("county", 1050), Some((0, 0)));
        assert_eq!(ch.rank("county", 2001), Some((1, 0)), "any system for rule 2");
        assert_eq!(ch.rank("county", 50712), Some((1, 1)));
        assert_eq!(ch.rank("other", 1050), None, "rule 1 is for county only");
        assert_eq!(ch.rank("county", 9999), None);
        assert!(ch.rank("county", 1050).unwrap() < ch.rank("county", 2001).unwrap());
    }

    #[test]
    fn overlapping_talkgroups_are_told_apart_by_system() {
        let mut c = ChannelConfig { id: "1".into(), enabled: true, ..Default::default() };
        c.sources = vec![SourceRule { systems: vec!["sys1".into()], talkgroups: vec!["101".into()] }];
        let ch = Channel::compile(&c).unwrap();
        assert!(ch.rank("sys1", 101).is_some());
        assert!(ch.rank("sys2", 101).is_none(), "sys2's talkgroup 101 isn't this channel's");
    }

    #[test]
    fn zero_is_every_talkgroup() {
        let mut c = ChannelConfig { id: "1".into(), enabled: true, ..Default::default() };
        c.sources = vec![SourceRule { systems: vec![], talkgroups: vec!["0".into()] }];
        let ch = Channel::compile(&c).unwrap();
        assert!(ch.rank("any", 1).is_some() && ch.rank("any", 65001).is_some());
        assert!(!ch.multi());
    }

    #[test]
    fn merge_keeps_saved_settings_and_adds_new_channels_off() {
        let existing = vec![saved("42", "Old name", true, ScanMode::PriorityScan, &["1000-1099"])];
        let m = merge(&existing, &[listed("42", "Dispatch"), listed("43", "")]);
        assert_eq!(m.channels[0].name, "Dispatch", "IaR's name");
        assert_eq!(m.channels[0].description, "Dispatch desc");
        assert!(m.channels[0].enabled && m.channels[0].scan_mode == ScanMode::PriorityScan, "saved settings kept");
        assert_eq!(m.channels[0].sources, existing[0].sources, "rules kept");
        assert_eq!((m.channels[1].id.as_str(), m.channels[1].name.as_str(), m.channels[1].enabled), ("43", "43", false));
        assert!(m.missing.is_empty());
    }

    #[test]
    fn merge_keeps_channels_iar_no_longer_lists() {
        let existing = vec![saved("42", "Gone", true, ScanMode::Scan, &["101"]), saved("7", "Kept", false, ScanMode::Scan, &[])];
        let m = merge(&existing, &[listed("7", "Kept")]);
        assert_eq!(m.channels.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["7", "42"]);
        assert_eq!(m.channels[1].sources, existing[0].sources, "its rules survive");
        assert_eq!(m.missing, ["42"]);
    }

    #[test]
    fn the_summary_line_lists_talkgroups_in_priority_order() {
        let mut c = saved("1", "Fire", true, ScanMode::PriorityScan, &["58909", "58910"]);
        c.sources.push(SourceRule { systems: vec![], talkgroups: vec!["1000-1099".into(), "507*".into()] });
        assert_eq!(sending_line(&c), "Priority scan · 58909 > 58910 > 1000-1099 > 507*");
        c.scan_mode = ScanMode::Scan;
        assert_eq!(sending_line(&c), "Scan · 58909, 58910, 1000-1099, 507*", "scan has no priority, so no >");
    }

    #[test]
    fn the_summary_line_says_so_when_there_is_nothing_to_show() {
        assert_eq!(sending_line(&ChannelConfig::default()), "Scan · no talkgroups yet");
        let all = saved("1", "All", true, ScanMode::Scan, &["0"]);
        assert_eq!(sending_line(&all), "Scan · every talkgroup");
    }

    #[test]
    fn a_long_summary_line_is_cut_with_a_count() {
        let tgs: Vec<String> = (58000..58100).map(|n| n.to_string()).collect();
        let refs: Vec<&str> = tgs.iter().map(String::as_str).collect();
        let line = sending_line(&saved("1", "Fire", true, ScanMode::PriorityScan, &refs));
        assert!(line.starts_with("Priority scan · 58000 > 58001 > 58002"), "{line}");
        assert!(line.ends_with(" > … +92 more"), "8 of the 100 fit: {line}");
        assert!(line.chars().count() < 100, "short enough for a card: {line}");
        // One that is long on its own is still shown.
        let one =
            sending_line(&saved("1", "Fire", true, ScanMode::Scan, &["12345678901234567890123456789012345678901234567890123456789012345678901234567890"]));
        assert!(one.contains("1234567890123456789012345678901234567890") && !one.contains("more"), "{one}");
    }

    #[test]
    fn filling_summaries_reports_only_a_change() {
        let mut channels = vec![saved("1", "Fire", true, ScanMode::Scan, &["101"])];
        assert!(fill_sending(&mut channels), "empty to filled");
        assert_eq!(channels[0].sending, "Scan · 101");
        assert!(!fill_sending(&mut channels), "nothing changed, so nothing to save");
        channels[0].scan_mode = ScanMode::PriorityScan;
        assert!(fill_sending(&mut channels), "the mode is in the line");
    }

    #[test]
    fn the_form_has_actions_secret_key_and_talkgroup_fields() {
        let s = trunk_recorder_plugin::schema::schema_for::<Config>();
        assert_eq!(s["x-actions"][0]["id"], "refresh");
        assert_eq!(s["properties"]["apiKey"]["x-secret"], true);
        let channel = &s["properties"]["channels"]["items"];
        assert_eq!(channel["x-title"], "name");
        assert_eq!(channel["properties"]["name"]["readOnly"], true);
        assert_eq!(channel["properties"]["sending"]["readOnly"], true);
        assert_eq!(channel["x-summary"], json!(["{sending|Not set up yet}"]), "the card's line is the plugin's summary");
        assert_eq!(channel["properties"]["scanMode"]["enum"], json!(["scan", "priority_scan"]));
        let rule = &channel["properties"]["sources"]["items"];
        assert_eq!(rule["properties"]["systems"]["x-systems"], true);
        assert_eq!(rule["properties"]["talkgroups"]["x-talkgroups"], true);
    }
}
