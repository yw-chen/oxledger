"""S1 e2e (manual; uses and modifies the dev DB in DATABASE_URL, needs docker container oxledger-pg and a built target/debug/oxledger): CLI -> serve (HTTP /api/v1) -> agent token -> MCP stdio, against the dev DB."""
import json, os, subprocess, time, urllib.request, urllib.error, http.cookiejar

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
EXE = ROOT + '/target/debug/oxledger.exe'
ENV = dict(os.environ, DATABASE_URL='postgres://oxledger:oxledger_dev@127.0.0.1:55432/oxledger',
           OXLEDGER_BIND='127.0.0.1:18080', OXLEDGER_SPA_ORIGIN='http://127.0.0.1:18080',
           OXLEDGER_INDEX_DIR=os.environ.get('TEMP', '.') + '/ox_e2e_idx')
PW = 'e2e password long enough'
BASE = 'http://127.0.0.1:18080/api/v1'

def cli(*args, stdin=None, ok=True):
    r = subprocess.run([EXE, *args], input=stdin, capture_output=True, text=True, env=ENV)
    print('$ oxledger', *args, '->', r.returncode, r.stdout.strip(), r.stderr.strip()[-200:])
    if ok:
        assert r.returncode == 0
    return r

cli('user', 'set-password', 'alice', stdin=PW)
cli('user', 'set-password', 'alice', stdin='short', ok=False)  # policy
r = cli('agent-client', 'add', 'E2E Agent', 'policy-e2e', ok=False)  # may exist from a previous run
cli('agent-client', 'enable', 'E2E Agent')

srv = subprocess.Popen([EXE, 'serve'], env=ENV, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
try:
    for _ in range(100):
        try:
            urllib.request.urlopen('http://127.0.0.1:18080/healthz', timeout=1); break
        except Exception:
            time.sleep(0.2)
    # Cookies are __Host- + Secure; over plain http a browser would drop them, so manage by hand.
    jar = {}
    def call(method, path, body=None, csrf=None):
        data = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(BASE + path, data=data, method=method)
        if jar:
            req.add_header('Cookie', '; '.join(f'{k}={v}' for k, v in jar.items()))
        if body is not None:
            req.add_header('Content-Type', 'application/json')
            req.add_header('Origin', 'http://127.0.0.1:18080')
        if csrf:
            req.add_header('X-CSRF-Token', csrf)
        try:
            resp = urllib.request.urlopen(req)
        except urllib.error.HTTPError as e:
            resp = e
        for c in resp.headers.get_all('Set-Cookie') or []:
            k, v = c.split(';')[0].split('=', 1)
            if v: jar[k] = v
            else: jar.pop(k, None)
        out = json.loads(resp.read() or b'null')
        print(method, path, resp.status, resp.headers.get('Cache-Control'), json.dumps(out)[:160])
        return resp.status, out
    _, lc = call('GET', '/auth/login-context')
    st, lo = call('POST', '/auth/login', {'login': 'alice', 'password': PW}, lc['data']['csrf_token'])
    assert st == 200
    csrf = lo['data']['csrf_token']
    _, clients = call('GET', '/agent-clients')
    client = next(c['id'] for c in clients['data'] if c['name'] == 'E2E Agent')
    sql = ("""INSERT INTO projects (key, name) VALUES ('ETOE','E2E') ON CONFLICT (key) DO NOTHING;
INSERT INTO enabled_modules SELECT id, 'issue_tracking' FROM projects WHERE key='ETOE' ON CONFLICT DO NOTHING;
INSERT INTO members (project_id, principal_id) SELECT p.id, u.id FROM projects p, principals u WHERE p.key='ETOE' AND u.login='alice' ON CONFLICT DO NOTHING;
INSERT INTO member_roles SELECT m.id, r.id FROM members m JOIN projects p ON p.id=m.project_id, roles r WHERE p.key='ETOE' AND r.name='Manager' ON CONFLICT DO NOTHING;
SELECT id FROM projects WHERE key='ETOE';""")
    proj = subprocess.run(['docker', 'exec', 'oxledger-pg', 'psql', '-U', 'oxledger', '-d', 'oxledger', '-Atqc', sql],
                          capture_output=True, text=True).stdout.strip().splitlines()[-1]
    print('project', proj)
    _, ctxs = call('GET', '/agent-contexts')
    st, d = call('POST', '/delegations', {'agent_client_id': client, 'project_id': proj,
                                          'proposal_ops': ['add_note'], 'writable_fields': []}, csrf)
    if st != 200:
        assert d['error']['details'].get('kind') in ('already_delegated',) or d['error']['code'] == 'context_unavailable', d
        if d['error']['code'] == 'context_unavailable':
            call('POST', '/agent-contexts', {'agent_client_id': client, 'fresh_context_ack': True}, csrf)
            st, d = call('POST', '/delegations', {'agent_client_id': client, 'project_id': proj,
                                                  'proposal_ops': ['add_note'], 'writable_fields': []}, csrf)
    _, ctxs = call('GET', '/agent-contexts')
    ctx = next(c['id'] for c in ctxs['data'] if c['agent_client_id'] == client and c['state'] == 'active')
    st, t = call('POST', f'/agent-contexts/{ctx}/credentials', {'label': 'e2e'}, csrf)
    assert st == 200, t
    token = t['data']['token']

    # Bearer on the SPA API is refused (T-A1a), with and without cookie (T-A3).
    req = urllib.request.Request(BASE + '/auth/session', headers={'Authorization': 'Bearer ' + token})
    try: urllib.request.urlopen(req)
    except urllib.error.HTTPError as e: print('bearer on /api/v1 ->', e.code, e.read()[:120])

    # MCP over HTTP with the agent token.
    def mcp_http(body, sid=None):
        req = urllib.request.Request('http://127.0.0.1:18080/mcp', data=json.dumps(body).encode(), method='POST')
        req.add_header('Authorization', 'Bearer ' + token)
        req.add_header('Content-Type', 'application/json')
        req.add_header('Accept', 'application/json, text/event-stream')
        if sid: req.add_header('Mcp-Session-Id', sid)
        try:
            resp = urllib.request.urlopen(req)
        except urllib.error.HTTPError as e:
            print('mcp http', e.code, e.read()[:200]); return None, None
        return resp.headers.get('Mcp-Session-Id'), resp.read().decode()
    sid, init = mcp_http({'jsonrpc': '2.0', 'id': 1, 'method': 'initialize', 'params': {
        'protocolVersion': '2025-06-18', 'capabilities': {}, 'clientInfo': {'name': 'e2e', 'version': '0'}}})
    print('mcp http initialize ok:', bool(init and 'serverInfo' in init))
    if sid:
        mcp_http({'jsonrpc': '2.0', 'method': 'notifications/initialized'}, sid)
        _, res = mcp_http({'jsonrpc': '2.0', 'id': 2, 'method': 'tools/call', 'params': {
            'name': 'search', 'arguments': {}}}, sid)
        print('mcp http search:', (res or '')[:400])
finally:
    srv.terminate()

# MCP stdio with the same token.
p = subprocess.Popen([EXE, 'stdio'], env=dict(ENV, OXLEDGER_API_KEY=token), stdin=subprocess.PIPE,
                     stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
msgs = [
    {'jsonrpc': '2.0', 'id': 1, 'method': 'initialize', 'params': {'protocolVersion': '2025-06-18', 'capabilities': {}, 'clientInfo': {'name': 'e2e', 'version': '0'}}},
    {'jsonrpc': '2.0', 'method': 'notifications/initialized'},
    {'jsonrpc': '2.0', 'id': 2, 'method': 'tools/call', 'params': {'name': 'search', 'arguments': {}}},
]
for m in msgs:
    p.stdin.write(json.dumps(m) + '\n'); p.stdin.flush()
out = []
for _ in range(2):
    out.append(p.stdout.readline())
print('stdio search:', out[1][:600])
# Revoke the token mid-process; the next call must fail (T-A10).
subprocess.run(['docker', 'exec', 'oxledger-pg', 'psql', '-U', 'oxledger', '-d', 'oxledger', '-Atc',
                "update agent_credentials set revoked_at = now() where revoked_at is null"], capture_output=True)
p.stdin.write(json.dumps({'jsonrpc': '2.0', 'id': 3, 'method': 'tools/call', 'params': {'name': 'search', 'arguments': {}}}) + '\n'); p.stdin.flush()
print('stdio after revoke:', p.stdout.readline()[:400])
p.kill()
