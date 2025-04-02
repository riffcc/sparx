use axum::{routing::{get}, extract::Extension, Router, response::{IntoResponse}, http::StatusCode};
use axum_login::{AuthManagerLayerBuilder};
use tower_sessions::{SessionManagerLayer};
use tower_sessions_sqlx_store::SqliteStore;
use std::sync::{Arc};
use tokio::sync::Mutex;
use tower_http::trace::TraceLayer;
use tracing::{info, error, warn, debug};
use std::net::SocketAddr;
use tower_cookies::CookieManagerLayer;
use tower_http::services::ServeDir;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::watch;
use anyhow::Context;
use listenfd::ListenFd;

use crate::auth::{AdminBackend, auth_router, load_credentials, generate_default_credentials, load_settings, Settings, Credentials};
use crate::db::init_db;
use crate::event_manager::EventManager;

// Add MiniJinja imports
use minijinja::path_loader;
use minijinja::{Environment};
use minijinja_autoreload::AutoReloader;

// Add Serialize for the enum
use serde::Serialize;
// Add back AtomicBool and Ordering imports
use std::sync::atomic::{AtomicBool, Ordering};

// Add back necessary tracing_subscriber imports
use tracing_subscriber::{fmt, layer::SubscriberExt, EnvFilter};

// Ensure prelude is still imported if needed elsewhere
// use tracing_subscriber::prelude::*;

mod auth;
mod api;
mod db;
mod filters; // Uncomment unused module
pub mod ui;
pub mod tinkerbell;
pub mod event_manager;
pub mod os_templates;
pub mod mode;

// Expose status module for integration tests
pub mod status;

// Add tokio::fs for directory check
use tokio::fs as async_fs;

// Global static for accessing event manager from other modules
use std::sync::RwLock;
use once_cell::sync::Lazy;
pub static EVENT_MANAGER_REF: Lazy<RwLock<Option<std::sync::Arc<EventManager>>>> = Lazy::new(|| {
    RwLock::new(None)
});

// Global static for installation state (used ONLY during install process itself)
pub static INSTALL_STATE_REF: Lazy<RwLock<Option<Arc<Mutex<InstallationState>>>>> = Lazy::new(|| {
    RwLock::new(None)
});

// Stub function to check installation status (Replace with real check later)
// Checks environment variable DRAGONFLY_FORCE_INSTALLED=true for testing
// Also checks for /var/lib/dragonfly and dragonfly StatefulSet status
pub async fn is_dragonfly_installed() -> bool {
    // 1. Check environment variable override first
    if std::env::var("DRAGONFLY_FORCE_INSTALLED").map_or(false, |val| val.to_lowercase() == "true") {
        info!("Installation status forced to TRUE via DRAGONFLY_FORCE_INSTALLED");
        return true;
    }

    // 2. Check for the existence of the directory
    let dir_path = "/var/lib/dragonfly";
    let dir_exists = match async_fs::metadata(dir_path).await {
        Ok(metadata) => metadata.is_dir(),
        Err(e) => {
            // Log specific error only if it's NOT NotFound
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!("Installation check: Error checking directory {}: {}", dir_path, e);
            }
            false
        },
    };

    if !dir_exists {
        debug!("Installation check: Directory '{}' not found.", dir_path);
        return false;
    }
    debug!("Installation check: Directory '{}' found.", dir_path);

    // 3. Check Kubernetes StatefulSet status
    let statefulset_ready = match status::check_dragonfly_statefulset_status().await {
        Ok(ready) => ready,
        Err(e) => {
            // Only log a warning here, as inability to check k8s might be temporary or expected
            // if k3s isn't fully up yet during startup.
            warn!("Installation check: Error checking Dragonfly StatefulSet status: {}. Assuming not installed.", e);
            false // If we can't check k8s, assume not installed for safety
        }
    };

    if !statefulset_ready {
        debug!("Installation check: Dragonfly StatefulSet not found or not ready.");
        return false;
    }
    debug!("Installation check: Dragonfly StatefulSet found and ready.");

    // If both directory exists and StatefulSet is ready, mark as installed
    info!("Installation check: Detected installed state (directory exists and StatefulSet ready).");
    true
}

// Enum to hold either static or reloading environment
#[derive(Clone)]
pub enum TemplateEnv {
    Static(Arc<Environment<'static>>),
    #[cfg(debug_assertions)]
    Reloading(Arc<AutoReloader>),
}

// Define the InstallationState enum here or import it
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum InstallationState {
    WaitingSudo,
    DetectingNetwork,
    InstallingK3s,
    WaitingK3s,
    DeployingTinkerbell,
    DeployingDragonfly,
    Ready,
    Failed(String), // Add Failed variant with error message
}

impl InstallationState {
    pub fn get_message(&self) -> &str {
        match self {
            // Phase 1
            InstallationState::WaitingSudo => "Dragonfly is ready to install. Enter your password in your install window — let's do this.",
            // Phase (Implied, added previously)
            InstallationState::DetectingNetwork => "Dragonfly is detecting network configuration...",
            // Phase 2
            InstallationState::InstallingK3s => "Dragonfly is installing k3s.",
            // Phase 3
            InstallationState::WaitingK3s => "Dragonfly is waiting for k3s to be ready.",
            // Phase 4
            InstallationState::DeployingTinkerbell => "Dragonfly is deploying Tinkerbell.",
            // Phase 5
            InstallationState::DeployingDragonfly => "Dragonfly is deploying... Dragonfly.",
            // Phase 6
            InstallationState::Ready => "Dragonfly is ready.",
            // Error
            InstallationState::Failed(_) => "Installation failed. Check installer logs for details.",
        }
    }
    pub fn get_animation_class(&self) -> &str {
        match self {
            // Phase 1 (Waiting) -> Idle (no specific animation)
            InstallationState::WaitingSudo => "rocket-idle",
            // Phase (Implied, added previously) -> Scanning (pulse/glow)
            InstallationState::DetectingNetwork => "rocket-scanning",
            // Phase 2 (Installing K3s) -> Sparks
            InstallationState::InstallingK3s => "rocket-sparks",
            // Phase 3 (Waiting K3s) -> Glowing
            InstallationState::WaitingK3s => "rocket-glowing",
            // Phase 4 (Deploying Tinkerbell) -> Smoke
            InstallationState::DeployingTinkerbell => "rocket-smoke",
            // Phase 5 (Deploying Dragonfly) -> Flicker
            InstallationState::DeployingDragonfly => "rocket-flicker",
            // Phase 6 (Ready) -> Fire + Shift (lift-off)
            InstallationState::Ready => "rocket-fire rocket-shift",
            // Error -> Error state
            InstallationState::Failed(_) => "rocket-error",
        }
    }
}

// Application state struct
#[derive(Clone)]
pub struct AppState {
    pub settings: Arc<Mutex<Settings>>,
    pub event_manager: Arc<EventManager>,
    pub setup_mode: bool,  // Explicit CLI setup mode
    pub first_run: bool,   // First run based on settings
    pub shutdown_tx: watch::Sender<()>,  // Channel to signal shutdown
    // Use the new enum for the environment
    pub template_env: TemplateEnv,
    // Add flags for Scenario B
    pub is_installed: bool,
    pub is_demo_mode: bool, // True if explicitly DEMO or if not installed
    pub is_installation_server: bool, // True if started via install command
}

// Clean up any existing processes
async fn cleanup_existing_processes() {
    // No complex process handling - removed
}

pub async fn run() -> anyhow::Result<()> {
    // --- Initialize Logging FIRST --- 
    // Use EnvFilter to respect RUST_LOG, defaulting to INFO if not set.
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    // Build the subscriber
    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt::layer());
    // --- Logging Initialized --- 

    // Determine modes SECOND (after logging is set up)
    let is_installation_server = std::env::var("DRAGONFLY_INSTALL_SERVER_MODE").is_ok();
    let is_explicit_demo_mode = std::env::var("DRAGONFLY_DEMO_MODE").is_ok();
    let setup_mode = std::env::var("DRAGONFLY_SETUP_MODE").is_ok();

    // Determine installation status
    let is_installed = is_dragonfly_installed().await;

    // Determine final demo mode status
    // It's demo mode if explicitly set OR if Dragonfly is not installed (and not the installer server itself)
    let is_demo_mode = is_explicit_demo_mode || (!is_installed && !is_installation_server);

    // --- Populate Install State IMMEDIATELY if needed ---
    if is_installation_server { 
        let state = Arc::new(Mutex::new(InstallationState::WaitingSudo));
        match INSTALL_STATE_REF.write() { 
            Ok(mut global_ref) => { *global_ref = Some(state.clone()); },
            Err(e) => { eprintln!("CRITICAL: Failed ... INSTALL_STATE_REF ...: {}", e); }
        }
    }
    
    // --- Create and Store Event Manager EARLY --- 
    // Create event manager (needed even if installing for SSE updates)
    let event_manager = Arc::new(EventManager::new());
    // Store the event manager in the global static ASAP
    match EVENT_MANAGER_REF.write() { 
        Ok(mut global_ref) => { 
            *global_ref = Some(event_manager.clone());
            // eprintln!("[DEBUG lib.rs] EVENT_MANAGER_REF populated.");
        }, 
        Err(e) => { 
            // Use eprintln! as tracing might not be set up
            eprintln!("CRITICAL: Failed to acquire write lock for EVENT_MANAGER_REF: {}. SSE events may not send.", e);
        }
    }
    // -------------------------------------------

    // --- COMPLETELY REMOVED LOGGING INITIALIZATION FROM LIB.RS --- 
    // Calls like info!() etc. will use whatever global dispatcher exists (or none).

    // --- Start Server Setup --- 
    // Conditional info!() calls remain appropriate for specific verbose messages
    // during install, but general logging now respects RUST_LOG.

    let _is_install_mode = is_installation_server;

    if is_demo_mode {
        if is_explicit_demo_mode {
            // This info message will now respect RUST_LOG level
            info!("Starting server explicitly in DEMO MODE - no hardware will be touched");
        } else if !is_installed && !is_installation_server {
            info!("Dragonfly not installed - starting server in DEMO MODE - no hardware will be touched");
        }
    } else if !is_installed && is_installation_server {
        info!("Starting server in INSTALLATION MODE");
    } else if is_installed {
        info!("Dragonfly installed - starting server in normal mode");
    }

    // Initialize the database 
    let db_pool = init_db().await?; // DB init is essential

    // Initialize timing database tables
    db::init_timing_tables().await?; // Essential

    // Load historical timing data
    tinkerbell::load_historical_timings().await?; // Essential

    // --- Start OS Templates Initialization --- 
    // Get current deployment mode from database
    let current_mode = mode::get_current_mode().await?;
    
    // Log the current deployment mode
    match &current_mode {
        Some(mode::DeploymentMode::Flight) => info!("Starting server in Flight mode"),
        Some(mode::DeploymentMode::Simple) => info!("Starting server in Simple mode"),
        Some(mode::DeploymentMode::Swarm) => info!("Starting server in Swarm mode"),
        None => info!("No deployment mode set in database"),
    }
    
    let is_flight_mode = matches!(current_mode, Some(mode::DeploymentMode::Flight));
    
    if is_flight_mode && !is_installation_server {
        info!("Starting OS templates initialization for Flight mode...");
        let event_manager_clone = event_manager.clone(); // Clone for the task
        tokio::spawn(async move { 
            match os_templates::init_os_templates().await {
                Ok(_) => { info!("OS templates initialized successfully"); },
                Err(e) => { warn!("Failed to initialize OS templates: {}", e); }
            }
            // Send event after templates are initialized
            let _ = event_manager_clone.send("templates_ready".to_string());
        });
    } else {
        debug!("Skipping OS templates initialization (not in Flight mode)");
    } // End conditional OS template init

    // --- Graceful Shutdown Setup --- 
    let (shutdown_tx, shutdown_rx) = watch::channel(());

    // Start the timing cleanup task
    tinkerbell::start_timing_cleanup_task(shutdown_rx.clone()).await; // Essential
    
    // Event Manager already created and stored above

    // Start the workflow polling task - only in Flight mode
    if is_flight_mode && !is_installation_server {
        info!("Starting workflow polling task with interval of 1s for Flight mode");
        tinkerbell::start_workflow_polling_task(event_manager.clone(), shutdown_rx.clone()).await;
    } else {
        debug!("Skipping workflow polling task (not in Flight mode)");
    }

    // Load or generate admin credentials
    let credentials = if is_demo_mode {
        // In demo mode, use a special credentials object
        info!("Demo mode: Using simplified admin credentials");
        Credentials {
            username: "admin".to_string(),
            password: None,
            password_hash: "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzb21lc2FsdA$WrTpFYXQY6pZu0K+uskWZwl8fOk0W4Dj/pXGXJ9qPXc".to_string(), // demo hash
        }
    } else {
        match load_credentials().await { // Essential logic
            Ok(creds) => {
                 if !is_installation_server { info!("Loaded existing admin credentials"); } // Conditional Log
                creds
            },
            Err(_) => {
                if !is_installation_server { info!("No admin credentials found, generating default credentials"); } // Cond Log
                match generate_default_credentials().await { // Essential logic
                    Ok(creds) => creds,
                    Err(e) => {
                        eprintln!("CRITICAL: Failed to generate default credentials: {}", e);
                        return Err(anyhow::anyhow!("Failed to generate default credentials: {}", e));
                    }
                }
            }
        }
    };

     // Load settings
    let settings = match load_settings().await { // Essential
        Ok(s) => s,
        Err(e) => {
            if !is_installation_server { info!("Failed to load settings: {}, using defaults", e); } // Cond Log
            Settings::default()
        }
    };

    // Reset setup flag if in setup mode
    if setup_mode {
        if !is_installation_server { info!("Setup mode enabled, resetting setup completion status"); } // Cond Log
        let mut settings_copy = settings.clone();
        settings_copy.setup_completed = false;
        if let Err(e) = auth::save_settings(&settings_copy).await { // Essential
             if !is_installation_server { warn!("Failed to reset setup status: {}", e); } // Cond Log
        }
    }

    // Determine first run status
    let first_run = !settings.setup_completed || setup_mode; // Essential

    // --- MiniJinja Setup --- 
    let preferred_template_path = "/opt/dragonfly/templates";
    let fallback_template_path = "crates/dragonfly-server/templates";
    let template_path = if std::path::Path::new(preferred_template_path).exists() {
        preferred_template_path
    } else {
        fallback_template_path
    }.to_string();

    let template_env = { // Logs inside handled by tracing setup
        #[cfg(debug_assertions)]
        {
            info!("Setting up MiniJinja with auto-reload for development");
            let templates_reloaded_flag = Arc::new(AtomicBool::new(false));
            let flag_clone_for_closure = templates_reloaded_flag.clone();
            let reloader = AutoReloader::new(move |notifier| {
                info!("MiniJinja environment is being (re)created...");
                let mut env = Environment::new();
                let path_for_closure = template_path.clone();
                env.set_loader(path_loader(&path_for_closure));
                
                // Set up filters and globals
                if let Err(e) = ui::setup_minijinja_environment(&mut env) {
                    error!("Failed to set up MiniJinja environment: {}", e);
                }
                
                flag_clone_for_closure.store(true, Ordering::SeqCst);
                notifier.watch_path(path_for_closure.as_str(), true);
                Ok(env)
            });
            let reloader_arc = Arc::new(reloader);
            let reloader_clone = reloader_arc.clone();
            let flag_clone_for_loop = templates_reloaded_flag.clone();
            let event_manager_weak = Arc::downgrade(&event_manager);
            tokio::spawn(async move {
                info!("Starting MiniJinja watcher loop...");
                loop {
                    match reloader_clone.acquire_env() {
                        Ok(_) => {
                            if flag_clone_for_loop.swap(false, Ordering::SeqCst) {
                                info!("Templates reloaded - sending refresh event");
                                if let Some(event_manager) = event_manager_weak.upgrade() {
                                    if let Err(e) = event_manager.send("template_changed:refresh".to_string()) {
                                        warn!("Failed to send template refresh event: {}", e);
                                    } else {
                                        info!("Reload event sent successfully.");
                                    }
                                } else {
                                    warn!("EventManager dropped, cannot send reload event.");
                                }
                            }
                        },
                        Err(e) => {
                            error!("MiniJinja watcher refresh failed: {}", e);
                        }
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                }
            });
            TemplateEnv::Reloading(reloader_arc)
        }
        #[cfg(not(debug_assertions))]
        {
            info!("Using static MiniJinja environment for release build");
            let mut env = Environment::new();
            env.set_loader(path_loader(&template_path));
            
            // Set up filters and globals
            if let Err(e) = ui::setup_minijinja_environment(&mut env) {
                error!("Failed to set up MiniJinja environment: {}", e);
            }
            
            TemplateEnv::Static(Arc::new(env))
        }
    };
    // --- End MiniJinja Setup --- 

    // Create application state
    let app_state = AppState {
        settings: Arc::new(Mutex::new(settings)),
        event_manager: event_manager.clone(), // Use the one created earlier
        setup_mode,
        first_run,
        shutdown_tx: shutdown_tx.clone(),
        template_env,
        // Add the new flags
        is_installed,
        is_demo_mode,
        is_installation_server,
    };

    // Session store setup
    let session_store = SqliteStore::new(db_pool.clone()); // Essential
    session_store.migrate().await?;

    // Session layer setup - use very permissive settings to ensure consistent behavior
    let session_layer = SessionManagerLayer::new(session_store)
        .with_secure(false)
        .with_same_site(tower_sessions::cookie::SameSite::Lax)
        .with_http_only(false);  // Allow JavaScript access to cookies

    // Auth backend setup
    let backend = AdminBackend::new(credentials);
    
    // Build the auth layer
    let auth_layer = AuthManagerLayerBuilder::new(backend, session_layer)
        .build();

    // --- Build Router --- 
    let app = Router::new()
        .merge(auth_router())
        .merge(ui::ui_router())
        .route("/favicon.ico", get(handle_favicon))
        .route("/{mac}", get(api::ipxe_script))
        .route("/ipxe/{*path}", get(api::serve_ipxe_artifact))
        .nest("/api", api::api_router())
        .nest_service("/static", {
            let preferred_path = "/opt/dragonfly/static";
            let fallback_path = "crates/dragonfly-server/static";
            let static_path = if std::path::Path::new(preferred_path).exists() {
                preferred_path
            } else {
                fallback_path
            };
            ServeDir::new(static_path)
        })
        .layer(CookieManagerLayer::new())
        .layer(auth_layer)
        .layer(Extension(db_pool.clone()))
        // Add back a STANDARD TraceLayer if desired for non-install runs (will respect RUST_LOG)
        .layer(TraceLayer::new_for_http()) // Standard layer respects RUST_LOG
        .with_state(app_state);

    // Handoff listener setup 
    if let Some(mode) = &current_mode {
        if *mode == mode::DeploymentMode::Flight {
            if !is_installation_server { info!("Running in Flight mode - starting handoff listener"); }
            tokio::spawn(async move {
                if let Err(e) = mode::start_handoff_listener(shutdown_rx.clone()).await {
                    error!("Handoff listener failed: {}", e);
                }
            });
        }
    }

    // --- Start Server --- 
    let server_port = 3000;
    let addr = SocketAddr::from(([0, 0, 0, 0], server_port));
    let mut listenfd = ListenFd::from_env();
    let socket_activation = std::env::var("LISTEN_FDS").is_ok();
    if socket_activation && !is_installation_server { // Conditional Log
        info!("Socket activation detected via LISTEN_FDS={}", std::env::var("LISTEN_FDS").unwrap_or_else(|_| "?".to_string()));
    }
    let listener = match listenfd.take_tcp_listener(0).context("Failed to take TCP listener from env") {
        Ok(Some(listener)) => {
            if !is_installation_server { info!("Acquired socket via socket activation"); }
            tokio::net::TcpListener::from_std(listener).context("Failed to convert TCP listener")?
        },
        Ok(None) => {
            if socket_activation && !is_installation_server { warn!("Socket activation detected but no socket found"); }
            if !is_installation_server { info!("Binding to port {} directly", server_port); }
            match tokio::net::TcpListener::bind(addr).await {
                Ok(listener) => listener,
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::AddrInUse {
                        error!("Failed to start server: Port {} is already in use", server_port);
                        error!("Another instance of Dragonfly may be running...");
                        return Err(anyhow::anyhow!("Port {} is already in use...", server_port));
                    }
                    return Err(anyhow::anyhow!("Failed to bind to address: {}", e));
                }
            }
        },
        Err(e) => {
            if !is_installation_server { warn!("Failed to check for socket activation: {}", e); }
            match tokio::net::TcpListener::bind(addr).await {
                Ok(listener) => listener,
                Err(e) => {
                    return Err(anyhow::anyhow!("Failed to bind to address: {}", e));
                }
            }
        }
    };
    if !is_installation_server { // Conditional Log
        info!("Dragonfly server listening on http://{}", listener.local_addr().context("Failed to get local address")?);
    }

    // --- Shutdown Signal Handling --- 
    let shutdown_signal = async move {
        // Set up a simple future for Ctrl+C
        let ctrl_c = async { 
            tokio::signal::ctrl_c().await.unwrap_or_else(|e| {
                error!("Failed to listen for Ctrl+C: {}", e);
            });
            info!("Received Ctrl+C");
            println!("\nShutting down...");
        };
        
        #[cfg(unix)]
        let terminate = async { 
            if let Ok(mut signal) = signal(SignalKind::terminate()) {
                signal.recv().await;
                info!("Received SIGTERM");
                println!("\nReceived SIGTERM, shutting down...");
            }
        };
        
        #[cfg(not(unix))] 
        let terminate = std::future::pending::<()>();
        
        // Wait for any signal
        tokio::select! {
            _ = ctrl_c => {},
            _ = terminate => {},
        }
        
        // Send the shutdown signal
        let _ = shutdown_tx.send(());
        info!("Sending shutdown signal to all components");
        
        // Force exit after 5 seconds if graceful shutdown hasn't completed
        tokio::spawn(async {
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            println!("Forcing exit after timeout");
            std::process::exit(0);
        });
    };

    // Start serving with graceful shutdown
    println!("Server started, press Ctrl+C to stop");
    axum::serve(listener, app) // Essential
        .with_graceful_shutdown(shutdown_signal)
        .await
        .context("Server error")?;

    if !is_installation_server { info!("Shutdown complete"); } // Cond Log

    Ok(())
}

async fn handle_favicon() -> impl IntoResponse {
    let path = if std::path::Path::new("/opt/dragonfly/static/favicon/favicon.ico").exists() {
        "/opt/dragonfly/static/favicon/favicon.ico"
    } else {
        "crates/dragonfly-server/static/favicon/favicon.ico"
    };
    match tokio::fs::read(path).await {
        Ok(contents) => (StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "image/x-icon")], contents).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "Favicon not found").into_response()
    }
}

// Access functions for main.rs to use
pub use db::database_exists;