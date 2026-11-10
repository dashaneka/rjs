import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).parent))
import coverage as c


class CoverageTests(unittest.TestCase):
    def test_selection_and_redaction(self):
        r = {'media': [{'id': '123', 'title': 'private title', 'formats': [
            {'id': 'large', 'url': 'https://cdn.example/x?secret=yes', 'protocol': 'http', 'size': 100},
            {'id': 'small', 'url': 'https://cdn.example/y?secret=yes', 'protocol': 'http', 'size': 20}]}]}
        y = {'formats': [{'format_id': '1', 'url': 'https://cdn.example/x?token=2', 'protocol': 'https'},
                         {'format_id': '2', 'url': 'https://cdn.example/y?token=2', 'protocol': 'https'}]}
        selected = c.shared_selection(r, y)
        self.assertEqual((selected['rjs_format'], selected['yt_dlp_format']), ('small', '2'))
        self.assertFalse(selected['verified_same_file_download'])
        self.assertNotIn('private', json.dumps(c.summary(r, 'rjs')))
        self.assertNotIn('secret', json.dumps(c.summary(r, 'rjs')))
        y['formats'][1]['protocol'] = 'm3u8_native'
        self.assertEqual(c.shared_selection(r, y)['rjs_format'], 'large')
        r['media'].append({})
        self.assertEqual(c.shared_selection(r, y)['status'], 'unsupported')

    def test_fake_cli_failures_and_success(self):
        with tempfile.TemporaryDirectory() as d:
            path = Path(d)
            script = path / 'fake.py'
            script.write_text('''import json, sys, time\nurl = sys.argv[-1]\nif "timeout" in url: time.sleep(2)\nif "failure" in url: sys.exit(4)\nif "invalid" in url: print("not json"); sys.exit()\nif "--dump-json" in sys.argv:\n print(json.dumps({"media":[{"id":"abc", "formats":[{"id":"x", "protocol":"http", "url":"https://cdn.test/file?secret=1"}]}]}))\nelse:\n print(json.dumps({"id":"abc", "title":"private title", "formats":[{"format_id":"y", "protocol":"https", "url":"https://cdn.test/file?secret=2"}]}))\n''')
            manifest = path / 'manifest.json'
            manifest.write_text(json.dumps([{'platform': 'demo', 'url': 'https://example.test/' + n} for n in ('ok', 'failure', 'invalid', 'timeout')]))
            command = f'{sys.executable} {script}'
            result = subprocess.run([sys.executable, str(Path(c.__file__)), str(manifest), '--rjs', command, '--yt-dlp', command, '--timeout', '0.15', '--jobs', '3'], capture_output=True, text=True, check=True)
            report = json.loads(result.stdout)
            self.assertEqual(report['metadata_pair_successes'], 1)
            self.assertEqual(report['verified_same_file_downloads'], 0)
            self.assertEqual([x['rjs']['status'] for x in report['results']], ['success', 'exit_error', 'invalid_json', 'timeout'])
            self.assertNotIn('secret', result.stdout)
            self.assertNotIn('private', result.stdout)
            self.assertNotIn('example.test', result.stdout)

    def test_malformed_metadata(self):
        self.assertEqual(c.formats({'media': [{'formats': None}]}, 'rjs'), [])
        self.assertEqual(c.shared_selection({'media': None}, {}), {'status': 'unsupported'})
        self.assertFalse(c.valid_metadata({'id': True, 'formats': []}, 'yt-dlp'))
        self.assertEqual(c.failure_class(b'HTTP Error 403: forbidden'), 'http_403')
        self.assertEqual(c.failure_class(b'private diagnostic text'), 'other')
        self.assertEqual(c.failure_class(b'certificate verify failed'), 'tls_error')
        self.assertEqual(c.failure_class(b'no impersonation target'), 'transport_unavailable')
        self.assertEqual(c.failure_class(b'no impersonate target'), 'transport_unavailable')
        self.assertEqual(c.number(10**1000), 10**1000)


if __name__ == '__main__':
    unittest.main()
