use serde::{Deserialize, Serialize};
use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WarrantyInput {
    pub duration_days: u32,
    pub label_snapshot: String,
    #[serde(default)]
    pub starts_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WarrantyTerms {
    pub duration_days: u32,
    pub label_snapshot: String,
    pub starts_at: String,
    pub expires_at: String,
}

pub fn resolve_warranty(
    input: Option<WarrantyInput>,
    default_starts_at: &str,
) -> Result<Option<WarrantyTerms>, String> {
    let Some(input) = input else {
        return Ok(None);
    };
    if input.duration_days == 0 || input.duration_days > 36_500 {
        return Err("质保期限必须在 1 到 36500 天之间".to_owned());
    }
    let label = input.label_snapshot.trim();
    if label.is_empty() || label.chars().count() > 40 {
        return Err("质保标签不能为空且不能超过 40 个字符".to_owned());
    }
    let starts_at = input
        .starts_at
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(default_starts_at);
    let parsed = OffsetDateTime::parse(starts_at, &Rfc3339)
        .map_err(|_| "质保起算时间必须是有效的 RFC3339 时间".to_owned())?;
    let expires = parsed
        .checked_add(Duration::days(i64::from(input.duration_days)))
        .ok_or_else(|| "质保到期时间超出支持范围".to_owned())?;
    let expires_at = expires
        .format(&Rfc3339)
        .map_err(|error| format!("无法格式化质保到期时间: {error}"))?;
    Ok(Some(WarrantyTerms {
        duration_days: input.duration_days,
        label_snapshot: label.to_owned(),
        starts_at: starts_at.to_owned(),
        expires_at,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_default_and_manual_start_dates() {
        let default = resolve_warranty(
            Some(WarrantyInput {
                duration_days: 30,
                label_snapshot: "一个月".to_owned(),
                starts_at: None,
            }),
            "2026-08-14T01:00:00Z",
        )
        .expect("default warranty")
        .expect("terms");
        assert_eq!(default.starts_at, "2026-08-14T01:00:00Z");
        assert_eq!(default.expires_at, "2026-09-13T01:00:00Z");

        let manual = resolve_warranty(
            Some(WarrantyInput {
                duration_days: 7,
                label_snapshot: "一个星期".to_owned(),
                starts_at: Some("2026-08-20T02:30:00Z".to_owned()),
            }),
            "2026-08-14T01:00:00Z",
        )
        .expect("manual warranty")
        .expect("terms");
        assert_eq!(manual.expires_at, "2026-08-27T02:30:00Z");
    }

    #[test]
    fn rejects_empty_or_unbounded_terms() {
        assert!(resolve_warranty(
            Some(WarrantyInput {
                duration_days: 0,
                label_snapshot: "无效".to_owned(),
                starts_at: None,
            }),
            "2026-08-14T01:00:00Z"
        )
        .is_err());
    }
}
