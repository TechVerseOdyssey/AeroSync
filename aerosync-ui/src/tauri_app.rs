#[cfg(feature = "tauri")]
use crate::AppState;

#[cfg(feature = "tauri")]
use aerosync_core::transfer::TransferTask;

#[cfg(feature = "tauri")]
use std::path::PathBuf;

#[cfg(feature = "tauri")]
use uuid::Uuid;

// Tauri commands for the frontend to call
#[cfg(feature = "tauri")]
#[tauri::command]
async fn select_files() -> Result<Vec<String>, String> {
    use rfd::AsyncFileDialog;

    let files = AsyncFileDialog::new()
        .set_title("Select Files to Transfer")
        .pick_files()
        .await;

    match files {
        Some(handles) => Ok(handles
            .into_iter()
            .map(|h| h.path().to_string_lossy().to_string())
            .collect()),
        None => Ok(vec![]),
    }
}

#[cfg(feature = "tauri")]
#[tauri::command]
async fn start_transfer(
    state: tauri::State<'_, AppState>,
    files: Vec<String>,
    destination: String,
) -> Result<Vec<String>, String> {
    let mut task_ids = Vec::new();

    for file_path in files {
        let path = PathBuf::from(&file_path);
        let file_size = tokio::fs::metadata(&path)
            .await
            .map_err(|e| format!("Failed to read file metadata for {file_path}: {e}"))?
            .len();

        let task = TransferTask::new_upload(path, destination.clone(), file_size);
        let task_id = task.id.to_string();

        state
            .transfer_engine
            .add_transfer(task)
            .await
            .map_err(|e| format!("Failed to queue transfer for {file_path}: {e}"))?;

        task_ids.push(task_id);
    }

    Ok(task_ids)
}

#[cfg(feature = "tauri")]
#[tauri::command]
async fn get_transfer_progress(
    state: tauri::State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let monitor_lock = state.transfer_engine.get_progress_monitor().await;
    let monitor = monitor_lock.read().await;

    let active = monitor
        .get_active_transfers()
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();

    let stats = monitor.get_stats();

    Ok(serde_json::json!({
        "active_transfers": active,
        "stats": stats,
    }))
}

#[cfg(feature = "tauri")]
#[tauri::command]
async fn cancel_transfer(
    state: tauri::State<'_, AppState>,
    task_id: String,
) -> Result<(), String> {
    let uuid = Uuid::parse_str(&task_id)
        .map_err(|e| format!("Invalid task_id '{task_id}': {e}"))?;

    state
        .transfer_engine
        .cancel_transfer(uuid)
        .await
        .map_err(|e| format!("Failed to cancel transfer {task_id}: {e}"))
}

#[cfg(feature = "tauri")]
pub fn create_tauri_app(app_state: AppState) -> tauri::Builder<tauri::Wry> {
    tauri::Builder::default()
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            select_files,
            start_transfer,
            get_transfer_progress,
            cancel_transfer
        ])
}

#[cfg(feature = "tauri")]
pub fn run_tauri_app(app_state: AppState) -> Result<(), Box<dyn std::error::Error>> {
    create_tauri_app(app_state)
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
    Ok(())
}
