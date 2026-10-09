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

Hardware bring-up is verified: ESP32-S3 revision v0.2, 16MB flash, DIO at 40MHz,
and 8MB octal PSRAM at 40MHz with a successful boot memory test. Firmware boots
idle and responds to USB diagnostics. Microphone tests captured 48,000 samples
per second without read timeouts or near clipping. User confirmed the quiet
speaker beep and recognizable recorded speech in VLC; VS Code playback was the
original recording issue. Both 1.5s and 5s PCM24 exports passed exact-length and
FNV-1a checks; yielding during export fixed an initial watchdog warning.

Rust now implements filtered PCM conversion (63-tap FIR, 48→24kHz PCM16 and
24→48kHz playback), bounded fragmented Realtime messages, local NVS provisioning,
Wi-Fi/DHCP/SNTP, certificate-validated TLS/WebSocket transport and a manual
five-second spoken turn. Eight host tests pass. The final networking build was
flashed and its USB commands verified. The on-device quiet filtered pipeline
test passed (48→24→48kHz), including complete playback queue draining.
Live Wi-Fi/DHCP/SNTP, TLS certificate validation, OpenAI authentication and
24kHz session configuration passed on 2026-10-09 using locally provisioned
credentials. The `check` command performs this verification without microphone
capture/upload. A spoken request and audible API response remain to verify.
The current bench turn captures before uploading 20ms chunks; continuous input
queues, multi-turn VAD sessions, capped reconnect backoff, DMA overrun/underrun
telemetry and TLS-load memory/stack measurements remain outstanding.

The full firmware exceeds the old 1MiB app partition. `tools/flash.sh` selects
the matching ESP-IDF bootloader and repository `partitions.csv` (3MiB factory app,
unchanged NVS/PHY offsets). Use this helper instead of the generated default
partition artifact. Credentials are entered with `tools/provision.py`, not stored
in source or a host file. Model/voice/instructions/gain are configurable; the
initial model follows current official examples and actual project access is
verified by successful session setup. The API turn is limited to 60 seconds, with gated microphone
uploads, 100ms playback priming, a two-second output buffer and clean cancellation.

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
