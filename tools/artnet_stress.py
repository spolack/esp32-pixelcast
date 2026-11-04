#!/usr/bin/env python3
"""Art-Net stress test for the esp32-pixelcast firmware.

Sends ArtDmx frames like a media server (all universes of a frame back to
back, optional ArtSync) and compares what was sent with what the board
reports on /api/status. Standard library only.

Examples:
  # 16 universes at 44 fps for 30 s, unicast
  tools/artnet_stress.py 192.168.1.50 steady -u 16 -f 44 -d 30

  # find the limit: 16 universes, 20..80 fps in steps of 5, 10 s each
  tools/artnet_stress.py 192.168.1.50 ramp -u 16 --start 20 --stop 80 --step 5

  # raw packet-rate limit: frames as fast as possible (floods the link;
  # on the setup Wi-Fi this mostly measures the radio, not the board)
  tools/artnet_stress.py 192.168.1.50 burst -u 16 -d 10

  # via the setup Wi-Fi
  tools/artnet_stress.py 192.168.4.1 steady -u 4 -f 30

The board's statistics cover 1-second windows, so short runs are noisy;
use at least 10 s per step. The board answers one HTTP connection at a
time, so this polls /api/status only every 2 s.
"""

import argparse
import json
import math
import socket
import statistics
import sys
import threading
import time
import urllib.request

ARTNET_PORT = 6454
OP_DMX = 0x5000
OP_SYNC = 0x5200


def art_dmx(universe: int, sequence: int, data: bytes) -> bytes:
    """One ArtDmx packet (protocol 14). `universe` is the 15-bit port address."""
    if len(data) % 2:
        data += b"\0"
    return (
        b"Art-Net\0"
        + OP_DMX.to_bytes(2, "little")
        + (14).to_bytes(2, "big")
        + bytes([sequence, 0])
        + bytes([universe & 0xFF, (universe >> 8) & 0x7F])
        + len(data).to_bytes(2, "big")
        + data
    )


def art_sync() -> bytes:
    return b"Art-Net\0" + OP_SYNC.to_bytes(2, "little") + (14).to_bytes(2, "big") + b"\0\0"


SACN_PORT = 5568
SACN_CID = bytes(range(16))


def sacn_data(universe: int, sequence: int, data: bytes, priority: int = 100) -> bytes:
    """One E1.31 data packet. `universe` is the sACN universe (1..63999)."""
    slots = b"\0" + data  # start code 0
    dmp = bytes([0x02, 0xA1]) + (0).to_bytes(2, "big") + (1).to_bytes(2, "big") + len(slots).to_bytes(2, "big") + slots
    dmp = (0x7000 | (len(dmp) + 3)).to_bytes(2, "big") + dmp
    framing_body = (
        (2).to_bytes(4, "big")
        + b"stress test".ljust(64, b"\0")
        + bytes([priority])
        + (0).to_bytes(2, "big")
        + bytes([sequence, 0])
        + universe.to_bytes(2, "big")
    )
    framing = (0x7000 | (len(framing_body) + 2 + len(dmp))).to_bytes(2, "big") + framing_body + dmp
    root_body = (4).to_bytes(4, "big") + SACN_CID
    root = (0x7000 | (len(root_body) + 2 + len(framing))).to_bytes(2, "big") + root_body + framing
    return (0x0010).to_bytes(2, "big") + (0).to_bytes(2, "big") + b"ASC-E1.17\0\0\0" + root


# LEDs the chase dot runs over (per output); set from --chase-leds.
CHASE_LEDS = 60


def frame_data(frame: int, universe: int, channels: int, pattern: str) -> bytes:
    """Channel data: a moving rainbow (RGBW), a full-white/black blink, or a
    chase: one white LED per output that moves one LED per frame (outputs
    start at a multiple of 4 universes, 128 RGBW LEDs per universe). With a
    too-short latch the strip skips frames and the dot jumps."""
    if pattern == "blink":
        return bytes([255 if frame % 2 else 0]) * channels
    if pattern == "chase":
        out = bytearray(channels)
        per = channels // 4
        led = frame % CHASE_LEDS - (universe % 4) * per
        if 0 <= led < per:
            out[led * 4 : led * 4 + 4] = b"\xff\xff\xff\xff"
        return bytes(out)
    leds = channels // 4
    out = bytearray(channels)
    for i in range(leds):
        h = ((universe * leds + i) * 3 + frame * 4) % 360
        r, g, b = hsv(h)
        out[i * 4 : i * 4 + 4] = bytes([r, g, b, 0])
    return bytes(out)


def hsv(h: int) -> tuple:
    x = int(255 * (1 - abs((h / 60) % 2 - 1)))
    return [(255, x, 0), (x, 255, 0), (0, 255, x), (0, x, 255), (x, 0, 255), (255, 0, x)][h // 60 % 6]


class Sender:
    """Sends frames at a fixed rate on a precise schedule (no drift)."""

    def __init__(self, target, universes, first, channels, sync, pattern, broadcast, protocol="artnet", multicast_if=None):
        self.protocol = protocol
        self.multicast = multicast_if is not None
        self.target = (target, SACN_PORT if protocol == "sacn" else ARTNET_PORT)
        self.universes = list(range(first, first + universes))
        self.channels = channels
        self.sync = sync
        self.pattern = pattern
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        if broadcast:
            self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 1 << 20)
        if self.multicast:
            self.sock.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_IF, socket.inet_aton(multicast_if))
            self.sock.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_TTL, 1)
        self.seq = 0
        self.frame = 0
        self.packets = 0
        self.errors = 0
        self.late = 0
        # Pre-build a few frames so packet building never limits the rate.
        # A chase repeats every CHASE_LEDS frames; cache exactly one round.
        count = CHASE_LEDS if pattern == "chase" else 90
        self.cache = [
            [frame_data(f, u, channels, pattern) for u in self.universes] for f in range(count)
        ]

    def send_frame(self):
        self.seq = self.seq % 255 + 1
        datas = self.cache[self.frame % len(self.cache)]
        for u, data in zip(self.universes, datas):
            try:
                if self.protocol == "sacn":
                    # sACN universe = port-address + 1.
                    to = (f"239.255.{(u + 1) >> 8}.{(u + 1) & 0xFF}", SACN_PORT) if self.multicast else self.target
                    self.sock.sendto(sacn_data(u + 1, self.seq, data), to)
                else:
                    self.sock.sendto(art_dmx(u, self.seq, data), self.target)
                self.packets += 1
            except OSError:
                self.errors += 1
        if self.sync:
            try:
                self.sock.sendto(art_sync(), self.target)
                self.packets += 1
            except OSError:
                self.errors += 1
        self.frame += 1

    def run(self, fps, duration, stop_event):
        """fps=None sends as fast as possible."""
        start = time.perf_counter()
        end = start + duration
        period = 1.0 / fps if fps else 0.0
        next_t = start
        while not stop_event.is_set():
            now = time.perf_counter()
            if now >= end:
                break
            if fps:
                if now < next_t:
                    # Sleep most of the wait, spin the rest for accuracy.
                    if next_t - now > 0.002:
                        time.sleep(next_t - now - 0.0015)
                    continue
                if now - next_t > period:
                    self.late += 1
                next_t += period
            self.send_frame()
        return time.perf_counter() - start


class Monitor:
    """Polls /api/status (sequentially, every 2 s) and keeps the samples."""

    def __init__(self, host, interval=2.0):
        self.url = f"http://{host}/api/status"
        self.interval = interval
        self.samples = []
        self.failures = 0
        self.latencies = []
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._loop, daemon=True)

    def start(self):
        self._thread.start()

    def stop(self):
        self._stop.set()
        if self._thread.is_alive():
            self._thread.join(timeout=5)

    def fetch(self):
        t0 = time.perf_counter()
        with urllib.request.urlopen(self.url, timeout=3) as r:
            body = json.loads(r.read())
        self.latencies.append(time.perf_counter() - t0)
        return body

    def _loop(self):
        while not self._stop.wait(self.interval):
            try:
                s = self.fetch()
                s["_t"] = time.time()
                self.samples.append(s)
            except Exception:
                self.failures += 1

    def reset(self):
        self.samples = []


def summarize(samples, sent_pps, universes, fps):
    """Board-side numbers over the samples, skipping the first (warm-up)."""
    a = [s["artnet"] for s in samples[1:] if s.get("artnet")]
    if not a:
        return None
    pps = [x["pps"] for x in a]
    return {
        "rx_pps": statistics.mean(pps),
        "rx_pps_min": min(pps),
        "fps_min": min(x["fps_min"] for x in a),
        "fps_max": max(x["fps_max"] for x in a),
        "universes": max(x["universes"] for x in a),
        "seq_gaps": sum(x["seq_gaps"] for x in a),
        "loss_pct": max(0.0, 100.0 * (1 - statistics.mean(pps) / sent_pps)) if sent_pps else 0.0,
        "heap_min": min(s["heap_free"] for s in samples),
        "network": a[-1]["network"],
    }


def run_step(args, sender, monitor, fps, duration):
    monitor.reset()
    stop = threading.Event()
    p0 = sender.packets
    elapsed = sender.run(fps, duration, stop)
    sent = sender.packets - p0
    sent_pps = sent / elapsed
    time.sleep(0.2)
    res = summarize(monitor.samples, sent_pps, args.universes, fps)
    return sent_pps, res


def fmt_row(label, sent_pps, res, expected_fps, monitored=True):
    if not monitored:
        return f"{label:>8} | sent {sent_pps:7.0f} pps (board not monitored)"
    if res is None:
        return f"{label:>8} | sent {sent_pps:7.0f} pps | board: no data (check IP / network)"
    flag = ""
    if res["loss_pct"] > 1.0 or res["seq_gaps"] > 0:
        flag = "  <-- loss"
    if expected_fps and res["fps_min"] < expected_fps * 0.9:
        flag += "  <-- slow universe"
    return (
        f"{label:>8} | sent {sent_pps:7.0f} pps | rx {res['rx_pps']:7.0f} pps "
        f"(min {res['rx_pps_min']:5d}) | loss {res['loss_pct']:5.1f}% | "
        f"fps {res['fps_min']:3d}-{res['fps_max']:3d} | univ {res['universes']:3d} | "
        f"gaps {res['seq_gaps']:5d} | heap>={res['heap_min']:6d} | {res['network']}{flag}"
    )


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("host", help="board IP (Ethernet address, or 192.168.4.1 on the setup Wi-Fi)")
    ap.add_argument("mode", choices=["steady", "ramp", "burst"])
    ap.add_argument("-u", "--universes", type=int, default=16, help="universes per frame (default 16)")
    ap.add_argument("--first", type=int, default=0, help="first universe (default 0)")
    ap.add_argument("-c", "--channels", type=int, default=512, help="channels per universe (default 512)")
    ap.add_argument("-f", "--fps", type=float, default=44, help="frames per second for steady (default 44)")
    ap.add_argument("-d", "--duration", type=float, default=20, help="seconds per run/step (default 20)")
    ap.add_argument("--start", type=float, default=20, help="ramp: first fps")
    ap.add_argument("--stop", type=float, default=80, help="ramp: last fps")
    ap.add_argument("--step", type=float, default=5, help="ramp: fps step")
    ap.add_argument("--sync", action="store_true", help="send ArtSync after each frame")
    ap.add_argument("--broadcast", metavar="ADDR", help="send to this broadcast address instead of the host")
    ap.add_argument("--pattern", choices=["rainbow", "blink", "chase"], default="rainbow")
    ap.add_argument("--chase-leds", type=int, default=60, help="chase: LEDs the dot runs over per output")
    ap.add_argument("--no-monitor", action="store_true", help="only send; do not poll the board")
    ap.add_argument("--protocol", choices=["artnet", "sacn"], default="artnet",
                    help="sacn: E1.31 to UDP 5568 (universe = port-address + 1); set the board to sACN first")
    ap.add_argument("--multicast", metavar="LOCAL_IP",
                    help="sacn: send to the multicast groups 239.255.x.y out of the interface with this address")
    args = ap.parse_args()
    global CHASE_LEDS
    CHASE_LEDS = max(1, args.chase_leds)

    target = args.broadcast or args.host
    sender = Sender(target, args.universes, args.first, args.channels, args.sync, args.pattern, bool(args.broadcast),
                    args.protocol, args.multicast)
    monitor = Monitor(args.host)

    if not args.no_monitor:
        try:
            s = monitor.fetch()
            print(f"Board: {s['name']} · firmware {s['firmware']['version']} ({s['firmware']['slot']}) · "
                  f"Art-Net on '{s['artnet']['network'] or '-'}' · heap free {s['heap_free']}")
        except Exception as e:
            print(f"Cannot reach http://{args.host}/api/status: {e}", file=sys.stderr)
            return 1
        monitor.start()

    pkt = len(art_dmx(0, 1, bytes(args.channels)))
    print(f"Sending {args.universes} universes x {args.channels} ch ({pkt} B/packet) to {target}:{ARTNET_PORT}"
          f"{' + ArtSync' if args.sync else ''}")
    leds = args.channels // 4
    print(f"(= {args.universes * leds} RGBW LEDs; SK6812 line limit for 512 LEDs per output is ~48 fps)\n")

    try:
        if args.mode == "steady":
            sent_pps, res = run_step(args, sender, monitor, args.fps, args.duration)
            print(fmt_row(f"{args.fps:g} fps", sent_pps, res, args.fps, not args.no_monitor))
        elif args.mode == "burst":
            sent_pps, res = run_step(args, sender, monitor, None, args.duration)
            print(fmt_row("max", sent_pps, res, None, not args.no_monitor))
            if res:
                print(f"\nBoard handled ~{res['rx_pps']:.0f} packets/s = "
                      f"~{res['rx_pps'] / args.universes:.0f} fps for {args.universes} universes")
        else:
            steps = []
            f = args.start
            while f <= args.stop + 1e-9:
                steps.append(f)
                f += args.step
            limit = None
            for f in steps:
                sent_pps, res = run_step(args, sender, monitor, f, args.duration)
                print(fmt_row(f"{f:g} fps", sent_pps, res, f, not args.no_monitor), flush=True)
                if res and (res["loss_pct"] > 1.0 or res["seq_gaps"] > 0) and limit is None:
                    limit = f
            if args.no_monitor:
                pass
            elif limit:
                print(f"\nFirst losses at {limit:g} fps with {args.universes} universes.")
            else:
                print("\nNo losses in the tested range.")
    except KeyboardInterrupt:
        print("\nstopped")
    finally:
        monitor.stop()

    if not args.no_monitor:
        lat = monitor.latencies
        if lat:
            print(f"\nWeb API during the test: {len(lat)} replies, median {statistics.median(lat) * 1000:.0f} ms, "
                  f"max {max(lat) * 1000:.0f} ms, {monitor.failures} failed")
    if sender.errors or sender.late:
        print(f"Sender: {sender.errors} send errors, {sender.late} late frames (PC could not keep the rate)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
