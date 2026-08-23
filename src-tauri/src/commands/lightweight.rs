use crate::error::AppError;
#[tauri::command]
pub fn enter_lightweight_mode(app: tauri::AppHandle) -> Result<(), AppError> {
    crate::lightweight::enter_lightweight_mode(&app).map_err(AppError::from)
}

#[tauri::command]
pub fn exit_lightweight_mode(app: tauri::AppHandle) -> Result<(), AppError> {
    crate::lightweight::exit_lightweight_mode(&app).map_err(AppError::from)
}

#[tauri::command]
pub fn is_lightweight_mode() -> bool {
    crate::lightweight::is_lightweight_mode()
}
