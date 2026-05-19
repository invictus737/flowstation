use std::process::{Command, Output};
use std::time::Duration;

const SERVICE_UNIT_ENV: &str = "FLOWSTATION_SERVICE_UNIT";
const DEFAULT_SERVICE_UNIT: &str = "tetra-bluestation.service";

#[derive(Debug, Clone, Copy)]
pub enum ServiceAction {
    Restart,
    Stop,
}

impl ServiceAction {
    fn systemctl_verb(self) -> &'static str {
        match self {
            ServiceAction::Restart => "restart",
            ServiceAction::Stop => "stop",
        }
    }

    fn label(self) -> &'static str {
        match self {
            ServiceAction::Restart => "restart",
            ServiceAction::Stop => "shutdown",
        }
    }
}

pub fn schedule_service_action(action: ServiceAction, delay: Duration) {
    let unit = resolve_service_unit();
    let service_user = service_user(&unit).unwrap_or_else(|| "unknown".to_string());
    tracing::warn!(
        "Service control: scheduling {} for {} (unit User={}) in {:?}",
        action.label(),
        unit,
        service_user,
        delay
    );

    std::thread::Builder::new()
        .name("service-control".into())
        .spawn(move || {
            std::thread::sleep(delay);
            match run_service_action(action, &unit) {
                Ok(()) => tracing::info!("Service control: {} requested for {}", action.label(), unit),
                Err(e) => tracing::error!("Service control: {} failed for {}: {}", action.label(), unit, e),
            }
        })
        .ok();
}

pub fn resolve_service_unit() -> String {
    if let Ok(value) = std::env::var(SERVICE_UNIT_ENV) {
        if let Some(unit) = normalize_service_unit(&value) {
            return unit;
        }
        tracing::warn!("Service control: ignoring invalid {}={:?}", SERVICE_UNIT_ENV, value);
    }

    std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|text| service_unit_from_cgroup_text(&text))
        .unwrap_or_else(|| DEFAULT_SERVICE_UNIT.to_string())
}

fn run_service_action(action: ServiceAction, unit: &str) -> Result<(), String> {
    let verb = action.systemctl_verb();
    match run_command("systemctl", &[verb, unit]) {
        Ok(()) => Ok(()),
        Err(systemctl_err) => match run_command("sudo", &["-n", "systemctl", verb, unit]) {
            Ok(()) => Ok(()),
            Err(sudo_err) => Err(format!("systemctl: {}; sudo -n: {}", systemctl_err, sudo_err)),
        },
    }
}

fn run_command(program: &str, args: &[&str]) -> Result<(), String> {
    match Command::new(program).args(args).output() {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(output_error(output)),
        Err(e) => Err(e.to_string()),
    }
}

fn output_error(output: Output) -> String {
    let status = output.status.to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stderr.is_empty() {
        format!("{}: {}", status, stderr)
    } else if !stdout.is_empty() {
        format!("{}: {}", status, stdout)
    } else {
        status
    }
}

fn service_user(unit: &str) -> Option<String> {
    let output = Command::new("systemctl")
        .args(["show", unit, "--property=User", "--value"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let user = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if user.is_empty() { Some("root".to_string()) } else { Some(user) }
}

fn service_unit_from_cgroup_text(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        line.split('/')
            .find(|component| component.ends_with(".service"))
            .and_then(normalize_service_unit)
    })
}

fn normalize_service_unit(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.contains('/') || trimmed.contains('\0') {
        return None;
    }

    let unit = if trimmed.ends_with(".service") {
        trimmed.to_string()
    } else {
        format!("{}.service", trimmed)
    };

    if unit
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'@' | b':' | b'\\'))
    {
        Some(unit)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{normalize_service_unit, service_unit_from_cgroup_text};

    #[test]
    fn finds_service_unit_in_cgroup_v2() {
        let text = "0::/system.slice/tetra-bluestation.service\n";
        assert_eq!(service_unit_from_cgroup_text(text).as_deref(), Some("tetra-bluestation.service"));
    }

    #[test]
    fn normalizes_unit_without_suffix() {
        assert_eq!(
            normalize_service_unit("tetra-bluestation").as_deref(),
            Some("tetra-bluestation.service")
        );
    }

    #[test]
    fn rejects_path_like_unit_names() {
        assert!(normalize_service_unit("../tetra").is_none());
    }
}
