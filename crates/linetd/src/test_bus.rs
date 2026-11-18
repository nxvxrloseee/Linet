use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

pub struct Bus { child: Child, address: String }
impl Bus {
    pub fn new() -> Self {
        let mut child = Command::new("dbus-daemon").args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
        let mut address = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut address).unwrap();
        Self { child, address: address.trim().into() }
    }
    pub async fn connect(&self) -> zbus::Connection {
        zbus::connection::Builder::address(self.address.as_str()).unwrap().build().await.unwrap()
    }
}
impl Drop for Bus {
    fn drop(&mut self) { let _ = self.child.kill(); let _ = self.child.wait(); }
}

pub async fn eventually(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !condition() { tokio::time::sleep(std::time::Duration::from_millis(5)).await; }
    }).await.expect("D-Bus state did not settle");
}
