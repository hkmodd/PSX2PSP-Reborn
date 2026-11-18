"""Probe 0 of the ME translator: me_probe.log (psp/me_probe/main.c) read into a go/no-go.

    python -I tools/me_probe_verdict.py me_probe.log

GO needs all of:
  1. the ME runs the translator with the CPU's hashes, frame by frame (M0 and M1 against C0);
  2. its projected cost at 45k polys/s, ME us/poly (M0) x 45000, is at most half the ME's second
     (GO_CORE: the other half for the queue, the waits and what the dump leaves out);
  3. the CPU loses less to the ME's memory traffic than it saves: the CPU translator's slowdown
     with the ME translating beside it (C4 against C2, both with the GE drawing), over a whole
     second of CPU, below the CPU's own translator time at 45k polys/s (C2 us/poly x 45000).
The ME also has to be up from boot to end, never taken back by the firmware (me end).
Exit 0 GO, 1 NO-GO, 2 the log lacks what the verdict needs.
"""
import sys

POLYS_PER_S = 45000
GO_CORE = 0.5
MEASURED_GPU_POLY_MS = (167, 349)  # psxrt's gpu poly time per second in the 3D scenes (design note)


def parse(path):
    """[(kind, {key: value})]: kind the words before the first key=value"""
    lines = []
    with open(path, encoding="utf-8", errors="replace") as f:
        for raw in f:
            words = raw.split()
            if not words:
                continue
            k = next((i for i, w in enumerate(words) if "=" in w), len(words))
            lines.append((" ".join(words[:k]), dict(w.split("=", 1) for w in words[k:] if "=" in w)))
    return lines


def num(v):
    try:
        return int(v, 10)
    except (TypeError, ValueError):
        return None


class Log:
    def __init__(self, lines):
        self.lines = lines

    def first(self, kind, **match):
        for k, kv in self.lines:
            if k == kind and all(kv.get(a) == b for a, b in match.items()):
                return kv
        return None

    def all(self, kind, **match):
        return [kv for k, kv in self.lines if k == kind and all(kv.get(a) == b for a, b in match.items())]


def main(argv):
    if len(argv) != 2:
        print(__doc__)
        return 2
    log = Log(parse(argv[1]))
    out, why_no, missing = [], [], []

    def say(s=""):
        out.append(s)

    info = log.first("info")
    if info:
        say("PSP model %s, fw %s, cpu %s MHz, bus %s MHz, GE eDRAM %s bytes, FCR31 %s; before boot RESETEN %s BUSCLKEN %s"
            % (info.get("model"), info.get("fw"), info.get("cpu"), info.get("bus"), info.get("edram"), info.get("fcr31"),
               info.get("reseten"), info.get("busclken")))
    mem = log.first("mem")
    if mem:
        say("memory: dump %s bytes, arenas %s of %s bytes, sizeof(Psx) %s" % (mem.get("dump"), mem.get("arenas"),
                                                                            mem.get("arena"), mem.get("psx")))
    if log.first("done") is None:
        last = log.lines[-1] if log.lines else ("", {})
        say("!! the log has no 'done': the probe stopped early (last line: %s %s)" % (last[0], last[1]))
    for t in log.all("me timeout"):
        say("!! the ME did not end job %s (progress %s, err %s)" % (t.get("job"), t.get("progress"), t.get("err")))
    if log.first("abort on the cpu") is not None:
        say("!! a CPU copy of the translator aborted")

    # a: the ME up, and its clock
    say()
    boots = log.all("me boot")
    for b in boots:
        say("a. ME boot (memory unlocked %s): %s handler words, %s read back wrong, alive %s after %s us"
            % (b.get("unlock"), b.get("words"), b.get("readback_bad"), b.get("alive"), b.get("us")))
    alive = any(b.get("alive") == "1" for b in boots)
    cal = log.first("me calib")
    tpu = None
    if cal and num(cal.get("us")):
        tpu = num(cal["count"]) / num(cal["us"])
        say("   ME clock: %s counts in %s us of the CPU's clock = %.2f counts/us" % (cal["count"], cal["us"], tpu))
    end = log.first("me end")
    end_ok = bool(end) and end.get("alive") == "1" and end.get("beat_ok") == "1" and end.get("err") == "0"
    if end:
        say("   at the end: alive %s, still counting %s, err %s, a job lost %s" % (end.get("alive"), end.get("beat_ok"),
                                                                                  end.get("err"), end.get("dead")))
    if not alive:
        why_no.append("the ME never came up")
    elif end is None:
        missing.append("me end")
    elif not end_ok:
        why_no.append("the ME did not stay ours to the end (me end)")
    if tpu is None:
        missing.append("me calib")

    # b: reading main RAM from the ME
    say()
    for name in ("read_unc", "read_cached", "copy"):
        b = log.first("b " + name)
        if b and tpu:
            c = num(b["count"]) / num(b["words"])
            us = num(b["count"]) / tpu
            say("b. %-11s %.2f counts/word, %.0f us/MB, %.1f MB/s, sum %s" % (name, c, us, 4 * num(b["words"]) / us,
                                                                       "ok" if b.get("sum_ok") == "1" else "WRONG"))
    d = log.first("b dma")
    if d:
        us = num(d["us"]) or 0
        say("b. DMACplus   %s bytes in %s LLIs, %s us (%.1f MB/s), status before %s after %s, sum %s"
            % (d["bytes"], d["llis"], d["us"], num(d["bytes"]) / us if us else 0, d["st0"], d["st"],
               "ok" if d.get("sum_ok") == "1" else "WRONG"))

    # e: the writeback
    say()
    for w in log.all("wb"):
        r = num(w["r"])
        dirty = num(w["dirty_us"])
        say("e. GE %s: 16 KB dirty then written back: Range %.2f us, All %.2f us (the dirtying %.2f us apart); "
            "Range of clean lines %.2f us; GE still drawing at the end %s"
            % ("drawing" if w["ge"] == "1" else "idle", (num(w["wb16k_us"]) - dirty) / r, (num(w["wball_us"]) - dirty) / r,
               dirty / r, num(w["clean_us"]) / r, "yes" if num(w["ge_tail_us"]) else "NO" if w["ge"] == "1" else "-"))
    ge = log.first("ge cal")
    if ge:
        say("   GE: %s draws of %s full-screen sprites in %s us" % (ge["draws"], ge["sprites"], ge["us"]))

    # c, d: the runs
    say()
    runs = {r["name"]: r for r in log.all("run")}
    frames = {}
    for f in log.all("frame"):
        frames.setdefault(f["name"], []).append(f)

    def us_poly(r):
        polys, t = num(r.get("polys")), num(r.get("t"))
        if not polys or t is None:
            return None
        if r.get("unit") == "count":
            return t / tpu / polys if tpu else None
        return t / polys

    for name in ("C0", "C1", "M0", "C2", "C3", "C4", "M1"):
        r = runs.get(name)
        if not r:
            continue
        u = us_poly(r)
        tail = "" if r.get("ge") != "1" or r.get("on") == "me" else ", GE still drawing at the end" if num(
            r.get("ge_tail_us")) else ", GE ENDED FIRST"
        say("%s on the %s (GE %s, ME %s): ok %s, %s frames, %s polys after the first, %s us/poly, %s/%s frames the sim's, "
            "arena peak %s%s%s" % (name, r["on"], "drawing" if r["ge"] == "1" else "idle", r["me"], r["ok"], r["frames"],
                                   r["polys"], "%.2f" % u if u else "?", r["matched"], r["frames"], r["peak"], tail,
                                   "" if r.get("err") == "none" else ", err " + r.get("err", "?")))
    c4 = log.first("c4")
    if c4:
        say("C4's end: the ME still translating %s (frame %s)" % (c4.get("me_busy_at_end"), c4.get("me_progress")))
    st = log.first("stream")
    if st and tpu:
        us = num(st["count"]) / tpu
        say("C3's ME: %s passes of %s bytes in %.0f us = %.1f MB/s from main RAM" % (st["passes"], st["bytes"], us,
                                                                                   num(st["passes"]) * num(st["bytes"]) / us))
    profs = {p["name"]: p for p in log.all("prof")}
    for name in ("C0", "C1", "C2", "C3", "C4"):
        p = profs.get(name)
        if p:
            ck, stall = num(p.get("r2")), num(p.get("r4"))
            say("   %s profiler: %s cycles, %s memory stall (%.0f%%), i miss %s, d miss %s" % (
                name, ck, stall, 100.0 * stall / ck if ck else 0, p.get("r13"), p.get("r14")))

    # 1. hashes
    say()
    base = frames.get("C0")
    hash_ok = True
    if not base:
        missing.append("C0's frames")
    for name in ("C1", "C2", "C3", "C4", "M0", "M1"):
        fs = frames.get(name)
        if base is None or fs is None:
            if name in ("M0", "M1"):
                missing.append(name + "'s frames")
            continue
        diff = [f["n"] for f, g in zip(fs, base) if (f["h"], f["mh"], f["polys"]) != (g["h"], g["mh"], g["polys"])]
        same = not diff and len(fs) == len(base)
        say("1. %s's lists against C0's: %s" % (name, "identical, %d frames" % len(fs) if same else
                                                "DIFFER at frames %s (%d against %d)" % (diff[:8], len(fs), len(base))))
        if not same and name in ("M0", "M1"):
            hash_ok = False
    if not hash_ok:
        why_no.append("the ME's lists are not the CPU's")

    # 2. the ME's time
    c0, c2, m0 = runs.get("C0"), runs.get("C2"), runs.get("M0")
    me_up = us_poly(m0) if m0 else None
    cpu_up = us_poly(c0) if c0 else None
    core = None
    if me_up and cpu_up:
        core = me_up * POLYS_PER_S / 1e6
        say("2. ME %.2f us/poly against the CPU's %.2f (x%.2f): at %d polys/s the ME is busy %.0f ms of its second"
            " (GO at most %.0f)" % (me_up, cpu_up, me_up / cpu_up, POLYS_PER_S, core * 1000, GO_CORE * 1000))
        if core > GO_CORE:
            why_no.append("the ME needs %.0f ms a second at %d polys/s" % (core * 1000, POLYS_PER_S))
    else:
        missing.append("M0 or C0 with polys")

    # 3. the CPU's loss against its saving
    c4r = runs.get("C4")
    if c2 and c4r and us_poly(c2) and us_poly(c4r):
        saved = us_poly(c2) * POLYS_PER_S / 1000
        delta = (num(c4r["t"]) - num(c2["t"])) / num(c2["t"])
        lost = max(delta, 0) * 1000
        say("3. the CPU translator with the ME translating beside it: %+.1f%% (C4 %s us against C2 %s us);"
            " over a CPU second %.0f ms lost against %.0f ms saved (C2 %.2f us/poly x %d; psxrt measured %d-%d ms/s)"
            % (delta * 100, c4r["t"], c2["t"], lost, saved, us_poly(c2), POLYS_PER_S, *MEASURED_GPU_POLY_MS))
        p2, p4 = profs.get("C2"), profs.get("C4")
        if p2 and p4:
            say("   memory stall C2 %s -> C4 %s cycles (%+d)" % (p2.get("r4"), p4.get("r4"), num(p4["r4"]) - num(p2["r4"])))
        if c4 and c4.get("me_busy_at_end") != "1":
            say("   !! the ME ended M1 before C4 ended: the overlap was partial, the loss is a lower bound")
        c3 = runs.get("C3")
        if c3 and num(c3.get("t")):
            say("   worst case, the ME copying at full rate (C3): %+.1f%%" % (100 * (num(c3["t"]) - num(c2["t"])) / num(c2["t"])))
        if lost >= saved:
            why_no.append("the CPU loses %.0f ms a second to the ME, more than the %.0f it saves" % (lost, saved))
    else:
        missing.append("C2 and C4")

    for name in ("me edram", "me prof"):
        e = log.first(name)
        if e:
            say("%s: %s" % (name, " ".join("%s=%s" % kv for kv in e.items())))

    say()
    if why_no:
        verdict, code = "NO-GO: " + "; ".join(why_no), 1
    elif missing:
        verdict, code = "INCOMPLETE: the log lacks " + ", ".join(missing), 2
    else:
        verdict, code = "GO", 0
    say("VERDICT " + verdict)
    print("\n".join(out))
    return code


if __name__ == "__main__":
    sys.exit(main(sys.argv))
