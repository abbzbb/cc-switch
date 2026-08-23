use super::env_checker::EnvConflict;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[cfg(target_os = "windows")]
use winreg::enums::*;
#[cfg(target_os = "windows")]
use winreg::RegKey;

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupInfo {
    pub backup_path: String,
    pub timestamp: String,
    pub conflicts: Vec<EnvConflict>,
}

/// Delete environment variables with automatic backup
pub fn delete_env_vars(conflicts: Vec<EnvConflict>) -> Result<BackupInfo, String> {
    // Step 1: Create backup
    let backup_info = create_backup(&conflicts)?;

    // Step 2: Delete variables
    for conflict in &conflicts {
        match delete_single_env(conflict) {
            Ok(_) => {}
            Err(e) => {
                // If deletion fails, we keep the backup but return error
                return Err(format!(
                    "删除环境变量失败: {}. 备份已保存到: {}",
                    e, backup_info.backup_path
                ));
            }
        }
    }

    Ok(backup_info)
}

/// Create backup file before deletion
fn create_backup(conflicts: &[EnvConflict]) -> Result<BackupInfo, String> {
    // Get backup directory
    let backup_dir = get_backup_dir()?;
    fs::create_dir_all(&backup_dir).map_err(|e| format!("创建备份目录失败: {e}"))?;

    // Generate backup file name with timestamp
    let timestamp = Utc::now().format("%Y%m%d_%H%M%S").to_string();
    let backup_file = backup_dir.join(format!("env-backup-{timestamp}.json"));

    // Create backup data
    let backup_info = BackupInfo {
        backup_path: backup_file.to_string_lossy().to_string(),
        timestamp: timestamp.clone(),
        conflicts: conflicts.to_vec(),
    };

    // Write backup file
    let json = serde_json::to_string_pretty(&backup_info)
        .map_err(|e| format!("序列化备份数据失败: {e}"))?;

    fs::write(&backup_file, json).map_err(|e| format!("写入备份文件失败: {e}"))?;

    Ok(backup_info)
}

/// Get backup directory path
fn get_backup_dir() -> Result<PathBuf, String> {
    Ok(crate::config::get_home_dir()
        .join(".cc-switch")
        .join("backups"))
}

/// Parse `path:line` without splitting on every colon (paths may contain `:`).
///
/// The line number is the trailing `:` + digits segment. Restore may omit it.
fn parse_path_and_line(source_path: &str) -> Result<(PathBuf, Option<usize>), String> {
    if source_path.is_empty() {
        return Err("无效的文件路径格式".to_string());
    }

    if let Some((path, line)) = source_path.rsplit_once(':') {
        if !path.is_empty() && !line.is_empty() && line.bytes().all(|b| b.is_ascii_digit()) {
            let n = line
                .parse::<usize>()
                .map_err(|_| "无效的行号".to_string())?;
            return Ok((PathBuf::from(path), Some(n)));
        }
    }

    Ok((PathBuf::from(source_path), None))
}

fn is_safe_env_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_') && name.len() <= 256
}

/// POSIX single-quote so spaces / metacharacters cannot break the rc file.
fn quote_shell_value(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

fn confine_backup_file_in(backup_path: &Path, backup_dir: &Path) -> Result<PathBuf, String> {
    fs::create_dir_all(backup_dir).map_err(|e| format!("创建备份目录失败: {e}"))?;
    let backup_dir = backup_dir
        .canonicalize()
        .map_err(|e| format!("无法解析备份目录: {e}"))?;

    if !backup_path.exists() {
        return Err(format!("备份文件不存在: {}", backup_path.display()));
    }

    let canon = backup_path
        .canonicalize()
        .map_err(|e| format!("无法解析备份路径: {e}"))?;

    if !canon.starts_with(&backup_dir) {
        return Err("备份路径不在允许的备份目录内".to_string());
    }
    if !canon.is_file() {
        return Err("备份路径必须是文件".to_string());
    }
    Ok(canon)
}

#[cfg(not(target_os = "windows"))]
fn confine_user_rc_path_in(path: &Path, home: &Path) -> Result<PathBuf, String> {
    if !path.exists() {
        return Err(format!("文件不存在: {}", path.display()));
    }

    let home_canon = home
        .canonicalize()
        .map_err(|e| format!("无法解析用户主目录: {e}"))?;
    let canon = path
        .canonicalize()
        .map_err(|e| format!("无法解析文件路径 {}: {e}", path.display()))?;

    if !canon.starts_with(&home_canon) {
        return Err(format!("拒绝写入非用户目录的文件: {}", path.display()));
    }

    let allowed = super::env_checker::user_shell_rc_files(home);
    let mut matched = false;
    for allowed_path in &allowed {
        if allowed_path.exists() {
            if let Ok(allowed_canon) = allowed_path.canonicalize() {
                if canon == allowed_canon {
                    matched = true;
                    break;
                }
            }
        }
    }

    if !matched {
        return Err(format!("不允许修改该配置文件: {}", path.display()));
    }

    Ok(canon)
}

fn atomic_write_text(path: &Path, contents: &str) -> Result<(), String> {
    crate::config::atomic_write(path, contents.as_bytes())
        .map_err(|e| format!("写入文件失败 {}: {e}", path.display()))
}

/// Delete a single environment variable
#[cfg(target_os = "windows")]
fn delete_single_env(conflict: &EnvConflict) -> Result<(), String> {
    match conflict.source_type.as_str() {
        "system" => {
            if conflict.source_path.contains("HKEY_CURRENT_USER") {
                let hkcu = RegKey::predef(HKEY_CURRENT_USER)
                    .open_subkey_with_flags("Environment", KEY_ALL_ACCESS)
                    .map_err(|e| format!("打开注册表失败: {}", e))?;

                hkcu.delete_value(&conflict.var_name)
                    .map_err(|e| format!("删除注册表项失败: {}", e))?;
            } else if conflict.source_path.contains("HKEY_LOCAL_MACHINE") {
                let hklm = RegKey::predef(HKEY_LOCAL_MACHINE)
                    .open_subkey_with_flags(
                        "SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Environment",
                        KEY_ALL_ACCESS,
                    )
                    .map_err(|e| format!("打开系统注册表失败 (需要管理员权限): {}", e))?;

                hklm.delete_value(&conflict.var_name)
                    .map_err(|e| format!("删除系统注册表项失败: {}", e))?;
            } else {
                return Err(format!(
                    "无法识别的 Windows 环境变量来源: {}",
                    conflict.source_path
                ));
            }
            Ok(())
        }
        "file" => Err("Windows 系统不应该有文件类型的环境变量".to_string()),
        _ => Err(format!("未知的环境变量来源类型: {}", conflict.source_type)),
    }
}

#[cfg(not(target_os = "windows"))]
fn delete_single_env(conflict: &EnvConflict) -> Result<(), String> {
    delete_single_env_in(conflict, &crate::config::get_home_dir())
}

#[cfg(not(target_os = "windows"))]
fn delete_single_env_in(conflict: &EnvConflict, home: &Path) -> Result<(), String> {
    match conflict.source_type.as_str() {
        "file" => {
            if !is_safe_env_var_name(&conflict.var_name) {
                return Err(format!("无效的环境变量名: {}", conflict.var_name));
            }

            let (file_path, line) = parse_path_and_line(&conflict.source_path)?;
            if line.is_none() {
                return Err("无效的文件路径格式".to_string());
            }

            let file_path = confine_user_rc_path_in(&file_path, home)?;

            let content = fs::read_to_string(&file_path)
                .map_err(|e| format!("读取文件失败 {}: {e}", file_path.display()))?;
            let had_trailing_newline = content.ends_with('\n');

            let new_content: Vec<&str> = content
                .lines()
                .filter(|line| {
                    let trimmed = line.trim();
                    let export_line = trimmed.strip_prefix("export ").unwrap_or(trimmed);

                    if let Some(eq_pos) = export_line.find('=') {
                        let var_name = export_line[..eq_pos].trim();
                        var_name != conflict.var_name
                    } else {
                        true
                    }
                })
                .collect();

            let mut joined = new_content.join("\n");
            if had_trailing_newline && !joined.ends_with('\n') {
                joined.push('\n');
            }

            atomic_write_text(&file_path, &joined)?;
            Ok(())
        }
        "system" => {
            // On Unix, we can't directly delete process environment variables
            Ok(())
        }
        _ => Err(format!("未知的环境变量来源类型: {}", conflict.source_type)),
    }
}

/// Restore environment variables from backup
pub fn restore_from_backup(backup_path: String) -> Result<(), String> {
    let backup_dir = get_backup_dir()?;
    let backup_path = confine_backup_file_in(Path::new(&backup_path), &backup_dir)?;

    let content = fs::read_to_string(&backup_path).map_err(|e| format!("读取备份文件失败: {e}"))?;

    let backup_info: BackupInfo =
        serde_json::from_str(&content).map_err(|e| format!("解析备份文件失败: {e}"))?;

    let home = crate::config::get_home_dir();
    for conflict in &backup_info.conflicts {
        restore_single_env_in(conflict, &home)?;
    }

    Ok(())
}

/// Restore a single environment variable
#[cfg(target_os = "windows")]
fn restore_single_env_in(conflict: &EnvConflict, _home: &Path) -> Result<(), String> {
    match conflict.source_type.as_str() {
        "system" => {
            if conflict.source_path.contains("HKEY_CURRENT_USER") {
                let (hkcu, _) = RegKey::predef(HKEY_CURRENT_USER)
                    .create_subkey("Environment")
                    .map_err(|e| format!("打开注册表失败: {}", e))?;

                hkcu.set_value(&conflict.var_name, &conflict.var_value)
                    .map_err(|e| format!("恢复注册表项失败: {}", e))?;
            } else if conflict.source_path.contains("HKEY_LOCAL_MACHINE") {
                let (hklm, _) = RegKey::predef(HKEY_LOCAL_MACHINE)
                    .create_subkey(
                        "SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Environment",
                    )
                    .map_err(|e| format!("打开系统注册表失败 (需要管理员权限): {}", e))?;

                hklm.set_value(&conflict.var_name, &conflict.var_value)
                    .map_err(|e| format!("恢复系统注册表项失败: {}", e))?;
            }
            Ok(())
        }
        _ => Err(format!(
            "无法恢复类型为 {} 的环境变量",
            conflict.source_type
        )),
    }
}

#[cfg(not(target_os = "windows"))]
fn restore_single_env_in(conflict: &EnvConflict, home: &Path) -> Result<(), String> {
    match conflict.source_type.as_str() {
        "file" => {
            if !is_safe_env_var_name(&conflict.var_name) {
                return Err(format!("无效的环境变量名: {}", conflict.var_name));
            }

            let (file_path, _) = parse_path_and_line(&conflict.source_path)?;
            let file_path = confine_user_rc_path_in(&file_path, home)?;

            let mut content = fs::read_to_string(&file_path)
                .map_err(|e| format!("读取文件失败 {}: {e}", file_path.display()))?;

            if !content.is_empty() && !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str(&format!(
                "export {}={}\n",
                conflict.var_name,
                quote_shell_value(&conflict.var_value)
            ));

            atomic_write_text(&file_path, &content)?;
            Ok(())
        }
        _ => Err(format!(
            "无法恢复类型为 {} 的环境变量",
            conflict.source_type
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_backup_dir_creation() {
        let backup_dir = get_backup_dir();
        assert!(backup_dir.is_ok());
    }

    #[test]
    fn parse_path_and_line_uses_trailing_line_number() {
        let (path, line) = parse_path_and_line("/home/user/.bashrc:12").expect("parse");
        assert_eq!(path, PathBuf::from("/home/user/.bashrc"));
        assert_eq!(line, Some(12));

        let (path, line) = parse_path_and_line("/home/user/weird:name/.zshrc:3").expect("parse");
        assert_eq!(path, PathBuf::from("/home/user/weird:name/.zshrc"));
        assert_eq!(line, Some(3));

        let (path, line) = parse_path_and_line("/home/user/.profile").expect("parse");
        assert_eq!(path, PathBuf::from("/home/user/.profile"));
        assert_eq!(line, None);
    }

    #[test]
    fn quote_shell_value_neutralizes_metacharacters() {
        assert_eq!(quote_shell_value("plain"), "'plain'");
        assert_eq!(quote_shell_value("foo bar"), "'foo bar'");
        assert_eq!(quote_shell_value("$(reboot)"), "'$(reboot)'");
        assert_eq!(quote_shell_value("x'y"), "'x'\\''y'");
    }

    #[test]
    fn restore_rejects_backup_outside_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backup_dir = tmp.path().join("backups");
        fs::create_dir_all(&backup_dir).expect("create backup dir");

        let outside = tmp.path().join("evil.json");
        fs::write(&outside, "{}").expect("write outside");

        let err =
            confine_backup_file_in(&outside, &backup_dir).expect_err("must reject path escape");
        assert!(err.contains("备份路径不在允许的备份目录内"), "{err}");

        let nested = backup_dir.join("env-backup-test.json");
        fs::write(&nested, "{}").expect("write backup");
        let confined = confine_backup_file_in(&nested, &backup_dir).expect("allow backup");
        assert_eq!(confined, nested.canonicalize().unwrap());
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn delete_and_restore_confined_to_user_rc_files() {
        let home = tempfile::tempdir().expect("home");
        let bashrc = home.path().join(".bashrc");
        fs::write(&bashrc, "export ANTHROPIC_API_KEY=old\nexport KEEP=1\n").expect("write bashrc");

        let conflict = EnvConflict {
            var_name: "ANTHROPIC_API_KEY".to_string(),
            var_value: "sk-new value; rm -rf /".to_string(),
            source_type: "file".to_string(),
            source_path: format!("{}:1", bashrc.display()),
        };

        delete_single_env_in(&conflict, home.path()).expect("delete from bashrc");
        let after_delete = fs::read_to_string(&bashrc).expect("read");
        assert!(!after_delete.contains("ANTHROPIC_API_KEY"));
        assert!(after_delete.contains("export KEEP=1"));

        restore_single_env_in(&conflict, home.path()).expect("restore");
        let after_restore = fs::read_to_string(&bashrc).expect("read");
        assert!(after_restore.contains("export ANTHROPIC_API_KEY='sk-new value; rm -rf /'"));
        assert!(!after_restore.contains("export ANTHROPIC_API_KEY=sk-new value; rm -rf /"));
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn delete_rejects_etc_and_paths_outside_home() {
        let home = tempfile::tempdir().expect("home");
        fs::write(home.path().join(".bashrc"), "export FOO=1\n").expect("write bashrc");

        let conflict = EnvConflict {
            var_name: "ANTHROPIC_API_KEY".to_string(),
            var_value: "x".to_string(),
            source_type: "file".to_string(),
            source_path: "/etc/profile:1".to_string(),
        };
        let err = delete_single_env_in(&conflict, home.path()).expect_err("reject /etc");
        assert!(
            err.contains("拒绝") || err.contains("不允许") || err.contains("不存在"),
            "{err}"
        );

        let outside = tempfile::NamedTempFile::new().expect("outside");
        let conflict = EnvConflict {
            var_name: "ANTHROPIC_API_KEY".to_string(),
            var_value: "x".to_string(),
            source_type: "file".to_string(),
            source_path: format!("{}:1", outside.path().display()),
        };
        let err = delete_single_env_in(&conflict, home.path()).expect_err("reject outside");
        assert!(err.contains("拒绝") || err.contains("不允许"), "{err}");
    }
}
