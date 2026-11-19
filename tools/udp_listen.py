#!/usr/bin/env python3
"""Linux receiver / verifier for aigakira-telemetry-scs-udp datagrams.

    python3 tools/udp_listen.py                 # one status line per packet
    python3 tools/udp_listen.py --changes       # print only indicator/state changes
    python3 tools/udp_listen.py --raw           # dump raw JSON
    python3 tools/udp_listen.py --port 45454 --host 127.0.0.1

Reports invalid JSON, schema mismatch, oversize datagrams, sequence gaps
(dropped packets), reordering, plugin restarts (sid change) and stale data.
"""
import argparse
import json
import socket
import sys
import time

SCHEMA = "aigakira.scs.telemetry/1"
MAX_DATAGRAM = 1400
STALE_MS = 1000


def flag(on, label):
    return label if on else "-" * len(label)


def summary(p):
    d, c, i, w = p["drive"], p["cruise"], p["ind"], p["warn"]
    hazard = i["hazard"]
    return (
        f"seq={p['seq']:<7} {'PAUSED ' if p['paused'] else 'RUNNING'} truck={int(p['truck'])} "
        f"{d['speed_kmh']:6.1f}km/h {d['rpm']:5.0f}rpm gear={d['gear_label']:>3} "
        f"thr={p['effective']['throttle']:.2f} brk={p['effective']['brake']:.2f} "
        f"fuel={p['fuel']['l']:.0f}l({p['fuel']['pct']:.0f}%) "
        f"[{flag(i['lblinker'], '<')}{flag(hazard, 'HZ') if hazard is not None else 'n/a'}{flag(i['rblinker'], '>')}] "
        f"cc={'%.0f' % c['set_kmh'] if c['on'] else 'off'} "
        f"{flag(i['park_brake'], 'PARK')} {flag(i['engine_brake'], 'EBRK')} ret={i['retarder']}/{i['retarder_steps']} "
        f"{flag(p['lights']['low'], 'LOW')} {flag(p['lights']['high'], 'HIGH')} "
        f"warn={''.join(k[0].upper() for k, v in w.items() if v) or '-'} age={p['age_ms']}ms"
    )


def discrete(p):
    i, l = p["ind"], p["lights"]
    return {
        "paused": p["paused"], "truck": p["truck"], "alive": p["alive"],
        "gear": p["drive"]["gear_label"],
        "lblinker": i["lblinker"], "rblinker": i["rblinker"], "hazard": i["hazard"],
        "cruise_on": p["cruise"]["on"], "cruise_kmh": p["cruise"]["set_kmh"],
        "park_brake": i["park_brake"], "engine_brake": i["engine_brake"], "retarder": i["retarder"],
        "engine": i["engine"], "electric": i["electric"], "wipers": i["wipers"], "diff_lock": i["diff_lock"],
        **{f"light.{k}": v for k, v in l.items()},
        **{f"warn.{k}": v for k, v in p["warn"].items()},
        "fuel.warn": p["fuel"]["warn"],
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=45454)
    mode = ap.add_mutually_exclusive_group()
    mode.add_argument("--raw", action="store_true", help="print raw JSON")
    mode.add_argument("--changes", action="store_true", help="print only discrete state changes")
    args = ap.parse_args()
    sys.stdout.reconfigure(line_buffering=True)  # stay live when piped to a file/tee

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.bind((args.host, args.port))
    sock.settimeout(2.0)
    print(f"listening on udp://{args.host}:{args.port}  (Ctrl+C to stop)", file=sys.stderr)

    sid = last_seq = None
    received = dropped = 0
    prev = {}
    while True:
        try:
            data, _ = sock.recvfrom(65535)
        except socket.timeout:
            print("... no datagrams for 2 s (game closed, plugin not loaded, or wrong port?)", file=sys.stderr)
            continue
        except KeyboardInterrupt:
            break

        if len(data) > MAX_DATAGRAM:
            print(f"!! oversize datagram: {len(data)} bytes", file=sys.stderr)
        try:
            p = json.loads(data)
        except ValueError as e:
            print(f"!! invalid JSON ({e}): {data[:120]!r}", file=sys.stderr)
            continue
        if p.get("schema") != SCHEMA:
            print(f"!! unexpected schema {p.get('schema')!r}", file=sys.stderr)
            continue

        received += 1
        if p["sid"] != sid:
            if sid is not None:
                print(f"== plugin restarted (sid {sid} -> {p['sid']})", file=sys.stderr)
            print(f"== session sid={p['sid']} game={p['game']} schema={p['game_ver']}", file=sys.stderr)
            sid, last_seq, prev = p["sid"], None, {}
        elif last_seq is not None:
            gap = p["seq"] - last_seq - 1
            if gap > 0:
                dropped += gap
                print(f"!! {gap} datagram(s) dropped before seq {p['seq']}", file=sys.stderr)
            elif gap < 0:
                print(f"!! out-of-order/duplicate seq {p['seq']} after {last_seq}", file=sys.stderr)
        last_seq = p["seq"]
        if p["age_ms"] > STALE_MS:
            print(f"!! stale: game thread silent for {p['age_ms']} ms (loading screen?)", file=sys.stderr)

        if args.raw:
            print(data.decode())
        elif args.changes:
            cur = discrete(p)
            diff = {k: v for k, v in cur.items() if k not in prev or prev[k] != v}
            if diff:
                ts = time.strftime("%H:%M:%S")
                print(f"{ts} seq={p['seq']} " + " ".join(f"{k}={json.dumps(v)}" for k, v in diff.items()))
            prev = cur
        else:
            print(summary(p))

        if not p["alive"]:
            print(f"== plugin shut down cleanly (received {received}, dropped {dropped})", file=sys.stderr)


if __name__ == "__main__":
    main()
