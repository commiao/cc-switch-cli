# OpenClaw cc-switch WebDAV sync operations

This document records the operational checks for the OpenClaw cc-switch WebDAV
sync path. It is intentionally small: the goal is to make the current recovery
state repeatable without depending on memory or shell history.

## Current target state

- OpenClaw host: `iZ0xi1l67zzk3qgaail2zvZ`
- cc-switch binary: `/home/admin/.local/bin/cc-switch`
- Expected version after the exporter fix: `cc-switch 5.8.1`
- Auto sync wrapper: `/home/admin/clawd/scripts/cc-switch-webdav-sync.sh`
- Auto sync log: `/home/admin/clawd/logs/cc-switch-webdav-sync.log`
- Cron schedule: `7,37 * * * *`
- Expected restored snapshot:
  - `42` rows in `skills`
  - `0` absolute `skills.directory` values
  - `91` restored `SKILL.md` files
  - required skills: `ocr`, `session-lifetime-manager`

Do not print WebDAV credentials in logs or chat. Use environment variables for
passwords when a command needs to access WebDAV directly.

## Quick health check

Run from a machine that can SSH to OpenClaw:

```bash
ssh -i /Users/mac/.ssh/openclaw_aliyun \
  -o IdentitiesOnly=yes \
  -o StrictHostKeyChecking=accept-new \
  -o ConnectTimeout=8 \
  admin@100.79.177.102 \
  'hostname;
   /home/admin/.local/bin/cc-switch --version;
   crontab -l | sed -n "/cc-switch WebDAV auto sync/,/cc-switch WebDAV auto sync/p";
   tail -n 20 /home/admin/clawd/logs/cc-switch-webdav-sync.log;
   python3 /home/admin/tmp/print_openclaw_webdav_safe_summary.py'
```

Healthy output should show:

- `cc-switch 5.8.1`
- the `7,37 * * * * /home/admin/clawd/scripts/cc-switch-webdav-sync.sh` cron entry
- recent `WebDAV upload completed` lines ending with `status=0`
- `enabled=True`, `autoSync=True`, `lastError=None`

## Black-box restore verification

Use an isolated config directory. This does not overwrite the real
`/home/admin/.cc-switch` state.

```bash
ssh -i /Users/mac/.ssh/openclaw_aliyun \
  -o IdentitiesOnly=yes \
  -o StrictHostKeyChecking=accept-new \
  -o ConnectTimeout=8 \
  admin@100.79.177.102 \
  'set -e;
   d=/home/admin/tmp/cc-switch-download-verify-$(date -u +%Y%m%d-%H%M%S);
   mkdir -p "$d";
   cp /home/admin/.cc-switch/settings.json "$d/settings.json";
   CC_SWITCH_CONFIG_DIR="$d" /home/admin/.local/bin/cc-switch config webdav download;
   sqlite3 "$d/cc-switch.db" "SELECT COUNT(*) FROM skills;" | sed "s/^/db_skills=/";
   sqlite3 "$d/cc-switch.db" "SELECT COUNT(*) FROM skills WHERE directory LIKE '\''/%'\'';" | sed "s/^/absolute_dirs=/";
   find "$d/skills" -name SKILL.md | wc -l | tr -d " " | sed "s/^/skill_md=/";
   sqlite3 "$d/cc-switch.db" "SELECT name,directory FROM skills WHERE name IN ('\''ocr'\'','\''session-lifetime-manager'\'') ORDER BY name;"'
```

Healthy output should include:

```text
db_skills=42
absolute_dirs=0
skill_md=91
ocr|ocr
session-lifetime-manager|session-lifetime-manager
```

## Snapshot artifact verification

When the WebDAV snapshot is available as local files, validate the manifest,
hashes, SQL dump, and zip contents:

```bash
python3 scripts/check_webdav_snapshot.py \
  --snapshot-dir /Users/mac/public-sync/cc-switch-sync/v2/db-v6/default \
  --expect-skills 42 \
  --expect-normalized-absolute-dirs 0 \
  --expect-skill-md 91 \
  --require-skill ocr \
  --require-skill session-lifetime-manager \
  --forbid-part node_modules \
  --forbid-part logs \
  --forbid-part tmp
```

For direct WebDAV access, keep the password in an environment variable:

```bash
export CC_SWITCH_WEBDAV_PASSWORD='...'
python3 scripts/check_webdav_snapshot.py \
  --url 'https://example.invalid/cc-switch-sync/v2/db-v6/default' \
  --username 'webdav-user' \
  --password-env CC_SWITCH_WEBDAV_PASSWORD \
  --expect-skills 42 \
  --expect-normalized-absolute-dirs 0 \
  --expect-skill-md 91 \
  --require-skill ocr \
  --require-skill session-lifetime-manager
```

The final line should be:

```text
snapshot_ok
```

## What was fixed in the fork

The forked exporter fix keeps external skill directories usable without copying
large runtime artifacts into WebDAV:

- external skill directories are included in `skills.zip`
- absolute source paths are normalized on restore
- a path map is generated for restore-time rewriting
- large or generated folders are excluded, including `node_modules`, `logs`,
  `.cache`, `.git`, `.next`, `target`, `dist`, `build`, `tmp`, and virtualenvs

The validated fork commit is:

```text
c7bfd06 fix(webdav): exclude bulky skill artifacts
```

## Recovery notes

- Do not restart networking, Tailscale, SSH, or OpenClaw services for this check.
- Keep `/home/admin/.local/bin/cc-switch.backup-*` files until the replacement
  binary has survived at least one normal auto-sync window.
- If an upload log reports success but a local mirror briefly shows a
  manifest/hash mismatch, wait and re-check. The mirror can lag behind the
  WebDAV write; the restore verification is the final source of truth.
- If black-box restore fails, preserve the isolated verification directory and
  inspect `manifest.json`, `db.sql`, and `skills.zip` before re-running upload.
