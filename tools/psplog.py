#!/usr/bin/env python3
"""psxrt.log reader: one record per logged second, grouped into scenes.

A scene is the executable that ran the second ("native: LEV01.EXE at ..."); seconds with MDEC
work are kept apart as "<exe>/fmv". For each scene: seconds, speed (mean, worst 10%, min), fps,
and the ms/s of the runtime's parts. "code" is the rest of the second: translated code, and
whatever no part covers (draw, mdec and spu are inside io; update's host part, the frontend's
frame, is counted once). Logs older than update's parts count that frame twice, so their code
is low. The end says how the run stopped: a bench line, a halt, or nothing (switched off or
crashed), with the last event logged before it.

Logs with the profiler (psxrt.prof) add, per scene, the CPU counters a second (M cycles, k misses;
"gpu dma" is the share of the PS1 display lists walked into GE commands), the runtime's sampled
brackets ("rt": host calls, updates and the empty bracket that calibrates them), the native
line's calls (lean ones and timer/IRQ/pad host calls, where logged) and the top host call sites.
Seconds whose GE ring was cached are a scene of their own ("<scene>/ring-cached").

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
# code: the rest of the second, not in any measured part: translated code, mostly
COLS = (*MS_KEYS, "code")
UPD_KEYS = tuple("upd_" + k for k in ("vbl", "spu", "host", "timers", "dma", "cd", "sio"))
CPU_LINE = re.compile(
    r"^-- cpu \(k\): on \d+ ck (\d+) stall \d+ memory (\d+) .*?uncached (\d+)/(\d+) .*?i_miss (\d+)"
    r" d_miss (\d+) wb \d+; gpu dma ck (\d+) memory (\d+) i_miss (\d+) d_miss (\d+)")
CPU_KEYS = ("ck", "memory", "unc_r", "unc_w", "i_miss", "d_miss", "gd_ck", "gd_memory", "gd_i_miss",
            "gd_d_miss")
RT_PART = re.compile(r"(\w+) ck (\d+) memory (\d+) i_miss (\d+) d_miss (\d+) \(w (\d+) kept (\d+)")
RING = re.compile(r"; ring (\w+) ")
NATIVE = re.compile(r"^-- native: (\d+) calls(?: \((\d+) lean(?:, (\d+) timer/irq/pad host)?)?")
SITE = re.compile(r" ([0-9A-F]{8})/(\d+)")


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
            if cur is not None and line.startswith("-- "):
                m = CPU_LINE.match(line)
                if m:
                    cur.update(("cpu_" + k, int(v)) for k, v in zip(CPU_KEYS, m.groups()))
                    continue
                if line.startswith("-- rt "):
                    for part, *v in RT_PART.findall(line):
                        cur.update(("rt_%s_%s" % (part, k), int(x)) for k, x in zip(
                            ("ck", "memory", "i_miss", "d_miss", "w", "kept"), v))
                    continue
                if line.startswith("-- sites "):
                    cur["sites"] = {pc: int(n) for pc, n in SITE.findall(line)}
                    continue
                m = NATIVE.match(line)
                if m:
                    cur.update(("nat_" + k, int(v)) for k, v in zip(("calls", "lean", "host"), m.groups())
                               if v is not None)
                    continue
                m = RING.search(line)
                if m and line.startswith("-- ge:"):
                    cur["ring"] = m[1]
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
        s["scene"] = s["exe"] + ("/fmv" if s.get("mdec", 0) > 0 else "") + (
            "/ring-cached" if s.get("ring") == "cached" else "")
        if "interp" in s:  # update holds the frontend's frame (upd_host: present, vsync wait)
            parts = ("interp", "io", "update", "present", "sleep")
            s["code"] = max(0, 1000 - sum(s[k] for k in parts) + s.get("upd_host", 0))
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
        for k in COLS:
            v = [s[k] for s in ss if k in s]
            r[k] = sum(v) / len(v) if v else 0
        for k in {k for s in ss for k in s if k.startswith(("io_", "upd_"))}:  # not listed: 0 ms
            r[k] = sum(s.get(k, 0) for s in ss) / len(ss)
        for k in {k for s in ss for k in s if k.startswith(("cpu_", "rt_", "nat_"))}:  # where logged
            v = [s[k] for s in ss if k in s]
            r[k] = sum(v) / len(v)
        sites = {}
        for s in ss:
            for pc, n in s.get("sites", {}).items():
                sites[pc] = sites.get(pc, 0) + n
        if sites:
            r["sites"] = sorted(sites.items(), key=lambda p: -p[1])[:12]
            r["sites_secs"] = sum(1 for s in ss if "sites" in s)
        rows[scene] = r
    return rows


HEAD = "%-14s %4s %6s %6s %4s %5s" % ("scene", "s", "speed", "worst", "min", "fps") + "".join(
    " %6s" % k for k in COLS)


def fmt(scene, r):
    row = "%-14s %4d %5.0f%% %5.0f%% %3d%% %5.1f" % (
        scene, r["n"], r["speed"], r["worst10"], r["min"], r["fps"]) + "".join(
        " %6.0f" % r[k] for k in COLS)
    upd = [(k[4:], r[k]) for k in UPD_KEYS if k in r]
    if upd:  # update's parts; host is the frontend's frame (present, sleep) run inside it
        row += "\n%-14s update: %s (sum %.0f)" % ("", ", ".join("%s %.0f" % p for p in upd),
                                                   sum(v for _, v in upd))
    if "cpu_ck" in r:  # M cycles and k misses a second
        g = lambda k: r.get("cpu_" + k, 0)
        row += ("\n%-14s cpu: ck %.0fM memory %.0fM i_miss %.0fk d_miss %.0fk uncached %.0fk/%.0fk;"
                " gpu dma ck %.0fM memory %.0fM i_miss %.0fk d_miss %.0fk; rest ck %.0fM i_miss %.0fk"
                " d_miss %.0fk") % (
            "", g("ck") / 1e3, g("memory") / 1e3, g("i_miss"), g("d_miss"), g("unc_r"), g("unc_w"),
            g("gd_ck") / 1e3, g("gd_memory") / 1e3, g("gd_i_miss"), g("gd_d_miss"),
            (g("ck") - g("gd_ck")) / 1e3, g("i_miss") - g("gd_i_miss"), g("d_miss") - g("gd_d_miss"))
    rt = [p for p in ("calls", "update", "empty") if "rt_%s_ck" % p in r]
    if rt:  # sums already weighted to the second; empty: the brackets' own cost
        row += "\n%-14s rt: %s" % ("", "; ".join(
            "%s ck %.1fM memory %.1fM i_miss %.0fk d_miss %.0fk (kept %.0f)" % (
                p, r["rt_%s_ck" % p] / 1e3, r["rt_%s_memory" % p] / 1e3, r["rt_%s_i_miss" % p],
                r["rt_%s_d_miss" % p], r["rt_%s_kept" % p]) for p in rt))
    if "nat_calls" in r:
        row += "\n%-14s native: %.0f calls" % ("", r["nat_calls"]) + (
            ", %.0f lean" % r["nat_lean"] if "nat_lean" in r else "") + (
            ", %.0f timer/irq/pad host" % r["nat_host"] if "nat_host" in r else "")
    if "sites" in r:
        row += "\n%-14s sites (n/s, %d s): %s" % ("", r["sites_secs"], " ".join(
            "%s/%.0f" % (pc, n / r["sites_secs"]) for pc, n in r["sites"]))
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
                w = csv.DictWriter(fh, ["frame", "scene", "pc", "speed", "vbl", "fps", *COLS],
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
            moved = [(k, x.get(k, 0), y.get(k, 0)) for k in (*COLS, *sorted(
                {k for k in (*x, *y) if k.startswith(("io_", "upd_"))}))]
            moved = ["%s %.0f->%.0f" % m for m in moved if abs(m[2] - m[1]) >= 10]
            if moved:
                print("%-14s ms/s: %s" % ("", ", ".join(moved)))
            cpu = [(k[4:], x[k], y[k]) for k in ("cpu_" + c for c in CPU_KEYS) if k in x and k in y]
            cpu = ["%s %.0f->%.0f" % c for c in cpu if abs(c[2] - c[1]) >= 0.03 * max(c[1], 1)]
            if cpu:  # k cycles or k misses a second, moves of 3% or more
                print("%-14s cpu (k): %s" % ("", ", ".join(cpu)))
        print("base end:", be)
        print("log end: ", le)
        return 0
    print(__doc__)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
