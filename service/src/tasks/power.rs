use std::sync::Arc;

use tokio::time::{sleep, Duration, Instant};
use tracing::{debug, info, warn};

use crate::cli::framework_tool_parser::PowerBatteryInfo;
use crate::cli::FrameworkTool;
use crate::types::{Config, PowerConfig, PowerProfile};
use crate::utils::reconciler::{ReconcileOutcome, Reconciler, ReconcilerPolicy, SettingIo};

#[cfg(target_os = "windows")]
use crate::cli::RyzenAdj;

#[cfg(target_os = "linux")]
use crate::cli::LinuxPower;

const LOOP_INTERVAL_SECS: u64 = 1;

fn select_profile_from_power(cfg_power: PowerConfig, power: &PowerBatteryInfo) -> Option<PowerProfile> {
    match (power.battery_present, power.ac_present) {
        (Some(false), _) => cfg_power.ac,
        (_, Some(true)) => cfg_power.ac,
        (_, Some(false)) => cfg_power.battery,
        _ => None,
    }
}

async fn get_profile(
    cfg: &Arc<tokio::sync::RwLock<Config>>,
    framework_tool_lock: &Arc<tokio::sync::RwLock<Option<FrameworkTool>>>,
) -> Option<PowerProfile> {
    let Some(ft) = framework_tool_lock.read().await.clone() else {
        return None;
    };

    let cfg_power = { cfg.read().await.power.clone() };

    let Ok(p) = ft.power().await else {
        return None;
    };
    select_profile_from_power(cfg_power, &p)
}

fn log_outcome(setting: &str, target: &str, outcome: &ReconcileOutcome) {
    match outcome {
        ReconcileOutcome::ApplyFailed(e) => {
            warn!("power: {} apply failed (target={}): {}", setting, target, e);
        }
        ReconcileOutcome::Noop => {}
        ReconcileOutcome::Cooldown { remaining } => {
            debug!(
                "power: {} outcome=Cooldown (remaining={}s, target={})",
                setting,
                remaining.as_secs(),
                target
            );
        }
        ReconcileOutcome::QuietWindow { remaining } => {
            debug!(
                "power: {} outcome=QuietWindow (remaining={}s, target={})",
                setting,
                remaining.as_secs(),
                target
            );
        }
        other => {
            debug!("power: {} outcome={:?} (target={})", setting, other, target);
        }
    }
}

#[cfg(target_os = "windows")]
struct WindowsTdpIo {
    ryz: RyzenAdj,
}

#[cfg(target_os = "windows")]
impl SettingIo<u32> for WindowsTdpIo {
    fn read_current<'a>(
        &'a self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Option<u32>, String>> + Send + 'a>> {
        Box::pin(async move { Ok(self.ryz.info().await.ok().and_then(|i| i.tdp_watts)) })
    }

    fn apply_target<'a>(
        &'a self,
        target: &'a u32,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move { self.ryz.set_tdp_watts(*target).await })
    }
}

#[cfg(target_os = "windows")]
struct WindowsThermalIo {
    ryz: RyzenAdj,
}

#[cfg(target_os = "windows")]
impl SettingIo<u32> for WindowsThermalIo {
    fn read_current<'a>(
        &'a self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Option<u32>, String>> + Send + 'a>> {
        Box::pin(async move { Ok(self.ryz.info().await.ok().and_then(|i| i.thermal_limit_c)) })
    }

    fn apply_target<'a>(
        &'a self,
        target: &'a u32,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move { self.ryz.set_thermal_limit_c(*target).await })
    }
}

#[cfg(target_os = "linux")]
struct LinuxGovernorIo {
    lp: LinuxPower,
}

#[cfg(target_os = "linux")]
impl SettingIo<String> for LinuxGovernorIo {
    fn read_current<'a>(
        &'a self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Option<String>, String>> + Send + 'a>> {
        Box::pin(async move { self.lp.get_state().await.map(|s| s.governor).map_err(|e| e) })
    }

    fn apply_target<'a>(
        &'a self,
        target: &'a String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move { self.lp.set_governor(target).await })
    }
}

#[cfg(target_os = "linux")]
struct LinuxEppIo {
    lp: LinuxPower,
}

#[cfg(target_os = "linux")]
impl SettingIo<String> for LinuxEppIo {
    fn read_current<'a>(
        &'a self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Option<String>, String>> + Send + 'a>> {
        Box::pin(async move { self.lp.get_state().await.map(|s| s.epp_preference).map_err(|e| e) })
    }

    fn apply_target<'a>(
        &'a self,
        target: &'a String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move { self.lp.set_epp_preference(target).await })
    }
}

#[cfg(target_os = "linux")]
struct LinuxFreqLimitsIo {
    lp: LinuxPower,
    target: (Option<u32>, Option<u32>),
}

#[cfg(target_os = "linux")]
impl SettingIo<(Option<u32>, Option<u32>)> for LinuxFreqLimitsIo {
    fn read_current<'a>(
        &'a self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<(Option<u32>, Option<u32>)>, String>> + Send + 'a>,
    > {
        let mask = (self.target.0.is_some(), self.target.1.is_some());
        Box::pin(async move {
            let (cur_min, cur_max) = self.lp.get_configured_frequency_limits().await?;
            Ok(Some((
                if mask.0 { Some(cur_min) } else { None },
                if mask.1 { Some(cur_max) } else { None },
            )))
        })
    }

    fn apply_target<'a>(
        &'a self,
        target: &'a (Option<u32>, Option<u32>),
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move { self.lp.set_freq_limits(target.0, target.1).await })
    }
}

#[cfg(target_os = "windows")]
pub async fn run(
    power_backend_lock: Arc<tokio::sync::RwLock<Option<RyzenAdj>>>,
    cfg: Arc<tokio::sync::RwLock<Config>>,
    framework_tool_lock: Arc<tokio::sync::RwLock<Option<FrameworkTool>>>,
) {
    info!("Power task started (Windows/RyzenAdj)");

    let now = Instant::now();

    let mut tdp = Reconciler::new(ReconcilerPolicy::default(), now);

    let mut thermal = Reconciler::new(ReconcilerPolicy::default(), now);

    loop {
        let Some(ryz) = power_backend_lock.read().await.clone() else {
            sleep(Duration::from_secs(LOOP_INTERVAL_SECS)).await;
            continue;
        };

        let Some(profile) = get_profile(&cfg, &framework_tool_lock).await else {
            sleep(Duration::from_secs(LOOP_INTERVAL_SECS)).await;
            continue;
        };

        if let Some(setting) = profile.tdp_watts.as_ref() {
            let enabled = setting.enabled && setting.value > 0;
            let io = WindowsTdpIo { ryz: ryz.clone() };
            let outcome = tdp.reconcile(enabled, Some(setting.value), &io).await;
            if let crate::utils::reconciler::ReconcileOutcome::ApplyFailed(e) = outcome {
                warn!("power: tdp apply failed: {}", e);
            }
        }

        if let Some(setting) = profile.thermal_limit_c.as_ref() {
            let enabled = setting.enabled && setting.value > 0;
            let io = WindowsThermalIo { ryz: ryz.clone() };
            let outcome = thermal.reconcile(enabled, Some(setting.value), &io).await;
            if let crate::utils::reconciler::ReconcileOutcome::ApplyFailed(e) = outcome {
                warn!("power: thermal apply failed: {}", e);
            }
        }

        sleep(Duration::from_secs(LOOP_INTERVAL_SECS)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{PowerConfig, PowerProfile, SettingU32};

    fn profile(value: u32) -> PowerProfile {
        PowerProfile {
            thermal_limit_c: Some(SettingU32 { enabled: true, value }),
            ..Default::default()
        }
    }

    fn selected_value(power: PowerBatteryInfo) -> Option<u32> {
        let cfg = PowerConfig {
            ac: Some(profile(90)),
            battery: Some(profile(70)),
        };
        select_profile_from_power(cfg, &power).and_then(|p| p.thermal_limit_c.map(|s| s.value))
    }

    #[test]
    fn selects_ac_profile_when_battery_present_and_ac_connected() {
        assert_eq!(
            selected_value(PowerBatteryInfo {
                battery_present: Some(true),
                ac_present: Some(true),
                ..Default::default()
            }),
            Some(90)
        );
    }

    #[test]
    fn selects_battery_profile_when_battery_present_and_ac_disconnected() {
        assert_eq!(
            selected_value(PowerBatteryInfo {
                battery_present: Some(true),
                ac_present: Some(false),
                ..Default::default()
            }),
            Some(70)
        );
    }

    #[test]
    fn selects_ac_profile_when_no_battery_is_present() {
        assert_eq!(
            selected_value(PowerBatteryInfo {
                battery_present: Some(false),
                ac_present: None,
                ..Default::default()
            }),
            Some(90)
        );
    }

    #[test]
    fn skips_when_power_source_is_unknown_and_battery_is_not_explicitly_absent() {
        assert_eq!(
            selected_value(PowerBatteryInfo {
                battery_present: None,
                ac_present: None,
                ..Default::default()
            }),
            None
        );
    }
}

#[cfg(target_os = "linux")]
pub async fn run(
    power_backend_lock: Arc<tokio::sync::RwLock<Option<LinuxPower>>>,
    cfg: Arc<tokio::sync::RwLock<Config>>,
    framework_tool_lock: Arc<tokio::sync::RwLock<Option<FrameworkTool>>>,
) {
    info!("Power task started (Linux native)");

    let now = Instant::now();

    let mut governor = Reconciler::new(ReconcilerPolicy::default(), now);
    let mut epp = Reconciler::new(ReconcilerPolicy::default(), now);
    let mut freq_limits = Reconciler::new(ReconcilerPolicy::default(), now);

    loop {
        let Some(lp) = power_backend_lock.read().await.clone() else {
            sleep(Duration::from_secs(LOOP_INTERVAL_SECS)).await;
            continue;
        };

        let Some(profile) = get_profile(&cfg, &framework_tool_lock).await else {
            sleep(Duration::from_secs(LOOP_INTERVAL_SECS)).await;
            continue;
        };

        if let Some(setting) = profile.governor.as_ref() {
            let enabled = setting.enabled && !setting.value.trim().is_empty();
            let io = LinuxGovernorIo { lp: lp.clone() };
            let outcome = governor.reconcile(enabled, Some(setting.value.clone()), &io).await;
            log_outcome("governor", &format!("'{}'", setting.value), &outcome);
        }

        if let Some(setting) = profile.epp_preference.as_ref() {
            let enabled = setting.enabled && !setting.value.trim().is_empty();
            let io = LinuxEppIo { lp: lp.clone() };
            let outcome = epp.reconcile(enabled, Some(setting.value.clone()), &io).await;
            log_outcome("epp", &format!("'{}'", setting.value), &outcome);
        }

        let min_setting = profile.min_freq_mhz.as_ref();
        let max_setting = profile.max_freq_mhz.as_ref();
        if min_setting.is_some() || max_setting.is_some() {
            let target = (
                min_setting.filter(|s| s.enabled && s.value > 0).map(|s| s.value),
                max_setting.filter(|s| s.enabled && s.value > 0).map(|s| s.value),
            );
            let enabled = target.0.is_some() || target.1.is_some();
            let io = LinuxFreqLimitsIo { lp: lp.clone(), target };
            let outcome = freq_limits.reconcile(enabled, Some(target), &io).await;
            log_outcome("freq limits", &format!("{:?}-{:?} MHz", target.0, target.1), &outcome);
        }

        sleep(Duration::from_secs(LOOP_INTERVAL_SECS)).await;
    }
}
