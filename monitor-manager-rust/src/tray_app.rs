use std::sync::{Arc, Mutex};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem},
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
    let status_item = MenuItem::new("📊 Status: Idle", false, None);
    let quit_item = MenuItem::new("❌ Exit", true, None);

    tray_menu.append(&settings_item).unwrap();
    tray_menu.append(&restore_item).unwrap();
    tray_menu.append(&status_item).unwrap();
    tray_menu.append(&quit_item).unwrap();

    // Load icon from file
    let icon = load_icon_from_file("icon.ico");

    // Build tray icon
    let _tray_icon = TrayIconBuilder::new()
        .with_menu(Box::new(tray_menu))
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

    // Spawn a thread to handle menu events
    let state_clone = Arc::clone(&state);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_millis(50));

            // Handle tray icon click events
            if let Ok(event) = tray_channel.try_recv() {
                if let TrayIconEvent::Click { 
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } = event {
                    // Left click shows settings
                    show_settings_dialog(&state_clone);
                }
            }

            // Handle menu events
            if let Ok(event) = menu_channel.try_recv() {
                if event.id == settings_id {
                    show_settings_dialog(&state_clone);
                } else if event.id == restore_id {
                    let monitor_manager = {
                        let state = state_clone.lock().unwrap();
                        state.monitor_manager.clone()
                    };
                    let manager = monitor_manager.lock().unwrap();
                    let restored = manager.restore_all_monitors();
                    
                    // Show notification
                    unsafe {
                        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                    }
                    if !restored.is_empty() {
                        nwg::simple_message("Monitors Restored", &format!("Restored {} monitors.", restored.len()));
                    } else {
                        nwg::simple_message("Info", "No monitors needed restoration.");
                    }
                } else if event.id == quit_id {
                    std::process::exit(0);
                }
            }
        }
    });

    // Windows message loop for processing tray icon events
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND(std::ptr::null_mut()), 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
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
    #[nwg_control(size: (550, 230), position: (300, 300), title: "Monitor Manager Settings", flags: "WINDOW|VISIBLE", icon: Some(&data.window_icon))]
    #[nwg_events( OnWindowClose: [SettingsDialog::close] )]
    window: nwg::Window,

    #[nwg_resource(source_bin: Some(include_bytes!("../icon.ico")))]
    window_icon: nwg::Icon,

    #[nwg_resource(family: "Segoe UI", size: 22, weight: 700)]
    title_font: nwg::Font,

    #[nwg_resource(family: "Segoe UI", size: 16)]
    ui_font: nwg::Font,

    #[nwg_layout(parent: window, spacing: 10, margin: [20, 20, 20, 20])]
    layout: nwg::GridLayout,

    #[nwg_control(text: "Monitor Manager Configuration", font: Some(&data.title_font))]
    #[nwg_layout_item(layout: layout, row: 0, col: 0, col_span: 4)]
    title_label: nwg::Label,

    #[nwg_control(text: "Select the .exe file to monitor:", font: Some(&data.ui_font))]
    #[nwg_layout_item(layout: layout, row: 1, col: 0, col_span: 4)]
    instruction_label: nwg::Label,

    #[nwg_control(text: "", readonly: false, font: Some(&data.ui_font))]
    #[nwg_layout_item(layout: layout, row: 2, col: 0, col_span: 3)]
    path_input: nwg::TextInput,

    #[nwg_control(text: "Browse...", font: Some(&data.ui_font))]
    #[nwg_layout_item(layout: layout, row: 2, col: 3)]
    #[nwg_events( OnButtonClick: [SettingsDialog::browse] )]
    browse_button: nwg::Button,

    #[nwg_control(text: "Save", font: Some(&data.ui_font))]
    #[nwg_layout_item(layout: layout, row: 4, col: 2)]
    #[nwg_events( OnButtonClick: [SettingsDialog::save] )]
    save_button: nwg::Button,

    #[nwg_control(text: "Cancel", font: Some(&data.ui_font))]
    #[nwg_layout_item(layout: layout, row: 4, col: 3)]
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

    let app = SettingsDialog::build_ui(Default::default()).expect("Failed to build UI");
    
    *app.state.borrow_mut() = Some(state.clone());
    app.path_input.set_text(&current_exe);
    
    nwg::dispatch_thread_events();
}

