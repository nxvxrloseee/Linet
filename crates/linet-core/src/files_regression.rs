use super::*;
use crate::test_support::TestLink;
use std::io::Cursor;
use tokio::sync::mpsc;

fn digest(bytes: &[u8]) -> String {
    hex::encode(ring::digest::digest(&ring::digest::SHA256, bytes))
}

fn transfers(dir: &Path) -> Arc<Transfers> {
    Transfers::new(dir.into(), DEFAULT_MAX_SIZE, Arc::new(|_| {}))
}

fn observed(dir: &Path) -> (Arc<Transfers>, mpsc::UnboundedReceiver<TransferUpdate>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (
        Transfers::new(
            dir.into(),
            DEFAULT_MAX_SIZE,
            Arc::new(move |u| {
                let _ = tx.send(u);
            }),
        ),
        rx,
    )
}

fn offer(transfers: &Transfers, session: &Session, id: u64, name: &str, content: &[u8]) -> Message {
    transfers.accept_offer(
        id,
        name.into(),
        content.len() as u64,
        None,
        Some(digest(content)),
        session,
    )
}

async fn receive_stream(events: &mut mpsc::Receiver<crate::Inbound>) -> RecvStream {
    tokio::time::timeout(Duration::from_secs(5), async {
        match events.recv().await.unwrap() {
            crate::Inbound::Stream(stream) => stream,
            event => panic!("ожидался поток, получено {event:?}"),
        }
    })
    .await
    .unwrap()
}

async fn send(link: &TestLink, outgoing: &Transfers, content: Vec<u8>) -> Result<()> {
    let size = content.len() as u64;
    tokio::time::timeout(
        Duration::from_secs(5),
        outgoing.send(
            &link.sender,
            1,
            Cursor::new(content),
            "file.bin",
            size,
            "application/octet-stream",
            None,
        ),
    )
    .await
    .expect("передача зависла")
}

#[tokio::test]
async fn expired_full_queue_accepts_new_offers() {
    let dir = tempfile::tempdir().unwrap();
    let link = TestLink::new().await;
    let incoming = transfers(dir.path());
    for id in 0..MAX_PEER_TRANSFERS as u64 {
        assert!(matches!(
            offer(&incoming, &link.receiver, id, &format!("f{id}"), b"data"),
            Message::FileAccept { .. }
        ));
    }
    assert!(matches!(
        offer(&incoming, &link.receiver, 100, "new", b"data"),
        Message::FileReject { .. }
    ));
    for offer in incoming.pending.lock().unwrap().values_mut() {
        offer.accepted = Instant::now() - OFFER_TTL - Duration::from_secs(1);
    }
    assert!(matches!(
        offer(&incoming, &link.receiver, 100, "new", b"data"),
        Message::FileAccept { .. }
    ));
    assert_eq!(incoming.pending.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn active_transfer_keeps_its_path_and_capacity_reserved() {
    let dir = tempfile::tempdir().unwrap();
    let mut link = TestLink::new().await;
    let (incoming, mut updates) = observed(dir.path());
    assert!(matches!(
        offer(&incoming, &link.receiver, 1, "file.bin", b"data"),
        Message::FileAccept { .. }
    ));
    let mut stream = link.sender.connection().open_uni().await.unwrap();
    stream.write_all(&1u64.to_be_bytes()).await.unwrap();
    stream.write_all(b"d").await.unwrap();
    let recv = receive_stream(link.receiver_events.as_mut().unwrap()).await;
    let worker = {
        let incoming = incoming.clone();
        let session = link.receiver.clone();
        tokio::spawn(async move { incoming.receive(&session, recv).await })
    };
    tokio::time::timeout(Duration::from_secs(5), updates.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        offer(&incoming, &link.receiver, 2, "file.bin", b"data"),
        Message::FileReject { .. }
    ));
    // Даже после TTL активный поток нельзя удалить из учёта.
    incoming
        .pending
        .lock()
        .unwrap()
        .values_mut()
        .next()
        .unwrap()
        .accepted = Instant::now() - OFFER_TTL;
    for id in 2..=MAX_PEER_TRANSFERS as u64 {
        assert!(matches!(
            offer(&incoming, &link.receiver, id, &format!("f{id}"), b"data"),
            Message::FileAccept { .. }
        ));
    }
    assert!(matches!(
        offer(&incoming, &link.receiver, 100, "new", b"data"),
        Message::FileReject { .. }
    ));
    stream.reset(1u32.into()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), worker)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        !incoming
            .pending
            .lock()
            .unwrap()
            .contains_key(&(link.receiver.connection().stable_id(), 1))
    );
}

#[tokio::test]
async fn stream_cannot_claim_another_connections_offer() {
    let dir = tempfile::tempdir().unwrap();
    let first = TestLink::new().await;
    let mut second = TestLink::new().await;
    let incoming = transfers(dir.path());
    assert!(matches!(
        offer(&incoming, &first.receiver, 1, "file.bin", b"data"),
        Message::FileAccept { .. }
    ));
    let mut stream = second.sender.connection().open_uni().await.unwrap();
    stream.write_all(&1u64.to_be_bytes()).await.unwrap();
    stream.finish().unwrap();
    let recv = receive_stream(second.receiver_events.as_mut().unwrap()).await;
    assert!(incoming.receive(&second.receiver, recv).await.is_err());
    assert!(
        incoming
            .pending
            .lock()
            .unwrap()
            .contains_key(&(first.receiver.connection().stable_id(), 1))
    );
    assert!(matches!(
        offer(&incoming, &second.receiver, 1, "file.bin", b"data"),
        Message::FileAccept { .. }
    ));
    assert_eq!(incoming.pending.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn verified_resume_preserves_bytes_and_confirms_persistence() {
    let dir = tempfile::tempdir().unwrap();
    let mut link = TestLink::new().await;
    let content = vec![42; CHUNK * 3 + 17];
    let prefix = CHUNK + 3;
    let (incoming, mut updates) = observed(dir.path());
    let outgoing = transfers(&dir.path().join("out"));
    let part = dir.path().join(part_name(
        &link.receiver.peer(),
        "file.bin",
        content.len() as u64,
        Some(&digest(&content)),
    ));
    std::fs::write(&part, &content[..prefix]).unwrap();
    link.pump(outgoing.clone(), incoming.clone());
    send(&link, &outgoing, content.clone()).await.unwrap();
    assert_eq!(std::fs::read(dir.path().join("file.bin")).unwrap(), content);
    assert_eq!(updates.recv().await.unwrap().done, prefix as u64);
    assert!(!part.exists());
    assert!(incoming.pending.lock().unwrap().is_empty());
    assert!(outgoing.results.lock().unwrap().is_empty());
}

#[tokio::test]
async fn changed_content_with_same_name_and_size_starts_from_zero() {
    let dir = tempfile::tempdir().unwrap();
    let mut link = TestLink::new().await;
    let (incoming, mut updates) = observed(dir.path());
    let outgoing = transfers(&dir.path().join("out"));
    let old = b"old content";
    let new = b"new content";
    let part = dir.path().join(part_name(
        &link.receiver.peer(),
        "file.bin",
        old.len() as u64,
        Some(&digest(old)),
    ));
    std::fs::write(&part, &old[..4]).unwrap();
    link.pump(outgoing.clone(), incoming);
    send(&link, &outgoing, new.to_vec()).await.unwrap();
    assert_eq!(updates.recv().await.unwrap().done, 0);
    assert_eq!(std::fs::read(dir.path().join("file.bin")).unwrap(), new);
}

#[tokio::test]
async fn corrupted_prefix_is_rejected_and_sender_does_not_report_success() {
    let dir = tempfile::tempdir().unwrap();
    let mut link = TestLink::new().await;
    let (incoming, mut updates) = observed(dir.path());
    let (outgoing, mut sent) = observed(&dir.path().join("out"));
    let content = b"correct content";
    let part = dir.path().join(part_name(
        &link.receiver.peer(),
        "file.bin",
        content.len() as u64,
        Some(&digest(content)),
    ));
    std::fs::write(&part, b"wrong").unwrap();
    link.pump(outgoing.clone(), incoming);
    assert!(send(&link, &outgoing, content.to_vec()).await.is_err());
    assert!(!dir.path().join("file.bin").exists());
    assert!(!part.exists());
    let mut failed = false;
    while let Ok(update) = updates.try_recv() {
        failed |= matches!(update.state, TransferState::Failed(error) if error.contains("SHA-256"));
    }
    assert!(failed);
    while let Ok(update) = sent.try_recv() {
        assert!(!matches!(update.state, TransferState::Done { .. }));
    }
}

#[tokio::test]
async fn inbox_creation_failure_is_reported_and_releases_reservation() {
    let dir = tempfile::tempdir().unwrap();
    let inbox = dir.path().join("not-a-directory");
    std::fs::write(&inbox, b"occupied").unwrap();
    let mut link = TestLink::new().await;
    let (incoming, mut updates) = observed(&inbox);
    let outgoing = transfers(&dir.path().join("out"));
    link.pump(outgoing.clone(), incoming.clone());
    assert!(send(&link, &outgoing, b"content".to_vec()).await.is_err());
    let mut failed = false;
    while let Ok(update) = updates.try_recv() {
        failed |= matches!(update.state, TransferState::Failed(_));
    }
    assert!(failed);
    assert!(incoming.pending.lock().unwrap().is_empty());
    assert!(outgoing.results.lock().unwrap().is_empty());
}

#[tokio::test]
async fn legacy_sender_does_not_resume_unverified_partial_file() {
    let dir = tempfile::tempdir().unwrap();
    let link = TestLink::new().await;
    let incoming = transfers(dir.path());
    let part = dir
        .path()
        .join(part_name(&link.receiver.peer(), "file.bin", 10, None));
    std::fs::write(&part, b"old").unwrap();
    let reply = incoming.accept_offer(1, "file.bin".into(), 10, None, None, &link.receiver);
    assert!(matches!(
        reply,
        Message::FileAccept {
            offset: 0,
            sha256: None,
            ..
        }
    ));
    assert!(!part.exists());
}

#[tokio::test]
async fn unverified_offset_from_legacy_receiver_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut link = TestLink::new().await;
    let outgoing = transfers(dir.path());
    let mut events = link.receiver_events.take().unwrap();
    let receiver = link.receiver.clone();
    let old_receiver = tokio::spawn(async move {
        let Some(crate::Inbound::Message(env)) = events.recv().await else {
            panic!("нет предложения")
        };
        receiver
            .reply(
                env.id,
                Message::FileAccept {
                    id: 1,
                    offset: 2,
                    sha256: None,
                },
            )
            .await
            .unwrap();
    });
    let error = send(&link, &outgoing, b"content".to_vec())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("безопасную докачку"));
    old_receiver.await.unwrap();
    assert!(outgoing.results.lock().unwrap().is_empty());
}

#[tokio::test]
async fn empty_file_is_verified_and_persisted() {
    let dir = tempfile::tempdir().unwrap();
    let mut link = TestLink::new().await;
    let incoming = transfers(dir.path());
    let outgoing = transfers(&dir.path().join("out"));
    link.pump(outgoing.clone(), incoming);
    send(&link, &outgoing, vec![]).await.unwrap();
    assert_eq!(
        std::fs::metadata(dir.path().join("file.bin"))
            .unwrap()
            .len(),
        0
    );
}

#[tokio::test]
async fn legacy_receiver_can_still_accept_a_complete_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut link = TestLink::new().await;
    let outgoing = transfers(dir.path());
    let mut events = link.receiver_events.take().unwrap();
    let receiver = link.receiver.clone();
    let old_receiver = tokio::spawn(async move {
        let Some(crate::Inbound::Message(env)) = events.recv().await else {
            panic!("нет предложения")
        };
        receiver
            .reply(
                env.id,
                Message::FileAccept {
                    id: 1,
                    offset: 0,
                    sha256: None,
                },
            )
            .await
            .unwrap();
        let mut stream = receive_stream(&mut events).await;
        let bytes = stream.read_to_end(1024).await.unwrap();
        assert_eq!(&bytes[8..], b"content");
        // Старый получатель не знает FileResult.
    });
    send(&link, &outgoing, b"content".to_vec()).await.unwrap();
    old_receiver.await.unwrap();
}

#[tokio::test]
async fn disconnect_before_file_result_does_not_wait_for_result_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let mut link = TestLink::new().await;
    let outgoing = transfers(dir.path());
    let mut events = link.receiver_events.take().unwrap();
    let receiver = link.receiver.clone();
    let disconnect = tokio::spawn(async move {
        let Some(crate::Inbound::Message(env)) = events.recv().await else {
            panic!("нет предложения")
        };
        let Message::FileOffer { sha256, .. } = env.body else {
            panic!("не FileOffer")
        };
        receiver
            .reply(
                env.id,
                Message::FileAccept {
                    id: 1,
                    offset: 0,
                    sha256,
                },
            )
            .await
            .unwrap();
        let mut stream = receive_stream(&mut events).await;
        stream.read_to_end(1024).await.unwrap();
        receiver.close("disconnect before FileResult");
    });
    assert!(send(&link, &outgoing, b"content".to_vec()).await.is_err());
    disconnect.await.unwrap();
    assert!(outgoing.results.lock().unwrap().is_empty());
}

#[tokio::test]
async fn source_changed_after_hashing_is_not_published() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.bin");
    std::fs::write(&source, b"old content").unwrap();
    let inbox = dir.path().join("inbox");
    let incoming = transfers(&inbox);
    let outgoing = transfers(&dir.path().join("out"));
    let mut link = TestLink::new().await;
    let sender = link.sender.clone();
    let receiver = link.receiver.clone();
    let mut sender_events = link.sender_events.take().unwrap();
    let mut receiver_events = link.receiver_events.take().unwrap();
    let send_pump = {
        let outgoing = outgoing.clone();
        tokio::spawn(async move {
            while let Some(event) = sender_events.recv().await {
                outgoing.handle(&sender, event);
            }
        })
    };
    let receive_pump = {
        let source = source.clone();
        tokio::spawn(async move {
            while let Some(event) = receiver_events.recv().await {
                if matches!(
                    &event,
                    crate::Inbound::Message(Envelope {
                        body: Message::FileOffer { .. },
                        ..
                    })
                ) {
                    // FileOffer приходит после хеширования; меняем тот же открытый файл.
                    std::fs::write(&source, b"new content").unwrap();
                }
                incoming.handle(&receiver, event);
            }
        })
    };
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        outgoing.send(
            &link.sender,
            1,
            tokio::fs::File::open(&source).await.unwrap(),
            "file.bin",
            11,
            "application/octet-stream",
            None,
        ),
    )
    .await
    .unwrap();
    assert!(result.is_err());
    assert!(!inbox.join("file.bin").exists());
    send_pump.abort();
    receive_pump.abort();
}

#[tokio::test]
async fn verified_file_survives_persistence_failure() {
    let dir = tempfile::tempdir().unwrap();
    let mut link = TestLink::new().await;
    let incoming = transfers(dir.path());
    let outgoing = transfers(&dir.path().join("out"));
    // Имя длиннее ограничения Linux в байтах: хеш корректен, но публикация невозможна.
    let name = "🚀".repeat(70);
    let content = b"received content";
    let part = dir.path().join(part_name(
        &link.receiver.peer(),
        &name,
        content.len() as u64,
        Some(&digest(content)),
    ));
    link.pump(outgoing.clone(), incoming.clone());
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        outgoing.send(
            &link.sender,
            1,
            Cursor::new(content),
            &name,
            content.len() as u64,
            "application/octet-stream",
            None,
        ),
    )
    .await
    .unwrap();
    assert!(result.is_err());
    assert_eq!(std::fs::read(part).unwrap(), content);
    assert!(incoming.pending.lock().unwrap().is_empty());
}

#[tokio::test]
async fn stalled_receive_releases_peer_slot() {
    let dir = tempfile::tempdir().unwrap();
    let mut link = TestLink::new().await;
    let (incoming, mut updates) = observed(dir.path());
    assert!(matches!(offer(&incoming, &link.receiver, 1, "idle.bin", b"data"), Message::FileAccept { .. }));
    let mut stream = link.sender.connection().open_uni().await.unwrap();
    stream.write_all(&1u64.to_be_bytes()).await.unwrap();
    stream.write_all(b"d").await.unwrap();
    let recv = receive_stream(link.receiver_events.as_mut().unwrap()).await;
    let worker = {
        let incoming = incoming.clone();
        let session = link.receiver.clone();
        tokio::spawn(async move { incoming.receive(&session, recv).await })
    };
    tokio::time::timeout(Duration::from_secs(5), updates.recv()).await.unwrap().unwrap();
    // Progress is emitted before async file creation; wait for actual writes
    // before advancing the clock so the idle-read timer has been installed.
    let part = incoming.pending.lock().unwrap().values().next().unwrap().part.clone();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if std::fs::metadata(&part).is_ok_and(|m| m.len() == 1) { break; }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await.unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    tokio::time::pause();
    tokio::time::advance(OFFER_TTL + Duration::from_secs(1)).await;
    tokio::time::resume();
    tokio::time::timeout(Duration::from_secs(5), worker).await.unwrap().unwrap().unwrap();
    assert!(incoming.pending.lock().unwrap().is_empty());
    let mut failed = false;
    while let Ok(update) = updates.try_recv() {
        failed |= matches!(update.state, TransferState::Failed(_));
    }
    assert!(failed);
}
