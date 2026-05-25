# Tranquil PDS containerized production deployment

This guide covers deploying Tranquil PDS using containers with podman.

- **Debian 13+**: Uses systemd quadlets
- **Alpine 3.23+**: Uses per-container OpenRC init scripts

## Prerequisites

- A server :p
- Disk space for blobs, around* 1GB per active user as a baseline
- A domain name pointing to your server's IP
- A **wildcard TLS certificate** for `*.pds.example.com`, since user handles are served as subdomains
- Root/sudo/doas access

> 🦪 Lewis
>
> * "around" here meaning "at absolute least!"

## Reverse proxy

The bundled pod ships nginx as its reverse proxy, and this guide uses it throughout. nginx is just the default. Swap in whatever you prefer. Tranquil serves its API and web UI on a single port, so any reverse proxy works once it forwards to the app on `[::1]:3000`.

Caddy is one good option. It grabs & renews TLS certificates automatically, including the wildcard this setup needs (if you're lucky, which Lewis is not), so the manual certbot steps later become unnecessary. We plan to soon also have the ability to terminate TLS directly in-Tranquil so that there's no reverse proxy needed.

## Quickstart (docker/podman compose)

If you just want to get running quickly:

```sh
cp example.toml config.toml
```

Edit `config.toml` with your values. Generate secrets with `openssl rand -base64 48`.

`docker-compose.prod.yaml` pulls the prebuilt image from atcr.io. Sign in to the registry first with `podman login atcr.io`.

nginx will not start without a certificate, so create a temporary self-signed one, bring the stack up, then swap in a real wildcard cert:
```sh
mkdir -p certs
openssl req -x509 -nodes -days 1 -newkey rsa:2048 \
  -keyout certs/privkey.pem -out certs/fullchain.pem \
  -subj "/CN=pds.example.com"
podman-compose -f docker-compose.prod.yaml up -d
```

To build the image from source instead, run `podman build -t atcr.io/tranquil.farm/tranquil-pds:latest .` before bringing the stack up.

User handles are subdomains, so the real certificate must be a wildcard for `*.pds.example.com`, which requires DNS-01 validation. Follow the DNS cert steps in the Wildcard TLS certificate section below, then:
```sh
podman-compose -f docker-compose.prod.yaml restart nginx
```

## Standalone container without compose

If you already have postgres running on the host, you can run just the app container.

Pull the image. atcr.io requires authentication, so sign in first:
```sh
podman login atcr.io
podman pull atcr.io/tranquil.farm/tranquil-pds:latest
```

Run with host networking so it can reach postgres on localhost, and mount config + storage:
```sh
podman run -d --name tranquil-pds \
  --network=host \
  -v /etc/tranquil-pds/config.toml:/etc/tranquil-pds/config.toml:ro,Z \
  -v /var/lib/tranquil-pds:/var/lib/tranquil-pds:Z \
  atcr.io/tranquil.farm/tranquil-pds:latest
```

To build from source instead, run `podman build -t atcr.io/tranquil.farm/tranquil-pds:latest .` and use that tag.

Then point your reverse proxy at the app on port 3000 for every route. With nginx that looks like:

```nginx
location /xrpc/ {
    proxy_pass http://[::1]:3000;
    # full proxy headers are in deploy/nginx/nginx-pod.conf
}

location / {
    proxy_pass http://[::1]:3000;
    proxy_http_version 1.1;
    proxy_set_header Host $host;
    proxy_set_header X-Real-IP $remote_addr;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    proxy_set_header X-Forwarded-Proto $scheme;
}
```

With Caddy the equivalent is a one-line `reverse_proxy [::1]:3000`, and it handles TLS for you. See `deploy/nginx/nginx-pod.conf` in the repo for the full nginx config with all routes.

The end!!!

Or wait, you want more? Perhaps a deployment that comes back on server restart?

---

# Common setup

Both service-managed deployments share the steps below. Do these first, then jump to the Debian or Alpine section for the init-specific stuff, and finish in the Wildcard TLS certificate section.

## Install podman

**Debian:**
```bash
apt update
apt install -y podman
```

**Alpine:**
```sh
apk update
apk add podman fuse-overlayfs
rc-update add cgroups
rc-service cgroups start
```

## Create the directory structure

```sh
mkdir -p /srv/tranquil-pds/{postgres,blobs,store,certs,acme,config}
```

## Clone the repo and pull the image

The repo provides the quadlet, OpenRC, and nginx files. The image comes prebuilt from atcr.io. Sign in before pulling:
```sh
cd /opt
git clone https://tangled.org/tranquil.farm/tranquil-pds tranquil-pds
podman login atcr.io
podman pull atcr.io/tranquil.farm/tranquil-pds:latest
```

To build from source instead, tag it with the same name so the service uses it:
```sh
cd tranquil-pds && podman build -t atcr.io/tranquil.farm/tranquil-pds:latest .
```

## Create a configuration file

```sh
cp /opt/tranquil-pds/example.toml /srv/tranquil-pds/config/config.toml
chmod 600 /srv/tranquil-pds/config/config.toml
```

Edit `/srv/tranquil-pds/config/config.toml` and fill in your values. Generate secrets with:
```sh
openssl rand -base64 48
```

> 🦪 Lewis
>
> Every config option can also be set via an environment variable
> named in the comments in `example.toml`. Environment variables always take
> precedence over the config file.

## Create the nginx config and database secret

The bundled pod runs nginx as its proxy. The app and nginx share the pod's network namespace, so the proxy reaches the app on `[::1]:3000`:
```sh
cp /opt/tranquil-pds/deploy/nginx/nginx-pod.conf /srv/tranquil-pds/config/nginx.conf
```

Create the podman secret for the database password. Use the same password in the `database.url` of your `config.toml`:
```sh
echo "$DB_PASSWORD" | podman secret create tranquil-pds-db-password -
```

## Create a temporary TLS certificate

The nginx that comes out of the box will not start without a certificate. The real wildcard cert needs DNS-01 validation and is set up once the stack is running, so for now let's add a self-signed placeholder so the services can start:
```sh
openssl req -x509 -nodes -days 1 -newkey rsa:2048 \
  -keyout /srv/tranquil-pds/certs/privkey.pem \
  -out /srv/tranquil-pds/certs/fullchain.pem \
  -subj "/CN=pds.example.com"
```

If your proxy obtains its own certificates, like Caddy does, you can skip this step!

The app runs migrations itself on first boot btw, so there is no separate migration step if you're looking for one.

---

# Debian with systemd quadlets

Quadlets are a nice way to run podman containers under systemd.

## Install quadlet definitions

Copy the quadlet files from the repository:
```bash
mkdir -p /etc/containers/systemd
cp /opt/tranquil-pds/deploy/quadlets/tranquil-pds.pod /etc/containers/systemd/
cp /opt/tranquil-pds/deploy/quadlets/tranquil-pds-db.container /etc/containers/systemd/
cp /opt/tranquil-pds/deploy/quadlets/tranquil-pds-app.container /etc/containers/systemd/
cp /opt/tranquil-pds/deploy/quadlets/tranquil-pds-nginx.container /etc/containers/systemd/
```

Optional quadlets for valkey and minio are also available in `deploy/quadlets/` if you need them.

## Start services

```bash
systemctl daemon-reload
systemctl start tranquil-pds-db
sleep 10
systemctl start tranquil-pds-app tranquil-pds-nginx
```

## Enable all services

```bash
systemctl enable tranquil-pds-db tranquil-pds-app tranquil-pds-nginx
```

## Configure firewall if you're into that sort of thing

```bash
apt install -y ufw
ufw allow ssh
ufw allow 80/tcp
ufw allow 443/tcp
ufw enable
```

Now finish in the Wildcard TLS certificate section below.

---

# Alpine with OpenRC

Alpine uses OpenRC, not systemd, bless its soul. So instead of quadlets we use a set of OpenRC init scripts, one per container.

## Install the OpenRC services

Copy the init scripts. They run the pod, postgres, the app, and nginx as separate services ordered with `depend()`:
```sh
cp /opt/tranquil-pds/deploy/openrc/tranquil-pds-pod /etc/init.d/
cp /opt/tranquil-pds/deploy/openrc/tranquil-pds-db /etc/init.d/
cp /opt/tranquil-pds/deploy/openrc/tranquil-pds-app /etc/init.d/
cp /opt/tranquil-pds/deploy/openrc/tranquil-pds-nginx /etc/init.d/
chmod +x /etc/init.d/tranquil-pds-pod /etc/init.d/tranquil-pds-db /etc/init.d/tranquil-pds-app /etc/init.d/tranquil-pds-nginx
```

The scripts default to `/srv/tranquil-pds` for data and `/srv/tranquil-pds/config/config.toml` for config. Override via `/etc/conf.d/tranquil-pds-app` and friends if your paths differ.

## Start services

Starting the nginx service pulls in the pod, postgres, and app through its dependencies:
```sh
rc-service tranquil-pds-nginx start
```

## Enable services at boot time

```sh
rc-update add tranquil-pds-pod tranquil-pds-db tranquil-pds-app tranquil-pds-nginx
```

## Configure firewall if you're into that sort of thing

```sh
apk add iptables ip6tables
iptables -A INPUT -p tcp --dport 22 -j ACCEPT
iptables -A INPUT -p tcp --dport 80 -j ACCEPT
iptables -A INPUT -p tcp --dport 443 -j ACCEPT
iptables -A INPUT -i lo -j ACCEPT
iptables -A INPUT -m state --state ESTABLISHED,RELATED -j ACCEPT
iptables -P INPUT DROP
ip6tables -A INPUT -p tcp --dport 22 -j ACCEPT
ip6tables -A INPUT -p tcp --dport 80 -j ACCEPT
ip6tables -A INPUT -p tcp --dport 443 -j ACCEPT
ip6tables -A INPUT -i lo -j ACCEPT
ip6tables -A INPUT -m state --state ESTABLISHED,RELATED -j ACCEPT
ip6tables -P INPUT DROP
rc-update add iptables
rc-update add ip6tables
/etc/init.d/iptables save
/etc/init.d/ip6tables save
```

Now finish in the Wildcard TLS certificate section below.

---

# Wildcard TLS certificate

This section sets up the real certificate for the bundled nginx.

With the stack running behind the temporary self-signed certificate, swap in a real wildcard cert. User handles are served as subdomains like `nel.pds.example.com`, so the certificate must cover `*.pds.example.com`, which requires DNS-01 validation.

Get a wildcard certificate using DNS validation:
```sh
podman run --rm -it \
  -v /srv/tranquil-pds/certs:/etc/letsencrypt:Z \
  docker.io/certbot/certbot:v5.2.2 certonly \
  --manual --preferred-challenges dns \
  -d pds.example.com -d '*.pds.example.com' \
  --agree-tos --email you@example.com
```

Follow the prompts to add TXT records to your DNS. Note: manual mode doesn't auto-renew. For automated renewal, use a DNS provider plugin or something.

Link the certificates into place:
```sh
ln -sf /srv/tranquil-pds/certs/live/pds.example.com/fullchain.pem /srv/tranquil-pds/certs/fullchain.pem
ln -sf /srv/tranquil-pds/certs/live/pds.example.com/privkey.pem /srv/tranquil-pds/certs/privkey.pem
```

Restart nginx to load them:

**Debian:**
```bash
systemctl restart tranquil-pds-nginx
```

**Alpine:**
```sh
rc-service tranquil-pds-nginx restart
```

## Cert renewal

Manual mode doesn't auto-renew, so add a renewal job to root's crontab with `crontab -e`.

**Debian:**
```
0 0 * * * podman run --rm -v /srv/tranquil-pds/certs:/etc/letsencrypt:Z -v /srv/tranquil-pds/acme:/var/www/acme:Z docker.io/certbot/certbot:v5.2.2 renew --quiet && systemctl reload tranquil-pds-nginx
```

**Alpine:**
```
0 0 * * * podman run --rm -v /srv/tranquil-pds/certs:/etc/letsencrypt -v /srv/tranquil-pds/acme:/var/www/acme docker.io/certbot/certbot:v5.2.2 renew --quiet && rc-service tranquil-pds-nginx restart
```

---

# Verification and maintenance

## Verify installation

```sh
curl -s https://pds.example.com/xrpc/_health | jq
curl -s https://pds.example.com/.well-known/atproto-did
```

## View logs

**Debian:**
```bash
journalctl -u tranquil-pds-app -f
podman logs -f tranquil-pds-app
```

**Alpine:**
```sh
rc-service tranquil-pds-app status
podman logs -f tranquil-pds-app
```

## Update Tranquil PDS

Pull the latest image:
```sh
podman login atcr.io
podman pull atcr.io/tranquil.farm/tranquil-pds:latest
```

Debian:
```bash
systemctl restart tranquil-pds-app
```

Alpine:
```sh
rc-service tranquil-pds-app restart
```

To update a source-built deployment, `git pull` in the repo and rebuild with `podman build -t atcr.io/tranquil.farm/tranquil-pds:latest .` before restarting.

## Backup database

```sh
podman exec tranquil-pds-db pg_dump -U tranquil_pds pds > /var/backups/pds-$(date +%Y%m%d).sql
```

## Custom homepage

If a `homepage.html` exists in the app's frontend directory it is served at `/`. The account dashboard stays at `/app/`. The directory defaults to `/var/lib/tranquil-pds/frontend` and is set by `FRONTEND_DIR`. Mount your own into the app container:

```sh
-v /srv/tranquil-pds/homepage.html:/var/lib/tranquil-pds/frontend/homepage.html:ro,Z
```

For ex:
```html
<!DOCTYPE html>
<html>
<head>
    <title>Welcome to my PDS</title>
    <style>
        body { font-family: system-ui; max-width: 600px; margin: 100px auto; padding: 20px; }
    </style>
</head>
<body>
    <h1>Welcome to my dark web popsocket store</h1>
    <p>This is a <a href="https://atproto.com">AT Protocol</a> Personal Data Server.</p>
    <p><a href="/app/">Sign in</a> or learn more at <a href="https://bsky.social">Bluesky</a>.</p>
</body>
</html>
```
