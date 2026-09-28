//! One visual contract for every active DIT engine.

use super::{CYAN, GRAY, GREEN, RED, RESET, TerminalSize, YELLOW, bar, sanitize_and_clip};

#[derive(Clone, Debug, PartialEq)]
pub struct DashboardMeter {
    pub label: String,
    pub percent: Option<f64>,
    pub suffix: String,
    pub detail: Option<String>,
}

impl DashboardMeter {
    #[must_use]
    pub fn new(label: impl Into<String>, percent: Option<f64>) -> Self {
        Self {
            label: label.into(),
            percent,
            suffix: String::new(),
            detail: None,
        }
    }

    #[must_use]
    pub fn suffix(mut self, suffix: impl Into<String>) -> Self {
        self.suffix = suffix.into();
        self
    }

    #[must_use]
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct UnifiedDashboard {
    pub engine: String,
    pub report_abi: String,
    pub profile: String,
    pub hardware_rows: Vec<String>,
    pub live_meters: Vec<DashboardMeter>,
    pub state: String,
    pub progress_percent: Option<f64>,
    pub elapsed: String,
    pub target: String,
    pub active_test: String,
    pub process: String,
    pub calculations_completed: u64,
    pub failures: u64,
    pub running_rows: Vec<String>,
    pub footer: String,
}

/// Render the stable DIT shell. Engines may only vary the meter list and the
/// RUNNING rows; geometry, headings, colors and progress semantics stay common.
#[must_use]
pub fn render_unified_dashboard(
    dashboard: &UnifiedDashboard,
    size: TerminalSize,
    color: bool,
) -> String {
    let width = usize::from(size.cols).clamp(40, 240);
    let height = usize::from(size.rows).saturating_sub(1).max(1);
    let mut lines = if width >= 110 && size.rows >= 29 {
        render_wide(dashboard, width)
    } else {
        render_compact(dashboard, width)
    };
    if lines.len() > height {
        lines.truncate(height.saturating_sub(2));
        lines.push(full_row(&dashboard.footer, width));
        lines.push(border("", width));
    } else {
        while lines.len() < height {
            let insert_at = lines.len().saturating_sub(2);
            lines.insert(insert_at, full_row("", width));
        }
    }
    debug_assert!(lines.len() <= height);
    if color {
        paint(&lines, dashboard.failures)
    } else {
        lines.join("\n")
    }
}

fn render_wide(dashboard: &UnifiedDashboard, width: usize) -> Vec<String> {
    let gap = 2;
    let left_width = (width - gap) / 2;
    let right_width = width - gap - left_width;
    let right_inner = right_width.saturating_sub(4);
    let mut live_rows = Vec::new();
    for metric in &dashboard.live_meters {
        live_rows.push(meter(metric, right_inner));
        if let Some(detail) = &metric.detail {
            live_rows.push(detail.clone());
        }
    }
    let panel_rows = dashboard.hardware_rows.len().max(live_rows.len());
    let mut lines = vec![
        border("", width),
        full_row("DIT-01 / DATA INTEGRITY TEST ENGINE", width),
        full_row(
            &format!(
                "ENGINE {}    REPORT ABI {}    PROFILE {}",
                dashboard.engine, dashboard.report_abi, dashboard.profile
            ),
            width,
        ),
        pair(
            &border("SYSTEM HARDWARE", left_width),
            &border("LIVE UTILIZATION", right_width),
            gap,
        ),
    ];
    for index in 0..panel_rows {
        lines.push(pair(
            &panel_row(
                dashboard
                    .hardware_rows
                    .get(index)
                    .map_or("", String::as_str),
                left_width,
            ),
            &panel_row(live_rows.get(index).map_or("", String::as_str), right_width),
            gap,
        ));
    }
    lines.push(pair(&border("", left_width), &border("", right_width), gap));
    lines.extend(render_running(dashboard, width));
    lines
}

fn render_compact(dashboard: &UnifiedDashboard, width: usize) -> Vec<String> {
    let mut lines = vec![
        border("", width),
        full_row("DIT-01 / DATA INTEGRITY TEST ENGINE", width),
        full_row(
            &format!(
                "ENGINE {} / ABI {} / PROFILE {}",
                dashboard.engine, dashboard.report_abi, dashboard.profile
            ),
            width,
        ),
        border("SYSTEM HARDWARE / LIVE UTILIZATION", width),
    ];
    for hardware in dashboard.hardware_rows.iter().take(3) {
        lines.push(full_row(hardware, width));
    }
    let inner = width.saturating_sub(4);
    for metric in &dashboard.live_meters {
        lines.push(full_row(&meter(metric, inner), width));
        if let Some(detail) = &metric.detail {
            lines.push(full_row(detail, width));
        }
    }
    lines.extend(render_running(dashboard, width));
    lines
}

fn render_running(dashboard: &UnifiedDashboard, width: usize) -> Vec<String> {
    let inner = width.saturating_sub(4);
    let mut lines = vec![
        border("TESTING / PROCESS INFORMATION", width),
        full_row(
            &format!(
                "STATE: {}    PROGRESS: {}    TIME: {} / {}",
                dashboard.state,
                dashboard.progress_percent.map_or_else(
                    || "N/A".to_owned(),
                    |value| format!("{:.0}%", value.clamp(0.0, 100.0))
                ),
                dashboard.elapsed,
                dashboard.target
            ),
            width,
        ),
        full_row(
            &meter(
                &DashboardMeter::new("PROGRESS", dashboard.progress_percent),
                inner,
            ),
            width,
        ),
        full_row(&format!("ACTIVE TEST   {}", dashboard.active_test), width),
        full_row(&format!("PROCESS       {}", dashboard.process), width),
        full_row(
            &format!(
                "CALCULATIONS COMPLETED  {}    |    FAIL TOTAL  {}",
                dashboard.calculations_completed, dashboard.failures
            ),
            width,
        ),
    ];
    lines.extend(
        dashboard
            .running_rows
            .iter()
            .map(|line| full_row(line, width)),
    );
    lines.push(full_row(&dashboard.footer, width));
    lines.push(border("", width));
    lines
}

fn meter(metric: &DashboardMeter, width: usize) -> String {
    let percent = metric.percent.map(|value| value.clamp(0.0, 100.0));
    let percent_text = percent.map_or_else(|| "N/A".to_owned(), |value| format!("{value:.0}%"));
    let suffix = if metric.suffix.is_empty() {
        format!(" {percent_text}")
    } else {
        format!(" {}", metric.suffix)
    };
    let bar_width = width
        .saturating_sub(metric.label.chars().count() + suffix.chars().count() + 3)
        .max(1);
    format!(
        "{} [{}]{}",
        metric.label,
        bar(percent.unwrap_or(0.0), 100.0, bar_width),
        suffix
    )
}

fn pair(left: &str, right: &str, gap: usize) -> String {
    format!("{left}{}{right}", " ".repeat(gap))
}

fn border(title: &str, width: usize) -> String {
    let inner = width.saturating_sub(2);
    if title.is_empty() {
        return format!("+{}+", "-".repeat(inner));
    }
    let title = sanitize_and_clip(&format!("-- {title} "), inner);
    format!("+{title}{}+", "-".repeat(inner.saturating_sub(title.len())))
}

fn panel_row(content: &str, width: usize) -> String {
    let inner = width.saturating_sub(4);
    let text = sanitize_and_clip(content, inner);
    format!("| {text:<inner$} |")
}

fn full_row(content: &str, width: usize) -> String {
    panel_row(content, width)
}

fn paint(lines: &[String], failures: u64) -> String {
    lines
        .iter()
        .map(|line| {
            let text_color =
                if failures > 0 && line.contains("FAIL") && !line.contains("FAIL TOTAL  0") {
                    RED
                } else if line.contains("PASS") {
                    GREEN
                } else if line.contains("INVALID")
                    || line.contains("ABORTED")
                    || line.contains("TEMP LIMIT")
                {
                    YELLOW
                } else if line.contains("ENGINE")
                    || line.contains("SYSTEM HARDWARE")
                    || line.contains("LIVE UTILIZATION")
                    || line.contains("TESTING / PROCESS")
                {
                    CYAN
                } else {
                    "\x1b[37m"
                };
            let mut result = String::new();
            let mut in_bar = false;
            let mut previous_color = "";
            for character in line.chars() {
                let current = if character == '[' {
                    in_bar = true;
                    CYAN
                } else if character == ']' {
                    in_bar = false;
                    CYAN
                } else if in_bar && character == '#' {
                    GREEN
                } else if in_bar && character == '-' {
                    GRAY
                } else if matches!(character, '+' | '|' | '-') {
                    CYAN
                } else {
                    text_color
                };
                if current != previous_color {
                    result.push_str(current);
                    previous_color = current;
                }
                result.push(character);
            }
            result.push_str(RESET);
            result
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> UnifiedDashboard {
        UnifiedDashboard {
            engine: "TEST-1".to_owned(),
            report_abi: "9".to_owned(),
            profile: "CPU + RAM".to_owned(),
            hardware_rows: vec!["CPU: Example".to_owned(), "RAM: 32 GiB".to_owned()],
            live_meters: vec![
                DashboardMeter::new("CPU USAGE", Some(50.0))
                    .detail("Freq 4.50 GHz / Temp 70.0 C / Power 100.0 W"),
            ],
            state: "RUNNING".to_owned(),
            progress_percent: Some(25.0),
            elapsed: "00:01:00".to_owned(),
            target: "00:04:00".to_owned(),
            active_test: "TEST-A".to_owned(),
            process: "engine-specific running text".to_owned(),
            calculations_completed: 123,
            failures: 0,
            running_rows: vec!["SEED 0x1234".to_owned()],
            footer: "TEMP LIMIT OFF | Ctrl+C preserves report".to_owned(),
        }
    }

    #[test]
    fn wide_and_compact_share_the_same_fixed_contract() {
        for size in [
            TerminalSize {
                rows: 36,
                cols: 140,
            },
            TerminalSize { rows: 24, cols: 90 },
        ] {
            let frame = render_unified_dashboard(&model(), size, false);
            assert!(frame.contains("DIT-01 / DATA INTEGRITY TEST ENGINE"));
            assert!(frame.contains("SYSTEM HARDWARE"));
            assert!(frame.contains("LIVE UTILIZATION"));
            assert!(frame.contains("TESTING / PROCESS INFORMATION"));
            assert!(frame.contains("ACTIVE TEST   TEST-A"));
            assert!(frame.contains("CALCULATIONS COMPLETED  123"));
            assert!(
                frame
                    .lines()
                    .all(|line| line.chars().count() == usize::from(size.cols))
            );
            assert!(frame.lines().count() < usize::from(size.rows));
        }
    }

    #[test]
    fn engines_change_running_payload_without_changing_dashboard_shell() {
        let cpu = model();
        let mut gpu = model();
        gpu.profile = "GPU + VRAM".to_owned();
        gpu.active_test = "GPU-XFER-MATRIX".to_owned();
        gpu.process = "Vulkan matrix + CPU oracle".to_owned();
        let size = TerminalSize {
            rows: 36,
            cols: 140,
        };
        let cpu_frame = render_unified_dashboard(&cpu, size, false);
        let gpu_frame = render_unified_dashboard(&gpu, size, false);
        for heading in [
            "DIT-01 / DATA INTEGRITY TEST ENGINE",
            "SYSTEM HARDWARE",
            "LIVE UTILIZATION",
            "TESTING / PROCESS INFORMATION",
            "CALCULATIONS COMPLETED",
        ] {
            let cpu_row = cpu_frame
                .lines()
                .position(|line| line.contains(heading))
                .unwrap();
            let gpu_row = gpu_frame
                .lines()
                .position(|line| line.contains(heading))
                .unwrap();
            assert_eq!(cpu_row, gpu_row, "fixed heading moved: {heading}");
        }
        assert!(cpu_frame.contains("ACTIVE TEST   TEST-A"));
        assert!(gpu_frame.contains("ACTIVE TEST   GPU-XFER-MATRIX"));
    }
}
