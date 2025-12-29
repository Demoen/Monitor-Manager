import sys
import time
import psutil
import ctypes
from ctypes import wintypes
from PyQt5.QtWidgets import (QApplication, QSystemTrayIcon, QMenu, QDialog, 
                              QVBoxLayout, QHBoxLayout, QLabel, QLineEdit, 
                              QPushButton, QGroupBox, QMessageBox, QFileDialog)
from PyQt5.QtCore import QTimer, Qt, QThread, pyqtSignal
from PyQt5.QtGui import QIcon, QFont
import os
import json

# Helper function to get resource path (works with PyInstaller)
def resource_path(relative_path):
    """Get absolute path to resource, works for dev and for PyInstaller"""
    try:
        # PyInstaller creates a temp folder and stores path in _MEIPASS
        base_path = sys._MEIPASS
    except Exception:
        base_path = os.path.abspath(".")
    return os.path.join(base_path, relative_path)

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


class MonitorThread(QThread):
    """Thread to monitor process and manage displays"""
    status_changed = pyqtSignal(str)
    
    def __init__(self, target_exe_path):
        super().__init__()
        self.target_exe_path = target_exe_path
        self.running = True
        self.process_was_running = False
        self.monitors_disabled = False
        
    def set_target_exe(self, path):
        """Update target exe path"""
        self.target_exe_path = path
        
    def is_target_running(self):
        """Check if target process is running"""
        if not self.target_exe_path:
            return False
            
        for proc in psutil.process_iter(['name', 'exe']):
            try:
                if proc.info['exe'] and proc.info['exe'].lower() == self.target_exe_path.lower():
                    return True
            except (psutil.NoSuchProcess, psutil.AccessDenied, psutil.ZombieProcess):
                pass
        return False
    
    def run(self):
        """Main monitoring loop"""
        while self.running:
            process_is_running = self.is_target_running()
            
            # Process just started
            if process_is_running and not self.process_was_running:
                self.status_changed.emit(f"Process detected! Disabling secondary monitors...")
                
                # Get secondary monitors
                monitors = get_all_monitors()
                secondary_monitors = [m for m in monitors if not m['is_primary']]
                
                # Disable all secondary monitors
                for monitor in secondary_monitors:
                    if disable_monitor(monitor['name']):
                        self.status_changed.emit(f"✓ Disabled {monitor['description']}")
                    else:
                        self.status_changed.emit(f"✗ Failed to disable {monitor['description']}")
                
                self.monitors_disabled = True
                self.process_was_running = True
                self.status_changed.emit("Monitoring active - secondary monitors disabled")
            
            # Process just stopped
            elif not process_is_running and self.process_was_running:
                self.status_changed.emit("Process closed. Re-enabling monitors...")
                
                # Get secondary monitors
                monitors = get_all_monitors()
                secondary_monitors = [m for m in monitors if not m['is_primary']]
                
                # Restore all secondary monitors
                for monitor in secondary_monitors:
                    if monitor['name'] in monitor_settings:
                        if restore_monitor(monitor['name'], monitor_settings[monitor['name']]):
                            self.status_changed.emit(f"✓ Restored {monitor['description']}")
                        else:
                            self.status_changed.emit(f"✗ Failed to restore {monitor['description']}")
                
                self.monitors_disabled = False
                self.process_was_running = False
                self.status_changed.emit("Idle - waiting for process")
            
            time.sleep(2)
    
    def stop(self):
        """Stop the monitoring thread"""
        self.running = False
        
        # Restore monitors if they were disabled
        if self.monitors_disabled:
            monitors = get_all_monitors()
            secondary_monitors = [m for m in monitors if not m['is_primary']]
            
            for monitor in secondary_monitors:
                if monitor['name'] in monitor_settings:
                    restore_monitor(monitor['name'], monitor_settings[monitor['name']])


class SettingsDialog(QDialog):
    """Modern settings dialog for configuring the monitor tool"""
    
    def __init__(self, parent=None):
        super().__init__(parent)
        self.setWindowTitle("Monitor Manager Settings")
        self.setFixedWidth(500)
        
        # Set window icon
        icon_path = resource_path('icon.ico')
        if os.path.exists(icon_path):
            self.setWindowIcon(QIcon(icon_path))
        else:
            # Create a simple default icon
            from PyQt5.QtGui import QPixmap, QPainter, QColor
            pixmap = QPixmap(64, 64)
            pixmap.fill(Qt.transparent)
            painter = QPainter(pixmap)
            painter.setBrush(QColor(0, 120, 212))
            painter.setPen(Qt.NoPen)
            painter.drawEllipse(4, 4, 56, 56)
            painter.end()
            self.setWindowIcon(QIcon(pixmap))
        
        self.init_ui()
        self.load_settings()
        
    def init_ui(self):
        """Initialize the UI"""
        layout = QVBoxLayout()
        layout.setSpacing(15)
        
        # Title
        title = QLabel("Monitor Manager Configuration")
        title_font = QFont()
        title_font.setPointSize(12)
        title_font.setBold(True)
        title.setFont(title_font)
        layout.addWidget(title)
        
        # Process Settings Group
        process_group = QGroupBox("Target Process")
        process_layout = QVBoxLayout()
        
        info_label = QLabel("Select the .exe file to monitor:")
        info_label.setWordWrap(True)
        process_layout.addWidget(info_label)
        
        # Exe path input with browse button
        exe_layout = QHBoxLayout()
        self.exe_input = QLineEdit()
        self.exe_input.setPlaceholderText("C:\\Path\\To\\Your\\Application.exe")
        exe_layout.addWidget(self.exe_input)
        
        browse_btn = QPushButton("Browse...")
        browse_btn.clicked.connect(self.browse_exe)
        exe_layout.addWidget(browse_btn)
        
        process_layout.addLayout(exe_layout)
        process_group.setLayout(process_layout)
        layout.addWidget(process_group)
        
        # Monitor Info Group
        monitor_group = QGroupBox("Monitor Information")
        monitor_layout = QVBoxLayout()
        
        monitors = get_all_monitors()
        monitor_layout.addWidget(QLabel(f"Total Monitors: {len(monitors)}"))
        
        for i, monitor in enumerate(monitors):
            role = "PRIMARY" if monitor['is_primary'] else "Secondary"
            label = QLabel(f"  • {monitor['description']} ({role})")
            monitor_layout.addWidget(label)
        
        if len(monitors) <= 1:
            warning = QLabel("⚠️ Only one monitor detected. This tool requires multiple monitors.")
            warning.setStyleSheet("color: orange;")
            monitor_layout.addWidget(warning)
        
        monitor_group.setLayout(monitor_layout)
        layout.addWidget(monitor_group)
        
        # Buttons
        button_layout = QHBoxLayout()
        button_layout.addStretch()
        
        save_btn = QPushButton("Save")
        save_btn.setDefault(True)
        save_btn.clicked.connect(self.save_settings)
        button_layout.addWidget(save_btn)
        
        cancel_btn = QPushButton("Cancel")
        cancel_btn.clicked.connect(self.reject)
        button_layout.addWidget(cancel_btn)
        
        layout.addLayout(button_layout)
        
        self.setLayout(layout)
        
    def browse_exe(self):
        """Open file browser to select exe"""
        file_path, _ = QFileDialog.getOpenFileName(
            self,
            "Select Executable",
            "C:\\",
            "Executable Files (*.exe);;All Files (*.*)"
        )
        if file_path:
            self.exe_input.setText(file_path)
    
    def load_settings(self):
        """Load settings from config file"""
        config_path = os.path.join(os.path.dirname(__file__), 'config.json')
        if os.path.exists(config_path):
            try:
                with open(config_path, 'r') as f:
                    config = json.load(f)
                    self.exe_input.setText(config.get('target_exe', ''))
            except:
                pass
        else:
            # Default to League of Legends path
            default_path = r"C:\Riot Games\League of Legends\Game\League of Legends.exe"
            self.exe_input.setText(default_path)
    
    def save_settings(self):
        """Save settings to config file"""
        exe_path = self.exe_input.text().strip()
        
        if not exe_path:
            QMessageBox.warning(self, "Invalid Path", "Please enter a valid executable path.")
            return
        
        if not os.path.exists(exe_path):
            result = QMessageBox.question(
                self, 
                "File Not Found", 
                f"The file '{exe_path}' does not exist.\n\nDo you want to save it anyway?",
                QMessageBox.Yes | QMessageBox.No
            )
            if result == QMessageBox.No:
                return
        
        config = {'target_exe': exe_path}
        config_path = os.path.join(os.path.dirname(__file__), 'config.json')
        
        try:
            with open(config_path, 'w') as f:
                json.dump(config, f, indent=4)
            self.accept()
        except Exception as e:
            QMessageBox.critical(self, "Error", f"Failed to save settings:\n{str(e)}")


class SystemTrayApp:
    """System tray application for monitor management"""
    
    def __init__(self):
        self.app = QApplication(sys.argv)
        self.app.setQuitOnLastWindowClosed(False)
        
        # Save monitor settings on startup
        save_monitor_settings()
        
        # Load config
        self.config = self.load_config()
        
        # Create system tray icon
        self.tray_icon = QSystemTrayIcon()
        self.setup_tray_icon()
        
        # Start monitor thread
        self.monitor_thread = MonitorThread(self.config.get('target_exe', ''))
        self.monitor_thread.status_changed.connect(self.on_status_changed)
        self.monitor_thread.start()
        
        # Status message for tooltip
        self.current_status = "Idle - waiting for process"
        self.update_tooltip()
    
    def load_config(self):
        """Load configuration from file"""
        config_path = os.path.join(os.path.dirname(__file__), 'config.json')
        if os.path.exists(config_path):
            try:
                with open(config_path, 'r') as f:
                    return json.load(f)
            except:
                pass
        return {'target_exe': r"C:\Riot Games\League of Legends\Game\League of Legends.exe"}
    
    def setup_tray_icon(self):
        """Setup the system tray icon and menu"""
        # Create icon (using a simple colored circle if icon.ico doesn't exist)
        icon_path = resource_path('icon.ico')
        if os.path.exists(icon_path):
            icon = QIcon(icon_path)
        else:
            # Create a simple default icon
            from PyQt5.QtGui import QPixmap, QPainter, QColor
            pixmap = QPixmap(64, 64)
            pixmap.fill(Qt.transparent)
            painter = QPainter(pixmap)
            painter.setBrush(QColor(0, 120, 212))
            painter.setPen(Qt.NoPen)
            painter.drawEllipse(4, 4, 56, 56)
            painter.end()
            icon = QIcon(pixmap)
        
        self.tray_icon.setIcon(icon)
        
        # Create menu
        menu = QMenu()
        
        settings_action = menu.addAction("⚙️ Settings")
        settings_action.triggered.connect(self.show_settings)
        
        menu.addSeparator()
        
        status_action = menu.addAction("📊 Status: Idle")
        status_action.setEnabled(False)
        self.status_menu_action = status_action
        
        menu.addSeparator()
        
        exit_action = menu.addAction("❌ Exit")
        exit_action.triggered.connect(self.exit_app)
        
        self.tray_icon.setContextMenu(menu)
        self.tray_icon.activated.connect(self.on_tray_activated)
        self.tray_icon.show()
    
    def on_tray_activated(self, reason):
        """Handle tray icon activation"""
        if reason == QSystemTrayIcon.Trigger:  # Left click
            self.show_settings()
    
    def show_settings(self):
        """Show settings dialog"""
        dialog = SettingsDialog()
        if dialog.exec_() == QDialog.Accepted:
            # Reload config and update monitor thread
            self.config = self.load_config()
            self.monitor_thread.set_target_exe(self.config.get('target_exe', ''))
            self.tray_icon.showMessage(
                "Settings Saved",
                f"Now monitoring: {os.path.basename(self.config.get('target_exe', 'None'))}",
                QSystemTrayIcon.Information,
                2000
            )
    
    def on_status_changed(self, status):
        """Update status when monitor thread reports changes"""
        self.current_status = status
        self.update_tooltip()
        
        # Update status in menu
        if hasattr(self, 'status_menu_action'):
            self.status_menu_action.setText(f"📊 Status: {status}")
    
    def update_tooltip(self):
        """Update the tray icon tooltip"""
        target = os.path.basename(self.config.get('target_exe', 'Not set'))
        self.tray_icon.setToolTip(f"Monitor Manager\nTarget: {target}\nStatus: {self.current_status}")
    
    def exit_app(self):
        """Exit the application"""
        # Stop monitor thread
        self.monitor_thread.stop()
        self.monitor_thread.wait()
        
        # Quit application
        self.tray_icon.hide()
        self.app.quit()
    
    def run(self):
        """Run the application"""
        return self.app.exec_()


if __name__ == "__main__":
    app = SystemTrayApp()
    sys.exit(app.run())
