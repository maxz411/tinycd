#!/usr/bin/env python3
"""Exercise a real hub, TLS agent, Git repository and deployed applications."""
import argparse
import base64
import hashlib
import html
import http.client
import json
import os
import re
from pathlib import Path
import signal
import socket
import ssl
import struct
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
import urllib.parse


def port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


def eventually(check, timeout=60):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            last = check()
            if last:
                return last
        except (OSError, ValueError, urllib.error.URLError):
            pass
        time.sleep(.1)
    raise AssertionError(f'condition timed out; last answer: {last!r}')


class AgentSocket:
    """Minimal WebSocket client for the untrusted-host boundary regression."""
    def __init__(self, port, join):
        raw = socket.create_connection(('127.0.0.1', port), timeout=5)
        self.sock = ssl._create_unverified_context().wrap_socket(raw, server_hostname='localhost')
        _, fingerprint, secret = join.split('.')
        assert hashlib.sha256(self.sock.getpeercert(binary_form=True)).hexdigest() == fingerprint
        key = base64.b64encode(os.urandom(16)).decode()
        self.sock.sendall((f'GET /agent HTTP/1.1\r\nHost: localhost:{port}\r\n'
                           f'Upgrade: websocket\r\nConnection: Upgrade\r\n'
                           f'Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n'
                           f'Authorization: Bearer {secret}\r\n\r\n').encode())
        headers = b''
        while not headers.endswith(b'\r\n\r\n'):
            headers += self.sock.recv(1)
        assert headers.startswith(b'HTTP/1.1 101'), headers
        self.send(type='register', name='rogue', os='test', version='test', runtimes=['process'])

    def send(self, **message):
        payload = json.dumps(dict(v=1, **message)).encode()
        mask = os.urandom(4)
        header = bytes([0x81, 0x80 | len(payload)]) if len(payload) < 126 else bytes([0x81, 0xfe]) + struct.pack('!H', len(payload))
        self.sock.sendall(header + mask + bytes(byte ^ mask[i % 4] for i, byte in enumerate(payload)))

    def close(self):
        self.sock.close()


class Smoke:
    def __init__(self, binary, root, oci):
        self.binary, self.root, self.oci = binary, root, oci
        self.children = []
        self.data = root / 'hub'
        self.bind, self.secure, self.app_port = port(), port(), port()
        self.url = f'http://127.0.0.1:{self.bind}'
        self.token = None
        self.repo = root / 'repo'
        self.repo.mkdir()
        self.git('init', '-b', 'main')
        self.git('config', 'user.email', 'smoke@example.invalid')
        self.git('config', 'user.name', 'tinycd smoke')
        self.commit('first')

    def git(self, *args):
        return subprocess.run(['git', '-C', str(self.repo), *args], check=True,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True).stdout.strip()

    def commit(self, version):
        (self.repo / 'version').write_text(version)
        self.git('add', '.')
        self.git('commit', '-m', version)

    def spawn(self, name, *args):
        log = (self.root / f'{name}.log').open('ab')
        child = subprocess.Popen([str(self.binary), *map(str, args)], stdin=subprocess.DEVNULL,
                                 stdout=log, stderr=log)
        log.close()
        self.children.append(child)
        return child

    def stop(self, child):
        if child.poll() is None:
            child.send_signal(signal.SIGTERM)
            child.wait(timeout=45)
        assert child.returncode == 0, (child.args, child.returncode)

    def hub(self):
        child = self.spawn('hub', '--data', self.data, 'serve', '--no-tui',
                           '--bind', f'127.0.0.1:{self.bind}',
                           '--secure-bind', f'127.0.0.1:{self.secure}')
        eventually(lambda: (self.data / 'api-token').exists())
        self.token = (self.data / 'api-token').read_text().strip()
        eventually(lambda: self.api('/api/overview')['hosts'][0]['online'])
        return child

    def api(self, path, body=None, method=None, token=True):
        headers = {'Authorization': f'Bearer {self.token}'} if token else {}
        if body is not None:
            body = json.dumps(body).encode()
            headers['Content-Type'] = 'application/json'
        request = urllib.request.Request(self.url + path, body, headers, method=method)
        with urllib.request.urlopen(request, timeout=90) as response:
            return json.load(response)

    def project(self, name):
        return self.api(f'/api/projects/{name}')

    def deploy(self, name, status='live', timeout=60):
        ident = self.api(f'/api/projects/{name}/deploy', method='POST')['deployment']
        def settled():
            release = next(d for d in self.project(name)['deployments'] if d['id'] == ident)
            if release['status'] in ('live', 'failed', 'cancelled'):
                assert release['status'] == status, release
                return release
        return eventually(settled, timeout)

    def body(self):
        return urllib.request.urlopen(f'http://127.0.0.1:{self.app_port}/version', timeout=2).read().decode()

    def lifecycle(self, host, runtime, name):
        recipe = {'start': 'exec python3 -m http.server "$PORT" --bind 127.0.0.1',
                  'port': self.app_port,
                  'check': f'curl -fsS http://127.0.0.1:{self.app_port}/version'}
        if runtime == 'oci':
            recipe.update(start='exec httpd -f -p "$PORT" -h /app', base='alpine:3.22')
        config = self.repo / '.tinycd' / 'config.toml'
        config.parent.mkdir(exist_ok=True)
        config.write_text('\n'.join(f'{key} = {json.dumps(value)}' for key, value in recipe.items()))
        self.git('add', '.')
        self.git('commit', '-m', 'deployment recipe')
        project = self.api('/api/projects', {'id': name, 'repo': str(self.repo), 'host': host,
                                           'runtime': runtime, 'poll': None})
        first = self.deploy(name)['id']
        eventually(lambda: self.body() == 'first')
        self.commit('second')
        second = self.deploy(name)['id']
        eventually(lambda: self.body() == 'second')
        result = self.api(f'/api/projects/{name}/rollback', {'to': first})
        assert result['deployment'] == first
        eventually(lambda: self.project(name)['current'] == first and self.body() == 'first')
        config.write_text('start = "exit 7"\n')
        self.commit('broken')
        self.deploy(name, status='failed')
        # The previous release must use its original recipe when recovering.
        eventually(lambda: self.body() == 'first')
        # The failed push did not move current or alter its recipe.
        assert self.project(name)['current'] == first
        self.api(f'/api/projects/{name}/stop', method='POST')
        eventually(lambda: not self.project(name)['running'])
        self.api(f'/api/projects/{name}/restart', method='POST')
        eventually(lambda: self.project(name)['running'] and self.body() == 'first')
        self.api(f'/api/projects/{name}?stop=true', method='DELETE')
        eventually(lambda: all(p['id'] != name for p in self.api('/api/overview')['projects']))
        print(f'PASS {runtime} lifecycle on {host}', flush=True)

    def keep_running(self):
        marker = self.root / 'detached.pid'
        self.api('/api/projects', {'id': 'detached', 'repo': str(self.repo), 'poll': None,
                                  'recipe': {'start': f'echo $$ > {marker}; exec sleep 300'}})
        self.deploy('detached')
        pid = int(marker.read_text())
        try:
            self.api('/api/projects/detached?stop=false', method='DELETE')
            time.sleep(.3)
            os.kill(pid, 0)
        finally:
            os.kill(pid, signal.SIGTERM)
        print('PASS explicit keep-running process removal', flush=True)

    def reconcile(self, hub, agent, host):
        marker = self.root / 'orphan.pid'
        self.api('/api/projects', {'id': 'orphan', 'repo': str(self.repo), 'host': host, 'poll': None,
                                  'recipe': {'start': f'echo $$ > {marker}; exec sleep 300'}})
        self.deploy('orphan')
        pid = int(marker.read_text())
        os.kill(agent.pid, signal.SIGSTOP)
        try:
            self.stop(hub)
            hub = self.hub()
            self.api('/api/projects/orphan?stop=true', method='DELETE')
        finally:
            os.kill(agent.pid, signal.SIGCONT)
        eventually(lambda: any(h['id'] == host and h['online'] for h in self.api('/api/overview')['hosts']))
        def gone():
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                return True
        eventually(gone)
        print('PASS removed assignment reconciles after offline host reconnect', flush=True)
        return hub

    def web_setup(self):
        def request(method, path, form=None):
            connection = http.client.HTTPConnection('127.0.0.1', self.bind, timeout=40)
            headers = {'Authorization': f'Bearer {self.token}'}
            if form is not None:
                headers['Content-Type'] = 'application/x-www-form-urlencoded'
                form = urllib.parse.urlencode(form)
            connection.request(method, path, form, headers)
            response = connection.getresponse()
            result = response.status, dict(response.getheaders()), response.read().decode()
            connection.close()
            return result
        status, _, page = request('GET', '/github/connect')
        assert status == 200
        state = re.search(r'state=([a-f0-9]+)', html.unescape(page)).group(1)
        status, headers, _ = request('GET', '/github/callback?state=wrong&code=invalid')
        assert status == 303 and headers['location'].startswith('/settings?error=')
        status, headers, _ = request('GET', f'/github/callback?state={state}&code=invalid')
        assert status == 303
        assert 'pending+connection' not in headers['location'], headers
        request('POST', '/settings', {'public_url': 'https://proxy.example.invalid',
                                     'poll': '30', 'password': '', 'confirm': ''})
        join = self.api('/api/hosts', {'name': 'proxy-test'})
        assert join['url'] != 'https://proxy.example.invalid'
        print('PASS cross-site GitHub callback state and direct TLS join URL', flush=True)

    def boundaries(self):
        # An unrelated authenticated host must not mutate a local project or
        # choose paths for logs/adopted releases.
        self.api('/api/projects', {'id': 'victim', 'repo': str(self.repo), 'poll': None})
        join = self.api('/api/hosts', {'name': 'rogue'})
        rogue = AgentSocket(self.secure, join['token'])
        try:
            rogue.send(type='state', project='victim', current='forged', running=True, retained=['forged'])
            rogue.send(type='log', project='../..', deployment='escaped', line='untrusted')
            eventually(lambda: any(h['name'] == 'rogue' and h['online'] for h in self.api('/api/overview')['hosts']))
            self.api('/api/projects', {'id': 'rogue-app', 'repo': str(self.repo), 'host': 'rogue', 'poll': None})
            rogue.send(type='state', project='rogue-app', current='../escaped', running=True, retained=['../escaped'])
            rogue.send(type='log', project='rogue-app', deployment='../escaped', line='untrusted')
            time.sleep(.3)
            assert self.project('victim')['current'] is None
            assert self.project('rogue-app')['current'] is None
            assert not (self.root / 'escaped.log').exists()
            assert not (self.data / 'deployments' / 'escaped.json').exists()
        finally:
            rogue.close()
        print('PASS host ownership and untrusted state/log paths', flush=True)

    def bounded_check(self):
        marker = self.root / 'check.pid'
        config = self.repo / '.tinycd' / 'config.toml'
        config.write_text(f'start = "exec sleep 300"\ncheck = {json.dumps(f"echo $$ > {marker}; sleep 300")}\n')
        self.commit('hanging-check')
        self.api('/api/projects', {'id': 'timeout-app', 'repo': str(self.repo), 'poll': None})
        started = time.monotonic()
        release = self.deploy('timeout-app', status='failed', timeout=50)
        assert '30 seconds' in release['error'], release
        assert time.monotonic() - started < 45
        pid = int(marker.read_text())
        try:
            os.kill(pid, 0)
            raise AssertionError(f'check process {pid} survived its deadline')
        except ProcessLookupError:
            pass
        assert not self.project('timeout-app')['running']
        self.api('/api/projects/timeout-app?stop=true', method='DELETE')
        print('PASS health-check deadline and child cleanup', flush=True)

    def run(self):
        hub = self.hub()
        try:
            self.api('/api/overview', token=False)
            raise AssertionError('unauthenticated API succeeded')
        except urllib.error.HTTPError as error:
            assert error.code == 401
        self.web_setup()
        self.boundaries()
        self.lifecycle('local', 'process', 'local-app')
        # Reset the fixture for the same lifecycle on an outbound TLS host.
        self.commit('first')
        join = self.api('/api/hosts', {'name': 'remote'})
        fingerprint = join['token'].split('.')[1]
        context = ssl._create_unverified_context()
        with socket.create_connection(('127.0.0.1', self.secure)) as sock:
            with context.wrap_socket(sock, server_hostname='localhost') as secure:
                assert hashlib.sha256(secure.getpeercert(binary_form=True)).hexdigest() == fingerprint
        agent_data = self.root / 'agent'
        agent = self.spawn('agent', '--data', agent_data, 'agent',
                           f'https://127.0.0.1:{self.secure}', '--token', join['token'])
        host = eventually(lambda: next((h['id'] for h in self.api('/api/overview')['hosts']
                                        if h['name'] == 'remote' and h['online']), None))
        self.lifecycle(host, 'process', 'remote-app')
        hub = self.reconcile(hub, agent, host)
        self.stop(agent)
        agent = self.spawn('agent', '--data', agent_data, 'agent')
        eventually(lambda: any(h['id'] == host and h['online'] for h in self.api('/api/overview')['hosts']))
        print('PASS hub and agent restart with saved TLS credentials', flush=True)
        self.stop(agent)
        self.keep_running()
        self.bounded_check()
        if self.oci:
            self.commit('first')
            self.lifecycle('local', 'oci', 'container-app')
        self.stop(hub)

    def close(self):
        for child in reversed(self.children):
            if child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=45)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait()


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=Path('target/debug/tinycd'))
    parser.add_argument('--oci', action='store_true', help='also exercise the real local container engine')
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix='tinycd-smoke-') as directory:
        smoke = Smoke(args.binary.resolve(), Path(directory), args.oci)
        try:
            smoke.run()
        except BaseException:
            for log in Path(directory).glob('*.log'):
                print(f'--- {log.name} ---\n{log.read_text(errors="replace")}')
            raise
        finally:
            smoke.close()
