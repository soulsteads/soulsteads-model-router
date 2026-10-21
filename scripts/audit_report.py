#!/usr/bin/env python3
"""Streaming, bounded-memory report over Slotline tracing JSON (stdin or file).

Counts delivery outcomes, not answer quality. No inference-path work, dependencies,
payload storage or request-ID output. Percentiles are fixed-bucket upper bounds.
"""
import argparse
import bisect
from collections import OrderedDict
import datetime
import json
import math
import re
import sys

MAX_LINE_BYTES = 65536
BUCKETS = (1, 5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000,
           10000, 30000, 60000, 120000, 300000, 900000)
MESSAGES = {'request routed', 'embedding routed', 'upstream request failed',
            'request response headers', 'request rejected or failed',
            'request body completed', 'request stream failed', 'request body cancelled',
            'request handler cancelled', 'request policy', 'selection decision',
            'upstream attempt started', 'upstream attempt finished', 'request first output'}
LABEL = re.compile(r'^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$')
REASONS = {'only_eligible', 'occupancy_rank', 'affinity_tie', 'priority_tie',
           'rotation_tie', 'no_eligible'}
ELIGIBILITY = {'eligible', 'alias_disabled', 'credential_route', 'already_tried',
               'disabled', 'context', 'streaming', 'capability', 'physical_unavailable',
               'cooling_down', 'stale', 'unreachable', 'not_serving', 'stale_health',
               'stale_qualification', 'full', 'fallback_gate', 'race_full'}
ATTEMPT_OUTCOMES = {'validated', 'stream_headers', 'invalid_body', 'transport',
                    'timeout', 'status', 'cancelled'}
MAX_ATTEMPTS = 64
MAX_TIMELINES = 64


def bounded_int(value, maximum):
    return value if isinstance(value, int) and not isinstance(value, bool) and 0 <= value <= maximum else None


def digest(value):
    return value if isinstance(value, str) and re.fullmatch(r'[0-9a-f]{64}', value) else 'unknown'


def enum(value, choices):
    return value if isinstance(value, str) and value in choices else 'unknown'


def duration(value):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return value if 0 <= value <= 86400000 and math.isfinite(value) else None


class Usage:
    def __init__(self):
        self.samples = 0
        self.tokens = 0

    def add(self, value):
        value = bounded_int(value, 1_000_000_000)
        if value is not None:
            self.samples += 1
            self.tokens += value

    def summary(self):
        return {'samples': self.samples, 'tokens': self.tokens,
                'mean_tokens': self.tokens / self.samples if self.samples else None}


def safe_label(value):
    return value if isinstance(value, str) and LABEL.fullmatch(value) else 'unknown'


def timestamp(value):
    if (not isinstance(value, str) or len(value) > 64 or not re.fullmatch(
            r'\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,9})?(?:Z|[+-](?:[01]\d|2[0-3]):[0-5]\d)', value)):
        raise ValueError('invalid timestamp; expected ISO T separator and valid timezone offset')
    dt = datetime.datetime.fromisoformat(value.replace('Z', '+00:00'))
    if dt.tzinfo is None:
        raise ValueError('timestamp requires timezone')
    return dt.timestamp()


class Histogram:
    def __init__(self):
        self.counts = [0] * (len(BUCKETS) + 1)
        self.total = 0.0
        self.samples = 0

    def add(self, value):
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            return
        if value < 0 or value > 86400000 or not math.isfinite(value):
            return
        self.counts[bisect.bisect_left(BUCKETS, value)] += 1
        self.samples += 1
        self.total += value

    def summary(self):
        def upper(fraction):
            if not self.samples:
                return None
            target = math.ceil(self.samples * fraction)
            seen = 0
            for i, count in enumerate(self.counts):
                seen += count
                if seen >= target:
                    return BUCKETS[i] if i < len(BUCKETS) else None
        return {'samples': self.samples,
                'mean_ms': self.total / self.samples if self.samples else None,
                'p50_upper_ms': upper(.5), 'p95_upper_ms': upper(.95),
                'overflow_samples': self.counts[-1]}


class Report:
    def __init__(self, max_pending=4096, max_groups=1024, since=None, until=None):
        if not 1 <= max_pending <= 65536 or not 1 <= max_groups <= 4096:
            raise ValueError('report bounds out of range')
        self.max_pending, self.max_groups = max_pending, max_groups
        self.since, self.until = since, until
        self.pending = OrderedDict()
        self.groups = {}
        self.coverage = dict.fromkeys(('lines', 'malformed_lines', 'oversized_lines',
            'pending_evicted', 'group_overflow', 'outside_window', 'terminal_without_start',
            'unidentified_terminal', 'attempt_overflow', 'timeline_overflow', 'candidates_omitted'), 0)

    def feed(self, event):
        if not isinstance(event, dict):
            self.coverage['malformed_lines'] += 1
            return
        fields = event.get('fields', {})
        if (not isinstance(fields, dict) or not isinstance(fields.get('message'), str)
                or fields['message'] not in MESSAGES):
            return
        spans = event.get('spans', [])
        if not isinstance(spans, list):
            spans = []
        spans = spans[:32] + [event.get('span', {})]
        rid = next((s.get('request_id') for s in reversed(spans)
                    if isinstance(s, dict) and isinstance(s.get('request_id'), str)), None)
        if not rid or len(rid) > 128:
            return
        endpoint = next((s.get('endpoint') for s in reversed(spans)
                         if isinstance(s, dict) and s.get('request_id') == rid and 'endpoint' in s), None)
        if endpoint == 'other':
            return
        message = fields['message']
        terminal = message in {'request body completed', 'request stream failed', 'request body cancelled', 'request handler cancelled'}
        try:
            at = timestamp(event.get('timestamp'))
        except (ValueError, TypeError, OverflowError):
            self.pending.pop(rid, None)
            self.coverage['malformed_lines'] += 1
            return
        # Select by terminal time while retaining pre-window correlation state.
        if terminal and (self.since is not None or self.until is not None):
            if ((self.since is not None and at < self.since) or
                    (self.until is not None and at >= self.until)):
                self.pending.pop(rid, None)
                self.coverage['outside_window'] += 1
                return
        if rid not in self.pending:
            if terminal:
                self.coverage['terminal_without_start'] += 1
            if len(self.pending) >= self.max_pending:
                self.pending.popitem(last=False)
                self.coverage['pending_evicted'] += 1
            self.pending[rid] = {'alias':'unknown', 'route':'unknown', 'class':'unknown',
                'failures':0, 'status':None, 'headers_ms':None, 'endpoint':enum(endpoint, {'chat','responses','embeddings'}),
                'build':'unknown', 'policy':'unknown', 'generation':-1, 'attempts':[],
                'selection_reasons':{}, 'eligibility':{}, 'first_output_ms':None}
        state = self.pending[rid]
        self.pending.move_to_end(rid)
        for name in ('alias', 'route', 'class'):
            if name in fields:
                state[name] = safe_label(fields[name])
        for field, name in [('build_source_sha256','build'), ('policy_sha256','policy')]:
            if field in fields:
                state[name] = digest(fields[field])
        generation = bounded_int(fields.get('policy_generation'), 2**64-1)
        if generation is not None:
            state['generation'] = generation
        if message == 'selection decision':
            reason = enum(fields.get('reason'), REASONS)
            state['selection_reasons'][reason] = state['selection_reasons'].get(reason, 0) + 1
            candidates = fields.get('candidates')
            if isinstance(candidates, str) and len(candidates) <= MAX_LINE_BYTES:
                try:
                    candidates = json.loads(candidates)
                except (ValueError, RecursionError):
                    candidates = []
            if isinstance(candidates, list):
                for candidate in candidates[:16]:
                    if isinstance(candidate, dict):
                        eligibility = enum(candidate.get('eligibility'), ELIGIBILITY)
                        state['eligibility'][eligibility] = state['eligibility'].get(eligibility, 0) + 1
            self.coverage['candidates_omitted'] += bounded_int(fields.get('candidates_omitted'), 1000000) or 0
        if message == 'upstream attempt finished':
            number = bounded_int(fields.get('attempt'), MAX_ATTEMPTS)
            if not number or len(state['attempts']) >= MAX_ATTEMPTS:
                self.coverage['attempt_overflow'] += 1
            else:
                state['attempts'].append((number, safe_label(fields.get('route')),
                    enum(fields.get('outcome'), ATTEMPT_OUTCOMES),
                    bounded_int(fields.get('http_status'), 599), duration(fields.get('elapsed_ms'))))
        if message == 'request first output':
            state['first_output_ms'] = duration(fields.get('elapsed_ms'))
        status = fields.get('status')
        if isinstance(status, int) and not isinstance(status, bool) and 100 <= status <= 599:
            state['status'] = status
        if message == 'upstream request failed':
            state['failures'] += 1
        if message in {'request response headers', 'request rejected or failed'}:
            state['headers_ms'] = duration(fields.get('elapsed_ms'))
        if not terminal:
            return
        self.pending.pop(rid)
        if state['endpoint'] == 'unknown' and state['alias'] == 'unknown':
            self.coverage['unidentified_terminal'] += 1
            return
        key = (state['alias'], state['route'], state['class'], state['endpoint'], state['build'], state['policy'], state['generation'])
        if key not in self.groups:
            if len(self.groups) >= self.max_groups:
                self.coverage['group_overflow'] += 1
                return
            self.groups[key] = {'total':0, 'completed':0, 'failed':0, 'rejected':0,
                'cancelled':0, 'retried':0, 'failed_attempts':0, 'retries':0,
                'recovered_after_retry':0, 'failed_after_retry':0, 'overload':0,
                'valid_backend_completed':0, 'backend_failed':0, 'backend_incomplete':0,
                'backend_unavailable':0, 'delivery_completed':0,
                'duration_ms':Histogram(), 'headers_ms':Histogram(), 'first_output_ms':Histogram(),
                'usage':{name:Usage() for name in ('input_tokens','output_tokens','total_tokens')},
                'attempt_timelines':{}, 'selection_reasons':{}, 'eligibility':{}}
        group = self.groups[key]
        group['total'] += 1
        attempts = state['attempts']
        failures = sum(outcome in {'invalid_body','transport','timeout','status'} for _,_,outcome,_,_ in attempts) if attempts else state['failures']
        group['failed_attempts'] += failures
        retries = bounded_int(fields.get('retries'), MAX_ATTEMPTS)
        if retries is None:
            count = bounded_int(fields.get('attempts'), MAX_ATTEMPTS)
            retries = max(0, count - 1) if count is not None else max(0, max((attempt[0] for attempt in attempts), default=0) - 1)
            if not attempts and count is None:
                retries = failures if message == 'request body completed' and (state['status'] or 500) < 400 else max(0, failures - 1)
        group['retries'] += retries
        # Retried records retry occurrence, independently of final delivery.
        recovered = int(retries > 0 and message == 'request body completed' and state['status'] is not None and state['status'] < 400)
        group['retried'] += int(retries > 0)
        group['recovered_after_retry'] += recovered
        if message in {'request body cancelled', 'request handler cancelled'}:
            outcome = 'cancelled'
        elif message == 'request stream failed' or (state['status'] or 0) >= 500:
            outcome = 'failed'
        elif (state['status'] or 0) >= 400:
            outcome = 'rejected'
        elif state['status'] is None:
            outcome = 'failed'
        else:
            outcome = 'completed'
        group[outcome] += 1
        group['failed_after_retry'] += int(outcome == 'failed' and retries > 0)
        group['overload'] += int(fields.get('outcome') == 'overload' or state['status'] == 429 or (state['status'] == 503 and not failures))
        group['delivery_completed'] += int(outcome == 'completed')
        backend = fields.get('backend_outcome')
        group[{'complete':'valid_backend_completed','failed':'backend_failed','incomplete':'backend_incomplete'}.get(backend if isinstance(backend,str) else None, 'backend_unavailable')] += 1
        group['duration_ms'].add(fields.get('elapsed_ms'))
        group['headers_ms'].add(state['headers_ms'])
        group['first_output_ms'].add(fields.get('first_output_ms', state['first_output_ms']))
        for name, usage in group['usage'].items():
            usage.add(fields.get(name))
        for name in ('selection_reasons','eligibility'):
            for category, count in state[name].items():
                group[name][category] = group[name].get(category, 0) + count
        for number, route, outcome, status, elapsed in attempts:
            timeline_key = (number, route, outcome, status or 0)
            timelines = group['attempt_timelines']
            if timeline_key not in timelines:
                if len(timelines) >= MAX_TIMELINES:
                    self.coverage['timeline_overflow'] += 1
                    continue
                timelines[timeline_key] = {'count':0, 'elapsed_ms':Histogram()}
            timelines[timeline_key]['count'] += 1
            timelines[timeline_key]['elapsed_ms'].add(elapsed)

    def read_lines(self, lines):
        for line in lines:
            self.coverage['lines'] += 1
            if len(line) > MAX_LINE_BYTES:
                self.coverage['oversized_lines'] += 1
                continue
            try:
                event = json.loads(line)
            except (ValueError, UnicodeError, RecursionError):
                self.coverage['malformed_lines'] += 1
                continue
            self.feed(event)

    def result(self):
        groups = []
        for (alias, route, cls, endpoint, build, policy, generation), group in sorted(self.groups.items()):
            row = {'alias':alias, 'route':route, 'class':cls, 'endpoint':endpoint,
                   'build_source_sha256':build, 'policy_sha256':policy,
                   'policy_generation':generation if generation >= 0 else None}
            row.update({k:v.summary() if isinstance(v, Histogram) else v for k,v in group.items() if k not in {'usage','attempt_timelines'}})
            row['usage'] = {name:usage.summary() for name, usage in group['usage'].items()}
            row['attempt_timelines'] = [{'attempt':number,'route':attempt_route,'outcome':outcome,
                'http_status':status or None,'count':values['count'],'elapsed_ms':values['elapsed_ms'].summary()}
                for (number,attempt_route,outcome,status),values in sorted(group['attempt_timelines'].items())]
            for name in ('completed', 'failed', 'rejected', 'cancelled', 'retried'):
                row[name + '_rate'] = row[name] / row['total']
            groups.append(row)
        return {'schema_version':2, 'outcome_semantics':'body polling completion is separate from valid backend completion; neither proves receipt or answer quality',
                'window_semantics':'terminal time; pre-window correlation retained when present',
                'percentiles':'fixed-bucket upper bounds; null for missing/overflow',
                'retry_semantics':'retried counts requests with retries; recovered_after_retry counts successful delivery after retries',
                'coverage':{**self.coverage, 'pending_at_end':len(self.pending),
                    'sink_loss':'not inferable from JSONL; compare protected sink counters'}, 'groups':groups}


def bounded_lines(stream):
    while True:
        line = stream.readline(MAX_LINE_BYTES + 1)
        if not line:
            return
        if len(line) > MAX_LINE_BYTES and not line.endswith(b'\n'):
            while True:
                tail = stream.readline(MAX_LINE_BYTES + 1)
                if not tail or tail.endswith(b'\n'):
                    break
        yield line


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--input', help='Slotline tracing JSONL; default stdin')
    parser.add_argument('--since', help='inclusive ISO timestamp with timezone')
    parser.add_argument('--until', help='exclusive ISO timestamp with timezone')
    args = parser.parse_args()
    try:
        since = timestamp(args.since) if args.since else None
        until = timestamp(args.until) if args.until else None
        if since is not None and until is not None and since >= until:
            parser.error('--since must precede --until')
        report = Report(since=since, until=until)
        if args.input:
            with open(args.input, 'rb') as stream:
                report.read_lines(bounded_lines(stream))
        else:
            report.read_lines(bounded_lines(sys.stdin.buffer))
        print(json.dumps(report.result(), sort_keys=True, allow_nan=False))
    except (OSError, ValueError) as error:
        parser.exit(2, str(error) + '\n')


if __name__ == '__main__':
    main()
