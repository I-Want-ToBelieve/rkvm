use rkvm_input::abs::{AbsAxis, AbsInfo};
use rkvm_input::event::Event;
use rkvm_input::key::{Key, KeyEvent, Keyboard};
use rkvm_input::monitor::Monitor;
use rkvm_input::rel::RelAxis;
use rkvm_net::auth::{AuthChallenge, AuthResponse, AuthStatus};
use rkvm_net::message::Message;
use rkvm_net::version::Version;
use rkvm_net::Update;
use slab::Slab;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::CString;
use std::io::{self, ErrorKind};
use std::net::SocketAddr;
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncWriteExt, BufStream};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::time;
use tokio_rustls::TlsAcceptor;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Network error: {0}")]
    Network(io::Error),
    #[error("Input error: {0}")]
    Input(io::Error),
    #[error("Event queue overflow")]
    Overflow,
}

pub async fn run(
    listen: SocketAddr,
    acceptor: TlsAcceptor,
    password: &str,
    switch_keys: &HashSet<Keyboard>,
) -> Result<(), Error> {
    let listener = TcpListener::bind(&listen).await.map_err(Error::Network)?;
    log::info!("Listening on {}", listen);

    let mut monitor = Monitor::new();
    let mut devices = Slab::<Device>::new();
    let mut clients = Slab::new();
    let mut current = 0;
    let mut pressed_keys = HashSet::new();

    let (events_sender, mut events_receiver) = mpsc::channel(1);

    loop {
        let event = async { events_receiver.recv().await.unwrap() };

        tokio::select! {
            result = listener.accept() => {
                let (stream, addr) = result.map_err(Error::Network)?;
                let acceptor = acceptor.clone();
                let password = password.to_owned();

                // Remove dead clients.
                clients.retain(|_, client: &mut Sender<_>| !client.is_closed());
                if !clients.contains(current) {
                    current = 0;
                }

                let init_updates = devices
                    .iter()
                    .map(|(id, device)| Update::CreateDevice {
                        id,
                        name: device.name.clone(),
                        version: device.version,
                        vendor: device.vendor,
                        product: device.product,
                        rel: device.rel.clone(),
                        abs: device.abs.clone(),
                        keys: device.keys.clone(),
                    })
                    .collect();

                let (sender, receiver) = mpsc::channel(1);
                clients.insert(sender);

                tokio::spawn(async move {
                    log::info!("{}: Connected", addr);

                    match client(init_updates, receiver, stream, addr, acceptor, &password).await {
                        Ok(()) => log::info!("{}: Disconnected", addr),
                        Err(err) => log::error!("{}: Disconnected: {}", addr, err),
                    }
                });
            }
            result = monitor.read() => {
                let mut interceptor = result.map_err(Error::Input)?;

                let name = interceptor.name().to_owned();
                let id = devices.vacant_key();
                let version = interceptor.version();
                let vendor = interceptor.vendor();
                let product = interceptor.product();
                let rel = interceptor.rel().collect::<HashSet<_>>();
                let abs = interceptor.abs().collect::<HashMap<_,_>>();
                let keys = interceptor.key().collect::<HashSet<_>>();

                for (_, sender) in &clients {
                    let update = Update::CreateDevice {
                        id,
                        name: name.clone(),
                        version: version.clone(),
                        vendor: vendor.clone(),
                        product: product.clone(),
                        rel: rel.clone(),
                        abs: abs.clone(),
                        keys: keys.clone(),
                    };

                    let _ = sender.send(update).await;
                }

                let (interceptor_sender, mut interceptor_receiver) = mpsc::channel(32);
                devices.insert(Device {
                    name,
                    version,
                    vendor,
                    product,
                    rel,
                    abs,
                    keys,
                    sender: interceptor_sender,
                });

                let events_sender = events_sender.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            event = interceptor.read() => {
                                if event.is_err() | events_sender.send((id, event)).await.is_err() {
                                    break;
                                }
                            }
                            event = interceptor_receiver.recv() => {
                                let event = match event {
                                    Some(event) => event,
                                    None => break,
                                };

                                match interceptor.write(&event).await {
                                    Ok(()) => {},
                                    Err(err) => {
                                        let _ = events_sender.send((id, Err(err))).await;
                                        break;
                                    }
                                }

                                log::trace!("Wrote an event to device {}", id);
                            }
                        }
                    }
                });

                let device = &devices[id];

                log::info!(
                    "Registered new device {} (name {:?}, vendor {}, product {}, version {})",
                    id,
                    device.name,
                    device.vendor,
                    device.product,
                    device.version
                );
            }
            (id, result) = event => match result {
                Ok(event) => {
                    let mut changed = false;

                    if let Event::Key(KeyEvent { key: Key::Key(key), down }) = event {
                        if switch_keys.contains(&key) {
                            changed = true;

                            match down {
                                true => pressed_keys.insert(key),
                                false => pressed_keys.remove(&key),
                            };
                        }
                    }

                    // Who to send this batch of events to.
                    let idx = current;

                    if changed && pressed_keys.len() == switch_keys.len() {
                        let exists = |idx| idx == 0 || clients.contains(idx - 1);
                        loop {
                            current = (current + 1) % (clients.len() + 1);
                            if exists(current) {
                                break;
                            }
                        }

                        log::debug!("Switched to client {}", current);
                    }

                    // Index 0 - special case to keep the modular arithmetic above working.
                    if idx == 0 {
                        // We do a try_send() here rather than a "blocking" send in order to prevent deadlocks.
                        // In this scenario, the interceptor task is sending events to the main task,
                        // while the main task is simultaneously sending events back to the interceptor.
                        // This creates a classic deadlock situation where both tasks are waiting for each other.
                        match devices[id].sender.try_send(event) {
                            Ok(()) | Err(TrySendError::Closed(_)) => continue,
                            Err(TrySendError::Full(_)) => return Err(Error::Overflow),
                        }
                    }

                    if clients[idx - 1].send(Update::Event { id, event }).await.is_err() {
                        clients.remove(idx - 1);

                        if current == idx {
                            current = 0;
                        }
                    }
                }
                Err(err) if err.kind() == ErrorKind::BrokenPipe => {
                    for (_, sender) in &clients {
                        let _ = sender.send(Update::DestroyDevice { id }).await;
                    }
                    devices.remove(id);

                    log::info!("Destroyed device {}", id);
                }
                Err(err) => return Err(Error::Input(err)),
            }
        }
    }
}

struct Device {
    name: CString,
    vendor: u16,
    product: u16,
    version: u16,
    rel: HashSet<RelAxis>,
    abs: HashMap<AbsAxis, AbsInfo>,
    keys: HashSet<Key>,
    sender: Sender<Event>,
}

#[derive(Error, Debug)]
enum ClientError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("Incompatible client version (got {client}, expected {server})")]
    Version { server: Version, client: Version },
    #[error("Invalid password")]
    Auth,
    #[error(transparent)]
    Rand(#[from] rand::Error),
}

async fn client(
    mut init_updates: VecDeque<Update>,
    mut receiver: Receiver<Update>,
    stream: TcpStream,
    addr: SocketAddr,
    acceptor: TlsAcceptor,
    password: &str,
) -> Result<(), ClientError> {
    let negotiate = async {
        stream.set_linger(None)?;
        stream.set_nodelay(false)?;

        let stream = acceptor.accept(stream).await?;
        log::info!("{}: TLS connected", addr);

        let mut stream = BufStream::with_capacity(1024, 1024, stream);

        Version::CURRENT.encode(&mut stream).await?;
        stream.flush().await?;

        let version = Version::decode(&mut stream).await?;
        if version != Version::CURRENT {
            return Err(ClientError::Version {
                server: Version::CURRENT,
                client: version,
            });
        }

        let challenge = AuthChallenge::generate().await?;

        challenge.encode(&mut stream).await?;
        stream.flush().await?;

        let response = AuthResponse::decode(&mut stream).await?;
        let status = match response.verify(&challenge, password) {
            true => AuthStatus::Passed,
            false => AuthStatus::Failed,
        };

        status.encode(&mut stream).await?;
        stream.flush().await?;

        if status == AuthStatus::Failed {
            return Err(ClientError::Auth);
        }

        log::info!("{}: Authenticated successfully", addr);

        Ok(stream)
    };

    let mut stream = time::timeout(Duration::from_secs(1), negotiate)
        .await
        .map_err(|_| io::Error::new(ErrorKind::TimedOut, "Negotiation took too long"))??;

    loop {
        let recv = async {
            match init_updates.pop_front() {
                Some(update) => Some((update, false)),
                None => receiver.recv().await.map(|update| (update, true)),
            }
        };

        let (update, more) = match recv.await {
            Some(update) => update,
            None => break,
        };

        let mut count = 1;

        let write = async {
            update.encode(&mut stream).await?;

            // Coalesce multiple updates into one chunk.
            if more {
                while let Ok(update) = receiver.try_recv() {
                    update.encode(&mut stream).await?;
                    count += 1;
                }
            }

            stream.flush().await
        };

        time::timeout(Duration::from_millis(500), write)
            .await
            .map_err(|_| io::Error::new(ErrorKind::TimedOut, "Update writing took too long"))??;

        log::trace!(
            "{}: Wrote {} update{}",
            addr,
            count,
            if count == 1 { "" } else { "s" }
        );
    }

    Ok(())
}
