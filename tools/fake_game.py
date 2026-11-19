#!/usr/bin/env python3
"""Sends fake aigakira.scs.telemetry/1 datagrams, so receivers (the Touch Portal
plugin, udp_listen.py) can be tested without running ETS2.

    python3 tools/fake_game.py                  # 127.0.0.1:45454 at 30 Hz, like the game plugin
    python3 tools/fake_game.py --port 50000 --hz 10

Speed ramps 0..120 km/h, gears follow speed, the right blinker toggles every
5 s, hazards every 13 s, cruise is on above 60 km/h. Ctrl+C sends a final
alive=false datagram, like the real game plugin on exit.
"""
import argparse
import json
import math
import random
import socket
import time

SCHEMA = "aigakira.scs.telemetry/1"


def payload(seq, sid, t, alive=True):
    kmh = round(60 + 60 * math.sin(t / 10), 1)
    gear = max(1, min(12, int(kmh / 10) + 1))
    cruise = kmh > 60
    blink = int(t * 2) % 2 == 0
    rblinker = int(t / 5) % 2 == 1
    hazard = int(t / 13) % 2 == 1
    fuel = max(0.0, 600 - t)
    return {
        "schema": SCHEMA, "seq": seq, "sid": sid, "ts": int(time.time() * 1000), "age_ms": 5,
        "alive": alive, "game": "ets2", "game_ver": "1.18", "paused": False, "truck": True,
        "drive": {"speed_ms": round(kmh / 3.6, 2), "speed_kmh": kmh, "rpm": round(900 + kmh * 8),
                  "rpm_max": 2500.0, "gear": gear, "dgear": gear, "gear_label": str(gear),
                  "gears_fwd": 12, "gears_rev": 2},
        "input": {"steer": round(0.3 * math.sin(t), 3), "throttle": 0.5, "brake": 0.0, "clutch": 0.0},
        "effective": {"steer": round(0.3 * math.sin(t), 3), "throttle": 0.5, "brake": 0.0, "clutch": 0.0},
        "fuel": {"l": round(fuel, 1), "cap_l": 800.0, "pct": round(fuel / 8, 1), "range_km": round(fuel * 3),
                 "avg_l100km": 31.4, "warn": fuel < 80},
        "cruise": {"on": cruise, "set_ms": 22.22 if cruise else 0.0, "set_kmh": 80.0 if cruise else 0.0},
        "ind": {"lblinker": False, "rblinker": rblinker, "hazard": hazard,
                "lblinker_lamp": hazard and blink, "rblinker_lamp": (rblinker or hazard) and blink,
                "park_brake": kmh < 1, "engine_brake": False, "retarder": int(t / 3) % 5, "retarder_steps": 4,
                "engine": True, "electric": True, "wipers": False, "diff_lock": False},
        "lights": {"parking": True, "low": True, "high": False, "beacon": False, "brake": False,
                   "reverse": False, "aux_front": 0, "aux_roof": 0},
        "warn": {"air": False, "air_emerg": False, "oil": False, "water": False, "battery": False,
                 "adblue": False},
        "nav": {"limit_kmh": 80.0},
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, action="append", help="repeatable; default 45454")
    ap.add_argument("--hz", type=float, default=30)
    args = ap.parse_args()
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    targets = [(args.host, p) for p in (args.port or [45454])]
    sid, seq, start = random.getrandbits(32), 0, time.monotonic()
    print(f"sending to {', '.join(f'udp://{h}:{p}' for h, p in targets)} at {args.hz} Hz (Ctrl+C to stop)")
    try:
        while True:
            data = json.dumps(payload(seq, sid, time.monotonic() - start)).encode()
            for t in targets:
                sock.sendto(data, t)
            seq += 1
            time.sleep(1 / args.hz)
    except KeyboardInterrupt:
        data = json.dumps(payload(seq, sid, time.monotonic() - start, alive=False)).encode()
        for t in targets:
            sock.sendto(data, t)
        print(f"\nsent {seq + 1} datagrams (last one alive=false)")


if __name__ == "__main__":
    main()
