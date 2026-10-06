# https_gps

Streams NMEA 0183 sentences from a GPS receiver over HTTPS, using an ESP32-S3.

The firmware joins a Wi-Fi network as a station, reads NMEA sentences from a
serial GPS module, and streams them to up to 32 concurrent HTTPS clients as a
chunked `text/plain` response. It is `no_std` Rust built on
[esp-hal](https://github.com/esp-rs/esp-hal), [embassy](https://embassy.dev),
[picoserve](https://github.com/sammhicks/picoserve) and
[mbedtls-rs](https://github.com/esp-rs/mbedtls-rs).

## Hardware

- An [M5Stack AtomS3R](https://docs.m5stack.com/en/core/AtomS3R), based on an ESP32-S3-PICO-1-N8R8.
- An [ATGM336H-6N-74 GPS](https://docs.m5stack.com/en/atom/Atomic_GPS_Base_v2.0) module at 115200 baud, wired as follows:

| GPS module pin | ESP32-S3 | Purpose |
|---|---|---|
| 2 (TXD) | GPIO5 | NMEA output → UART1 RX |
| 3 (RXD) | GPIO6 | UART1 TX → GPS |
| 9 (~RESET) | GPIO7 | Held high |

## How it works

- **GPS:** a task on a dedicated interrupt executor reads UART1. It assembles
  sentences of up to 128 bytes, drops malformed ones, and publishes each to a
  64-entry pub/sub channel. A raised interrupt priority and a lowered UART RX
  FIFO threshold keep the UART from overflowing while 32 TLS sessions are
  active.
- **HTTPS:** a pool of 32 server tasks accepts connections on port 443. Each
  task runs a TLS 1.3 handshake with mbedtls and then serves `GET /` with
  picoserve. Each subscriber drains every sentence that is waiting, encrypts
  them together, and sends them with one flush. Sentences go out as soon as
  they are read from the GPS.
- **TLS:** the server uses a self-signed ECDSA P-256 certificate that is
  embedded at build time. SHA, RSA and AES run on the ESP32-S3's crypto
  hardware. mbedtls allocates from PSRAM. Its record buffers are reduced to
  4 KB in and 2 KB out, from the 16 KB defaults.
- **Memory:** the TCP, HTTP and TLS buffers for each connection live in PSRAM.
  Internal RAM is kept for Wi-Fi and the network stack.
- **Stats:** CPU load (measured from an idle hook) and internal and PSRAM heap
  usage are logged every 5 s.

## Building and flashing

You need:
- The Xtensa Rust toolchain from [espup](https://github.com/esp-rs/espup).
- `espflash`.
- `cmake` and `ninja`. mbedtls is compiled from source because the record
  buffer sizes are not the defaults.

1. Generate the certificate and private key. They are written to `certs/`,
   which git ignores. List every IP address or hostname that clients will use
   to reach the board:

   ```sh
   tools/gen-cert.sh 192.168.1.50
   ```

2. Build and flash. The Wi-Fi credentials are read from the environment at
   build time and are never stored in the repository:

   ```sh
   source ~/export-esp.sh
   WIFI_SSID='my-network' WIFI_PASSWORD='secret' cargo run --release
   ```

   The board logs `HTTPS server listening on https://<ip>:443/` once it has an
   address.

## Using it

```sh
curl -N --cacert certs/server.crt.pem https://192.168.1.50/
```

`tools/nmea_client.py` is a test client that uses only the Python standard
library. It opens several connections at once, checks each sentence's
checksum, and prints statistics for each connection:

```sh
tools/nmea_client.py 192.168.1.50 -n 32 -d 120 -i 60   # 32 streams for 2 minutes
tools/nmea_client.py 192.168.1.50 -n 1 --show          # print the sentences
tools/nmea_client.py --help
```

By default the client verifies the board against `certs/server.crt.pem`.
`--insecure` skips the verification.

## Performance

These figures come from the last run on the hardware: 32 concurrent HTTPS
streams for 120 s using `tools/nmea_client.py`, with the certificate verified.
The connections were opened 0.2 s apart. Every connection negotiated TLS 1.3
with `TLS_AES_256_GCM_SHA384`, X25519 key exchange and an ECDSA P-256
certificate.

| Metric | Result |
|---|---|
| Sentences delivered | 61,920 across 32 streams |
| Bad checksums | 0 |
| Sentences dropped (subscriber lag) | 0 |
| UART FIFO overflows | 0 |
| CPU while idle | 0.2% |
| CPU while streaming 32 connections | 8.9–9.9% |
| CPU while the 32 connections open | 79–82% for about 10 s |
| TLS handshake time | 224–2,812 ms (handshakes wait for each other while the 32 connections open) |
| Internal heap | 47 of 104 KB (51 KB peak) |
| PSRAM, idle | 192 KB of 8 MB |
| PSRAM, 32 sessions | 730 KB, about 17 KB per session, all freed when the clients disconnect |

A ClientHello carrying a post-quantum key share (OpenSSL's 1,469-byte
`X25519MLKEM768` ClientHello) still completes the handshake within the 4 KB
incoming record buffer.

## Notes

- The ATGM336H on this board has a passive ceramic patch antenna. Indoors it
  reports `ANTENNA OPEN` and may not get a fix. That report is expected without
  an active antenna and does not affect the stream.
- The firmware embeds the TLS private key, and the key is not committed. A
  fresh clone needs `tools/gen-cert.sh` before it will build.
