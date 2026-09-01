#!/usr/bin/env python3
import re
import ssl
import subprocess
from datetime import datetime, timezone
from pathlib import Path


def openssl(*arguments):
    return subprocess.run(
        ["openssl", *arguments], check=True, text=True, capture_output=True
    ).stdout


def main():
    certificate = Path("ssl/server.crt")
    key = Path("ssl/server.key")
    combined = Path("ssl/server.pem")
    if not all(path.is_file() for path in (certificate, key, combined)):
        raise SystemExit("Required certificate files are missing")

    openssl("verify", "-CAfile", str(certificate), str(certificate))

    context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    context.load_cert_chain(str(certificate), str(key))

    subject = openssl("x509", "-in", str(certificate), "-noout", "-subject")
    common_name = re.search(r"CN\s*=\s*([^,\n]+)", subject)
    if not common_name:
        raise SystemExit("Certificate has no Common Name")

    end_date = openssl("x509", "-in", str(certificate), "-noout", "-enddate")
    expiration = datetime.strptime(end_date.strip().split("=", 1)[1], "%b %d %H:%M:%S %Y %Z").replace(tzinfo=timezone.utc)

    print(common_name.group(1).strip())
    print(expiration.strftime("%Y-%m-%d"))
    print("Certificate verification successful")


if __name__ == "__main__":
    main()
