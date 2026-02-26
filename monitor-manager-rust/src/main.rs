#![windows_subsystem = "windows"]

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;
use std::path::{Path, PathBuf};
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
    let monitor_manager = MonitorManager::new();
    let app_state = Arc::new(Mutex::new(AppState::new(monitor_manager)));

    let state_clone = Arc::clone(&app_state);
    let monitor_thread = thread::spawn(move || {
        monitor_loop(state_clone);
    });

    tray_app::run(app_state);

    monitor_thread.join().unwrap();
}

fn is_target_running(system: &System, target_exe: &str) -> bool {
    let target_lower = target_exe.to_lowercase();
    let target_filename = Path::new(target_exe)
        .file_name()
        .map(|f| f.to_string_lossy().to_lowercase());

    system.processes().values().any(|process| {
        if let Some(exe_path) = process.exe() {
            if exe_path.to_string_lossy().to_lowercase() == target_lower {
                return true;
            }
            if let Some(ref target_fn) = target_filename {
                if let Some(proc_fn) = exe_path.file_name() {
                    return proc_fn.to_string_lossy().to_lowercase() == *target_fn;
                }
            }
        } else if let Some(ref target_fn) = target_filename {
            return process.name().to_string_lossy().to_lowercase() == *target_fn;
        }
        false
    })
}

fn monitor_loop(state: Arc<Mutex<AppState>>) {
    let mut system = System::new_all();
    let mut was_running = false;

    loop {
        let shutdown = { state.lock().unwrap().shutdown.load(Ordering::Relaxed) };
        if shutdown {
            let monitor_manager = { state.lock().unwrap().monitor_manager.clone() };
            let mut manager = monitor_manager.lock().unwrap();
            if manager.are_monitors_disabled() {
                let _ = manager.restore_all_monitors();
            }
            break;
        }

        system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

        let target_exe = { state.lock().unwrap().config.target_exe.clone() };
        let is_running = is_target_running(&system, &target_exe);

        if is_running && !was_running {
            let monitor_manager = { state.lock().unwrap().monitor_manager.clone() };
            let disabled_count = {
                let mut manager = monitor_manager.lock().unwrap();
                manager.save_current_settings();
                manager.disable_secondary_monitors()
            };

            {
                let mut state = state.lock().unwrap();
                state.monitoring = true;
                state.status = if disabled_count > 0 {
                    format!("Active - disabled {} monitor(s)", disabled_count)
                } else {
                    "Active - no secondary monitors to disable".to_string()
                };
            }

            was_running = true;
        } else if !is_running && was_running {
            let monitor_manager = { state.lock().unwrap().monitor_manager.clone() };
            let restored_count = {
                let mut manager = monitor_manager.lock().unwrap();
                manager.restore_all_monitors().len()
            };

            {
                let mut state = state.lock().unwrap();
                state.monitoring = false;
                state.status = if restored_count > 0 {
                    format!("Idle - restored {} monitor(s)", restored_count)
                } else {
                    "Idle - no monitors needed restoration".to_string()
                };
            }

            was_running = false;
        }

        thread::sleep(Duration::from_secs(2));
    }
}
