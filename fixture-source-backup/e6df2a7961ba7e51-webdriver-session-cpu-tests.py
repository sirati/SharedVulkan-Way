import pathlib,subprocess,tempfile,os,json,time,hashlib,sys
PYTHON=sys.executable
JQ='/nix/store/m3y7hqmy20nd1ps6qy8410a326255bf0-jq-1.8.2-bin/bin/jq'
PAYLOAD=pathlib.Path('/mnt/datasets/builds/scratch/linux-bar-rdd/product-wayland/firefox-product-payload.sh')
E=pathlib.Path('/mnt/datasets/builds/evidence/linux-bar-rdd/product-wayland/webdriver-session-cpu');E.mkdir(exist_ok=True)
source=PAYLOAD.read_text();functions=source[source.index('# BEGIN fixture WebDriver client'):source.index('# END fixture WebDriver client')]
functions+='\n'+source[source.index('descendants() {'):source.index('capture_browser() {')]
functions+='\n'+source[source.index('browser_live() {'):source.index('cleanup() {')]
server_source=r'''
import http.server,json,pathlib,sys,time
base=pathlib.Path(sys.argv[1]);mode=sys.argv[2]
class Handler(http.server.BaseHTTPRequestHandler):
 def log_message(self,*args):pass
 def do_GET(self):self.reply(200,{'value':{'ready':True}})
 def do_POST(self):
  body=self.rfile.read(int(self.headers.get('Content-Length','0'))).decode()
  with (base/'requests.jsonl').open('a') as out:out.write(json.dumps({'path':self.path,'body':body})+'\n')
  if self.path=='/session':
   if mode=='timeout':time.sleep(.5)
   if mode=='http-error':return self.reply(500,{'value':{'error':'session not created','message':'first error retained'}})
   if mode=='malformed':return self.reply(200,None,b'{broken')
   return self.reply(200,{'value':{'sessionId':'owned-session-123','capabilities':{}}})
  if self.path.endswith('/execute/sync'):
   if 'quitRequested' in body:
    (base/'browser-quit').touch();return self.reply(200,{'value':{'quitRequested':True,'flag':'eAttemptQuit'}})
   return self.reply(500,{'value':{'error':'unknown error','message':'later command failed'}})
  return self.reply(200,{'value':None})
 def reply(self,code,value,raw=None):
  data=raw if raw is not None else json.dumps(value).encode()
  self.send_response(code);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(data)));self.end_headers()
  try:self.wfile.write(data)
  except BrokenPipeError:pass
server=http.server.HTTPServer(('127.0.0.1',0),Handler)
(base/'port').write_text(str(server.server_port));server.serve_forever()
'''
checks=[]
for mode in ['timeout','http-error','malformed','wrong-driver','success-later-failure']:
 with tempfile.TemporaryDirectory(prefix='webdriver-owned-session-') as temp:
  base=pathlib.Path(temp);(base/'server.py').write_text(server_source);(base/'client.sh').write_text(functions)
  log=(base/'geckodriver.log').open('wb');server=subprocess.Popen([PYTHON,'-B',str(base/'server.py'),temp,mode],stdout=log,stderr=subprocess.STDOUT)
  browser=None
  try:
   end=time.monotonic()+3
   while not (base/'port').exists():
    if time.monotonic()>end:raise AssertionError('mock driver startup deadline')
    time.sleep(.01)
   env=os.environ.copy();env.update(RUN=temp,GD=PYTHON,GD_PID=str(server.pid),JQ=JQ,WD_PORT=(base/'port').read_text())
   if mode=='timeout':
    # Scale only the test transport clock; record the source's actual75s request bound.
    (base/'curl').write_text('#!'+PYTHON+'\nimport os,sys,json\na=sys.argv[1:]\nopen(os.environ["RUN"]+"/curl-arguments.json","w").write(json.dumps(a))\nfor i,x in enumerate(a):\n if x=="--max-time":a[i+1]="0.10"\nos.execv("/run/current-system/sw/bin/curl",["curl"]+a)\n');(base/'curl').chmod(0o755);env['PATH']=temp+':'+env['PATH']
   if mode=='timeout':
    browser=subprocess.Popen([PYTHON,'-B','-c','import time; time.sleep(.3)']);env['FFPID']=str(browser.pid)
   if mode=='success-later-failure':
    browser=subprocess.Popen([PYTHON,'-B','-c','import pathlib,time,sys\np=pathlib.Path(sys.argv[1])\nwhile not p.exists():time.sleep(.02)',str(base/'browser-quit')]);env['FFPID']=str(browser.pid)
   script='set -euo pipefail\nsource "$RUN/client.sh"\nGD_START=$(wd_driver_start)\n'
   if mode=='wrong-driver':script+='GD_START=wrong-start-time\n'
   script+='sid=\nwd_wait_ready\n' if mode!='wrong-driver' else 'sid=\n'
   script+='if wd_create_owned_session; then create_rc=0; else create_rc=$?; fi\nprintf "%s\\n" "$create_rc" > "$RUN/create-rc"\n'
   script+='if [ -f "$RUN/session-first-response.json" ]; then sha256sum "$RUN/session-first-response.json" > "$RUN/first-before"; fi\n'
   script+='if wd_create_owned_session; then exit 91; fi\n'
   script+='if [ -f "$RUN/session-first-response.json" ]; then sha256sum "$RUN/session-first-response.json" > "$RUN/first-after"; cmp "$RUN/first-before" "$RUN/first-after"; fi\n'
   if mode=='success-later-failure':
    script+='test "$sid" = owned-session-123\nif wd "/session/$sid/execute/sync" \'{"script":"fail"}\' > "$RUN/later-failed.json"; then exit 92; fi\ntest "$sid" = owned-session-123\ngraceful_browser_shutdown\ntest -f "$RUN/shutdown-observed.txt"\ntest ! -e "$RUN/gpu-completion-uncertain.txt"\n'
   else:
    script+='test -z "$sid"\ntest ! -e "$RUN/owned-session-id.txt"\n'
    if mode=='timeout':script+='if graceful_browser_shutdown; then exit 93; fi\ntest -f "$RUN/gpu-completion-uncertain.txt"\n'
   result=subprocess.run(['bash','-c',script],env=env,capture_output=True,timeout=15)
   (E/(mode+'.stdout')).write_bytes(result.stdout);(E/(mode+'.stderr')).write_bytes(result.stderr);assert result.returncode==0,(mode,result.returncode,result.stderr.decode())
   time.sleep(.6 if mode=='timeout' else .02)
   requests=[json.loads(x) for x in (base/'requests.jsonl').read_text().splitlines()] if (base/'requests.jsonl').exists() else []
   assert len([r for r in requests if r['path']=='/session'])==(0 if mode=='wrong-driver' else 1),(mode,requests)
   if mode=='timeout':
    args=json.loads((base/'curl-arguments.json').read_text());assert args[args.index('--max-time')+1]=='75';assert (base/'webdriver/session-create/curl-exit.txt').read_text().strip()=='28'
   if mode=='http-error':assert (base/'webdriver/session-create/http-status.txt').read_text().strip()=='500' and b'first error retained' in (base/'session-first-response.json').read_bytes()
   if mode=='success-later-failure':
    browser.wait(timeout=2);assert all(r['path']=='/session' or r['path'].startswith('/session/owned-session-123/') for r in requests)
   checks.append({'case':mode,'POST_session_count':sum(r['path']=='/session' for r in requests),'passed':True})
   dest=E/mode;dest.mkdir(exist_ok=True)
   for name in ['session-first-response.json','session-create-failed.txt','session-owner.json','owned-session-id.txt','geckodriver-after-session-create.log','requests.jsonl','shutdown-observed.txt','gpu-completion-uncertain.txt']:
    if (base/name).exists():(dest/name).write_bytes((base/name).read_bytes())
  finally:
   server.terminate();server.wait(timeout=3);log.close()
   if browser and browser.poll() is None:browser.terminate();browser.wait(timeout=3)
receipt={'payload_sha256':hashlib.sha256(PAYLOAD.read_bytes()).hexdigest(),'cases':checks,'GPU_workloads':False,'all_test_servers_and_browser_surrogates_stopped':True}
(E/'receipt.json').write_text(json.dumps(receipt,indent=2)+'\n')
for c in checks:print('PASS',c['case'],'POST/session count',c['POST_session_count'])
print('payload',receipt['payload_sha256'])
