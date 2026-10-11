# ESPal

Standalone Nano ESP32 voice assistant. Architecture and acceptance criteria are
in [the software strategy](docs/software-strategy.md); connections are in
[the pinout](docs/pinout.md).

## Initial firmware

The application supports USB bench commands and a saved standalone mode.
`python3 tools/console.py standalone on` enables offline Jarvis listening after
a five-second USB recovery window at boot. Say **Jarvis**, wait for the ready
beep, then speak naturally. A 1.2-second pause ends the request; follow-up
questions use the same OpenAI session. After 15 seconds without a new question,
the Nano returns to offline wake listening. Idle wake audio stays on the Nano.
Cedar remains the reply voice. Continuous input has a ten-minute limit, and
each conversation has an eleven-minute deadline.

`python3 tools/console.py standalone off` restores bench mode. Opening the
console resets the board; send this command within the five-second boot window.
The offline wake model uses ESP-SR 2.0.5 in a separate model partition; the flash
helper packs and writes it without changing NVS/PHY offsets. Boot and speech
acceptance for standalone mode are recorded in the software strategy.

Microphone recording and clean speaker speech are verified. TX-first duplex
initialization fixed speaker distortion. Both input and reply audio now stream
through bounded queues, with Cedar voice and 4× speaker attenuation.

- `help`: display the command descriptions.
- `stats`: run shared-clock duplex I2S for one second, transmitting silence,
  and log left-slot microphone sample count, peak, RMS, near-clipping and read
  timeouts. The signed 24-bit alignment is supported by the recognizable speech recording.
- `tone`: run the same diagnostic with a 200 ms, 440 Hz tone at 1% digital full
  scale and a 10 ms envelope. Actual speaker loudness depends on the amplifier.
- `record`: capture 5 seconds (240,000 samples) of native mono PCM24 with silent
  output, stop both channels, then export bounded hexadecimal chunks and an
  FNV-1a checksum over USB. Requires 720KB of available contiguous capture memory;
  allocation or capture failure aborts the recording.

Commands end with Enter. The command buffer is capped at 2048 bytes (including provisioning JSON). Both I2S
channels stop after each diagnostic, including an error. DMA overrun/underrun
telemetry remains to be implemented.

To record speech, run this locally, press Enter when ready, then speak immediately:

```sh
python3 tools/record.py
```

The tool requires `pyserial`, verifies the exact length and checksum before
writing a mono PCM24 WAV, and defaults to
`/home/leonmarq/Dokumente/Codex/ESPal/microphone-speech-5s.wav`. Choose a different
filename with `--output` for subsequent recordings; it refuses to overwrite.
The WAV applies 16× listening gain by default and reports clipped samples. Use
`--gain 1` for unmodified native samples, or a lower gain if speech clips.
No resampling is applied. USB port access is required.

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
Realtime I2S, wake detection and echo cancellation live in `crates/espal-audio`,
compiled for speed independently of the session/upload firmware. Keep these
profile settings: whole-firmware speed optimization caused a reproducible
microphone-upload crash on the current Xtensa toolchain.
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

`cargo build --locked` has passed for this scaffold. Flashing and boot are verified on the Nano: 16MB flash, DIO at 40MHz. The Nano’s 8MB octal PSRAM is enabled at 40MHz and passed the boot memory test. Do not treat a build as an audio test.

## Wi-Fi and first OpenAI turn

The integration is implemented, host-tested and flashed. The offline filtered
playback pipeline passes on the Nano. Live Wi-Fi, DHCP, synchronized time,
TLS certificate validation, OpenAI authentication and session configuration
were verified on 2026-10-09. Received reply PCM is clear in a VLC-tested WAV; the user confirmed clean physical speaker speech after initializing TX as duplex clock master.

```sh
python3 tools/provision.py
python3 tools/console.py connect
python3 tools/console.py check
python3 tools/console.py turn
```

Change the saved voice without re-entering credentials using
`python3 tools/console.py voice cedar` (or another supported Realtime voice).
The voice applies to subsequent turns and persists across restarts.

The setup tool prompts locally for a 2.4GHz Wi-Fi SSID/password and a dedicated
OpenAI project key. Password/key entry is hidden. A validated config is saved
atomically as one NVS blob; no secret file is created. Ordinary NVS does not
protect credentials against physical extraction. `forget` removes the blob.

Model, voice, instructions and microphone gain are provisioned settings. The
initial model is `gpt-realtime-2.1` from the current official examples; actual
project access must be checked. Microphone gain starts at 1 because the raw
recording played correctly in VLC. Keep credentials out of chat and source.

`connect` establishes Wi-Fi, DHCP and SNTP. TLS is not attempted without time
synchronization. `check` verifies OpenAI authentication and audio session configuration
without capturing or uploading microphone audio. `turn` opens a certificate/hostname-validated WebSocket to
OpenAI, configures mono 24kHz PCM16 with turn detection disabled, then prints
`LISTENING`. Speak naturally until finished. Separate tasks capture the
microphone, filter 48→24kHz with a 63-tap low-pass FIR, and upload 100ms base64
PCM chunks while you speak. Two bounded queues each hold up to two seconds
between stages. Upload temporarily uses the same priority as the audio workers;
its original priority is restored afterward. The turn is committed only after
all emitted native samples become the matching number of uploaded PCM bytes and both queues
drain. An overflow or upload failure cancels input and closes the session.
`Input streaming:` logs capture time, total time, buffer peaks and clipping.

Reply audio is filtered and packed on a separate task while the speaker task
services DMA. The conversion task is pinned to core 1, separate from Wi-Fi
on core 0. Playback starts after 600ms is prepared (or a shorter reply ends),
with a bounded two-second prepared queue. The raw PCM queue holds up to sixty
seconds (2.88MB), reduced automatically if necessary to leave 2MiB of free PSRAM
for prepared audio and networking. `Reply buffer:` reports the actual capacity.
This allows speech to begin before the complete reply arrives. Playback logs
report preparation gaps; filter and DMA tasks preserve sample order.
Speaker output is attenuated 4× (-12dB), with 80ms of I2S DMA buffering.
Microphone uploads stay gated during playback and a 500ms acoustic tail. The
eleven-minute conversation deadline and bounded packet queues remain enforced. Errors stop
audio and close the socket; automatic reconnect is disabled for bench testing.

Experimental interruption: say **Jarvis** while Cedar is speaking. A local
16kHz echo canceller receives microphone audio and the attenuated speaker
reference, aligned with the 80ms DMA preload. WakeNet examines the cleaned
signal on core 0 while reply conversion runs on core 1; the CPU runs at 240MHz.
Detection stops playback, cancels unfinished generation, and truncates the
assistant's conversation item to the portion played. Wait for the ready beep,
then say your next request. General speech does not trigger interruption yet.
`aec-check` checks synthetic echo suppression; `interrupt-check` runs twelve
seconds of quiet local playback with AEC/WakeNet and checks detector reuse.
`interrupt-network-check` repeats that test with Wi-Fi/TLS connected, then plays
an OpenAI reply to a fixed text prompt. It does not upload microphone audio.
Audio blocks larger than 1KiB and TLS allocations prefer PSRAM, with 64KiB
reserved for internal-only allocations. The echo task checks remaining internal
memory before playback starts. Network receive and speaker tasks run at audio
priority so echo processing cannot starve them.
Real speech and speaker echo rejection still require physical acceptance tests.

WebSocket callbacks only copy bounded fragments into a queue. JSON assembly
and parsing happen outside the callback, with a 64KiB message cap and explicit
support for receive-buffer splits and continuation frames. Server error text is
not logged; HTTP authentication/access/quota errors are redacted descriptions.

An optional `pipeline` command plays a brief quiet tone through the same
48→24→48kHz filters and bounded playback path without any network request.
`input-test` captures locally with a deliberately slow consumer, then injects
an upload failure and verifies the capture driver remains usable. It neither
uploads microphone audio nor plays it through the speaker.

## Build, flash and test

```sh
cargo build --locked
bash tools/flash.sh
cargo +stable test --manifest-path crates/espal-core/Cargo.toml --target x86_64-unknown-linux-gnu --locked
```

Always flash through `tools/flash.sh` (also the `cargo run` runner). It selects
the matching built ESP-IDF bootloader and the repository's `partitions.csv`,
which gives the factory app 3MiB; the old 1MiB diagnostic partition is too small
for Wi-Fi/TLS. NVS and PHY offsets stay the same. ESP-IDF's generated default
partition artifact is not used by the flash helper.

Host tests cover PCM alignment/gain/saturation, exact chunk counts and filter
continuity, alias/image rejection, fragmented message limits, PCM event decoding,
and credential header/URL injection rejection. `Cargo.lock` and
`components_esp32s3.lock` pin the Rust and managed-component builds.

## Remaining milestones

1. Verify a spoken API response and clear speaker playback (Wi-Fi/SNTP/TLS and account session access pass).
2. Add DMA overrun/underrun telemetry and measure memory/stack use under TLS load.
3. Replace the initial capture-then-upload bench turn with continuously serviced
   bounded input queues, then add finite multi-turn sessions and VAD.
4. Exercise Wi-Fi loss, slow playback, cancellation and repeated-turn failures.

Never commit credentials or upload microphone audio automatically at boot.

For speaker troubleshooting, `python3 tools/console.py loopback` verifies a
brief low-level signed stereo pattern through the internal GPIO input matrix.
It restores the microphone routing and never uploads microphone audio. This
checks digital sample transport, not external amplifier wiring or analog output.
`tone16` initializes a separate 16-bit TX-only path; reset before another audio
command in the same USB session after this isolated test.
