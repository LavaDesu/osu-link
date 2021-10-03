# osu-link
[![Build Artifacts](https://img.shields.io/github/workflow/status/LavaDesu/osu-link/Build%20Artifacts/master?style=for-the-badge)](https://github.com/LavaDesu/osu-link/actions/workflows/push.yml)
[![Releases](https://img.shields.io/github/downloads/LavaDesu/osu-link/total?style=for-the-badge)](https://github.com/LavaDesu/osu-link/releases)
[![License](https://img.shields.io/github/license/LavaDesu/osu-link?style=for-the-badge)](./LICENSE)

osu-link is a program which links osu!stable beatmaps to osu!lazer's new store format, saving you disk space.

## Installation

No installation required, just grab the latest release from the [releases tab](https://github.com/LavaDesu/osu-link/releases),
run it, and follow the steps.

## Contributing

Contributions are welcome! This is my first Rust project, so expect some weird unidiomatic code (please tell me about it however!).

## FAQ
### What's the difference between osu!lazer's built-in import and this?
Instead of copying files like osu!lazer, osu-link simply *links* the files (hard-link on windows; soft-link on unix).

By linking, no files are actually copied, saving you a lot of disk space.

### I linked my beatmaps and some of them are corrupted/not working/broken!
**Before opening an issue to the osu!lazer project, try downloading it normally in osu!lazer first.**
If it works fine there, open an issue here. Otherwise, refer to [osu!lazer's contributing guidelines](https://github.com/ppy/osu/blob/master/CONTRIBUTING.md#i-would-like-to-submit-an-issue)
and open an issue there.

### My unsubmitted beatmaps aren't imported!
Unsubmitted beatmaps are currently unsupported. Check back later!

### My beatmap edits aren't imported!
Beatmap edits (e.g., editing a beatmap and saving as another difficulty) are currently unsupported. Check back later!

## License
This project is licensed under the [MIT License](./LICENSE).
