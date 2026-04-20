use std::io::Read;
use std::net::TcpStream;
use ssh2::Session;
use tracing::{debug, error, warn};

pub fn get_ip_pppoe_mk(
    sn: &str,
    ip: &str,
    port: &str,
    user: &str,
    pass: &str,
) -> Result<String, String> {
    if sn.len() < 8 {
        let msg = format!("SN '{}' demasiado corto (mínimo 8 caracteres), búsqueda omitida", sn);
        warn!(target: "api_abdo::mikrotik", "{}", msg);
        return Err(msg);
    }

    debug!("Intentando localizar SN: {} en BRAS {}:{}", sn, ip, port);

    let tcp = TcpStream::connect(format!("{}:{}", ip, port)).map_err(|e| {
        let msg = format!("Fallo de conexión TCP al router {}: {}", ip, e);
        error!(target: "api_abdo::mikrotik", "{}", msg);
        msg
    })?;

    let mut sess = Session::new().map_err(|e| {
        let msg = format!("No se pudo inicializar la estructura de sesión SSH2: {}", e);
        error!(target: "api_abdo::mikrotik", "{}", msg);
        msg
    })?;

    sess.set_tcp_stream(tcp);
    sess.handshake().map_err(|e| {
        let msg = format!("Fallo en handshake SSH con el router {}: {}", ip, e);
        error!(target: "api_abdo::mikrotik", "{}", msg);
        msg
    })?;

    sess.userauth_password(user, pass).map_err(|e| {
        let msg = format!("Error interno de autenticación SSH en {}: {}", ip, e);
        error!(target: "api_abdo::mikrotik", "{}", msg);
        msg
    })?;

    if !sess.authenticated() {
        let msg = format!("Credenciales SSH rechazadas por el router {}", ip);
        warn!(target: "api_abdo::mikrotik", "{}", msg);
        return Err("Autenticación fallida".to_string());
    }

    let regex_sn: String = sn
        .chars()
        .map(|c| {
            if c.is_ascii_alphabetic() {
                format!("[{}{}]", c.to_ascii_lowercase(), c.to_ascii_uppercase())
            } else {
                c.to_string()
            }
        })
        .collect();

    let command = format!(
        "{{ :do {{ :put [/ppp active get [:pick [find name~\"{}\"] 0] address] }} on-error={{ :put \"NOT_FOUND\" }} }}",
        regex_sn
    );

    let mut channel = sess.channel_session().map_err(|e| {
        let msg = format!("No se pudo abrir el canal SSH en {}: {}", ip, e);
        error!(target: "api_abdo::mikrotik", "{}", msg);
        msg
    })?;

    channel.exec(&command).map_err(|e| {
        let msg = format!("Fallo al inyectar comando en {}: {}", ip, e);
        error!(target: "api_abdo::mikrotik", "{}", msg);
        msg
    })?;

    let mut output = String::new();
    channel.read_to_string(&mut output).map_err(|e| {
        let msg = format!("Error leyendo el buffer SSH de {}: {}", ip, e);
        error!(target: "api_abdo::mikrotik", "{}", msg);
        msg
    })?;
    channel.wait_close().ok();

    let ip_result = output.trim();

    if ip_result.is_empty() || ip_result == "NOT_FOUND" {
        debug!(target: "api_abdo::mikrotik", "El SN {} no está en el BRAS {}", sn, ip);
        return Err(format!("El SN {} no tiene una sesión activa", sn));
    }

    if ip_result.contains('.') {
        debug!(target: "api_abdo::mikrotik", "SN {} localizado exitosamente con IP {}", sn, ip_result);
        Ok(ip_result.to_string())
    } else {
        let msg = format!("El router {} devolvió datos corruptos o inesperados: {}", ip, ip_result);
        error!(target: "api_abdo::mikrotik", "{}", msg);
        Err(msg)
    }
}
