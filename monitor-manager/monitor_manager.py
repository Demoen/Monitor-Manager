import time
import psutil
import ctypes
from ctypes import wintypes

# Constants for monitor management
ENUM_CURRENT_SETTINGS = -1
CDS_UPDATEREGISTRY = 0x01
DISP_CHANGE_SUCCESSFUL = 0
DMDO_DEFAULT = 0
DMDO_90 = 1
DMDO_180 = 2
DMDO_270 = 3
DM_PELSWIDTH = 0x80000
DM_PELSHEIGHT = 0x100000
DM_DISPLAYORIENTATION = 0x00000080
DM_POSITION = 0x00000020

class DEVMODE(ctypes.Structure):
    _fields_ = [
        ('dmDeviceName', ctypes.c_wchar * 32),
        ('dmSpecVersion', ctypes.c_ushort),
        ('dmDriverVersion', ctypes.c_ushort),
        ('dmSize', ctypes.c_ushort),
        ('dmDriverExtra', ctypes.c_ushort),
        ('dmFields', ctypes.c_ulong),
        ('dmPositionX', ctypes.c_long),
        ('dmPositionY', ctypes.c_long),
        ('dmDisplayOrientation', ctypes.c_ulong),
        ('dmDisplayFixedOutput', ctypes.c_ulong),
        ('dmColor', ctypes.c_short),
        ('dmDuplex', ctypes.c_short),
        ('dmYResolution', ctypes.c_short),
        ('dmTTOption', ctypes.c_short),
        ('dmCollate', ctypes.c_short),
        ('dmFormName', ctypes.c_wchar * 32),
        ('dmLogPixels', ctypes.c_ushort),
        ('dmBitsPerPel', ctypes.c_ulong),
        ('dmPelsWidth', ctypes.c_ulong),
        ('dmPelsHeight', ctypes.c_ulong),
        ('dmDisplayFlags', ctypes.c_ulong),
        ('dmDisplayFrequency', ctypes.c_ulong),
        ('dmICMMethod', ctypes.c_ulong),
        ('dmICMIntent', ctypes.c_ulong),
        ('dmMediaType', ctypes.c_ulong),
        ('dmDitherType', ctypes.c_ulong),
        ('dmReserved1', ctypes.c_ulong),
        ('dmReserved2', ctypes.c_ulong),
        ('dmPanningWidth', ctypes.c_ulong),
        ('dmPanningHeight', ctypes.c_ulong),
    ]

# Windows API functions
user32 = ctypes.windll.user32
EnumDisplayDevices = ctypes.windll.user32.EnumDisplayDevicesW
ChangeDisplaySettingsEx = ctypes.windll.user32.ChangeDisplaySettingsExW
EnumDisplaySettings = ctypes.windll.user32.EnumDisplaySettingsW

class DISPLAY_DEVICE(ctypes.Structure):
    _fields_ = [
        ('cb', ctypes.c_ulong),
        ('DeviceName', ctypes.c_wchar * 32),
        ('DeviceString', ctypes.c_wchar * 128),
        ('StateFlags', ctypes.c_ulong),
        ('DeviceID', ctypes.c_wchar * 128),
        ('DeviceKey', ctypes.c_wchar * 128),
    ]

# Store original monitor settings
monitor_settings = {}

def get_all_monitors():
    """Get information about all monitors"""
    monitors = []
    i = 0
    while True:
        device = DISPLAY_DEVICE()
        device.cb = ctypes.sizeof(device)
        if not EnumDisplayDevices(None, i, ctypes.byref(device), 0):
            break
        
        # Check if this is an active display
        if device.StateFlags & 1:  # DISPLAY_DEVICE_ATTACHED_TO_DESKTOP
            is_primary = bool(device.StateFlags & 4) # DISPLAY_DEVICE_PRIMARY_DEVICE
            devmode = DEVMODE()
            devmode.dmSize = ctypes.sizeof(DEVMODE)
            if EnumDisplaySettings(device.DeviceName, ENUM_CURRENT_SETTINGS, ctypes.byref(devmode)):
                monitors.append({
                    'index': i,
                    'name': device.DeviceName,
                    'description': device.DeviceString,
                    'settings': devmode,
                    'is_primary': is_primary
                })
        i += 1
    return monitors

def save_monitor_settings():
    """Save current monitor settings"""
    global monitor_settings
    monitors = get_all_monitors()
    for monitor in monitors:
        monitor_settings[monitor['name']] = monitor['settings']
    print(f"Saved settings for {len(monitor_settings)} monitors")

def disable_monitor(monitor_name):
    """Disable a specific monitor"""
    devmode = DEVMODE()
    devmode.dmSize = ctypes.sizeof(DEVMODE)
    devmode.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_POSITION
    devmode.dmPelsWidth = 0
    devmode.dmPelsHeight = 0
    
    result = ChangeDisplaySettingsEx(monitor_name, ctypes.byref(devmode), None, 0, None)
    return result == DISP_CHANGE_SUCCESSFUL

def restore_monitor(monitor_name, settings):
    """Restore a monitor to its previous settings"""
    result = ChangeDisplaySettingsEx(monitor_name, ctypes.byref(settings), None, 0, None)
    return result == DISP_CHANGE_SUCCESSFUL

def is_lol_running():
    """Check if League of Legends process is running"""
    for proc in psutil.process_iter(['name', 'exe']):
        try:
            if proc.info['exe'] and proc.info['exe'].lower() == r"c:\riot games\league of legends\game\league of legends.exe".lower():
                return True
        except (psutil.NoSuchProcess, psutil.AccessDenied, psutil.ZombieProcess):
            pass
    return False

def main():
    print("League of Legends Monitor Manager")
    print("==================================")
    print("This script will disable ALL secondary monitors when League of Legends is running")
    print("Press Ctrl+C to exit\n")
    
    # Save initial monitor settings
    save_monitor_settings()
    
    monitors = get_all_monitors()
    print(f"Found {len(monitors)} monitor(s):")
    primary_monitor = None
    secondary_monitors = []
    
    for i, monitor in enumerate(monitors):
        role = "PRIMARY" if monitor['is_primary'] else "Secondary"
        print(f"  Monitor {i+1}: {monitor['description']} ({role})")
        if monitor['is_primary']:
            primary_monitor = monitor
        else:
            secondary_monitors.append(monitor)
    print()
    
    if not secondary_monitors:
        print("Warning: No secondary monitors detected. Nothing to disable.")
        print("Continuing anyway...\n")
    
    lol_was_running = False
    monitors_disabled = False
    
    try:
        while True:
            lol_is_running = is_lol_running()
            
            # LoL just started
            if lol_is_running and not lol_was_running:
                print(f"[{time.strftime('%H:%M:%S')}] League of Legends detected!")
                print("⚠️  WARNING: Secondary monitors will be disabled in 5 seconds...")
                print("    Move Discord/apps to MAIN Monitor NOW!")
                for i in range(5, 0, -1):
                    print(f"    {i}...")
                    time.sleep(1)
                
                print("\nDisabling secondary monitors...")
                
                for monitor in secondary_monitors:
                    print(f"  Attempting to disable: {monitor['description']}")
                    result = disable_monitor(monitor['name'])
                    if result:
                        print(f"  ✓ Successfully disabled {monitor['description']}")
                    else:
                        print(f"  ✗ Failed to disable {monitor['description']}")
                        # Retry once
                        time.sleep(0.5)
                        if disable_monitor(monitor['name']):
                            print(f"  ✓ Retry successful")
                
                monitors_disabled = True
                lol_was_running = True
                print("✓ Monitor disabling complete!\n")
            
            # LoL just stopped
            elif not lol_is_running and lol_was_running:
                print(f"\n[{time.strftime('%H:%M:%S')}] League of Legends closed. Re-enabling monitors...")
                
                for monitor in secondary_monitors:
                    if monitor['name'] in monitor_settings:
                        print(f"  Restoring: {monitor['description']}")
                        result = restore_monitor(monitor['name'], monitor_settings[monitor['name']])
                        if result:
                            print(f"  ✓ Restored successfully")
                        else:
                            print(f"  ✗ Failed to restore")
                            time.sleep(0.5)
                            if restore_monitor(monitor['name'], monitor_settings[monitor['name']]):
                                print(f"  ✓ Retry successful")
                
                monitors_disabled = False
                lol_was_running = False
                print("✓ All monitors restored!\n")
            
            time.sleep(2)  # Check every 2 seconds
            
    except KeyboardInterrupt:
        print("\n\nExiting...")
        if monitors_disabled:
            print("Re-enabling monitors before exit...")
            for monitor in secondary_monitors:
                if monitor['name'] in monitor_settings:
                    restore_monitor(monitor['name'], monitor_settings[monitor['name']])
                    print(f"  ✓ Restored {monitor['description']}")
        print("Goodbye!")

if __name__ == "__main__":
    main()
