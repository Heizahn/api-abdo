use anyhow::{Context, Result};
use ssh2::Session;
use std::fs::File;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

fn normalize_mac(s: &str) -> String {
    s.trim()
        .to_uppercase()
        .replace('-', ":")
        .replace('.', ":")
        .replace(' ', "")
}

fn parse_kv_line(line: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let mut iter = line.split_whitespace().peekable();
    while let Some(token) = iter.next() {
        if let Some(eq_pos) = token.find('=') {
            let key = token[..eq_pos].to_string();
            let mut val = token[eq_pos + 1..].to_string();
            if val.starts_with('"') && !val.ends_with('"') {
                while let Some(next_tok) = iter.next() {
                    val.push(' ');
                    val.push_str(next_tok);
                    if next_tok.ends_with('"') {
                        break;
                    }
                }
            }
            let val = val.trim_matches('"').to_string();
            map.insert(key, val);
        }
    }
    map
}

pub fn fetch_mikrotik_leases_to_file(
    ip: &str,
    port: &str,
    user: &str,
    pass: &str,
    output_path: &str,
) -> Result<()> {
    let addr = format!("{}:{}", ip, port);
    let tcp = TcpStream::connect_timeout(
        &addr.parse().context("IP o Puerto inválido")?,
        Duration::from_secs(10),
    )
    .context(format!("No se pudo conectar a {}", addr))?;

    tcp.set_read_timeout(Some(Duration::from_secs(30)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(30)))?;

    let mut sess = Session::new()?;
    sess.set_tcp_stream(tcp);
    sess.handshake().context("Fallo handshake SSH")?;
    sess.userauth_password(user, pass)
        .context("Fallo autenticación SSH")?;

    if !sess.authenticated() {
        return Err(anyhow::anyhow!("Autenticación denegada en MikroTik"));
    }

    let mut channel = sess.channel_session()?;
    channel.exec("/ip dhcp-server lease print terse without-paging")?;

    let mut buffer = Vec::new();
    channel.read_to_end(&mut buffer)?;
    channel.wait_close()?;

    let output = String::from_utf8_lossy(&buffer);

    let mut file = File::create(output_path)
        .with_context(|| format!("No se pudo crear el archivo {}", output_path))?;

    writeln!(
        file,
        "{:<15} | {:<18} | {:<20}",
        "IP ADDRESS", "MAC ADDRESS", "HOST NAME"
    )?;
    writeln!(
        file,
        "------------------------------------------------------------"
    )?;

    let mut count = 0;

    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let kv = parse_kv_line(line);

        let ip = kv.get("address").cloned().unwrap_or_else(|| "---".into());
        let mac_raw = kv.get("mac-address").cloned().unwrap_or_default();
        let mac = normalize_mac(&mac_raw);
        let host = kv.get("host-name").cloned().unwrap_or_else(|| "---".into());

        if !mac.is_empty() {
            writeln!(file, "{:<15} | {:<18} | {}", ip, mac, host)?;
            count += 1;
        }
    }

    println!("✅ Guardados {} registros en {}", count, output_path);

    Ok(())
}
