#[cfg(feature = "tauri")]
use crate::AppState;

// Tauri commands for the frontend to call
#[cfg(feature = "tauri")]
#[tauri::command]
async fn select_files() -> Result<Vec<String>, String> {
    // TODO: Implement file selection dialog
    Ok(vec![])
}

#[cfg(feature = "tauri")]
#[tauri::command]
async fn start_transfer(files: Vec<String>, destination: String) -> Result<(), String> {
    // TODO: Implement transfer start
    Ok(())
}

#[cfg(feature = "tauri")]
#[tauri::command]
async fn get_transfer_progress() -> Result<serde_json::Value, String> {
    // TODO: Return current transfer progress
    Ok(serde_json::json!({}))
}

#[cfg(feature = "tauri")]
#[tauri::command]
async fn cancel_transfer(task_id: String) -> Result<(), String> {
    // TODO: Implement transfer cancellation
    Ok(())
}

#[cfg(feature = "tauri")]
pub fn create_tauri_app() -> tauri::Builder<tauri::Wry> {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            select_files,
            start_transfer,
            get_transfer_progress,
            cancel_transfer
        ])
}

// Example of how to run the Tauri app
#[cfg(feature = "tauri")]
pub fn run_tauri_app() -> Result<(), Box<dyn std::error::Error>> {
    create_tauri_app()
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
    Ok(())
}