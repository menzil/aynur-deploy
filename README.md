# aynur-deploy

`aynur-deploy` accepts authenticated Gitee Tag WebHooks and serializes deployments through SQLite. It fetches the exact Tag object, builds an immutable release, atomically replaces a `current` symlink, checks the configured HTTP endpoint, and rolls back on failure.

The supported release targets are `static` and `rust_aynur`. Rust releases always run `cargo build --release --locked --package <package> --bin <binary>` and invoke only `aynur reload <app> --update-env`; the app must already be registered in the configured `AYNUR_HOME`.

## Commands

Every command requires an explicit configuration path and writes one JSON object to stdout or stderr.

```bash
aynur-deploy check-config --config /etc/aynur-deploy/config.toml
aynur-deploy status --config /etc/aynur-deploy/config.toml --project-id orhan-blog
aynur-deploy retry --config /etc/aynur-deploy/config.toml --deployment-id <deployment-id>
aynur-deploy rollback --config /etc/aynur-deploy/config.toml --project-id orhan-blog --commit-sha <commit-sha>
aynur-deploy unblock --config /etc/aynur-deploy/config.toml --project-id orhan-blog
aynur-deploy serve --config /etc/aynur-deploy/config.toml
```

Configuration files reject unknown fields and require every documented field. Each project's WebHook token is read from its configured environment file; that file must be a regular file with mode `0600`.

## Host Layout

The production assets in `ops/` expect:

```text
/usr/local/bin/aynur-deploy
/usr/local/bin/aynur
/var/lib/aynur-deploy/cargo/bin/cargo
/etc/aynur-deploy/config.toml
/etc/aynur-deploy/projects/orhan-blog.toml
/etc/aynur-deploy/secrets/orhan-blog.env
/var/lib/aynur-deploy/
```

Install `aynur-deploy` as a prebuilt Linux binary produced on a compatible Ubuntu/glibc build host. Do not compile it on the production host. Cargo and Aynur are required only when at least one configured project uses the `rust_aynur` target; static-only deployments do not require either executable to be installed.

The systemd unit runs as the dedicated `deploy` user and permits writes only below `/var/lib/aynur-deploy`. The Nginx configuration exposes only the project-specific hook route and continues to perform authentication in the Rust service.

Before enabling the unit, run `check-config`, ensure the `deploy` user can fetch every configured repository, and register each Rust application in its configured Aynur home. Validate Nginx with `nginx -t` before reload.
