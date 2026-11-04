#!/usr/bin/env python3
"""Provision ESPal credentials locally over USB; never writes a secret file."""
import argparse
import getpass
import json
from pathlib import Path
import time
import serial


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", default="/dev/ttyACM0")
    parser.add_argument("--wifi-profile", type=int, choices=(1, 2, 3),
                        help="Update only Wi-Fi using WIFI_SSID_N and WIFI_PASSWORD_N from .env")
    args = parser.parse_args()
    if args.wifi_profile is not None:
        env = {}
        for line in (Path(__file__).resolve().parent.parent / ".env").read_text().splitlines():
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            key, sep, value = line.partition("=")
            if sep:
                value = value.strip()
                if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
                    value = value[1:-1]
                env[key.strip()] = value
        config = {"ssid": env[f"WIFI_SSID_{args.wifi_profile}"],
                  "password": env[f"WIFI_PASSWORD_{args.wifi_profile}"]}
        command, acknowledgment = "wifi ", b"ESPAL_WIFI_SAVED"
    else:
        config = prompt_config()
        command, acknowledgment = "provision ", b"ESPAL_PROVISIONED"
    send_config(args.port, command, acknowledgment, config)


def prompt_config():
    return {
        "ssid": input("2.4 GHz Wi-Fi SSID: "),
        "password": getpass.getpass("Wi-Fi password (empty for open network): "),
        "api_key": getpass.getpass("Dedicated OpenAI project API key: "),
        "model": input("Realtime model [gpt-realtime-2.1]: ") or "gpt-realtime-2.1",
        "voice": input("Voice [marin]: ") or "marin",
        "instructions": input("Instructions [Brief answers in the speaker's language]: ") or "Reply briefly in the language spoken by the user.",
        "mic_gain": int(input("Microphone gain, 1–32 [1]: ") or "1"),
    }


def send_config(device, command, acknowledgment, config):
    wire = (command + json.dumps(config, ensure_ascii=True, separators=(",", ":")) + "\n").encode()
    if len(wire) > 2049:
        raise ValueError("Provisioning message exceeds device command limit")
    port = serial.Serial()
    port.port = device
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
            if acknowledgment in line:
                print("Settings saved on the Nano. No secret file created.")
                return
            if b"Provisioning failed:" in line or b"Wi-Fi setting failed:" in line:
                # Do not print arbitrary serial data while handling secrets.
                raise RuntimeError("Device rejected provisioning; check lengths, model, voice and gain")
        raise TimeoutError("Device did not acknowledge provisioning")
    finally:
        port.close()


if __name__ == "__main__":
    main()
