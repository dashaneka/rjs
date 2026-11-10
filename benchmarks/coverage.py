#!/usr/bin/env python3
"""Compare extractor metadata across a manifest without retaining URLs or diagnostics."""
import argparse
from datetime import datetime, timezone
import importlib.metadata
import os
from pathlib import Path
from concurrent.futures import ThreadPoolExecutor
import json
import math
import re
import shlex
import subprocess
import sys
from urllib.parse import urlsplit


def token(value):
    # Prevent arbitrary titles, diagnostic text and URLs from entering the report.
    return value if isinstance(value, str) and re.fullmatch(r'[A-Za-z0-9_.-]{1,100}', value) else None


def number(value):
    if isinstance(value, bool) or not isinstance(value, (int, float)) or value < 0:
        return None
    if isinstance(value, float) and not math.isfinite(value):
        return None
    return value


def formats(data, tool):
    if tool == 'rjs':
        media = data.get('media', [])
        return [f for m in media if isinstance(m, dict) for f in (m.get('formats') if isinstance(m.get('formats'), list) else []) if isinstance(f, dict)] if isinstance(media, list) else []
    fs = data.get('formats', [])
    return [f for f in fs if isinstance(f, dict)] if isinstance(fs, list) else []


def summary(data, tool):
    media = data.get('media', []) if tool == 'rjs' else [data]
    if not isinstance(media, list):
        media = []
    fs = formats(data, tool)
    return {'items': [{'id': token(str(m.get('id'))) if m.get('id') is not None else None,
                       'duration': number(m.get('duration'))} for m in media if isinstance(m, dict)],
            'format_count': len(fs),
            'formats': [{'id': token(str(f.get('id' if tool == 'rjs' else 'format_id'))) if f.get('id' if tool == 'rjs' else 'format_id') is not None else None,
                         'width': number(f.get('width')), 'height': number(f.get('height')),
                         'video_codec': token(f.get('codec' if tool == 'rjs' else 'vcodec')),
                         'audio_codec': token(f.get('acodec')),
                         'size': number(f.get('size' if tool == 'rjs' else 'filesize')),
                         'size_approx': number(f.get('filesize_approx')),
                         'bitrate': number(f.get('bitrate' if tool == 'rjs' else 'tbr'))} for f in fs]}


def direct_path(f, tool):
    protocol = f.get('protocol')
    if tool == 'rjs' and protocol != 'http':
        return None
    if tool == 'yt-dlp' and protocol not in ('http', 'https'):
        return None
    try:
        u = urlsplit(f.get('url', ''))
        if u.scheme not in ('http', 'https') or not u.hostname or not u.path or u.path.lower().endswith(('.m3u8', '.mpd')):
            return None
        return (u.scheme.lower(), u.hostname.lower(), u.port, u.path)
    except (ValueError, TypeError):
        return None


def shared_selection(rjs, yt):
    # Multi-item rjs posts cannot be selected unambiguously by format alone.
    if not isinstance(rjs.get('media'), list) or len(rjs['media']) != 1:
        return {'status': 'unsupported'}
    candidates = []
    for r in formats(rjs, 'rjs'):
        path = direct_path(r, 'rjs')
        rid = token(r.get('id'))
        if not path or not rid or rid in ('best', 'worst', 'audio', 'bestaudio', 'worstaudio'):
            continue
        for y in formats(yt, 'yt-dlp'):
            yid = token(y.get('format_id'))
            if not yid or path != direct_path(y, 'yt-dlp'):
                continue
            sizes = [v for v in (number(r.get('size')), number(y.get('filesize')), number(y.get('filesize_approx'))) if v is not None and v > 0]
            rates = [v for v in (number(r.get('bitrate')), (number(y.get('tbr')) or 0) * 1000) if v is not None and v > 0]
            rank = (0, min(sizes)) if sizes else ((1, min(rates)) if rates else (2, 0))
            candidates.append((rank, rid, yid))
    if not candidates:
        return {'status': 'unsupported'}
    _, rid, yid = min(candidates)
    return {'status': 'candidate', 'rjs_format': rid, 'yt_dlp_format': yid,
            'basis': 'same scheme, host, port and path; query ignored', 'verified_same_file_download': False}


def failure_class(stderr):
    text = stderr.decode('utf-8', errors='replace').lower() if isinstance(stderr, bytes) else str(stderr).lower()
    patterns = [
        ('http_404', r'\b(?:http error |http/\S+ )?404\b'),
        ('http_403', r'\b(?:http error |http/\S+ )?403\b'),
        ('http_429', r'\b(?:http error |http/\S+ )?429\b'),
        ('login_required', r'login required|log in required|sign in to confirm|authentication required'),
        ('challenge', r'captcha|cloudflare challenge|verify you are human'),
        ('geo_restricted', r'geo.restricted|not available in your country|geographic restriction'),
        ('tls_error', r'certificate verify failed|certificate problem|certificate verify locations'),
        ('transport_unavailable', r'unsupported url scheme|no supported .*transport|unsupported protocol|no impersonation target|no impersonate target'),
        ('network', r'connection (?:refused|reset|timed out)|network is unreachable|temporary failure in name resolution|failed to resolve|could not resolve'),
        ('parser_error', r'failed to parse|unable to parse|parse error|invalid json|json decode'),
    ]
    return next((name for name, pattern in patterns if re.search(pattern, text)), 'other')


def invoke(command, timeout, diagnostics=None):
    try:
        result = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout)
    except subprocess.TimeoutExpired as e:
        if diagnostics:
            save_diagnostics(diagnostics, e.stdout or b'', e.stderr or b'')
        return {'status': 'timeout', 'failure_class': 'timeout'}, None
    except OSError:
        return {'status': 'launch_error', 'failure_class': 'launch_error'}, None
    if diagnostics:
        save_diagnostics(diagnostics, result.stdout, result.stderr)
    if result.returncode:
        return {'status': 'exit_error', 'exit_code': result.returncode, 'failure_class': failure_class(result.stderr)}, None
    try:
        data = json.loads(result.stdout)
        if not isinstance(data, dict):
            raise ValueError('expected object')
    except (ValueError, UnicodeError):
        return {'status': 'invalid_json', 'exit_code': result.returncode, 'failure_class': 'parser_error'}, None
    return {'status': 'success', 'exit_code': result.returncode}, data


def save_diagnostics(prefix, stdout, stderr):
    for suffix, content in (('stdout', stdout), ('stderr', stderr)):
        path = Path(str(prefix) + '.' + suffix)
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, 'wb') as f:
            f.write(content if isinstance(content, bytes) else content.encode())


def valid_format(f):
    if not isinstance(f, dict) or not isinstance(f.get('url'), str):
        return False
    try:
        u = urlsplit(f['url'])
        return u.scheme in ('http', 'https') and bool(u.hostname)
    except ValueError:
        return False


def usable_id(value):
    return isinstance(value, (str, int)) and not isinstance(value, bool) and bool(str(value).strip())


def valid_metadata(data, tool):
    if tool == 'rjs':
        media = data.get('media')
        return isinstance(media, list) and bool(media) and all(isinstance(m, dict) and usable_id(m.get('id')) and isinstance(m.get('formats'), list) and any(valid_format(f) for f in m['formats']) for m in media)
    return usable_id(data.get('id')) and isinstance(data.get('formats'), list) and any(valid_format(f) for f in data['formats'])


def compare(entry, rjs, yt, timeout, index=0, diagnostics_dir=None):
    prefix = Path(diagnostics_dir) / str(index) if diagnostics_dir else None
    a, rd = invoke(rjs + ['--dump-json', entry['url']], timeout, str(prefix) + '.rjs' if prefix else None)
    b, yd = invoke(yt + ['--no-config', '--no-playlist', '--dump-single-json', entry['url']], timeout, str(prefix) + '.yt-dlp' if prefix else None)
    if rd is not None and not valid_metadata(rd, 'rjs'):
        a['status'], a['failure_class'], rd = 'invalid_metadata', 'parser_error', None
    if yd is not None and not valid_metadata(yd, 'yt-dlp'):
        b['status'], b['failure_class'], yd = 'invalid_metadata', 'parser_error', None
    if rd is not None:
        a['metadata'] = summary(rd, 'rjs')
    if yd is not None:
        b['metadata'] = summary(yd, 'yt-dlp')
    return {'platform': token(entry['platform']), 'rjs': a, 'yt_dlp': b,
            'metadata_pair_success': rd is not None and yd is not None,
            'selection': shared_selection(rd, yd) if rd is not None and yd is not None else {'status': 'unsupported'},
            'verified_same_file_download': False}


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('manifest', help='JSON array of {platform, url} entries')
    p.add_argument('--rjs', default='rjs', help='shell-quoted executable and arguments')
    p.add_argument('--yt-dlp', default='yt-dlp', help='shell-quoted executable and arguments')
    p.add_argument('--timeout', type=float, default=60)
    p.add_argument('--jobs', type=int, choices=range(1, 4), default=1)
    p.add_argument('--output', default='-')
    p.add_argument('--private-diagnostics-dir', help='save sensitive raw tool output locally, indexed by manifest entry')
    a = p.parse_args()
    if not math.isfinite(a.timeout) or a.timeout <= 0:
        p.error('timeout must be positive and finite')
    try:
        rjs, yt = shlex.split(a.rjs), shlex.split(a.yt_dlp)
        with open(a.manifest, encoding='utf-8') as f:
            entries = json.load(f)
        if not rjs or not yt or not isinstance(entries, list) or any(not isinstance(e, dict) or not token(e.get('platform')) or not isinstance(e.get('url'), str) or urlsplit(e['url']).scheme not in ('http', 'https') or not urlsplit(e['url']).hostname for e in entries):
            raise ValueError('expected a JSON array of valid {platform, url} entries')
    except (ValueError, OSError) as e:
        p.error('invalid manifest or command')
    if a.private_diagnostics_dir:
        Path(a.private_diagnostics_dir).mkdir(mode=0o700, parents=True, exist_ok=True)
    versions = {}
    for name, command in (('rjs', rjs), ('yt_dlp', yt)):
        try:
            v = subprocess.run(command + ['--version'], capture_output=True, timeout=a.timeout)
            value = v.stdout.decode('utf-8', errors='replace').strip()
            match = re.search(r'\b\d+(?:\.\d+){1,3}(?:[-+][A-Za-z0-9.]+)?\b', value) if v.returncode == 0 else None
            versions[name] = match.group(0) if match else None
        except (OSError, subprocess.TimeoutExpired):
            versions[name] = None
    try:
        curl_cffi = token(importlib.metadata.version('curl_cffi'))
    except importlib.metadata.PackageNotFoundError:
        curl_cffi = None
    started = datetime.now(timezone.utc).isoformat()
    with ThreadPoolExecutor(max_workers=a.jobs) as pool:
        results = list(pool.map(lambda item: compare(item[1], rjs, yt, a.timeout, item[0], a.private_diagnostics_dir), enumerate(entries)))
    report = {'started_at_utc': started, 'finished_at_utc': datetime.now(timezone.utc).isoformat(), 'versions': versions, 'optional_dependencies': {'curl_cffi': curl_cffi}, 'entries': len(results), 'metadata_pair_successes': sum(r['metadata_pair_success'] for r in results),
              'verified_same_file_downloads': 0, 'results': results}
    output = json.dumps(report, indent=2, allow_nan=False) + '\n'
    if a.output == '-':
        sys.stdout.write(output)
    else:
        with open(a.output, 'w', encoding='utf-8') as f:
            f.write(output)
    return 0


if __name__ == '__main__':
    sys.exit(main())
