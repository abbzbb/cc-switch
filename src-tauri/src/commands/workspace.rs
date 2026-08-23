use regex::Regex;
use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt;

use crate::config::write_text_file;
use crate::error::AppError;
use crate::openclaw_config::get_openclaw_dir;

/// Allowed workspace filenames (whitelist for security)
const ALLOWED_FILES: &[&str] = &[
    "AGENTS.md",
    "SOUL.md",
    "USER.md",
    "IDENTITY.md",
    "TOOLS.md",
    "MEMORY.md",
    "HEARTBEAT.md",
    "BOOTSTRAP.md",
    "BOOT.md",
];

fn validate_filename(filename: &str) -> Result<(), AppError> {
    if !ALLOWED_FILES.contains(&filename) {
        return Err(AppError::from(format!(
            "Invalid workspace filename: {filename}. Allowed: {}",
            ALLOWED_FILES.join(", ")
        )));
    }
    Ok(())
}

// --- Daily memory files (memory/YYYY-MM-DD.md) ---

static DAILY_MEMORY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d{4}-\d{2}-\d{2}\.md$").unwrap());

const MAX_DAILY_MEMORY_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_DAILY_MEMORY_SCAN_BYTES: u64 = 32 * 1024 * 1024;
const MAX_DAILY_MEMORY_FILES: usize = 2_000;
const MAX_DAILY_MEMORY_DIRECTORY_ENTRIES: usize = 10_000;
const MAX_DAILY_MEMORY_RESULTS: usize = 200;
const MAX_DAILY_MEMORY_QUERY_BYTES: usize = 256;
const DAILY_MEMORY_PREVIEW_BYTES: u64 = 8 * 1024;

fn validate_daily_memory_filename(filename: &str) -> Result<(), AppError> {
    if !DAILY_MEMORY_RE.is_match(filename) {
        return Err(AppError::from(format!(
            "Invalid daily memory filename: {filename}. Expected: YYYY-MM-DD.md"
        )));
    }
    Ok(())
}

fn daily_memory_dir() -> PathBuf {
    get_openclaw_dir().join("workspace").join("memory")
}

fn is_link_or_reparse_point(meta: &Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    false
}

#[cfg(windows)]
fn stable_windows_directory_path(handle: &File) -> io::Result<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFinalPathNameByHandleW, FILE_NAME_NORMALIZED, FILE_NAME_OPENED, VOLUME_NAME_DOS,
    };

    let raw_handle = handle.as_raw_handle();
    for name_flag in [FILE_NAME_NORMALIZED, FILE_NAME_OPENED] {
        let mut buffer = vec![0_u16; 512];
        loop {
            // SAFETY: raw_handle belongs to `handle`; buffer is writable for the
            // provided length and remains alive for the call.
            let length = unsafe {
                GetFinalPathNameByHandleW(
                    raw_handle,
                    buffer.as_mut_ptr(),
                    buffer.len() as u32,
                    VOLUME_NAME_DOS | name_flag,
                )
            };
            if length == 0 {
                break;
            }
            let length = length as usize;
            if length < buffer.len() {
                buffer.truncate(length);
                return Ok(PathBuf::from(OsString::from_wide(&buffer)));
            }
            buffer.resize(length.saturating_add(1), 0);
        }
    }
    Err(io::Error::last_os_error())
}

struct OpenedMemoryDirectory {
    #[cfg(unix)]
    handle: File,
    #[cfg(not(unix))]
    _handle: File,
    #[cfg(not(unix))]
    path: PathBuf,
}

fn open_memory_directory(memory_dir: &Path) -> Result<Option<OpenedMemoryDirectory>, AppError> {
    let meta = match std::fs::symlink_metadata(memory_dir) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(AppError::from(format!(
                "Failed to inspect daily memory directory: {error}"
            )))
        }
    };
    if is_link_or_reparse_point(&meta) {
        return Err(AppError::from(
            "Daily memory directory must not be a symbolic link or reparse point",
        ));
    }
    if !meta.is_dir() {
        return Err(AppError::from("Daily memory path is not a directory"));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
            FILE_SHARE_WRITE,
        };
        options
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let handle = options.open(memory_dir).map_err(|error| {
        AppError::from(format!(
            "Failed to safely open daily memory directory: {error}"
        ))
    })?;
    let opened_meta = handle.metadata().map_err(|error| {
        AppError::from(format!(
            "Failed to inspect opened daily memory directory: {error}"
        ))
    })?;
    if is_link_or_reparse_point(&opened_meta) || !opened_meta.is_dir() {
        return Err(AppError::from("Daily memory path is not a safe directory"));
    }
    #[cfg(windows)]
    let stable_path = stable_windows_directory_path(&handle).map_err(|error| {
        AppError::from(format!(
            "Failed to resolve the opened daily memory directory: {error}"
        ))
    })?;
    Ok(Some(OpenedMemoryDirectory {
        #[cfg(unix)]
        handle,
        #[cfg(not(unix))]
        _handle: handle,
        #[cfg(not(unix))]
        path: {
            #[cfg(windows)]
            {
                stable_path
            }
            #[cfg(not(windows))]
            {
                memory_dir.to_path_buf()
            }
        },
    }))
}

fn open_memory_directory_for_write(memory_dir: &Path) -> Result<OpenedMemoryDirectory, AppError> {
    if let Some(directory) = open_memory_directory(memory_dir)? {
        return Ok(directory);
    }
    std::fs::create_dir_all(memory_dir)
        .map_err(|error| AppError::from(format!("Failed to create memory directory: {error}")))?;
    open_memory_directory(memory_dir)?.ok_or_else(|| {
        AppError::from("Daily memory directory disappeared immediately after creation")
    })
}

#[cfg(unix)]
fn filename_cstring(filename: &str) -> Result<std::ffi::CString, AppError> {
    std::ffi::CString::new(filename)
        .map_err(|_| AppError::from("Daily memory filename contains a NUL byte"))
}

fn open_nofollow_at(directory: &OpenedMemoryDirectory, filename: &str) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::fd::{AsRawFd, FromRawFd};
        let filename = std::ffi::CString::new(filename)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in filename"))?;
        // SAFETY: the directory fd and NUL-terminated filename remain valid for
        // the call. O_NOFOLLOW prevents a final-component symlink escape.
        let fd = unsafe {
            libc::openat(
                directory.handle.as_raw_fd(),
                filename.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openat returned a new owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    #[cfg(not(unix))]
    {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
            options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        }
        options.open(directory.path.join(filename))
    }
}

#[cfg(unix)]
struct DirectoryStream(*mut libc::DIR);

#[cfg(unix)]
impl Drop for DirectoryStream {
    fn drop(&mut self) {
        // SAFETY: fdopendir returned this DIR pointer and it is closed once here.
        unsafe {
            libc::closedir(self.0);
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn reset_readdir_errno() {
    // SAFETY: this writes the current thread's errno slot.
    unsafe { *libc::__errno_location() = 0 };
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn readdir_errno() -> i32 {
    // SAFETY: this reads the current thread's errno slot.
    unsafe { *libc::__errno_location() }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn reset_readdir_errno() {
    // SAFETY: this writes the current thread's errno slot.
    unsafe { *libc::__error() = 0 };
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn readdir_errno() -> i32 {
    // SAFETY: this reads the current thread's errno slot.
    unsafe { *libc::__error() }
}

#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    ))
))]
fn reset_readdir_errno() {}

#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    ))
))]
fn readdir_errno() -> i32 {
    0
}

#[cfg(unix)]
fn directory_entry_names(directory: &OpenedMemoryDirectory) -> io::Result<Vec<String>> {
    use std::ffi::CStr;
    use std::os::fd::AsRawFd;

    // SAFETY: dup creates an independent descriptor consumed by fdopendir.
    let duplicated = unsafe { libc::dup(directory.handle.as_raw_fd()) };
    if duplicated < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: duplicated is an owned directory descriptor.
    let stream = unsafe { libc::fdopendir(duplicated) };
    if stream.is_null() {
        // SAFETY: fdopendir did not consume the descriptor on failure.
        unsafe { libc::close(duplicated) };
        return Err(io::Error::last_os_error());
    }
    let stream = DirectoryStream(stream);
    let mut names = Vec::new();
    loop {
        reset_readdir_errno();
        // SAFETY: stream remains valid for the loop and readdir's returned entry
        // is consumed before the next call.
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let errno = readdir_errno();
            if errno != 0 {
                return Err(io::Error::from_raw_os_error(errno));
            }
            break;
        }
        // SAFETY: POSIX guarantees d_name is NUL-terminated for this entry.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        names.push(name.to_string_lossy().into_owned());
        if names.len() > MAX_DAILY_MEMORY_DIRECTORY_ENTRIES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "daily memory directory exceeds the {MAX_DAILY_MEMORY_DIRECTORY_ENTRIES} entry limit"
                ),
            ));
        }
    }
    Ok(names)
}

#[cfg(not(unix))]
fn directory_entry_names(directory: &OpenedMemoryDirectory) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&directory.path)? {
        names.push(entry?.file_name().to_string_lossy().into_owned());
        if names.len() > MAX_DAILY_MEMORY_DIRECTORY_ENTRIES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "daily memory directory exceeds the {MAX_DAILY_MEMORY_DIRECTORY_ENTRIES} entry limit"
                ),
            ));
        }
    }
    Ok(names)
}

#[cfg(unix)]
fn atomic_write_memory_file(
    directory: &OpenedMemoryDirectory,
    filename: &str,
    content: &str,
) -> Result<(), AppError> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
    let directory_fd = directory.handle.as_raw_fd();
    let target = filename_cstring(filename)?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut last_error = None;
    for _ in 0..16 {
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temporary_name = format!(
            ".{filename}.tmp.{}.{}.{}",
            std::process::id(),
            nonce,
            counter
        );
        let temporary = filename_cstring(&temporary_name)?;
        // SAFETY: directory_fd and both C strings are valid. O_EXCL prevents
        // clobbering an attacker-created temporary entry.
        let fd = unsafe {
            libc::openat(
                directory_fd,
                temporary.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o666,
            )
        };
        if fd < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::AlreadyExists {
                last_error = Some(error);
                continue;
            }
            return Err(AppError::from(format!(
                "Failed to create daily memory temporary file: {error}"
            )));
        }
        // SAFETY: openat returned a new owned descriptor.
        let mut file = unsafe { File::from_raw_fd(fd) };
        if let Err(error) = file
            .write_all(content.as_bytes())
            .and_then(|_| file.flush())
        {
            drop(file);
            // SAFETY: temporary names a child of the held directory.
            unsafe { libc::unlinkat(directory_fd, temporary.as_ptr(), 0) };
            return Err(AppError::from(format!(
                "Failed to write daily memory file {filename}: {error}"
            )));
        }
        drop(file);
        // SAFETY: renameat operates on entries relative to the same held
        // directory, so replacing the directory path cannot redirect the write.
        let renamed = unsafe {
            libc::renameat(
                directory_fd,
                temporary.as_ptr(),
                directory_fd,
                target.as_ptr(),
            )
        };
        if renamed == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        // SAFETY: best-effort cleanup of our own temporary entry.
        unsafe { libc::unlinkat(directory_fd, temporary.as_ptr(), 0) };
        return Err(AppError::from(format!(
            "Failed to atomically replace daily memory file {filename}: {error}"
        )));
    }
    Err(AppError::from(format!(
        "Failed to allocate a daily memory temporary file: {}",
        last_error.unwrap_or_else(|| io::Error::from(io::ErrorKind::AlreadyExists))
    )))
}

#[cfg(windows)]
fn atomic_write_memory_file(
    directory: &OpenedMemoryDirectory,
    filename: &str,
    content: &str,
) -> Result<(), AppError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let target = directory.path.join(filename);
    let temporary = directory
        .path
        .join(format!(".{filename}.tmp.{}", uuid::Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| {
            AppError::from(format!(
                "Failed to create daily memory temporary file: {error}"
            ))
        })?;
    if let Err(error) = file
        .write_all(content.as_bytes())
        .and_then(|_| file.flush())
    {
        drop(file);
        let _ = std::fs::remove_file(&temporary);
        return Err(AppError::from(format!(
            "Failed to write daily memory file {filename}: {error}"
        )));
    }
    drop(file);

    let temporary_wide = temporary
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let target_wide = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // MoveFileExW renames the directory entry itself (including when the old
    // target is a reparse point); it does not open and overwrite the link target.
    // SAFETY: both UTF-16 paths are NUL-terminated and live for the call.
    let moved = unsafe {
        MoveFileExW(
            temporary_wide.as_ptr(),
            target_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        let error = io::Error::last_os_error();
        let _ = std::fs::remove_file(&temporary);
        return Err(AppError::from(format!(
            "Failed to atomically replace daily memory file {filename}: {error}"
        )));
    }
    Ok(())
}

#[cfg(all(not(unix), not(windows)))]
fn atomic_write_memory_file(
    directory: &OpenedMemoryDirectory,
    filename: &str,
    content: &str,
) -> Result<(), AppError> {
    write_text_file(&directory.path.join(filename), content)
        .map_err(|error| AppError::from(format!("Failed to write daily memory file: {error}")))
}

fn delete_memory_file(directory: &OpenedMemoryDirectory, filename: &str) -> Result<(), AppError> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let filename_c = filename_cstring(filename)?;
        // SAFETY: unlinkat removes only the named directory entry and never
        // follows a final symlink to an outside file.
        let result =
            unsafe { libc::unlinkat(directory.handle.as_raw_fd(), filename_c.as_ptr(), 0) };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            return Ok(());
        }
        Err(AppError::from(format!(
            "Failed to delete daily memory file {filename}: {error}"
        )))
    }

    #[cfg(not(unix))]
    {
        let path = directory.path.join(filename);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(AppError::from(format!(
                "Failed to delete daily memory file {filename}: {error}"
            ))),
        }
    }
}

struct OpenedDailyMemoryFile {
    file: File,
    metadata: Metadata,
}

fn open_daily_memory_file(
    memory_dir: &OpenedMemoryDirectory,
    filename: &str,
) -> Result<Option<OpenedDailyMemoryFile>, AppError> {
    validate_daily_memory_filename(filename)?;
    let file = match open_nofollow_at(memory_dir, filename) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            #[cfg(unix)]
            if error.raw_os_error() == Some(libc::ELOOP) {
                return Err(AppError::from(format!(
                    "Daily memory file {filename} must not be a symbolic link or reparse point"
                )));
            }
            return Err(AppError::from(format!(
                "Failed to safely open daily memory file {filename}: {error}"
            )));
        }
    };
    let metadata = file.metadata().map_err(|error| {
        AppError::from(format!(
            "Failed to inspect opened daily memory file {filename}: {error}"
        ))
    })?;
    if is_link_or_reparse_point(&metadata) || !metadata.is_file() {
        return Err(AppError::from(format!(
            "Daily memory file {filename} is not a regular file"
        )));
    }
    Ok(Some(OpenedDailyMemoryFile { file, metadata }))
}

fn daily_memory_entries(memory_dir: &OpenedMemoryDirectory) -> Result<Vec<String>, AppError> {
    let entries = directory_entry_names(memory_dir)
        .map_err(|e| AppError::from(format!("Failed to read memory directory: {e}")))?;
    let mut files = Vec::new();
    for name in entries {
        if validate_daily_memory_filename(&name).is_err() {
            continue;
        }
        files.push(name);
    }
    files.sort_by(|a, b| b.cmp(a));
    files.truncate(MAX_DAILY_MEMORY_FILES);
    Ok(files)
}

fn read_up_to(file: &mut File, byte_cap: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(byte_cap).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn ensure_file_size(filename: &str, metadata: &Metadata) -> Result<(), AppError> {
    if metadata.len() > MAX_DAILY_MEMORY_FILE_BYTES {
        return Err(AppError::from(format!(
            "Daily memory file {filename} exceeds the {} byte limit",
            MAX_DAILY_MEMORY_FILE_BYTES
        )));
    }
    Ok(())
}

fn bytes_to_text(filename: &str, bytes: Vec<u8>) -> Result<String, AppError> {
    String::from_utf8(bytes).map_err(|error| {
        AppError::from(format!(
            "Daily memory file {filename} is not valid UTF-8: {error}"
        ))
    })
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyMemoryFileInfo {
    pub filename: String,
    pub date: String,
    pub size_bytes: u64,
    pub modified_at: u64,
    pub preview: String,
}

// --- Daily memory commands ---

/// List all daily memory files under `workspace/memory/`.
#[tauri::command]
pub async fn list_daily_memory_files() -> Result<Vec<DailyMemoryFileInfo>, AppError> {
    tauri::async_runtime::spawn_blocking(|| list_daily_memory_files_in(&daily_memory_dir()))
        .await
        .map_err(|e| AppError::from(format!("Failed to list daily memory files: {e}")))?
}

fn list_daily_memory_files_in(memory_dir: &Path) -> Result<Vec<DailyMemoryFileInfo>, AppError> {
    let Some(memory_dir) = open_memory_directory(memory_dir)? else {
        return Ok(Vec::new());
    };
    let mut files = Vec::new();
    for name in daily_memory_entries(&memory_dir)? {
        let Some(mut opened) = open_daily_memory_file(&memory_dir, &name)? else {
            continue;
        };
        ensure_file_size(&name, &opened.metadata)?;
        let mut preview_bytes = read_up_to(
            &mut opened.file,
            DAILY_MEMORY_PREVIEW_BYTES.saturating_add(1),
        )
        .map_err(|error| {
            AppError::from(format!("Failed to read daily memory file {name}: {error}"))
        })?;
        preview_bytes.truncate(DAILY_MEMORY_PREVIEW_BYTES as usize);
        let preview = String::from_utf8_lossy(&preview_bytes)
            .chars()
            .take(200)
            .collect();
        files.push(DailyMemoryFileInfo {
            date: name.trim_end_matches(".md").to_string(),
            filename: name,
            size_bytes: opened.metadata.len(),
            modified_at: modified_at_secs(&opened.metadata),
            preview,
        });
    }
    Ok(files)
}

/// Read a daily memory file.
#[tauri::command]
pub async fn read_daily_memory_file(filename: String) -> Result<Option<String>, AppError> {
    validate_daily_memory_filename(&filename)?;
    tauri::async_runtime::spawn_blocking(move || {
        read_daily_memory_file_from(&daily_memory_dir(), &filename)
    })
    .await
    .map_err(|e| AppError::from(format!("Failed to read daily memory file: {e}")))?
}

fn read_daily_memory_file_from(
    memory_dir: &Path,
    filename: &str,
) -> Result<Option<String>, AppError> {
    let Some(memory_dir) = open_memory_directory(memory_dir)? else {
        return Ok(None);
    };
    let Some(mut opened) = open_daily_memory_file(&memory_dir, filename)? else {
        return Ok(None);
    };
    ensure_file_size(filename, &opened.metadata)?;
    let bytes = read_up_to(
        &mut opened.file,
        MAX_DAILY_MEMORY_FILE_BYTES.saturating_add(1),
    )
    .map_err(|error| {
        AppError::from(format!(
            "Failed to read daily memory file {filename}: {error}"
        ))
    })?;
    if bytes.len() as u64 > MAX_DAILY_MEMORY_FILE_BYTES {
        return Err(AppError::from(format!(
            "Daily memory file {filename} grew beyond the {} byte limit while being read",
            MAX_DAILY_MEMORY_FILE_BYTES
        )));
    }
    bytes_to_text(filename, bytes).map(Some)
}

/// Write a daily memory file (atomic write).
#[tauri::command]
pub async fn write_daily_memory_file(filename: String, content: String) -> Result<(), AppError> {
    validate_daily_memory_filename(&filename)?;
    if content.len() as u64 > MAX_DAILY_MEMORY_FILE_BYTES {
        return Err(AppError::from(format!(
            "Daily memory file {filename} exceeds the {} byte limit",
            MAX_DAILY_MEMORY_FILE_BYTES
        )));
    }
    tauri::async_runtime::spawn_blocking(move || {
        write_daily_memory_file_to(&daily_memory_dir(), &filename, &content)
    })
    .await
    .map_err(|e| AppError::from(format!("Failed to write daily memory file: {e}")))?
}

fn write_daily_memory_file_to(
    memory_dir: &Path,
    filename: &str,
    content: &str,
) -> Result<(), AppError> {
    let memory_dir = open_memory_directory_for_write(memory_dir)?;
    atomic_write_memory_file(&memory_dir, filename, content)
}

fn modified_at_secs(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Find the largest index `<= i` that is a valid UTF-8 char boundary.
/// Equivalent to the unstable `str::floor_char_boundary` (stabilized in 1.91).
fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Find the smallest index `>= i` that is a valid UTF-8 char boundary.
/// Equivalent to the unstable `str::ceil_char_boundary` (stabilized in 1.91).
fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Search result for daily memory full-text search.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyMemorySearchResult {
    pub filename: String,
    pub date: String,
    pub size_bytes: u64,
    pub modified_at: u64,
    pub snippet: String,
    pub match_count: usize,
}

/// Full-text search across all daily memory files.
///
/// Performs case-insensitive search on both the date field and file content.
/// Returns results sorted by filename descending (newest first), each with a
/// snippet showing ~120 characters of context around the first match.
#[tauri::command]
pub async fn search_daily_memory_files(
    query: String,
) -> Result<Vec<DailyMemorySearchResult>, AppError> {
    if query.is_empty() {
        return Ok(Vec::new());
    }
    if query.len() > MAX_DAILY_MEMORY_QUERY_BYTES {
        return Err(AppError::from(format!(
            "Daily memory search query exceeds the {} byte limit",
            MAX_DAILY_MEMORY_QUERY_BYTES
        )));
    }

    tauri::async_runtime::spawn_blocking(move || {
        search_daily_memory_files_in(&daily_memory_dir(), &query)
    })
    .await
    .map_err(|e| AppError::from(format!("Failed to search daily memory files: {e}")))?
}

fn search_daily_memory_files_in(
    memory_dir: &Path,
    query: &str,
) -> Result<Vec<DailyMemorySearchResult>, AppError> {
    let query_pattern = regex::RegexBuilder::new(&regex::escape(query))
        .case_insensitive(true)
        .build()
        .map_err(|e| AppError::from(format!("Invalid daily memory search query: {e}")))?;
    let mut results = Vec::new();
    let mut scanned_bytes = 0_u64;
    let Some(memory_dir) = open_memory_directory(memory_dir)? else {
        return Ok(results);
    };
    for name in daily_memory_entries(&memory_dir)? {
        if scanned_bytes >= MAX_DAILY_MEMORY_SCAN_BYTES {
            break;
        }
        let Some(mut opened) = open_daily_memory_file(&memory_dir, &name)? else {
            continue;
        };
        ensure_file_size(&name, &opened.metadata)?;

        let remaining = MAX_DAILY_MEMORY_SCAN_BYTES - scanned_bytes;
        let byte_cap = MAX_DAILY_MEMORY_FILE_BYTES.saturating_add(1).min(remaining);
        let bytes = read_up_to(&mut opened.file, byte_cap).map_err(|error| {
            AppError::from(format!(
                "Failed to read daily memory file {name} while searching: {error}"
            ))
        })?;
        let actual_read = bytes.len() as u64;
        scanned_bytes += actual_read;

        if actual_read > MAX_DAILY_MEMORY_FILE_BYTES {
            return Err(AppError::from(format!(
                "Daily memory file {name} grew beyond the {} byte limit while being searched",
                MAX_DAILY_MEMORY_FILE_BYTES
            )));
        }

        let content = bytes_to_text(&name, bytes)?;
        let date = name.trim_end_matches(".md").to_string();

        let mut content_matches = query_pattern.find_iter(&content);
        let first_content_match = content_matches.next().map(|matched| matched.start());
        let match_count = first_content_match
            .map(|_| 1 + content_matches.count())
            .unwrap_or(0);

        // Also check date field
        let date_matches = query_pattern.is_match(&date);

        if first_content_match.is_none() && !date_matches {
            continue;
        }

        // Build snippet around first content match (~120 characters of context)
        let snippet = if let Some(first_pos) = first_content_match {
            let start = if first_pos > 50 {
                floor_char_boundary(&content, first_pos - 50)
            } else {
                0
            };
            let end = ceil_char_boundary(&content, (first_pos + 70).min(content.len()));
            let mut snippet = String::new();
            if start > 0 {
                snippet.push_str("...");
            }
            snippet.push_str(&content[start..end]);
            if end < content.len() {
                snippet.push_str("...");
            }
            snippet
        } else {
            let end = ceil_char_boundary(&content, 120.min(content.len()));
            let mut snippet = content[..end].to_string();
            if end < content.len() {
                snippet.push_str("...");
            }
            snippet
        };

        results.push(DailyMemorySearchResult {
            filename: name,
            date,
            size_bytes: opened.metadata.len(),
            modified_at: modified_at_secs(&opened.metadata),
            snippet,
            match_count,
        });
        if results.len() >= MAX_DAILY_MEMORY_RESULTS {
            break;
        }
    }
    Ok(results)
}

/// Delete a daily memory file (idempotent).
#[tauri::command]
pub async fn delete_daily_memory_file(filename: String) -> Result<(), AppError> {
    validate_daily_memory_filename(&filename)?;
    tauri::async_runtime::spawn_blocking(move || {
        delete_daily_memory_file_from(&daily_memory_dir(), &filename)
    })
    .await
    .map_err(|e| AppError::from(format!("Failed to delete daily memory file: {e}")))?
}

fn delete_daily_memory_file_from(memory_dir: &Path, filename: &str) -> Result<(), AppError> {
    let Some(memory_dir) = open_memory_directory(memory_dir)? else {
        return Ok(());
    };
    delete_memory_file(&memory_dir, filename)
}

// --- Workspace file commands ---

/// Read an OpenClaw workspace file content.
/// Returns None if the file does not exist.
#[tauri::command]
pub async fn read_workspace_file(filename: String) -> Result<Option<String>, AppError> {
    validate_filename(&filename)?;

    let path = get_openclaw_dir().join("workspace").join(&filename);

    if !path.exists() {
        return Ok(None);
    }

    std::fs::read_to_string(&path)
        .map(Some)
        .map_err(|e| AppError::from(format!("Failed to read workspace file {filename}: {e}")))
}

/// Write content to an OpenClaw workspace file (atomic write).
/// Creates the workspace directory if it does not exist.
#[tauri::command]
pub async fn write_workspace_file(filename: String, content: String) -> Result<(), AppError> {
    validate_filename(&filename)?;

    let workspace_dir = get_openclaw_dir().join("workspace");

    // Ensure workspace directory exists
    std::fs::create_dir_all(&workspace_dir)
        .map_err(|e| AppError::from(format!("Failed to create workspace directory: {e}")))?;

    let path = workspace_dir.join(&filename);

    write_text_file(&path, &content)
        .map_err(|e| AppError::from(format!("Failed to write workspace file {filename}: {e}")))
}

/// Open the workspace or memory directory in the system file manager.
/// `subdir`: "workspace" opens `~/.openclaw/workspace/`,
///           "memory" opens `~/.openclaw/workspace/memory/`.
#[tauri::command]
pub async fn open_workspace_directory(handle: AppHandle, subdir: String) -> Result<bool, AppError> {
    let dir = match subdir.as_str() {
        "memory" => get_openclaw_dir().join("workspace").join("memory"),
        _ => get_openclaw_dir().join("workspace"),
    };

    if !dir.exists() {
        std::fs::create_dir_all(&dir)
            .map_err(|e| AppError::from(format!("Failed to create directory: {e}")))?;
    }

    handle
        .opener()
        .open_path(dir.to_string_lossy().to_string(), None::<String>)
        .map_err(|e| AppError::from(format!("Failed to open directory: {e}")))?;

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_memory_filename_validation_is_exact() {
        assert!(validate_daily_memory_filename("2026-08-23.md").is_ok());
        for invalid in [
            "notes.md",
            "2026-8-23.md",
            "2026-08-23.md.bak",
            "../2026-08-23.md",
        ] {
            assert!(validate_daily_memory_filename(invalid).is_err());
        }
    }

    #[test]
    fn daily_memory_entries_use_the_same_filename_validation() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("2026-08-23.md"), "valid").unwrap();
        std::fs::write(temp.path().join("notes.md"), "not daily memory").unwrap();

        let directory = open_memory_directory(temp.path()).unwrap().unwrap();
        let entries = daily_memory_entries(&directory).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0], "2026-08-23.md");
    }

    #[cfg(unix)]
    #[test]
    fn daily_memory_symlink_escape_is_rejected_by_list_read_and_search() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let memory_dir = temp.path().join("memory");
        std::fs::create_dir(&memory_dir).unwrap();
        let outside = temp.path().join("outside.md");
        std::fs::write(&outside, "outside secret").unwrap();
        symlink(&outside, memory_dir.join("2026-08-23.md")).unwrap();

        for error in [
            list_daily_memory_files_in(&memory_dir).unwrap_err(),
            read_daily_memory_file_from(&memory_dir, "2026-08-23.md").unwrap_err(),
            search_daily_memory_files_in(&memory_dir, "secret").unwrap_err(),
        ] {
            assert!(format!("{error:?}").contains("symbolic link or reparse point"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn daily_memory_directory_symlink_is_rejected_for_every_operation() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let outside_file = outside.join("2026-08-23.md");
        std::fs::write(&outside_file, "outside secret").unwrap();
        let memory_dir = temp.path().join("memory");
        symlink(&outside, &memory_dir).unwrap();

        for error in [
            list_daily_memory_files_in(&memory_dir).unwrap_err(),
            read_daily_memory_file_from(&memory_dir, "2026-08-23.md").unwrap_err(),
            search_daily_memory_files_in(&memory_dir, "secret").unwrap_err(),
            write_daily_memory_file_to(&memory_dir, "2026-08-23.md", "overwrite").unwrap_err(),
            delete_daily_memory_file_from(&memory_dir, "2026-08-23.md").unwrap_err(),
        ] {
            assert!(format!("{error:?}").contains("symbolic link or reparse point"));
        }
        assert_eq!(
            std::fs::read_to_string(outside_file).unwrap(),
            "outside secret"
        );
    }

    #[cfg(unix)]
    #[test]
    fn opened_directory_handle_survives_path_replacement_without_escaping() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let memory_dir = temp.path().join("memory");
        let original_dir = temp.path().join("original-memory");
        let outside_dir = temp.path().join("outside");
        std::fs::create_dir(&memory_dir).unwrap();
        std::fs::create_dir(&outside_dir).unwrap();
        std::fs::write(memory_dir.join("2026-08-23.md"), "inside").unwrap();
        std::fs::write(outside_dir.join("2026-08-23.md"), "outside").unwrap();

        let directory = open_memory_directory(&memory_dir).unwrap().unwrap();
        std::fs::rename(&memory_dir, &original_dir).unwrap();
        symlink(&outside_dir, &memory_dir).unwrap();

        let mut opened = open_daily_memory_file(&directory, "2026-08-23.md")
            .unwrap()
            .unwrap();
        let mut content = String::new();
        opened.file.read_to_string(&mut content).unwrap();
        assert_eq!(content, "inside");

        atomic_write_memory_file(&directory, "2026-08-23.md", "updated").unwrap();
        assert_eq!(
            std::fs::read_to_string(original_dir.join("2026-08-23.md")).unwrap(),
            "updated"
        );
        assert_eq!(
            std::fs::read_to_string(outside_dir.join("2026-08-23.md")).unwrap(),
            "outside"
        );

        delete_memory_file(&directory, "2026-08-23.md").unwrap();
        assert!(!original_dir.join("2026-08-23.md").exists());
        assert!(outside_dir.join("2026-08-23.md").exists());
    }

    #[test]
    fn over_limit_file_is_rejected_consistently() {
        let temp = tempfile::tempdir().unwrap();
        let filename = "2026-08-23.md";
        let path = temp.path().join(filename);
        let file = File::create(&path).unwrap();
        file.set_len(MAX_DAILY_MEMORY_FILE_BYTES + 1).unwrap();

        for error in [
            list_daily_memory_files_in(temp.path()).unwrap_err(),
            read_daily_memory_file_from(temp.path(), filename).unwrap_err(),
            search_daily_memory_files_in(temp.path(), "2026").unwrap_err(),
        ] {
            assert!(format!("{error:?}").contains("exceeds the"));
        }
    }

    #[test]
    fn opened_file_growth_is_detected_with_a_limit_plus_one_read() {
        let temp = tempfile::tempdir().unwrap();
        let filename = "2026-08-23.md";
        let path = temp.path().join(filename);
        std::fs::write(&path, b"small").unwrap();

        let directory = open_memory_directory(temp.path()).unwrap().unwrap();
        let mut opened = open_daily_memory_file(&directory, filename)
            .unwrap()
            .unwrap();
        assert!(ensure_file_size(filename, &opened.metadata).is_ok());

        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_DAILY_MEMORY_FILE_BYTES + 100)
            .unwrap();
        let bytes = read_up_to(
            &mut opened.file,
            MAX_DAILY_MEMORY_FILE_BYTES.saturating_add(1),
        )
        .unwrap();

        assert_eq!(bytes.len() as u64, MAX_DAILY_MEMORY_FILE_BYTES + 1);
    }

    #[test]
    fn search_stops_at_the_actual_read_budget() {
        let temp = tempfile::tempdir().unwrap();
        for day in 1..=17 {
            let path = temp.path().join(format!("2026-08-{day:02}.md"));
            File::create(path)
                .unwrap()
                .set_len(MAX_DAILY_MEMORY_FILE_BYTES)
                .unwrap();
        }

        let results = search_daily_memory_files_in(temp.path(), "2026").unwrap();

        assert_eq!(results.len(), 16);
        assert_eq!(
            results.iter().map(|result| result.size_bytes).sum::<u64>(),
            MAX_DAILY_MEMORY_SCAN_BYTES
        );
    }
}
