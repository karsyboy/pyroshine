# UDP bandwidth calibration

Paired clients can measure how much video traffic the path to the host carries
before they choose a bitrate. The host sends frame-shaped UDP traffic through
its production video sender, the client reports what arrived, and the host
returns per-step measurements and a conservative bitrate suggestion. Nothing is
applied automatically. The source is `moonshine-core/src/calibration/`.

The older PyroWave HTTPS probe (`/pyrowave-bandwidth-probe`, see
[PyroWave compatibility](PYROWAVE_COMPATIBILITY.md#authenticated-calibration))
is unchanged and remains the fallback for clients and hosts without this
protocol. It measures TCP throughput, which hides UDP loss, bursts and receive
buffer limits; the UDP probe measures those directly.

## Protocol (version 1)

| Step | Transport | Content |
| --- | --- | --- |
| Discovery | HTTPS `/serverinfo`, paired clients only | `PyroshineUdpProbeVersion=1`, `PyroshineUdpProbeMaxKbps=3000000`; offered for every codec set |
| Start | HTTPS `GET /pyroshine-udp-probe` | Request below; reply holds the run identifier, a 32-byte secret key, the video UDP port and the schedule |
| Hello | UDP client → host video port | Authenticated; fixes the client's endpoint |
| Probe | UDP host → client | Frame-shaped datagrams, one stream shard each |
| Reports | UDP client → host, every 100 ms | Authenticated received counts per step; the host's consent signal |
| Stop | UDP client → host | Authenticated; ends the run at once |
| Result | HTTPS `GET /pyroshine-udp-probe-result` | Client statistics in, measurements and suggestion out |

Start parameters:

| Parameter | Domain | Meaning |
| --- | --- | --- |
| `version` | `1` | Required. Any other value is refused with status 400 and the supported version. |
| `maxkbps` | 1,000–3,000,000 | Top of the ladder, IP-level kbit/s. |
| `steps` | 1–8, default 5 | Ladder steps at `maxkbps × i / steps`. |
| `stepms` | 250–2000, default 1000 | Step duration; `steps × stepms` ≤ 12,000 ms. |
| `fps` | 10–480 | Frame rate to emulate. |
| `shape` | `burst` or `paced` | Conventional (H.264/HEVC/AV1) or PyroWave transmission. |
| `packetsize` | 256–9000, default 1392 | Moonlight `packetSize`; clamped by `stream.video.max_packet_size` like RTSP. |
| `encrypted` | 0/1 | The client would request video encryption; honored only when `stream.video.encrypt` is on, as in RTSP. |
| `qos` | 0/1 | Mark datagrams with the video DSCP class (CS5), as a QoS stream would be. |

Errors use the GameStream convention: HTTP 200 with `status_code` and
`status_message` on the XML root. 400 is an invalid request, 404 an unknown run
(or another client's), 409 an active or starting session, 429 a run already in
progress or more than 12 runs per client per minute, 503 a busy video port.

The datagram formats are in `calibration/wire.rs`. Probe datagrams carry a
24-byte header (`PYB1`, run tag, sequence, step, flags, index in frame, frame
number, scheduled frame offset in µs) and filler up to the stream's shard
size. Control datagrams are 76 bytes (`PYC1`, run identifier, kind, a strictly
increasing counter, eight per-step received counts) followed by the first 16
bytes of HMAC-SHA256 over them, keyed by the run secret. Both implementations
test one shared vector (`control_matches_the_cross_implementation_vector`).

## Authorization and traffic safety

- Runs start only over HTTPS with a paired client certificate. The result can
  be fetched only by the certificate that started the run.
- The run key reaches the client only over that TLS connection. The host
  accepts control datagrams only from the TLS peer's IP address, only with a
  valid tag and counter, and after the hello only from the hello's endpoint.
- The destination is the source of the authenticated hello. No parameter names
  an address or port, so a client cannot direct traffic at a third party, and
  the host sends nothing before the hello: there is no reflection or
  amplification primitive for unauthenticated senders.
- The host stops when no authenticated report arrived for one second
  (consent freshness), on a client stop, after a step lost more than 10 %, on
  a session start or service shutdown, and at the plan's end plus two seconds.
- One run exists at a time. A run is refused while a session exists, and a
  session start stops the run and waits (at most two seconds) until its socket
  is closed before binding the stream ports (`VideoPortArbiter`).
- Requests are validated before admission: bounded digits only, rates, steps,
  durations, frame sizes (≤ 48 MiB, ≤ 65,535 datagrams) and packet sizes.

## Traffic shape

The probe binds the configured video UDP port, so it uses the same firewall
rules and port forwards as the stream. It runs on its own thread and Tokio
runtime and sends through `UdpGsoSocket::send_batch`, the production sender:

- `burst`: each frame's datagrams are submitted at the frame's start in GSO
  chunks, as the conventional sender does.
- `paced`: each frame is spread across its interval by the production frame
  pacer, chunk by chunk (`ChunkPackets` in the start reply).

Datagrams have the stream's shard size for the requested `packetSize` (plus the
encryption prefix when it applies). Rates are IP-level, accounting UDP and IP
headers (28 or 48 bytes) per datagram like `SendStats::wire_bytes`; Ethernet
framing (38 bytes per packet) is used only when comparing against link speeds.
Frame starts follow a monotonic schedule (`timerfd`).

## Measurements

| Side | Per step |
| --- | --- |
| Host | Frames sent, datagrams attempted, accepted by the kernel (`Submitted`), failed, socket back-pressure events, frames started more than one interval late, maximum start lateness |
| Client | Datagrams received, duplicates, reordered, gap events and longest gap, complete frames, frames FEC could not have repaired, queueing delay p50/p95/max, frame-delivery spread |

Network loss is `Submitted − Received`; host submission failures and late
frames are reported separately and make a step `host_limited`, never `lossy`.
Kernel acceptance is not treated as delivery. Queueing delay is each
datagram's one-way delay against its own scheduled send time (frame start for
`burst`, its chunk's deadline for `paced`), minus the run's minimum, so clock
offset cancels. A frame is unrecoverable when it misses more datagrams than its
FEC share (`FecPercent` of data datagrams) could repair; the whole frame is
treated as one block, an approximation of the stream's per-block FEC. A step
the run ended in, or one without client statistics, is `incomplete`.

## Suggestion

A step is `clean` when it was sent as planned, network loss is at most 0.2 %,
at most one frame in 200 is unrecoverable, and the queueing delay p95 stays
below ¾ of a frame interval (at least 2 ms): a frame burst always waits for its
own serialization, but a queue that does not drain before the next frame shows
saturation before loss does. Otherwise it is `lossy`, `delayed`,
`host_limited`, `incomplete` or `not_run`.

The usable wire rate is the highest offered rate reached through clean steps
only. A clean step above a failed one marks the run unstable. The suggested
Moonlight bitrate removes per-datagram overhead (`wire bytes / (packetSize −
16)`), the stream's FEC parity (`stream.video.fec_percentage`; the maximum for
automatic FEC) and a 20 % reserve for encoder rate variation (key frames, scene
changes), audio and control traffic, other traffic and the test's short
duration. It is capped by any known physical link speed at either end and
rounded down to 500 kbit/s.

| Confidence | When |
| --- | --- |
| `high` | At least two clean steps, a failing step above them, stable |
| `medium` | Every step clean: the result is a lower bound |
| `low` | One clean step, an unstable ladder, or a host-limited ceiling |
| `none` | No clean step; no suggestion |

## Compatibility

| Client | Host | Result |
| --- | --- | --- |
| Pyrolight with UDP calibration | Pyroshine with `PyroshineUdpProbeVersion=1` | UDP calibration, any codec |
| Pyrolight with UDP calibration | Older Pyroshine, PyroWave-capable | HTTPS probe (unchanged) |
| Pyrolight with UDP calibration | Sunshine, GameStream | Calibration unavailable |
| Older Pyrolight or Moonlight | This host | Unaffected: discovery is a new optional element; the HTTPS probe is unchanged |
| A future client speaking another version | This host | Status 400 naming version 1; Pyrolight falls back to HTTPS |

No streaming protocol, capability bit, SDP attribute or moonlight-common-c
change is involved, and nothing in the media pipeline changes while no run is
active.

## Developer tool

`moonshine-udp-probe` (in `moonshine-tools`) is a terminal client with the
same receiver as the tests. Create an identity, add its fingerprint to a test
host's `paired_certs`, then run:

```sh
target/release/moonshine-udp-probe identity ./probe-id
target/release/moonshine-udp-probe run --host HOST --identity ./probe-id \
  --max-kbps 500000 --steps 6 --fps 60 --shape paced --repeat 3
```

`--json` prints one object per run. The tool sizes its receive buffer as
Moonlight's video socket does (2048 datagrams, 8192 for PyroWave) and warns
when `net.core.rmem_max` lowers it; otherwise its own buffer overflowing would
read as network loss.

## Validation

Automated (`cargo test -p moonshine-core calibration`, `webserver::bandwidth_tests`):

- Parameter domains, schedules, packet-size clamping, wire formats and the
  shared HMAC vector.
- Loopback runs through the production sender for both shapes; forged keys and
  foreign source addresses never receive traffic; consent timeout; client stop;
  session preemption releases the port; admission, rate limiting and result
  ownership.
- A simulated drop-tail bottleneck bounds the suggestion below its capacity.
- The recommendation rules (verdicts, conversion, confidence, link caps).
- A complete run through the real TLS handlers, version negotiation, and
  discovery only for paired peers, unchanged HTTPS probe discovery.

Manual acceptance: run `moonshine-udp-probe` (or Pyrolight's calibration) over
the real path (wired and Wi-Fi), repeat it three times, and compare the
suggestion with a stream at that bitrate: Moonlight's statistics should show no
sustained loss and no growing latency. Simulations (netem) treat GSO
super-packets on loopback as one packet; disable GSO for them with
`MOONSHINE_VIDEO_DISABLE_GSO=1`.
