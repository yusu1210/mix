use base64::{engine::general_purpose::STANDARD, Engine as _};
use minisign_verify::{PublicKey, Signature};
use std::{env, fs, process};

fn decode_text(label: &str, value: &str) -> Result<String, String> {
    let decoded = STANDARD
        .decode(value.trim())
        .map_err(|error| format!("invalid base64 {label}: {error}"))?;
    String::from_utf8(decoded).map_err(|error| format!("invalid UTF-8 {label}: {error}"))
}

fn run() -> Result<(), String> {
    let mut arguments = env::args().skip(1);
    let public_key_value = arguments.next().ok_or_else(|| {
        "usage: verify_update_signature <public-key> <signature> <archive>".to_string()
    })?;
    let signature_path = arguments
        .next()
        .ok_or_else(|| "signature path is required".to_string())?;
    let archive_path = arguments
        .next()
        .ok_or_else(|| "archive path is required".to_string())?;
    if arguments.next().is_some() {
        return Err("unexpected extra arguments".to_string());
    }

    // Tauri stores both the public key file and the minisign signature file as
    // base64 strings. Decode them exactly as tauri-plugin-updater does before
    // asking minisign-verify to authenticate the complete archive bytes.
    let public_key_text = decode_text("public key", &public_key_value)?;
    let signature_value = fs::read_to_string(&signature_path)
        .map_err(|error| format!("cannot read signature: {error}"))?;
    let signature_text = decode_text("signature", &signature_value)?;
    let public_key = PublicKey::decode(&public_key_text)
        .map_err(|error| format!("invalid updater public key: {error}"))?;
    let signature = Signature::decode(&signature_text)
        .map_err(|error| format!("invalid updater signature: {error}"))?;
    let archive =
        fs::read(&archive_path).map_err(|error| format!("cannot read updater archive: {error}"))?;
    public_key
        .verify(&archive, &signature, true)
        .map_err(|error| format!("updater signature verification failed: {error}"))?;
    println!("verified updater signature: {archive_path}");
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        process::exit(2);
    }
}
