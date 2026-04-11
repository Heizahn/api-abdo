#![allow(dead_code)]

use anyhow::{Context, Result};
use chrono::Local;
use regex::Regex;
use ssh2::Session;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::{Duration, Instant};
use tracing::info;

use crate::config::Config; // Usamos tracing en lugar de println!

// Estructura interna para manejar el Shell
struct PromptAwareShell {
    channel: ssh2::Channel,
    prompt_re: Regex,
    buf_size: usize,
    read_timeout: Duration,
}

impl PromptAwareShell {
    fn new(channel: ssh2::Channel, prompt_re: Regex) -> Self {
        Self {
            channel,
            prompt_re,
            buf_size: 8192,
            read_timeout: Duration::from_secs(6),
        }
    }

    fn send_line(&mut self, cmd: &str) -> std::io::Result<()> {
        self.channel.write_all(cmd.as_bytes())?;
        if !cmd.ends_with('\n') {
            self.channel.write_all(b"\n")?;
        }
        self.channel.flush()?;
        Ok(())
    }

    fn read_until_prompt(&mut self, timeout: Duration) -> std::io::Result<String> {
        let start = Instant::now();
        let mut out = String::new();
        let mut buf = vec![0u8; self.buf_size];

        while start.elapsed() < timeout {
            let n = self.channel.read(&mut buf)?;
            if n == 0 {
                continue;
            }
            out.push_str(&String::from_utf8_lossy(&buf[..n]));
            if self.prompt_re.is_match(&out) {
                break;
            }
        }
        Ok(out)
    }

    fn drain(&mut self) -> std::io::Result<String> {
        self.read_until_prompt(Duration::from_secs(2))
    }

    fn exec(&mut self, cmd: &str) -> std::io::Result<String> {
        self.send_line(cmd)?;
        self.read_until_prompt(self.read_timeout)
    }

    fn exec_with_timeout(&mut self, cmd: &str, timeout: Duration) -> std::io::Result<String> {
        self.send_line(cmd)?;
        self.read_until_prompt(timeout)
    }
}

fn formatear_mac(mac_zte: &str) -> String {
    let limpia = mac_zte.replace('.', "");
    let mut resultado = String::new();
    for (i, c) in limpia.chars().enumerate() {
        if i > 0 && i % 2 == 0 {
            resultado.push(':');
        }
        resultado.push(c.to_ascii_uppercase());
    }
    resultado
}

// ================================================================
// FUNCIÓN PRINCIPAL PÚBLICA (ASÍNCRONA)
// ================================================================
pub async fn procesar_olt_zte(file_name: String) -> Result<String> {
    // Usamos spawn_blocking para mover la carga pesada y sincrónica (SSH) fuera del hilo async
    let result = tokio::task::spawn_blocking(move || -> Result<String> {
        // 1. Configuración de Rutas
        let output_dir = "olt_reports";
        let dir_path = Path::new(output_dir);
        let file_path = dir_path.join(&file_name);

        // Crear directorio si no existe
        if !dir_path.exists() {
            fs::create_dir_all(dir_path).context("No se pudo crear el directorio de reportes")?;
        }

        // Eliminar archivo anterior si existe
        if file_path.exists() {
            let _ = fs::remove_file(&file_path);
        }

        // 2. Credenciales (Asumimos que las vars de entorno ya están cargadas en main)
        let olt_host = "10.1.5.22:22";
        let olt_user = "rust_api";
        // Forzamos error si no hay pass, usando anyhow context
        let olt_pass = Config::from_env().olt_zte_pass;

        info!("🔌 Conectando a {}...", olt_host);

        // 3. Conexión SSH
        let tcp = TcpStream::connect(&olt_host).context("Fallo conexión TCP")?;
        let mut sess = Session::new().context("Fallo crear sesión SSH")?;
        sess.set_tcp_stream(tcp);
        sess.handshake().context("Fallo handshake SSH")?;
        sess.userauth_password(&olt_user, &olt_pass)
            .context("Fallo autenticación")?;

        if !sess.authenticated() {
            return Err(anyhow::anyhow!("Autenticación SSH denegada"));
        }

        let mut channel = sess.channel_session()?;
        channel.request_pty("vt100", None, None)?;
        channel.shell()?;

        let prompt_re = Regex::new(r"(?m)(\([^)]+\))?[#>]\s*$")?;
        let mut sh = PromptAwareShell::new(channel, prompt_re);

        let _ = sh.drain();
        let _ = sh.exec_with_timeout("enable", Duration::from_secs(2));
        let _ = sh.exec_with_timeout("terminal length 0", Duration::from_secs(2));
        let _ = sh.exec_with_timeout("terminal pager 0", Duration::from_secs(2));

        // Obtener Hostname
        let out_h = sh
            .exec_with_timeout("show hostname", Duration::from_secs(3))
            .unwrap_or_default();
        let re_host = Regex::new(r"(?i)Hostname\s*:\s*([^\s\r\n]+)")?;
        let hostname = re_host
            .captures(&out_h)
            .map(|cap| cap[1].to_string())
            .unwrap_or_else(|| "OLT".to_string());

        info!("🚀 Procesando OLT: {}", hostname);

        // Regex
        let re_onu = Regex::new(r"(gpon_onu-\d+/\d+/\d+:\d+)\s+\S+\s+sn\s+SN:([A-Z0-9]+)\s+(\w+)")?;
        let re_mac = Regex::new(r"([0-9a-fA-F]{4}\.[0-9a-fA-F]{4}\.[0-9a-fA-F]{4})")?;

        // Abrir archivo
        let mut f_txt = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file_path)
            .context("No se pudo abrir el archivo de reporte")?;

        writeln!(
            f_txt,
            "Fecha: {}, Hostname: {}\n",
            Local::now().format("%Y-%m-%d %H:%M:%S"),
            hostname
        )?;
        writeln!(f_txt, "Fecha | PON | Interfaz | SN | Estado | MAC")?;
        writeln!(
            f_txt,
            "-------------------------------------------------------------"
        )?;

        let mut total_onus = 0;
        let mut total_ready = 0;

        // Bucle Principal
        for board in 1..=15 {
            writeln!(f_txt, "\n==== Tarjeta {} ====", board)?;
            for pon in 1..=16 {
                let pon_port = format!("gpon_olt-1/{}/{}", board, pon);
                // info!("📡 Consultando {}", pon_port); // Comentado para no saturar logs del API

                let out_baseinfo = sh.exec(&format!("show gpon onu baseinfo {}", pon_port))?;

                for cap in re_onu.captures_iter(&out_baseinfo) {
                    total_onus += 1;
                    let interfaz = &cap[1];
                    let serial = &cap[2];
                    let estado = &cap[3];

                    if estado.eq_ignore_ascii_case("ready") {
                        total_ready += 1;
                    }

                    let mac_final = if estado.eq_ignore_ascii_case("ready") {
                        let out_mac = sh.exec(&format!("show mac interface {}", interfaz))?;
                        re_mac
                            .captures_iter(&out_mac)
                            .next()
                            .map(|cap| formatear_mac(&cap[1]))
                            .unwrap_or_else(|| "NO_APRENDIDA".to_string())
                    } else {
                        "NO_APLICA".to_string()
                    };

                    let fecha_linea = Local::now().format("%Y-%m-%d %H:%M:%S");
                    writeln!(
                        f_txt,
                        "{} | {} | {} | {} | {} | {}",
                        fecha_linea, pon_port, interfaz, serial, estado, mac_final
                    )?;
                }
            }
        }

        // Resumen
        writeln!(
            f_txt,
            "\n-------------------------------------------------------------"
        )?;
        writeln!(
            f_txt,
            "Resumen: Total ONUs: {}, Activas: {}, Inactivas: {}",
            total_onus,
            total_ready,
            total_onus - total_ready
        )?;

        info!(
            "✅ Proceso OLT Finalizado. Reporte guardado en {:?}",
            file_path
        );

        // Retornamos la ruta como string
        Ok(file_path.to_string_lossy().into_owned())
    })
    .await??; // El primer ? es del JoinHandle (spawn), el segundo ? es del Result interno

    Ok(result)
}
