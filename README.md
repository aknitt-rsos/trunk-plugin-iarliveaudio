# IaR Live Audio plugin for Trunk Recorder Pro

Sends call audio from [Trunk Recorder Pro](https://github.com/aknitt-rsos/trunk-recorder-pro) straight to [IamResponding](https://iamresponding.com) Live Audio channels. It doesn't need the simplestream plugin or IaR-Stream.

## Quick start

1. Download the release for your platform and add it in Trunk Recorder Pro with **Add from a file...** (Plugins page).
2. Open the **IaR Live Audio** page and enter your IaR Live Audio API key.
3. Click **Refresh channels**. The channels your key can use appear as cards.
4. Turn on a channel and add rules: pick the systems and talkgroups to send. Leave the systems list empty to match every system.
5. Start recording. Audio is sent live while calls are active.

## Settings

| Setting | What it does |
| --- | --- |
| `apiKey` | Your IaR Live Audio API key. Stored as a secret. |
| `channels[].enabled` | Turns streaming to this channel on or off. New channels start off. |
| `channels[].scanMode` | `scan`: the first active talkgroup holds the channel. `priority_scan`: a higher-priority talkgroup preempts a lower one (see below). |
| `channels[].sources[].systems` | Systems this item applies to. Empty means all systems. |
| `channels[].sources[].talkgroups` | Talkgroup IDs (`101`), ranges (`1000-1099`), wildcards (`507*`), or `0` for all. In Priority scan the order matters. |
| `channels[].sending` | Read only. The line each channel's card shows without being opened: its scan mode and talkgroups in priority order, cut with "+N more" when long. The plugin keeps it up to date. |

When you refresh channels, your settings are kept by channel ID. Channels the API no longer returns are skipped, but their rules are kept.

## Priority scan

There is no separate priority setting. Priority is the order you write things in, from the top down:

- **Inside one item**, a talkgroup earlier in the list outranks one later in the list. `58909, 58910, 58421` means 58909 is highest and 58421 lowest.
- **Across items**, everything in an earlier item outranks everything in a later item. Put dispatch talkgroups in Item 1 and the rest in Item 2.
- A talkgroup listed more than once counts at its first place.

When two talkgroups are on the air at once in **Priority scan**, the higher one takes the channel (the lower one's transmission ends and the higher one's starts). A lower talkgroup never interrupts a higher one: its audio is skipped while the channel is busy. In **Scan** mode the order doesn't matter. The first talkgroup to start keeps the channel until its call ends.

A card's summary line shows the talkgroups in priority order, with `>` meaning "outranks": `Priority scan · 58909 > 58910 > 58421 > … +37 more`. The line is saved by the plugin, so it updates when the plugin is running and settings change.

## Health

The plugin's Health card in Trunk Recorder Pro has one row per channel that is on:

- A light and the state of its connection to IaR: up, down (with the last error), or unknown while it is still connecting.
- **{channel} packets**: how many 60 ms audio packets that channel has sent since the plugin started.
- **{channel} last audio**: how long ago it last sent audio (`just now`, `12 s ago`, `5 min ago`).
- **Data**: bytes of audio sent across all channels. This is a running total: turning a channel off doesn't lower it.

Turning every channel off clears the rows. A channel that is down turns the whole plugin red, so a single bad channel is easy to see.

## Multisite

If a system is received at more than one site, the plugin streams one copy at a time per talkgroup. It switches sites only after the current one goes quiet, so the channel doesn't get the same audio twice.

## Building

Requires a recent stable Rust toolchain. Opus is compiled from bundled source, so no system libraries are needed. Linux builds need `cmake`, and Windows builds need MSVC.

```
cargo build --release
cargo test
```

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
