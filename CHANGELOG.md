# Changelog

## [Unreleased]

- Health shows one row per channel: its connection, its packet count, and how long ago it last sent audio. Data is a running total of bytes sent.
- Turning every channel off clears the rows instead of leaving old ones showing.

## [0.1.1]

- Each channel's card now shows its scan mode and talkgroups, in priority order, without being opened. A long list ends "+N more".
- The help text explains how Priority scan works: priority runs from the top, inside an item and across items.
- Release builds for macOS (one binary for Apple silicon and Intel) are back.

## [0.1.0]

- First release. Sends Trunk Recorder Pro call audio straight to IamResponding Live Audio channels, with no simplestream or IaR-Stream in between.
- Enter an IaR Live Audio API key and the plugin fetches the channels it can use.
- Per-channel rules pick the systems and talkgroups to send, with Scan and Priority Scan arbitration.
