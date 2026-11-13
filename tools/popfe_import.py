"""Regenerates resources/popfe.ini from a git checkout of pop-fe.

    git clone --depth 1 --filter=blob:none https://github.com/sahlberg/pop-fe.git <dir>
    python -I tools/popfe_import.py <dir> resources/popfe.ini

pop-fe's gamedb.py is parsed as data (ast.literal_eval of each top-level dict entry), never
executed. Files are read through `git cat-file`, so names that collide by case or are not valid on
Windows still resolve. What is imported, as pop-fe uses it on PSP:
- `games[code]['pspconfig']`: the POPS config of each product code;
- `libcrypt[code]['magic_word']`: the LibCrypt word POPS answers the protection's subchannel
  reads from; its optional krHACKen `ppf` (an executable crack) becomes an opt-in patch;
- `ppf_fixes[id]`: PPF fixes for the codes whose `games[code]['id']` is `id`, unless tagged for
  other consoles only; `hashes` limits a fix to the dumps with that track-1 MD5;
- Terracon: pop-fe builds SCES02836 as SCPS02836 on PSP.
Codes whose file is missing, xdelta fixes and zipped external PPFs are reported and skipped.
"""
import ast
import os
import subprocess
import sys
import textwrap


# pop-fe.py: on PSP, Terracon (SCES02836) is built with the Japanese ID.
POPS_IDS = {'SCES02836': 'SCPS02836'}


def literal_dicts(path):
    tree = ast.parse(open(path, encoding='utf-8').read())
    out = {}
    for node in tree.body:
        if isinstance(node, ast.Assign) and isinstance(node.value, ast.Dict) and isinstance(node.targets[0], ast.Name):
            d = {}
            for k, v in zip(node.value.keys, node.value.values):
                d[ast.literal_eval(k)] = ast.literal_eval(v)
            out[node.targets[0].id] = d
    return out


def git(repo, *args, data=None):
    return subprocess.run(['git', '-C', repo, *args], input=data, capture_output=True, check=True).stdout


def blobs(repo, paths):
    """{path: bytes} for every path that exists, through one `git cat-file --batch`."""
    req = ''.join(f'HEAD:{p}\n' for p in paths).encode('utf-8')
    raw, out, i = git(repo, 'cat-file', '--batch', data=req), {}, 0
    for p in paths:
        nl = raw.index(b'\n', i)
        head = raw[i:nl].split()
        if head[-1] == b'missing':
            i = nl + 1
            continue
        size = int(head[2])
        out[p] = raw[nl + 1:nl + 1 + size]
        i = nl + 1 + size + 1
    return out


def wrap(key, text):
    """`key = text` folded into indented continuation lines."""
    text = ' '.join(text.split())
    lines = textwrap.wrap(text, 96, initial_indent=f'{key} = ', subsequent_indent='    ', break_long_words=False, break_on_hyphens=False)
    return lines or [f'{key} =']


def hexlines(key, data, per=32):
    return [f'{key} ='] + ['    ' + data[i:i + per].hex(' ') for i in range(0, len(data), per)]


def main():
    repo, dest = sys.argv[1], sys.argv[2]
    db = literal_dicts(os.path.join(repo, 'gamedb.py'))
    games, libcrypt, fixes = db['games'], db['libcrypt'], db['ppf_fixes']
    games = {k: g for k, g in games.items() if isinstance(g, dict)}
    commit = git(repo, 'log', '-1', '--format=%H %cs').decode().split()
    tree = set(git(repo, 'ls-tree', '-r', '--name-only', '-z', 'HEAD').decode('utf-8').split('\0'))
    lower = {p.lower(): p for p in sorted(tree)}
    resolve = lambda ref: ref if ref in tree else lower.get(ref.lower())
    skipped = []

    # ── [game]: config file, LibCrypt word and POPS ID, grouped by all three ──
    config_of = {}
    for code, g in games.items():
        ref = g.get('pspconfig')
        if ref:
            path = resolve(ref)
            if path is None:
                skipped.append(f'{code}: config {ref} not in pop-fe')
            else:
                config_of[code] = path
    groups = {}
    for code in sorted(set(config_of) | set(libcrypt) | set(POPS_IDS)):
        lc = libcrypt.get(code, {}).get('magic_word')
        if lc is not None and not 0 <= lc <= 0xFFFF:
            raise SystemExit(f'{code}: LibCrypt magic word {lc} out of range')
        groups.setdefault((config_of.get(code), lc, POPS_IDS.get(code)), []).append(code)

    # ── [patch]: ppf_fixes (by canonical id) and the LibCrypt cracks ──
    patches = []  # (codes, name, kind, md5s, file, note)
    for fid, f in fixes.items():
        if 'tags' in f and 'psp' not in f['tags']:
            continue
        codes = sorted(c for c, g in games.items() if g.get('id') == fid) or [fid]
        by_file = {}
        if 'ppf' in f:
            if 'hashes' in f:
                raise SystemExit(f'{fid}: both a ppf and per-dump hashes, unknown precedence')
            by_file[f['ppf']] = []
        for md5, e in f.get('hashes', {}).items():
            if 'ppf' in e:
                by_file.setdefault(e['ppf'], []).append(md5.lower())
            else:
                skipped.append(f'{fid}: {", ".join(e)} fix {"".join(e.values())} (not a PPF)')
        for ppf, md5s in by_file.items():
            stem = ppf.rsplit('/', 1)[-1].rsplit('.', 1)[0]
            patches.append((codes, f'pop-fe fix {stem}', 'fix', md5s, ppf, f.get('desc', '')))
    for code, e in sorted(libcrypt.items()):
        if 'ppfzip' in e:
            skipped.append(f'{code}: LibCrypt crack {e["ppfzip"][-1]} (in a zip outside pop-fe)')
        if 'ppf' in e:
            note = (f'Removes the LibCrypt check from the game ({e.get("credit", "krHACKen")}; redump '
                    f'{e["url"]}). Normally not needed: the EBOOT carries the LibCrypt subchannel table '
                    f'POPS answers the check from. Made for the redump dump; skipped if its validation '
                    f'block does not match.')
            patches.append(([code], 'LibCrypt crack (krHACKen)', 'other', [], e['ppf'], note))

    files = sorted({p[4] for p in patches} | {c for c, _, _ in groups if c})
    paths = {f: resolve(f) for f in files}
    data = blobs(repo, sorted({p for p in paths.values() if p}))
    kept = []
    for p in patches:
        body = data.get(paths[p[4]] or '')
        if body is None:
            skipped.append(f'{",".join(p[0])}: {p[4]} not in pop-fe')
        elif not body.startswith((b'PPF10', b'PPF20', b'PPF30')):
            raise SystemExit(f'{p[4]}: not a PPF file')
        else:
            kept.append((*p, body))

    lines = [
        '; Per-game data from pop-fe (https://github.com/sahlberg/pop-fe: gamedb.py, pspconfigs/, ppf/,',
        f'; libcrypt/), commit {commit[0][:12]} of {commit[1]}. GENERATED by tools/popfe_import.py: do not edit.',
        '; Corrections go in patches.ini (its [game] sections win over this file) or in the user',
        '; patches.ini. pop-fe is LGPL-2.1; most configs are the ones Sony shipped in its PS1 Classics,',
        '; the rest, the fixes and the LibCrypt data come from the community work credited in pop-fe.',
        f'; {sum(map(len, groups.values()))} product codes in {len(groups)} [game] sections '
        f'({len({c for c, _, _ in groups if c})} config files, {sum(1 for k in groups if k[1] is not None)} LibCrypt), '
        f'{len(kept)} PPF patches.',
    ]
    for (cfg, lc, pops_id), codes in sorted(groups.items(), key=lambda kv: (kv[0][0] or '~' + games.get(kv[1][0], {}).get('title', kv[1][0])).lower()):
        title = cfg.split('/')[1] if cfg else games.get(codes[0], {}).get('title', '')
        lines += ['', f'[game {" ".join(codes)}]']
        if title:
            lines.append(f'title = {title}')
        src = ['pop-fe ' + cfg] if cfg else []
        if lc is not None:
            src.append('pop-fe gamedb.py libcrypt')
        lines.append(f'source = {", ".join(src) if src else "pop-fe.py"}')
        if lc is not None:
            lines.append(f'libcrypt = 0x{lc:04X}')
            urls = sorted({libcrypt[c]['url'] for c in codes})
            lines += wrap('note', f'LibCrypt protected (redump {" ".join(urls)}).')
        if pops_id:
            lines.append(f'pops_id = {pops_id}')
        if cfg:
            c = data[paths[cfg]]
            if not 0 < len(c) <= 992:
                raise SystemExit(f'{cfg}: {len(c)} bytes is not a POPS config')
            lines.append(f'config = {c.hex(" ")}')
    for codes, name, kind, md5s, ppf, note, body in sorted(kept, key=lambda p: (p[0], p[1])):
        lines += ['', f'[patch {" ".join(codes)}]', f'name = {name}', f'kind = {kind}']
        if md5s:
            lines.append(f'md5 = {" ".join(sorted(md5s))}')
        lines += wrap('note', f'{note} (pop-fe {ppf})')
        lines += hexlines('ppf', body)
    with open(dest, 'w', encoding='utf-8', newline='\n') as f:
        f.write('\n'.join(lines) + '\n')
    print(f'{dest}: {len(groups)} games for {sum(map(len, groups.values()))} codes, {len(kept)} PPF patches '
          f'({sum(len(p[-1]) for p in kept)} B); skipped {len(skipped)}:')
    for s in skipped:
        print('  ' + s)


main()
