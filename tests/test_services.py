from __future__ import annotations

from pathlib import Path
import unittest


SERVICE_DIR = Path(__file__).resolve().parents[1] / "services"
SERVICE_FILES = (
    SERVICE_DIR / "hy-mt2-quality.service",
    SERVICE_DIR / "hy-mt2-throughput.service",
)
TAILSCALE_HOST = "100.78.159.38"


class ServiceSecurityTests(unittest.TestCase):
    def test_llama_server_binds_tailscale_interface_only(self) -> None:
        for service_path in SERVICE_FILES:
            with self.subTest(service=service_path.name):
                service_text = service_path.read_text(encoding="utf-8")
                self.assertIn(f"--host {TAILSCALE_HOST}", service_text)
                self.assertNotIn("--host 0.0.0.0", service_text)


if __name__ == "__main__":
    unittest.main()
