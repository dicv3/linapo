# LinAPO

Equalizer APO-style channel routing and equalization for Linux audio systems using
[PipeWire](https://pipewire.org/).

LinAPO gives you a graphical editor and a command-line interface for building audio
setups with:

- An arbitrary input-to-output channel matrix
- Per-output-channel EQ chains
- Master gain and EQ bypass for A/B comparison
- Stereo, quad, 5.1, 7.1, and custom channel layouts
- Import of Equalizer APO profiles, including `Copy:`, `Channel:`, and `GraphicEQ:`
- Live changes without restarting PipeWire when the graph structure is unchanged
- Named profiles with per-device bindings and automatic switching
- Offline WAV rendering and frequency-response inspection

```text
inputs -> channel matrix -> per-output EQ -> master gain -> outputs
```

## Screenshots

| Matrix | Equalizer |
| --- | --- |
| ![LinAPO channel matrix](docs/matrix.png) | ![LinAPO equalizer editor](docs/equalizer.png) |

| Setup | GraphicEQ import |
| --- | --- |
| ![LinAPO setup page](docs/setup.png) | ![GraphicEQ fit](docs/graphic-eq.png) |

## Requirements

- Linux
- Rust and Cargo
- PipeWire and WirePlumber for live audio
- `xdg-desktop-portal` and a desktop portal backend for native file dialogs

The core library and offline processing commands do not require a running PipeWire
instance. The PipeWire integration uses the command-line tools `pw-dump`, `pw-cli`,
and `systemctl`; no native PipeWire development library is required.

## Install

### Download a release

Pre-built release binaries can be downloaded from the
[GitHub Releases](https://github.com/dicv3/linapo/releases) page. Download the
archive for your system, extract it, and run `linapo` or `linapo-gui` directly.
No Rust toolchain is required for a release binary. For example:

```sh
tar -xf linapo-*.tar.gz
cd linapo-*/
./linapo-gui
```

The release binaries are self-contained LinAPO executables, but live audio still
requires PipeWire, WirePlumber, and the command-line tools listed in
[Requirements](#requirements).

### Build from source

Clone the repository and build the release binaries:

```sh
git clone https://github.com/<your-user>/linapo.git
cd linapo
cargo build --release
```

The binaries are written to `target/release/linapo` and
`target/release/linapo-gui`. You can also run them directly with Cargo:

```sh
cargo run --release -p linapo-gui
cargo run --release -p linapo-cli -- --help
```

## Quick start

Import an Equalizer APO profile into a LinAPO setup:

```sh
linapo setup import examples/sample.txt -o setup.json
linapo info setup.json
```

Install it as a PipeWire virtual sink:

```sh
linapo install setup.json
```

Select **LinAPO EQ** as the output device in your desktop sound settings, or set it
as the default with `wpctl`. Audio then flows through LinAPO before reaching the
selected output device. To target a specific PipeWire sink:

```sh
linapo install setup.json --target <pipewire-node-name>
```

Installation restarts PipeWire, WirePlumber, and PipeWire Pulse, briefly interrupting
audio. Use `--no-restart` when you only want to generate the configuration.

Change values in a running setup without restarting PipeWire:

```sh
linapo live master setup.json -6
linapo live route setup.json L SUB -3
linapo live bypass setup.json on
linapo live apply setup.json
```

Remove the virtual sink with:

```sh
linapo uninstall
```

## GUI

Launch the graphical editor with:

```sh
linapo-gui
# or open a setup directly
linapo-gui setup.json
```

The default setup is saved at `~/.config/linapo/setup.json`. Edits are saved
automatically and pushed to PipeWire after a short debounce period.

Named profiles are stored in `~/.config/linapo/profiles/`. Profiles can be
selected and edited live, and can be associated with specific output devices.
Enable automatic switching to apply the matching profile when the active output
device changes.

### Matrix

The matrix editor controls the linear gain from every input channel to every output
channel. Click a cell to connect or disconnect a route, drag to adjust its gain, and
right-click for an exact value or polarity inversion. Presets include pass-through,
stereo downmix, and clear.

### Equalizer

Create, duplicate, delete, enable, and disable named EQ presets. An EQ can apply to
multiple output channels, while multiple EQs can be stacked on one channel. The
response plot supports draggable frequency/gain handles, mouse-wheel Q adjustment,
and double-click insertion of new filters.

### Setup

Choose input and output layouts, select the destination device, install or remove the
PipeWire graph, and import APO profiles either as additional EQs or as a complete
replacement. Undo and redo cover edits, imports, and layout changes.

Structural changes such as channel layouts, output devices, and filter types require
an install or PipeWire restart. Numeric changes, filter enable state, and EQ bypass
are live. Switching between profiles is also live when the installed graph has
enough filter slots for the selected profile.

## CLI commands

```text
linapo info <setup-or-apo-file>
linapo response <setup-or-apo-file> -c L
linapo process <setup-or-apo-file> in.wav out.wav
linapo setup new --layout stereo --out-layout 5.1 -o setup.json
linapo setup import profile.txt --layout stereo -o setup.json
linapo gen-config <setup-or-apo-file>
linapo install <setup-or-apo-file>
linapo uninstall
linapo live apply <setup.json>
linapo live master <setup.json> <db>
linapo live route <setup.json> <input> <output> <db|off>
linapo live bypass <setup.json> <on|off>
linapo profile new <name>
linapo profile save <name> <setup-or-apo-file>
linapo profile use <name>
linapo bindings set <pipewire-node-name> <profile>
linapo bindings default <profile>
linapo bindings auto <on|off>
linapo watch
linapo autostart <enable|status|disable>
```

Commands that accept a setup also accept an Equalizer APO profile. Use `--layout`
when loading an APO profile; JSON setup files contain their own layouts.

`process` renders a 32-bit float WAV through the reference engine, including the
matrix, EQ, master gain, and limiter. `response` prints CSV frequency-response data.
`profile use` switches the active profile without restarting PipeWire when the
graph structure is unchanged. `watch` monitors the current output device and
switches profiles according to the configured bindings. `autostart` manages a
systemd user service that runs `watch` at login.

## Setup files

Setup files are JSON. The matrix is indexed as `[input][output]` and stores linear
gains: `1` is unity, `0.5` is approximately -6 dB, and negative values invert
polarity.

```json
{
  "input_channels": ["L", "R"],
  "output_channels": ["L", "R", "C", "SUB", "RL", "RR"],
  "matrix": [
    [1, 0, 0, 0.5, 0.5, 0],
    [0, 1, 0, 0.5, 0, 0.5]
  ],
  "eqs": [
    {
      "name": "sub low-pass",
      "channels": ["SUB"],
      "filters": [
        {"kind": "low_pass", "fc_hz": 500, "q": 0.7071}
      ]
    }
  ],
  "master_gain_db": -2,
  "limiter": true
}
```

Supported filter kinds are `peaking`, `low_shelf`, `high_shelf`, `low_shelf1`,
`high_shelf1`, `low_pass`, `high_pass`, `band_pass`, `notch`, and `all_pass`.
Channel names include `L`, `R`, `C`, `SUB`, `RL`, `RR`, `SL`, `SR`, and numbered
channels such as `CH1` and `CH2`. Common APO aliases such as `FL`, `FR`, `FC`,
`LFE`, `BL`, and `BR` are accepted.

## Equalizer APO import

LinAPO understands the following APO constructs:

- `Preamp`
- `Filter` types including peaking, shelves, pass, notch, and all-pass filters
- `Channel:` assignments
- `Copy:` routing and gain expressions
- `GraphicEQ:` curves
- `Include:` files when parsing from disk

`GraphicEQ:` curves are fitted to ordinary biquad filters rather than imported as
linear-phase FIR filters. This keeps the result latency-free, editable, switchable,
and usable with live PipeWire controls. The fit reports its maximum and RMS magnitude
error. The approximation changes phase and may be less accurate around very sharp
features.

Unsupported APO features such as `Device:`, `Delay:`, `Convolution:`, conditional
blocks, VST, and LADSPA lines are reported as warnings and skipped.

## Profiles and device switching

Profiles are setup files stored under `~/.config/linapo/profiles/`. Use the CLI
or GUI to create, duplicate, rename, delete, and switch profiles. A profile can
be bound to a specific PipeWire output device, while a default profile can be
used for all other devices.

Enable automatic switching with:

```sh
linapo bindings set <pipewire-node-name> <profile>
linapo bindings default <profile>
linapo bindings auto on
linapo watch
```

`linapo watch` checks for output-device changes and applies the matching profile
without rebuilding the graph when possible. To start it automatically at login:

```sh
linapo autostart enable
```

Use `linapo autostart status` or `linapo autostart disable` to inspect or remove
the systemd user service.

## Architecture

```text
linapo-core  DSP, biquads, APO parser, setup model, engine, PipeWire config generation
linapo-pw    PipeWire discovery, installation, status, profiles, and live control updates
linapo-cli   Command-line interface and offline processing commands
linapo-gui   egui/eframe editor and background PipeWire worker
```

PipeWire's `filter-chain` module runs the live DSP graph. Native PipeWire nodes are
used where possible; filters needing exact cookbook coefficients are emitted as
`bq_raw` nodes. The core crate also contains a reference engine used by offline
processing and backend tests.

## Testing

Run the Rust test suite with:

```sh
cargo test --workspace
```

The end-to-end PipeWire parity test starts a private headless PipeWire instance, so
it does not change the user's normal audio session:

```sh
python3 tests/pipewire/parity.py examples/stereo-to-5.1.json
```

The parity test additionally needs PipeWire, WirePlumber, `pw-cat`, `pw-cli`,
`dbus-run-session`, Python 3, and NumPy. GUI end-to-end tests live in
`tests/pipewire/gui_e2e/`.

## Limitations

- The PipeWire backend has been tested against PipeWire 1.0.5; run the parity test
  when validating another version.
- The limiter is implemented by the reference engine but is not currently applied by
  the PipeWire backend.
- The built-in PipeWire mixer supports at most eight input channels.
- EQ is applied after the matrix, so profiles that filter before a `Copy:` operation
  can produce different results from Equalizer APO.
- PipeWire gain changes currently jump instead of using the reference engine's smooth
  ramps.
- Automatic device switching may take up to about one second after the output device
  changes.
- Arbitrary channel lists work in JSON setup files but the GUI layout picker offers
  mono, stereo, quad, 5.1, and 7.1.

## License

LinAPO is dual-licensed under either of:

- [MIT License](https://opensource.org/licenses/MIT)
- [Apache License 2.0](https://www.apache.org/licenses/LICENSE-2.0)

at your option.
