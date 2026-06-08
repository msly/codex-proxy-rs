# Scripts

## deploy-run.sh

Deploy the current repo build into the unpacked runtime directory.

Default target:

```bash
../run-v0.0.1-linux-x86_64
```

Build, back up, and replace files without restarting:

```bash
scripts/deploy-run.sh
```

Build, replace files, and restart the running process from the target directory:

```bash
scripts/deploy-run.sh --restart
```

Faster deploy when tests and frontend build are not needed:

```bash
scripts/deploy-run.sh --skip-tests --no-frontend --restart
```

Deploy to a different unpacked runtime directory:

```bash
scripts/deploy-run.sh --target /path/to/run-dir --restart
```

The script preserves runtime data: `config.yaml`, `auths*`, `data`, and other
files are not deleted. Existing `codex-proxy-rs` and `frontend/dist` are backed
up under:

```bash
<target>/backups/<timestamp>/
```

On Linux, replacing the binary does not update an already-running process. Run
with `--restart` when the new code must take effect immediately.

When `--restart` is used, the script writes the actual `codex-proxy-rs` process
ID to:

```bash
<target>/codex-proxy.pid
```

The PID is resolved after the `/health` endpoint succeeds, so the pidfile should
match the process that is actually serving port `18080`.
