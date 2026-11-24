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
| `channels[].scanMode` | `scan`: the first active talkgroup holds the channel. `priority_scan`: an earlier rule preempts a later one. |
| `channels[].sources[].systems` | Systems this rule applies to. Empty means all systems. |
| `channels[].sources[].talkgroups` | Talkgroup IDs (`101`), ranges (`1000-1099`), wildcards (`507*`), or `0` for all. |

When you refresh channels, your settings are kept by channel ID. Channels the API no longer returns are skipped, but their rules are kept.

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
