use local_ip_address::local_ip;

pub fn get_local_ip() -> String {
    match local_ip() {
        Ok(ip) => ip.to_string(),
        Err(_) => "127.0.0.1".to_string(),
    }
}

pub fn get_public_url(port: u16) -> String {
    let ip = get_local_ip();
    // Wrap IPv6 in brackets if needed? local_ip usually returns IPv4 on typical LANs,
    // but good to be aware. For now, simple format
    format!("http://{}:{}", ip, port)
}
