import json
import os
from pathlib import Path
import subprocess
import unittest


FIXTURE = Path(__file__).parent / "fixtures/response-code-map.json"


class ResponseCodeMapTests(unittest.TestCase):
    def classify(self, body):
        runner = os.environ.get("CORE_RUNNER")
        if not runner:
            self.skipTest("set CORE_RUNNER to the Core alignment test executable")
        read_fd, write_fd = os.pipe()
        try:
            subprocess.run(
                [runner, "--ignored", "--exact", "plugin::execution::alignment_tests::response_code_map_probe"],
                input=json.dumps({"response_code_map": json.loads(FIXTURE.read_text()), "body": body}),
                text=True,
                capture_output=True,
                env={**os.environ, "STRAVIA_ALIGNMENT_REPORT_FD": str(write_fd)},
                pass_fds=(write_fd,),
                check=True,
            )
            os.close(write_fd)
            write_fd = -1
            with os.fdopen(read_fd) as report:
                read_fd = -1
                return json.load(report)["category"]
        finally:
            if read_fd >= 0:
                os.close(read_fd)
            if write_fd >= 0:
                os.close(write_fd)

    def test_code_6004_is_rate_limit(self):
        self.assertEqual(self.classify({"code": 6004}), "rate_limit")
        self.assertEqual(self.classify({"code": "6004"}), "rate_limit")

    def test_other_fields_and_codes_are_unknown(self):
        self.assertEqual(self.classify({"status": 6004}), "unknown")
        self.assertEqual(self.classify({"code": 6005}), "unknown")


if __name__ == "__main__":
    unittest.main()
