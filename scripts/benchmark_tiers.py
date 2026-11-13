#!/usr/bin/env python3
"""Record stock architecture harness counters; Stage 20 includes cold compilation."""
import argparse, json, pathlib, re, subprocess, time
p = argparse.ArgumentParser()
p.add_argument('--core', required=True)
p.add_argument('--runtime', default='target/release/ounce-runtime')
p.add_argument('--output', required=True)
p.add_argument('--repeats', type=int, default=5)
a = p.parse_args(); out = pathlib.Path(a.output); out.mkdir(parents=True, exist_ok=True)
rows = []
for engine, binary, tiers in [('core', a.core, ['tier3']), ('runtime', a.runtime, ['tier3','tier4','nce'])]:
    for tier in tiers:
        for trial in range(a.repeats + 1):
            cmd = [str(pathlib.Path(binary).resolve()), 'test', f'--cpu={tier}', '--present=headless']
            start = time.perf_counter()
            r = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
            elapsed = time.perf_counter() - start
            log = r.stdout + r.stderr
            name = f'synthetic-{engine}-{tier}-{trial}.log'; (out/name).write_text(log)
            mips = re.search(r'\[([^\]]+) Throughput\].*?: ([\d.]+) MIPS', log)
            counters = re.search(r'\[[^\]]+ JIT Engine\] Executed (\d+) insns \((.*?)\) \| Exit: (.*)', log)
            rows.append(dict(engine=engine, requested_tier=tier, trial=trial, warmup=trial==0,
                command=cmd, wall_seconds=elapsed, exit_code=r.returncode, log=name,
                actual_tier=mips[1] if mips else None, mips=float(mips[2]) if mips else None,
                stage20_instructions=int(counters[1]) if counters else None,
                counters=counters[2] if counters else None, stop=counters[3] if counters else None))
            (out/'tiers.json').write_text(json.dumps(rows, indent=2)+'\n')
            print(engine,tier,trial,r.returncode,mips[0] if mips else 'no throughput',flush=True)

if any(x['exit_code'] != 0 or x['actual_tier'] is None or x['stage20_instructions'] is None for x in rows):
    raise SystemExit('One or more synthetic runs failed validation; inspect tiers.json and logs')
