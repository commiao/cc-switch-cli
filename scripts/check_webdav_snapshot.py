#!/usr/bin/env python3
"""Validate a cc-switch WebDAV sync snapshot.

The checker works with either a local snapshot directory containing
manifest.json, db.sql, and skills.zip, or a WebDAV base URL. When a URL is used,
credentials are read from an environment variable so secrets do not need to be
placed in shell history.
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import io
import json
import os
import re
import sqlite3
import sys
import urllib.error
import urllib.request
import zipfile
from pathlib import Path
from typing import Dict, Iterable, List, Optional, Tuple


ARTIFACTS = ("db.sql", "skills.zip")


class CheckError(Exception):
    pass


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def read_local(snapshot_dir: Path, name: str) -> bytes:
    path = snapshot_dir / name
    if not path.is_file():
        raise CheckError(f"missing local artifact: {path}")
    return path.read_bytes()


def read_remote(base_url: str, name: str, username: Optional[str], password: Optional[str]) -> bytes:
    url = f"{base_url.rstrip('/')}/{name}"
    request = urllib.request.Request(url)
    if username is not None:
        import base64

        token = base64.b64encode(f"{username}:{password or ''}".encode("utf-8")).decode("ascii")
        request.add_header("Authorization", f"Basic {token}")
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            return response.read()
    except urllib.error.URLError as exc:
        raise CheckError(f"failed to fetch remote artifact {name}: {exc}") from exc


def load_artifacts(args: argparse.Namespace) -> Dict[str, bytes]:
    password = None
    if args.password_env:
        password = os.environ.get(args.password_env)
        if password is None:
            raise CheckError(f"environment variable not set: {args.password_env}")

    if args.snapshot_dir:
        base = Path(args.snapshot_dir)
        return {name: read_local(base, name) for name in ("manifest.json", *ARTIFACTS)}

    if args.url:
        return {
            name: read_remote(args.url, name, args.username, password)
            for name in ("manifest.json", *ARTIFACTS)
        }

    raise CheckError("provide either --snapshot-dir or --url")


def parse_manifest(data: bytes) -> dict:
    try:
        manifest = json.loads(data.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise CheckError(f"invalid manifest.json: {exc}") from exc
    if manifest.get("format") != "cc-switch-webdav-sync":
        raise CheckError(f"unexpected manifest format: {manifest.get('format')!r}")
    if not isinstance(manifest.get("artifacts"), dict):
        raise CheckError("manifest is missing artifacts")
    return manifest


def verify_manifest_artifacts(manifest: dict, artifacts: Dict[str, bytes]) -> List[str]:
    lines = []
    for name in ARTIFACTS:
        metadata = manifest["artifacts"].get(name)
        if not isinstance(metadata, dict):
            raise CheckError(f"manifest missing artifact metadata: {name}")
        data = artifacts[name]
        expected_size = metadata.get("size")
        expected_sha = metadata.get("sha256")
        actual_sha = sha256(data)
        if len(data) != expected_size:
            raise CheckError(
                f"{name} size mismatch: actual={len(data)} manifest={expected_size}"
            )
        if actual_sha != expected_sha:
            raise CheckError(
                f"{name} sha256 mismatch: actual={actual_sha} manifest={expected_sha}"
            )
        lines.append(f"artifact_ok {name} size={len(data)} sha256={actual_sha}")
    return lines


def parse_manifest_age(manifest: dict) -> Optional[dt.datetime]:
    created_at = manifest.get("createdAt")
    if not isinstance(created_at, str):
        return None
    normalized = created_at.replace("Z", "+00:00")
    normalized = re.sub(r"(\.\d{6})\d+([+-]\d\d:\d\d)$", r"\1\2", normalized)
    try:
        parsed = dt.datetime.fromisoformat(normalized)
    except ValueError as exc:
        raise CheckError(f"invalid manifest createdAt: {created_at}") from exc
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=dt.timezone.utc)
    return parsed


def load_sql_dump(db_sql: bytes) -> sqlite3.Connection:
    conn = sqlite3.connect(":memory:")
    try:
        conn.executescript(db_sql.decode("utf-8"))
    except (UnicodeDecodeError, sqlite3.Error) as exc:
        conn.close()
        raise CheckError(f"failed to load db.sql into sqlite: {exc}") from exc
    return conn


def fetch_one(conn: sqlite3.Connection, query: str, params: Tuple[object, ...] = ()) -> int:
    row = conn.execute(query, params).fetchone()
    if row is None:
        raise CheckError(f"query returned no row: {query}")
    return int(row[0])


def load_path_map(skills_zip: bytes) -> Dict[str, str]:
    try:
        with zipfile.ZipFile(io.BytesIO(skills_zip)) as zf:
            try:
                data = zf.read(".cc-switch-sync/skill-path-map.json")
            except KeyError:
                return {}
    except zipfile.BadZipFile as exc:
        raise CheckError(f"invalid skills.zip: {exc}") from exc

    try:
        payload = json.loads(data.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise CheckError(f"invalid .cc-switch-sync/skill-path-map.json: {exc}") from exc

    mappings = payload.get("mappings")
    if not isinstance(mappings, list):
        raise CheckError("path map is missing mappings")

    result: Dict[str, str] = {}
    for mapping in mappings:
        if not isinstance(mapping, dict):
            raise CheckError("path map contains a non-object mapping")
        original = mapping.get("originalDirectory")
        archive = mapping.get("archiveDirectory")
        if not isinstance(original, str) or not isinstance(archive, str):
            raise CheckError("path map mapping must contain string directories")
        result[original] = archive
    return result


def verify_db(db_sql: bytes, skills_zip: bytes, args: argparse.Namespace) -> List[str]:
    conn = load_sql_dump(db_sql)
    try:
        skill_count = fetch_one(conn, "SELECT COUNT(*) FROM skills")
        absolute_dirs = fetch_one(conn, "SELECT COUNT(*) FROM skills WHERE directory LIKE '/%'")

        if args.expect_skills is not None and skill_count != args.expect_skills:
            raise CheckError(
                f"skill count mismatch: actual={skill_count} expected={args.expect_skills}"
            )
        if args.expect_absolute_dirs is not None and absolute_dirs != args.expect_absolute_dirs:
            raise CheckError(
                "absolute directory count mismatch: "
                f"actual={absolute_dirs} expected={args.expect_absolute_dirs}"
            )

        lines = [
            f"db_ok skills={skill_count}",
            f"db_ok absolute_dirs={absolute_dirs}",
        ]

        if args.expect_normalized_absolute_dirs is not None:
            path_map = load_path_map(skills_zip)
            dirs = [str(row[0]) for row in conn.execute("SELECT directory FROM skills").fetchall()]
            normalized_absolute_dirs = sum(
                1 for directory in dirs if path_map.get(directory, directory).startswith("/")
            )
            if normalized_absolute_dirs != args.expect_normalized_absolute_dirs:
                raise CheckError(
                    "normalized absolute directory count mismatch: "
                    f"actual={normalized_absolute_dirs} "
                    f"expected={args.expect_normalized_absolute_dirs}"
                )
            lines.append(
                "db_ok normalized_absolute_dirs="
                f"{normalized_absolute_dirs} path_map_entries={len(path_map)}"
            )

        for name in args.require_skill:
            rows = conn.execute(
                "SELECT name, directory FROM skills WHERE name = ? OR directory = ?",
                (name, name),
            ).fetchall()
            if not rows:
                raise CheckError(f"required skill missing from db: {name}")
            dirs = ",".join(sorted(str(row[1]) for row in rows))
            lines.append(f"db_ok required_skill={name} directories={dirs}")
        return lines
    finally:
        conn.close()


def zip_names(skills_zip: bytes) -> List[str]:
    try:
        with zipfile.ZipFile(io.BytesIO(skills_zip)) as zf:
            return zf.namelist()
    except zipfile.BadZipFile as exc:
        raise CheckError(f"invalid skills.zip: {exc}") from exc


def contains_forbidden_part(name: str, forbidden_parts: Iterable[str]) -> str | None:
    parts = [part for part in name.split("/") if part]
    for part in parts:
        if part in forbidden_parts:
            return part
    return None


def verify_zip(skills_zip: bytes, args: argparse.Namespace) -> List[str]:
    names = zip_names(skills_zip)
    skill_md_count = sum(1 for name in names if name.endswith("/SKILL.md") or name == "SKILL.md")
    if args.expect_skill_md is not None and skill_md_count != args.expect_skill_md:
        raise CheckError(
            f"SKILL.md count mismatch: actual={skill_md_count} expected={args.expect_skill_md}"
        )

    for required in args.require_skill:
        expected_root = f"{required}/SKILL.md"
        if expected_root not in names and not any(name.endswith(f"/{expected_root}") for name in names):
            raise CheckError(f"required skill missing from skills.zip: {required}")

    forbidden_hits = []
    for name in names:
        part = contains_forbidden_part(name, args.forbid_part)
        if part:
            forbidden_hits.append((name, part))
            if len(forbidden_hits) >= 10:
                break
    if forbidden_hits:
        formatted = ", ".join(f"{name} ({part})" for name, part in forbidden_hits)
        raise CheckError(f"forbidden paths found in skills.zip: {formatted}")

    return [
        f"zip_ok entries={len(names)}",
        f"zip_ok skill_md={skill_md_count}",
    ]


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--snapshot-dir", help="Local directory with manifest.json, db.sql, skills.zip")
    source.add_argument("--url", help="WebDAV snapshot base URL ending at the profile directory")
    parser.add_argument("--username", help="WebDAV username for --url")
    parser.add_argument("--password-env", help="Read WebDAV password from this environment variable")
    parser.add_argument("--expect-skills", type=int, help="Expected row count in skills table")
    parser.add_argument(
        "--expect-absolute-dirs",
        type=int,
        help="Expected count of skills.directory values that start with /",
    )
    parser.add_argument(
        "--expect-normalized-absolute-dirs",
        type=int,
        help=(
            "Expected absolute directory count after applying "
            ".cc-switch-sync/skill-path-map.json from skills.zip"
        ),
    )
    parser.add_argument("--expect-skill-md", type=int, help="Expected SKILL.md count in skills.zip")
    parser.add_argument(
        "--max-age-minutes",
        type=int,
        help="Fail when manifest createdAt is older than this many minutes",
    )
    parser.add_argument(
        "--require-skill",
        action="append",
        default=[],
        help="Skill name that must exist in db.sql and skills.zip; repeatable",
    )
    parser.add_argument(
        "--forbid-part",
        action="append",
        default=[],
        help="Zip path segment that must not appear, such as node_modules or logs; repeatable",
    )
    return parser


def main(argv: List[str]) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)

    try:
        artifacts = load_artifacts(args)
        manifest = parse_manifest(artifacts["manifest.json"])
        lines = []
        lines.extend(verify_manifest_artifacts(manifest, artifacts))

        created_at = parse_manifest_age(manifest)
        if created_at is not None:
            age = dt.datetime.now(dt.timezone.utc) - created_at.astimezone(dt.timezone.utc)
            age_minutes = max(0, int(age.total_seconds() // 60))
            if args.max_age_minutes is not None and age_minutes > args.max_age_minutes:
                raise CheckError(
                    f"snapshot too old: age_minutes={age_minutes} max={args.max_age_minutes}"
                )
            lines.append(f"manifest_ok createdAt={manifest.get('createdAt')} age_minutes={age_minutes}")
        else:
            lines.append("manifest_ok createdAt=<missing>")

        lines.append(f"manifest_ok snapshotId={manifest.get('snapshotId')}")
        lines.extend(verify_db(artifacts["db.sql"], artifacts["skills.zip"], args))
        lines.extend(verify_zip(artifacts["skills.zip"], args))

        for line in lines:
            print(line)
        print("snapshot_ok")
        return 0
    except CheckError as exc:
        print(f"snapshot_error {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
