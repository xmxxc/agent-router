"""Public Linux relay contracts against a loopback-only OpenCode fixture."""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import hashlib
import json
from pathlib import Path
import socket
import threading
import unittest

import relay


class OpenCodeRelay(unittest.TestCase):
    meta = relay.Relay.meta
    cli = relay.Relay.cli
    data = relay.Relay.data
    publish = relay.Relay.publish
    calls = relay.Relay.calls
    until = relay.Relay.until

    session = "ses_controller"

    def setUp(self):
        relay.Relay.setUp(self)
        self.posts = []
        self.mode = 204
        self.finish = threading.Event()
        fixture = self

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                body = self.rfile.read(int(self.headers["Content-Length"]))
                fixture.posts.append((self.command, self.path, self.headers.get("Content-Type"),
                                      self.headers.get("Authorization"), json.loads(body)))
                if fixture.mode == "disconnect":
                    # The entire request reached the server before EOF.
                    return
                if fixture.mode == "incomplete_headers":
                    self.wfile.write(b"HTTP/1.1 204 No Content\r\n")
                    return
                if fixture.mode == "timeout":
                    fixture.finish.wait(5)
                    return
                if fixture.mode == "accepted_headers":
                    self.send_response(202)
                    self.send_header("Content-Length", "10")
                    self.end_headers()
                    fixture.finish.wait(5)
                    return
                self.respond()

            def do_GET(self):
                # One redirect can change POST to GET. Record either form.
                fixture.posts.append((self.command, self.path, None, None, None))
                self.respond()

            def respond(self):
                status = 204 if self.path == "/redirected" else fixture.mode
                self.send_response(status)
                if status in (302, 307):
                    self.send_header("Location", "/redirected")
                self.send_header("Content-Length", "0")
                self.end_headers()

            def log_message(self, *args):
                pass

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        server.daemon_threads = False
        self.url = f"http://127.0.0.1:{server.server_port}/api/"
        thread = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": .05})

        def cleanup():
            self.finish.set()
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)
            self.assertFalse(thread.is_alive())

        thread.start()
        self.addCleanup(cleanup)

    def target(self, url=None, session=None):
        return ("--opencode-url", url or self.url, "--opencode-session", session or self.session)

    def send(self, duration=".7", *, url=None, **env):
        return self.cli("relay", *self.target(url), "--scope", "ours", "--for", duration, **env)

    def batch(self, post, session=None):
        method, path, content_type, auth, body = post
        self.assertEqual((method, path, content_type, auth),
                         ("POST", f"/api/session/{session or self.session}/prompt_async",
                          "application/json", None))
        self.assertEqual(set(body), {"parts"})
        part, = body["parts"]
        self.assertEqual(set(part), {"type", "text"})
        self.assertEqual(part["type"], "text")
        self.assertTrue(part["text"].startswith(relay.PREFIX))
        return json.loads(part["text"][len(relay.PREFIX):])["messages"]

    def row(self, receipt):
        return next(row for row in self.data("status")["undelivered"]
                    if row["messageId"] == receipt["messageId"])

    def test_204_detached_preserves_batch_order_scope_and_session(self):
        # Keep CODEX_THREAD_ID set and make its rollout unusable. The explicit
        # HTTP target is sufficient and does not consult the Codex session.
        self.rollout.unlink()
        original = [self.publish("ours"), self.publish("ours")]
        neighbor = self.publish("other")
        child = self.data("relay", *self.target(), "--scope", "ours", "--for", "2", "--detach")

        def registered():
            return next((w for w in self.data("status")["waiters"]
                         if w["pid"] == child["pid"]), None)

        entry = self.until(registered)
        self.assertEqual((entry["scope"], entry["kind"], entry["session"]),
                         ("ours", "relay", self.session))
        self.until(self.posts.__len__)
        self.until(lambda: not self.data("status")["waiters"])
        post, = self.posts
        batch = self.batch(post)
        self.assertEqual([m["id"] for m in batch], [r["message"] for r in original])
        self.assertEqual([m["messageId"] for m in batch], [r["messageId"] for r in original])
        for m in batch:
            self.assertEqual((m["scope"], m["state"], m["statusVersion"], m["delivery"]),
                             ("ours", "unknown", 2, "pending"))
            self.assertEqual(m["deliveryAttempt"], "relay:" + m["messageId"] + ":2")
            dispatch, = self.data("status", "--dispatch", m["dispatch"])["dispatches"]
            history, = dispatch["messages"]
            self.assertEqual((history["messageId"], history["state"], history["statusVersion"]),
                             (m["messageId"], "received", 3))
            ack = self.data("ack", m["messageId"])
            self.assertEqual((ack["message"], ack["messageId"], ack["state"], ack["statusVersion"]),
                             (m["id"], m["messageId"], "processed", 4))
            self.assertTrue(self.data("ack", m["messageId"])["duplicate"])
        self.send(".3")
        self.assertEqual(len(self.posts), 1)
        self.assertEqual(self.calls(), [])
        self.assertEqual(self.row(neighbor)["state"], "unsent")
        self.cli("relay", *self.target(session="ses_neighbor"), "--scope", "other", "--for", ".7")
        second, = self.batch(self.posts[1], "ses_neighbor")
        self.assertEqual(second["messageId"], neighbor["messageId"])
        self.assertEqual(len(self.posts), 2)

    def test_connection_refused_is_unsent_and_can_retry_original_id(self):
        receipt = self.publish("ours")
        with socket.socket() as closed:
            # Hold the port without listening so a different test cannot claim it.
            closed.bind(("127.0.0.1", 0))
            refused_url = f"http://127.0.0.1:{closed.getsockname()[1]}"
            log = self.send(".4", url=refused_url)
        self.assertIn("connection refused", log)
        row = self.row(receipt)
        self.assertEqual((row["state"], row["statusVersion"]), ("unsent", 3))
        self.assertEqual(self.posts, [])
        self.cli("ack", receipt["messageId"], code=1)
        self.send()
        post, = self.posts
        message, = self.batch(post)
        self.assertEqual((message["id"], message["messageId"], message["statusVersion"]),
                         (receipt["message"], receipt["messageId"], 4))
        ack = self.data("ack", receipt["messageId"])
        self.assertEqual((ack["state"], ack["statusVersion"]), ("processed", 6))

    def test_errors_redirects_timeouts_and_eof_never_replay(self):
        for count, mode in enumerate((302, 307, 401, 500, "timeout", "incomplete_headers", "disconnect"), start=1):
            with self.subTest(mode=mode):
                self.mode = mode
                receipt = self.publish("ours")
                log = self.send(WFC_QUEUE_TIMEOUT=".25")
                self.assertIn("uncertain", log)
                if mode == "timeout":
                    self.assertIn("curl code 28", log)
                elif isinstance(mode, int):
                    self.assertIn(f"HTTP {mode}", log)
                self.assertEqual(len(self.posts), count)
                message, = self.batch(self.posts[-1])
                self.assertEqual(message["messageId"], receipt["messageId"])
                row = self.row(receipt)
                self.assertEqual((row["state"], row["statusVersion"]), ("unknown", 2))
                self.mode = 204
                self.send(".3")
                self.assertEqual(len(self.posts), count)
                self.cli("wait", "--scope", "ours", "--timeout", "0", code=3)
                self.cli("ack", receipt["messageId"], code=1)
                self.assertEqual(self.data("status")["waiters"], [])
                if mode == "disconnect":
                    body = {k: message[k] for k in
                            ("attempt", "head", "dirty", "report", "role", "round", "scope")}
                    digest = hashlib.sha256(json.dumps(body, ensure_ascii=False, sort_keys=True).encode()).hexdigest()
                    mid = receipt["messageId"]
                    resolved = self.data("resolve", mid, "--actor", "fixture", "--reason", "observed request",
                                         "--evidence", "loopback request record", "--body-hash", digest)
                    self.assertEqual((resolved["messageId"], resolved["state"], resolved["statusVersion"]),
                                     (mid, "received", 3))
                    ack = self.data("ack", mid)
                    self.assertEqual((ack["messageId"], ack["state"], ack["statusVersion"]),
                                     (mid, "processed", 4))
                if mode == "timeout":
                    self.finish.set()

    def test_success_headers_confirm_without_waiting_for_body(self):
        self.mode = "accepted_headers"
        receipt = self.publish("ours")
        # An inherited proxy must neither see nor rewrite this explicit request.
        self.send(WFC_QUEUE_TIMEOUT=".3", http_proxy=self.url.removesuffix("/api/"))
        post, = self.posts
        m, = self.batch(post)
        self.assertEqual(m["messageId"], receipt["messageId"])
        ack = self.data("ack", receipt["messageId"])
        self.assertEqual((ack["state"], ack["statusVersion"]), ("processed", 4))

    def test_invalid_ports_are_rejected_before_claiming(self):
        receipt = self.publish("ours")
        for authority in ("127.0.0.1:65536", "127.0.0.1:xyz", "127.0.0.1:+80",
                          "127.0.0.1:-1", "127.0.0.1:", "127.0.0.1:0", "[::1]:65536"):
            with self.subTest(authority=authority):
                self.cli("relay", *self.target(f"http://{authority}/api"), "--scope", "ours",
                         "--for", ".2", code=2)
                row = self.row(receipt)
                self.assertEqual((row["messageId"], row["state"], row["statusVersion"]),
                                 (receipt["messageId"], "unsent", 1))
                self.assertEqual(self.posts, [])
                self.assertEqual(self.data("status")["waiters"], [])
        for url in ("http://127.0.0.1", "http://[::1]", "https://[::1]:65535"):
            self.cli("relay", *self.target(url), "--scope", "empty", "--for", ".05")
        self.assertEqual(self.posts, [])
        self.send()
        post, = self.posts
        message, = self.batch(post)
        self.assertEqual((message["id"], message["messageId"], message["statusVersion"]),
                         (receipt["message"], receipt["messageId"], 2))
        ack = self.data("ack", receipt["messageId"])
        self.assertEqual((ack["state"], ack["statusVersion"]), ("processed", 4))

    def test_invalid_target_and_worker_callback_do_not_claim(self):
        receipt = self.publish("ours")
        for args in ((), ("--opencode-url", self.url), ("--opencode-session", self.session),
                     ("--codex", *self.target()), ("--codex-thread", self.thread, *self.target()),
                     (*self.target(), "--opencode-session", "other"),
                     self.target("file:///tmp/opencode"), self.target(self.url + "?query"),
                     self.target(self.url + "#frag"), self.target("http://user@127.0.0.1"),
                     self.target(session="../other"), self.target(session="%2Fother")):
            with self.subTest(args=args):
                self.cli("relay", *args, "--scope", "ours", "--for", ".05", code=2)
        self.cli("relay", *self.target(), "--scope", "ours", "--for", ".05",
                 code=1, WFC_WORKER_CALLBACK="fixture")
        self.assertEqual(self.posts, [])
        self.assertEqual(self.calls(), [])
        row = self.row(receipt)
        self.assertEqual((row["state"], row["statusVersion"]), ("unsent", 1))
        path = Path(self.env["WFC_STATE"], "dispatches", row["dispatch"] + ".json")
        assignment = json.loads(path.read_bytes())
        assignment["controllerOwner"] = {"owner": "fixture"}
        path.write_text(json.dumps(assignment))
        self.cli("relay", *self.target(), "--scope", "ours", "--for", ".05", code=1)
        self.assertEqual(self.row(receipt)["statusVersion"], 1)
        self.assertEqual(self.posts, [])
        self.assertEqual(self.data("status")["waiters"], [])
        self.assertFalse(Path(self.env["WFC_STATE"], "relay.log").exists())


if __name__ == "__main__":
    unittest.main()
