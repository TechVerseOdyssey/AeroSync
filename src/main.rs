use aerosync_core::TransferConfig;
use aerosync_ui::AppState;

#[cfg(feature = "egui")]
use aerosync_ui::EguiApp;

#[cfg(not(feature = "egui"))]
use aerosync_ui::CliApp;

#[cfg(feature = "egui")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt::init();

    // Create default configuration
    let config = TransferConfig::default();
    
    // Create application state
    let app_state = AppState::new(config);
    
    // Run egui application
    EguiApp::run(app_state)?;

    Ok(())
}

#[cfg(not(feature = "egui"))]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt::init();

    // Create default configuration
    let config = TransferConfig::default();
    
    // Create application state
    let app_state = AppState::new(config);
    
    // Create and run CLI application
    let mut cli_app = CliApp::new(app_state);
    cli_app.run().await?;

    Ok(())
}