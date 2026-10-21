#!/usr/bin/env python3
"""Independent isolated HTTP acceptance. No production endpoints or credentials."""
import argparse
import collections
import hashlib
import http.client
import json
import os
import pathlib
import secrets
import socket
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

COUNTS = collections.Counter()
LOCK = threading.Lock()

class Backend(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    def log_message(self, *args):
        pass
    def do_GET(self):
        payload = json.dumps({'data': [{'id': 'fixture'}]}).encode()
        self.send_response(200)
        self.send_header('Content-Length', str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        scenario = body.get('messages', [{}])[0].get('content', 'probe')
        with LOCK:
            COUNTS[scenario] += 1
        if not body.get('stream'):
            if scenario == 'nonstream':
                time.sleep(1.3)
            payload = json.dumps({'model': 'fixture', 'choices': [{'message': {'role': 'assistant', 'content': 'OK'}}]}).encode()
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return
        if scenario in ('late-headers', 'headers-consume-budget'):
            time.sleep(1.4 if scenario == 'late-headers' else .6)
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Transfer-Encoding', 'chunked')
        self.end_headers()
        def send(raw):
            self.wfile.write(f'{len(raw):x}\r\n'.encode() + raw + b'\r\n')
            self.wfile.flush()
        def delta(content):
            send(('data: ' + json.dumps({'model': 'fixture', 'choices': [{'delta': content, 'finish_reason': None}]}) + '\n\n').encode())
        try:
            if scenario in ('silence', 'omitted', 'idle-only', 'headers-consume-budget'):
                time.sleep(.6 if scenario == 'headers-consume-budget' else 1.4)
                delta({'content': 'OK'})
                if scenario == 'idle-only':
                    time.sleep(1.5)
                    delta({'content': 'again'})
            elif scenario in ('heartbeat', 'role-only', 'partial'):
                for _ in range(16):
                    if scenario == 'heartbeat':
                        send(b': alive\n\n')
                    elif scenario == 'role-only':
                        delta({'role': 'assistant', 'content': ''})
                    else:
                        send(b'data:' if _ == 0 else b' ')
                    time.sleep(.12)
            elif scenario == 'idle':
                delta({'content': 'OK'})
                time.sleep(1.5)
                delta({'content': 'again'})
            elif scenario in ('productive', 'reasoning', 'tool', 'hard-cap'):
                for i in range(16 if scenario == 'hard-cap' else 5):
                    if scenario == 'tool':
                        call = {'index': 0, 'function': {'arguments': '{}' if i == 0 else ' '}}
                        if i == 0:
                            call.update({'id': 'call_fixture', 'type': 'function'})
                            call['function']['name'] = 'echo'
                        delta({'tool_calls': [call]})
                    else:
                        delta({'reasoning_content' if scenario == 'reasoning' else 'content': 'x'})
                    time.sleep(.3)
            else:
                delta({'content': 'OK'})
            send(('data: ' + json.dumps({'model': 'fixture', 'choices': [{'delta': {}, 'finish_reason': 'stop'}]}) + '\n\n').encode())
            send(b'data: [DONE]\n\n')
            self.wfile.write(b'0\r\n\r\n')
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', required=True)
    parser.add_argument('--result', required=True)
    args = parser.parse_args()
    binary = pathlib.Path(args.binary).resolve()
    backend = ThreadingHTTPServer(('127.0.0.1', 0), Backend)
    backend.daemon_threads = True
    threading.Thread(target=backend.serve_forever, daemon=True).start()
    host = os.environ.get('SLOTLINE_TEST_HOST', '100.64.0.1')
    with socket.socket() as probe:
        probe.bind((host, 0))
        port = probe.getsockname()[1]
    rows = []
    with tempfile.TemporaryDirectory(prefix='slotline-independent-progress-') as td:
        root = pathlib.Path(td)
        key = root / 'key'
        key.write_text(secrets.token_urlsafe(32))
        key.chmod(0o600)
        credentials = []
        for cls in ['timed', 'legacy', 'idleonly']:
            credentials.append({'name': cls, 'key_file': str(key), 'aliases': ['test'], 'class': cls, 'requests_per_minute': 200, 'burst': 20, 'max_in_flight': 2})
        # Different keys avoid ambiguous credential resolution.
        legacykey = root / 'legacy-key'
        legacykey.write_text(secrets.token_urlsafe(32))
        legacykey.chmod(0o600)
        credentials[1]['key_file'] = str(legacykey)
        idlekey = root / 'idle-key'
        idlekey.write_text(secrets.token_urlsafe(32))
        idlekey.chmod(0o600)
        credentials[2]['key_file'] = str(idlekey)
        config = {'bind': host, 'port': port, 'refresh_seconds': 60,
                  'credentials': credentials,
                  'class_timeout_budgets': {'timed': {'attempt_seconds': 3, 'total_seconds': 4, 'first_output_seconds': 1, 'stream_idle_seconds': 1}, 'legacy': {'attempt_seconds': 3, 'total_seconds': 4}, 'idleonly': {'attempt_seconds': 4, 'total_seconds': 5, 'stream_idle_seconds': 1}},
                  'routes': [{'name': 'fixture', 'kind': 'local', 'base_url': f'http://127.0.0.1:{backend.server_port}/v1', 'model': 'fixture', 'safe_concurrency': 1, 'context_limit': 2000, 'adapter_version': 'fixture', 'capabilities': ['text'], 'streaming': True, 'timeout_seconds': 3, 'enabled': True}],
                  'aliases': {'test': {'context_limit': 2000, 'capabilities': ['text'], 'streaming': True, 'routes': [{'route': 'fixture', 'priority': 10}]}}}
        path = root / 'config.json'
        path.write_text(json.dumps(config))
        check = subprocess.run([str(binary), '--check-config', str(path)], capture_output=True, text=True)
        assert check.returncode == 0, check.stderr
        with (root / 'log').open('w+') as log:
            process = subprocess.Popen([str(binary), str(path)], stdout=log, stderr=log)
            def snapshot():
                return json.loads(subprocess.check_output([str(binary), 'status', str(path), '--json'], text=True, stderr=subprocess.DEVNULL))
            try:
                for _ in range(100):
                    try:
                        if snapshot()['routes']['fixture']['state'] == 'serving':
                            break
                    except (subprocess.CalledProcessError, json.JSONDecodeError):
                        pass
                    time.sleep(.05)
                else:
                    raise AssertionError('Fixture qualification failed')
                for scenario, success, cls in [('silence', False, 'timed'), ('heartbeat', False, 'timed'), ('role-only', False, 'timed'), ('partial', False, 'timed'), ('idle', False, 'timed'), ('productive', True, 'timed'), ('reasoning', True, 'timed'), ('tool', True, 'timed'), ('headers-consume-budget', False, 'timed'), ('idle-only', False, 'idleonly'), ('late-headers', False, 'timed'), ('hard-cap', False, 'timed'), ('omitted', True, 'legacy'), ('nonstream', True, 'timed')]:
                    streaming = scenario != 'nonstream'
                    started = time.monotonic()
                    connection = http.client.HTTPConnection(host, port, timeout=7)
                    payload = {'model': 'test', 'messages': [{'role': 'user', 'content': scenario}], 'stream': streaming, 'max_tokens': 16}
                    credential = {'timed': key, 'legacy': legacykey, 'idleonly': idlekey}[cls]
                    before = COUNTS[scenario]
                    data = b''
                    error = None
                    status = None
                    try:
                        connection.request('POST', '/v1/chat/completions', json.dumps(payload), {'Authorization': 'Bearer ' + credential.read_text(), 'Content-Type': 'application/json'})
                        response = connection.getresponse()
                        status = response.status
                        request_id = response.getheader('x-request-id')
                        try:
                            data = response.read()
                        except http.client.IncompleteRead as exc:
                            data = exc.partial
                            error = 'IncompleteRead'
                    finally:
                        connection.close()
                    elapsed = time.monotonic() - started
                    completed = status == 200 and (b'[DONE]' in data if streaming else json.loads(data)['choices'][0]['message']['content'] == 'OK')
                    assert completed == success, (scenario, status, error, data)
                    assert COUNTS[scenario] - before == 1, (scenario, 'unexpected replay')
                    for _ in range(40):
                        snap = snapshot()
                        if snap['routes']['fixture']['active'] == 0:
                            break
                        time.sleep(.025)
                    assert snap['routes']['fixture']['active'] == 0, scenario
                    if scenario not in ('hard-cap',):
                        assert snap['routes']['fixture']['state'] == 'serving', (scenario, snap['routes']['fixture']['state'])
                    if scenario in ('silence', 'heartbeat', 'idle'):
                        assert .7 < elapsed < 1.4, (scenario, elapsed)
                    log.flush()
                    log.seek(0)
                    events = [json.loads(line) for line in log.read().splitlines() if line.startswith('{')]
                    correlated = [e.get('fields', {}) for e in events if e.get('span', {}).get('request_id') == request_id]
                    expected_category = {'silence': 'first_output_timeout', 'heartbeat': 'first_output_timeout', 'late-headers': 'first_output_timeout', 'idle': 'stream_idle_timeout', 'idle-only': 'stream_idle_timeout', 'headers-consume-budget': 'first_output_timeout', 'role-only': 'first_output_timeout', 'partial': 'first_output_timeout', 'hard-cap': 'deadline_exceeded'}.get(scenario)
                    if expected_category:
                        assert any(e.get('error_category') == expected_category for e in correlated), (scenario, 'missing correlated category')
                        if status == 200:
                            assert not any(e.get('message') == 'request body completed' for e in correlated), (scenario, 'false completion')
                    rows.append({'scenario': scenario, 'status': status, 'completed': completed, 'elapsed_seconds': round(elapsed, 3), 'error': error, 'active_after': 0, 'backend_attempts': 1})
                    print(json.dumps(rows[-1]), flush=True)
            finally:
                process.terminate()
                process.wait(timeout=10)
        backend.shutdown()
    result = {'passed': True, 'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(), 'scenarios': rows}
    pathlib.Path(args.result).write_text(json.dumps(result, indent=2))
    print('PASS: independent isolated SSE acceptance')

if __name__ == '__main__':
    main()
