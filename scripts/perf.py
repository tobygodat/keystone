#!/usr/bin/env python3
"""Measure build and render performance and append one row to docs/perf-log.tsv.

Run from anywhere: python scripts/perf.py [--runs N] [--note TEXT] [--no-log]
"""
import argparse
import datetime
import json
import os
import pathlib
import re
import statistics
import subprocess
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
LOG = ROOT / 'docs/perf-log.tsv'
COLUMNS = ['date', 'commit', 'touch_desktop_s', 'touch_ui_s', 'desktop_debug_mb',
           'sidebar_draw_ms', 'sidebar_refresh_ms', 'note', 'transcript_draw_ms',
           'transcript_stream_ms']
BUILD = ['cargo', 'build', '-p', 'keystone-desktop', '--locked', '-q']


def run(cmd):
    subprocess.run(cmd, cwd=ROOT, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)


def timed_rebuild(path, runs):
    samples = []
    for _ in range(runs):
        (ROOT / path).touch()
        start = time.perf_counter()
        run(BUILD)
        samples.append(time.perf_counter() - start)
    return statistics.median(samples)


def render_ms(runs):
    """Return the median of each PERF metric printed by the ignored render benchmarks."""
    samples = {}
    for _ in range(runs):
        out = subprocess.run(
            ['cargo', 'test', '-p', 'keystone-desktop', '--features', 'ui-tests', '--locked',
             '-q', 'perf_', '--', '--ignored', '--nocapture'],
            cwd=ROOT, check=True, capture_output=True, text=True)
        for name, value in re.findall(r'PERF (\w+)=([0-9.]+)', out.stdout + out.stderr):
            samples.setdefault(name, []).append(float(value))
    for name in ('sidebar_draw_ms', 'sidebar_refresh_ms', 'transcript_draw_ms'):
        if name not in samples:
            raise SystemExit(f'render benchmark printed no {name} line')
    return {name: f'{statistics.median(values):.2f}' for name, values in samples.items()}


def target_dir():
    metadata = subprocess.run(['cargo', 'metadata', '--format-version', '1', '--no-deps'],
                              cwd=ROOT, check=True, capture_output=True, text=True).stdout
    return pathlib.Path(json.loads(metadata)['target_directory'])


def commit():
    head = subprocess.run(['git', 'rev-parse', '--short', 'HEAD'], cwd=ROOT,
                          capture_output=True, text=True).stdout.strip()
    dirty = subprocess.run(['git', 'status', '--porcelain', '--untracked-files=no'], cwd=ROOT,
                           capture_output=True, text=True).stdout.strip()
    return f'{head}+dirty' if dirty else head


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--runs', type=int, default=3)
    parser.add_argument('--note', default='')
    parser.add_argument('--no-log', action='store_true')
    args = parser.parse_args()

    run(BUILD)
    render = render_ms(args.runs)
    row = {
        'date': datetime.datetime.now(datetime.timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ'),
        'commit': commit(),
        'touch_desktop_s': f"{timed_rebuild('crates/keystone-desktop/src/main.rs', args.runs):.2f}",
        'touch_ui_s': f"{timed_rebuild('crates/keystone-ui/src/button.rs', args.runs):.2f}",
        'desktop_debug_mb': f"{os.path.getsize(target_dir() / 'debug/keystone-desktop') / 1e6:.0f}",
        'sidebar_draw_ms': render['sidebar_draw_ms'],
        'sidebar_refresh_ms': render['sidebar_refresh_ms'],
        'note': args.note,
        'transcript_draw_ms': render['transcript_draw_ms'],
        # Added with the virtualized transcript; earlier code has no streaming benchmark.
        'transcript_stream_ms': render.get('transcript_stream_ms', ''),
    }
    print('\t'.join(f'{k}={row[k]}' for k in COLUMNS))
    if not args.no_log:
        new = not LOG.exists()
        with LOG.open('a') as log:
            if new:
                log.write('\t'.join(COLUMNS) + '\n')
            log.write('\t'.join(row[k] for k in COLUMNS) + '\n')


if __name__ == '__main__':
    main()
