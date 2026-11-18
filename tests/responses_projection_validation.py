#!/usr/bin/env python3
"""Compiled HTTP fault diagnosis for local Responses projection; no model calls.

Use --result outside the source tree. This fixture does not identify the cause
of a historical stream failure whose original event bytes were not captured.
"""
import argparse
import hashlib
import http.client
import json
import pathlib
import secrets
import socket
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Backend(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, *_args):
        pass

    def send_json(self, value):
        raw = json.dumps(value).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self):
        self.send_json({'data': [{'id': 'fixture'}]})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        output = [{'type': 'message', 'content': [{'type': 'output_text', 'text': 'OK'}]}]
        response = {'object': 'response', 'model': 'fixture', 'status': 'completed', 'output': output}
        scenario = body.get('input')
        active = isinstance(scenario, str) and scenario.startswith('case:')
        if active:
            self.server.case_requests += 1
        if not body.get('stream'):
            if self.path.endswith('/responses'):
                if body.get('tools'):
                    response['output'] = [{'type': 'function_call', 'call_id': 'call_probe', 'name': body['tools'][0]['name'], 'arguments': '{}'}]
                self.send_json(response)
            else:
                message = {'role': 'assistant', 'content': 'OK'}
                if body.get('tools'):
                    message['tool_calls'] = [{'id': 'call_probe', 'type': 'function', 'function': {'name': body['tools'][0]['function']['name'], 'arguments': '{}'}}]
                self.send_json({'model': 'fixture', 'choices': [{'message': message}]})
            return
        events = [{'type': 'response.created', 'response': {'object': 'response', 'model': 'fixture'}}]
        if active:
            scenario = scenario.removeprefix('case:')
            tool = body['tools'][0]
            item = {'type': 'function_call', 'id': 'item_fixture', 'call_id': 'call_fixture', 'name': tool['name'], 'arguments': json.dumps({'input': 'PRIVATE_ARGUMENT_MARKER'})}
            if scenario == 'undeclared':
                item['name'] = 'undeclared_PRIVATE_NAME_MARKER'
            elif scenario == 'native_custom':
                item['type'] = 'custom_tool_call'
                item['input'] = item.pop('arguments')
            elif scenario == 'malformed_custom':
                item['arguments'] = '{PRIVATE_ARGUMENT_MARKER'
            elif scenario == 'extra_custom_field':
                item['arguments'] = json.dumps({'input': 'PRIVATE_ARGUMENT_MARKER', 'extra': True})
            if scenario != 'truncated':
                events.append({'type': 'response.output_item.added', 'item': item})
                events.append({'type': 'response.output_item.done', 'item': item})
                response['output'] = [item]
        if scenario != 'truncated':
            events.append({'type': 'response.completed', 'response': response})
        raw = b''.join(('event: ' + e['type'] + '\ndata: ' + json.dumps(e) + '\n\n').encode() for e in events)
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Content-Length', str(len(raw)))
        self.end_headers()
        try:
            self.wfile.write(raw)
        except (BrokenPipeError, ConnectionResetError):
            pass


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', required=True)
    parser.add_argument('--result', required=True)
    args = parser.parse_args()
    binary = pathlib.Path(args.binary).resolve()
    destination = pathlib.Path(args.result).resolve()
    destination.parent.mkdir(parents=True, exist_ok=True)
    receipt = {'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(), 'scope': 'Synthetic HTTP diagnostics, not live inference or historical root-cause proof', 'cases': [], 'ok': False}
    backend = ThreadingHTTPServer(('127.0.0.1', 0), Backend)
    backend.case_requests = 0
    backend.daemon_threads = True
    threading.Thread(target=backend.serve_forever, daemon=True).start()
    try:
        for scenario, reason in [('valid', None), ('undeclared', 'upstream called an undeclared tool'), ('native_custom', 'unsupported projected output item'), ('malformed_custom', 'invalid custom tool arguments'), ('extra_custom_field', 'invalid custom tool arguments'), ('truncated', 'Upstream SSE ended without terminal')]:
            row = {'scenario': scenario, 'expected_reason': reason, 'passed': False}
            receipt['cases'].append(row)
            with tempfile.TemporaryDirectory(prefix='rsp-') as td:
                root = pathlib.Path(td)
                key = secrets.token_urlsafe(32)
                keyfile = root / 'key'
                keyfile.write_text(key)
                keyfile.chmod(0o600)
                with socket.socket() as sock:
                    sock.bind(('127.0.0.1', 0))
                    port = sock.getsockname()[1]
                caps = ['text', 'tools', 'responses']
                config = {'listener_mode': 'loopback', 'bind': '127.0.0.1', 'port': port, 'refresh_seconds': 60,
                          'routes': [{'name': 'fixture', 'kind': 'local', 'base_url': f'http://127.0.0.1:{backend.server_port}/v1', 'model': 'fixture', 'adapter_version': 'test', 'safe_concurrency': 1, 'context_limit': 8192, 'capabilities': caps, 'streaming': True, 'enabled': True}],
                          'aliases': {'test': {'context_limit': 8192, 'capabilities': caps, 'streaming': True, 'routes': [{'route': 'fixture', 'priority': 10}]}},
                          'credentials': [{'name': 'test', 'key_file': str(keyfile), 'aliases': ['test'], 'operator': True, 'cloud_allowed': False, 'class': 'interactive', 'requests_per_minute': 600, 'burst': 60, 'max_in_flight': 1}]}
                path = root / 'config.json'
                path.write_text(json.dumps(config))
                check = subprocess.run([str(binary), '--check-config', str(path)], capture_output=True, timeout=10)
                assert check.returncode == 0, check.stderr.decode()
                with (root / 'log').open('w+') as log:
                    process = subprocess.Popen([str(binary), str(path)], stdout=log, stderr=log)
                    def request(method, endpoint, payload=None):
                        connection = http.client.HTTPConnection('127.0.0.1', port, timeout=8)
                        data = b''
                        try:
                            connection.request(method, endpoint, json.dumps(payload) if payload else None, {'Authorization': 'Bearer ' + key, 'Content-Type': 'application/json'})
                            try:
                                response = connection.getresponse()
                            except http.client.RemoteDisconnected:
                                return None, data, 'RemoteDisconnected'
                            try:
                                while chunk := response.read1(4096):
                                    data += chunk
                                error = None
                            except (http.client.IncompleteRead, ConnectionResetError) as exc:
                                data += getattr(exc, 'partial', b'')
                                error = type(exc).__name__
                            return response.status, data, error
                        finally:
                            connection.close()
                    try:
                        for _ in range(100):
                            try:
                                status, data, _ = request('GET', '/healthz')
                                if status == 200 and json.loads(data)['routes']['fixture']['state'] == 'serving':
                                    break
                            except (OSError, http.client.HTTPException):
                                pass
                            time.sleep(.05)
                        else:
                            raise AssertionError('backend qualification failed')
                        payload = {'model': 'test', 'input': 'case:' + scenario, 'stream': True, 'tools': [{'type': 'namespace', 'name': 'native', 'tools': [{'type': 'custom', 'name': 'execute', 'description': 'Test only'}]}]}
                        before = backend.case_requests
                        status, data, error = request('POST', '/v1/responses', payload)
                        assert status == 200 or (reason and status is None), (scenario, status, data)
                        assert backend.case_requests == before + 1, 'unexpected upstream replay'
                        events = [json.loads(line[6:]) for line in data.decode().splitlines() if line.startswith('data: ')]
                        completed = any(e['type'] == 'response.completed' for e in events)
                        assert completed == (reason is None), (scenario, events)
                        if reason is None:
                            item = next(e['item'] for e in events if e['type'] == 'response.output_item.done')
                            assert (item['name'], item['namespace'], item['type'], item['input']) == ('execute', 'native', 'custom_tool_call', 'PRIVATE_ARGUMENT_MARKER')
                        else:
                            assert b'PRIVATE_' not in data and b'output_item' not in data, 'rejected tool leaked to client'
                            status2, _, _ = request('POST', '/v1/responses', payload)
                            assert status2 == 503, (scenario, status2)
                            assert backend.case_requests == before + 1, 'cooldown dispatched a retry'
                            row['immediate_retry_status'] = status2
                        for _ in range(40):
                            _, health, _ = request('GET', '/healthz')
                            health = json.loads(health)['routes']['fixture']
                            if health['active'] == 0:
                                break
                            time.sleep(.025)
                        assert health['active'] == 0, 'permit leaked'
                        log.flush()
                        log.seek(0)
                        logs = log.read()
                        assert 'PRIVATE_' not in logs and key not in logs, 'sensitive diagnostic payload'
                        failures = [json.loads(line)['fields'] for line in logs.splitlines() if 'upstream stream failed' in line]
                        assert [f.get('validation_reason') for f in failures] == ([reason] if reason else []), failures
                        row.update(passed=True, actual_reasons=[f.get('validation_reason') for f in failures], body_aborted=bool(error), returned_event_types=[e['type'] for e in events], upstream_case_requests=backend.case_requests-before, final_active=health['active'], final_state=health['state'], private_payload_absent_from_logs=True)
                    finally:
                        process.terminate()
                        try:
                            process.wait(timeout=5)
                        except subprocess.TimeoutExpired:
                            process.kill()
                            process.wait(timeout=5)
                        row['process_stopped'] = process.poll() is not None
        receipt['ok'] = True
    except Exception as exc:
        receipt['error'] = str(exc)
        raise
    finally:
        backend.shutdown()
        backend.server_close()
        destination.write_text(json.dumps(receipt, indent=2) + '\n')
    print(json.dumps({'ok': receipt['ok'], 'cases': len(receipt['cases']), 'result': str(destination)}))


if __name__ == '__main__':
    main()
