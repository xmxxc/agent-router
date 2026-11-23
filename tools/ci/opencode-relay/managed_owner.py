"""Run the public OpenCode relay with current and stale Linux scope ownership."""
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path("tests").resolve()))
from opencode_relay import OpenCodeRelay
from owner import Owner


class ManagedOpenCode(OpenCodeRelay):
    dispatch = Owner.dispatch
    done = Owner.done
    error = Owner.error
    run_cli = Owner.run_cli

    def test_current_owner_delivers_and_old_owner_cannot_claim(self):
        neighbor = self.publish("neighbor")
        first = self.data("owner", "acquire", "--scope", "ours")
        previous = {"WFC_CONTROLLER_TOKEN": first["token"]}
        worker = self.dispatch("ours", "/bin/true", **previous)
        receipt = self.done(worker, **previous)
        second = self.data("takeover", "--scope", "ours")
        current = {"WFC_CONTROLLER_TOKEN": second["token"]}
        self.assertEqual(second["generation"], first["generation"] + 1)

        relay = ("relay", *self.target(), "--scope", "ours", "--for", ".7")
        for env in ({}, previous):
            self.error("stale_controller_owner", *relay, **env)
            row = self.row(receipt)
            self.assertEqual((row["messageId"], row["state"], row["statusVersion"]),
                             (receipt["messageId"], "unsent", 1))
            self.assertEqual(self.posts, [])
            self.assertEqual(self.data("status")["waiters"], [])

        self.cli(*relay, **current)
        post, = self.posts
        message, = self.batch(post)
        self.assertEqual((message["id"], message["messageId"]),
                         (receipt["message"], receipt["messageId"]))
        self.error("stale_controller_owner", "ack", receipt["messageId"], **previous)
        ack = self.data("ack", receipt["messageId"], **current)
        self.assertEqual((ack["messageId"], ack["state"], ack["statusVersion"]),
                         (receipt["messageId"], "processed", 4))
        self.cli(*relay, **current)
        self.assertEqual(len(self.posts), 1)
        self.assertEqual(self.calls(), [])
        row = self.row(neighbor)
        self.assertEqual((row["messageId"], row["state"], row["statusVersion"]),
                         (neighbor["messageId"], "unsent", 1))
        self.assertEqual(self.data("status")["waiters"], [])


if __name__ == "__main__":
    unittest.main()
