#!/usr/bin/env python3
"""Save received OpenAI PCM for VLC diagnosis, with no speaker playback."""
import argparse
import time
import wave
from pathlib import Path
import serial
from record import capture

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--port", default="/dev/ttyACM0")
parser.add_argument("--output", type=Path, default=Path("/home/leonmarq/Dokumente/Codex/ESPal/openai-reply.wav"))
args = parser.parse_args()
if args.output.exists():
    parser.error("Output exists; choose another --output filename")
input("Press Enter when ready; speak when LISTENING appears. Speaker stays silent: ")
port = serial.Serial()
port.port, port.baudrate, port.timeout = args.port, 115200, 0.5
port.dtr = port.rts = False
port.open()
try:
    time.sleep(1.5)
    port.reset_input_buffer()
    rate, pcm = capture(port, reply=True)
finally:
    port.close()
args.output.parent.mkdir(parents=True, exist_ok=True)
with wave.open(str(args.output), "wb") as wav:
    wav.setnchannels(1)
    wav.setsampwidth(2)
    wav.setframerate(rate)
    wav.writeframes(pcm)
print(f"Saved checksum-verified OpenAI PCM to {args.output}; open in VLC at low volume")
