use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::{Signer, SigningKey};
use inventory_manager_lib::activation::{
    current_utc_date, license_payload_part, LicenseClaims, PRODUCT_ID,
};
use rand_core::OsRng;
use std::env;
use std::process;

fn main() {
    let args: Vec<String> = env::args().collect();
    let result = match args.get(1).map(String::as_str) {
        Some("init-keypair") => init_keypair(),
        Some("public-key") => public_key(&args[2..]),
        Some("issue") => issue_license(&args[2..]),
        _ => {
            print_usage();
            Ok(())
        }
    };

    if let Err(err) = result {
        eprintln!("{err}");
        process::exit(1);
    }
}

fn init_keypair() -> Result<(), String> {
    let signing_key = SigningKey::generate(&mut OsRng);
    let private_key = URL_SAFE_NO_PAD.encode(signing_key.to_bytes());
    let public_key = URL_SAFE_NO_PAD.encode(signing_key.verifying_key().to_bytes());

    println!("private_key_seed={private_key}");
    println!("public_key={public_key}");
    println!();
    println!("将 public_key 作为 INVENTORY_LICENSE_PUBLIC_KEY 用于正式构建。private_key_seed 只保存在发码机器上。");
    Ok(())
}

fn public_key(args: &[String]) -> Result<(), String> {
    let signing_key = signing_key_from_arg(args)?;
    println!(
        "{}",
        URL_SAFE_NO_PAD.encode(signing_key.verifying_key().to_bytes())
    );
    Ok(())
}

fn issue_license(args: &[String]) -> Result<(), String> {
    let signing_key = signing_key_from_arg(args)?;
    let machine_code = required_arg(args, "--machine")?;
    let customer = required_arg(args, "--customer")?;
    let license_id = arg_value(args, "--license-id")
        .unwrap_or_else(|| format!("LIC-{}", current_utc_date().replace('-', "")));
    let expires_at = arg_value(args, "--expires");
    let issued_at = arg_value(args, "--issued-at").unwrap_or_else(current_utc_date);

    let claims = LicenseClaims {
        product: PRODUCT_ID.to_string(),
        license_id,
        customer,
        machine_code,
        issued_at,
        expires_at,
    };
    let payload_part = license_payload_part(&claims)?;
    let signature = signing_key.sign(payload_part.as_bytes());
    let activation_code = format!(
        "{}.{}",
        payload_part,
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    );

    println!("{activation_code}");
    Ok(())
}

fn signing_key_from_arg(args: &[String]) -> Result<SigningKey, String> {
    let private_key = required_arg(args, "--private-key")?;
    let private_key_bytes = URL_SAFE_NO_PAD
        .decode(private_key)
        .map_err(|_| "--private-key 不是有效的 base64url".to_string())?;
    let private_key_bytes: [u8; 32] = private_key_bytes
        .try_into()
        .map_err(|_| "--private-key 必须是 32 字节 Ed25519 seed".to_string())?;
    Ok(SigningKey::from_bytes(&private_key_bytes))
}

fn required_arg(args: &[String], name: &str) -> Result<String, String> {
    arg_value(args, name).ok_or_else(|| format!("缺少参数 {name}"))
}

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.windows(2)
        .find_map(|pair| (pair[0] == name).then(|| pair[1].clone()))
}

fn print_usage() {
    eprintln!(
        "用法:
  cargo run --example license_keygen -- init-keypair
  cargo run --example license_keygen -- public-key --private-key <seed>
  cargo run --example license_keygen -- issue --private-key <seed> --machine <machine-code> --customer <name> [--expires YYYY-MM-DD] [--license-id LIC-001]"
    );
}
