#!/usr/bin/env python3
"""Provision ESPal credentials locally over USB; never writes a secret file."""
import argparse
import getpass
import json
import time
import serial


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", default="/dev/ttyACM0")
    args = parser.parse_args()
    config = {
        "ssid": input("2.4 GHz Wi-Fi SSID: "),
        "password": getpass.getpass("Wi-Fi password (empty for open network): "),
        "api_key": getpass.getpass("Dedicated OpenAI project API key: "),
        "model": input("Realtime model [gpt-realtime-2.1]: ") or "gpt-realtime-2.1",
        "voice": input("Voice [marin]: ") or "marin",
        "instructions": input("Instructions [Brief answers in the speaker's language]: ") or "Reply briefly in the language spoken by the user.",
        "mic_gain": int(input("Microphone gain, 1–32 [1]: ") or "1"),
    }
    # Raw voice worked in VLC. Start without added gain; tune only if measured.
    wire = ("provision " + json.dumps(config, ensure_ascii=True, separators=(",", ":")) + "\n").encode()
    if len(wire) > 2049:
        raise ValueError("Provisioning message exceeds device command limit")
    port = serial.Serial()
    port.port = args.port
    port.baudrate = 115200
    port.timeout = 0.2
    port.dtr = False
    port.rts = False
    port.open()
    try:
        time.sleep(1.5)
        port.reset_input_buffer()
        port.write(wire)
        port.flush()
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            line = port.readline(4096)
            if b"ESPAL_PROVISIONED" in line:
                print("Credentials saved on the Nano. No secret file created.")
                return
            if b"Provisioning failed:" in line:
                # Do not print arbitrary serial data while handling secrets.
                raise RuntimeError("Device rejected provisioning; check lengths, model, voice and gain")
        raise TimeoutError("Device did not acknowledge provisioning")
    finally:
        port.close()


if __name__ == "__main__":
    main()
