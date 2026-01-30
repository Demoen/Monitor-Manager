use std::sync::{Arc, Mutex};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu},
    TrayIconBuilder, Icon as TrayIconImage, TrayIconEvent, MouseButton, MouseButtonState,
};
use native_windows_gui as nwg;
use nwg::NativeUi;
use native_windows_derive::NwgUi;
use std::cell::RefCell;
use crate::AppState;
use std::path::Path;
use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, TranslateMessage, DispatchMessageW, MSG};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};

pub fn run(state: Arc<Mutex<AppState>>) {
    nwg::init().expect("Failed to init Native Windows GUI");
    
    // Create tray menu
    let tray_menu = Menu::new();
    let settings_item = MenuItem::new("⚙️ Settings", true, None);
    let restore_item = MenuItem::new("🔄 Re-enable Monitors", true, None);
    let monitors_submenu = Submenu::new("🖥️ Monitors", true);
    let status_item = MenuItem::new("📊 Status: Idle", false, None);
    let quit_item = MenuItem::new("❌ Exit", true, None);

    tray_menu.append(&settings_item).unwrap();
    tray_menu.append(&restore_item).unwrap();
    tray_menu.append(&monitors_submenu).unwrap();
    tray_menu.append(&status_item).unwrap();
    tray_menu.append(&quit_item).unwrap();

    // Initial population
    refresh_monitors_submenu(&monitors_submenu, &state);

    // Load icon from file
    let icon = load_icon_from_file("icon.ico");

    // Build tray icon
    let tray_icon = TrayIconBuilder::new()
        .with_menu(Box::new(tray_menu))
        // Important UX: left-click should NOT open the context menu.
        // We use left-click to open the settings window directly.
        .with_menu_on_left_click(false)
        .with_tooltip("Monitor Manager\nStatus: Idle")
        .with_icon(icon)
        .build()
        .unwrap();

    // Menu event handler
    let menu_channel = MenuEvent::receiver();
    let tray_channel = TrayIconEvent::receiver();

    // Store menu item IDs for comparison
    let settings_id = settings_item.id().clone();
    let restore_id = restore_item.id().clone();
    let quit_id = quit_item.id().clone();

    // Windows message loop for processing tray icon events
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND(std::ptr::null_mut()), 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);

            // Drain menu events
            while let Ok(event) = menu_channel.try_recv() {
                if event.id == settings_id {
                    show_settings_dialog(&state);
                } else if event.id == restore_id {
                    let monitor_manager = {
                        let state = state.lock().unwrap();
                        state.monitor_manager.clone()
                    };
                    let manager = monitor_manager.lock().unwrap();
                    let restored = manager.restore_all_monitors();

                    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                    if !restored.is_empty() {
                        nwg::simple_message(
                            "Monitors Restored",
                            &format!("Restored {} monitors.", restored.len()),
                        );
                    } else {
                        nwg::simple_message("Info", "No monitors needed restoration.");
                    }

                    // Refresh menu after manual restore
                    refresh_monitors_submenu(&monitors_submenu, &state);
                } else if event.id == quit_id {
                    drop(tray_icon);
                    std::process::exit(0);
                }
            }

            // Drain tray events
            while let Ok(event) = tray_channel.try_recv() {
                match event {
                    TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } => {
                        show_settings_dialog(&state);
                    }
                    TrayIconEvent::Click {
                        button: MouseButton::Right,
                        button_state: MouseButtonState::Down,
                        ..
                    } => {
                        // Right-click opens the context menu: refresh dynamic items right before.
                        refresh_monitors_submenu(&monitors_submenu, &state);

                        // Update status line to current app status
                        let current_status = {
                            let state = state.lock().unwrap();
                            state.status.clone()
                        };
                        status_item.set_text(format!("📊 Status: {}", current_status));
                    }
                    _ => {}
                }
            }
        }
    }
}

fn refresh_monitors_submenu(monitors_submenu: &Submenu, state: &Arc<Mutex<AppState>>) {
    // Clear all submenu items
    while monitors_submenu.remove_at(0).is_some() {}

    let monitors = {
        let monitor_manager = {
            let state = state.lock().unwrap();
            state.monitor_manager.clone()
        };
        let manager = monitor_manager.lock().unwrap();
        manager
            .get_all_monitors()
            .into_iter()
            .filter(|m| m.is_active)
            .collect::<Vec<_>>()
    };

    let header = MenuItem::new(
        format!("Total Monitors: {}", monitors.len()),
        false,
        None,
    );
    let _ = monitors_submenu.append(&header);
    let _ = monitors_submenu.append(&PredefinedMenuItem::separator());

    if monitors.is_empty() {
        let empty = MenuItem::new("No monitors detected", false, None);
        let _ = monitors_submenu.append(&empty);
        return;
    }

    let mut monitors_sorted = monitors;
    monitors_sorted.sort_by(|a, b| {
        b.is_primary
            .cmp(&a.is_primary)
            .then_with(|| a.description.to_lowercase().cmp(&b.description.to_lowercase()))
    });

    for monitor in monitors_sorted {
        let role = if monitor.is_primary { "PRIMARY" } else { "Secondary" };
        let text = format!("• {} ({})", monitor.description, role);
        let item = MenuItem::new(text, false, None);
        let _ = monitors_submenu.append(&item);
    }
}

fn load_icon_from_file(path: &str) -> TrayIconImage {
    if Path::new(path).exists() {
        // Try to load from file
        if let Ok(img) = image::open(path) {
            let rgba = img.to_rgba8();
            let (width, height) = rgba.dimensions();
            let rgba_data = rgba.into_raw();
            
            if let Ok(icon) = TrayIconImage::from_rgba(rgba_data, width, height) {
                return icon;
            }
        }
    }
    
    // Fallback: create default blue circle icon
    create_default_icon()
}

fn create_default_icon() -> TrayIconImage {
    let mut rgba = vec![0u8; 64 * 64 * 4];
    
    // Draw a blue circle
    for y in 0..64 {
        for x in 0..64 {
            let dx = x as f32 - 32.0;
            let dy = y as f32 - 32.0;
            let distance = (dx * dx + dy * dy).sqrt();
            
            if distance < 28.0 {
                let idx = (y * 64 + x) * 4;
                rgba[idx] = 0;      // R
                rgba[idx + 1] = 120; // G
                rgba[idx + 2] = 212; // B
                rgba[idx + 3] = 255; // A
            }
        }
    }
    
    TrayIconImage::from_rgba(rgba, 64, 64).expect("Failed to create icon")
}
#[derive(Default, NwgUi)]
pub struct SettingsDialog {
    #[nwg_control(size: (740, 440), position: (300, 300), title: "Monitor Manager Settings", flags: "WINDOW|VISIBLE", icon: Some(&data.window_icon))]
    #[nwg_events( OnWindowClose: [SettingsDialog::close] )]
    window: nwg::Window,

    #[nwg_resource(source_bin: Some(include_bytes!("../icon.ico")))]
    window_icon: nwg::Icon,

    #[nwg_resource(family: "Segoe UI", size: 22, weight: 700)]
    title_font: nwg::Font,

    #[nwg_resource(family: "Segoe UI", size: 14, weight: 600)]
    section_font: nwg::Font,

    #[nwg_resource(family: "Segoe UI", size: 13)]
    ui_font: nwg::Font,

    #[nwg_layout(parent: window, spacing: 4, margin: [10, 10, 10, 10])]
    layout: nwg::GridLayout,

    #[nwg_control(text: "Monitor Manager", font: Some(&data.title_font))]
    #[nwg_layout_item(layout: layout, row: 0, col: 0, col_span: 6)]
    title_label: nwg::Label,

    #[nwg_control(text: "🎯 Target executable", font: Some(&data.section_font))]
    #[nwg_layout_item(layout: layout, row: 1, col: 0, col_span: 6)]
    target_header: nwg::Label,

    #[nwg_control(text: "", readonly: false, font: Some(&data.ui_font))]
    #[nwg_layout_item(layout: layout, row: 2, col: 0, col_span: 5)]
    path_input: nwg::TextInput,

    #[nwg_control(text: "Browse…", font: Some(&data.ui_font), size: (120, 28))]
    #[nwg_layout_item(layout: layout, row: 2, col: 5)]
    #[nwg_events( OnButtonClick: [SettingsDialog::browse] )]
    browse_button: nwg::Button,

    #[nwg_control(text: "📊 Status", font: Some(&data.section_font))]
    #[nwg_layout_item(layout: layout, row: 3, col: 0, col_span: 6)]
    status_header: nwg::Label,

    #[nwg_control(text: "", readonly: true, font: Some(&data.ui_font))]
    #[nwg_layout_item(layout: layout, row: 4, col: 0, col_span: 6)]
    status_value: nwg::TextInput,

    #[nwg_control(text: "🖥️ Monitors", font: Some(&data.section_font))]
    #[nwg_layout_item(layout: layout, row: 5, col: 0, col_span: 6)]
    monitors_header: nwg::Label,

    #[nwg_control(size: (720, 160), font: Some(&data.ui_font))]
    #[nwg_layout_item(layout: layout, row: 6, col: 0, col_span: 6, row_span: 4)]
    monitors_list: nwg::ListBox<String>,

    #[nwg_control(text: "", font: Some(&data.ui_font))]
    #[nwg_layout_item(layout: layout, row: 10, col: 0, col_span: 4)]
    footer_spacer: nwg::Label,

    #[nwg_control(text: "Save", font: Some(&data.ui_font), size: (110, 30))]
    #[nwg_layout_item(layout: layout, row: 10, col: 4)]
    #[nwg_events( OnButtonClick: [SettingsDialog::save] )]
    save_button: nwg::Button,

    #[nwg_control(text: "Cancel", font: Some(&data.ui_font), size: (110, 30))]
    #[nwg_layout_item(layout: layout, row: 10, col: 5)]
    #[nwg_events( OnButtonClick: [SettingsDialog::close] )]
    cancel_button: nwg::Button,

    #[nwg_resource(title: "Select Executable", action: nwg::FileDialogAction::Open, filters: "Executables(*.exe)")]
    file_dialog: nwg::FileDialog,

    state: RefCell<Option<Arc<Mutex<AppState>>>>,
}

impl SettingsDialog {
    fn browse(&self) {
        if self.file_dialog.run(Some(&self.window)) {
            if let Ok(path) = self.file_dialog.get_selected_item() {
                self.path_input.set_text(&path.to_string_lossy());
            }
        }
    }

    fn save(&self) {
        let path = self.path_input.text();
        if let Some(state) = self.state.borrow().as_ref() {
            let mut state = state.lock().unwrap();
            state.config.target_exe = path.clone();
            let _ = state.config.save();
            nwg::simple_message("Settings Saved", &format!("Now monitoring:\n{}", path));
        }
        nwg::stop_thread_dispatch();
    }

    fn close(&self) {
        nwg::stop_thread_dispatch();
    }
}

fn show_settings_dialog(state: &Arc<Mutex<AppState>>) {
    // Initialize COM on this thread for FileDialog
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    let current_exe = {
        let state = state.lock().unwrap();
        state.config.target_exe.clone()
    };

    let (status_text, monitors_items) = {
        let (status, monitoring, monitor_manager) = {
            let state = state.lock().unwrap();
            (state.status.clone(), state.monitoring, state.monitor_manager.clone())
        };

        let manager = monitor_manager.lock().unwrap();
        let mut monitors = manager.get_all_monitors();
        monitors.sort_by(|a, b| {
            b.is_primary
                .cmp(&a.is_primary)
                .then_with(|| a.description.to_lowercase().cmp(&b.description.to_lowercase()))
        });

        let status_text = format!(
            "{} (Monitoring: {})",
            status,
            if monitoring { "On" } else { "Off" }
        );

        let items = if monitors.is_empty() {
            vec!["No monitors detected".to_string()]
        } else {
            monitors
                .into_iter()
                .map(|m| {
                    let role = if m.is_primary { "PRIMARY" } else { "Secondary" };
                    let active = if m.is_active { "Active" } else { "Disabled" };
                    format!("{}  —  {} / {}  ({})", m.description, role, active, m.device_name)
                })
                .collect::<Vec<_>>()
        };

        (status_text, items)
    };

    let app = SettingsDialog::build_ui(Default::default()).expect("Failed to build UI");
    
    *app.state.borrow_mut() = Some(state.clone());
    app.path_input.set_text(&current_exe);
    app.status_value.set_text(&status_text);

    app.monitors_list.clear();
    for (idx, item) in monitors_items.iter().cloned().enumerate() {
        app.monitors_list.insert(idx, item);
    }
    
    nwg::dispatch_thread_events();
}

