#!/usr/bin/env python3
"""Real A/B/C inferd processes, real mTLS, deterministic local text backends.

All identities, credentials, state, discovery directories and ports are private
to a TemporaryDirectory. No installed Console or daemon is modified.
"""
from __future__ import annotations

import argparse
import concurrent.futures
from datetime import datetime, timezone
import hashlib
import http.server
import json
import os
from pathlib import Path
import socket
import ssl
import struct
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request

PROTOCOL = "infer.node.text@20260922.1"
CORE = "infer-runtime.consumer-core@20260813.1"
CAPABILITY = "infer.responses@20260812.1"


def wait_for(predicate, timeout=12):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        time.sleep(0.05)
    raise AssertionError("condition did not become true before deadline")


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def write_private(path, value):
    temporary = path.with_suffix(".new")
    temporary.write_text(json.dumps(value), encoding="utf-8")
    temporary.chmod(0o600)
    temporary.replace(path)


def file_sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


class Backend:
    def __init__(self, name):
        self.name, self.calls = name, []
        self.started = threading.Event()
        self.release = threading.Event()
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_POST(self):
                request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                owner.calls.append(request)
                if owner.name == "B" and request["input"] == "fail-on-b":
                    self.send_response(503)
                    self.send_header("Content-Type", "application/json")
                    self.end_headers()
                    self.wfile.write(b'{"error":"deterministic backend failure"}')
                    return
                if request["input"] == "hold":
                    owner.started.set()
                    owner.release.wait(20)
                text = "x" * 600_000 if request["input"] == "large-result" else owner.name
                body = json.dumps({"output": [{"type": "message", "content": [
                    {"type": "output_text", "text": text}
                ]}], "usage": {"input_tokens": 1, "output_tokens": 1}}).encode()
                try:
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
                except (BrokenPipeError, ConnectionResetError):
                    pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    @property
    def port(self):
        return self.server.server_address[1]

    def close(self):
        self.release.set()
        self.server.shutdown()
        self.server.server_close()


def certificates(root):
    def openssl(*args):
        subprocess.run(["openssl", *map(str, args)], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    ca_config = root / "ca.cnf"
    ca_config.write_text("[req]\ndistinguished_name=dn\nx509_extensions=ca\n[dn]\n[ca]\nbasicConstraints=critical,CA:TRUE\nkeyUsage=critical,keyCertSign,cRLSign\nsubjectKeyIdentifier=hash\n")
    openssl("req", "-config", ca_config, "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "2", "-subj", "/CN=Infer smoke CA",
            "-keyout", root / "ca.key", "-out", root / "ca.pem")
    fingerprints = {}
    for name in ("a", "b", "c", "rogue"):
        openssl("req", "-newkey", "rsa:2048", "-nodes", "-subj", f"/CN={name}.test",
                "-keyout", root / f"{name}.key", "-out", root / f"{name}.csr")
        ext = root / f"{name}.ext"
        ext.write_text(f"basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth,clientAuth\nsubjectAltName=DNS:{name}.test\n")
        openssl("x509", "-req", "-in", root / f"{name}.csr", "-CA", root / "ca.pem", "-CAkey", root / "ca.key",
                "-CAcreateserial", "-days", "2", "-extfile", ext, "-out", root / f"{name}.pem")
        (root / f"{name}.key").chmod(0o600)
        der = ssl.PEM_cert_to_DER_cert((root / f"{name}.pem").read_text())
        fingerprints[name] = hashlib.sha256(der).hexdigest()
    (root / "ca.key").chmod(0o600)
    return fingerprints


def tls_config(root, name):
    return f'certificate = "{root / (name + ".pem")}"\nprivate_key = "{root / (name + ".key")}"\nca_certificate = "{root / "ca.pem"}"\n'


def base_config(root, name, port, backend):
    return f'''[server]
bind = "127.0.0.1:{port}"
[observer]
enabled = false
instance_id = "smoke-{name}"
[auth]
managed_credentials_directory = "{root / name / 'credentials'}"
[persistence]
path = "{root / name / 'jobs.sqlite3'}"
[defaults]
policy = "local-first"
[profiles.local-first]
order = ["placement", "cost"]
[providers.local]
kind = "responses"
base_url = "http://127.0.0.1:{backend.port}/v1"
placement = "local"
[providers.local.capability_profile]
version = 1
protocol = "responses"
capabilities = ["responses", "instructions", "max_output_tokens", "metadata"]
[intents."text.summarize"]
input_modalities = ["text"]
output_modalities = ["text"]
default_capability_floor = "foundational"
[model_profiles.dummy]
family = "deterministic-test-backend"
[model_profiles.dummy.ratings."text.summarize"]
level = "foundational"
status = "provisional"
[model_builds.local]
profile = "dummy"
model_id = "test-model"
input_modalities = ["text"]
output_modalities = ["text"]
[deployments.local_text]
provider = "local"
build = "local"
[apps.consumer]
credential = {{ source = "environment", variable = "INFER_NODE_SMOKE_TOKEN" }}
allowed_intents = ["text.summarize"]
allowed_policies = ["local-first"]
allowed_cloud_input_modalities = []
[apps.consumer.routing]
deployment_ids = ["local_text"]
[apps.consumer.request_overrides]
placement = ["local_only", "private", "anywhere", "cloud_only"]
prefer = ["local", "trusted_node"]
offline_required = true
fallback = ["none", "equivalent"]
'''


def node_config(root, name, port):
    return f'''[node_server]
node_id = "{name}"
bind = "127.0.0.1:{port}"
peers_file = "{root / (name + '-peers.json')}"
max_active = 2
[node_server.tls]
{tls_config(root, name)}
[node_server.exports.shared]
deployment = "local_text"
intent = "text.summarize"
'''


def import_config(root, name, port, fingerprint, contract):
    return f'''[providers.node_{name}]
kind = "trusted_node"
placement = "trusted_node"
[providers.node_{name}.capability_profile]
version = 1
protocol = "responses"
capabilities = ["responses", "instructions", "max_output_tokens", "metadata"]
[providers.node_{name}.node]
node_id = "{name}"
address = "127.0.0.1:{port}"
server_name = "{name}.test"
certificate_sha256 = "{fingerprint}"
[providers.node_{name}.node.imports]
shared = "{contract}"
[providers.node_{name}.node.tls]
{tls_config(root, 'a')}
[model_builds.remote_{name}]
profile = "dummy"
model_id = "shared"
input_modalities = ["text"]
output_modalities = ["text"]
[deployments.{name}_text]
provider = "node_{name}"
build = "remote_{name}"
'''


class Harness:
    def __init__(self, root, binary):
        self.root, self.binary = root, binary
        self.processes, self.logs, self.backends = {}, {}, {}
        self.ports = {name: free_port() for name in ("a", "b", "c")}
        self.node_ports = {name: free_port() for name in ("b", "c")}
        self.token = os.urandom(32).hex()
        self.fingerprints = certificates(root)
        self.grants = {self.fingerprints['a']: {"node_id": "a", "apps": {"consumer": "consumer"}, "exports": ["shared"]}}
        self.contracts = {}
        for name in ("a", "b", "c"):
            (root / name).mkdir(mode=0o700)
            self.backends[name] = Backend(name.upper())
            config = base_config(root, name, self.ports[name], self.backends[name])
            if name != "a":
                write_private(root / (name + "-peers.json"), self.grants)
                config += node_config(root, name, self.node_ports[name])
            (root / f"{name}.toml").write_text(config)
            if name != "a":
                offers = subprocess.check_output([binary, "--config", root / f"{name}.toml", "--print-node-offers"])
                self.contracts[name] = json.loads(offers)[0]["contract_digest"]
        config = (root / "a.toml").read_text().replace('deployment_ids = ["local_text"]', 'deployment_ids = ["local_text", "b_text", "c_text"]')
        for name in ("b", "c"):
            config += import_config(root, name, self.node_ports[name], self.fingerprints[name], self.contracts[name])
        (root / "a.toml").write_text(config)

    def start(self, name):
        environment = os.environ.copy()
        environment.update(INFRA_PROTOCOL_RUNTIME_DIR=str(self.root / name / "discovery"), INFER_NODE_SMOKE_TOKEN=self.token)
        log = open(self.root / f"{name}.log", "ab")
        self.logs[name] = log
        self.processes[name] = subprocess.Popen([self.binary, "--config", self.root / f"{name}.toml"], env=environment, stdout=log, stderr=log)
        def ready():
            if self.processes[name].poll() is not None:
                raise AssertionError(f"{name} exited: {(self.root / (name + '.log')).read_text()[-4000:]}")
            try:
                return self.http(name, "/health")[0] == 200
            except OSError:
                return False
        wait_for(ready, 25)

    def stop(self, name, kill=False):
        process = self.processes.pop(name, None)
        if process is not None:
            (process.kill if kill else process.terminate)()
            process.wait(timeout=8)
        log = self.logs.pop(name, None)
        if log:
            log.close()

    def http(self, name, path, body=None):
        request = urllib.request.Request(f"http://127.0.0.1:{self.ports[name]}{path}", data=None if body is None else json.dumps(body).encode(), headers={
            "Authorization": "Bearer " + self.token, "Infer-Consumer-Contract": CORE,
            "Infer-Capability-Contract": CAPABILITY, "Content-Type": "application/json",
        })
        try:
            with urllib.request.urlopen(request, timeout=25) as response:
                return response.status, json.load(response)
        except urllib.error.HTTPError as error:
            return error.code, json.load(error)

    def infer(self, input="hello", **metadata):
        return self.http("a", "/v1/responses", {"model": "text.summarize", "input": input, "metadata": {
            "infer.placement": "private", "infer.fallback": "none", **{"infer." + key: value for key, value in metadata.items()}
        }})

    def probe(self):
        result = subprocess.run([self.binary, "--config", self.root / "a.toml", "--probe-nodes"],
                                capture_output=True, text=True, check=False)
        return result.returncode, json.loads(result.stdout)

    def rpc(self, name, command, generation=None, client="a", disconnect=False, protocol=PROTOCOL):
        context = ssl.create_default_context(cafile=str(self.root / "ca.pem"))
        context.load_cert_chain(self.root / f"{client}.pem", self.root / f"{client}.key")
        context.set_alpn_protocols([PROTOCOL])
        with socket.create_connection(("127.0.0.1", self.node_ports[name]), timeout=3) as raw:
            with context.wrap_socket(raw, server_hostname=f"{name}.test") as sock:
                payload = json.dumps({"protocol": protocol, "generation": generation, "command": command}).encode()
                sock.sendall(struct.pack("!I", len(payload)) + payload)
                if disconnect:
                    return None
                def read(size):
                    data = b""
                    while len(data) < size:
                        part = sock.recv(size - len(data))
                        if not part:
                            raise ConnectionError("node closed connection")
                        data += part
                    return data
                size = struct.unpack("!I", read(4))[0]
                assert 0 < size <= 1024 * 1024
                return json.loads(read(size))

    def close(self):
        for name in list(self.processes):
            self.stop(name)
        for backend in self.backends.values():
            backend.close()


def output(response):
    status, value = response
    assert status == 200, (status, value)
    return value["output"][0]["content"][0]["text"], value["id"]


def exercise(h):
    evidence = []
    def passed(name):
        evidence.append(name)
        print("PASS " + name, flush=True)
    for name in ("a", "b", "c"):
        h.start(name)
    assert len({p.pid for p in h.processes.values()}) == 3
    code, report = h.probe()
    assert code == 0 and report["ready"]
    assert {node["provider"] for node in report["nodes"]} == {"node_b", "node_c"}
    assert all(node["status"] == "ready" and node["available_admissions"] == 2
               and node["imports"] == [{"export": "shared", "present": True}]
               for node in report["nodes"])
    empty = subprocess.run([h.binary, "--config", h.root / "b.toml", "--probe-nodes"],
                           capture_output=True, text=True, check=False)
    assert empty.returncode != 0 and json.loads(empty.stdout) == {"ready": False, "nodes": []}
    passed("operator probe authenticates both live nodes and matches approved imports")
    assert output(h.infer())[0] == "A"
    passed("three independent runtimes; overlapping capability defaults to local A")
    text, job = output(h.infer(deployment_ids="b_text"))
    assert text == "B"
    status, snapshot = h.http("a", "/infer/v1/jobs/" + job)
    assert status == 200 and snapshot["placement"] == "trusted_node" and snapshot["deployment"] == "b_text"
    passed("forced B execution crosses real mTLS and records trusted_node provenance")
    assert output(h.infer(deployment_ids="c_text"))[0] == "C"
    # Both backends must enter execution before either is released. This proves
    # separate remote nodes can make progress on independent Jobs at once.
    for name in ("b", "c"):
        h.backends[name].started.clear()
        h.backends[name].release.clear()
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        b_job = pool.submit(h.infer, "hold", deployment_ids="b_text")
        c_job = pool.submit(h.infer, "hold", deployment_ids="c_text")
        try:
            assert h.backends["b"].started.wait(8)
            assert h.backends["c"].started.wait(8)
            for name in ("b", "c"):
                assert h.rpc(name, {"op": "catalog"})["reply"]["value"]["available_admissions"] == 1
        finally:
            h.backends["b"].release.set()
            h.backends["c"].release.set()
        assert output(b_job.result(timeout=10))[0] == "B"
        assert output(c_job.result(timeout=10))[0] == "C"
    passed("B and C execute independent Jobs simultaneously with separate admission slots")
    before = len(h.backends['b'].calls)
    assert h.infer(placement="local_only", deployment_ids="b_text")[0] != 200
    assert len(h.backends['b'].calls) == before
    passed("local_only rejects same-host B without backend execution")
    assert output(h.infer(placement="anywhere", deployment_ids="b_text"))[0] == "B"
    before = len(h.backends['b'].calls)
    assert h.infer(placement="cloud_only", deployment_ids="b_text")[0] != 200
    assert len(h.backends['b'].calls) == before
    passed("anywhere admits a paired node; cloud_only excludes it")
    oversized = "x" * (1024 * 1024)
    before = len(h.backends['b'].calls)
    status, rejected = h.infer(oversized, deployment_ids="b_text")
    assert status == 400 and rejected["error"]["code"] == "upstream_invalid_request", (status, rejected)
    assert len(h.backends['b'].calls) == before
    assert h.infer("large-result", deployment_ids="b_text")[0] != 200
    assert len(h.backends['b'].calls) == before + 1
    passed("oversized input and output are rejected at the node transport boundary")
    h.stop("b")
    code, report = h.probe()
    assert code != 0 and not report["ready"]
    assert {node["provider"]: node["status"] for node in report["nodes"]} == {
        "node_b": "unavailable", "node_c": "ready"}
    passed("operator probe reports B offline while C remains ready")
    assert output(h.infer(prefer="trusted_node"))[0] == "C"
    h.start("b")
    assert output(h.infer(deployment_ids="b_text"))[0] == "B"
    passed("B offline removes its candidates; C remains usable; B rejoin works")
    try:
        h.rpc("b", {"op": "catalog"}, client="rogue")
        raise AssertionError("unpaired client accepted")
    except (ssl.SSLError, ConnectionError, OSError):
        pass
    assert h.rpc("b", {"op": "catalog"}, protocol="wrong")["reply"] == {"kind": "error", "value": "protocol"}
    passed("unpaired certificate and incompatible protocol rejected")
    # Revocation applies without restarting either node.
    write_private(h.root / "b-peers.json", {})
    before = len(h.backends['b'].calls)
    assert h.infer(deployment_ids="b_text")[0] != 200
    assert len(h.backends['b'].calls) == before
    write_private(h.root / "b-peers.json", h.grants)
    passed("pairing revocation takes effect without daemon restart")
    original = (h.root / "a.toml").read_text()
    for label, modified in (
        ("certificate pin", original.replace(h.fingerprints['b'], "0" * 64)),
        ("contract digest", original.replace('shared = "' + h.contracts['b'] + '"', 'shared = "' + "0" * 64 + '"', 1)),
        ("server name", original.replace('server_name = "b.test"', 'server_name = "wrong.test"')),
    ):
        h.stop("a")
        (h.root / "a.toml").write_text(modified)
        h.start("a")
        before = len(h.backends['b'].calls)
        assert h.infer(deployment_ids="b_text")[0] != 200, label
        assert len(h.backends['b'].calls) == before, label
        code, report = h.probe()
        assert code != 0 and not report["ready"], label
        assert {node["provider"]: node["status"] for node in report["nodes"]} == {
            "node_b": "contract_mismatch" if label == "contract digest" else "unavailable",
            "node_c": "ready"}, label
    h.stop("a")
    legacy = original.replace('[apps.consumer.routing]\ndeployment_ids = ["local_text", "b_text", "c_text"]\n', '')
    assert legacy != original
    (h.root / "a.toml").write_text(legacy)
    h.start("a")
    assert output(h.infer(prefer="trusted_node"))[0] == "A"
    h.stop("a")
    (h.root / "a.toml").write_text(original)
    h.start("a")
    passed("certificate pin, server name, contract digest and explicit App routing are enforced")
    generation = h.rpc("b", {"op": "catalog"})["generation"]
    key = {"job_id": "lost-ack", "attempt": 1}
    reserve = {"op": "reserve", "key": key, "app_id": "consumer", "deployment": "shared", "contract_digest": h.contracts['b'], "ttl_ms": 20000}
    assert h.rpc("b", reserve, generation)["reply"]["value"]["state"] == "reserved"
    request = {"model": "text.summarize", "input": "hello"}
    before = len(h.backends['b'].calls)
    h.rpc("b", {"op": "dispatch", "key": key, "request": request}, generation, disconnect=True)
    def completed():
        reply = h.rpc("b", {"op": "status", "key": key}, generation)["reply"]
        return reply if reply.get("value", {}).get("state") == "succeeded" else None
    wait_for(completed)
    h.rpc("b", {"op": "dispatch", "key": key, "request": request}, generation)
    assert len(h.backends['b'].calls) == before + 1
    assert h.rpc("b", {"op": "dispatch", "key": key, "request": {**request, "input": "changed"}}, generation)["reply"] == {"kind": "error", "value": "protocol"}
    passed("lost dispatch acknowledgement reconciles; duplicate dispatch executes once; altered replay rejected")
    # Reserve-only leases compete across origin jobs and release on expiry.
    for i in range(2):
        assert h.rpc("b", {**reserve, "key": {"job_id": f"lease-{i}", "attempt": 1}}, generation)["reply"]["value"]["state"] == "reserved"
    code, report = h.probe()
    assert code != 0 and not report["ready"]
    assert {node["provider"]: node["status"] for node in report["nodes"]} == {
        "node_b": "busy", "node_c": "ready"}
    calls_b = len(h.backends['b'].calls)
    assert output(h.infer(prefer="trusted_node"))[0] == "C"
    assert h.infer(deployment_ids="b_text")[0] != 200
    assert len(h.backends['b'].calls) == calls_b
    passed("saturated B is excluded while available C executes the trusted-node request")
    assert h.rpc("b", {**reserve, "key": {"job_id": "overflow", "attempt": 1}}, generation)["reply"] == {"kind": "error", "value": "busy"}
    wait_for(lambda: h.rpc("b", {"op": "catalog"})["reply"]["value"]["available_admissions"] == 2, 8)
    passed("atomic admission reservations and abandoned lease reclamation")
    def compete(index):
        key = {"job_id": f"race-{index}", "attempt": 1}
        reply = h.rpc("b", {**reserve, "key": key}, generation)["reply"]
        return key, reply
    with concurrent.futures.ThreadPoolExecutor(max_workers=12) as pool:
        races = list(pool.map(compete, range(12)))
    admitted = [key for key, reply in races if reply == {"kind": "task", "value": {"state": "reserved"}}]
    rejected = [reply for _, reply in races if reply == {"kind": "error", "value": "busy"}]
    assert len(admitted) == 2 and len(rejected) == 10, races
    assert h.rpc("b", {"op": "catalog"})["reply"]["value"]["available_admissions"] == 0
    for key in admitted:
        assert h.rpc("b", {"op": "cancel", "key": key}, generation)["reply"]["value"]["state"] == "failed"
    wait_for(lambda: h.rpc("b", {"op": "catalog"})["reply"]["value"]["available_admissions"] == 2)
    passed("twelve simultaneous reservations admit only two and reclaim both slots")
    # An accepted running task loses its lease when its ingress disappears.
    h.backends['b'].started.clear()
    h.backends['b'].release.clear()
    key = {"job_id": "abandoned-running", "attempt": 1}
    assert h.rpc("b", {**reserve, "key": key}, generation)["reply"]["value"]["state"] == "reserved"
    h.rpc("b", {"op": "dispatch", "key": key, "request": {"model": "text.summarize", "input": "hold"}}, generation)
    assert h.backends['b'].started.wait(8)
    wait_for(lambda: h.rpc("b", {"op": "catalog"})["reply"]["value"]["available_admissions"] == 2, 8)
    assert h.rpc("b", {"op": "status", "key": key}, generation)["reply"]["value"] == {"state": "failed", "error": "cancelled"}
    h.backends['b'].release.set()
    passed("running task is cancelled and reclaimed after ingress lease loss")
    # Cancel via A's real Consumer API while B is running.
    with concurrent.futures.ThreadPoolExecutor() as pool:
        h.backends['b'].started.clear()
        h.backends['b'].release.clear()
        future = pool.submit(h.infer, "hold", deployment_ids="b_text")
        assert h.backends['b'].started.wait(8)
        def running():
            jobs = h.http("a", "/infer/v1/jobs?limit=20")[1]["jobs"]
            return next((j["id"] for j in jobs if j["deployment"] == "b_text" and j["state"] == "running"), None)
        job = wait_for(running)
        assert h.http("a", f"/infer/v1/jobs/{job}/cancel", {})[0] == 200
        assert future.result(timeout=10)[0] != 200
        wait_for(lambda: h.rpc("b", {"op": "catalog"})["reply"]["value"]["available_admissions"] == 2)
        h.backends['b'].release.set()
        assert h.http("a", f"/infer/v1/jobs/{job}")[1]["state"] == "cancelled"
    passed("A cancellation reaches B; reservation released; late backend output cannot succeed")
    # A may not automatically replay an accepted, ambiguous attempt on C.
    with concurrent.futures.ThreadPoolExecutor() as pool:
        h.backends['b'].started.clear()
        h.backends['b'].release.clear()
        calls_c = len(h.backends['c'].calls)
        future = pool.submit(h.infer, "hold", deployment_ids="b_text,c_text", fallback="equivalent")
        assert h.backends['b'].started.wait(8)
        h.stop("b", kill=True)
        status, response = future.result(timeout=15)
        assert status != 200 and "unknown" in response["error"]["message"]
        assert len(h.backends['c'].calls) == calls_c
        h.backends['b'].release.set()
    passed("B crash after dispatch yields unknown outcome without replaying on C")
    h.start("b")
    before = len(h.backends['c'].calls)
    assert output(h.infer("fail-on-b", deployment_ids="b_text,c_text", fallback="equivalent"))[0] == "C"
    assert len(h.backends['c'].calls) == before + 1
    passed("confirmed B execution failure allows explicitly authorized fallback to C")
    return evidence


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/debug/inferd"))
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="infer-node-smoke-") as directory:
        harness = None
        try:
            harness = Harness(Path(directory), str(args.binary.resolve()))
            evidence = exercise(harness)
            if args.report:
                args.report.write_text(json.dumps({
                    "scope": "same-host multi-process mTLS; deterministic inference only",
                    "completed_at": datetime.now(timezone.utc).isoformat(),
                    "binary_sha256": file_sha256(args.binary),
                    "harness_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                    "checks": evidence,
                }, indent=2) + "\n")
        finally:
            if harness:
                harness.close()


if __name__ == "__main__":
    main()
