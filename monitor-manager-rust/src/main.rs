#![windows_subsystem = "windows"]

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use std::path::PathBuf;
use sysinfo::System;
use serde::{Deserialize, Serialize};
use std::fs;

mod monitor;
mod tray_app;

use monitor::MonitorManager;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub target_exe: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            target_exe: r"C:\Riot Games\League of Legends\Game\League of Legends.exe".to_string(),
        }
    }
}

impl Config {
    pub fn load() -> Self {
        let config_path = Self::config_path();
        if let Ok(content) = fs::read_to_string(&config_path) {
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            Self::default()
        }
    }

    pub fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        let config_path = Self::config_path();
        let content = serde_json::to_string_pretty(self)?;
        fs::write(&config_path, content)?;
        Ok(())
    }

    fn config_path() -> PathBuf {
        let mut path = std::env::current_exe().unwrap_or_default();
        path.pop();
        path.push("config.json");
        path
    }
}

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub monitoring: bool,
    pub status: String,
    pub monitor_manager: Arc<Mutex<MonitorManager>>,
}

impl AppState {
    pub fn new(monitor_manager: MonitorManager) -> Self {
        Self {
            config: Config::load(),
            monitoring: false,
            status: "Idle - waiting for process".to_string(),
            monitor_manager: Arc::new(Mutex::new(monitor_manager)),
        }
    }
}

fn main() {
    // Initialize monitor manager and save initial settings
    let mut monitor_manager = MonitorManager::new();
    monitor_manager.save_current_settings();

    // Create shared app state
    let app_state = Arc::new(Mutex::new(AppState::new(monitor_manager)));

    // Start monitoring thread
    let state_clone = Arc::clone(&app_state);
    let monitor_thread = thread::spawn(move || {
        monitor_loop(state_clone);
    });

    // Run system tray application
    tray_app::run(app_state);

    // Wait for monitor thread to finish
    monitor_thread.join().unwrap();
}

fn monitor_loop(state: Arc<Mutex<AppState>>) {
    let mut system = System::new_all();
    let mut was_running = false;

    loop {
        // Refresh process list
        system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

        // Get current target exe from config
        let target_exe = {
            let state = state.lock().unwrap();
            state.config.target_exe.clone()
        };

        // Check if target process is running
        let is_running = system.processes().values().any(|process| {
            if let Some(exe_path) = process.exe() {
                exe_path.to_string_lossy().to_lowercase() == target_exe.to_lowercase()
            } else {
                false
            }
        });

        // Process just started
        if is_running && !was_running {
            {
                let mut state = state.lock().unwrap();
                state.status = "Process detected! Disabling secondary monitors...".to_string();
            }

            // Get secondary monitors and disable them
            let monitor_manager = {
                let state = state.lock().unwrap();
                state.monitor_manager.clone()
            };
            let manager = monitor_manager.lock().unwrap();
            
            let monitors = manager.get_all_monitors();
            for monitor in monitors {
                if !monitor.is_primary && monitor.is_active {
                    if manager.disable_monitor(&monitor.device_name) {
                        let mut state = state.lock().unwrap();
                        state.status = format!("✓ Disabled {}", monitor.description);
                    }
                }
            }

            {
                let mut state = state.lock().unwrap();
                state.status = "Monitoring active - secondary monitors disabled".to_string();
                state.monitoring = true;
            }

            was_running = true;
        }
        // Process just stopped
        else if !is_running && was_running {
            {
                let mut state = state.lock().unwrap();
                state.status = "Process closed. Re-enabling monitors...".to_string();
            }

            // Restore all secondary monitors
            let monitor_manager = {
                let state = state.lock().unwrap();
                state.monitor_manager.clone()
            };
            let manager = monitor_manager.lock().unwrap();
            
            let restored = manager.restore_all_monitors();
            for _ in restored {
                let mut state = state.lock().unwrap();
                state.status = format!("✓ Restored monitor");
            }

            {
                let mut state = state.lock().unwrap();
                state.status = "Idle - waiting for process".to_string();
                state.monitoring = false;
            }

            was_running = false;
        }

        thread::sleep(Duration::from_secs(2));
    }
}
