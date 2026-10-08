# Deploying the runner to your own server

`zrunner` is a bash script. It runs on your workstation and drives one Linux server over SSH with Docker. The runner makes outbound connections only; open no inbound port.

## You need

- On the workstation: `bash`, `git`, `ssh`, `scp`, and Docker with buildx. The repo must be a git checkout with a clean working tree (the image is tagged with the commit).
- On the server: Docker with the compose plugin, and SSH access for the user you pass in `ZRUNNER_HOST`.
- A runner config file (see `../examples/config.json`) and your runner token.

## Settings

| Variable | Meaning |
| --- | --- |
| `ZRUNNER_HOST` | Required. SSH target, for example `deploy@server.example.com`. |
| `ZRUNNER_DIR` | Install directory on the server. Default `/opt/zriz-runner`. |
| `ZRUNNER_PLATFORM` | Image platform. Default `linux/amd64`. |
| `ZRUNNER_CONFIG` | Path of the config JSON. Required for `deploy`. |

## First deploy

    export ZRUNNER_HOST=deploy@server.example.com
    export ZRUNNER_CONFIG=./config.json

    # Secrets go into $ZRUNNER_DIR/.env on the server, read from a file, never on the command line.
    printf '%s' 'zrt_...' > /tmp/token && ./ops/zrunner set-secret ZRIZ_RUNNER_TOKEN /tmp/token
    ./ops/zrunner set-secret SHOP_DB_PASSWORD ./db-password.txt

    ./ops/zrunner deploy

Do `set-secret ZRIZ_RUNNER_TOKEN` before the first `deploy`: it refuses to run without it. Use one `set-secret` per `${NAME}` your config needs. Names must match `^[A-Z][A-Z0-9_]*$`. The file must hold one line.

`deploy` builds the runner and worker images, sends them to the server with `docker save | ssh docker load`, copies the compose files and config, sets the image tags in `.env`, runs `docker compose up -d`, waits up to 60 seconds for the runner's first good exchange, and removes old images.

## Day to day

| Command | What it does |
| --- | --- |
| `zrunner deploy` | Build, ship and start. Use it again to update the config or the code. |
| `zrunner set-secret NAME FILE` | Set `NAME` in the server's `.env`. Run `zrunner restart` after. |
| `zrunner logs [-f] [-n N]` | Runner logs. |
| `zrunner ps` | Container status. |
| `zrunner restart` | Restart the runner. |
| `zrunner rollback` | Go back to the previous runner and worker images. |

## The worker

`docker-compose.yml` starts two containers. `runner` has your secrets (`.env`). `worker` is the browser and cli sidecar: it has no `.env`, runs as a non-root user with a read-only filesystem and no capabilities, and listens only on a unix socket in a shared tmpfs volume. Set `"worker": {"socket": "/run/zriz/worker.sock"}` in the config to use it. The worker needs outbound access to the sites you test.

## Optional: reach a database on another Docker network

If your database is on a Docker network of the same host, add this to `.env` on the server:

    ZRIZ_RUNNER_DB_NETWORK=<name of the existing docker network>

`zrunner` then adds `docker-compose.db.yml`, which joins the runner (not the worker) to that network. The network must already exist. If the variable is empty or missing, the overlay is not used.
