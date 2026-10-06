import json
import pathlib
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import argparse
import atexit
parser = argparse.ArgumentParser(description='Exercise actual tunnel-client harpoon routing against a local mock MCP server; no production keys or network services.')
parser.add_argument('--binary', default='tunnel-client')
args = parser.parse_args()
if not __debug__: parser.error("Run without -O so verification assertions remain enabled")
BINARY = args.binary
TEMPLATE = pathlib.Path(__file__).resolve().parents[1] / 'deploy/tunnel-client.example.yaml'
seen = []
redirect = False

class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args): pass
    def do_GET(self):
        seen.append((self.path, self.headers.get('Authorization')))
        if self.path != '/healthz':
            self.send_error(404)
            return
        self.send_response(302 if redirect else 200)
        if redirect: self.send_header('Location', '/local')
        self.end_headers()
        self.wfile.write(b'ok')
    def do_POST(self):
        seen.append((self.path, self.headers.get('Authorization')))
        raw = self.rfile.read(int(self.headers.get('Content-Length',0)))
        if not raw:
            self.send_response(202)
            self.end_headers()
            return
        msg = json.loads(raw)
        if msg['method'] == 'initialize':
            result = {'protocolVersion':'2025-03-26', 'capabilities':{'tools':{}},
                      'serverInfo':{'name':'test-main','version':'1'}}
        elif msg['method'] == 'tools/list':
            result = {'tools':[{'name':'test-read','inputSchema':{'type':'object'}}]}
        else: result = {}
        self.send_response(200)
        self.send_header('Content-Type','application/json')
        self.end_headers()
        self.wfile.write(json.dumps({'jsonrpc':'2.0','id':msg.get('id'),'result':result}).encode())

def post(url, method, params=None):
    msg = {'jsonrpc':'2.0','id':1,'method':method}
    if params is not None: msg['params'] = params
    req = urllib.request.Request(url, data=json.dumps(msg).encode(), headers={
        'Content-Type':'application/json','Accept':'application/json, text/event-stream'})
    try: response = urllib.request.urlopen(req, timeout=10)
    except urllib.error.HTTPError as error: response = error
    raw = response.read().decode()
    return response.status, json.loads(next((line[6:] for line in raw.splitlines()
        if line.startswith('data: ')), raw))

server = ThreadingHTTPServer(('127.0.0.1',0),Handler)
threading.Thread(target=server.serve_forever,daemon=True).start()
atexit.register(server.server_close)
atexit.register(server.shutdown)
origin = 'http://127.0.0.1:'+str(server.server_port)
with tempfile.TemporaryDirectory() as temp:
    root = pathlib.Path(temp)
    profile = 'config_version: 1\nadmin_ui:\n  open_browser: false\nmcp:\n  server_urls:\n    - channel: main\n      url: '+origin+'/mcp\n  extra_headers:\n    Authorization: Bearer synthetic-main-only\n  discovery_extra_headers:\n    Authorization: Bearer synthetic-discovery-only\n'
    profile += '\nharpoon:'+TEMPLATE.read_text().split('\nharpoon:',1)[1].replace('http://127.0.0.1:8080',origin)
    (root/'profile.yaml').write_text(profile)
    with (root/'proxy.log').open('w') as log:
        proc = subprocess.Popen([BINARY,'dev','proxy','--backend','go','--profile-file',str(root/'profile.yaml'),
            '--url-file',str(root/'url.json'),'--response-timeout','5s'],stdout=log,stderr=log)
        try:
            deadline = time.monotonic()+15
            while not (root/'url.json').exists():
                if proc.poll() is not None: raise RuntimeError((root/'proxy.log').read_text())
                if time.monotonic()>deadline: raise TimeoutError('proxy readiness')
                time.sleep(.05)
            main = json.loads((root/'url.json').read_text())['mcp_url']
            harpoon = main+'/harpoon'
            init = {'protocolVersion':'2025-03-26','capabilities':{},'clientInfo':{'name':'regression','version':'1'}}
            status, body = post(harpoon,'initialize',init)
            assert status == 200 and body['result']['serverInfo']['name']=='harpoon',body
            status, body = post(harpoon,'tools/list')
            assert status == 200 and any(t['name']=='call_target' for t in body['result']['tools']),body
            _,body = post(harpoon,'tools/call',{'name':'list_targets','arguments':{}})
            serialized = json.dumps(body)
            assert 'localmcp-health' in serialized and 'oauth-issuer' not in serialized,body
            before = len(seen)
            _,body = post(harpoon,'tools/call',{'name':'call_target','arguments':{'label':'localmcp-health','method':'GET'}})
            assert not body['result'].get('isError'),body
            assert seen[before:]==[('/healthz',None)],seen[before:]
            for label in ['unknown','http://127.0.0.1:8081/api/harpoon/status','/local','../local','%2e%2e/local','/healthz/']:
                before = len(seen)
                _,body = post(harpoon,'tools/call',{'name':'call_target','arguments':{'label':label,'method':'GET'}})
                assert 'error' in body or body.get('result',{}).get('isError'),body
                assert len(seen)==before,seen[before:]
            for url in [origin+'/local',origin+'/healthz/',origin+'/healthz?x=1',origin+'/healthz/../local','http://127.0.0.1:8081/readyz']:
                before = len(seen)
                _,body = post(harpoon,'tools/call',{'name':'call_target','arguments':{'label':'localmcp-health','method':'GET','url':url}})
                assert 'error' in body or body.get('result',{}).get('isError'),body
                assert len(seen)==before,seen[before:]
            redirect = True
            before = len(seen)
            _,body = post(harpoon,'tools/call',{'name':'call_target','arguments':{'label':'localmcp-health','method':'GET'}})
            assert 'error' in body or body.get('result',{}).get('isError'),body
            assert seen[before:]==[('/healthz',None)],seen[before:]
            status,body = post(main,'initialize',init)
            assert status==200 and body['result']['serverInfo']['name']=='test-main',body
            assert ('/mcp','Bearer synthetic-main-only') in seen,seen
            print('PASS: harpoon initialize/tools/list, health GET, header separation, denied labels/URL overrides, no redirects, main initialize/auth header')
        finally:
            proc.terminate()
            proc.wait(timeout=10)
server.shutdown()
