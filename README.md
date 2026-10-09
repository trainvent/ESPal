# ESPal

Standalone Nano ESP32 voice assistant. Architecture and acceptance criteria are
in [the software strategy](docs/software-strategy.md); connections are in
[the pinout](docs/pinout.md).

## Initial firmware

The current application boots idle and accepts USB commands. It does not
initialize audio until requested, and does not initialize Wi-Fi or OpenAI.
Initial device checks passed: idle boot and USB commands, 48,000 microphone samples per second with no read timeouts, and a clear quiet beep confirmed by the user. Recorded-speech quality and sample alignment remain unverified.

- `help`: display the command descriptions.
- `stats`: run shared-clock duplex I2S for one second, transmitting silence,
  and log left-slot microphone sample count, peak, RMS, near-clipping and read
  timeouts. The assumed signed 24-bit alignment still needs speech validation.
- `tone`: run the same diagnostic with a 200 ms, 440 Hz tone at 1% digital full
  scale and a 10 ms envelope. Actual speaker loudness depends on the amplifier.

Commands end with Enter. The command buffer is capped at 64 bytes. Both I2S
channels stop after each diagnostic, including an error. These are initial
bench commands; DMA overrun/underrun telemetry and recording export remain to
be implemented.

Build dependencies are locked in `Cargo.lock`. The initial stack uses
`esp-idf-svc` 0.53.0 and ESP-IDF v5.5.5. The development toolchain installed for
bring-up is Espressif Rust 1.97.0.0, with espup 0.18.0, espflash 4.6.0 and
ldproxy 0.3.5.

```sh
cargo +stable install espup --version 0.18.0 --locked
cargo +stable install espflash --version 4.6.0 --locked
cargo +stable install ldproxy --version 0.3.5 --locked
espup install --std --targets esp32s3 --toolchain-version 1.97.0.0 --export-file ./export-esp.sh
. ./export-esp.sh
cargo build --locked
```

The first build downloads ESP-IDF and its managed tools into `.embuild/`.
Run `. ./export-esp.sh` in every new terminal before building.

## USB bring-up

On Linux, the account needs access to the Nano's serial device. A temporary
grant, using the actual enumerated port, is:

```sh
sudo setfacl -m u:"$(id -un)":rw /dev/ttyACM0
```

USB re-enumeration may remove this grant. For firmware download mode, connect
**B1 to GND**, press **RST** once, then remove the B1–GND connection.
This is the [Arduino download-mode procedure](https://support.arduino.cc/hc/en-us/articles/9810414060188-Reset-the-Arduino-bootloader-on-the-Nano-ESP32).
Then inspect the board before writing:

```sh
espflash board-info --port /dev/ttyACM0 --before no-reset --after no-reset --skip-update-check
```

`cargo build --locked` has passed for this scaffold. Flashing and boot are verified on the Nano: 16MB flash, DIO at 40MHz. PSRAM
is disabled during initial bring-up. Do not treat a build as an audio test.

## Next milestones

1. Flash and verify idle boot logs; establish flash and PSRAM configuration.
2. Verify the microphone statistics and quiet tone on hardware, then add
   recording export and DMA overrun/underrun telemetry.
3. Add and test PCM conversion, filtered resampling and bounded audio queues.
4. Add local NVS provisioning, Wi-Fi/SNTP and verified TLS/WebSocket transport.
5. Complete a manually delimited OpenAI voice turn, then finite conversations.

Never commit credentials or upload microphone audio automatically at boot.
