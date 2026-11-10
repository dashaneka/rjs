import tempfile
import unittest
from pathlib import Path
from fixture_corpus import build, fixture_url


class FixturesTest(unittest.TestCase):
    def test_preserves_full_literal_url_and_index(self):
        source = "class SiteIE:\n    _TESTS = [{'url': 'https://example.test/old'}, {'url': 'https://example.test/video/1/exact-slug?q=2#x'}]\n"
        self.assertEqual(fixture_url(source, 'SiteIE', 1), 'https://example.test/video/1/exact-slug?q=2#x')
        with self.assertRaises(ValueError):
            fixture_url(source, 'SiteIE', 2)

    def test_does_not_execute_fixture_expressions(self):
        source = "raise RuntimeError('must not run')\nclass SiteIE:\n    _TESTS = generate_tests()\n"
        with self.assertRaises(ValueError):
            fixture_url(source, 'SiteIE', 0)

    def test_rejects_path_traversal(self):
        with tempfile.TemporaryDirectory() as folder:
            refs = {'fixtures': [{'platform': 'site', 'source_file': '../outside.py'}]}
            with self.assertRaises(ValueError):
                build(refs, Path(folder))
