import json
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

RUNNER = Path(__file__).with_name('compare.py')

class RunnerTest(unittest.TestCase):
    def test_download_and_failure(self):
        with tempfile.TemporaryDirectory() as d:
            fake = Path(d) / 'fake.py'
            fake.write_text('''import json, os, pathlib, sys\na=sys.argv[1:]\nif "--dump-json" in a or "--dump-single-json" in a:\n print(json.dumps({"id":"example", "formats":[{"format_id":"same"}]}))\nelif os.environ.get("FAIL_FAKE"):\n sys.exit(7)\nelse:\n p=pathlib.Path(a[a.index("--output")+1])\n if "%" in str(p): p=p.parent / "example.mp4"\n else: p=p / "example.mp4"\n p.write_bytes(b"example")\n''')
            # A small wrapper gives the fake the executable shape of rjs.
            wrapper = Path(d) / 'fake'
            wrapper.write_text('#!/bin/sh\nexec "' + sys.executable + '" "' + str(fake) + '" "$@"\n')
            wrapper.chmod(0o755)
            probe = Path(d) / 'ffprobe'
            probe.write_text('#!/bin/sh\nprintf \'{\"streams\": [{\"codec_type\": \"video\"}]}\'\n')
            probe.chmod(0o755)
            env = {**os.environ, 'PATH': d + os.pathsep + os.environ.get('PATH', '')}
            cmd = [sys.executable, str(RUNNER), 'https://example.test/video', '--rjs', str(wrapper), '--yt-dlp', str(wrapper), '--rjs-format', 'same', '--yt-format', 'same', '--cache-mode', 'warm', '--repeats', '2']
            result = subprocess.run(cmd, capture_output=True, text=True, check=True, env=env)
            data = json.loads(result.stdout)
            self.assertEqual(len(data['measurements']), 4)
            self.assertEqual(data['summary']['rjs']['successful'], 2)
            self.assertEqual(data['summary']['yt-dlp']['bytes'], [7, 7])
            self.assertIsNotNone(data['preflight']['rjs']['metadata'])
            failed = subprocess.run(cmd, capture_output=True, text=True, env={**env, 'FAIL_FAKE':'1'})
            self.assertEqual(failed.returncode, 1)
            self.assertEqual(json.loads(failed.stdout)['summary']['rjs']['successful'], 0)

    def test_helpers(self):
        spec = importlib.util.spec_from_file_location('compare', RUNNER)
        compare = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(compare)
        self.assertEqual(compare.redact({'url': 'https://example/x?secret=1', 'message': 'https://example/x?token=secret'}), {'url': '[redacted]', 'message': 'https://example/x'})
        rjs = {'media': [{'id': '1', 'formats': [{'id': 'a', 'width': 640, 'height': 480, 'codec': 'avc1.123'}]}]}
        yt = {'id': '1', 'width': 1280, 'height': 480, 'vcodec': 'h264'}
        self.assertTrue(any('width differs' in w for w in compare.selection_warnings(rjs, yt, 'a')))
        self.assertTrue(compare.selection_warnings(rjs, yt, 'missing'))
        with tempfile.TemporaryDirectory() as d:
            f = Path(d) / 'sample'
            f.write_bytes(b'abc')
            self.assertEqual(compare.file_info(f)['sha256'], 'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad')

    def test_help(self):
        self.assertEqual(subprocess.run([sys.executable, str(RUNNER), '--help'], capture_output=True).returncode, 0)

if __name__ == '__main__':
    unittest.main()
