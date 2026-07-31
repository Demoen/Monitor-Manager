#![windows_subsystem = "windows"]

use std::error::Error;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use native_windows_gui as nwg;
use windows::core::HSTRING;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

pub mod app_paths;
pub mod config;
pub mod controller;
pub mod display;
pub mod logging;
pub mod process;
pub mod single_instance;
pub mod startup;
pub mod tray_app;

use app_paths::AppPaths;
use config::{ConfigSource, ConfigStore, DisabledDisplay};
use controller::{spawn_controller, ControllerRuntime, ControllerStartup};
use display::{DisplayManager, TransitionStatus};
use logging::{install_panic_hook, FileLogger};
use single_instance::{activate_existing, activation_message, broadcast_activation, InstanceGuard};
use tray_app::TrayApp;

const MAX_LOG_BYTES: u64 = 1_048_576;
const LOG_BACKUPS: usize = 3;

fn main() {
    if let Err(error) = run() {
        show_fatal_error(&error.to_string());
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let background = std::env::args_os().skip(1).any(|arg| arg == "--background");
    let executable = std::env::current_exe()?;
    let paths = AppPaths::resolve()?;
    paths.ensure_root()?;

    let Some(_instance) = InstanceGuard::try_acquire(&paths.lock)? else {
        if !background && !activate_existing(Duration::from_secs(5))? {
            broadcast_activation()?;
        }
        return Ok(());
    };

    let mut display = DisplayManager::native(paths.recovery.clone());
    let (initial_recovery_message, initial_recovery_error) = match display.recover() {
        Ok(outcome) if outcome.status == TransitionStatus::Recovered => (
            Some(format!(
                "Recovered {} display path(s) from the previous run.",
                outcome.affected_display_ids.len()
            )),
            None,
        ),
        Ok(outcome) if outcome.status == TransitionStatus::AlreadyRestored => (
            Some("The previous display topology was already restored.".to_owned()),
            None,
        ),
        Ok(_) => (None, None),
        Err(error) => (None, Some(format!("Startup recovery failed: {error}"))),
    };

    let (logger, log_warning) = match FileLogger::open(&paths.log, MAX_LOG_BYTES, LOG_BACKUPS) {
        Ok(logger) => (Arc::new(logger), None),
        Err(error) => (
            Arc::new(FileLogger::disabled(paths.log.clone())),
            Some(format!("Logging is unavailable: {error}")),
        ),
    };
    install_panic_hook(logger.clone());
    logger.info(format!(
        "Monitor Manager {} starting{}",
        env!("CARGO_PKG_VERSION"),
        if background { " in background" } else { "" }
    ))?;
    if let Some(error) = &initial_recovery_error {
        logger.warn(error)?;
    }

    let initial_displays = match display.list_displays() {
        Ok(displays) => displays,
        Err(error) => {
            logger.warn(format!("initial display discovery failed: {error}"))?;
            Vec::new()
        }
    };
    let secondary_displays = initial_displays
        .iter()
        .filter(|display| display.is_active && !display.is_protected)
        .map(|display| DisabledDisplay {
            device_path: display.device_path.clone(),
            last_known_name: display.friendly_name.clone(),
        })
        .collect::<Vec<_>>();

    let config_store = ConfigStore::new(
        paths.config.clone(),
        AppPaths::legacy_config_path(&executable),
    );
    let loaded = config_store.load(&secondary_displays)?;
    let mut warnings = loaded
        .warnings
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let logging_unavailable = log_warning.is_some();
    warnings.extend(log_warning);
    match &loaded.source {
        ConfigSource::MigratedLegacy => warnings.push(
            "Legacy settings were imported with automation paused; review and save them."
                .to_owned(),
        ),
        ConfigSource::RecoveredCorrupt(path) => warnings.push(format!(
            "The invalid configuration was preserved at {}.",
            path.display()
        )),
        ConfigSource::Current | ConfigSource::FirstRun => {}
    }
    let show_settings = !background
        || loaded.source != ConfigSource::Current
        || loaded.config.validate().is_err()
        || logging_unavailable;
    let review_required = loaded.source == ConfigSource::MigratedLegacy;

    let _com = ComApartment::initialize()?;
    nwg::init()?;
    nwg::Font::set_global_family("Segoe UI")?;

    let (command_tx, command_rx) = mpsc::channel();
    let (event_tx, event_rx) = mpsc::channel();
    let failsafe_tx = command_tx.clone();
    let app = TrayApp::build(
        command_tx,
        event_rx,
        paths.root.clone(),
        activation_message(),
    )?;
    let notice = app.notice_sender();
    let runtime = ControllerRuntime::native(
        display,
        config_store,
        executable,
        logger.clone(),
        ControllerStartup {
            config: loaded.config,
            warnings,
            review_required,
            recovery_message: initial_recovery_message,
        },
    );
    let controller = spawn_controller(runtime, command_rx, event_tx, move || notice.notice())?;

    let dispatch_result = catch_unwind(AssertUnwindSafe(|| app.run(show_settings)));
    if dispatch_result.is_err() {
        let _ = failsafe_tx.send(controller::ControllerCommand::Shutdown { force: false });
    }
    drop(app);
    drop(failsafe_tx);
    let controller_result = controller
        .join()
        .map_err(|_| "controller thread terminated unexpectedly")?;
    controller_result.map_err(|error| -> Box<dyn Error> { error.into() })?;
    if dispatch_result.is_err() {
        return Err("the Windows UI terminated unexpectedly".into());
    }
    logger.info("Monitor Manager stopped")?;
    Ok(())
}

struct ComApartment;

impl ComApartment {
    fn initialize() -> windows::core::Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
        Ok(Self)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

fn show_fatal_error(message: &str) {
    let caption = HSTRING::from("Monitor Manager");
    let text = HSTRING::from(format!("Monitor Manager could not start.\n\n{message}"));
    unsafe {
        MessageBoxW(None, &text, &caption, MB_OK | MB_ICONERROR);
    }
}
