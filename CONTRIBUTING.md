# Contributing to Tranquil PDS

## When PRing

In order of importance:

- **You must run your change! Every contribution that says "here's xyz. untested." does not help the project.**
- Relevant tests to your PR must pass. The whole suite doesn't have to be proven to have run, because there are a *ton* of tests and they're quite heavy, but hopefully there are existing tests for whatever you're PRing, and if there aren't, please add those too.
- Run cargo fmt :P

> 🦪 Lewis
>
> Good CI fixes some of these. We should really get around to that.

Things that would also be nice but aren't like, a pain in our side:

- Big changes should be stacked PRs that are broken up into digestible pieces. Those stacked PRs should hopefully be able to be merged individually if necessary.

## Local Development

### Prerequisites

- [Docker](https://docs.docker.com/get-docker/) and Docker Compose
- Add `pds.test` to your hosts file (one-time setup):

  ```
  127.0.0.1 pds.test
  ```

  - **macOS / Linux:** `/etc/hosts`
  - **Windows:** `C:\Windows\System32\drivers\etc\hosts`

### Starting the dev environment

```bash
just run-dev
```

This starts the following services via `docker-compose`:

- **Traefik** — HTTPS reverse proxy at `https://pds.test`
- **Backend** — Rust server with `cargo-watch` (auto-rebuilds on file changes)
- **Frontend** — Vite dev server with hot module replacement
- **Postgres** — Database on port 5432
- **PLC Directory** — Local [did-method-plc](https://github.com/did-method-plc/did-method-plc) server for DID registration
- **Mailpit** — Local email server with web UI at [http://localhost:8025](http://localhost:8025)

Once all services are running, open **https://pds.test** in your browser.

### Trusting the self-signed certificate

Traefik generates a self-signed TLS certificate. Your browser will show a security warning on first visit. You can either click through it, or add the certificate to your system trust store for a seamless experience:

**macOS:**

```bash
# Extract the cert from traefik and add it to the system keychain
echo | openssl s_client -connect localhost:443 -servername pds.test 2>/dev/null | openssl x509 > /tmp/pds-test.pem
sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain /tmp/pds-test.pem
```

**Linux (Debian/Ubuntu):**

```bash
echo | openssl s_client -connect localhost:443 -servername pds.test 2>/dev/null | openssl x509 | sudo tee /usr/local/share/ca-certificates/pds-test.crt
sudo update-ca-certificates
```

**Linux (Fedora/RHEL):**

```bash
echo | openssl s_client -connect localhost:443 -servername pds.test 2>/dev/null | openssl x509 | sudo tee /etc/pki/ca-trust/source/anchors/pds-test.pem
sudo update-ca-trust
```

**Windows (PowerShell as Administrator):**

```powershell
$cert = New-Object System.Security.Cryptography.X509Certificates.X509Certificate2
$cert.Import([System.Text.Encoding]::UTF8.GetBytes((echo | openssl s_client -connect localhost:443 -servername pds.test 2>$null | openssl x509)))
$store = New-Object System.Security.Cryptography.X509Certificates.X509Store("Root", "LocalMachine")
$store.Open("ReadWrite")
$store.Add($cert)
$store.Close()
```

Restart your browser after adding the certificate.

### Stopping the dev environment

```bash
# Stop containers (preserves database + build cache)
docker compose --profile dev down

# Stop and wipe all data (fresh start)
docker compose --profile dev down -v
```

### Direct database access

Postgres is exposed on port 5432:

```bash
psql postgres://postgres:postgres@localhost:5432/pds
```

### How it works

- **Source code** is bind-mounted into the containers so that changes made on the host will be immediately reflected in the application
- **Backend** uses `cargo-watch` to recompile and restart when Rust files change
- **Frontend** uses Vite's HMR for instant browser updates when frontend files change
- **Build cache** (`target/` directory and cargo registry) are stored in Docker volumes, so incremental compilation persists across container restarts
- **Traefik** routes `/`, `/xrpc`, `/oauth`, `/.well-known`, `/u`, and `/health` to the backend; everything else goes to the Vite dev server
- **Mailpit** captures all outgoing email — open [http://localhost:8025](http://localhost:8025) to view verification emails during registration
- **PLC Directory** runs locally so DID registration doesn't hit the real `plc.directory`

### Running the backend natively

If you prefer running the Rust backend outside Docker (faster incremental builds on host), you need:

- Rust toolchain (see `rust-toolchain.toml`)
- `protoc` (`brew install protobuf` on macOS)
- PostgreSQL (start with `docker compose up db`)

Then run:

```bash
cargo run -p tranquil-server -- --config config.toml
```

And start the frontend separately:

```bash
cd frontend && pnpm install && pnpm dev
```
