//! D-Bus-клиент демона linetd: общий для linetctl и linet-nmh.

#[zbus::proxy(
    interface = "org.linet.Daemon1",
    default_service = "org.linet.Daemon",
    default_path = "/org/linet/Daemon"
)]
pub trait Daemon {
    fn devices(&self) -> zbus::Result<Vec<(String, String, String, bool)>>;
    fn info(&self) -> zbus::Result<(String, String)>;
    fn start_pairing(&self) -> zbus::Result<String>;
    fn cancel_pairing(&self) -> zbus::Result<()>;
    fn ping(&self, id: &str) -> zbus::Result<f64>;
    fn unpair(&self, id: &str) -> zbus::Result<()>;
    fn send_file(&self, id: &str, path: &str) -> zbus::Result<u64>;
    fn send_url(&self, id: &str, url: &str, title: &str) -> zbus::Result<()>;
    fn request_unlock(&self) -> zbus::Result<bool>;

    #[zbus(signal)]
    fn transfer_progress(&self, id: u64, peer: String, name: String, incoming: bool, done: u64, total: u64)
    -> zbus::Result<()>;

    #[zbus(signal)]
    fn transfer_finished(&self, id: u64, peer: String, name: String, incoming: bool, ok: bool, detail: String)
    -> zbus::Result<()>;
}
