# Docker Restarter

A Rust-based utility for monitoring Docker container logs and automatically restarting services when specific log messages appear.

## Usage

#### From source

```sh
cargo run -- --watch container-1 --restart container-2 --pattern hello_world --skip-first=true
```

#### Docker

You can run the program manually with the following command:

```sh
docker run --rm \
  -v /var/run/docker.sock:/var/run/docker.sock \
  ghcr.io/zachjw34/docker-restarter:latest \
  --watch container-1 \
  --restart container-2 \
  --pattern hello_world \
  --skip-first=true
```

#### Docker Compose Setup

```yaml
version: "3.8"

services:
  app:
    image: ghcr.io/zachjw34/docker-restarter:latest
    environment:
      - RUST_LOG=info
    restart: always
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock # Access Docker socket for control
    command: --watch container-1 --restart container-2 --pattern hello_world --skip-first=true
```

#### Args

| Flag           | Description                                 | Example                 |
| -------------- | ------------------------------------------- | ----------------------- |
| `--watch`      | Container to monitor                        | `--watch logger`        |
| `--restart`    | Containers to restart, comma delimitted     | `--restart logger`      |
| `--pattern`    | Log pattern to watch for                    | `--pattern hello_world` |
| `--skip-first` | Skip the first occurrence before restarting | `--skip-first=true`     |

## Contributing

Feel free to submit issues or PRs to improve functionality.

## License

MIT License
