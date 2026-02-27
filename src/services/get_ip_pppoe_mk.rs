use std::io::Read;
use std::net::TcpStream;
use ssh2::Session;

pub fn get_ip_pppoe_mk(
    sn: &str,
    ip: &str,
    port: &str,
    user: &str,
    pass: &str,
) -> Result<String, String> {
    println!("--> [DEBUG] Iniciando conexión a {}:{} con usuario '{}'", ip, port, user);

    // 1. Configurar la conexión SSH
    let tcp = TcpStream::connect(format!("{}:{}", ip, port)).map_err(|e| {
        println!("--> [ERROR] Falló conexión TCP a {}: {}", ip, e);
        format!("Error de conexión TCP: {}", e)
    })?;
    println!("--> [DEBUG] TCP conectado correctamente a {}", ip);

    let mut sess = Session::new().unwrap();
    sess.set_tcp_stream(tcp);
    sess.handshake().map_err(|e| {
        println!("--> [ERROR] Falló handshake SSH: {}", e);
        format!("Error de handshake SSH: {}", e)
    })?;
    println!("--> [DEBUG] Handshake SSH exitoso");

    // Autenticación
    sess.userauth_password(user, pass).map_err(|e| {
        println!("--> [ERROR] Error en autenticación con {}: {}", user, e);
        format!("Error de autenticación: {}", e)
    })?;

    if !sess.authenticated() {
        println!("--> [ERROR] Autenticación fallida. Credenciales incorrectas.");
        return Err("Autenticación fallida".to_string());
    }
    println!("--> [DEBUG] Sesión autenticada correctamente");

    // 2. Construir regex para que sea case-insensitive
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

    // 3. Comando protegido. Nota las llaves externas {{ }} para agrupar el script en SSH
    let command = format!(
        "{{ :do {{ :put [/ppp active get [:pick [find name~\"{}\"] 0] address] }} on-error={{ :put \"NOT_FOUND\" }} }}",
        regex_sn
    );
    println!("--> [DEBUG] Comando a enviar: {}", command);

    // 4. Ejecutar el comando
    let mut channel = sess.channel_session().map_err(|e| {
        println!("--> [ERROR] Falló al abrir el canal de sesión SSH: {}", e);
        e.to_string()
    })?;

    channel.exec(&command).map_err(|e| {
        println!("--> [ERROR] Falló al ejecutar comando en MikroTik: {}", e);
        e.to_string()
    })?;

    let mut output = String::new();
    channel.read_to_string(&mut output).map_err(|e| {
        println!("--> [ERROR] Falló al leer respuesta: {}", e);
        e.to_string()
    })?;
    channel.wait_close().ok();

    // AQUÍ ESTÁ LA MAGIA DEL DEBUG: Veremos qué devuelve exactamente MikroTik
    println!("--> [DEBUG] Respuesta cruda (RAW) del MikroTik: {:?}", output);

    // 5. Limpiar y validar la respuesta
    let ip_result = output.trim();
    println!("--> [DEBUG] Respuesta limpia (TRIM): '{}'", ip_result);

    if ip_result.is_empty() || ip_result == "NOT_FOUND" {
        println!("--> [ERROR] Respuesta indica que no existe el cliente.");
        return Err(format!("El SN {} no tiene una sesión activa", sn));
    }

    // Validar que lo recibido tenga formato de IP
    if ip_result.contains('.') {
        println!("--> [DEBUG] ÉXITO. IP extraída: {}", ip_result);
        Ok(ip_result.to_string())
    } else {
        println!("--> [ERROR] La respuesta no parece una IP válida.");
        Err(format!("Respuesta inesperada del BRAS: {}", ip_result))
    }
}