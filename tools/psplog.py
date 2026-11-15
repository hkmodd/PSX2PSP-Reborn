#!/usr/bin/env python3
"""psxrt.log reader: one record per logged second, grouped into scenes.

A scene is the executable that ran the second ("native: LEV01.EXE at ..."); seconds with MDEC
work are kept apart as "<exe>/fmv". For each scene: seconds, speed (mean, worst 10%, min), fps,
and the ms/s of the runtime's parts. The end says how the run stopped: a bench line, a halt, or
nothing (switched off or crashed), with the last event logged before it.

    python -I tools/psplog.py LOG              table by scene
    python -I tools/psplog.py LOG --csv OUT    also every second as CSV
    python -I tools/psplog.py BASE LOG         LOG against BASE, scene by scene
"""
import csv
import re
import sys

F_LINE = re.compile(r"^-- f (\d+) pc (\S+) +(\d+)% +(\d+) vbl +(\d+) fps")
MS_LINE = re.compile(
    r"^-- ms: interp (\d+) io (\d+) \(draw (\d+) mdec (\d+) spu (\d+)\) update (\d+) present (\d+)"
    r".*?sleep (\d+);")
EXE = re.compile(r"^native: (\S+?)(?:\.EXE)? at ([0-9A-F]{8})")
STOP = re.compile(r"^STOP: (.*)")
BENCH = re.compile(r"^-- bench (\d+) s at frame (\d+): speed (\d+)% vbl (\d+) fps ([\d.]+)")
IO_PAIR = re.compile(r"(\w+) (\d+)/(\d+)")
UPD_PAIR = re.compile(r"(\w+) (\d+)")
MS_KEYS = ("interp", "io", "draw", "mdec", "spu", "update", "present", "sleep")
UPD_KEYS = tuple("upd_" + k for k in ("vbl", "spu", "host", "timers", "dma", "cd", "sio"))


def read(path):
    """The log's seconds, as dicts, and how it ended."""
    secs, exe, cur, last_event, end = [], "boot", None, "", None
    with open(path, encoding="latin-1") as fh:
        for raw in fh:
            line = raw.rstrip("\r\n")
            m = F_LINE.match(line)
            if m:
                cur = {"frame": int(m[1]), "pc": m[2], "speed": int(m[3]), "vbl": int(m[4]),
                       "fps": int(m[5]), "exe": exe}
                secs.append(cur)
                continue
            m = MS_LINE.match(line)
            if m and cur is not None and "interp" not in cur:
                cur.update(zip(MS_KEYS, map(int, m.groups())))
                continue
            if line.startswith("-- io (n/ms):") and cur is not None:
                for kind, _, ms in IO_PAIR.findall(line[13:]):
                    cur["io_" + kind] = int(ms)
                continue
            if line.startswith("-- update (ms):") and cur is not None:
                for part, ms in UPD_PAIR.findall(line[15:]):
                    cur["upd_" + part] = int(ms)
                continue
            m = EXE.match(line)
            if m:
                exe = m[1]
            if line.startswith("-- "):
                m = BENCH.match(line)
                if m:
                    end = "bench %s s: speed %s%% fps %s" % (m[1], m[3], m[5])
                continue
            m = STOP.match(line)
            if m:
                end = (end + "; " if end else "") + "STOP: " + m[1]
            elif line:
                last_event = line
    for s in secs:
        s["scene"] = s["exe"] + ("/fmv" if s.get("mdec", 0) > 0 else "")
    if not end:
        end = "no end line: switched off or crashed after f%s; last event: %s" % (
            secs[-1]["frame"] if secs else "?", last_event)
    return secs, end


def summary(secs):
    """Per scene, in order of first appearance."""
    out = {}
    for s in secs:
        out.setdefault(s["scene"], []).append(s)
    rows = {}
    for scene, ss in out.items():
        sp = sorted(s["speed"] for s in ss)
        worst = sp[: max(1, len(sp) // 10)]
        r = {"n": len(ss), "speed": sum(sp) / len(sp), "worst10": sum(worst) / len(worst),
             "min": sp[0], "fps": sum(s["fps"] for s in ss) / len(ss)}
        for k in MS_KEYS:
            v = [s[k] for s in ss if k in s]
            r[k] = sum(v) / len(v) if v else 0
        for k in {k for s in ss for k in s if k.startswith(("io_", "upd_"))}:  # not listed: 0 ms
            r[k] = sum(s.get(k, 0) for s in ss) / len(ss)
        rows[scene] = r
    return rows


HEAD = "%-14s %4s %6s %6s %4s %5s" % ("scene", "s", "speed", "worst", "min", "fps") + "".join(
    " %6s" % k for k in MS_KEYS)


def fmt(scene, r):
    row = "%-14s %4d %5.0f%% %5.0f%% %3d%% %5.1f" % (
        scene, r["n"], r["speed"], r["worst10"], r["min"], r["fps"]) + "".join(
        " %6.0f" % r[k] for k in MS_KEYS)
    upd = [(k[4:], r[k]) for k in UPD_KEYS if k in r]
    if upd:  # update's parts; host is the frontend's frame (present, sleep) run inside it
        row += "\n%-14s update: %s (sum %.0f)" % ("", ", ".join("%s %.0f" % p for p in upd),
                                                   sum(v for _, v in upd))
    return row


def main(argv):
    args = [a for a in argv if not a.startswith("--")]
    csv_out = argv[argv.index("--csv") + 1] if "--csv" in argv else None
    if csv_out:
        args.remove(csv_out)
    if len(args) == 1:
        secs, end = read(args[0])
        print(HEAD)
        for scene, r in summary(secs).items():
            print(fmt(scene, r))
        print("end:", end)
        if csv_out:
            with open(csv_out, "w", newline="") as fh:
                w = csv.DictWriter(fh, ["frame", "scene", "pc", "speed", "vbl", "fps", *MS_KEYS],
                                   extrasaction="ignore")
                w.writeheader()
                w.writerows(secs)
        return 0
    if len(args) == 2:
        (bs, be), (ls, le) = read(args[0]), read(args[1])
        b, l = summary(bs), summary(ls)
        print("%-14s %13s %13s %13s" % ("scene", "speed", "worst 10%", "fps"))
        for scene in list(b) + [s for s in l if s not in b]:
            if scene not in b or scene not in l:
                print("%-14s only in %s" % (scene, "base" if scene in b else "log"))
                continue
            x, y = b[scene], l[scene]
            print("%-14s %4.0f->%3.0f%% %+4.0f %4.0f->%3.0f%% %+4.0f %5.1f->%4.1f" % (
                scene, x["speed"], y["speed"], y["speed"] - x["speed"], x["worst10"],
                y["worst10"], y["worst10"] - x["worst10"], x["fps"], y["fps"]))
            moved = [(k, x.get(k, 0), y.get(k, 0)) for k in (*MS_KEYS, *sorted(
                {k for k in (*x, *y) if k.startswith(("io_", "upd_"))}))]
            moved = ["%s %.0f->%.0f" % m for m in moved if abs(m[2] - m[1]) >= 10]
            if moved:
                print("%-14s ms/s: %s" % ("", ", ".join(moved)))
        print("base end:", be)
        print("log end: ", le)
        return 0
    print(__doc__)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
