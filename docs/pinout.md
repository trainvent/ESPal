# ESPal — Nano ESP32 audio pinout

Recorded 2026-10-09. User confirms these reused modules are connected.
Hardware: Arduino Nano ESP32, ICS-43434 I2S microphone, MAX98357A I2S amplifier, passive speaker.
Physical wiring and audio operation have not yet been tested by firmware.

## Connections

| Nano printed label | ESP32 GPIO | ICS-43434 | MAX98357A |
| --- | --- | --- | --- |
| D2 | GPIO5 | BCLK / SCK | BCLK |
| D3 | GPIO6 | WS / LRCL | LRC |
| D4 | GPIO7 | DOUT / SD | — |
| D5 | GPIO8 | — | DIN |
| 3V3 | — | VDD / 3V | — |
| VBUS | — | — | VIN |
| GND | — | GND and SEL / L-R | GND |

Rust uses ESP32 GPIO numbers, not Arduino digital pin numbers.
Mic SEL/L-R to GND selects its left slot.

```text
Nano D2 / GPIO5 ----+---- Mic BCLK/SCK
                   +---- Amp BCLK
Nano D3 / GPIO6 ----+---- Mic WS/LRCL
                   +---- Amp LRC
Nano D4 / GPIO7 <-------- Mic DOUT/SD
Nano D5 / GPIO8 --------> Amp DIN
Nano 3V3 --------------- Mic VDD/3V
Nano VBUS -------------- Amp VIN
Nano GND ----------+---- Mic GND
                   +---- Mic SEL/L-R
                   +---- Amp GND
Amp SPK+ -------- [passive speaker] -------- Amp SPK-
```

## Power and remaining module details

- VBUS supplies approximately 5V from USB while the Nano is USB powered. Nano VIN is a different pin.
- Microphone power and GPIO signals are 3.3V. Do not apply 5V to them.
- Neither speaker output goes to GND: connect the speaker across SPK+ and SPK-.
- Keep BCLK, WS and data wires short and share ground between both modules and the Nano.
- Leave amplifier SD/MODE and GAIN in their breakout's factory configuration pending identification of its pull resistors. These settings have not been verified.
- Speaker impedance/power rating and USB supply current budget remain unverified. Initial firmware should be silent and offer a brief low-amplitude tone explicitly.
- Playback will duplicate mono audio into both slots, supporting left/right/mixed amplifier selection when the amplifier is enabled.

## References

- [Arduino pinout](https://docs.arduino.cc/resources/pinouts/ABX00083-full-pinout.pdf)
- [Arduino USB power](https://support.arduino.cc/hc/en-us/articles/360014779679-Supply-USB-power-via-the-VUSB-VBUS-or-5V-pin-on-Nano-boards)
- [ICS-43434 datasheet](https://product.tdk.com/system/files/dam/doc/product/sw_piezo/mic/mems-mic/data_sheet/ds-000069-ics-43434-v1.2.pdf)
- [MAX98357A datasheet](https://www.analog.com/media/en/technical-documentation/data-sheets/MAX98357A-MAX98357B.pdf)
