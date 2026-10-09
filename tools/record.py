#!/usr/bin/env python3
"""Export an explicit ESPal recording to a checksum-verified PCM24 WAV."""
import argparse
import time
import wave
from pathlib import Path

import serial


def capture(port):
    port.write(b"record\n")
    deadline = time.monotonic() + 90
    data = bytearray()
    header = None
    checksum = 2166136261
    while time.monotonic() < deadline:
        line = port.readline(1025)
        if len(line) > 1024:
            raise ValueError("Oversized export line")
        if line.startswith(b"ESPAL_RECORD "):
            if header is not None:
                raise ValueError("Duplicate recording header")
            rate, frames, size = map(int, line.split()[1:])
            if (rate, frames, size) != (48000, 240000, 720000):
                raise ValueError("Unexpected recording dimensions")
            header = (rate, size)
        elif line.startswith(b"PCM "):
            if header is None:
                raise ValueError("PCM before header")
            chunk = bytes.fromhex(line[4:].decode().strip())
            if not chunk or len(chunk) > 48 or len(data) + len(chunk) > header[1]:
                raise ValueError("Invalid PCM chunk length")
            data.extend(chunk)
            for byte in chunk:
                checksum = ((checksum ^ byte) * 16777619) & 0xFFFFFFFF
        elif line.startswith(b"ESPAL_RECORD_END "):
            if header is None or len(data) != header[1]:
                raise ValueError("Incomplete recording")
            if int(line.split()[1], 16) != checksum:
                raise ValueError("Recording checksum mismatch")
            return header[0], data
        elif line:
            print(line.decode("utf-8", "replace").strip())
    raise TimeoutError("No complete recording within 90 seconds")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", default="/dev/ttyACM0")
    parser.add_argument("--gain", type=int, default=16, help="WAV listening gain (1–64; default 16)")
    parser.add_argument("--output", type=Path, default=Path("/home/leonmarq/Dokumente/Codex/ESPal/microphone-speech-5s.wav"))
    args = parser.parse_args()
    if not 1 <= args.gain <= 64:
        parser.error("Gain must be between 1 and 64")
    if args.output.exists():
        parser.error("Output already exists; choose a new filename")
    port = serial.Serial()
    port.port = args.port
    port.baudrate = 115200
    port.timeout = 0.5
    port.dtr = False
    port.rts = False
    port.open()
    try:
        time.sleep(0.7)
        port.reset_input_buffer()
        input("Press Enter, then speak immediately for 5 seconds: ")
        rate, data = capture(port)
    finally:
        port.close()
    clipped = 0
    output = bytearray()
    for offset in range(0, len(data), 3):
        sample = int.from_bytes(data[offset:offset + 3], "little", signed=True) * args.gain
        clipped += not -8388608 <= sample <= 8388607
        output.extend(max(-8388608, min(8388607, sample)).to_bytes(3, "little", signed=True))
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with wave.open(str(args.output), "wb") as wav:
        wav.setnchannels(1)
        wav.setsampwidth(3)
        wav.setframerate(rate)
        wav.writeframes(output)
    print(f"Saved {len(data) // 3} verified PCM24 samples to {args.output}")
    print(f"Listening gain: {args.gain}x; clipped samples: {clipped} (use a lower --gain if needed)")


if __name__ == "__main__":
    main()
