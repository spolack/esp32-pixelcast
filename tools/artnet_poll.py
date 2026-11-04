#!/usr/bin/env python3
"""Sends an ArtPoll and lists the ArtPollReply packets that come back, like
the node list of a lighting console. Standard library only.

    tools/artnet_poll.py 2.255.255.255      # broadcast on the 2.x network
    tools/artnet_poll.py 2.14.66.63         # one node
"""
import socket
import struct
import sys
import time

PORT = 6454


def main():
    target = sys.argv[1] if len(sys.argv) > 1 else "255.255.255.255"
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
    s.bind(("", PORT))
    s.settimeout(0.5)
    # ArtPoll: ID, OpPoll, ProtVer 14, Flags (send replies), DiagPriority.
    s.sendto(b"Art-Net\0" + struct.pack("<H", 0x2000) + bytes([0, 14, 0x02, 0x10]), (target, PORT))
    end = time.time() + 2
    n = 0
    while time.time() < end:
        try:
            p, (ip, _) = s.recvfrom(1024)
        except socket.timeout:
            continue
        if len(p) < 239 or p[:8] != b"Art-Net\0" or struct.unpack("<H", p[8:10])[0] != 0x2100:
            continue
        n += 1
        cstr = lambda b: b.split(b"\0")[0].decode(errors="replace")
        net, sub = p[18], p[19]
        ports = p[173]
        univ = [(net << 8) | (sub << 4) | (p[190 + i] & 0xF) for i in range(ports)]
        print(
            f"{ip:15} bind {p[211]}: '{cstr(p[26:44])}' / '{cstr(p[44:108])}'  "
            f"universes {univ}  mac {p[201:207].hex(':')}  "
            f"fw {p[16]}.{p[17]}  refresh {struct.unpack('>H', p[226:228])[0]} Hz  "
            f"report '{cstr(p[108:172])}'"
        )
    print(f"{n} replies")


if __name__ == "__main__":
    main()
