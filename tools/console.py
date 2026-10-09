#!/usr/bin/env python3
"""Run one ESPal USB bench command and show bounded, redacted device logs."""
import argparse
import time
import serial


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["help", "stats", "tone", "pipeline", "connect", "check", "turn", "forget"])
    parser.add_argument("--port", default="/dev/ttyACM0")
    args = parser.parse_args()
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
        if args.command == "turn":
            input("Press Enter when ready. Speak when LISTENING appears: ")
        port.write((args.command + "\n").encode())
        port.flush()
        deadline = time.monotonic() + (120 if args.command == "turn" else 40)
        while time.monotonic() < deadline:
            line = port.readline(4096)
            if line:
                print(line.decode("utf-8", "replace").strip(), flush=True)
                if b"OpenAI session ready;" in line or b"Turn finished;" in line or b"synchronized time ready" in line and args.command == "connect":
                    return
                if b"espal:" in line and b"E (" in line:
                    return
                if args.command in ("help", "stats", "tone", "pipeline", "forget") and b"espal" in line:
                    return
        raise TimeoutError("No command completion within the deadline")
    finally:
        port.close()


if __name__ == "__main__":
    main()
