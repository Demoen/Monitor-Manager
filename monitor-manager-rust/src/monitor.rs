use std::collections::HashMap;
use windows::Win32::Graphics::Gdi::*;
use windows::core::PCWSTR;
use std::mem;

#[derive(Debug, Clone)]
pub struct MonitorInfo {
    pub device_name: String,
    pub description: String,
    pub is_primary: bool,
    pub is_active: bool,
}

pub struct MonitorManager {
    saved_settings: HashMap<String, DEVMODEW>,
}

impl MonitorManager {
    pub fn new() -> Self {
        Self {
            saved_settings: HashMap::new(),
        }
    }

    pub fn save_current_settings(&mut self) {
        let monitors = self.get_all_monitors();
        for monitor in monitors {
            if monitor.is_active {
                if let Some(settings) = self.get_monitor_settings(&monitor.device_name) {
                    self.saved_settings.insert(monitor.device_name.clone(), settings);
                }
            }
        }
    }

    pub fn get_all_monitors(&self) -> Vec<MonitorInfo> {
        let mut monitors = Vec::new();
        let mut i = 0u32;

        loop {
            let mut display_device: DISPLAY_DEVICEW = unsafe { mem::zeroed() };
            display_device.cb = mem::size_of::<DISPLAY_DEVICEW>() as u32;

            unsafe {
                if EnumDisplayDevicesW(PCWSTR::null(), i, &mut display_device, 0).as_bool() {
                    let is_active = (display_device.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP) != 0;
                    let is_primary = (display_device.StateFlags & DISPLAY_DEVICE_PRIMARY_DEVICE) != 0;
                    
                    let device_name = String::from_utf16_lossy(
                        &display_device.DeviceName[..display_device.DeviceName.iter()
                            .position(|&c| c == 0)
                            .unwrap_or(display_device.DeviceName.len())]
                    );

                    let description = String::from_utf16_lossy(
                        &display_device.DeviceString[..display_device.DeviceString.iter()
                            .position(|&c| c == 0)
                            .unwrap_or(display_device.DeviceString.len())]
                    );

                    monitors.push(MonitorInfo {
                        device_name,
                        description,
                        is_primary,
                        is_active,
                    });
                    
                    i += 1;
                } else {
                    break;
                }
            }
        }

        monitors
    }

    fn get_monitor_settings(&self, device_name: &str) -> Option<DEVMODEW> {
        let mut dev_mode: DEVMODEW = unsafe { mem::zeroed() };
        dev_mode.dmSize = mem::size_of::<DEVMODEW>() as u16;

        let device_name_wide: Vec<u16> = device_name.encode_utf16().chain(Some(0)).collect();

        unsafe {
            if EnumDisplaySettingsW(
                PCWSTR(device_name_wide.as_ptr()),
                ENUM_CURRENT_SETTINGS,
                &mut dev_mode,
            ).as_bool() {
                Some(dev_mode)
            } else {
                None
            }
        }
    }

    pub fn disable_monitor(&self, device_name: &str) -> bool {
        let mut dev_mode: DEVMODEW = unsafe { mem::zeroed() };
        dev_mode.dmSize = mem::size_of::<DEVMODEW>() as u16;
        dev_mode.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_POSITION;
        dev_mode.dmPelsWidth = 0;
        dev_mode.dmPelsHeight = 0;

        let device_name_wide: Vec<u16> = device_name.encode_utf16().chain(Some(0)).collect();

        unsafe {
            ChangeDisplaySettingsExW(
                PCWSTR(device_name_wide.as_ptr()),
                Some(&dev_mode),
                None,
                CDS_TYPE(0),
                None,
            ) == DISP_CHANGE_SUCCESSFUL
        }
    }

    pub fn restore_monitor(&self, device_name: &str) -> bool {
        if let Some(settings) = self.saved_settings.get(device_name) {
            let device_name_wide: Vec<u16> = device_name.encode_utf16().chain(Some(0)).collect();

            unsafe {
                ChangeDisplaySettingsExW(
                    PCWSTR(device_name_wide.as_ptr()),
                    Some(settings),
                    None,
                    CDS_TYPE(0),
                    None,
                ) == DISP_CHANGE_SUCCESSFUL
            }
        } else {
            false
        }
    }

    pub fn restore_all_monitors(&self) -> Vec<String> {
        let mut restored = Vec::new();
        // Stage per-monitor changes with NORESET, then flush once — more reliable for multi-monitor restore.
        let stage_flags = CDS_TYPE(CDS_UPDATEREGISTRY.0 | CDS_NORESET.0);

        for (device_name, settings) in &self.saved_settings {
            let device_name_wide: Vec<u16> = device_name.encode_utf16().chain(Some(0)).collect();

            unsafe {
                let result = ChangeDisplaySettingsExW(
                    PCWSTR(device_name_wide.as_ptr()),
                    Some(settings),
                    None,
                    stage_flags,
                    None,
                );

                if result == DISP_CHANGE_SUCCESSFUL {
                    restored.push(device_name.clone());
                }
            }
        }

        unsafe {
            let _ = ChangeDisplaySettingsExW(PCWSTR::null(), None, None, CDS_TYPE(0), None);
        }
        restored
    }
}
