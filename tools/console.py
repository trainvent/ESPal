#!/usr/bin/env python3
"""Run one ESPal USB bench command and show bounded, redacted device logs."""
import argparse
import time
import serial


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["help", "stats", "tone", "failure-sound", "waiting-sound", "tone16", "tone-pico", "pipeline", "loopback", "input-test", "wake-check", "wake-test", "aec-check", "interrupt-check", "interrupt-network-check", "standalone", "connect", "check", "turn", "forget", "voice"])
    parser.add_argument("voice", nargs="?", choices=["alloy", "ash", "ballad", "coral", "echo", "sage", "shimmer", "verse", "marin", "cedar", "on", "off"])
    parser.add_argument("--port", default="/dev/ttyACM0")
    args = parser.parse_args()
    if (args.command in ("voice", "standalone")) != (args.voice is not None):
        parser.error("Use voice NAME or standalone on/off")
    if args.command == "voice" and args.voice in ("on", "off"):
        parser.error("Invalid voice")
    if args.command == "standalone" and args.voice not in ("on", "off"):
        parser.error("Use standalone on or standalone off")
    if args.command == "turn":
        input("Press Enter when ready. Speak when LISTENING appears: ")
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
        command = f"{args.command} {args.voice}" if args.command in ("voice", "standalone") else args.command
        port.write((command + "\n").encode())
        port.flush()
        deadline = time.monotonic() + (720 if args.command == "turn" else 90 if args.command == "interrupt-network-check" else 40)
        while time.monotonic() < deadline:
            line = port.readline(4096)
            if line:
                print(line.decode("utf-8", "replace").strip(), flush=True)
                if args.command == "waiting-sound" and b"Retry waiting sound complete" in line:
                    return
                if args.command == "failure-sound" and b"Connection failure sound complete" in line:
                    return
                if args.command == "turn" and b"Reply interrupted:" in line:
                    return
                if args.command == "standalone" and line.strip() == f"ESPAL_STANDALONE {args.voice}".encode():
                    return
                if args.command == "interrupt-network-check" and b"Network interruption diagnostic complete" in line:
                    return
                if args.command == "interrupt-check" and b"Interruption diagnostic complete" in line:
                    return
                if args.command == "aec-check" and b"AEC diagnostic complete" in line:
                    return
                if args.command in ("wake-check", "wake-test") and b"Wake diagnostic complete" in line:
                    return
                if args.command == "voice" and line.strip() == f"ESPAL_VOICE {args.voice}".encode():
                    return
                if (b"OpenAI session ready;" in line and args.command == "check"
                        or b"Turn finished;" in line and args.command == "turn"
                        or b"synchronized time ready" in line and args.command == "connect"):
                    return
                if b"espal:" in line and b"E (" in line:
                    return
                if args.command == "tone-pico" and b"Pico-equivalent 32-bit TX tone complete;" in line:
                    return
                if args.command == "tone16" and b"16-bit TX-only tone complete;" in line:
                    return
                if args.command == "loopback" and b"I2S loopback complete;" in line:
                    return
                if args.command == "pipeline" and b"Filtered pipeline test complete:" in line:
                    return
                if args.command == "input-test" and b"Input streaming test complete;" in line:
                    return
                if args.command in ("help", "stats", "tone", "tone16", "forget") and b"espal" in line:
                    return
        raise TimeoutError("No command completion within the deadline")
    finally:
        port.close()


if __name__ == "__main__":
    main()
