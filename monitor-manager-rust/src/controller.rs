use std::collections::HashSet;
use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::config::{ConfigStore, ConfigV2};
use crate::display::{
    DisplayBackend, DisplayError, DisplayId, DisplayInfo, DisplayManager, DisplayPhase,
    RecoveryStore, RecoveryTopologyState, RollbackStatus, TransitionOutcome, TransitionStatus,
};
use crate::logging::FileLogger;
use crate::process::{NativeProcessBackend, ProcessBackend};
use crate::startup::{
    save_config_transactionally, save_persisted_config_transactionally, RegistryStartup,
    StartupBackend,
};

const PROCESS_POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum AppPhase {
    Unconfigured,
    Watching,
    Activating,
    Focused,
    SuppressedUntilClear,
    Paused,
    Restoring,
    Recovering,
    Error,
    ShuttingDown,
}

#[derive(Debug, Clone)]
pub struct UiSnapshot {
    pub phase: AppPhase,
    pub status: String,
    pub config: ConfigV2,
    pub displays: Vec<DisplayInfo>,
    pub matched_triggers: Vec<PathBuf>,
    pub last_error: Option<String>,
    pub can_restore: bool,
}

pub enum ControllerCommand {
    SaveConfig(ConfigV2),
    SetAutomationEnabled(bool),
    RestoreForRun,
    RetryRecovery,
    RefreshDisplays,
    Shutdown { force: bool },
    PrepareForSessionEnd(Sender<bool>),
    SessionEndCancelled,
}

#[derive(Debug, Clone)]
pub enum ControllerEvent {
    Snapshot(UiSnapshot),
    Notification {
        title: String,
        message: String,
        is_error: bool,
    },
    ConfigSaved,
    ShutdownReady,
    ShutdownFailed(String),
    ControllerFailed(String),
}

pub trait DisplayService: Send {
    fn list_displays(&mut self) -> Result<Vec<DisplayInfo>, DisplayError>;
    fn activate(
        &mut self,
        selected_displays: &HashSet<DisplayId>,
    ) -> Result<TransitionOutcome, DisplayError>;
    fn restore(&mut self) -> Result<TransitionOutcome, DisplayError>;
    fn recover(&mut self) -> Result<TransitionOutcome, DisplayError>;
    fn recovery_topology_state(&mut self) -> Result<RecoveryTopologyState, DisplayError>;
}

impl<B, S> DisplayService for DisplayManager<B, S>
where
    B: DisplayBackend + Send,
    S: RecoveryStore + Send,
{
    fn list_displays(&mut self) -> Result<Vec<DisplayInfo>, DisplayError> {
        DisplayManager::list_displays(self)
    }

    fn activate(
        &mut self,
        selected_displays: &HashSet<DisplayId>,
    ) -> Result<TransitionOutcome, DisplayError> {
        DisplayManager::activate(self, selected_displays)
    }

    fn restore(&mut self) -> Result<TransitionOutcome, DisplayError> {
        DisplayManager::restore(self)
    }

    fn recover(&mut self) -> Result<TransitionOutcome, DisplayError> {
        DisplayManager::recover(self)
    }

    fn recovery_topology_state(&mut self) -> Result<RecoveryTopologyState, DisplayError> {
        DisplayManager::recovery_topology_state(self)
    }
}

pub struct ControllerRuntime {
    display: Box<dyn DisplayService>,
    process: Box<dyn ProcessBackend>,
    startup: Box<dyn StartupBackend>,
    config_store: ConfigStore,
    executable: PathBuf,
    logger: Arc<FileLogger>,
    config: ConfigV2,
    initial_warnings: Vec<String>,
    review_required: bool,
    initial_recovery_message: Option<String>,
    poll_interval: Duration,
}

pub struct ControllerStartup {
    pub config: ConfigV2,
    pub warnings: Vec<String>,
    pub review_required: bool,
    pub recovery_message: Option<String>,
}

pub struct ControllerContext {
    pub config_store: ConfigStore,
    pub executable: PathBuf,
    pub logger: Arc<FileLogger>,
    pub startup: ControllerStartup,
}

impl ControllerRuntime {
    pub fn new<D, P, S>(display: D, process: P, startup: S, context: ControllerContext) -> Self
    where
        D: DisplayService + 'static,
        P: ProcessBackend + 'static,
        S: StartupBackend + 'static,
    {
        Self {
            display: Box::new(display),
            process: Box::new(process),
            startup: Box::new(startup),
            config_store: context.config_store,
            executable: context.executable,
            logger: context.logger,
            config: context.startup.config,
            initial_warnings: context.startup.warnings,
            review_required: context.startup.review_required,
            initial_recovery_message: context.startup.recovery_message,
            poll_interval: PROCESS_POLL_INTERVAL,
        }
    }

    pub fn native(
        display: DisplayManager,
        config_store: ConfigStore,
        executable: PathBuf,
        logger: Arc<FileLogger>,
        startup: ControllerStartup,
    ) -> Self {
        Self::new(
            display,
            NativeProcessBackend::new(),
            RegistryStartup::new(),
            ControllerContext {
                config_store,
                executable,
                logger,
                startup,
            },
        )
    }
}

pub fn spawn_controller<W>(
    runtime: ControllerRuntime,
    command_rx: Receiver<ControllerCommand>,
    event_tx: Sender<ControllerEvent>,
    wake_ui: W,
) -> io::Result<JoinHandle<Result<(), String>>>
where
    W: Fn() + Send + 'static,
{
    thread::Builder::new()
        .name("monitor-controller".to_owned())
        .spawn(move || {
            let mut controller = Controller::new(runtime, event_tx, wake_ui);
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                controller.run(command_rx)
            })) {
                Ok(()) => Ok(()),
                Err(_) => {
                    let restored = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        controller.restore_after_channel_failure()
                    }));
                    match restored {
                        Ok(Ok(())) => {
                            controller.emit(ControllerEvent::ShutdownReady);
                        }
                        Ok(Err(message)) => {
                            controller.emit(ControllerEvent::ControllerFailed(message));
                        }
                        Err(_) => {
                            controller.emit(ControllerEvent::ControllerFailed(
                                "The controller and its emergency restoration both failed. Recovery data was retained when available."
                                    .to_owned(),
                            ));
                        }
                    }
                    Err("controller thread panicked".to_owned())
                }
            }
        })
}

struct Controller<W> {
    display: Box<dyn DisplayService>,
    process: Box<dyn ProcessBackend>,
    startup: Box<dyn StartupBackend>,
    config_store: ConfigStore,
    executable: PathBuf,
    logger: Arc<FileLogger>,
    config: ConfigV2,
    phase: AppPhase,
    displays: Vec<DisplayInfo>,
    matched_triggers: Vec<PathBuf>,
    last_error: Option<String>,
    error_retry: ErrorRetry,
    process_error_active: bool,
    basename_fallback_active: bool,
    recovery_pending: bool,
    session_end_pending: bool,
    review_required: bool,
    initial_recovery_message: Option<String>,
    trigger_tracker: TriggerTracker,
    poll_interval: Duration,
    event_tx: Sender<ControllerEvent>,
    wake_ui: W,
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
enum ErrorRetry {
    #[default]
    None,
    Reconcile,
    Restore,
    Recover,
}

impl<W: Fn()> Controller<W> {
    fn new(runtime: ControllerRuntime, event_tx: Sender<ControllerEvent>, wake_ui: W) -> Self {
        Self {
            display: runtime.display,
            process: runtime.process,
            startup: runtime.startup,
            config_store: runtime.config_store,
            executable: runtime.executable,
            logger: runtime.logger,
            config: runtime.config,
            phase: AppPhase::Recovering,
            displays: Vec::new(),
            matched_triggers: Vec::new(),
            last_error: (!runtime.initial_warnings.is_empty())
                .then(|| runtime.initial_warnings.join("\n")),
            error_retry: ErrorRetry::None,
            process_error_active: false,
            basename_fallback_active: false,
            recovery_pending: false,
            session_end_pending: false,
            review_required: runtime.review_required,
            initial_recovery_message: runtime.initial_recovery_message,
            trigger_tracker: TriggerTracker::default(),
            poll_interval: runtime.poll_interval,
            event_tx,
            wake_ui,
        }
    }

    fn run(&mut self, command_rx: Receiver<ControllerCommand>) {
        self.initialize();
        if !self.emit_snapshot() {
            let _ = self.restore_after_channel_failure();
            return;
        }

        let mut next_poll = Instant::now();
        loop {
            let timeout = next_poll.saturating_duration_since(Instant::now());
            match command_rx.recv_timeout(timeout) {
                Ok(command) => {
                    if !self.handle_command(command) {
                        return;
                    }
                    if Instant::now() >= next_poll {
                        self.poll_processes();
                        next_poll = Instant::now() + self.poll_interval;
                    }
                    if !self.emit_snapshot() {
                        let _ = self.restore_after_channel_failure();
                        return;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.poll_processes();
                    next_poll = Instant::now() + self.poll_interval;
                    if !self.emit_snapshot() {
                        let _ = self.restore_after_channel_failure();
                        return;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = self.restore_after_channel_failure();
                    return;
                }
            }
        }
    }

    fn initialize(&mut self) {
        let _ = self.logger.info("controller starting");
        match self.display.recover() {
            Ok(outcome) => {
                self.recovery_pending = false;
                self.error_retry = ErrorRetry::None;
                if outcome.status == TransitionStatus::Recovered {
                    let message = format!(
                        "Recovered {} display path(s) from the previous run.",
                        outcome.affected_display_ids.len()
                    );
                    let _ = self.logger.info(&message);
                    self.notify("Display recovery complete", message, false);
                } else if let Some(message) = self.initial_recovery_message.take() {
                    let _ = self.logger.info(&message);
                    self.notify("Display recovery complete", message, false);
                }
                self.enter_idle_phase();
            }
            Err(error) => {
                self.recovery_pending = true;
                self.set_display_error("Startup recovery failed", error, ErrorRetry::Recover);
            }
        }

        match self.display.list_displays() {
            Ok(displays) => self.displays = displays,
            Err(error) if self.phase == AppPhase::Error => {
                let _ = self.logger.error(format!("display query failed: {error}"));
            }
            Err(error) => {
                self.set_display_error("Display discovery failed", error, ErrorRetry::Reconcile)
            }
        }
    }

    fn handle_command(&mut self, command: ControllerCommand) -> bool {
        match command {
            ControllerCommand::SaveConfig(config) => self.save_config(config, true),
            ControllerCommand::SetAutomationEnabled(enabled) => {
                self.set_automation_enabled(enabled)
            }
            ControllerCommand::RestoreForRun => self.restore_for_run(),
            ControllerCommand::RetryRecovery => self.retry_recovery(),
            ControllerCommand::RefreshDisplays => self.reconcile_displays(),
            ControllerCommand::Shutdown { force } => return self.shutdown(force),
            ControllerCommand::PrepareForSessionEnd(reply) => {
                let restored = self.prepare_for_session_end();
                let _ = reply.send(restored);
            }
            ControllerCommand::SessionEndCancelled => {
                if self.session_end_pending {
                    self.session_end_pending = false;
                    self.enter_idle_phase();
                    self.trigger_tracker.rearm_activation();
                    let _ = self.logger.info("session shutdown was cancelled");
                }
            }
        }
        true
    }

    fn save_config(&mut self, config: ConfigV2, report_saved: bool) {
        let rule_changed = config.trigger_apps != self.config.trigger_apps
            || config.disabled_displays != self.config.disabled_displays;
        let enabling_state = (config.automation_enabled && !self.config.automation_enabled)
            || (config.start_with_windows && !self.config.start_with_windows);
        let require_full_validation = self.review_required || rule_changed || enabling_state;
        let normalized = if require_full_validation {
            match config.normalized_for_save() {
                Ok(config) => config,
                Err(error) => {
                    self.report_nonfatal("Configuration was not saved", error.to_string());
                    return;
                }
            }
        } else {
            if let Err(error) = config.validate_persisted() {
                self.report_nonfatal("Configuration was not saved", error.to_string());
                return;
            }
            config
        };

        if self.config.automation_enabled
            && !normalized.automation_enabled
            && self.restoration_required()
        {
            let mut paused = self.config.clone();
            paused.automation_enabled = false;
            if let Err(error) = self.config_store.save_persisted(&paused) {
                self.report_nonfatal("Configuration was not saved", error.to_string());
                return;
            }
            self.config = paused;
        }

        if (self.recovery_pending || self.phase == AppPhase::Focused)
            && self.restore_display_state("Configuration change").is_err()
        {
            return;
        }

        let save_result = if require_full_validation {
            save_config_transactionally(
                &self.config_store,
                self.startup.as_ref(),
                &self.executable,
                &normalized,
            )
        } else {
            save_persisted_config_transactionally(
                &self.config_store,
                self.startup.as_ref(),
                &self.executable,
                &normalized,
            )
        };
        match save_result {
            Ok(()) => {
                self.config = normalized;
                self.review_required = false;
                self.trigger_tracker.reset();
                self.matched_triggers.clear();
                self.process_error_active = false;
                self.basename_fallback_active = false;
                self.last_error = None;
                self.error_retry = ErrorRetry::None;
                self.enter_idle_phase();
                let _ = self.refresh_display_list();
                let _ = self.logger.info("configuration saved");
                if report_saved {
                    self.emit(ControllerEvent::ConfigSaved);
                }
            }
            Err(error) => {
                self.enter_idle_phase();
                self.report_nonfatal("Configuration was not saved", error.to_string());
            }
        }
    }

    fn set_automation_enabled(&mut self, enabled: bool) {
        if enabled && self.review_required {
            self.report_nonfatal(
                "Configuration review required",
                "Review the migrated rule in Settings and use Save before enabling automation."
                    .to_owned(),
            );
            return;
        }

        let mut config = self.config.clone();
        config.automation_enabled = enabled;
        let validation = if enabled {
            config.validate()
        } else {
            config.validate_persisted()
        };
        if let Err(error) = validation {
            self.report_nonfatal("Automation state was not saved", error.to_string());
            return;
        }

        if !enabled {
            if let Err(error) = self.config_store.save_persisted(&config) {
                self.report_nonfatal("Automation state was not saved", error.to_string());
                return;
            }
            self.config = config;
            if self.restoration_required()
                && self
                    .restore_display_state("Automation state change")
                    .is_err()
            {
                let _ = self
                    .logger
                    .warn("automation pause was saved while display restoration remains pending");
                return;
            }
            self.complete_automation_state_change(false);
            return;
        }

        if (self.recovery_pending || self.phase == AppPhase::Focused)
            && self
                .restore_display_state("Automation state change")
                .is_err()
        {
            return;
        }

        match self.config_store.save_persisted(&config) {
            Ok(()) => {
                self.config = config;
                self.complete_automation_state_change(true);
            }
            Err(error) => {
                self.enter_idle_phase();
                self.report_nonfatal("Automation state was not saved", error.to_string());
            }
        }
    }

    fn complete_automation_state_change(&mut self, enabled: bool) {
        self.trigger_tracker.reset();
        self.matched_triggers.clear();
        self.process_error_active = false;
        self.basename_fallback_active = false;
        self.last_error = None;
        self.error_retry = ErrorRetry::None;
        self.enter_idle_phase();
        let _ = self.logger.info(if enabled {
            "automation resumed"
        } else {
            "automation paused"
        });
    }

    fn restore_for_run(&mut self) {
        let was_running = self.current_trigger_state();
        if self.restore_display_state("Manual restore").is_err() {
            return;
        }

        if self.config.automation_enabled && self.config.is_configured() && was_running {
            self.trigger_tracker.mark_running();
            self.phase = AppPhase::SuppressedUntilClear;
        } else {
            self.enter_idle_phase();
        }
    }

    fn retry_recovery(&mut self) {
        let restoration_required = self.restoration_required();
        self.phase = AppPhase::Recovering;
        self.emit_snapshot();
        match verify_restoration_outcome(
            self.display.recover(),
            restoration_required,
            DisplayPhase::Recover,
        ) {
            Ok(outcome) => {
                self.recovery_pending = false;
                self.last_error = None;
                self.error_retry = ErrorRetry::None;
                self.enter_idle_phase();
                let _ = self.refresh_display_list();
                let message = if outcome.status == TransitionStatus::Recovered {
                    format!(
                        "Recovered {} display path(s).",
                        outcome.affected_display_ids.len()
                    )
                } else {
                    "No display recovery was required.".to_owned()
                };
                let _ = self.logger.info(&message);
                self.notify("Display recovery", message, false);
            }
            Err(error) => {
                self.recovery_pending = true;
                self.set_display_error("Display recovery failed", error, ErrorRetry::Recover);
            }
        }
    }

    fn reconcile_displays(&mut self) {
        if self.phase == AppPhase::Error {
            match self.error_retry {
                ErrorRetry::Restore => {
                    self.retry_failed_restore();
                    return;
                }
                ErrorRetry::Recover => {
                    self.retry_recovery();
                    return;
                }
                ErrorRetry::None | ErrorRetry::Reconcile => {}
            }
        }
        if self.restoration_required() {
            match self.display.recovery_topology_state() {
                Ok(RecoveryTopologyState::Focused) => {
                    if !self.config.automation_enabled {
                        if self
                            .restore_display_state("Paused display reconciliation")
                            .is_ok()
                        {
                            self.enter_idle_phase();
                        }
                        return;
                    }
                    self.recovery_pending = true;
                    self.last_error = None;
                    self.error_retry = ErrorRetry::None;
                    self.phase = AppPhase::Focused;
                    return;
                }
                Ok(RecoveryTopologyState::Original) => {
                    if self
                        .restore_display_state("Display topology changed")
                        .is_err()
                    {
                        return;
                    }
                    if self.config.automation_enabled && self.trigger_tracker.running {
                        self.phase = AppPhase::SuppressedUntilClear;
                    } else {
                        self.enter_idle_phase();
                    }
                }
                Ok(RecoveryTopologyState::Diverged) => {
                    let _ = self.restore_display_state("Display topology changed");
                    let _ = self.refresh_display_list();
                    return;
                }
                Ok(RecoveryTopologyState::NoJournal) => {
                    self.recovery_pending = true;
                    self.set_display_error(
                        "Display topology changed",
                        DisplayError::new(
                            DisplayPhase::SafetyCheck,
                            "display restoration is required but its recovery journal is missing",
                        ),
                        ErrorRetry::None,
                    );
                    let _ = self.refresh_display_list();
                    return;
                }
                Err(error) => {
                    self.set_display_error(
                        "Display topology check failed",
                        error,
                        ErrorRetry::Reconcile,
                    );
                    return;
                }
            }
        }
        let was_reconciling = self.error_retry == ErrorRetry::Reconcile;
        match self.refresh_display_list() {
            Err(error) if self.phase == AppPhase::Error && !was_reconciling => {
                let _ = self
                    .logger
                    .error(format!("display discovery also failed: {error}"));
            }
            Err(error) => {
                self.set_display_error("Display discovery failed", error, ErrorRetry::Reconcile)
            }
            Ok(()) => {
                if was_reconciling {
                    self.last_error = None;
                    self.error_retry = ErrorRetry::None;
                    self.enter_idle_phase();
                }
                if self.phase == AppPhase::Watching && !self.matched_triggers.is_empty() {
                    self.trigger_tracker.rearm_activation();
                }
            }
        }
    }

    fn shutdown(&mut self, force: bool) -> bool {
        if force {
            let _ = self
                .logger
                .warn("exiting without verified display restoration");
            self.emit(ControllerEvent::ShutdownReady);
            return false;
        }

        let restoration_required = self.restoration_required();
        self.phase = AppPhase::ShuttingDown;
        self.emit_snapshot();
        match verify_restoration_outcome(
            self.display.restore(),
            restoration_required,
            DisplayPhase::Restore,
        ) {
            Ok(_) => {
                self.recovery_pending = false;
                self.error_retry = ErrorRetry::None;
                let _ = self.logger.info("display restoration verified; exiting");
                self.emit(ControllerEvent::ShutdownReady);
                false
            }
            Err(error) => {
                self.recovery_pending = true;
                let message = format_display_error("Exit restoration failed", &error);
                let _ = self.logger.error(&message);
                self.last_error = Some(message.clone());
                self.error_retry = ErrorRetry::None;
                self.phase = AppPhase::Error;
                self.emit(ControllerEvent::ShutdownFailed(message));
                true
            }
        }
    }

    fn prepare_for_session_end(&mut self) -> bool {
        let restoration_required = self.restoration_required();
        self.phase = AppPhase::ShuttingDown;
        self.emit_snapshot();
        match verify_restoration_outcome(
            self.display.restore(),
            restoration_required,
            DisplayPhase::Restore,
        ) {
            Ok(_) => {
                self.recovery_pending = false;
                self.error_retry = ErrorRetry::None;
                self.session_end_pending = true;
                let _ = self
                    .logger
                    .info("display restoration verified for session shutdown");
                true
            }
            Err(error) => {
                self.recovery_pending = true;
                self.session_end_pending = false;
                self.set_display_error(
                    "Session shutdown restoration failed",
                    error,
                    ErrorRetry::None,
                );
                false
            }
        }
    }

    fn poll_processes(&mut self) {
        if self.phase == AppPhase::Error {
            match self.error_retry {
                ErrorRetry::None => {}
                ErrorRetry::Reconcile => self.reconcile_displays(),
                ErrorRetry::Restore => self.retry_failed_restore(),
                ErrorRetry::Recover => self.retry_recovery(),
            }
            return;
        }
        if !matches!(
            self.phase,
            AppPhase::Watching | AppPhase::Focused | AppPhase::SuppressedUntilClear
        ) {
            return;
        }

        let scan = match self.process.scan(&self.config.trigger_apps) {
            Ok(scan) => scan,
            Err(error) => {
                let message = format!("Process scan failed: {error}");
                if !self.process_error_active {
                    self.notify("Automation check failed", &message, true);
                    let _ = self.logger.error(&message);
                }
                self.process_error_active = true;
                self.last_error = Some(message.clone());
                return;
            }
        };

        if self.process_error_active {
            self.process_error_active = false;
            self.last_error = None;
        }
        if scan.used_basename_fallback && !self.basename_fallback_active {
            let _ = self
                .logger
                .warn("a trigger matched by executable name because Windows denied path access");
        }
        self.basename_fallback_active = scan.used_basename_fallback;
        self.matched_triggers = scan.matched_triggers;

        match self
            .trigger_tracker
            .observe(!self.matched_triggers.is_empty(), self.phase)
        {
            TriggerAction::None => {}
            TriggerAction::Activate => self.activate(),
            TriggerAction::Restore => {
                if self.restore_display_state("Automatic restore").is_ok() {
                    self.enter_idle_phase();
                }
            }
            TriggerAction::ClearSuppression => {
                self.enter_idle_phase();
                self.last_error = None;
                self.error_retry = ErrorRetry::None;
            }
        }
    }

    fn retry_failed_restore(&mut self) {
        if self
            .restore_display_state("Display restoration retry")
            .is_err()
        {
            return;
        }
        if self.config.automation_enabled && self.trigger_tracker.running {
            self.phase = AppPhase::SuppressedUntilClear;
        } else {
            self.enter_idle_phase();
        }
    }

    fn activate(&mut self) {
        self.phase = AppPhase::Activating;
        self.emit_snapshot();
        let selected = self
            .config
            .disabled_displays
            .iter()
            .map(|display| DisplayId::new(&display.device_path))
            .collect::<HashSet<_>>();
        match self.display.activate(&selected) {
            Ok(outcome)
                if matches!(
                    outcome.status,
                    TransitionStatus::Activated | TransitionStatus::AlreadyFocused
                ) =>
            {
                self.recovery_pending = true;
                self.error_retry = ErrorRetry::None;
                self.phase = AppPhase::Focused;
                self.last_error = missing_display_message(&outcome);
                let _ = self.logger.info(format!(
                    "focus mode active; {} display path(s) disabled",
                    outcome.affected_display_ids.len()
                ));
                if let Some(message) = self.last_error.clone() {
                    self.notify("Configured display unavailable", message, true);
                }
                let affected = outcome.affected_display_ids.iter().collect::<HashSet<_>>();
                for display in &mut self.displays {
                    if affected.contains(&display.id) {
                        display.is_active = false;
                    }
                }
            }
            Ok(outcome) => {
                self.recovery_pending = false;
                self.error_retry = ErrorRetry::None;
                self.enter_idle_phase();
                let message = missing_display_message(&outcome).unwrap_or_else(|| {
                    "None of the selected displays can be disabled in the current topology."
                        .to_owned()
                });
                self.last_error = Some(message.clone());
                self.notify("Focus mode was not activated", &message, true);
                let _ = self.logger.warn(message);
            }
            Err(error) => {
                self.recovery_pending = error.recovery_journal_retained
                    || matches!(error.rollback, RollbackStatus::Failed { .. });
                let retry = if self.recovery_pending {
                    ErrorRetry::Recover
                } else {
                    ErrorRetry::None
                };
                self.set_display_error("Focus mode activation failed", error, retry);
            }
        }
    }

    fn restore_display_state(&mut self, context: &str) -> Result<TransitionOutcome, ()> {
        let restoration_required = self.restoration_required();
        self.phase = AppPhase::Restoring;
        self.emit_snapshot();
        match verify_restoration_outcome(
            self.display.restore(),
            restoration_required,
            DisplayPhase::Restore,
        ) {
            Ok(outcome) => {
                self.recovery_pending = false;
                self.last_error = None;
                self.error_retry = ErrorRetry::None;
                let _ = self.logger.info(format!(
                    "{context}: display restoration verified ({:?})",
                    outcome.status
                ));
                let _ = self.refresh_display_list();
                Ok(outcome)
            }
            Err(error) => {
                self.recovery_pending = true;
                self.set_display_error(&format!("{context} failed"), error, ErrorRetry::Restore);
                Err(())
            }
        }
    }

    fn current_trigger_state(&mut self) -> bool {
        match self.process.scan(&self.config.trigger_apps) {
            Ok(scan) => {
                self.matched_triggers = scan.matched_triggers;
                !self.matched_triggers.is_empty()
            }
            Err(error) => {
                let _ = self
                    .logger
                    .error(format!("manual restore process scan failed: {error}"));
                self.trigger_tracker.running
            }
        }
    }

    fn refresh_display_list(&mut self) -> Result<(), DisplayError> {
        self.displays = self.display.list_displays()?;
        Ok(())
    }

    fn idle_phase(&self) -> AppPhase {
        if self.review_required {
            AppPhase::Unconfigured
        } else {
            phase_for_config(&self.config)
        }
    }

    fn enter_idle_phase(&mut self) {
        self.phase = self.idle_phase();
        if matches!(self.phase, AppPhase::Error | AppPhase::Paused) && self.last_error.is_none() {
            if let Err(error) = self.config.validate() {
                self.last_error = Some(format!("Configuration requires attention: {error}"));
            }
        }
    }

    fn set_display_error(&mut self, title: &str, error: DisplayError, retry: ErrorRetry) {
        let message = format_display_error(title, &error);
        let repeated = self.last_error.as_deref() == Some(message.as_str());
        if !repeated {
            let _ = self.logger.error(&message);
            self.notify(title, &message, true);
        }
        self.last_error = Some(message.clone());
        self.error_retry = retry;
        self.phase = AppPhase::Error;
    }

    fn report_nonfatal(&mut self, title: &str, message: String) {
        let _ = self.logger.error(format!("{title}: {message}"));
        self.last_error = Some(message.clone());
        self.error_retry = ErrorRetry::None;
        self.notify(title, message, true);
    }

    fn restore_after_channel_failure(&mut self) -> Result<(), String> {
        let _ = self
            .logger
            .warn("controller channel closed; attempting display restoration");
        let restoration_required = self.restoration_required();
        match verify_restoration_outcome(
            self.display.restore(),
            restoration_required,
            DisplayPhase::Restore,
        ) {
            Ok(_) => {
                let _ = self
                    .logger
                    .info("display restoration verified after channel closure");
                Ok(())
            }
            Err(error) => {
                let message = format!("display restoration failed after channel closure: {error}");
                let _ = self.logger.error(&message);
                Err(message)
            }
        }
    }

    fn emit_snapshot(&self) -> bool {
        self.emit(ControllerEvent::Snapshot(UiSnapshot {
            phase: self.phase,
            status: self.status_text(),
            config: self.config.clone(),
            displays: self.displays.clone(),
            matched_triggers: self.matched_triggers.clone(),
            last_error: self.last_error.clone(),
            can_restore: self.recovery_pending || self.phase == AppPhase::Focused,
        }))
    }

    fn restoration_required(&self) -> bool {
        self.recovery_pending || self.phase == AppPhase::Focused
    }

    fn status_text(&self) -> String {
        match self.phase {
            AppPhase::Unconfigured => "Configuration required".to_owned(),
            AppPhase::Watching => {
                if self.matched_triggers.is_empty() {
                    "Watching for configured applications".to_owned()
                } else {
                    format!(
                        "Watching ({} trigger(s) detected)",
                        self.matched_triggers.len()
                    )
                }
            }
            AppPhase::Activating => "Activating focus mode".to_owned(),
            AppPhase::Focused => format!(
                "Focus mode active ({} trigger(s))",
                self.matched_triggers.len()
            ),
            AppPhase::SuppressedUntilClear => {
                "Restored for this run; waiting for all triggers to exit".to_owned()
            }
            AppPhase::Paused => "Automation paused".to_owned(),
            AppPhase::Restoring => "Restoring the original display topology".to_owned(),
            AppPhase::Recovering => "Recovering the previous display topology".to_owned(),
            AppPhase::Error => "Action required".to_owned(),
            AppPhase::ShuttingDown => "Restoring displays before exit".to_owned(),
        }
    }

    fn notify(&self, title: impl Into<String>, message: impl Into<String>, is_error: bool) {
        self.emit(ControllerEvent::Notification {
            title: title.into(),
            message: message.into(),
            is_error,
        });
    }

    fn emit(&self, event: ControllerEvent) -> bool {
        if self.event_tx.send(event).is_err() {
            return false;
        }
        (self.wake_ui)();
        true
    }
}

fn verify_restoration_outcome(
    result: Result<TransitionOutcome, DisplayError>,
    restoration_required: bool,
    phase: DisplayPhase,
) -> Result<TransitionOutcome, DisplayError> {
    let outcome = result?;
    if restoration_required && outcome.status == TransitionStatus::NoChange {
        return Err(DisplayError::new(
            phase,
            "display restoration was required but no recovery journal was available",
        ));
    }
    Ok(outcome)
}

fn phase_for_config(config: &ConfigV2) -> AppPhase {
    if !config.is_configured() {
        AppPhase::Unconfigured
    } else if !config.automation_enabled {
        AppPhase::Paused
    } else if config.validate().is_err() {
        AppPhase::Error
    } else {
        AppPhase::Watching
    }
}

fn missing_display_message(outcome: &TransitionOutcome) -> Option<String> {
    (!outcome.missing_display_ids.is_empty()).then(|| {
        format!(
            "{} configured display(s) are not connected.",
            outcome.missing_display_ids.len()
        )
    })
}

fn format_display_error(context: &str, error: &DisplayError) -> String {
    let rollback = match &error.rollback {
        RollbackStatus::NotRequired => String::new(),
        RollbackStatus::Succeeded => "; the original topology was restored".to_owned(),
        RollbackStatus::Failed { message, .. } => {
            format!("; rollback also failed: {message}")
        }
    };
    format!("{context}: {error}{rollback}")
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum TriggerAction {
    None,
    Activate,
    Restore,
    ClearSuppression,
}

#[derive(Debug, Default)]
struct TriggerTracker {
    consecutive_absent: u8,
    running: bool,
}

impl TriggerTracker {
    fn observe(&mut self, running: bool, phase: AppPhase) -> TriggerAction {
        if running {
            let newly_running = !self.running;
            self.running = true;
            self.consecutive_absent = 0;
            return if newly_running && phase == AppPhase::Watching {
                TriggerAction::Activate
            } else {
                TriggerAction::None
            };
        }

        if !self.running {
            return TriggerAction::None;
        }

        self.consecutive_absent = self.consecutive_absent.saturating_add(1);
        if self.consecutive_absent < 3 {
            return TriggerAction::None;
        }

        self.running = false;
        self.consecutive_absent = 0;
        match phase {
            AppPhase::Focused | AppPhase::Activating => TriggerAction::Restore,
            AppPhase::SuppressedUntilClear => TriggerAction::ClearSuppression,
            _ => TriggerAction::None,
        }
    }

    fn mark_running(&mut self) {
        self.running = true;
        self.consecutive_absent = 0;
    }

    fn rearm_activation(&mut self) {
        self.running = false;
        self.consecutive_absent = 0;
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DisabledDisplay;
    use crate::display::{DisplayPhase, RollbackStatus};
    use crate::process::{ProcessError, ProcessScan};
    use crate::startup::startup_command;
    use std::collections::VecDeque;
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::io;
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let suffix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "monitor-manager-controller-{}-{suffix}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Debug)]
    struct MockDisplayState {
        activation_status: TransitionStatus,
        restore_status: TransitionStatus,
        activate_count: usize,
        restore_count: usize,
        fail_activate: bool,
        fail_restore: bool,
        fail_list_displays: bool,
        recovery_state: RecoveryTopologyState,
    }

    struct MockDisplay {
        state: Arc<Mutex<MockDisplayState>>,
    }

    impl DisplayService for MockDisplay {
        fn list_displays(&mut self) -> Result<Vec<DisplayInfo>, DisplayError> {
            if self.state.lock().unwrap().fail_list_displays {
                return Err(DisplayError::new(
                    DisplayPhase::Query,
                    "mock display discovery failure",
                ));
            }
            Ok(vec![
                display("primary", true, true),
                display("secondary", false, false),
            ])
        }

        fn activate(
            &mut self,
            selected_displays: &HashSet<DisplayId>,
        ) -> Result<TransitionOutcome, DisplayError> {
            let mut state = self.state.lock().unwrap();
            state.activate_count += 1;
            if state.fail_activate {
                return Err(DisplayError::new(
                    DisplayPhase::Apply,
                    "mock activation failure after verified rollback",
                ));
            }
            let mut outcome = transition(state.activation_status);
            if state.activation_status == TransitionStatus::Activated {
                outcome.affected_display_ids = selected_displays.iter().cloned().collect();
                state.recovery_state = RecoveryTopologyState::Focused;
            } else if state.activation_status == TransitionStatus::NoChange {
                outcome.missing_display_ids = selected_displays.iter().cloned().collect();
            }
            Ok(outcome)
        }

        fn restore(&mut self) -> Result<TransitionOutcome, DisplayError> {
            let mut state = self.state.lock().unwrap();
            state.restore_count += 1;
            if state.fail_restore {
                return Err(DisplayError {
                    phase: DisplayPhase::Restore,
                    win32_code: Some(31),
                    rollback: RollbackStatus::NotRequired,
                    affected_display_ids: vec![DisplayId::new("secondary")],
                    recovery_journal_retained: true,
                    message: "mock restore failure".to_owned(),
                });
            }
            let status = state.restore_status;
            if status != TransitionStatus::NoChange {
                state.recovery_state = RecoveryTopologyState::NoJournal;
            }
            Ok(transition(status))
        }

        fn recover(&mut self) -> Result<TransitionOutcome, DisplayError> {
            Ok(transition(TransitionStatus::NoChange))
        }

        fn recovery_topology_state(&mut self) -> Result<RecoveryTopologyState, DisplayError> {
            Ok(self.state.lock().unwrap().recovery_state)
        }
    }

    struct MockProcess {
        scans: VecDeque<ProcessScan>,
    }

    impl ProcessBackend for MockProcess {
        fn scan(&mut self, _trigger_apps: &[PathBuf]) -> Result<ProcessScan, ProcessError> {
            Ok(self.scans.pop_front().unwrap_or_default())
        }
    }

    #[derive(Clone, Default)]
    struct MockStartup {
        command: Arc<Mutex<Option<OsString>>>,
    }

    impl StartupBackend for MockStartup {
        fn read_command(&self) -> io::Result<Option<OsString>> {
            Ok(self.command.lock().unwrap().clone())
        }

        fn write_command(&self, command: Option<&OsStr>) -> io::Result<()> {
            *self.command.lock().unwrap() = command.map(OsString::from);
            Ok(())
        }
    }

    fn display(id: &str, primary: bool, protected: bool) -> DisplayInfo {
        DisplayInfo {
            id: DisplayId::new(id),
            friendly_name: id.to_owned(),
            device_path: id.to_owned(),
            is_active: true,
            is_primary: primary,
            is_protected: protected,
        }
    }

    fn transition(status: TransitionStatus) -> TransitionOutcome {
        TransitionOutcome {
            status,
            affected_display_ids: Vec::new(),
            missing_display_ids: Vec::new(),
            original_fingerprint: None,
            focused_fingerprint: None,
            used_database_fallback: false,
        }
    }

    fn running(path: &Path) -> ProcessScan {
        ProcessScan {
            matched_triggers: vec![path.to_path_buf()],
            used_basename_fallback: false,
        }
    }

    fn valid_config(directory: &Path) -> ConfigV2 {
        let trigger = directory.join("trigger.exe");
        fs::write(&trigger, b"exe").unwrap();
        ConfigV2 {
            trigger_apps: vec![trigger.canonicalize().unwrap()],
            disabled_displays: vec![DisabledDisplay {
                device_path: "secondary".to_owned(),
                last_known_name: "Secondary".to_owned(),
            }],
            automation_enabled: true,
            ..ConfigV2::default()
        }
    }

    fn build_controller(
        directory: &TestDirectory,
        config: ConfigV2,
        scans: Vec<ProcessScan>,
        display_state: Arc<Mutex<MockDisplayState>>,
        review_required: bool,
    ) -> (Controller<fn()>, Receiver<ControllerEvent>) {
        let store = ConfigStore::new(
            directory.0.join("config.json"),
            directory.0.join("legacy.json"),
        );
        let logger =
            Arc::new(FileLogger::open(&directory.0.join("test.log"), 64 * 1024, 1).unwrap());
        let runtime = ControllerRuntime::new(
            MockDisplay {
                state: display_state,
            },
            MockProcess {
                scans: scans.into(),
            },
            MockStartup::default(),
            ControllerContext {
                config_store: store,
                executable: directory.0.join("MonitorManager.exe"),
                logger,
                startup: ControllerStartup {
                    config,
                    warnings: Vec::new(),
                    review_required,
                    recovery_message: None,
                },
            },
        );
        let (event_tx, event_rx) = mpsc::channel();
        let mut controller = Controller::new(runtime, event_tx, (|| {}) as fn());
        controller.initialize();
        (controller, event_rx)
    }

    fn display_state(status: TransitionStatus) -> Arc<Mutex<MockDisplayState>> {
        Arc::new(Mutex::new(MockDisplayState {
            activation_status: status,
            restore_status: TransitionStatus::Restored,
            activate_count: 0,
            restore_count: 0,
            fail_activate: false,
            fail_restore: false,
            fail_list_displays: false,
            recovery_state: RecoveryTopologyState::NoJournal,
        }))
    }

    #[test]
    fn activates_on_first_running_scan() {
        let mut tracker = TriggerTracker::default();
        assert_eq!(
            tracker.observe(true, AppPhase::Watching),
            TriggerAction::Activate
        );
    }

    #[test]
    fn requires_three_absent_scans_before_restore() {
        let mut tracker = TriggerTracker::default();
        tracker.observe(true, AppPhase::Watching);
        assert_eq!(
            tracker.observe(false, AppPhase::Focused),
            TriggerAction::None
        );
        assert_eq!(
            tracker.observe(false, AppPhase::Focused),
            TriggerAction::None
        );
        assert_eq!(
            tracker.observe(false, AppPhase::Focused),
            TriggerAction::Restore
        );
    }

    #[test]
    fn clears_suppression_only_after_triggers_exit() {
        let mut tracker = TriggerTracker::default();
        tracker.observe(true, AppPhase::Watching);
        tracker.observe(false, AppPhase::SuppressedUntilClear);
        tracker.observe(false, AppPhase::SuppressedUntilClear);
        assert_eq!(
            tracker.observe(false, AppPhase::SuppressedUntilClear),
            TriggerAction::ClearSuppression
        );
    }

    #[test]
    fn transient_running_scan_resets_exit_debounce() {
        let mut tracker = TriggerTracker::default();
        tracker.observe(true, AppPhase::Watching);
        tracker.observe(false, AppPhase::Focused);
        tracker.observe(false, AppPhase::Focused);
        assert_eq!(
            tracker.observe(true, AppPhase::Focused),
            TriggerAction::None
        );
        assert_eq!(
            tracker.observe(false, AppPhase::Focused),
            TriggerAction::None
        );
    }

    #[test]
    fn phase_selection_honors_configuration_and_persistent_pause() {
        let mut config = ConfigV2::default();
        assert_eq!(phase_for_config(&config), AppPhase::Unconfigured);

        config.trigger_apps.push(PathBuf::from(r"C:\missing.exe"));
        config
            .disabled_displays
            .push(crate::config::DisabledDisplay {
                device_path: "display-2".to_owned(),
                last_known_name: "Display 2".to_owned(),
            });
        assert_eq!(phase_for_config(&config), AppPhase::Paused);
        config.automation_enabled = true;
        assert_eq!(phase_for_config(&config), AppPhase::Error);
    }

    #[test]
    fn multiple_triggers_use_or_semantics_and_manual_restore_suppresses_until_clear() {
        let directory = TestDirectory::new();
        let mut config = valid_config(&directory.0);
        let second = directory.0.join("second.exe");
        fs::write(&second, b"exe").unwrap();
        config.trigger_apps.push(second.canonicalize().unwrap());
        let state = display_state(TransitionStatus::Activated);
        let scans = vec![
            running(&config.trigger_apps[1]),
            running(&config.trigger_apps[1]),
            ProcessScan::default(),
            ProcessScan::default(),
            ProcessScan::default(),
        ];
        let (mut controller, _) = build_controller(&directory, config, scans, state.clone(), false);

        controller.poll_processes();
        assert_eq!(controller.phase, AppPhase::Focused);
        controller.restore_for_run();
        assert_eq!(controller.phase, AppPhase::SuppressedUntilClear);
        controller.poll_processes();
        controller.poll_processes();
        assert_eq!(controller.phase, AppPhase::SuppressedUntilClear);
        controller.poll_processes();
        assert_eq!(controller.phase, AppPhase::Watching);
        assert_eq!(state.lock().unwrap().activate_count, 1);
        assert_eq!(state.lock().unwrap().restore_count, 1);
    }

    #[test]
    fn missing_selected_displays_do_not_retry_every_scan() {
        let directory = TestDirectory::new();
        let config = valid_config(&directory.0);
        let state = display_state(TransitionStatus::NoChange);
        let scans = vec![
            running(&config.trigger_apps[0]),
            running(&config.trigger_apps[0]),
        ];
        let (mut controller, _) = build_controller(&directory, config, scans, state.clone(), false);

        controller.poll_processes();
        controller.poll_processes();

        assert_eq!(controller.phase, AppPhase::Watching);
        assert_eq!(state.lock().unwrap().activate_count, 1);
    }

    #[test]
    fn rolled_back_activation_error_does_not_retry_while_trigger_remains() {
        let directory = TestDirectory::new();
        let config = valid_config(&directory.0);
        let state = display_state(TransitionStatus::Activated);
        state.lock().unwrap().fail_activate = true;
        let scans = vec![running(&config.trigger_apps[0])];
        let (mut controller, _) = build_controller(&directory, config, scans, state.clone(), false);

        controller.poll_processes();
        controller.poll_processes();
        controller.poll_processes();

        assert_eq!(controller.phase, AppPhase::Error);
        assert_eq!(controller.error_retry, ErrorRetry::None);
        assert_eq!(state.lock().unwrap().activate_count, 1);
    }

    #[test]
    fn focused_reconciliation_preserves_disabled_display_entries() {
        let directory = TestDirectory::new();
        let config = valid_config(&directory.0);
        let state = display_state(TransitionStatus::Activated);
        let scans = vec![running(&config.trigger_apps[0])];
        let (mut controller, _) = build_controller(&directory, config, scans, state.clone(), false);
        controller.poll_processes();
        state.lock().unwrap().fail_list_displays = true;

        controller.reconcile_displays();

        assert_eq!(controller.phase, AppPhase::Focused);
        assert_eq!(controller.displays.len(), 2);
        assert!(controller
            .displays
            .iter()
            .any(|display| display.id == DisplayId::new("secondary") && !display.is_active));
    }

    #[test]
    fn pause_restores_and_persists_without_touching_startup() {
        let directory = TestDirectory::new();
        let config = valid_config(&directory.0);
        let state = display_state(TransitionStatus::Activated);
        let scans = vec![running(&config.trigger_apps[0])];
        let (mut controller, _) = build_controller(&directory, config, scans, state.clone(), false);
        controller.poll_processes();

        controller.set_automation_enabled(false);

        assert_eq!(controller.phase, AppPhase::Paused);
        assert!(!controller.config.automation_enabled);
        assert_eq!(state.lock().unwrap().restore_count, 1);
        let saved: ConfigV2 =
            serde_json::from_slice(&fs::read(directory.0.join("config.json")).unwrap()).unwrap();
        assert!(!saved.automation_enabled);
    }

    #[test]
    fn pause_remains_persisted_when_restoration_fails() {
        let directory = TestDirectory::new();
        let config = valid_config(&directory.0);
        let state = display_state(TransitionStatus::Activated);
        let scans = vec![running(&config.trigger_apps[0])];
        let (mut controller, _) = build_controller(&directory, config, scans, state.clone(), false);
        controller.poll_processes();
        state.lock().unwrap().fail_restore = true;

        controller.set_automation_enabled(false);

        assert_eq!(controller.phase, AppPhase::Error);
        assert!(controller.recovery_pending);
        assert!(!controller.config.automation_enabled);
        let saved: ConfigV2 =
            serde_json::from_slice(&fs::read(directory.0.join("config.json")).unwrap()).unwrap();
        assert!(!saved.automation_enabled);
    }

    #[test]
    fn failed_automatic_restore_retries_until_verified() {
        let directory = TestDirectory::new();
        let config = valid_config(&directory.0);
        let state = display_state(TransitionStatus::Activated);
        let scans = vec![
            running(&config.trigger_apps[0]),
            ProcessScan::default(),
            ProcessScan::default(),
            ProcessScan::default(),
        ];
        let (mut controller, _) = build_controller(&directory, config, scans, state.clone(), false);
        controller.poll_processes();
        state.lock().unwrap().fail_restore = true;
        controller.poll_processes();
        controller.poll_processes();
        controller.poll_processes();

        assert_eq!(controller.phase, AppPhase::Error);
        assert_eq!(controller.error_retry, ErrorRetry::Restore);

        state.lock().unwrap().fail_restore = false;
        controller.poll_processes();

        assert_eq!(controller.phase, AppPhase::Watching);
        assert!(!controller.recovery_pending);
        assert_eq!(state.lock().unwrap().restore_count, 2);
    }

    #[test]
    fn settings_pause_remains_persisted_when_restoration_fails() {
        let directory = TestDirectory::new();
        let config = valid_config(&directory.0);
        let state = display_state(TransitionStatus::Activated);
        let scans = vec![running(&config.trigger_apps[0])];
        let (mut controller, _) =
            build_controller(&directory, config.clone(), scans, state.clone(), false);
        controller.poll_processes();
        state.lock().unwrap().fail_restore = true;
        let mut updated = config;
        updated.automation_enabled = false;

        controller.save_config(updated, true);

        assert_eq!(controller.phase, AppPhase::Error);
        assert!(controller.recovery_pending);
        assert!(!controller.config.automation_enabled);
        let saved: ConfigV2 =
            serde_json::from_slice(&fs::read(directory.0.join("config.json")).unwrap()).unwrap();
        assert!(!saved.automation_enabled);
    }

    #[test]
    fn successful_display_refresh_clears_a_transient_display_error() {
        let directory = TestDirectory::new();
        let config = valid_config(&directory.0);
        let state = display_state(TransitionStatus::Activated);
        let (mut controller, _) =
            build_controller(&directory, config, Vec::new(), state.clone(), false);
        state.lock().unwrap().fail_list_displays = true;

        controller.reconcile_displays();
        assert_eq!(controller.phase, AppPhase::Error);

        state.lock().unwrap().fail_list_displays = false;
        controller.poll_processes();
        assert_eq!(controller.phase, AppPhase::Watching);
        assert!(controller.last_error.is_none());
    }

    #[test]
    fn focused_topology_clears_a_transient_reconciliation_error() {
        let directory = TestDirectory::new();
        let config = valid_config(&directory.0);
        let state = display_state(TransitionStatus::Activated);
        let scans = vec![running(&config.trigger_apps[0])];
        let (mut controller, _) = build_controller(&directory, config, scans, state, false);
        controller.poll_processes();
        controller.phase = AppPhase::Error;
        controller.last_error = Some("transient display error".to_owned());

        controller.reconcile_displays();

        assert_eq!(controller.phase, AppPhase::Focused);
        assert!(controller.last_error.is_none());
    }

    #[test]
    fn explicit_save_repairs_a_stale_startup_command() {
        let directory = TestDirectory::new();
        let mut config = valid_config(&directory.0);
        config.start_with_windows = true;
        let state = display_state(TransitionStatus::Activated);
        let (mut controller, _) =
            build_controller(&directory, config.clone(), Vec::new(), state, false);
        controller
            .startup
            .write_command(Some(OsStr::new("\"C:\\stale.exe\" --background")))
            .unwrap();

        controller.save_config(config, true);

        assert_eq!(
            controller.startup.read_command().unwrap(),
            Some(startup_command(&controller.executable).unwrap())
        );
    }

    #[test]
    fn active_configuration_change_restores_before_saving_new_rule() {
        let directory = TestDirectory::new();
        let config = valid_config(&directory.0);
        let state = display_state(TransitionStatus::Activated);
        let scans = vec![running(&config.trigger_apps[0])];
        let (mut controller, events) =
            build_controller(&directory, config.clone(), scans, state.clone(), false);
        controller.poll_processes();
        let replacement = directory.0.join("replacement.exe");
        fs::write(&replacement, b"exe").unwrap();
        let mut updated = config;
        updated.trigger_apps = vec![replacement];

        controller.save_config(updated, true);

        assert_eq!(state.lock().unwrap().restore_count, 1);
        assert_eq!(controller.phase, AppPhase::Watching);
        assert!(events
            .try_iter()
            .any(|event| matches!(event, ControllerEvent::ConfigSaved)));
    }

    #[test]
    fn session_end_stays_suppressed_until_cancelled() {
        let directory = TestDirectory::new();
        let config = valid_config(&directory.0);
        let state = display_state(TransitionStatus::Activated);
        let scans = vec![
            running(&config.trigger_apps[0]),
            running(&config.trigger_apps[0]),
            running(&config.trigger_apps[0]),
        ];
        let (mut controller, _) = build_controller(&directory, config, scans, state.clone(), false);
        controller.poll_processes();

        assert!(controller.prepare_for_session_end());
        assert_eq!(controller.phase, AppPhase::ShuttingDown);
        controller.poll_processes();
        assert_eq!(state.lock().unwrap().activate_count, 1);
        controller.handle_command(ControllerCommand::SessionEndCancelled);
        controller.poll_processes();
        assert_eq!(state.lock().unwrap().activate_count, 2);
    }

    #[test]
    fn shutdown_failure_requires_explicit_force_and_keeps_controller_alive() {
        let directory = TestDirectory::new();
        let config = valid_config(&directory.0);
        let state = display_state(TransitionStatus::Activated);
        state.lock().unwrap().fail_restore = true;
        let (mut controller, events) =
            build_controller(&directory, config, Vec::new(), state, false);

        assert!(controller.shutdown(false));
        assert_eq!(controller.phase, AppPhase::Error);
        assert!(events
            .try_iter()
            .any(|event| matches!(event, ControllerEvent::ShutdownFailed(_))));
        assert!(!controller.shutdown(true));
    }

    #[test]
    fn shutdown_rejects_no_change_when_restoration_is_required() {
        let directory = TestDirectory::new();
        let config = valid_config(&directory.0);
        let state = display_state(TransitionStatus::Activated);
        let scans = vec![running(&config.trigger_apps[0])];
        let (mut controller, events) =
            build_controller(&directory, config, scans, state.clone(), false);
        controller.poll_processes();
        state.lock().unwrap().restore_status = TransitionStatus::NoChange;

        assert!(controller.shutdown(false));

        assert_eq!(controller.phase, AppPhase::Error);
        assert!(controller.recovery_pending);
        assert!(events
            .try_iter()
            .any(|event| matches!(event, ControllerEvent::ShutdownFailed(_))));
    }

    #[test]
    fn channel_failure_attempts_verified_restoration() {
        let directory = TestDirectory::new();
        let config = valid_config(&directory.0);
        let state = display_state(TransitionStatus::Activated);
        let (mut controller, _) =
            build_controller(&directory, config, Vec::new(), state.clone(), false);

        assert!(controller.restore_after_channel_failure().is_ok());
        assert_eq!(state.lock().unwrap().restore_count, 1);
    }

    #[test]
    fn channel_failure_rejects_a_missing_required_journal() {
        let directory = TestDirectory::new();
        let config = valid_config(&directory.0);
        let state = display_state(TransitionStatus::Activated);
        state.lock().unwrap().restore_status = TransitionStatus::NoChange;
        let (mut controller, _) = build_controller(&directory, config, Vec::new(), state, false);
        controller.recovery_pending = true;

        assert!(controller.restore_after_channel_failure().is_err());
    }

    #[test]
    fn migrated_configuration_cannot_resume_before_explicit_save() {
        let directory = TestDirectory::new();
        let mut config = valid_config(&directory.0);
        config.automation_enabled = false;
        let state = display_state(TransitionStatus::Activated);
        let (mut controller, _) = build_controller(&directory, config, Vec::new(), state, true);
        assert_eq!(controller.phase, AppPhase::Unconfigured);

        controller.handle_command(ControllerCommand::SetAutomationEnabled(true));

        assert!(!controller.config.automation_enabled);
        assert_eq!(controller.phase, AppPhase::Unconfigured);
    }
}
