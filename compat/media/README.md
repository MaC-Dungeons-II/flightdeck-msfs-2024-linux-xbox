# Wine media compatibility module

The system-memory capsfilter in `system-memory.patch` is adapted from
[OnlyCrayZ-Labs/wine](https://github.com/OnlyCrayZ-Labs/wine/tree/2aa1523c4ff41f6bf6b7ed05e9bd52c554a8fe9b).
Wine and this modification are LGPL-2.1-or-later; the complete license is
[`../LICENSES/LGPL-2.1.txt`](../LICENSES/LGPL-2.1.txt). The Flightdeck launcher
and its module selection/build tooling remain MIT licensed. The module is
loaded separately by Wine and can be rebuilt and replaced before embedding.
For a modified local module, update the module hash/size in `media.lock.json`
and rebuild the launcher; the verification pins are not a restriction on
modification or redistribution under the original component licenses.

`media.lock.json` in the parent directory pins the original source archives,
patch, before/after parser hashes, rebuilt module and its compatible runner
inputs. The only Wine runtime change is the capsfilter before the
CPU capssetter/videoflip chain. It requires `video/x-raw` in system memory,
preventing CUDA/GL-memory video frames from entering that chain directly.
This does not introduce a new DXVK/VKD3D renderer or qualify NVIDIA gameplay.

## Rebuild and distribute

Use Ubuntu 24.04 with the packages in `native-build.Dockerfile`, Rust 1.98
and the checked-out Flightdeck source:

```sh
cargo build --locked -p flightdeck-tools
target/debug/flightdeck-tools media-build --download \
  --sources build/media-inputs --work build/media-work --output build/media --jobs 2
cargo build --locked --features bundled-media
```

Both work and output directories must be new. Source hashes are checked before
extraction; unexpected links, special files and oversized archives are rejected.
The builder populates the pinned xgameruntime submodule, regenerates specfiles,
server protocol and Vulkan headers, configures Wine without FFmpeg/unwind/X/
FreeType and builds only the module target and its build dependencies. It strips
the module and requires its expected hash. Generated NTDLL/wineserver binaries
are never distributed or installed. Debug/source path remapping and a fixed
build epoch make the module independent of the build directory.

Build `--features bundled-media` only after placing the verified module in
`build/media/winegstreamer.so`. Complete packages reject missing or mismatched
payloads. The installer archive layout stays compatible with older updaters.
The module is embedded as data and extracted for Wine, not linked into Rust.
For distribution, run `source-release --media-sources build/media-inputs`:
the matching Flightdeck source archive then includes both exact original LGPL
source archives under `media-sources/`, this patch, the build instructions and
the source manifest. Preserve that source archive alongside the binary and
the existing native/graphics corresponding-source archives.

To rebuild from a published source archive without downloading, use
`--sources media-sources` and omit `--download`.

## Selection and validation

Only the pinned loader and Wine GStreamer PE/Unix pair qualify. Other Proton
builds use their original modules. A small content-addressed view under
`local/media-runners/` copies the current loader and NTDLL pair, keeping Fenix
NTDLL modifications, and installs the rebuilt media module with the original
matching PE partner. Other DLLs resolve from the selected runner. Views are
reused and fully checked for hashes, ownership, extra files and link targets;
changed views are not executed. The selected runner and game prefix are not
rewritten. Companion helpers keep the original loader/environment.

`FLIGHTDECK_MEDIA_SYSTEM_MEMORY=0` disables selection. Source builds without the
feature also retain the previous media path. `--native-probe media-bundle`
reports whether a binary embeds the expected payload.

`tests/graphics/media-caps.c` checks real GStreamer caps negotiation and a CPU
video pipeline. The isolated `flightdeck-tools probe media` exercises the actual
launcher configuration and reports the overlay decision, two decode/rewind
passes, EOS and shutdown independently. On 25 October 2026 the rebuilt module
passed 3840×2160 H.264 decoding on the software/headless path. NVIDIA device
negotiation and full MSFS gameplay still require NVIDIA hardware validation.

## Reviewed upstream snapshot

The public source heads reviewed again on 25 October 2026 were:

| Repository | Commit |
| --- | --- |
| OnlyCrayZ-Labs/MSFS2024-Linux | `c36d4b9bdf534ccb1861ef0d8a4065a9158e437d` |
| OnlyCrayZ-Labs/wine | `2aa1523c4ff41f6bf6b7ed05e9bd52c554a8fe9b` |
| OnlyCrayZ-Labs/xgameruntime | `0975046858cbb146ee4cc88720a54f860fb7d1a5` |
| OnlyCrayZ-Labs/xodus | `cb5b5bf0d601e9ff8a2c08fd506f54b9c9b03ac6` |
| OnlyCrayZ-Labs/xodus-proton | `93b43dc94d51d2d285a9e731c73ff1c8a26c955f` |

Their taskqueue lifecycle scenarios are adapted to Flightdeck's existing C++
queue in `tests/compat/taskqueue-test.cpp` and run through `driver run-native
--suite queue`. Flightdeck keeps its existing auth broker, broader WineGDK API
and cloud/backup-capable `XDLOCAL1` save implementation; importing their second
broker or incompatible local-only `XGS1` format would lose existing behavior.
Their public repositories describe the successful RTX 5090 run as a private
checkpoint; the sanitized public sources do not reproduce that validation.

For new upstream code, compare against these exact heads and the existing
Flightdeck implementation, review source/license changes, run the affected
synthetic suites and actual media probe, then update the pins and corresponding
sources together. Never replace the lock with a moving branch or presume a
private checkpoint is present in public sources.
