# Connection error notification

`connection-error.pcm` is the user-provided Universfield clip
`universfield-error-notification-010-206498.mp3`, converted to mono, 24 kHz,
signed 16-bit little-endian PCM. The firmware embeds it for offline playback.
Playback halves the clip's amplitude before applying the existing speaker
attenuation.

To regenerate from the original:

```sh
ffmpeg -i /path/to/universfield-error-notification-010-206498.mp3 \
  -ac 1 -ar 24000 -f s16le assets/connection-error.pcm
```
