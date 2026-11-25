#!/usr/bin/env python3
"""Process checks for offline model setup. Uses temporary HOME and no real models."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile


def main():
    backend = Path(sys.argv[1]).resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="kaic-setup-") as temporary:
        root = Path(temporary)
        home = root / "home"
        env = {key: value for key, value in os.environ.items() if not key.startswith("KAIC_")}
        env["HOME"] = str(home)

        def setup(success=True):
            result = subprocess.run([str(backend), "--setup-models"], env=env, cwd=root,
                                    text=True, capture_output=True, timeout=12)
            assert (result.returncode == 0) == success, result.stderr
            return json.loads(result.stdout) if success else result.stderr

        catalog = setup()
        assert Path(catalog["root"]) == home / "models"
        assert not (home / "Models").exists()
        keep = home / "models" / "preserve.gguf"
        keep.write_bytes(b"not a valid model, but never delete it")
        before = keep.read_bytes()
        assert len(setup()["rejected"]) == 1 and keep.read_bytes() == before
        override = root / "explicit" / "weights"
        env["KAIC_MODELS_DIR"] = str(override)
        assert Path(setup()["root"]) == override
        del env["HOME"]
        assert Path(setup()["root"]) == override
        assert keep.read_bytes() == before
        env["KAIC_MODELS_DIR"] = str(keep)
        assert "directory" in setup(success=False).lower()
        del env["KAIC_MODELS_DIR"]
        assert "HOME" in setup(success=False)
        assert not (root / "kaic.db").exists()
    print("offline setup: PASS (lowercase, create, preserve, override, missing HOME, invalid path)")


if __name__ == "__main__":
    main()
