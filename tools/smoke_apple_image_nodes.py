#!/usr/bin/env python3
"""Two isolated inferd processes, real mTLS, real native image execution on B.

Uses a small encoded image, not shared host paths. Does not modify live services.
This is same-host protocol acceptance, not physical LAN validation.
"""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import socket
import ssl
import struct
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
from smoke_trusted_nodes import certificates, free_port, tls_config, write_private, wait_for, file_sha256
from smoke_apple_image import png_fixture, CORE, CAP, ROOT

IMAGE_PROTOCOL = 'infer.node.apple-image@20260926.2'
TEXT_PROTOCOL = 'infer.node.text@20260922.1'
OPERATIONS = ('ocr', 'aesthetics', 'segment')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--photo', type=Path)
    parser.add_argument('--sdk', type=Path)
    parser.add_argument('--report', type=Path, required=True)
    args = parser.parse_args()
    binary, worker = ROOT / 'target/debug/inferd', ROOT / 'target/apple-image-worker'
    report = {'scope': 'same_host_two_process_mtls', 'binary_sha256': file_sha256(binary),
              'worker_sha256': file_sha256(worker), 'checks': []}
    with tempfile.TemporaryDirectory(prefix='infer-apple-nodes-') as tmp:
        root = Path(tmp)
        fingerprints = certificates(root)
        ports = {name: free_port() for name in ('a', 'b')}
        node_port = free_port()
        fixture = root / 'ocr.png'
        png_fixture(fixture)
        token = os.urandom(32).hex()
        sample = (ROOT / 'config/apple-image.example.toml').read_text()
        configs = {}
        for name in ('a', 'b'):
            (root / name).mkdir()
            configs[name] = sample.replace('127.0.0.1:8788', f'127.0.0.1:{ports[name]}').replace(
                '/absolute/path/to/infer-runtime/target/apple-image-worker', str(worker)).replace(
                '.infer-runtime/apple-example', str(root / name)).replace(
                'placement = ["local_only"]', 'placement = ["local_only", "private"]')
        for name in ('a', 'b'):
            grants = ['apple_' + op for op in OPERATIONS]
            if name == 'a':
                grants += ['remote_' + op for op in OPERATIONS]
            configs[name] += '\n[apps.apple-example.routing]\ndeployment_ids = ' + json.dumps(grants) + '\n'
        peers = root / 'peers.json'
        write_private(peers, {fingerprints['a']: {'node_id': 'a', 'apps': {'apple-example': 'apple-example'}, 'exports': list(OPERATIONS)}})
        configs['b'] += f'\n[node_server]\nnode_id = "b"\nbind = "127.0.0.1:{node_port}"\npeers_file = "{peers}"\nmax_active = 2\n[node_server.tls]\n{tls_config(root, "b")}\n'
        for op in OPERATIONS:
            configs['b'] += f'[node_server.exports.{op}]\ndeployment = "apple_{op}"\nintent = "apple.{op}"\n'
        (root / 'b.toml').write_text(configs['b'])
        offers = json.loads(subprocess.check_output([binary, '--config', root / 'b.toml', '--print-node-offers']))
        configs['a'] += f'''\n[providers.node_b]
kind = "trusted_node"
placement = "trusted_node"
[providers.node_b.capability_profile]
version = 1
protocol = "responses"
capabilities = ["responses", "metadata"]
[providers.node_b.node]
node_id = "b"
address = "127.0.0.1:{node_port}"
server_name = "b.test"
certificate_sha256 = "{fingerprints['b']}"
[providers.node_b.node.tls]
{tls_config(root, 'a')}
[providers.node_b.node.imports]
'''
        configs['a'] += ''.join(f'{offer["deployment_id"]} = "{offer["contract_digest"]}"\n' for offer in offers)
        for op in OPERATIONS:
            configs['a'] += f'''[model_builds.remote_{op}]
profile = "apple_{op}"
model_id = "{op}"
input_modalities = ["image"]
output_modalities = ["json"]
[deployments.remote_{op}]
provider = "node_b"
build = "remote_{op}"
'''
        (root / 'a.toml').write_text(configs['a'])
        processes, logs = {}, []
        def passed(check, **details):
            report['checks'].append({'check': check, **details})
            print('PASS ' + check, flush=True)
            args.report.write_text(json.dumps(report, indent=2) + '\n')
        def call(op, data, placement='private'):
            options = {'operation': op}
            if op == 'segment':
                options['points'] = [{'x': .3, 'y': .5, 'include': True}]
            parameters = {'model': 'apple.' + op, 'source_revision': 'node-smoke-r1', 'options': options,
                          'metadata': {'infer.placement': placement, 'infer.offline_required': 'true',
                                       'infer.fallback': 'none', 'infer.deployment_ids': 'remote_' + op}}
            boundary = 'apple-node-smoke'
            body = (f'--{boundary}\r\nContent-Disposition: form-data; name="request"\r\n\r\n' + json.dumps(parameters) +
                    f'\r\n--{boundary}\r\nContent-Disposition: form-data; name="image"; filename="input"\r\n\r\n').encode() + data + f'\r\n--{boundary}--\r\n'.encode()
            request = urllib.request.Request(f'http://127.0.0.1:{ports["a"]}/infer/v1/vision/apple-images', data=body, headers={
                'Authorization': 'Bearer ' + token, 'Infer-Consumer-Contract': CORE,
                'Infer-Capability-Contract': CAP, 'Content-Type': 'multipart/form-data; boundary=' + boundary})
            try:
                with urllib.request.urlopen(request, timeout=125) as response:
                    return response.status, json.load(response)
            except urllib.error.HTTPError as error:
                return error.code, json.load(error)
        def rpc(command, generation=None, alpn=IMAGE_PROTOCOL, protocol=IMAGE_PROTOCOL):
            context = ssl.create_default_context(cafile=str(root / 'ca.pem'))
            context.load_cert_chain(root / 'a.pem', root / 'a.key')
            context.set_alpn_protocols([alpn])
            with socket.create_connection(('127.0.0.1', node_port), timeout=3) as raw:
                with context.wrap_socket(raw, server_hostname='b.test') as sock:
                    assert sock.selected_alpn_protocol() == alpn
                    payload = json.dumps({'protocol': protocol, 'generation': generation, 'command': command}).encode()
                    sock.sendall(struct.pack('!I', len(payload)) + payload)
                    def read(size):
                        data = b''
                        while len(data) < size:
                            part = sock.recv(size - len(data))
                            if not part:
                                raise ConnectionError('node closed connection')
                            data += part
                        return data
                    size = struct.unpack('!I', read(4))[0]
                    assert 0 < size <= 1024 * 1024
                    return json.loads(read(size))
        try:
            for name in ('b', 'a'):
                log = (root / f'{name}.log').open('w+')
                logs.append(log)
                env = dict(os.environ, INFER_APPLE_IMAGE_TOKEN=token, INFRA_PROTOCOL_RUNTIME_DIR=str(root / name / 'discovery'))
                processes[name] = subprocess.Popen([binary, '--config', root / f'{name}.toml'], env=env, stdout=log, stderr=log)
                def ready():
                    if processes[name].poll() is not None:
                        log.seek(0)
                        raise AssertionError(log.read()[-5000:])
                    try:
                        with urllib.request.urlopen(f'http://127.0.0.1:{ports[name]}/health', timeout=1) as response:
                            return response.status == 200
                    except OSError:
                        return False
                wait_for(ready, 25)
            assert len({p.pid for p in processes.values()}) == 2
            catalog = rpc({'op': 'catalog'})
            assert catalog['node_id'] == 'b' and catalog['protocol'] == IMAGE_PROTOCOL
            passed('image ALPN authenticates paired node B')
            for op in OPERATIONS:
                path = args.photo if op == 'segment' and args.photo else fixture
                assert path.stat().st_size <= 192 * 1024, 'test image exceeds bounded node input limit'
                start = time.monotonic()
                status, value = call(op, path.read_bytes())
                assert status == 200, (status, value)
                assert value['provider'] == 'node_b' and value['deployment'] == 'remote_' + op
                assert value['source_revision'] == 'node-smoke-r1' and value['result']['operation'] == op
                provenance = value['provenance']
                assert provenance['execution_node'] == 'b' and provenance['execution_location'] == 'device'
                assert provenance['worker_sha256'] == report['worker_sha256']
                if op == 'ocr':
                    assert any('INFER' in line['text'].upper() for line in value['result']['lines'])
                if op == 'segment':
                    raster = value['result']['raster']
                    assert hashlib.sha256(base64.b64decode(raster['data_base64'], validate=True)).hexdigest() == raster['sha256']
                passed('native ' + op + ' executes on B and returns authenticated provenance', round_trip_ms=int((time.monotonic() - start) * 1000))
            if args.sdk:
                credential=root/'sdk-token';credential.write_text(token);credential.chmod(0o600)
                sdk=subprocess.run([str(args.sdk),'--endpoint',f'http://127.0.0.1:{ports["a"]}',
                                    '--node-deployment','remote_ocr',str(credential),str(fixture),'ocr'],
                                   capture_output=True,text=True,timeout=125)
                assert sdk.returncode==0,sdk.stderr
                result=json.loads(sdk.stdout)
                assert result['provenance']['execution_node']=='b'
                assert result['provider']=='node_b' and result['job_state']=='succeeded'
                passed('negotiated SDK consumer executes over paired image transport', result=result)
            assert call('ocr', fixture.read_bytes(), 'local_only')[0] != 200
            passed('local_only refuses forced remote image deployment')
            assert call('ocr', b'x' * (192 * 1024 + 1))[0] != 200
            passed('oversized encoded image fails before remote dispatch')
            mismatch = rpc({'op': 'catalog'}, alpn=TEXT_PROTOCOL)
            assert mismatch['reply'] == {'kind': 'error', 'value': 'protocol'}
            passed('text ALPN cannot carry image protocol envelope')
            retired = rpc({'op': 'catalog'}, protocol='infer.node.apple-image@20260926.1')
            assert retired['reply'] == {'kind': 'error', 'value': 'protocol'}
            passed('retired image protocol is rejected')
            write_private(peers, {})
            assert call('ocr', fixture.read_bytes())[0] != 200
            passed('revoked peer cannot execute native image work')
        finally:
            for process in processes.values():
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
            for log in logs:
                log.close()
    args.report.write_text(json.dumps(report, indent=2) + '\n')


if __name__ == '__main__':
    main()
