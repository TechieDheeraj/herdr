use super::*;

const MIN_HEIGHT: u16 = 16;
const MIN_PRIMARY_ROWS: u16 = 6;
const MIN_HEALTH_ROWS: u16 = 3;
const HEALTH_ROWS: u16 = 6;

/// Keep the upstream sidebar geometry intact inside the space above Health Check.
pub(super) fn primary_area(area: Rect, split: Option<f32>) -> Rect {
    if area.width < 2 || area.height < MIN_HEIGHT {
        return area;
    }
    let requested = split
        .filter(|ratio| ratio.is_finite())
        .map(|ratio| (f32::from(area.height) * ratio.clamp(0.0, 1.0)).round() as u16)
        .unwrap_or_else(|| area.height.saturating_sub(HEALTH_ROWS));
    Rect::new(
        area.x,
        area.y,
        area.width,
        requested.clamp(
            MIN_PRIMARY_ROWS,
            area.height.saturating_sub(MIN_HEALTH_ROWS),
        ),
    )
}

pub(super) fn health_area(area: Rect, split: Option<f32>) -> Rect {
    let primary = primary_area(area, split);
    if primary == area {
        return Rect::default();
    }
    Rect::new(
        area.x,
        primary.bottom(),
        area.width.saturating_sub(1),
        area.height - primary.height,
    )
}

pub(super) fn split_for_row(area: Rect, row: u16) -> Option<f32> {
    (area.height >= MIN_HEIGHT).then(|| {
        f32::from(
            row.saturating_sub(area.y)
                .clamp(MIN_PRIMARY_ROWS, area.height - MIN_HEALTH_ROWS),
        ) / f32::from(area.height)
    })
}

pub(super) fn render(
    buffer: &mut Buffer,
    area: Rect,
    snapshot: Option<&ClientShellSnapshot>,
    split: Option<f32>,
    palette: &Palette,
    hits: &mut ShellHitMap,
) {
    let area = health_area(area, split);
    if area.is_empty() {
        return;
    }
    hits.sidebar_health_divider = Rect::new(area.x, area.y, area.width, 1);
    render::put_text(
        buffer,
        area.x,
        area.y,
        area.width,
        &"─".repeat(usize::from(area.width)),
        Style::default().fg(palette.surface_dim),
    );
    render::put_text(
        buffer,
        area.x,
        area.y + 1,
        area.width,
        " health check",
        Style::default()
            .fg(palette.overlay0)
            .add_modifier(Modifier::BOLD),
    );
    let health = snapshot.map(|snapshot| snapshot.health).unwrap_or_default();
    let cpu = health
        .cpu_usage_percentage
        .map(|value| format!("{value}%"))
        .unwrap_or_else(|| "n/a".into());
    let temperature = health
        .temperature_celsius
        .map(|value| format!("{value}°C"))
        .unwrap_or_else(|| "n/a".into());
    let battery = health
        .battery
        .map(|battery| match battery.charging {
            Some(true) => format!("{}% ↑", battery.percentage),
            Some(false) => format!("{}% ↓", battery.percentage),
            None => format!("{}%", battery.percentage),
        })
        .unwrap_or_else(|| "n/a".into());
    let value_width = usize::from(area.width.saturating_sub(9));
    let disk_reserve = if area.height == HEALTH_ROWS { 2 } else { 0 };
    let ram = health
        .ram
        .map(|usage| format_usage(usage, value_width))
        .unwrap_or_else(|| "n/a".into());
    let disk = health
        .disk
        .map(|usage| format_usage(usage, value_width.saturating_sub(usize::from(disk_reserve))))
        .unwrap_or_else(|| "n/a".into());
    for (offset, label, value, reserve) in [
        (2, "cpu/temp", format!("{cpu} · {temperature}"), 0),
        (3, "battery", battery, 0),
        (4, "ram", ram, 0),
        (5, "disk", disk, disk_reserve),
    ] {
        if offset >= area.height {
            continue;
        }
        let row = area.y + offset;
        render::put_text(
            buffer,
            area.x,
            row,
            area.width.min(9),
            &format!(" {label}"),
            Style::default().fg(palette.overlay0),
        );
        let values = Rect::new(
            area.x + 9.min(area.width),
            row,
            area.width.saturating_sub(9 + reserve),
            1,
        );
        render::put_right_text(
            buffer,
            values,
            row,
            &value,
            Style::default().fg(palette.subtext0),
        );
    }
}

fn format_usage(usage: crate::health::ResourceUsage, width: usize) -> String {
    const UNITS: [(&str, &str, u64); 5] = [
        ("B", "B", 1),
        ("KiB", "K", 1024),
        ("MiB", "M", 1024 * 1024),
        ("GiB", "G", 1024 * 1024 * 1024),
        ("TiB", "T", 1024 * 1024 * 1024 * 1024),
    ];
    let (unit, compact, divisor) = UNITS
        .iter()
        .rev()
        .find(|(_, _, divisor)| usage.total_bytes >= *divisor)
        .copied()
        .unwrap_or(UNITS[0]);
    let used = usage.used_bytes as f64 / divisor as f64;
    let total = usage.total_bytes as f64 / divisor as f64;
    [
        format!("{used:.1}/{total:.1} {unit}"),
        format!("{used:.0}/{total:.0} {unit}"),
        format!("{used:.0}/{total:.0}{compact}"),
    ]
    .into_iter()
    .find(|value| UnicodeWidthStr::width(value.as_str()) <= width)
    .unwrap_or_else(|| "n/a".into())
}

impl ClientShellState {
    pub(super) fn set_sidebar_health_from_row(&mut self, row: u16, outcome: &mut ClientShellInput) {
        if let Some(split) = split_for_row(self.hits.sidebar_divider, row) {
            self.sidebar_health_split = Some(split);
            outcome.repaint = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_section_reserves_six_rows_and_clamps_drag() {
        let area = Rect::new(0, 0, 26, 24);
        assert_eq!(primary_area(area, None), Rect::new(0, 0, 26, 18));
        assert_eq!(health_area(area, None), Rect::new(0, 18, 25, 6));
        assert_eq!(primary_area(area, split_for_row(area, 1)).height, 6);
        assert_eq!(primary_area(area, split_for_row(area, 24)).height, 21);
        assert!(health_area(Rect::new(0, 0, 26, 15), None).is_empty());
    }

    #[test]
    fn health_divider_preference_round_trips_and_old_preferences_default() {
        let preferences = super::super::preferences::ClientChromePreferences {
            sidebar_health_split: Some(0.7),
            ..Default::default()
        };
        let json = serde_json::to_string(&preferences).expect("preferences JSON");
        let restored: super::super::preferences::ClientChromePreferences =
            serde_json::from_str(&json).expect("restore");
        assert_eq!(restored.sidebar_health_split, Some(0.7));
        let legacy: super::super::preferences::ClientChromePreferences =
            serde_json::from_str("{}").expect("legacy preferences");
        assert_eq!(legacy.sidebar_health_split, None);
    }
}
