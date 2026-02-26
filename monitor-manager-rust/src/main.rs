#![windows_subsystem = "windows"]

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
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
    pub shutdown: Arc<AtomicBool>,
}

impl AppState {
    pub fn new(monitor_manager: MonitorManager) -> Self {
        Self {
            config: Config::load(),
            monitoring: false,
            status: "Idle - waiting for process".to_string(),
            monitor_manager: Arc::new(Mutex::new(monitor_manager)),
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }
}

fn main() {
    let mut monitor_manager = MonitorManager::new();
    monitor_manager.save_current_settings();

    let app_state = Arc::new(Mutex::new(AppState::new(monitor_manager)));

    let state_clone = Arc::clone(&app_state);
    let monitor_thread = thread::spawn(move || {
        monitor_loop(state_clone);
    });

    tray_app::run(app_state);

    monitor_thread.join().unwrap();
}

fn monitor_loop(state: Arc<Mutex<AppState>>) {
    let mut system = System::new_all();
    let mut was_running = false;

    loop {
        if state.lock().unwrap().shutdown.load(Ordering::Relaxed) {
            let monitor_manager = { state.lock().unwrap().monitor_manager.clone() };
            let _ = monitor_manager.lock().unwrap().restore_all_monitors();
            break;
        }

        system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

        let target_exe = {
            let state = state.lock().unwrap();
            state.config.target_exe.clone()
        };

        let is_running = system.processes().values().any(|process| {
            if let Some(exe_path) = process.exe() {
                exe_path.to_string_lossy().to_lowercase() == target_exe.to_lowercase()
            } else {
                false
            }
        });

        if is_running && !was_running {
            {
                let mut state = state.lock().unwrap();
                state.status = "Process detected! Disabling secondary monitors...".to_string();
            }

            // Never lock `state` while holding the monitor_manager lock — deadlock risk.
            let monitor_manager = { state.lock().unwrap().monitor_manager.clone() };

            let disabled_count = {
                let mut manager = monitor_manager.lock().unwrap();
                manager.save_current_settings();

                let to_disable = manager
                    .get_all_monitors()
                    .into_iter()
                    .filter(|m| !m.is_primary && m.is_active)
                    .map(|m| m.device_name)
                    .collect::<Vec<_>>();

                let mut count = 0usize;
                for device_name in &to_disable {
                    if manager.disable_monitor(device_name) {
                        count += 1;
                    }
                }

                count
            };

            {
                let mut state = state.lock().unwrap();
                if disabled_count > 0 {
                    state.status = format!("Monitoring active - disabled {} monitor(s)", disabled_count);
                } else {
                    state.status = "Monitoring active - no secondary monitors to disable".to_string();
                }
                state.monitoring = true;
            }

            was_running = true;
        } else if !is_running && was_running {
            {
                let mut state = state.lock().unwrap();
                state.status = "Process closed. Re-enabling monitors...".to_string();
            }

            let monitor_manager = { state.lock().unwrap().monitor_manager.clone() };
            let restored_count = {
                let manager = monitor_manager.lock().unwrap();
                manager.restore_all_monitors().len()
            };

            {
                let mut state = state.lock().unwrap();
                if restored_count > 0 {
                    state.status = format!("Idle - restored {} monitor(s)", restored_count);
                } else {
                    state.status = "Idle - no monitors needed restoration".to_string();
                }
                state.monitoring = false;
            }

            was_running = false;
        }

        thread::sleep(Duration::from_secs(2));
    }
}
