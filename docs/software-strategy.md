# ESPal — Rust software strategy

Designed 2026-10-09. Source repository: /home/leonmarq/Code/ESPal.

## Architecture

ICS-43434 → Nano ESP32 → Wi-Fi/TLS → OpenAI Realtime API → Nano ESP32 → MAX98357A → speaker.
The Nano is the only local runtime compute device. A development PC is used to build, flash and inspect recordings; it is not part of normal operation.

## Recommended stack

| Responsibility | Proposed software | Reason |
| --- | --- | --- |
| Firmware | Rust with std on ESP-IDF/FreeRTOS | Access to established Espressif audio and networking drivers |
| Toolchain | espup, Espressif Xtensa Rust, cargo, ldproxy | ESP32-S3 needs an Xtensa toolchain; target xtensa-esp32s3-espidf |
| Flash/logs | espflash or cargo-espflash | USB flashing and monitor; verify Nano bootloader/partition compatibility first |
| Hardware | esp-idf-hal and esp-idf-sys | GPIO and I2S; small Rust FFI wrapper where HAL does not expose the required duplex API |
| Wi-Fi/config | esp-idf-svc | Wi-Fi, event loop, SNTP and NVS |
| Secure transport | ESP-IDF TLS (mbedTLS) and esp_websocket_client | Secure WebSockets, authentication headers and fragmented messages |
| Protocol | serde/serde_json, base64 | Typed subset of OpenAI events; capped allocations and explicit message limits |
| Audio DSP | Small Rust module | Signed sample conversion, gain, filtered sample-rate conversion, statistics |

Pin exact compatible crate, ESP-IDF and WebSocket component versions together at initial build; commit Cargo.lock. Do not assume the newest ESP-IDF release is supported by the chosen Rust bindings. Current esp-rs repository organization is evolving; verify crate publication/version paths during scaffold setup.

Rust application code will call the C-based ESP-IDF drivers underneath. This is Rust firmware, not a Rust-only implementation of Wi-Fi/TLS. Bare-metal esp-hal/Embassy is an alternative, but adds integration work for secure networking; it is not the first implementation choice here.

## Audio design

Start with shared-clock Philips I2S at 48kHz, stereo frames with two 32-bit slots: BCLK 3.072MHz. RX and TX must have matching clock/slot configuration and be allocated as a duplex pair. Use fixed DMA buffers in internal DMA-capable RAM. No external MCLK required by these modules.

Capture the left mic slot, interpret its signed 24-bit sample alignment, and convert to PCM16 with documented gain/saturation. Confirm alignment with a recorded speech test. Stream 24kHz mono PCM16 little-endian to OpenAI: low-pass filter then decimate 48→24kHz. For received 24kHz audio, interpolate/filter to 48kHz and duplicate mono samples into both TX slots. Avoid raw every-other-sample decimation.

A later measured optimization can use a compatible native I2S rate to remove conversion. Keep initial timing aligned with the prior diagnostic approach.

Use 20ms input chunks: 480 samples at 24kHz = 960 bytes PCM = 1280 base64 characters before JSON overhead. Start playback with roughly 100–200ms buffered; cap the playback queue to about 1–2 seconds. These are initial tuning targets. Never silently discard arbitrary speech chunks on overflow: log the condition and cancel/reset the affected response or session.

Separate audio servicing, WebSocket I/O and conversation control using bounded queues. WebSocket callbacks copy bounded fragments and return promptly; they do not perform playback or blocking audio writes. Handle both WebSocket fragmentation and application messages larger than one receive callback, with a strict maximum assembled message size.

Verify and enable the board's PSRAM configuration before relying on it. Use it only for suitable larger queues; keep DMA storage in memory supported by the selected I2S driver.

## OpenAI connection

Use a direct secure WebSocket connection to the Realtime API. Keep model, voice, language/instructions and session timeout configurable; resolve the exact available model at implementation time rather than bake in an assumed model name.

Use the current Realtime client/server event references as the contract, not snippets from the older beta API or the separate GPT-Live session protocol. Implement session configuration, audio append/commit, response request, audio delta/completion, error and disconnect handling. Use server VAD only after deterministic manual turns work.

The API audio format is mono PCM16 at 24kHz. JSON messages carry base64 audio. TLS must validate the certificate chain and hostname; establish system time via SNTP before connecting. Handle authentication failure without retry storms, and reconnect with capped exponential backoff.

Provision Wi-Fi credentials and a dedicated OpenAI project API key locally into NVS through a USB setup command. Never commit secrets, embed them into source, or print them in logs. Direct standalone authentication requires a credential on the device; ordinary NVS is not protection against physical extraction. Flash encryption/secure boot can be considered after bring-up. No token broker or relay is part of this design.

## Conversation behavior

States: idle → connecting → listening → waiting → speaking → listening; failures enter an error/reconnect state.

Initially activate sessions using a USB command for bench testing; later add an optional button without altering the audio wiring. Use a finite configurable session deadline (initial target 60 seconds). Gate microphone transmission from response start until playback has drained plus a 500ms acoustic tail. Continue servicing I2S while gated. This provides turn-taking, not acoustic echo cancellation. Defer interruption/barge-in until audio and playback accounting are reliable.

On stop/disconnect: close the session, flush stale audio and output silence. Do not automatically upload audio at boot.

## Implementation milestones and acceptance criteria

1. **Toolchain and USB:** reproducible Rust build, correct flash/PSRAM configuration, successful flashing, readable boot logs. No speaker output at boot.
2. **Audio diagnostics:** silence by default; explicit quiet tone command; live microphone peak/RMS/clipping; export a short recording for recognizable speech. Confirm shared-clock duplex operation and count timeouts/underruns.
3. **Local pipeline:** test sample alignment, conversion, filter behavior, volume bounds and ring-buffer overflow handling. Exercise simultaneous capture/playback without acoustic loopback.
4. **Secure networking:** Wi-Fi/SNTP, certificate-validated TLS and WebSocket session setup. Redacted errors and reconnection behavior. Measure free heap/PSRAM and stack use under TLS load.
5. **Single API turn:** upload a manually delimited spoken request, receive and play the response, verify language and clear audio. Check the actual account's model access.
6. **Conversation:** finite session, VAD, playback microphone gating, cancellation and clean shutdown. Test Wi-Fi loss, slow output, server errors and buffer overflow during repeated turns.

Host tests should focus on PCM/DSP, event parsing/fragment assembly and state transitions. Device tests establish electrical/audio behavior; successful compilation is not hardware validation.

## Proposed source layout

- src/main.rs: startup and task wiring
- src/board.rs: GPIO5/6/7/8 and board configuration
- src/audio/: I2S, PCM, resampling and queues
- src/network.rs: Wi-Fi, time and WebSocket lifecycle
- src/realtime.rs: typed OpenAI protocol subset
- src/conversation.rs: states, gating and deadlines
- src/config.rs: local provisioning and NVS
- sdkconfig.defaults, Cargo.toml, Cargo.lock: reproducible build settings

## Current status

USB/firmware/microphone bring-up is verified; speaker acceptance remains open.
The board is ESP32-S3 revision v0.2, with 16MB flash, DIO at 40MHz,
and 8MB octal PSRAM at 40MHz with a successful boot memory test. Firmware boots
idle and responds to USB diagnostics. Microphone tests captured 48,000 samples
per second without read timeouts or near clipping. User initially heard the beep, but later isolated tone tests are still distorted.
Recognizable microphone speech in VLC is confirmed; VS Code playback was the
original recording issue. Both 1.5s and 5s PCM24 exports passed exact-length and
FNV-1a checks; yielding during export fixed an initial watchdog warning.

Rust now implements filtered PCM conversion (63-tap FIR, 48→24kHz PCM16 and
24→48kHz playback), bounded fragmented Realtime messages, local NVS provisioning,
Wi-Fi/DHCP/SNTP, certificate-validated TLS/WebSocket transport and a manual
five-second spoken turn. Nine host tests pass. The final networking build was
flashed and its USB commands verified. The on-device quiet filtered pipeline
test passed (48→24→48kHz), including complete playback queue draining.
Live Wi-Fi/DHCP/SNTP, TLS certificate validation, OpenAI authentication and
24kHz session configuration passed on 2026-10-09 using locally provisioned
credentials. The `check` command performs this verification without microphone
capture/upload. Spoken requests produce complete responses after queue fixes,
and the exported received-response WAV sounds clear in VLC. Clean physical speaker
playback is now confirmed by the user after TX-master duplex initialization.
The current bench turn captures before uploading 100ms chunks; continuous input
queues, multi-turn VAD sessions, capped reconnect backoff, DMA overrun/underrun
telemetry and TLS-load memory/stack measurements remain outstanding.

The full firmware exceeds the old 1MiB app partition. `tools/flash.sh` selects
the matching ESP-IDF bootloader and repository `partitions.csv` (3MiB factory app,
unchanged NVS/PHY offsets). Use this helper instead of the generated default
partition artifact. Credentials are entered with `tools/provision.py`, not stored
in source or a host file. Model/voice/instructions/gain are configurable; the
initial model follows current official examples and actual project access is
verified by successful session setup. The API turn is limited to 60 seconds, with gated microphone
uploads, 100ms playback priming, a ten-second output buffer and clean cancellation.

Installed tools: espup 0.18.0, espflash 4.6.0, ldproxy 0.3.5, Espressif Rust
1.97.0.0. Build pins esp-idf-svc 0.53.0, ESP-IDF v5.5.5 and WebSocket component
1.6.1; Rust and component lockfiles record transitive versions. Manual B1-to-GND
plus RST initially enabled download mode; native USB reset now works. USB reset
can remove a temporary serial ACL. The local Python tools allow 1.5s for startup
including the PSRAM test. Normal operation is entirely Rust on the Nano.

## Sources

- [Rust ESP-IDF stack](https://github.com/esp-rs/esp-idf)
- [Espressif Rust toolchain setup](https://docs.espressif.com/projects/rust/book/getting-started/toolchain.html)
- [ESP-IDF I2S](https://docs.espressif.com/projects/esp-idf/en/v5.4.2/esp32s3/api-reference/peripherals/i2s.html)
- [Espressif WebSocket component](https://github.com/espressif/esp-protocols/blob/master/components/esp_websocket_client/README.md)
- [OpenAI WebSockets](https://developers.openai.com/api/docs/guides/voice-websockets)
- [OpenAI Realtime client events](https://developers.openai.com/api/reference/resources/realtime/client-events)

Playback investigation (2026-10-10): the original FIR interpolation took up to
23,958us per 20ms audio block in an offline device test, exceeding its timing
budget. Equivalent polyphase interpolation reduces the measured maximum to
9,937us. Nine host tests pass, including exact PCM agreement with the original
zero-stuffed FIR. The optimization is flashed; live speech playback still needs
verification. Playback diagnostics now report conversion time, PCM count, peak,
network gaps and queue high-water mark. Failed turns no longer print the success
marker before the actual error, so the host console preserves failure details.

The next live run confirmed `Playback overflow; response cancelled`: queued
PCM reached 100,800 bytes, exceeding the initial 96,000-byte/two-second limit.
The output budget is now 480,000 bytes (ten seconds), with 256 packet slots,
using the Nano's PSRAM to absorb generation bursts. Conversion under live
Wi-Fi load also reached 44,857us; timing counters remain enabled. Clear, complete
playback still requires a live rerun with the larger buffer.

A further live run failed in WebSocket reception with only 19,200 PCM bytes
queued, plus an upload watchdog warning. The fragment callback queue grew from
32 to 256 slots (each fragment is capped at 4096 bytes). Callback queue overflow
and invalid metadata now have separate errors. Uploads now use 100ms PCM chunks
and a 10ms scheduling pause at the configured 100Hz RTOS tick rate; this reduces
message count fivefold and gives a full tick between sends. Live verification
of these receive/upload changes remains pending.

The receive/upload changes allowed a complete reply, but the user reported
excessive loudness and unintelligible distortion. Speaker PCM is now attenuated
by 16 (-24dB) before I2S conversion. Hardware DMA buffers increased from six
to sixteen 5ms descriptors (30→80ms), covering previously measured 32–45ms
processing stalls. Playback diagnostics include the speaker divisor. These
changes need a listening check; excessive output level is the current leading
explanation for distortion.

The user confirmed the checksum-verified captured OpenAI reply WAV sounds clear
in VLC. Remaining distortion is therefore downstream of decoded incoming PCM.
Local zero-PCM (`stats`) and quiet-tone tests are requested to distinguish
playback processing from I2S wiring/amplifier/power behavior. No hardware cause
has yet been established.

2026-10-11 resumed bring-up: the interrupted flash was completed. The runtime
format-switch diagnostic was replaced by direct, standalone 16-bit TX-only
initialization. On-device `tone16` completes without driver errors; listening
confirmation is pending. The user reports zero-PCM `stats` is silent, the previous
32-bit duplex tone is distorted, amplifier wiring matches the documented pinout,
and the amplifier is MAX98357A. Current build and all nine host tests pass.

Listening result: standalone 16-bit TX tone was quiet but still somewhat
distorted. This does not prove a hardware fault, but rules out OpenAI/network/
resampling as the sole cause. Speaker impedance/power rating, amplifier supply
and wiring lengths are being checked before further playback changes.

Internal I2S loopback added and verified on 2026-10-11: D5/GPIO8 TX is routed
through the GPIO input matrix into I2S0 RX for a bounded low-amplitude signed
stereo pattern, then microphone D4/GPIO7 routing is restored. The live result
was 20,157 matched frames, zero invalid frames and zero silence frames after
data began. This verifies the tested local 32-bit TX/RX packing, sign and channel
sequence; it does not measure signal integrity at the external amplifier, analog
speaker output, or playback under Wi-Fi load. Hardware ratings/supply/wiring
checks remain pending. Command: `python3 tools/console.py loopback`.

Pico comparison (2026-10-11): `/home/leonmarq/Code/pi_ai_rpipico` uses 48kHz,
32-bit MSB-first I2S slots, signed PCM16 shifted left 16, and an integer 1kHz
triangle with peak512. `tone-pico` reproduces those samples using a precomputed
DMA block. The user confirmed this tone is clean; its 1.7-second stream completed
in 1699ms. Earlier on-the-fly sine tests are not sufficient evidence of a hardware
fault. As a diagnostic, speech playback now collects a complete bounded reply
(max ten seconds), prepares FIR-interpolated mono PCM48 before starting I2S,
and packs signed stereo words during DMA service without live FIR work. This
adds response startup latency and is an isolation step, not acceptance of the
planned continuous streaming architecture. Offline pipeline verification passes:
204ms preparation for 200ms PCM, 2315us maximum packing per 20ms block, 720ms
playback plus silence/tail, queue fully drained. Live speech listening is pending.

The first prepared-speech run did not reach playback: the response grew to
499,200 PCM bytes and tripped the initial ten-second cap. Diagnostic capacity
is now 960,000 bytes/twenty seconds with 512 packet slots; mono prepared PCM48
uses at most about 1.92MB plus the received PCM buffer, within the 8MB PSRAM
budget subject to allocation checks. The diagnostic API deadline is 120 seconds
and the USB turn monitor waits up to 180 seconds. These temporary bench limits
allow preparation and complete playback; a lower-latency streaming design still
remains outstanding. Live clean-speech acceptance is still pending.

Prepared speech still sounded distorted. Source inspection revealed HAL 0.47.0
initializes RX before TX; ESP-IDF 5.5.5 assigns the second channel the duplex
slave role. A small owned ESP-IDF wrapper (`src/duplex.rs`) now initializes TX
first, preserving identical shared 48kHz/32-bit Philips framing. Device channel
info confirms TX master (0), RX slave (1), BCLK 3,072,000Hz. Signed stereo
loopback passes with 20,160 matched frames, zero invalid frames and zero gaps.
This tests the clock-ownership difference from the clean standalone tone; it
does not yet prove clean speech. Microphone RX remains enabled for capture and
serviced during playback; no wiring change is requested for this test.

The user confirmed clean, understandable speaker speech after the TX-master
change. The level was somewhat quiet, so output attenuation is reduced from
16× to 8× (-24→-18dB, a 6dB increase). This confirms the initial manual API
turn works end to end; current capture/preparation latency, finite multi-turn
conversation/VAD, fault testing and runtime resource telemetry remain open.

Microphone decimation now stores all 48kHz input samples but computes the
63-tap convolution only for emitted 24kHz samples, halving convolution work.
An exact comparison against the full-rate reference covers three gains,
signed inputs and clipping counts; all ten core tests and the firmware build
pass. The firmware is flashed; on-device turn latency remains to be measured.

The user confirmed clean speech at 8× attenuation and requested another
doubling of speaker amplitude. Output attenuation is now 4× (-12dB),
another 6dB increase; the shared-clock and conversion configuration is unchanged.

The user confirmed a clean 18.6-second answer at divisor 4. Its diagnostic
full-response preparation took 18.812 seconds, motivating streaming playback.
Reply conversion/slot packing now runs on a separate task from DMA servicing:
400ms initial priming, a bounded two-second prepared queue, and the existing
twenty-second raw PCM queue cap. Cancellation drops the prepared receiver to
unblock the converter, and stops both I2S channels. Conversion retains FIR state
across packets and flushes its tail; playback still services gated RX and sends
500ms silence afterward. No full-response wait or ten-millisecond sleep per
20ms converted block remains; conversion yields one RTOS tick per 100ms audio.

All ten host tests and the firmware build pass. The flashed Nano's 1.2-second
local pipeline test primed 400ms in 188ms, played 58,560 output frames including
filter-tail padding in 1.720 seconds including acoustic tail, reported zero
preparation gaps, and drained the raw queue. Maximum measured conversion batch
was 9,831 microseconds. Live Wi-Fi/OpenAI speech quality and gap counts still
require a user listening test; local timing is not a network-load guarantee.
