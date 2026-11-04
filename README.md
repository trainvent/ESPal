# ESPal

**A standalone voice assistant for the Arduino Nano ESP32, built in Rust.**

![ESP32-S3](https://img.shields.io/badge/ESP32--S3-Nano_ESP32-red)
![Rust](https://img.shields.io/badge/Firmware-Rust-orange)
![ESP-IDF](https://img.shields.io/badge/ESP--IDF-v5.5.5-blue)
![OpenAI Realtime](https://img.shields.io/badge/AI-OpenAI_Realtime-412991)
[![License: GPL v3](https://img.shields.io/badge/License-GPL_v3-blue)](LICENSE.md)

Say **Jarvis**, wait for the ready beep, and ask a question. ESPal streams your
voice directly to OpenAI over Wi-Fi and plays the spoken response through an
I2S speaker. The Nano handles audio, networking, and conversation control;
a computer is only needed for setup and diagnostics.

> **Development status:** microphone recording and clear speaker speech have
> been verified on the Nano. Standalone wake listening and streaming conversations
> are implemented. Wake-word interruption during playback is experimental and
> still needs physical speech and echo-rejection acceptance tests.

[Features](#-key-features) · [Hardware](#-hardware--wiring) ·
[Quick start](#-quick-start) · [Commands](#-usb-commands) ·
[Troubleshooting](#-troubleshooting) · [Development](#-development)

## Key features

- **Local wake word:** ESP-SR detects “Jarvis” on the Nano. Idle wake audio stays on the device.
- **Streaming conversations:** microphone capture, filtering, upload, and reply playback use separate tasks and bounded queues.
- **Follow-up questions:** keep talking in the same OpenAI session; 15 seconds of inactivity returns to wake listening.
- **Natural speech endings:** a 1.2-second pause ends a request, with a ready beep before listening.
- **Configurable replies:** choose the Realtime model, voice, instructions, and microphone gain over USB.
- **Secure transport:** certificate and hostname validation, with SNTP time synchronization before TLS.
- **Local diagnostics:** test the microphone, speaker, audio pipeline, wake detector, and echo cancellation from USB.
- **Experimental interruption:** say “Jarvis” during a reply to stop playback and begin another request.

## 🔌 Hardware & wiring

### Required components

| Component | ESPal setup |
| --- | --- |
| Controller | Arduino Nano ESP32 / ESP32-S3, 16MB flash and 8MB octal PSRAM |
| Microphone | ICS-43434 I2S microphone |
| Amplifier | MAX98357A I2S amplifier |
| Output | Passive speaker; confirm impedance and power rating for your amplifier |
| Power & setup | USB cable with data support and a suitable USB supply |
| Network | 2.4GHz Wi-Fi and an OpenAI project API key with Realtime access |

### Connections

The microphone and amplifier share the I2S clock lines.

| Nano printed pin | ESP32 GPIO | ICS-43434 | MAX98357A |
| --- | --- | --- | --- |
| D2 | GPIO5 | BCLK / SCK | BCLK |
| D3 | GPIO6 | WS / LRCL | LRC |
| D4 | GPIO7 | DOUT / SD | — |
| D5 | GPIO8 | — | DIN |
| 3V3 | — | VDD / 3V | — |
| VBUS | — | — | VIN |
| GND | — | GND and SEL / L-R | GND |

Connect the speaker across **SPK+ and SPK−**, never from a speaker output to GND.
Mic SEL/L-R to GND selects the left slot. Microphone power and GPIO signals are
**3.3V**; VBUS supplies approximately **5V** while USB powered. Rust uses ESP32
GPIO numbers, not Arduino digital pin numbers.

See [the full pinout](docs/pinout.md) for a wiring diagram, module details,
and hardware references.

## 🚀 Quick start

The commands below use the Linux development setup and `/dev/ttyACM0`.
Run them from the repository root; substitute your board's actual serial port.

### 1. Install the development tools

You need Rust/Cargo, Python 3, and the ESP-IDF host build prerequisites.
The bring-up toolchain uses these pinned versions:

```sh
cargo +stable install espup --version 0.18.0 --locked
cargo +stable install espflash --version 4.6.0 --locked
cargo +stable install ldproxy --version 0.3.5 --locked
espup install --std --targets esp32s3 --toolchain-version 1.97.0.0 --export-file ./export-esp.sh
. ./export-esp.sh

python3 -m venv .venv
. .venv/bin/activate
python3 -m pip install pyserial
```

Source `export-esp.sh` in every new build terminal and activate `.venv` before
using the Python tools. The first build downloads ESP-IDF and managed tools
into `.embuild/`.

### 2. Build and flash

```sh
cargo build --locked
bash tools/flash.sh
```

Always use `tools/flash.sh` (also the `cargo run` runner). It writes the offline
wake model, selects the matching ESP-IDF bootloader, and uses `partitions.csv`
with a 3MiB factory application partition. NVS and PHY offsets remain unchanged.
The generated default ESP-IDF partition table is not used.

For another port:

```sh
ESPFLASH_PORT=/dev/ttyACM1 bash tools/flash.sh
```

If USB access or download mode fails, see [troubleshooting](#-troubleshooting).

### 3. Provision Wi-Fi and OpenAI

```sh
python3 tools/provision.py
python3 tools/console.py connect
python3 tools/console.py check
```

The setup tool prompts locally for Wi-Fi credentials, a dedicated OpenAI project
key, model, voice, instructions, and microphone gain. Password and key entry are
hidden; settings are saved atomically as one NVS blob without creating a secret file.

The tool currently suggests `gpt-realtime-2.1` and `marin`; model access depends
on your project. The tested device uses **Cedar**, which you can select during
setup or save afterward:

```sh
python3 tools/console.py voice cedar
```

`connect` checks Wi-Fi, DHCP, and synchronized time. `check` verifies OpenAI
authentication and audio-session configuration without capturing or uploading
microphone audio.

To switch to Wi-Fi profile 1 from the local `.env` while keeping the saved API
key and other settings:

```sh
python3 tools/provision.py --wifi-profile 1
python3 tools/console.py connect
```

### 4. Try a conversation

```sh
python3 tools/console.py turn
```

Press Enter when ready, then speak when `LISTENING` appears. Pause to finish
your request, listen to the reply, and ask a follow-up question.

### 5. Enable standalone mode

```sh
python3 tools/console.py standalone on
```

The setting persists across restarts. After a five-second USB recovery window
at boot, the Nano starts offline wake listening:

1. Say **Jarvis**.
2. Wait for the ready beep.
3. Speak naturally; a 1.2-second pause ends the request.
4. Listen, then ask another question in the same session.
5. After 15 seconds without a new question, ESPal returns to wake listening.

A short error notification signals a connection or conversation failure.
Wi-Fi rejection events end an attempt immediately; otherwise association and
DHCP share an eight-second timeout.
In standalone mode, temporary connection failures trigger up to three retries
after 1, 2, and 4 seconds. Quiet waiting ticks start when Jarvis is detected and
continue during retry waits and Wi-Fi/time connection. The error clip plays
only if recovery gives up.
A ready beep means you can speak again; repeat your question after a dropped connection. Failed audio
is discarded and each reconnect starts a fresh conversation. Exhausted retries,
authentication, quota, configuration, and local audio errors return to wake
listening. Say Jarvis to try again. USB commands remain single-attempt diagnostics.
Preview the notification with `python3 tools/console.py failure-sound`.
Preview the retry ticks with `python3 tools/console.py waiting-sound`.

Continuous input is limited to ten minutes, and each conversation has an
eleven-minute deadline.

To return to USB bench mode:

```sh
python3 tools/console.py standalone off
```

Opening the console resets the board. Send the command during the five-second
boot recovery window.

## 🛠 USB commands

Use `python3 tools/console.py COMMAND`, adding `--port /dev/ttyACM1` if needed.

| Command | Purpose |
| --- | --- |
| `help` | Display firmware command descriptions |
| `connect` | Connect Wi-Fi and synchronize time |
| `check` | Validate OpenAI authentication and session setup; no microphone upload |
| `turn` | Start a speech-ended conversation |
| `voice cedar` | Save the reply voice for subsequent turns |
| `standalone on` / `standalone off` | Save the boot mode |
| `forget` | Remove the saved configuration blob |
| `stats` | One second of microphone statistics with silent output |
| `tone` | Brief, low-amplitude 440Hz tone through duplex I2S |
| `pipeline` | Local tone through the filtered playback pipeline; no network request |
| `loopback` | Check digital I2S sample transport and restore microphone routing |
| `input-test` | Stress input queues and cancellation locally; no microphone upload |
| `wake-check` / `wake-test` | Wake detector self-check / live detection test |
| `aec-check` | Synthetic echo-suppression diagnostic |
| `interrupt-check` | Local playback with AEC/WakeNet and detector-reuse checks |
| `interrupt-network-check` | Check interruption with Wi-Fi/TLS and a fixed-text OpenAI reply; no microphone upload |

For an isolated 16-bit speaker test, use `tone16`; reset before another audio
command in that USB session. `tone-pico` plays a precomputed comparison tone.
Loopback checks digital transport, not amplifier wiring or analog output.

### Record the microphone locally

```sh
python3 tools/record.py
```

Press Enter when ready and speak immediately. The tool captures five seconds
of native 48kHz mono PCM24, verifies length and checksum, and writes a WAV.
It refuses to overwrite an existing file. Default listening gain is 16×;
use `--gain 1` for native samples or lower gain if speech clips. Without
`--output`, recordings go to `/home/leonmarq/Dokumente/Codex/ESPal/microphone-speech-5s.wav`.
Recording requires 720KB of contiguous capture memory and aborts on failure.

## ⚙️ How it works

1. **Wake:** the Nano listens locally for “Jarvis” and plays a ready beep.
2. **Capture:** the ICS-43434 microphone sends your speech to the Nano over I2S.
3. **Request:** the Nano filters and streams audio to OpenAI Realtime over Wi-Fi and TLS.
4. **Reply:** OpenAI streams response audio back to the Nano.
5. **Playback:** the Nano sends processed audio to the MAX98357A amplifier and speaker.

| Stage | Implementation |
| --- | --- |
| Hardware audio | Shared-clock 48kHz Philips I2S, two 32-bit slots; TX initializes first as clock master |
| Microphone upload | Left-slot PCM24 → 63-tap low-pass FIR → mono 24kHz PCM16, uploaded in 100ms chunks |
| Input buffering | Separate capture/filter/upload stages; two queues, each bounded to two seconds |
| Reply playback | Filtered 24→48kHz conversion on core 1; mono duplicated into stereo slots |
| Playback buffering | 600ms initial preparation, two-second prepared queue, up to sixty seconds of raw reply PCM subject to PSRAM availability |
| Speaker level | 4× digital attenuation (−12dB), with 80ms I2S DMA buffering |
| Interruption | Local 16kHz echo cancellation and WakeNet on core 0; detected wake cancels playback and truncates conversation history to audio played |

Microphone uploads are gated during normal reply playback and a 500ms acoustic
tail. General speech does not yet trigger interruption. Errors stop audio and
close the socket. Standalone mode retries temporary connection failures with
bounded backoff; USB diagnostics do not reconnect automatically.

WebSocket callbacks copy bounded fragments into a queue; assembly and JSON
parsing happen outside the callback, with a 64KiB message limit and support for
split receive buffers and continuation frames. Input is committed only after
sample/byte counts match and both input queues drain. Overflow or upload failure
cancels the input rather than committing incomplete audio.

For architecture decisions and recorded hardware acceptance results, see
[the software strategy](docs/software-strategy.md).

## 🔍 Troubleshooting

| Symptom | What to check |
| --- | --- |
| Serial permission denied | Confirm the actual port and grant your Linux account access; USB re-enumeration may remove a temporary grant |
| Board will not flash | Enter download mode using B1 and RST, then inspect the board before retrying |
| Wi-Fi connection fails | Check the saved SSID/password and use a 2.4GHz network |
| TLS connection fails | Run `connect`; TLS requires synchronized time |
| OpenAI authentication or access fails | Check the project key, model access, and quota; run `check` before an audio turn |
| No microphone audio | Check D4/GPIO7, shared clocks, 3.3V power, and SEL/L-R to GND; run `stats` and record a WAV |
| Speaker audio is distorted | Check amplifier power, common ground, and wiring; use `tone-pico` and `loopback` to isolate the path |
| Reply has audible gaps | Inspect `Playback` preparation-gap logs and `Reply buffer` capacity under live network load |
| Standalone starts before a USB command | Use the five-second recovery window after the console resets the board |

Temporary Linux serial access:

```sh
sudo setfacl -m u:"$(id -un)":rw /dev/ttyACM0
```

For download mode, connect **B1 to GND**, press **RST** once, then remove the
B1–GND connection. Inspect the board before writing:

```sh
espflash board-info --port /dev/ttyACM0 --before no-reset --after no-reset --skip-update-check
```

## 🔐 Credentials & audio privacy

- Idle wake detection runs locally; idle wake audio stays on the Nano.
- Active requests send microphone audio directly to OpenAI over validated TLS.
- Provision credentials locally over USB; keep keys out of source, logs, and chat.
- Ordinary NVS does not protect credentials against physical extraction.
- `forget` removes the saved configuration blob.
- Standalone mode must be explicitly enabled; boot then starts local wake listening.

## 🧑‍💻 Development

### Project structure

```text
ESPal/
├── src/                      # Configuration, networking, sessions, standalone mode
├── crates/
│   ├── espal-audio/          # I2S, wake detection, echo cancellation, playback
│   └── espal-core/           # DSP, speech detection, protocol, configuration checks
├── tools/                    # USB setup, console, recording, and flash helpers
├── docs/
│   ├── pinout.md             # Wiring and hardware references
│   └── software-strategy.md  # Architecture and hardware acceptance history
├── partitions.csv            # Flash layout, including wake-model partition
├── sdkconfig.defaults        # ESP-IDF configuration
├── Cargo.toml                # Firmware dependencies and build profiles
└── LICENSE.md                # GNU GPL v3
```

### Build and host tests

```sh
. ./export-esp.sh
cargo build --locked
cargo +stable test --manifest-path crates/espal-core/Cargo.toml --target x86_64-unknown-linux-gnu --locked
```

`Cargo.lock` and `components_esp32s3.lock` pin the Rust and managed-component
builds. Host tests cover PCM alignment/gain/saturation, sample counts and filter
continuity, alias/image rejection, fragmented message limits, PCM event decoding,
and credential header/URL injection rejection.

Keep the per-crate speed optimization settings in `Cargo.toml`: whole-firmware
speed optimization caused a reproducible microphone-upload crash on the current
Xtensa toolchain. A successful build or host test does not establish physical
audio quality; verify relevant changes on the Nano.

### Remaining work

- Complete real-speech interruption and speaker echo-rejection acceptance tests.
- Add DMA overrun/underrun telemetry and measure memory/stack use under TLS load.
- Exercise Wi-Fi loss, slow playback, cancellation, and repeated-turn failures.
- Measure longer conversations and playback gaps under live network load.

## 📄 License

See [LICENSE.md](LICENSE.md) for the GNU General Public License, version 3.

## Acknowledgments

Built with the esp-rs ecosystem, Espressif ESP-IDF and ESP-SR, and OpenAI Realtime.
README presentation inspired by
[Ingeimaks's ESP32 AI Voice Assistant](https://github.com/Ingeimaks/ESP32-AI-Voice-Assistant).
