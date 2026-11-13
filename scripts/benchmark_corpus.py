#!/usr/bin/env python3
"""macOS process measurements; builds must be completed before running."""
import argparse, hashlib, json, pathlib, re, subprocess, time
p = argparse.ArgumentParser()
p.add_argument('--core', required=True)
p.add_argument('--runtime', default='target/release/ounce-runtime')
p.add_argument('--output', required=True)
p.add_argument('--repeats', type=int, default=5)
a = p.parse_args()
out = pathlib.Path(a.output); out.mkdir(parents=True, exist_ok=True)
rows = []
for fixture in ('hello', 'services', 'romfs', 'threads'):
    nro = pathlib.Path(f'tests/fixtures/nro/{fixture}/{fixture}.nro').resolve()
    for engine, binary, verb in [('core', a.core, 'run-nro'), ('runtime', a.runtime, 'run')]:
        for trial in range(a.repeats + 1):
            cmd = ['/usr/bin/time', '-l', str(pathlib.Path(binary).resolve()), verb, str(nro), '--present=headless'] + (['fixture'] if fixture == 'hello' else [])
            start = time.perf_counter()
            try:
                r = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
                elapsed = time.perf_counter() - start
                log = r.stdout + r.stderr
                rc = r.returncode
            except subprocess.TimeoutExpired as e:
                elapsed = time.perf_counter() - start
                log = (e.stdout or b'').decode() + (e.stderr or b'').decode()
                rc = 'timeout'
            name = f'{fixture}-{engine}-{trial}.log'
            (out / name).write_text(log)
            ins = re.search(r'Executed (\d+) instructions', log)
            rss = re.search(r'(\d+)\s+maximum resident set size', log)
            rows.append(dict(fixture=fixture, engine=engine, trial=trial, warmup=trial==0,
                command=cmd, wall_seconds=elapsed, peak_rss_bytes=int(rss[1]) if rss else None,
                instructions=int(ins[1]) if ins else None, exit_code=rc, log=name,
                guest_pass='result=0x00000EEE' in log, graceful_exit='Process exited gracefully' in log,
                sha256=hashlib.sha256(nro.read_bytes()).hexdigest()))
            (out / 'measurements.json').write_text(json.dumps(rows, indent=2) + '\n')
            print(f'{fixture} {engine} {trial}: {elapsed:.4f}s rc={rc}', flush=True)

if any(x['exit_code'] != 0 or not x['guest_pass'] or not x['graceful_exit'] or x['peak_rss_bytes'] is None for x in rows):
    raise SystemExit('One or more fixture runs failed validation; inspect measurements.json and logs')
