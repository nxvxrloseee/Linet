//! Простой ввода через Wayland `ext-idle-notify-v1` (Hyprland поддерживает).
//! Протокол учитывает блокировки простоя: пока идёт видео, «простоя» нет.

use std::time::Duration;

use anyhow::Result;
use tokio::sync::watch;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notification_v1::{self, ExtIdleNotificationV1};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::ExtIdleNotifierV1;

struct State {
    idle: watch::Sender<bool>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(_: &mut Self, _: &wl_seat::WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ExtIdleNotifierV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ExtIdleNotifierV1,
        _: <ExtIdleNotifierV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtIdleNotificationV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_idle_notification_v1::Event::Idled => drop(state.idle.send_replace(true)),
            ext_idle_notification_v1::Event::Resumed => drop(state.idle.send_replace(false)),
            _ => {}
        }
    }
}

/// `true`, когда пользователь не трогал ввод дольше `after`.
/// Нет Wayland или протокола — всегда `false` (автоблокировка просто не сработает).
pub fn watch(after: Duration) -> watch::Receiver<bool> {
    let (tx, rx) = watch::channel(false);
    std::thread::Builder::new()
        .name("linet-idle".into())
        .spawn(move || {
            if let Err(e) = run(tx, after) {
                tracing::warn!("слежение за простоем недоступно: {e:#}");
            }
        })
        .expect("поток для Wayland");
    rx
}

fn run(idle: watch::Sender<bool>, after: Duration) -> Result<()> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=1, ())?;
    let notifier: ExtIdleNotifierV1 = globals.bind(&qh, 1..=1, ())?;
    let timeout = u32::try_from(after.as_millis()).unwrap_or(u32::MAX);
    let _notification = notifier.get_idle_notification(timeout, &seat, &qh, ());
    let mut state = State { idle };
    loop {
        queue.blocking_dispatch(&mut state)?;
    }
}
