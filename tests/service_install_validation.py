#!/usr/bin/env python3
"""Exercise service installation without contacting systemd or any provider."""

import argparse
import os
from pathlib import Path
import pwd
import subprocess
import tempfile


def invoke(binary: Path, env: dict[str, str], *args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [str(binary), "service", "install", *args],
        env=env,
        text=True,
        capture_output=True,
        timeout=15,
        check=False,
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve()
    if not binary.is_file():
        raise SystemExit(f"binary is not a regular file: {binary}")
    account = pwd.getpwuid(os.getuid()).pw_name
    if os.getuid() == 0:
        raise SystemExit("fixture requires a non-root invoking account")

    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        xdg = root / "xdg config $percent%"
        env = dict(os.environ, HOME=str(root / "home"), XDG_CONFIG_HOME=str(xdg))
        expected_config = xdg / "slotline" / "config.yaml"

        user_preview = invoke(binary, env, "--user", "--dry-run")
        if user_preview.returncode:
            raise SystemExit(f"user preview failed: {user_preview.stderr}")
        serialized_config = str(expected_config).replace("$", "$$").replace("%", "%%")
        if serialized_config not in user_preview.stdout:
            raise SystemExit("user preview did not use the absolute XDG policy path")
        if "Environment=\"XDG_CONFIG_HOME=" not in user_preview.stdout:
            raise SystemExit("user preview did not quote the environment assignment")
        if "$$percent%%" not in user_preview.stdout:
            raise SystemExit("user preview did not safely serialize literal ExecStart arguments")
        for directive in ("ProtectSystem=", "ProtectHome=", "ReadWritePaths=", "PrivateTmp="):
            if directive in user_preview.stdout:
                raise SystemExit(f"user preview contains namespace-dependent sandboxing: {directive}")
        if "RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX" not in user_preview.stdout:
            raise SystemExit("user preview did not retain supported IPv4, IPv6, and Unix sockets")
        if "WantedBy=default.target" not in user_preview.stdout:
            raise SystemExit("user preview did not target the user default target")
        if (xdg / "systemd").exists():
            raise SystemExit("user --dry-run unexpectedly wrote a unit")

        system_preview = invoke(binary, env, "--system", "--run-as", account, "--dry-run")
        if system_preview.returncode:
            raise SystemExit(f"system preview failed: {system_preview.stderr}")
        if "/etc/systemd/system/soulsteads-router.service" not in system_preview.stdout:
            raise SystemExit("system preview did not use the fixed unit directory")
        if f"User={account}" not in system_preview.stdout:
            raise SystemExit("system preview did not set the requested non-root account")
        for directive in ("ProtectSystem=strict", "ProtectHome=read-only", "ReadWritePaths=", "PrivateTmp=yes"):
            if directive not in system_preview.stdout:
                raise SystemExit(f"system preview omitted system sandboxing: {directive}")
        if "WantedBy=multi-user.target" not in system_preview.stdout:
            raise SystemExit("system preview did not target multi-user.target")

        first = invoke(binary, env, "--user")
        if first.returncode:
            raise SystemExit(f"user install failed: {first.stderr}")
        unit = xdg / "systemd" / "user" / "soulsteads-router.service"
        initial = unit.read_bytes()
        second = invoke(binary, env, "--user")
        if second.returncode == 0:
            raise SystemExit("second user install overwrote an existing unit")
        if unit.read_bytes() != initial:
            raise SystemExit("failed user install changed the existing unit")

        symlink_root = root / "symlink-xdg"
        symlink_unit_dir = symlink_root / "systemd" / "user"
        symlink_unit_dir.mkdir(parents=True)
        victim = root / "victim"
        victim.write_text("unchanged", encoding="utf-8")
        (symlink_unit_dir / "soulsteads-router.service").symlink_to(victim)
        symlink_env = dict(env, XDG_CONFIG_HOME=str(symlink_root))
        symlinked = invoke(binary, symlink_env, "--user")
        if symlinked.returncode == 0 or victim.read_text(encoding="utf-8") != "unchanged":
            raise SystemExit("user install followed or replaced a symlinked unit")


if __name__ == "__main__":
    main()
