#!/usr/bin/env python3
"""Load and verify the self-signed TLS certificate in ssl/.

Uses only the Python standard library plus OpenSSL command-line subprocesses.
Prints the certificate Common Name and expiration date (YYYY-MM-DD), and
prints "Certificate verification successful" when every check passes.
"""

import datetime
import re
import subprocess
import sys
from pathlib import Path

BASE_DIR = Path(__file__).resolve().parent
CERT_FILE = BASE_DIR / "ssl" / "server.crt"
KEY_FILE = BASE_DIR / "ssl" / "server.key"
EXPECTED_CN = "dev-internal.company.local"
VALIDITY_DAYS = 365
DATE_FMT = "%b %d %H:%M:%S %Y %Z"


class VerificationError(Exception):
    """Raised when any certificate check fails."""


def openssl(*args):
    """Run an OpenSSL command and return its stdout, raising on failure."""
    try:
        proc = subprocess.run(
            ["openssl", *args], capture_output=True, text=True, check=True
        )
    except FileNotFoundError as exc:
        raise VerificationError("openssl command not found") from exc
    except subprocess.CalledProcessError as exc:
        detail = (exc.stderr or "").strip() or f"exit status {exc.returncode}"
        raise VerificationError(f"openssl {' '.join(args)} failed: {detail}") from exc
    return proc.stdout


def field(output, name):
    """Extract the value of a 'name=value' line from OpenSSL output."""
    match = re.search(rf"^{re.escape(name)}=(.*)$", output, re.MULTILINE)
    if not match:
        raise VerificationError(f"could not find field {name!r} in OpenSSL output")
    return match.group(1).strip()


def parse_date(value):
    try:
        return datetime.datetime.strptime(value, DATE_FMT)
    except ValueError as exc:
        raise VerificationError(f"could not parse date {value!r}") from exc


def verify():
    checks = []

    # Required files must exist.
    for path in (CERT_FILE, KEY_FILE):
        if not path.is_file():
            raise VerificationError(f"missing required file: {path}")
    checks.append("certificate and private key files exist")

    # 1. Load (parse) the certificate with OpenSSL.
    openssl("x509", "-in", str(CERT_FILE), "-noout", "-text")
    checks.append("certificate loads and parses correctly")

    # 2. Verify the certificate (self-signed: it is its own CA).
    verify_out = openssl("verify", "-CAfile", str(CERT_FILE), str(CERT_FILE))
    if "OK" not in verify_out:
        raise VerificationError(f"openssl verify failed: {verify_out.strip()}")
    checks.append("signature/chain verification OK")

    # 3. The private key must match the certificate.
    cert_modulus = field(
        openssl("x509", "-in", str(CERT_FILE), "-noout", "-modulus"), "Modulus"
    )
    key_modulus = field(
        openssl("rsa", "-in", str(KEY_FILE), "-noout", "-modulus"), "Modulus"
    )
    if cert_modulus != key_modulus:
        raise VerificationError("private key does not match the certificate")
    checks.append("private key matches certificate")

    # 4. Subject / Common Name.
    subject_line = field(
        openssl(
            "x509", "-in", str(CERT_FILE), "-noout", "-subject", "-nameopt", "RFC2253"
        ),
        "subject",
    )
    cn_match = re.search(r"(?:^|,)CN=([^,]+)", subject_line)
    if not cn_match:
        raise VerificationError(f"no Common Name in subject: {subject_line}")
    common_name = cn_match.group(1).strip()
    if common_name != EXPECTED_CN:
        raise VerificationError(
            f"unexpected Common Name {common_name!r} (expected {EXPECTED_CN!r})"
        )
    checks.append(f"Common Name is {common_name}")

    # 5. Validity: exactly 365 days and currently within the window.
    not_before = parse_date(
        field(openssl("x509", "-in", str(CERT_FILE), "-noout", "-startdate"), "notBefore")
    )
    not_after = parse_date(
        field(openssl("x509", "-in", str(CERT_FILE), "-noout", "-enddate"), "notAfter")
    )
    span = (not_after - not_before).days
    if span != VALIDITY_DAYS:
        raise VerificationError(f"validity is {span} days, expected {VALIDITY_DAYS}")
    now = datetime.datetime.now(datetime.timezone.utc).replace(tzinfo=None)
    if not (not_before <= now <= not_after):
        raise VerificationError("certificate is not currently valid")
    checks.append(f"valid for exactly {VALIDITY_DAYS} days and currently valid")

    return common_name, not_after, checks


def main():
    try:
        common_name, not_after, checks = verify()
    except VerificationError as exc:
        print(f"Certificate verification failed: {exc}", file=sys.stderr)
        return 1
    except Exception as exc:  # unexpected errors must not print a success line
        print(f"Certificate verification failed: {exc}", file=sys.stderr)
        return 1

    print(f"Common Name: {common_name}")
    print(f"Expiration Date: {not_after.strftime('%Y-%m-%d')}")
    for check in checks:
        print(f"OK: {check}")
    print("Certificate verification successful")
    return 0


if __name__ == "__main__":
    sys.exit(main())
