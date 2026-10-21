import importlib.util
import pathlib
import unittest

P = pathlib.Path(__file__).resolve().parents[1] / 'scripts' / 'audit_report.py'

def load():
    spec = importlib.util.spec_from_file_location('audit_report', P)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod

def event(rid, message, **fields):
    return {'timestamp':'2026-10-02T12:00:00Z','fields':{'message':message,**fields},'span':{'request_id':rid,'name':'request','endpoint':'chat'}}

class AuditReportTests(unittest.TestCase):
    def test_policy_build_and_backend_delivery_are_independent(self):
        m=load();r=m.Report()
        for rid, policy in [('a','a'*64),('b','b'*64)]:
            r.feed(event(rid,'request policy',policy_sha256=policy,build_source_sha256='c'*64,policy_generation=2))
            r.feed(event(rid,'request body cancelled',status=200,backend_outcome='complete',input_tokens=0,total_tokens=5,first_output_ms=10))
        out=r.result();self.assertEqual(len(out['groups']),2)
        for group in out['groups']:
            self.assertEqual(group['valid_backend_completed'],1)
            self.assertEqual(group['delivery_completed'],0)
            self.assertEqual(group['usage']['input_tokens']['samples'],1)
            self.assertEqual(group['usage']['output_tokens']['samples'],0)
            self.assertEqual(group['first_output_ms']['samples'],1)

    def test_attempt_timelines_reasons_and_failed_retry(self):
        m=load();r=m.Report()
        r.feed(event('a','selection decision',reason='occupancy_rank',candidates='[{"eligibility":"full"}]'))
        r.feed(event('a','upstream attempt finished',attempt=1,route='first',outcome='status',http_status=502,elapsed_ms=5))
        r.feed(event('a','upstream request failed',route='first'))
        r.feed(event('a','upstream attempt finished',attempt=2,route='second',outcome='invalid_body',http_status=200,elapsed_ms=6))
        r.feed(event('a','upstream request failed',route='second'))
        r.feed(event('a','request body completed',attempts=2,retries=1,outcome='failed_after_retry',status=502,backend_outcome='failed'))
        group=r.result()['groups'][0]
        self.assertEqual(group['retried'],1)
        self.assertEqual(group['retried_rate'],1)
        self.assertEqual(group['failed_after_retry'],1)
        self.assertEqual(group['retries'],1)
        self.assertEqual(group['failed_attempts'],2)
        self.assertEqual(len(group['attempt_timelines']),2)
        self.assertEqual(group['selection_reasons']['occupancy_rank'],1)
        self.assertEqual(group['eligibility']['full'],1)

    def test_malformed_timestamp_is_rejected_without_window(self):
        m=load();r=m.Report();e=event('a','request body completed',status=200)
        e['timestamp']='2026-10-02 12:00:00';r.feed(e)
        self.assertEqual(r.result()['groups'],[])
        self.assertEqual(r.result()['coverage']['malformed_lines'],1)

    def test_untrusted_enum_and_sample_fields_are_bounded(self):
        m=load();r=m.Report()
        r.feed(event('a','selection decision',reason='PRIVATE',candidates='[{"eligibility":"PRIVATE"}]'))
        r.feed(event('a','request handler cancelled',status=0,route='PRIVATE\n',input_tokens=True,total_tokens=10**12,first_output_ms=False))
        out=r.result();self.assertNotIn('PRIVATE',str(out))
        group=out['groups'][0];self.assertEqual(group['cancelled'],1)
        self.assertEqual(group['usage']['input_tokens']['samples'],0)
        self.assertEqual(group['usage']['total_tokens']['samples'],0)

    def test_attempt_and_aggregate_timeline_bounds(self):
        m=load();r=m.Report()
        for attempt in range(m.MAX_ATTEMPTS+10):
            r.feed(event('a','upstream attempt finished',attempt=1,route='route'+str(attempt),outcome='timeout',elapsed_ms={'PRIVATE':'x'*1000}))
        r.feed(event('a','request body completed',status=502))
        out=r.result();self.assertEqual(out['coverage']['attempt_overflow'],10)
        self.assertEqual(len(out['groups'][0]['attempt_timelines']),m.MAX_TIMELINES)
        self.assertNotIn('PRIVATE',str(out))
        self.assertTrue(all(entry['elapsed_ms']['samples']==0 for entry in out['groups'][0]['attempt_timelines']))

    def test_new_policy_correlation_is_terminal_time_windowed(self):
        m=load();r=m.Report(since=m.timestamp('2026-10-02T12:01:00Z'))
        r.feed(event('a','request policy',build_source_sha256='a'*64,policy_sha256='b'*64,policy_generation=1))
        r.feed(event('a','upstream attempt finished',attempt=1,route='route',outcome='validated',elapsed_ms=2))
        terminal=event('a','request body completed',status=200,backend_outcome='complete',attempts=1)
        terminal['timestamp']='2026-10-02T12:02:00Z';r.feed(terminal)
        group=r.result()['groups'][0];self.assertEqual(group['policy_sha256'],'b'*64)
        self.assertEqual(group['attempt_timelines'][0]['count'],1)

    def test_stream_failure_is_not_success(self):
        m=load();r=m.Report()
        r.feed(event('a','request routed',alias='qwen',route='spark',**{'class':'interactive'}))
        r.feed(event('a','request response headers',status=200,elapsed_ms=20))
        r.feed(event('a','request stream failed',status=200,elapsed_ms=30))
        out=r.result();g=out['groups'][0]
        self.assertEqual(g['completed'],0)
        self.assertEqual(g['failed'],1)
        self.assertEqual(g['route'],'spark')
        self.assertEqual(g['duration_ms']['samples'],1)
    def test_pending_and_groups_are_bounded(self):
        m=load();r=m.Report(max_pending=2,max_groups=1)
        for i in range(3):r.feed(event(str(i),'request response headers',status=200))
        self.assertEqual(len(r.pending),2)
        self.assertEqual(r.result()['coverage']['pending_evicted'],1)
    def test_retries_and_private_fields(self):
        m=load();r=m.Report()
        r.feed(event('a','upstream request failed',route='aipc',alias='qwen',credential='SECRET',prompt='PRIVATE',**{'class':'interactive'}))
        r.feed(event('a','request routed',route='spark',alias='qwen',**{'class':'interactive'}))
        r.feed(event('a','request body completed',status=200,elapsed_ms=80))
        out=r.result();self.assertEqual(out['groups'][0]['retried'],1)
        self.assertNotIn('SECRET',str(out));self.assertNotIn('PRIVATE',str(out))

    def test_failed_delivery_after_retry_counts_as_retried_not_recovered(self):
        m=load();r=m.Report()
        r.feed(event('a','upstream attempt finished',attempt=1,route='first',outcome='timeout'))
        r.feed(event('a','request body completed',attempts=2,retries=1,status=502,backend_outcome='failed'))
        group=r.result()['groups'][0]
        self.assertEqual(group['retried'],1)
        self.assertEqual(group['retried_rate'],1)
        self.assertEqual(group['recovered_after_retry'],0)
        self.assertEqual(group['failed_after_retry'],1)
    def test_missing_timing_stays_unavailable(self):
        m=load();r=m.Report();r.feed(event('a','request body completed',status=200))
        self.assertIsNone(r.result()['groups'][0]['duration_ms']['p95_upper_ms'])
    def test_straddling_request_keeps_route_attribution(self):
        m=load();r=m.Report(since=m.timestamp('2026-10-02T12:01:00Z'))
        r.feed(event('a','request routed',alias='qwen',route='spark',**{'class':'interactive'}))
        end=event('a','request body completed',status=200,elapsed_ms=80000)
        end['timestamp']='2026-10-02T12:02:00Z';r.feed(end)
        self.assertEqual(r.result()['groups'][0]['route'],'spark')

    def test_timestamp_grammar_rejects_invalid_offsets(self):
        m=load()
        for value in ['2026-10-02 12:00:00+00:00','2026-10-02T12:00:00+00:60']:
            with self.assertRaises(ValueError):m.timestamp(value)

    def test_operator_calls_are_excluded(self):
        m=load();r=m.Report()
        e=event('op','request body completed',status=200)
        e['span']['endpoint']='other'
        r.feed(e)
        self.assertEqual(r.result()['groups'],[])

    def test_time_window_excludes_events(self):
        m=load();r=m.Report(since=m.timestamp('2026-10-02T13:00:00Z'))
        r.feed(event('a','request body completed',status=200))
        self.assertEqual(r.result()['groups'],[])
        self.assertEqual(r.result()['coverage']['outside_window'],1)

    def test_malformed_input_and_oversized_lines(self):
        m=load();r=m.Report();r.read_lines([b'not json\n',b'x'* (m.MAX_LINE_BYTES+1)])
        self.assertEqual(r.result()['coverage']['malformed_lines'],1)
        self.assertEqual(r.result()['coverage']['oversized_lines'],1)

if __name__=='__main__':unittest.main()
