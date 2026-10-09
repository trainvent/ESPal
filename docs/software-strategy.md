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

User reports the wiring is connected. No firmware or hardware audio test has been run. Stable/nightly Rust are installed, but espup/espflash and the Espressif toolchain were not found during this session. The previously detected Nano serial symlink is not currently visible; USB enumeration must be restored before flash/monitor tests.

## Sources

- [Rust ESP-IDF stack](https://github.com/esp-rs/esp-idf)
- [Espressif Rust toolchain setup](https://docs.espressif.com/projects/rust/book/getting-started/toolchain.html)
- [ESP-IDF I2S](https://docs.espressif.com/projects/esp-idf/en/v5.4.2/esp32s3/api-reference/peripherals/i2s.html)
- [Espressif WebSocket component](https://github.com/espressif/esp-protocols/blob/master/components/esp_websocket_client/README.md)
- [OpenAI WebSockets](https://developers.openai.com/api/docs/guides/voice-websockets)
- [OpenAI Realtime client events](https://developers.openai.com/api/reference/resources/realtime/client-events)
