#!/usr/bin/env python3
"""Run paired subprocess benchmarks. Uses only the Python standard library."""
import argparse
import hashlib
import json
import os
import re
import shutil
from pathlib import Path
import shlex
import statistics
import subprocess
import sys
import tempfile
import time
from urllib.parse import urlsplit, urlunsplit


def run(command, timeout):
    start = time.perf_counter()
    try:
        p = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                           text=True, errors='replace', timeout=timeout)
        return {'seconds': time.perf_counter() - start, 'exit_code': p.returncode,
                'stdout': p.stdout, 'stderr': p.stderr, 'timeout': False}
    except subprocess.TimeoutExpired as e:
        return {'seconds': time.perf_counter() - start, 'exit_code': None,
                'stdout': (e.stdout or b'').decode(errors='replace') if isinstance(e.stdout, bytes) else (e.stdout or ''),
                'stderr': (e.stderr or b'').decode(errors='replace') if isinstance(e.stderr, bytes) else (e.stderr or ''), 'timeout': True}
    except OSError as e:
        return {'seconds': time.perf_counter() - start, 'exit_code': None,
                'stdout': '', 'stderr': str(e), 'timeout': False}


def parse_metadata(result, tool):
    try:
        data = json.loads(result['stdout'])
        if tool == 'rjs':
            return data
        return {k: data.get(k) for k in ('id', 'title', 'duration', 'format_id', 'ext', 'width', 'height', 'vcodec', 'acodec', 'filesize', 'filesize_approx', 'formats')}
    except (ValueError, TypeError):
        return None


def redact(value, key=''):
    """Remove URL fields and query strings, including URLs in diagnostic text."""
    if isinstance(value, dict):
        return {k: ('[redacted]' if 'url' in k.lower() and isinstance(v, str) else redact(v, k)) for k, v in value.items()}
    if isinstance(value, list):
        return [redact(v, key) for v in value]
    if isinstance(value, str):
        def clean(match):
            try:
                u = urlsplit(match.group(0))
                host = u.hostname or ''
                if u.port:
                    host += ':' + str(u.port)
                return urlunsplit((u.scheme, host, u.path, '', ''))
            except ValueError:
                return '[redacted-url]'
        return re.sub(r'https?://[^\s"<>]+', clean, value)
    return value


def file_info(path):
    digest = hashlib.sha256()
    with path.open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(chunk)
    info = {'bytes': path.stat().st_size, 'sha256': digest.hexdigest()}
    if shutil.which('ffprobe'):
        probe = run(['ffprobe', '-v', 'error', '-show_entries',
                     'format=duration:stream=codec_type,codec_name,width,height',
                     '-of', 'json', str(path)], 30)
        try:
            info['ffprobe'] = json.loads(probe['stdout']) if probe['exit_code'] == 0 else None
        except ValueError:
            info['ffprobe'] = None
        if not info['ffprobe'] or not info['ffprobe'].get('streams'):
            info['ffprobe_error'] = probe['stderr']
    return info


def selection_warnings(rjs, yt, selector):
    """Only compare known fields for an exact single-format rjs selection."""
    warnings = []
    media = rjs.get('media', []) if isinstance(rjs, dict) else []
    matches = [f for m in media if isinstance(m, dict) for f in m.get('formats', [])
               if isinstance(f, dict) and f.get('id') == selector]
    if len(media) != 1 or len(matches) != 1 or not isinstance(yt, dict):
        return ['Could not verify a single exact rjs format against yt-dlp metadata; inspect manually.']
    r = matches[0]
    if media[0].get('id') and yt.get('id') and str(media[0]['id']) != str(yt['id']):
        warnings.append('Media IDs differ; verify the same item was selected.')
    for field in ('width', 'height'):
        if r.get(field) and yt.get(field) and r[field] != yt[field]:
            warnings.append(f'Selected {field} differs: rjs={r[field]}, yt-dlp={yt[field]}.')
    def codec_family(v):
        if not v or v == 'none':
            return None
        v = v.lower().split('.')[0]
        return {'avc1': 'h264', 'avc': 'h264', 'hev1': 'hevc', 'hvc1': 'hevc', 'vp09': 'vp9', 'av01': 'av1'}.get(v, v)
    rc, yc = codec_family(r.get('codec')), codec_family(yt.get('vcodec'))
    if rc and yc and rc != yc:
        warnings.append(f'Selected video codec differs: rjs={rc}, yt-dlp={yc}.')
    if not all((r.get('width'), r.get('height'), yt.get('width'), yt.get('height'), rc, yc)):
        warnings.append('Some selected dimensions or codec are unknown; equivalence is unverified.')
    return warnings


def main():
    p = argparse.ArgumentParser(description='Compare rjs and yt-dlp on one URL; inspect formats before interpreting timings.')
    p.add_argument('url')
    p.add_argument('--rjs', default='rjs', help='rjs executable')
    p.add_argument('--yt-dlp', default='yt-dlp', help='yt-dlp command, shell-quoted if needed (no shell is invoked)')
    p.add_argument('--rjs-format', required=True, help='rjs format ID or selector')
    p.add_argument('--yt-format', required=True, help='yt-dlp format ID or selector for the same media')
    p.add_argument('--mode', choices=['download', 'metadata'], default='download')
    p.add_argument('--cache-mode', choices=['cold', 'warm'], default='cold', help='cold: no runner warmup; warm: one unmeasured paired warmup (OS/network caches are not cleared)')
    p.add_argument('--connections', type=int, default=1, choices=range(1, 33), metavar='1-32')
    p.add_argument('--repeats', type=int, default=3)
    p.add_argument('--timeout', type=float, default=120)
    p.add_argument('--output', default='-', help='JSON file, or - for stdout')
    a = p.parse_args()
    if a.repeats < 1 or a.timeout <= 0:
        p.error('repeats and timeout must be positive')
    yt = shlex.split(a.yt_dlp)
    if not yt:
        p.error('--yt-dlp must not be empty')
    versions = {}
    for tool, command in [('rjs', [a.rjs, '--version']), ('yt-dlp', yt + ['--version'])]:
        version = run(command, a.timeout)
        versions[tool] = {'exit_code': version['exit_code'], 'stdout': version['stdout'].strip(), 'stderr': version['stderr']}
    preflight = {}
    for tool, command in [('rjs', [a.rjs, '--dump-json', a.url]), ('yt-dlp', yt + ['--no-config', '--no-playlist', '--dump-single-json', '--format', a.yt_format, a.url])]:
        result = run(command, a.timeout)
        preflight[tool] = {'command': command, 'exit_code': result['exit_code'], 'timeout': result['timeout'],
                           'metadata': parse_metadata(result, tool), 'stderr': result['stderr']}
    measurements = []
    with tempfile.TemporaryDirectory(prefix='rjs-benchmark-') as tmp:
        def sample(tool, iteration, warmup):
            outdir = Path(tmp) / ('warmup' if warmup else 'measured') / str(iteration) / tool
            outdir.mkdir(parents=True, exist_ok=True)
            if a.mode == 'metadata':
                command = ([a.rjs, '--dump-json', a.url] if tool == 'rjs' else
                           yt + ['--no-config', '--no-playlist', '--dump-single-json', '--format', a.yt_format, a.url])
            else:
                command = ([a.rjs, '--quiet', '--connections', str(a.connections), '--format', a.rjs_format, '--output', str(outdir), a.url] if tool == 'rjs' else
                           yt + ['--no-config', '--no-playlist', '--no-progress', '--concurrent-fragments', str(a.connections), '--format', a.yt_format, '--output', str(outdir / '%(id)s.%(ext)s'), a.url])
            result = run(command, a.timeout)
            files = [f for f in outdir.rglob('*') if f.is_file()]
            details = {str(f.relative_to(outdir)): file_info(f) for f in files}
            sizes = {name: info['bytes'] for name, info in details.items()}
            metadata = parse_metadata(result, tool) if a.mode == 'metadata' else None
            success = result['exit_code'] == 0 and not result['timeout'] and (metadata is not None if a.mode == 'metadata' else bool(sizes) and all(n > 0 for n in sizes.values()) and not any(name.endswith(('.part', '.ytdl')) for name in sizes) and all('ffprobe_error' not in info for info in details.values()))
            return {'tool': tool, 'iteration': iteration, 'command': command, 'seconds': result['seconds'],
                    'exit_code': result['exit_code'], 'timeout': result['timeout'], 'success': success,
                    'bytes': sum(sizes.values()), 'files': details, 'stderr': result['stderr']}
        if a.cache_mode == 'warm':
            for tool in ('rjs', 'yt-dlp'):
                sample(tool, 0, True)
        for i in range(a.repeats):
            # Alternate order to reduce systematic order bias.
            for tool in (('rjs', 'yt-dlp') if i % 2 == 0 else ('yt-dlp', 'rjs')):
                measurements.append(sample(tool, i + 1, False))
    summary = {}
    for tool in ('rjs', 'yt-dlp'):
        runs = [m for m in measurements if m['tool'] == tool]
        ok = [m['seconds'] for m in runs if m['success']]
        summary[tool] = {'successful': len(ok), 'attempted': len(runs), 'median_seconds': statistics.median(ok) if ok else None,
                         'bytes': [m['bytes'] for m in runs]}
    warnings = selection_warnings(preflight['rjs']['metadata'], preflight['yt-dlp']['metadata'], a.rjs_format)
    if a.mode == 'download':
        first = {tool: next((m for m in measurements if m['tool'] == tool and m['success']), None) for tool in ('rjs', 'yt-dlp')}
        def video(m):
            if not m:
                return None
            for info in m['files'].values():
                for stream in (info.get('ffprobe') or {}).get('streams', []):
                    if stream.get('codec_type') == 'video':
                        return stream
            return None
        rv, yv = video(first['rjs']), video(first['yt-dlp'])
        if rv and yv:
            for field in ('width', 'height', 'codec_name'):
                if rv.get(field) and yv.get(field) and rv[field] != yv[field]:
                    warnings.append(f'Downloaded video {field} differs: rjs={rv[field]}, yt-dlp={yv[field]}.')
        else:
            warnings.append('Downloaded video streams could not both be inspected; equivalence is unverified.')
    document = {'ffprobe_available': bool(shutil.which('ffprobe')), 'versions': versions, 'compatibility_warnings': warnings, 'link': a.url, 'mode': a.mode, 'cache_mode': a.cache_mode, 'connections': a.connections,
                'repeats': a.repeats, 'format_selectors': {'rjs': a.rjs_format, 'yt-dlp': a.yt_format},
                'caveats': ['Selectors and metadata must be checked manually for equivalent media.', 'Connection options have protocol-dependent meanings; yt-dlp concurrent fragments does not parallelize ordinary HTTP downloads.', 'Cold means no runner warmup, not cleared caches. Each measurement starts a new process and empty output directory.', 'Bytes are a sanity check, not proof of identical media.'],
                'preflight': preflight, 'measurements': measurements, 'summary': summary}
    payload = json.dumps(redact(document), indent=2, ensure_ascii=False) + '\n'
    if a.output == '-':
        sys.stdout.write(payload)
    else:
        Path(a.output).write_text(payload, encoding='utf-8')
    return 0 if all(m['success'] for m in measurements) else 1


if __name__ == '__main__':
    sys.exit(main())
