use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager};

pub const PRODUCT_ID: &str = "inventory-manager";

const ACTIVATION_FILE: &str = "activation.json";
const DEBUG_LICENSE_PUBLIC_KEY: &str = "GX9rI-FshTLGq8g4-s1ep4m-DHaykgM0A5v6iz02jWE";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LicenseClaims {
    pub product: String,
    pub license_id: String,
    pub customer: String,
    pub machine_code: String,
    pub issued_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActivationStatus {
    pub activated: bool,
    pub machine_code: String,
    pub customer: Option<String>,
    pub license_id: Option<String>,
    pub expires_at: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredActivation {
    activation_code: String,
}

pub fn activation_status(app: &AppHandle) -> ActivationStatus {
    let machine_code = machine_code(app);

    match read_stored_activation(app) {
        Ok(Some(stored)) => match verify_license_for_app(app, &stored.activation_code) {
            Ok(claims) => status_from_claims(machine_code, claims),
            Err(err) => ActivationStatus {
                activated: false,
                machine_code,
                customer: None,
                license_id: None,
                expires_at: None,
                message: err,
            },
        },
        Ok(None) => ActivationStatus {
            activated: false,
            machine_code,
            customer: None,
            license_id: None,
            expires_at: None,
            message: "程序未激活".to_string(),
        },
        Err(err) => ActivationStatus {
            activated: false,
            machine_code,
            customer: None,
            license_id: None,
            expires_at: None,
            message: err,
        },
    }
}

pub fn activate_license(
    app: &AppHandle,
    activation_code: &str,
) -> Result<ActivationStatus, String> {
    let claims = verify_license_for_app(app, activation_code)?;
    let path = activation_file_path(app)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("无法创建激活目录: {err}"))?;
    }

    let stored = StoredActivation {
        activation_code: normalize_activation_code(activation_code),
    };
    let bytes = serde_json::to_vec_pretty(&stored).map_err(|err| err.to_string())?;
    fs::write(&path, bytes).map_err(|err| format!("无法保存激活信息: {err}"))?;

    Ok(status_from_claims(machine_code(app), claims))
}

pub fn deactivate_license(app: &AppHandle) -> Result<ActivationStatus, String> {
    let path = activation_file_path(app)?;
    if path.exists() {
        fs::remove_file(&path).map_err(|err| format!("无法清除激活信息: {err}"))?;
    }
    Ok(ActivationStatus {
        activated: false,
        machine_code: machine_code(app),
        customer: None,
        license_id: None,
        expires_at: None,
        message: "已取消激活".to_string(),
    })
}

pub fn require_activated(app: &AppHandle) -> Result<(), String> {
    let stored =
        read_stored_activation(app)?.ok_or_else(|| "程序未激活，请先输入激活码".to_string())?;
    verify_license_for_app(app, &stored.activation_code).map(|_| ())
}

pub fn machine_code(app: &AppHandle) -> String {
    let mut hasher = Sha256::new();
    hasher.update(PRODUCT_ID.as_bytes());
    hasher.update(b"\n");
    hasher.update(env::consts::OS.as_bytes());
    hasher.update(b"\n");
    hasher.update(env::consts::ARCH.as_bytes());
    hasher.update(b"\n");

    for key in ["COMPUTERNAME", "HOSTNAME", "USERNAME", "USER"] {
        if let Ok(value) = env::var(key) {
            hasher.update(key.as_bytes());
            hasher.update(b"=");
            hasher.update(value.as_bytes());
            hasher.update(b"\n");
        }
    }

    if let Ok(path) = app.path().app_data_dir() {
        hasher.update(path.to_string_lossy().as_bytes());
    }

    let digest = hasher.finalize();
    let hex: String = digest[..12]
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect();
    let groups: Vec<_> = hex
        .as_bytes()
        .chunks(4)
        .map(|chunk| std::str::from_utf8(chunk).unwrap())
        .collect();
    format!("INV-{}", groups.join("-"))
}

pub fn current_utc_date() -> String {
    let day = current_epoch_day();
    let (year, month, date) = civil_from_epoch_day(day);
    format!("{year:04}-{month:02}-{date:02}")
}

pub fn license_payload_part(claims: &LicenseClaims) -> Result<String, String> {
    let payload = serde_json::to_vec(claims).map_err(|err| err.to_string())?;
    Ok(URL_SAFE_NO_PAD.encode(payload))
}

fn status_from_claims(machine_code: String, claims: LicenseClaims) -> ActivationStatus {
    ActivationStatus {
        activated: true,
        machine_code,
        customer: Some(claims.customer),
        license_id: Some(claims.license_id),
        expires_at: claims.expires_at,
        message: "程序已激活".to_string(),
    }
}

fn verify_license_for_app(app: &AppHandle, activation_code: &str) -> Result<LicenseClaims, String> {
    verify_license_token(
        activation_code,
        license_public_key(),
        &machine_code(app),
        current_epoch_day(),
    )
}

fn verify_license_token(
    activation_code: &str,
    public_key: &str,
    current_machine_code: &str,
    current_day: i64,
) -> Result<LicenseClaims, String> {
    let token = normalize_activation_code(activation_code);
    let (payload_part, signature_part) = token
        .split_once('.')
        .ok_or_else(|| "激活码格式不正确".to_string())?;

    let public_key_bytes = URL_SAFE_NO_PAD
        .decode(public_key)
        .map_err(|_| "授权公钥配置错误".to_string())?;
    let public_key_bytes: [u8; 32] = public_key_bytes
        .try_into()
        .map_err(|_| "授权公钥长度不正确".to_string())?;
    let verifying_key =
        VerifyingKey::from_bytes(&public_key_bytes).map_err(|_| "授权公钥无效".to_string())?;

    let signature_bytes = URL_SAFE_NO_PAD
        .decode(signature_part)
        .map_err(|_| "激活码签名格式不正确".to_string())?;
    let signature =
        Signature::from_slice(&signature_bytes).map_err(|_| "激活码签名长度不正确".to_string())?;

    verifying_key
        .verify(payload_part.as_bytes(), &signature)
        .map_err(|_| "激活码签名无效".to_string())?;

    let payload = URL_SAFE_NO_PAD
        .decode(payload_part)
        .map_err(|_| "激活码内容格式不正确".to_string())?;
    let claims: LicenseClaims =
        serde_json::from_slice(&payload).map_err(|_| "激活码内容无法读取".to_string())?;

    if claims.product != PRODUCT_ID {
        return Err("激活码不适用于当前程序".to_string());
    }
    if normalize_machine_code(&claims.machine_code) != normalize_machine_code(current_machine_code)
    {
        return Err("激活码不属于本机".to_string());
    }
    if claims.customer.trim().is_empty() {
        return Err("激活码缺少客户信息".to_string());
    }
    if claims.license_id.trim().is_empty() {
        return Err("激活码缺少授权编号".to_string());
    }

    if let Some(expires_at) = claims
        .expires_at
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let expiry_day = parse_date_to_epoch_day(expires_at)?;
        if current_day > expiry_day {
            return Err(format!("激活码已于 {expires_at} 过期"));
        }
    }

    Ok(claims)
}

fn read_stored_activation(app: &AppHandle) -> Result<Option<StoredActivation>, String> {
    let path = activation_file_path(app)?;
    if !path.exists() {
        return Ok(None);
    }
    let content = fs::read(&path).map_err(|err| format!("无法读取激活信息: {err}"))?;
    serde_json::from_slice(&content)
        .map(Some)
        .map_err(|err| format!("激活信息损坏: {err}"))
}

fn activation_file_path(app: &AppHandle) -> Result<PathBuf, String> {
    let mut path = app
        .path()
        .app_data_dir()
        .map_err(|err| format!("无法获取应用数据目录: {err}"))?;
    path.push(ACTIVATION_FILE);
    Ok(path)
}

fn license_public_key() -> &'static str {
    option_env!("INVENTORY_LICENSE_PUBLIC_KEY").unwrap_or(DEBUG_LICENSE_PUBLIC_KEY)
}

fn normalize_activation_code(value: &str) -> String {
    value.chars().filter(|ch| !ch.is_whitespace()).collect()
}

fn normalize_machine_code(value: &str) -> String {
    value.trim().to_ascii_uppercase()
}

fn current_epoch_day() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
        / 86_400
}

fn parse_date_to_epoch_day(value: &str) -> Result<i64, String> {
    let mut parts = value.split('-');
    let year = parts
        .next()
        .and_then(|part| part.parse::<i32>().ok())
        .ok_or_else(|| "到期日期格式应为 YYYY-MM-DD".to_string())?;
    let month = parts
        .next()
        .and_then(|part| part.parse::<u32>().ok())
        .ok_or_else(|| "到期日期格式应为 YYYY-MM-DD".to_string())?;
    let day = parts
        .next()
        .and_then(|part| part.parse::<u32>().ok())
        .ok_or_else(|| "到期日期格式应为 YYYY-MM-DD".to_string())?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err("到期日期格式应为 YYYY-MM-DD".to_string());
    }
    Ok(days_from_civil(year, month, day))
}

fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let year = year - i32::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month = month as i32;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day as i32 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe - 719_468) as i64
}

fn civil_from_epoch_day(day: i64) -> (i32, u32, u32) {
    let day = day + 719_468;
    let era = if day >= 0 { day } else { day - 146_096 } / 146_097;
    let doe = day - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let date = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = year + i64::from(month <= 2);
    (year as i32, month as u32, date as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn signed_token(signing_key: &SigningKey, claims: &LicenseClaims) -> String {
        let payload_part = license_payload_part(claims).unwrap();
        let signature = signing_key.sign(payload_part.as_bytes());
        format!(
            "{}.{}",
            payload_part,
            URL_SAFE_NO_PAD.encode(signature.to_bytes())
        )
    }

    #[test]
    fn verifies_signed_license_for_matching_machine() {
        let signing_key = SigningKey::from_bytes(&[7_u8; 32]);
        let public_key = URL_SAFE_NO_PAD.encode(signing_key.verifying_key().to_bytes());
        let claims = LicenseClaims {
            product: PRODUCT_ID.to_string(),
            license_id: "LIC-001".to_string(),
            customer: "测试客户".to_string(),
            machine_code: "INV-ABCD-EF01".to_string(),
            issued_at: "2026-07-05".to_string(),
            expires_at: Some("2026-12-31".to_string()),
        };

        let token = signed_token(&signing_key, &claims);
        let verified = verify_license_token(
            &token,
            &public_key,
            "inv-abcd-ef01",
            days_from_civil(2026, 7, 5),
        )
        .unwrap();

        assert_eq!(verified, claims);
    }

    #[test]
    fn rejects_license_for_other_machine() {
        let signing_key = SigningKey::from_bytes(&[8_u8; 32]);
        let public_key = URL_SAFE_NO_PAD.encode(signing_key.verifying_key().to_bytes());
        let claims = LicenseClaims {
            product: PRODUCT_ID.to_string(),
            license_id: "LIC-002".to_string(),
            customer: "测试客户".to_string(),
            machine_code: "INV-AAAA-BBBB".to_string(),
            issued_at: "2026-07-05".to_string(),
            expires_at: None,
        };

        let token = signed_token(&signing_key, &claims);
        let err = verify_license_token(
            &token,
            &public_key,
            "INV-CCCC-DDDD",
            days_from_civil(2026, 7, 5),
        )
        .unwrap_err();

        assert_eq!(err, "激活码不属于本机");
    }

    #[test]
    fn rejects_expired_license() {
        let signing_key = SigningKey::from_bytes(&[9_u8; 32]);
        let public_key = URL_SAFE_NO_PAD.encode(signing_key.verifying_key().to_bytes());
        let claims = LicenseClaims {
            product: PRODUCT_ID.to_string(),
            license_id: "LIC-003".to_string(),
            customer: "测试客户".to_string(),
            machine_code: "INV-AAAA-BBBB".to_string(),
            issued_at: "2026-07-05".to_string(),
            expires_at: Some("2026-07-04".to_string()),
        };

        let token = signed_token(&signing_key, &claims);
        let err = verify_license_token(
            &token,
            &public_key,
            "INV-AAAA-BBBB",
            days_from_civil(2026, 7, 5),
        )
        .unwrap_err();

        assert_eq!(err, "激活码已于 2026-07-04 过期");
    }

    #[test]
    fn formats_current_date_from_epoch_day() {
        assert_eq!(
            civil_from_epoch_day(days_from_civil(2026, 7, 5)),
            (2026, 7, 5)
        );
    }
}
