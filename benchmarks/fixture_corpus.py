#!/usr/bin/env python3
"""Resolve versioned upstream fixture references into a local URL manifest."""
import argparse
import ast
import importlib.metadata
import json
from pathlib import Path
import re
import sys


def fixture_url(source, class_name, index):
    """Read literal test data without executing the extractor or its expressions."""
    if not isinstance(index, int) or isinstance(index, bool) or index < 0:
        raise ValueError('invalid fixture index')
    tree = ast.parse(source)
    for cls in tree.body:
        if not isinstance(cls, ast.ClassDef) or cls.name != class_name:
            continue
        for statement in cls.body:
            if not isinstance(statement, ast.Assign) or not any(
                isinstance(target, ast.Name) and target.id == '_TESTS' for target in statement.targets
            ):
                continue
            if not isinstance(statement.value, ast.List) or index >= len(statement.value.elts):
                raise ValueError('fixture list or index changed')
            test = statement.value.elts[index]
            if isinstance(test, ast.Dict):
                for key, value in zip(test.keys, test.values):
                    if isinstance(key, ast.Constant) and key.value == 'url' and isinstance(value, ast.Constant) and isinstance(value.value, str):
                        return value.value
            raise ValueError('fixture has no literal URL')
    raise ValueError('fixture class or test list not found')


def build(references, extractor_dir):
    entries = []
    for ref in references['fixtures']:
        filename = ref['source_file']
        if not isinstance(filename, str) or not re.fullmatch(r'[a-z0-9_]+\.py', filename):
            raise ValueError('invalid extractor filename')
        source = (extractor_dir / filename).read_text(encoding='utf-8')
        url = fixture_url(source, ref['source_class'], ref['test_index'])
        if not url.startswith(('http://', 'https://')):
            raise ValueError('fixture URL is not an HTTP link')
        entries.append({'platform': ref['platform'], 'url': url})
    return entries


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--references', default=str(Path(__file__).with_name('remaining-fixtures.json')))
    parser.add_argument('--output', required=True, help='local URL manifest; do not commit private input files')
    args = parser.parse_args()
    try:
        import yt_dlp
        references = json.loads(Path(args.references).read_text(encoding='utf-8'))
        version = importlib.metadata.version('yt-dlp')
        if version != references['yt_dlp_version']:
            raise ValueError('installed yt-dlp does not match the fixture reference version')
        entries = build(references, Path(yt_dlp.__file__).parent / 'extractor')
        Path(args.output).write_text(json.dumps(entries, indent=2) + '\n', encoding='utf-8')
    except (ImportError, OSError, ValueError, KeyError, TypeError) as error:
        parser.error(str(error))
    print(f'wrote {len(entries)} fixture inputs')


if __name__ == '__main__':
    sys.exit(main())
