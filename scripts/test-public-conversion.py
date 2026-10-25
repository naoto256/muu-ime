#!/usr/bin/env python3
"""Pinned public AJIMEE replay; quality and quiet-window measurements are separate."""

import argparse
import contextlib
import copy
import hashlib
import io
import json
import math
import os
from pathlib import Path
import select
import signal
import statistics
import subprocess
import sys
import tempfile
import time
import unicodedata
import unittest
from unittest import mock
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
SOURCE = json.loads((ROOT / 'test-support/public-conversion-data.json').read_text())
MAX_DATA = 1024 * 1024
MAX_RESPONSE = 4 * 1024 * 1024


def require(condition, message):
    if not condition:
        raise ValueError(message)


def digest(path):
    value = hashlib.sha256()
    with path.open('rb') as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b''):
            value.update(block)
    return value.hexdigest()


# Preparation of the accepted public comparison: NFC reading, Katakana to
# Hiragana, NFC context cut to its last 40 characters. All 200 cases with
# unique ids and non-empty references are required.
def items(raw):
    rows = json.loads(raw)
    require(isinstance(rows, list) and len(rows) == SOURCE['cases'], 'expected all 200 public cases')
    result = []
    for row in rows:
        require(isinstance(row, dict), 'invalid public case')
        require(all(isinstance(row.get(key), str) for key in ('index', 'input', 'context_text')),
                'public case strings missing')
        refs = row.get('expected_output')
        require(isinstance(refs, list) and refs and all(isinstance(x, str) and x for x in refs),
                'public references missing')
        reading = unicodedata.normalize('NFC', row['input'])
        reading = ''.join(chr(ord(c) - 0x60) if 'ァ' <= c <= 'ヶ' else c for c in reading)
        require(row['index'] and reading, 'empty public id or reading')
        result.append({'id': row['index'], 'reading': reading, 'refs': refs,
                       'context': unicodedata.normalize('NFC', row['context_text'])[-40:]})
    require(len({x['id'] for x in result}) == len(result), 'duplicate public id')
    return result


def load_data(path):
    require(path.stat().st_size <= MAX_DATA, 'public data exceeds limit')
    raw = path.read_bytes()
    require(hashlib.sha256(raw).hexdigest() == SOURCE['sha256'], 'public data SHA-256 mismatch')
    return items(raw)


# The only network step. An existing file is verified and kept, never
# refetched or overwritten. The download is checked before it is staged, and
# the stage sits in the output directory so the final replace is atomic.
def fetch(output):
    if output.exists():
        load_data(output)
        print(f'pinned public data already verified: {output}')
        return
    url = f"https://raw.githubusercontent.com/azooKey/AJIMEE-Bench/{SOURCE['revision']}/{SOURCE['path']}"
    with urllib.request.urlopen(url, timeout=30) as response:
        raw = response.read(MAX_DATA + 1)
    require(len(raw) <= MAX_DATA and hashlib.sha256(raw).hexdigest() == SOURCE['sha256'],
            'downloaded public data size or SHA-256 mismatch')
    items(raw)
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(dir=output.parent, delete=False) as temporary:
        staged = Path(temporary.name)
        temporary.write(raw)
    try:
        staged.replace(output)
    finally:
        staged.unlink(missing_ok=True)
    print(f'pinned public data verified: {output}')


# One LF-terminated JSON line from the consumer. `buffered` carries any bytes
# read past the line into the next call. The 60 s deadline per response is a
# guard against a stuck consumer, not a performance limit.
def read_response(stream, buffered):
    deadline = time.monotonic() + 60
    while b'\n' not in buffered:
        require(len(buffered) <= MAX_RESPONSE, 'consumer response exceeds limit')
        remaining = deadline - time.monotonic()
        require(remaining > 0 and select.select([stream], [], [], remaining)[0], 'consumer response timeout')
        block = os.read(stream.fileno(), 65536)
        require(block, 'consumer ended before a complete LF-terminated response')
        buffered.extend(block)
    line, _, tail = buffered.partition(b'\n')
    require(len(line) <= MAX_RESPONSE, 'consumer response exceeds limit')
    buffered[:] = tail
    return json.loads(line)


def check_memory(memory):
    require(isinstance(memory, dict), 'missing memory facts')
    rss, peak = memory.get('rss_bytes'), memory.get('peak_rss_bytes')
    require(isinstance(rss, int) and isinstance(peak, int) and 0 < rss <= peak, 'invalid memory facts')


# One observation: same id and order as the input, a non-empty output that
# the preedit and the commit both equal, and the record fields that
# `comparable` normalizes.
def check_row(row, item):
    require(row.get('id') == item['id'] and row.get('status') == 'ok', 'missing or reordered public result')
    require(isinstance(row.get('output'), str) and row['output'], 'empty conversion output')
    elapsed = row.get('conversion_ms')
    require(isinstance(elapsed, (float, int)) and math.isfinite(elapsed) and elapsed >= 0, 'invalid conversion time')
    check_memory(row.get('memory'))
    require(''.join(x['text'] for x in row['view']['preedit']) == row['output'], 'preedit/output mismatch')
    require(row['commit']['commit_text'] == row['output'], 'commit/output mismatch')
    event = row['event']
    require(isinstance(event.get('session_nonce'), str) and isinstance(event.get('timing'), dict),
            'missing record normalization fields')
    require(all(key in event['timing'] for key in ('conversion_to_commit_us', 'conversion_to_result_us')),
            'missing record timing facts')


def comparable(row):
    value = copy.deepcopy(row)
    # Validation happens before normalization; missing facts are never invented.
    del value['conversion_ms']
    del value['memory']
    value['event']['session_nonce'] = '<session>'
    for key in ('conversion_to_commit_us', 'conversion_to_result_us'):
        value['event']['timing'][key] = '<duration>'
    return value


def reference_rows(path, dataset):
    raw = path.read_bytes()
    require(raw.endswith(b'\n'), 'reference has an incomplete trailing line')
    rows = [json.loads(line) for line in raw.splitlines()]
    require(len(rows) == len(dataset), 'reference does not contain all public cases')
    for row, item in zip(rows, dataset):
        check_row(row, item)
    return rows


# Quality and performance share this run. zenz must give exactly
# SOURCE['zenz_correct'] (173); an intentional change in quality means
# updating that value in test-support/public-conversion-data.json. With
# --reference, every case must match the earlier observations except the
# normalized nonce and timings.
def run(args):
    dataset = load_data(args.input)
    before = os.getpriority(os.PRIO_PROCESS, 0)
    print(f'public conversion gate: priority before={before}', flush=True)
    try:
        if before < 19:
            os.setpriority(os.PRIO_PROCESS, 0, 19)
    finally:
        after = os.getpriority(os.PRIO_PROCESS, 0)
        print(f'public conversion gate: priority after={after}', flush=True)
    require(after >= 19, 'run this gate with nice -n 19')
    require(os.environ.get('VECLIB_MAXIMUM_THREADS') == '2', 'set VECLIB_MAXIMUM_THREADS=2')
    require(args.consumer.is_file() and args.dictionary.is_file() and args.model_dir.is_dir(), 'missing explicit public assets')
    dictionary_sha = digest(args.dictionary)
    require(dictionary_sha == (ROOT / 'dict-src/mozc-oss.sha256').read_text().split()[0],
            'dictionary differs from the pinned public artifact')
    require(not args.output.exists(), 'choose a new output directory')
    reference = reference_rows(args.reference, dataset) if args.reference else None
    args.output.mkdir(parents=True)
    results, controls = [], []
    with (args.output / 'consumer.stderr').open('wb') as errors, (args.output / 'observations.jsonl').open('wb') as observed:
        # `/usr/bin/time -l` writes the consumer's resource summary to
        # consumer.stderr. The new session lets the cleanup below signal both
        # time and the consumer as one group.
        child = subprocess.Popen(['/usr/bin/time', '-l', str(args.consumer.resolve()), str(args.dictionary.resolve()),
                                  str(args.model_dir.resolve()), args.model], stdin=subprocess.PIPE,
                                 stdout=subprocess.PIPE, stderr=errors, env=dict(os.environ), start_new_session=True)
        buffered = bytearray()
        try:
            initialized = read_response(child.stdout, buffered)
            require(initialized.get('event') == 'initialized' and initialized.get('model') == args.model,
                    'requested model did not initialize')
            check_memory(initialized.get('memory'))
            controls.append(initialized)
            child.stdin.write(b'{"control":"warmup"}\n'); child.stdin.flush()
            warm = read_response(child.stdout, buffered)
            require(warm.get('event') == 'warmup', 'warmup did not complete')
            check_memory(warm.get('memory')); controls.append(warm)
            last_rest = time.monotonic()
            for item in dataset:
                request = {key: item[key] for key in ('id', 'reading', 'context')}
                child.stdin.write((json.dumps(request, ensure_ascii=False) + '\n').encode()); child.stdin.flush()
                row = read_response(child.stdout, buffered)
                check_row(row, item)
                observed.write((json.dumps(row, ensure_ascii=False, sort_keys=True) + '\n').encode()); observed.flush()
                results.append(row)
                # Only the performance run pauses: 10 s after every 45 s of work.
                if args.purpose == 'performance' and time.monotonic() - last_rest >= 45:
                    time.sleep(10); last_rest = time.monotonic()
            child.stdin.close()
            require(child.wait(timeout=30) == 0 and not buffered, 'consumer failed or emitted extra data')
            require(not child.stdout.read(), 'consumer emitted extra observations')
        finally:
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGTERM)
                try:
                    child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL); child.wait()
            child.stdout.close()
            if not child.stdin.closed:
                child.stdin.close()
    # A case is correct when the output equals any reference after NFKC, so
    # full-width and half-width forms count as the same. p95 is nearest-rank.
    correct = sum(unicodedata.normalize('NFKC', row['output']) in
                  {unicodedata.normalize('NFKC', ref) for ref in item['refs']}
                  for row, item in zip(results, dataset))
    durations = sorted(row['conversion_ms'] for row in results)
    differences = [] if reference is None else [item['id'] for item, left, right in zip(dataset, reference, results)
        if json.dumps(comparable(left), sort_keys=True) != json.dumps(comparable(right), sort_keys=True)]
    summary = {'purpose': args.purpose, 'cases': len(results), 'correct': correct, 'model': args.model,
               'dataset_sha256': SOURCE['sha256'], 'dictionary_sha256': dictionary_sha,
               'median_ms': statistics.median(durations), 'p95_ms': durations[math.ceil(.95 * len(durations)) - 1],
               'maximum_ms': max(durations), 'initialized': controls[0], 'warmup': controls[1],
               'final_memory': results[-1]['memory'],
               'peak_rss_bytes': max(row['memory']['peak_rss_bytes'] for row in results),
               'compared_reference': reference is not None, 'different_case_ids': differences,
               'veclib_maximum_threads': os.environ['VECLIB_MAXIMUM_THREADS']}
    (args.output / 'summary.json').write_text(json.dumps(summary, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps(summary, ensure_ascii=False, sort_keys=True))
    require(args.model != 'zenz' or correct == SOURCE['zenz_correct'], 'zenz public quality changed from 173/200')
    require(not differences, 'public observations differ from the supplied reference')


class PublicGateTests(unittest.TestCase):
    def setUp(self):
        self.rows = [{'index': str(i), 'input': 'ニホンゴ', 'context_text': '', 'expected_output': ['日本語']}
                     for i in range(200)]

    def priority_result(self, purpose, before, after, set_failure=None):
        output = io.StringIO()
        calls = mock.Mock()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            args = argparse.Namespace(input=root / 'input.json', consumer=root / 'consumer',
                                      dictionary=root / 'dictionary', model_dir=root / 'model', purpose=purpose)
            with mock.patch.dict(os.environ, {'VECLIB_MAXIMUM_THREADS': '2'}, clear=True), \
                    mock.patch(__name__ + '.load_data', return_value=[]), \
                    mock.patch.object(os, 'getpriority', side_effect=[before, after]) as getpriority, \
                    mock.patch.object(os, 'setpriority', side_effect=set_failure) as setpriority, \
                    contextlib.redirect_stdout(output):
                calls.attach_mock(getpriority, 'getpriority')
                calls.attach_mock(setpriority, 'setpriority')
                with self.assertRaises((ValueError, OSError)) as failure:
                    run(args)
        return failure.exception, output.getvalue(), calls

    def test_low_inherited_priority_is_set_to_nineteen_before_public_asset_checks(self):
        for purpose in ('quality', 'performance'):
            for before in (-5, 0, 18):
                with self.subTest(purpose=purpose, before=before):
                    error, _, calls = self.priority_result(purpose, before, 19)
                    self.assertEqual(str(error), 'missing explicit public assets')
                    self.assertEqual(calls.mock_calls, [mock.call.getpriority(os.PRIO_PROCESS, 0),
                        mock.call.setpriority(os.PRIO_PROCESS, 0, 19), mock.call.getpriority(os.PRIO_PROCESS, 0)])
            for before in (19, 20):
                with self.subTest(purpose=purpose, before=before):
                    error, _, calls = self.priority_result(purpose, before, before)
                    self.assertEqual(str(error), 'missing explicit public assets')
                    self.assertEqual(calls.mock_calls, [mock.call.getpriority(os.PRIO_PROCESS, 0)] * 2)

    def test_failed_or_ineffective_priority_adjustment_is_rejected(self):
        for purpose in ('quality', 'performance'):
            with self.subTest(purpose=purpose):
                failure = PermissionError('priority adjustment refused')
                error, _, calls = self.priority_result(purpose, 0, 0, failure)
                self.assertIs(error, failure)
                self.assertEqual(calls.mock_calls, [mock.call.getpriority(os.PRIO_PROCESS, 0),
                    mock.call.setpriority(os.PRIO_PROCESS, 0, 19), mock.call.getpriority(os.PRIO_PROCESS, 0)])
                error, _, calls = self.priority_result(purpose, 0, 18)
                self.assertEqual(str(error), 'run this gate with nice -n 19')
                calls.setpriority.assert_called_once_with(os.PRIO_PROCESS, 0, 19)
                for before, after in ((PermissionError('priority query refused'), 19),
                                      (0, PermissionError('priority recheck refused'))):
                    error, _, calls = self.priority_result(purpose, before, after)
                    self.assertIs(error, before if isinstance(before, OSError) else after)
                    if isinstance(before, OSError):
                        calls.setpriority.assert_not_called()

    def test_priority_before_and_after_are_logged_even_when_setting_fails(self):
        for purpose in ('quality', 'performance'):
            for before, after, failure in ((0, 19, None), (19, 19, None), (20, 20, None),
                                           (0, 0, PermissionError('priority adjustment refused'))):
                with self.subTest(purpose=purpose, before=before, after=after, failure=failure):
                    _, output, _ = self.priority_result(purpose, before, after, failure)
                    self.assertEqual(output, f'public conversion gate: priority before={before}\n'
                                             f'public conversion gate: priority after={after}\n')

    # Negatives: a short set, a repeated id, and an empty reference list.
    def test_complete_unique_public_cases_and_nonempty_references_are_required(self):
        self.assertEqual(len(items(json.dumps(self.rows))), 200)
        for change in ('missing', 'duplicate', 'empty_refs'):
            rows = copy.deepcopy(self.rows)
            if change == 'missing': rows.pop()
            elif change == 'duplicate': rows[-1]['index'] = rows[0]['index']
            else: rows[-1]['expected_output'] = []
            with self.subTest(change=change), self.assertRaises(ValueError):
                items(json.dumps(rows))

    # Well-formed data that is not the pinned file is still refused.
    def test_pinned_digest_rejects_an_unrelated_input(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'data.json'
            path.write_text(json.dumps(self.rows))
            with self.assertRaisesRegex(ValueError, 'SHA-256'):
                load_data(path)

    # A decomposed dakuten composes under NFC before the Katakana map, and the
    # context keeps its last 40 characters.
    def test_preparation_matches_the_published_comparison_protocol(self):
        self.rows[0].update(input='ニホンコ\u3099', context_text='前' * 50)
        row = items(json.dumps(self.rows))[0]
        self.assertEqual(row['reading'], 'にほんご')
        self.assertEqual(row['context'], '前' * 40)

    # A closed pipe without LF fails at once as truncated; it does not wait
    # for the 60 s deadline.
    def test_truncated_response_is_rejected_without_a_real_time_wait(self):
        read, write = os.pipe()
        os.write(write, b'{"partial":1}'); os.close(write)
        with os.fdopen(read, 'rb') as stream, self.assertRaisesRegex(ValueError, 'complete LF'):
            read_response(stream, bytearray())

    # Validation refuses a wrong id, a commit that differs from the output, and
    # a missing timing fact. Normalization hides only the nonce and the
    # timings: a changed score (nll) still counts as a difference. A reference
    # with an unterminated last line, or with the wrong number of rows, fails.
    def test_observations_require_ids_commits_and_normalization_facts(self):
        item = items(json.dumps(self.rows))[0]
        valid = {'id': item['id'], 'status': 'ok', 'output': '日本語', 'conversion_ms': 1.0,
                 'memory': {'rss_bytes': 1, 'peak_rss_bytes': 2},
                 'view': {'preedit': [{'text': '日本語'}]}, 'commit': {'commit_text': '日本語'},
                 'event': {'session_nonce': 'synthetic', 'timing': {
                     'conversion_to_commit_us': 1, 'conversion_to_result_us': 2}, 'nll': 1.0}}
        check_row(valid, item)
        for defect in ('id', 'commit', 'duration'):
            row = copy.deepcopy(valid)
            if defect == 'id': row['id'] = 'different'
            elif defect == 'commit': row['commit']['commit_text'] = 'different'
            else: del row['event']['timing']['conversion_to_result_us']
            with self.subTest(defect=defect), self.assertRaises(ValueError):
                check_row(row, item)
        other = copy.deepcopy(valid)
        other['event']['session_nonce'] = 'new'
        other['event']['timing']['conversion_to_commit_us'] = 999
        self.assertEqual(comparable(valid), comparable(other))
        other['event']['nll'] = 2.0
        self.assertNotEqual(comparable(valid), comparable(other))
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'reference.jsonl'
            for raw in (b'{}', b'{}\n'):
                path.write_bytes(raw)
                with self.assertRaises(ValueError): reference_rows(path, items(json.dumps(self.rows)))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='action', required=True)
    download = sub.add_parser('fetch'); download.add_argument('--output', type=Path, required=True)
    sub.add_parser('self-test')
    runner = sub.add_parser('run')
    for name in ('consumer', 'input', 'output', 'dictionary', 'model-dir'):
        runner.add_argument('--' + name, type=Path, required=True)
    runner.add_argument('--model', choices=('edge', 'zenz'), required=True)
    runner.add_argument('--purpose', choices=('quality', 'performance'), required=True)
    runner.add_argument('--reference', type=Path)
    args = parser.parse_args()
    try:
        if args.action == 'self-test':
            result = unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(PublicGateTests))
            return 0 if result.wasSuccessful() else 1
        if args.action == 'fetch': fetch(args.output)
        else: run(args)
        return 0
    except (ValueError, OSError, KeyError, TypeError, subprocess.SubprocessError) as error:
        print('public conversion gate: ' + str(error), file=sys.stderr)
        return 1


if __name__ == '__main__':
    sys.exit(main())
