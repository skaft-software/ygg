# Terminal-Bench 2.1 openssl-selfsigned-cert task

Your company needs a self-signed TLS certificate for an internal development server. Complete this Terminal-Bench 2.1 task in the current workspace, which is mounted as `/app` for verification.

1. Create `ssl/` to store all certificate files.
2. Generate a 2048-bit RSA private key at `ssl/server.key` with permissions 600.
3. Create a self-signed certificate valid for exactly 365 days with Organization Name `DevOps Team` and Common Name `dev-internal.company.local`; save it as `ssl/server.crt`.
4. Create `ssl/server.pem` containing both the private key and certificate.
5. Create `ssl/verification.txt` containing the certificate subject, validity dates, and SHA-256 fingerprint.
6. Create `check_cert.py` using only the Python standard library and OpenSSL subprocesses. It must load and verify the certificate, print its Common Name and expiration date in YYYY-MM-DD format, and print `Certificate verification successful` when all checks pass. The verifier environment does not install third-party Python packages.

Use OpenSSL commands, ensure all files have the correct formats and permissions, and do not merely explain the solution. This is a speed-focused run: do not inspect the empty workspace or plan at length. Immediately create every required artifact, preferably in one shell invocation, run a concise verification, and finish.
