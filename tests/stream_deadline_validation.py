"""Verify class deadlines cover full SSE consumption and release route permits.

Run after cargo build --release --locked. Set SLOTLINE_TEST_HOST to a local
Tailnet IPv4 and SLOTLINE_TEST_PORT to an unused port when needed.
"""
import json, pathlib, tempfile, subprocess, threading, time, urllib.request, urllib.error, os
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
ROOT = pathlib.Path(__file__).resolve().parents[1]
BIN = os.environ.get('SLOTLINE_TEST_BINARY', str(ROOT / 'target/release/slotline'))
HOST = os.environ.get('SLOTLINE_TEST_HOST', '100.127.255.254')
PORT = int(os.environ.get('SLOTLINE_TEST_PORT', '14013'))
class Backend(BaseHTTPRequestHandler):
 protocol_version='HTTP/1.1'
 def log_message(self,format,*args): pass
 def do_GET(self):
  b=json.dumps({'data':[{'id':'fixture'},{'id':'fallback'}]}).encode(); self.send_response(200);self.send_header('Content-Length',str(len(b)));self.end_headers();self.wfile.write(b)
 def do_POST(self):
  body=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
  if body.get('model')=='fixture' and body.get('messages',[{}])[0].get('content')=='fail-primary':
   self.close_connection=True; self.connection.shutdown(2); self.connection.close(); return
  if not body.get('stream'):
   b=json.dumps({'model':body.get('model'),'choices':[{'message':{'content':'OK'}}]}).encode();self.send_response(200);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(b)));self.end_headers();self.wfile.write(b);return
  self.send_response(200); self.send_header('Content-Type','text/event-stream');self.send_header('Transfer-Encoding','chunked');self.end_headers()
  try:
   for i in range(4):
    b=('data: '+json.dumps({'model':body.get('model'),'choices':[{'delta':{'content':'OK' if i==0 else ''},'finish_reason':'stop' if i==3 else None}]})+'\n\n').encode();self.wfile.write(f'{len(b):x}\r\n'.encode()+b+b'\r\n');self.wfile.flush();time.sleep(.45)
   b=b'data: [DONE]\n\n';self.wfile.write(f'{len(b):x}\r\n'.encode()+b+b'\r\n0\r\n\r\n');self.wfile.flush()
  except (BrokenPipeError,ConnectionResetError): pass
server=ThreadingHTTPServer(('127.0.0.1',0),Backend);threading.Thread(target=server.serve_forever,daemon=True).start();backend_port=server.server_port
results=[]
request_ids=set()
with tempfile.TemporaryDirectory(prefix='slotline-stream-regression-') as td:
 root=pathlib.Path(td); key=root/'key'; key.write_text('fixture-test-key-012345678901234567890');key.chmod(0o600)
 for budget in [False,True]:
  cfg={'bind':HOST,'port':PORT,'refresh_seconds':30,'credentials':[{'name':'fixture','key_file':str(key),'aliases':['test'],'class':'long-stream','requests_per_minute':100,'burst':10,'max_in_flight':2}],'routes':[{'name':'fixture','kind':'local','base_url':f'http://127.0.0.1:{backend_port}/v1','model':'fixture','safe_concurrency':1,'context_limit':2000,'adapter_version':'fixture','capabilities':['text'],'streaming':True,'timeout_seconds':1,'enabled':True}],'aliases':{'test':{'context_limit':2000,'capabilities':['text'],'streaming':True,'routes':[{'route':'fixture','priority':10}]}}}
  if budget:
   cfg['class_timeout_budgets']={'long-stream':{'attempt_seconds':3,'total_seconds':4}}
   fallback=dict(cfg['routes'][0],name='fallback',model='fallback')
   cfg['routes'].append(fallback)
   cfg['aliases']['test']['routes'].append({'route':'fallback','priority':20})
  path=root/'config.json';path.write_text(json.dumps(cfg));check=subprocess.run([BIN,'--check-config',str(path)],capture_output=True,text=True);assert check.returncode==0,check.stderr; log=(root/'stderr').open('w+');p=subprocess.Popen([BIN,str(path)],stdout=log,stderr=log)
  try:
   for _ in range(100):
    status=subprocess.run([BIN,'status',str(path),'--json'],capture_output=True,text=True)
    if status.returncode==0 and all(route['state']=='serving' for route in json.loads(status.stdout)['routes'].values()): break
    time.sleep(.05)
   else: raise AssertionError('fixture not ready')
   # A rejection must also return and log a server-owned ID.
   rejected=urllib.request.Request(f'http://{HOST}:{PORT}/v1/chat/completions',data=b'{}',headers={'Content-Type':'application/json','X-Request-ID':'untrusted-caller-id'})
   try:
    urllib.request.urlopen(rejected,timeout=5)
    raise AssertionError('unauthenticated request accepted')
   except urllib.error.HTTPError as rejection:
    assert rejection.code==401
    rejected_id=rejection.headers.get('x-request-id')
    assert rejected_id and len(rejected_id)==32
    rejection.read()
   req=urllib.request.Request(f'http://{HOST}:{PORT}/v1/chat/completions',data=json.dumps({'model':'test','messages':[{'role':'user','content':'ok'}],'stream':True,'max_tokens':16}).encode(),headers={'Authorization':'Bearer '+key.read_text(),'Content-Type':'application/json','X-Request-ID':'untrusted-caller-id'})
   t=time.monotonic();data=b'';err=None;request_id=None
   try:
    with urllib.request.urlopen(req,timeout=5) as r:
     request_id = r.headers.get('x-request-id')
     assert request_id and len(request_id) == 32, 'missing generated request ID'
     data=r.read()
   except Exception as e: err=type(e).__name__
   assert request_id != 'untrusted-caller-id' and request_id not in request_ids
   request_ids.add(request_id)
   log.flush(); log.seek(0)
   events=[json.loads(line) for line in log.read().splitlines() if line.startswith('{')]
   correlated=[event for event in events if event.get('span',{}).get('request_id')==request_id]
   messages={event.get('fields',{}).get('message') for event in correlated}
   assert any(e.get('span',{}).get('request_id')==rejected_id and e.get('fields',{}).get('message')=='request rejected or failed' for e in events)
   assert request_id != rejected_id
   assert 'route selected' in messages, messages
   assert 'request routed' in messages, messages
   assert ('request stream failed' if not budget else 'request body completed') in messages, messages
   assert 'untrusted-caller-id' not in json.dumps(events)
   row={'class_override':budget,'elapsed_s':round(time.monotonic()-t,2),'done':b'[DONE]' in data,'error':err};results.append(row);print(json.dumps(row),flush=True)
   assert row['done']==budget
   if not budget: assert err == 'IncompleteRead', row
   if budget:
    fallback_req=urllib.request.Request(f'http://{HOST}:{PORT}/v1/chat/completions',data=json.dumps({'model':'test','messages':[{'role':'user','content':'fail-primary'}],'max_tokens':16}).encode(),headers={'Authorization':'Bearer '+key.read_text(),'Content-Type':'application/json'})
    with urllib.request.urlopen(fallback_req,timeout=5) as response:
     fallback_id=response.headers.get('x-request-id'); assert fallback_id and fallback_id!=request_id
     assert json.loads(response.read())['choices'][0]['message']['content']=='OK'
    log.flush();log.seek(0)
    fallback_events=[json.loads(l) for l in log.read().splitlines() if l.startswith('{')]
    fallback_events=[e for e in fallback_events if e.get('span',{}).get('request_id')==fallback_id]
    assert any(e['fields'].get('message')=='upstream request failed' for e in fallback_events),fallback_events
    assert any(e['fields'].get('message')=='request routed' and e['fields'].get('route')=='fallback' for e in fallback_events),fallback_events
   snapshot=json.loads(subprocess.check_output([BIN,'status',str(path),'--json'],text=True));assert snapshot['routes']['fixture']['active']==0
  finally: p.terminate();p.wait(timeout=10)
server.shutdown()
print('PASS: timeout reproduces truncated stream; scoped budget completes same stream and releases permit')
