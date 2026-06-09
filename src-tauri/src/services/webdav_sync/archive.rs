//! Skills ZIP 打包 / 解压 + 备份回滚

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tempfile::{tempdir, TempDir};
use zip::{write::SimpleFileOptions, DateTime};

use crate::database::Database;
use crate::error::AppError;
use crate::services::skill::SkillService;

const MAX_ZIP_ENTRIES: usize = 10_000;
const MAX_ZIP_EXTRACT_BYTES: u64 = 512 * 1024 * 1024; // 512 MB
const SYNC_METADATA_DIR: &str = ".cc-switch-sync";
const SKILL_PATH_MAP_FILE: &str = "skill-path-map.json";

fn localized(key: &'static str, zh: impl Into<String>, en: impl Into<String>) -> AppError {
    AppError::localized(key, zh, en)
}

fn io_context_localized(
    _key: &'static str,
    zh: impl Into<String>,
    en: impl Into<String>,
    source: std::io::Error,
) -> AppError {
    let zh_msg = zh.into();
    let en_msg = en.into();
    AppError::IoContext {
        context: format!("{zh_msg} ({en_msg})"),
        source,
    }
}

// ---------------------------------------------------------------------------
// Skills 备份 / 回滚
// ---------------------------------------------------------------------------

pub struct SkillsBackup {
    _tmp: TempDir,
    backup_path: PathBuf,
    ssot_dir: PathBuf,
}

impl SkillsBackup {
    pub fn backup_current_skills() -> Result<Self, AppError> {
        let ssot = SkillService::get_ssot_dir()?;
        let tmp = tempdir().map_err(|e| {
            io_context_localized(
                "webdav.sync.skills_backup_tmpdir_failed",
                "创建 skills 备份临时目录失败",
                "Failed to create temporary directory for skills backup",
                e,
            )
        })?;
        let backup_path = tmp.path().join("skills-backup");
        if ssot.exists() {
            copy_dir_recursive(&ssot, &backup_path)?;
        }
        Ok(Self {
            _tmp: tmp,
            backup_path,
            ssot_dir: ssot,
        })
    }

    pub fn restore(self) -> Result<(), AppError> {
        if self.ssot_dir.exists() {
            fs::remove_dir_all(&self.ssot_dir).map_err(|e| AppError::io(&self.ssot_dir, e))?;
        }
        if self.backup_path.exists() {
            fs::create_dir_all(&self.ssot_dir).map_err(|e| AppError::io(&self.ssot_dir, e))?;
            copy_dir_recursive(&self.backup_path, &self.ssot_dir)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ZIP 打包
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SkillPathMapping {
    pub original_directory: String,
    pub archive_directory: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct SkillPathMap {
    version: u32,
    mappings: Vec<SkillPathMapping>,
}

pub fn zip_skills_for_sync(db: &Database, dest_path: &Path) -> Result<(), AppError> {
    let source = SkillService::get_ssot_dir()?;
    let tmp = tempdir().map_err(|e| {
        io_context_localized(
            "webdav.sync.skills_overlay_tmpdir_failed",
            "创建 skills 同步临时目录失败",
            "Failed to create temporary directory for skills sync overlay",
            e,
        )
    })?;
    let overlay = tmp.path().join("skills-overlay");

    if source.exists() {
        copy_dir_recursive(&source, &overlay)?;
        let _ = fs::remove_dir_all(overlay.join(SYNC_METADATA_DIR));
    } else {
        fs::create_dir_all(&overlay).map_err(|e| AppError::io(&overlay, e))?;
    }

    let mappings = copy_external_skill_dirs_into_overlay(db, &source, &overlay)?;
    if !mappings.is_empty() {
        write_skill_path_map(&overlay, mappings)?;
    }

    zip_dir_to_path(&overlay, dest_path)
}

fn zip_dir_to_path(source: &Path, dest_path: &Path) -> Result<(), AppError> {
    if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
    }

    let file = fs::File::create(dest_path).map_err(|e| AppError::io(dest_path, e))?;
    let mut writer = zip::ZipWriter::new(file);
    let options = zip_file_options();

    if source.exists() {
        let canonical_root = fs::canonicalize(&source).unwrap_or_else(|_| source.clone());
        let mut visited = HashSet::new();
        mark_visited_dir(&canonical_root, &mut visited)?;
        zip_dir_recursive(
            &canonical_root,
            &canonical_root,
            &mut writer,
            options,
            &mut visited,
        )?;
    }

    writer.finish().map_err(|e| {
        localized(
            "webdav.sync.skills_zip_write_failed",
            format!("写入 skills.zip 失败: {e}"),
            format!("Failed to write skills.zip: {e}"),
        )
    })?;
    Ok(())
}

fn copy_external_skill_dirs_into_overlay(
    db: &Database,
    ssot_dir: &Path,
    overlay: &Path,
) -> Result<Vec<SkillPathMapping>, AppError> {
    let canonical_ssot = fs::canonicalize(ssot_dir).ok();
    let mut used_dirs = collect_overlay_top_level_dirs(overlay)?;
    let mut mappings = Vec::new();

    for skill in db.get_all_installed_skills()?.values() {
        let source = PathBuf::from(&skill.directory);
        if !source.is_absolute() || !source.join("SKILL.md").is_file() {
            continue;
        }

        let canonical_source = match fs::canonicalize(&source) {
            Ok(path) => path,
            Err(_) => continue,
        };
        if canonical_ssot
            .as_ref()
            .is_some_and(|root| canonical_source.starts_with(root))
        {
            continue;
        }

        let archive_dir = unique_external_archive_dir(&canonical_source, &mut used_dirs);
        let dest = overlay.join(&archive_dir);
        if dest.exists() {
            fs::remove_dir_all(&dest).map_err(|e| AppError::io(&dest, e))?;
        }
        copy_dir_recursive(&canonical_source, &dest)?;
        mappings.push(SkillPathMapping {
            original_directory: skill.directory.clone(),
            archive_directory: archive_dir,
        });
    }

    mappings.sort_by(|a, b| a.original_directory.cmp(&b.original_directory));
    Ok(mappings)
}

fn collect_overlay_top_level_dirs(overlay: &Path) -> Result<HashSet<String>, AppError> {
    let mut dirs = HashSet::new();
    if !overlay.exists() {
        return Ok(dirs);
    }
    for entry in fs::read_dir(overlay).map_err(|e| AppError::io(overlay, e))? {
        let entry = entry.map_err(|e| AppError::io(overlay, e))?;
        if entry.path().is_dir() {
            dirs.insert(entry.file_name().to_string_lossy().into_owned());
        }
    }
    Ok(dirs)
}

fn unique_external_archive_dir(source: &Path, used_dirs: &mut HashSet<String>) -> String {
    let base = safe_archive_dir_name(source);
    if used_dirs.insert(base.clone()) {
        return base;
    }
    for idx in 2.. {
        let candidate = format!("{base}-{idx}");
        if used_dirs.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!("unbounded collision loop should always return");
}

fn safe_archive_dir_name(source: &Path) -> String {
    let raw = source
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("external-skill");
    let sanitized: String = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = sanitized.trim_matches(|ch| matches!(ch, '.' | '-' | '_'));
    if trimmed.is_empty() {
        "external-skill".to_string()
    } else {
        trimmed.to_string()
    }
}

fn write_skill_path_map(
    overlay: &Path,
    mappings: Vec<SkillPathMapping>,
) -> Result<(), AppError> {
    let metadata_dir = overlay.join(SYNC_METADATA_DIR);
    fs::create_dir_all(&metadata_dir).map_err(|e| AppError::io(&metadata_dir, e))?;
    let map = SkillPathMap {
        version: 1,
        mappings,
    };
    let bytes =
        serde_json::to_vec_pretty(&map).map_err(|e| AppError::JsonSerialize { source: e })?;
    crate::config::atomic_write(&metadata_dir.join(SKILL_PATH_MAP_FILE), &bytes)
}

pub(crate) fn read_restored_skill_path_map() -> Result<HashMap<String, String>, AppError> {
    let path = SkillService::get_ssot_dir()?
        .join(SYNC_METADATA_DIR)
        .join(SKILL_PATH_MAP_FILE);
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let bytes = fs::read(&path).map_err(|e| AppError::io(&path, e))?;
    let map: SkillPathMap = serde_json::from_slice(&bytes)
        .map_err(|e| AppError::Config(format!("Invalid skill path map: {e}")))?;
    Ok(map
        .mappings
        .into_iter()
        .map(|entry| (entry.original_directory, entry.archive_directory))
        .collect())
}

pub(crate) fn remove_restored_skill_sync_metadata() -> Result<(), AppError> {
    let metadata_dir = SkillService::get_ssot_dir()?.join(SYNC_METADATA_DIR);
    if metadata_dir.exists() {
        fs::remove_dir_all(&metadata_dir).map_err(|e| AppError::io(&metadata_dir, e))?;
    }
    Ok(())
}

pub fn zip_file_options() -> SimpleFileOptions {
    SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(DateTime::default())
}

/// 记录已访问目录的 canonical path，返回 true 表示首次访问。
fn mark_visited_dir(path: &Path, visited: &mut HashSet<PathBuf>) -> Result<bool, AppError> {
    let canonical = fs::canonicalize(path).map_err(|e| AppError::io(path, e))?;
    Ok(visited.insert(canonical))
}

pub fn zip_dir_recursive(
    root: &Path,
    current: &Path,
    writer: &mut zip::ZipWriter<fs::File>,
    options: SimpleFileOptions,
    visited: &mut HashSet<PathBuf>,
) -> Result<(), AppError> {
    let mut entries = fs::read_dir(current)
        .map_err(|e| AppError::io(current, e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| AppError::io(current, e))?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // 跳过 dotfiles，但保留 WebDAV sync 自己的可移植路径元数据。
        if name_str.starts_with('.') && name_str != SYNC_METADATA_DIR {
            continue;
        }

        // 解析符号链接，确保目标在 root 内
        let real_path = match fs::canonicalize(&path) {
            Ok(p) if p.starts_with(root) => p,
            Ok(_) => {
                log::warn!(
                    "[WebDAV] Skipping symlink outside skills root: {}",
                    path.display()
                );
                continue;
            }
            Err(_) => path.clone(),
        };

        let rel = real_path
            .strip_prefix(root)
            .or_else(|_| path.strip_prefix(root))
            .map_err(|e| {
                localized(
                    "webdav.sync.zip_relative_path_failed",
                    format!("生成 ZIP 相对路径失败: {e}"),
                    format!("Failed to build relative ZIP path: {e}"),
                )
            })?;
        let rel_str = rel.to_string_lossy().replace('\\', "/");

        if real_path.is_dir() {
            // 跳过已访问的目录（符号链接循环检测）
            if !mark_visited_dir(&real_path, visited)? {
                log::warn!(
                    "[WebDAV] Skipping already visited directory: {}",
                    real_path.display()
                );
                continue;
            }
            writer
                .add_directory(format!("{rel_str}/"), options)
                .map_err(|e| {
                    localized(
                        "webdav.sync.zip_add_directory_failed",
                        format!("写入 ZIP 目录失败: {e}"),
                        format!("Failed to write ZIP directory entry: {e}"),
                    )
                })?;
            zip_dir_recursive(root, &real_path, writer, options, visited)?;
        } else {
            writer.start_file(&rel_str, options).map_err(|e| {
                localized(
                    "webdav.sync.zip_start_file_failed",
                    format!("写入 ZIP 文件头失败: {e}"),
                    format!("Failed to start ZIP file entry: {e}"),
                )
            })?;
            let mut f = fs::File::open(&real_path).map_err(|e| AppError::io(&real_path, e))?;
            let mut buf = Vec::new();
            f.read_to_end(&mut buf)
                .map_err(|e| AppError::io(&real_path, e))?;
            writer.write_all(&buf).map_err(|e| {
                localized(
                    "webdav.sync.zip_write_file_failed",
                    format!("写入 ZIP 文件内容失败: {e}"),
                    format!("Failed to write ZIP file content: {e}"),
                )
            })?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// ZIP 解压 + 恢复
// ---------------------------------------------------------------------------

pub fn restore_skills_zip(raw: &[u8]) -> Result<(), AppError> {
    let tmp = tempdir().map_err(|e| {
        io_context_localized(
            "webdav.sync.skills_extract_tmpdir_failed",
            "创建 skills 解压临时目录失败",
            "Failed to create temporary directory for skills extraction",
            e,
        )
    })?;
    let zip_path = tmp.path().join("skills.zip");
    crate::config::atomic_write(&zip_path, raw)?;

    let file = fs::File::open(&zip_path).map_err(|e| AppError::io(&zip_path, e))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| {
        localized(
            "webdav.sync.skills_zip_parse_failed",
            format!("解析 skills.zip 失败: {e}"),
            format!("Failed to parse skills.zip: {e}"),
        )
    })?;

    if archive.len() > MAX_ZIP_ENTRIES {
        return Err(localized(
            "webdav.sync.skills_zip_too_many_entries",
            format!(
                "skills.zip 条目数过多（{}），上限 {MAX_ZIP_ENTRIES}",
                archive.len()
            ),
            format!(
                "skills.zip has too many entries ({}), limit is {MAX_ZIP_ENTRIES}",
                archive.len()
            ),
        ));
    }

    let extracted = tmp.path().join("skills-extracted");
    fs::create_dir_all(&extracted).map_err(|e| AppError::io(&extracted, e))?;

    let mut total_bytes: u64 = 0;
    for idx in 0..archive.len() {
        let mut entry = archive.by_index(idx).map_err(|e| {
            localized(
                "webdav.sync.skills_zip_entry_read_failed",
                format!("读取 ZIP 项失败: {e}"),
                format!("Failed to read ZIP entry: {e}"),
            )
        })?;
        let Some(safe_name) = entry.enclosed_name() else {
            continue;
        };
        let out_path = extracted.join(safe_name);
        if entry.is_dir() {
            fs::create_dir_all(&out_path).map_err(|e| AppError::io(&out_path, e))?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
        }
        let mut out = fs::File::create(&out_path).map_err(|e| AppError::io(&out_path, e))?;
        let _written = copy_entry_with_total_limit(
            &mut entry,
            &mut out,
            &mut total_bytes,
            MAX_ZIP_EXTRACT_BYTES,
            &out_path,
        )?;
    }

    let ssot = SkillService::get_ssot_dir()?;
    let bak = ssot.with_extension("bak");

    // 原子替换：先 rename 到 .bak，再 copy，失败则回滚
    if ssot.exists() {
        if bak.exists() {
            let _ = fs::remove_dir_all(&bak);
        }
        fs::rename(&ssot, &bak).map_err(|e| AppError::io(&ssot, e))?;
    }

    if let Err(e) = copy_dir_recursive(&extracted, &ssot) {
        if bak.exists() {
            let _ = fs::remove_dir_all(&ssot);
            let _ = fs::rename(&bak, &ssot);
        }
        return Err(e);
    }

    let _ = fs::remove_dir_all(&bak);
    Ok(())
}

/// 带总量限制的流式复制，在写入前检查大小是否超限。
fn copy_entry_with_total_limit(
    reader: &mut impl Read,
    writer: &mut impl Write,
    total_bytes: &mut u64,
    max_total_bytes: u64,
    out_path: &Path,
) -> Result<u64, AppError> {
    let mut buf = [0u8; 16 * 1024];
    let mut written = 0u64;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| AppError::io(out_path, e))?;
        if n == 0 {
            break;
        }

        if total_bytes.saturating_add(n as u64) > max_total_bytes {
            let max_mb = max_total_bytes / 1024 / 1024;
            return Err(localized(
                "webdav.sync.skills_zip_too_large",
                format!("skills.zip 解压后体积超过上限（{max_mb} MB）"),
                format!("skills.zip extracted size exceeds limit ({max_mb} MB)"),
            ));
        }

        writer
            .write_all(&buf[..n])
            .map_err(|e| AppError::io(out_path, e))?;
        *total_bytes += n as u64;
        written += n as u64;
    }
    Ok(written)
}

// ---------------------------------------------------------------------------
// 目录递归复制
// ---------------------------------------------------------------------------

pub fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<(), AppError> {
    let mut visited = HashSet::new();
    copy_dir_recursive_inner(src, dest, &mut visited)
}

fn copy_dir_recursive_inner(
    src: &Path,
    dest: &Path,
    visited: &mut HashSet<PathBuf>,
) -> Result<(), AppError> {
    if !src.exists() {
        return Ok(());
    }
    if !mark_visited_dir(src, visited)? {
        log::warn!(
            "[WebDAV] Skipping already visited copy path: {}",
            src.display()
        );
        return Ok(());
    }
    fs::create_dir_all(dest).map_err(|e| AppError::io(dest, e))?;
    for entry in fs::read_dir(src).map_err(|e| AppError::io(src, e))? {
        let entry = entry.map_err(|e| AppError::io(src, e))?;
        let path = entry.path();
        let dest_path = dest.join(entry.file_name());
        if path.is_dir() {
            copy_dir_recursive_inner(&path, &dest_path, visited)?;
        } else {
            fs::copy(&path, &dest_path).map_err(|e| AppError::io(&dest_path, e))?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn zip_output_is_stable_for_same_content() {
        let tmp = tempdir().expect("create temp dir");
        let source = tmp.path().join("skills");
        fs::create_dir_all(source.join("nested")).expect("create source dirs");
        fs::write(source.join("b.txt"), b"bbb").expect("write b");
        fs::write(source.join("nested").join("a.txt"), b"aaa").expect("write a");

        let zip1 = tmp.path().join("first.zip");
        let zip2 = tmp.path().join("second.zip");

        let file1 = fs::File::create(&zip1).expect("create zip1");
        let mut writer1 = zip::ZipWriter::new(file1);
        let mut visited1 = HashSet::new();
        mark_visited_dir(&source, &mut visited1).expect("mark root");
        zip_dir_recursive(
            &source,
            &source,
            &mut writer1,
            zip_file_options(),
            &mut visited1,
        )
        .expect("zip source #1");
        writer1.finish().expect("finish zip1");

        std::thread::sleep(std::time::Duration::from_secs(1));

        let file2 = fs::File::create(&zip2).expect("create zip2");
        let mut writer2 = zip::ZipWriter::new(file2);
        let mut visited2 = HashSet::new();
        mark_visited_dir(&source, &mut visited2).expect("mark root");
        zip_dir_recursive(
            &source,
            &source,
            &mut writer2,
            zip_file_options(),
            &mut visited2,
        )
        .expect("zip source #2");
        writer2.finish().expect("finish zip2");

        let bytes1 = fs::read(&zip1).expect("read zip1");
        let bytes2 = fs::read(&zip2).expect("read zip2");
        assert_eq!(bytes1, bytes2, "zip output should be deterministic");
    }

    #[test]
    fn mark_visited_dir_tracks_canonical_duplicates() {
        let temp = tempdir().expect("tempdir");
        let dir = temp.path().join("skills");
        fs::create_dir_all(&dir).expect("create dir");

        let mut visited = HashSet::new();
        assert!(mark_visited_dir(&dir, &mut visited).expect("first visit"));
        assert!(!mark_visited_dir(&dir, &mut visited).expect("second visit"));
    }

    #[test]
    fn safe_archive_dir_name_sanitizes_external_paths() {
        assert_eq!(
            safe_archive_dir_name(Path::new("/tmp/openclaw/my skill")),
            "my-skill"
        );
        assert_eq!(
            safe_archive_dir_name(Path::new("/tmp/openclaw/.hidden")),
            "hidden"
        );
    }

    #[test]
    fn unique_external_archive_dir_avoids_collisions() {
        let mut used = HashSet::from(["calendar".to_string(), "calendar-2".to_string()]);
        assert_eq!(
            unique_external_archive_dir(Path::new("/tmp/openclaw/calendar"), &mut used),
            "calendar-3"
        );
    }

    #[test]
    fn zip_dir_recursive_keeps_sync_metadata_but_skips_other_dotdirs() {
        let tmp = tempdir().expect("tempdir");
        let source = tmp.path().join("skills");
        fs::create_dir_all(source.join(SYNC_METADATA_DIR)).expect("create metadata");
        fs::create_dir_all(source.join(".hidden")).expect("create hidden");
        fs::write(
            source.join(SYNC_METADATA_DIR).join(SKILL_PATH_MAP_FILE),
            "{}",
        )
        .expect("write metadata");
        fs::write(source.join(".hidden").join("secret.txt"), "secret").expect("write hidden");

        let zip_path = tmp.path().join("skills.zip");
        let file = fs::File::create(&zip_path).expect("create zip");
        let mut writer = zip::ZipWriter::new(file);
        let mut visited = HashSet::new();
        mark_visited_dir(&source, &mut visited).expect("mark root");
        zip_dir_recursive(
            &source,
            &source,
            &mut writer,
            zip_file_options(),
            &mut visited,
        )
        .expect("zip source");
        writer.finish().expect("finish zip");

        let file = fs::File::open(zip_path).expect("open zip");
        let mut archive = zip::ZipArchive::new(file).expect("read zip");
        let mut names = Vec::new();
        for idx in 0..archive.len() {
            names.push(archive.by_index(idx).expect("entry").name().to_string());
        }
        assert!(names.contains(&format!("{SYNC_METADATA_DIR}/")));
        assert!(names.contains(&format!(
            "{SYNC_METADATA_DIR}/{SKILL_PATH_MAP_FILE}"
        )));
        assert!(!names.iter().any(|name| name.contains(".hidden")));
    }

    #[test]
    fn copy_entry_with_total_limit_rejects_oversized_stream_before_write() {
        use std::io::Cursor;
        let mut reader = Cursor::new(vec![1u8; 16]);
        let mut writer = Vec::new();
        let mut total_bytes = 0u64;

        let err = copy_entry_with_total_limit(
            &mut reader,
            &mut writer,
            &mut total_bytes,
            8,
            Path::new("skills-extracted/file.bin"),
        )
        .expect_err("stream larger than limit should be rejected");
        assert!(err.to_string().contains("超过"), "unexpected error: {err}");
        assert_eq!(
            writer.len(),
            0,
            "should not write when the first chunk exceeds limit"
        );
    }
}
