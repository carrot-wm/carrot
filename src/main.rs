mod socket;
mod client;


fn main() {
    let sock = socket::WaylandSocket::new().unwrap();
    println!("listenting on {}", sock.name);

    let client_fs = sock.accept().unwrap();
    println!("")

}

