use std::io::Read;
use std::net::TcpStream;
use ssh2::Session;

pub fn get_ip_pppoe_mk(sn: &str,     ip: &str,
                       port: &str,
                       user: &str,
                       pass: &str,) -> Result<String, String> {
    // 1. Configurar la conexión (Asegúrate de usar variables de entorno para esto)
    let tcp = TcpStream::connect(ip.to_string() + ":" + port)
        .map_err(|e| format!("Error de conexión: {}", e))?;

    let mut sess = Session::new().unwrap();
    sess.set_tcp_stream(tcp);
    sess.handshake().map_err(|e| format!("Error de handshake: {}", e))?;

    // Autenticación
    sess.userauth_password(user, pass)
        .map_err(|e| format!("Error de autenticación: {}", e))?;

    if !sess.authenticated() {
        return Err("Autenticación fallida".to_string());
    }

    // 2. Preparar el comando que probamos en la terminal
    // Usamos comillas para el SN y manejamos el error con :on-error para que no rompa el script
    let interface_name = format!("<pppoe-{}>", sn);
    let command = format!(
        ":do {{ :put ([/interface pppoe-server monitor \"{}\" once as-value]->\"remote-address\") }} on-error={{ :put \"NOT_FOUND\" }}",
        interface_name
    );

    // 3. Ejecutar el comando
    let mut channel = sess.channel_session().map_err(|e| e.to_string())?;
    channel.exec(&command).map_err(|e| e.to_string())?;

    let mut output = String::new();
    channel.read_to_string(&mut output).map_err(|e| e.to_string())?;
    channel.wait_close().ok();

    // 4. Limpiar y validar la respuesta
    let ip = output.trim();

    if ip.is_empty() || ip == "NOT_FOUND" {
        return Err(format!("El SN {} no tiene una sesión activa en el BRAS", sn));
    }

    // Validar que lo recibido parezca una IP (opcional pero recomendado)
    if ip.contains('.') {
        Ok(ip.to_string())
    } else {
        Err(format!("Respuesta inesperada del BRAS: {}", ip))
    }
}