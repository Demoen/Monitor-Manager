use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ops::Deref;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::Duration;

use native_windows_gui as nwg;
use windows::core::{w, PWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_SETVERSION,
    NOTIFYICONDATAW, NOTIFYICON_VERSION_4,
};
use windows::Win32::UI::WindowsAndMessaging::{
    SetForegroundWindow, SetMenuItemInfoW, HICON, HMENU, MENUITEMINFOW, MIIM_STRING,
    PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMECRITICAL, PBT_APMRESUMESTANDBY, PBT_APMRESUMESUSPEND,
    WM_DEVICECHANGE, WM_DISPLAYCHANGE, WM_ENDSESSION, WM_POWERBROADCAST, WM_QUERYENDSESSION,
};

use crate::config::{ConfigV2, DisabledDisplay};
use crate::controller::{AppPhase, ControllerCommand, ControllerEvent, UiSnapshot};
use crate::display::DisplayInfo;

const RAW_EVENT_HANDLER_ID: usize = 0x4D4D_0001;
const NWG_TRAY_MESSAGE: u32 = 0x0400 + 102;
const DISPLAY_ROWS_PER_COLUMN: usize = 9;

struct DisplayRow {
    checkbox: nwg::CheckBox,
    selection: DisabledDisplay,
    editable: bool,
}

struct TrayControls {
    message_window: nwg::MessageWindow,
    settings_window: nwg::Window,
    icon: nwg::Icon,
    ui_font: nwg::Font,
    _heading_font: nwg::Font,
    tray: nwg::TrayNotification,
    tray_menu: nwg::Menu,
    tray_status: nwg::MenuItem,
    _tray_separator_one: nwg::MenuSeparator,
    tray_settings: nwg::MenuItem,
    tray_pause: nwg::MenuItem,
    tray_restore: nwg::MenuItem,
    tray_logs: nwg::MenuItem,
    _tray_separator_two: nwg::MenuSeparator,
    tray_exit: nwg::MenuItem,
    notice: nwg::Notice,
    _title_label: nwg::Label,
    _status_header: nwg::Label,
    status_value: nwg::Label,
    error_value: nwg::Label,
    _applications_header: nwg::Label,
    applications_list: nwg::ListBox<String>,
    add_application: nwg::Button,
    remove_application: nwg::Button,
    _displays_header: nwg::Label,
    display_frame: nwg::Frame,
    display_hint: nwg::Label,
    automation_enabled: nwg::CheckBox,
    start_with_windows: nwg::CheckBox,
    restore_button: nwg::Button,
    logs_button: nwg::Button,
    save_button: nwg::Button,
    cancel_button: nwg::Button,
    shutdown_failure_label: nwg::Label,
    shutdown_retry_button: nwg::Button,
    shutdown_cancel_button: nwg::Button,
    shutdown_force_button: nwg::Button,
    status_bar: nwg::StatusBar,
    file_dialog: nwg::FileDialog,
    display_rows: RefCell<Vec<DisplayRow>>,
    command_tx: Sender<ControllerCommand>,
    event_rx: RefCell<Receiver<ControllerEvent>>,
    log_directory: PathBuf,
    snapshot: RefCell<Option<UiSnapshot>>,
    draft: RefCell<ConfigV2>,
    rendered_displays: RefCell<Vec<DisplayInfo>>,
    save_pending: Cell<bool>,
    shutdown_pending: Cell<bool>,
    shutdown_failure_visible: Cell<bool>,
    display_render_failed: Cell<bool>,
    channel_error_shown: Cell<bool>,
    controller_available: Cell<bool>,
}

pub struct TrayApp {
    inner: Rc<TrayControls>,
    handlers: RefCell<Vec<nwg::EventHandler>>,
    raw_handlers: RefCell<Vec<nwg::RawEventHandler>>,
}

impl TrayApp {
    pub fn build(
        command_tx: Sender<ControllerCommand>,
        event_rx: Receiver<ControllerEvent>,
        log_directory: PathBuf,
        activation_message: u32,
    ) -> Result<Self, nwg::NwgError> {
        let resources = nwg::EmbedResource::load(None)?;
        let mut icon = nwg::Icon::default();
        nwg::Icon::builder()
            .source_embed(Some(&resources))
            .source_embed_id(1)
            .size(Some((32, 32)))
            .build(&mut icon)?;

        let mut ui_font = nwg::Font::default();
        nwg::Font::builder()
            .family("Segoe UI")
            .size(16)
            .build(&mut ui_font)?;
        let mut heading_font = nwg::Font::default();
        nwg::Font::builder()
            .family("Segoe UI Semibold")
            .size(20)
            .weight(600)
            .build(&mut heading_font)?;

        let mut message_window = nwg::MessageWindow::default();
        nwg::MessageWindow::builder().build(&mut message_window)?;

        let mut settings_window = nwg::Window::default();
        nwg::Window::builder()
            .title("Monitor Manager Settings")
            .size((840, 690))
            .center(true)
            .flags(nwg::WindowFlags::WINDOW | nwg::WindowFlags::MINIMIZE_BOX)
            .icon(Some(&icon))
            .build(&mut settings_window)?;

        let mut tray = nwg::TrayNotification::default();
        nwg::TrayNotification::builder()
            .parent(&message_window)
            .icon(Some(&icon))
            .tip(Some("Monitor Manager - Starting"))
            .build(&mut tray)?;

        let mut tray_menu = nwg::Menu::default();
        nwg::Menu::builder()
            .popup(true)
            .parent(&message_window)
            .build(&mut tray_menu)?;
        let mut tray_status = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .text("Status: Starting")
            .disabled(true)
            .parent(&tray_menu)
            .build(&mut tray_status)?;
        let mut tray_separator_one = nwg::MenuSeparator::default();
        nwg::MenuSeparator::builder()
            .parent(&tray_menu)
            .build(&mut tray_separator_one)?;
        let mut tray_settings = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .text("Open Settings")
            .parent(&tray_menu)
            .build(&mut tray_settings)?;
        let mut tray_pause = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .text("Pause Automation")
            .disabled(true)
            .parent(&tray_menu)
            .build(&mut tray_pause)?;
        let mut tray_restore = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .text("Restore for This Run")
            .disabled(true)
            .parent(&tray_menu)
            .build(&mut tray_restore)?;
        let mut tray_logs = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .text("Open Logs")
            .parent(&tray_menu)
            .build(&mut tray_logs)?;
        let mut tray_separator_two = nwg::MenuSeparator::default();
        nwg::MenuSeparator::builder()
            .parent(&tray_menu)
            .build(&mut tray_separator_two)?;
        let mut tray_exit = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .text("Exit")
            .parent(&tray_menu)
            .build(&mut tray_exit)?;

        let mut notice = nwg::Notice::default();
        nwg::Notice::builder()
            .parent(&message_window)
            .build(&mut notice)?;

        let mut title_label = nwg::Label::default();
        nwg::Label::builder()
            .text("Monitor Manager")
            .position((20, 14))
            .size((790, 32))
            .font(Some(&heading_font))
            .parent(&settings_window)
            .build(&mut title_label)?;
        let mut status_header = nwg::Label::default();
        nwg::Label::builder()
            .text("Status")
            .position((20, 54))
            .size((790, 23))
            .font(Some(&heading_font))
            .parent(&settings_window)
            .build(&mut status_header)?;
        let mut status_value = nwg::Label::default();
        nwg::Label::builder()
            .text("Starting controller...")
            .position((20, 80))
            .size((790, 24))
            .font(Some(&ui_font))
            .parent(&settings_window)
            .build(&mut status_value)?;
        let mut error_value = nwg::Label::default();
        nwg::Label::builder()
            .text("")
            .position((20, 104))
            .size((790, 38))
            .font(Some(&ui_font))
            .parent(&settings_window)
            .build(&mut error_value)?;

        let mut applications_header = nwg::Label::default();
        nwg::Label::builder()
            .text("Applications")
            .position((20, 148))
            .size((790, 26))
            .font(Some(&heading_font))
            .parent(&settings_window)
            .build(&mut applications_header)?;
        let mut applications_list = nwg::ListBox::<String>::default();
        nwg::ListBox::builder()
            .position((20, 177))
            .size((635, 118))
            .font(Some(&ui_font))
            .parent(&settings_window)
            .build(&mut applications_list)?;
        let mut add_application = nwg::Button::default();
        nwg::Button::builder()
            .text("Add Application...")
            .position((670, 177))
            .size((140, 34))
            .font(Some(&ui_font))
            .parent(&settings_window)
            .build(&mut add_application)?;
        let mut remove_application = nwg::Button::default();
        nwg::Button::builder()
            .text("Remove")
            .position((670, 220))
            .size((140, 34))
            .font(Some(&ui_font))
            .parent(&settings_window)
            .build(&mut remove_application)?;

        let mut displays_header = nwg::Label::default();
        nwg::Label::builder()
            .text("Displays")
            .position((20, 307))
            .size((790, 26))
            .font(Some(&heading_font))
            .parent(&settings_window)
            .build(&mut displays_header)?;
        let mut display_frame = nwg::Frame::default();
        nwg::Frame::builder()
            .position((20, 337))
            .size((790, 225))
            .flags(nwg::FrameFlags::VISIBLE | nwg::FrameFlags::BORDER)
            .parent(&settings_window)
            .build(&mut display_frame)?;
        let mut display_hint = nwg::Label::default();
        nwg::Label::builder()
            .text("Waiting for display information...")
            .position((10, 9))
            .size((765, 25))
            .font(Some(&ui_font))
            .parent(&display_frame)
            .build(&mut display_hint)?;

        let mut automation_enabled = nwg::CheckBox::default();
        nwg::CheckBox::builder()
            .text("Automation Enabled")
            .position((20, 576))
            .size((210, 28))
            .font(Some(&ui_font))
            .parent(&settings_window)
            .build(&mut automation_enabled)?;
        let mut start_with_windows = nwg::CheckBox::default();
        nwg::CheckBox::builder()
            .text("Start with Windows")
            .position((245, 576))
            .size((210, 28))
            .font(Some(&ui_font))
            .parent(&settings_window)
            .build(&mut start_with_windows)?;

        let mut restore_button = nwg::Button::default();
        nwg::Button::builder()
            .text("Restore")
            .position((20, 616))
            .size((125, 34))
            .font(Some(&ui_font))
            .enabled(false)
            .parent(&settings_window)
            .build(&mut restore_button)?;
        let mut logs_button = nwg::Button::default();
        nwg::Button::builder()
            .text("Open Logs")
            .position((155, 616))
            .size((125, 34))
            .font(Some(&ui_font))
            .parent(&settings_window)
            .build(&mut logs_button)?;
        let mut save_button = nwg::Button::default();
        nwg::Button::builder()
            .text("Save")
            .position((550, 616))
            .size((125, 34))
            .font(Some(&ui_font))
            .parent(&settings_window)
            .build(&mut save_button)?;
        let mut cancel_button = nwg::Button::default();
        nwg::Button::builder()
            .text("Cancel")
            .position((685, 616))
            .size((125, 34))
            .font(Some(&ui_font))
            .parent(&settings_window)
            .build(&mut cancel_button)?;

        let mut shutdown_failure_label = nwg::Label::default();
        nwg::Label::builder()
            .text("")
            .position((20, 568))
            .size((790, 42))
            .font(Some(&ui_font))
            .parent(&settings_window)
            .build(&mut shutdown_failure_label)?;
        shutdown_failure_label.set_visible(false);
        let mut shutdown_retry_button = nwg::Button::default();
        nwg::Button::builder()
            .text("Retry")
            .position((415, 616))
            .size((125, 34))
            .font(Some(&ui_font))
            .parent(&settings_window)
            .build(&mut shutdown_retry_button)?;
        shutdown_retry_button.set_visible(false);
        let mut shutdown_cancel_button = nwg::Button::default();
        nwg::Button::builder()
            .text("Cancel")
            .position((550, 616))
            .size((125, 34))
            .font(Some(&ui_font))
            .parent(&settings_window)
            .build(&mut shutdown_cancel_button)?;
        shutdown_cancel_button.set_visible(false);
        let mut shutdown_force_button = nwg::Button::default();
        nwg::Button::builder()
            .text("Exit Anyway")
            .position((685, 616))
            .size((125, 34))
            .font(Some(&ui_font))
            .parent(&settings_window)
            .build(&mut shutdown_force_button)?;
        shutdown_force_button.set_visible(false);

        let mut status_bar = nwg::StatusBar::default();
        nwg::StatusBar::builder()
            .text("Starting")
            .font(Some(&ui_font))
            .parent(&settings_window)
            .build(&mut status_bar)?;

        let mut file_dialog = nwg::FileDialog::default();
        nwg::FileDialog::builder()
            .title("Add trigger applications")
            .action(nwg::FileDialogAction::Open)
            .multiselect(true)
            .filters("Windows applications (*.exe)|All files (*.*)")
            .build(&mut file_dialog)?;

        let inner = Rc::new(TrayControls {
            message_window,
            settings_window,
            icon,
            ui_font,
            _heading_font: heading_font,
            tray,
            tray_menu,
            tray_status,
            _tray_separator_one: tray_separator_one,
            tray_settings,
            tray_pause,
            tray_restore,
            tray_logs,
            _tray_separator_two: tray_separator_two,
            tray_exit,
            notice,
            _title_label: title_label,
            _status_header: status_header,
            status_value,
            error_value,
            _applications_header: applications_header,
            applications_list,
            add_application,
            remove_application,
            _displays_header: displays_header,
            display_frame,
            display_hint,
            automation_enabled,
            start_with_windows,
            restore_button,
            logs_button,
            save_button,
            cancel_button,
            shutdown_failure_label,
            shutdown_retry_button,
            shutdown_cancel_button,
            shutdown_force_button,
            status_bar,
            file_dialog,
            display_rows: RefCell::new(Vec::new()),
            command_tx,
            event_rx: RefCell::new(event_rx),
            log_directory,
            snapshot: RefCell::new(None),
            draft: RefCell::new(ConfigV2::default()),
            rendered_displays: RefCell::new(Vec::new()),
            save_pending: Cell::new(false),
            shutdown_pending: Cell::new(false),
            shutdown_failure_visible: Cell::new(false),
            display_render_failed: Cell::new(false),
            channel_error_shown: Cell::new(false),
            controller_available: Cell::new(true),
        });

        let handlers = vec![bind_message_events(&inner), bind_settings_events(&inner)];

        let taskbar_created = unsafe {
            windows::Win32::UI::WindowsAndMessaging::RegisterWindowMessageW(w!("TaskbarCreated"))
        };
        let weak = Rc::downgrade(&inner);
        let raw_handler = nwg::bind_raw_event_handler(
            &inner.settings_window.handle,
            RAW_EVENT_HANDLER_ID,
            move |_hwnd, message, wparam, _lparam| {
                let controls = weak.upgrade()?;
                let result = catch_unwind(AssertUnwindSafe(|| {
                    if activation_message != 0 && message == activation_message {
                        controls.show_settings();
                        return Some(0);
                    }
                    if taskbar_created != 0 && message == taskbar_created {
                        controls.readd_tray_icon();
                        return Some(0);
                    }
                    match message {
                        WM_DISPLAYCHANGE | WM_DEVICECHANGE => {
                            controls.send_command(ControllerCommand::RefreshDisplays);
                        }
                        WM_POWERBROADCAST
                            if matches!(
                                wparam as u32,
                                PBT_APMRESUMEAUTOMATIC
                                    | PBT_APMRESUMECRITICAL
                                    | PBT_APMRESUMESTANDBY
                                    | PBT_APMRESUMESUSPEND
                            ) =>
                        {
                            controls.send_command(ControllerCommand::RefreshDisplays);
                        }
                        WM_QUERYENDSESSION => {
                            let (reply_tx, reply_rx) = mpsc::channel();
                            if controls
                                .send_command(ControllerCommand::PrepareForSessionEnd(reply_tx))
                            {
                                let restored = reply_rx
                                    .recv_timeout(Duration::from_secs(4))
                                    .unwrap_or(false);
                                return Some(if restored { 1 } else { 0 });
                            }
                            return Some(0);
                        }
                        WM_ENDSESSION if wparam == 0 => {
                            controls.send_command(ControllerCommand::SessionEndCancelled);
                        }
                        _ => {}
                    }
                    None
                }));
                match result {
                    Ok(value) => value,
                    Err(_) => {
                        controls.handle_ui_panic();
                        Some(0)
                    }
                }
            },
        )?;
        if let Some(window) = inner.settings_window.handle.hwnd() {
            let _ = crate::single_instance::mark_activation_ready(HWND(window.cast()));
        }

        Ok(Self {
            inner,
            handlers: RefCell::new(handlers),
            raw_handlers: RefCell::new(vec![raw_handler]),
        })
    }

    pub fn notice_sender(&self) -> nwg::NoticeSender {
        self.inner.notice.sender()
    }

    pub fn show_settings(&self) {
        self.inner.show_settings();
    }

    pub fn run(&self, show_settings: bool) {
        if show_settings {
            self.inner.show_settings();
        }
        nwg::dispatch_thread_events();
    }
}

impl Deref for TrayApp {
    type Target = nwg::MessageWindow;

    fn deref(&self) -> &Self::Target {
        &self.inner.message_window
    }
}

impl Drop for TrayApp {
    fn drop(&mut self) {
        for handler in self.raw_handlers.borrow_mut().drain(..) {
            let _ = nwg::unbind_raw_event_handler(&handler);
        }
        for handler in self.handlers.borrow_mut().drain(..) {
            nwg::unbind_event_handler(&handler);
        }
    }
}

fn bind_message_events(controls: &Rc<TrayControls>) -> nwg::EventHandler {
    let weak = Rc::downgrade(controls);
    nwg::full_bind_event_handler(
        &controls.message_window.handle,
        move |event, _event_data, handle| {
            let Some(controls) = weak.upgrade() else {
                return;
            };
            let result = catch_unwind(AssertUnwindSafe(|| match event {
                nwg::Event::OnNotice if handle == controls.notice.handle => {
                    controls.consume_controller_events();
                }
                nwg::Event::OnContextMenu if handle == controls.tray.handle => {
                    controls.show_tray_menu();
                }
                nwg::Event::OnMousePress(nwg::MousePressEvent::MousePressLeftUp)
                    if handle == controls.tray.handle =>
                {
                    controls.show_settings();
                }
                nwg::Event::OnMenuItemSelected if handle == controls.tray_settings.handle => {
                    controls.show_settings();
                }
                nwg::Event::OnMenuItemSelected if handle == controls.tray_pause.handle => {
                    controls.toggle_automation();
                }
                nwg::Event::OnMenuItemSelected if handle == controls.tray_restore.handle => {
                    controls.restore_or_recover();
                }
                nwg::Event::OnMenuItemSelected if handle == controls.tray_logs.handle => {
                    controls.open_logs();
                }
                nwg::Event::OnMenuItemSelected if handle == controls.tray_exit.handle => {
                    controls.request_shutdown(false);
                }
                _ => {}
            }));
            if result.is_err() {
                controls.handle_ui_panic();
            }
        },
    )
}

fn bind_settings_events(controls: &Rc<TrayControls>) -> nwg::EventHandler {
    let weak = Rc::downgrade(controls);
    nwg::full_bind_event_handler(
        &controls.settings_window.handle,
        move |event, event_data, handle| {
            let Some(controls) = weak.upgrade() else {
                return;
            };
            let result = catch_unwind(AssertUnwindSafe(|| match event {
                nwg::Event::OnWindowClose if handle == controls.settings_window.handle => {
                    if let nwg::EventData::OnWindowClose(close) = event_data {
                        close.close(false);
                    }
                    if controls.controller_available.get() {
                        controls.dismiss_shutdown_failure();
                        controls.hide_settings();
                    } else {
                        controls.settings_window.restore();
                        controls.settings_window.set_visible(true);
                        controls.settings_window.set_focus();
                    }
                }
                nwg::Event::OnButtonClick if handle == controls.add_application.handle => {
                    controls.add_applications();
                }
                nwg::Event::OnButtonClick if handle == controls.remove_application.handle => {
                    controls.remove_application();
                }
                nwg::Event::OnListBoxSelect if handle == controls.applications_list.handle => {
                    controls.remove_application.set_enabled(
                        !controls.save_pending.get()
                            && !controls.shutdown_failure_visible.get()
                            && controls.applications_list.selection().is_some(),
                    );
                }
                nwg::Event::OnButtonClick if handle == controls.restore_button.handle => {
                    controls.restore_or_recover();
                }
                nwg::Event::OnButtonClick if handle == controls.logs_button.handle => {
                    controls.open_logs();
                }
                nwg::Event::OnButtonClick if handle == controls.save_button.handle => {
                    controls.save_configuration();
                }
                nwg::Event::OnButtonClick if handle == controls.cancel_button.handle => {
                    controls.hide_settings();
                }
                nwg::Event::OnButtonClick if handle == controls.shutdown_retry_button.handle => {
                    controls.dismiss_shutdown_failure();
                    controls.request_shutdown(false);
                }
                nwg::Event::OnButtonClick if handle == controls.shutdown_cancel_button.handle => {
                    controls.dismiss_shutdown_failure();
                }
                nwg::Event::OnButtonClick if handle == controls.shutdown_force_button.handle => {
                    controls.dismiss_shutdown_failure();
                    controls.request_shutdown(true);
                }
                _ => {}
            }));
            if result.is_err() {
                controls.handle_ui_panic();
            }
        },
    )
}

impl TrayControls {
    fn handle_ui_panic(&self) {
        let _ = self
            .command_tx
            .send(ControllerCommand::Shutdown { force: false });
        nwg::stop_thread_dispatch();
    }

    fn send_command(&self, command: ControllerCommand) -> bool {
        if self.command_tx.send(command).is_ok() {
            return true;
        }
        self.show_channel_error();
        false
    }

    fn show_channel_error(&self) {
        self.controller_available.set(false);
        if self.channel_error_shown.replace(true) {
            return;
        }
        self.handle_controller_failure(
            "The controller connection was lost. Display restoration could not be confirmed; restart Monitor Manager to retry recovery.",
        );
        self.tray.show(
            "The controller stopped responding. Restart Monitor Manager to retry recovery.",
            Some("Monitor Manager error"),
            Some(nwg::TrayNotificationFlags::ERROR_ICON),
            None,
        );
    }

    fn consume_controller_events(&self) {
        loop {
            let event = self.event_rx.borrow().try_recv();
            match event {
                Ok(event) => self.handle_controller_event(event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if !self.shutdown_pending.get() {
                        self.show_channel_error();
                    }
                    break;
                }
            }
        }
    }

    fn handle_controller_event(&self, event: ControllerEvent) {
        match event {
            ControllerEvent::Snapshot(snapshot) => self.apply_snapshot(snapshot),
            ControllerEvent::Notification {
                title,
                message,
                is_error,
            } => {
                let flags = if is_error {
                    nwg::TrayNotificationFlags::ERROR_ICON
                } else {
                    nwg::TrayNotificationFlags::INFO_ICON
                };
                self.tray.show(&message, Some(&title), Some(flags), None);
            }
            ControllerEvent::ConfigSaved => {
                self.set_save_pending(false);
                self.hide_settings();
            }
            ControllerEvent::ShutdownReady => {
                nwg::stop_thread_dispatch();
            }
            ControllerEvent::ShutdownFailed(message) => {
                self.shutdown_pending.set(false);
                self.update_tray_state();
                self.handle_shutdown_failure(&message);
            }
            ControllerEvent::ControllerFailed(message) => {
                self.controller_available.set(false);
                self.channel_error_shown.set(true);
                self.shutdown_pending.set(false);
                self.handle_controller_failure(&message);
            }
        }
    }

    fn apply_snapshot(&self, snapshot: UiSnapshot) {
        let previous = self.snapshot.borrow().clone();
        let current_draft = self.settings_window.visible().then(|| self.collect_draft());
        let preserve_draft = match (&previous, &current_draft) {
            (Some(previous), Some(current)) => {
                previous.config == snapshot.config || current != &previous.config
            }
            _ => false,
        };
        if preserve_draft {
            *self.draft.borrow_mut() = current_draft.unwrap_or_default();
        } else {
            *self.draft.borrow_mut() = snapshot.config.clone();
        }

        if self.save_pending.get() && snapshot.last_error.is_some() {
            self.set_save_pending(false);
        }

        let status = snapshot.status.clone();
        let phase = snapshot.phase;
        let error = snapshot.last_error.clone().unwrap_or_default();
        let matched = snapshot
            .matched_triggers
            .iter()
            .filter_map(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let detail = if matched.is_empty() {
            status
        } else {
            format!("{} - matched: {}", status, matched.join(", "))
        };

        self.status_value.set_text(&detail);
        self.error_value.set_text(&error);
        self.status_bar.set_text(0, phase_name(phase));
        *self.snapshot.borrow_mut() = Some(snapshot.clone());

        if self.settings_window.visible() {
            if preserve_draft {
                if *self.rendered_displays.borrow() != snapshot.displays {
                    let draft = self.draft.borrow().clone();
                    if self.rebuild_display_rows(&snapshot.displays, &draft.disabled_displays) {
                        *self.rendered_displays.borrow_mut() = snapshot.displays.clone();
                    }
                }
            } else {
                self.populate_edit_controls(&snapshot.displays);
            }
        }
        self.update_tray_state();
    }

    fn show_settings(&self) {
        if let Some(snapshot) = self.snapshot.borrow().clone() {
            *self.draft.borrow_mut() = snapshot.config.clone();
            self.populate_edit_controls(&snapshot.displays);
        } else {
            self.populate_edit_controls(&[]);
        }
        self.set_save_pending(false);
        self.settings_window.restore();
        self.settings_window.set_visible(true);
        self.settings_window.set_focus();
        if let Some(hwnd) = self.settings_window.handle.hwnd() {
            unsafe {
                let _ = SetForegroundWindow(HWND(hwnd.cast()));
            }
        }
        self.send_command(ControllerCommand::RefreshDisplays);
    }

    fn hide_settings(&self) {
        self.settings_window.set_visible(false);
        if let Some(snapshot) = self.snapshot.borrow().clone() {
            *self.draft.borrow_mut() = snapshot.config;
        }
    }

    fn populate_edit_controls(&self, displays: &[DisplayInfo]) {
        let draft = self.draft.borrow().clone();
        let applications = draft
            .trigger_apps
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        self.applications_list.set_collection(applications);
        self.remove_application.set_enabled(
            !self.shutdown_failure_visible.get() && self.applications_list.selection().is_some(),
        );
        self.automation_enabled
            .set_check_state(checkbox_state(draft.automation_enabled));
        self.start_with_windows
            .set_check_state(checkbox_state(draft.start_with_windows));
        if self.rebuild_display_rows(displays, &draft.disabled_displays) {
            *self.rendered_displays.borrow_mut() = displays.to_vec();
        }
    }

    fn rebuild_display_rows(
        &self,
        displays: &[DisplayInfo],
        configured: &[DisabledDisplay],
    ) -> bool {
        let selected = configured
            .iter()
            .map(|display| (display.device_path.to_lowercase(), display.clone()))
            .collect::<HashMap<_, _>>();
        let mut seen = HashSet::new();
        let mut rows = Vec::new();

        let mut ordered = displays.to_vec();
        ordered.sort_by(|left, right| {
            right
                .is_primary
                .cmp(&left.is_primary)
                .then_with(|| left.friendly_name.cmp(&right.friendly_name))
        });

        for display in ordered {
            let key = display.device_path.to_lowercase();
            seen.insert(key.clone());
            let was_selected = selected.contains_key(&key);
            let selectable = !display.is_protected;
            let state = was_selected && selectable;
            let role = if display.is_primary {
                "Primary - protected"
            } else if display.is_protected {
                "Protected clone"
            } else if display.is_active {
                "Active"
            } else {
                "Inactive"
            };
            rows.push((
                format!("Turn off  {} ({})", display.friendly_name, role),
                DisabledDisplay {
                    device_path: display.device_path,
                    last_known_name: display.friendly_name,
                },
                selectable,
                state,
            ));
        }

        for display in configured {
            if !seen.contains(&display.device_path.to_lowercase()) {
                rows.push((
                    format!(
                        "Turn off  {} (Missing - not connected)",
                        display.last_known_name
                    ),
                    display.clone(),
                    true,
                    true,
                ));
            }
        }

        if rows.is_empty() {
            self.display_rows.borrow_mut().clear();
            self.display_render_failed.set(false);
            self.display_hint
                .set_text("No displays are available. Connect a display and refresh Settings.");
            self.display_hint.set_visible(true);
            return true;
        }

        let column_count = rows.len().div_ceil(DISPLAY_ROWS_PER_COLUMN).max(1);
        let column_width = (765 / column_count as i32).max(120);
        let mut controls = Vec::with_capacity(rows.len());
        for (index, (text, selection, enabled, checked)) in rows.into_iter().enumerate() {
            let column = index / DISPLAY_ROWS_PER_COLUMN;
            let row = index % DISPLAY_ROWS_PER_COLUMN;
            let max_chars = ((column_width - 18).max(80) / 7) as usize;
            let text = truncate_middle(&text, max_chars);
            let mut checkbox = nwg::CheckBox::default();
            if nwg::CheckBox::builder()
                .text(&text)
                .position((8 + column as i32 * column_width, 7 + row as i32 * 23))
                .size((column_width - 12, 22))
                .enabled(enabled)
                .check_state(checkbox_state(checked))
                .font(Some(&self.ui_font))
                .parent(&self.display_frame)
                .build(&mut checkbox)
                .is_ok()
            {
                controls.push(DisplayRow {
                    checkbox,
                    selection,
                    editable: enabled,
                });
            } else {
                self.display_render_failed.set(true);
                self.display_hint.set_text(
                    "Display controls could not be created. Close and reopen Settings to retry.",
                );
                self.display_hint.set_visible(true);
                return false;
            }
        }
        self.display_render_failed.set(false);
        self.display_hint.set_visible(false);
        *self.display_rows.borrow_mut() = controls;
        true
    }

    fn add_applications(&self) {
        if !self.file_dialog.run(Some(&self.settings_window)) {
            return;
        }
        let Ok(paths) = self.file_dialog.get_selected_items() else {
            nwg::modal_error_message(
                &self.settings_window,
                "Could not add application",
                "Windows did not return the selected application path.",
            );
            return;
        };

        let mut draft = self.collect_draft();
        let mut known = draft
            .trigger_apps
            .iter()
            .map(|path| path.to_string_lossy().to_lowercase())
            .collect::<HashSet<_>>();
        for path in paths.into_iter().map(PathBuf::from) {
            let key = path.to_string_lossy().to_lowercase();
            if known.insert(key) {
                draft.trigger_apps.push(path);
            }
        }
        self.applications_list.set_collection(
            draft
                .trigger_apps
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
        );
        *self.draft.borrow_mut() = draft;
    }

    fn remove_application(&self) {
        let Some(index) = self.applications_list.selection() else {
            return;
        };
        let mut draft = self.collect_draft();
        if index < draft.trigger_apps.len() {
            draft.trigger_apps.remove(index);
            self.applications_list.remove(index);
            self.applications_list.set_selection(None);
            self.remove_application.set_enabled(false);
            *self.draft.borrow_mut() = draft;
        }
    }

    fn collect_draft(&self) -> ConfigV2 {
        let mut draft = self.draft.borrow().clone();
        draft.automation_enabled = is_checked(&self.automation_enabled);
        draft.start_with_windows = is_checked(&self.start_with_windows);
        draft.disabled_displays = self
            .display_rows
            .borrow()
            .iter()
            .filter(|row| is_checked(&row.checkbox))
            .map(|row| row.selection.clone())
            .collect();
        draft
    }

    fn save_configuration(&self) {
        if self.display_render_failed.get() {
            nwg::modal_error_message(
                &self.settings_window,
                "Display list unavailable",
                "The display controls could not be created. Close and reopen Settings before saving.",
            );
            return;
        }
        let draft = self.collect_draft();
        if draft.trigger_apps.is_empty() {
            nwg::modal_error_message(
                &self.settings_window,
                "Configuration incomplete",
                "Add at least one application before saving.",
            );
            return;
        }
        if draft.disabled_displays.is_empty() {
            nwg::modal_error_message(
                &self.settings_window,
                "Configuration incomplete",
                "Select at least one non-primary display to turn off.",
            );
            return;
        }
        *self.draft.borrow_mut() = draft.clone();
        if self.send_command(ControllerCommand::SaveConfig(draft)) {
            self.set_save_pending(true);
            self.status_bar.set_text(0, "Saving configuration...");
        }
    }

    fn set_save_pending(&self, pending: bool) {
        self.save_pending.set(pending);
        let enabled = !pending && !self.shutdown_failure_visible.get();
        self.save_button.set_enabled(enabled);
        self.cancel_button.set_enabled(enabled);
        self.add_application.set_enabled(enabled);
        self.remove_application
            .set_enabled(enabled && self.applications_list.selection().is_some());
        self.automation_enabled.set_enabled(enabled);
        self.start_with_windows.set_enabled(enabled);
        for row in self.display_rows.borrow().iter() {
            row.checkbox.set_enabled(enabled && row.editable);
        }
    }

    fn toggle_automation(&self) {
        let Some(snapshot) = self.snapshot.borrow().clone() else {
            return;
        };
        self.send_command(ControllerCommand::SetAutomationEnabled(
            !snapshot.config.automation_enabled,
        ));
    }

    fn restore_or_recover(&self) {
        let snapshot = self.snapshot.borrow();
        let command = match snapshot.as_ref() {
            Some(value)
                if value.phase == AppPhase::Recovering
                    || (value.phase == AppPhase::Error && value.can_restore) =>
            {
                ControllerCommand::RetryRecovery
            }
            Some(value) if value.can_restore || value.phase == AppPhase::Focused => {
                ControllerCommand::RestoreForRun
            }
            _ => return,
        };
        drop(snapshot);
        self.send_command(command);
    }

    fn request_shutdown(&self, force: bool) {
        if !self.controller_available.get() {
            if force {
                nwg::stop_thread_dispatch();
            } else if !self.shutdown_failure_visible.get() {
                self.handle_controller_failure(
                    "The controller is unavailable. Display restoration could not be confirmed; restart Monitor Manager to retry recovery.",
                );
            }
            return;
        }
        if self.shutdown_pending.replace(true) {
            return;
        }
        self.update_tray_state();
        if !self.send_command(ControllerCommand::Shutdown { force }) {
            self.shutdown_pending.set(false);
            self.update_tray_state();
        }
    }

    fn handle_shutdown_failure(&self, message: &str) {
        self.settings_window.restore();
        self.settings_window.set_visible(true);
        self.settings_window.set_focus();
        self.shutdown_failure_visible.set(true);
        self.error_value.set_text(message);
        self.status_bar.set_text(0, "Display restoration failed");
        self.shutdown_failure_label.set_text(
            "Restoration could not be verified. Retry, keep Monitor Manager running, or exit while retaining recovery data.",
        );
        self.set_configuration_controls_enabled(false);
        self.automation_enabled.set_visible(false);
        self.start_with_windows.set_visible(false);
        self.restore_button.set_visible(false);
        self.logs_button.set_visible(false);
        self.save_button.set_visible(false);
        self.cancel_button.set_visible(false);
        self.shutdown_failure_label.set_visible(true);
        self.shutdown_retry_button.set_visible(true);
        self.shutdown_cancel_button.set_visible(true);
        self.shutdown_force_button.set_visible(true);
        self.update_tray_state();
    }

    fn handle_controller_failure(&self, message: &str) {
        self.handle_shutdown_failure(message);
        self.status_bar
            .set_text(0, "Controller unavailable; recovery requires a restart");
        self.shutdown_failure_label.set_text(
            "The controller stopped after emergency restoration failed. Restart Monitor Manager to retry recovery, or exit while retaining recovery data.",
        );
        self.shutdown_retry_button.set_visible(false);
        self.shutdown_cancel_button.set_visible(false);
    }

    fn dismiss_shutdown_failure(&self) {
        if !self.shutdown_failure_visible.replace(false) {
            return;
        }
        self.shutdown_failure_label.set_visible(false);
        self.shutdown_retry_button.set_visible(false);
        self.shutdown_cancel_button.set_visible(false);
        self.shutdown_force_button.set_visible(false);
        self.automation_enabled.set_visible(true);
        self.start_with_windows.set_visible(true);
        self.restore_button.set_visible(true);
        self.logs_button.set_visible(true);
        self.save_button.set_visible(true);
        self.cancel_button.set_visible(true);
        self.set_configuration_controls_enabled(true);
        self.set_save_pending(false);
        self.update_tray_state();
    }

    fn set_configuration_controls_enabled(&self, enabled: bool) {
        self.applications_list.set_enabled(enabled);
        self.add_application.set_enabled(enabled);
        self.remove_application
            .set_enabled(enabled && self.applications_list.selection().is_some());
        self.display_frame.set_enabled(enabled);
        self.automation_enabled.set_enabled(enabled);
        self.start_with_windows.set_enabled(enabled);
        self.save_button.set_enabled(enabled);
        self.cancel_button.set_enabled(enabled);
        for row in self.display_rows.borrow().iter() {
            row.checkbox.set_enabled(enabled && row.editable);
        }
    }

    fn open_logs(&self) {
        if let Err(error) = Command::new("explorer.exe")
            .arg(&self.log_directory)
            .spawn()
        {
            nwg::modal_error_message(
                &self.settings_window,
                "Could not open logs",
                &format!("{}\n\n{}", self.log_directory.display(), error),
            );
        }
    }

    fn show_tray_menu(&self) {
        self.update_tray_state();
        let (x, y) = nwg::GlobalCursor::position();
        self.tray_menu.popup(x, y);
    }

    fn update_tray_state(&self) {
        let snapshot = self.snapshot.borrow();
        let (phase, configured, automation, can_restore) = snapshot
            .as_ref()
            .map(|value| {
                (
                    value.phase,
                    value.config.is_configured() && value.phase != AppPhase::Unconfigured,
                    value.config.automation_enabled,
                    value.can_restore,
                )
            })
            .unwrap_or((AppPhase::Recovering, false, false, false));
        let shutting_down = self.shutdown_pending.get()
            || self.shutdown_failure_visible.get()
            || phase == AppPhase::ShuttingDown;
        let status = phase_name(phase);
        set_menu_item_text(&self.tray_status, &format!("Status: {}", status));
        set_menu_item_text(
            &self.tray_pause,
            if automation {
                "Pause Automation"
            } else {
                "Resume Automation"
            },
        );
        let recovery = phase == AppPhase::Recovering || (phase == AppPhase::Error && can_restore);
        set_menu_item_text(
            &self.tray_restore,
            if recovery {
                "Retry Recovery"
            } else {
                "Restore for This Run"
            },
        );
        self.tray_pause.set_enabled(configured && !shutting_down);
        self.tray_restore
            .set_enabled((can_restore || recovery) && !shutting_down);
        self.tray_settings.set_enabled(!shutting_down);
        self.tray_exit.set_enabled(!shutting_down);
        self.restore_button
            .set_enabled((can_restore || recovery) && !shutting_down);
        self.restore_button.set_text(if recovery {
            "Retry Recovery"
        } else {
            "Restore"
        });
        self.tray.set_tip(&format!("Monitor Manager - {}", status));
    }

    fn readd_tray_icon(&self) {
        let Some(parent) = self.message_window.handle.hwnd() else {
            return;
        };
        let mut data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: HWND(parent.cast()),
            uID: 0,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP,
            uCallbackMessage: NWG_TRAY_MESSAGE,
            hIcon: HICON(self.icon.handle.cast()),
            ..Default::default()
        };
        let status = self
            .snapshot
            .borrow()
            .as_ref()
            .map(|snapshot| phase_name(snapshot.phase))
            .unwrap_or("Starting");
        copy_wide(&mut data.szTip, &format!("Monitor Manager - {}", status));
        unsafe {
            if Shell_NotifyIconW(NIM_ADD, &data).as_bool() {
                data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
                let _ = Shell_NotifyIconW(NIM_SETVERSION, &data);
            }
        }
    }
}

fn checkbox_state(value: bool) -> nwg::CheckBoxState {
    if value {
        nwg::CheckBoxState::Checked
    } else {
        nwg::CheckBoxState::Unchecked
    }
}

fn is_checked(checkbox: &nwg::CheckBox) -> bool {
    checkbox.check_state() == nwg::CheckBoxState::Checked
}

fn phase_name(phase: AppPhase) -> &'static str {
    match phase {
        AppPhase::Unconfigured => "Unconfigured",
        AppPhase::Watching => "Watching",
        AppPhase::Activating => "Activating",
        AppPhase::Focused => "Focused",
        AppPhase::SuppressedUntilClear => "Suppressed Until Clear",
        AppPhase::Paused => "Paused",
        AppPhase::Restoring => "Restoring",
        AppPhase::Recovering => "Recovering",
        AppPhase::Error => "Error",
        AppPhase::ShuttingDown => "Shutting Down",
    }
}

fn truncate_middle(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars || max_chars < 8 {
        return value.to_string();
    }
    let left = (max_chars - 3) / 2;
    let right = max_chars - 3 - left;
    let prefix = value.chars().take(left).collect::<String>();
    let suffix = value
        .chars()
        .skip(count.saturating_sub(right))
        .collect::<String>();
    format!("{}...{}", prefix, suffix)
}

fn set_menu_item_text(item: &nwg::MenuItem, text: &str) {
    let Some((menu, id)) = item.handle.hmenu_item() else {
        return;
    };
    let mut wide = text
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let info = MENUITEMINFOW {
        cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
        fMask: MIIM_STRING,
        dwTypeData: PWSTR(wide.as_mut_ptr()),
        cch: wide.len().saturating_sub(1) as u32,
        ..Default::default()
    };
    unsafe {
        let _ = SetMenuItemInfoW(HMENU(menu.cast()), id, false, &info);
    }
}

fn copy_wide<const N: usize>(target: &mut [u16; N], value: &str) {
    target.fill(0);
    for (slot, character) in target
        .iter_mut()
        .take(N.saturating_sub(1))
        .zip(value.encode_utf16())
    {
        *slot = character;
    }
}
