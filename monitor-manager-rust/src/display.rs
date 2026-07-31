use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use windows::core::PCWSTR;
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig, SetDisplayConfig,
    DISPLAYCONFIG_2DREGION, DISPLAYCONFIG_DESKTOP_IMAGE_INFO,
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_MODE_INFO_0,
    DISPLAYCONFIG_MODE_INFO_TYPE_DESKTOP_IMAGE, DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE,
    DISPLAYCONFIG_MODE_INFO_TYPE_TARGET, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_PATH_SOURCE_INFO,
    DISPLAYCONFIG_PATH_SOURCE_INFO_0, DISPLAYCONFIG_PATH_TARGET_INFO,
    DISPLAYCONFIG_PATH_TARGET_INFO_0, DISPLAYCONFIG_PIXELFORMAT, DISPLAYCONFIG_RATIONAL,
    DISPLAYCONFIG_ROTATION, DISPLAYCONFIG_SCALING, DISPLAYCONFIG_SCANLINE_ORDERING,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_SOURCE_MODE, DISPLAYCONFIG_TARGET_DEVICE_NAME,
    DISPLAYCONFIG_TARGET_MODE, DISPLAYCONFIG_TOPOLOGY_ID, DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY,
    DISPLAYCONFIG_VIDEO_SIGNAL_INFO, DISPLAYCONFIG_VIDEO_SIGNAL_INFO_0, QDC_DATABASE_CURRENT,
    QDC_ONLY_ACTIVE_PATHS, QDC_VIRTUAL_MODE_AWARE, QDC_VIRTUAL_REFRESH_RATE_AWARE,
    QUERY_DISPLAY_CONFIG_FLAGS, SDC_APPLY, SDC_USE_DATABASE_CURRENT,
    SDC_USE_SUPPLIED_DISPLAY_CONFIG, SDC_VALIDATE, SDC_VIRTUAL_MODE_AWARE,
    SDC_VIRTUAL_REFRESH_RATE_AWARE, SET_DISPLAY_CONFIG_FLAGS,
};
use windows::Win32::Foundation::{
    BOOL, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_PARAMETER, ERROR_SUCCESS, LUID, POINTL, RECTL,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayDevicesW, DISPLAYCONFIG_PATH_ACTIVE, DISPLAYCONFIG_PATH_CLONE_GROUP_INVALID,
    DISPLAYCONFIG_PATH_DESKTOP_IMAGE_IDX_INVALID, DISPLAYCONFIG_PATH_SOURCE_MODE_IDX_INVALID,
    DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE, DISPLAYCONFIG_PATH_TARGET_MODE_IDX_INVALID,
    DISPLAY_DEVICEW, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICE_PRIMARY_DEVICE,
};

const RECOVERY_VERSION: u32 = 1;
const INVALID_MODE_INDEX: u32 = u32::MAX;
const MAX_QUERY_ATTEMPTS: usize = 8;
const DISPLAYCONFIG_PATH_BOOST_REFRESH_RATE: u32 = 0x10;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DisplayId(pub String);

impl DisplayId {
    pub fn new(device_path: impl AsRef<str>) -> Self {
        Self(device_path.as_ref().trim().to_lowercase())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DisplayId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub id: DisplayId,
    pub friendly_name: String,
    pub device_path: String,
    pub is_active: bool,
    pub is_primary: bool,
    pub is_protected: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DisplayPhase {
    Query,
    Resolve,
    SafetyCheck,
    JournalRead,
    JournalWrite,
    JournalClear,
    Validate,
    Apply,
    Verify,
    Rollback,
    Restore,
    Recover,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RollbackStatus {
    NotRequired,
    Succeeded,
    Failed {
        win32_code: Option<u32>,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayError {
    pub phase: DisplayPhase,
    pub win32_code: Option<u32>,
    pub rollback: RollbackStatus,
    pub affected_display_ids: Vec<DisplayId>,
    pub recovery_journal_retained: bool,
    pub message: String,
}

impl DisplayError {
    pub fn new(phase: DisplayPhase, message: impl Into<String>) -> Self {
        Self {
            phase,
            win32_code: None,
            rollback: RollbackStatus::NotRequired,
            affected_display_ids: Vec::new(),
            recovery_journal_retained: false,
            message: message.into(),
        }
    }

    pub fn with_win32_code(mut self, code: u32) -> Self {
        self.win32_code = Some(code);
        self
    }

    fn at_phase(mut self, phase: DisplayPhase) -> Self {
        self.phase = phase;
        self
    }

    fn with_affected(mut self, affected: &[DisplayId]) -> Self {
        self.affected_display_ids = affected.to_vec();
        self
    }
}

impl fmt::Display for DisplayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.win32_code {
            Some(code) => write!(
                formatter,
                "{} ({:?}, Win32 error {})",
                self.message, self.phase, code
            ),
            None => write!(formatter, "{} ({:?})", self.message, self.phase),
        }
    }
}

impl std::error::Error for DisplayError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryStoreError {
    pub operation: String,
    pub message: String,
}

impl RecoveryStoreError {
    pub fn new(operation: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            operation: operation.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for RecoveryStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.operation, self.message)
    }
}

impl std::error::Error for RecoveryStoreError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransitionStatus {
    NoChange,
    Activated,
    AlreadyFocused,
    Restored,
    Recovered,
    AlreadyRestored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryTopologyState {
    NoJournal,
    Original,
    Focused,
    Diverged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionOutcome {
    pub status: TransitionStatus,
    pub affected_display_ids: Vec<DisplayId>,
    pub missing_display_ids: Vec<DisplayId>,
    pub original_fingerprint: Option<TopologyFingerprint>,
    pub focused_fingerprint: Option<TopologyFingerprint>,
    pub used_database_fallback: bool,
}

impl TransitionOutcome {
    fn new(status: TransitionStatus) -> Self {
        Self {
            status,
            affected_display_ids: Vec::new(),
            missing_display_ids: Vec::new(),
            original_fingerprint: None,
            focused_fingerprint: None,
            used_database_fallback: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TopologyFingerprint(pub String);

impl fmt::Display for TopologyFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AdapterId {
    pub low_part: u32,
    pub high_part: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SourceKey {
    pub adapter: AdapterId,
    pub id: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum LogicalSourceGroup {
    Legacy(SourceKey),
    Virtual(u16),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourcePathSnapshot {
    pub adapter: AdapterId,
    pub id: u32,
    pub mode_reference: SourceModeReference,
    pub status_flags: u32,
    pub gdi_device_name: String,
    pub is_primary: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceModeReference {
    Legacy {
        mode_info_idx: u32,
    },
    Virtual {
        clone_group_id: u16,
        source_mode_info_idx: u16,
    },
}

impl SourceModeReference {
    fn from_raw(raw: u32, virtual_mode: bool) -> Self {
        if virtual_mode {
            Self::Virtual {
                clone_group_id: raw as u16,
                source_mode_info_idx: (raw >> 16) as u16,
            }
        } else {
            Self::Legacy { mode_info_idx: raw }
        }
    }

    fn raw(self) -> u32 {
        match self {
            Self::Legacy { mode_info_idx } => mode_info_idx,
            Self::Virtual {
                clone_group_id,
                source_mode_info_idx,
            } => u32::from(clone_group_id) | (u32::from(source_mode_info_idx) << 16),
        }
    }

    fn source_mode_index(self) -> Option<u32> {
        match self {
            Self::Legacy { mode_info_idx } if mode_info_idx != INVALID_MODE_INDEX => {
                Some(mode_info_idx)
            }
            Self::Virtual {
                source_mode_info_idx,
                ..
            } if source_mode_info_idx != DISPLAYCONFIG_PATH_SOURCE_MODE_IDX_INVALID as u16 => {
                Some(u32::from(source_mode_info_idx))
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TargetModeReference {
    Legacy {
        mode_info_idx: u32,
    },
    Virtual {
        desktop_mode_info_idx: u16,
        target_mode_info_idx: u16,
    },
}

impl TargetModeReference {
    fn from_raw(raw: u32, virtual_mode: bool) -> Self {
        if virtual_mode {
            Self::Virtual {
                desktop_mode_info_idx: raw as u16,
                target_mode_info_idx: (raw >> 16) as u16,
            }
        } else {
            Self::Legacy { mode_info_idx: raw }
        }
    }

    fn raw(self) -> u32 {
        match self {
            Self::Legacy { mode_info_idx } => mode_info_idx,
            Self::Virtual {
                desktop_mode_info_idx,
                target_mode_info_idx,
            } => u32::from(desktop_mode_info_idx) | (u32::from(target_mode_info_idx) << 16),
        }
    }

    fn target_mode_index(self) -> Option<u32> {
        match self {
            Self::Legacy { mode_info_idx } if mode_info_idx != INVALID_MODE_INDEX => {
                Some(mode_info_idx)
            }
            Self::Virtual {
                target_mode_info_idx,
                ..
            } if target_mode_info_idx != DISPLAYCONFIG_PATH_TARGET_MODE_IDX_INVALID as u16 => {
                Some(u32::from(target_mode_info_idx))
            }
            _ => None,
        }
    }

    fn desktop_mode_index(self) -> Option<u32> {
        match self {
            Self::Virtual {
                desktop_mode_info_idx,
                ..
            } if desktop_mode_info_idx != DISPLAYCONFIG_PATH_DESKTOP_IMAGE_IDX_INVALID as u16 => {
                Some(u32::from(desktop_mode_info_idx))
            }
            _ => None,
        }
    }
}

impl SourcePathSnapshot {
    fn key(&self) -> SourceKey {
        SourceKey {
            adapter: self.adapter,
            id: self.id,
        }
    }

    fn logical_group(&self) -> LogicalSourceGroup {
        match self.mode_reference {
            SourceModeReference::Virtual {
                clone_group_id,
                source_mode_info_idx,
            } if source_mode_info_idx == DISPLAYCONFIG_PATH_SOURCE_MODE_IDX_INVALID as u16
                && clone_group_id != DISPLAYCONFIG_PATH_CLONE_GROUP_INVALID as u16 =>
            {
                LogicalSourceGroup::Virtual(clone_group_id)
            }
            _ => LogicalSourceGroup::Legacy(self.key()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetPathSnapshot {
    pub adapter: AdapterId,
    pub id: u32,
    pub mode_reference: TargetModeReference,
    pub output_technology: i32,
    pub rotation: i32,
    pub scaling: i32,
    pub refresh_numerator: u32,
    pub refresh_denominator: u32,
    pub scanline_ordering: i32,
    pub target_available: bool,
    pub status_flags: u32,
    pub friendly_name: String,
    pub device_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathSnapshot {
    pub source: SourcePathSnapshot,
    pub target: TargetPathSnapshot,
    pub flags: u32,
}

impl PathSnapshot {
    fn display_id(&self) -> DisplayId {
        DisplayId::new(&self.target.device_path)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PointSnapshot {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RectSnapshot {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionSnapshot {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceModeSnapshot {
    pub width: u32,
    pub height: u32,
    pub pixel_format: i32,
    pub position: PointSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetModeSnapshot {
    pub pixel_rate: u64,
    pub hsync_numerator: u32,
    pub hsync_denominator: u32,
    pub vsync_numerator: u32,
    pub vsync_denominator: u32,
    pub active_size: RegionSnapshot,
    pub total_size: RegionSnapshot,
    pub video_standard: u32,
    pub scanline_ordering: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesktopImageModeSnapshot {
    pub path_source_size: PointSnapshot,
    pub desktop_image_region: RectSnapshot,
    pub desktop_image_clip: RectSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ModeValueSnapshot {
    Source(SourceModeSnapshot),
    Target(TargetModeSnapshot),
    DesktopImage(DesktopImageModeSnapshot),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModeReferenceKind {
    Source,
    Target,
    DesktopImage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeSnapshot {
    pub adapter: AdapterId,
    pub id: u32,
    pub value: ModeValueSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologySnapshot {
    pub paths: Vec<PathSnapshot>,
    pub modes: Vec<ModeSnapshot>,
    pub virtual_mode_aware: bool,
    pub virtual_refresh_rate_aware: bool,
}

impl TopologySnapshot {
    pub fn fingerprint(&self) -> Result<TopologyFingerprint, DisplayError> {
        self.validate()?;
        let source_groups = self.source_groups();
        let mut encoded_paths = Vec::with_capacity(self.paths.len());

        for path in &self.paths {
            let mut bytes = Vec::new();
            push_string(&mut bytes, &path.display_id().0);
            let group = source_groups
                .get(&path.source.logical_group())
                .ok_or_else(|| {
                    DisplayError::new(DisplayPhase::SafetyCheck, "display source group is missing")
                })?;
            push_u32(&mut bytes, group.len() as u32);
            for display_id in group {
                push_string(&mut bytes, display_id);
            }
            push_i32(&mut bytes, path.target.output_technology);
            push_i32(&mut bytes, path.target.rotation);
            push_i32(&mut bytes, path.target.scaling);
            push_u32(&mut bytes, path.target.refresh_numerator);
            push_u32(&mut bytes, path.target.refresh_denominator);
            push_i32(&mut bytes, path.target.scanline_ordering);
            push_bool(&mut bytes, path.target.target_available);
            push_u32(&mut bytes, path.flags);
            match path.source.mode_reference {
                SourceModeReference::Legacy { mode_info_idx } => {
                    bytes.push(0);
                    self.push_optional_mode(
                        &mut bytes,
                        (mode_info_idx != INVALID_MODE_INDEX).then_some(mode_info_idx),
                    )?;
                }
                SourceModeReference::Virtual {
                    clone_group_id,
                    source_mode_info_idx,
                } => {
                    bytes.push(1);
                    push_u16(&mut bytes, clone_group_id);
                    self.push_optional_mode(
                        &mut bytes,
                        (source_mode_info_idx != DISPLAYCONFIG_PATH_SOURCE_MODE_IDX_INVALID as u16)
                            .then_some(u32::from(source_mode_info_idx)),
                    )?;
                }
            }
            match path.target.mode_reference {
                TargetModeReference::Legacy { mode_info_idx } => {
                    bytes.push(0);
                    self.push_optional_mode(
                        &mut bytes,
                        (mode_info_idx != INVALID_MODE_INDEX).then_some(mode_info_idx),
                    )?;
                }
                TargetModeReference::Virtual {
                    desktop_mode_info_idx,
                    target_mode_info_idx,
                } => {
                    bytes.push(1);
                    if desktop_mode_info_idx == DISPLAYCONFIG_PATH_DESKTOP_IMAGE_IDX_INVALID as u16
                    {
                        bytes.push(0);
                    } else {
                        bytes.push(1);
                        self.push_mode(&mut bytes, u32::from(desktop_mode_info_idx))?;
                    }
                    self.push_optional_mode(
                        &mut bytes,
                        (target_mode_info_idx != DISPLAYCONFIG_PATH_TARGET_MODE_IDX_INVALID as u16)
                            .then_some(u32::from(target_mode_info_idx)),
                    )?;
                }
            }
            encoded_paths.push(bytes);
        }

        encoded_paths.sort();
        let mut hasher = Fnv1a64::new();
        hasher.write_u32(encoded_paths.len() as u32);
        for path in encoded_paths {
            hasher.write_u32(path.len() as u32);
            hasher.write(&path);
        }
        Ok(TopologyFingerprint(format!("{:016x}", hasher.finish())))
    }

    pub fn display_infos(&self) -> Result<Vec<DisplayInfo>, DisplayError> {
        self.validate()?;
        let mut displays = BTreeMap::<DisplayId, DisplayInfo>::new();
        for path in &self.paths {
            let id = path.display_id();
            let info = DisplayInfo {
                id: id.clone(),
                friendly_name: path.target.friendly_name.clone(),
                device_path: path.target.device_path.clone(),
                is_active: path.flags & DISPLAYCONFIG_PATH_ACTIVE != 0,
                is_primary: path.source.is_primary,
                is_protected: path.source.is_primary,
            };
            displays.entry(id).or_insert(info);
        }
        Ok(displays.into_values().collect())
    }

    fn validate(&self) -> Result<(), DisplayError> {
        if self.paths.is_empty() {
            return Err(DisplayError::new(
                DisplayPhase::SafetyCheck,
                "the active display topology has no paths",
            ));
        }

        let mut primary_sources = BTreeSet::new();
        let mut display_ids = BTreeSet::new();
        for path in &self.paths {
            if path.flags & DISPLAYCONFIG_PATH_ACTIVE == 0 {
                return Err(DisplayError::new(
                    DisplayPhase::SafetyCheck,
                    "an inactive path was present in the active topology",
                ));
            }
            if path.target.device_path.trim().is_empty() {
                return Err(DisplayError::new(
                    DisplayPhase::Resolve,
                    "a display has no CCD monitor device path",
                ));
            }
            let display_id = path.display_id();
            if !display_ids.insert(display_id.clone()) {
                return Err(DisplayError::new(
                    DisplayPhase::Resolve,
                    format!("duplicate active path for display {display_id}"),
                ));
            }
            if path.source.is_primary {
                primary_sources.insert(path.source.key());
            }
            let path_virtual = matches!(
                path.source.mode_reference,
                SourceModeReference::Virtual { .. }
            ) || matches!(
                path.target.mode_reference,
                TargetModeReference::Virtual { .. }
            );
            if path_virtual
                != (self.virtual_mode_aware
                    && path.flags & DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE != 0)
            {
                return Err(DisplayError::new(
                    DisplayPhase::SafetyCheck,
                    "display path mode references do not match its virtual-mode flags",
                ));
            }
            let source_mode_index = path.source.mode_reference.source_mode_index();
            let target_mode_index = path.target.mode_reference.target_mode_index();
            let desktop_mode_index = path.target.mode_reference.desktop_mode_index();
            if source_mode_index.is_none()
                && (target_mode_index.is_some() || desktop_mode_index.is_some())
            {
                return Err(DisplayError::new(
                    DisplayPhase::SafetyCheck,
                    "a display path cannot specify a target mode without a source mode",
                ));
            }
            if let Some(source_mode_index) = source_mode_index {
                self.validate_mode_reference(
                    source_mode_index,
                    path.source.adapter,
                    path.source.id,
                    ModeReferenceKind::Source,
                )?;
            } else if let SourceModeReference::Virtual { clone_group_id, .. } =
                path.source.mode_reference
            {
                if clone_group_id == DISPLAYCONFIG_PATH_CLONE_GROUP_INVALID as u16 {
                    return Err(DisplayError::new(
                        DisplayPhase::SafetyCheck,
                        "a virtual path without a source mode has no clone group",
                    ));
                }
            }
            if let Some(target_mode_index) = target_mode_index {
                self.validate_mode_reference(
                    target_mode_index,
                    path.target.adapter,
                    path.target.id,
                    ModeReferenceKind::Target,
                )?;
            }
            if let Some(desktop_mode_index) = desktop_mode_index {
                self.validate_mode_reference(
                    desktop_mode_index,
                    path.source.adapter,
                    path.source.id,
                    ModeReferenceKind::DesktopImage,
                )?;
            }
        }

        if self.virtual_refresh_rate_aware && !self.virtual_mode_aware {
            return Err(DisplayError::new(
                DisplayPhase::SafetyCheck,
                "virtual refresh-rate awareness requires virtual-mode awareness",
            ));
        }
        if self.paths.iter().any(|path| {
            path.flags & DISPLAYCONFIG_PATH_BOOST_REFRESH_RATE != 0
                && !self.virtual_refresh_rate_aware
        }) {
            return Err(DisplayError::new(
                DisplayPhase::SafetyCheck,
                "a boosted refresh-rate path requires refresh-rate awareness",
            ));
        }
        if primary_sources.len() != 1 {
            return Err(DisplayError::new(
                DisplayPhase::SafetyCheck,
                format!(
                    "expected one unambiguous primary display source, found {}",
                    primary_sources.len()
                ),
            ));
        }
        Ok(())
    }

    fn validate_mode_reference(
        &self,
        index: u32,
        adapter: AdapterId,
        id: u32,
        expected_kind: ModeReferenceKind,
    ) -> Result<(), DisplayError> {
        if index == INVALID_MODE_INDEX {
            return Err(DisplayError::new(
                DisplayPhase::SafetyCheck,
                "an active display path has no mode",
            ));
        }
        let mode = self.modes.get(index as usize).ok_or_else(|| {
            DisplayError::new(
                DisplayPhase::SafetyCheck,
                format!("display path references missing mode index {index}"),
            )
        })?;
        let kind_matches = matches!(
            (&mode.value, expected_kind),
            (ModeValueSnapshot::Source(_), ModeReferenceKind::Source)
                | (ModeValueSnapshot::Target(_), ModeReferenceKind::Target)
                | (
                    ModeValueSnapshot::DesktopImage(_),
                    ModeReferenceKind::DesktopImage
                )
        );
        let endpoint_matches = expected_kind == ModeReferenceKind::DesktopImage
            || (mode.adapter == adapter && mode.id == id);
        if !endpoint_matches || !kind_matches {
            return Err(DisplayError::new(
                DisplayPhase::SafetyCheck,
                format!("display path mode index {index} does not match its endpoint"),
            ));
        }
        Ok(())
    }

    fn source_groups(&self) -> BTreeMap<LogicalSourceGroup, Vec<String>> {
        let mut groups = BTreeMap::<LogicalSourceGroup, Vec<String>>::new();
        for path in &self.paths {
            groups
                .entry(path.source.logical_group())
                .or_default()
                .push(path.display_id().0);
        }
        for members in groups.values_mut() {
            members.sort();
        }
        groups
    }

    fn push_mode(&self, output: &mut Vec<u8>, index: u32) -> Result<(), DisplayError> {
        let mode = self.modes.get(index as usize).ok_or_else(|| {
            DisplayError::new(
                DisplayPhase::SafetyCheck,
                format!("display path references missing mode index {index}"),
            )
        })?;
        match &mode.value {
            ModeValueSnapshot::Source(value) => {
                output.push(1);
                push_u32(output, value.width);
                push_u32(output, value.height);
                push_i32(output, value.pixel_format);
                push_i32(output, value.position.x);
                push_i32(output, value.position.y);
            }
            ModeValueSnapshot::Target(value) => {
                output.push(2);
                push_u64(output, value.pixel_rate);
                push_u32(output, value.hsync_numerator);
                push_u32(output, value.hsync_denominator);
                push_u32(output, value.vsync_numerator);
                push_u32(output, value.vsync_denominator);
                push_u32(output, value.active_size.width);
                push_u32(output, value.active_size.height);
                push_u32(output, value.video_standard);
                push_i32(output, value.scanline_ordering);
            }
            ModeValueSnapshot::DesktopImage(value) => {
                output.push(3);
                push_i32(output, value.path_source_size.x);
                push_i32(output, value.path_source_size.y);
                push_i32(output, value.desktop_image_region.left);
                push_i32(output, value.desktop_image_region.top);
                push_i32(output, value.desktop_image_region.right);
                push_i32(output, value.desktop_image_region.bottom);
                push_i32(output, value.desktop_image_clip.left);
                push_i32(output, value.desktop_image_clip.top);
                push_i32(output, value.desktop_image_clip.right);
                push_i32(output, value.desktop_image_clip.bottom);
            }
        }
        Ok(())
    }

    fn push_optional_mode(
        &self,
        output: &mut Vec<u8>,
        index: Option<u32>,
    ) -> Result<(), DisplayError> {
        match index {
            Some(index) => {
                output.push(1);
                self.push_mode(output, index)
            }
            None => {
                output.push(0);
                Ok(())
            }
        }
    }

    fn without_displays(&self, selected: &HashSet<String>) -> Result<Self, DisplayError> {
        let kept_paths = self
            .paths
            .iter()
            .filter(|path| !selected.contains(path.display_id().as_str()))
            .cloned()
            .collect::<Vec<_>>();

        if kept_paths.is_empty() {
            return Err(DisplayError::new(
                DisplayPhase::SafetyCheck,
                "focus mode would remove every active display path",
            ));
        }
        if !kept_paths.iter().any(|path| path.target.target_available) {
            return Err(DisplayError::new(
                DisplayPhase::SafetyCheck,
                "focus mode would leave no available display target",
            ));
        }

        let mut modes = Vec::new();
        let mut remapped_indices = HashMap::<u32, u32>::new();
        let mut paths = kept_paths;
        for path in &mut paths {
            path.source.mode_reference = remap_source_reference(
                path.source.mode_reference,
                &self.modes,
                &mut modes,
                &mut remapped_indices,
            )?;
            path.target.mode_reference = remap_target_reference(
                path.target.mode_reference,
                &self.modes,
                &mut modes,
                &mut remapped_indices,
            )?;
        }

        let reduced = Self {
            paths,
            modes,
            virtual_mode_aware: self.virtual_mode_aware,
            virtual_refresh_rate_aware: self.virtual_refresh_rate_aware,
        };
        reduced.validate()?;
        Ok(reduced)
    }
}

fn remap_source_reference(
    reference: SourceModeReference,
    source: &[ModeSnapshot],
    target: &mut Vec<ModeSnapshot>,
    remapped: &mut HashMap<u32, u32>,
) -> Result<SourceModeReference, DisplayError> {
    match reference {
        SourceModeReference::Legacy { mode_info_idx } => Ok(SourceModeReference::Legacy {
            mode_info_idx: remap_mode(mode_info_idx, source, target, remapped)?,
        }),
        SourceModeReference::Virtual {
            clone_group_id,
            source_mode_info_idx,
        } => Ok(SourceModeReference::Virtual {
            clone_group_id,
            source_mode_info_idx: remap_virtual_mode(
                source_mode_info_idx,
                DISPLAYCONFIG_PATH_SOURCE_MODE_IDX_INVALID as u16,
                source,
                target,
                remapped,
            )?,
        }),
    }
}

fn remap_target_reference(
    reference: TargetModeReference,
    source: &[ModeSnapshot],
    target: &mut Vec<ModeSnapshot>,
    remapped: &mut HashMap<u32, u32>,
) -> Result<TargetModeReference, DisplayError> {
    match reference {
        TargetModeReference::Legacy { mode_info_idx } => Ok(TargetModeReference::Legacy {
            mode_info_idx: remap_mode(mode_info_idx, source, target, remapped)?,
        }),
        TargetModeReference::Virtual {
            desktop_mode_info_idx,
            target_mode_info_idx,
        } => Ok(TargetModeReference::Virtual {
            desktop_mode_info_idx: remap_virtual_mode(
                desktop_mode_info_idx,
                DISPLAYCONFIG_PATH_DESKTOP_IMAGE_IDX_INVALID as u16,
                source,
                target,
                remapped,
            )?,
            target_mode_info_idx: remap_virtual_mode(
                target_mode_info_idx,
                DISPLAYCONFIG_PATH_TARGET_MODE_IDX_INVALID as u16,
                source,
                target,
                remapped,
            )?,
        }),
    }
}

fn remap_virtual_mode(
    index: u16,
    invalid: u16,
    source: &[ModeSnapshot],
    target: &mut Vec<ModeSnapshot>,
    remapped: &mut HashMap<u32, u32>,
) -> Result<u16, DisplayError> {
    if index == invalid {
        return Ok(index);
    }
    let remapped = remap_mode(u32::from(index), source, target, remapped)?;
    if remapped >= u32::from(invalid) {
        return Err(DisplayError::new(
            DisplayPhase::SafetyCheck,
            "virtual display topology has too many mode records",
        ));
    }
    u16::try_from(remapped).map_err(|_| {
        DisplayError::new(
            DisplayPhase::SafetyCheck,
            "virtual display topology has too many mode records",
        )
    })
}

fn remap_mode(
    index: u32,
    source: &[ModeSnapshot],
    target: &mut Vec<ModeSnapshot>,
    remapped: &mut HashMap<u32, u32>,
) -> Result<u32, DisplayError> {
    if index == INVALID_MODE_INDEX {
        return Ok(index);
    }
    if let Some(mapped) = remapped.get(&index) {
        return Ok(*mapped);
    }
    let mode = source.get(index as usize).ok_or_else(|| {
        DisplayError::new(
            DisplayPhase::SafetyCheck,
            format!("display path references missing mode index {index}"),
        )
    })?;
    let mapped = target.len() as u32;
    target.push(mode.clone());
    remapped.insert(index, mapped);
    Ok(mapped)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryRecord {
    pub version: u32,
    pub original_topology: TopologySnapshot,
    pub original_fingerprint: TopologyFingerprint,
    pub focused_fingerprint: TopologyFingerprint,
    pub affected_display_ids: Vec<DisplayId>,
}

impl RecoveryRecord {
    fn validate(&self) -> Result<(), DisplayError> {
        if self.version != RECOVERY_VERSION {
            return Err(DisplayError::new(
                DisplayPhase::JournalRead,
                format!("unsupported recovery journal version {}", self.version),
            ));
        }
        let original_fingerprint = self.original_topology.fingerprint()?;
        if original_fingerprint != self.original_fingerprint {
            return Err(DisplayError::new(
                DisplayPhase::JournalRead,
                "recovery journal original fingerprint does not match its topology",
            ));
        }
        let affected = self
            .affected_display_ids
            .iter()
            .map(|id| DisplayId::new(id.as_str()).0)
            .collect::<HashSet<_>>();
        if affected.len() != self.affected_display_ids.len() || affected.is_empty() {
            return Err(DisplayError::new(
                DisplayPhase::JournalRead,
                "recovery journal has invalid affected display identifiers",
            ));
        }
        let existing = self
            .original_topology
            .paths
            .iter()
            .map(|path| path.display_id().0)
            .collect::<HashSet<_>>();
        if !affected.is_subset(&existing) {
            return Err(DisplayError::new(
                DisplayPhase::JournalRead,
                "recovery journal references a display outside its original topology",
            ));
        }
        if self
            .original_topology
            .paths
            .iter()
            .any(|path| path.source.is_primary && affected.contains(path.display_id().as_str()))
        {
            return Err(DisplayError::new(
                DisplayPhase::JournalRead,
                "recovery journal attempts to disable a protected primary path",
            ));
        }
        let focused = self.original_topology.without_displays(&affected)?;
        if focused.fingerprint()? != self.focused_fingerprint {
            return Err(DisplayError::new(
                DisplayPhase::JournalRead,
                "recovery journal focused fingerprint does not match its recorded transition",
            ));
        }
        Ok(())
    }
}

pub trait DisplayBackend {
    fn query_topology(&mut self) -> Result<TopologySnapshot, DisplayError>;
    fn query_database_topology(&mut self) -> Result<TopologySnapshot, DisplayError>;
    fn validate_topology(&mut self, topology: &TopologySnapshot) -> Result<(), DisplayError>;
    fn apply_topology(&mut self, topology: &TopologySnapshot) -> Result<(), DisplayError>;
    fn apply_database_current(
        &mut self,
        database_topology: &TopologySnapshot,
    ) -> Result<(), DisplayError>;
}

pub trait RecoveryStore {
    fn load(&mut self) -> Result<Option<RecoveryRecord>, RecoveryStoreError>;
    fn save(&mut self, record: &RecoveryRecord) -> Result<(), RecoveryStoreError>;
    fn clear(&mut self) -> Result<(), RecoveryStoreError>;
}

pub struct DisplayManager<B = NativeWindowsBackend, S = JsonRecoveryStore> {
    backend: B,
    recovery_store: S,
}

impl DisplayManager<NativeWindowsBackend, JsonRecoveryStore> {
    pub fn native(recovery_path: impl Into<PathBuf>) -> Self {
        Self::new(NativeWindowsBackend, JsonRecoveryStore::new(recovery_path))
    }
}

impl<B: DisplayBackend, S: RecoveryStore> DisplayManager<B, S> {
    pub fn new(backend: B, recovery_store: S) -> Self {
        Self {
            backend,
            recovery_store,
        }
    }

    pub fn into_parts(self) -> (B, S) {
        (self.backend, self.recovery_store)
    }

    pub fn list_displays(&mut self) -> Result<Vec<DisplayInfo>, DisplayError> {
        self.backend.query_topology()?.display_infos()
    }

    pub fn activate(
        &mut self,
        selected_displays: &HashSet<DisplayId>,
    ) -> Result<TransitionOutcome, DisplayError> {
        let original = self.backend.query_topology()?;
        original.validate()?;
        let original_fingerprint = original.fingerprint()?;

        if let Some(record) = self.load_record()? {
            if original_fingerprint == record.focused_fingerprint {
                let mut outcome = TransitionOutcome::new(TransitionStatus::AlreadyFocused);
                outcome.affected_display_ids = record.affected_display_ids;
                outcome.original_fingerprint = Some(record.original_fingerprint);
                outcome.focused_fingerprint = Some(record.focused_fingerprint);
                return Ok(outcome);
            }
            if original_fingerprint == record.original_fingerprint {
                self.clear_record()?;
            } else {
                return Err(DisplayError::new(
                    DisplayPhase::SafetyCheck,
                    "a recovery journal exists, but the current topology matches neither recorded state",
                ));
            }
        }

        let selected = selected_displays
            .iter()
            .map(|id| DisplayId::new(id.as_str()).0)
            .collect::<HashSet<_>>();
        let existing = original
            .paths
            .iter()
            .map(|path| path.display_id().0)
            .collect::<HashSet<_>>();
        let mut missing = selected
            .difference(&existing)
            .cloned()
            .map(DisplayId)
            .collect::<Vec<_>>();
        missing.sort();

        let mut protected = original
            .paths
            .iter()
            .filter(|path| path.source.is_primary && selected.contains(path.display_id().as_str()))
            .map(PathSnapshot::display_id)
            .collect::<Vec<_>>();
        protected.sort();
        protected.dedup();
        if !protected.is_empty() {
            return Err(DisplayError::new(
                DisplayPhase::SafetyCheck,
                "the selected displays include the primary source or one of its clone paths",
            )
            .with_affected(&protected));
        }

        let mut affected = original
            .paths
            .iter()
            .filter(|path| selected.contains(path.display_id().as_str()))
            .map(PathSnapshot::display_id)
            .collect::<Vec<_>>();
        affected.sort();
        affected.dedup();
        if affected.is_empty() {
            let mut outcome = TransitionOutcome::new(TransitionStatus::NoChange);
            outcome.missing_display_ids = missing;
            outcome.original_fingerprint = Some(original_fingerprint);
            return Ok(outcome);
        }

        let focused = original.without_displays(&selected)?;
        let focused_fingerprint = focused.fingerprint()?;
        let record = RecoveryRecord {
            version: RECOVERY_VERSION,
            original_topology: original.clone(),
            original_fingerprint: original_fingerprint.clone(),
            focused_fingerprint: focused_fingerprint.clone(),
            affected_display_ids: affected.clone(),
        };
        self.save_record(&record)?;

        if let Err(error) = self.backend.validate_topology(&focused) {
            let cleanup_error = self.recovery_store.clear().err();
            let mut error = error
                .at_phase(DisplayPhase::Validate)
                .with_affected(&affected);
            if let Some(cleanup_error) = cleanup_error {
                error.recovery_journal_retained = true;
                error.message = format!(
                    "{}; recovery journal cleanup failed: {}",
                    error.message, cleanup_error
                );
            }
            return Err(error);
        }

        let pre_apply = self.backend.query_topology().and_then(|current| {
            let fingerprint = current.fingerprint()?;
            if fingerprint == original_fingerprint {
                Ok(())
            } else {
                Err(DisplayError::new(
                    DisplayPhase::Verify,
                    "display topology changed while focus mode was being validated",
                ))
            }
        });
        if let Err(error) = pre_apply {
            return Err(self.abort_activation_before_apply(error.with_affected(&affected)));
        }

        if let Err(error) = self.backend.apply_topology(&focused) {
            let error = error.at_phase(DisplayPhase::Apply).with_affected(&affected);
            return Err(self.rollback_after_failure(&original, &original_fingerprint, error));
        }

        let applied = match self.backend.query_topology() {
            Ok(applied) => applied,
            Err(error) => {
                let error = error
                    .at_phase(DisplayPhase::Verify)
                    .with_affected(&affected);
                return Err(self.rollback_after_failure(&original, &original_fingerprint, error));
            }
        };
        match applied.fingerprint() {
            Ok(actual) if actual == focused_fingerprint => {}
            Ok(actual) => {
                let error = DisplayError::new(
                    DisplayPhase::Verify,
                    format!(
                        "focused topology verification failed: expected {}, got {}",
                        focused_fingerprint, actual
                    ),
                )
                .with_affected(&affected);
                return Err(self.rollback_after_failure(&original, &original_fingerprint, error));
            }
            Err(error) => {
                return Err(self.rollback_after_failure(
                    &original,
                    &original_fingerprint,
                    error.with_affected(&affected),
                ));
            }
        }

        let mut outcome = TransitionOutcome::new(TransitionStatus::Activated);
        outcome.affected_display_ids = affected;
        outcome.missing_display_ids = missing;
        outcome.original_fingerprint = Some(original_fingerprint);
        outcome.focused_fingerprint = Some(focused_fingerprint);
        Ok(outcome)
    }

    pub fn restore(&mut self) -> Result<TransitionOutcome, DisplayError> {
        self.restore_recorded_topology(false)
    }

    pub fn recover(&mut self) -> Result<TransitionOutcome, DisplayError> {
        self.restore_recorded_topology(true)
    }

    pub fn recovery_topology_state(&mut self) -> Result<RecoveryTopologyState, DisplayError> {
        let Some(record) = self.load_record()? else {
            return Ok(RecoveryTopologyState::NoJournal);
        };
        let current = self.backend.query_topology()?;
        let fingerprint = current.fingerprint()?;
        if fingerprint == record.original_fingerprint {
            Ok(RecoveryTopologyState::Original)
        } else if fingerprint == record.focused_fingerprint {
            Ok(RecoveryTopologyState::Focused)
        } else {
            Ok(RecoveryTopologyState::Diverged)
        }
    }

    fn restore_recorded_topology(
        &mut self,
        recovering: bool,
    ) -> Result<TransitionOutcome, DisplayError> {
        let Some(record) = self.load_record()? else {
            return Ok(TransitionOutcome::new(TransitionStatus::NoChange));
        };
        let phase = if recovering {
            DisplayPhase::Recover
        } else {
            DisplayPhase::Restore
        };
        let current = self.backend.query_topology()?;
        let current_fingerprint = current.fingerprint()?;
        if current_fingerprint == record.original_fingerprint {
            self.clear_record()?;
            let mut outcome = TransitionOutcome::new(TransitionStatus::AlreadyRestored);
            outcome.affected_display_ids = record.affected_display_ids;
            outcome.original_fingerprint = Some(record.original_fingerprint);
            outcome.focused_fingerprint = Some(record.focused_fingerprint);
            return Ok(outcome);
        }
        if current_fingerprint != record.focused_fingerprint {
            return Err(DisplayError::new(
                phase,
                "current display topology changed after focus mode; refusing to overwrite it",
            )
            .with_affected(&record.affected_display_ids));
        }

        let mut used_database_fallback = false;
        let exact_result = match self.backend.validate_topology(&record.original_topology) {
            Ok(()) => {
                self.ensure_focused_before_apply(
                    &record,
                    phase,
                    "display topology changed while restoration was being validated",
                )?;
                self.backend.apply_topology(&record.original_topology)
            }
            Err(error) => Err(error),
        };
        if let Err(error) = exact_result {
            if !database_fallback_allowed(&error) {
                return Err(error
                    .at_phase(phase)
                    .with_affected(&record.affected_display_ids));
            }
            let database_topology = self.backend.query_database_topology().map_err(|error| {
                error
                    .at_phase(phase)
                    .with_affected(&record.affected_display_ids)
            })?;
            let database_fingerprint = database_topology.fingerprint()?;
            if database_fingerprint != record.original_fingerprint {
                return Err(DisplayError::new(
                    phase,
                    format!(
                        "database-current topology does not match the recorded original: expected {}, got {}",
                        record.original_fingerprint, database_fingerprint
                    ),
                )
                .with_affected(&record.affected_display_ids));
            }
            self.ensure_focused_before_apply(
                &record,
                phase,
                "display topology changed while database restoration was being preflighted",
            )?;
            self.backend
                .apply_database_current(&database_topology)
                .map_err(|error| {
                    error
                        .at_phase(phase)
                        .with_affected(&record.affected_display_ids)
                })?;
            used_database_fallback = true;
        }

        let restored = self
            .backend
            .query_topology()
            .map_err(|error| error.at_phase(DisplayPhase::Verify))?;
        let restored_fingerprint = restored.fingerprint()?;
        if restored_fingerprint != record.original_fingerprint {
            return Err(DisplayError::new(
                DisplayPhase::Verify,
                format!(
                    "restored topology verification failed: expected {}, got {}",
                    record.original_fingerprint, restored_fingerprint
                ),
            )
            .with_affected(&record.affected_display_ids));
        }

        self.clear_record()?;
        let mut outcome = TransitionOutcome::new(if recovering {
            TransitionStatus::Recovered
        } else {
            TransitionStatus::Restored
        });
        outcome.affected_display_ids = record.affected_display_ids;
        outcome.original_fingerprint = Some(record.original_fingerprint);
        outcome.focused_fingerprint = Some(record.focused_fingerprint);
        outcome.used_database_fallback = used_database_fallback;
        Ok(outcome)
    }

    fn ensure_focused_before_apply(
        &mut self,
        record: &RecoveryRecord,
        phase: DisplayPhase,
        message: &str,
    ) -> Result<(), DisplayError> {
        let current = self.backend.query_topology()?;
        if current.fingerprint()? != record.focused_fingerprint {
            return Err(
                DisplayError::new(phase, message).with_affected(&record.affected_display_ids)
            );
        }
        Ok(())
    }

    fn rollback_after_failure(
        &mut self,
        original: &TopologySnapshot,
        original_fingerprint: &TopologyFingerprint,
        mut cause: DisplayError,
    ) -> DisplayError {
        let rollback_result = self.backend.apply_topology(original).and_then(|_| {
            let restored = self.backend.query_topology()?;
            let fingerprint = restored.fingerprint()?;
            if &fingerprint == original_fingerprint {
                Ok(())
            } else {
                Err(DisplayError::new(
                    DisplayPhase::Rollback,
                    format!(
                        "rollback verification failed: expected {}, got {}",
                        original_fingerprint, fingerprint
                    ),
                ))
            }
        });

        match rollback_result {
            Ok(()) => {
                cause.rollback = RollbackStatus::Succeeded;
                if let Err(error) = self.recovery_store.clear() {
                    cause.recovery_journal_retained = true;
                    cause.message = format!(
                        "{}; rollback succeeded but recovery journal cleanup failed: {}",
                        cause.message, error
                    );
                }
            }
            Err(error) => {
                cause.recovery_journal_retained = true;
                cause.rollback = RollbackStatus::Failed {
                    win32_code: error.win32_code,
                    message: error.message,
                };
            }
        }
        cause
    }

    fn abort_activation_before_apply(&mut self, mut cause: DisplayError) -> DisplayError {
        if let Err(error) = self.recovery_store.clear() {
            cause.recovery_journal_retained = true;
            cause.message = format!(
                "{}; recovery journal cleanup failed: {}",
                cause.message, error
            );
        }
        cause
    }

    fn load_record(&mut self) -> Result<Option<RecoveryRecord>, DisplayError> {
        let record = self
            .recovery_store
            .load()
            .map_err(|error| DisplayError::new(DisplayPhase::JournalRead, error.to_string()))?;
        if let Some(record) = &record {
            record
                .validate()
                .map_err(|error| error.at_phase(DisplayPhase::JournalRead))?;
        }
        Ok(record)
    }

    fn save_record(&mut self, record: &RecoveryRecord) -> Result<(), DisplayError> {
        self.recovery_store.save(record).map_err(|error| {
            DisplayError::new(DisplayPhase::JournalWrite, error.to_string())
                .with_affected(&record.affected_display_ids)
        })
    }

    fn clear_record(&mut self) -> Result<(), DisplayError> {
        self.recovery_store
            .clear()
            .map_err(|error| DisplayError::new(DisplayPhase::JournalClear, error.to_string()))
    }
}

fn database_fallback_allowed(error: &DisplayError) -> bool {
    matches!(error.win32_code, Some(31 | 50 | 87 | 1167 | 1610))
}

#[derive(Debug, Clone)]
pub struct JsonRecoveryStore {
    path: PathBuf,
}

impl JsonRecoveryStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl RecoveryStore for JsonRecoveryStore {
    fn load(&mut self) -> Result<Option<RecoveryRecord>, RecoveryStoreError> {
        match fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|error| {
                RecoveryStoreError::new("read recovery journal", error.to_string())
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(RecoveryStoreError::new(
                "read recovery journal",
                error.to_string(),
            )),
        }
    }

    fn save(&mut self, record: &RecoveryRecord) -> Result<(), RecoveryStoreError> {
        let parent = self.path.parent().ok_or_else(|| {
            RecoveryStoreError::new("write recovery journal", "journal path has no parent")
        })?;
        fs::create_dir_all(parent).map_err(|error| {
            RecoveryStoreError::new("create recovery directory", error.to_string())
        })?;
        let payload = serde_json::to_vec_pretty(record).map_err(|error| {
            RecoveryStoreError::new("serialize recovery journal", error.to_string())
        })?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let file_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("recovery.json");
        let temporary = parent.join(format!(".{file_name}.{}.{}.tmp", std::process::id(), nonce));
        let write_result = (|| {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)?;
            file.write_all(&payload)?;
            file.flush()?;
            file.sync_all()?;
            atomic_replace(&temporary, &self.path)
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        write_result
            .map_err(|error| RecoveryStoreError::new("write recovery journal", error.to_string()))
    }

    fn clear(&mut self) -> Result<(), RecoveryStoreError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(RecoveryStoreError::new(
                "clear recovery journal",
                error.to_string(),
            )),
        }
    }
}

#[cfg(windows)]
fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    unsafe {
        MoveFileExW(
            PCWSTR(source.as_ptr()),
            PCWSTR(destination.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
        .map_err(io::Error::other)
    }
}

#[cfg(not(windows))]
fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

fn push_string(output: &mut Vec<u8>, value: &str) {
    let value = value.to_lowercase();
    push_u32(output, value.len() as u32);
    output.extend_from_slice(value.as_bytes());
}

fn push_bool(output: &mut Vec<u8>, value: bool) {
    output.push(u8::from(value));
}

fn push_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn push_i32(output: &mut Vec<u8>, value: i32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_le_bytes());
}

struct Fnv1a64(u64);

impl Fnv1a64 {
    fn new() -> Self {
        Self(0xcbf29ce484222325)
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x100000001b3);
        }
    }

    fn write_u32(&mut self, value: u32) {
        self.write(&value.to_le_bytes());
    }

    fn finish(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct NativeWindowsBackend;

impl DisplayBackend for NativeWindowsBackend {
    fn query_topology(&mut self) -> Result<TopologySnapshot, DisplayError> {
        let mut api = SystemQueryApi;
        let raw = query_topology_with_awareness(&mut api)?;
        topology_from_raw(raw)
    }

    fn query_database_topology(&mut self) -> Result<TopologySnapshot, DisplayError> {
        let mut api = SystemQueryApi;
        let raw = query_topology_with_awareness_for(&mut api, QDC_DATABASE_CURRENT)?;
        topology_from_raw(raw)
    }

    fn validate_topology(&mut self, topology: &TopologySnapshot) -> Result<(), DisplayError> {
        topology.validate()?;
        set_supplied_topology(topology, SDC_VALIDATE, DisplayPhase::Validate)
    }

    fn apply_topology(&mut self, topology: &TopologySnapshot) -> Result<(), DisplayError> {
        topology.validate()?;
        set_supplied_topology(topology, SDC_APPLY, DisplayPhase::Apply)
    }

    fn apply_database_current(
        &mut self,
        _database_topology: &TopologySnapshot,
    ) -> Result<(), DisplayError> {
        let flags = database_apply_flags();
        let result = unsafe { SetDisplayConfig(None, None, flags) };
        if result == ERROR_SUCCESS.0 as i32 {
            Ok(())
        } else {
            Err(DisplayError::new(
                DisplayPhase::Restore,
                "Windows rejected the database-current display topology",
            )
            .with_win32_code(result as u32))
        }
    }
}

fn topology_from_raw(raw: RawTopologyQuery) -> Result<TopologySnapshot, DisplayError> {
    let primary_gdi_name = primary_gdi_device_name()?;
    let mut paths = Vec::with_capacity(raw.paths.len());

    for path in &raw.paths {
        let source_name = source_device_name(path.sourceInfo.adapterId, path.sourceInfo.id)?;
        let (friendly_name, device_path) =
            target_device_name(path.targetInfo.adapterId, path.targetInfo.id, &source_name)?;
        let path_virtual =
            raw.virtual_mode_aware && path.flags & DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE != 0;
        let source = SourcePathSnapshot {
            adapter: adapter_from_luid(path.sourceInfo.adapterId),
            id: path.sourceInfo.id,
            mode_reference: SourceModeReference::from_raw(
                unsafe { path.sourceInfo.Anonymous.modeInfoIdx },
                path_virtual,
            ),
            status_flags: path.sourceInfo.statusFlags,
            is_primary: source_name.eq_ignore_ascii_case(&primary_gdi_name),
            gdi_device_name: source_name,
        };
        let target = TargetPathSnapshot {
            adapter: adapter_from_luid(path.targetInfo.adapterId),
            id: path.targetInfo.id,
            mode_reference: TargetModeReference::from_raw(
                unsafe { path.targetInfo.Anonymous.modeInfoIdx },
                path_virtual,
            ),
            output_technology: path.targetInfo.outputTechnology.0,
            rotation: path.targetInfo.rotation.0,
            scaling: path.targetInfo.scaling.0,
            refresh_numerator: path.targetInfo.refreshRate.Numerator,
            refresh_denominator: path.targetInfo.refreshRate.Denominator,
            scanline_ordering: path.targetInfo.scanLineOrdering.0,
            target_available: path.targetInfo.targetAvailable.as_bool(),
            status_flags: path.targetInfo.statusFlags,
            friendly_name,
            device_path,
        };
        paths.push(PathSnapshot {
            source,
            target,
            flags: path.flags,
        });
    }

    let primary_sources = paths
        .iter()
        .filter(|path| path.source.is_primary)
        .map(|path| path.source.key())
        .collect::<BTreeSet<_>>();
    if primary_sources.len() != 1 {
        return Err(DisplayError::new(
            DisplayPhase::Resolve,
            format!(
                "could not map GDI primary {} to one CCD source",
                primary_gdi_name
            ),
        ));
    }

    let modes = raw
        .modes
        .iter()
        .map(mode_from_raw)
        .collect::<Result<Vec<_>, _>>()?;
    let topology = TopologySnapshot {
        paths,
        modes,
        virtual_mode_aware: raw.virtual_mode_aware,
        virtual_refresh_rate_aware: raw.virtual_refresh_rate_aware,
    };
    topology.validate()?;
    Ok(topology)
}

fn awareness_flags(topology: &TopologySnapshot) -> SET_DISPLAY_CONFIG_FLAGS {
    let mut flags = SET_DISPLAY_CONFIG_FLAGS(0);
    if topology.virtual_mode_aware {
        flags |= SDC_VIRTUAL_MODE_AWARE;
    }
    if topology.virtual_refresh_rate_aware {
        flags |= SDC_VIRTUAL_REFRESH_RATE_AWARE;
    }
    flags
}

fn supplied_flags(
    action: SET_DISPLAY_CONFIG_FLAGS,
    topology: &TopologySnapshot,
) -> SET_DISPLAY_CONFIG_FLAGS {
    action | SDC_USE_SUPPLIED_DISPLAY_CONFIG | awareness_flags(topology)
}

fn database_apply_flags() -> SET_DISPLAY_CONFIG_FLAGS {
    SDC_APPLY | SDC_USE_DATABASE_CURRENT
}

fn set_supplied_topology(
    topology: &TopologySnapshot,
    action: SET_DISPLAY_CONFIG_FLAGS,
    phase: DisplayPhase,
) -> Result<(), DisplayError> {
    let (paths, modes) = topology_to_raw(topology)?;
    let result = unsafe {
        SetDisplayConfig(
            Some(paths.as_slice()),
            Some(modes.as_slice()),
            supplied_flags(action, topology),
        )
    };
    if result == ERROR_SUCCESS.0 as i32 {
        Ok(())
    } else {
        Err(
            DisplayError::new(phase, "Windows rejected the supplied display topology")
                .with_win32_code(result as u32),
        )
    }
}

trait QueryApi {
    fn buffer_sizes(
        &mut self,
        flags: QUERY_DISPLAY_CONFIG_FLAGS,
        path_count: &mut u32,
        mode_count: &mut u32,
    ) -> u32;

    fn query(
        &mut self,
        flags: QUERY_DISPLAY_CONFIG_FLAGS,
        path_count: &mut u32,
        paths: &mut [DISPLAYCONFIG_PATH_INFO],
        mode_count: &mut u32,
        modes: &mut [DISPLAYCONFIG_MODE_INFO],
        require_topology_id: bool,
    ) -> u32;
}

struct SystemQueryApi;

impl QueryApi for SystemQueryApi {
    fn buffer_sizes(
        &mut self,
        flags: QUERY_DISPLAY_CONFIG_FLAGS,
        path_count: &mut u32,
        mode_count: &mut u32,
    ) -> u32 {
        unsafe { GetDisplayConfigBufferSizes(flags, path_count, mode_count).0 }
    }

    fn query(
        &mut self,
        flags: QUERY_DISPLAY_CONFIG_FLAGS,
        path_count: &mut u32,
        paths: &mut [DISPLAYCONFIG_PATH_INFO],
        mode_count: &mut u32,
        modes: &mut [DISPLAYCONFIG_MODE_INFO],
        require_topology_id: bool,
    ) -> u32 {
        let mut topology_id = DISPLAYCONFIG_TOPOLOGY_ID::default();
        let topology_id = require_topology_id.then_some(&mut topology_id as *mut _);
        unsafe {
            QueryDisplayConfig(
                flags,
                path_count,
                paths.as_mut_ptr(),
                mode_count,
                modes.as_mut_ptr(),
                topology_id,
            )
            .0
        }
    }
}

struct RawTopologyQuery {
    paths: Vec<DISPLAYCONFIG_PATH_INFO>,
    modes: Vec<DISPLAYCONFIG_MODE_INFO>,
    virtual_mode_aware: bool,
    virtual_refresh_rate_aware: bool,
}

fn query_topology_with_awareness<A: QueryApi>(
    api: &mut A,
) -> Result<RawTopologyQuery, DisplayError> {
    query_topology_with_awareness_for(api, QDC_ONLY_ACTIVE_PATHS)
}

fn query_topology_with_awareness_for<A: QueryApi>(
    api: &mut A,
    base_flags: QUERY_DISPLAY_CONFIG_FLAGS,
) -> Result<RawTopologyQuery, DisplayError> {
    let preferred = base_flags | QDC_VIRTUAL_MODE_AWARE | QDC_VIRTUAL_REFRESH_RATE_AWARE;
    match query_topology_with_flags(api, preferred) {
        Ok((paths, modes)) => Ok(RawTopologyQuery {
            paths,
            modes,
            virtual_mode_aware: true,
            virtual_refresh_rate_aware: true,
        }),
        Err(error) if error.win32_code == Some(ERROR_INVALID_PARAMETER.0) => {
            let flags = base_flags | QDC_VIRTUAL_MODE_AWARE;
            let (paths, modes) = query_topology_with_flags(api, flags)?;
            Ok(RawTopologyQuery {
                paths,
                modes,
                virtual_mode_aware: true,
                virtual_refresh_rate_aware: false,
            })
        }
        Err(error) => Err(error),
    }
}

fn query_topology_with_flags<A: QueryApi>(
    api: &mut A,
    flags: QUERY_DISPLAY_CONFIG_FLAGS,
) -> Result<(Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>), DisplayError> {
    let mut last_code = ERROR_INSUFFICIENT_BUFFER.0;
    for _ in 0..MAX_QUERY_ATTEMPTS {
        let mut path_count = 0;
        let mut mode_count = 0;
        let size_result = api.buffer_sizes(flags, &mut path_count, &mut mode_count);
        if size_result != ERROR_SUCCESS.0 {
            return Err(DisplayError::new(
                DisplayPhase::Query,
                "GetDisplayConfigBufferSizes failed",
            )
            .with_win32_code(size_result));
        }

        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        let path_capacity = paths.len();
        let mode_capacity = modes.len();
        let query_result = api.query(
            flags,
            &mut path_count,
            &mut paths,
            &mut mode_count,
            &mut modes,
            flags.contains(QDC_DATABASE_CURRENT),
        );
        last_code = query_result;
        if query_result == ERROR_INSUFFICIENT_BUFFER.0 {
            continue;
        }
        if query_result != ERROR_SUCCESS.0 {
            return Err(
                DisplayError::new(DisplayPhase::Query, "QueryDisplayConfig failed")
                    .with_win32_code(query_result),
            );
        }
        if path_count as usize > path_capacity || mode_count as usize > mode_capacity {
            last_code = ERROR_INSUFFICIENT_BUFFER.0;
            continue;
        }
        paths.truncate(path_count as usize);
        modes.truncate(mode_count as usize);
        return Ok((paths, modes));
    }

    Err(DisplayError::new(
        DisplayPhase::Query,
        "display topology kept changing while it was queried",
    )
    .with_win32_code(last_code))
}

fn primary_gdi_device_name() -> Result<String, DisplayError> {
    let mut primary_names = Vec::new();
    let mut index = 0;
    loop {
        let mut device = DISPLAY_DEVICEW {
            cb: size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        let found = unsafe { EnumDisplayDevicesW(PCWSTR::null(), index, &mut device, 0).as_bool() };
        if !found {
            break;
        }
        if device.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP != 0
            && device.StateFlags & DISPLAY_DEVICE_PRIMARY_DEVICE != 0
        {
            primary_names.push(utf16_string(&device.DeviceName));
        }
        index += 1;
    }

    if primary_names.len() != 1 || primary_names[0].is_empty() {
        return Err(DisplayError::new(
            DisplayPhase::Resolve,
            format!(
                "expected one active GDI primary display, found {}",
                primary_names.len()
            ),
        ));
    }
    Ok(primary_names.remove(0))
}

fn source_device_name(adapter: LUID, id: u32) -> Result<String, DisplayError> {
    let mut request = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
            size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
            adapterId: adapter,
            id,
        },
        ..Default::default()
    };
    let result = unsafe { DisplayConfigGetDeviceInfo(&mut request.header) };
    if result != ERROR_SUCCESS.0 as i32 {
        return Err(DisplayError::new(
            DisplayPhase::Resolve,
            "DisplayConfigGetDeviceInfo could not resolve a source name",
        )
        .with_win32_code(result as u32));
    }
    let name = utf16_string(&request.viewGdiDeviceName);
    if name.is_empty() {
        return Err(DisplayError::new(
            DisplayPhase::Resolve,
            "CCD returned an empty GDI source name",
        ));
    }
    Ok(name)
}

fn target_device_name(
    adapter: LUID,
    id: u32,
    source_name: &str,
) -> Result<(String, String), DisplayError> {
    let mut request = DISPLAYCONFIG_TARGET_DEVICE_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
            size: size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
            adapterId: adapter,
            id,
        },
        ..Default::default()
    };
    let result = unsafe { DisplayConfigGetDeviceInfo(&mut request.header) };
    if result != ERROR_SUCCESS.0 as i32 {
        return Err(DisplayError::new(
            DisplayPhase::Resolve,
            "DisplayConfigGetDeviceInfo could not resolve a target name",
        )
        .with_win32_code(result as u32));
    }
    let device_path = utf16_string(&request.monitorDevicePath);
    if device_path.is_empty() {
        return Err(DisplayError::new(
            DisplayPhase::Resolve,
            "CCD returned an empty monitor device path",
        ));
    }
    let friendly_name = utf16_string(&request.monitorFriendlyDeviceName);
    let friendly_name = if friendly_name.is_empty() {
        source_name.to_string()
    } else {
        friendly_name
    };
    Ok((friendly_name, device_path))
}

fn utf16_string(buffer: &[u16]) -> String {
    let length = buffer
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..length])
}

fn adapter_from_luid(value: LUID) -> AdapterId {
    AdapterId {
        low_part: value.LowPart,
        high_part: value.HighPart,
    }
}

fn luid_from_adapter(value: AdapterId) -> LUID {
    LUID {
        LowPart: value.low_part,
        HighPart: value.high_part,
    }
}

fn point_from_raw(value: POINTL) -> PointSnapshot {
    PointSnapshot {
        x: value.x,
        y: value.y,
    }
}

fn point_to_raw(value: PointSnapshot) -> POINTL {
    POINTL {
        x: value.x,
        y: value.y,
    }
}

fn rect_from_raw(value: RECTL) -> RectSnapshot {
    RectSnapshot {
        left: value.left,
        top: value.top,
        right: value.right,
        bottom: value.bottom,
    }
}

fn rect_to_raw(value: RectSnapshot) -> RECTL {
    RECTL {
        left: value.left,
        top: value.top,
        right: value.right,
        bottom: value.bottom,
    }
}

fn mode_from_raw(raw: &DISPLAYCONFIG_MODE_INFO) -> Result<ModeSnapshot, DisplayError> {
    let value = if raw.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
        let source = unsafe { raw.Anonymous.sourceMode };
        ModeValueSnapshot::Source(SourceModeSnapshot {
            width: source.width,
            height: source.height,
            pixel_format: source.pixelFormat.0,
            position: point_from_raw(source.position),
        })
    } else if raw.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_TARGET {
        let target = unsafe { raw.Anonymous.targetMode };
        let signal = target.targetVideoSignalInfo;
        ModeValueSnapshot::Target(TargetModeSnapshot {
            pixel_rate: signal.pixelRate,
            hsync_numerator: signal.hSyncFreq.Numerator,
            hsync_denominator: signal.hSyncFreq.Denominator,
            vsync_numerator: signal.vSyncFreq.Numerator,
            vsync_denominator: signal.vSyncFreq.Denominator,
            active_size: RegionSnapshot {
                width: signal.activeSize.cx,
                height: signal.activeSize.cy,
            },
            total_size: RegionSnapshot {
                width: signal.totalSize.cx,
                height: signal.totalSize.cy,
            },
            video_standard: unsafe { signal.Anonymous.videoStandard },
            scanline_ordering: signal.scanLineOrdering.0,
        })
    } else if raw.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_DESKTOP_IMAGE {
        let desktop = unsafe { raw.Anonymous.desktopImageInfo };
        ModeValueSnapshot::DesktopImage(DesktopImageModeSnapshot {
            path_source_size: point_from_raw(desktop.PathSourceSize),
            desktop_image_region: rect_from_raw(desktop.DesktopImageRegion),
            desktop_image_clip: rect_from_raw(desktop.DesktopImageClip),
        })
    } else {
        return Err(DisplayError::new(
            DisplayPhase::Query,
            format!("unsupported DISPLAYCONFIG mode type {}", raw.infoType.0),
        ));
    };
    Ok(ModeSnapshot {
        adapter: adapter_from_luid(raw.adapterId),
        id: raw.id,
        value,
    })
}

fn mode_to_raw(mode: &ModeSnapshot) -> DISPLAYCONFIG_MODE_INFO {
    let mut raw = DISPLAYCONFIG_MODE_INFO {
        adapterId: luid_from_adapter(mode.adapter),
        id: mode.id,
        ..Default::default()
    };
    match &mode.value {
        ModeValueSnapshot::Source(value) => {
            raw.infoType = DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE;
            raw.Anonymous = DISPLAYCONFIG_MODE_INFO_0 {
                sourceMode: DISPLAYCONFIG_SOURCE_MODE {
                    width: value.width,
                    height: value.height,
                    pixelFormat: DISPLAYCONFIG_PIXELFORMAT(value.pixel_format),
                    position: point_to_raw(value.position),
                },
            };
        }
        ModeValueSnapshot::Target(value) => {
            raw.infoType = DISPLAYCONFIG_MODE_INFO_TYPE_TARGET;
            raw.Anonymous = DISPLAYCONFIG_MODE_INFO_0 {
                targetMode: DISPLAYCONFIG_TARGET_MODE {
                    targetVideoSignalInfo: DISPLAYCONFIG_VIDEO_SIGNAL_INFO {
                        pixelRate: value.pixel_rate,
                        hSyncFreq: DISPLAYCONFIG_RATIONAL {
                            Numerator: value.hsync_numerator,
                            Denominator: value.hsync_denominator,
                        },
                        vSyncFreq: DISPLAYCONFIG_RATIONAL {
                            Numerator: value.vsync_numerator,
                            Denominator: value.vsync_denominator,
                        },
                        activeSize: DISPLAYCONFIG_2DREGION {
                            cx: value.active_size.width,
                            cy: value.active_size.height,
                        },
                        totalSize: DISPLAYCONFIG_2DREGION {
                            cx: value.total_size.width,
                            cy: value.total_size.height,
                        },
                        Anonymous: DISPLAYCONFIG_VIDEO_SIGNAL_INFO_0 {
                            videoStandard: value.video_standard,
                        },
                        scanLineOrdering: DISPLAYCONFIG_SCANLINE_ORDERING(value.scanline_ordering),
                    },
                },
            };
        }
        ModeValueSnapshot::DesktopImage(value) => {
            raw.infoType = DISPLAYCONFIG_MODE_INFO_TYPE_DESKTOP_IMAGE;
            raw.Anonymous = DISPLAYCONFIG_MODE_INFO_0 {
                desktopImageInfo: DISPLAYCONFIG_DESKTOP_IMAGE_INFO {
                    PathSourceSize: point_to_raw(value.path_source_size),
                    DesktopImageRegion: rect_to_raw(value.desktop_image_region),
                    DesktopImageClip: rect_to_raw(value.desktop_image_clip),
                },
            };
        }
    }
    raw
}

fn topology_to_raw(
    topology: &TopologySnapshot,
) -> Result<(Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>), DisplayError> {
    topology.validate()?;
    let paths = topology
        .paths
        .iter()
        .map(|path| DISPLAYCONFIG_PATH_INFO {
            sourceInfo: DISPLAYCONFIG_PATH_SOURCE_INFO {
                adapterId: luid_from_adapter(path.source.adapter),
                id: path.source.id,
                Anonymous: DISPLAYCONFIG_PATH_SOURCE_INFO_0 {
                    modeInfoIdx: path.source.mode_reference.raw(),
                },
                statusFlags: path.source.status_flags,
            },
            targetInfo: DISPLAYCONFIG_PATH_TARGET_INFO {
                adapterId: luid_from_adapter(path.target.adapter),
                id: path.target.id,
                Anonymous: DISPLAYCONFIG_PATH_TARGET_INFO_0 {
                    modeInfoIdx: path.target.mode_reference.raw(),
                },
                outputTechnology: DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY(
                    path.target.output_technology,
                ),
                rotation: DISPLAYCONFIG_ROTATION(path.target.rotation),
                scaling: DISPLAYCONFIG_SCALING(path.target.scaling),
                refreshRate: DISPLAYCONFIG_RATIONAL {
                    Numerator: path.target.refresh_numerator,
                    Denominator: path.target.refresh_denominator,
                },
                scanLineOrdering: DISPLAYCONFIG_SCANLINE_ORDERING(path.target.scanline_ordering),
                targetAvailable: BOOL(i32::from(path.target.target_available)),
                statusFlags: path.target.status_flags,
            },
            flags: path.flags,
        })
        .collect();
    let modes = topology.modes.iter().map(mode_to_raw).collect();
    Ok((paths, modes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use windows::Win32::Devices::Display::SDC_SAVE_TO_DATABASE;

    #[derive(Clone)]
    struct FakeBackend {
        current: TopologySnapshot,
        query_results: VecDeque<Result<TopologySnapshot, DisplayError>>,
        validate_failures: VecDeque<Option<DisplayError>>,
        apply_failures: VecDeque<Option<DisplayError>>,
        database_query_failure: Option<DisplayError>,
        database_failure: Option<DisplayError>,
        database_topology: Option<TopologySnapshot>,
        validated: Vec<TopologySnapshot>,
        applied: Vec<TopologySnapshot>,
        database_query_count: usize,
        database_apply_count: usize,
    }

    impl FakeBackend {
        fn new(current: TopologySnapshot) -> Self {
            Self {
                current,
                query_results: VecDeque::new(),
                validate_failures: VecDeque::new(),
                apply_failures: VecDeque::new(),
                database_query_failure: None,
                database_failure: None,
                database_topology: None,
                validated: Vec::new(),
                applied: Vec::new(),
                database_query_count: 0,
                database_apply_count: 0,
            }
        }
    }

    impl DisplayBackend for FakeBackend {
        fn query_topology(&mut self) -> Result<TopologySnapshot, DisplayError> {
            self.query_results
                .pop_front()
                .unwrap_or_else(|| Ok(self.current.clone()))
        }

        fn query_database_topology(&mut self) -> Result<TopologySnapshot, DisplayError> {
            self.database_query_count += 1;
            if let Some(error) = self.database_query_failure.take() {
                return Err(error);
            }
            self.database_topology.clone().ok_or_else(|| {
                DisplayError::new(DisplayPhase::Query, "database topology was not configured")
            })
        }

        fn validate_topology(&mut self, topology: &TopologySnapshot) -> Result<(), DisplayError> {
            self.validated.push(topology.clone());
            match self.validate_failures.pop_front().flatten() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }

        fn apply_topology(&mut self, topology: &TopologySnapshot) -> Result<(), DisplayError> {
            self.applied.push(topology.clone());
            match self.apply_failures.pop_front().flatten() {
                Some(error) => Err(error),
                None => {
                    self.current = topology.clone();
                    Ok(())
                }
            }
        }

        fn apply_database_current(
            &mut self,
            database_topology: &TopologySnapshot,
        ) -> Result<(), DisplayError> {
            self.database_apply_count += 1;
            if let Some(error) = self.database_failure.take() {
                return Err(error);
            }
            self.current = database_topology.clone();
            Ok(())
        }
    }

    #[derive(Debug, Default, Clone)]
    struct MemoryRecoveryStore {
        record: Option<RecoveryRecord>,
        fail_load: bool,
        fail_save: bool,
        fail_clear: bool,
        save_count: usize,
        clear_count: usize,
    }

    impl RecoveryStore for MemoryRecoveryStore {
        fn load(&mut self) -> Result<Option<RecoveryRecord>, RecoveryStoreError> {
            if self.fail_load {
                Err(RecoveryStoreError::new("load", "injected failure"))
            } else {
                Ok(self.record.clone())
            }
        }

        fn save(&mut self, record: &RecoveryRecord) -> Result<(), RecoveryStoreError> {
            self.save_count += 1;
            if self.fail_save {
                Err(RecoveryStoreError::new("save", "injected failure"))
            } else {
                self.record = Some(record.clone());
                Ok(())
            }
        }

        fn clear(&mut self) -> Result<(), RecoveryStoreError> {
            self.clear_count += 1;
            if self.fail_clear {
                Err(RecoveryStoreError::new("clear", "injected failure"))
            } else {
                self.record = None;
                Ok(())
            }
        }
    }

    fn injected_error(phase: DisplayPhase, code: u32, message: &str) -> DisplayError {
        DisplayError::new(phase, message).with_win32_code(code)
    }

    fn topology(display_count: usize) -> TopologySnapshot {
        let adapter = AdapterId {
            low_part: 7,
            high_part: 0,
        };
        let mut paths = Vec::new();
        let mut modes = Vec::new();
        for index in 0..display_count {
            let source_id = index as u32;
            let target_id = 100 + index as u32;
            let source_mode_idx = modes.len() as u32;
            modes.push(ModeSnapshot {
                adapter,
                id: source_id,
                value: ModeValueSnapshot::Source(SourceModeSnapshot {
                    width: 1920,
                    height: 1080,
                    pixel_format: 4,
                    position: PointSnapshot {
                        x: index as i32 * 1920,
                        y: 0,
                    },
                }),
            });
            let target_mode_idx = modes.len() as u32;
            modes.push(ModeSnapshot {
                adapter,
                id: target_id,
                value: ModeValueSnapshot::Target(TargetModeSnapshot {
                    pixel_rate: 148_500_000,
                    hsync_numerator: 67_500,
                    hsync_denominator: 1,
                    vsync_numerator: 60_000,
                    vsync_denominator: 1_000,
                    active_size: RegionSnapshot {
                        width: 1920,
                        height: 1080,
                    },
                    total_size: RegionSnapshot {
                        width: 2200,
                        height: 1125,
                    },
                    video_standard: 0,
                    scanline_ordering: 1,
                }),
            });
            paths.push(PathSnapshot {
                source: SourcePathSnapshot {
                    adapter,
                    id: source_id,
                    mode_reference: SourceModeReference::Legacy {
                        mode_info_idx: source_mode_idx,
                    },
                    status_flags: 1,
                    gdi_device_name: format!(r"\\.\DISPLAY{}", index + 1),
                    is_primary: index == 0,
                },
                target: TargetPathSnapshot {
                    adapter,
                    id: target_id,
                    mode_reference: TargetModeReference::Legacy {
                        mode_info_idx: target_mode_idx,
                    },
                    output_technology: 5,
                    rotation: 1,
                    scaling: 1,
                    refresh_numerator: 60_000,
                    refresh_denominator: 1_000,
                    scanline_ordering: 1,
                    target_available: true,
                    status_flags: 1,
                    friendly_name: format!("Monitor {}", index + 1),
                    device_path: format!(r"\\?\DISPLAY#MONITOR{}#UID{}", index + 1, index + 1),
                },
                flags: DISPLAYCONFIG_PATH_ACTIVE,
            });
        }
        TopologySnapshot {
            paths,
            modes,
            virtual_mode_aware: false,
            virtual_refresh_rate_aware: false,
        }
    }

    fn cloned_topology() -> TopologySnapshot {
        let mut value = topology(2);
        value.paths[1].source.adapter = value.paths[0].source.adapter;
        value.paths[1].source.id = value.paths[0].source.id;
        value.paths[1].source.mode_reference = value.paths[0].source.mode_reference;
        value.paths[1].source.gdi_device_name = value.paths[0].source.gdi_device_name.clone();
        value.paths[1].source.is_primary = true;
        value
    }

    fn virtual_topology(display_count: usize) -> TopologySnapshot {
        let mut value = topology(display_count);
        for path in &mut value.paths {
            let source_mode_info_idx = path.source.mode_reference.source_mode_index().unwrap();
            let target_mode_info_idx = path.target.mode_reference.target_mode_index().unwrap();
            let desktop_mode_info_idx = value.modes.len() as u16;
            value.modes.push(ModeSnapshot {
                adapter: path.source.adapter,
                id: path.source.id,
                value: ModeValueSnapshot::DesktopImage(DesktopImageModeSnapshot {
                    path_source_size: PointSnapshot { x: 1920, y: 1080 },
                    desktop_image_region: RectSnapshot {
                        left: 0,
                        top: 0,
                        right: 1920,
                        bottom: 1080,
                    },
                    desktop_image_clip: RectSnapshot {
                        left: 0,
                        top: 0,
                        right: 1920,
                        bottom: 1080,
                    },
                }),
            });
            path.source.mode_reference = SourceModeReference::Virtual {
                clone_group_id: path.source.id as u16 + 10,
                source_mode_info_idx: source_mode_info_idx as u16,
            };
            path.target.mode_reference = TargetModeReference::Virtual {
                desktop_mode_info_idx,
                target_mode_info_idx: target_mode_info_idx as u16,
            };
            path.flags |=
                DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE | DISPLAYCONFIG_PATH_BOOST_REFRESH_RATE;
        }
        value.virtual_mode_aware = true;
        value.virtual_refresh_rate_aware = true;
        value
    }

    fn selected(topology: &TopologySnapshot, indices: &[usize]) -> HashSet<DisplayId> {
        indices
            .iter()
            .map(|index| topology.paths[*index].display_id())
            .collect()
    }

    #[test]
    fn activates_selected_non_primary_displays_transactionally() {
        let original = topology(3);
        let selection = selected(&original, &[1, 2]);
        let backend = FakeBackend::new(original.clone());
        let store = MemoryRecoveryStore::default();
        let mut manager = DisplayManager::new(backend, store);

        let outcome = manager.activate(&selection).unwrap();

        assert_eq!(outcome.status, TransitionStatus::Activated);
        assert_eq!(outcome.affected_display_ids.len(), 2);
        let (backend, store) = manager.into_parts();
        assert_eq!(backend.validated.len(), 1);
        assert_eq!(backend.applied.len(), 1);
        assert_eq!(backend.current.paths.len(), 1);
        assert!(backend.current.paths[0].source.is_primary);
        assert_eq!(store.save_count, 1);
        assert!(store.record.is_some());
    }

    #[test]
    fn protects_the_only_display() {
        let original = topology(1);
        let selection = selected(&original, &[0]);
        let mut manager =
            DisplayManager::new(FakeBackend::new(original), MemoryRecoveryStore::default());

        let error = manager.activate(&selection).unwrap_err();

        assert_eq!(error.phase, DisplayPhase::SafetyCheck);
        assert_eq!(error.affected_display_ids.len(), 1);
        let (backend, store) = manager.into_parts();
        assert!(backend.applied.is_empty());
        assert_eq!(store.save_count, 0);
    }

    #[test]
    fn protects_every_clone_of_the_primary_source() {
        let original = cloned_topology();
        let selection = selected(&original, &[1]);
        let mut manager =
            DisplayManager::new(FakeBackend::new(original), MemoryRecoveryStore::default());

        let error = manager.activate(&selection).unwrap_err();

        assert_eq!(error.phase, DisplayPhase::SafetyCheck);
        assert!(error.message.contains("clone"));
    }

    #[test]
    fn rejects_ambiguous_primary_sources() {
        let mut original = topology(2);
        original.paths[1].source.is_primary = true;
        let mut manager =
            DisplayManager::new(FakeBackend::new(original), MemoryRecoveryStore::default());

        let error = manager.list_displays().unwrap_err();

        assert_eq!(error.phase, DisplayPhase::SafetyCheck);
        assert!(error.message.contains("found 2"));
    }

    #[test]
    fn reports_missing_selected_displays_without_mutation() {
        let original = topology(2);
        let missing = DisplayId::new(r"\\?\DISPLAY#MISSING#UID9");
        let selection = HashSet::from([missing.clone()]);
        let mut manager =
            DisplayManager::new(FakeBackend::new(original), MemoryRecoveryStore::default());

        let outcome = manager.activate(&selection).unwrap();

        assert_eq!(outcome.status, TransitionStatus::NoChange);
        assert_eq!(outcome.missing_display_ids, vec![missing]);
        let (backend, store) = manager.into_parts();
        assert!(backend.applied.is_empty());
        assert_eq!(store.save_count, 0);
    }

    #[test]
    fn clears_journal_when_validation_fails_before_apply() {
        let original = topology(2);
        let selection = selected(&original, &[1]);
        let mut backend = FakeBackend::new(original);
        backend.validate_failures.push_back(Some(injected_error(
            DisplayPhase::Validate,
            87,
            "validation failed",
        )));
        let mut manager = DisplayManager::new(backend, MemoryRecoveryStore::default());

        let error = manager.activate(&selection).unwrap_err();

        assert_eq!(error.phase, DisplayPhase::Validate);
        let (backend, store) = manager.into_parts();
        assert!(backend.applied.is_empty());
        assert!(store.record.is_none());
        assert_eq!(store.clear_count, 1);
    }

    #[test]
    fn activation_rechecks_topology_after_validation() {
        let original = topology(3);
        let selection = selected(&original, &[2]);
        let mut drift = original.clone();
        drift.paths[1].target.rotation = 2;
        let mut backend = FakeBackend::new(original.clone());
        backend.query_results.push_back(Ok(original));
        backend.query_results.push_back(Ok(drift));
        let mut manager = DisplayManager::new(backend, MemoryRecoveryStore::default());

        let error = manager.activate(&selection).unwrap_err();

        assert_eq!(error.phase, DisplayPhase::Verify);
        assert_eq!(error.rollback, RollbackStatus::NotRequired);
        let (backend, store) = manager.into_parts();
        assert_eq!(backend.validated.len(), 1);
        assert!(backend.applied.is_empty());
        assert!(store.record.is_none());
        assert_eq!(store.clear_count, 1);
    }

    #[test]
    fn focus_mode_requires_an_available_keeper_target() {
        let mut original = topology(2);
        original.paths[0].target.target_available = false;
        let selection = selected(&original, &[1]);
        let mut manager =
            DisplayManager::new(FakeBackend::new(original), MemoryRecoveryStore::default());

        let error = manager.activate(&selection).unwrap_err();

        assert_eq!(error.phase, DisplayPhase::SafetyCheck);
        assert!(error.message.contains("available"));
        let (backend, store) = manager.into_parts();
        assert!(backend.validated.is_empty());
        assert_eq!(store.save_count, 0);
    }

    #[test]
    fn rolls_back_and_verifies_after_apply_failure() {
        let original = topology(2);
        let selection = selected(&original, &[1]);
        let mut backend = FakeBackend::new(original.clone());
        backend.apply_failures.push_back(Some(injected_error(
            DisplayPhase::Apply,
            31,
            "apply failed",
        )));
        backend.apply_failures.push_back(None);
        let mut manager = DisplayManager::new(backend, MemoryRecoveryStore::default());

        let error = manager.activate(&selection).unwrap_err();

        assert_eq!(error.rollback, RollbackStatus::Succeeded);
        let (backend, store) = manager.into_parts();
        assert_eq!(
            backend.current.fingerprint().unwrap(),
            original.fingerprint().unwrap()
        );
        assert_eq!(backend.applied.len(), 2);
        assert!(store.record.is_none());
    }

    #[test]
    fn retains_recovery_record_when_rollback_fails() {
        let original = topology(2);
        let selection = selected(&original, &[1]);
        let mut backend = FakeBackend::new(original);
        backend.apply_failures.push_back(Some(injected_error(
            DisplayPhase::Apply,
            31,
            "apply failed",
        )));
        backend.apply_failures.push_back(Some(injected_error(
            DisplayPhase::Rollback,
            1167,
            "rollback failed",
        )));
        let mut manager = DisplayManager::new(backend, MemoryRecoveryStore::default());

        let error = manager.activate(&selection).unwrap_err();

        assert!(matches!(error.rollback, RollbackStatus::Failed { .. }));
        let (_, store) = manager.into_parts();
        assert!(store.record.is_some());
        assert_eq!(store.clear_count, 0);
    }

    #[test]
    fn verification_mismatch_triggers_exact_rollback() {
        let original = topology(3);
        let selection = selected(&original, &[2]);
        let mut drift = topology(2);
        drift.paths[1].target.rotation = 2;
        let mut backend = FakeBackend::new(original.clone());
        backend.query_results.push_back(Ok(original.clone()));
        backend.query_results.push_back(Ok(original.clone()));
        backend.query_results.push_back(Ok(drift));
        let mut manager = DisplayManager::new(backend, MemoryRecoveryStore::default());

        let error = manager.activate(&selection).unwrap_err();

        assert_eq!(error.phase, DisplayPhase::Verify);
        assert_eq!(error.rollback, RollbackStatus::Succeeded);
        let (backend, store) = manager.into_parts();
        assert_eq!(
            backend.current.fingerprint().unwrap(),
            original.fingerprint().unwrap()
        );
        assert!(store.record.is_none());
    }

    #[test]
    fn repeated_activation_is_idempotent() {
        let original = topology(2);
        let selection = selected(&original, &[1]);
        let mut manager =
            DisplayManager::new(FakeBackend::new(original), MemoryRecoveryStore::default());

        manager.activate(&selection).unwrap();
        let outcome = manager.activate(&selection).unwrap();

        assert_eq!(outcome.status, TransitionStatus::AlreadyFocused);
        let (backend, store) = manager.into_parts();
        assert_eq!(backend.applied.len(), 1);
        assert_eq!(store.save_count, 1);
    }

    #[test]
    fn restores_exact_snapshot_and_clears_journal() {
        let original = topology(3);
        let selection = selected(&original, &[1, 2]);
        let mut manager = DisplayManager::new(
            FakeBackend::new(original.clone()),
            MemoryRecoveryStore::default(),
        );
        manager.activate(&selection).unwrap();

        let outcome = manager.restore().unwrap();

        assert_eq!(outcome.status, TransitionStatus::Restored);
        assert!(!outcome.used_database_fallback);
        let (backend, store) = manager.into_parts();
        assert_eq!(backend.current, original);
        assert!(store.record.is_none());
    }

    #[test]
    fn restore_rechecks_focused_topology_after_validation() {
        let original = topology(3);
        let selection = selected(&original, &[2]);
        let mut manager = DisplayManager::new(
            FakeBackend::new(original.clone()),
            MemoryRecoveryStore::default(),
        );
        manager.activate(&selection).unwrap();
        let (mut backend, store) = manager.into_parts();
        let focused = backend.current.clone();
        let mut drift = focused.clone();
        drift.paths[1].target.rotation = 2;
        let apply_count = backend.applied.len();
        backend.query_results.push_back(Ok(focused));
        backend.query_results.push_back(Ok(drift));
        let mut manager = DisplayManager::new(backend, store);

        let error = manager.restore().unwrap_err();

        assert_eq!(error.phase, DisplayPhase::Restore);
        let (backend, store) = manager.into_parts();
        assert_eq!(backend.applied.len(), apply_count);
        assert!(store.record.is_some());
    }

    #[test]
    fn refuses_restore_after_unrelated_topology_drift() {
        let original = topology(3);
        let selection = selected(&original, &[2]);
        let mut manager = DisplayManager::new(
            FakeBackend::new(original.clone()),
            MemoryRecoveryStore::default(),
        );
        manager.activate(&selection).unwrap();
        let (mut backend, store) = manager.into_parts();
        backend.current = topology(2);
        backend.current.paths[1].target.rotation = 2;
        let apply_count = backend.applied.len();
        let mut manager = DisplayManager::new(backend, store);

        let error = manager.restore().unwrap_err();

        assert_eq!(error.phase, DisplayPhase::Restore);
        let (backend, store) = manager.into_parts();
        assert_eq!(backend.applied.len(), apply_count);
        assert!(store.record.is_some());
    }

    #[test]
    fn database_fallback_requires_expected_focused_topology() {
        let original = topology(2);
        let selection = selected(&original, &[1]);
        let mut manager = DisplayManager::new(
            FakeBackend::new(original.clone()),
            MemoryRecoveryStore::default(),
        );
        manager.activate(&selection).unwrap();
        let (mut backend, store) = manager.into_parts();
        backend.validate_failures.push_back(Some(injected_error(
            DisplayPhase::Validate,
            87,
            "session identifiers expired",
        )));
        let mut database = original.clone();
        for mode in &mut database.modes {
            if let ModeValueSnapshot::Target(target) = &mut mode.value {
                target.total_size = RegionSnapshot {
                    width: 0,
                    height: 0,
                };
            }
        }
        backend.database_topology = Some(database);
        let mut manager = DisplayManager::new(backend, store);

        let outcome = manager.restore().unwrap();

        assert_eq!(outcome.status, TransitionStatus::Restored);
        assert!(outcome.used_database_fallback);
        let (backend, store) = manager.into_parts();
        assert_eq!(backend.database_apply_count, 1);
        assert_eq!(backend.database_query_count, 1);
        assert_eq!(
            backend.current.fingerprint().unwrap(),
            original.fingerprint().unwrap()
        );
        assert!(store.record.is_none());
    }

    #[test]
    fn database_fallback_refuses_a_different_persisted_topology() {
        let original = topology(3);
        let selection = selected(&original, &[2]);
        let mut manager = DisplayManager::new(
            FakeBackend::new(original.clone()),
            MemoryRecoveryStore::default(),
        );
        manager.activate(&selection).unwrap();
        let (mut backend, store) = manager.into_parts();
        backend.validate_failures.push_back(Some(injected_error(
            DisplayPhase::Validate,
            87,
            "session identifiers expired",
        )));
        let mut database = original;
        database.paths[1].target.rotation = 2;
        backend.database_topology = Some(database);
        let mut manager = DisplayManager::new(backend, store);

        let error = manager.restore().unwrap_err();

        assert_eq!(error.phase, DisplayPhase::Restore);
        assert!(error.message.contains("database-current"));
        let (backend, store) = manager.into_parts();
        assert_eq!(backend.database_query_count, 1);
        assert_eq!(backend.database_apply_count, 0);
        assert!(store.record.is_some());
    }

    #[test]
    fn failed_restore_keeps_journal_for_a_later_retry() {
        let original = topology(2);
        let selection = selected(&original, &[1]);
        let mut manager = DisplayManager::new(
            FakeBackend::new(original.clone()),
            MemoryRecoveryStore::default(),
        );
        manager.activate(&selection).unwrap();
        let (mut backend, store) = manager.into_parts();
        backend.apply_failures.push_back(Some(injected_error(
            DisplayPhase::Apply,
            5,
            "access denied",
        )));
        let mut manager = DisplayManager::new(backend, store);

        let error = manager.restore().unwrap_err();

        assert_eq!(error.win32_code, Some(5));
        let (backend, store) = manager.into_parts();
        assert!(store.record.is_some());
        let mut manager = DisplayManager::new(backend, store);
        let outcome = manager.restore().unwrap();
        assert_eq!(outcome.status, TransitionStatus::Restored);
        let (backend, store) = manager.into_parts();
        assert_eq!(backend.current, original);
        assert!(store.record.is_none());
    }

    #[test]
    fn recovery_reports_recovered_status() {
        let original = topology(2);
        let selection = selected(&original, &[1]);
        let mut manager =
            DisplayManager::new(FakeBackend::new(original), MemoryRecoveryStore::default());
        manager.activate(&selection).unwrap();

        let outcome = manager.recover().unwrap();

        assert_eq!(outcome.status, TransitionStatus::Recovered);
    }

    #[test]
    fn journal_write_failure_prevents_display_mutation() {
        let original = topology(2);
        let selection = selected(&original, &[1]);
        let store = MemoryRecoveryStore {
            fail_save: true,
            ..Default::default()
        };
        let mut manager = DisplayManager::new(FakeBackend::new(original), store);

        let error = manager.activate(&selection).unwrap_err();

        assert_eq!(error.phase, DisplayPhase::JournalWrite);
        let (backend, _) = manager.into_parts();
        assert!(backend.validated.is_empty());
        assert!(backend.applied.is_empty());
    }

    #[test]
    fn rejects_internally_inconsistent_recovery_journal() {
        let original = topology(2);
        let focused = original
            .without_displays(&HashSet::from([original.paths[1].display_id().0]))
            .unwrap();
        let record = RecoveryRecord {
            version: RECOVERY_VERSION,
            original_topology: original.clone(),
            original_fingerprint: original.fingerprint().unwrap(),
            focused_fingerprint: TopologyFingerprint("tampered".to_string()),
            affected_display_ids: vec![original.paths[1].display_id()],
        };
        let store = MemoryRecoveryStore {
            record: Some(record),
            ..Default::default()
        };
        let mut manager = DisplayManager::new(FakeBackend::new(focused), store);

        let error = manager.recover().unwrap_err();

        assert_eq!(error.phase, DisplayPhase::JournalRead);
        let (backend, store) = manager.into_parts();
        assert!(backend.applied.is_empty());
        assert!(store.record.is_some());
    }

    #[test]
    fn fingerprints_ignore_session_identifiers_and_path_order() {
        let original = topology(3);
        let mut reidentified = original.clone();
        reidentified.paths.reverse();
        for (path_index, path) in reidentified.paths.iter_mut().enumerate() {
            let adapter = AdapterId {
                low_part: 900 + path_index as u32,
                high_part: 4,
            };
            let source_mode_index = path.source.mode_reference.source_mode_index().unwrap();
            let source_mode = &mut reidentified.modes[source_mode_index as usize];
            source_mode.adapter = adapter;
            source_mode.id = 700 + path_index as u32;
            path.source.adapter = adapter;
            path.source.id = source_mode.id;
            let target_mode_index = path.target.mode_reference.target_mode_index().unwrap();
            let target_mode = &mut reidentified.modes[target_mode_index as usize];
            target_mode.adapter = adapter;
            target_mode.id = 800 + path_index as u32;
            path.target.adapter = adapter;
            path.target.id = target_mode.id;
        }

        assert_eq!(
            original.fingerprint().unwrap(),
            reidentified.fingerprint().unwrap()
        );
    }

    #[test]
    fn fingerprints_detect_display_mode_changes() {
        let original = topology(2);
        let mut changed = original.clone();
        if let ModeValueSnapshot::Source(source) = &mut changed.modes[2].value {
            source.position.x += 1;
        }

        assert_ne!(
            original.fingerprint().unwrap(),
            changed.fingerprint().unwrap()
        );
    }

    #[test]
    fn fingerprints_tolerate_database_query_zeroed_total_signal_size() {
        let original = topology(2);
        let mut database = original.clone();
        for mode in &mut database.modes {
            if let ModeValueSnapshot::Target(target) = &mut mode.value {
                target.total_size = RegionSnapshot {
                    width: 0,
                    height: 0,
                };
            }
        }

        assert_eq!(
            original.fingerprint().unwrap(),
            database.fingerprint().unwrap()
        );
    }

    #[test]
    fn virtual_mode_references_and_drr_flags_round_trip_exactly() {
        let topology = virtual_topology(2);

        let (paths, modes) = topology_to_raw(&topology).unwrap();

        for (snapshot, raw) in topology.paths.iter().zip(&paths) {
            let raw_source = unsafe { raw.sourceInfo.Anonymous.modeInfoIdx };
            let raw_target = unsafe { raw.targetInfo.Anonymous.modeInfoIdx };
            assert_eq!(raw_source, snapshot.source.mode_reference.raw());
            assert_eq!(raw_target, snapshot.target.mode_reference.raw());
            assert_eq!(
                SourceModeReference::from_raw(raw_source, true),
                snapshot.source.mode_reference
            );
            assert_eq!(
                TargetModeReference::from_raw(raw_target, true),
                snapshot.target.mode_reference
            );
            assert_ne!(raw.flags & DISPLAYCONFIG_PATH_BOOST_REFRESH_RATE, 0);
        }
        let round_tripped_modes = modes
            .iter()
            .map(mode_from_raw)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(round_tripped_modes, topology.modes);
    }

    #[test]
    fn virtual_desktop_modes_are_remapped_with_kept_paths() {
        let original = virtual_topology(3);
        let focused = original
            .without_displays(&HashSet::from([original.paths[1].display_id().0]))
            .unwrap();

        assert_eq!(focused.paths.len(), 2);
        assert_eq!(focused.modes.len(), 6);
        assert!(focused.virtual_mode_aware);
        assert!(focused.virtual_refresh_rate_aware);
        for path in &focused.paths {
            let desktop = path.target.mode_reference.desktop_mode_index().unwrap();
            assert!(matches!(
                focused.modes[desktop as usize].value,
                ModeValueSnapshot::DesktopImage(_)
            ));
        }
    }

    #[test]
    fn fingerprints_include_virtual_clone_drr_and_desktop_state() {
        let original = virtual_topology(2);
        let original_fingerprint = original.fingerprint().unwrap();

        let mut without_boost = original.clone();
        without_boost.paths[1].flags &= !DISPLAYCONFIG_PATH_BOOST_REFRESH_RATE;
        assert_ne!(original_fingerprint, without_boost.fingerprint().unwrap());

        let mut different_clone = original.clone();
        if let SourceModeReference::Virtual { clone_group_id, .. } =
            &mut different_clone.paths[1].source.mode_reference
        {
            *clone_group_id += 1;
        }
        assert_ne!(original_fingerprint, different_clone.fingerprint().unwrap());

        let mut different_desktop = original.clone();
        let desktop = different_desktop.paths[1]
            .target
            .mode_reference
            .desktop_mode_index()
            .unwrap();
        if let ModeValueSnapshot::DesktopImage(mode) =
            &mut different_desktop.modes[desktop as usize].value
        {
            mode.desktop_image_clip.right -= 1;
        }
        assert_ne!(
            original_fingerprint,
            different_desktop.fingerprint().unwrap()
        );
    }

    struct RacingQueryApi {
        query_count: usize,
    }

    impl QueryApi for RacingQueryApi {
        fn buffer_sizes(
            &mut self,
            _flags: QUERY_DISPLAY_CONFIG_FLAGS,
            path_count: &mut u32,
            mode_count: &mut u32,
        ) -> u32 {
            *path_count = 1;
            *mode_count = 2;
            ERROR_SUCCESS.0
        }

        fn query(
            &mut self,
            _flags: QUERY_DISPLAY_CONFIG_FLAGS,
            path_count: &mut u32,
            _paths: &mut [DISPLAYCONFIG_PATH_INFO],
            mode_count: &mut u32,
            _modes: &mut [DISPLAYCONFIG_MODE_INFO],
            _require_topology_id: bool,
        ) -> u32 {
            self.query_count += 1;
            if self.query_count == 1 {
                ERROR_INSUFFICIENT_BUFFER.0
            } else {
                *path_count = 0;
                *mode_count = 0;
                ERROR_SUCCESS.0
            }
        }
    }

    #[test]
    fn query_retries_when_topology_changes_between_calls() {
        let mut api = RacingQueryApi { query_count: 0 };

        let raw = query_topology_with_awareness(&mut api).unwrap();

        assert_eq!(api.query_count, 2);
        assert!(raw.paths.is_empty());
        assert!(raw.modes.is_empty());
        assert!(raw.virtual_refresh_rate_aware);
    }

    struct NeverStableQueryApi;

    impl QueryApi for NeverStableQueryApi {
        fn buffer_sizes(
            &mut self,
            _flags: QUERY_DISPLAY_CONFIG_FLAGS,
            path_count: &mut u32,
            mode_count: &mut u32,
        ) -> u32 {
            *path_count = 1;
            *mode_count = 2;
            ERROR_SUCCESS.0
        }

        fn query(
            &mut self,
            _flags: QUERY_DISPLAY_CONFIG_FLAGS,
            _path_count: &mut u32,
            _paths: &mut [DISPLAYCONFIG_PATH_INFO],
            _mode_count: &mut u32,
            _modes: &mut [DISPLAYCONFIG_MODE_INFO],
            _require_topology_id: bool,
        ) -> u32 {
            ERROR_INSUFFICIENT_BUFFER.0
        }
    }

    #[test]
    fn query_stops_after_bounded_retries() {
        let error = match query_topology_with_awareness(&mut NeverStableQueryApi) {
            Ok(_) => panic!("query unexpectedly succeeded"),
            Err(error) => error,
        };

        assert_eq!(error.phase, DisplayPhase::Query);
        assert_eq!(error.win32_code, Some(ERROR_INSUFFICIENT_BUFFER.0));
    }

    #[derive(Default)]
    struct RefreshFallbackQueryApi {
        buffer_flags: Vec<u32>,
        query_flags: Vec<u32>,
        topology_id_requests: Vec<bool>,
    }

    impl QueryApi for RefreshFallbackQueryApi {
        fn buffer_sizes(
            &mut self,
            flags: QUERY_DISPLAY_CONFIG_FLAGS,
            path_count: &mut u32,
            mode_count: &mut u32,
        ) -> u32 {
            self.buffer_flags.push(flags.0);
            if flags.contains(QDC_VIRTUAL_REFRESH_RATE_AWARE) {
                return ERROR_INVALID_PARAMETER.0;
            }
            *path_count = 0;
            *mode_count = 0;
            ERROR_SUCCESS.0
        }

        fn query(
            &mut self,
            flags: QUERY_DISPLAY_CONFIG_FLAGS,
            path_count: &mut u32,
            _paths: &mut [DISPLAYCONFIG_PATH_INFO],
            mode_count: &mut u32,
            _modes: &mut [DISPLAYCONFIG_MODE_INFO],
            require_topology_id: bool,
        ) -> u32 {
            self.query_flags.push(flags.0);
            self.topology_id_requests.push(require_topology_id);
            *path_count = 0;
            *mode_count = 0;
            ERROR_SUCCESS.0
        }
    }

    #[test]
    fn query_falls_back_to_windows_10_virtual_mode_awareness() {
        let mut api = RefreshFallbackQueryApi::default();

        let raw = query_topology_with_awareness(&mut api).unwrap();

        assert!(raw.virtual_mode_aware);
        assert!(!raw.virtual_refresh_rate_aware);
        assert_eq!(api.buffer_flags.len(), 2);
        assert_ne!(api.buffer_flags[0] & QDC_VIRTUAL_REFRESH_RATE_AWARE.0, 0);
        assert_eq!(api.buffer_flags[1] & QDC_VIRTUAL_REFRESH_RATE_AWARE.0, 0);
        assert_ne!(api.buffer_flags[1] & QDC_VIRTUAL_MODE_AWARE.0, 0);
        assert_eq!(api.topology_id_requests, vec![false]);
    }

    #[test]
    fn database_query_requests_topology_id_and_virtual_awareness() {
        let mut api = RefreshFallbackQueryApi::default();

        let raw = query_topology_with_awareness_for(&mut api, QDC_DATABASE_CURRENT).unwrap();

        assert!(raw.virtual_mode_aware);
        assert!(!raw.virtual_refresh_rate_aware);
        assert_eq!(api.topology_id_requests, vec![true]);
        assert_ne!(api.query_flags[0] & QDC_DATABASE_CURRENT.0, 0);
        assert_ne!(api.query_flags[0] & QDC_VIRTUAL_MODE_AWARE.0, 0);
    }

    #[test]
    fn display_flags_never_persist_focus_topology() {
        let topology = topology(1);
        let validation = supplied_flags(SDC_VALIDATE, &topology);
        let application = supplied_flags(SDC_APPLY, &topology);
        let database = database_apply_flags();

        assert!(!validation.contains(SDC_SAVE_TO_DATABASE));
        assert!(!application.contains(SDC_SAVE_TO_DATABASE));
        assert!(!database.contains(SDC_SAVE_TO_DATABASE));
        assert!(validation.contains(SDC_USE_SUPPLIED_DISPLAY_CONFIG));
        assert!(application.contains(SDC_USE_SUPPLIED_DISPLAY_CONFIG));
        assert!(database.contains(SDC_USE_DATABASE_CURRENT));
        assert!(!validation.contains(SDC_VIRTUAL_MODE_AWARE));
        assert!(!application.contains(SDC_VIRTUAL_REFRESH_RATE_AWARE));

        let virtual_topology = virtual_topology(1);
        let validation = supplied_flags(SDC_VALIDATE, &virtual_topology);
        let application = supplied_flags(SDC_APPLY, &virtual_topology);
        for flags in [validation, application] {
            assert!(flags.contains(SDC_VIRTUAL_MODE_AWARE));
            assert!(flags.contains(SDC_VIRTUAL_REFRESH_RATE_AWARE));
            assert!(!flags.contains(SDC_SAVE_TO_DATABASE));
        }
        let database = database_apply_flags();
        assert!(!database.contains(SDC_VIRTUAL_MODE_AWARE));
        assert!(!database.contains(SDC_VIRTUAL_REFRESH_RATE_AWARE));
    }

    #[test]
    fn json_recovery_store_round_trips_record() {
        let root = std::env::temp_dir().join(format!(
            "monitor-manager-display-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = root.join("recovery.json");
        let original = topology(2);
        let focused = original
            .without_displays(&HashSet::from([original.paths[1].display_id().0]))
            .unwrap();
        let record = RecoveryRecord {
            version: RECOVERY_VERSION,
            original_fingerprint: original.fingerprint().unwrap(),
            focused_fingerprint: focused.fingerprint().unwrap(),
            affected_display_ids: vec![original.paths[1].display_id()],
            original_topology: original,
        };
        let mut store = JsonRecoveryStore::new(&path);

        store.save(&record).unwrap();
        assert_eq!(store.load().unwrap(), Some(record));
        store.clear().unwrap();
        assert_eq!(store.load().unwrap(), None);
        fs::remove_dir_all(&root).unwrap();
    }
}
