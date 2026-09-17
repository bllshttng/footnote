#!/usr/bin/env python3
"""Load driver for the graph store keeper's read path.

Copies a store (sqlite online backup of <source>/graph.db plus a copy of
<source>/graph.json, both opened read only) into --store, launches
`fno-agents-worker --store-keeper` on the copy, opens N client threads that
each send M frames of one method, samples the keeper's RSS every 5 s with
`ps`, and prints one END line.

Sandbox discipline: the source is opened read only; the keeper runs against
the store copy's own socket under the --store dir; the only process this
script ever signals is the exact pid it itself spawned. The live ~/.fno
keeper is never addressed.

Usage:
  python3 scripts/analysis/keeper-read-load.py \
      --worker crates/fno-agents/target/release/fno-agents-worker \
      --store "$(mktemp -d)" --backend sqlite --method read \
      --clients 12 --per 4
"""

import argparse
import json
import os
import shutil
import sqlite3
import socket
import struct
import subprocess
import threading
import time

METHODS = ("read", "begin", "api-rows")
BACKENDS = ("json", "sqlite")
# The AC12 pass bar: every run at least 4 reads per second, at most 500 MB
# idle RSS, 12 clients, both backends.
RPS_FLOOR = 4.0
IDLE_RSS_CEILING_MB = 500


def parse_args():
    p = argparse.ArgumentParser(description="keeper read-path load driver")
    p.add_argument("--worker", required=True, help="path to fno-agents-worker")
    p.add_argument("--source", default=os.path.expanduser("~/.fno"),
                   help="store dir to copy from (read only)")
    p.add_argument("--store", required=True,
                   help="destination dir for the copy (never under ~/.fno)")
    p.add_argument("--backend", choices=BACKENDS, default="sqlite",
                   help="backend the copy runs on")
    p.add_argument("--method", choices=METHODS, required=True)
    p.add_argument("--clients", type=int, default=12)
    p.add_argument("--per", type=int, default=4, help="requests per client")
    return p.parse_args()


def refuse_store_under_home(store):
    home = os.path.realpath(os.path.expanduser("~/.fno"))
    candidate = os.path.realpath(store)
    if candidate == home or candidate.startswith(home + os.sep):
        print("refusing --store under ~/.fno: " + store, flush=True)
        raise SystemExit(2)


def copy_store(source_dir, store_dir):
    """Copy the store read-only: sqlite online backup + graph.json copy."""
    src_db = os.path.join(source_dir, "graph.db")
    src_json = os.path.join(source_dir, "graph.json")
    if not os.path.exists(src_db):
        raise SystemExit("no graph.db in source dir: " + source_dir)
    if not os.path.exists(src_json):
        raise SystemExit("no graph.json in source dir: " + source_dir)
    src_conn = sqlite3.connect("file:" + src_db + "?mode=ro", uri=True)
    dst_conn = sqlite3.connect(os.path.join(store_dir, "graph.db"))
    src_conn.backup(dst_conn)
    dst_conn.close()
    src_conn.close()
    shutil.copyfile(src_json, os.path.join(store_dir, "graph.json"))


def stamp_backend(store_dir, backend):
    db = os.path.join(store_dir, "graph.db")
    conn = sqlite3.connect(db)
    with conn:
        conn.execute(
            "INSERT INTO graph_meta(key, value) VALUES('backend', ?1)"
            " ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            (backend,),
        )
    conn.close()


def frame(payload):
    return bytes([1]) + struct.pack("<I", len(payload)) + payload


def method_payload(method):
    if method == "read":
        return json.dumps({"id": 1, "method": "read", "params": {}}).encode()
    if method == "begin":
        return json.dumps({"id": 1, "method": "begin"}).encode()
    return json.dumps({"id": 1, "method": "api", "params": {"op": "rows"}}).encode()


def read_exact(client, n):
    buf = bytearray(n)
    view = memoryview(buf)
    got = 0
    while got < n:
        chunk = client.recv_into(view[got:], min(262144, n - got))
        if not chunk:
            raise ConnectionError("keeper hung up mid-frame")
        got += chunk
    return bytes(buf)


def one_client(sock_path, payload, count):
    """One connection, `count` sequential frames. Returns ok replies."""
    ok = 0
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    client.connect(sock_path)
    try:
        for _ in range(count):
            client.sendall(frame(payload))
            reply = read_exact(client, 5)
            length = struct.unpack("<I", reply[1:5])[0]
            body = read_exact(client, length)
            if reply[0] == 4 and b'"ok":true' in body:
                ok += 1
    finally:
        client.close()
    return ok


def sample_rss_mb(pid):
    out = subprocess.run(
        ["ps", "-o", "rss=", "-p", str(pid)],
        capture_output=True, text=True,
    ).stdout.strip()
    if not out:
        return None
    return int(out) / 1024.0


def wait_for_socket(sock_path, deadline_s=20.0):
    end = time.time() + deadline_s
    while time.time() < end:
        probe = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            probe.connect(sock_path)
            return True
        except OSError:
            time.sleep(0.05)
        finally:
            probe.close()
    return False


def main():
    args = parse_args()
    refuse_store_under_home(args.store)
    store_dir = os.path.abspath(args.store)
    os.makedirs(store_dir, exist_ok=True)
    copy_store(os.path.abspath(os.path.expanduser(args.source)), store_dir)
    stamp_backend(store_dir, args.backend)

    sock_path = os.path.join(store_dir, "load.store.sock")
    env = dict(os.environ)
    env["FNO_STORE_KEEPER_IDLE_SECS"] = "0"
    worker = subprocess.Popen(
        [args.worker, "--store-keeper", "--sock", sock_path,
         "--graph", os.path.join(store_dir, "graph.json")],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        if not wait_for_socket(sock_path):
            raise SystemExit("keeper never bound " + sock_path)
        payload = method_payload(args.method)
        samples = []

        def sampler():
            while not stop.is_set():
                rss = sample_rss_mb(worker.pid)
                if rss is not None:
                    samples.append(rss)
                stop.wait(5.0)

        stop = threading.Event()
        sampler_thread = threading.Thread(target=sampler, daemon=True)
        sampler_thread.start()

        started = time.time()
        ok_counts = [0] * args.clients
        threads = []
        for i in range(args.clients):
            def run(i=i):
                ok_counts[i] = one_client(sock_path, payload, args.per)
            t = threading.Thread(target=run)
            t.start()
            threads.append(t)
        for t in threads:
            t.join()
        elapsed = time.time() - started

        time.sleep(6.0)  # let the idle RSS settle before the last samples
        stop.set()
        sampler_thread.join(timeout=10.0)

        reqs = args.clients * args.per
        rps = reqs / elapsed if elapsed > 0 else 0.0
        peak = max(samples) if samples else 0.0
        idle = samples[-1] if samples else 0.0
        verdict = "pass" if (rps >= RPS_FLOOR and idle <= IDLE_RSS_CEILING_MB) else "SLOW_OR_FAT"
        print(
            "END method=%s backend=%s clients=%d reqs=%d secs=%.2f rps=%.2f"
            " peak_rss_mb=%.0f idle_rss_mb=%.0f %s"
            % (args.method, args.backend, args.clients, reqs, elapsed, rps,
               peak, idle, verdict),
            flush=True,
        )
    finally:
        worker.terminate()
        try:
            worker.wait(timeout=10)
        except subprocess.TimeoutExpired:
            worker.kill()
        try:
            worker.wait(timeout=5)
        except subprocess.TimeoutExpired:
            pass


if __name__ == "__main__":
    main()
